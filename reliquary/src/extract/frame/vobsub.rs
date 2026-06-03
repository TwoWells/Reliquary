// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! `VobSub` (DVD subpicture) passthrough framing.
//!
//! `VobSub` is the only subtitle format on DVD: bitmap "subpictures" (SPU)
//! carried in private stream 1 with sub-stream IDs `0x20`–`0x3F`. The PS
//! demuxer strips the 1-byte sub-stream ID, so this framer receives raw SPU
//! packets beginning with the 2-byte SPDSZ length field.
//!
//! Unlike audio/video (codec-level sync words) or PGS (an explicit end
//! marker), `VobSub` uses **length-prefixed** framing: the SPDSZ field gives
//! the total packet size, and a single SPU may span several PES packets.
//! The framer accumulates payload until SPDSZ bytes are collected, then
//! emits the whole SPU as one [`Frame`]. Each SPU is independently
//! decodable, so every frame is a keyframe.
//!
//! The on-screen duration is extracted from the Display Control Sequence
//! (DCSQ) chain — the delay between the start-display and stop-display
//! commands — and reported on the [`Frame`] in 90 kHz ticks. The Matroska
//! muxer writes this as a `BlockGroup` with `BlockDuration`.
//!
//! Matroska codec ID: `S_VOBSUB`. `CodecPrivate` is an IDX-format header
//! carrying the video frame size and the 16-entry RGB palette converted
//! from the IFO's PGC color lookup table (CLUT).
//!
//! Reference: `reference/VOBSUB.md` (subpicture packet format, DCSQ
//! commands, BT.601 palette conversion, `S_VOBSUB` `CodecPrivate`).

use super::Frame;

// ── Constants ──────────────────────────────────────────────────────────

/// Size of the SPU packet header: SPDSZ (2 bytes) + `SP_DCSQTA` (2 bytes).
const SPU_HEADER_SIZE: usize = 4;

/// Size of a DCSQ header: `SP_DCSQ_STM` delay (2) + `SP_NXT_DCSQ` offset (2).
const DCSQ_HEADER_SIZE: usize = 4;

/// One DCSQ delay unit, in 90 kHz clock ticks (1024 / 90000 s).
const DELAY_UNIT_TICKS: u64 = 1024;

/// Upper bound on DCSQs walked per packet — guards against cyclic offsets.
const MAX_DCSQ: usize = 256;

// DCSQ command opcodes.
const CMD_FSTA_DSP: u8 = 0x00; // forced start display
const CMD_STA_DSP: u8 = 0x01; // start display
const CMD_STP_DSP: u8 = 0x02; // stop display
const CMD_SET_COLOR: u8 = 0x03; // 2 params
const CMD_SET_CONTR: u8 = 0x04; // 2 params
const CMD_SET_DAREA: u8 = 0x05; // 6 params
const CMD_SET_DSPXA: u8 = 0x06; // 4 params
const CMD_CHG_COLCON: u8 = 0x07; // variable (self-describing length)
const CMD_END: u8 = 0xFF; // end of command sequence

// ── Palette ────────────────────────────────────────────────────────────

/// A single YCbCr entry from a PGC color lookup table (CLUT).
///
/// DVD CLUT entries are stored in the IFO as four bytes — a reserved byte
/// followed by Y, Cr, Cb. This carries the three meaningful components.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClutEntry {
    /// Luminance (0–255).
    pub y: u8,
    /// Red chrominance (0–255).
    pub cr: u8,
    /// Blue chrominance (0–255).
    pub cb: u8,
}

impl ClutEntry {
    /// Creates a CLUT entry from its luminance and chrominance components.
    #[must_use]
    pub const fn new(y: u8, cr: u8, cb: u8) -> Self {
        Self { y, cr, cb }
    }

    /// Converts the entry to 8-bit RGB using the BT.601 matrix.
    fn to_rgb(self) -> (u8, u8, u8) {
        let y = f64::from(self.y);
        let cr = f64::from(self.cr) - 128.0;
        let cb = f64::from(self.cb) - 128.0;

        let r = cr.mul_add(1.402, y);
        let g = cr.mul_add(-0.714_136, cb.mul_add(-0.344_136, y));
        let b = cb.mul_add(1.772, y);

        (clamp_u8(r), clamp_u8(g), clamp_u8(b))
    }
}

/// Rounds to the nearest integer and clamps into the `u8` range.
const fn clamp_u8(v: f64) -> u8 {
    let clamped = v.round().clamp(0.0, 255.0);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "value is rounded and clamped to 0.0..=255.0 before the cast"
    )]
    {
        clamped as u8
    }
}

/// Builds the `S_VOBSUB` `CodecPrivate` IDX header from frame size and palette.
///
/// Emits the two lines the Matroska spec marks as required for `VobSub`:
/// `size:` (video frame dimensions) and `palette:` (16 RGB entries). Player
/// defaults cover the optional IDX fields (org, scale, alpha, …).
fn build_codec_private(width: u16, height: u16, palette: &[ClutEntry; 16]) -> Vec<u8> {
    use std::fmt::Write as _;

    let mut idx = format!("size: {width}x{height}\npalette: ");
    for (i, entry) in palette.iter().enumerate() {
        let (r, g, b) = entry.to_rgb();
        if i > 0 {
            idx.push_str(", ");
        }
        // `write!` to a String is infallible.
        let _ = write!(idx, "{r:02x}{g:02x}{b:02x}");
    }
    idx.into_bytes()
}

// ── Duration extraction ──────────────────────────────────────────────────

/// Extracts the on-screen duration (in 90 kHz ticks) from an SPU packet.
///
/// Walks the Display Control Sequence (DCSQ) chain, recording the delay of
/// the start-display command (`STA_DSP` / `FSTA_DSP`) and the stop-display
/// command (`STP_DSP`). The duration is the difference between the two delays,
/// scaled by [`DELAY_UNIT_TICKS`].
///
/// Returns `None` when the packet is malformed or carries no `STP_DSP` — such
/// a subtitle has no explicit end time, and the muxer falls back to a
/// duration-less block.
fn spu_duration(packet: &[u8]) -> Option<u64> {
    if packet.len() < SPU_HEADER_SIZE {
        return None;
    }

    // The DCSQ table begins at SP_DCSQTA (offset from packet start).
    let dcsqta = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    if dcsqta < SPU_HEADER_SIZE || dcsqta >= packet.len() {
        return None;
    }

    let mut start_delay: Option<u16> = None;
    let mut stop_delay: Option<u16> = None;

    let mut off = dcsqta;
    for _ in 0..MAX_DCSQ {
        if off + DCSQ_HEADER_SIZE > packet.len() {
            break;
        }

        let delay = u16::from_be_bytes([packet[off], packet[off + 1]]);
        let next = usize::from(u16::from_be_bytes([packet[off + 2], packet[off + 3]]));

        scan_commands(
            &packet[off + DCSQ_HEADER_SIZE..],
            delay,
            &mut start_delay,
            &mut stop_delay,
        );

        // The last DCSQ self-references; anything non-advancing or out of
        // bounds also terminates the chain.
        if next <= off || next + DCSQ_HEADER_SIZE > packet.len() {
            break;
        }
        off = next;
    }

    let stop = stop_delay?;
    let start = start_delay.unwrap_or(0);
    // A stop earlier than the start is malformed ordering — report no
    // duration rather than a zero-length block.
    let units = u64::from(stop.checked_sub(start)?);
    Some(units * DELAY_UNIT_TICKS)
}

/// Scans one DCSQ's command list, noting start/stop display delays.
///
/// `cmds` is the byte slice following the DCSQ header. Commands are
/// single-byte opcodes with fixed-length parameters, except `CHG_COLCON`
/// (`0x07`) whose length is self-describing. Scanning stops at `CMD_END`, an
/// unknown opcode, or the end of the slice.
fn scan_commands(
    cmds: &[u8],
    delay: u16,
    start_delay: &mut Option<u16>,
    stop_delay: &mut Option<u16>,
) {
    let mut pos = 0;
    while pos < cmds.len() {
        // CMD_END (0xFF) terminates the command sequence.
        if cmds[pos] == CMD_END {
            break;
        }
        let advance = match cmds[pos] {
            CMD_FSTA_DSP | CMD_STA_DSP => {
                if start_delay.is_none() {
                    *start_delay = Some(delay);
                }
                1
            }
            CMD_STP_DSP => {
                if stop_delay.is_none() {
                    *stop_delay = Some(delay);
                }
                1
            }
            CMD_SET_COLOR | CMD_SET_CONTR => 1 + 2,
            CMD_SET_DSPXA => 1 + 4,
            CMD_SET_DAREA => 1 + 6,
            CMD_CHG_COLCON => {
                // Self-describing: the u16 after the opcode counts the bytes
                // following the opcode (including the size field itself), so
                // the whole command spans 1 + that value.
                if pos + 3 > cmds.len() {
                    break;
                }
                1 + usize::from(u16::from_be_bytes([cmds[pos + 1], cmds[pos + 2]]))
            }
            // An unknown opcode has an unknown length — stop scanning.
            _ => break,
        };
        pos += advance;
    }
}

// ── Framer ─────────────────────────────────────────────────────────────

/// `VobSub` subtitle framer.
///
/// Accumulates SPU packet bytes and emits one [`Frame`] per complete
/// subpicture, sized by the SPU's SPDSZ length field. Every frame is a
/// keyframe and carries the on-screen duration parsed from its DCSQ chain
/// (when present).
///
/// # Timestamps
///
/// The presentation timestamp passed to [`feed`](Self::feed) is captured
/// when a new SPU starts accumulating and assigned to that SPU when it
/// completes. Continuation fragments (which may omit the PTS) do not
/// overwrite it. The framer does not interpolate or synthesize timestamps.
pub struct VobSubFramer {
    /// Accumulation buffer for the in-progress SPU packet.
    buf: Vec<u8>,
    /// PTS captured at the start of the in-progress SPU.
    pending_pts: Option<u64>,
    /// Most recent assigned PTS, used as a fallback when none was captured.
    last_pts: u64,
    /// Precomputed `S_VOBSUB` `CodecPrivate` (IDX header).
    codec_private: Vec<u8>,
}

impl VobSubFramer {
    /// Creates a new framer for a subtitle stream.
    ///
    /// `width` and `height` are the video frame dimensions (e.g. 720×480
    /// NTSC, 720×576 PAL) from the IFO video attributes. `palette` is the
    /// PGC's 16-entry YCbCr color lookup table; it is converted to RGB for
    /// the IDX `palette:` line.
    #[must_use]
    pub fn new(width: u16, height: u16, palette: [ClutEntry; 16]) -> Self {
        Self {
            buf: Vec::new(),
            pending_pts: None,
            last_pts: 0,
            codec_private: build_codec_private(width, height, &palette),
        }
    }

    /// Feeds SPU packet bytes into the framer.
    ///
    /// `pts` is the presentation timestamp (90 kHz clock) from the PES
    /// header, if available. It applies to the SPU currently being
    /// accumulated — captured on the first fragment, retained across
    /// continuation fragments.
    ///
    /// Returns all complete subpictures detected in the accumulated buffer.
    pub fn feed(&mut self, pts: Option<u64>, data: &[u8]) -> Vec<Frame> {
        // Capture the PTS at the start of an SPU; later fragments keep it.
        self.pending_pts = self.pending_pts.or(pts);
        self.buf.extend_from_slice(data);

        let mut frames = Vec::new();
        while let Some(frame) = self.next_packet() {
            frames.push(frame);
        }
        frames
    }

    /// Discards any buffered partial SPU packet.
    ///
    /// A subpicture truncated at end-of-stream cannot be decoded. Returns an
    /// empty list.
    pub fn flush(&mut self) -> Vec<Frame> {
        self.buf.clear();
        self.pending_pts = None;
        Vec::new()
    }

    /// Returns the Matroska `CodecPrivate` payload (IDX header).
    #[must_use]
    pub fn codec_private(&self) -> &[u8] {
        &self.codec_private
    }

    /// Attempts to extract the next complete SPU packet from the buffer.
    fn next_packet(&mut self) -> Option<Frame> {
        // Need the SPDSZ field to know the packet size.
        if self.buf.len() < 2 {
            return None;
        }

        let spdsz = usize::from(u16::from_be_bytes([self.buf[0], self.buf[1]]));

        // A valid SPU must at least hold its header. Anything smaller is
        // corrupt; with no sync word to resync to, drop the buffer.
        if spdsz < SPU_HEADER_SIZE {
            self.buf.clear();
            self.pending_pts = None;
            return None;
        }

        // Wait for the complete packet.
        if self.buf.len() < spdsz {
            return None;
        }

        let data = self.buf[..spdsz].to_vec();
        self.buf.drain(..spdsz);

        let duration = spu_duration(&data);
        let pts = self.pending_pts.take().unwrap_or(self.last_pts);
        self.last_pts = pts;

        Some(Frame {
            pts,
            dts: None,
            duration,
            data,
            keyframe: true,
        })
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

    // ── Builders ──────────────────────────────────────────────────────

    /// A neutral 16-entry palette (mid-gray) for framing tests.
    fn gray_palette() -> [ClutEntry; 16] {
        [ClutEntry::new(128, 128, 128); 16]
    }

    /// Builds a DCSQ: delay, next-offset, then the command bytes + `CMD_END`.
    fn dcsq(delay: u16, next_offset: u16, commands: &[u8]) -> Vec<u8> {
        let mut seq = Vec::new();
        seq.extend_from_slice(&delay.to_be_bytes());
        seq.extend_from_slice(&next_offset.to_be_bytes());
        seq.extend_from_slice(commands);
        seq.push(CMD_END);
        seq
    }

    /// Builds a complete SPU packet from RLE pixel bytes and a list of
    /// DCSQ command sequences. Each DCSQ's next-offset is wired to point at
    /// the following DCSQ, with the last one self-referencing.
    ///
    /// Returns the packet with a correct SPDSZ and `SP_DCSQTA`.
    fn spu_packet(rle: &[u8], dcsqs: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let dcsqta = SPU_HEADER_SIZE + rle.len();

        // First pass: compute each DCSQ's absolute offset.
        let mut offsets = Vec::with_capacity(dcsqs.len());
        let mut cursor = dcsqta;
        for (_, cmds) in dcsqs {
            offsets.push(cursor);
            cursor += DCSQ_HEADER_SIZE + cmds.len() + 1; // +1 for CMD_END
        }
        let total = cursor;

        let mut packet = Vec::with_capacity(total);
        packet.extend_from_slice(&u16::try_from(total).expect("packet fits u16").to_be_bytes());
        packet.extend_from_slice(
            &u16::try_from(dcsqta)
                .expect("dcsqta fits u16")
                .to_be_bytes(),
        );
        packet.extend_from_slice(rle);

        for (i, (delay, cmds)) in dcsqs.iter().enumerate() {
            let next = if i + 1 < offsets.len() {
                offsets[i + 1]
            } else {
                offsets[i] // last DCSQ self-references
            };
            packet.extend_from_slice(&dcsq(
                *delay,
                u16::try_from(next).expect("next offset fits u16"),
                cmds,
            ));
        }

        assert_eq!(packet.len(), total, "constructed packet matches its SPDSZ");
        packet
    }

    /// A typical two-DCSQ subtitle: start at delay 0, stop at `stop_delay`.
    fn typical_spu(rle_len: usize, stop_delay: u16) -> Vec<u8> {
        spu_packet(
            &vec![0xAA; rle_len],
            &[
                (
                    0,
                    vec![
                        CMD_SET_COLOR,
                        0x10,
                        0x32,
                        CMD_SET_CONTR,
                        0xFF,
                        0xF0,
                        CMD_STA_DSP,
                    ],
                ),
                (stop_delay, vec![CMD_STP_DSP]),
            ],
        )
    }

    // ── Basic framing ─────────────────────────────────────────────────

    #[test]
    fn single_packet_emits() {
        let mut framer = VobSubFramer::new(720, 480, gray_palette());
        let spu = typical_spu(40, 90);
        let frames = framer.feed(Some(1000), &spu);

        assert_eq!(frames.len(), 1, "should emit exactly one SPU");
        assert_eq!(frames[0].pts, 1000, "PTS should match input");
        assert!(frames[0].keyframe, "VobSub frames are always keyframes");
        assert!(frames[0].dts.is_none(), "VobSub has no decode timestamp");
        assert_eq!(
            frames[0].data.len(),
            spu.len(),
            "frame data should contain the entire SPU packet"
        );
        assert_eq!(frames[0].data, spu, "frame data is the raw SPU packet");
    }

    #[test]
    fn multiple_packets_single_feed() {
        let mut framer = VobSubFramer::new(720, 480, gray_palette());
        let spu1 = typical_spu(30, 50);
        let spu2 = typical_spu(60, 80);

        let mut data = spu1.clone();
        data.extend_from_slice(&spu2);

        let frames = framer.feed(Some(0), &data);
        assert_eq!(frames.len(), 2, "should emit two SPUs");
        assert_eq!(frames[0].data.len(), spu1.len(), "first SPU size");
        assert_eq!(frames[1].data.len(), spu2.len(), "second SPU size");
    }

    // ── Spanning (length-prefixed accumulation) ───────────────────────

    #[test]
    fn packet_spanning_multiple_feeds() {
        let mut framer = VobSubFramer::new(720, 480, gray_palette());
        let spu = typical_spu(200, 120);
        let mid = spu.len() / 2;

        let f1 = framer.feed(Some(2000), &spu[..mid]);
        assert!(f1.is_empty(), "partial SPU should not emit");

        let f2 = framer.feed(None, &spu[mid..]);
        assert_eq!(f2.len(), 1, "completing the SPU should emit");
        assert_eq!(f2[0].pts, 2000, "PTS from first fragment applies");
        assert_eq!(f2[0].data.len(), spu.len(), "frame contains entire SPU");
    }

    #[test]
    fn continuation_fragment_pts_does_not_override() {
        let mut framer = VobSubFramer::new(720, 480, gray_palette());
        let spu = typical_spu(120, 100);
        let mid = spu.len() / 2;

        framer.feed(Some(7000), &spu[..mid]);
        // A continuation fragment that (erroneously) carries a PTS must not
        // replace the PTS captured at the start of the SPU.
        let frames = framer.feed(Some(9999), &spu[mid..]);
        assert_eq!(frames.len(), 1, "SPU completes");
        assert_eq!(frames[0].pts, 7000, "first fragment's PTS is retained");
    }

    #[test]
    fn byte_at_a_time_feed() {
        let mut framer = VobSubFramer::new(720, 480, gray_palette());
        let spu = typical_spu(24, 60);

        let mut frames = Vec::new();
        for (i, &byte) in spu.iter().enumerate() {
            let pts = if i == 0 { Some(1234) } else { None };
            frames.extend(framer.feed(pts, &[byte]));
        }

        assert_eq!(frames.len(), 1, "should emit one SPU");
        assert_eq!(frames[0].data.len(), spu.len(), "entire SPU assembled");
        assert_eq!(frames[0].pts, 1234, "PTS captured from first byte's feed");
    }

    #[test]
    fn one_byte_feed_waits_for_length() {
        let mut framer = VobSubFramer::new(720, 480, gray_palette());
        // A single byte cannot reveal SPDSZ.
        let frames = framer.feed(Some(0), &[0x00]);
        assert!(frames.is_empty(), "one byte is not enough to frame");
    }

    // ── Duration extraction ───────────────────────────────────────────

    #[test]
    fn duration_from_dcsq_chain() {
        let mut framer = VobSubFramer::new(720, 480, gray_palette());
        // stop at delay 90 units → 90 × 1024 = 92160 ticks (≈ 1.024 s).
        let spu = typical_spu(40, 90);
        let frames = framer.feed(Some(0), &spu);

        assert_eq!(frames.len(), 1, "one SPU emitted");
        assert_eq!(
            frames[0].duration,
            Some(90 * DELAY_UNIT_TICKS),
            "duration is stop-delay units scaled to 90 kHz ticks"
        );
    }

    #[test]
    fn duration_subtracts_start_delay() {
        let mut framer = VobSubFramer::new(720, 480, gray_palette());
        // Start at delay 10, stop at delay 70 → 60 units.
        let spu = spu_packet(
            &[0xAA; 20],
            &[(10, vec![CMD_STA_DSP]), (70, vec![CMD_STP_DSP])],
        );
        let frames = framer.feed(Some(0), &spu);
        assert_eq!(
            frames[0].duration,
            Some(60 * DELAY_UNIT_TICKS),
            "duration is (stop - start) delay units"
        );
    }

    #[test]
    fn stop_before_start_yields_no_duration() {
        let mut framer = VobSubFramer::new(720, 480, gray_palette());
        // Malformed ordering: stop delay (20) precedes start delay (90).
        let spu = spu_packet(
            &[0xAA; 20],
            &[(90, vec![CMD_STA_DSP]), (20, vec![CMD_STP_DSP])],
        );
        let frames = framer.feed(Some(0), &spu);
        assert_eq!(frames.len(), 1, "malformed SPU still emits as a block");
        assert!(
            frames[0].duration.is_none(),
            "a stop earlier than the start yields no duration, not zero"
        );
    }

    #[test]
    fn no_stop_command_yields_no_duration() {
        let mut framer = VobSubFramer::new(720, 480, gray_palette());
        // Only a start command — no STP_DSP, so no explicit end time.
        let spu = spu_packet(&[0xAA; 16], &[(0, vec![CMD_STA_DSP])]);
        let frames = framer.feed(Some(0), &spu);
        assert_eq!(frames.len(), 1, "SPU still emits without a stop command");
        assert!(
            frames[0].duration.is_none(),
            "missing STP_DSP yields no duration"
        );
    }

    #[test]
    fn forced_start_counts_as_start() {
        let mut framer = VobSubFramer::new(720, 480, gray_palette());
        // FSTA_DSP (forced) at delay 5, stop at 45 → 40 units.
        let spu = spu_packet(
            &[0xAA; 12],
            &[(5, vec![CMD_FSTA_DSP]), (45, vec![CMD_STP_DSP])],
        );
        let frames = framer.feed(Some(0), &spu);
        assert_eq!(
            frames[0].duration,
            Some(40 * DELAY_UNIT_TICKS),
            "FSTA_DSP is treated as a start-display marker"
        );
    }

    #[test]
    fn variable_length_colcon_is_skipped() {
        let mut framer = VobSubFramer::new(720, 480, gray_palette());
        // A CHG_COLCON before STP_DSP must be skipped via its length field
        // so the stop command is still found. Length value counts the bytes
        // after the opcode including the 2-byte size field: here 2 + 3 = 5.
        let colcon = vec![CMD_CHG_COLCON, 0x00, 0x05, 0xDE, 0xAD, 0xBE];
        let mut first = vec![CMD_STA_DSP];
        first.extend_from_slice(&colcon);
        let spu = spu_packet(&[0xAA; 16], &[(0, first), (75, vec![CMD_STP_DSP])]);
        let frames = framer.feed(Some(0), &spu);
        assert_eq!(
            frames[0].duration,
            Some(75 * DELAY_UNIT_TICKS),
            "STP_DSP after a CHG_COLCON is still found"
        );
    }

    #[test]
    fn multi_command_first_dcsq() {
        let mut framer = VobSubFramer::new(720, 480, gray_palette());
        // Realistic first DCSQ: full command set with correct param lengths.
        let first = vec![
            CMD_SET_COLOR,
            0x10,
            0x32, // 2 params
            CMD_SET_CONTR,
            0xFF,
            0xF0, // 2 params
            CMD_SET_DAREA,
            0x00,
            0x01,
            0x00,
            0x00,
            0x02,
            0x00, // 6 params
            CMD_SET_DSPXA,
            0x00,
            0x04,
            0x00,
            0x40, // 4 params
            CMD_STA_DSP,
        ];
        let spu = spu_packet(&[0xAA; 80], &[(0, first), (110, vec![CMD_STP_DSP])]);
        let frames = framer.feed(Some(0), &spu);
        assert_eq!(
            frames[0].duration,
            Some(110 * DELAY_UNIT_TICKS),
            "params are skipped correctly to reach the stop command"
        );
    }

    // ── CodecPrivate ──────────────────────────────────────────────────

    #[test]
    fn codec_private_has_size_and_palette() {
        let framer = VobSubFramer::new(720, 480, gray_palette());
        let idx = String::from_utf8(framer.codec_private().to_vec())
            .expect("CodecPrivate is valid UTF-8");

        assert!(idx.contains("size: 720x480"), "size line present: {idx}");
        assert!(idx.contains("palette: "), "palette line present: {idx}");
        // 16 entries → 15 separators.
        assert_eq!(
            idx.matches(", ").count(),
            15,
            "palette has 16 comma-separated entries"
        );
    }

    #[test]
    fn codec_private_pal_dimensions() {
        let framer = VobSubFramer::new(720, 576, gray_palette());
        let idx = String::from_utf8(framer.codec_private().to_vec()).expect("utf8");
        assert!(idx.contains("size: 720x576"), "PAL dimensions: {idx}");
    }

    #[test]
    fn palette_ycbcr_to_rgb_white() {
        // Y=235, Cr=Cb=128 (BT.601 white point) → near-white RGB.
        let mut palette = gray_palette();
        palette[0] = ClutEntry::new(235, 128, 128);
        let framer = VobSubFramer::new(720, 480, palette);
        let idx = String::from_utf8(framer.codec_private().to_vec()).expect("utf8");
        // 235 with zero chroma offset → 0xEB on all channels.
        assert!(
            idx.contains("palette: ebebeb"),
            "neutral luma maps to equal RGB: {idx}"
        );
    }

    #[test]
    fn palette_ycbcr_to_rgb_clamps() {
        // Y=255 with strong red chroma overflows R → clamps at 0xff.
        let mut palette = gray_palette();
        palette[0] = ClutEntry::new(255, 255, 128);
        let framer = VobSubFramer::new(720, 480, palette);
        let idx = String::from_utf8(framer.codec_private().to_vec()).expect("utf8");
        let first = idx
            .split("palette: ")
            .nth(1)
            .and_then(|s| s.split(',').next())
            .expect("first palette entry");
        assert_eq!(&first[0..2], "ff", "red channel clamps to ff: {first}");
    }

    // ── Robustness ────────────────────────────────────────────────────

    #[test]
    fn malformed_tiny_spdsz_drops_buffer() {
        let mut framer = VobSubFramer::new(720, 480, gray_palette());
        // SPDSZ = 2, below the 4-byte structural minimum.
        let frames = framer.feed(Some(0), &[0x00, 0x02, 0xFF, 0xFF]);
        assert!(frames.is_empty(), "malformed packet produces no frame");

        // Framer recovers for the next valid SPU.
        let spu = typical_spu(20, 50);
        let recovered = framer.feed(Some(100), &spu);
        assert_eq!(recovered.len(), 1, "framer works after dropping garbage");
        assert_eq!(recovered[0].pts, 100, "fresh PTS after recovery");
    }

    #[test]
    fn dcsqta_out_of_range_yields_no_duration() {
        let mut framer = VobSubFramer::new(720, 480, gray_palette());
        // Hand-build an SPU whose SP_DCSQTA points past the packet.
        let mut spu = vec![0u8; 10];
        let total = u16::try_from(spu.len()).expect("fits");
        spu[0..2].copy_from_slice(&total.to_be_bytes());
        spu[2..4].copy_from_slice(&999u16.to_be_bytes()); // bogus DCSQTA
        let frames = framer.feed(Some(0), &spu);
        assert_eq!(frames.len(), 1, "SPU still emits as a passthrough block");
        assert!(
            frames[0].duration.is_none(),
            "out-of-range DCSQTA yields no duration"
        );
    }

    // ── Flush and edge cases ──────────────────────────────────────────

    #[test]
    fn flush_discards_partial() {
        let mut framer = VobSubFramer::new(720, 480, gray_palette());
        let spu = typical_spu(100, 60);
        framer.feed(Some(0), &spu[..spu.len() / 2]);

        let flushed = framer.flush();
        assert!(flushed.is_empty(), "partial SPU is discarded on flush");

        let frames = framer.feed(Some(500), &typical_spu(20, 40));
        assert_eq!(frames.len(), 1, "framer works after flush");
        assert_eq!(frames[0].pts, 500, "PTS correct after flush");
    }

    #[test]
    fn flush_on_empty_buffer() {
        let mut framer = VobSubFramer::new(720, 480, gray_palette());
        assert!(framer.flush().is_empty(), "flush on empty produces nothing");
    }

    #[test]
    fn empty_feed() {
        let mut framer = VobSubFramer::new(720, 480, gray_palette());
        assert!(
            framer.feed(None, &[]).is_empty(),
            "empty input produces no frames"
        );
    }

    #[test]
    fn pts_defaults_to_zero_without_input() {
        let mut framer = VobSubFramer::new(720, 480, gray_palette());
        let frames = framer.feed(None, &typical_spu(20, 40));
        assert_eq!(frames[0].pts, 0, "default PTS is 0 when none provided");
    }

    #[test]
    fn second_packet_captures_own_pts() {
        let mut framer = VobSubFramer::new(720, 480, gray_palette());
        let f1 = framer.feed(Some(1000), &typical_spu(20, 40));
        assert_eq!(f1[0].pts, 1000, "first SPU PTS");

        let f2 = framer.feed(Some(5000), &typical_spu(20, 40));
        assert_eq!(f2[0].pts, 5000, "second SPU captures its own PTS");
    }
}
