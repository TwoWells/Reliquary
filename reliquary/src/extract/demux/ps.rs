// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! MPEG Program Stream demuxer for DVD VOB files.
//!
//! DVD VOB files are MPEG-2 Program Streams — sequences of 2048-byte packs,
//! each containing a pack header and one PES (Packetized Elementary Stream)
//! packet. The demuxer parses packs, extracts PES payloads, routes private
//! stream 1 sub-streams by codec type, strips container sub-headers, and
//! produces [`DemuxedUnit`] values ready for downstream codec framers.
//!
//! # References
//!
//! - ISO/IEC 13818-1 (MPEG-2 Systems)
//! - `reference/MPEG_PS.md` in the planning repository

use std::collections::{HashSet, VecDeque};
use std::io::{self, Read, Seek, SeekFrom};

use thiserror::Error;

use super::{DemuxedUnit, StreamId};

// ── Constants ────────────────────────────────────────────────────────────

/// DVD sector size in bytes. Each pack occupies exactly one sector.
const SECTOR_SIZE: usize = 2048;

/// Pack start code — begins every MPEG-2 pack.
const PACK_START_CODE: [u8; 4] = [0x00, 0x00, 0x01, 0xBA];

/// System header start code — optional, appears after pack header.
#[allow(dead_code, reason = "format constant used in tests")]
const SYSTEM_HEADER_CODE: u8 = 0xBB;

/// Program end code — marks the end of the program stream.
const PROGRAM_END_CODE: [u8; 4] = [0x00, 0x00, 0x01, 0xB9];

/// PES start code prefix (3 bytes).
const PES_PREFIX: [u8; 3] = [0x00, 0x00, 0x01];

/// Private stream 1 — carries AC-3, DTS, LPCM, and `VobSub`.
const PRIVATE_STREAM_1: u8 = 0xBD;

/// Padding stream — filler bytes, no content.
#[allow(dead_code, reason = "format constant used in tests")]
const PADDING_STREAM: u8 = 0xBE;

/// Private stream 2 — NAV packs (PCI/DSI navigation data).
#[allow(dead_code, reason = "format constant used in tests")]
const PRIVATE_STREAM_2: u8 = 0xBF;

/// Sub-header size for AC-3 and DTS audio in private stream 1.
const AUDIO_SUB_HEADER: usize = 4;

/// Sub-header size for LPCM audio in private stream 1.
const LPCM_SUB_HEADER: usize = 7;

/// Pack header size in bytes (MPEG-2, excluding stuffing).
const PACK_HEADER_BASE: usize = 14;

// ── Errors ───────────────────────────────────────────────────────────────

/// Errors from MPEG-PS demuxing.
#[derive(Debug, Error)]
pub enum PsError {
    /// An I/O error occurred reading from the underlying source.
    #[error("I/O error reading sector {sector}: {source}")]
    Io {
        /// The sector that was being read.
        sector: u64,
        /// The underlying I/O error.
        source: io::Error,
    },

    /// A sector does not begin with the expected pack start code.
    #[error("invalid pack start code at sector {sector}")]
    InvalidPackStart {
        /// The sector number (0-based).
        sector: u64,
    },

    /// An MPEG-1 pack was encountered (only MPEG-2 is supported).
    #[error("MPEG-1 pack at sector {sector} (only MPEG-2 is supported)")]
    Mpeg1NotSupported {
        /// The sector number (0-based).
        sector: u64,
    },
}

// ── Demuxer ──────────────────────────────────────────────────────────────

/// MPEG Program Stream demuxer.
///
/// Reads 2048-byte packs from a DVD VOB stream and produces [`DemuxedUnit`]
/// values — one per PES packet containing audio, video, or subtitle data.
///
/// NAV packs (private stream 2), padding, and system headers are silently
/// skipped. An optional stream filter limits which stream IDs are returned.
pub struct PsDemuxer<R> {
    reader: R,
    filter: Option<HashSet<u16>>,
    sector_count: u64,
    buf: [u8; SECTOR_SIZE],
    pending: VecDeque<DemuxedUnit>,
}

impl<R: Read> PsDemuxer<R> {
    /// Creates a new demuxer that returns all elementary streams.
    #[must_use]
    pub const fn new(reader: R) -> Self {
        Self {
            reader,
            filter: None,
            sector_count: 0,
            buf: [0u8; SECTOR_SIZE],
            pending: VecDeque::new(),
        }
    }

    /// Creates a new demuxer that returns only streams in the given filter
    /// set.
    ///
    /// Streams not in the set are parsed (for correct sector advancement)
    /// but their payloads are discarded.
    #[must_use]
    pub const fn with_filter(reader: R, filter: HashSet<u16>) -> Self {
        Self {
            reader,
            filter: Some(filter),
            sector_count: 0,
            buf: [0u8; SECTOR_SIZE],
            pending: VecDeque::new(),
        }
    }

    /// Returns the next demuxed elementary stream unit.
    ///
    /// Reads sectors until a content PES packet is found (skipping NAV
    /// packs, padding, and filtered-out streams). Returns `Ok(None)` at end
    /// of stream.
    ///
    /// # Errors
    ///
    /// Returns [`PsError`] on I/O errors, invalid pack structure, or
    /// unsupported MPEG-1 packs.
    pub fn next_unit(&mut self) -> Result<Option<DemuxedUnit>, PsError> {
        // Return buffered units first (rare: multiple content PES in one
        // sector).
        if let Some(unit) = self.pending.pop_front() {
            return Ok(Some(unit));
        }

        loop {
            self.sector_count += 1;
            if !self.read_sector()? {
                self.sector_count -= 1;
                return Ok(None);
            }
            self.parse_sector()?;

            if let Some(unit) = self.pending.pop_front() {
                return Ok(Some(unit));
            }
        }
    }

    /// Returns the number of sectors read so far.
    #[must_use]
    pub const fn sector_count(&self) -> u64 {
        self.sector_count
    }

    /// Reads one sector into the internal buffer.
    ///
    /// Returns `true` if a full sector was read, `false` at clean EOF.
    fn read_sector(&mut self) -> Result<bool, PsError> {
        // Read the first byte to distinguish clean EOF from truncated data.
        let n = self
            .reader
            .read(&mut self.buf[..1])
            .map_err(|e| PsError::Io {
                sector: self.sector_count,
                source: e,
            })?;
        if n == 0 {
            return Ok(false);
        }
        self.reader
            .read_exact(&mut self.buf[1..])
            .map_err(|e| PsError::Io {
                sector: self.sector_count,
                source: e,
            })?;
        Ok(true)
    }

    /// Parses the current sector buffer and pushes content units to
    /// `self.pending`.
    fn parse_sector(&mut self) -> Result<(), PsError> {
        let sector = self.sector_count;

        // Program end code — treat as end of stream.
        if self.buf[..4] == PROGRAM_END_CODE {
            return Ok(());
        }

        // Verify pack start code.
        if self.buf[..4] != PACK_START_CODE {
            return Err(PsError::InvalidPackStart { sector });
        }

        // Verify MPEG-2 marker (bits 7:6 of byte 4 = '01').
        if (self.buf[4] & 0xC0) != 0x40 {
            return Err(PsError::Mpeg1NotSupported { sector });
        }

        // Pack stuffing length (lower 3 bits of byte 13).
        let stuffing = usize::from(self.buf[13] & 0x07);
        let mut offset = PACK_HEADER_BASE + stuffing;

        // Walk PES packets within the sector.
        while offset + 6 <= SECTOR_SIZE {
            if self.buf[offset..offset + 3] != PES_PREFIX {
                break;
            }

            let stream_id = self.buf[offset + 3];
            let pes_length = u16::from_be_bytes([self.buf[offset + 4], self.buf[offset + 5]]);
            let pes_end = offset + 6 + usize::from(pes_length);

            if pes_end > SECTOR_SIZE {
                break;
            }

            match stream_id {
                PRIVATE_STREAM_1 => {
                    self.emit_private_stream_1(offset + 6, pes_end);
                }
                0xC0..=0xEF => {
                    self.emit_mpeg_pes(stream_id, offset + 6, pes_end);
                }
                // Padding, NAV packs, system headers, unknown — skip.
                _ => {}
            }

            offset = pes_end;
        }

        Ok(())
    }

    /// Parses a private stream 1 PES packet and pushes the unit if it
    /// passes the filter.
    fn emit_private_stream_1(&mut self, header_start: usize, pes_end: usize) {
        let Some((pts, dts, payload_start)) = parse_pes_header(&self.buf, header_start, pes_end)
        else {
            return;
        };

        if payload_start >= pes_end {
            return;
        }

        let sub_id = self.buf[payload_start];
        let pid = pack_ps_id(PRIVATE_STREAM_1, sub_id);

        if self.filter.as_ref().is_some_and(|f| !f.contains(&pid)) {
            return;
        }

        // Sub-header size by sub-stream ID range.
        #[allow(
            clippy::match_same_arms,
            reason = "explicit sub-stream ranges document the DVD PS format"
        )]
        let sub_header = match sub_id {
            0x20..=0x3F => 1,                              // VobSub
            0x80..=0x8F | 0x98..=0x9F => AUDIO_SUB_HEADER, // AC-3 / DTS
            0xA0..=0xA7 => LPCM_SUB_HEADER,                // LPCM
            _ => 1,                                        // unknown
        };

        let data_start = payload_start + sub_header;
        if data_start > pes_end {
            return;
        }

        self.pending.push_back(DemuxedUnit {
            stream_id: StreamId {
                pid,
                coding_type: ps_coding_type(PRIVATE_STREAM_1, sub_id),
            },
            pts,
            dts,
            payload: self.buf[data_start..pes_end].to_vec(),
        });
    }

    /// Parses an MPEG audio/video PES packet and pushes the unit if it
    /// passes the filter.
    fn emit_mpeg_pes(&mut self, stream_id: u8, header_start: usize, pes_end: usize) {
        let Some((pts, dts, payload_start)) = parse_pes_header(&self.buf, header_start, pes_end)
        else {
            return;
        };

        let pid = pack_ps_id(stream_id, 0);

        if self.filter.as_ref().is_some_and(|f| !f.contains(&pid)) {
            return;
        }

        if payload_start > pes_end {
            return;
        }

        self.pending.push_back(DemuxedUnit {
            stream_id: StreamId {
                pid,
                coding_type: ps_coding_type(stream_id, 0),
            },
            pts,
            dts,
            payload: self.buf[payload_start..pes_end].to_vec(),
        });
    }
}

// ── PS stream helpers ────────────────────────────────────────────────────

/// Packs a PS `stream_id` and `sub_id` into the [`StreamId::pid`] field.
///
/// DVD PS streams are identified by `(stream_id << 8) | sub_id`. For MPEG
/// video/audio (no sub-stream), `sub_id` is 0.
const fn pack_ps_id(stream_id: u8, sub_id: u8) -> u16 {
    (stream_id as u16) << 8 | sub_id as u16
}

/// Derives the MPEG coding type from a DVD PS stream.
///
/// Maps the PES `stream_id` and private stream 1 `sub_id` to the coding
/// type used by the framing layer.
const fn ps_coding_type(stream_id: u8, sub_id: u8) -> u8 {
    match stream_id {
        0xE0..=0xEF => 0x02, // MPEG-2 video
        0xC0..=0xDF => 0x04, // MPEG audio
        0xBD => match sub_id {
            0x80..=0x87 => 0x81, // AC-3
            0x88..=0x9F => 0x82, // DTS
            0xA0..=0xA7 => 0x80, // LPCM
            _ => 0x00,           // subtitle / unknown
        },
        _ => 0x00,
    }
}

// ── PES header parsing ───────────────────────────────────────────────────

/// Parses a PES optional header, returning (PTS, DTS, payload offset).
///
/// `start` is the offset of the first byte after the 6-byte PES fixed
/// header (i.e. `flags_1`). Returns `None` if the header is too short or
/// has an invalid MPEG-2 marker.
fn parse_pes_header(
    buf: &[u8],
    start: usize,
    end: usize,
) -> Option<(Option<u64>, Option<u64>, usize)> {
    // Need at least 3 bytes: flags_1 + flags_2 + header_data_length.
    if start + 3 > end {
        return None;
    }

    // Verify MPEG-2 PES marker (bits 7:6 of flags_1 = '10').
    if (buf[start] & 0xC0) != 0x80 {
        return None;
    }

    let flags_2 = buf[start + 1];
    let header_data_len = usize::from(buf[start + 2]);
    let payload_start = start + 3 + header_data_len;

    if payload_start > end {
        return None;
    }

    let pts_dts_flags = (flags_2 >> 6) & 0x03;

    let mut pts = None;
    let mut dts = None;

    match pts_dts_flags {
        0b10 if start + 8 <= end => {
            pts = Some(parse_timestamp(&buf[start + 3..start + 8]));
        }
        0b11 if start + 13 <= end => {
            pts = Some(parse_timestamp(&buf[start + 3..start + 8]));
            dts = Some(parse_timestamp(&buf[start + 8..start + 13]));
        }
        _ => {}
    }

    Some((pts, dts, payload_start))
}

/// Decodes a 33-bit MPEG timestamp from a 5-byte PTS/DTS field.
///
/// Layout (MSB first):
/// ```text
/// prefix[3:0]  TS[32:30]  '1'  TS[29:22]  TS[21:15]  '1'  TS[14:7]  TS[6:0]  '1'
/// ```
fn parse_timestamp(data: &[u8]) -> u64 {
    let bits_32_30 = u64::from((data[0] >> 1) & 0x07);
    let bits_29_22 = u64::from(data[1]);
    let bits_21_15 = u64::from((data[2] >> 1) & 0x7F);
    let bits_14_7 = u64::from(data[3]);
    let bits_6_0 = u64::from((data[4] >> 1) & 0x7F);

    (bits_32_30 << 30) | (bits_29_22 << 22) | (bits_21_15 << 15) | (bits_14_7 << 7) | bits_6_0
}

// ── VOB file set ─────────────────────────────────────────────────────────

/// Chains multiple readers into a single contiguous byte stream.
///
/// Used to treat a multi-file VOB set (`VTS_nn_1.VOB`, `VTS_nn_2.VOB`, ...)
/// as a single logical stream for the demuxer.
pub struct ChainRead<R> {
    readers: Vec<R>,
    current: usize,
}

impl<R> ChainRead<R> {
    /// Creates a chain reader from a list of sources in order.
    #[must_use]
    pub const fn new(readers: Vec<R>) -> Self {
        Self {
            readers,
            current: 0,
        }
    }
}

impl<R: Read> Read for ChainRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.current < self.readers.len() {
            let n = self.readers[self.current].read(buf)?;
            if n > 0 {
                return Ok(n);
            }
            self.current += 1;
        }
        Ok(0)
    }
}

// ── Cell reader ──────────────────────────────────────────────────────────

/// A sector range within a VOB file set, typically from a PGC cell.
#[derive(Debug, Clone, Copy)]
pub struct SectorRange {
    /// First sector (inclusive).
    pub first_sector: u32,
    /// Last sector (inclusive).
    pub last_sector: u32,
}

/// Reads specific sector ranges from a seekable VOB source.
///
/// Given a list of [`SectorRange`] values (e.g. derived from PGC cell
/// playback entries), this reader yields only the bytes within those
/// ranges, in order. When one range is exhausted the reader seeks to the
/// next.
pub struct CellReader<R> {
    source: R,
    ranges: Vec<SectorRange>,
    range_idx: usize,
    remaining: u64,
    seeked: bool,
}

impl<R: Read + Seek> CellReader<R> {
    /// Creates a cell reader for the given sector ranges.
    #[must_use]
    pub const fn new(source: R, ranges: Vec<SectorRange>) -> Self {
        Self {
            source,
            ranges,
            range_idx: 0,
            remaining: 0,
            seeked: false,
        }
    }

    /// Seeks to the start of the current range if not already positioned.
    fn ensure_seeked(&mut self) -> io::Result<()> {
        if !self.seeked && self.range_idx < self.ranges.len() {
            let range = self.ranges[self.range_idx];
            let byte_start = u64::from(range.first_sector) * SECTOR_SIZE as u64;
            let byte_end = (u64::from(range.last_sector) + 1) * SECTOR_SIZE as u64;
            self.source.seek(SeekFrom::Start(byte_start))?;
            self.remaining = byte_end - byte_start;
            self.seeked = true;
        }
        Ok(())
    }
}

impl<R: Read + Seek> Read for CellReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.range_idx >= self.ranges.len() {
                return Ok(0);
            }

            self.ensure_seeked()?;

            if self.remaining == 0 {
                self.range_idx += 1;
                self.seeked = false;
                continue;
            }

            let limit = usize::try_from(self.remaining).unwrap_or(usize::MAX);
            let to_read = buf.len().min(limit);
            let n = self.source.read(&mut buf[..to_read])?;
            if n == 0 {
                return Ok(0);
            }
            self.remaining -= n as u64;
            return Ok(n);
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
#[allow(
    clippy::cast_possible_truncation,
    reason = "test fixtures use small values that always fit"
)]
mod tests {
    use std::io::Cursor;

    use super::*;

    // ── Fixture builders ─────────────────────────────────────────────

    /// Writes a valid MPEG-2 pack header into `sector[0..14+stuffing]`.
    fn write_pack_header(sector: &mut [u8; SECTOR_SIZE], stuffing: u8) {
        sector[0..4].copy_from_slice(&PACK_START_CODE);
        // Byte 4: MPEG-2 marker '01' in bits 7:6.
        sector[4] = 0x44;
        // Bytes 5-9: SCR (valid but unused by demuxer).
        sector[5] = 0x00;
        sector[6] = 0x04;
        sector[7] = 0x00;
        sector[8] = 0x04;
        sector[9] = 0x01;
        // Bytes 10-12: mux rate + markers.
        sector[10] = 0x01;
        sector[11] = 0x89;
        sector[12] = 0xC3;
        // Byte 13: top 5 bits = 1, low 3 = stuffing count.
        sector[13] = 0xF8 | (stuffing & 0x07);
        // Fill stuffing bytes.
        for i in 0..usize::from(stuffing) {
            sector[14 + i] = 0xFF;
        }
    }

    /// Writes a PES packet at `offset`. Returns the offset after the
    /// packet.
    fn write_pes(
        sector: &mut [u8; SECTOR_SIZE],
        offset: usize,
        stream_id: u8,
        pts: Option<u64>,
        dts: Option<u64>,
        payload: &[u8],
    ) -> usize {
        let has_optional = stream_id == PRIVATE_STREAM_1 || (0xC0..=0xEF).contains(&stream_id);

        let ts_len = match (pts, dts) {
            (Some(_), Some(_)) => 10,
            (Some(_), None) => 5,
            _ => 0,
        };

        let optional_len = if has_optional { 3 + ts_len } else { 0 };
        let pes_data_len = optional_len + payload.len();

        // PES fixed header (6 bytes).
        sector[offset..offset + 3].copy_from_slice(&PES_PREFIX);
        sector[offset + 3] = stream_id;
        let len_be = (pes_data_len as u16).to_be_bytes();
        sector[offset + 4] = len_be[0];
        sector[offset + 5] = len_be[1];

        let mut pos = offset + 6;

        if has_optional {
            // flags_1: MPEG-2 marker '10'.
            sector[pos] = 0x80;
            // flags_2: PTS_DTS_flags.
            let pts_dts_flags: u8 = match (pts, dts) {
                (Some(_), Some(_)) => 0b11,
                (Some(_), None) => 0b10,
                _ => 0b00,
            };
            sector[pos + 1] = pts_dts_flags << 6;
            // header_data_length.
            sector[pos + 2] = ts_len as u8;
            pos += 3;

            if let Some(p) = pts {
                let prefix = if dts.is_some() { 0b0011 } else { 0b0010 };
                write_timestamp(&mut sector[pos..pos + 5], p, prefix);
                pos += 5;
            }
            if let Some(d) = dts {
                write_timestamp(&mut sector[pos..pos + 5], d, 0b0001);
                pos += 5;
            }
        }

        sector[pos..pos + payload.len()].copy_from_slice(payload);
        pos + payload.len()
    }

    /// Encodes a 33-bit timestamp into 5-byte PTS/DTS format.
    fn write_timestamp(buf: &mut [u8], ts: u64, prefix: u8) {
        let b32_30 = ((ts >> 30) & 0x07) as u8;
        let b29_22 = ((ts >> 22) & 0xFF) as u8;
        let b21_15 = ((ts >> 15) & 0x7F) as u8;
        let b14_7 = ((ts >> 7) & 0xFF) as u8;
        let b6_0 = (ts & 0x7F) as u8;

        buf[0] = (prefix << 4) | (b32_30 << 1) | 0x01;
        buf[1] = b29_22;
        buf[2] = (b21_15 << 1) | 0x01;
        buf[3] = b14_7;
        buf[4] = (b6_0 << 1) | 0x01;
    }

    /// Fills the rest of the sector with a padding PES.
    fn write_padding(sector: &mut [u8; SECTOR_SIZE], offset: usize) {
        if offset + 6 > SECTOR_SIZE {
            return;
        }
        sector[offset..offset + 3].copy_from_slice(&PES_PREFIX);
        sector[offset + 3] = PADDING_STREAM;
        let pad_len = (SECTOR_SIZE - offset - 6) as u16;
        let len_be = pad_len.to_be_bytes();
        sector[offset + 4] = len_be[0];
        sector[offset + 5] = len_be[1];
        for byte in &mut sector[offset + 6..] {
            *byte = 0xFF;
        }
    }

    /// Builds a complete video sector with the given payload and PTS.
    fn video_sector(pts: u64, payload: &[u8]) -> [u8; SECTOR_SIZE] {
        let mut s = [0u8; SECTOR_SIZE];
        write_pack_header(&mut s, 0);
        let off = write_pes(&mut s, 14, 0xE0, Some(pts), None, payload);
        write_padding(&mut s, off);
        s
    }

    /// Builds a complete AC-3 audio sector.  The 4-byte sub-header
    /// (sub-ID + frame count + AU pointer) is prepended to `ac3_data`.
    fn ac3_sector(pts: u64, sub_id: u8, ac3_data: &[u8]) -> [u8; SECTOR_SIZE] {
        let mut ps1_payload = vec![sub_id, 0x01, 0x00, 0x00];
        ps1_payload.extend_from_slice(ac3_data);
        let mut s = [0u8; SECTOR_SIZE];
        write_pack_header(&mut s, 0);
        let off = write_pes(&mut s, 14, PRIVATE_STREAM_1, Some(pts), None, &ps1_payload);
        write_padding(&mut s, off);
        s
    }

    /// Builds a NAV sector (private stream 2).
    fn nav_sector() -> [u8; SECTOR_SIZE] {
        let mut s = [0u8; SECTOR_SIZE];
        write_pack_header(&mut s, 0);
        // PCI packet (private stream 2).
        let pci_len: u16 = 980;
        let off = write_pes(
            &mut s,
            14,
            PRIVATE_STREAM_2,
            None,
            None,
            &vec![0u8; usize::from(pci_len)],
        );
        // DSI packet (private stream 2).
        let dsi_len = SECTOR_SIZE - off - 6;
        write_pes(
            &mut s,
            off,
            PRIVATE_STREAM_2,
            None,
            None,
            &vec![0u8; dsi_len],
        );
        s
    }

    // ── Core demuxer tests ───────────────────────────────────────────

    #[test]
    fn demux_video_pes() {
        let payload = vec![0x00, 0x00, 0x01, 0x00, 0xAA, 0xBB];
        let s = video_sector(90_000, &payload);

        let mut d = PsDemuxer::new(Cursor::new(s.to_vec()));
        let unit = d
            .next_unit()
            .expect("should not error")
            .expect("should produce a unit");

        assert_eq!(unit.stream_id.pid, 0xE000, "PID should be MPEG video 0");
        assert_eq!(
            unit.stream_id.coding_type, 0x02,
            "coding type should be MPEG-2"
        );
        assert_eq!(unit.pts, Some(90_000), "PTS should be 90 000");
        assert!(unit.dts.is_none(), "DTS should be absent");
        assert_eq!(unit.payload, payload, "payload should match input");
        assert!(
            d.next_unit().expect("should not error").is_none(),
            "no more units expected"
        );
    }

    #[test]
    fn demux_ac3_strips_subheader() {
        let ac3_data = vec![0x0B, 0x77, 0x44, 0x55, 0x66];
        let s = ac3_sector(45_000, 0x80, &ac3_data);

        let mut d = PsDemuxer::new(Cursor::new(s.to_vec()));
        let unit = d
            .next_unit()
            .expect("should not error")
            .expect("should produce a unit");

        assert_eq!(unit.stream_id.pid, 0xBD80, "PID should be AC-3 sub-ID 0x80");
        assert_eq!(
            unit.stream_id.coding_type, 0x81,
            "coding type should be AC-3"
        );
        assert_eq!(unit.pts, Some(45_000), "PTS should be 45 000");
        assert_eq!(
            unit.payload, ac3_data,
            "payload should be AC-3 data with 4-byte sub-header stripped"
        );
    }

    #[test]
    fn demux_dts_strips_subheader() {
        let dts_data = vec![0x7F, 0xFE, 0x80, 0x01, 0x00, 0x00];
        let mut ps1_payload = vec![0x88, 0x01, 0x00, 0x00]; // 4-byte sub-header
        ps1_payload.extend_from_slice(&dts_data);

        let mut s = [0u8; SECTOR_SIZE];
        write_pack_header(&mut s, 0);
        let off = write_pes(
            &mut s,
            14,
            PRIVATE_STREAM_1,
            Some(60_000),
            None,
            &ps1_payload,
        );
        write_padding(&mut s, off);

        let mut d = PsDemuxer::new(Cursor::new(s.to_vec()));
        let unit = d
            .next_unit()
            .expect("should not error")
            .expect("should produce a unit");

        assert_eq!(unit.stream_id.pid, 0xBD88, "PID should be DTS sub-ID 0x88");
        assert_eq!(
            unit.stream_id.coding_type, 0x82,
            "coding type should be DTS"
        );
        assert_eq!(
            unit.payload, dts_data,
            "payload should be DTS data with 4-byte sub-header stripped"
        );
    }

    #[test]
    fn demux_lpcm_strips_subheader() {
        // 7-byte LPCM sub-header: sub-ID + frames + AU ptr + format bytes.
        let pcm_data = vec![0x01, 0x02, 0x03, 0x04];
        let mut ps1_payload = vec![0xA0, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00];
        ps1_payload.extend_from_slice(&pcm_data);

        let mut s = [0u8; SECTOR_SIZE];
        write_pack_header(&mut s, 0);
        let off = write_pes(
            &mut s,
            14,
            PRIVATE_STREAM_1,
            Some(30_000),
            None,
            &ps1_payload,
        );
        write_padding(&mut s, off);

        let mut d = PsDemuxer::new(Cursor::new(s.to_vec()));
        let unit = d
            .next_unit()
            .expect("should not error")
            .expect("should produce a unit");

        assert_eq!(unit.stream_id.pid, 0xBDA0, "PID should be LPCM sub-ID 0xA0");
        assert_eq!(
            unit.stream_id.coding_type, 0x80,
            "coding type should be LPCM"
        );
        assert_eq!(
            unit.payload, pcm_data,
            "payload should be PCM data with 7-byte sub-header stripped"
        );
    }

    #[test]
    fn demux_subtitle_strips_subid() {
        // VobSub: 1-byte sub-header (just the sub-stream ID).
        let spu_data = vec![0x00, 0x1A, 0x00, 0x12, 0xAA];
        let mut ps1_payload = vec![0x20]; // sub-ID
        ps1_payload.extend_from_slice(&spu_data);

        let mut s = [0u8; SECTOR_SIZE];
        write_pack_header(&mut s, 0);
        let off = write_pes(
            &mut s,
            14,
            PRIVATE_STREAM_1,
            Some(10_000),
            None,
            &ps1_payload,
        );
        write_padding(&mut s, off);

        let mut d = PsDemuxer::new(Cursor::new(s.to_vec()));
        let unit = d
            .next_unit()
            .expect("should not error")
            .expect("should produce a unit");

        assert_eq!(
            unit.stream_id.pid, 0xBD20,
            "PID should be subtitle sub-ID 0x20"
        );
        assert_eq!(
            unit.payload, spu_data,
            "payload should be SPU data with 1-byte sub-ID stripped"
        );
    }

    #[test]
    fn nav_pack_skipped() {
        // NAV sector followed by a video sector.
        let nav = nav_sector();
        let video_payload = vec![0xAA; 4];
        let vid = video_sector(1000, &video_payload);

        let mut stream = Vec::with_capacity(SECTOR_SIZE * 2);
        stream.extend_from_slice(&nav);
        stream.extend_from_slice(&vid);

        let mut d = PsDemuxer::new(Cursor::new(stream));
        let unit = d
            .next_unit()
            .expect("should not error")
            .expect("should produce a unit");

        assert_eq!(
            unit.stream_id.pid, 0xE000,
            "NAV sector should be skipped, video unit returned"
        );
        assert_eq!(unit.payload, video_payload, "payload should match video");
        assert_eq!(d.sector_count(), 2, "two sectors should have been read");
    }

    #[test]
    fn pts_and_dts_extraction() {
        let pts: u64 = 180_000; // 2 seconds
        let dts: u64 = 171_000; // slightly earlier
        let payload = vec![0x00, 0x00, 0x01, 0x00, 0xFF];

        let mut s = [0u8; SECTOR_SIZE];
        write_pack_header(&mut s, 0);
        let off = write_pes(&mut s, 14, 0xE0, Some(pts), Some(dts), &payload);
        write_padding(&mut s, off);

        let mut d = PsDemuxer::new(Cursor::new(s.to_vec()));
        let unit = d
            .next_unit()
            .expect("should not error")
            .expect("should produce a unit");

        assert_eq!(unit.pts, Some(180_000), "PTS should be 180 000");
        assert_eq!(unit.dts, Some(171_000), "DTS should be 171 000");
    }

    #[test]
    fn no_timestamps() {
        let payload = vec![0x00, 0x00, 0x01, 0x00, 0x11];

        let mut s = [0u8; SECTOR_SIZE];
        write_pack_header(&mut s, 0);
        let off = write_pes(&mut s, 14, 0xE0, None, None, &payload);
        write_padding(&mut s, off);

        let mut d = PsDemuxer::new(Cursor::new(s.to_vec()));
        let unit = d
            .next_unit()
            .expect("should not error")
            .expect("should produce a unit");

        assert!(unit.pts.is_none(), "PTS should be absent");
        assert!(unit.dts.is_none(), "DTS should be absent");
    }

    #[test]
    fn system_header_skipped() {
        let mut s = [0u8; SECTOR_SIZE];
        write_pack_header(&mut s, 0);

        // System header at offset 14 (stream_id 0xBB, length 6).
        let sys_data = [0x00; 6];
        let off = write_pes(&mut s, 14, SYSTEM_HEADER_CODE, None, None, &sys_data);

        // Content PES after system header.
        let video_data = vec![0xCC; 8];
        let off2 = write_pes(&mut s, off, 0xE0, Some(5000), None, &video_data);
        write_padding(&mut s, off2);

        let mut d = PsDemuxer::new(Cursor::new(s.to_vec()));
        let unit = d
            .next_unit()
            .expect("should not error")
            .expect("should produce a unit");

        assert_eq!(
            unit.stream_id.pid, 0xE000,
            "video unit should follow system header"
        );
        assert_eq!(unit.payload, video_data, "payload should match video data");
    }

    #[test]
    fn filter_includes_matching_stream() {
        let s = video_sector(1000, &[0xAA; 4]);
        let filter: HashSet<u16> = [pack_ps_id(0xE0, 0)].into();

        let mut d = PsDemuxer::with_filter(Cursor::new(s.to_vec()), filter);
        let unit = d
            .next_unit()
            .expect("should not error")
            .expect("should produce a unit");

        assert_eq!(
            unit.stream_id.pid, 0xE000,
            "matching stream should pass filter"
        );
    }

    #[test]
    fn filter_rejects_non_matching_stream() {
        let s = video_sector(1000, &[0xAA; 4]);
        // Filter accepts only AC-3, not video.
        let filter: HashSet<u16> = [pack_ps_id(PRIVATE_STREAM_1, 0x80)].into();

        let mut d = PsDemuxer::with_filter(Cursor::new(s.to_vec()), filter);
        assert!(
            d.next_unit().expect("should not error").is_none(),
            "non-matching stream should be filtered out"
        );
    }

    #[test]
    fn multiple_sectors() {
        let s1 = nav_sector();
        let s2 = video_sector(1000, &[0x11; 4]);
        let s3 = ac3_sector(2000, 0x80, &[0x0B, 0x77]);

        let mut stream = Vec::new();
        stream.extend_from_slice(&s1);
        stream.extend_from_slice(&s2);
        stream.extend_from_slice(&s3);

        let mut d = PsDemuxer::new(Cursor::new(stream));

        let u1 = d
            .next_unit()
            .expect("should not error")
            .expect("first content unit");
        assert_eq!(
            u1.stream_id.pid, 0xE000,
            "first content unit should be video"
        );

        let u2 = d
            .next_unit()
            .expect("should not error")
            .expect("second content unit");
        assert_eq!(
            u2.stream_id.pid, 0xBD80,
            "second content unit should be AC-3"
        );

        assert!(
            d.next_unit().expect("should not error").is_none(),
            "no more units after three sectors"
        );
        assert_eq!(d.sector_count(), 3, "three sectors should have been read");
    }

    #[test]
    fn empty_stream_returns_none() {
        let mut d = PsDemuxer::new(Cursor::new(Vec::new()));
        assert!(
            d.next_unit().expect("should not error").is_none(),
            "empty stream should return None immediately"
        );
    }

    #[test]
    fn program_end_sector() {
        // Sector starting with program end code, rest is padding.
        let mut s = [0u8; SECTOR_SIZE];
        s[0..4].copy_from_slice(&PROGRAM_END_CODE);

        let mut d = PsDemuxer::new(Cursor::new(s.to_vec()));
        assert!(
            d.next_unit().expect("should not error").is_none(),
            "program end code should signal end of stream"
        );
    }

    #[test]
    fn pack_with_stuffing() {
        let stuffing = 3u8;
        let mut s = [0u8; SECTOR_SIZE];
        write_pack_header(&mut s, stuffing);
        let pes_start = PACK_HEADER_BASE + usize::from(stuffing); // 17
        let payload = vec![0xDD; 4];
        let off = write_pes(&mut s, pes_start, 0xE0, Some(500), None, &payload);
        write_padding(&mut s, off);

        let mut d = PsDemuxer::new(Cursor::new(s.to_vec()));
        let unit = d
            .next_unit()
            .expect("should not error")
            .expect("should produce a unit");

        assert_eq!(
            unit.payload, payload,
            "stuffing bytes should be skipped correctly"
        );
    }

    #[test]
    fn timestamp_roundtrip() {
        // Verify known values survive encode → decode.
        let cases: &[(u64, &str)] = &[
            (0, "zero"),
            (90_000, "1 second"),
            (180_000, "2 seconds"),
            (0x1_FFFF_FFFF, "max 33-bit value"),
            (8_589_934_591, "max 33-bit decimal"),
        ];

        for &(ts, label) in cases {
            let mut buf = [0u8; 5];
            write_timestamp(&mut buf, ts, 0b0010);
            let decoded = parse_timestamp(&buf);
            assert_eq!(decoded, ts, "roundtrip failed for {label} ({ts})");
        }
    }

    // ── Utility tests ────────────────────────────────────────────────

    #[test]
    fn chain_read_joins_readers() {
        let a: &[u8] = &[1, 2, 3];
        let b: &[u8] = &[4, 5];
        let c: &[u8] = &[6];
        let mut chain = ChainRead::new(vec![a, b, c]);

        let mut out = Vec::new();
        chain.read_to_end(&mut out).expect("should read all");
        assert_eq!(out, vec![1, 2, 3, 4, 5, 6], "chain should concatenate");
    }

    #[test]
    fn cell_reader_reads_ranges() {
        // Build a fake VOB: 4 sectors of distinct data.
        let mut vob = vec![0u8; SECTOR_SIZE * 4];
        for i in 0..4 {
            let fill = (i + 1) as u8;
            for byte in &mut vob[i * SECTOR_SIZE..(i + 1) * SECTOR_SIZE] {
                *byte = fill;
            }
        }

        // Read sectors 1 (0-based) and 3.
        let ranges = vec![
            SectorRange {
                first_sector: 1,
                last_sector: 1,
            },
            SectorRange {
                first_sector: 3,
                last_sector: 3,
            },
        ];
        let mut reader = CellReader::new(Cursor::new(vob), ranges);

        let mut out = Vec::new();
        reader.read_to_end(&mut out).expect("should read ranges");

        assert_eq!(
            out.len(),
            SECTOR_SIZE * 2,
            "should read exactly two sectors"
        );
        assert!(
            out[..SECTOR_SIZE].iter().all(|&b| b == 2),
            "first range should be sector 1 (filled with 2)"
        );
        assert!(
            out[SECTOR_SIZE..].iter().all(|&b| b == 4),
            "second range should be sector 3 (filled with 4)"
        );
    }
}
