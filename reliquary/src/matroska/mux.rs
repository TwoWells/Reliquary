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

// Track elements
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_UID: u32 = 0x73C5;
const TRACK_TYPE: u32 = 0x83;
const CODEC_ID: u32 = 0x86;
const CODEC_PRIVATE: u32 = 0x63A2;
const LANGUAGE: u32 = 0x0022_B59C;
const NAME: u32 = 0x536E;
const FLAG_DEFAULT: u32 = 0x88;
const FLAG_FORCED: u32 = 0x55AA;
const FLAG_LACING: u32 = 0x9C;
const DEFAULT_DURATION: u32 = 0x0023_E383;

// Video sub-elements
const VIDEO: u32 = 0xE0;
const PIXEL_WIDTH: u32 = 0xB0;
const PIXEL_HEIGHT: u32 = 0xBA;
const DISPLAY_WIDTH: u32 = 0x54B0;
const DISPLAY_HEIGHT: u32 = 0x54BA;
const FLAG_INTERLACED: u32 = 0x9A;
const COLOUR: u32 = 0x55B0;
const MATRIX_COEFF: u32 = 0x55B1;
const TRANSFER_CHAR: u32 = 0x55BA;
const PRIMARIES: u32 = 0x55BB;
const RANGE: u32 = 0x55B9;
const BITS_PER_CHAN: u32 = 0x55B2;

// Audio sub-elements
const AUDIO: u32 = 0xE1;
const SAMPLING_FREQ: u32 = 0xB5;
const CHANNELS: u32 = 0x9F;
const BIT_DEPTH: u32 = 0x6264;

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

/// A track to add to the MKV file.
pub enum TrackSpec {
    /// Video track.
    Video(VideoTrack),
    /// Audio track.
    Audio(AudioTrack),
    /// Subtitle track.
    Subtitle(SubtitleTrack),
}

/// Video track description.
pub struct VideoTrack {
    /// Matroska `CodecID` string (e.g. `"V_MPEG4/ISO/AVC"`).
    pub codec_id: &'static str,
    /// Codec initialization data (e.g. `AVCDecoderConfigurationRecord`).
    /// `None` for codecs that don't need it (MPEG-2).
    pub codec_private: Option<Vec<u8>>,
    /// Coded pixel width.
    pub pixel_width: u32,
    /// Coded pixel height.
    pub pixel_height: u32,
    /// Display width (for aspect ratio). `None` = same as pixel width.
    pub display_width: Option<u32>,
    /// Display height (for aspect ratio). `None` = same as pixel height.
    pub display_height: Option<u32>,
    /// Frame duration in nanoseconds (e.g. `41_708_333` for 23.976 fps).
    /// `None` = variable frame rate.
    pub default_duration_ns: Option<u64>,
    /// Interlacing flag. `None` = undetermined, `Some(true)` = interlaced,
    /// `Some(false)` = progressive.
    pub interlaced: Option<bool>,
    /// Track name (e.g. `"Main Video"`). Optional.
    pub name: Option<String>,
    /// Colour metadata for HDR content. Optional.
    pub colour: Option<VideoColour>,
}

/// Colour metadata for a video track (HDR).
pub struct VideoColour {
    /// ITU-T H.273 `MatrixCoefficients`.
    pub matrix_coefficients: u8,
    /// ITU-T H.273 `TransferCharacteristics`.
    pub transfer_characteristics: u8,
    /// ITU-T H.273 `ColourPrimaries`.
    pub primaries: u8,
    /// Signal range (1 = broadcast, 2 = full).
    pub range: u8,
    /// Bits per channel (e.g. 10 for HDR10).
    pub bits_per_channel: u8,
}

/// Audio track description.
pub struct AudioTrack {
    /// Matroska `CodecID` string (e.g. `"A_AC3"`).
    pub codec_id: &'static str,
    /// Codec initialization data. `None` for most audio codecs.
    pub codec_private: Option<Vec<u8>>,
    /// Sampling rate in Hz (e.g. 48000.0).
    pub sampling_frequency: f64,
    /// Channel count.
    pub channels: u8,
    /// Bits per sample (required for LPCM).
    pub bit_depth: Option<u8>,
    /// ISO 639-2 language code (e.g. `"eng"`).
    pub language: String,
    /// Track name (e.g. `"Director's Commentary"`).
    pub name: Option<String>,
    /// Whether this is the default audio track.
    pub is_default: bool,
}

/// Subtitle track description.
pub struct SubtitleTrack {
    /// Matroska `CodecID` string (e.g. `"S_HDMV/PGS"`).
    pub codec_id: &'static str,
    /// Codec initialization data. `None` for most subtitle codecs.
    pub codec_private: Option<Vec<u8>>,
    /// ISO 639-2 language code.
    pub language: String,
    /// Track name.
    pub name: Option<String>,
    /// Whether this is the default subtitle track.
    pub is_default: bool,
    /// Whether this is a forced subtitle track.
    pub is_forced: bool,
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
    /// Number of tracks added so far (used for `TrackNumber` assignment).
    track_count: u32,
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
            track_count: 0,
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

    /// Writes the Tracks element containing all provided track entries.
    ///
    /// Returns the assigned track numbers (1-based, in input order).
    /// Track numbers are globally sequential across multiple calls.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] if writing fails.
    pub fn add_tracks(&mut self, tracks: &[TrackSpec]) -> io::Result<Vec<u32>> {
        // Buffer all TrackEntry elements to measure the Tracks content size.
        let mut track_buf: Vec<u8> = Vec::new();
        let mut assigned = Vec::with_capacity(tracks.len());

        for spec in tracks {
            self.track_count += 1;
            let track_number = self.track_count;
            assigned.push(track_number);

            let uid = generate_track_uid(track_number);
            write_track_entry(&mut track_buf, track_number, uid, spec)?;
        }

        // Record position for SeekHead backpatch.
        let tracks_position = self.position;

        // Write the Tracks master element.
        let mut written =
            ebml::write_master(&mut self.writer, TRACKS, track_buf.len() as u64)? as u64;
        self.writer.write_all(&track_buf)?;
        written += track_buf.len() as u64;

        self.position += written;

        // Backpatch SeekHead entry for Tracks.
        self.backpatch_seek_entry(TRACKS, tracks_position)?;

        Ok(assigned)
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
// Track element writers
// ---------------------------------------------------------------------------

/// Writes a single `TrackEntry` master element.
///
/// `pub(crate)` for direct testing without the full muxer (avoids
/// random `SegmentUID` bytes in test assertions).
pub(crate) fn write_track_entry(
    w: &mut impl Write,
    track_number: u32,
    uid: u64,
    spec: &TrackSpec,
) -> io::Result<usize> {
    // Buffer children to measure content size.
    let mut children: Vec<u8> = Vec::new();
    write_track_entry_children(&mut children, track_number, uid, spec)?;

    let mut written = ebml::write_master(w, TRACK_ENTRY, children.len() as u64)?;
    w.write_all(&children)?;
    written += children.len();
    Ok(written)
}

/// Writes the children of a `TrackEntry`.
fn write_track_entry_children(
    w: &mut impl Write,
    track_number: u32,
    uid: u64,
    spec: &TrackSpec,
) -> io::Result<usize> {
    let mut written = 0;

    written += ebml::write_uint(w, TRACK_NUMBER, u64::from(track_number))?;
    written += ebml::write_uint(w, TRACK_UID, uid)?;

    let (track_type, codec_id, codec_private, language, name, is_default, is_forced) = match spec {
        TrackSpec::Video(v) => (
            1u64,
            v.codec_id,
            v.codec_private.as_deref(),
            None,
            v.name.as_deref(),
            true,
            false,
        ),
        TrackSpec::Audio(a) => (
            2u64,
            a.codec_id,
            a.codec_private.as_deref(),
            Some(a.language.as_str()),
            a.name.as_deref(),
            a.is_default,
            false,
        ),
        TrackSpec::Subtitle(s) => (
            17u64,
            s.codec_id,
            s.codec_private.as_deref(),
            Some(s.language.as_str()),
            s.name.as_deref(),
            s.is_default,
            s.is_forced,
        ),
    };

    written += ebml::write_uint(w, TRACK_TYPE, track_type)?;
    written += ebml::write_uint(w, FLAG_LACING, 0)?;
    written += ebml::write_string(w, CODEC_ID, codec_id)?;

    if let Some(data) = codec_private {
        written += ebml::write_binary(w, CODEC_PRIVATE, data)?;
    }

    if let Some(lang) = language {
        written += ebml::write_string(w, LANGUAGE, lang)?;
    }

    if let Some(n) = name {
        written += ebml::write_utf8(w, NAME, n)?;
    }

    if !is_default {
        written += ebml::write_uint(w, FLAG_DEFAULT, 0)?;
    }

    if is_forced {
        written += ebml::write_uint(w, FLAG_FORCED, 1)?;
    }

    // Type-specific sub-elements.
    match spec {
        TrackSpec::Video(v) => {
            if let Some(dur) = v.default_duration_ns {
                written += ebml::write_uint(w, DEFAULT_DURATION, dur)?;
            }
            written += write_video_sub(w, v)?;
        }
        TrackSpec::Audio(a) => {
            written += write_audio_sub(w, a)?;
        }
        TrackSpec::Subtitle(_) => {}
    }

    Ok(written)
}

/// Writes the Video master sub-element.
fn write_video_sub(w: &mut impl Write, v: &VideoTrack) -> io::Result<usize> {
    // Buffer to measure content size.
    let mut children: Vec<u8> = Vec::new();
    let mut child_written = 0;

    child_written += ebml::write_uint(&mut children, PIXEL_WIDTH, u64::from(v.pixel_width))?;
    child_written += ebml::write_uint(&mut children, PIXEL_HEIGHT, u64::from(v.pixel_height))?;

    if let Some(dw) = v.display_width {
        child_written += ebml::write_uint(&mut children, DISPLAY_WIDTH, u64::from(dw))?;
    }
    if let Some(dh) = v.display_height {
        child_written += ebml::write_uint(&mut children, DISPLAY_HEIGHT, u64::from(dh))?;
    }

    if let Some(interlaced) = v.interlaced {
        let flag = if interlaced { 1u64 } else { 2u64 };
        child_written += ebml::write_uint(&mut children, FLAG_INTERLACED, flag)?;
    }

    if let Some(ref colour) = v.colour {
        child_written += write_colour_sub(&mut children, colour)?;
    }

    debug_assert_eq!(
        child_written,
        children.len(),
        "video sub-element size tracking mismatch"
    );

    let mut written = ebml::write_master(w, VIDEO, children.len() as u64)?;
    w.write_all(&children)?;
    written += children.len();
    Ok(written)
}

/// Writes the Colour master sub-element.
fn write_colour_sub(w: &mut impl Write, c: &VideoColour) -> io::Result<usize> {
    let mut children: Vec<u8> = Vec::new();
    let mut child_written = 0;

    child_written += ebml::write_uint(
        &mut children,
        MATRIX_COEFF,
        u64::from(c.matrix_coefficients),
    )?;
    child_written += ebml::write_uint(
        &mut children,
        TRANSFER_CHAR,
        u64::from(c.transfer_characteristics),
    )?;
    child_written += ebml::write_uint(&mut children, PRIMARIES, u64::from(c.primaries))?;
    child_written += ebml::write_uint(&mut children, RANGE, u64::from(c.range))?;
    child_written += ebml::write_uint(&mut children, BITS_PER_CHAN, u64::from(c.bits_per_channel))?;

    debug_assert_eq!(
        child_written,
        children.len(),
        "colour sub-element size tracking mismatch"
    );

    let mut written = ebml::write_master(w, COLOUR, children.len() as u64)?;
    w.write_all(&children)?;
    written += children.len();
    Ok(written)
}

/// Writes the Audio master sub-element.
fn write_audio_sub(w: &mut impl Write, a: &AudioTrack) -> io::Result<usize> {
    let mut children: Vec<u8> = Vec::new();
    let mut child_written = 0;

    child_written += ebml::write_float(&mut children, SAMPLING_FREQ, a.sampling_frequency)?;
    child_written += ebml::write_uint(&mut children, CHANNELS, u64::from(a.channels))?;

    if let Some(depth) = a.bit_depth {
        child_written += ebml::write_uint(&mut children, BIT_DEPTH, u64::from(depth))?;
    }

    debug_assert_eq!(
        child_written,
        children.len(),
        "audio sub-element size tracking mismatch"
    );

    let mut written = ebml::write_master(w, AUDIO, children.len() as u64)?;
    w.write_all(&children)?;
    written += children.len();
    Ok(written)
}

/// Generates a track UID from the track number using [`RandomState`].
fn generate_track_uid(track_number: u32) -> u64 {
    let state = RandomState::new();
    state.hash_one(u64::from(track_number))
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
        AudioTrack, CHAPTERS, CUES, MkvMuxer, SEEK_HEAD_REGION_SIZE, SEEK_POSITION_RESERVED,
        SegmentInfo, SubtitleTrack, TAGS, TRACKS, TrackSpec, VideoColour, VideoTrack,
        write_ebml_header, write_seek_head_region, write_segment_info,
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
    // Track entries
    // -------------------------------------------------------------------

    fn make_muxer() -> MkvMuxer<Cursor<Vec<u8>>> {
        let info = SegmentInfo {
            duration_ns: None,
            title: None,
        };
        MkvMuxer::new(Cursor::new(Vec::new()), &info, false, false).expect("create muxer")
    }

    #[test]
    fn single_video_track() {
        let mut muxer = make_muxer();
        let codec_private = vec![0x01, 0x64, 0x00, 0x28]; // AVC profile

        let tracks = [TrackSpec::Video(VideoTrack {
            codec_id: "V_MPEG4/ISO/AVC",
            codec_private: Some(codec_private.clone()),
            pixel_width: 1920,
            pixel_height: 1080,
            display_width: None,
            display_height: None,
            default_duration_ns: None,
            interlaced: None,
            name: None,
            colour: None,
        })];

        let assigned = muxer.add_tracks(&tracks).expect("add_tracks");
        assert_eq!(assigned, [1], "first track should be number 1");

        let data = muxer.finalize().expect("finalize").into_inner();

        // Tracks element ID should be present.
        assert!(
            contains_bytes(&data, &TRACKS.to_be_bytes()),
            "output should contain Tracks element ID"
        );

        // CodecID string.
        assert!(
            contains_bytes(&data, b"V_MPEG4/ISO/AVC"),
            "output should contain CodecID string"
        );

        // CodecPrivate data.
        assert!(
            contains_bytes(&data, &codec_private),
            "output should contain CodecPrivate bytes"
        );

        // TrackType = 1 (video): element ID 0x83, size 0x81, value 0x01.
        assert!(
            contains_bytes(&data, &[0x83, 0x81, 0x01]),
            "output should contain TrackType = 1 (video)"
        );

        // PixelWidth = 1920 = 0x0780: ID 0xB0, size 0x82, value 0x07 0x80.
        assert!(
            contains_bytes(&data, &[0xB0, 0x82, 0x07, 0x80]),
            "output should contain PixelWidth = 1920"
        );

        // PixelHeight = 1080 = 0x0438: ID 0xBA, size 0x82, value 0x04 0x38.
        assert!(
            contains_bytes(&data, &[0xBA, 0x82, 0x04, 0x38]),
            "output should contain PixelHeight = 1080"
        );
    }

    #[test]
    fn multiple_tracks_numbering() {
        let mut muxer = make_muxer();

        let tracks = [
            TrackSpec::Video(VideoTrack {
                codec_id: "V_MPEG2",
                codec_private: None,
                pixel_width: 720,
                pixel_height: 480,
                display_width: None,
                display_height: None,
                default_duration_ns: None,
                interlaced: None,
                name: None,
                colour: None,
            }),
            TrackSpec::Audio(AudioTrack {
                codec_id: "A_AC3",
                codec_private: None,
                sampling_frequency: 48000.0,
                channels: 6,
                bit_depth: None,
                language: "eng".to_string(),
                name: Some("Surround".to_string()),
                is_default: true,
            }),
            TrackSpec::Audio(AudioTrack {
                codec_id: "A_AC3",
                codec_private: None,
                sampling_frequency: 48000.0,
                channels: 2,
                bit_depth: None,
                language: "fra".to_string(),
                name: None,
                is_default: false,
            }),
            TrackSpec::Subtitle(SubtitleTrack {
                codec_id: "S_HDMV/PGS",
                codec_private: None,
                language: "eng".to_string(),
                name: None,
                is_default: true,
                is_forced: false,
            }),
        ];

        let assigned = muxer.add_tracks(&tracks).expect("add_tracks");
        assert_eq!(
            assigned,
            [1, 2, 3, 4],
            "track numbers should be 1-based sequential"
        );

        let data = muxer.finalize().expect("finalize").into_inner();

        // Video track type.
        assert!(
            contains_bytes(&data, &[0x83, 0x81, 0x01]),
            "should contain TrackType = 1 (video)"
        );
        // Audio track type.
        assert!(
            contains_bytes(&data, &[0x83, 0x81, 0x02]),
            "should contain TrackType = 2 (audio)"
        );
        // Subtitle track type.
        assert!(
            contains_bytes(&data, &[0x83, 0x81, 0x11]),
            "should contain TrackType = 17 (subtitle)"
        );

        // Languages.
        assert!(
            contains_bytes(&data, b"eng"),
            "should contain English language"
        );
        assert!(
            contains_bytes(&data, b"fra"),
            "should contain French language"
        );

        // Track name.
        assert!(
            contains_bytes(&data, b"Surround"),
            "should contain track name"
        );

        // CodecIDs.
        assert!(
            contains_bytes(&data, b"V_MPEG2"),
            "should contain video CodecID"
        );
        assert!(
            contains_bytes(&data, b"A_AC3"),
            "should contain audio CodecID"
        );
        assert!(
            contains_bytes(&data, b"S_HDMV/PGS"),
            "should contain subtitle CodecID"
        );
    }

    #[test]
    fn audio_lpcm_with_bit_depth() {
        let mut muxer = make_muxer();

        let tracks = [TrackSpec::Audio(AudioTrack {
            codec_id: "A_PCM/INT/BIG",
            codec_private: None,
            sampling_frequency: 48000.0,
            channels: 2,
            bit_depth: Some(24),
            language: "eng".to_string(),
            name: None,
            is_default: true,
        })];

        muxer.add_tracks(&tracks).expect("add_tracks");
        let data = muxer.finalize().expect("finalize").into_inner();

        // BitDepth = 24: ID 0x62 0x64, size 0x81, value 0x18.
        assert!(
            contains_bytes(&data, &[0x62, 0x64, 0x81, 0x18]),
            "should contain BitDepth = 24"
        );
    }

    #[test]
    fn audio_without_codec_private() {
        let spec = TrackSpec::Audio(AudioTrack {
            codec_id: "A_AC3",
            codec_private: None,
            sampling_frequency: 48000.0,
            channels: 6,
            bit_depth: None,
            language: "eng".to_string(),
            name: None,
            is_default: true,
        });

        let mut buf = Vec::new();
        super::write_track_entry(&mut buf, 1, 42, &spec).expect("write_track_entry");

        // CodecPrivate ID (0x63 0xA2) should NOT be present.
        assert!(
            !contains_bytes(&buf, &[0x63, 0xA2]),
            "should not contain CodecPrivate element when None"
        );
    }

    #[test]
    fn subtitle_forced_flag() {
        let mut muxer = make_muxer();

        let tracks = [TrackSpec::Subtitle(SubtitleTrack {
            codec_id: "S_HDMV/PGS",
            codec_private: None,
            language: "eng".to_string(),
            name: None,
            is_default: false,
            is_forced: true,
        })];

        muxer.add_tracks(&tracks).expect("add_tracks");
        let data = muxer.finalize().expect("finalize").into_inner();

        // FlagForced = 1: ID 0x55 0xAA, size 0x81, value 0x01.
        assert!(
            contains_bytes(&data, &[0x55, 0xAA, 0x81, 0x01]),
            "should contain FlagForced = 1"
        );
    }

    #[test]
    fn flag_default_written_when_false() {
        let mut muxer = make_muxer();

        let tracks = [
            TrackSpec::Audio(AudioTrack {
                codec_id: "A_AC3",
                codec_private: None,
                sampling_frequency: 48000.0,
                channels: 6,
                bit_depth: None,
                language: "eng".to_string(),
                name: None,
                is_default: true,
            }),
            TrackSpec::Audio(AudioTrack {
                codec_id: "A_AC3",
                codec_private: None,
                sampling_frequency: 48000.0,
                channels: 2,
                bit_depth: None,
                language: "eng".to_string(),
                name: None,
                is_default: false,
            }),
        ];

        muxer.add_tracks(&tracks).expect("add_tracks");
        let data = muxer.finalize().expect("finalize").into_inner();

        // FlagDefault = 0: ID 0x88, size 0x80 (value 0 = empty body).
        assert!(
            contains_bytes(&data, &[0x88, 0x80]),
            "should contain FlagDefault = 0 for non-default track"
        );
    }

    #[test]
    fn video_hdr_colour() {
        let mut muxer = make_muxer();

        let tracks = [TrackSpec::Video(VideoTrack {
            codec_id: "V_MPEGH/ISO/HEVC",
            codec_private: None,
            pixel_width: 3840,
            pixel_height: 2160,
            display_width: None,
            display_height: None,
            default_duration_ns: Some(41_708_333),
            interlaced: Some(false),
            name: None,
            colour: Some(VideoColour {
                matrix_coefficients: 9,
                transfer_characteristics: 16,
                primaries: 9,
                range: 1,
                bits_per_channel: 10,
            }),
        })];

        muxer.add_tracks(&tracks).expect("add_tracks");
        let data = muxer.finalize().expect("finalize").into_inner();

        // Colour element ID: 0x55 0xB0.
        assert!(
            contains_bytes(&data, &[0x55, 0xB0]),
            "should contain Colour element"
        );

        // MatrixCoefficients = 9: ID 0x55 0xB1, size 0x81, value 0x09.
        assert!(
            contains_bytes(&data, &[0x55, 0xB1, 0x81, 0x09]),
            "should contain MatrixCoefficients = 9"
        );

        // TransferCharacteristics = 16: ID 0x55 0xBA, size 0x81, value 0x10.
        assert!(
            contains_bytes(&data, &[0x55, 0xBA, 0x81, 0x10]),
            "should contain TransferCharacteristics = 16"
        );

        // Primaries = 9: ID 0x55 0xBB, size 0x81, value 0x09.
        assert!(
            contains_bytes(&data, &[0x55, 0xBB, 0x81, 0x09]),
            "should contain Primaries = 9"
        );

        // BitsPerChannel = 10: ID 0x55 0xB2, size 0x81, value 0x0A.
        assert!(
            contains_bytes(&data, &[0x55, 0xB2, 0x81, 0x0A]),
            "should contain BitsPerChannel = 10"
        );

        // FlagInterlaced = 2 (progressive): ID 0x9A, size 0x81, value 0x02.
        assert!(
            contains_bytes(&data, &[0x9A, 0x81, 0x02]),
            "should contain FlagInterlaced = 2 (progressive)"
        );

        // DefaultDuration should be present.
        // ID 0x23 0xE3 0x83 (3-byte ID).
        assert!(
            contains_bytes(&data, &[0x23, 0xE3, 0x83]),
            "should contain DefaultDuration element"
        );
    }

    #[test]
    fn position_updated_after_add_tracks() {
        let mut muxer = make_muxer();
        let pos_before = muxer.position();

        let tracks = [TrackSpec::Video(VideoTrack {
            codec_id: "V_MPEG2",
            codec_private: None,
            pixel_width: 720,
            pixel_height: 480,
            display_width: None,
            display_height: None,
            default_duration_ns: None,
            interlaced: None,
            name: None,
            colour: None,
        })];

        muxer.add_tracks(&tracks).expect("add_tracks");
        let pos_after = muxer.position();

        assert!(
            pos_after > pos_before,
            "position should advance after add_tracks"
        );

        let data = muxer.finalize().expect("finalize").into_inner();
        assert_eq!(
            pos_after,
            data.len() as u64,
            "tracked position should equal actual output length"
        );
    }

    #[test]
    fn seekhead_backpatched_for_tracks() {
        let mut muxer = make_muxer();
        let segment_start = muxer.segment_data_start();

        let tracks = [TrackSpec::Video(VideoTrack {
            codec_id: "V_MPEG2",
            codec_private: None,
            pixel_width: 720,
            pixel_height: 480,
            display_width: None,
            display_height: None,
            default_duration_ns: None,
            interlaced: None,
            name: None,
            colour: None,
        })];

        muxer.add_tracks(&tracks).expect("add_tracks");
        let data = muxer.finalize().expect("finalize").into_inner();

        // SeekPosition for Tracks is at segment_start + 18 (same calc as
        // backpatch_overwrites_placeholder test).
        #[allow(
            clippy::cast_possible_truncation,
            reason = "test uses a Cursor<Vec<u8>> — position is always small"
        )]
        let value_offset = segment_start as usize + 18;
        let seek_pos_bytes = &data[value_offset..value_offset + 5];

        // The Tracks element should not be at offset 0 (placeholder was
        // overwritten).
        assert_ne!(
            seek_pos_bytes, [0x00; 5],
            "SeekPosition for Tracks should be backpatched (non-zero)"
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
