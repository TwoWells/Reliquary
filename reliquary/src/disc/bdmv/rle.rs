// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! RLE decoder for Blu-ray IG/PGS bitmap objects.
//!
//! Decodes run-length encoded button bitmaps from IG Object Definition
//! Segments into RGBA pixel buffers. The RLE format is shared between
//! IG (Interactive Graphics) and PGS (Presentation Graphics) streams.
//!
//! Input: [`ObjectDefinition`] (raw RLE data + dimensions) and [`Palette`]
//! (`YCrCbA` color entries).
//! Output: [`Bitmap`] containing RGBA pixel data.
//!
//! Reference: libbluray `src/libbluray/decoders/rle.c`.

use thiserror::Error;

use super::ig::{ObjectDefinition, Palette};

// ── Errors ──────────────────────────────────────────────────────────────

/// Errors that can occur while decoding RLE bitmap data.
#[derive(Debug, Error)]
pub enum RleError {
    /// RLE data ended before the bitmap was fully decoded.
    #[error(
        "unexpected end of RLE data: decoded {decoded_pixels} pixels, expected {expected_pixels}"
    )]
    UnexpectedEnd {
        /// Number of pixels successfully decoded.
        decoded_pixels: usize,
        /// Total number of pixels expected (width × height).
        expected_pixels: usize,
    },

    /// A run extends beyond the current row boundary.
    #[error(
        "run overflow on row {row}: run length {run_length}, only {remaining_in_row} pixels remaining"
    )]
    RunOverflow {
        /// Row where the overflow occurred (0-indexed).
        row: u16,
        /// Length of the offending run.
        run_length: usize,
        /// Pixels remaining in the current row.
        remaining_in_row: usize,
    },
}

// ── Public types ────────────────────────────────────────────────────────

/// A decoded RGBA bitmap.
#[derive(Debug)]
pub struct Bitmap {
    /// Width in pixels.
    pub width: u16,
    /// Height in pixels.
    pub height: u16,
    /// RGBA pixel data, row-major, 4 bytes per pixel.
    /// Length = width × height × 4.
    pub data: Vec<u8>,
}

// ── Public API ──────────────────────────────────────────────────────────

/// Decodes an RLE-compressed IG object into an RGBA bitmap.
///
/// Applies the palette to convert indexed color to RGBA via
/// `YCrCbA` → RGBA conversion (BT.601 coefficients).
///
/// Palette indices not present in the palette are rendered as
/// transparent black (`[0, 0, 0, 0]`) for graceful degradation
/// with sparse palettes.
///
/// # Errors
///
/// Returns [`RleError::UnexpectedEnd`] if the RLE data is exhausted
/// before all rows are decoded, or [`RleError::RunOverflow`] if a
/// run extends past the current row boundary.
pub fn decode(object: &ObjectDefinition, palette: &Palette) -> Result<Bitmap, RleError> {
    let lut = build_rgba_lut(palette);
    let data = decode_rle(&object.rle_data, object.width, object.height, &lut)?;
    Ok(Bitmap {
        width: object.width,
        height: object.height,
        data,
    })
}

// ── Color conversion ────────────────────────────────────────────────────

/// Builds a 256-entry RGBA lookup table from a palette.
///
/// Converts each `YCrCbA` palette entry to RGBA using BT.601 coefficients.
/// Entries not present in the palette default to transparent black.
fn build_rgba_lut(palette: &Palette) -> [[u8; 4]; 256] {
    let mut lut = [[0u8; 4]; 256];

    for entry in &palette.entries {
        let y = f64::from(entry.y);
        let cr = f64::from(entry.cr) - 128.0;
        let cb = f64::from(entry.cb) - 128.0;

        let r = 1.402f64.mul_add(cr, y).round();
        let g = 0.714_136f64
            .mul_add(-cr, 0.344_136f64.mul_add(-cb, y))
            .round();
        let b = 1.772f64.mul_add(cb, y).round();

        lut[entry.index as usize] = [clamp_u8(r), clamp_u8(g), clamp_u8(b), entry.alpha];
    }

    lut
}

/// Clamps a floating-point value to the `[0, 255]` range and truncates.
#[allow(
    clippy::cast_possible_truncation,
    reason = "value is clamped to 0..=255 before truncation"
)]
#[allow(
    clippy::cast_sign_loss,
    reason = "value is clamped to non-negative before cast"
)]
#[allow(
    clippy::missing_const_for_fn,
    reason = "f64::clamp is not const-stable"
)]
fn clamp_u8(value: f64) -> u8 {
    value.clamp(0.0, 255.0) as u8
}

// ── RLE decoding ────────────────────────────────────────────────────────

/// Decodes RLE-compressed data into an RGBA pixel buffer.
///
/// RLE encoding format (Blu-ray IG/PGS):
/// - Non-zero byte `I`: single pixel of color `I`
/// - `0x00 0x00`: end of line
/// - `0x00 [00CCCCCC]`: run of C pixels of color 0 (C in 1..63)
/// - `0x00 [01CCCCCC CCCCCCCC]`: run of C pixels of color 0 (C in 64..16383)
/// - `0x00 [10CCCCCC] [II]`: run of C pixels of color I (C in 1..63)
/// - `0x00 [11CCCCCC CCCCCCCC] [II]`: run of C pixels of color I (C in 64..16383)
fn decode_rle(
    rle_data: &[u8],
    width: u16,
    height: u16,
    lut: &[[u8; 4]; 256],
) -> Result<Vec<u8>, RleError> {
    let width_usize = width as usize;
    let height_usize = height as usize;
    let total_pixels = width_usize * height_usize;
    let mut pixels = vec![0u8; total_pixels * 4];

    let mut row: u16 = 0;
    let mut col: usize = 0;
    let mut pos: usize = 0;

    while (row as usize) < height_usize {
        if pos >= rle_data.len() {
            let decoded = row as usize * width_usize + col;
            if decoded < total_pixels {
                return Err(RleError::UnexpectedEnd {
                    decoded_pixels: decoded,
                    expected_pixels: total_pixels,
                });
            }
            break;
        }

        let byte = rle_data[pos];
        pos += 1;

        if byte != 0x00 {
            // Single pixel of color `byte`
            if col >= width_usize {
                return Err(RleError::RunOverflow {
                    row,
                    run_length: 1,
                    remaining_in_row: 0,
                });
            }
            let pixel_offset = (row as usize * width_usize + col) * 4;
            pixels[pixel_offset..pixel_offset + 4].copy_from_slice(&lut[byte as usize]);
            col += 1;
        } else {
            // Escape byte — read the control byte
            if pos >= rle_data.len() {
                let decoded = row as usize * width_usize + col;
                return Err(RleError::UnexpectedEnd {
                    decoded_pixels: decoded,
                    expected_pixels: total_pixels,
                });
            }

            let control = rle_data[pos];
            pos += 1;

            if control == 0x00 {
                // End of line — fill remaining columns with color 0
                let pixel_offset = (row as usize * width_usize + col) * 4;
                let remaining = width_usize - col;
                for i in 0..remaining {
                    let offset = pixel_offset + i * 4;
                    pixels[offset..offset + 4].copy_from_slice(&lut[0]);
                }
                row += 1;
                col = 0;
            } else {
                let (run_length, color_index) = parse_run(rle_data, &mut pos, control)?;

                if run_length > width_usize - col {
                    return Err(RleError::RunOverflow {
                        row,
                        run_length,
                        remaining_in_row: width_usize - col,
                    });
                }

                let pixel_offset = (row as usize * width_usize + col) * 4;
                let rgba = &lut[color_index as usize];
                for i in 0..run_length {
                    let offset = pixel_offset + i * 4;
                    pixels[offset..offset + 4].copy_from_slice(rgba);
                }
                col += run_length;
            }
        }
    }

    Ok(pixels)
}

/// Parses a run from the RLE stream given the control byte.
///
/// Returns `(run_length, color_index)`.
///
/// # Errors
///
/// Returns [`RleError::UnexpectedEnd`] if the stream is truncated mid-run.
fn parse_run(rle_data: &[u8], pos: &mut usize, control: u8) -> Result<(usize, u8), RleError> {
    let prefix = control & 0xC0;

    match prefix {
        0x00 => {
            // Short zero run: count = lower 6 bits
            let count = (control & 0x3F) as usize;
            Ok((count, 0))
        }
        0x40 => {
            // Long zero run: count = lower 6 bits << 8 | next byte
            if *pos >= rle_data.len() {
                return Err(RleError::UnexpectedEnd {
                    decoded_pixels: 0,
                    expected_pixels: 0,
                });
            }
            let count = ((control & 0x3F) as usize) << 8 | rle_data[*pos] as usize;
            *pos += 1;
            Ok((count, 0))
        }
        0x80 => {
            // Short color run: count = lower 6 bits, color = next byte
            if *pos >= rle_data.len() {
                return Err(RleError::UnexpectedEnd {
                    decoded_pixels: 0,
                    expected_pixels: 0,
                });
            }
            let count = (control & 0x3F) as usize;
            let color = rle_data[*pos];
            *pos += 1;
            Ok((count, color))
        }
        _ => {
            // 0xC0: Long color run: count = lower 6 bits << 8 | next byte, color after
            if *pos + 1 >= rle_data.len() {
                return Err(RleError::UnexpectedEnd {
                    decoded_pixels: 0,
                    expected_pixels: 0,
                });
            }
            let count = ((control & 0x3F) as usize) << 8 | rle_data[*pos] as usize;
            *pos += 1;
            let color = rle_data[*pos];
            *pos += 1;
            Ok((count, color))
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod tests {
    use super::*;
    use crate::disc::bdmv::ig::tests::IgBuilder;
    use crate::disc::bdmv::ig::{self, PaletteEntry};

    /// Helper to create a palette with given entries.
    fn make_palette(entries: &[(u8, u8, u8, u8, u8)]) -> Palette {
        Palette {
            palette_id: 0,
            entries: entries
                .iter()
                .map(|&(index, y, cr, cb, alpha)| PaletteEntry {
                    index,
                    y,
                    cr,
                    cb,
                    alpha,
                })
                .collect(),
        }
    }

    /// Helper to create an `ObjectDefinition` with given parameters.
    fn make_object(width: u16, height: u16, rle_data: Vec<u8>) -> ObjectDefinition {
        ObjectDefinition {
            object_id: 0,
            width,
            height,
            rle_data,
        }
    }

    // ── YCrCbA → RGBA conversion tests ─────────────────────────────────

    #[test]
    fn ycrcba_white_conversion() {
        // Y=235, Cr=128, Cb=128 → neutral (235, 235, 235)
        let palette = make_palette(&[(1, 235, 128, 128, 255)]);
        let lut = build_rgba_lut(&palette);

        assert_eq!(lut[1][0], 235, "white R");
        assert_eq!(lut[1][1], 235, "white G");
        assert_eq!(lut[1][2], 235, "white B");
        assert_eq!(lut[1][3], 255, "white A");
    }

    #[test]
    fn ycrcba_black_conversion() {
        // Y=16, Cr=128, Cb=128 → neutral dark (16, 16, 16)
        let palette = make_palette(&[(1, 16, 128, 128, 255)]);
        let lut = build_rgba_lut(&palette);

        assert_eq!(lut[1][0], 16, "black R");
        assert_eq!(lut[1][1], 16, "black G");
        assert_eq!(lut[1][2], 16, "black B");
        assert_eq!(lut[1][3], 255, "black A");
    }

    #[test]
    fn ycrcba_red_conversion() {
        // BT.601: Red at Y=81, Cr=240, Cb=90
        let palette = make_palette(&[(1, 81, 240, 90, 255)]);
        let lut = build_rgba_lut(&palette);

        assert!(lut[1][0] > 230, "red R should be high: got {}", lut[1][0]);
        assert!(lut[1][1] < 20, "red G should be low: got {}", lut[1][1]);
        assert!(lut[1][2] < 20, "red B should be low: got {}", lut[1][2]);
    }

    #[test]
    fn ycrcba_blue_conversion() {
        // BT.601: Blue at Y=41, Cr=110, Cb=240
        let palette = make_palette(&[(1, 41, 110, 240, 255)]);
        let lut = build_rgba_lut(&palette);

        assert!(lut[1][0] < 20, "blue R should be low: got {}", lut[1][0]);
        assert!(lut[1][1] < 20, "blue G should be low: got {}", lut[1][1]);
        assert!(lut[1][2] > 230, "blue B should be high: got {}", lut[1][2]);
    }

    #[test]
    fn ycrcba_transparent() {
        let palette = make_palette(&[(1, 235, 128, 128, 0)]);
        let lut = build_rgba_lut(&palette);

        assert_eq!(lut[1][3], 0, "alpha should be 0");
    }

    #[test]
    fn ycrcba_opaque() {
        let palette = make_palette(&[(1, 235, 128, 128, 255)]);
        let lut = build_rgba_lut(&palette);

        assert_eq!(lut[1][3], 255, "alpha should be 255");
    }

    #[test]
    fn ycrcba_clamps_overflow() {
        // Y=255, Cr=255 → R would overflow without clamping
        let palette = make_palette(&[(1, 255, 255, 128, 255)]);
        let lut = build_rgba_lut(&palette);

        assert_eq!(lut[1][0], 255, "R clamped to 255");
    }

    #[test]
    fn ycrcba_clamps_underflow() {
        // Y=0, Cr=0 → R = 0 + 1.402*(0-128) = -179 → clamped to 0
        let palette = make_palette(&[(1, 0, 0, 255, 255)]);
        let lut = build_rgba_lut(&palette);

        assert_eq!(lut[1][0], 0, "R clamped to 0");
    }

    #[test]
    fn unused_palette_indices_are_transparent_black() {
        // Only index 5 is defined
        let palette = make_palette(&[(5, 235, 128, 128, 255)]);
        let lut = build_rgba_lut(&palette);

        assert_eq!(lut[0], [0, 0, 0, 0], "index 0 is transparent black");
        assert_eq!(lut[1], [0, 0, 0, 0], "index 1 is transparent black");
        assert_eq!(lut[255], [0, 0, 0, 0], "index 255 is transparent black");
        assert_ne!(lut[5], [0, 0, 0, 0], "index 5 is defined");
    }

    // ── RLE decoding tests ──────────────────────────────────────────────

    #[test]
    fn single_pixel_non_zero() {
        // 1×1 bitmap, single non-zero pixel followed by end-of-line
        let palette = make_palette(&[(0xAA, 235, 128, 128, 255)]);
        let rle_data = vec![0xAA, 0x00, 0x00];
        let obj = make_object(1, 1, rle_data);

        let bitmap = decode(&obj, &palette).expect("should decode single pixel");
        assert_eq!(bitmap.width, 1, "width");
        assert_eq!(bitmap.height, 1, "height");
        assert_eq!(bitmap.data.len(), 4, "data length");
        assert_eq!(bitmap.data[0..3], [235, 235, 235], "pixel is white");
        assert_eq!(bitmap.data[3], 255, "pixel alpha");
    }

    #[test]
    fn solid_color_row_short_run() {
        // 4×1 bitmap: run of 4 pixels of color 1
        let palette = make_palette(&[(1, 235, 128, 128, 255)]);
        // 0x00 [10000100] [01] = short color run of 4 pixels of color 1
        let rle_data = vec![0x00, 0x84, 0x01, 0x00, 0x00];
        let obj = make_object(4, 1, rle_data);

        let bitmap = decode(&obj, &palette).expect("should decode solid row");
        assert_eq!(bitmap.data.len(), 16, "4 pixels × 4 bytes");
        for i in 0..4 {
            let offset = i * 4;
            assert_eq!(
                bitmap.data[offset..offset + 4],
                [235, 235, 235, 255],
                "pixel {i} should be white"
            );
        }
    }

    #[test]
    fn run_of_zeros_short() {
        // 4×1 bitmap: short run of 4 zeros
        let palette = make_palette(&[(0, 16, 128, 128, 255)]);
        // 0x00 [00000100] = run of 4 zeros
        let rle_data = vec![0x00, 0x04, 0x00, 0x00];
        let obj = make_object(4, 1, rle_data);

        let bitmap = decode(&obj, &palette).expect("should decode zero run");
        for i in 0..4 {
            let offset = i * 4;
            assert_eq!(
                bitmap.data[offset..offset + 4],
                [16, 16, 16, 255],
                "pixel {i} should be palette[0]"
            );
        }
    }

    #[test]
    fn run_of_zeros_long() {
        // 100×1 bitmap: long run of 100 zeros (count > 63)
        let palette = make_palette(&[(0, 16, 128, 128, 0)]);
        // 0x00 [01000000 01100100] = long zero run, count = (0 << 8) | 100 = 100
        let rle_data = vec![0x00, 0x40, 0x64, 0x00, 0x00];
        let obj = make_object(100, 1, rle_data);

        let bitmap = decode(&obj, &palette).expect("should decode long zero run");
        assert_eq!(bitmap.data.len(), 400, "100 pixels × 4 bytes");
        for i in 0..100 {
            let offset = i * 4;
            assert_eq!(
                bitmap.data[offset + 3],
                0,
                "pixel {i} should be transparent"
            );
        }
    }

    #[test]
    fn color_run_long() {
        // 100×1 bitmap: long color run of 100 pixels of color 2
        let palette = make_palette(&[(2, 81, 240, 90, 255)]);
        // 0x00 [11000000 01100100] [02] = long color run, count = 100, color = 2
        let rle_data = vec![0x00, 0xC0, 0x64, 0x02, 0x00, 0x00];
        let obj = make_object(100, 1, rle_data);

        let bitmap = decode(&obj, &palette).expect("should decode long color run");
        let lut = build_rgba_lut(&palette);
        for i in 0..100 {
            let offset = i * 4;
            assert_eq!(
                bitmap.data[offset..offset + 4],
                lut[2],
                "pixel {i} should match palette[2]"
            );
        }
    }

    #[test]
    fn alternating_pixels() {
        // 4×1 bitmap: alternating colors 1 and 2
        let palette = make_palette(&[(1, 235, 128, 128, 255), (2, 16, 128, 128, 255)]);
        let rle_data = vec![0x01, 0x02, 0x01, 0x02, 0x00, 0x00];
        let obj = make_object(4, 1, rle_data);

        let bitmap = decode(&obj, &palette).expect("should decode alternating");
        let lut = build_rgba_lut(&palette);
        assert_eq!(bitmap.data[0..4], lut[1], "pixel 0");
        assert_eq!(bitmap.data[4..8], lut[2], "pixel 1");
        assert_eq!(bitmap.data[8..12], lut[1], "pixel 2");
        assert_eq!(bitmap.data[12..16], lut[2], "pixel 3");
    }

    #[test]
    fn end_of_line_pads_with_color_zero() {
        // 4×1 bitmap: 2 pixels then end-of-line (remaining 2 filled with palette[0])
        let palette = make_palette(&[(0, 16, 128, 128, 0), (1, 235, 128, 128, 255)]);
        let rle_data = vec![0x01, 0x01, 0x00, 0x00];
        let obj = make_object(4, 1, rle_data);

        let bitmap = decode(&obj, &palette).expect("should decode with padding");
        let lut = build_rgba_lut(&palette);
        assert_eq!(bitmap.data[0..4], lut[1], "pixel 0 is color 1");
        assert_eq!(bitmap.data[4..8], lut[1], "pixel 1 is color 1");
        assert_eq!(bitmap.data[8..12], lut[0], "pixel 2 padded with color 0");
        assert_eq!(bitmap.data[12..16], lut[0], "pixel 3 padded with color 0");
    }

    #[test]
    fn multi_row_bitmap() {
        // 2×2 bitmap
        let palette = make_palette(&[(1, 235, 128, 128, 255), (2, 16, 128, 128, 255)]);
        let rle_data = vec![
            0x01, 0x02, 0x00, 0x00, // row 0
            0x02, 0x01, 0x00, 0x00, // row 1
        ];
        let obj = make_object(2, 2, rle_data);

        let bitmap = decode(&obj, &palette).expect("should decode 2×2");
        let lut = build_rgba_lut(&palette);
        assert_eq!(bitmap.data[0..4], lut[1], "row 0 col 0");
        assert_eq!(bitmap.data[4..8], lut[2], "row 0 col 1");
        assert_eq!(bitmap.data[8..12], lut[2], "row 1 col 0");
        assert_eq!(bitmap.data[12..16], lut[1], "row 1 col 1");
    }

    #[test]
    fn mixed_runs_and_singles() {
        // 8×1: 2 singles, run of 3, 3 singles
        let palette = make_palette(&[(1, 100, 128, 128, 255), (2, 200, 128, 128, 255)]);
        let rle_data = vec![
            0x01, 0x02, // 2 singles (color 1, color 2)
            0x00, 0x83, 0x01, // short color run of 3, color 1
            0x02, 0x01, 0x02, // 3 singles (color 2, 1, 2)
            0x00, 0x00, // end of line
        ];
        let obj = make_object(8, 1, rle_data);

        let bitmap = decode(&obj, &palette).expect("should decode mixed");
        let lut = build_rgba_lut(&palette);
        assert_eq!(bitmap.data[0..4], lut[1], "pixel 0");
        assert_eq!(bitmap.data[4..8], lut[2], "pixel 1");
        assert_eq!(bitmap.data[8..12], lut[1], "pixel 2 (run)");
        assert_eq!(bitmap.data[12..16], lut[1], "pixel 3 (run)");
        assert_eq!(bitmap.data[16..20], lut[1], "pixel 4 (run)");
        assert_eq!(bitmap.data[20..24], lut[2], "pixel 5");
        assert_eq!(bitmap.data[24..28], lut[1], "pixel 6");
        assert_eq!(bitmap.data[28..32], lut[2], "pixel 7");
    }

    // ── Edge case tests ─────────────────────────────────────────────────

    #[test]
    fn empty_rle_data_returns_error() {
        let palette = make_palette(&[(0, 0, 0, 0, 0)]);
        let obj = make_object(4, 4, vec![]);

        let result = decode(&obj, &palette);
        assert!(result.is_err(), "empty RLE data should error");
        let err = result.expect_err("should be UnexpectedEnd");
        assert!(
            matches!(err, RleError::UnexpectedEnd { .. }),
            "should be UnexpectedEnd, got: {err}"
        );
    }

    #[test]
    fn run_overflow_returns_error() {
        // 2×1 bitmap: run of 3 in a 2-wide row
        let palette = make_palette(&[(1, 235, 128, 128, 255)]);
        let rle_data = vec![0x00, 0x83, 0x01, 0x00, 0x00];
        let obj = make_object(2, 1, rle_data);

        let result = decode(&obj, &palette);
        let err = result.expect_err("should be RunOverflow");
        assert!(
            matches!(
                err,
                RleError::RunOverflow {
                    row: 0,
                    run_length: 3,
                    remaining_in_row: 2,
                }
            ),
            "expected RunOverflow(row=0, run=3, remaining=2), got: {err}"
        );
    }

    #[test]
    fn sparse_palette_uses_transparent_black() {
        // Palette only defines index 5, RLE data uses index 99
        let palette = make_palette(&[(5, 235, 128, 128, 255)]);
        let rle_data = vec![99, 0x00, 0x00];
        let obj = make_object(1, 1, rle_data);

        let bitmap = decode(&obj, &palette).expect("should decode with missing index");
        assert_eq!(
            bitmap.data[0..4],
            [0, 0, 0, 0],
            "undefined index renders as transparent black"
        );
    }

    #[test]
    fn long_run_two_byte_count() {
        // 200×1 bitmap: long run of 200 pixels of color 3
        let palette = make_palette(&[(3, 128, 128, 128, 255)]);
        // 0x00 [11000000 11001000] [03] = long color run, count = 200
        let rle_data = vec![0x00, 0xC0, 0xC8, 0x03, 0x00, 0x00];
        let obj = make_object(200, 1, rle_data);

        let bitmap = decode(&obj, &palette).expect("should decode 200-pixel run");
        let lut = build_rgba_lut(&palette);
        for i in 0..200 {
            let offset = i * 4;
            assert_eq!(
                bitmap.data[offset..offset + 4],
                lut[3],
                "pixel {i} should be color 3"
            );
        }
    }

    #[test]
    fn larger_bitmap_multiple_rows() {
        // 10×3 bitmap with different patterns per row
        let palette = make_palette(&[(0, 0, 128, 128, 0), (1, 235, 128, 128, 255)]);
        let rle_data = vec![
            // Row 0: short color run of 10 pixels of color 1
            0x00, 0x8A, 0x01, 0x00, 0x00, // Row 1: short zero run of 10
            0x00, 0x0A, 0x00, 0x00,
            // Row 2: 5 singles of color 1, then end-of-line (pads remaining 5)
            0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00,
        ];
        let obj = make_object(10, 3, rle_data);

        let bitmap = decode(&obj, &palette).expect("should decode 10×3");
        let lut = build_rgba_lut(&palette);

        // Row 0: all color 1
        for i in 0..10 {
            let offset = i * 4;
            assert_eq!(bitmap.data[offset..offset + 4], lut[1], "row 0 pixel {i}");
        }
        // Row 1: all color 0
        for i in 0..10 {
            let offset = (10 + i) * 4;
            assert_eq!(bitmap.data[offset..offset + 4], lut[0], "row 1 pixel {i}");
        }
        // Row 2: first 5 = color 1, last 5 = color 0 (padded)
        for i in 0..5 {
            let offset = (20 + i) * 4;
            assert_eq!(bitmap.data[offset..offset + 4], lut[1], "row 2 pixel {i}");
        }
        for i in 5..10 {
            let offset = (20 + i) * 4;
            assert_eq!(bitmap.data[offset..offset + 4], lut[0], "row 2 pixel {i}");
        }
    }

    // ── Integration test ────────────────────────────────────────────────

    #[test]
    fn ig_parse_then_rle_decode() {
        // Build a synthetic IG stream with a known object and palette,
        // parse it, then decode the RLE object.
        let rle_data = vec![
            0x01, 0x02, 0x00, 0x00, // row 0: color 1, color 2
            0x02, 0x01, 0x00, 0x00, // row 1: color 2, color 1
        ];

        let ig_data = IgBuilder::new()
            .palette(
                0,
                &[
                    (0, 0, 128, 128, 0),     // index 0: transparent black
                    (1, 235, 128, 128, 255), // index 1: white
                    (2, 16, 128, 128, 255),  // index 2: dark
                ],
            )
            .object(0, 2, 2, &rle_data)
            .end_of_display()
            .build();

        let stream = ig::parse(&ig_data).expect("should parse IG stream");
        let ds = &stream.display_sets[0];
        let object = &ds.objects[0];
        let palette = &ds.palettes[0];

        let bitmap = decode(object, palette).expect("should decode RLE");
        assert_eq!(bitmap.width, 2, "decoded width");
        assert_eq!(bitmap.height, 2, "decoded height");
        assert_eq!(bitmap.data.len(), 16, "2×2×4 bytes");

        let lut = build_rgba_lut(palette);
        assert_eq!(bitmap.data[0..4], lut[1], "row 0, col 0 = white");
        assert_eq!(bitmap.data[4..8], lut[2], "row 0, col 1 = dark");
        assert_eq!(bitmap.data[8..12], lut[2], "row 1, col 0 = dark");
        assert_eq!(bitmap.data[12..16], lut[1], "row 1, col 1 = white");
    }
}
