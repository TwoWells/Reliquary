// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Matroska muxer — writes complete MKV files from framed media data.
//!
//! The muxer produces MKV files with proper EBML framing, `SeekHead`
//! indexing, and segment structure.  It uses a phased API: construct
//! with [`MkvMuxer::new`], add tracks, write clusters, then finalize.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::io::{self, Seek, SeekFrom, Write};

use super::ebml;

// ---------------------------------------------------------------------------
// Element IDs
// ---------------------------------------------------------------------------

// Top-level
const EBML_ID: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;

// EBML Header children
const EBML_VERSION: u32 = 0x4286;
const EBML_READ_VER: u32 = 0x42F7;
const EBML_MAX_ID_LEN: u32 = 0x42F2;
const EBML_MAX_SZ_LEN: u32 = 0x42F3;
const DOC_TYPE: u32 = 0x4282;
const DOC_TYPE_VER: u32 = 0x4287;
const DOC_TYPE_READ: u32 = 0x4285;

// SeekHead
const SEEK_HEAD: u32 = 0x114D_9B74;
const SEEK: u32 = 0x4DBB;
const SEEK_ID: u32 = 0x53AB;
const SEEK_POSITION: u32 = 0x53AC;

// Segment Info
const INFO: u32 = 0x1549_A966;
const TIMESTAMP_SCALE: u32 = 0x002A_D7B1;
const DURATION: u32 = 0x4489;
const MUXING_APP: u32 = 0x4D80;
const WRITING_APP: u32 = 0x5741;
const SEGMENT_UID: u32 = 0x73A4;
const TITLE: u32 = 0x7BA9;

// Top-level element IDs referenced by SeekHead entries
/// Tracks element ID.
pub(crate) const TRACKS: u32 = 0x1654_AE6B;
/// Cues element ID.
pub(crate) const CUES: u32 = 0x1C53_BB6B;
/// Chapters element ID.
pub(crate) const CHAPTERS: u32 = 0x1043_A770;
/// Tags element ID.
pub(crate) const TAGS: u32 = 0x1254_C367;

// ---------------------------------------------------------------------------
// Layout constants
// ---------------------------------------------------------------------------

/// EBML header content size (sum of all child elements).
///
/// ```text
/// EBMLVersion(4) + EBMLReadVersion(4) + EBMLMaxIDLength(4) +
/// EBMLMaxSizeLength(4) + DocType(11) + DocTypeVersion(4) +
/// DocTypeReadVersion(4) = 35
/// ```
const EBML_HEADER_CONTENT_SIZE: u64 = 35;

/// Bytes per `Seek` entry: master header (3) + `SeekID` (7) +
/// `SeekPosition` (8) = 18.
const SEEK_ENTRY_SIZE: usize = 18;

/// Number of bytes reserved for each `SeekPosition` uint value.
/// 5 bytes supports offsets up to 2^40 (1 TiB).
const SEEK_POSITION_RESERVED: u8 = 5;

/// Total size of the `SeekHead` region (`SeekHead` element + Void padding).
const SEEK_HEAD_REGION_SIZE: usize = 256;

/// Default timestamp scale: 1,000,000 ns (1 ms resolution).
const DEFAULT_TIMESTAMP_SCALE: u64 = 1_000_000;

/// Muxing application name written into Segment Info.
const MUXING_APP_STR: &str = "libreliquary";

/// Writing application name written into Segment Info.
const WRITING_APP_STR: &str = "reliquary";

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Configuration for the Segment Info element.
pub struct SegmentInfo {
    /// Total duration in nanoseconds (optional — can be omitted).
    pub duration_ns: Option<u64>,
    /// Content title (optional).
    pub title: Option<String>,
}

/// Matroska muxer.  Writes a complete MKV file to the output.
pub struct MkvMuxer<W: Write + Seek> {
    writer: W,
    /// Byte position of the first Segment data byte (after the Segment
    /// ID + unknown-size VINT).  All Segment-relative offsets are computed
    /// by subtracting this value from absolute file positions.
    segment_data_start: u64,
    /// `SeekHead` placeholder locations for later backpatching.
    seek_placeholders: Vec<SeekPlaceholder>,
    /// Current byte position in the output.
    position: u64,
}

/// Recorded location of a `SeekPosition` placeholder in the `SeekHead`.
struct SeekPlaceholder {
    /// The element ID this entry is for (e.g. `TRACKS`, `CUES`).
    element_id: u32,
    /// Absolute file position of the `SeekPosition` value bytes.
    file_offset: u64,
    /// Number of bytes reserved for the `SeekPosition` value.
    reserved_bytes: u8,
}

// ---------------------------------------------------------------------------
// MkvMuxer implementation
// ---------------------------------------------------------------------------

impl<W: Write + Seek> MkvMuxer<W> {
    /// Creates a new muxer, writing the EBML header, Segment element,
    /// `SeekHead` with placeholders, and Segment Info.
    ///
    /// Set `has_chapters` / `has_tags` to `true` if the file will contain
    /// chapters or tags — this reserves `SeekHead` entries for them.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] if any write to the output fails.
    pub fn new(
        mut writer: W,
        info: &SegmentInfo,
        has_chapters: bool,
        has_tags: bool,
    ) -> io::Result<Self> {
        let mut position = 0u64;

        // EBML header.
        position += write_ebml_header(&mut writer)? as u64;

        // Segment element with unknown size.
        position += ebml::write_master_unknown_size(&mut writer, SEGMENT)? as u64;
        let segment_data_start = position;

        // SeekHead with placeholder entries + Void padding.
        let mut seek_placeholders = Vec::new();
        position += write_seek_head_region(
            &mut writer,
            position,
            has_chapters,
            has_tags,
            &mut seek_placeholders,
        )? as u64;

        // Segment Info.
        position += write_segment_info(&mut writer, info)? as u64;

        Ok(Self {
            writer,
            segment_data_start,
            seek_placeholders,
            position,
        })
    }

    /// Returns the current write position in the output.
    #[must_use]
    pub const fn position(&self) -> u64 {
        self.position
    }

    /// Returns the Segment data start position.
    ///
    /// Segment-relative offsets (used in `SeekHead` and `Cues`) are
    /// computed as `absolute_position - segment_data_start()`.
    #[must_use]
    pub const fn segment_data_start(&self) -> u64 {
        self.segment_data_start
    }

    /// Overwrites a `SeekHead` placeholder with the actual byte position
    /// of a top-level element.
    ///
    /// `absolute_position` is the absolute file position where the element
    /// was written.  The method converts it to a Segment-relative offset
    /// and writes it into the reserved placeholder bytes.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] if `element_id` has no placeholder, or if
    /// seeking/writing fails.
    pub fn backpatch_seek_entry(
        &mut self,
        element_id: u32,
        absolute_position: u64,
    ) -> io::Result<()> {
        let placeholder = self
            .seek_placeholders
            .iter()
            .find(|p| p.element_id == element_id)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("no SeekHead placeholder for element ID {element_id:#010X}"),
                )
            })?;

        let relative_position = absolute_position - self.segment_data_start;
        let file_offset = placeholder.file_offset;
        let reserved = placeholder.reserved_bytes;

        // Seek to the placeholder and overwrite with the actual offset.
        self.writer.seek(SeekFrom::Start(file_offset))?;
        let bytes = relative_position.to_be_bytes();
        self.writer.write_all(&bytes[8 - usize::from(reserved)..])?;

        // Seek back to the current write position.
        self.writer.seek(SeekFrom::Start(self.position))?;

        Ok(())
    }

    /// Finalizes the MKV file and returns the underlying writer.
    ///
    /// Full implementation (cues, final backpatch) is added in a later
    /// ticket.  This stub returns the writer as-is.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] if finalization fails.
    pub fn finalize(self) -> io::Result<W> {
        Ok(self.writer)
    }
}

// ---------------------------------------------------------------------------
// Internal element writers
// ---------------------------------------------------------------------------

/// Writes the fixed EBML header (40 bytes).
fn write_ebml_header(w: &mut impl Write) -> io::Result<usize> {
    let mut written = ebml::write_master(w, EBML_ID, EBML_HEADER_CONTENT_SIZE)?;
    written += ebml::write_uint(w, EBML_VERSION, 1)?;
    written += ebml::write_uint(w, EBML_READ_VER, 1)?;
    written += ebml::write_uint(w, EBML_MAX_ID_LEN, 4)?;
    written += ebml::write_uint(w, EBML_MAX_SZ_LEN, 8)?;
    written += ebml::write_string(w, DOC_TYPE, "matroska")?;
    written += ebml::write_uint(w, DOC_TYPE_VER, 4)?;
    written += ebml::write_uint(w, DOC_TYPE_READ, 2)?;
    Ok(written)
}

/// Writes the `SeekHead` element followed by Void padding.
///
/// Total bytes written is always [`SEEK_HEAD_REGION_SIZE`].
fn write_seek_head_region(
    w: &mut impl Write,
    base_position: u64,
    has_chapters: bool,
    has_tags: bool,
    placeholders: &mut Vec<SeekPlaceholder>,
) -> io::Result<usize> {
    let mut entry_ids = vec![TRACKS, CUES];
    if has_chapters {
        entry_ids.push(CHAPTERS);
    }
    if has_tags {
        entry_ids.push(TAGS);
    }

    let content_size = entry_ids.len() * SEEK_ENTRY_SIZE;

    let mut written = ebml::write_master(w, SEEK_HEAD, content_size as u64)?;

    let mut entry_abs_pos = base_position + written as u64;
    for &id in &entry_ids {
        let n = write_seek_entry(w, id, entry_abs_pos, placeholders)?;
        written += n;
        entry_abs_pos += n as u64;
    }

    // Void padding to fill the rest of the reserved region.
    let void_size = SEEK_HEAD_REGION_SIZE - written;
    written += ebml::write_void(w, void_size)?;

    debug_assert_eq!(
        written, SEEK_HEAD_REGION_SIZE,
        "SeekHead region size mismatch: expected {SEEK_HEAD_REGION_SIZE}, got {written}"
    );

    Ok(written)
}

/// Writes a single `Seek` entry and records its placeholder.
fn write_seek_entry(
    w: &mut impl Write,
    element_id: u32,
    entry_abs_pos: u64,
    placeholders: &mut Vec<SeekPlaceholder>,
) -> io::Result<usize> {
    let content_size = seek_entry_content_size();
    let mut written = ebml::write_master(w, SEEK, content_size)?;

    // SeekID: binary element with the 4-byte target element ID.
    let id_bytes = element_id.to_be_bytes();
    written += ebml::write_binary(w, SEEK_ID, &id_bytes)?;

    // SeekPosition: uint with reserved bytes for backpatching.
    // The value bytes start after the SeekPosition element header
    // (element ID + VINT size).
    let seek_pos_header_len = element_id_width(SEEK_POSITION) + 1; // +1 for VINT(5)
    let value_file_offset = entry_abs_pos + written as u64 + seek_pos_header_len as u64;

    written += write_uint_fixed(w, SEEK_POSITION, 0, SEEK_POSITION_RESERVED)?;

    placeholders.push(SeekPlaceholder {
        element_id,
        file_offset: value_file_offset,
        reserved_bytes: SEEK_POSITION_RESERVED,
    });

    Ok(written)
}

/// Writes the Segment Info element.
fn write_segment_info(w: &mut impl Write, info: &SegmentInfo) -> io::Result<usize> {
    let uid = generate_segment_uid();

    let duration_value = info.duration_ns.map(ns_to_timestamp_scale_units);

    // Pre-compute content size.
    let mut content_size: usize = 0;
    content_size += measure_uint(TIMESTAMP_SCALE, DEFAULT_TIMESTAMP_SCALE);
    if duration_value.is_some() {
        content_size += measure_float(DURATION);
    }
    content_size += measure_utf8(MUXING_APP, MUXING_APP_STR);
    content_size += measure_utf8(WRITING_APP, WRITING_APP_STR);
    content_size += measure_binary(SEGMENT_UID, &uid);
    if let Some(ref title) = info.title {
        content_size += measure_utf8(TITLE, title);
    }

    // Write Info master + children.
    let mut written = ebml::write_master(w, INFO, content_size as u64)?;
    written += ebml::write_uint(w, TIMESTAMP_SCALE, DEFAULT_TIMESTAMP_SCALE)?;
    if let Some(duration) = duration_value {
        written += ebml::write_float(w, DURATION, duration)?;
    }
    written += ebml::write_utf8(w, MUXING_APP, MUXING_APP_STR)?;
    written += ebml::write_utf8(w, WRITING_APP, WRITING_APP_STR)?;
    written += ebml::write_binary(w, SEGMENT_UID, &uid)?;
    if let Some(ref title) = info.title {
        written += ebml::write_utf8(w, TITLE, title)?;
    }

    Ok(written)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Writes a uint element with a fixed-width value (zero-padded to `width`
/// bytes).  Used for `SeekPosition` placeholders.
fn write_uint_fixed(w: &mut impl Write, id: u32, value: u64, width: u8) -> io::Result<usize> {
    let mut written = ebml::write_element_id(w, id)?;
    written += ebml::write_vint(w, u64::from(width), 1)?;
    let bytes = value.to_be_bytes();
    w.write_all(&bytes[8 - usize::from(width)..])?;
    written += usize::from(width);
    Ok(written)
}

/// Converts nanoseconds to timestamp-scale units (milliseconds with the
/// default 1 ms timestamp scale).
#[allow(
    clippy::cast_precision_loss,
    reason = "Matroska Duration is a float element — precision loss is inherent to the format"
)]
fn ns_to_timestamp_scale_units(ns: u64) -> f64 {
    ns as f64 / DEFAULT_TIMESTAMP_SCALE as f64
}

/// Generates a 16-byte random Segment UID using [`RandomState`].
fn generate_segment_uid() -> [u8; 16] {
    let state = RandomState::new();
    let hash1 = state.hash_one(0u64);
    let hash2 = state.hash_one(1u64);
    let mut uid = [0u8; 16];
    uid[..8].copy_from_slice(&hash1.to_le_bytes());
    uid[8..].copy_from_slice(&hash2.to_le_bytes());
    uid
}

/// Content size of a `Seek` entry (`SeekID` + `SeekPosition` children).
fn seek_entry_content_size() -> u64 {
    // SeekID: 2-byte ID + 1-byte VINT(4) + 4-byte data = 7.
    let seek_id_size: u64 = 7;
    // SeekPosition: 2-byte ID + 1-byte VINT(width) + width-byte data.
    let seek_pos_size = 2 + 1 + u64::from(SEEK_POSITION_RESERVED);
    seek_id_size + seek_pos_size
}

/// Returns the byte width of an element ID.  Only used with known-valid
/// IDs from module constants.
const fn element_id_width(id: u32) -> usize {
    match id {
        0x81..=0xFE => 1,
        0x4000..=0x7FFF => 2,
        0x20_0000..=0x3F_FFFF => 3,
        _ => 4,
    }
}

// ---------------------------------------------------------------------------
// Measurement helpers — compute element sizes without writing
// ---------------------------------------------------------------------------

/// Measures a complete uint element (ID + size VINT + value bytes).
const fn measure_uint(id: u32, value: u64) -> usize {
    let data_len = uint_byte_len(value);
    element_id_width(id) + vint_width(data_len as u64) + data_len
}

/// Measures a complete float element (ID + size VINT + 8-byte value).
const fn measure_float(id: u32) -> usize {
    element_id_width(id) + 1 + 8
}

/// Measures a complete UTF-8 string element.
const fn measure_utf8(id: u32, value: &str) -> usize {
    let data_len = value.len();
    element_id_width(id) + vint_width(data_len as u64) + data_len
}

/// Measures a complete binary element.
const fn measure_binary(id: u32, data: &[u8]) -> usize {
    element_id_width(id) + vint_width(data.len() as u64) + data.len()
}

/// Returns the minimum byte count for a uint value (0 returns 0).
const fn uint_byte_len(value: u64) -> usize {
    if value == 0 {
        return 0;
    }
    ((u64::BITS - value.leading_zeros()) as usize).div_ceil(8)
}

/// Returns the minimum VINT width for a value.
const fn vint_width(value: u64) -> usize {
    if value <= (1u64 << 7) - 2 {
        1
    } else if value <= (1u64 << 14) - 2 {
        2
    } else if value <= (1u64 << 21) - 2 {
        3
    } else if value <= (1u64 << 28) - 2 {
        4
    } else if value <= (1u64 << 35) - 2 {
        5
    } else if value <= (1u64 << 42) - 2 {
        6
    } else if value <= (1u64 << 49) - 2 {
        7
    } else {
        8
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod tests {
    use std::io::{self, Cursor};

    use super::{
        CHAPTERS, CUES, MkvMuxer, SEEK_HEAD_REGION_SIZE, SEEK_POSITION_RESERVED, SegmentInfo, TAGS,
        TRACKS, write_ebml_header, write_seek_head_region, write_segment_info,
    };

    // -------------------------------------------------------------------
    // EBML header
    // -------------------------------------------------------------------

    #[test]
    fn ebml_header_exact_bytes() {
        let mut buf = Vec::new();
        let n = write_ebml_header(&mut buf).expect("write EBML header");
        assert_eq!(n, 40, "EBML header total size");
        assert_eq!(n, buf.len(), "return value matches output length");

        #[rustfmt::skip]
        let expected: &[u8] = &[
            0x1A, 0x45, 0xDF, 0xA3,                         // EBML ID
            0xA3,                                             // Size = 35
            0x42, 0x86, 0x81, 0x01,                          // EBMLVersion = 1
            0x42, 0xF7, 0x81, 0x01,                          // EBMLReadVersion = 1
            0x42, 0xF2, 0x81, 0x04,                          // EBMLMaxIDLength = 4
            0x42, 0xF3, 0x81, 0x08,                          // EBMLMaxSizeLength = 8
            0x42, 0x82, 0x88,                                // DocType ID + size(8)
            0x6D, 0x61, 0x74, 0x72, 0x6F, 0x73, 0x6B, 0x61, // "matroska"
            0x42, 0x87, 0x81, 0x04,                          // DocTypeVersion = 4
            0x42, 0x85, 0x81, 0x02,                          // DocTypeReadVersion = 2
        ];
        assert_eq!(buf, expected, "EBML header byte sequence");
    }

    // -------------------------------------------------------------------
    // SeekHead
    // -------------------------------------------------------------------

    #[test]
    fn seek_head_two_entries_structure() {
        let mut cursor = Cursor::new(Vec::new());
        let mut placeholders = Vec::new();
        let n = write_seek_head_region(&mut cursor, 0, false, false, &mut placeholders)
            .expect("write SeekHead");

        assert_eq!(n, SEEK_HEAD_REGION_SIZE, "region total size");
        assert_eq!(placeholders.len(), 2, "placeholder count");
        assert_eq!(placeholders[0].element_id, TRACKS, "first entry is Tracks");
        assert_eq!(placeholders[1].element_id, CUES, "second entry is Cues");
        assert_eq!(
            placeholders[0].reserved_bytes, SEEK_POSITION_RESERVED,
            "reserved bytes per entry"
        );

        let data = cursor.into_inner();
        assert_eq!(data.len(), SEEK_HEAD_REGION_SIZE, "output length");

        // SeekHead ID.
        assert_eq!(&data[..4], [0x11, 0x4D, 0x9B, 0x74], "SeekHead element ID");

        // SeekHead content size = 36 (2 entries * 18 bytes).
        assert_eq!(data[4], 0xA4, "SeekHead content size VINT = 36");

        // First Seek entry starts at offset 5.
        assert_eq!(&data[5..7], [0x4D, 0xBB], "first Seek element ID");
        assert_eq!(data[7], 0x8F, "first Seek content size = 15");

        // First SeekID.
        assert_eq!(&data[8..10], [0x53, 0xAB], "SeekID element ID");
        assert_eq!(data[10], 0x84, "SeekID size = 4");
        assert_eq!(
            &data[11..15],
            TRACKS.to_be_bytes(),
            "SeekID value = Tracks ID"
        );

        // First SeekPosition.
        assert_eq!(&data[15..17], [0x53, 0xAC], "SeekPosition element ID");
        assert_eq!(data[17], 0x85, "SeekPosition size = 5");
        assert_eq!(
            &data[18..23],
            [0x00; 5],
            "SeekPosition placeholder is zeros"
        );
    }

    #[test]
    fn seek_head_four_entries() {
        let mut cursor = Cursor::new(Vec::new());
        let mut placeholders = Vec::new();
        let n = write_seek_head_region(&mut cursor, 0, true, true, &mut placeholders)
            .expect("write SeekHead with all entries");

        assert_eq!(n, SEEK_HEAD_REGION_SIZE, "region total size with 4 entries");
        assert_eq!(placeholders.len(), 4, "placeholder count");
        assert_eq!(placeholders[0].element_id, TRACKS, "Tracks entry");
        assert_eq!(placeholders[1].element_id, CUES, "Cues entry");
        assert_eq!(placeholders[2].element_id, CHAPTERS, "Chapters entry");
        assert_eq!(placeholders[3].element_id, TAGS, "Tags entry");
    }

    // -------------------------------------------------------------------
    // Segment Info
    // -------------------------------------------------------------------

    #[test]
    fn segment_info_with_duration_and_title() {
        let mut buf = Vec::new();
        let info = SegmentInfo {
            duration_ns: Some(120_000_000_000),
            title: Some("Test Title".to_string()),
        };
        let n = write_segment_info(&mut buf, &info).expect("write Segment Info");

        assert_eq!(n, buf.len(), "return value matches output length");
        assert_eq!(n, 82, "Info element total size with duration and title");

        // Info ID.
        assert_eq!(&buf[..4], [0x15, 0x49, 0xA9, 0x66], "Info element ID");

        // Content size = 77.
        assert_eq!(buf[4], 0xCD, "Info content size VINT = 77");

        assert!(
            contains_bytes(&buf, b"libreliquary"),
            "should contain MuxingApp"
        );
        assert!(
            contains_bytes(&buf, b"reliquary"),
            "should contain WritingApp"
        );
        assert!(contains_bytes(&buf, b"Test Title"), "should contain title");

        // Duration = 120_000.0 ms.
        let duration_bytes = 120_000.0_f64.to_be_bytes();
        assert!(
            contains_bytes(&buf, &duration_bytes),
            "should contain duration float"
        );
    }

    #[test]
    fn segment_info_without_optional_fields() {
        let mut buf = Vec::new();
        let info = SegmentInfo {
            duration_ns: None,
            title: None,
        };
        let n = write_segment_info(&mut buf, &info).expect("write Segment Info");

        assert_eq!(n, buf.len(), "return value matches output length");
        assert_eq!(n, 58, "Info element total size without optionals");
        assert_eq!(&buf[..4], [0x15, 0x49, 0xA9, 0x66], "Info element ID");

        assert!(
            contains_bytes(&buf, b"libreliquary"),
            "should contain MuxingApp"
        );
        assert!(
            contains_bytes(&buf, b"reliquary"),
            "should contain WritingApp"
        );
    }

    // -------------------------------------------------------------------
    // Backpatch
    // -------------------------------------------------------------------

    #[test]
    fn backpatch_overwrites_placeholder() {
        let info = SegmentInfo {
            duration_ns: None,
            title: None,
        };
        let mut muxer =
            MkvMuxer::new(Cursor::new(Vec::new()), &info, false, false).expect("create muxer");

        let segment_start = muxer.segment_data_start();
        let target = segment_start + 1000;

        muxer
            .backpatch_seek_entry(TRACKS, target)
            .expect("backpatch TRACKS");

        let data = muxer.finalize().expect("finalize").into_inner();

        // The TRACKS SeekPosition value bytes are at absolute offset:
        // segment_data_start + 5 (SeekHead header) + 3 (Seek header)
        // + 7 (SeekID) + 3 (SeekPosition header) = segment_data_start + 18.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "test uses a Cursor<Vec<u8>> — position is always small"
        )]
        let value_offset = segment_start as usize + 18;
        let actual = &data[value_offset..value_offset + 5];

        // Relative position = 1000 = 0x3E8, as 5-byte big-endian.
        assert_eq!(
            actual,
            [0x00, 0x00, 0x00, 0x03, 0xE8],
            "backpatched position bytes"
        );
    }

    #[test]
    fn backpatch_unknown_element_fails() {
        let info = SegmentInfo {
            duration_ns: None,
            title: None,
        };
        let mut muxer =
            MkvMuxer::new(Cursor::new(Vec::new()), &info, false, false).expect("create muxer");

        let err = muxer
            .backpatch_seek_entry(CHAPTERS, 100)
            .expect_err("should fail for missing placeholder");
        assert_eq!(
            err.kind(),
            io::ErrorKind::InvalidInput,
            "error kind for missing placeholder"
        );
    }

    // -------------------------------------------------------------------
    // Round-trip / integration
    // -------------------------------------------------------------------

    #[test]
    fn round_trip_file_structure() {
        let info = SegmentInfo {
            duration_ns: Some(60_000_000_000),
            title: Some("Round Trip Test".to_string()),
        };
        let muxer =
            MkvMuxer::new(Cursor::new(Vec::new()), &info, true, true).expect("create muxer");

        let data = muxer.finalize().expect("finalize").into_inner();

        // EBML header ID at byte 0.
        assert_eq!(
            &data[..4],
            [0x1A, 0x45, 0xDF, 0xA3],
            "file starts with EBML ID"
        );

        // Segment ID at byte 40.
        assert_eq!(
            &data[40..44],
            [0x18, 0x53, 0x80, 0x67],
            "Segment ID at expected position"
        );

        // Segment unknown size.
        assert_eq!(
            &data[44..52],
            [0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
            "Segment unknown size"
        );

        assert!(
            contains_bytes(&data, b"libreliquary"),
            "MuxingApp in output"
        );
        assert!(contains_bytes(&data, b"reliquary"), "WritingApp in output");
    }

    // -------------------------------------------------------------------
    // Offset tracking
    // -------------------------------------------------------------------

    #[test]
    fn segment_data_start_offset() {
        let info = SegmentInfo {
            duration_ns: None,
            title: None,
        };
        let muxer =
            MkvMuxer::new(Cursor::new(Vec::new()), &info, false, false).expect("create muxer");

        // EBML header (40) + Segment ID (4) + unknown size (8) = 52.
        assert_eq!(
            muxer.segment_data_start(),
            52,
            "segment_data_start should be 52"
        );
    }

    #[test]
    fn position_tracks_written_bytes() {
        let info = SegmentInfo {
            duration_ns: None,
            title: None,
        };
        let muxer =
            MkvMuxer::new(Cursor::new(Vec::new()), &info, false, false).expect("create muxer");

        let position = muxer.position();
        let data = muxer.finalize().expect("finalize").into_inner();

        assert_eq!(
            position,
            data.len() as u64,
            "tracked position equals actual output length"
        );
    }

    // -------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------

    #[must_use]
    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}
