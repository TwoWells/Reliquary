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

// Cluster/Block elements
const CLUSTER: u32 = 0x1F43_B675;
const TIMESTAMP: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const BLOCK_GROUP: u32 = 0xA0;
const BLOCK: u32 = 0xA1;
const BLOCK_DURATION: u32 = 0x9B;
const PREV_SIZE: u32 = 0xAB;

// Chapter elements
const EDITION_ENTRY: u32 = 0x45B9;
const EDITION_UID: u32 = 0x45BC;
const EDITION_FLAG_DEFAULT: u32 = 0x45DB;
const EDITION_FLAG_HIDDEN: u32 = 0x45BD;
const CHAPTER_ATOM: u32 = 0xB6;
const CHAPTER_UID: u32 = 0x73C4;
const CHAPTER_TIME_START: u32 = 0x91;
const CHAPTER_TIME_END: u32 = 0x92;
const CHAPTER_FLAG_HIDDEN: u32 = 0x98;
const CHAPTER_FLAG_ENABLED: u32 = 0x4598;
const CHAPTER_DISPLAY: u32 = 0x80;
const CHAP_STRING: u32 = 0x85;
const CHAP_LANGUAGE: u32 = 0x437C;

// Tag elements
const TAG: u32 = 0x7373;
const TARGETS: u32 = 0x63C0;
const TARGET_TYPE_VALUE: u32 = 0x68CA;
const SIMPLE_TAG: u32 = 0x67C8;
const TAG_NAME: u32 = 0x45A3;
const TAG_STRING: u32 = 0x4487;
const TAG_LANGUAGE: u32 = 0x447A;

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
    /// Segment UID (16 bytes).  If `None`, a random UID is generated.
    /// Provide a value for reproducible (deterministic) output.
    pub segment_uid: Option<[u8; 16]>,
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

/// A frame to write into the current cluster.
pub struct Frame<'a> {
    /// Track number (1-based, from [`MkvMuxer::add_tracks`] return value).
    pub track: u32,
    /// Absolute timestamp in `TimestampScale` units (milliseconds
    /// with the default scale of 1,000,000 ns).
    pub timestamp_ms: u64,
    /// Frame data (raw codec data — length-prefixed NAL units for
    /// H.264/HEVC, syncframes for AC-3, etc.).
    pub data: &'a [u8],
    /// Whether this frame is a keyframe (random access point).
    pub keyframe: bool,
    /// Whether this frame is discardable (B-frames).
    pub discardable: bool,
    /// Duration in `TimestampScale` units.  Required for subtitle
    /// display segments.  `None` for video/audio (duration is implied
    /// by the next frame's timestamp or `DefaultDuration`).
    pub duration_ms: Option<u64>,
}

/// A chapter marker for the MKV file.
pub struct Chapter {
    /// Chapter start time in nanoseconds from segment start.
    pub start_ns: u64,
    /// Chapter end time in nanoseconds. `None` means the chapter runs
    /// until the next chapter or end of content.
    pub end_ns: Option<u64>,
    /// Display name (e.g. "Chapter 1", "Opening Credits").
    pub title: String,
    /// Language for the title (ISO 639-2, e.g. "eng").
    pub language: String,
}

/// A segment-level tag for the MKV file.
pub struct ContentTag {
    /// Tag name (e.g. "TITLE").
    pub name: String,
    /// Tag value.
    pub value: String,
}

/// Collected cue point for later Cues writing.
#[allow(dead_code, reason = "fields consumed by Cues writing in ticket 06")]
pub(crate) struct CueEntry {
    /// Timestamp in `TimestampScale` units.
    pub timestamp_ms: u64,
    /// Track number.
    pub track: u32,
    /// Absolute byte position of the cluster in the file.
    pub cluster_position: u64,
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
    /// Active cluster being written.
    current_cluster: Option<ClusterState>,
    /// Cue entries collected from keyframes, consumed by finalize.
    #[allow(dead_code, reason = "read by Cues writing in ticket 06")]
    pub(crate) cue_entries: Vec<CueEntry>,
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

/// Active cluster state tracked by the muxer.
struct ClusterState {
    /// Absolute byte position of this cluster's element ID in the file.
    start_position: u64,
    /// Base timestamp of this cluster (ms).
    base_timestamp_ms: u64,
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
            current_cluster: None,
            cue_entries: Vec::new(),
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

    /// Starts a new cluster at the given timestamp.
    ///
    /// If a cluster is already open, it is implicitly closed and its
    /// size is recorded for the `PrevSize` element in the new cluster.
    ///
    /// The caller is responsible for cluster boundary policy — typically
    /// starting a new cluster at each video keyframe:
    ///
    /// ```ignore
    /// for frame in demuxed_frames {
    ///     if frame.keyframe && frame.track == video_track {
    ///         muxer.start_cluster(frame.timestamp_ms)?;
    ///     }
    ///     muxer.write_frame(&frame)?;
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] if writing fails.
    pub fn start_cluster(&mut self, timestamp_ms: u64) -> io::Result<()> {
        // Close the previous cluster and record its size for PrevSize.
        let prev_size = self
            .current_cluster
            .take()
            .map(|c| self.position - c.start_position);

        let start_position = self.position;

        // Cluster master with unknown size.
        self.position += ebml::write_master_unknown_size(&mut self.writer, CLUSTER)? as u64;

        // Timestamp child element.
        self.position += ebml::write_uint(&mut self.writer, TIMESTAMP, timestamp_ms)? as u64;

        // PrevSize element (omitted for the first cluster).
        if let Some(size) = prev_size {
            self.position += ebml::write_uint(&mut self.writer, PREV_SIZE, size)? as u64;
        }

        self.current_cluster = Some(ClusterState {
            start_position,
            base_timestamp_ms: timestamp_ms,
        });

        Ok(())
    }

    /// Writes a frame into the current cluster.
    ///
    /// Frames without `duration_ms` are written as `SimpleBlock` elements
    /// (video and audio).  Frames with `duration_ms` are written as
    /// `BlockGroup` elements with `BlockDuration` (subtitles).
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] if no cluster is open, if the relative
    /// timestamp exceeds i16 range, or if writing fails.
    pub fn write_frame(&mut self, frame: &Frame<'_>) -> io::Result<()> {
        if frame.duration_ms.is_some() {
            return self.write_block_group(frame);
        }

        let cluster = self
            .current_cluster
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no cluster is open"))?;
        let base_timestamp_ms = cluster.base_timestamp_ms;
        let cluster_position = cluster.start_position;

        let relative = relative_timestamp(frame.timestamp_ms, base_timestamp_ms)?;

        // SimpleBlock header: track VINT + timestamp i16 + flags u8.
        let track_vint_len = vint_width(u64::from(frame.track));
        let header_len = track_vint_len + 3;
        let data_size = (header_len + frame.data.len()) as u64;

        self.position += ebml::write_element_id(&mut self.writer, SIMPLE_BLOCK)? as u64;
        self.position += ebml::write_vint(&mut self.writer, data_size, 1)? as u64;
        self.position += ebml::write_vint(&mut self.writer, u64::from(frame.track), 1)? as u64;

        self.writer.write_all(&relative.to_be_bytes())?;
        self.position += 2;

        let flags = simple_block_flags(frame.keyframe, frame.discardable);
        self.writer.write_all(&[flags])?;
        self.position += 1;

        self.writer.write_all(frame.data)?;
        self.position += frame.data.len() as u64;

        if frame.keyframe {
            self.cue_entries.push(CueEntry {
                timestamp_ms: frame.timestamp_ms,
                track: frame.track,
                cluster_position,
            });
        }

        Ok(())
    }

    /// Writes a frame as a `BlockGroup` with `BlockDuration`.
    fn write_block_group(&mut self, frame: &Frame<'_>) -> io::Result<()> {
        let cluster = self
            .current_cluster
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no cluster is open"))?;
        let base_timestamp_ms = cluster.base_timestamp_ms;
        let cluster_position = cluster.start_position;

        let relative = relative_timestamp(frame.timestamp_ms, base_timestamp_ms)?;
        let duration_ms = frame.duration_ms.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "BlockGroup requires duration_ms",
            )
        })?;

        // Block header: track VINT + timestamp i16 + flags u8.
        let track_vint_len = vint_width(u64::from(frame.track));
        let block_data_len = track_vint_len + 3 + frame.data.len();

        // Pre-compute sizes for BlockGroup master.
        let block_element_size =
            element_id_width(BLOCK) + vint_width(block_data_len as u64) + block_data_len;
        let block_duration_size = measure_uint(BLOCK_DURATION, duration_ms);
        let content_size = block_element_size + block_duration_size;

        // BlockGroup master.
        self.position +=
            ebml::write_master(&mut self.writer, BLOCK_GROUP, content_size as u64)? as u64;

        // Block element.
        self.position += ebml::write_element_id(&mut self.writer, BLOCK)? as u64;
        self.position += ebml::write_vint(&mut self.writer, block_data_len as u64, 1)? as u64;
        self.position += ebml::write_vint(&mut self.writer, u64::from(frame.track), 1)? as u64;

        self.writer.write_all(&relative.to_be_bytes())?;
        self.position += 2;

        // Block flags: bits 7 (keyframe) and 0 (discardable) are unused — always 0.
        self.writer.write_all(&[0x00])?;
        self.position += 1;

        self.writer.write_all(frame.data)?;
        self.position += frame.data.len() as u64;

        // BlockDuration.
        self.position += ebml::write_uint(&mut self.writer, BLOCK_DURATION, duration_ms)? as u64;

        if frame.keyframe {
            self.cue_entries.push(CueEntry {
                timestamp_ms: frame.timestamp_ms,
                track: frame.track,
                cluster_position,
            });
        }

        Ok(())
    }

    /// Writes chapter markers into the MKV file.
    ///
    /// Chapters appear between tracks and the first cluster.  If
    /// `chapters` is empty, nothing is written.  The `SeekHead` entry
    /// for Chapters is backpatched with the written position.
    ///
    /// The muxer must have been created with `has_chapters: true` so
    /// that a `SeekHead` placeholder exists.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] if writing or backpatching fails.
    pub fn write_chapters(&mut self, chapters: &[Chapter]) -> io::Result<()> {
        if chapters.is_empty() {
            return Ok(());
        }

        let chapters_position = self.position;

        // Buffer the edition entry to compute the Chapters content size.
        let mut edition_buf: Vec<u8> = Vec::new();
        write_edition_entry(&mut edition_buf, chapters)?;

        self.position +=
            ebml::write_master(&mut self.writer, CHAPTERS, edition_buf.len() as u64)? as u64;
        self.writer.write_all(&edition_buf)?;
        self.position += edition_buf.len() as u64;

        self.backpatch_seek_entry(CHAPTERS, chapters_position)?;

        Ok(())
    }

    /// Writes segment-level tags into the MKV file.
    ///
    /// Tags appear between tracks and the first cluster (after chapters
    /// if both are present).  If `tags` is empty, nothing is written.
    /// The `SeekHead` entry for Tags is backpatched with the written
    /// position.
    ///
    /// The muxer must have been created with `has_tags: true` so that
    /// a `SeekHead` placeholder exists.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] if writing or backpatching fails.
    pub fn write_tags(&mut self, tags: &[ContentTag]) -> io::Result<()> {
        if tags.is_empty() {
            return Ok(());
        }

        let tags_position = self.position;

        // Buffer the Tag element to compute the Tags content size.
        let mut tag_buf: Vec<u8> = Vec::new();
        write_tag_element(&mut tag_buf, tags)?;

        self.position += ebml::write_master(&mut self.writer, TAGS, tag_buf.len() as u64)? as u64;
        self.writer.write_all(&tag_buf)?;
        self.position += tag_buf.len() as u64;

        self.backpatch_seek_entry(TAGS, tags_position)?;

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
    let uid = info.segment_uid.unwrap_or_else(generate_segment_uid);

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

// ---------------------------------------------------------------------------
// Chapter and tag element writers
// ---------------------------------------------------------------------------

/// Writes one `EditionEntry` containing all chapter atoms.
fn write_edition_entry(w: &mut impl Write, chapters: &[Chapter]) -> io::Result<usize> {
    let mut children: Vec<u8> = Vec::new();
    let mut child_written = 0;

    child_written += ebml::write_uint(&mut children, EDITION_UID, 1)?;
    child_written += ebml::write_uint(&mut children, EDITION_FLAG_DEFAULT, 1)?;
    child_written += ebml::write_uint(&mut children, EDITION_FLAG_HIDDEN, 0)?;

    for (i, chapter) in chapters.iter().enumerate() {
        let uid = (i as u64) + 1;
        child_written += write_chapter_atom(&mut children, chapter, uid)?;
    }

    debug_assert_eq!(
        child_written,
        children.len(),
        "edition entry size tracking mismatch"
    );

    let mut written = ebml::write_master(w, EDITION_ENTRY, children.len() as u64)?;
    w.write_all(&children)?;
    written += children.len();
    Ok(written)
}

/// Writes one `ChapterAtom` element.
fn write_chapter_atom(w: &mut impl Write, chapter: &Chapter, uid: u64) -> io::Result<usize> {
    let mut children: Vec<u8> = Vec::new();
    let mut child_written = 0;

    child_written += ebml::write_uint(&mut children, CHAPTER_UID, uid)?;
    child_written += write_uint_min1(&mut children, CHAPTER_TIME_START, chapter.start_ns)?;
    if let Some(end_ns) = chapter.end_ns {
        child_written += write_uint_min1(&mut children, CHAPTER_TIME_END, end_ns)?;
    }
    child_written += ebml::write_uint(&mut children, CHAPTER_FLAG_HIDDEN, 0)?;
    child_written += ebml::write_uint(&mut children, CHAPTER_FLAG_ENABLED, 1)?;
    child_written += write_chapter_display(&mut children, chapter)?;

    debug_assert_eq!(
        child_written,
        children.len(),
        "chapter atom size tracking mismatch"
    );

    let mut written = ebml::write_master(w, CHAPTER_ATOM, children.len() as u64)?;
    w.write_all(&children)?;
    written += children.len();
    Ok(written)
}

/// Writes one `ChapterDisplay` element.
fn write_chapter_display(w: &mut impl Write, chapter: &Chapter) -> io::Result<usize> {
    let mut children: Vec<u8> = Vec::new();
    let mut child_written = 0;

    child_written += ebml::write_utf8(&mut children, CHAP_STRING, &chapter.title)?;
    child_written += ebml::write_string(&mut children, CHAP_LANGUAGE, &chapter.language)?;

    debug_assert_eq!(
        child_written,
        children.len(),
        "chapter display size tracking mismatch"
    );

    let mut written = ebml::write_master(w, CHAPTER_DISPLAY, children.len() as u64)?;
    w.write_all(&children)?;
    written += children.len();
    Ok(written)
}

/// Writes one `Tag` element with `Targets` and `SimpleTag` children.
fn write_tag_element(w: &mut impl Write, tags: &[ContentTag]) -> io::Result<usize> {
    let mut children: Vec<u8> = Vec::new();
    let mut child_written = 0;

    // Targets: TargetTypeValue = 50 (segment/movie level).
    let targets_content_size = measure_uint(TARGET_TYPE_VALUE, 50);
    child_written += ebml::write_master(&mut children, TARGETS, targets_content_size as u64)?;
    child_written += ebml::write_uint(&mut children, TARGET_TYPE_VALUE, 50)?;

    for tag in tags {
        child_written += write_simple_tag(&mut children, tag)?;
    }

    debug_assert_eq!(
        child_written,
        children.len(),
        "tag element size tracking mismatch"
    );

    let mut written = ebml::write_master(w, TAG, children.len() as u64)?;
    w.write_all(&children)?;
    written += children.len();
    Ok(written)
}

/// Writes one `SimpleTag` element.
fn write_simple_tag(w: &mut impl Write, tag: &ContentTag) -> io::Result<usize> {
    let mut children: Vec<u8> = Vec::new();
    let mut child_written = 0;

    child_written += ebml::write_utf8(&mut children, TAG_NAME, &tag.name)?;
    child_written += ebml::write_utf8(&mut children, TAG_STRING, &tag.value)?;
    child_written += ebml::write_string(&mut children, TAG_LANGUAGE, "und")?;

    debug_assert_eq!(
        child_written,
        children.len(),
        "simple tag size tracking mismatch"
    );

    let mut written = ebml::write_master(w, SIMPLE_TAG, children.len() as u64)?;
    w.write_all(&children)?;
    written += children.len();
    Ok(written)
}

/// Computes the relative timestamp for a block within a cluster.
///
/// Returns an i16 suitable for the `SimpleBlock` / `Block` timestamp field.
fn relative_timestamp(timestamp_ms: u64, base_timestamp_ms: u64) -> io::Result<i16> {
    // Timestamps in milliseconds are far below i64::MAX (~292 million years).
    #[allow(
        clippy::cast_possible_wrap,
        reason = "timestamps in ms are far below i64::MAX — wrap is impossible"
    )]
    let relative = timestamp_ms as i64 - base_timestamp_ms as i64;
    i16::try_from(relative).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "relative timestamp {relative} ms out of i16 range \
                 (timestamp {timestamp_ms}, cluster base {base_timestamp_ms})"
            ),
        )
    })
}

/// Builds the `SimpleBlock` flags byte.
const fn simple_block_flags(keyframe: bool, discardable: bool) -> u8 {
    let mut flags = 0u8;
    if keyframe {
        flags |= 0x80;
    }
    if discardable {
        flags |= 0x01;
    }
    flags
}

/// Generates a deterministic track UID from the track number.
///
/// Track numbers are 1-based sequential, so the resulting UIDs are
/// unique within the file and non-zero.
fn generate_track_uid(track_number: u32) -> u64 {
    u64::from(track_number)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Writes a uint element with at least 1 byte of data, even for value 0.
///
/// Standard EBML encodes uint 0 as an empty element (size = 0, no data).
/// Some Matroska parsers (including ffprobe's lavf demuxer) don't handle
/// empty uints for mandatory timestamp elements.  This writes `0x00` for
/// value 0, ensuring parser compatibility.
fn write_uint_min1(w: &mut impl Write, id: u32, value: u64) -> io::Result<usize> {
    if value == 0 {
        let mut written = ebml::write_element_id(w, id)?;
        written += ebml::write_vint(w, 1, 1)?;
        w.write_all(&[0x00])?;
        written += 1;
        Ok(written)
    } else {
        ebml::write_uint(w, id, value)
    }
}

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
        0x80..=0xFE => 1,
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
        AudioTrack, CHAPTERS, CLUSTER, CUES, Chapter, ContentTag, Frame, MkvMuxer,
        SEEK_HEAD_REGION_SIZE, SEEK_POSITION_RESERVED, SegmentInfo, SubtitleTrack, TAGS, TRACKS,
        TrackSpec, VideoColour, VideoTrack, write_ebml_header, write_seek_head_region,
        write_segment_info,
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
            segment_uid: None,
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
            segment_uid: None,
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
            segment_uid: None,
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
            segment_uid: None,
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
            segment_uid: None,
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
            segment_uid: None,
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
            segment_uid: None,
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
            segment_uid: None,
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
    // Cluster and block tests
    // -------------------------------------------------------------------

    fn make_muxer_with_video_track() -> MkvMuxer<Cursor<Vec<u8>>> {
        let mut muxer = make_muxer();
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
        muxer.add_tracks(&tracks).expect("add video track");
        muxer
    }

    fn make_muxer_with_two_tracks() -> MkvMuxer<Cursor<Vec<u8>>> {
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
                name: None,
                is_default: true,
            }),
        ];
        muxer.add_tracks(&tracks).expect("add two tracks");
        muxer
    }

    /// Returns the current muxer position as `usize` for indexing test output.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "test uses a Cursor<Vec<u8>> — position is always small"
    )]
    fn pos(muxer: &MkvMuxer<Cursor<Vec<u8>>>) -> usize {
        muxer.position() as usize
    }

    #[test]
    fn simple_block_keyframe_encoding() {
        let mut muxer = make_muxer_with_video_track();
        muxer.start_cluster(0).expect("start_cluster");
        let frame_offset = pos(&muxer);

        let data = [0xAA; 100];
        let frame = Frame {
            track: 1,
            timestamp_ms: 0,
            data: &data,
            keyframe: true,
            discardable: false,
            duration_ms: None,
        };
        muxer.write_frame(&frame).expect("write_frame");

        let output = muxer.finalize().expect("finalize").into_inner();

        // SimpleBlock ID.
        assert_eq!(output[frame_offset], 0xA3, "SimpleBlock ID");

        // Size = 1 (track VINT) + 2 (timestamp) + 1 (flags) + 100 (data) = 104.
        // VINT(104) = 0x80 | 104 = 0xE8.
        assert_eq!(
            output[frame_offset + 1],
            0xE8,
            "SimpleBlock size VINT = 104"
        );

        // Track 1 VINT = 0x81.
        assert_eq!(output[frame_offset + 2], 0x81, "track 1 VINT");

        // Relative timestamp = 0.
        assert_eq!(
            &output[frame_offset + 3..frame_offset + 5],
            [0x00, 0x00],
            "relative timestamp = 0"
        );

        // Flags = 0x80 (keyframe).
        assert_eq!(output[frame_offset + 5], 0x80, "flags = keyframe");

        // Data follows.
        assert_eq!(
            &output[frame_offset + 6..frame_offset + 106],
            &data,
            "frame data"
        );
    }

    #[test]
    fn simple_block_relative_timestamp() {
        let mut muxer = make_muxer_with_two_tracks();
        muxer.start_cluster(400).expect("start_cluster");
        let frame_offset = pos(&muxer);

        let data = [0xBB; 10];
        let frame = Frame {
            track: 2,
            timestamp_ms: 500,
            data: &data,
            keyframe: false,
            discardable: false,
            duration_ms: None,
        };
        muxer.write_frame(&frame).expect("write_frame");

        let output = muxer.finalize().expect("finalize").into_inner();

        // Track 2 VINT = 0x82.
        assert_eq!(output[frame_offset + 2], 0x82, "track 2 VINT");

        // Relative timestamp = 500 - 400 = 100 = 0x0064.
        assert_eq!(
            &output[frame_offset + 3..frame_offset + 5],
            [0x00, 0x64],
            "relative timestamp = 100"
        );

        // Flags = 0x00 (non-keyframe, non-discardable).
        assert_eq!(output[frame_offset + 5], 0x00, "flags = P-frame");
    }

    #[test]
    fn simple_block_discardable() {
        let mut muxer = make_muxer_with_video_track();
        muxer.start_cluster(0).expect("start_cluster");
        let frame_offset = pos(&muxer);

        let frame = Frame {
            track: 1,
            timestamp_ms: 40,
            data: &[0xCC; 5],
            keyframe: false,
            discardable: true,
            duration_ms: None,
        };
        muxer.write_frame(&frame).expect("write_frame");

        let output = muxer.finalize().expect("finalize").into_inner();

        // Flags = 0x01 (discardable B-frame).
        assert_eq!(output[frame_offset + 5], 0x01, "flags = discardable");
    }

    #[test]
    fn simple_block_negative_relative_timestamp() {
        let mut muxer = make_muxer_with_video_track();
        muxer.start_cluster(1000).expect("start_cluster");
        let frame_offset = pos(&muxer);

        let frame = Frame {
            track: 1,
            timestamp_ms: 980,
            data: &[0xDD; 5],
            keyframe: false,
            discardable: true,
            duration_ms: None,
        };
        muxer.write_frame(&frame).expect("write_frame");

        let output = muxer.finalize().expect("finalize").into_inner();

        // Relative = 980 - 1000 = -20.  i16 big-endian = 0xFFEC.
        assert_eq!(
            &output[frame_offset + 3..frame_offset + 5],
            [0xFF, 0xEC],
            "negative relative timestamp = -20"
        );
    }

    #[test]
    fn block_group_subtitle_with_duration() {
        let mut muxer = make_muxer();
        let tracks = [TrackSpec::Subtitle(SubtitleTrack {
            codec_id: "S_HDMV/PGS",
            codec_private: None,
            language: "eng".to_string(),
            name: None,
            is_default: true,
            is_forced: false,
        })];
        muxer.add_tracks(&tracks).expect("add subtitle track");

        muxer.start_cluster(0).expect("start_cluster");
        let frame_offset = pos(&muxer);

        let data = [0xEE; 20];
        let frame = Frame {
            track: 1,
            timestamp_ms: 0,
            data: &data,
            keyframe: true,
            discardable: false,
            duration_ms: Some(5000),
        };
        muxer.write_frame(&frame).expect("write_frame");

        let output = muxer.finalize().expect("finalize").into_inner();

        // BlockGroup ID = 0xA0.
        assert_eq!(output[frame_offset], 0xA0, "BlockGroup ID");

        // Inside the BlockGroup, find Block element (0xA1).
        let bg_content_start = frame_offset + 1 + 1; // ID + 1-byte size VINT
        assert_eq!(output[bg_content_start], 0xA1, "Block ID");

        // Block flags byte should be 0x00 (bits 7 and 0 unused in Block).
        // Block header is after Block ID + size VINT + track VINT + timestamp.
        let block_header_offset = bg_content_start + 1 + 1 + 1 + 2; // ID + size + track + ts
        assert_eq!(
            output[block_header_offset], 0x00,
            "Block flags = 0x00 (no keyframe/discardable bits)"
        );

        // BlockDuration = 5000: ID 0x9B, then the value.
        // The BlockDuration element should follow the Block element.
        let block_end = bg_content_start + 1 + 1 + 1 + 2 + 1 + data.len(); // full Block element
        assert_eq!(output[block_end], 0x9B, "BlockDuration ID");

        // Value 5000 = 0x1388, uint_byte_len = 2.
        // Size VINT = 0x82, value = 0x13 0x88.
        assert_eq!(output[block_end + 1], 0x82, "BlockDuration size = 2");
        assert_eq!(
            &output[block_end + 2..block_end + 4],
            [0x13, 0x88],
            "BlockDuration value = 5000"
        );
    }

    #[test]
    fn cluster_structure_with_prevsize() {
        let mut muxer = make_muxer_with_video_track();

        // First cluster at timestamp 0.
        let cluster1_offset = pos(&muxer);
        muxer.start_cluster(0).expect("start_cluster 0");

        let frame1 = Frame {
            track: 1,
            timestamp_ms: 0,
            data: &[0x11; 50],
            keyframe: true,
            discardable: false,
            duration_ms: None,
        };
        muxer.write_frame(&frame1).expect("write frame 1");

        let frame2 = Frame {
            track: 1,
            timestamp_ms: 40,
            data: &[0x22; 50],
            keyframe: false,
            discardable: false,
            duration_ms: None,
        };
        muxer.write_frame(&frame2).expect("write frame 2");

        let frame3 = Frame {
            track: 1,
            timestamp_ms: 80,
            data: &[0x33; 50],
            keyframe: false,
            discardable: false,
            duration_ms: None,
        };
        muxer.write_frame(&frame3).expect("write frame 3");

        // Second cluster at timestamp 1000.
        let cluster2_offset = pos(&muxer);
        muxer.start_cluster(1000).expect("start_cluster 1000");

        let output = muxer.finalize().expect("finalize").into_inner();
        let cluster_id_bytes = CLUSTER.to_be_bytes();

        // First cluster starts at cluster1_offset.
        assert_eq!(
            &output[cluster1_offset..cluster1_offset + 4],
            cluster_id_bytes,
            "first Cluster ID"
        );

        // Second cluster starts at cluster2_offset.
        assert_eq!(
            &output[cluster2_offset..cluster2_offset + 4],
            cluster_id_bytes,
            "second Cluster ID"
        );

        // Second cluster has a Timestamp element, then PrevSize.
        // Cluster ID (4) + unknown size (8) = 12 bytes for header.
        let ts_offset = cluster2_offset + 12;
        assert_eq!(output[ts_offset], 0xE7, "second cluster Timestamp ID");

        // PrevSize should be present after Timestamp.
        // Timestamp for value 1000 (0x03E8): ID(1) + size(1) + value(2) = 4 bytes.
        let prevsize_offset = ts_offset + 4;
        assert_eq!(output[prevsize_offset], 0xAB, "PrevSize ID");

        // PrevSize value = cluster2_offset - cluster1_offset (size of first cluster).
        let expected_size = (cluster2_offset - cluster1_offset) as u64;
        let ps_size_vint = output[prevsize_offset + 1];
        let ps_data_len = (ps_size_vint & 0x7F) as usize;
        let ps_value_bytes = &output[prevsize_offset + 2..prevsize_offset + 2 + ps_data_len];
        let mut ps_value = 0u64;
        for &b in ps_value_bytes {
            ps_value = (ps_value << 8) | u64::from(b);
        }
        assert_eq!(
            ps_value, expected_size,
            "PrevSize value matches first cluster size"
        );
    }

    #[test]
    fn cue_collection() {
        let mut muxer = make_muxer_with_video_track();

        let cluster1_pos = muxer.position();
        muxer.start_cluster(0).expect("start_cluster 0");

        // Keyframe at 0.
        muxer
            .write_frame(&Frame {
                track: 1,
                timestamp_ms: 0,
                data: &[0x00; 10],
                keyframe: true,
                discardable: false,
                duration_ms: None,
            })
            .expect("keyframe 0");

        // Non-keyframes.
        for i in 1..=8 {
            muxer
                .write_frame(&Frame {
                    track: 1,
                    timestamp_ms: i * 40,
                    data: &[0x00; 10],
                    keyframe: false,
                    discardable: false,
                    duration_ms: None,
                })
                .expect("non-keyframe");
        }

        // Second keyframe at 5000 in a new cluster.
        let cluster2_start = muxer.position();
        muxer.start_cluster(5000).expect("start_cluster 5000");

        muxer
            .write_frame(&Frame {
                track: 1,
                timestamp_ms: 5000,
                data: &[0x00; 10],
                keyframe: true,
                discardable: false,
                duration_ms: None,
            })
            .expect("keyframe 5000");

        let cues = &muxer.cue_entries;
        assert_eq!(cues.len(), 2, "should have 2 cue entries");
        assert_eq!(cues[0].timestamp_ms, 0, "first cue timestamp");
        assert_eq!(cues[0].track, 1, "first cue track");
        assert_eq!(
            cues[0].cluster_position, cluster1_pos,
            "first cue cluster position"
        );
        assert_eq!(cues[1].timestamp_ms, 5000, "second cue timestamp");
        assert_eq!(cues[1].track, 1, "second cue track");
        assert_eq!(
            cues[1].cluster_position, cluster2_start,
            "second cue cluster position"
        );
    }

    #[test]
    fn write_frame_without_cluster_fails() {
        let mut muxer = make_muxer_with_video_track();
        let err = muxer
            .write_frame(&Frame {
                track: 1,
                timestamp_ms: 0,
                data: &[0x00],
                keyframe: true,
                discardable: false,
                duration_ms: None,
            })
            .expect_err("should fail without cluster");
        assert_eq!(
            err.kind(),
            io::ErrorKind::InvalidInput,
            "error kind for missing cluster"
        );
    }

    #[test]
    fn position_tracks_cluster_writes() {
        let mut muxer = make_muxer_with_video_track();
        muxer.start_cluster(0).expect("start_cluster");

        muxer
            .write_frame(&Frame {
                track: 1,
                timestamp_ms: 0,
                data: &[0xAA; 100],
                keyframe: true,
                discardable: false,
                duration_ms: None,
            })
            .expect("write_frame");

        let pos = muxer.position();
        let data = muxer.finalize().expect("finalize").into_inner();
        assert_eq!(
            pos,
            data.len() as u64,
            "tracked position equals actual output length after cluster writes"
        );
    }

    #[test]
    fn playable_output_structure() {
        let mut muxer = make_muxer_with_two_tracks();

        // Write two clusters with synthetic data.
        muxer.start_cluster(0).expect("start_cluster 0");
        muxer
            .write_frame(&Frame {
                track: 1,
                timestamp_ms: 0,
                data: &[0x00; 1000],
                keyframe: true,
                discardable: false,
                duration_ms: None,
            })
            .expect("video keyframe");
        muxer
            .write_frame(&Frame {
                track: 2,
                timestamp_ms: 0,
                data: &[0x00; 256],
                keyframe: true,
                discardable: false,
                duration_ms: None,
            })
            .expect("audio frame");

        muxer.start_cluster(1000).expect("start_cluster 1000");
        muxer
            .write_frame(&Frame {
                track: 1,
                timestamp_ms: 1000,
                data: &[0x00; 1000],
                keyframe: true,
                discardable: false,
                duration_ms: None,
            })
            .expect("video keyframe 2");

        let data = muxer.finalize().expect("finalize").into_inner();

        // File starts with EBML header.
        assert_eq!(
            &data[..4],
            [0x1A, 0x45, 0xDF, 0xA3],
            "file starts with EBML ID"
        );

        // Contains two Cluster elements.
        let cluster_id = CLUSTER.to_be_bytes();
        let cluster_count = data.windows(4).filter(|w| *w == cluster_id).count();
        assert_eq!(cluster_count, 2, "output contains 2 clusters");
    }

    // -------------------------------------------------------------------
    // Chapter and tag tests
    // -------------------------------------------------------------------

    #[test]
    fn chapters_two_basic() {
        let info = SegmentInfo {
            duration_ns: None,
            title: None,
            segment_uid: None,
        };
        let mut muxer =
            MkvMuxer::new(Cursor::new(Vec::new()), &info, true, false).expect("create muxer");

        let chapters = [
            Chapter {
                start_ns: 0,
                end_ns: None,
                title: "Chapter 1".to_string(),
                language: "eng".to_string(),
            },
            Chapter {
                start_ns: 60_000_000_000,
                end_ns: None,
                title: "Chapter 2".to_string(),
                language: "eng".to_string(),
            },
        ];

        muxer.write_chapters(&chapters).expect("write_chapters");
        let data = muxer.finalize().expect("finalize").into_inner();

        // Chapters element ID: 0x10 0x43 0xA7 0x70.
        assert!(
            contains_bytes(&data, &[0x10, 0x43, 0xA7, 0x70]),
            "output should contain Chapters element ID"
        );

        // EditionEntry element ID: 0x45 0xB9.
        assert!(
            contains_bytes(&data, &[0x45, 0xB9]),
            "output should contain EditionEntry element ID"
        );

        // ChapterAtom element ID: 0xB6 — should appear twice.
        let atom_count = data.windows(1).filter(|w| w == &[0xB6]).count();
        assert!(atom_count >= 2, "should contain at least 2 ChapterAtom IDs");

        // Chapter titles.
        assert!(
            contains_bytes(&data, b"Chapter 1"),
            "should contain first chapter title"
        );
        assert!(
            contains_bytes(&data, b"Chapter 2"),
            "should contain second chapter title"
        );

        // ChapLanguage = "eng".
        assert!(
            contains_bytes(&data, b"eng"),
            "should contain chapter language"
        );

        // ChapterTimeStart for second chapter = 60_000_000_000 = 0x0D_F847_5800.
        // As a 5-byte uint: [0x0D, 0xF8, 0x47, 0x58, 0x00].
        assert!(
            contains_bytes(&data, &[0x0D, 0xF8, 0x47, 0x58, 0x00]),
            "should contain second chapter timestamp (60 billion ns)"
        );
    }

    #[test]
    fn chapter_with_end_time() {
        let info = SegmentInfo {
            duration_ns: None,
            title: None,
            segment_uid: None,
        };
        let mut muxer =
            MkvMuxer::new(Cursor::new(Vec::new()), &info, true, false).expect("create muxer");

        let chapters = [Chapter {
            start_ns: 0,
            end_ns: Some(30_000_000_000),
            title: "Intro".to_string(),
            language: "eng".to_string(),
        }];

        muxer.write_chapters(&chapters).expect("write_chapters");
        let data = muxer.finalize().expect("finalize").into_inner();

        // ChapterTimeEnd element ID: 0x92.
        assert!(
            contains_bytes(&data, &[0x92]),
            "should contain ChapterTimeEnd element ID"
        );

        // ChapterTimeEnd = 30_000_000_000 = 0x06_FC23_AC00.
        // As a 5-byte uint: [0x06, 0xFC, 0x23, 0xAC, 0x00].
        assert!(
            contains_bytes(&data, &[0x06, 0xFC, 0x23, 0xAC, 0x00]),
            "should contain ChapterTimeEnd value (30 billion ns)"
        );
    }

    #[test]
    fn chapters_empty_writes_nothing() {
        let info = SegmentInfo {
            duration_ns: None,
            title: None,
            segment_uid: None,
        };
        let mut muxer =
            MkvMuxer::new(Cursor::new(Vec::new()), &info, false, false).expect("create muxer");

        let pos_before = muxer.position();
        muxer.write_chapters(&[]).expect("write empty chapters");
        let pos_after = muxer.position();

        assert_eq!(
            pos_before, pos_after,
            "position should not change for empty chapters"
        );
    }

    #[test]
    fn tags_single_title() {
        let info = SegmentInfo {
            duration_ns: None,
            title: None,
            segment_uid: None,
        };
        let mut muxer =
            MkvMuxer::new(Cursor::new(Vec::new()), &info, false, true).expect("create muxer");

        let tags = [ContentTag {
            name: "TITLE".to_string(),
            value: "Test Movie".to_string(),
        }];

        muxer.write_tags(&tags).expect("write_tags");
        let data = muxer.finalize().expect("finalize").into_inner();

        // Tags element ID: 0x12 0x54 0xC3 0x67.
        assert!(
            contains_bytes(&data, &[0x12, 0x54, 0xC3, 0x67]),
            "output should contain Tags element ID"
        );

        // Tag element ID: 0x73 0x73.
        assert!(
            contains_bytes(&data, &[0x73, 0x73]),
            "output should contain Tag element ID"
        );

        // Targets element ID: 0x63 0xC0.
        assert!(
            contains_bytes(&data, &[0x63, 0xC0]),
            "output should contain Targets element ID"
        );

        // TargetTypeValue = 50: ID 0x68 0xCA, size 0x81, value 0x32.
        assert!(
            contains_bytes(&data, &[0x68, 0xCA, 0x81, 0x32]),
            "should contain TargetTypeValue = 50"
        );

        // SimpleTag element ID: 0x67 0xC8.
        assert!(
            contains_bytes(&data, &[0x67, 0xC8]),
            "output should contain SimpleTag element ID"
        );

        // TagName and TagString values.
        assert!(contains_bytes(&data, b"TITLE"), "should contain tag name");
        assert!(
            contains_bytes(&data, b"Test Movie"),
            "should contain tag value"
        );

        // TagLanguage = "und".
        assert!(contains_bytes(&data, b"und"), "should contain tag language");
    }

    #[test]
    fn tags_multiple() {
        let info = SegmentInfo {
            duration_ns: None,
            title: None,
            segment_uid: None,
        };
        let mut muxer =
            MkvMuxer::new(Cursor::new(Vec::new()), &info, false, true).expect("create muxer");

        let tags = [
            ContentTag {
                name: "TITLE".to_string(),
                value: "Test Movie".to_string(),
            },
            ContentTag {
                name: "DESCRIPTION".to_string(),
                value: "A test description".to_string(),
            },
        ];

        muxer.write_tags(&tags).expect("write_tags");
        let data = muxer.finalize().expect("finalize").into_inner();

        // Two SimpleTag elements: ID 0x67 0xC8 should appear twice.
        let simple_tag_id = [0x67, 0xC8];
        let count = data.windows(2).filter(|w| *w == simple_tag_id).count();
        assert_eq!(count, 2, "should contain 2 SimpleTag elements");

        assert!(
            contains_bytes(&data, b"TITLE"),
            "should contain first tag name"
        );
        assert!(
            contains_bytes(&data, b"DESCRIPTION"),
            "should contain second tag name"
        );
        assert!(
            contains_bytes(&data, b"A test description"),
            "should contain second tag value"
        );
    }

    #[test]
    fn tags_empty_writes_nothing() {
        let info = SegmentInfo {
            duration_ns: None,
            title: None,
            segment_uid: None,
        };
        let mut muxer =
            MkvMuxer::new(Cursor::new(Vec::new()), &info, false, false).expect("create muxer");

        let pos_before = muxer.position();
        muxer.write_tags(&[]).expect("write empty tags");
        let pos_after = muxer.position();

        assert_eq!(
            pos_before, pos_after,
            "position should not change for empty tags"
        );
    }

    #[test]
    fn seekhead_backpatched_for_chapters_and_tags() {
        let info = SegmentInfo {
            duration_ns: None,
            title: None,
            segment_uid: None,
        };
        let mut muxer =
            MkvMuxer::new(Cursor::new(Vec::new()), &info, true, true).expect("create muxer");
        let segment_start = muxer.segment_data_start();

        let chapters = [Chapter {
            start_ns: 0,
            end_ns: None,
            title: "Chapter 1".to_string(),
            language: "eng".to_string(),
        }];
        muxer.write_chapters(&chapters).expect("write_chapters");

        let tags = [ContentTag {
            name: "TITLE".to_string(),
            value: "Test".to_string(),
        }];
        muxer.write_tags(&tags).expect("write_tags");

        let data = muxer.finalize().expect("finalize").into_inner();

        // With 4 SeekHead entries (Tracks, Cues, Chapters, Tags):
        // Each entry is 18 bytes. Chapters is the 3rd entry (index 2).
        // SeekHead header = 5 bytes (4-byte ID + 1-byte size VINT).
        // Entry offset = 5 + entry_index * 18.
        // SeekPosition value bytes within an entry: at offset 13 (3 + 7 + 3).
        // Chapters SeekPosition absolute offset = segment_start + 5 + 2*18 + 13
        //                                      = segment_start + 54.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "test uses a Cursor<Vec<u8>> — position is always small"
        )]
        let chapters_seek_offset = segment_start as usize + 54;
        let chapters_seek_bytes = &data[chapters_seek_offset..chapters_seek_offset + 5];
        assert_ne!(
            chapters_seek_bytes, [0x00; 5],
            "SeekPosition for Chapters should be backpatched (non-zero)"
        );

        // Tags is the 4th entry (index 3).
        // Tags SeekPosition absolute offset = segment_start + 5 + 3*18 + 13
        //                                   = segment_start + 72.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "test uses a Cursor<Vec<u8>> — position is always small"
        )]
        let tags_seek_offset = segment_start as usize + 72;
        let tags_seek_bytes = &data[tags_seek_offset..tags_seek_offset + 5];
        assert_ne!(
            tags_seek_bytes, [0x00; 5],
            "SeekPosition for Tags should be backpatched (non-zero)"
        );
    }

    #[test]
    fn chapters_and_tags_integration() {
        let info = SegmentInfo {
            duration_ns: Some(120_000_000_000),
            title: Some("Integration Test".to_string()),
            segment_uid: None,
        };
        let mut muxer =
            MkvMuxer::new(Cursor::new(Vec::new()), &info, true, true).expect("create muxer");

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

        let chapters = [
            Chapter {
                start_ns: 0,
                end_ns: None,
                title: "Opening".to_string(),
                language: "eng".to_string(),
            },
            Chapter {
                start_ns: 30_000_000_000,
                end_ns: None,
                title: "Main Feature".to_string(),
                language: "eng".to_string(),
            },
        ];
        muxer.write_chapters(&chapters).expect("write_chapters");

        let tags = [ContentTag {
            name: "TITLE".to_string(),
            value: "Behind the Scenes - Themyscira".to_string(),
        }];
        muxer.write_tags(&tags).expect("write_tags");

        let pos = muxer.position();
        let data = muxer.finalize().expect("finalize").into_inner();

        assert_eq!(
            pos,
            data.len() as u64,
            "tracked position equals actual output length"
        );

        // EBML header.
        assert_eq!(
            &data[..4],
            [0x1A, 0x45, 0xDF, 0xA3],
            "file starts with EBML ID"
        );

        // All major elements present.
        assert!(
            contains_bytes(&data, &TRACKS.to_be_bytes()),
            "output should contain Tracks element"
        );
        assert!(
            contains_bytes(&data, &CHAPTERS.to_be_bytes()),
            "output should contain Chapters element"
        );
        assert!(
            contains_bytes(&data, &TAGS.to_be_bytes()),
            "output should contain Tags element"
        );

        // Content from each section.
        assert!(
            contains_bytes(&data, b"V_MPEG2"),
            "should contain video codec"
        );
        assert!(
            contains_bytes(&data, b"Opening"),
            "should contain first chapter title"
        );
        assert!(
            contains_bytes(&data, b"Main Feature"),
            "should contain second chapter title"
        );
        assert!(
            contains_bytes(&data, b"Behind the Scenes - Themyscira"),
            "should contain content title tag"
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
