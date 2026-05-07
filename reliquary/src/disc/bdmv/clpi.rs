// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! CLPI binary parser — converts raw bytes into parse structs.
//!
//! Reference: `reference/CLPI.md` in the planning repository.
//! All multi-byte integers are big-endian.

use thiserror::Error;

// ── Errors ──────────────────────────────────────────────────────────────

/// Errors that can occur while parsing a CLPI file.
#[derive(Debug, Error)]
pub enum ClpiError {
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

    /// The file does not start with the `HDMV` magic bytes.
    #[error("invalid magic: expected \"HDMV\", got {found:?}")]
    InvalidMagic {
        /// The four bytes found at the start of the file.
        found: [u8; 4],
    },

    /// The version string is not a recognised CLPI version.
    #[error("unsupported version: {version:?}")]
    UnsupportedVersion {
        /// The four-byte ASCII version string.
        version: [u8; 4],
    },
}

// ── Reader helper ───────────────────────────────────────────────────────

/// A cursor over a byte slice with bounds-checked reads.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

#[allow(
    clippy::missing_const_for_fn,
    reason = "internal helper — const adds no value"
)]
impl<'a> Reader<'a> {
    const fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    const fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn ensure(&self, n: usize) -> Result<(), ClpiError> {
        if self.remaining() < n {
            return Err(ClpiError::UnexpectedEof {
                offset: self.pos,
                needed: n,
                available: self.remaining(),
            });
        }
        Ok(())
    }

    fn skip(&mut self, n: usize) -> Result<(), ClpiError> {
        self.ensure(n)?;
        self.pos += n;
        Ok(())
    }

    fn read_u8(&mut self) -> Result<u8, ClpiError> {
        self.ensure(1)?;
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }

    fn read_u16(&mut self) -> Result<u16, ClpiError> {
        self.ensure(2)?;
        let v = u16::from_be_bytes([self.data[self.pos], self.data[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    fn read_u32(&mut self) -> Result<u32, ClpiError> {
        self.ensure(4)?;
        let v = u32::from_be_bytes([
            self.data[self.pos],
            self.data[self.pos + 1],
            self.data[self.pos + 2],
            self.data[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(v)
    }

    fn read_bytes(&mut self, n: usize) -> Result<&'a [u8], ClpiError> {
        self.ensure(n)?;
        let slice = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn seek(&mut self, new_pos: usize) -> Result<(), ClpiError> {
        if new_pos > self.data.len() {
            return Err(ClpiError::UnexpectedEof {
                offset: new_pos,
                needed: 0,
                available: 0,
            });
        }
        self.pos = new_pos;
        Ok(())
    }
}

// ── Parse structs ───────────────────────────────────────────────────────

/// A parsed CLPI file.
#[derive(Debug, Clone)]
pub struct ClipInfo {
    /// Clip ID derived from the filename (e.g. `00299.clpi` → `"00299"`).
    pub clip_id: String,
    /// Application type (1=Main TS, 2=slideshow, 3=interactive menu,
    /// 4=text subtitle, 5=enhanced video, 6=enhanced audio).
    pub application_type: u8,
    /// Transport stream recording rate in bytes/sec.
    pub ts_recording_rate: u32,
    /// Total 192-byte source packets in the matching m2ts.
    pub num_source_packets: u32,
    /// Elementary streams in this clip.
    pub streams: Vec<ClipStream>,
}

/// An elementary stream within a clip.
#[derive(Debug, Clone)]
pub struct ClipStream {
    /// MPEG-TS PID.
    pub pid: u16,
    /// Coding type (same values as MPLS STN: 0x1B=H.264, 0x81=AC-3,
    /// 0x91=IG, etc.).
    pub coding_type: u8,
    /// Stream-specific attributes.
    pub attrs: StreamAttrs,
}

/// Stream-specific attributes, matching the MPLS STN format.
#[derive(Debug, Clone)]
pub enum StreamAttrs {
    /// Video stream attributes.
    Video {
        /// Resolution indicator.
        video_format: u8,
        /// Frame rate indicator.
        frame_rate: u8,
    },
    /// Audio stream attributes.
    Audio {
        /// Channel layout indicator.
        audio_format: u8,
        /// Sample rate indicator.
        sample_rate: u8,
        /// Three-letter ISO 639-2 language code.
        language: String,
    },
    /// PGS subtitle stream attributes.
    Subtitle {
        /// Three-letter ISO 639-2 language code.
        language: String,
    },
    /// Interactive Graphics stream attributes.
    Ig {
        /// Three-letter ISO 639-2 language code.
        language: String,
    },
    /// Stream type not specifically handled.
    Other,
}

// ── Parsing ─────────────────────────────────────────────────────────────

/// Parses a CLPI file from raw bytes.
///
/// `clip_id` is the clip identifier derived from the filename
/// (e.g. `"00299"` from `00299.clpi`).
///
/// # Errors
///
/// Returns [`ClpiError`] if the file is malformed or truncated.
pub fn parse(data: &[u8], clip_id: String) -> Result<ClipInfo, ClpiError> {
    let mut r = Reader::new(data);

    // ── Header (40 bytes) ──────────────────────────────────────────
    let magic = r.read_bytes(4)?;
    if magic != b"HDMV" {
        let mut found = [0u8; 4];
        found.copy_from_slice(magic);
        return Err(ClpiError::InvalidMagic { found });
    }

    let version_bytes = r.read_bytes(4)?;
    match version_bytes {
        b"0100" | b"0200" | b"0300" => {}
        _ => {
            let mut version = [0u8; 4];
            version.copy_from_slice(version_bytes);
            return Err(ClpiError::UnsupportedVersion { version });
        }
    }

    // sequence_info_addr (u32) — not needed
    r.skip(4)?;
    // program_info_addr (u32) at offset 0x0C
    let program_info_addr = r.read_u32()? as usize;
    // cpi_addr (u32) — not needed
    r.skip(4)?;
    // clip_mark_addr (u32) — not needed
    r.skip(4)?;
    // ext_data_addr (u32) — not needed
    r.skip(4)?;
    // reserved (12 bytes)
    r.skip(12)?;

    // ── ClipInfo section (at offset 0x28) ──────────────────────────
    let _clip_info_length = r.read_u32()?;
    let _reserved = r.read_u16()?;
    let _clip_stream_type = r.read_u8()?;
    let application_type = r.read_u8()?;
    // flags: 31 reserved bits + 1 is_atc_delta bit
    r.skip(4)?;
    let ts_recording_rate = r.read_u32()?;
    let num_source_packets = r.read_u32()?;
    // Skip remaining ClipInfo section bytes (reserved, TS type info, etc.)

    // ── ProgramInfo section ────────────────────────────────────────
    r.seek(program_info_addr)?;
    let _program_info_length = r.read_u32()?;
    let _reserved = r.read_u8()?;
    let num_programs = r.read_u8()?;

    let mut streams = Vec::new();

    for _ in 0..num_programs {
        // spn_program_seq_start (u32) + program_map_pid (u16)
        r.skip(6)?;
        let num_streams = r.read_u8()?;
        let _num_groups = r.read_u8()?;

        for _ in 0..num_streams {
            let pid = r.read_u16()?;
            let stream = parse_stream_attrs(&mut r, pid)?;
            streams.push(stream);
        }
    }

    Ok(ClipInfo {
        clip_id,
        application_type,
        ts_recording_rate,
        num_source_packets,
        streams,
    })
}

/// Parses stream attributes for a single elementary stream.
fn parse_stream_attrs(r: &mut Reader<'_>, pid: u16) -> Result<ClipStream, ClpiError> {
    let attr_length = r.read_u8()? as usize;
    let attr_start = r.pos;

    let coding_type = r.read_u8()?;

    let attrs = match coding_type {
        // Video: MPEG-1, MPEG-2, VC-1, H.264, HEVC
        0x01 | 0x02 | 0xea | 0x1b | 0x24 => {
            let format_rate = r.read_u8()?;
            StreamAttrs::Video {
                video_format: format_rate >> 4,
                frame_rate: format_rate & 0x0F,
            }
        }

        // Audio: MPEG-1/2, LPCM, AC-3, DTS, TrueHD, E-AC-3, DTS-HD HR,
        // DTS-HD MA, E-AC-3 2nd, DTS-HD 2nd
        0x03 | 0x04 | 0x80..=0x86 | 0xa1 | 0xa2 => {
            let format_rate = r.read_u8()?;
            let lang_bytes = r.read_bytes(3)?;
            StreamAttrs::Audio {
                audio_format: format_rate >> 4,
                sample_rate: format_rate & 0x0F,
                language: String::from_utf8_lossy(lang_bytes).into_owned(),
            }
        }

        // PGS subtitles
        0x90 => {
            let lang_bytes = r.read_bytes(3)?;
            StreamAttrs::Subtitle {
                language: String::from_utf8_lossy(lang_bytes).into_owned(),
            }
        }

        // Interactive Graphics
        0x91 => {
            let lang_bytes = r.read_bytes(3)?;
            StreamAttrs::Ig {
                language: String::from_utf8_lossy(lang_bytes).into_owned(),
            }
        }

        // Text subtitles
        0x92 => {
            // char_code (1 byte) + language (3 bytes)
            r.skip(1)?;
            let lang_bytes = r.read_bytes(3)?;
            StreamAttrs::Subtitle {
                language: String::from_utf8_lossy(lang_bytes).into_owned(),
            }
        }

        _ => StreamAttrs::Other,
    };

    // Skip any remaining attribute bytes
    let consumed = r.pos - attr_start;
    if consumed < attr_length {
        r.skip(attr_length - consumed)?;
    }

    Ok(ClipStream {
        pid,
        coding_type,
        attrs,
    })
}

// ── Coding type constant ────────────────────────────────────────────────

/// Coding type for Interactive Graphics streams.
pub const CODING_TYPE_IG: u8 = 0x91;

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

    // ── Stream spec types for the builder ───────────────────────────

    enum StreamSpec {
        Video {
            pid: u16,
            coding_type: u8,
            video_format: u8,
            frame_rate: u8,
        },
        Audio {
            pid: u16,
            coding_type: u8,
            audio_format: u8,
            sample_rate: u8,
            language: [u8; 3],
        },
        Subtitle {
            pid: u16,
            coding_type: u8,
            language: [u8; 3],
        },
        Ig {
            pid: u16,
            language: [u8; 3],
        },
    }

    /// Builds a minimal valid CLPI file from a fluent API.
    ///
    /// Constructs binary CLPI according to the format specification,
    /// useful for unit-testing the parser without real disc fixtures.
    pub struct ClpiBuilder {
        application_type: u8,
        ts_recording_rate: u32,
        num_source_packets: u32,
        streams: Vec<StreamSpec>,
    }

    impl ClpiBuilder {
        pub(crate) fn new() -> Self {
            Self {
                application_type: 1,
                ts_recording_rate: 6_000_000,
                num_source_packets: 0,
                streams: Vec::new(),
            }
        }

        pub(crate) fn application_type(mut self, val: u8) -> Self {
            self.application_type = val;
            self
        }

        pub(crate) fn ts_recording_rate(mut self, val: u32) -> Self {
            self.ts_recording_rate = val;
            self
        }

        pub(crate) fn num_source_packets(mut self, val: u32) -> Self {
            self.num_source_packets = val;
            self
        }

        pub(crate) fn video(
            mut self,
            pid: u16,
            coding_type: u8,
            video_format: u8,
            frame_rate: u8,
        ) -> Self {
            self.streams.push(StreamSpec::Video {
                pid,
                coding_type,
                video_format,
                frame_rate,
            });
            self
        }

        pub(crate) fn audio(
            mut self,
            pid: u16,
            coding_type: u8,
            audio_format: u8,
            sample_rate: u8,
            language: [u8; 3],
        ) -> Self {
            self.streams.push(StreamSpec::Audio {
                pid,
                coding_type,
                audio_format,
                sample_rate,
                language,
            });
            self
        }

        pub(crate) fn subtitle(mut self, pid: u16, coding_type: u8, language: [u8; 3]) -> Self {
            self.streams.push(StreamSpec::Subtitle {
                pid,
                coding_type,
                language,
            });
            self
        }

        pub(crate) fn ig(mut self, pid: u16, language: [u8; 3]) -> Self {
            self.streams.push(StreamSpec::Ig { pid, language });
            self
        }

        pub(crate) fn build(&self) -> Vec<u8> {
            let mut buf = Vec::new();

            // ── Header (40 bytes) ──
            buf.extend_from_slice(b"HDMV");
            buf.extend_from_slice(b"0200");
            // sequence_info_addr placeholder (offset 0x08)
            buf.extend_from_slice(&[0u8; 4]);
            // program_info_addr placeholder (offset 0x0C)
            buf.extend_from_slice(&[0u8; 4]);
            // cpi_addr (offset 0x10)
            buf.extend_from_slice(&[0u8; 4]);
            // clip_mark_addr (offset 0x14)
            buf.extend_from_slice(&[0u8; 4]);
            // ext_data_addr (offset 0x18)
            buf.extend_from_slice(&[0u8; 4]);
            // reserved (12 bytes, offset 0x1C–0x27)
            buf.extend_from_slice(&[0u8; 12]);

            // ── ClipInfo section (at offset 0x28) ──
            // We build a minimal ClipInfo: 2 (reserved) + 1 (stream_type) +
            // 1 (app_type) + 4 (flags) + 4 (rate) + 4 (packets) + 128 (reserved)
            // = 144 bytes of data after the length field.
            let clip_info_length: u32 = 144;
            buf.extend_from_slice(&clip_info_length.to_be_bytes());
            buf.extend_from_slice(&[0u8; 2]); // reserved
            buf.push(1); // clip_stream_type
            buf.push(self.application_type);
            buf.extend_from_slice(&[0u8; 4]); // flags (is_atc_delta = 0)
            buf.extend_from_slice(&self.ts_recording_rate.to_be_bytes());
            buf.extend_from_slice(&self.num_source_packets.to_be_bytes());
            buf.extend_from_slice(&[0u8; 128]); // reserved

            // ── SequenceInfo section (minimal) ──
            let seq_info_addr = buf.len() as u32;
            buf[0x08..0x0C].copy_from_slice(&seq_info_addr.to_be_bytes());
            // length=0, no sequences
            buf.extend_from_slice(&0u32.to_be_bytes());

            // ── ProgramInfo section ──
            let program_info_addr = buf.len() as u32;
            buf[0x0C..0x10].copy_from_slice(&program_info_addr.to_be_bytes());

            // Build stream entries
            let mut stream_data = Vec::new();
            for stream in &self.streams {
                match stream {
                    StreamSpec::Video {
                        pid,
                        coding_type,
                        video_format,
                        frame_rate,
                    } => {
                        stream_data.extend_from_slice(&pid.to_be_bytes());
                        stream_data.push(2); // attr_length
                        stream_data.push(*coding_type);
                        stream_data.push((video_format << 4) | frame_rate);
                    }
                    StreamSpec::Audio {
                        pid,
                        coding_type,
                        audio_format,
                        sample_rate,
                        language,
                    } => {
                        stream_data.extend_from_slice(&pid.to_be_bytes());
                        stream_data.push(5); // attr_length
                        stream_data.push(*coding_type);
                        stream_data.push((audio_format << 4) | sample_rate);
                        stream_data.extend_from_slice(language);
                    }
                    StreamSpec::Subtitle {
                        pid,
                        coding_type,
                        language,
                    } => {
                        stream_data.extend_from_slice(&pid.to_be_bytes());
                        stream_data.push(4); // attr_length
                        stream_data.push(*coding_type);
                        stream_data.extend_from_slice(language);
                    }
                    StreamSpec::Ig { pid, language } => {
                        stream_data.extend_from_slice(&pid.to_be_bytes());
                        stream_data.push(4); // attr_length
                        stream_data.push(CODING_TYPE_IG);
                        stream_data.extend_from_slice(language);
                    }
                }
            }

            // ProgramInfo: length(4) + reserved(1) + num_programs(1) +
            //   program: spn(4) + pmt_pid(2) + num_streams(1) + num_groups(1)
            //   + stream_data
            let program_body_len = 1 + 1 + 4 + 2 + 1 + 1 + stream_data.len();
            buf.extend_from_slice(&(program_body_len as u32).to_be_bytes());
            buf.push(0); // reserved
            buf.push(1); // num_programs
            // Program entry
            buf.extend_from_slice(&0u32.to_be_bytes()); // spn_program_seq_start
            buf.extend_from_slice(&0x0100_u16.to_be_bytes()); // program_map_pid
            buf.push(self.streams.len() as u8); // num_streams
            buf.push(0); // num_groups
            buf.extend_from_slice(&stream_data);

            buf
        }
    }

    // ── Unit tests ──────────────────────────────────────────────────

    #[test]
    fn parse_content_clip() {
        let data = ClpiBuilder::new()
            .application_type(1)
            .ts_recording_rate(6_000_000)
            .num_source_packets(242_272)
            .video(0x1011, 0x1b, 6, 1) // H.264 1080p 23.976
            .audio(0x1100, 0x81, 3, 1, *b"eng") // AC-3 2.0 48kHz eng
            .subtitle(0x1200, 0x90, *b"eng") // PGS eng
            .build();

        let clip = parse(&data, "00004".into()).expect("should parse content clip");
        assert_eq!(clip.clip_id, "00004", "clip id");
        assert_eq!(clip.application_type, 1, "application type");
        assert_eq!(clip.ts_recording_rate, 6_000_000, "recording rate");
        assert_eq!(clip.num_source_packets, 242_272, "source packets");
        assert_eq!(clip.streams.len(), 3, "stream count");

        // Video
        let v = &clip.streams[0];
        assert_eq!(v.pid, 0x1011, "video pid");
        assert_eq!(v.coding_type, 0x1b, "video coding type");
        assert!(
            matches!(
                v.attrs,
                StreamAttrs::Video {
                    video_format: 6,
                    frame_rate: 1
                }
            ),
            "video attrs"
        );

        // Audio
        let a = &clip.streams[1];
        assert_eq!(a.pid, 0x1100, "audio pid");
        assert_eq!(a.coding_type, 0x81, "audio coding type");
        assert!(
            matches!(
                &a.attrs,
                StreamAttrs::Audio {
                    audio_format: 3,
                    sample_rate: 1,
                    language
                } if language == "eng"
            ),
            "audio attrs"
        );

        // Subtitle
        let s = &clip.streams[2];
        assert_eq!(s.pid, 0x1200, "subtitle pid");
        assert_eq!(s.coding_type, 0x90, "subtitle coding type");
        assert!(
            matches!(
                &s.attrs,
                StreamAttrs::Subtitle { language } if language == "eng"
            ),
            "subtitle attrs"
        );
    }

    #[test]
    fn parse_ig_only_clip() {
        let data = ClpiBuilder::new()
            .application_type(5)
            .num_source_packets(100)
            .ig(0x1400, *b"eng")
            .build();

        let clip = parse(&data, "00291".into()).expect("should parse IG-only clip");
        assert_eq!(clip.application_type, 5, "application type");
        assert_eq!(clip.streams.len(), 1, "stream count");

        let ig = &clip.streams[0];
        assert_eq!(ig.pid, 0x1400, "ig pid");
        assert_eq!(ig.coding_type, CODING_TYPE_IG, "ig coding type");
        assert!(
            matches!(
                &ig.attrs,
                StreamAttrs::Ig { language } if language == "eng"
            ),
            "ig attrs"
        );
    }

    #[test]
    fn parse_ig_with_video_clip() {
        let data = ClpiBuilder::new()
            .application_type(3)
            .num_source_packets(500)
            .video(0x1011, 0x02, 6, 1) // MPEG-2 1080p 23.976
            .ig(0x1400, *b"eng")
            .build();

        let clip = parse(&data, "00098".into()).expect("should parse IG+video clip");
        assert_eq!(clip.application_type, 3, "application type");
        assert_eq!(clip.streams.len(), 2, "stream count");

        assert_eq!(clip.streams[0].coding_type, 0x02, "video coding type");
        assert_eq!(
            clip.streams[1].coding_type, CODING_TYPE_IG,
            "ig coding type"
        );
    }

    #[test]
    fn reject_invalid_magic() {
        let data = b"MPLS0200\x00\x00\x00\x00\x00\x00\x00\x00";
        let err = parse(data, "00000".into()).expect_err("should reject invalid magic");
        assert!(
            matches!(err, ClpiError::InvalidMagic { found } if &found == b"MPLS"),
            "error should be InvalidMagic"
        );
    }

    #[test]
    fn reject_unsupported_version() {
        let data = b"HDMV9999\x00\x00\x00\x00\x00\x00\x00\x00";
        let err = parse(data, "00000".into()).expect_err("should reject unsupported version");
        assert!(
            matches!(err, ClpiError::UnsupportedVersion { .. }),
            "error should be UnsupportedVersion"
        );
    }

    #[test]
    fn reject_truncated_file() {
        let err = parse(b"HDM", "00000".into()).expect_err("should reject truncated file");
        assert!(
            matches!(err, ClpiError::UnexpectedEof { .. }),
            "error should be UnexpectedEof"
        );
    }
}
