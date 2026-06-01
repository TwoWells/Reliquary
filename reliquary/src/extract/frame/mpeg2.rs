// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! MPEG-2 video access unit framing and sequence header extraction.
//!
//! MPEG-2 video uses byte-aligned start codes (`0x000001xx`) to delimit
//! structural elements. The framer detects picture boundaries (start code
//! `0x00000100`) and emits one [`Frame`] per picture.
//!
//! Keyframe detection reads the `picture_coding_type` field from the
//! picture header — type 1 is an I-frame (random access point).
//!
//! Matroska codec ID: `V_MPEG2`. `CodecPrivate` is the sequence header
//! plus its sequence extension (start codes `0x000001B3` and `0x000001B5`),
//! giving the decoder resolution, frame rate, profile, and level.
//!
//! Reference: ISO/IEC 13818-2 (MPEG-2 Video).

use super::Frame;

// ── Constants ──────────────────────────────────────────────────────────

/// Start code prefix — three bytes preceding every start code ID.
const START_CODE_PREFIX: [u8; 3] = [0x00, 0x00, 0x01];

/// Picture start code ID.
const PICTURE_START_CODE: u8 = 0x00;

/// Sequence header start code ID.
const SEQUENCE_HEADER_CODE: u8 = 0xB3;

/// Extension start code ID (follows sequence header for profile/level).
const EXTENSION_START_CODE: u8 = 0xB5;

/// Minimum bytes in a picture header needed to read `picture_coding_type`.
/// Start code (4) + `temporal_reference` (10 bits) + `picture_coding_type` (3 bits)
/// = 4 bytes of start code + 2 bytes of header data = 6 bytes total.
const MIN_PICTURE_HEADER: usize = 6;

// ── Start code scanning ───────────────────────────────────────────────

/// Finds the byte offset of the next start code prefix (`0x000001`) in
/// `buf`, starting from `offset`.
fn find_start_code(buf: &[u8], offset: usize) -> Option<usize> {
    if buf.len() < offset + 3 {
        return None;
    }
    buf[offset..]
        .windows(3)
        .position(|w| w == START_CODE_PREFIX)
        .map(|pos| pos + offset)
}

/// Reads the `picture_coding_type` from a picture header at `pos`.
///
/// `pos` is the offset of the `0x000001` prefix. The picture header
/// layout (after the 4-byte start code) is:
///   - `temporal_reference`: 10 bits
///   - `picture_coding_type`: 3 bits
///
/// Returns `None` if there aren't enough bytes.
fn picture_coding_type(buf: &[u8], pos: usize) -> Option<u8> {
    if buf.len() < pos + MIN_PICTURE_HEADER {
        return None;
    }
    // Byte 4 (offset+4): temporal_reference[9:2] (8 bits)
    // Byte 5 (offset+5): temporal_reference[1:0] (2 bits) | picture_coding_type (3 bits) | ...
    let pct = (buf[pos + 5] >> 3) & 0x07;
    Some(pct)
}

// ── Framer ────────────────────────────────────────────────────────────

/// MPEG-2 video framer.
///
/// Accumulates elementary stream bytes and emits complete [`Frame`]s at
/// picture start code boundaries. Each emitted frame contains all bytes
/// from one picture start code up to (but not including) the next.
///
/// # Timestamps
///
/// The presentation timestamp passed to [`feed`](Self::feed) is assigned
/// to the next emitted picture. DTS is passed through when the demuxer
/// provides it (B-frames have DTS != PTS). The framer does not
/// interpolate or synthesize timestamps.
///
/// # `CodecPrivate`
///
/// The sequence header (`0x000001B3`) and its trailing sequence extension
/// (`0x000001B5`) are captured on first encounter and exposed via
/// [`codec_private`](Self::codec_private).
#[derive(Default)]
pub struct Mpeg2Framer {
    /// Accumulation buffer for incomplete pictures.
    buf: Vec<u8>,
    /// Offset of the current picture start code within `buf`, if one has
    /// been seen. `None` means we haven't yet found a picture start code.
    picture_start: Option<usize>,
    /// Whether the current picture is an I-frame.
    current_keyframe: bool,
    /// Sequence header + extension bytes for Matroska `CodecPrivate`.
    codec_private: Option<Vec<u8>>,
    /// Most recent PTS from the demuxer.
    last_pts: u64,
    /// Most recent DTS from the demuxer.
    last_dts: Option<u64>,
}

impl Mpeg2Framer {
    /// Creates a new framer with an empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds elementary stream bytes into the framer.
    ///
    /// `pts` is the presentation timestamp (90 kHz clock) from the PES
    /// header. `dts` is the decode timestamp, if different from PTS
    /// (present for B-frames).
    ///
    /// Returns all complete pictures detected in the accumulated buffer.
    pub fn feed(&mut self, pts: Option<u64>, dts: Option<u64>, data: &[u8]) -> Vec<Frame> {
        if let Some(p) = pts {
            self.last_pts = p;
        }
        if dts.is_some() {
            self.last_dts = dts;
        }
        self.buf.extend_from_slice(data);

        let mut frames = Vec::new();
        while let Some(frame) = self.next_frame() {
            frames.push(frame);
        }
        frames
    }

    /// Flushes any remaining buffered data as a final frame.
    ///
    /// Call this at the end of the stream to emit the last picture, which
    /// has no following picture start code to trigger emission.
    pub fn flush(&mut self) -> Vec<Frame> {
        let mut frames = Vec::new();
        if let Some(start) = self.picture_start.take() {
            if self.buf.len() > start {
                let data = self.buf[start..].to_vec();
                self.buf.clear();
                frames.push(Frame {
                    pts: self.last_pts,
                    dts: self.last_dts,
                    data,
                    keyframe: self.current_keyframe,
                });
            }
        } else {
            self.buf.clear();
        }
        frames
    }

    /// Returns the Matroska `CodecPrivate` payload.
    ///
    /// Contains the sequence header and sequence extension, captured from
    /// the first occurrence in the stream. Returns `None` until a sequence
    /// header has been seen.
    #[must_use]
    pub fn codec_private(&self) -> Option<&[u8]> {
        self.codec_private.as_deref()
    }

    /// Attempts to extract the next complete picture from the buffer.
    fn next_frame(&mut self) -> Option<Frame> {
        // Start scanning past the current picture's start code to avoid
        // re-discovering it and emitting an empty frame in an infinite loop.
        let mut scan_from = self.picture_start.map_or(0, |pos| pos + 4);

        loop {
            let sc_pos = find_start_code(&self.buf, scan_from)?;

            // Need at least the start code ID byte after the prefix.
            if sc_pos + 3 >= self.buf.len() {
                return None;
            }

            let code_id = self.buf[sc_pos + 3];

            match code_id {
                SEQUENCE_HEADER_CODE => {
                    self.try_capture_codec_private(sc_pos);
                    scan_from = sc_pos + 4;
                }
                PICTURE_START_CODE => {
                    // Need enough bytes to read picture_coding_type.
                    if self.buf.len() < sc_pos + MIN_PICTURE_HEADER {
                        return None;
                    }

                    let pct = picture_coding_type(&self.buf, sc_pos);
                    let is_keyframe = pct == Some(1);

                    if let Some(prev_start) = self.picture_start {
                        // Emit the previous picture: bytes from prev_start
                        // up to sc_pos.
                        let data = self.buf[prev_start..sc_pos].to_vec();
                        let keyframe = self.current_keyframe;

                        // Remove emitted bytes, adjust state.
                        self.buf.drain(..sc_pos);
                        self.picture_start = Some(0);
                        self.current_keyframe = is_keyframe;

                        return Some(Frame {
                            pts: self.last_pts,
                            dts: self.last_dts,
                            data,
                            keyframe,
                        });
                    }

                    // First picture start code — mark position.
                    // Discard any data before it (sequence headers before
                    // the first picture are captured in codec_private).
                    if sc_pos > 0 {
                        self.buf.drain(..sc_pos);
                    }
                    self.picture_start = Some(0);
                    self.current_keyframe = is_keyframe;
                    scan_from = 4; // continue scanning past this start code
                }
                _ => {
                    scan_from = sc_pos + 4;
                }
            }
        }
    }

    /// Tries to capture the sequence header + extension for `CodecPrivate`.
    ///
    /// Only captures on the first occurrence. Scans forward from `pos`
    /// (the sequence header start code) to find the end of the sequence
    /// extension, then stores everything up to the next non-extension
    /// start code.
    fn try_capture_codec_private(&mut self, pos: usize) {
        if self.codec_private.is_some() {
            return;
        }

        // Scan forward from the sequence header to find where the
        // sequence header block ends. The block includes the sequence
        // header and any immediately following extension start codes.
        let mut end = pos + 4;
        loop {
            match find_start_code(&self.buf, end) {
                Some(next_sc) => {
                    if next_sc + 3 >= self.buf.len() {
                        // Can't read the start code ID — wait for more data.
                        return;
                    }
                    if self.buf[next_sc + 3] == EXTENSION_START_CODE {
                        // Sequence extension — include it.
                        end = next_sc + 4;
                    } else {
                        // Non-extension start code — this terminates the
                        // sequence header block.
                        self.codec_private = Some(self.buf[pos..next_sc].to_vec());
                        return;
                    }
                }
                None => {
                    // Not enough data to find the terminating start code.
                    return;
                }
            }
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

    // ── Helpers ────────────────────────────────────────────────────────

    /// Builds a 4-byte start code: `0x000001` prefix + `id`.
    fn start_code(id: u8) -> Vec<u8> {
        vec![0x00, 0x00, 0x01, id]
    }

    /// Builds a picture header with the given `picture_coding_type`.
    ///
    /// Layout after the 4-byte start code:
    ///   byte 0: `temporal_reference[9:2]`
    ///   byte 1: `temporal_reference[1:0]` | `picture_coding_type[2:0]` | `vbv_delay[15:13]`
    fn picture_header(pct: u8) -> Vec<u8> {
        let mut hdr = start_code(PICTURE_START_CODE);
        hdr.push(0x00); // temporal_reference[9:2] = 0
        hdr.push(pct << 3); // temporal_reference[1:0]=0 | pct | vbv_delay bits
        hdr
    }

    /// Builds a minimal sequence header (start code + 8 bytes of config).
    ///
    /// The config bytes encode:
    ///   - `horizontal_size`: 720
    ///   - `vertical_size`: 480
    ///   - `aspect_ratio`: 3 (16:9)
    ///   - `frame_rate`: 4 (29.97 fps)
    fn sequence_header() -> Vec<u8> {
        let mut hdr = start_code(SEQUENCE_HEADER_CODE);
        // horizontal_size[11:0] = 720 = 0x2D0
        // vertical_size[11:0] = 480 = 0x1E0
        // aspect_ratio_information[3:0] = 3
        // frame_rate_code[3:0] = 4
        hdr.push(0x2D); // horizontal_size[11:4]
        hdr.push(0x01); // horizontal_size[3:0] | vertical_size[11:8]
        hdr.push(0xE0); // vertical_size[7:0]
        hdr.push(0x34); // aspect_ratio(3) | frame_rate(4)
        // bit_rate_value (18 bits) + marker + vbv_buffer_size (10 bits) etc.
        hdr.extend_from_slice(&[0x00; 4]);
        hdr
    }

    /// Builds a sequence extension start code with minimal data.
    fn sequence_extension() -> Vec<u8> {
        let mut ext = start_code(EXTENSION_START_CODE);
        // extension_start_code_identifier (4 bits) = 1 (sequence extension)
        // + profile_and_level, progressive_sequence, etc.
        ext.extend_from_slice(&[0x14, 0x00, 0x00, 0x00]);
        ext
    }

    /// Builds a GOP header start code with minimal data.
    fn gop_header() -> Vec<u8> {
        let mut gop = start_code(0xB8);
        gop.extend_from_slice(&[0x00; 4]); // time_code + closed/broken flags
        gop
    }

    /// Builds a slice start code (0x01–0xAF) with some payload bytes.
    fn slice(id: u8, payload_len: usize) -> Vec<u8> {
        let mut s = start_code(id);
        s.resize(s.len() + payload_len, 0xAA);
        s
    }

    /// Builds an I-frame picture with slice data.
    fn i_picture(slice_bytes: usize) -> Vec<u8> {
        let mut pic = picture_header(1); // I-frame
        pic.extend_from_slice(&slice(0x01, slice_bytes));
        pic
    }

    /// Builds a P-frame picture with slice data.
    fn p_picture(slice_bytes: usize) -> Vec<u8> {
        let mut pic = picture_header(2); // P-frame
        pic.extend_from_slice(&slice(0x01, slice_bytes));
        pic
    }

    /// Builds a B-frame picture with slice data.
    fn b_picture(slice_bytes: usize) -> Vec<u8> {
        let mut pic = picture_header(3); // B-frame
        pic.extend_from_slice(&slice(0x01, slice_bytes));
        pic
    }

    /// Builds a complete stream: seq header + extension + GOP + pictures.
    fn make_stream(pictures: &[Vec<u8>]) -> Vec<u8> {
        let mut stream = sequence_header();
        stream.extend_from_slice(&sequence_extension());
        stream.extend_from_slice(&gop_header());
        for pic in pictures {
            stream.extend_from_slice(pic);
        }
        stream
    }

    // ── Single frame emission ─────────────────────────────────────────

    #[test]
    fn two_pictures_emits_first() {
        let mut framer = Mpeg2Framer::new();
        let pic_i = i_picture(100);
        let pic_p = p_picture(80);
        let stream = make_stream(&[pic_i, pic_p]);

        let frames = framer.feed(Some(1000), None, &stream);

        assert_eq!(frames.len(), 1, "two pictures should emit the first");
        assert_eq!(
            frames[0].data.len(),
            110,
            "i_picture(100) = 6 hdr + 4 sc + 100 payload"
        );
        assert_eq!(frames[0].pts, 1000, "PTS should match input");
        assert!(frames[0].keyframe, "I-frame should be a keyframe");
        assert!(frames[0].dts.is_none(), "no DTS provided");
    }

    #[test]
    fn three_pictures_emits_two() {
        let mut framer = Mpeg2Framer::new();
        let stream = make_stream(&[i_picture(100), p_picture(80), b_picture(60)]);

        let frames = framer.feed(Some(0), None, &stream);

        assert_eq!(frames.len(), 2, "three pictures should emit two");
        assert_eq!(frames[0].data.len(), 110, "i_picture(100) = 110 bytes");
        assert_eq!(frames[1].data.len(), 90, "p_picture(80) = 90 bytes");
        assert!(frames[0].keyframe, "first picture is I-frame");
        assert!(!frames[1].keyframe, "second picture is P-frame");
    }

    #[test]
    fn flush_emits_last_picture() {
        let mut framer = Mpeg2Framer::new();
        let stream = make_stream(&[i_picture(100), p_picture(80)]);

        let frames = framer.feed(Some(0), None, &stream);
        assert_eq!(frames.len(), 1, "feed emits first picture");

        let flushed = framer.flush();
        assert_eq!(flushed.len(), 1, "flush emits the trailing picture");
        assert_eq!(flushed[0].data.len(), 90, "p_picture(80) = 90 bytes");
        assert!(!flushed[0].keyframe, "P-frame is not a keyframe");
    }

    // ── Keyframe detection ────────────────────────────────────────────

    #[test]
    fn i_frame_is_keyframe() {
        let mut framer = Mpeg2Framer::new();
        let stream = make_stream(&[i_picture(50), p_picture(50)]);

        let frames = framer.feed(None, None, &stream);

        assert_eq!(frames.len(), 1, "should emit one frame");
        assert!(frames[0].keyframe, "I-frame (pct=1) is a keyframe");
    }

    #[test]
    fn p_frame_is_not_keyframe() {
        let mut framer = Mpeg2Framer::new();
        let stream = make_stream(&[p_picture(50), i_picture(50)]);

        let frames = framer.feed(None, None, &stream);

        assert_eq!(frames.len(), 1, "should emit one frame");
        assert!(!frames[0].keyframe, "P-frame (pct=2) is not a keyframe");
    }

    #[test]
    fn b_frame_is_not_keyframe() {
        let mut framer = Mpeg2Framer::new();
        let stream = make_stream(&[b_picture(50), i_picture(50)]);

        let frames = framer.feed(None, None, &stream);

        assert_eq!(frames.len(), 1, "should emit one frame");
        assert!(!frames[0].keyframe, "B-frame (pct=3) is not a keyframe");
    }

    // ── CodecPrivate ──────────────────────────────────────────────────

    #[test]
    fn codec_private_none_before_data() {
        let framer = Mpeg2Framer::new();
        assert!(
            framer.codec_private().is_none(),
            "no CodecPrivate before any data"
        );
    }

    #[test]
    fn codec_private_captures_sequence_header_and_extension() {
        let mut framer = Mpeg2Framer::new();
        let stream = make_stream(&[i_picture(50), p_picture(50)]);

        framer.feed(None, None, &stream);

        let cp = framer
            .codec_private()
            .expect("CodecPrivate should be captured after sequence header");

        // sequence_header (12) + sequence_extension (8) = 20 bytes.
        assert_eq!(cp.len(), 20, "CodecPrivate = seq hdr (12) + seq ext (8)");
        // Should start with sequence header start code.
        assert_eq!(
            &cp[..4],
            &[0x00, 0x00, 0x01, SEQUENCE_HEADER_CODE],
            "CodecPrivate should start with sequence header start code"
        );
        // Should contain the extension start code.
        assert!(
            cp.windows(4)
                .any(|w| w == [0x00, 0x00, 0x01, EXTENSION_START_CODE]),
            "CodecPrivate should include the sequence extension"
        );
    }

    #[test]
    fn codec_private_excludes_gop_header() {
        let mut framer = Mpeg2Framer::new();
        let stream = make_stream(&[i_picture(50), p_picture(50)]);

        framer.feed(None, None, &stream);

        let cp = framer
            .codec_private()
            .expect("CodecPrivate should be present");

        assert!(
            !cp.windows(4).any(|w| w == [0x00, 0x00, 0x01, 0xB8]),
            "CodecPrivate should not include the GOP header"
        );
    }

    #[test]
    fn codec_private_captured_only_once() {
        let mut framer = Mpeg2Framer::new();

        // First GOP with one sequence header.
        let stream1 = make_stream(&[i_picture(50), p_picture(50)]);
        framer.feed(None, None, &stream1);
        let cp1 = framer.codec_private().expect("first capture").to_vec();

        // Second GOP — would produce a different CodecPrivate (30 bytes
        // vs 20) if the framer captured again.
        let mut stream2 = sequence_header();
        stream2.extend_from_slice(&[0xFF; 10]); // padding between header and extension
        stream2.extend_from_slice(&sequence_extension());
        stream2.extend_from_slice(&gop_header());
        stream2.extend_from_slice(&i_picture(50));
        stream2.extend_from_slice(&p_picture(50));
        framer.feed(None, None, &stream2);

        let cp2 = framer
            .codec_private()
            .expect("should still have CodecPrivate");

        assert_eq!(
            cp1, cp2,
            "CodecPrivate should not change after first capture"
        );
    }

    // ── Partial frame buffering ───────────────────────────────────────

    #[test]
    fn partial_picture_across_feeds() {
        let mut framer = Mpeg2Framer::new();
        let stream = make_stream(&[i_picture(100), p_picture(80)]);

        // Split mid-stream.
        let mid = stream.len() / 2;
        let frames1 = framer.feed(Some(500), None, &stream[..mid]);
        // Depending on where the split lands, might emit 0 or 1 frames.
        let frames2 = framer.feed(None, None, &stream[mid..]);

        let total = frames1.len() + frames2.len();
        assert_eq!(total, 1, "should emit exactly one complete picture");
    }

    #[test]
    fn byte_at_a_time_feed() {
        let mut framer = Mpeg2Framer::new();
        let stream = make_stream(&[i_picture(20), p_picture(20), b_picture(20)]);

        let mut total_frames = Vec::new();
        for &byte in &stream {
            total_frames.extend(framer.feed(None, None, &[byte]));
        }
        total_frames.extend(framer.flush());

        assert_eq!(total_frames.len(), 3, "should emit all three pictures");
        assert_eq!(total_frames[0].data.len(), 30, "i_picture(20) = 30 bytes");
        assert_eq!(total_frames[1].data.len(), 30, "p_picture(20) = 30 bytes");
        assert_eq!(total_frames[2].data.len(), 30, "b_picture(20) = 30 bytes");
        assert!(total_frames[0].keyframe, "first is I-frame");
        assert!(!total_frames[1].keyframe, "second is P-frame");
        assert!(!total_frames[2].keyframe, "third is B-frame");
    }

    // ── PTS / DTS assignment ──────────────────────────────────────────

    #[test]
    fn pts_carried_forward() {
        let mut framer = Mpeg2Framer::new();
        let stream = make_stream(&[i_picture(50), p_picture(50), b_picture(50)]);

        let frames = framer.feed(Some(9000), None, &stream);
        assert_eq!(frames.len(), 2, "should emit two pictures");
        assert_eq!(frames[0].pts, 9000, "first picture gets PTS");
        assert_eq!(frames[1].pts, 9000, "second picture carries PTS forward");
    }

    #[test]
    fn pts_updates_between_feeds() {
        let mut framer = Mpeg2Framer::new();

        let stream1 = make_stream(&[i_picture(50), p_picture(50)]);
        let f1 = framer.feed(Some(1000), None, &stream1);
        assert_eq!(f1.len(), 1, "first feed emits one frame");
        assert_eq!(f1[0].pts, 1000, "first PTS");

        // Second feed with new PTS and a new picture boundary.
        let pic = i_picture(50);
        let f2 = framer.feed(Some(5000), None, &pic);
        assert_eq!(f2.len(), 1, "second feed emits the pending P-frame");
        assert_eq!(f2[0].pts, 5000, "updated PTS");
    }

    #[test]
    fn dts_passed_through() {
        let mut framer = Mpeg2Framer::new();
        let stream = make_stream(&[i_picture(50), p_picture(50)]);

        let frames = framer.feed(Some(3000), Some(2500), &stream);
        assert_eq!(frames.len(), 1, "should emit one frame");
        assert_eq!(frames[0].dts, Some(2500), "DTS passed through");
    }

    #[test]
    fn pts_defaults_to_zero() {
        let mut framer = Mpeg2Framer::new();
        let stream = make_stream(&[i_picture(50), p_picture(50)]);

        let frames = framer.feed(None, None, &stream);
        assert_eq!(frames[0].pts, 0, "default PTS is 0");
    }

    // ── Flush behavior ────────────────────────────────────────────────

    #[test]
    fn flush_on_empty_buffer() {
        let mut framer = Mpeg2Framer::new();
        let flushed = framer.flush();
        assert!(flushed.is_empty(), "flush on empty produces nothing");
    }

    #[test]
    fn flush_without_picture_start() {
        let mut framer = Mpeg2Framer::new();
        // Feed only a sequence header — no picture start code.
        framer.feed(None, None, &sequence_header());
        let flushed = framer.flush();
        assert!(
            flushed.is_empty(),
            "flush without a picture start emits nothing"
        );
    }

    #[test]
    fn framer_works_after_flush() {
        let mut framer = Mpeg2Framer::new();
        let stream = make_stream(&[i_picture(50)]);

        framer.feed(None, None, &stream);
        framer.flush();

        // Feed a new stream.
        let stream2 = make_stream(&[p_picture(40), i_picture(40)]);
        let frames = framer.feed(None, None, &stream2);
        assert_eq!(frames.len(), 1, "framer should work after flush");
    }

    // ── Edge cases ────────────────────────────────────────────────────

    #[test]
    fn empty_feed() {
        let mut framer = Mpeg2Framer::new();
        let frames = framer.feed(None, None, &[]);
        assert!(frames.is_empty(), "empty input produces no frames");
    }

    #[test]
    fn garbage_before_first_picture() {
        let mut framer = Mpeg2Framer::new();
        let mut data = vec![0xFF; 200]; // garbage
        data.extend_from_slice(&make_stream(&[i_picture(50), p_picture(50)]));

        let frames = framer.feed(None, None, &data);
        assert_eq!(frames.len(), 1, "should find and emit the first picture");
        assert!(frames[0].keyframe, "first picture is I-frame");
    }

    #[test]
    fn sequence_header_between_pictures_does_not_split() {
        let mut framer = Mpeg2Framer::new();
        // GOP structure: seq + ext + gop + I + seq + ext + gop + P + I (terminator)
        let mut stream = sequence_header();
        stream.extend_from_slice(&sequence_extension());
        stream.extend_from_slice(&gop_header());
        stream.extend_from_slice(&i_picture(50));
        // Repeat sequence header (common at GOP boundaries).
        stream.extend_from_slice(&sequence_header());
        stream.extend_from_slice(&sequence_extension());
        stream.extend_from_slice(&gop_header());
        stream.extend_from_slice(&p_picture(50));
        // Terminator so the P-frame emits.
        stream.extend_from_slice(&i_picture(50));

        let frames = framer.feed(None, None, &stream);
        assert_eq!(frames.len(), 2, "should emit two complete pictures");
        // I-frame includes trailing seq+ext+gop: 60 + 12 + 8 + 8 = 88 bytes.
        assert_eq!(
            frames[0].data.len(),
            88,
            "I-picture + mid-stream seq/ext/gop"
        );
        // P-frame is just the picture: 60 bytes.
        assert_eq!(frames[1].data.len(), 60, "p_picture(50) = 60 bytes");
        assert!(frames[0].keyframe, "first is I-frame");
        assert!(!frames[1].keyframe, "second is P-frame");
    }

    #[test]
    fn frame_data_boundaries_correct() {
        let mut framer = Mpeg2Framer::new();
        let stream = make_stream(&[i_picture(100), p_picture(80)]);

        let frames = framer.feed(None, None, &stream);
        assert_eq!(frames.len(), 1, "should emit one frame");

        // The emitted frame should start with a picture start code.
        assert_eq!(
            &frames[0].data[..4],
            &[0x00, 0x00, 0x01, PICTURE_START_CODE],
            "frame data should start with picture start code"
        );

        // The emitted frame's data should be exactly the I-picture bytes.
        assert_eq!(
            frames[0].data.len(),
            110,
            "i_picture(100) = 6 hdr + 4 sc + 100 payload = 110 bytes"
        );
    }

    #[test]
    fn multiple_slices_within_picture() {
        let mut framer = Mpeg2Framer::new();
        let mut pic = picture_header(1);
        // Multiple slice start codes (0x01..0xAF).
        pic.extend_from_slice(&slice(0x01, 50));
        pic.extend_from_slice(&slice(0x02, 50));
        pic.extend_from_slice(&slice(0x03, 50));

        let terminator = p_picture(20);
        let stream = make_stream(&[pic, terminator]);

        let frames = framer.feed(None, None, &stream);
        assert_eq!(frames.len(), 1, "multi-slice picture emits as one frame");
        // 6 hdr + 3 slices × (4 sc + 50 payload) = 6 + 162 = 168 bytes.
        assert_eq!(frames[0].data.len(), 168, "picture header + 3 slices");
        assert!(frames[0].keyframe, "I-frame with multiple slices");
    }
}
