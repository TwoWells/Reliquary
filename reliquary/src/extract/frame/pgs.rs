// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! PGS (Presentation Graphic Stream) display set segmentation.
//!
//! PGS is the only subtitle format on Blu-ray. Segments share header
//! format with IG streams (parsed in `disc::bdmv::ig`): a 1-byte type
//! followed by a 2-byte big-endian length. Unlike IG, PGS subtitles are
//! passed through opaquely — the player's subtitle decoder handles
//! rendering.
//!
//! Each Matroska block contains one complete display set: all segments
//! from the first segment through the End of Display Set marker (0x80).
//! Every display set is independently decodable, so all frames are
//! keyframes.
//!
//! Matroska codec ID: `S_HDMV/PGS`. No `CodecPrivate` needed.
//!
//! Reference: US Patent 2009/0185789 (PGS segment format),
//! libbluray `src/libbluray/decoders/pg_decode.c`.

use super::Frame;

// ── Constants ──────────────────────────────────────────────────────────

/// Segment header size: type (1 byte) + length (2 bytes).
const SEGMENT_HEADER_SIZE: usize = 3;

/// End of Display Set segment type.
const SEG_END_OF_DISPLAY: u8 = 0x80;

// ── Framer ─────────────────────────────────────────────────────────────

/// PGS subtitle framer.
///
/// Accumulates elementary stream bytes and emits complete [`Frame`]s
/// when an End of Display Set segment (0x80) is encountered. Each
/// emitted frame contains one complete display set — all segments from
/// the start through the end marker, inclusive.
///
/// # Timestamps
///
/// The presentation timestamp passed to [`feed`](Self::feed) is assigned
/// to the next emitted display set. The framer does not interpolate or
/// synthesize timestamps.
///
/// # Display set structure
///
/// A typical display set contains:
/// 1. Palette Definition Segment (0x14)
/// 2. Object Definition Segment(s) (0x15)
/// 3. Presentation Composition Segment (0x16) or Window Definition (0x17)
/// 4. End of Display Set Segment (0x80)
///
/// The framer does not validate segment ordering or types — it simply
/// accumulates until the end marker, then emits everything as one block.
#[derive(Default)]
pub struct PgsFramer {
    /// Accumulation buffer for the current display set.
    buf: Vec<u8>,
    /// Byte offset where the current display set starts within `buf`.
    display_set_start: usize,
    /// Most recent PTS from the demuxer.
    last_pts: u64,
}

impl PgsFramer {
    /// Creates a new framer with an empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds elementary stream bytes into the framer.
    ///
    /// `pts` is the presentation timestamp (90 kHz clock) from the PES
    /// header, if available. It applies to the next emitted display set.
    ///
    /// Returns all complete display sets detected in the accumulated
    /// buffer.
    pub fn feed(&mut self, pts: Option<u64>, data: &[u8]) -> Vec<Frame> {
        if let Some(p) = pts {
            self.last_pts = p;
        }
        self.buf.extend_from_slice(data);

        let mut frames = Vec::new();
        while let Some(frame) = self.next_display_set() {
            frames.push(frame);
        }
        frames
    }

    /// Discards any buffered partial display set data.
    ///
    /// A partial display set at end-of-stream is incomplete and cannot
    /// be decoded. Returns an empty list.
    pub fn flush(&mut self) -> Vec<Frame> {
        self.buf.clear();
        self.display_set_start = 0;
        Vec::new()
    }

    /// Returns the Matroska `CodecPrivate` payload.
    ///
    /// Always empty for PGS — no codec configuration is needed.
    #[must_use]
    pub const fn codec_private(&self) -> &[u8] {
        &[]
    }

    /// Attempts to extract the next complete display set from the buffer.
    ///
    /// Scans segment headers sequentially from `display_set_start`. When
    /// an End of Display Set segment is found, emits everything from
    /// `display_set_start` through the end of that segment as one frame.
    fn next_display_set(&mut self) -> Option<Frame> {
        let mut pos = self.display_set_start;

        loop {
            // Need at least a segment header to continue.
            if pos + SEGMENT_HEADER_SIZE > self.buf.len() {
                return None;
            }

            let seg_type = self.buf[pos];
            let seg_length =
                usize::from(u16::from_be_bytes([self.buf[pos + 1], self.buf[pos + 2]]));
            let seg_end = pos + SEGMENT_HEADER_SIZE + seg_length;

            // Wait for the complete segment payload.
            if seg_end > self.buf.len() {
                return None;
            }

            if seg_type == SEG_END_OF_DISPLAY {
                // Emit everything from display_set_start through seg_end.
                let data = self.buf[self.display_set_start..seg_end].to_vec();
                let pts = self.last_pts;

                // Remove emitted bytes from the buffer.
                self.buf.drain(..seg_end);
                self.display_set_start = 0;

                return Some(Frame {
                    pts,
                    dts: None,
                    data,
                    keyframe: true,
                });
            }

            // Advance past this segment.
            pos = seg_end;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::similar_names,
        reason = "framer/frame/frames are distinct concepts in tests"
    )]
    #![allow(
        clippy::expect_used,
        reason = "expect is appropriate in tests for infallible conversions"
    )]

    use super::*;

    // ── Constants ─────────────────────────────────────────────────────

    /// Palette Definition Segment type.
    const SEG_PALETTE: u8 = 0x14;
    /// Object Definition Segment type.
    const SEG_OBJECT: u8 = 0x15;
    /// Presentation Composition Segment type.
    const SEG_PCS: u8 = 0x16;
    /// Window Definition Segment type.
    const SEG_WDS: u8 = 0x17;

    // ── Helpers ───────────────────────────────────────────────────────

    /// Builds a PGS segment with the given type and payload.
    fn segment(seg_type: u8, payload: &[u8]) -> Vec<u8> {
        let len = u16::try_from(payload.len()).expect("payload fits in u16");
        let mut seg = Vec::with_capacity(SEGMENT_HEADER_SIZE + payload.len());
        seg.push(seg_type);
        seg.extend_from_slice(&len.to_be_bytes());
        seg.extend_from_slice(payload);
        seg
    }

    /// Builds a minimal display set: PCS + END.
    fn minimal_display_set() -> Vec<u8> {
        let mut ds = segment(SEG_PCS, &[0x00; 11]);
        ds.extend_from_slice(&segment(SEG_END_OF_DISPLAY, &[]));
        ds
    }

    /// Builds a typical display set: PDS + ODS + PCS + END.
    fn typical_display_set(object_size: usize) -> Vec<u8> {
        let mut ds = segment(SEG_PALETTE, &[0x00; 5]);
        ds.extend_from_slice(&segment(SEG_OBJECT, &vec![0xAA; object_size]));
        ds.extend_from_slice(&segment(SEG_PCS, &[0x00; 11]));
        ds.extend_from_slice(&segment(SEG_END_OF_DISPLAY, &[]));
        ds
    }

    /// Builds a display set with a Window Definition Segment instead of PCS.
    fn wds_display_set() -> Vec<u8> {
        let mut ds = segment(SEG_PALETTE, &[0x00; 5]);
        ds.extend_from_slice(&segment(SEG_OBJECT, &[0xBB; 20]));
        ds.extend_from_slice(&segment(SEG_WDS, &[0x00; 10]));
        ds.extend_from_slice(&segment(SEG_END_OF_DISPLAY, &[]));
        ds
    }

    // ── Single display set ────────────────────────────────────────────

    #[test]
    fn single_display_set() {
        let mut framer = PgsFramer::new();
        let ds = typical_display_set(50);
        let frames = framer.feed(Some(1000), &ds);

        assert_eq!(frames.len(), 1, "should emit exactly one display set");
        assert_eq!(frames[0].pts, 1000, "PTS should match input");
        assert!(frames[0].keyframe, "PGS frames are always keyframes");
        assert!(frames[0].dts.is_none(), "PGS has no decode timestamp");
        assert_eq!(
            frames[0].data.len(),
            ds.len(),
            "frame data should contain the entire display set"
        );
    }

    #[test]
    fn minimal_display_set_emits() {
        let mut framer = PgsFramer::new();
        let ds = minimal_display_set();
        let frames = framer.feed(Some(0), &ds);

        assert_eq!(frames.len(), 1, "minimal display set should emit");
        assert_eq!(frames[0].data.len(), ds.len(), "data length matches");
    }

    #[test]
    fn wds_display_set_emits() {
        let mut framer = PgsFramer::new();
        let ds = wds_display_set();
        let frames = framer.feed(Some(500), &ds);

        assert_eq!(frames.len(), 1, "WDS-based display set should emit");
        assert_eq!(frames[0].data.len(), ds.len(), "data length matches");
    }

    // ── Multiple display sets ─────────────────────────────────────────

    #[test]
    fn multiple_display_sets_single_feed() {
        let mut framer = PgsFramer::new();
        let ds1 = typical_display_set(30);
        let ds2 = typical_display_set(60);
        let ds3 = minimal_display_set();

        let mut data = ds1.clone();
        data.extend_from_slice(&ds2);
        data.extend_from_slice(&ds3);

        let frames = framer.feed(Some(0), &data);

        assert_eq!(frames.len(), 3, "should emit three display sets");
        assert_eq!(frames[0].data.len(), ds1.len(), "first display set size");
        assert_eq!(frames[1].data.len(), ds2.len(), "second display set size");
        assert_eq!(frames[2].data.len(), ds3.len(), "third display set size");
    }

    #[test]
    fn consecutive_feeds_each_emit() {
        let mut framer = PgsFramer::new();

        let f1 = framer.feed(Some(1000), &typical_display_set(20));
        assert_eq!(f1.len(), 1, "first feed emits");
        assert_eq!(f1[0].pts, 1000, "first PTS");

        let f2 = framer.feed(Some(5000), &typical_display_set(40));
        assert_eq!(f2.len(), 1, "second feed emits");
        assert_eq!(f2[0].pts, 5000, "second PTS");
    }

    // ── Partial display set buffering ─────────────────────────────────

    #[test]
    fn partial_display_set_across_feeds() {
        let mut framer = PgsFramer::new();
        let ds = typical_display_set(100);
        let mid = ds.len() / 2;

        let frames1 = framer.feed(Some(2000), &ds[..mid]);
        assert!(frames1.is_empty(), "partial display set should not emit");

        let frames2 = framer.feed(None, &ds[mid..]);
        assert_eq!(frames2.len(), 1, "completing the display set should emit");
        assert_eq!(frames2[0].pts, 2000, "PTS from first feed applies");
        assert_eq!(
            frames2[0].data.len(),
            ds.len(),
            "frame contains entire display set"
        );
    }

    #[test]
    fn partial_segment_header_waits() {
        let mut framer = PgsFramer::new();

        // Feed just the segment type byte — not enough for a full header.
        let frames = framer.feed(Some(100), &[SEG_PCS]);
        assert!(frames.is_empty(), "one header byte should not emit");

        // Feed the rest of a minimal display set.
        let mut rest = Vec::new();
        // Complete the PCS segment header + payload.
        let pcs_len: u16 = 11;
        rest.extend_from_slice(&pcs_len.to_be_bytes());
        rest.extend_from_slice(&[0x00; 11]);
        // End of display set.
        rest.extend_from_slice(&segment(SEG_END_OF_DISPLAY, &[]));

        let frames = framer.feed(None, &rest);
        assert_eq!(frames.len(), 1, "completing the data should emit");
    }

    #[test]
    fn partial_segment_payload_waits() {
        let mut framer = PgsFramer::new();

        // Feed a segment header claiming 100 bytes but only give 50.
        let mut data = vec![SEG_OBJECT];
        data.extend_from_slice(&100_u16.to_be_bytes());
        data.extend_from_slice(&[0xAA; 50]);

        let frames = framer.feed(Some(0), &data);
        assert!(
            frames.is_empty(),
            "incomplete segment payload should not emit"
        );

        // Feed the remaining 50 bytes + end segment.
        let mut rest = vec![0xAA; 50];
        rest.extend_from_slice(&segment(SEG_END_OF_DISPLAY, &[]));

        let frames = framer.feed(None, &rest);
        assert_eq!(frames.len(), 1, "completing payload + end should emit");
    }

    #[test]
    fn byte_at_a_time_feed() {
        let mut framer = PgsFramer::new();
        let ds = typical_display_set(20);

        let mut total_frames = Vec::new();
        for &byte in &ds {
            total_frames.extend(framer.feed(None, &[byte]));
        }

        assert_eq!(total_frames.len(), 1, "should emit one display set");
        assert_eq!(
            total_frames[0].data.len(),
            ds.len(),
            "frame contains entire display set"
        );
    }

    // ── Multi-object display sets ─────────────────────────────────────

    #[test]
    fn multi_object_display_set() {
        let mut framer = PgsFramer::new();

        // Large bitmap split across multiple ODS segments.
        let mut ds = segment(SEG_PALETTE, &[0x00; 5]);
        ds.extend_from_slice(&segment(SEG_OBJECT, &[0xAA; 200]));
        ds.extend_from_slice(&segment(SEG_OBJECT, &[0xBB; 200]));
        ds.extend_from_slice(&segment(SEG_OBJECT, &[0xCC; 200]));
        ds.extend_from_slice(&segment(SEG_PCS, &[0x00; 11]));
        ds.extend_from_slice(&segment(SEG_END_OF_DISPLAY, &[]));

        let frames = framer.feed(Some(3000), &ds);

        assert_eq!(frames.len(), 1, "multi-object display set emits as one");
        assert_eq!(frames[0].data.len(), ds.len(), "all segments included");
    }

    // ── PTS assignment ────────────────────────────────────────────────

    #[test]
    fn pts_carried_forward_without_explicit_pts() {
        let mut framer = PgsFramer::new();

        let f1 = framer.feed(Some(5000), &typical_display_set(20));
        assert_eq!(f1[0].pts, 5000, "first frame gets explicit PTS");

        let f2 = framer.feed(None, &typical_display_set(20));
        assert_eq!(f2[0].pts, 5000, "second frame carries forward last PTS");
    }

    #[test]
    fn pts_updates_on_new_value() {
        let mut framer = PgsFramer::new();

        let f1 = framer.feed(Some(1000), &typical_display_set(20));
        assert_eq!(f1[0].pts, 1000, "first PTS");

        let f2 = framer.feed(Some(9000), &typical_display_set(20));
        assert_eq!(f2[0].pts, 9000, "updated PTS");

        let f3 = framer.feed(None, &typical_display_set(20));
        assert_eq!(f3[0].pts, 9000, "carries forward updated PTS");
    }

    #[test]
    fn pts_defaults_to_zero() {
        let mut framer = PgsFramer::new();
        let frames = framer.feed(None, &typical_display_set(20));
        assert_eq!(frames[0].pts, 0, "default PTS is 0");
    }

    // ── Frame data boundaries ─────────────────────────────────────────

    #[test]
    fn frame_starts_with_valid_segment_header() {
        let mut framer = PgsFramer::new();
        let frames = framer.feed(Some(0), &typical_display_set(50));

        assert_eq!(frames.len(), 1, "should emit one frame");
        // First byte should be a known PGS segment type.
        let first_type = frames[0].data[0];
        assert!(
            [SEG_PALETTE, SEG_OBJECT, SEG_PCS, SEG_WDS].contains(&first_type),
            "frame should start with a valid segment type, got {first_type:#04x}"
        );
    }

    #[test]
    fn frame_ends_with_end_segment() {
        let mut framer = PgsFramer::new();
        let frames = framer.feed(Some(0), &typical_display_set(50));

        assert_eq!(frames.len(), 1, "should emit one frame");
        let data = &frames[0].data;
        // The last segment should be END (type 0x80, length 0x0000).
        assert!(
            data.len() >= SEGMENT_HEADER_SIZE,
            "frame should be at least one segment header"
        );
        let end_seg = &data[data.len() - SEGMENT_HEADER_SIZE..];
        assert_eq!(
            end_seg,
            &[SEG_END_OF_DISPLAY, 0x00, 0x00],
            "frame should end with END segment (0x80, length 0)"
        );
    }

    // ── CodecPrivate ──────────────────────────────────────────────────

    #[test]
    fn codec_private_is_empty() {
        let framer = PgsFramer::new();
        assert!(
            framer.codec_private().is_empty(),
            "PGS CodecPrivate is always empty"
        );
    }

    // ── Flush ─────────────────────────────────────────────────────────

    #[test]
    fn flush_discards_partial() {
        let mut framer = PgsFramer::new();
        let ds = typical_display_set(50);

        // Feed half a display set.
        framer.feed(Some(0), &ds[..ds.len() / 2]);

        let flushed = framer.flush();
        assert!(
            flushed.is_empty(),
            "partial display set is discarded on flush"
        );

        // Framer should work normally after flush.
        let frames = framer.feed(Some(100), &typical_display_set(20));
        assert_eq!(frames.len(), 1, "framer works after flush");
    }

    #[test]
    fn flush_on_empty_buffer() {
        let mut framer = PgsFramer::new();
        let flushed = framer.flush();
        assert!(flushed.is_empty(), "flush on empty produces nothing");
    }

    // ── Edge cases ────────────────────────────────────────────────────

    #[test]
    fn empty_feed() {
        let mut framer = PgsFramer::new();
        let frames = framer.feed(None, &[]);
        assert!(frames.is_empty(), "empty input produces no frames");
    }

    #[test]
    fn end_segment_only() {
        let mut framer = PgsFramer::new();
        // A display set that is just an END marker (clear subtitle).
        let ds = segment(SEG_END_OF_DISPLAY, &[]);
        let frames = framer.feed(Some(7000), &ds);

        assert_eq!(frames.len(), 1, "end-only display set should emit");
        assert_eq!(
            frames[0].data.len(),
            SEGMENT_HEADER_SIZE,
            "frame is just the END segment header"
        );
        assert_eq!(frames[0].pts, 7000, "PTS matches");
    }

    #[test]
    fn two_display_sets_split_across_feeds() {
        let mut framer = PgsFramer::new();
        let ds1 = typical_display_set(30);
        let ds2 = typical_display_set(60);

        let mut combined = ds1.clone();
        combined.extend_from_slice(&ds2);

        // Split in the middle of the second display set.
        let split = ds1.len() + ds2.len() / 2;

        let f1 = framer.feed(Some(1000), &combined[..split]);
        assert_eq!(f1.len(), 1, "first display set emits");
        assert_eq!(f1[0].data.len(), ds1.len(), "first display set size");

        let f2 = framer.feed(Some(2000), &combined[split..]);
        assert_eq!(f2.len(), 1, "second display set completes");
        assert_eq!(f2[0].data.len(), ds2.len(), "second display set size");
    }

    #[test]
    fn framer_works_after_flush() {
        let mut framer = PgsFramer::new();

        framer.feed(Some(0), &typical_display_set(20));
        framer.flush();

        let frames = framer.feed(Some(500), &typical_display_set(30));
        assert_eq!(frames.len(), 1, "framer works normally after flush");
        assert_eq!(frames[0].pts, 500, "PTS correct after flush");
    }
}
