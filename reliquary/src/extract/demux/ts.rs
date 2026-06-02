// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! MPEG-TS streaming demuxer for Blu-ray m2ts extraction.
//!
//! Reads 192-byte Blu-ray m2ts packets one at a time, accumulates PES data
//! per PID, and emits [`DemuxedUnit`]s when PES boundaries are reached.
//! Only PIDs registered in the PID map are processed; all others are skipped.
//!
//! # Usage
//!
//! ```ignore
//! use std::collections::HashMap;
//! use std::fs::File;
//!
//! let mut pid_map = HashMap::new();
//! pid_map.insert(0x1011, 0xea); // video: VC-1
//! pid_map.insert(0x1100, 0x81); // audio: AC-3
//!
//! let file = File::open("00001.m2ts")?;
//! let mut demuxer = TsDemuxer::new(file, pid_map);
//!
//! while let Some(unit) = demuxer.next_unit()? {
//!     // route unit.stream_id.coding_type to appropriate framer
//! }
//! ```

use std::collections::HashMap;
use std::io::Read;

use thiserror::Error;

use crate::disc::bdmv::ts::{PesError, parse_pes};

use super::{DemuxedUnit, StreamId};

// ── Constants ─────────────────────────────────────────────────────────────

/// Size of a Blu-ray m2ts packet (4-byte `TP_extra_header` + 188-byte TS).
const M2TS_PACKET_LEN: usize = 192;

/// MPEG-TS sync byte.
const SYNC_BYTE: u8 = 0x47;

/// PID value for null packets (padding).
const NULL_PID: u16 = 0x1FFF;

// ── Errors ────────────────────────────────────────────────────────────────

/// Errors from the streaming MPEG-TS demuxer.
#[derive(Debug, Error)]
pub enum DemuxError {
    /// I/O error reading from the underlying stream.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Invalid TS sync byte encountered.
    #[error("invalid sync byte at packet {packet}: expected 0x47, found 0x{found:02X}")]
    InvalidSync {
        /// Zero-based packet index.
        packet: usize,
        /// The actual byte value found.
        found: u8,
    },

    /// PES header parsing failed.
    #[error("PES parse error on PID 0x{pid:04X}: {source}")]
    Pes {
        /// PID of the stream with the bad PES data.
        pid: u16,
        /// Underlying PES parse error.
        source: PesError,
    },
}

// ── TsDemuxer ─────────────────────────────────────────────────────────────

/// Streaming MPEG-TS demuxer for Blu-ray m2ts files.
///
/// Reads 192-byte packets from the underlying reader, filters by PID,
/// reassembles PES packets, and emits [`DemuxedUnit`]s one at a time.
pub struct TsDemuxer<R> {
    reader: R,
    /// Maps PID → `coding_type` for streams to extract.
    pid_map: HashMap<u16, u8>,
    /// Per-PID PES accumulation buffers.
    accumulators: HashMap<u16, Vec<u8>>,
    /// Pending units from EOF flush (drained on subsequent `next_unit` calls).
    flush_queue: Vec<DemuxedUnit>,
    /// Number of packets read so far (for error reporting).
    packet_count: usize,
    /// Whether EOF has been reached on the underlying reader.
    eof: bool,
}

impl<R: Read> TsDemuxer<R> {
    /// Creates a new streaming TS demuxer.
    ///
    /// The `pid_map` maps MPEG-TS PIDs to their coding types (from the MPLS
    /// STN table). Only packets matching these PIDs are processed; all other
    /// PIDs are silently skipped.
    pub fn new(reader: R, pid_map: HashMap<u16, u8>) -> Self {
        Self {
            reader,
            pid_map,
            accumulators: HashMap::new(),
            flush_queue: Vec::new(),
            packet_count: 0,
            eof: false,
        }
    }

    /// Returns the next demuxed elementary stream unit.
    ///
    /// Returns `Ok(Some(unit))` for each complete PES packet, or `Ok(None)`
    /// when the stream is exhausted. PES packets are emitted when the next
    /// PES boundary is detected (PUSI bit set on a subsequent packet for the
    /// same PID) or at end-of-stream.
    ///
    /// # Errors
    ///
    /// Returns [`DemuxError::Io`] on read failures,
    /// [`DemuxError::InvalidSync`] on corrupted TS packets, or
    /// [`DemuxError::Pes`] on malformed PES headers.
    pub fn next_unit(&mut self) -> Result<Option<DemuxedUnit>, DemuxError> {
        // Drain any pending flush units first.
        if let Some(unit) = self.flush_queue.pop() {
            return Ok(Some(unit));
        }

        if self.eof {
            return Ok(None);
        }

        let mut buf = [0u8; M2TS_PACKET_LEN];

        loop {
            match read_exact_or_eof(&mut self.reader, &mut buf)? {
                ReadResult::Full => {}
                ReadResult::Eof | ReadResult::Short => {
                    self.eof = true;
                    self.flush_accumulators()?;
                    return Ok(self.flush_queue.pop());
                }
            }

            self.packet_count += 1;

            // Validate sync byte (byte 4, after 4-byte TP_extra_header).
            if buf[4] != SYNC_BYTE {
                return Err(DemuxError::InvalidSync {
                    packet: self.packet_count - 1,
                    found: buf[4],
                });
            }

            // Parse TS header (bytes 4–7).
            let pid = u16::from(buf[5] & 0x1F) << 8 | u16::from(buf[6]);
            let pusi = buf[5] & 0x40 != 0;
            let adaptation = (buf[7] >> 4) & 0x03;

            // Skip null packets and PIDs not in the filter.
            if pid == NULL_PID || !self.pid_map.contains_key(&pid) {
                continue;
            }

            // Locate payload start past any adaptation field.
            let payload_start = match adaptation {
                0x01 => 8, // payload only
                0x03 => {
                    // adaptation field + payload
                    let af_len = usize::from(buf[8]);
                    8 + 1 + af_len
                }
                // 0x00 (reserved) or 0x02 (adaptation only, no payload)
                _ => continue,
            };

            if payload_start >= M2TS_PACKET_LEN {
                continue;
            }

            let payload = &buf[payload_start..];

            if pusi {
                // PUSI set — finalize any in-progress PES for this PID.
                let completed = self.accumulators.remove(&pid);
                // Start accumulating the new PES.
                self.accumulators.insert(pid, payload.to_vec());

                if let Some(pes_data) = completed {
                    return self.finalize_pes(pid, &pes_data).map(Some);
                }
            } else if let Some(acc) = self.accumulators.get_mut(&pid) {
                acc.extend_from_slice(payload);
            }
            // Continuation packet without a prior PUSI — orphan, skip.
        }
    }

    /// Parses a completed PES buffer into a [`DemuxedUnit`].
    fn finalize_pes(&self, pid: u16, pes_data: &[u8]) -> Result<DemuxedUnit, DemuxError> {
        let coding_type = self.pid_map[&pid];
        let parsed = parse_pes(pes_data).map_err(|e| DemuxError::Pes { pid, source: e })?;

        Ok(DemuxedUnit {
            stream_id: StreamId { pid, coding_type },
            pts: parsed.pts,
            dts: parsed.dts,
            payload: parsed.payload,
        })
    }

    /// Drains all remaining PES accumulators into the flush queue.
    fn flush_accumulators(&mut self) -> Result<(), DemuxError> {
        let remaining: Vec<(u16, Vec<u8>)> = self.accumulators.drain().collect();
        for (pid, pes_data) in &remaining {
            self.flush_queue.push(self.finalize_pes(*pid, pes_data)?);
        }
        Ok(())
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────

/// Outcome of a read attempt for exactly `buf.len()` bytes.
enum ReadResult {
    /// Read exactly the requested number of bytes.
    Full,
    /// Zero bytes available — clean end of stream.
    Eof,
    /// Some bytes read but fewer than requested — trailing partial packet.
    Short,
}

/// Reads exactly `buf.len()` bytes, distinguishing clean EOF from a short
/// trailing read.
fn read_exact_or_eof<R: Read>(
    reader: &mut R,
    buf: &mut [u8],
) -> Result<ReadResult, std::io::Error> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..])? {
            0 => {
                return if filled == 0 {
                    Ok(ReadResult::Eof)
                } else {
                    Ok(ReadResult::Short)
                };
            }
            n => filled += n,
        }
    }
    Ok(ReadResult::Full)
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod tests {
    use std::collections::HashMap;
    use std::io::Cursor;

    use super::*;

    // ── Test PIDs and coding types ────────────────────────────────────

    const VIDEO_PID: u16 = 0x1011;
    const AUDIO_PID: u16 = 0x1100;
    const SUBTITLE_PID: u16 = 0x1200;
    const UNREGISTERED_PID: u16 = 0x1400;

    const CODING_VC1: u8 = 0xEA;
    const CODING_AC3: u8 = 0x81;
    const CODING_PGS: u8 = 0x90;

    fn default_pid_map() -> HashMap<u16, u8> {
        let mut map = HashMap::new();
        map.insert(VIDEO_PID, CODING_VC1);
        map.insert(AUDIO_PID, CODING_AC3);
        map.insert(SUBTITLE_PID, CODING_PGS);
        map
    }

    // ── Packet builder ───────────────────────────────────────────────

    /// Builds valid 192-byte m2ts packets for testing the streaming demuxer.
    struct PacketBuilder {
        packets: Vec<u8>,
        continuity: HashMap<u16, u8>,
    }

    impl PacketBuilder {
        fn new() -> Self {
            Self {
                packets: Vec::new(),
                continuity: HashMap::new(),
            }
        }

        /// Adds a PES packet (no PTS) for the given PID.
        fn pes(mut self, pid: u16, payload: &[u8]) -> Self {
            let pes_data = Self::wrap_pes(payload);
            self.fragment(pid, &pes_data);
            self
        }

        /// Adds a PES packet with a PTS for the given PID.
        fn pes_with_pts(mut self, pid: u16, payload: &[u8], pts: u64) -> Self {
            let pes_data = Self::wrap_pes_pts(payload, pts);
            self.fragment(pid, &pes_data);
            self
        }

        /// Adds a PES packet with PTS and DTS for the given PID.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "PTS/DTS bit slices always fit in u8"
        )]
        fn pes_with_pts_dts(mut self, pid: u16, payload: &[u8], pts: u64, dts: u64) -> Self {
            let mut pes = Vec::with_capacity(19 + payload.len());
            pes.extend_from_slice(&[0x00, 0x00, 0x01]); // start code
            pes.push(0xE0); // stream_id (video)
            pes.extend_from_slice(&[0x00, 0x00]); // length (unbounded)
            pes.push(0x80); // flags
            pes.push(0xC0); // PTS + DTS
            pes.push(0x0A); // header_data_length = 10
            // PTS (5 bytes)
            pes.push(0x31 | (((pts >> 30) & 0x07) as u8) << 1);
            pes.push(((pts >> 22) & 0xFF) as u8);
            pes.push(0x01 | (((pts >> 15) & 0x7F) as u8) << 1);
            pes.push(((pts >> 7) & 0xFF) as u8);
            pes.push(0x01 | ((pts & 0x7F) as u8) << 1);
            // DTS (5 bytes)
            pes.push(0x11 | (((dts >> 30) & 0x07) as u8) << 1);
            pes.push(((dts >> 22) & 0xFF) as u8);
            pes.push(0x01 | (((dts >> 15) & 0x7F) as u8) << 1);
            pes.push(((dts >> 7) & 0xFF) as u8);
            pes.push(0x01 | ((dts & 0x7F) as u8) << 1);
            pes.extend_from_slice(payload);

            self.fragment(pid, &pes);
            self
        }

        /// Adds a null packet (PID 0x1FFF).
        fn null(mut self) -> Self {
            let mut pkt = [0u8; M2TS_PACKET_LEN];
            pkt[4] = SYNC_BYTE;
            pkt[5] = 0x1F;
            pkt[6] = 0xFF;
            pkt[7] = 0x10;
            self.packets.extend_from_slice(&pkt);
            self
        }

        /// Builds the raw byte stream.
        fn build(self) -> Vec<u8> {
            self.packets
        }

        /// Wraps payload in a minimal PES header (no PTS).
        fn wrap_pes(payload: &[u8]) -> Vec<u8> {
            let mut pes = Vec::with_capacity(9 + payload.len());
            pes.extend_from_slice(&[0x00, 0x00, 0x01]);
            pes.push(0xBD); // private_stream_1
            pes.extend_from_slice(&[0x00, 0x00]); // length (unbounded)
            pes.push(0x80); // flags
            pes.push(0x00); // no PTS/DTS
            pes.push(0x00); // header_data_length = 0
            pes.extend_from_slice(payload);
            pes
        }

        /// Wraps payload in a PES header with PTS.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "PTS bit slices always fit in u8"
        )]
        fn wrap_pes_pts(payload: &[u8], pts: u64) -> Vec<u8> {
            let mut pes = Vec::with_capacity(14 + payload.len());
            pes.extend_from_slice(&[0x00, 0x00, 0x01]);
            pes.push(0xBD);
            pes.extend_from_slice(&[0x00, 0x00]);
            pes.push(0x80);
            pes.push(0x80); // PTS only
            pes.push(0x05); // header_data_length = 5
            pes.push(0x21 | (((pts >> 30) & 0x07) as u8) << 1);
            pes.push(((pts >> 22) & 0xFF) as u8);
            pes.push(0x01 | (((pts >> 15) & 0x7F) as u8) << 1);
            pes.push(((pts >> 7) & 0xFF) as u8);
            pes.push(0x01 | ((pts & 0x7F) as u8) << 1);
            pes.extend_from_slice(payload);
            pes
        }

        /// Fragments PES data across 192-byte TS packets.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "PID bits and padding lengths always fit in u8"
        )]
        fn fragment(&mut self, pid: u16, pes_data: &[u8]) {
            let max_payload = M2TS_PACKET_LEN - 8; // 184 bytes
            let mut offset = 0;
            let mut first = true;

            while offset < pes_data.len() {
                let remaining = pes_data.len() - offset;
                let chunk_len = remaining.min(max_payload);
                let needs_padding = chunk_len < max_payload;

                let cc = self.next_cc(pid);
                let mut pkt = [0u8; M2TS_PACKET_LEN];

                pkt[4] = SYNC_BYTE;
                pkt[5] = ((pid >> 8) & 0x1F) as u8 | if first { 0x40 } else { 0x00 };
                pkt[6] = (pid & 0xFF) as u8;

                if needs_padding {
                    let pad_len = max_payload - chunk_len;
                    pkt[7] = 0x30 | (cc & 0x0F);
                    pkt[8] = (pad_len - 1) as u8;
                    if pad_len > 1 {
                        pkt[9] = 0x00;
                    }
                    let payload_start = 8 + pad_len;
                    pkt[payload_start..payload_start + chunk_len]
                        .copy_from_slice(&pes_data[offset..offset + chunk_len]);
                } else {
                    pkt[7] = 0x10 | (cc & 0x0F);
                    pkt[8..8 + chunk_len].copy_from_slice(&pes_data[offset..offset + chunk_len]);
                }

                self.packets.extend_from_slice(&pkt);
                offset += chunk_len;
                first = false;
            }
        }

        fn next_cc(&mut self, pid: u16) -> u8 {
            let cc = self.continuity.entry(pid).or_insert(0);
            let val = *cc;
            *cc = (*cc + 1) & 0x0F;
            val
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────

    /// Collects all units from a demuxer into a Vec.
    fn collect_units(data: &[u8], pid_map: HashMap<u16, u8>) -> Vec<DemuxedUnit> {
        let mut demuxer = TsDemuxer::new(Cursor::new(data), pid_map);
        let mut units = Vec::new();
        while let Some(unit) = demuxer.next_unit().expect("demux should succeed") {
            units.push(unit);
        }
        units
    }

    // ── Basic demuxing ───────────────────────────────────────────────

    #[test]
    fn single_pes_single_pid() {
        let payload = b"video frame data";
        let data = PacketBuilder::new().pes(VIDEO_PID, payload).build();

        let units = collect_units(&data, default_pid_map());
        assert_eq!(units.len(), 1, "should produce one unit");
        assert_eq!(units[0].stream_id.pid, VIDEO_PID, "PID should match");
        assert_eq!(
            units[0].stream_id.coding_type, CODING_VC1,
            "coding_type should match"
        );
        assert_eq!(units[0].payload, payload, "payload should match");
    }

    #[test]
    fn multiple_pes_same_pid() {
        let payload_a = b"frame one";
        let payload_b = b"frame two";
        let data = PacketBuilder::new()
            .pes(AUDIO_PID, payload_a)
            .pes(AUDIO_PID, payload_b)
            .build();

        let units = collect_units(&data, default_pid_map());
        assert_eq!(units.len(), 2, "should produce two units");
        assert_eq!(units[0].payload, payload_a, "first payload should match");
        assert_eq!(units[1].payload, payload_b, "second payload should match");
    }

    #[test]
    fn large_pes_spanning_multiple_packets() {
        let payload: Vec<u8> = (0u16..500).map(|i| (i & 0xFF) as u8).collect();
        let data = PacketBuilder::new().pes(VIDEO_PID, &payload).build();

        let units = collect_units(&data, default_pid_map());
        assert_eq!(units.len(), 1, "should reassemble into one unit");
        assert_eq!(
            units[0].payload, payload,
            "reassembled payload should match"
        );
    }

    // ── PID routing ──────────────────────────────────────────────────

    #[test]
    fn interleaved_pids_routed_correctly() {
        let video = b"video data";
        let audio = b"audio data";
        let subs = b"subtitle data";
        let data = PacketBuilder::new()
            .pes(VIDEO_PID, video)
            .pes(AUDIO_PID, audio)
            .pes(SUBTITLE_PID, subs)
            .build();

        let units = collect_units(&data, default_pid_map());
        assert_eq!(units.len(), 3, "should produce three units");

        let video_units: Vec<_> = units
            .iter()
            .filter(|u| u.stream_id.pid == VIDEO_PID)
            .collect();
        let audio_units: Vec<_> = units
            .iter()
            .filter(|u| u.stream_id.pid == AUDIO_PID)
            .collect();
        let sub_units: Vec<_> = units
            .iter()
            .filter(|u| u.stream_id.pid == SUBTITLE_PID)
            .collect();

        assert_eq!(video_units.len(), 1, "should have one video unit");
        assert_eq!(audio_units.len(), 1, "should have one audio unit");
        assert_eq!(sub_units.len(), 1, "should have one subtitle unit");

        assert_eq!(video_units[0].payload, video, "video payload should match");
        assert_eq!(audio_units[0].payload, audio, "audio payload should match");
        assert_eq!(sub_units[0].payload, subs, "subtitle payload should match");
    }

    #[test]
    fn coding_type_tagged_per_stream() {
        let data = PacketBuilder::new()
            .pes(VIDEO_PID, b"v")
            .pes(AUDIO_PID, b"a")
            .pes(SUBTITLE_PID, b"s")
            .build();

        let units = collect_units(&data, default_pid_map());

        let expected_map = default_pid_map();
        for unit in &units {
            let expected = expected_map
                .get(&unit.stream_id.pid)
                .expect("PID should be in the expected map");
            assert_eq!(
                unit.stream_id.coding_type, *expected,
                "coding_type for PID 0x{:04X} should be 0x{expected:02X}",
                unit.stream_id.pid
            );
        }
    }

    // ── Filtering ────────────────────────────────────────────────────

    #[test]
    fn unregistered_pid_skipped() {
        let data = PacketBuilder::new()
            .pes(UNREGISTERED_PID, b"should be skipped")
            .pes(VIDEO_PID, b"should appear")
            .build();

        let units = collect_units(&data, default_pid_map());
        assert_eq!(units.len(), 1, "unregistered PID should be skipped");
        assert_eq!(
            units[0].stream_id.pid, VIDEO_PID,
            "only video should appear"
        );
    }

    #[test]
    fn null_packets_skipped() {
        let data = PacketBuilder::new()
            .null()
            .pes(VIDEO_PID, b"data")
            .null()
            .build();

        let units = collect_units(&data, default_pid_map());
        assert_eq!(units.len(), 1, "null packets should be skipped");
    }

    // ── Timestamps ───────────────────────────────────────────────────

    #[test]
    fn pts_extracted() {
        let pts = 90_000; // 1 second at 90 kHz
        let data = PacketBuilder::new()
            .pes_with_pts(AUDIO_PID, b"audio", pts)
            .build();

        let units = collect_units(&data, default_pid_map());
        assert_eq!(units.len(), 1, "should produce one unit");
        assert_eq!(units[0].pts, Some(pts), "PTS should be extracted");
        assert!(units[0].dts.is_none(), "DTS should be absent for PTS-only");
    }

    #[test]
    fn pts_and_dts_extracted() {
        let pts: u64 = 180_000;
        let dts: u64 = 162_000;
        let data = PacketBuilder::new()
            .pes_with_pts_dts(VIDEO_PID, b"B-frame", pts, dts)
            .build();

        let units = collect_units(&data, default_pid_map());
        assert_eq!(units.len(), 1, "should produce one unit");
        assert_eq!(units[0].pts, Some(pts), "PTS should be extracted");
        assert_eq!(units[0].dts, Some(dts), "DTS should be extracted");
    }

    #[test]
    fn no_pts_when_absent() {
        let data = PacketBuilder::new().pes(VIDEO_PID, b"data").build();

        let units = collect_units(&data, default_pid_map());
        assert_eq!(units.len(), 1, "should produce one unit");
        assert!(units[0].pts.is_none(), "PTS should be absent");
        assert!(units[0].dts.is_none(), "DTS should be absent");
    }

    // ── Edge cases ───────────────────────────────────────────────────

    #[test]
    fn empty_input() {
        let units = collect_units(&[], default_pid_map());
        assert!(units.is_empty(), "empty input should produce no units");
    }

    #[test]
    fn trailing_bytes_ignored() {
        let mut data = PacketBuilder::new().pes(VIDEO_PID, b"data").build();
        data.extend_from_slice(&[0xFF; 100]); // trailing partial packet

        let units = collect_units(&data, default_pid_map());
        assert_eq!(units.len(), 1, "trailing bytes should be ignored");
    }

    #[test]
    fn empty_pid_map_produces_no_units() {
        let data = PacketBuilder::new()
            .pes(VIDEO_PID, b"video")
            .pes(AUDIO_PID, b"audio")
            .build();

        let units = collect_units(&data, HashMap::new());
        assert!(units.is_empty(), "empty PID map should skip all packets");
    }

    // ── Error cases ──────────────────────────────────────────────────

    #[test]
    fn invalid_sync_byte_returns_error() {
        let mut data = PacketBuilder::new().pes(VIDEO_PID, b"data").build();
        data[4] = 0x00; // corrupt sync byte

        let mut demuxer = TsDemuxer::new(Cursor::new(data), default_pid_map());
        let err = demuxer
            .next_unit()
            .expect_err("bad sync should produce error");
        assert!(
            matches!(
                err,
                DemuxError::InvalidSync {
                    packet: 0,
                    found: 0x00
                }
            ),
            "should report InvalidSync at packet 0"
        );
    }

    // ── Streaming behavior ───────────────────────────────────────────

    #[test]
    fn units_emitted_incrementally() {
        let data = PacketBuilder::new()
            .pes(VIDEO_PID, b"frame 1")
            .pes(VIDEO_PID, b"frame 2")
            .pes(VIDEO_PID, b"frame 3")
            .build();

        let mut demuxer = TsDemuxer::new(Cursor::new(data), default_pid_map());

        // Each call should return exactly one unit.
        let u1 = demuxer
            .next_unit()
            .expect("should succeed")
            .expect("should have unit 1");
        assert_eq!(u1.payload, b"frame 1", "first unit payload should match");

        let u2 = demuxer
            .next_unit()
            .expect("should succeed")
            .expect("should have unit 2");
        assert_eq!(u2.payload, b"frame 2", "second unit payload should match");

        let u3 = demuxer
            .next_unit()
            .expect("should succeed")
            .expect("should have unit 3");
        assert_eq!(u3.payload, b"frame 3", "third unit payload should match");

        assert!(
            demuxer.next_unit().expect("should succeed").is_none(),
            "should signal end of stream"
        );
    }

    #[test]
    fn eof_returns_none_repeatedly() {
        let data = PacketBuilder::new().pes(VIDEO_PID, b"only").build();
        let mut demuxer = TsDemuxer::new(Cursor::new(data), default_pid_map());

        demuxer
            .next_unit()
            .expect("should succeed")
            .expect("should have one unit");

        for _ in 0..3 {
            assert!(
                demuxer.next_unit().expect("should succeed").is_none(),
                "should keep returning None after EOF"
            );
        }
    }

    // ── Multi-clip sequential usage ──────────────────────────────────

    #[test]
    fn sequential_clips_demuxed_independently() {
        // Simulates processing two clips from a multi-clip playlist.
        let clip_1 = PacketBuilder::new()
            .pes_with_pts(VIDEO_PID, b"clip1 video", 90_000)
            .pes_with_pts(AUDIO_PID, b"clip1 audio", 90_000)
            .build();

        let clip_2 = PacketBuilder::new()
            .pes_with_pts(VIDEO_PID, b"clip2 video", 180_000)
            .pes_with_pts(AUDIO_PID, b"clip2 audio", 180_000)
            .build();

        let pid_map = default_pid_map();

        let units_1 = collect_units(&clip_1, pid_map.clone());
        assert_eq!(units_1.len(), 2, "clip 1 should produce two units");

        let units_2 = collect_units(&clip_2, pid_map);
        assert_eq!(units_2.len(), 2, "clip 2 should produce two units");

        // Verify timestamps are per-clip (not accumulated).
        let v1 = units_1
            .iter()
            .find(|u| u.stream_id.pid == VIDEO_PID)
            .expect("clip 1 should have video");
        let v2 = units_2
            .iter()
            .find(|u| u.stream_id.pid == VIDEO_PID)
            .expect("clip 2 should have video");

        assert_eq!(v1.pts, Some(90_000), "clip 1 video PTS");
        assert_eq!(v2.pts, Some(180_000), "clip 2 video PTS");
    }
}
