// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! MPEG-TS demuxer — demultiplexes Blu-ray m2ts streams into PES packets.
//!
//! Blu-ray m2ts files use 192-byte packets (4-byte `TP_extra_header` +
//! 188-byte MPEG-TS packet). The demuxer reassembles PES packets across
//! TS packet boundaries and tags each with its PID.
//!
//! Two layers:
//! - [`demux`] — iterates 192-byte packets, reassembles PES packets.
//! - [`parse_pes`] — strips PES headers, extracts optional PTS.

use std::collections::BTreeMap;

use thiserror::Error;

// ── Constants ─────────────────────────────────────────────────────────────

/// Size of a Blu-ray m2ts packet (4-byte `TP_extra_header` + 188-byte TS).
const M2TS_PACKET_LEN: usize = 192;

/// MPEG-TS sync byte.
const SYNC_BYTE: u8 = 0x47;

/// PID value for null packets (padding).
const NULL_PID: u16 = 0x1FFF;

/// Minimum PES header length (start code + `stream_id` + length + flags + `header_data_length`).
const PES_HEADER_MIN: usize = 9;

// ── Errors ────────────────────────────────────────────────────────────────

/// Errors from MPEG-TS demuxing.
#[derive(Debug, Error)]
pub enum TsError {
    /// Invalid TS sync byte encountered.
    #[error("invalid sync byte at offset {offset}: expected 0x47, found 0x{found:02X}")]
    InvalidSync {
        /// Byte offset within the input where the bad sync was found.
        offset: usize,
        /// The actual byte value found.
        found: u8,
    },
}

/// Errors from PES header parsing.
#[derive(Debug, Error)]
pub enum PesError {
    /// Missing or invalid PES start code.
    #[error("invalid PES start code")]
    InvalidStartCode,

    /// PES header is truncated.
    #[error("PES data is truncated")]
    Truncated,
}

// ── Types ─────────────────────────────────────────────────────────────────

/// A reassembled PES packet from an MPEG-TS stream.
#[derive(Debug, Clone)]
pub struct PesPacket {
    /// PID of the elementary stream this packet belongs to.
    pub pid: u16,
    /// Raw PES data including header (spans multiple TS packets).
    pub data: Vec<u8>,
}

/// A parsed PES packet with header stripped.
#[derive(Debug, Clone)]
pub struct ParsedPes {
    /// Presentation timestamp (90 kHz clock), if present.
    pub pts: Option<u64>,
    /// Elementary stream payload (PES header stripped).
    pub payload: Vec<u8>,
}

// ── Public API ────────────────────────────────────────────────────────────

/// Demultiplexes an m2ts byte stream into PES packets.
///
/// Returns all reassembled PES packets tagged with their PID.
/// Callers filter by PID and parse PES headers as needed.
///
/// Processes complete 192-byte packets only; trailing bytes shorter than
/// one packet are ignored.
///
/// # Errors
///
/// Returns [`TsError::InvalidSync`] if any packet has a bad sync byte.
pub fn demux(data: &[u8]) -> Result<Vec<PesPacket>, TsError> {
    let packet_count = data.len() / M2TS_PACKET_LEN;
    let mut accumulators: BTreeMap<u16, Vec<u8>> = BTreeMap::new();
    let mut result = Vec::new();

    for i in 0..packet_count {
        let offset = i * M2TS_PACKET_LEN;
        let packet = &data[offset..offset + M2TS_PACKET_LEN];

        // Byte 4 is the sync byte (after 4-byte TP_extra_header)
        if packet[4] != SYNC_BYTE {
            return Err(TsError::InvalidSync {
                offset: offset + 4,
                found: packet[4],
            });
        }

        // Parse TS header (bytes 4-7)
        let pid = u16::from(packet[5] & 0x1F) << 8 | u16::from(packet[6]);
        let pusi = packet[5] & 0x40 != 0;
        let adaptation = (packet[7] >> 4) & 0x03;

        // Skip null packets
        if pid == NULL_PID {
            continue;
        }

        // Determine payload start
        let payload_start = match adaptation {
            0x01 => 8, // payload only
            0x03 => {
                // adaptation field + payload
                let af_len = usize::from(packet[8]);
                8 + 1 + af_len
            }
            // 0x00 (reserved) or 0x02 (adaptation only, no payload)
            _ => continue,
        };

        if payload_start >= M2TS_PACKET_LEN {
            continue;
        }

        let payload = &packet[payload_start..];

        if pusi {
            // Finalize any in-progress accumulator for this PID
            if let Some(acc) = accumulators.remove(&pid) {
                result.push(PesPacket { pid, data: acc });
            }
            // Start a new accumulator
            accumulators.insert(pid, payload.to_vec());
        } else if let Some(acc) = accumulators.get_mut(&pid) {
            acc.extend_from_slice(payload);
        }
    }

    // Finalize all remaining accumulators
    for (pid, acc) in accumulators {
        result.push(PesPacket { pid, data: acc });
    }

    Ok(result)
}

/// Parses a raw PES packet — strips header, extracts PTS.
///
/// Takes the raw PES bytes (as stored in [`PesPacket::data`]) and returns
/// the elementary stream payload with optional presentation timestamp.
///
/// # Errors
///
/// Returns [`PesError::InvalidStartCode`] if bytes 0-2 are not `00 00 01`.
/// Returns [`PesError::Truncated`] if the data is too short for a valid header.
pub fn parse_pes(data: &[u8]) -> Result<ParsedPes, PesError> {
    if data.len() < PES_HEADER_MIN {
        return Err(PesError::Truncated);
    }

    // Validate start code
    if data[0] != 0x00 || data[1] != 0x00 || data[2] != 0x01 {
        return Err(PesError::InvalidStartCode);
    }

    // Byte 7: flags2 — bits 6-7 are PTS/DTS flags
    let pts_dts_flags = (data[7] >> 6) & 0x03;

    // Byte 8: PES_header_data_length
    let header_data_len = usize::from(data[8]);

    let pts = if pts_dts_flags >= 0x02 {
        // PTS present (flags 0b10 or 0b11)
        if data.len() < 14 {
            return Err(PesError::Truncated);
        }
        Some(extract_pts(&data[9..14]))
    } else {
        None
    };

    let payload_start = 9 + header_data_len;
    if payload_start > data.len() {
        return Err(PesError::Truncated);
    }

    Ok(ParsedPes {
        pts,
        payload: data[payload_start..].to_vec(),
    })
}

// ── Internal ──────────────────────────────────────────────────────────────

/// Extracts a 33-bit PTS from 5 bytes encoded per MPEG-2 spec.
///
/// Layout (5 bytes, 40 bits):
/// ```text
/// byte 0: [4 bits marker][3 bits PTS[32..30]][1 marker bit]
/// byte 1: [8 bits PTS[29..22]]
/// byte 2: [7 bits PTS[21..15]][1 marker bit]
/// byte 3: [8 bits PTS[14..7]]
/// byte 4: [7 bits PTS[6..0]][1 marker bit]
/// ```
fn extract_pts(bytes: &[u8]) -> u64 {
    let b0 = u64::from(bytes[0]);
    let b1 = u64::from(bytes[1]);
    let b2 = u64::from(bytes[2]);
    let b3 = u64::from(bytes[3]);
    let b4 = u64::from(bytes[4]);

    ((b0 >> 1) & 0x07) << 30 | b1 << 22 | ((b2 >> 1) & 0x7F) << 15 | b3 << 7 | (b4 >> 1) & 0x7F
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    // ── TsBuilder ─────────────────────────────────────────────────────

    /// Builds valid 192-byte m2ts packets from PES payloads.
    ///
    /// Each `pes_packet` call wraps the payload in a PES header and
    /// fragments it across 192-byte TS packets with correct PUSI, PID,
    /// continuity counter, and adaptation field padding.
    struct TsBuilder {
        packets: Vec<u8>,
        continuity: HashMap<u16, u8>,
    }

    impl TsBuilder {
        fn new() -> Self {
            Self {
                packets: Vec::new(),
                continuity: HashMap::new(),
            }
        }

        /// Adds a PES packet for the given PID, auto-fragmenting across
        /// TS packets.
        fn pes_packet(mut self, pid: u16, payload: &[u8]) -> Self {
            let pes_data = Self::build_pes(payload);
            self.fragment(pid, &pes_data);
            self
        }

        /// Adds a null packet (PID 0x1FFF).
        fn null_packet(mut self) -> Self {
            let mut pkt = [0u8; M2TS_PACKET_LEN];
            // TP_extra_header (4 bytes, zeros)
            pkt[4] = SYNC_BYTE;
            pkt[5] = 0x1F; // PID high bits
            pkt[6] = 0xFF; // PID low bits = 0x1FFF
            pkt[7] = 0x10; // adaptation=01 (payload only)
            self.packets.extend_from_slice(&pkt);
            self
        }

        /// Adds a packet with an adaptation field only (no payload).
        fn adaptation_only(mut self, pid: u16) -> Self {
            let mut pkt = [0u8; M2TS_PACKET_LEN];
            pkt[4] = SYNC_BYTE;
            #[allow(
                clippy::cast_possible_truncation,
                reason = "PID high 5 bits always fit in u8"
            )]
            {
                pkt[5] = ((pid >> 8) & 0x1F) as u8;
                pkt[6] = (pid & 0xFF) as u8;
            }
            pkt[7] = 0x20; // adaptation=10 (adaptation only)
            pkt[8] = 183; // adaptation field length = rest of packet
            self.packets.extend_from_slice(&pkt);
            self
        }

        /// Adds a PES packet with an adaptation field on the first TS packet.
        fn pes_packet_with_adaptation(mut self, pid: u16, payload: &[u8], af_len: u8) -> Self {
            let pes_data = Self::build_pes(payload);
            self.fragment_with_adaptation(pid, &pes_data, af_len);
            self
        }

        fn build(self) -> Vec<u8> {
            self.packets
        }

        /// Wraps payload in a minimal PES header (no PTS).
        fn build_pes(payload: &[u8]) -> Vec<u8> {
            let mut pes = Vec::with_capacity(9 + payload.len());
            // Start code
            pes.extend_from_slice(&[0x00, 0x00, 0x01]);
            // stream_id (private_stream_1 for IG)
            pes.push(0xBD);
            // PES_packet_length (0 = unbounded)
            pes.extend_from_slice(&[0x00, 0x00]);
            // Flags byte 6: marker bits
            pes.push(0x80);
            // Flags byte 7: no PTS/DTS
            pes.push(0x00);
            // PES_header_data_length
            pes.push(0x00);
            // Payload
            pes.extend_from_slice(payload);
            pes
        }

        /// Wraps payload in a PES header with PTS.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "PTS bit slices always fit in u8"
        )]
        fn build_pes_with_pts(payload: &[u8], pts: u64) -> Vec<u8> {
            let mut pes = Vec::with_capacity(14 + payload.len());
            // Start code
            pes.extend_from_slice(&[0x00, 0x00, 0x01]);
            // stream_id
            pes.push(0xBD);
            // PES_packet_length (0 = unbounded)
            pes.extend_from_slice(&[0x00, 0x00]);
            // Flags byte 6: marker bits
            pes.push(0x80);
            // Flags byte 7: PTS only (0b10xx_xxxx)
            pes.push(0x80);
            // PES_header_data_length = 5 (PTS)
            pes.push(0x05);
            // PTS (5 bytes, MPEG-2 encoding with marker bits)
            pes.push(0x21 | (((pts >> 30) & 0x07) as u8) << 1);
            pes.push(((pts >> 22) & 0xFF) as u8);
            pes.push(0x01 | (((pts >> 15) & 0x7F) as u8) << 1);
            pes.push(((pts >> 7) & 0xFF) as u8);
            pes.push(0x01 | ((pts & 0x7F) as u8) << 1);
            // Payload
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

                // TP_extra_header (zeros)
                pkt[4] = SYNC_BYTE;
                pkt[5] = ((pid >> 8) & 0x1F) as u8 | if first { 0x40 } else { 0x00 };
                pkt[6] = (pid & 0xFF) as u8;

                if needs_padding {
                    // Use adaptation field to pad the final (or only) packet
                    let pad_len = max_payload - chunk_len;
                    pkt[7] = 0x30 | (cc & 0x0F); // adaptation + payload
                    pkt[8] = (pad_len - 1) as u8; // adaptation_field_length
                    if pad_len > 1 {
                        pkt[9] = 0x00; // flags byte
                    }
                    let payload_start = 8 + pad_len;
                    pkt[payload_start..payload_start + chunk_len]
                        .copy_from_slice(&pes_data[offset..offset + chunk_len]);
                } else {
                    pkt[7] = 0x10 | (cc & 0x0F); // payload only
                    pkt[8..8 + chunk_len].copy_from_slice(&pes_data[offset..offset + chunk_len]);
                }

                self.packets.extend_from_slice(&pkt);
                offset += chunk_len;
                first = false;
            }
        }

        /// Fragments PES data with an adaptation field on the first packet.
        #[allow(clippy::cast_possible_truncation, reason = "PID bits always fit in u8")]
        fn fragment_with_adaptation(&mut self, pid: u16, pes_data: &[u8], af_len: u8) {
            let max_payload_area = M2TS_PACKET_LEN - 8; // 184 bytes
            let min_af_total = 1 + usize::from(af_len);
            let first_payload_capacity = max_payload_area - min_af_total;

            // If PES fits in one packet, expand AF to absorb slack
            let chunk_len = pes_data.len().min(first_payload_capacity);
            let slack = first_payload_capacity - chunk_len;
            let actual_af_len = usize::from(af_len) + slack;
            let af_total = 1 + actual_af_len;

            let cc = self.next_cc(pid);
            let mut pkt = [0u8; M2TS_PACKET_LEN];
            pkt[4] = SYNC_BYTE;
            pkt[5] = ((pid >> 8) & 0x1F) as u8 | 0x40; // PUSI
            pkt[6] = (pid & 0xFF) as u8;
            pkt[7] = 0x30 | (cc & 0x0F); // adaptation + payload
            pkt[8] = actual_af_len as u8;

            let payload_start = 8 + af_total;
            pkt[payload_start..payload_start + chunk_len].copy_from_slice(&pes_data[..chunk_len]);

            self.packets.extend_from_slice(&pkt);

            // Fragment remaining data as continuation packets
            if chunk_len < pes_data.len() {
                self.fragment_continuation(pid, &pes_data[chunk_len..]);
            }
        }

        /// Appends continuation TS packets (no PUSI) for remaining PES data.
        #[allow(clippy::cast_possible_truncation, reason = "PID bits always fit in u8")]
        fn fragment_continuation(&mut self, pid: u16, data: &[u8]) {
            let max_payload = M2TS_PACKET_LEN - 8;
            let mut offset = 0;

            while offset < data.len() {
                let remaining = data.len() - offset;
                let chunk_len = remaining.min(max_payload);

                let cc = self.next_cc(pid);
                let mut pkt = [0u8; M2TS_PACKET_LEN];
                pkt[4] = SYNC_BYTE;
                pkt[5] = ((pid >> 8) & 0x1F) as u8;
                pkt[6] = (pid & 0xFF) as u8;
                pkt[7] = 0x10 | (cc & 0x0F); // payload only
                pkt[8..8 + chunk_len].copy_from_slice(&data[offset..offset + chunk_len]);

                self.packets.extend_from_slice(&pkt);
                offset += chunk_len;
            }
        }

        fn next_cc(&mut self, pid: u16) -> u8 {
            let cc = self.continuity.entry(pid).or_insert(0);
            let val = *cc;
            *cc = (*cc + 1) & 0x0F;
            val
        }
    }

    // ── Demuxer tests ─────────────────────────────────────────────────

    #[test]
    fn single_pes_one_packet() {
        let payload = b"hello IG segment";
        let data = TsBuilder::new().pes_packet(0x1400, payload).build();

        let packets = demux(&data).expect("demux should succeed");
        assert_eq!(packets.len(), 1, "should produce one PES packet");
        assert_eq!(packets[0].pid, 0x1400, "PID should match");

        let parsed = parse_pes(&packets[0].data).expect("PES parse should succeed");
        assert_eq!(parsed.payload, payload, "payload should match");
    }

    #[test]
    fn single_pes_spanning_multiple_packets() {
        // 500 bytes needs multiple TS packets (184 bytes payload each)
        let payload: Vec<u8> = (0u16..500).map(|i| (i & 0xFF) as u8).collect();
        let data = TsBuilder::new().pes_packet(0x1400, &payload).build();

        let packets = demux(&data).expect("demux should succeed");
        assert_eq!(packets.len(), 1, "should reassemble into one PES packet");
        assert_eq!(packets[0].pid, 0x1400, "PID should match");

        let parsed = parse_pes(&packets[0].data).expect("PES parse should succeed");
        assert_eq!(parsed.payload, payload, "reassembled payload should match");
    }

    #[test]
    fn multiple_pes_same_pid() {
        let payload_a = b"first IG segment";
        let payload_b = b"second IG segment";
        let data = TsBuilder::new()
            .pes_packet(0x1400, payload_a)
            .pes_packet(0x1400, payload_b)
            .build();

        let packets = demux(&data).expect("demux should succeed");
        assert_eq!(packets.len(), 2, "should produce two PES packets");

        assert!(
            packets.iter().all(|p| p.pid == 0x1400),
            "all packets should have PID 0x1400"
        );

        let parsed_a = parse_pes(&packets[0].data).expect("first PES parse should succeed");
        let parsed_b = parse_pes(&packets[1].data).expect("second PES parse should succeed");
        assert_eq!(
            parsed_a.payload,
            payload_a.as_slice(),
            "first payload should match"
        );
        assert_eq!(
            parsed_b.payload,
            payload_b.as_slice(),
            "second payload should match"
        );
    }

    #[test]
    fn mixed_pids() {
        let ig_payload = b"IG data";
        let video_payload = b"video data";
        let data = TsBuilder::new()
            .pes_packet(0x1400, ig_payload)
            .pes_packet(0x1011, video_payload)
            .build();

        let packets = demux(&data).expect("demux should succeed");
        assert_eq!(packets.len(), 2, "should produce two PES packets");

        let ig: Vec<&PesPacket> = packets.iter().filter(|p| p.pid == 0x1400).collect();
        let video: Vec<&PesPacket> = packets.iter().filter(|p| p.pid == 0x1011).collect();

        assert_eq!(ig.len(), 1, "should have one IG packet");
        assert_eq!(video.len(), 1, "should have one video packet");

        let parsed_ig = parse_pes(&ig[0].data).expect("IG PES parse should succeed");
        let parsed_video = parse_pes(&video[0].data).expect("video PES parse should succeed");
        assert_eq!(
            parsed_ig.payload,
            ig_payload.as_slice(),
            "IG payload should match"
        );
        assert_eq!(
            parsed_video.payload,
            video_payload.as_slice(),
            "video payload should match"
        );
    }

    #[test]
    fn null_packets_skipped() {
        let payload = b"real data";
        let data = TsBuilder::new()
            .null_packet()
            .pes_packet(0x1400, payload)
            .null_packet()
            .build();

        let packets = demux(&data).expect("demux should succeed");
        assert_eq!(packets.len(), 1, "null packets should be skipped");
        assert_eq!(packets[0].pid, 0x1400, "PID should match real packet");
    }

    #[test]
    fn adaptation_field_with_payload() {
        let payload = b"after adaptation";
        let data = TsBuilder::new()
            .pes_packet_with_adaptation(0x1400, payload, 10)
            .build();

        let packets = demux(&data).expect("demux should succeed");
        assert_eq!(packets.len(), 1, "should produce one PES packet");

        let parsed = parse_pes(&packets[0].data).expect("PES parse should succeed");
        assert_eq!(
            parsed.payload,
            payload.as_slice(),
            "payload should survive adaptation field"
        );
    }

    #[test]
    fn adaptation_only_skipped() {
        let payload = b"real data";
        let data = TsBuilder::new()
            .adaptation_only(0x1400)
            .pes_packet(0x1400, payload)
            .build();

        let packets = demux(&data).expect("demux should succeed");
        assert_eq!(
            packets.len(),
            1,
            "adaptation-only packet should not create a PES"
        );

        let parsed = parse_pes(&packets[0].data).expect("PES parse should succeed");
        assert_eq!(parsed.payload, payload.as_slice(), "payload should match");
    }

    #[test]
    fn empty_input() {
        let packets = demux(&[]).expect("empty input should succeed");
        assert!(packets.is_empty(), "empty input should produce no packets");
    }

    #[test]
    fn trailing_bytes_ignored() {
        let payload = b"data";
        let mut data = TsBuilder::new().pes_packet(0x1400, payload).build();
        // Add trailing bytes (less than one full packet)
        data.extend_from_slice(&[0xFF; 100]);

        let packets = demux(&data).expect("trailing bytes should be ignored");
        assert_eq!(packets.len(), 1, "should still produce one PES packet");
    }

    #[test]
    fn invalid_sync_byte() {
        let mut data = TsBuilder::new().pes_packet(0x1400, b"data").build();
        // Corrupt the sync byte of the first packet
        data[4] = 0x00;

        let err = demux(&data).expect_err("bad sync should produce error");
        assert!(
            matches!(
                err,
                TsError::InvalidSync {
                    offset: 4,
                    found: 0x00
                }
            ),
            "should report InvalidSync at offset 4"
        );
    }

    // ── PES parser tests ──────────────────────────────────────────────

    #[test]
    fn pes_no_pts() {
        let payload = b"elementary stream data";
        let pes = TsBuilder::build_pes(payload);

        let parsed = parse_pes(&pes).expect("PES parse should succeed");
        assert!(parsed.pts.is_none(), "PTS should be absent");
        assert_eq!(parsed.payload, payload, "payload should match");
    }

    #[test]
    fn pes_with_pts() {
        let payload = b"timed data";
        let pts: u64 = 90_000; // 1 second at 90 kHz
        let pes = TsBuilder::build_pes_with_pts(payload, pts);

        let parsed = parse_pes(&pes).expect("PES parse should succeed");
        assert_eq!(parsed.pts, Some(pts), "PTS should be 90000");
        assert_eq!(parsed.payload, payload, "payload should match");
    }

    #[test]
    #[allow(
        clippy::cast_possible_truncation,
        reason = "PTS bit slices always fit in u8"
    )]
    fn pes_with_pts_and_dts() {
        let payload = b"bidirectional frame";
        let pts: u64 = 180_000;
        let dts: u64 = 162_000;
        let mut pes = Vec::new();
        pes.extend_from_slice(&[0x00, 0x00, 0x01]); // start code
        pes.push(0xE0); // stream_id (video)
        pes.extend_from_slice(&[0x00, 0x00]); // length
        pes.push(0x80); // flags
        pes.push(0xC0); // PTS + DTS (0b11xx_xxxx)
        pes.push(0x0A); // header_data_length = 10 (5 PTS + 5 DTS)
        // PTS
        pes.push(0x31 | (((pts >> 30) & 0x07) as u8) << 1);
        pes.push(((pts >> 22) & 0xFF) as u8);
        pes.push(0x01 | (((pts >> 15) & 0x7F) as u8) << 1);
        pes.push(((pts >> 7) & 0xFF) as u8);
        pes.push(0x01 | ((pts & 0x7F) as u8) << 1);
        // DTS
        pes.push(0x11 | (((dts >> 30) & 0x07) as u8) << 1);
        pes.push(((dts >> 22) & 0xFF) as u8);
        pes.push(0x01 | (((dts >> 15) & 0x7F) as u8) << 1);
        pes.push(((dts >> 7) & 0xFF) as u8);
        pes.push(0x01 | ((dts & 0x7F) as u8) << 1);
        // Payload
        pes.extend_from_slice(payload);

        let parsed = parse_pes(&pes).expect("PES parse should succeed");
        assert_eq!(parsed.pts, Some(pts), "PTS should be extracted");
        assert_eq!(parsed.payload, payload, "payload should match");
    }

    #[test]
    fn pes_with_extra_header_data() {
        let payload = b"extended header";
        let mut pes = Vec::new();
        pes.extend_from_slice(&[0x00, 0x00, 0x01]);
        pes.push(0xBD);
        pes.extend_from_slice(&[0x00, 0x00]);
        pes.push(0x80);
        pes.push(0x00); // no PTS/DTS
        pes.push(0x03); // header_data_length = 3
        pes.extend_from_slice(&[0xFF, 0xFF, 0xFF]); // stuffing
        pes.extend_from_slice(payload);

        let parsed = parse_pes(&pes).expect("PES parse should succeed");
        assert!(parsed.pts.is_none(), "PTS should be absent");
        assert_eq!(parsed.payload, payload, "should skip header extension");
    }

    #[test]
    fn pes_invalid_start_code() {
        let data = [0x00, 0x00, 0x00, 0xBD, 0x00, 0x00, 0x80, 0x00, 0x00];
        let err = parse_pes(&data).expect_err("bad start code should fail");
        assert!(
            matches!(err, PesError::InvalidStartCode),
            "should report InvalidStartCode"
        );
    }

    #[test]
    fn pes_truncated() {
        let data = [0x00, 0x00, 0x01, 0xBD, 0x00];
        let err = parse_pes(&data).expect_err("truncated PES should fail");
        assert!(
            matches!(err, PesError::Truncated),
            "should report Truncated"
        );
    }

    // ── Filtering tests (caller-side) ─────────────────────────────────

    #[test]
    fn filter_by_pid() {
        let data = TsBuilder::new()
            .pes_packet(0x1400, b"IG one")
            .pes_packet(0x1011, b"video")
            .pes_packet(0x1400, b"IG two")
            .build();

        let all = demux(&data).expect("demux should succeed");
        let ig: Vec<&PesPacket> = all.iter().filter(|p| p.pid == 0x1400).collect();

        assert_eq!(ig.len(), 2, "should find two IG packets");
        for p in &ig {
            assert_eq!(p.pid, 0x1400, "filtered packets should have IG PID");
        }
    }

    #[test]
    fn filter_absent_pid() {
        let data = TsBuilder::new().pes_packet(0x1400, b"IG data").build();

        let all = demux(&data).expect("demux should succeed");
        assert!(
            !all.iter().any(|p| p.pid == 0x1234),
            "absent PID should produce no results"
        );
    }

    // ── Integration test ──────────────────────────────────────────────

    #[test]
    fn roundtrip_mixed_pid_stream() {
        let ig_payload_1 = vec![0xAAu8; 300];
        let ig_payload_2 = vec![0xBBu8; 150];
        let video_payload = vec![0xCCu8; 500];

        let data = TsBuilder::new()
            .pes_packet(0x1400, &ig_payload_1)
            .pes_packet(0x1011, &video_payload)
            .pes_packet(0x1400, &ig_payload_2)
            .build();

        let all = demux(&data).expect("demux should succeed");

        // Filter and parse IG
        let ig_packets: Vec<&PesPacket> = all.iter().filter(|p| p.pid == 0x1400).collect();
        assert_eq!(ig_packets.len(), 2, "should find two IG PES packets");

        let ig_parsed_1 = parse_pes(&ig_packets[0].data).expect("IG PES 1 should parse");
        let ig_parsed_2 = parse_pes(&ig_packets[1].data).expect("IG PES 2 should parse");
        assert_eq!(
            ig_parsed_1.payload, ig_payload_1,
            "IG payload 1 should match"
        );
        assert_eq!(
            ig_parsed_2.payload, ig_payload_2,
            "IG payload 2 should match"
        );

        // Filter and parse video
        let video_packets: Vec<&PesPacket> = all.iter().filter(|p| p.pid == 0x1011).collect();
        assert_eq!(video_packets.len(), 1, "should find one video PES packet");

        let video_parsed = parse_pes(&video_packets[0].data).expect("video PES should parse");
        assert_eq!(
            video_parsed.payload, video_payload,
            "video payload should match"
        );
    }

    // ── PTS roundtrip ─────────────────────────────────────────────────

    #[test]
    fn pts_roundtrip_various_values() {
        let test_values: &[u64] = &[
            0,
            1,
            90_000,        // 1 second
            8_100_000,     // 90 seconds
            0x1_FFFF_FFFF, // max 33-bit value
        ];

        for &pts in test_values {
            let pes = TsBuilder::build_pes_with_pts(b"test", pts);
            let parsed = parse_pes(&pes).expect("PES parse should succeed");
            assert_eq!(
                parsed.pts,
                Some(pts),
                "PTS roundtrip failed for value {pts}"
            );
        }
    }
}
