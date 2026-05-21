// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Terminal image rendering — kitty, sixel, and halfblock protocols.

use std::collections::HashMap;

use crate::util::terminal_columns;

// ── Terminal image rendering ───────────────────────────────────────────

/// Graphics protocol for inline image rendering.
pub enum GraphicsProtocol {
    /// Kitty graphics protocol (Kitty, `WezTerm`, Ghostty, Konsole 5.30+).
    Kitty,
    /// Sixel (foot, xterm, mlterm, Windows Terminal 1.22+, mintty).
    Sixel,
    /// Halfblock characters — universal fallback.
    Halfblock,
}

/// Detects the best available terminal graphics protocol.
pub fn detect_graphics_protocol() -> GraphicsProtocol {
    if let Ok(term_program) = std::env::var("TERM_PROGRAM") {
        match term_program.to_ascii_lowercase().as_str() {
            "kitty" | "wezterm" | "ghostty" => return GraphicsProtocol::Kitty,
            "foot" | "mlterm" | "mintty" => return GraphicsProtocol::Sixel,
            _ => {}
        }
    }

    if let Ok(term) = std::env::var("TERM") {
        if term.contains("kitty") {
            return GraphicsProtocol::Kitty;
        }
        // foot sets TERM=foot-extra or foot
        if term.starts_with("foot") {
            return GraphicsProtocol::Sixel;
        }
    }

    // Konsole 5.30+ supports Kitty graphics
    if std::env::var("KONSOLE_VERSION")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .is_some_and(|v| v >= 230_000)
    {
        return GraphicsProtocol::Kitty;
    }

    // Windows Terminal supports Sixel (1.22+)
    if std::env::var_os("WT_SESSION").is_some() {
        return GraphicsProtocol::Sixel;
    }

    GraphicsProtocol::Halfblock
}

/// Renders an RGBA bitmap inline in the terminal.
///
/// Auto-detects the terminal graphics protocol and uses the best
/// available: Kitty > Sixel > halfblock. Pre-scales the image to
/// fit within the terminal width for pixel-based protocols.
#[allow(clippy::print_stderr, reason = "CLI bitmap rendering")]
pub fn render_bitmap(width: u16, height: u16, rgba: &[u8]) {
    let protocol = detect_graphics_protocol();
    match protocol {
        GraphicsProtocol::Kitty | GraphicsProtocol::Sixel => {
            // Scale to fit terminal: approximate cell width of 8 pixels,
            // capped at 960px to keep data transfer reasonable.
            let max_w = (terminal_columns() * 8).min(960);
            let (sw, sh, scaled) = scale_canvas(width, height, rgba, max_w);
            if matches!(protocol, GraphicsProtocol::Kitty) {
                render_kitty(sw, sh, &scaled);
            } else {
                render_sixel(sw, sh, &scaled);
            }
        }
        GraphicsProtocol::Halfblock => render_halfblock(width, height, rgba),
    }
}

// ── Image scaling ────────────────────────────────────────────────────

/// Scales an RGBA canvas to fit within `max_width` pixels, preserving
/// aspect ratio via nearest-neighbor sampling.
///
/// Returns the original data unchanged if already within bounds.
#[allow(
    clippy::cast_possible_truncation,
    reason = "scaled dimensions are bounded by max_width which fits u16"
)]
fn scale_canvas(width: u16, height: u16, rgba: &[u8], max_width: usize) -> (u16, u16, Vec<u8>) {
    let w = usize::from(width);
    let h = usize::from(height);

    if w <= max_width {
        return (width, height, rgba.to_vec());
    }

    let new_w = max_width;
    let new_h = (h * new_w / w).max(1);

    let mut scaled = Vec::with_capacity(new_w * new_h * 4);
    for row in 0..new_h {
        for col in 0..new_w {
            let src_x = (col * w / new_w).min(w - 1);
            let src_y = (row * h / new_h).min(h - 1);
            let off = (src_y * w + src_x) * 4;
            scaled.extend_from_slice(&rgba[off..off + 4]);
        }
    }

    (new_w as u16, new_h as u16, scaled)
}

// ── Kitty graphics protocol ───────────────────────────────────────────

/// Renders via the Kitty graphics protocol (APC escape sequences).
///
/// Sends raw RGBA data base64-encoded. Chunks at 4096 bytes to stay
/// within protocol limits. Uses the `c=` placement parameter to
/// constrain the image to the terminal width.
#[allow(clippy::print_stderr, reason = "CLI bitmap rendering")]
fn render_kitty(width: u16, height: u16, rgba: &[u8]) {
    let cols = terminal_columns();
    let encoded = base64_encode(rgba);
    let chunks: Vec<&[u8]> = encoded.as_bytes().chunks(4096).collect();

    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i < chunks.len() - 1);
        let chunk_str = std::str::from_utf8(chunk).unwrap_or("");
        if i == 0 {
            eprint!("\x1b_Gf=32,s={width},v={height},a=T,c={cols},m={more};{chunk_str}\x1b\\");
        } else {
            eprint!("\x1b_Gm={more};{chunk_str}\x1b\\");
        }
    }
    eprintln!();
}

/// Base64-encodes a byte slice (RFC 4648, no line breaks).
#[allow(
    clippy::cast_possible_truncation,
    reason = "index masked to 0-63 always fits in usize"
)]
fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut result = String::with_capacity(data.len().div_ceil(3) * 4);

    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let n = (b0 << 16) | (b1 << 8) | b2;

        result.push(char::from(CHARS[(n >> 18 & 0x3F) as usize]));
        result.push(char::from(CHARS[(n >> 12 & 0x3F) as usize]));
        if chunk.len() > 1 {
            result.push(char::from(CHARS[(n >> 6 & 0x3F) as usize]));
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(char::from(CHARS[(n & 0x3F) as usize]));
        } else {
            result.push('=');
        }
    }

    result
}

// ── Sixel graphics ────────────────────────────────────────────────────

/// Renders via the Sixel graphics protocol (DCS escape sequences).
///
/// Quantizes the image to ≤256 colors and encodes as sixel bands
/// (6-pixel-high rows) with RLE compression.
#[allow(clippy::print_stderr, reason = "CLI bitmap rendering")]
fn render_sixel(width: u16, height: u16, rgba: &[u8]) {
    let w = usize::from(width);
    let h = usize::from(height);
    let (palette, indices) = build_sixel_palette(rgba, w, h);

    // DCS introducer + raster attributes
    eprint!("\x1bP0;0;0q\"1;1;{w};{h}");

    // Color registers (RGB percentages 0-100)
    for (i, &[r, g, b]) in palette.iter().enumerate() {
        let rp = u32::from(r) * 100 / 255;
        let gp = u32::from(g) * 100 / 255;
        let bp = u32::from(b) * 100 / 255;
        eprint!("#{i};2;{rp};{gp};{bp}");
    }

    // Sixel data: process in 6-row bands
    let num_bands = h.div_ceil(6);
    for band in 0..num_bands {
        let band_y = band * 6;
        let mut first_color = true;

        for (color_idx, _) in palette.iter().enumerate() {
            // Skip colors absent from this band
            if !band_has_color(&indices, w, h, band_y, color_idx) {
                continue;
            }

            if first_color {
                first_color = false;
            } else {
                eprint!("$");
            }
            eprint!("#{color_idx}");

            // Encode columns with RLE
            let mut run_char: Option<u8> = None;
            let mut run_len: usize = 0;

            for x in 0..w {
                let mut bits: u8 = 0;
                for row_off in 0..6u8 {
                    let y = band_y + usize::from(row_off);
                    if y < h && indices[y * w + x] == Some(color_idx) {
                        bits |= 1 << row_off;
                    }
                }
                let ch = bits + 0x3F;

                if run_char == Some(ch) {
                    run_len += 1;
                } else {
                    emit_sixel_run(run_char, run_len);
                    run_char = Some(ch);
                    run_len = 1;
                }
            }
            emit_sixel_run(run_char, run_len);
        }

        if band < num_bands - 1 {
            eprint!("-");
        }
    }

    // String terminator
    eprint!("\x1b\\");
    eprintln!();
}

/// Checks whether a color appears in a given 6-row band.
fn band_has_color(
    indices: &[Option<usize>],
    w: usize,
    h: usize,
    band_y: usize,
    color_idx: usize,
) -> bool {
    for row_off in 0..6 {
        let y = band_y + row_off;
        if y >= h {
            break;
        }
        for x in 0..w {
            if indices[y * w + x] == Some(color_idx) {
                return true;
            }
        }
    }
    false
}

/// Emits a sixel run (single character or `!count<char>` for repeats).
#[allow(clippy::print_stderr, reason = "sixel output fragment")]
fn emit_sixel_run(ch: Option<u8>, len: usize) {
    let Some(ch) = ch else { return };
    if len == 1 {
        eprint!("{}", char::from(ch));
    } else if len > 1 {
        eprint!("!{len}{}", char::from(ch));
    }
}

/// Builds a color palette and per-pixel index map for sixel encoding.
///
/// Collects unique RGB values (ignoring transparent pixels). If more
/// than 256 unique colors exist, quantizes to a 6×6×6 RGB cube.
fn build_sixel_palette(rgba: &[u8], w: usize, h: usize) -> (Vec<[u8; 3]>, Vec<Option<usize>>) {
    let total = w * h;
    let mut color_map: HashMap<[u8; 3], usize> = HashMap::new();
    let mut palette = Vec::new();
    let mut indices = Vec::with_capacity(total);

    for pixel in rgba.chunks_exact(4) {
        if pixel[3] == 0 {
            indices.push(None);
            continue;
        }
        let rgb = [pixel[0], pixel[1], pixel[2]];
        let idx = *color_map.entry(rgb).or_insert_with(|| {
            let i = palette.len();
            palette.push(rgb);
            i
        });
        indices.push(Some(idx));
    }

    // If under the 256-color limit, we're done
    if palette.len() <= 256 {
        return (palette, indices);
    }

    // Quantize to 6×6×6 RGB cube (216 colors)
    color_map.clear();
    palette.clear();
    indices.clear();

    for pixel in rgba.chunks_exact(4) {
        if pixel[3] == 0 {
            indices.push(None);
            continue;
        }
        let qr = pixel[0] / 43;
        let qg = pixel[1] / 43;
        let qb = pixel[2] / 43;
        let rgb = [qr * 51, qg * 51, qb * 51];
        let idx = *color_map.entry(rgb).or_insert_with(|| {
            let i = palette.len();
            palette.push(rgb);
            i
        });
        indices.push(Some(idx));
    }

    (palette, indices)
}

// ── Halfblock fallback ────────────────────────────────────────────────

/// Renders via Unicode halfblock characters with truecolor ANSI escapes.
///
/// Each pair of pixel rows becomes one terminal row: top pixel sets the
/// background color, bottom pixel sets the foreground, using `▄`.
/// Scales to fit terminal width via nearest-neighbor sampling.
#[allow(clippy::print_stderr, reason = "CLI bitmap rendering")]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "pixel coordinates and scaling factors are small positive values"
)]
fn render_halfblock(width: u16, height: u16, rgba: &[u8]) {
    let term_cols = terminal_columns();
    let w = usize::from(width);
    let h = usize::from(height);

    let (scaled_w, scaled_h) = if w > term_cols {
        let scale = term_cols as f64 / w as f64;
        (
            (w as f64 * scale) as usize,
            (h as f64 * scale).max(1.0) as usize,
        )
    } else {
        (w, h)
    };

    for row in (0..scaled_h).step_by(2) {
        eprint!("  ");
        for col in 0..scaled_w {
            let top = sample_pixel(rgba, w, h, scaled_w, scaled_h, col, row);
            let bot = if row + 1 < scaled_h {
                sample_pixel(rgba, w, h, scaled_w, scaled_h, col, row + 1)
            } else {
                [0, 0, 0, 0]
            };

            if top[3] == 0 && bot[3] == 0 {
                eprint!(" ");
            } else if bot[3] == 0 {
                eprint!("\x1b[38;2;{};{};{}m\u{2580}\x1b[0m", top[0], top[1], top[2]);
            } else if top[3] == 0 {
                eprint!("\x1b[38;2;{};{};{}m\u{2584}\x1b[0m", bot[0], bot[1], bot[2]);
            } else {
                eprint!(
                    "\x1b[48;2;{};{};{}m\x1b[38;2;{};{};{}m\u{2584}\x1b[0m",
                    top[0], top[1], top[2], bot[0], bot[1], bot[2]
                );
            }
        }
        eprintln!();
    }
}

/// Samples a pixel from the source bitmap using nearest-neighbor scaling.
fn sample_pixel(
    rgba: &[u8],
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
    col: usize,
    row: usize,
) -> [u8; 4] {
    let src_x = (col * src_w / dst_w).min(src_w - 1);
    let src_y = (row * src_h / dst_h).min(src_h - 1);
    let offset = (src_y * src_w + src_x) * 4;
    if offset + 3 < rgba.len() {
        [
            rgba[offset],
            rgba[offset + 1],
            rgba[offset + 2],
            rgba[offset + 3],
        ]
    } else {
        [0, 0, 0, 0]
    }
}
