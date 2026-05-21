// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! MPLS binary parser — converts raw bytes into parse structs.
//!
//! Reference: `reference/MPLS.md` in the planning repository.
//! All multi-byte integers are big-endian. Timestamps are 45 kHz PTS ticks.

use thiserror::Error;

use super::cursor::{Cursor, CursorError};

/// Clock frequency for MPEG-2 PTS timestamps (ticks per second).
pub const PTS_CLOCK_HZ: u32 = 45_000;

// ── Errors ──────────────────────────────────────────────────────────────

/// Errors that can occur while parsing an MPLS file.
#[derive(Debug, Error)]
pub enum MplsError {
    /// The file is too short to contain the expected data.
    #[error("unexpected end of data at offset {offset} (need {needed} bytes, have {available})")]
    UnexpectedEof {
        /// Byte offset where the read was attempted.
        offset: usize,
        /// Number of bytes requested.
        needed: usize,
        /// Number of bytes actually available from that offset.
        available: usize,
    },

    /// The file does not start with the `MPLS` magic bytes.
    #[error("invalid magic: expected \"MPLS\", got {found:?}")]
    InvalidMagic {
        /// The four bytes found at the start of the file.
        found: [u8; 4],
    },

    /// The version string is not a recognised MPLS version.
    #[error("unsupported version: {version:?}")]
    UnsupportedVersion {
        /// The four-byte ASCII version string.
        version: [u8; 4],
    },
}

impl From<CursorError> for MplsError {
    fn from(e: CursorError) -> Self {
        Self::UnexpectedEof {
            offset: e.offset,
            needed: e.needed,
            available: e.available,
        }
    }
}

// ── Parse structs ───────────────────────────────────────────────────────

/// A parsed MPLS playlist.
#[derive(Debug, Clone)]
pub struct Playlist {
    /// Playlist number derived from the filename (e.g. `00200.mpls` → 200).
    pub number: u32,
    /// Ordered list of play items in this playlist.
    pub play_items: Vec<PlayItem>,
    /// Chapter marks referencing play items by index.
    pub marks: Vec<Mark>,
    /// Clip IDs referenced by sub-paths (e.g. IG overlay clips).
    ///
    /// Sub-paths associate secondary clips (interactive graphics, pip
    /// video) with the main play items. Only the clip IDs are extracted;
    /// timing and sync details are not parsed.
    pub sub_path_clip_ids: Vec<String>,
}

/// A single play item referencing an m2ts clip with timing information.
#[derive(Debug, Clone)]
pub struct PlayItem {
    /// Clip identifier (e.g. `"00299"` → `00299.m2ts`).
    pub clip_id: String,
    /// Start timestamp in 45 kHz PTS ticks.
    pub in_time: u32,
    /// End timestamp in 45 kHz PTS ticks.
    pub out_time: u32,
    /// Connection condition: 1 = non-seamless, 5/6 = seamless.
    pub connection_condition: u8,
    /// Whether this play item has multiple angles.
    pub is_multi_angle: bool,
    /// Alternate angle clip identifiers (empty when `is_multi_angle` is false).
    pub angle_clip_ids: Vec<String>,
    /// Stream number table describing video, audio, and subtitle streams.
    pub streams: StnTable,
}

/// A chapter mark referencing a play item.
#[derive(Debug, Clone)]
pub struct Mark {
    /// Mark type (1 = entry/chapter mark, 2 = link point).
    pub mark_type: u8,
    /// Index into the playlist's `play_items` array.
    pub play_item_ref: u16,
    /// Timestamp in 45 kHz PTS ticks (absolute PTS within the clip).
    pub timestamp: u32,
}

/// Stream number table — lists all streams in a play item.
#[derive(Debug, Clone)]
pub struct StnTable {
    /// Video streams.
    pub video: Vec<VideoStream>,
    /// Audio streams.
    pub audio: Vec<AudioStream>,
    /// PGS subtitle streams.
    pub subtitles: Vec<SubtitleStream>,
}

/// A video stream entry from the STN table.
#[derive(Debug, Clone)]
pub struct VideoStream {
    /// Codec type (e.g. 0x1b = H.264, 0x24 = HEVC).
    pub coding_type: u8,
    /// Video format (resolution indicator).
    pub video_format: u8,
    /// Frame rate indicator.
    pub frame_rate: u8,
}

/// An audio stream entry from the STN table.
#[derive(Debug, Clone)]
pub struct AudioStream {
    /// Codec type (e.g. 0x81 = AC-3, 0x86 = DTS-HD MA).
    pub coding_type: u8,
    /// Audio format (channel layout indicator).
    pub audio_format: u8,
    /// Sample rate indicator.
    pub sample_rate: u8,
    /// Three-letter ISO 639-2 language code.
    pub language: String,
}

/// A PGS subtitle stream entry from the STN table.
#[derive(Debug, Clone)]
pub struct SubtitleStream {
    /// Codec type (0x90 = PGS, 0x92 = text subtitle).
    pub coding_type: u8,
    /// Three-letter ISO 639-2 language code.
    pub language: String,
}

// ── Display helpers ─────────────────────────────────────────────────────

/// Returns a human-readable codec name for a video coding type.
#[must_use]
pub const fn video_codec_name(coding_type: u8) -> &'static str {
    match coding_type {
        0x01 => "MPEG-1",
        0x02 => "MPEG-2",
        0xea => "VC-1",
        0x1b => "H.264",
        0x24 => "HEVC",
        _ => "Unknown",
    }
}

/// Returns a human-readable resolution for a video format value.
#[must_use]
pub const fn video_resolution(video_format: u8) -> &'static str {
    match video_format {
        1 => "480i",
        2 => "576i",
        3 => "480p",
        4 => "1080i",
        5 => "720p",
        6 => "1080p",
        7 => "576p",
        8 => "2160p",
        _ => "?",
    }
}

/// Returns a human-readable frame rate string.
#[must_use]
pub const fn video_frame_rate(frame_rate: u8) -> &'static str {
    match frame_rate {
        1 => "23.976",
        2 => "24",
        3 => "25",
        4 => "29.97",
        6 => "50",
        7 => "59.94",
        _ => "?",
    }
}

/// Returns a human-readable codec name for an audio coding type.
#[must_use]
pub const fn audio_codec_name(coding_type: u8) -> &'static str {
    match coding_type {
        0x03 => "MPEG-1",
        0x04 => "MPEG-2",
        0x80 => "LPCM",
        0x81 => "AC-3",
        0x82 => "DTS",
        0x83 => "TrueHD",
        0x84 => "E-AC-3",
        0x85 => "DTS-HD HR",
        0x86 => "DTS-HD MA",
        0xa1 => "E-AC-3 2nd",
        0xa2 => "DTS-HD 2nd",
        _ => "Unknown",
    }
}

/// Returns a human-readable channel layout for an audio format value.
#[must_use]
pub const fn audio_channels(audio_format: u8) -> &'static str {
    match audio_format {
        1 => "1.0",
        3 => "2.0",
        6 => "5.1+",
        12 => "combo",
        _ => "?",
    }
}

/// Returns a human-readable sample rate string.
#[must_use]
pub const fn audio_sample_rate(sample_rate: u8) -> &'static str {
    match sample_rate {
        1 => "48kHz",
        4 => "96kHz",
        5 => "192kHz",
        12 => "48/192kHz",
        14 => "48/96kHz",
        _ => "?",
    }
}

// ── Parsing ─────────────────────────────────────────────────────────────

/// Parses an MPLS file from raw bytes.
///
/// `number` is the playlist number derived from the filename
/// (e.g. `00200.mpls` → 200).
///
/// # Errors
///
/// Returns [`MplsError`] if the file is malformed or truncated.
pub fn parse(data: &[u8], number: u32) -> Result<Playlist, MplsError> {
    let mut r = Cursor::new(data);

    // ── Header ──────────────────────────────────────────────────────
    let magic = r.read_bytes(4)?;
    if magic != b"MPLS" {
        let mut found = [0u8; 4];
        found.copy_from_slice(magic);
        return Err(MplsError::InvalidMagic { found });
    }

    let version_bytes = r.read_bytes(4)?;
    match version_bytes {
        b"0100" | b"0200" | b"0300" => {}
        _ => {
            let mut version = [0u8; 4];
            version.copy_from_slice(version_bytes);
            return Err(MplsError::UnsupportedVersion { version });
        }
    }

    let playlist_offset = r.read_u32()? as usize;
    let mark_offset = r.read_u32()? as usize;
    // extension_offset (u32) + reserved (20 bytes) — skip
    // We don't need them for Phase 1.

    // ── PlayList section ────────────────────────────────────────────
    r.seek(playlist_offset)?;
    let _section_length = r.read_u32()?;
    let _reserved = r.read_u16()?;
    let num_play_items = r.read_u16()?;
    let num_sub_paths = r.read_u16()?;

    // Compute expected cursor position after all play items by walking
    // the raw length fields. This lets us recover if the play item
    // parser leaves the cursor misaligned (e.g. from STN table edge
    // cases with many streams).
    let play_items_start = r.pos;
    let expected_after_items = {
        let mut pos = play_items_start;
        for _ in 0..num_play_items {
            if pos + 2 > data.len() {
                break;
            }
            let item_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2 + item_len;
        }
        pos
    };

    let mut play_items = Vec::with_capacity(num_play_items as usize);
    for _ in 0..num_play_items {
        play_items.push(parse_play_item(&mut r)?);
    }

    // Recover cursor alignment if the parser drifted
    if r.pos != expected_after_items && expected_after_items <= data.len() {
        r.pos = expected_after_items;
    }

    let mut sub_path_clip_ids = Vec::new();
    for _ in 0..num_sub_paths {
        if let Ok(clips) = parse_sub_path_clip_ids(&mut r) {
            sub_path_clip_ids.extend(clips);
        } else {
            break;
        }
    }

    // ── PlayListMark section ────────────────────────────────────────
    r.seek(mark_offset)?;
    let _mark_section_length = r.read_u32()?;
    let num_marks = r.read_u16()?;

    let mut marks = Vec::with_capacity(num_marks as usize);
    for _ in 0..num_marks {
        marks.push(parse_mark(&mut r)?);
    }

    Ok(Playlist {
        number,
        play_items,
        marks,
        sub_path_clip_ids,
    })
}

/// Parses a single `PlayItem` entry.
fn parse_play_item(r: &mut Cursor<'_>) -> Result<PlayItem, MplsError> {
    let item_length = r.read_u16()? as usize;
    let item_start = r.pos;

    // clip_id: 5 ASCII bytes
    let clip_id_bytes = r.read_bytes(5)?;
    let clip_id = String::from_utf8_lossy(clip_id_bytes).into_owned();

    // codec_id: 4 ASCII bytes (e.g. "M2TS") — skip
    r.skip(4)?;

    // flags: 11 reserved + 1 is_multi_angle + 4 connection_condition
    let flags = r.read_u16()?;
    let is_multi_angle = (flags >> 4) & 1 == 1;
    let connection_condition = (flags & 0x0F) as u8;

    // stc_id — skip
    r.skip(1)?;

    let in_time = r.read_u32()?;
    let out_time = r.read_u32()?;

    // UO_mask (8 bytes) + flags2 (1 byte) + still_mode (1 byte) + still_time (2 bytes)
    r.skip(12)?;

    // Multi-angle data (before STN table)
    let mut angle_clip_ids = Vec::new();
    if is_multi_angle {
        let angle_count = r.read_u8()?;
        // 6 reserved bits + 1 is_different_audio + 1 is_seamless_angle
        r.skip(1)?;
        // Each additional angle: clip_id(5) + codec_id(4) + stc_id(1) = 10 bytes
        for _ in 1..angle_count {
            let angle_clip = r.read_bytes(5)?;
            angle_clip_ids.push(String::from_utf8_lossy(angle_clip).into_owned());
            // codec_id (4) + stc_id (1)
            r.skip(5)?;
        }
    }

    let streams = parse_stn_table(r)?;

    // Skip any remaining bytes in this play item
    let consumed = r.pos - item_start;
    if consumed < item_length {
        r.skip(item_length - consumed)?;
    }

    Ok(PlayItem {
        clip_id,
        in_time,
        out_time,
        connection_condition,
        is_multi_angle,
        angle_clip_ids,
        streams,
    })
}

/// Extracts clip IDs from a single sub-path entry.
///
/// Each sub-path contains one or more `SubPlayItem` entries, each
/// referencing a clip. Only the clip IDs are extracted — timing,
/// sync, and type information are skipped.
///
/// Reference: BD spec `SubPath` structure.
fn parse_sub_path_clip_ids(r: &mut Cursor<'_>) -> Result<Vec<String>, MplsError> {
    let sub_path_length = r.read_u32()? as usize;
    let sub_path_start = r.pos;

    // padding (u8) + sub_path_type (u8) + reserved/is_repeat (u16)
    // + reserved (u8) + num_sub_play_items (u8) = 6 bytes header
    if sub_path_length < 6 {
        r.skip(sub_path_length)?;
        return Ok(Vec::new());
    }
    r.skip(1)?; // padding
    r.skip(1)?; // sub_path_type
    r.skip(2)?; // reserved (15 bits) + is_repeat_SubPath (1 bit)
    r.skip(1)?; // reserved
    let num_items = r.read_u8()?;

    let mut clip_ids = Vec::with_capacity(num_items as usize);
    for _ in 0..num_items {
        let item_length = r.read_u16()? as usize;
        let item_start = r.pos;

        if item_length >= 9 {
            // clip_id: 5 ASCII bytes + codec_id: 4 bytes
            let clip_id_bytes = r.read_bytes(5)?;
            clip_ids.push(String::from_utf8_lossy(clip_id_bytes).into_owned());
        }

        // Skip remaining bytes in this sub-play item
        let consumed = r.pos - item_start;
        if consumed < item_length {
            r.skip(item_length - consumed)?;
        }
    }

    // Skip any remaining bytes in this sub-path
    let total_consumed = r.pos - sub_path_start;
    if total_consumed < sub_path_length {
        r.skip(sub_path_length - total_consumed)?;
    }

    Ok(clip_ids)
}

/// Parses the STN (Stream Number Table) from the current reader position.
#[allow(
    clippy::similar_names,
    reason = "field names match the MPLS binary spec"
)]
fn parse_stn_table(r: &mut Cursor<'_>) -> Result<StnTable, MplsError> {
    let table_length = r.read_u16()? as usize;
    let table_start = r.pos;

    if table_length == 0 {
        return Ok(StnTable {
            video: Vec::new(),
            audio: Vec::new(),
            subtitles: Vec::new(),
        });
    }

    // reserved (2 bytes)
    r.skip(2)?;

    let num_video = r.read_u8()?;
    let num_audio = r.read_u8()?;
    let num_pg = r.read_u8()?;
    let num_ig = r.read_u8()?;
    let num_secondary_audio = r.read_u8()?;
    let num_secondary_video = r.read_u8()?;
    let num_pip_pg = r.read_u8()?;
    // reserved (5 bytes)
    r.skip(5)?;

    let mut video = Vec::with_capacity(num_video as usize);
    for _ in 0..num_video {
        video.push(parse_video_stream(r)?);
    }

    let mut audio = Vec::with_capacity(num_audio as usize);
    for _ in 0..num_audio {
        audio.push(parse_audio_stream(r)?);
    }

    let mut subtitles = Vec::with_capacity(num_pg as usize);
    for _ in 0..num_pg {
        subtitles.push(parse_subtitle_stream(r)?);
    }

    // Skip IG streams
    for _ in 0..num_ig {
        skip_stream_entry(r)?;
    }

    // Skip secondary audio
    for _ in 0..num_secondary_audio {
        skip_stream_entry(r)?;
    }

    // Skip secondary video
    for _ in 0..num_secondary_video {
        skip_stream_entry(r)?;
    }

    // Skip PiP PG
    for _ in 0..num_pip_pg {
        skip_stream_entry(r)?;
    }

    // Jump past any remaining table bytes
    let consumed = r.pos - table_start;
    if consumed < table_length {
        r.skip(table_length - consumed)?;
    }

    Ok(StnTable {
        video,
        audio,
        subtitles,
    })
}

/// Parses a video stream entry from the STN table.
fn parse_video_stream(r: &mut Cursor<'_>) -> Result<VideoStream, MplsError> {
    // entry_length (1 byte) + entry data
    let entry_length = r.read_u8()? as usize;
    let entry_start = r.pos;
    // stream_type
    let _stream_type = r.read_u8()?;
    // PID (for stream_type=1, which is the common case)
    let _pid = r.read_u16()?;
    // Skip remaining entry bytes
    let entry_consumed = r.pos - entry_start;
    if entry_consumed < entry_length {
        r.skip(entry_length - entry_consumed)?;
    }

    // attrs_length (1 byte) + attributes
    let attrs_length = r.read_u8()? as usize;
    let attrs_start = r.pos;
    let coding_type = r.read_u8()?;
    let format_rate = r.read_u8()?;
    let video_format = format_rate >> 4;
    let frame_rate = format_rate & 0x0F;
    // Skip remaining attribute bytes
    let attrs_consumed = r.pos - attrs_start;
    if attrs_consumed < attrs_length {
        r.skip(attrs_length - attrs_consumed)?;
    }

    Ok(VideoStream {
        coding_type,
        video_format,
        frame_rate,
    })
}

/// Parses an audio stream entry from the STN table.
fn parse_audio_stream(r: &mut Cursor<'_>) -> Result<AudioStream, MplsError> {
    // entry_length (1 byte) + entry data
    let entry_length = r.read_u8()? as usize;
    let entry_start = r.pos;
    let _stream_type = r.read_u8()?;
    let _pid = r.read_u16()?;
    let entry_consumed = r.pos - entry_start;
    if entry_consumed < entry_length {
        r.skip(entry_length - entry_consumed)?;
    }

    // attrs_length (1 byte) + attributes
    let attrs_length = r.read_u8()? as usize;
    let attrs_start = r.pos;
    let coding_type = r.read_u8()?;
    let format_rate = r.read_u8()?;
    let audio_format = format_rate >> 4;
    let sample_rate = format_rate & 0x0F;
    let lang_bytes = r.read_bytes(3)?;
    let language = String::from_utf8_lossy(lang_bytes).into_owned();
    let attrs_consumed = r.pos - attrs_start;
    if attrs_consumed < attrs_length {
        r.skip(attrs_length - attrs_consumed)?;
    }

    Ok(AudioStream {
        coding_type,
        audio_format,
        sample_rate,
        language,
    })
}

/// Parses a PGS/text subtitle stream entry from the STN table.
fn parse_subtitle_stream(r: &mut Cursor<'_>) -> Result<SubtitleStream, MplsError> {
    // entry_length (1 byte) + entry data
    let entry_length = r.read_u8()? as usize;
    let entry_start = r.pos;
    let _stream_type = r.read_u8()?;
    let _pid = r.read_u16()?;
    let entry_consumed = r.pos - entry_start;
    if entry_consumed < entry_length {
        r.skip(entry_length - entry_consumed)?;
    }

    // attrs_length (1 byte) + attributes
    let attrs_length = r.read_u8()? as usize;
    let attrs_start = r.pos;
    let coding_type = r.read_u8()?;

    // Text subtitle (0x92) has a char_code byte before the language
    if coding_type == 0x92 {
        r.skip(1)?;
    }
    let lang_bytes = r.read_bytes(3)?;
    let language = String::from_utf8_lossy(lang_bytes).into_owned();

    let attrs_consumed = r.pos - attrs_start;
    if attrs_consumed < attrs_length {
        r.skip(attrs_length - attrs_consumed)?;
    }

    Ok(SubtitleStream {
        coding_type,
        language,
    })
}

/// Skips a stream entry (entry + attributes) without parsing details.
fn skip_stream_entry(r: &mut Cursor<'_>) -> Result<(), MplsError> {
    let entry_length = r.read_u8()? as usize;
    r.skip(entry_length)?;
    let attrs_length = r.read_u8()? as usize;
    r.skip(attrs_length)?;
    Ok(())
}

/// Parses a single chapter mark entry (14 bytes).
fn parse_mark(r: &mut Cursor<'_>) -> Result<Mark, MplsError> {
    // reserved (1 byte)
    r.skip(1)?;
    let mark_type = r.read_u8()?;
    let play_item_ref = r.read_u16()?;
    let timestamp = r.read_u32()?;
    // entry_ES_PID (2 bytes) + duration (4 bytes)
    r.skip(6)?;

    Ok(Mark {
        mark_type,
        play_item_ref,
        timestamp,
    })
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
#[allow(
    clippy::cast_possible_truncation,
    reason = "test builder values are small known constants"
)]
pub(crate) mod tests {
    use super::*;

    /// Builds a minimal valid MPLS file with the given play items and marks.
    ///
    /// This constructs a binary MPLS according to the format specification,
    /// useful for unit-testing the parser without real disc fixtures.
    pub struct MplsBuilder {
        play_items: Vec<PlayItemSpec>,
        marks: Vec<MarkSpec>,
    }

    struct PlayItemSpec {
        clip_id: [u8; 5],
        in_time: u32,
        out_time: u32,
        connection_condition: u8,
        is_multi_angle: bool,
        angle_clip_ids: Vec<[u8; 5]>,
        video: Vec<VideoStreamSpec>,
        audio: Vec<AudioStreamSpec>,
        subtitles: Vec<SubtitleStreamSpec>,
    }

    struct VideoStreamSpec {
        coding_type: u8,
        video_format: u8,
        frame_rate: u8,
    }

    struct AudioStreamSpec {
        coding_type: u8,
        audio_format: u8,
        sample_rate: u8,
        language: [u8; 3],
    }

    struct SubtitleStreamSpec {
        coding_type: u8,
        language: [u8; 3],
    }

    struct MarkSpec {
        mark_type: u8,
        play_item_ref: u16,
        timestamp: u32,
    }

    impl MplsBuilder {
        pub(crate) fn new() -> Self {
            Self {
                play_items: Vec::new(),
                marks: Vec::new(),
            }
        }

        pub(crate) fn play_item(mut self, clip_id: &str, in_time: u32, out_time: u32) -> Self {
            let mut id = [b'0'; 5];
            for (i, b) in clip_id.bytes().take(5).enumerate() {
                id[i] = b;
            }
            self.play_items.push(PlayItemSpec {
                clip_id: id,
                in_time,
                out_time,
                connection_condition: 1,
                is_multi_angle: false,
                angle_clip_ids: Vec::new(),
                video: vec![VideoStreamSpec {
                    coding_type: 0x1b,
                    video_format: 6,
                    frame_rate: 1,
                }],
                audio: vec![AudioStreamSpec {
                    coding_type: 0x81,
                    audio_format: 3,
                    sample_rate: 1,
                    language: *b"eng",
                }],
                subtitles: Vec::new(),
            });
            self
        }

        pub(crate) fn play_item_seamless(
            mut self,
            clip_id: &str,
            in_time: u32,
            out_time: u32,
        ) -> Self {
            self = self.play_item(clip_id, in_time, out_time);
            if let Some(item) = self.play_items.last_mut() {
                item.connection_condition = 5;
            }
            self
        }

        pub(crate) fn play_item_multi_angle(
            mut self,
            clip_id: &str,
            in_time: u32,
            out_time: u32,
            angles: &[&str],
        ) -> Self {
            self = self.play_item(clip_id, in_time, out_time);
            if let Some(item) = self.play_items.last_mut() {
                item.is_multi_angle = true;
                for angle in angles {
                    let mut id = [b'0'; 5];
                    for (i, b) in angle.bytes().take(5).enumerate() {
                        id[i] = b;
                    }
                    item.angle_clip_ids.push(id);
                }
            }
            self
        }

        pub(crate) fn mark(mut self, play_item_ref: u16, timestamp: u32) -> Self {
            self.marks.push(MarkSpec {
                mark_type: 1,
                play_item_ref,
                timestamp,
            });
            self
        }

        pub(crate) fn build(&self) -> Vec<u8> {
            let mut buf = Vec::new();

            // Header: magic + version + offsets placeholder + reserved
            buf.extend_from_slice(b"MPLS");
            buf.extend_from_slice(b"0200");
            // playlist_offset placeholder (offset 8)
            buf.extend_from_slice(&[0u8; 4]);
            // mark_offset placeholder (offset 12)
            buf.extend_from_slice(&[0u8; 4]);
            // extension_offset (0)
            buf.extend_from_slice(&[0u8; 4]);
            // reserved (20 bytes)
            buf.extend_from_slice(&[0u8; 20]);

            // AppInfoPlayList (minimal: length=14 + 10 bytes of data)
            buf.extend_from_slice(&0x0000_000E_u32.to_be_bytes());
            buf.extend_from_slice(&[0u8; 14]);

            // Playlist section offset
            let playlist_offset = buf.len() as u32;
            buf[8..12].copy_from_slice(&playlist_offset.to_be_bytes());

            // Build play items first to compute section length
            let mut items_buf = Vec::new();
            for item in &self.play_items {
                let item_data = Self::build_play_item(item);
                let item_length = item_data.len() as u16;
                items_buf.extend_from_slice(&item_length.to_be_bytes());
                items_buf.extend_from_slice(&item_data);
            }

            // Section: length (4) + reserved (2) + num_play_items (2) + num_sub_paths (2)
            let section_length = (6 + items_buf.len()) as u32;
            buf.extend_from_slice(&section_length.to_be_bytes());
            buf.extend_from_slice(&[0u8; 2]); // reserved
            buf.extend_from_slice(&(self.play_items.len() as u16).to_be_bytes());
            buf.extend_from_slice(&0u16.to_be_bytes()); // num_sub_paths
            buf.extend_from_slice(&items_buf);

            // Mark section offset
            let mark_offset = buf.len() as u32;
            buf[12..16].copy_from_slice(&mark_offset.to_be_bytes());

            // Marks section
            let marks_length = (2 + self.marks.len() * 14) as u32;
            buf.extend_from_slice(&marks_length.to_be_bytes());
            buf.extend_from_slice(&(self.marks.len() as u16).to_be_bytes());
            for m in &self.marks {
                buf.push(0); // reserved
                buf.push(m.mark_type);
                buf.extend_from_slice(&m.play_item_ref.to_be_bytes());
                buf.extend_from_slice(&m.timestamp.to_be_bytes());
                buf.extend_from_slice(&0xFFFF_u16.to_be_bytes()); // entry_ES_PID
                buf.extend_from_slice(&0u32.to_be_bytes()); // duration
            }

            buf
        }

        fn build_play_item(item: &PlayItemSpec) -> Vec<u8> {
            let mut buf = Vec::new();

            // clip_id (5) + codec_id (4)
            buf.extend_from_slice(&item.clip_id);
            buf.extend_from_slice(b"M2TS");

            // flags: 11 reserved + 1 is_multi_angle + 4 connection_condition
            let flags: u16 = if item.is_multi_angle {
                (1 << 4) | u16::from(item.connection_condition)
            } else {
                u16::from(item.connection_condition)
            };
            buf.extend_from_slice(&flags.to_be_bytes());

            // stc_id
            buf.push(0);

            buf.extend_from_slice(&item.in_time.to_be_bytes());
            buf.extend_from_slice(&item.out_time.to_be_bytes());

            // UO_mask (8) + flags2 (1) + still_mode (1) + still_time (2) = 12 bytes
            buf.extend_from_slice(&[0u8; 12]);

            // Multi-angle data
            if item.is_multi_angle {
                let angle_count = (item.angle_clip_ids.len() + 1) as u8;
                buf.push(angle_count);
                buf.push(0); // flags (is_different_audio=0, is_seamless_angle=0)
                for angle_id in &item.angle_clip_ids {
                    buf.extend_from_slice(angle_id);
                    buf.extend_from_slice(b"M2TS");
                    buf.push(0); // stc_id
                }
            }

            // STN table
            let stn = Self::build_stn_table(item);
            buf.extend_from_slice(&stn);

            buf
        }

        fn build_stn_table(item: &PlayItemSpec) -> Vec<u8> {
            let mut entries = Vec::new();

            for v in &item.video {
                // entry: stream_type(1) + PID(2) = 3 bytes
                entries.push(3u8); // entry_length
                entries.push(1); // stream_type = PlayItem stream
                entries.extend_from_slice(&0x1011_u16.to_be_bytes()); // PID
                // attrs: coding_type(1) + format_rate(1) = 2 bytes
                entries.push(2); // attrs_length
                entries.push(v.coding_type);
                entries.push((v.video_format << 4) | v.frame_rate);
            }

            for a in &item.audio {
                entries.push(3u8); // entry_length
                entries.push(1);
                entries.extend_from_slice(&0x1100_u16.to_be_bytes());
                // attrs: coding_type(1) + format_rate(1) + language(3) = 5 bytes
                entries.push(5); // attrs_length
                entries.push(a.coding_type);
                entries.push((a.audio_format << 4) | a.sample_rate);
                entries.extend_from_slice(&a.language);
            }

            for s in &item.subtitles {
                entries.push(3u8);
                entries.push(1);
                entries.extend_from_slice(&0x1200_u16.to_be_bytes());
                // attrs: coding_type(1) + language(3) = 4 bytes
                entries.push(4); // attrs_length
                entries.push(s.coding_type);
                entries.extend_from_slice(&s.language);
            }

            let mut stn = Vec::new();
            // table_length = reserved(2) + counts(7) + reserved(5) + entries
            let table_length = 2 + 7 + 5 + entries.len();
            stn.extend_from_slice(&(table_length as u16).to_be_bytes());
            stn.extend_from_slice(&[0u8; 2]); // reserved
            stn.push(item.video.len() as u8);
            stn.push(item.audio.len() as u8);
            stn.push(item.subtitles.len() as u8);
            stn.push(0); // num_ig
            stn.push(0); // num_secondary_audio
            stn.push(0); // num_secondary_video
            stn.push(0); // num_pip_pg
            stn.extend_from_slice(&[0u8; 5]); // reserved
            stn.extend_from_slice(&entries);

            stn
        }
    }

    #[test]
    fn parse_single_clip_playlist() {
        let data = MplsBuilder::new()
            .play_item("00004", 27_000_000, 59_040_000)
            .mark(0, 27_000_000)
            .mark(0, 28_144_890)
            .build();

        let pl = parse(&data, 100).expect("should parse single-clip playlist");
        assert_eq!(pl.number, 100, "playlist number");
        assert_eq!(pl.play_items.len(), 1, "play item count");
        assert_eq!(pl.marks.len(), 2, "mark count");

        let item = &pl.play_items[0];
        assert_eq!(item.clip_id, "00004", "clip id");
        assert_eq!(item.in_time, 27_000_000, "in time");
        assert_eq!(item.out_time, 59_040_000, "out time");
        assert_eq!(item.connection_condition, 1, "connection condition");
        assert!(!item.is_multi_angle, "not multi-angle");

        assert_eq!(item.streams.video.len(), 1, "video stream count");
        assert_eq!(item.streams.video[0].coding_type, 0x1b, "video codec");
        assert_eq!(item.streams.video[0].video_format, 6, "video format");
        assert_eq!(item.streams.video[0].frame_rate, 1, "frame rate");

        assert_eq!(item.streams.audio.len(), 1, "audio stream count");
        assert_eq!(item.streams.audio[0].coding_type, 0x81, "audio codec");
        assert_eq!(item.streams.audio[0].language, "eng", "audio language");

        let m0 = &pl.marks[0];
        assert_eq!(m0.play_item_ref, 0, "mark 0 play_item_ref");
        assert_eq!(m0.timestamp, 27_000_000, "mark 0 timestamp");
    }

    #[test]
    fn parse_multi_segment_playlist() {
        let data = MplsBuilder::new()
            .play_item("00100", 188_955_000, 222_570_000)
            .play_item_seamless("00102", 222_525_000, 498_645_000)
            .play_item_seamless("00103", 498_375_000, 511_155_000)
            .mark(0, 188_955_000)
            .mark(1, 225_000_000)
            .mark(2, 500_000_000)
            .build();

        let pl = parse(&data, 200).expect("should parse multi-segment playlist");
        assert_eq!(pl.play_items.len(), 3, "play item count");

        assert_eq!(
            pl.play_items[0].connection_condition, 1,
            "item 0 conn = non-seamless"
        );
        assert_eq!(
            pl.play_items[1].connection_condition, 5,
            "item 1 conn = seamless"
        );
        assert_eq!(
            pl.play_items[2].connection_condition, 5,
            "item 2 conn = seamless"
        );

        assert_eq!(pl.marks.len(), 3, "mark count");
        assert_eq!(pl.marks[1].play_item_ref, 1, "mark 1 references item 1");
    }

    #[test]
    fn parse_multi_angle_playlist() {
        let data = MplsBuilder::new()
            .play_item_multi_angle("00100", 188_955_000, 222_570_000, &["00101"])
            .play_item_seamless("00102", 222_525_000, 498_645_000)
            .build();

        let pl = parse(&data, 200).expect("should parse multi-angle playlist");
        let item = &pl.play_items[0];
        assert!(item.is_multi_angle, "item 0 is multi-angle");
        assert_eq!(item.angle_clip_ids, vec!["00101"], "angle clip ids");
        assert_eq!(item.clip_id, "00100", "primary clip id");
    }

    #[test]
    fn parse_26_episode_playlist() {
        let mut builder = MplsBuilder::new();
        for i in 0..26u32 {
            let clip_id = format!("{:05}", 4 + i);
            builder = builder.play_item(&clip_id, 27_000_000, 59_040_000 + i * 45_000);
            builder = builder.mark(i as u16, 27_000_000);
        }
        let data = builder.build();

        let pl = parse(&data, 1).expect("should parse 26-episode playlist");
        assert_eq!(pl.play_items.len(), 26, "26 play items");
        assert_eq!(pl.marks.len(), 26, "26 marks");
        assert_eq!(pl.play_items[0].clip_id, "00004", "first clip");
        assert_eq!(pl.play_items[25].clip_id, "00029", "last clip");
    }

    #[test]
    fn reject_invalid_magic() {
        let data = b"HDMV0200\x00\x00\x00\x00\x00\x00\x00\x00";
        let err = parse(data, 0).expect_err("should reject invalid magic");
        assert!(
            matches!(err, MplsError::InvalidMagic { found } if &found == b"HDMV"),
            "error should be InvalidMagic"
        );
    }

    #[test]
    fn reject_unsupported_version() {
        let data = b"MPLS9999\x00\x00\x00\x00\x00\x00\x00\x00";
        let err = parse(data, 0).expect_err("should reject unsupported version");
        assert!(
            matches!(err, MplsError::UnsupportedVersion { .. }),
            "error should be UnsupportedVersion"
        );
    }

    #[test]
    fn reject_truncated_file() {
        let err = parse(b"MPL", 0).expect_err("should reject truncated file");
        assert!(
            matches!(err, MplsError::UnexpectedEof { .. }),
            "error should be UnexpectedEof"
        );
    }
}
