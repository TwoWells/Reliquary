// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Codec framing — access unit detection and `CodecPrivate` extraction.
//!
//! Each codec framer takes raw elementary stream bytes (from either the
//! Blu-ray MPEG-TS or DVD MPEG-PS demuxer) and produces [`Frame`]s:
//! complete, independently addressable access units ready for Matroska
//! block writing.

pub mod ac3;
pub mod mpeg2;

/// A framed access unit ready for the Matroska muxer.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Presentation timestamp (90 kHz clock).
    pub pts: u64,
    /// Decode timestamp, if different from PTS.
    pub dts: Option<u64>,
    /// Frame payload (one complete access unit).
    pub data: Vec<u8>,
    /// Whether this frame is a random access point.
    pub keyframe: bool,
}
