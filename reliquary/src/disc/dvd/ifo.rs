// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! IFO binary parser — converts raw bytes from DVD IFO files into parse structs.
//!
//! Two file types: VMG (`VIDEO_TS.IFO`) and VTS (`VTS_nn_0.IFO`).
//! Reference: `reference/DVD.md` in the planning repository.
//! All multi-byte integers are big-endian. Sector addresses are 2048-byte blocks.

use std::time::Duration;

use thiserror::Error;

/// DVD sector size in bytes.
const SECTOR_SIZE: usize = 2048;

// ── Errors ──────────────────────────────────────────────────────────────

/// Errors that can occur while parsing an IFO file.
#[derive(Debug, Error)]
pub enum IfoError {
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

    /// The file does not start with the expected magic bytes.
    #[error("invalid magic: expected \"{expected}\", got {found:?}")]
    InvalidMagic {
        /// The expected magic string.
        expected: &'static str,
        /// The bytes found at the start of the file.
        found: String,
    },

    /// A sector offset points outside the file.
    #[error("sector offset {sector} (byte {byte_offset}) is beyond file size {file_size}")]
    SectorOutOfRange {
        /// The sector number from the IFO.
        sector: u32,
        /// The computed byte offset.
        byte_offset: usize,
        /// The total file size.
        file_size: usize,
    },

    /// A byte offset points outside the table.
    #[error("byte offset {offset} is beyond table size {table_size}")]
    OffsetOutOfRange {
        /// The byte offset from the IFO.
        offset: usize,
        /// The size of the containing table.
        table_size: usize,
    },

    /// A BCD-encoded byte contains an invalid digit.
    #[error("invalid BCD value {value:#04x} at offset {offset}")]
    InvalidBcd {
        /// The raw byte value.
        value: u8,
        /// The byte offset in the file.
        offset: usize,
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

    fn ensure(&self, n: usize) -> Result<(), IfoError> {
        if self.remaining() < n {
            return Err(IfoError::UnexpectedEof {
                offset: self.pos,
                needed: n,
                available: self.remaining(),
            });
        }
        Ok(())
    }

    fn skip(&mut self, n: usize) -> Result<(), IfoError> {
        self.ensure(n)?;
        self.pos += n;
        Ok(())
    }

    fn read_u8(&mut self) -> Result<u8, IfoError> {
        self.ensure(1)?;
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }

    fn read_u16(&mut self) -> Result<u16, IfoError> {
        self.ensure(2)?;
        let v = u16::from_be_bytes([self.data[self.pos], self.data[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    fn read_u32(&mut self) -> Result<u32, IfoError> {
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

    fn seek(&mut self, new_pos: usize) -> Result<(), IfoError> {
        if new_pos > self.data.len() {
            return Err(IfoError::UnexpectedEof {
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

/// Parsed Video Manager (`VIDEO_TS.IFO`).
#[derive(Debug, Clone)]
pub struct Vmg {
    /// Number of title sets on the disc.
    pub nr_of_title_sets: u16,
    /// Title search pointer table entries.
    pub titles: Vec<TitlePointer>,
}

/// A title entry from the VMG title search pointer table.
#[derive(Debug, Clone)]
pub struct TitlePointer {
    /// VTS number (1-based) — maps to the corresponding VTS IFO file.
    pub title_set_nr: u8,
    /// Title number within the VTS (1-based).
    pub vts_ttn: u8,
    /// Number of chapters (parts of title).
    pub nr_of_ptts: u16,
    /// Number of angles.
    pub nr_of_angles: u8,
}

/// Parsed Video Title Set (`VTS_nn_0.IFO`).
#[derive(Debug, Clone)]
pub struct Vts {
    /// Title video attributes.
    pub video_attr: VideoAttr,
    /// Title audio stream attributes (up to 8).
    pub audio_attrs: Vec<AudioAttr>,
    /// Title subpicture stream attributes (up to 32).
    pub subp_attrs: Vec<SubpictureAttr>,
    /// Part-of-title table: for each VTS title, the list of chapter entries.
    pub ptt_table: Vec<Vec<PttEntry>>,
    /// Title-domain PGC table.
    pub pgc_table: Vec<PgcEntry>,
}

/// A chapter-to-PGC mapping entry.
#[derive(Debug, Clone)]
pub struct PttEntry {
    /// PGC number (1-based index into the PGC table).
    pub pgcn: u16,
    /// Program number within that PGC (1-based).
    pub pgn: u16,
}

/// A PGC search pointer entry paired with the parsed PGC data.
#[derive(Debug, Clone)]
pub struct PgcEntry {
    /// Entry type identifier.
    pub entry_id: u8,
    /// The parsed program chain.
    pub pgc: Pgc,
}

/// A parsed Program Chain.
#[derive(Debug, Clone)]
pub struct Pgc {
    /// Number of programs (chapters) in this PGC.
    pub nr_of_programs: u8,
    /// Number of cells in this PGC.
    pub nr_of_cells: u8,
    /// Total PGC playback time (from the header, not computed from cells).
    #[allow(
        dead_code,
        reason = "format-complete parse struct — used for validation"
    )]
    pub playback_time: DvdTime,
    /// Audio stream control words (8 entries). Bit 15 = available.
    pub audio_control: [u16; 8],
    /// Subpicture stream control words (32 entries). Bit 31 = available.
    pub subp_control: [u32; 32],
    /// Starting cell number (1-based) for each program.
    pub program_map: Vec<u8>,
    /// Cell playback information.
    pub cells: Vec<CellPlayback>,
}

/// Cell playback information (24 bytes per cell).
#[derive(Debug, Clone)]
pub struct CellPlayback {
    /// Block mode: 0=normal, 1=first angle, 2=middle angle, 3=last angle.
    pub block_mode: u8,
    /// Block type: 0=normal, 1=angle block.
    pub block_type: u8,
    /// Seconds to pause after cell (0=none, 0xFF=infinite).
    #[allow(
        dead_code,
        reason = "format-complete parse struct — needed for identify phase"
    )]
    pub still_time: u8,
    /// Cell duration.
    pub playback_time: DvdTime,
    /// Start sector in VOB.
    pub first_sector: u32,
    /// End sector in VOB.
    pub last_sector: u32,
}

/// BCD-encoded DVD time with frame rate.
#[derive(Debug, Clone, Copy)]
pub struct DvdTime {
    /// Hours (0–99).
    pub hours: u8,
    /// Minutes (0–59).
    pub minutes: u8,
    /// Seconds (0–59).
    pub seconds: u8,
    /// Frame count within the current second.
    pub frames: u8,
    /// Frame rate.
    pub frame_rate: FrameRate,
}

/// DVD frame rate, derived from the top 2 bits of the frame byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameRate {
    /// 25 fps (PAL).
    Pal,
    /// 29.97 fps (NTSC).
    Ntsc,
}

impl DvdTime {
    /// Converts to a [`Duration`].
    #[allow(
        clippy::cast_precision_loss,
        reason = "frame counts are small integers, well within f64 precision"
    )]
    pub fn to_duration(self) -> Duration {
        let secs =
            u64::from(self.hours) * 3600 + u64::from(self.minutes) * 60 + u64::from(self.seconds);
        let fps = match self.frame_rate {
            FrameRate::Pal => 25.0,
            FrameRate::Ntsc => 29.97,
        };
        let frac = f64::from(self.frames) / fps;
        Duration::from_secs_f64(secs as f64 + frac)
    }

    /// Returns true if this time is all zeros.
    #[cfg(test)]
    pub const fn is_zero(self) -> bool {
        self.hours == 0 && self.minutes == 0 && self.seconds == 0 && self.frames == 0
    }
}

/// Video attributes (2 bytes from IFO header).
#[derive(Debug, Clone)]
pub struct VideoAttr {
    /// 0=MPEG-1, 1=MPEG-2.
    pub mpeg_version: u8,
    /// 0=NTSC, 1=PAL.
    pub video_format: u8,
    /// 0=4:3, 3=16:9.
    pub aspect_ratio: u8,
    /// Resolution index (combined with `video_format` for actual resolution).
    pub picture_size: u8,
}

/// Audio stream attributes (8 bytes from IFO header).
#[derive(Debug, Clone)]
pub struct AudioAttr {
    /// Codec format: 0=AC-3, 2=MPEG-1, 3=MPEG-2ext, 4=LPCM, 6=DTS.
    pub format: u8,
    /// Channel count minus 1 (0=mono, 1=stereo, 5=5.1).
    pub channels: u8,
    /// Sample rate: 0=48kHz, 1=96kHz.
    #[allow(
        dead_code,
        reason = "format-complete parse struct — not surfaced in description yet"
    )]
    pub sample_rate: u8,
    /// ISO 639 language code (2 ASCII bytes).
    pub lang_code: [u8; 2],
}

/// Subpicture stream attributes (6 bytes from IFO header).
#[derive(Debug, Clone)]
pub struct SubpictureAttr {
    /// ISO 639 language code (2 ASCII bytes).
    pub lang_code: [u8; 2],
    /// Code extension: 1=normal, 5=CC, 9=forced, 13=commentary.
    pub code_extension: u8,
}

// ── BCD decoding ────────────────────────────────────────────────────────

/// Decodes a BCD-encoded byte into its decimal value.
const fn decode_bcd(value: u8, offset: usize) -> Result<u8, IfoError> {
    let hi = value >> 4;
    let lo = value & 0x0F;
    if hi > 9 || lo > 9 {
        return Err(IfoError::InvalidBcd { value, offset });
    }
    Ok(hi * 10 + lo)
}

/// Parses a 4-byte DVD time (BCD-encoded with frame rate flag).
fn parse_dvd_time(reader: &mut Reader<'_>) -> Result<DvdTime, IfoError> {
    let offset = reader.pos;
    let h = reader.read_u8()?;
    let m = reader.read_u8()?;
    let s = reader.read_u8()?;
    let frame_u = reader.read_u8()?;

    let rate_bits = frame_u >> 6;
    #[allow(
        clippy::match_same_arms,
        reason = "3 documents the valid NTSC value; _ is the fallback for malformed data"
    )]
    let frame_rate = match rate_bits {
        1 => FrameRate::Pal,
        3 => FrameRate::Ntsc,
        // Invalid rate — treat as NTSC to be lenient with malformed data.
        // A zero time (00:00:00.00) often has rate_bits=0; that's fine since
        // the frame count is also 0 and the rate doesn't matter.
        _ => FrameRate::Ntsc,
    };

    let hours = decode_bcd(h, offset)?;
    let minutes = decode_bcd(m, offset + 1)?;
    let seconds = decode_bcd(s, offset + 2)?;
    // Frame count is the lower 6 bits of frame_u, BCD-encoded.
    let frame_bcd = frame_u & 0x3F;
    let frames = decode_bcd(frame_bcd, offset + 3)?;

    Ok(DvdTime {
        hours,
        minutes,
        seconds,
        frames,
        frame_rate,
    })
}

// ── VMG parsing ─────────────────────────────────────────────────────────

/// Parses a `VIDEO_TS.IFO` (Video Manager) file.
///
/// # Errors
///
/// Returns [`IfoError`] if the file is truncated, has an invalid magic
/// string, or contains out-of-range offsets.
pub fn parse_vmg(data: &[u8]) -> Result<Vmg, IfoError> {
    let mut r = Reader::new(data);

    // Magic: "DVDVIDEO-VMG" at offset 0x000
    r.ensure(12)?;
    let magic = &data[..12];
    if magic != b"DVDVIDEO-VMG" {
        return Err(IfoError::InvalidMagic {
            expected: "DVDVIDEO-VMG",
            found: String::from_utf8_lossy(magic).into_owned(),
        });
    }

    // nr_of_title_sets at 0x03E
    r.seek(0x3E)?;
    let nr_of_title_sets = r.read_u16()?;

    // tt_srpt sector at 0x0C4
    r.seek(0xC4)?;
    let tt_srpt_sector = r.read_u32()?;

    // Read title search pointer table
    let tt_srpt_offset = sector_to_byte(tt_srpt_sector, data.len())?;
    r.seek(tt_srpt_offset)?;

    // Table header: nr_of_srpts (2) + zero (2) + last_byte (4)
    let nr_of_srpts = r.read_u16()?;
    r.skip(2)?; // zero
    r.skip(4)?; // last_byte

    // Title info entries (12 bytes each)
    let mut titles = Vec::with_capacity(usize::from(nr_of_srpts));
    for _ in 0..nr_of_srpts {
        r.skip(1)?; // pb_ty
        let nr_of_angles = r.read_u8()?;
        let nr_of_ptts = r.read_u16()?;
        r.skip(2)?; // parental_id
        let title_set_nr = r.read_u8()?;
        let vts_ttn = r.read_u8()?;
        r.skip(4)?; // title_set_sector

        titles.push(TitlePointer {
            title_set_nr,
            vts_ttn,
            nr_of_ptts,
            nr_of_angles,
        });
    }

    Ok(Vmg {
        nr_of_title_sets,
        titles,
    })
}

// ── VTS parsing ─────────────────────────────────────────────────────────

/// Parses a `VTS_nn_0.IFO` (Video Title Set) file.
///
/// # Errors
///
/// Returns [`IfoError`] if the file is truncated, has an invalid magic
/// string, or contains out-of-range offsets.
pub fn parse_vts(data: &[u8]) -> Result<Vts, IfoError> {
    let mut r = Reader::new(data);

    // Magic: "DVDVIDEO-VTS" at offset 0x000
    r.ensure(12)?;
    let magic = &data[..12];
    if magic != b"DVDVIDEO-VTS" {
        return Err(IfoError::InvalidMagic {
            expected: "DVDVIDEO-VTS",
            found: String::from_utf8_lossy(magic).into_owned(),
        });
    }

    // Stream attributes
    let video_attr = parse_video_attr(data)?;
    let audio_attrs = parse_audio_attrs(data)?;
    let subp_attrs = parse_subp_attrs(data)?;

    // PTT table sector at 0x0C8
    r.seek(0xC8)?;
    let ptt_sector = r.read_u32()?;

    // PGC table sector at 0x0CC
    let pgcit_sector = r.read_u32()?;

    // Parse PTT table
    let ptt_table = parse_ptt_table(data, ptt_sector)?;

    // Parse PGC table
    let pgc_table = parse_pgc_table(data, pgcit_sector)?;

    Ok(Vts {
        video_attr,
        audio_attrs,
        subp_attrs,
        ptt_table,
        pgc_table,
    })
}

/// Parses title video attributes at VTS offset 0x200.
fn parse_video_attr(data: &[u8]) -> Result<VideoAttr, IfoError> {
    let mut r = Reader::new(data);
    r.seek(0x200)?;
    let raw = r.read_u16()?;

    #[allow(
        clippy::cast_possible_truncation,
        reason = "bitfield extractions produce values that fit in u8"
    )]
    Ok(VideoAttr {
        mpeg_version: ((raw >> 14) & 0x03) as u8,
        video_format: ((raw >> 12) & 0x03) as u8,
        aspect_ratio: ((raw >> 10) & 0x03) as u8,
        picture_size: ((raw >> 2) & 0x03) as u8,
    })
}

/// Parses title audio attributes starting at VTS offset 0x203.
fn parse_audio_attrs(data: &[u8]) -> Result<Vec<AudioAttr>, IfoError> {
    let mut r = Reader::new(data);
    r.seek(0x203)?;
    let nr_of_audio = r.read_u8()?;

    // Audio attributes start at 0x204, 8 bytes each
    r.seek(0x204)?;
    let mut attrs = Vec::with_capacity(usize::from(nr_of_audio));
    for _ in 0..nr_of_audio {
        let b0 = r.read_u8()?;
        let b1 = r.read_u8()?;
        let lang_hi = r.read_u8()?;
        let lang_lo = r.read_u8()?;
        r.skip(4)?; // extension + code_extension + app_info

        let format = b0 >> 5; // bits 7-5
        let sample_rate = (b1 >> 4) & 0x03; // bits 5-4
        let channels = b1 & 0x07; // bits 2-0

        attrs.push(AudioAttr {
            format,
            channels,
            sample_rate,
            lang_code: [lang_hi, lang_lo],
        });
    }

    Ok(attrs)
}

/// Parses title subpicture attributes starting at VTS offset 0x255.
fn parse_subp_attrs(data: &[u8]) -> Result<Vec<SubpictureAttr>, IfoError> {
    let mut r = Reader::new(data);
    r.seek(0x255)?;
    let nr_of_subp = r.read_u8()?;

    // Subpicture attributes start at 0x256, 6 bytes each
    r.seek(0x256)?;
    let mut attrs = Vec::with_capacity(usize::from(nr_of_subp));
    for _ in 0..nr_of_subp {
        r.skip(2)?; // code_mode + type
        let lang_hi = r.read_u8()?;
        let lang_lo = r.read_u8()?;
        r.skip(1)?; // lang_extension
        let code_extension = r.read_u8()?;

        attrs.push(SubpictureAttr {
            lang_code: [lang_hi, lang_lo],
            code_extension,
        });
    }

    Ok(attrs)
}

/// Parses the part-of-title search pointer table.
fn parse_ptt_table(data: &[u8], sector: u32) -> Result<Vec<Vec<PttEntry>>, IfoError> {
    if sector == 0 {
        return Ok(Vec::new());
    }

    let table_offset = sector_to_byte(sector, data.len())?;
    let mut r = Reader::new(data);
    r.seek(table_offset)?;

    // Header: nr_of_srpts (2) + zero (2) + last_byte (4)
    let nr_of_srpts = r.read_u16()?;
    r.skip(2)?; // zero
    let last_byte = r.read_u32()?;

    #[allow(
        clippy::cast_possible_truncation,
        reason = "last_byte is a table-relative offset, always small enough for usize"
    )]
    let table_size = last_byte as usize + 1;

    // Read byte offsets for each VTS title's PTT array
    let mut ttu_offsets = Vec::with_capacity(usize::from(nr_of_srpts));
    for _ in 0..nr_of_srpts {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "PTT offsets are table-relative, always small enough for usize"
        )]
        let offset = r.read_u32()? as usize;
        ttu_offsets.push(offset);
    }

    // Parse each title's PTT entries
    let mut ptt_table = Vec::with_capacity(usize::from(nr_of_srpts));
    for (i, &offset) in ttu_offsets.iter().enumerate() {
        if offset >= table_size {
            return Err(IfoError::OffsetOutOfRange { offset, table_size });
        }

        // End of this title's entries: either the next title's offset or table end
        let end = if i + 1 < ttu_offsets.len() {
            ttu_offsets[i + 1]
        } else {
            table_size
        };

        if end < offset {
            ptt_table.push(Vec::new());
            continue;
        }

        let entry_bytes = end - offset;
        let chapter_count = entry_bytes / 4; // 4 bytes per PTT entry

        let abs_offset = table_offset + offset;
        r.seek(abs_offset)?;

        let mut ptts = Vec::with_capacity(chapter_count);
        for _ in 0..chapter_count {
            let pgcn = r.read_u16()?;
            let program_nr = r.read_u16()?;
            ptts.push(PttEntry {
                pgcn,
                pgn: program_nr,
            });
        }

        ptt_table.push(ptts);
    }

    Ok(ptt_table)
}

/// Parses the PGC information table.
fn parse_pgc_table(data: &[u8], sector: u32) -> Result<Vec<PgcEntry>, IfoError> {
    if sector == 0 {
        return Ok(Vec::new());
    }

    let table_offset = sector_to_byte(sector, data.len())?;
    let mut r = Reader::new(data);
    r.seek(table_offset)?;

    // Header: nr_of_pgci_srp (2) + zero (2) + last_byte (4)
    let nr_of_pgci_srp = r.read_u16()?;
    r.skip(2)?; // zero
    r.skip(4)?; // last_byte

    // Read PGC search pointers (8 bytes each)
    let mut pointers = Vec::with_capacity(usize::from(nr_of_pgci_srp));
    for _ in 0..nr_of_pgci_srp {
        let entry_id = r.read_u8()?;
        r.skip(1)?; // block_mode + block_type + zero
        r.skip(2)?; // ptl_id_mask
        #[allow(
            clippy::cast_possible_truncation,
            reason = "PGC start bytes are table-relative offsets, always small for usize"
        )]
        let pgc_start_byte = r.read_u32()? as usize;
        pointers.push((entry_id, pgc_start_byte));
    }

    // Parse each PGC
    let mut pgc_entries = Vec::with_capacity(pointers.len());
    for (entry_id, pgc_start_byte) in pointers {
        let pgc_abs = table_offset + pgc_start_byte;
        let pgc = parse_pgc(data, pgc_abs)?;
        pgc_entries.push(PgcEntry { entry_id, pgc });
    }

    Ok(pgc_entries)
}

/// Parses a single PGC at the given absolute byte offset.
fn parse_pgc(data: &[u8], pgc_offset: usize) -> Result<Pgc, IfoError> {
    let mut r = Reader::new(data);
    r.seek(pgc_offset)?;

    // PGC fixed fields (236 bytes)
    r.skip(2)?; // zero_1
    let nr_of_programs = r.read_u8()?;
    let nr_of_cells = r.read_u8()?;
    let playback_time = parse_dvd_time(&mut r)?;
    r.skip(4)?; // prohibited_ops

    // audio_control: 8 entries × 2 bytes = 16 bytes
    let mut audio_control = [0u16; 8];
    for ac in &mut audio_control {
        *ac = r.read_u16()?;
    }

    // subp_control: 32 entries × 4 bytes = 128 bytes
    let mut subp_control = [0u32; 32];
    for sc in &mut subp_control {
        *sc = r.read_u32()?;
    }

    // next/prev/goup PGC + playback mode + still time
    r.skip(8)?;

    r.skip(64)?; // palette[16]

    r.skip(2)?; // command_tbl_offset
    let program_map_offset = r.read_u16()?;
    let cell_playback_offset = r.read_u16()?;
    r.skip(2)?; // cell_position_offset

    // Parse program map
    let program_map = if nr_of_programs > 0 && program_map_offset > 0 {
        let map_abs = pgc_offset + usize::from(program_map_offset);
        r.seek(map_abs)?;
        let mut map = Vec::with_capacity(usize::from(nr_of_programs));
        for _ in 0..nr_of_programs {
            map.push(r.read_u8()?);
        }
        map
    } else {
        Vec::new()
    };

    // Parse cell playback table
    let cells = if nr_of_cells > 0 && cell_playback_offset > 0 {
        let cell_abs = pgc_offset + usize::from(cell_playback_offset);
        r.seek(cell_abs)?;
        let mut cells = Vec::with_capacity(usize::from(nr_of_cells));
        for _ in 0..nr_of_cells {
            cells.push(parse_cell_playback(&mut r)?);
        }
        cells
    } else {
        Vec::new()
    };

    Ok(Pgc {
        nr_of_programs,
        nr_of_cells,
        playback_time,
        audio_control,
        subp_control,
        program_map,
        cells,
    })
}

/// Parses a single cell playback entry (24 bytes).
fn parse_cell_playback(r: &mut Reader<'_>) -> Result<CellPlayback, IfoError> {
    let flags0 = r.read_u8()?;
    let block_mode = (flags0 >> 6) & 0x03;
    let block_type = (flags0 >> 4) & 0x03;

    r.skip(1)?; // flags1
    let still_time = r.read_u8()?;
    r.skip(1)?; // cell_cmd_nr

    let playback_time = parse_dvd_time(r)?;
    let first_sector = r.read_u32()?;
    r.skip(4)?; // first_ilvu_end_sector
    r.skip(4)?; // last_vobu_start_sector
    let last_sector = r.read_u32()?;

    Ok(CellPlayback {
        block_mode,
        block_type,
        still_time,
        playback_time,
        first_sector,
        last_sector,
    })
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Converts a sector number to a byte offset, checking bounds.
const fn sector_to_byte(sector: u32, file_size: usize) -> Result<usize, IfoError> {
    let byte_offset = sector as usize * SECTOR_SIZE;
    if byte_offset > file_size {
        return Err(IfoError::SectorOutOfRange {
            sector,
            byte_offset,
            file_size,
        });
    }
    Ok(byte_offset)
}

// ── Display helpers for stream attributes ───────────────────────────────

impl VideoAttr {
    /// Returns a human-readable description (e.g. "MPEG-2 NTSC 16:9 720×480").
    pub fn description(&self) -> String {
        let codec = match self.mpeg_version {
            0 => "MPEG-1",
            _ => "MPEG-2",
        };
        let system = match self.video_format {
            0 => "NTSC",
            _ => "PAL",
        };
        let aspect = match self.aspect_ratio {
            3 => "16:9",
            _ => "4:3",
        };
        let resolution = match (self.picture_size, self.video_format) {
            (0, 0) => "720\u{d7}480",
            (0, _) => "720\u{d7}576",
            (1, 0) => "704\u{d7}480",
            (1, _) => "704\u{d7}576",
            (2, 0) => "352\u{d7}480",
            (2, _) => "352\u{d7}576",
            (3, 0) => "352\u{d7}240",
            (3, _) => "352\u{d7}288",
            _ => "unknown",
        };
        format!("{codec} {system} {aspect} {resolution}")
    }
}

impl AudioAttr {
    /// Returns a human-readable description (e.g. "AC-3 5.1 (en)").
    pub fn description(&self) -> String {
        let codec = match self.format {
            0 => "AC-3",
            2 => "MPEG-1",
            3 => "MPEG-2",
            4 => "LPCM",
            6 => "DTS",
            _ => "Unknown",
        };
        let channels = match self.channels {
            0 => "mono",
            1 => "stereo",
            5 => "5.1",
            _ => "multi",
        };
        let lang = lang_code_string(self.lang_code);
        format!("{codec} {channels} ({lang})")
    }
}

impl SubpictureAttr {
    /// Returns a human-readable description (e.g. "Subtitle (en)").
    pub fn description(&self) -> String {
        let kind = match self.code_extension {
            5 => "CC",
            9 => "Forced",
            13 => "Commentary",
            _ => "Subtitle",
        };
        let lang = lang_code_string(self.lang_code);
        format!("{kind} ({lang})")
    }
}

/// Converts a 2-byte ISO 639 language code to a string.
fn lang_code_string(code: [u8; 2]) -> String {
    if code[0].is_ascii_alphabetic() && code[1].is_ascii_alphabetic() {
        String::from_utf8_lossy(&code).into_owned()
    } else {
        String::from("??")
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
#[allow(
    clippy::cast_possible_truncation,
    reason = "test builder values are small known constants"
)]
#[allow(
    clippy::redundant_pub_crate,
    reason = "pub(crate) is intentional — tests module is used by dvd/mod.rs tests"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "test builder method mirrors the full IFO cell structure"
)]
pub(crate) mod tests {
    use super::*;

    // ── BCD time tests ──────────────────────────────────────────────────

    #[test]
    fn bcd_decode_valid() {
        assert_eq!(
            decode_bcd(0x00, 0).expect("should decode 0x00"),
            0,
            "0x00 = 0"
        );
        assert_eq!(
            decode_bcd(0x59, 0).expect("should decode 0x59"),
            59,
            "0x59 = 59"
        );
        assert_eq!(
            decode_bcd(0x99, 0).expect("should decode 0x99"),
            99,
            "0x99 = 99"
        );
        assert_eq!(
            decode_bcd(0x12, 0).expect("should decode 0x12"),
            12,
            "0x12 = 12"
        );
    }

    #[test]
    fn bcd_decode_invalid() {
        assert!(decode_bcd(0xAF, 0).is_err(), "0xAF has invalid high nibble");
        assert!(decode_bcd(0x0A, 0).is_err(), "0x0A has invalid low nibble");
    }

    #[test]
    fn dvd_time_ntsc() {
        // a DVD title: 0x01 0x36 0x50 0xC8 → 1h36m50s + 8 frames @ 29.97fps
        let data = [0x01, 0x36, 0x50, 0xC8];
        let mut r = Reader::new(&data);
        let t = parse_dvd_time(&mut r).expect("should parse NTSC time");

        assert_eq!(t.hours, 1, "hours");
        assert_eq!(t.minutes, 36, "minutes");
        assert_eq!(t.seconds, 50, "seconds");
        assert_eq!(t.frames, 8, "frames");
        assert_eq!(t.frame_rate, FrameRate::Ntsc, "frame rate");

        let dur = t.to_duration();
        let expected = 5810.267;
        let diff = (dur.as_secs_f64() - expected).abs();
        assert!(
            diff < 0.01,
            "duration should be ~{expected}s, got {}",
            dur.as_secs_f64()
        );
    }

    #[test]
    fn dvd_time_pal() {
        // a PAL DVD title: 0x00 0x00 0x31 0x43 → 0h0m31s + 3 frames @ 25fps
        let data = [0x00, 0x00, 0x31, 0x43];
        let mut r = Reader::new(&data);
        let t = parse_dvd_time(&mut r).expect("should parse PAL time");

        assert_eq!(t.hours, 0, "hours");
        assert_eq!(t.minutes, 0, "minutes");
        assert_eq!(t.seconds, 31, "seconds");
        assert_eq!(t.frames, 3, "frames");
        assert_eq!(t.frame_rate, FrameRate::Pal, "frame rate");

        let dur = t.to_duration();
        let expected = 31.12;
        let diff = (dur.as_secs_f64() - expected).abs();
        assert!(
            diff < 0.01,
            "duration should be ~{expected}s, got {}",
            dur.as_secs_f64()
        );
    }

    #[test]
    fn dvd_time_zero() {
        let data = [0x00, 0x00, 0x00, 0x00];
        let mut r = Reader::new(&data);
        let t = parse_dvd_time(&mut r).expect("should parse zero time");
        assert!(t.is_zero(), "zero time should be zero");
        assert_eq!(
            t.to_duration(),
            Duration::ZERO,
            "zero time should be Duration::ZERO"
        );
    }

    #[test]
    fn dvd_time_ntsc_short() {
        // a TV-series DVD S1 VTS1: 0x00 0x00 0x13 0xD4 → 0h0m13s + 14 frames @ 29.97fps
        let data = [0x00, 0x00, 0x13, 0xD4];
        let mut r = Reader::new(&data);
        let t = parse_dvd_time(&mut r).expect("should parse a TV-series DVD time");

        assert_eq!(t.hours, 0, "hours");
        assert_eq!(t.minutes, 0, "minutes");
        assert_eq!(t.seconds, 13, "seconds");
        assert_eq!(t.frames, 14, "frames");
        assert_eq!(t.frame_rate, FrameRate::Ntsc, "frame rate");

        let expected = 13.467;
        let diff = (t.to_duration().as_secs_f64() - expected).abs();
        assert!(diff < 0.01, "duration should be ~{expected}s");
    }

    // ── VMG parsing tests ───────────────────────────────────────────────

    #[test]
    fn vmg_reject_invalid_magic() {
        let data = b"DVDVIDEO-VTS\x00\x00\x00\x00";
        let err = parse_vmg(data).expect_err("should reject VTS magic in VMG");
        assert!(
            matches!(
                err,
                IfoError::InvalidMagic {
                    expected: "DVDVIDEO-VMG",
                    ..
                }
            ),
            "error should be InvalidMagic"
        );
    }

    #[test]
    fn vmg_reject_truncated() {
        let err = parse_vmg(b"DVDVID").expect_err("should reject truncated file");
        assert!(
            matches!(err, IfoError::UnexpectedEof { .. }),
            "error should be UnexpectedEof"
        );
    }

    #[test]
    fn vmg_parse_simple() {
        let data = build_vmg(
            &[
                TitlePointer {
                    title_set_nr: 1,
                    vts_ttn: 1,
                    nr_of_ptts: 7,
                    nr_of_angles: 1,
                },
                TitlePointer {
                    title_set_nr: 1,
                    vts_ttn: 2,
                    nr_of_ptts: 1,
                    nr_of_angles: 1,
                },
                TitlePointer {
                    title_set_nr: 1,
                    vts_ttn: 3,
                    nr_of_ptts: 2,
                    nr_of_angles: 1,
                },
            ],
            1,
        );
        let vmg = parse_vmg(&data).expect("should parse simple VMG");

        assert_eq!(vmg.nr_of_title_sets, 1, "nr_of_title_sets");
        assert_eq!(vmg.titles.len(), 3, "title count");
        assert_eq!(vmg.titles[0].title_set_nr, 1, "title 0 VTS");
        assert_eq!(vmg.titles[0].vts_ttn, 1, "title 0 vts_ttn");
        assert_eq!(vmg.titles[0].nr_of_ptts, 7, "title 0 chapters");
        assert_eq!(vmg.titles[1].nr_of_ptts, 1, "title 1 chapters");
        assert_eq!(vmg.titles[2].nr_of_ptts, 2, "title 2 chapters");
    }

    #[test]
    fn vmg_parse_multi_vts() {
        let data = build_vmg(
            &[
                TitlePointer {
                    title_set_nr: 1,
                    vts_ttn: 1,
                    nr_of_ptts: 32,
                    nr_of_angles: 1,
                },
                TitlePointer {
                    title_set_nr: 2,
                    vts_ttn: 1,
                    nr_of_ptts: 1,
                    nr_of_angles: 1,
                },
                TitlePointer {
                    title_set_nr: 3,
                    vts_ttn: 1,
                    nr_of_ptts: 1,
                    nr_of_angles: 1,
                },
            ],
            3,
        );
        let vmg = parse_vmg(&data).expect("should parse multi-VTS VMG");

        assert_eq!(vmg.nr_of_title_sets, 3, "nr_of_title_sets");
        assert_eq!(vmg.titles[0].title_set_nr, 1, "title 0 VTS");
        assert_eq!(vmg.titles[1].title_set_nr, 2, "title 1 VTS");
        assert_eq!(vmg.titles[2].title_set_nr, 3, "title 2 VTS");
    }

    // ── VTS parsing tests ───────────────────────────────────────────────

    #[test]
    fn vts_reject_invalid_magic() {
        let data = b"DVDVIDEO-VMG\x00\x00\x00\x00";
        let err = parse_vts(data).expect_err("should reject VMG magic in VTS");
        assert!(
            matches!(
                err,
                IfoError::InvalidMagic {
                    expected: "DVDVIDEO-VTS",
                    ..
                }
            ),
            "error should be InvalidMagic"
        );
    }

    #[test]
    fn vts_parse_simple() {
        let data = VtsBuilder::new()
            .video(1, 0, 3, 0)
            .audio(0, 1, 0, *b"en")
            .subpicture(*b"en", 1)
            .pgc(
                PgcBuilder::new()
                    .programs(7)
                    .time(1, 36, 50, 8, FrameRate::Ntsc)
                    .cells_simple(8, 500)
                    .audio_available(&[0]),
            )
            .title_ptts(&[&[(1, 1), (1, 2), (1, 3), (1, 4), (1, 5), (1, 6), (1, 7)]])
            .build();

        let vts = parse_vts(&data).expect("should parse simple VTS");

        assert_eq!(vts.video_attr.mpeg_version, 1, "MPEG-2");
        assert_eq!(vts.video_attr.video_format, 0, "NTSC");
        assert_eq!(vts.video_attr.aspect_ratio, 3, "16:9");
        assert_eq!(vts.audio_attrs.len(), 1, "audio count");
        assert_eq!(vts.audio_attrs[0].format, 0, "AC-3");
        assert_eq!(vts.audio_attrs[0].channels, 1, "stereo");
        assert_eq!(vts.audio_attrs[0].lang_code, *b"en", "language");
        assert_eq!(vts.subp_attrs.len(), 1, "subpicture count");
        assert_eq!(vts.subp_attrs[0].lang_code, *b"en", "subtitle language");
        assert_eq!(vts.pgc_table.len(), 1, "PGC count");
        assert_eq!(vts.pgc_table[0].pgc.nr_of_programs, 7, "programs");
        assert_eq!(vts.pgc_table[0].pgc.nr_of_cells, 8, "cells");
        assert_eq!(vts.ptt_table.len(), 1, "PTT title count");
        assert_eq!(vts.ptt_table[0].len(), 7, "PTT chapter count");
    }

    #[test]
    fn vts_parse_multi_audio() {
        let data = VtsBuilder::new()
            .video(1, 0, 3, 0)
            .audio(0, 1, 0, *b"en")
            .audio(0, 5, 0, *b"en")
            .audio(0, 1, 0, *b"fr")
            .pgc(
                PgcBuilder::new()
                    .programs(1)
                    .time(0, 5, 0, 0, FrameRate::Ntsc)
                    .cells_simple(1, 500)
                    .audio_available(&[0, 1]),
            )
            .title_ptts(&[&[(1, 1)]])
            .build();

        let vts = parse_vts(&data).expect("should parse multi-audio VTS");

        assert_eq!(vts.audio_attrs.len(), 3, "audio count");
        assert_eq!(vts.audio_attrs[0].channels, 1, "audio 0: stereo");
        assert_eq!(vts.audio_attrs[1].channels, 5, "audio 1: 5.1");
        assert_eq!(vts.audio_attrs[2].lang_code, *b"fr", "audio 2: French");

        let pgc = &vts.pgc_table[0].pgc;
        assert!(pgc.audio_control[0] & 0x8000 != 0, "audio 0 available");
        assert!(pgc.audio_control[1] & 0x8000 != 0, "audio 1 available");
        assert_eq!(pgc.audio_control[2], 0, "audio 2 not available");
    }

    #[test]
    fn vts_parse_cell_sectors() {
        let data = VtsBuilder::new()
            .video(1, 0, 3, 0)
            .audio(0, 1, 0, *b"en")
            .pgc(
                PgcBuilder::new()
                    .programs(2)
                    .cell(0, 5, 0, 0, FrameRate::Ntsc, 0, 999, 0, 0)
                    .cell(0, 5, 0, 0, FrameRate::Ntsc, 1000, 1999, 0, 0)
                    .time(0, 10, 0, 0, FrameRate::Ntsc)
                    .audio_available(&[0]),
            )
            .title_ptts(&[&[(1, 1), (1, 2)]])
            .build();

        let vts = parse_vts(&data).expect("should parse cell sectors");
        let pgc = &vts.pgc_table[0].pgc;

        assert_eq!(pgc.cells.len(), 2, "cell count");
        assert_eq!(pgc.cells[0].first_sector, 0, "cell 0 first_sector");
        assert_eq!(pgc.cells[0].last_sector, 999, "cell 0 last_sector");
        assert_eq!(pgc.cells[1].first_sector, 1000, "cell 1 first_sector");
        assert_eq!(pgc.cells[1].last_sector, 1999, "cell 1 last_sector");
    }

    #[test]
    fn vts_parse_angle_cells() {
        let data = VtsBuilder::new()
            .video(1, 0, 3, 0)
            .audio(0, 1, 0, *b"en")
            .pgc(
                PgcBuilder::new()
                    .programs(1)
                    .cell(0, 5, 0, 0, FrameRate::Ntsc, 0, 999, 1, 1)
                    .cell(0, 5, 0, 0, FrameRate::Ntsc, 1000, 1999, 3, 1)
                    .time(0, 5, 0, 0, FrameRate::Ntsc),
            )
            .title_ptts(&[&[(1, 1)]])
            .build();

        let vts = parse_vts(&data).expect("should parse angle cells");
        let pgc = &vts.pgc_table[0].pgc;

        assert_eq!(pgc.cells.len(), 2, "cell count");
        assert_eq!(pgc.cells[0].block_mode, 1, "cell 0: first in angle block");
        assert_eq!(pgc.cells[0].block_type, 1, "cell 0: angle block");
        assert_eq!(pgc.cells[1].block_mode, 3, "cell 1: last in angle block");
        assert_eq!(pgc.cells[1].block_type, 1, "cell 1: angle block");
    }

    // ── Test builders ───────────────────────────────────────────────────

    /// Builds a minimal VMG (`VIDEO_TS.IFO`) binary for testing.
    fn build_vmg(titles: &[TitlePointer], nr_of_title_sets: u16) -> Vec<u8> {
        let mut buf = vec![0u8; SECTOR_SIZE * 2 + titles.len() * 12 + 8];

        // Magic
        buf[..12].copy_from_slice(b"DVDVIDEO-VMG");

        // nr_of_title_sets at 0x03E
        buf[0x3E..0x40].copy_from_slice(&nr_of_title_sets.to_be_bytes());

        // tt_srpt sector at 0x0C4 — point to sector 1
        buf[0xC4..0xC8].copy_from_slice(&1u32.to_be_bytes());

        // Build tt_srpt at sector 1
        let tt_offset = SECTOR_SIZE;
        let nr = titles.len() as u16;
        buf[tt_offset..tt_offset + 2].copy_from_slice(&nr.to_be_bytes());
        let last_byte = (8 + titles.len() * 12 - 1) as u32;
        buf[tt_offset + 4..tt_offset + 8].copy_from_slice(&last_byte.to_be_bytes());

        for (i, t) in titles.iter().enumerate() {
            let base = tt_offset + 8 + i * 12;
            buf[base] = 0; // pb_ty
            buf[base + 1] = t.nr_of_angles;
            buf[base + 2..base + 4].copy_from_slice(&t.nr_of_ptts.to_be_bytes());
            buf[base + 4..base + 6].copy_from_slice(&0u16.to_be_bytes());
            buf[base + 6] = t.title_set_nr;
            buf[base + 7] = t.vts_ttn;
            buf[base + 8..base + 12].copy_from_slice(&0u32.to_be_bytes());
        }

        buf
    }

    /// Builder for constructing VTS IFO binaries for testing.
    pub struct VtsBuilder {
        video: (u8, u8, u8, u8),
        audio: Vec<(u8, u8, u8, [u8; 2])>,
        subp: Vec<([u8; 2], u8)>,
        pgcs: Vec<(u8, PgcData)>,
        ptts: Vec<Vec<(u16, u16)>>,
    }

    struct PgcData {
        nr_of_programs: u8,
        time: [u8; 4],
        audio_control: [u16; 8],
        subp_control: [u32; 32],
        cells: Vec<CellData>,
    }

    struct CellData {
        time: [u8; 4],
        first_sector: u32,
        last_sector: u32,
        block_mode: u8,
        block_type: u8,
    }

    /// Builder for constructing PGC data for testing.
    pub struct PgcBuilder {
        entry_id: u8,
        nr_of_programs: u8,
        time: [u8; 4],
        audio_control: [u16; 8],
        subp_control: [u32; 32],
        cells: Vec<CellData>,
    }

    impl PgcBuilder {
        pub fn new() -> Self {
            Self {
                entry_id: 0x81,
                nr_of_programs: 0,
                time: [0; 4],
                audio_control: [0; 8],
                subp_control: [0; 32],
                cells: Vec::new(),
            }
        }

        pub fn entry_id(mut self, id: u8) -> Self {
            self.entry_id = id;
            self
        }

        pub fn programs(mut self, n: u8) -> Self {
            self.nr_of_programs = n;
            self
        }

        pub fn time(mut self, h: u8, m: u8, s: u8, f: u8, rate: FrameRate) -> Self {
            let rate_bits: u8 = match rate {
                FrameRate::Pal => 1,
                FrameRate::Ntsc => 3,
            };
            self.time = [
                (h / 10) << 4 | (h % 10),
                (m / 10) << 4 | (m % 10),
                (s / 10) << 4 | (s % 10),
                (rate_bits << 6) | ((f / 10) << 4) | (f % 10),
            ];
            self
        }

        pub fn audio_available(mut self, indices: &[usize]) -> Self {
            for &i in indices {
                if i < 8 {
                    self.audio_control[i] = 0x8000 | (i as u16);
                }
            }
            self
        }

        pub fn subp_available(mut self, indices: &[usize]) -> Self {
            for &i in indices {
                if i < 32 {
                    self.subp_control[i] = 0x8000_0000;
                }
            }
            self
        }

        /// Adds a cell with full control over all fields.
        pub fn cell(
            mut self,
            h: u8,
            m: u8,
            s: u8,
            f: u8,
            rate: FrameRate,
            first_sector: u32,
            last_sector: u32,
            block_mode: u8,
            block_type: u8,
        ) -> Self {
            let rate_bits: u8 = match rate {
                FrameRate::Pal => 1,
                FrameRate::Ntsc => 3,
            };
            self.cells.push(CellData {
                time: [
                    (h / 10) << 4 | (h % 10),
                    (m / 10) << 4 | (m % 10),
                    (s / 10) << 4 | (s % 10),
                    (rate_bits << 6) | ((f / 10) << 4) | (f % 10),
                ],
                first_sector,
                last_sector,
                block_mode,
                block_type,
            });
            if self.nr_of_programs == 0 {
                self.nr_of_programs = 1;
            }
            self
        }

        /// Adds N simple cells with sequential sectors.
        pub fn cells_simple(mut self, n: u8, sector_size: u32) -> Self {
            for i in 0..n {
                let start = u32::from(i) * sector_size;
                let end = start + sector_size - 1;
                self.cells.push(CellData {
                    time: self.time,
                    first_sector: start,
                    last_sector: end,
                    block_mode: 0,
                    block_type: 0,
                });
            }
            self
        }
    }

    impl VtsBuilder {
        pub fn new() -> Self {
            Self {
                video: (1, 0, 3, 0),
                audio: Vec::new(),
                subp: Vec::new(),
                pgcs: Vec::new(),
                ptts: Vec::new(),
            }
        }

        pub fn video(mut self, mpeg: u8, format: u8, aspect: u8, size: u8) -> Self {
            self.video = (mpeg, format, aspect, size);
            self
        }

        pub fn audio(mut self, format: u8, channels: u8, sample_rate: u8, lang: [u8; 2]) -> Self {
            self.audio.push((format, channels, sample_rate, lang));
            self
        }

        pub fn subpicture(mut self, lang: [u8; 2], code_ext: u8) -> Self {
            self.subp.push((lang, code_ext));
            self
        }

        pub fn pgc(mut self, builder: PgcBuilder) -> Self {
            let data = PgcData {
                nr_of_programs: builder.nr_of_programs,
                time: builder.time,
                audio_control: builder.audio_control,
                subp_control: builder.subp_control,
                cells: builder.cells,
            };
            self.pgcs.push((builder.entry_id, data));
            self
        }

        pub fn title_ptts(mut self, ptts: &[&[(u16, u16)]]) -> Self {
            self.ptts = ptts.iter().map(|p| p.to_vec()).collect();
            self
        }

        #[allow(
            clippy::cast_possible_truncation,
            reason = "test builder values are small known constants"
        )]
        pub fn build(&self) -> Vec<u8> {
            // Build PGC bodies first to know sizes
            let mut pgc_bodies: Vec<Vec<u8>> = Vec::new();
            for (_, pgc_data) in &self.pgcs {
                pgc_bodies.push(Self::build_pgc_body(pgc_data));
            }

            // Calculate PGC table size
            let pgci_header_size = 8;
            let pgci_srp_size = self.pgcs.len() * 8;
            let pgc_data_size: usize = pgc_bodies.iter().map(Vec::len).sum();
            let pgcit_total = pgci_header_size + pgci_srp_size + pgc_data_size;

            // Calculate PTT table size
            let ptt_header_size = 8;
            let ptt_offsets_size = self.ptts.len() * 4;
            let ptt_entries_size: usize = self.ptts.iter().map(|p| p.len() * 4).sum();
            let ptt_total = ptt_header_size + ptt_offsets_size + ptt_entries_size;

            // Total: 3 sectors + tables
            let total_size = 3 * SECTOR_SIZE + ptt_total + pgcit_total;
            let mut buf = vec![0u8; total_size];

            // ── Header (sector 0) ──
            buf[..12].copy_from_slice(b"DVDVIDEO-VTS");

            // Video attr at 0x200
            let (mpeg, vfmt, aspect, psize) = self.video;
            let video_raw: u16 = (u16::from(mpeg) << 14)
                | (u16::from(vfmt) << 12)
                | (u16::from(aspect) << 10)
                | (u16::from(psize) << 2);
            buf[0x200..0x202].copy_from_slice(&video_raw.to_be_bytes());

            // Audio count at 0x203
            buf[0x203] = self.audio.len() as u8;
            for (i, &(fmt, ch, sr, lang)) in self.audio.iter().enumerate() {
                let base = 0x204 + i * 8;
                buf[base] = fmt << 5;
                buf[base + 1] = (sr << 4) | ch;
                buf[base + 2] = lang[0];
                buf[base + 3] = lang[1];
            }

            // Subpicture count at 0x255
            buf[0x255] = self.subp.len() as u8;
            for (i, &(lang, code_ext)) in self.subp.iter().enumerate() {
                let base = 0x256 + i * 6;
                buf[base + 2] = lang[0];
                buf[base + 3] = lang[1];
                buf[base + 5] = code_ext;
            }

            // PTT sector at 0x0C8 — sector 1
            buf[0xC8..0xCC].copy_from_slice(&1u32.to_be_bytes());

            // PGC sector at 0x0CC — sector 2
            buf[0xCC..0xD0].copy_from_slice(&2u32.to_be_bytes());

            // ── PTT table (sector 1) ──
            let ptt_base = SECTOR_SIZE;
            buf[ptt_base..ptt_base + 2].copy_from_slice(&(self.ptts.len() as u16).to_be_bytes());
            let ptt_last_byte = (ptt_total - 1) as u32;
            buf[ptt_base + 4..ptt_base + 8].copy_from_slice(&ptt_last_byte.to_be_bytes());

            let mut entry_offset = ptt_offsets_size + ptt_header_size;
            for (i, ptt) in self.ptts.iter().enumerate() {
                let off_pos = ptt_base + 8 + i * 4;
                buf[off_pos..off_pos + 4].copy_from_slice(&(entry_offset as u32).to_be_bytes());
                let abs_entry = ptt_base + entry_offset;
                for (j, &(pgcn, pgn)) in ptt.iter().enumerate() {
                    let e = abs_entry + j * 4;
                    buf[e..e + 2].copy_from_slice(&pgcn.to_be_bytes());
                    buf[e + 2..e + 4].copy_from_slice(&pgn.to_be_bytes());
                }
                entry_offset += ptt.len() * 4;
            }

            // ── PGC table (sector 2) ──
            let pgcit_base = SECTOR_SIZE * 2;
            buf[pgcit_base..pgcit_base + 2]
                .copy_from_slice(&(self.pgcs.len() as u16).to_be_bytes());
            let pgcit_last_byte = (pgcit_total - 1) as u32;
            buf[pgcit_base + 4..pgcit_base + 8].copy_from_slice(&pgcit_last_byte.to_be_bytes());

            let mut pgc_data_offset = pgci_header_size + pgci_srp_size;
            for (i, &(entry_id, _)) in self.pgcs.iter().enumerate() {
                let srp_base = pgcit_base + 8 + i * 8;
                buf[srp_base] = entry_id;
                buf[srp_base + 4..srp_base + 8]
                    .copy_from_slice(&(pgc_data_offset as u32).to_be_bytes());

                let body = &pgc_bodies[i];
                let pgc_abs = pgcit_base + pgc_data_offset;
                buf[pgc_abs..pgc_abs + body.len()].copy_from_slice(body);

                pgc_data_offset += body.len();
            }

            buf
        }

        fn build_pgc_body(pgc: &PgcData) -> Vec<u8> {
            let nr_of_cells = pgc.cells.len() as u8;
            let nr_of_programs = pgc.nr_of_programs;

            let program_map_offset: u16 = 236;
            let cell_playback_offset: u16 = program_map_offset + u16::from(nr_of_programs);
            let total_size = usize::from(cell_playback_offset) + usize::from(nr_of_cells) * 24;

            let mut buf = vec![0u8; total_size];

            buf[2] = nr_of_programs;
            buf[3] = nr_of_cells;
            buf[4..8].copy_from_slice(&pgc.time);

            for (i, &ac) in pgc.audio_control.iter().enumerate() {
                let offset = 12 + i * 2;
                buf[offset..offset + 2].copy_from_slice(&ac.to_be_bytes());
            }

            for (i, &sc) in pgc.subp_control.iter().enumerate() {
                let offset = 28 + i * 4;
                buf[offset..offset + 4].copy_from_slice(&sc.to_be_bytes());
            }

            buf[230..232].copy_from_slice(&program_map_offset.to_be_bytes());
            buf[232..234].copy_from_slice(&cell_playback_offset.to_be_bytes());

            // Program map: sequential starting cell numbers
            if nr_of_programs > 0 && nr_of_cells > 0 {
                let cells_per_prog = nr_of_cells / nr_of_programs;
                for i in 0..nr_of_programs {
                    let start_cell = i * cells_per_prog + 1;
                    buf[usize::from(program_map_offset) + usize::from(i)] = start_cell;
                }
            }

            // Cell playback: 24 bytes each
            for (i, cell) in pgc.cells.iter().enumerate() {
                let base = usize::from(cell_playback_offset) + i * 24;
                buf[base] = (cell.block_mode << 6) | (cell.block_type << 4);
                buf[base + 4..base + 8].copy_from_slice(&cell.time);
                buf[base + 8..base + 12].copy_from_slice(&cell.first_sector.to_be_bytes());
                buf[base + 20..base + 24].copy_from_slice(&cell.last_sector.to_be_bytes());
            }

            buf
        }
    }
}
