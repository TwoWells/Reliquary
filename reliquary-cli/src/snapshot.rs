// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Page composition and video background extraction.
//!
//! Composites IG button bitmaps onto a canvas (optionally over a video
//! background frame) and writes the result as PNG for diagnostic output.

use std::collections::HashMap;

use crate::identify::{ExtractedButton, PageComposition};

/// Draws a colored border rectangle on an RGBA canvas.
///
/// Marks a button's region with a visible outline so the user can see
/// which area of the page corresponds to a playlist, even when the
/// button's selected-state bitmap is invisible or identical to normal.
pub fn draw_highlight_border(
    canvas: &mut [u8],
    canvas_width: usize,
    canvas_height: usize,
    bx: usize,
    by: usize,
    bw: usize,
    bh: usize,
) {
    const COLOR: [u8; 4] = [255, 50, 50, 255];
    const THICKNESS: usize = 3;

    let x_start = bx.saturating_sub(THICKNESS);
    let x_end = (bx + bw + THICKNESS).min(canvas_width);

    // Horizontal edges (top and bottom)
    for t in 0..THICKNESS {
        let top_y = by.saturating_sub(THICKNESS) + t;
        let bot_y = by + bh + t;
        for x in x_start..x_end {
            if top_y < canvas_height {
                let off = (top_y * canvas_width + x) * 4;
                canvas[off..off + 4].copy_from_slice(&COLOR);
            }
            if bot_y < canvas_height {
                let off = (bot_y * canvas_width + x) * 4;
                canvas[off..off + 4].copy_from_slice(&COLOR);
            }
        }
    }

    // Vertical edges (left and right)
    for y in by..(by + bh).min(canvas_height) {
        for t in 0..THICKNESS {
            let left_x = bx.saturating_sub(THICKNESS) + t;
            let right_x = bx + bw + t;
            if left_x < canvas_width {
                let off = (y * canvas_width + left_x) * 4;
                canvas[off..off + 4].copy_from_slice(&COLOR);
            }
            if right_x < canvas_width {
                let off = (y * canvas_width + right_x) * 4;
                canvas[off..off + 4].copy_from_slice(&COLOR);
            }
        }
    }
}

/// Composites a full menu page into an RGBA canvas.
///
/// When a video `background` is provided, button bitmaps are alpha-composited
/// on top of the decoded video frame — producing the same view a person sees
/// on their TV. Without a background, transparent regions are left as black.
///
/// All buttons are rendered at their `(x, y)` positions in the normal state.
/// If `highlight` is provided, that button is rendered in its selected state
/// instead.
pub fn composite_page(
    page: &PageComposition,
    highlight: Option<u16>,
    background: Option<&[u8]>,
) -> Vec<u8> {
    let w = usize::from(page.canvas_width);
    let h = usize::from(page.canvas_height);
    let expected = w * h * 4;
    let mut canvas = match background {
        Some(bg) if bg.len() == expected => bg.to_vec(),
        _ => vec![0u8; expected],
    };

    for btn in &page.buttons {
        let use_selected = highlight == Some(btn.button_id);
        let bitmap = if use_selected {
            btn.selected.as_ref().or(btn.normal.as_ref())
        } else {
            btn.normal.as_ref().or(btn.selected.as_ref())
        };
        let Some(bmp) = bitmap else { continue };

        let bx = usize::from(btn.x);
        let by = usize::from(btn.y);
        let bw = usize::from(bmp.width);
        let bh = usize::from(bmp.height);

        for row in 0..bh {
            let dst_y = by + row;
            if dst_y >= h {
                break;
            }
            for col in 0..bw {
                let dst_x = bx + col;
                if dst_x >= w {
                    break;
                }
                let src_off = (row * bw + col) * 4;
                let dst_off = (dst_y * w + dst_x) * 4;
                let alpha = bmp.data[src_off + 3];
                if alpha > 0 {
                    canvas[dst_off..dst_off + 4].copy_from_slice(&bmp.data[src_off..src_off + 4]);
                }
            }
        }
    }

    canvas
}

/// Writes an RGBA canvas as a PPM file (RGB, no alpha).
///
/// PPM is a trivial image format that needs no external dependencies.
/// The alpha channel is composited against black.
/// Writes an RGBA canvas as a scaled-down PNG file.
///
/// Uses uncompressed DEFLATE stored blocks (no compression library
/// needed). The canvas is scaled to `target_width` pixels wide
/// (preserving aspect ratio) and alpha is composited against black.
#[allow(clippy::print_stderr, reason = "CLI diagnostic output")]
#[allow(
    clippy::cast_possible_truncation,
    reason = "alpha-premultiply and dimension results fit target types"
)]
#[allow(
    clippy::cast_sign_loss,
    reason = "inputs are non-negative (u8 * normalized alpha)"
)]
pub fn write_png(
    path: &std::path::Path,
    width: u16,
    height: u16,
    rgba: &[u8],
    target_width: usize,
) {
    use std::io::Write;

    let src_w = usize::from(width);
    let src_h = usize::from(height);
    let dst_w = target_width.min(src_w);
    let dst_h = src_h * dst_w / src_w;

    // Build filtered scanlines: filter byte (0 = None) + RGB per pixel
    let row_bytes = 1 + dst_w * 3;
    let mut raw = Vec::with_capacity(dst_h * row_bytes);
    for row in 0..dst_h {
        raw.push(0); // filter: None
        for col in 0..dst_w {
            let sx = col * src_w / dst_w;
            let sy = row * src_h / dst_h;
            let off = (sy * src_w + sx) * 4;
            let a = f32::from(rgba[off + 3]) / 255.0;
            raw.push((f32::from(rgba[off]) * a) as u8);
            raw.push((f32::from(rgba[off + 1]) * a) as u8);
            raw.push((f32::from(rgba[off + 2]) * a) as u8);
        }
    }

    // DEFLATE stored blocks (uncompressed): split into <=65535 byte blocks
    let mut deflate = Vec::new();
    let chunks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let last = u8::from(i + 1 == chunks.len());
        deflate.push(last);
        let len = chunk.len() as u16;
        deflate.extend_from_slice(&len.to_le_bytes());
        deflate.extend_from_slice(&(!len).to_le_bytes());
        deflate.extend_from_slice(chunk);
    }

    // Adler-32 checksum of raw data
    let mut s1: u32 = 1;
    let mut s2: u32 = 0;
    for &b in &raw {
        s1 = (s1 + u32::from(b)) % 65521;
        s2 = (s2 + s1) % 65521;
    }
    let adler = (s2 << 16) | s1;

    // zlib wrapper: CMF + FLG + deflate + adler32
    let mut zlib = vec![0x78, 0x01];
    zlib.extend_from_slice(&deflate);
    zlib.extend_from_slice(&adler.to_be_bytes());

    let Ok(mut f) = std::fs::File::create(path) else {
        eprintln!("warning: could not create {}", path.display());
        return;
    };

    let png_crc = |tag: &[u8], data: &[u8]| -> u32 {
        let mut crc: u32 = 0xFFFF_FFFF;
        for &b in tag.iter().chain(data.iter()) {
            let mut c = u32::from((crc ^ u32::from(b)) as u8);
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            crc = c ^ (crc >> 8);
        }
        crc ^ 0xFFFF_FFFF
    };

    let png_chunk = |f: &mut std::fs::File,
                     tag: &[u8; 4],
                     data: &[u8],
                     crc_fn: &dyn Fn(&[u8], &[u8]) -> u32| {
        let _ = f.write_all(&(data.len() as u32).to_be_bytes());
        let _ = f.write_all(tag);
        let _ = f.write_all(data);
        let _ = f.write_all(&crc_fn(tag, data).to_be_bytes());
    };

    // PNG signature
    let _ = f.write_all(&[137, 80, 78, 71, 13, 10, 26, 10]);
    // IHDR
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(dst_w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(dst_h as u32).to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(2); // color type: RGB
    ihdr.extend_from_slice(&[0, 0, 0]); // compression, filter, interlace
    png_chunk(&mut f, b"IHDR", &ihdr, &png_crc);
    // IDAT
    png_chunk(&mut f, b"IDAT", &zlib, &png_crc);
    // IEND
    png_chunk(&mut f, b"IEND", &[], &png_crc);
}

/// Dumps composited page images for all content buttons.
///
/// Writes one PPM per content button to `dir`, named by playlist number
/// and breadcrumb step. Used for visual inspection of page composition.
#[allow(clippy::print_stderr, reason = "CLI diagnostic output")]
pub fn dump_page_images(
    dir: &std::path::Path,
    buttons: &[&ExtractedButton],
    pages: &[PageComposition],
    backgrounds: &HashMap<usize, Vec<u8>>,
) {
    let _ = std::fs::create_dir_all(dir);

    for button in buttons {
        let Some(playlist) = button.playlist else {
            continue;
        };

        let steps = if button.breadcrumb.is_empty() {
            vec![reliquary::disc::bdmv::mobj::BreadcrumbStep {
                clip_index: button.clip_index,
                page_id: button.page_id,
                button_id: button.button_id,
            }]
        } else {
            button.breadcrumb.clone()
        };

        for (i, step) in steps.iter().enumerate() {
            if let Some(page) = pages
                .iter()
                .find(|p| p.clip_index == step.clip_index && p.page_id == step.page_id)
            {
                let bg = backgrounds.get(&page.clip_index).map(Vec::as_slice);
                let canvas = composite_page(page, Some(step.button_id), bg);
                let name = format!("pl{playlist:03}_step{i}_page{}.png", step.page_id);
                write_png(
                    &dir.join(name),
                    page.canvas_width,
                    page.canvas_height,
                    &canvas,
                    240,
                );
            }
        }
    }

    eprintln!("wrote page images to {}", dir.display());
}

/// Extracts the first video frame from an m2ts clip as RGBA pixel data.
///
/// Uses `ffmpeg` to decode one video frame, scaled to the given dimensions.
/// Returns `None` if `ffmpeg` is not available or frame extraction fails.
/// Callers fall back to a black background (the pre-existing behavior).
#[allow(clippy::print_stderr, reason = "CLI diagnostic output")]
pub fn extract_video_frame(clip_data: &[u8], width: u16, height: u16) -> Option<Vec<u8>> {
    use std::io::Write;

    // Use ~/tmp if it exists (avoids tmpfs/ramdisk pressure for large clips),
    // otherwise fall back to the system temp directory.
    let temp_dir = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .map(|h| h.join("tmp"))
        .filter(|d| d.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let temp_path = temp_dir.join(format!("reliquary_menu_{}.m2ts", std::process::id()));

    std::fs::File::create(&temp_path)
        .and_then(|mut f| f.write_all(clip_data))
        .ok()?;

    let output = std::process::Command::new("ffmpeg")
        .arg("-v")
        .arg("quiet")
        .arg("-i")
        .arg(&temp_path)
        .arg("-vframes")
        .arg("1")
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg("rgba")
        .arg("-s")
        .arg(format!("{width}x{height}"))
        .arg("pipe:1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok();

    let _ = std::fs::remove_file(&temp_path);

    let output = output?;
    let expected = usize::from(width) * usize::from(height) * 4;
    if output.status.success() && output.stdout.len() == expected {
        Some(output.stdout)
    } else {
        None
    }
}
