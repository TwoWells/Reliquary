// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! AC-3 (Dolby Digital) sync frame detection and metadata extraction.
//!
//! AC-3 uses a syncword-based format (`0x0B77`) where every sync frame
//! is independently decodable — every frame is a random access point.
//! Frame size is determined from the sample rate code (`fscod`) and
//! frame size code (`frmsizecod`) in the first five bytes of the header.
//!
//! Matroska codec ID: `A_AC3`. `CodecPrivate` is empty — decoders read
//! configuration from each sync frame's BSI (bit stream information)
//! header.
//!
//! Reference: ATSC A/52:2018 §5.4.

use super::Frame;

// ── Constants ──────────────────────────────────────────────────────────

/// Minimum number of header bytes needed for frame metadata parsing.
const MIN_HEADER_BYTES: usize = 7;

/// Maximum AC-3 bit stream identification. Values 11–16 indicate
/// E-AC-3, which uses a different framing scheme.
const MAX_AC3_BSID: u8 = 8;

/// Channel count (excluding LFE) for each `acmod` value.
///
/// Index is the 3-bit `acmod` field from the BSI header.
/// ATSC A/52:2018 Table 5.7.
const ACMOD_CHANNELS: [u8; 8] = [
    2, // 0b000 — Ch1, Ch2 (dual mono)
    1, // 0b001 — C
    2, // 0b010 — L, R
    3, // 0b011 — L, C, R
    3, // 0b100 — L, R, S
    4, // 0b101 — L, C, R, S
    4, // 0b110 — L, R, SL, SR
    5, // 0b111 — L, C, R, SL, SR
];

/// Frame sizes in bytes, indexed by `[frmsizecod][fscod]`.
///
/// Columns: 48 kHz (`fscod` 0), 44.1 kHz (`fscod` 1), 32 kHz (`fscod` 2).
/// Converted from 16-bit word counts in ATSC A/52:2018 Table 5.18.
///
/// For 44.1 kHz, odd `frmsizecod` values are one word (2 bytes) larger
/// than even values at the same bitrate, compensating for the non-integer
/// sample-rate-to-bitrate ratio.
#[rustfmt::skip]
const FRAME_SIZE_BYTES: [[u16; 3]; 38] = [
    // frmsizecod  bitrate    48 kHz  44.1 kHz  32 kHz
    [  128,  138,  192], //  0    32 kbps
    [  128,  140,  192], //  1    32 kbps
    [  160,  174,  240], //  2    40 kbps
    [  160,  176,  240], //  3    40 kbps
    [  192,  208,  288], //  4    48 kbps
    [  192,  210,  288], //  5    48 kbps
    [  224,  242,  336], //  6    56 kbps
    [  224,  244,  336], //  7    56 kbps
    [  256,  278,  384], //  8    64 kbps
    [  256,  280,  384], //  9    64 kbps
    [  320,  348,  480], // 10    80 kbps
    [  320,  350,  480], // 11    80 kbps
    [  384,  416,  576], // 12    96 kbps
    [  384,  418,  576], // 13    96 kbps
    [  448,  486,  672], // 14   112 kbps
    [  448,  488,  672], // 15   112 kbps
    [  512,  556,  768], // 16   128 kbps
    [  512,  558,  768], // 17   128 kbps
    [  640,  696,  960], // 18   160 kbps
    [  640,  698,  960], // 19   160 kbps
    [  768,  834, 1152], // 20   192 kbps
    [  768,  836, 1152], // 21   192 kbps
    [  896,  974, 1344], // 22   224 kbps
    [  896,  976, 1344], // 23   224 kbps
    [ 1024, 1114, 1536], // 24   256 kbps
    [ 1024, 1116, 1536], // 25   256 kbps
    [ 1280, 1392, 1920], // 26   320 kbps
    [ 1280, 1394, 1920], // 27   320 kbps
    [ 1536, 1670, 2304], // 28   384 kbps
    [ 1536, 1672, 2304], // 29   384 kbps
    [ 1792, 1950, 2688], // 30   448 kbps
    [ 1792, 1952, 2688], // 31   448 kbps
    [ 2048, 2228, 3072], // 32   512 kbps
    [ 2048, 2230, 3072], // 33   512 kbps
    [ 2304, 2506, 3456], // 34   576 kbps
    [ 2304, 2508, 3456], // 35   576 kbps
    [ 2560, 2786, 3840], // 36   640 kbps
    [ 2560, 2788, 3840], // 37   640 kbps
];

// ── Header parsing ─────────────────────────────────────────────────────

/// Parsed fields from an AC-3 sync frame header.
struct Ac3Header {
    /// Total frame size in bytes (including syncword).
    frame_size: usize,
    /// Sample rate in Hz.
    sample_rate: u32,
    /// Channel count including LFE.
    channels: u8,
}

/// Attempts to parse an AC-3 header from the start of `data`.
///
/// Returns `None` if the data is too short, the syncword is missing,
/// or any header field is out of range (including E-AC-3 `bsid` > 8).
fn parse_header(data: &[u8]) -> Option<Ac3Header> {
    if data.len() < MIN_HEADER_BYTES {
        return None;
    }

    // Syncword check
    if data[0] != 0x0B || data[1] != 0x77 {
        return None;
    }

    // Byte 4: fscod (2 bits) + frmsizecod (6 bits)
    let fscod = (data[4] >> 6) & 0x03;
    let frmsizecod = data[4] & 0x3F;

    if fscod >= 3 || frmsizecod > 37 {
        return None;
    }

    // Byte 5: bsid (5 bits) + bsmod (3 bits)
    let bsid = (data[5] >> 3) & 0x1F;
    if bsid > MAX_AC3_BSID {
        return None;
    }

    // Byte 6: acmod (3 bits) + conditional fields + lfeon (1 bit)
    let acmod = (data[6] >> 5) & 0x07;

    // Count bits consumed by conditional fields between acmod and lfeon.
    let mut bit_offset: u8 = 3; // after acmod
    if (acmod & 0x01) != 0 && acmod != 0x01 {
        bit_offset += 2; // cmixlev
    }
    if (acmod & 0x04) != 0 {
        bit_offset += 2; // surmixlev
    }
    if acmod == 0x02 {
        bit_offset += 2; // dsurmod
    }
    let lfeon = (data[6] >> (7 - bit_offset)) & 1;

    let sample_rate = match fscod {
        0 => 48_000,
        1 => 44_100,
        2 => 32_000,
        _ => return None,
    };

    let frame_size = usize::from(FRAME_SIZE_BYTES[usize::from(frmsizecod)][usize::from(fscod)]);

    let channels = ACMOD_CHANNELS[usize::from(acmod)] + lfeon;

    Some(Ac3Header {
        frame_size,
        sample_rate,
        channels,
    })
}

/// Finds the byte offset of the first `0x0B77` syncword in `buf`.
fn find_syncword(buf: &[u8]) -> Option<usize> {
    if buf.len() < 2 {
        return None;
    }
    buf.windows(2).position(|w| w[0] == 0x0B && w[1] == 0x77)
}

// ── Framer ─────────────────────────────────────────────────────────────

/// Stream metadata captured from the first parsed header.
struct Ac3Info {
    sample_rate: u32,
    channels: u8,
}

/// AC-3 (Dolby Digital) framer.
///
/// Accumulates elementary stream bytes and emits complete [`Frame`]s as
/// sync frame boundaries are detected. Every AC-3 frame is marked
/// `keyframe = true` (random access point).
///
/// # Timestamps
///
/// The presentation timestamp passed to [`feed`](Self::feed) is carried
/// forward: every emitted frame receives the most recent PTS provided by
/// the demuxer. The framer does not interpolate or synthesize timestamps
/// — timestamp spacing is the muxer's concern.
///
/// # Sync recovery
///
/// If invalid data is encountered (missing syncword, out-of-range header
/// fields, or E-AC-3 `bsid`), the framer skips forward until a valid
/// AC-3 sync frame is found.
#[derive(Default)]
pub struct Ac3Framer {
    /// Accumulation buffer for incomplete frames.
    buf: Vec<u8>,
    /// Metadata from the first successfully parsed header.
    info: Option<Ac3Info>,
    /// Most recent PTS from the demuxer, carried forward to each frame.
    last_pts: u64,
}

impl Ac3Framer {
    /// Creates a new framer with an empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds elementary stream bytes into the framer.
    ///
    /// `pts` is the presentation timestamp (90 kHz clock) from the PES
    /// header, if available. It applies to the first sync frame starting
    /// at or after the beginning of `data`.
    ///
    /// Returns all complete frames detected in the accumulated buffer.
    pub fn feed(&mut self, pts: Option<u64>, data: &[u8]) -> Vec<Frame> {
        if let Some(p) = pts {
            self.last_pts = p;
        }
        self.buf.extend_from_slice(data);

        let mut frames = Vec::new();
        while let Some(frame) = self.next_frame() {
            frames.push(frame);
        }
        frames
    }

    /// Discards any buffered partial frame data.
    ///
    /// AC-3 sync frames are independently decodable, so a trailing
    /// partial frame at the end of a stream carries no useful data.
    /// Returns an empty list.
    pub fn flush(&mut self) -> Vec<Frame> {
        self.buf.clear();
        Vec::new()
    }

    /// Returns the Matroska `CodecPrivate` payload.
    ///
    /// Always empty for AC-3 — decoders read configuration from each
    /// sync frame's BSI header.
    #[must_use]
    pub const fn codec_private(&self) -> &[u8] {
        &[]
    }

    /// Sample rate in Hz, parsed from the first valid frame header.
    ///
    /// Returns `None` until the first frame has been successfully parsed.
    #[must_use]
    pub fn sample_rate(&self) -> Option<u32> {
        self.info.as_ref().map(|i| i.sample_rate)
    }

    /// Channel count (including LFE), parsed from the first valid frame
    /// header.
    ///
    /// Returns `None` until the first frame has been successfully parsed.
    #[must_use]
    pub fn channels(&self) -> Option<u8> {
        self.info.as_ref().map(|i| i.channels)
    }

    /// Attempts to extract the next complete frame from the buffer.
    fn next_frame(&mut self) -> Option<Frame> {
        loop {
            let sync_pos = find_syncword(&self.buf)?;

            // Discard any bytes before the syncword (sync recovery).
            if sync_pos > 0 {
                self.buf.drain(..sync_pos);
            }

            if self.buf.len() < MIN_HEADER_BYTES {
                return None;
            }

            if let Some(header) = parse_header(&self.buf) {
                if self.buf.len() < header.frame_size {
                    return None; // Wait for more data.
                }

                let data = self.buf[..header.frame_size].to_vec();
                self.buf.drain(..header.frame_size);

                // Capture stream metadata from the first valid header.
                if self.info.is_none() {
                    self.info = Some(Ac3Info {
                        sample_rate: header.sample_rate,
                        channels: header.channels,
                    });
                }

                return Some(Frame {
                    pts: self.last_pts,
                    dts: None,
                    data,
                    keyframe: true,
                });
            }

            // Invalid header at this syncword — skip past it.
            self.buf.drain(..2);
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

    /// Builds a minimal AC-3 sync frame with the specified header fields.
    ///
    /// The payload (beyond the header) is filled with zeros.
    fn make_frame(fscod: u8, frmsizecod: u8, acmod: u8, lfeon: bool) -> Vec<u8> {
        let size = usize::from(FRAME_SIZE_BYTES[usize::from(frmsizecod)][usize::from(fscod)]);
        let mut frame = vec![0u8; size];

        // Syncword
        frame[0] = 0x0B;
        frame[1] = 0x77;

        // fscod + frmsizecod
        frame[4] = (fscod << 6) | frmsizecod;

        // bsid=8 (AC-3) + bsmod=0
        frame[5] = 8 << 3;

        // acmod + conditional fields + lfeon
        let mut byte6: u8 = acmod << 5;
        let mut bit_offset: u8 = 3;
        if (acmod & 0x01) != 0 && acmod != 0x01 {
            bit_offset += 2; // cmixlev
        }
        if (acmod & 0x04) != 0 {
            bit_offset += 2; // surmixlev
        }
        if acmod == 0x02 {
            bit_offset += 2; // dsurmod
        }
        if lfeon {
            byte6 |= 1 << (7 - bit_offset);
        }
        frame[6] = byte6;

        frame
    }

    /// Standard 48 kHz / 320 kbps / 5.1 frame (common Blu-ray config).
    fn standard_frame() -> Vec<u8> {
        make_frame(0, 26, 0x07, true) // 48 kHz, 320 kbps, L/C/R/SL/SR + LFE
    }

    // ── Syncword detection ─────────────────────────────────────────────

    #[test]
    fn single_frame() {
        let mut framer = Ac3Framer::new();
        let data = standard_frame();
        let frames = framer.feed(Some(1000), &data);

        assert_eq!(frames.len(), 1, "should emit exactly one frame");
        assert_eq!(frames[0].pts, 1000, "PTS should match input");
        assert!(frames[0].keyframe, "AC-3 frames are always keyframes");
        assert!(frames[0].dts.is_none(), "AC-3 has no decode delay");
        assert_eq!(
            frames[0].data.len(),
            1280,
            "320 kbps at 48 kHz = 1280 bytes"
        );
    }

    #[test]
    fn multiple_frames_single_feed() {
        let mut framer = Ac3Framer::new();
        let frame = standard_frame();
        let mut data = Vec::new();
        data.extend_from_slice(&frame);
        data.extend_from_slice(&frame);
        data.extend_from_slice(&frame);

        let frames = framer.feed(Some(0), &data);

        assert_eq!(frames.len(), 3, "should emit three frames");
        assert_eq!(frames[0].pts, 0, "first frame carries PTS");
        assert_eq!(
            frames[1].pts, 0,
            "second frame carries same PTS (no interpolation)"
        );
        assert_eq!(frames[2].pts, 0, "third frame carries same PTS");
    }

    // ── Frame size parsing ─────────────────────────────────────────────

    #[test]
    fn frame_sizes_48khz() {
        let mut framer = Ac3Framer::new();

        // 32 kbps (frmsizecod 0) → 128 bytes
        let data = make_frame(0, 0, 0x02, false);
        assert_eq!(data.len(), 128, "32 kbps at 48 kHz");
        let frames = framer.feed(None, &data);
        assert_eq!(frames.len(), 1, "should parse 32 kbps frame");

        // 640 kbps (frmsizecod 36) → 2560 bytes
        let data = make_frame(0, 36, 0x07, true);
        assert_eq!(data.len(), 2560, "640 kbps at 48 kHz");
        let frames = framer.feed(None, &data);
        assert_eq!(frames.len(), 1, "should parse 640 kbps frame");
    }

    #[test]
    fn frame_sizes_44_1khz() {
        let mut framer = Ac3Framer::new();

        // 96 kbps even (frmsizecod 12) → 416 bytes
        let data = make_frame(1, 12, 0x02, false);
        assert_eq!(data.len(), 416, "96 kbps even at 44.1 kHz");
        let frames = framer.feed(None, &data);
        assert_eq!(frames.len(), 1, "should parse 44.1 kHz even frame");

        // 96 kbps odd (frmsizecod 13) → 418 bytes
        let data = make_frame(1, 13, 0x02, false);
        assert_eq!(data.len(), 418, "96 kbps odd at 44.1 kHz");
        let frames = framer.feed(None, &data);
        assert_eq!(frames.len(), 1, "should parse 44.1 kHz odd frame");
    }

    #[test]
    fn frame_sizes_32khz() {
        let mut framer = Ac3Framer::new();

        // 192 kbps (frmsizecod 20) → 1152 bytes
        let data = make_frame(2, 20, 0x02, false);
        assert_eq!(data.len(), 1152, "192 kbps at 32 kHz");
        let frames = framer.feed(None, &data);
        assert_eq!(frames.len(), 1, "should parse 32 kHz frame");
    }

    // ── Sync recovery ──────────────────────────────────────────────────

    #[test]
    fn sync_recovery_skips_garbage() {
        let mut framer = Ac3Framer::new();
        let frame = standard_frame();

        let mut data = vec![0xFF; 100]; // garbage
        data.extend_from_slice(&frame);

        let frames = framer.feed(Some(5000), &data);

        assert_eq!(frames.len(), 1, "should recover and emit the valid frame");
        assert_eq!(frames[0].pts, 5000, "PTS should still be assigned");
    }

    #[test]
    fn sync_recovery_skips_false_syncword() {
        let mut framer = Ac3Framer::new();
        let valid_frame = standard_frame();

        // Craft a false syncword with invalid fscod=3
        let mut data = vec![0x0B, 0x77, 0x00, 0x00, 0xC0]; // fscod=3 (reserved)
        data.resize(10, 0x00);
        data.extend_from_slice(&valid_frame);

        let frames = framer.feed(None, &data);

        assert_eq!(
            frames.len(),
            1,
            "should skip false syncword and emit valid frame"
        );
        assert_eq!(
            frames[0].data.len(),
            valid_frame.len(),
            "emitted frame should match valid frame size"
        );
    }

    #[test]
    fn sync_recovery_skips_eac3_bsid() {
        let mut framer = Ac3Framer::new();
        let valid_frame = standard_frame();

        // Craft a syncword with E-AC-3 bsid=16
        let mut data = vec![0x0B, 0x77, 0x00, 0x00, 0x00, 16 << 3];
        data.resize(10, 0x00);
        data.extend_from_slice(&valid_frame);

        let frames = framer.feed(None, &data);

        assert_eq!(frames.len(), 1, "should skip E-AC-3 header");
    }

    // ── Partial frame buffering ────────────────────────────────────────

    #[test]
    fn partial_frame_across_feeds() {
        let mut framer = Ac3Framer::new();
        let frame = standard_frame();
        let mid = frame.len() / 2;

        let frames1 = framer.feed(Some(900), &frame[..mid]);
        assert!(frames1.is_empty(), "partial data should not emit a frame");

        let frames2 = framer.feed(None, &frame[mid..]);
        assert_eq!(frames2.len(), 1, "completing the frame should emit it");
        assert_eq!(frames2[0].pts, 900, "PTS from first feed applies");
    }

    #[test]
    fn partial_header_waits() {
        let mut framer = Ac3Framer::new();
        let frame = standard_frame();

        // Feed just the syncword — not enough for header parsing.
        let frames = framer.feed(None, &frame[..3]);
        assert!(frames.is_empty(), "syncword alone should not emit");

        // Feed the rest.
        let frames = framer.feed(None, &frame[3..]);
        assert_eq!(frames.len(), 1, "full data should emit");
    }

    // ── Sample rate / channel extraction ───────────────────────────────

    #[test]
    fn sample_rate_detection() {
        let mut framer = Ac3Framer::new();
        assert!(
            framer.sample_rate().is_none(),
            "no sample rate before first frame"
        );

        let data = make_frame(0, 26, 0x07, true);
        framer.feed(None, &data);
        assert_eq!(framer.sample_rate(), Some(48_000), "should detect 48 kHz");

        let mut framer = Ac3Framer::new();
        let data = make_frame(1, 12, 0x02, false);
        framer.feed(None, &data);
        assert_eq!(framer.sample_rate(), Some(44_100), "should detect 44.1 kHz");

        let mut framer = Ac3Framer::new();
        let data = make_frame(2, 20, 0x02, false);
        framer.feed(None, &data);
        assert_eq!(framer.sample_rate(), Some(32_000), "should detect 32 kHz");
    }

    #[test]
    fn channel_count_stereo() {
        let mut framer = Ac3Framer::new();
        let data = make_frame(0, 26, 0x02, false); // L, R — no LFE
        framer.feed(None, &data);
        assert_eq!(framer.channels(), Some(2), "stereo without LFE");
    }

    #[test]
    fn channel_count_5_1() {
        let mut framer = Ac3Framer::new();
        let data = make_frame(0, 26, 0x07, true); // L, C, R, SL, SR + LFE
        framer.feed(None, &data);
        assert_eq!(framer.channels(), Some(6), "5.1 surround");
    }

    #[test]
    fn channel_count_mono() {
        let mut framer = Ac3Framer::new();
        let data = make_frame(0, 0, 0x01, false); // C only
        framer.feed(None, &data);
        assert_eq!(framer.channels(), Some(1), "mono");
    }

    #[test]
    fn channel_count_all_acmod_values() {
        let expected: [(u8, bool, u8); 8] = [
            (0x00, false, 2), // dual mono
            (0x01, false, 1), // C
            (0x02, false, 2), // L, R
            (0x03, false, 3), // L, C, R
            (0x04, false, 3), // L, R, S
            (0x05, false, 4), // L, C, R, S
            (0x06, false, 4), // L, R, SL, SR
            (0x07, false, 5), // L, C, R, SL, SR
        ];

        for (acmod, lfeon, want) in expected {
            let mut framer = Ac3Framer::new();
            let data = make_frame(0, 26, acmod, lfeon);
            framer.feed(None, &data);
            assert_eq!(
                framer.channels(),
                Some(want),
                "acmod={acmod:#05b} lfeon={lfeon} should give {want} channels"
            );
        }
    }

    #[test]
    fn channel_count_with_lfe_all_acmod() {
        let expected: [(u8, u8); 8] = [
            (0x00, 3), // dual mono + LFE
            (0x01, 2), // C + LFE
            (0x02, 3), // L, R + LFE
            (0x03, 4), // L, C, R + LFE
            (0x04, 4), // L, R, S + LFE
            (0x05, 5), // L, C, R, S + LFE
            (0x06, 5), // L, R, SL, SR + LFE
            (0x07, 6), // L, C, R, SL, SR + LFE
        ];

        for (acmod, want) in expected {
            let mut framer = Ac3Framer::new();
            let data = make_frame(0, 26, acmod, true);
            framer.feed(None, &data);
            assert_eq!(
                framer.channels(),
                Some(want),
                "acmod={acmod:#05b} with LFE should give {want} channels"
            );
        }
    }

    // ── PTS assignment ─────────────────────────────────────────────────

    #[test]
    fn pts_from_separate_feeds() {
        let mut framer = Ac3Framer::new();
        let frame = standard_frame();

        let f1 = framer.feed(Some(1000), &frame);
        assert_eq!(f1[0].pts, 1000, "first frame PTS");

        let f2 = framer.feed(Some(3880), &frame);
        assert_eq!(f2[0].pts, 3880, "second frame PTS from feed");
    }

    #[test]
    fn pts_carried_forward_without_explicit_pts() {
        let mut framer = Ac3Framer::new();
        let frame = standard_frame();

        let f1 = framer.feed(Some(5000), &frame);
        assert_eq!(f1[0].pts, 5000, "first frame gets explicit PTS");

        // Feed without PTS — carries forward the last seen value.
        let f2 = framer.feed(None, &frame);
        assert_eq!(f2[0].pts, 5000, "second frame carries forward last PTS");
    }

    #[test]
    fn pts_defaults_to_zero_when_never_provided() {
        let mut framer = Ac3Framer::new();
        let frame = standard_frame();

        let frames = framer.feed(None, &frame);
        assert_eq!(frames[0].pts, 0, "default PTS is 0");
    }

    // ── CodecPrivate ───────────────────────────────────────────────────

    #[test]
    fn codec_private_is_empty() {
        let framer = Ac3Framer::new();
        assert!(
            framer.codec_private().is_empty(),
            "AC-3 CodecPrivate is always empty"
        );
    }

    // ── Flush ──────────────────────────────────────────────────────────

    #[test]
    fn flush_discards_partial() {
        let mut framer = Ac3Framer::new();
        let frame = standard_frame();

        // Feed half a frame.
        framer.feed(Some(0), &frame[..frame.len() / 2]);

        let flushed = framer.flush();
        assert!(flushed.is_empty(), "partial frame is discarded on flush");
        assert_eq!(
            framer.feed(None, &frame).len(),
            1,
            "framer should work normally after flush"
        );
    }

    // ── Edge cases ─────────────────────────────────────────────────────

    #[test]
    fn empty_feed() {
        let mut framer = Ac3Framer::new();
        let frames = framer.feed(None, &[]);
        assert!(frames.is_empty(), "empty input produces no frames");
    }

    #[test]
    fn syncword_in_payload_not_false_positive() {
        let mut framer = Ac3Framer::new();
        let mut frame = standard_frame();

        // Embed a false syncword deep in the payload (past the header).
        // The framer should not split on it because the outer frame's
        // size governs consumption.
        frame[100] = 0x0B;
        frame[101] = 0x77;

        let frames = framer.feed(None, &frame);
        assert_eq!(
            frames.len(),
            1,
            "embedded syncword should not cause a split"
        );
        assert_eq!(
            frames[0].data.len(),
            frame.len(),
            "frame size should match the header, not the embedded syncword"
        );
    }

    // ── Frame size table verification ──────────────────────────────────

    /// Nominal bitrates in kbps, indexed by `frmsizecod / 2`.
    /// ATSC A/52:2018 Table 5.18.
    const BITRATES: [u32; 19] = [
        32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640,
    ];

    #[test]
    fn frame_size_table_matches_spec_formula() {
        for (i, &bitrate) in BITRATES.iter().enumerate() {
            let even = i * 2;
            let odd = even + 1;

            // 48 kHz: words = 2 × bitrate (exact, both even and odd)
            let bytes_48 = u16::try_from(4 * bitrate).expect("48 kHz overflow");
            assert_eq!(
                FRAME_SIZE_BYTES[even][0], bytes_48,
                "48 kHz even frmsizecod={even} bitrate={bitrate}"
            );
            assert_eq!(
                FRAME_SIZE_BYTES[odd][0], bytes_48,
                "48 kHz odd frmsizecod={odd} bitrate={bitrate}"
            );

            // 44.1 kHz: words = bitrate × 320 / 147
            //   even: floor, odd: ceil
            let even_words = (bitrate * 320) / 147;
            let odd_words = (bitrate * 320).div_ceil(147);
            let bytes_44_even = u16::try_from(even_words * 2).expect("44.1 kHz even overflow");
            let bytes_44_odd = u16::try_from(odd_words * 2).expect("44.1 kHz odd overflow");
            assert_eq!(
                FRAME_SIZE_BYTES[even][1], bytes_44_even,
                "44.1 kHz even frmsizecod={even} bitrate={bitrate}"
            );
            assert_eq!(
                FRAME_SIZE_BYTES[odd][1], bytes_44_odd,
                "44.1 kHz odd frmsizecod={odd} bitrate={bitrate}"
            );

            // 32 kHz: words = 3 × bitrate (exact, both even and odd)
            let bytes_32 = u16::try_from(6 * bitrate).expect("32 kHz overflow");
            assert_eq!(
                FRAME_SIZE_BYTES[even][2], bytes_32,
                "32 kHz even frmsizecod={even} bitrate={bitrate}"
            );
            assert_eq!(
                FRAME_SIZE_BYTES[odd][2], bytes_32,
                "32 kHz odd frmsizecod={odd} bitrate={bitrate}"
            );
        }
    }

    #[test]
    fn all_114_fscod_frmsizecod_combinations_parse() {
        for fscod in 0..3_u8 {
            for frmsizecod in 0..38_u8 {
                let mut framer = Ac3Framer::new();
                let data = make_frame(fscod, frmsizecod, 0x02, false);
                let frames = framer.feed(None, &data);
                assert_eq!(
                    frames.len(),
                    1,
                    "fscod={fscod} frmsizecod={frmsizecod} should produce one frame"
                );
                assert_eq!(
                    frames[0].data.len(),
                    data.len(),
                    "emitted size should match for fscod={fscod} frmsizecod={frmsizecod}"
                );
            }
        }
    }

    // ── Header rejection boundaries ────────────────────────────────────

    #[test]
    fn bsid_0_through_8_accepted() {
        for bsid in 0..=MAX_AC3_BSID {
            let mut framer = Ac3Framer::new();
            let mut data = standard_frame();
            data[5] = bsid << 3; // bsid occupies upper 5 bits
            let frames = framer.feed(None, &data);
            assert_eq!(frames.len(), 1, "bsid={bsid} should be accepted");
        }
    }

    #[test]
    fn bsid_9_and_above_rejected() {
        for bsid in (MAX_AC3_BSID + 1)..=31 {
            let mut framer = Ac3Framer::new();
            let mut data = standard_frame();
            data[5] = bsid << 3;
            let frames = framer.feed(None, &data);
            assert!(frames.is_empty(), "bsid={bsid} should be rejected");
        }
    }

    #[test]
    fn frmsizecod_37_accepted() {
        let mut framer = Ac3Framer::new();
        let data = make_frame(0, 37, 0x02, false);
        let frames = framer.feed(None, &data);
        assert_eq!(frames.len(), 1, "frmsizecod=37 is the maximum valid value");
    }

    #[test]
    fn frmsizecod_38_and_above_rejected() {
        for frmsizecod in 38..=63_u8 {
            let mut framer = Ac3Framer::new();
            // Build raw bytes — can't use make_frame for invalid frmsizecod.
            let mut data = vec![0x0B, 0x77, 0x00, 0x00, frmsizecod, 8 << 3, 0x40];
            data.resize(128, 0x00);
            let frames = framer.feed(None, &data);
            assert!(
                frames.is_empty(),
                "frmsizecod={frmsizecod} should be rejected"
            );
        }
    }

    #[test]
    fn fscod_3_rejected() {
        let mut framer = Ac3Framer::new();
        let mut data = vec![0x0B, 0x77, 0x00, 0x00, 0xC0, 8 << 3, 0x40];
        data.resize(128, 0x00);
        let frames = framer.feed(None, &data);
        assert!(frames.is_empty(), "fscod=3 (reserved) should be rejected");
    }

    // ── PTS carry-forward ───────────────────────────────────────────────

    #[test]
    fn pts_updates_on_new_explicit_value() {
        let mut framer = Ac3Framer::new();
        let frame = standard_frame();

        let f1 = framer.feed(Some(1000), &frame);
        assert_eq!(f1[0].pts, 1000, "first PTS");

        // No PTS — carries forward.
        let f2 = framer.feed(None, &frame);
        assert_eq!(f2[0].pts, 1000, "carried forward");

        // New explicit PTS replaces the old one.
        let f3 = framer.feed(Some(9000), &frame);
        assert_eq!(f3[0].pts, 9000, "updated PTS");

        // Carries forward again from the new value.
        let f4 = framer.feed(None, &frame);
        assert_eq!(f4[0].pts, 9000, "carries new value");
    }
}
