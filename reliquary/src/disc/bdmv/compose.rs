// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Page composition — combines IG button bitmaps with video backgrounds
//! into full-resolution menu page renders.
//!
//! This module provides the compositing types and function used by both
//! the CLI (`identify` subcommand) and the navigator GUI to render disc
//! menus at full resolution with button-state highlighting.

use super::rle::Bitmap;

// ── Types ─────────────────���────────────────────────────────────────────

/// All decoded button bitmaps for one IG page, with positions and canvas size.
///
/// Used for full-page rendering: composite all buttons at their `(x, y)`
/// coordinates onto a canvas at the composition window dimensions, then
/// highlight the active button by overwriting its region with the selected-
/// state bitmap.
pub struct PageComposition {
    /// Index into the clips list (identifies the IG clip).
    pub clip_index: usize,
    /// Page identifier.
    pub page_id: u8,
    /// Canvas width in pixels (from the IG composition descriptor).
    pub canvas_width: u16,
    /// Canvas height in pixels.
    pub canvas_height: u16,
    /// Decoded button bitmaps with positions.
    pub buttons: Vec<ButtonComposition>,
}

/// A single button's position and decoded bitmaps (both states).
pub struct ButtonComposition {
    /// Button identifier.
    pub button_id: u16,
    /// Horizontal position on the canvas.
    pub x: u16,
    /// Vertical position on the canvas.
    pub y: u16,
    /// Normal (unselected) state bitmap, if decodable.
    pub normal: Option<Bitmap>,
    /// Selected (highlighted) state bitmap, if decodable.
    pub selected: Option<Bitmap>,
}

// ── Compositing ──────────────────────���──────────────────────────��──────

/// Composites a full menu page into an RGBA canvas.
///
/// When a video `background` is provided, button bitmaps are alpha-composited
/// on top of the decoded video frame — producing the same view a person sees
/// on their TV. Without a background, transparent regions are left as black.
///
/// All buttons are rendered at their `(x, y)` positions in the normal state.
/// If `highlight` is provided, that button is rendered in its selected state
/// instead.
#[must_use]
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

#[cfg(test)]
mod tests {
    use super::{ButtonComposition, PageComposition, composite_page};
    use crate::disc::bdmv::rle::Bitmap;

    #[test]
    fn composite_empty_page_returns_black_canvas() {
        let page = PageComposition {
            clip_index: 0,
            page_id: 0,
            canvas_width: 4,
            canvas_height: 2,
            buttons: Vec::new(),
        };
        let canvas = composite_page(&page, None, None);
        assert_eq!(
            canvas.len(),
            4 * 2 * 4,
            "canvas byte count should match w*h*4"
        );
        assert!(
            canvas.iter().all(|&b| b == 0),
            "empty page should be all black"
        );
    }

    #[test]
    fn composite_uses_background_when_provided() {
        let page = PageComposition {
            clip_index: 0,
            page_id: 0,
            canvas_width: 2,
            canvas_height: 2,
            buttons: Vec::new(),
        };
        let bg = vec![128u8; 2 * 2 * 4];
        let canvas = composite_page(&page, None, Some(&bg));
        assert_eq!(canvas, bg, "canvas should equal background when no buttons");
    }

    #[test]
    fn composite_renders_button_normal_state() {
        let page = PageComposition {
            clip_index: 0,
            page_id: 0,
            canvas_width: 4,
            canvas_height: 4,
            buttons: vec![ButtonComposition {
                button_id: 1,
                x: 1,
                y: 1,
                normal: Some(Bitmap {
                    width: 2,
                    height: 2,
                    data: vec![
                        255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
                    ],
                }),
                selected: None,
            }],
        };
        let canvas = composite_page(&page, None, None);
        // Check pixel at (1,1) — first pixel of the button
        // row=1 * width=4 + col=1 = index 5, times 4 bytes per pixel
        let off = (4 + 1) * 4;
        assert_eq!(
            &canvas[off..off + 4],
            &[255, 0, 0, 255],
            "button pixel (0,0) should be red"
        );
    }

    #[test]
    fn composite_highlights_selected_button() {
        let normal_data = vec![255, 0, 0, 255]; // red
        let selected_data = vec![0, 255, 0, 255]; // green

        let page = PageComposition {
            clip_index: 0,
            page_id: 0,
            canvas_width: 2,
            canvas_height: 2,
            buttons: vec![ButtonComposition {
                button_id: 5,
                x: 0,
                y: 0,
                normal: Some(Bitmap {
                    width: 1,
                    height: 1,
                    data: normal_data,
                }),
                selected: Some(Bitmap {
                    width: 1,
                    height: 1,
                    data: selected_data,
                }),
            }],
        };

        // Without highlight: should use normal (red)
        let canvas = composite_page(&page, None, None);
        assert_eq!(
            &canvas[0..4],
            &[255, 0, 0, 255],
            "no highlight should show normal state"
        );

        // With highlight on button 5: should use selected (green)
        let canvas = composite_page(&page, Some(5), None);
        assert_eq!(
            &canvas[0..4],
            &[0, 255, 0, 255],
            "highlight should show selected state"
        );
    }

    #[test]
    fn composite_skips_transparent_pixels() {
        let bg = vec![100, 100, 100, 255, 200, 200, 200, 255];
        let page = PageComposition {
            clip_index: 0,
            page_id: 0,
            canvas_width: 2,
            canvas_height: 1,
            buttons: vec![ButtonComposition {
                button_id: 1,
                x: 0,
                y: 0,
                normal: Some(Bitmap {
                    width: 2,
                    height: 1,
                    data: vec![
                        255, 0, 0, 255, // opaque red
                        0, 0, 0, 0, // fully transparent
                    ],
                }),
                selected: None,
            }],
        };
        let canvas = composite_page(&page, None, Some(&bg));
        // First pixel: button overwrites (opaque)
        assert_eq!(
            &canvas[0..4],
            &[255, 0, 0, 255],
            "opaque pixel should overwrite background"
        );
        // Second pixel: transparent, background preserved
        assert_eq!(
            &canvas[4..8],
            &[200, 200, 200, 255],
            "transparent pixel should preserve background"
        );
    }
}
