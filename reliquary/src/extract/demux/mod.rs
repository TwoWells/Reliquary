// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Demuxing — extract elementary stream units from container formats.
//!
//! Each demuxer reads a container format (MPEG-TS for Blu-ray, MPEG-PS for
//! DVD) and produces [`DemuxedUnit`]s: raw elementary stream payloads tagged
//! with stream identity and timestamps. The framing layer then processes
//! these into codec access units.

pub mod ps;
pub mod ts;

/// Identifies an elementary stream within a container.
///
/// Wraps the container-level stream identifier (TS PID for Blu-ray, packed
/// stream/sub-stream ID for DVD) along with the MPEG coding type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamId {
    /// Container-level stream identifier.
    ///
    /// For MPEG-TS (Blu-ray): the 13-bit PID from the TS header.
    /// For MPEG-PS (DVD): packed as `(stream_id << 8) | sub_stream_id`.
    pub pid: u16,
    /// MPEG coding type from the playlist metadata.
    ///
    /// Determines which codec framer to use. Common values:
    /// - `0x02` — MPEG-2 video
    /// - `0x1b` — H.264/AVC
    /// - `0x24` — HEVC
    /// - `0xea` — VC-1
    /// - `0x80` — LPCM
    /// - `0x81` — AC-3
    /// - `0x82` — DTS
    /// - `0x83` — `TrueHD`
    /// - `0x86` — DTS-HD MA
    /// - `0x90` — PGS
    pub coding_type: u8,
}

/// A demuxed elementary stream unit.
///
/// Contains one complete PES packet's payload with headers stripped and
/// timestamps extracted. Produced by container demuxers and consumed by
/// codec framers.
#[derive(Debug, Clone)]
pub struct DemuxedUnit {
    /// Stream this unit belongs to.
    pub stream_id: StreamId,
    /// Presentation timestamp (90 kHz clock), if present in the PES header.
    pub pts: Option<u64>,
    /// Decode timestamp (90 kHz clock), if present in the PES header.
    ///
    /// Only present when DTS differs from PTS (video B-frames).
    pub dts: Option<u64>,
    /// Raw elementary stream payload (PES header stripped).
    pub payload: Vec<u8>,
}
