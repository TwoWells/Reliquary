// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Pure overlay-compositing logic: a software IG plane.
//!
//! libbluray decodes HDMV menus into a stream of **compressed YUV overlay**
//! commands on the Interactive Graphics (IG) plane — `INIT`, `DRAW`, `CLEAR`,
//! `WIPE`, `HIDE`, `FLUSH`, `CLOSE`. This module turns that command stream into
//! a flat ARGB canvas: it owns the plane buffer, RLE-decodes each `DRAW`
//! bitmap, converts the BT.601 limited-range `Y/Cr/Cb/T` palette to straight
//! RGBA, and blits the decoded region into the plane (clipped to plane bounds).
//!
//! Everything here is deliberately **disc-free and `unsafe`-free** so it can be
//! unit-tested exhaustively. The FFI trampoline in [`crate::ffi`] is the only
//! thing that feeds it real overlay events; it does so by translating the raw
//! `BD_OVERLAY` into the borrowed [`DrawCommand`] / plane calls below.

/// A single palette entry as libbluray delivers it: BT.601 limited-range
/// luma/chroma plus an 8-bit transparency. Mirrors `BD_PG_PALETTE_ENTRY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaletteEntry {
    /// Luma component, nominally `16..=235`.
    pub y: u8,
    /// Cr (red-difference) chroma, nominally `16..=240`.
    pub cr: u8,
    /// Cb (blue-difference) chroma, nominally `16..=240`.
    pub cb: u8,
    /// Transparency: `0` fully transparent, `255` fully opaque.
    pub t: u8,
}

/// One RLE run as libbluray delivers it. Mirrors `BD_PG_RLE_ELEM`.
///
/// `len == 0` is the **end-of-line** marker (per the libbluray overlay
/// contract); any `color` it carries is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RleElem {
    /// Run length in pixels. `0` marks the end of the current scanline.
    pub len: u16,
    /// Palette index for this run (`0..=255`).
    pub color: u16,
}

/// A borrowed `DRAW` command: an RLE bitmap plus its 256-entry palette, to be
/// composited at `(x, y)` with declared dimensions `w x h` on the plane.
#[derive(Debug, Clone, Copy)]
pub struct DrawCommand<'a> {
    /// Destination x of the region's top-left, in plane coordinates.
    pub x: u16,
    /// Destination y of the region's top-left, in plane coordinates.
    pub y: u16,
    /// Declared region width in pixels.
    pub w: u16,
    /// Declared region height in pixels.
    pub h: u16,
    /// The overlay palette (libbluray always supplies 256 entries).
    pub palette: &'a [PaletteEntry],
    /// The RLE-compressed bitmap, row-major, each row ended by a `len == 0`
    /// element.
    pub img: &'a [RleElem],
}

/// Convert a single BT.601 limited-range palette entry to straight (non-pre-
/// multiplied) RGBA bytes `[r, g, b, a]`.
///
/// libbluray documents the overlay palette as carrying the *same* color matrix
/// as the associated video stream; HDMV menu graphics use BT.601 limited range
/// (`Y` in `16..=235`, `Cr`/`Cb` in `16..=240`, centred on 128). The standard
/// inverse is applied and the result is clamped to `0..=255`. The alpha is the
/// entry's transparency byte verbatim (`0` transparent … `255` opaque).
#[must_use]
pub fn palette_entry_to_rgba(entry: PaletteEntry) -> [u8; 4] {
    // Promote to f32 and remove the limited-range offsets. Coefficients are the
    // standard BT.601 YCbCr -> R'G'B' inverse for 8-bit limited range.
    let y = (f32::from(entry.y) - 16.0) * (255.0 / 219.0);
    let cb = f32::from(entry.cb) - 128.0;
    let cr = f32::from(entry.cr) - 128.0;

    let r = 1.402_f32.mul_add(cr, y);
    let g = 0.714_136_f32.mul_add(-cr, 0.344_136_f32.mul_add(-cb, y));
    let b = 1.772_f32.mul_add(cb, y);

    [clamp_u8(r), clamp_u8(g), clamp_u8(b), entry.t]
}

/// Round to nearest and clamp a float color component into a `u8`.
const fn clamp_u8(v: f32) -> u8 {
    // Clamp into the representable range first, then round to nearest. After the
    // clamp the value is a finite float in `0.0..=255.0`, so the subsequent
    // `as u8` is exact: no negative values (sign loss) and no values above 255
    // (truncation) can occur.
    let clamped = v.clamp(0.0, 255.0).round();
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "value is clamped to 0.0..=255.0 and rounded, so the cast to u8 is exact"
    )]
    {
        clamped as u8
    }
}

/// Errors from decoding an RLE bitmap into a rectangular pixel buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RleError {
    /// A run referenced a palette index past the supplied palette.
    #[error("RLE run referenced palette index {index} but palette has {len} entries")]
    PaletteIndexOutOfRange {
        /// The offending palette index.
        index: usize,
        /// The palette length actually supplied.
        len: usize,
    },
}

/// Decode an RLE bitmap into a row-major buffer of palette **indices**,
/// `w * h` entries, one per pixel.
///
/// Each scanline is a sequence of runs terminated by a `len == 0` element.
/// Runs that would overflow the declared row width are clipped to the row;
/// rows that end (via `len == 0`) before filling `w` pixels are padded with
/// index `0`. At most `h` rows are produced — trailing input is ignored, and a
/// short input is zero-padded to the full `w * h`. This mirrors how players
/// tolerate the (occasionally loose) RLE that real discs emit, while always
/// yielding exactly `w * h` indices so compositing bounds are static.
#[must_use]
pub fn decode_rle_indices(img: &[RleElem], w: u16, h: u16) -> Vec<u16> {
    let width = usize::from(w);
    let height = usize::from(h);
    let mut out = vec![0u16; width * height];

    let mut elems = img.iter();
    for row in 0..height {
        let row_start = row * width;
        let mut col = 0usize;
        // Consume runs until the end-of-line marker or input exhaustion.
        loop {
            let Some(elem) = elems.next() else {
                // Input exhausted: remaining rows stay zero-filled.
                return out;
            };
            if elem.len == 0 {
                // End of this scanline; remaining columns stay index 0.
                break;
            }
            let run = usize::from(elem.len);
            // Clip the run to what fits in the row; ignore overflow pixels.
            let writable = run.min(width.saturating_sub(col));
            if writable > 0 {
                let start = row_start + col;
                out[start..start + writable].fill(elem.color);
            }
            col += run;
            // If the run (logically) filled or overran the row, keep consuming
            // until the explicit end-of-line marker so row framing stays in
            // sync with the input.
        }
    }
    out
}

/// An axis-aligned rectangle in plane coordinates: the top-left corner plus
/// width and height, all in pixels.
///
/// This is the bounding box of a single overlay `DRAW` — and, for a clean
/// lone-draw (a `FLUSH` preceded by exactly one IG `DRAW`), the bounding box of
/// the currently-selected menu button. It is exposed on the safe API so callers
/// can crop the composited frame to just that button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// Top-left x in plane coordinates.
    pub x: u16,
    /// Top-left y in plane coordinates.
    pub y: u16,
    /// Width in pixels.
    pub w: u16,
    /// Height in pixels.
    pub h: u16,
}

/// Crop a row-major RGBA buffer to `rect`, returning a tight
/// `out_w * out_h * 4` RGBA sub-buffer plus its dimensions, or `None` if the
/// rect does not overlap the source at all (or has zero area after clipping).
///
/// The source is `plane_w * plane_h` pixels, `[R, G, B, A]` per pixel. `rect`
/// is clipped to the source bounds first, so a rect that runs off the right or
/// bottom edge yields the in-bounds portion rather than panicking; the returned
/// dimensions are the clipped ones. A zero-area rect (or one entirely outside
/// the source) yields `None`.
///
/// This is the pure crop the recognizer needs: a single-button image carved out
/// of the full menu canvas with no color cue or layout assumption.
#[must_use]
pub fn crop_rgba(
    rgba: &[u8],
    plane_w: u32,
    plane_h: u32,
    rect: Rect,
) -> Option<(Vec<u8>, u32, u32)> {
    // Clip the rect to the source bounds (exclusive end), mirroring how the
    // plane clips a WIPE: a draw rect from a previous, differently-sized page
    // must never index out of range.
    let x0 = u32::from(rect.x).min(plane_w);
    let y0 = u32::from(rect.y).min(plane_h);
    let x1 = (u32::from(rect.x) + u32::from(rect.w)).min(plane_w);
    let y1 = (u32::from(rect.y) + u32::from(rect.h)).min(plane_h);

    // Empty after clipping (zero area, or fully off-canvas): nothing to crop.
    if x1 <= x0 || y1 <= y0 {
        return None;
    }

    let out_w = x1 - x0;
    let out_h = y1 - y0;
    let row_bytes = (out_w as usize) * 4;
    let mut out = vec![0u8; row_bytes * (out_h as usize)];

    // The source row stride must match the declared geometry; if it does not
    // (a truncated buffer) we refuse rather than read past the end.
    let src_stride = (plane_w as usize) * 4;
    if rgba.len() < (plane_h as usize) * src_stride {
        return None;
    }

    for row in 0..out_h {
        let src_base = ((y0 + row) as usize) * src_stride + (x0 as usize) * 4;
        let dst_base = (row as usize) * row_bytes;
        out[dst_base..dst_base + row_bytes].copy_from_slice(&rgba[src_base..src_base + row_bytes]);
    }
    Some((out, out_w, out_h))
}

/// An ARGB plane: a flat, row-major RGBA byte buffer of fixed dimensions.
///
/// One byte order is used throughout: `[R, G, B, A]` per pixel, which is what
/// egui's `ColorImage`/`TextureHandle` expects for an unmultiplied RGBA upload.
/// The plane is allocated on `INIT` and cleared/drawn/wiped by the overlay
/// command stream; [`Plane::pixels`] hands the whole buffer to the renderer on
/// `FLUSH`.
#[derive(Debug, Clone)]
pub struct Plane {
    width: u32,
    height: u32,
    /// `width * height * 4` bytes, `[R, G, B, A]` per pixel.
    rgba: Vec<u8>,
}

impl Plane {
    /// Allocate a fully transparent plane of `width x height`.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        let len = (width as usize)
            .saturating_mul(height as usize)
            .saturating_mul(4);
        Self {
            width,
            height,
            rgba: vec![0u8; len],
        }
    }

    /// Plane width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Plane height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The raw RGBA bytes, `width * height * 4`, `[R, G, B, A]` per pixel.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.rgba
    }

    /// Reset the entire plane to fully transparent (the `CLEAR` command).
    pub fn clear(&mut self) {
        self.rgba.fill(0);
    }

    /// Clear a rectangular region to fully transparent (the `WIPE` command),
    /// clipped to plane bounds.
    pub fn wipe(&mut self, x: u16, y: u16, w: u16, h: u16) {
        let (x0, y0, x1, y1) = self.clip_rect(x, y, w, h);
        for row in y0..y1 {
            let base = (row * self.width + x0) as usize * 4;
            let span = (x1 - x0) as usize * 4;
            self.rgba[base..base + span].fill(0);
        }
    }

    /// Composite a decoded `DRAW` command onto the plane.
    ///
    /// The RLE bitmap is decoded to palette indices, each index mapped through
    /// the palette to RGBA, and the result written at `(cmd.x, cmd.y)`. Pixels
    /// outside the plane are clipped. Fully transparent source pixels
    /// (`alpha == 0`) are **skipped** (left as-is) so a `DRAW` over existing
    /// content behaves like an overlay rather than an opaque rectangle; any
    /// non-zero alpha overwrites the destination (source-over with the source
    /// treated as already-composited, matching how the IG plane is authored as
    /// a single opaque layer per region).
    ///
    /// # Errors
    ///
    /// Returns [`RleError::PaletteIndexOutOfRange`] if any run references a
    /// palette index beyond `cmd.palette`.
    pub fn draw(&mut self, cmd: &DrawCommand<'_>) -> Result<(), RleError> {
        let indices = decode_rle_indices(cmd.img, cmd.w, cmd.h);

        // Pre-convert the palette once; indices reuse the cache.
        let palette_len = cmd.palette.len();
        let region_w = usize::from(cmd.w);

        for (i, &idx) in indices.iter().enumerate() {
            let index = usize::from(idx);
            let entry = *cmd
                .palette
                .get(index)
                .ok_or(RleError::PaletteIndexOutOfRange {
                    index,
                    len: palette_len,
                })?;
            let rgba = palette_entry_to_rgba(entry);
            if rgba[3] == 0 {
                // Fully transparent source pixel: leave the destination intact.
                continue;
            }

            // Destination pixel = region origin + position within the region.
            // `i < w*h` and `region_w == w`, so both local coordinates fit a
            // u16 and therefore a u32 without loss.
            let local_x = u32::try_from(i % region_w).unwrap_or(u32::MAX);
            let local_y = u32::try_from(i / region_w).unwrap_or(u32::MAX);
            let dst_x = u32::from(cmd.x) + local_x;
            let dst_y = u32::from(cmd.y) + local_y;
            if dst_x >= self.width || dst_y >= self.height {
                // Outside the plane — clip.
                continue;
            }
            let base = (dst_y * self.width + dst_x) as usize * 4;
            self.rgba[base..base + 4].copy_from_slice(&rgba);
        }
        Ok(())
    }

    /// Clip a rectangle to plane bounds, returning `(x0, y0, x1, y1)` as an
    /// exclusive-end range with `x0 <= x1` and `y0 <= y1`.
    fn clip_rect(&self, x: u16, y: u16, w: u16, h: u16) -> (u32, u32, u32, u32) {
        let x0 = u32::from(x).min(self.width);
        let y0 = u32::from(y).min(self.height);
        let x1 = (u32::from(x) + u32::from(w)).min(self.width);
        let y1 = (u32::from(y) + u32::from(h)).min(self.height);
        (x0, y0, x1.max(x0), y1.max(y0))
    }
}

/// One IG-plane overlay command, decoded from a raw libbluray `BD_OVERLAY`
/// into owned, `unsafe`-free data the [`OverlayCtx`] can apply.
///
/// The FFI trampoline in [`crate::ffi`] is responsible for the raw-pointer
/// walk that produces these (copying the palette and RLE image out of the
/// borrowed event); from here on the data is plain Rust, so the command
/// dispatch is fully testable.
#[derive(Debug, Clone)]
pub enum OverlayEvent {
    /// `BD_OVERLAY_INIT`: (re)allocate the plane at the given size.
    Init {
        /// Plane width in pixels.
        w: u16,
        /// Plane height in pixels.
        h: u16,
    },
    /// `BD_OVERLAY_DRAW`: composite an RLE bitmap at `(x, y)`.
    Draw {
        /// Destination x within the plane.
        x: u16,
        /// Destination y within the plane.
        y: u16,
        /// Region width.
        w: u16,
        /// Region height.
        h: u16,
        /// The overlay palette (256 entries as supplied by libbluray).
        palette: Vec<PaletteEntry>,
        /// The RLE-compressed bitmap.
        img: Vec<RleElem>,
    },
    /// `BD_OVERLAY_CLEAR`: clear the whole plane.
    Clear,
    /// `BD_OVERLAY_WIPE`: clear a rectangle.
    Wipe {
        /// Rectangle top-left x.
        x: u16,
        /// Rectangle top-left y.
        y: u16,
        /// Rectangle width.
        w: u16,
        /// Rectangle height.
        h: u16,
    },
    /// `BD_OVERLAY_HIDE`: the plane is empty and may be hidden.
    Hide,
    /// `BD_OVERLAY_FLUSH`: present accumulated changes; marks a frame complete.
    Flush,
    /// `BD_OVERLAY_CLOSE`: release the plane.
    Close,
}

/// The mutable overlay state shared between the FFI trampoline and the safe
/// [`crate::Disc`] API.
///
/// libbluray invokes the overlay callback **synchronously**, on the same
/// thread, from inside our `bd_read_ext` calls; the callback and the
/// `Disc`-side frame fetch never run concurrently. A [`core::cell::RefCell`]
/// therefore suffices to grant the trampoline `&mut` access without any
/// `unsafe` and without cross-thread synchronisation. The whole context is
/// owned by a `Box` in the `Disc`; its address is the `void* handle` handed to
/// `bd_register_overlay_proc`.
#[derive(Debug, Default)]
pub struct OverlayCtx {
    /// The current IG plane, allocated on `INIT`, dropped on `CLOSE`.
    plane: core::cell::RefCell<Option<Plane>>,
    /// Incremented on every `FLUSH`. Lets the UI cheaply detect "new frame".
    generation: core::cell::Cell<u64>,
    /// Number of `DRAW` commands seen since the last `FLUSH`. A `FLUSH` with a
    /// count of exactly `1` is a clean lone-draw (see [`Self::selected_button_rect`]).
    draws_this_frame: core::cell::Cell<u32>,
    /// The rect of the most recent `DRAW` since the last `FLUSH`. Only meaningful
    /// when `draws_this_frame == 1` at `FLUSH` time; otherwise discarded.
    last_draw_rect: core::cell::Cell<Option<Rect>>,
    /// The currently-selected button's bounding box, recorded from the most
    /// recent clean lone-draw. `None` until one is observed, and reset to `None`
    /// on a full-plane `CLEAR`/`HIDE` to avoid pointing at a stale page.
    selected_button_rect: core::cell::Cell<Option<Rect>>,
}

impl OverlayCtx {
    /// Create an empty overlay context (no plane until the first `INIT`).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one decoded overlay command to the plane.
    ///
    /// `Draw` errors (e.g. a bad palette index) are reported to the caller so
    /// the trampoline can log them; they never abort the plane. `Flush` bumps
    /// the generation counter so the UI knows a frame is ready.
    ///
    /// # Errors
    ///
    /// Propagates [`RleError`] from a malformed `Draw`'s RLE/palette.
    pub fn apply(&self, event: &OverlayEvent) -> Result<(), RleError> {
        match event {
            OverlayEvent::Init { w, h } => {
                *self.plane.borrow_mut() = Some(Plane::new(u32::from(*w), u32::from(*h)));
            }
            OverlayEvent::Draw {
                x,
                y,
                w,
                h,
                palette,
                img,
            } => {
                // Track per-frame draws for lone-draw detection regardless of
                // whether a plane is allocated, so the count reflects the actual
                // command stream the disc emitted this frame.
                self.draws_this_frame
                    .set(self.draws_this_frame.get().saturating_add(1));
                self.last_draw_rect.set(Some(Rect {
                    x: *x,
                    y: *y,
                    w: *w,
                    h: *h,
                }));
                if let Some(plane) = self.plane.borrow_mut().as_mut() {
                    plane.draw(&DrawCommand {
                        x: *x,
                        y: *y,
                        w: *w,
                        h: *h,
                        palette,
                        img,
                    })?;
                }
            }
            // CLEAR empties the plane; HIDE signals the plane is empty and may
            // be hidden — for our single-plane UI both mean "show nothing", so
            // they share a body (clear the plane, leaving it allocated). A clear
            // also invalidates the recorded selected-button rect: it precedes a
            // full page redraw, and the old rect's coordinates would point into
            // a now-different page. A brief `None` until the new page's
            // lone-draw lands is preferable to cropping the wrong region.
            OverlayEvent::Clear | OverlayEvent::Hide => {
                self.selected_button_rect.set(None);
                if let Some(plane) = self.plane.borrow_mut().as_mut() {
                    plane.clear();
                }
            }
            OverlayEvent::Wipe { x, y, w, h } => {
                if let Some(plane) = self.plane.borrow_mut().as_mut() {
                    plane.wipe(*x, *y, *w, *h);
                }
            }
            OverlayEvent::Flush => {
                // A lone-draw — exactly one DRAW since the previous FLUSH — is
                // the highlight redraw of the selected button: that single
                // DRAW's rect is the button's bounding box. A full page redraw
                // (many draws) or the top menu's deselect-old + select-new
                // pattern (two draws) is not a clean lone-draw, so the recorded
                // rect is left untouched. Because we always take the *latest*
                // clean lone-draw, the main-bar highlight (which lone-draws at a
                // different y) is naturally superseded by the actual item.
                if self.draws_this_frame.get() == 1
                    && let Some(rect) = self.last_draw_rect.get()
                {
                    self.selected_button_rect.set(Some(rect));
                }
                // Reset per-frame draw tracking for the next frame.
                self.draws_this_frame.set(0);
                self.last_draw_rect.set(None);
                self.generation.set(self.generation.get().wrapping_add(1));
            }
            OverlayEvent::Close => {
                *self.plane.borrow_mut() = None;
            }
        }
        Ok(())
    }

    /// The current flush generation. Increments once per `FLUSH`; a change
    /// since the last observation means a new composited frame is available.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation.get()
    }

    /// Snapshot the current plane as `(rgba, width, height)`, or `None` if no
    /// plane is allocated. The RGBA buffer is copied out so the caller holds no
    /// borrow into the context.
    #[must_use]
    pub fn snapshot(&self) -> Option<(Vec<u8>, u32, u32)> {
        self.plane
            .borrow()
            .as_ref()
            .map(|p| (p.pixels().to_vec(), p.width(), p.height()))
    }

    /// The currently-selected button's bounding box, derived from the most
    /// recent clean **lone-draw** — a `FLUSH` preceded by exactly one IG
    /// `DRAW`. `None` until one has been observed (or after a `CLEAR`/`HIDE`
    /// invalidated it).
    ///
    /// # Known limitation
    ///
    /// The top menu redraws a selection with a **two-draw** pattern (deselect
    /// the old button, then draw the new one) — that is not a lone-draw, so the
    /// rect is not cleanly isolable there and the previous value (or `None`) is
    /// retained. This is a deliberate consequence of the lone-draw rule observed
    /// on real hardware, not a defect: the submenu, which uses the clean
    /// single-draw highlight, is where this signal is reliable.
    #[must_use]
    pub const fn selected_button_rect(&self) -> Option<Rect> {
        self.selected_button_rect.get()
    }

    /// Crop the current composited plane to the selected-button rect, returning
    /// a tight `(rgba, width, height)` single-button image, or `None` if no
    /// plane is allocated or no clean lone-draw rect is available yet.
    ///
    /// The rect is clipped to the plane bounds, so a rect carried over from a
    /// differently-sized page can never index out of range.
    #[must_use]
    pub fn cropped_selection(&self) -> Option<(Vec<u8>, u32, u32)> {
        let rect = self.selected_button_rect.get()?;
        let plane = self.plane.borrow();
        let plane = plane.as_ref()?;
        crop_rgba(plane.pixels(), plane.width(), plane.height(), rect)
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod tests {
    use super::{
        DrawCommand, OverlayCtx, OverlayEvent, PaletteEntry, Plane, Rect, RleElem, RleError,
        crop_rgba, decode_rle_indices, palette_entry_to_rgba,
    };

    /// A `DRAW` event with a given rect; palette/img are minimal (a single
    /// transparent run) since these tests only exercise the rect bookkeeping,
    /// not the compositing already covered above.
    fn draw_event(x: u16, y: u16, w: u16, h: u16) -> OverlayEvent {
        OverlayEvent::Draw {
            x,
            y,
            w,
            h,
            palette: vec![
                PaletteEntry {
                    y: 16,
                    cr: 128,
                    cb: 128,
                    t: 0
                };
                256
            ],
            img: vec![RleElem { len: 0, color: 0 }; usize::from(h.max(1))],
        }
    }

    /// A 256-entry palette where entry 0 is transparent black and the given
    /// index/entry pairs are filled in. Keeps tests terse.
    fn palette(entries: &[(usize, PaletteEntry)]) -> Vec<PaletteEntry> {
        let mut p = vec![
            PaletteEntry {
                y: 16,
                cr: 128,
                cb: 128,
                t: 0
            };
            256
        ];
        for &(i, e) in entries {
            p[i] = e;
        }
        p
    }

    #[test]
    fn palette_white_is_opaque_white() {
        // BT.601 limited-range white: Y=235, neutral chroma, opaque.
        let rgba = palette_entry_to_rgba(PaletteEntry {
            y: 235,
            cr: 128,
            cb: 128,
            t: 255,
        });
        assert_eq!(
            rgba,
            [255, 255, 255, 255],
            "limited-range Y=235 neutral chroma should map to opaque white, got {rgba:?}"
        );
    }

    #[test]
    fn palette_black_is_opaque_black() {
        // BT.601 limited-range black: Y=16, neutral chroma, opaque.
        let rgba = palette_entry_to_rgba(PaletteEntry {
            y: 16,
            cr: 128,
            cb: 128,
            t: 255,
        });
        assert_eq!(
            rgba,
            [0, 0, 0, 255],
            "limited-range Y=16 neutral chroma should map to opaque black, got {rgba:?}"
        );
    }

    #[test]
    fn palette_alpha_is_passed_through() {
        // Transparency byte maps directly to the RGBA alpha channel.
        let rgba = palette_entry_to_rgba(PaletteEntry {
            y: 128,
            cr: 128,
            cb: 128,
            t: 100,
        });
        assert_eq!(
            rgba[3], 100,
            "the transparency byte should pass through to RGBA alpha verbatim, got {}",
            rgba[3]
        );
    }

    #[test]
    fn palette_primary_red_channels_make_sense() {
        // Saturated BT.601 red (Cr high, Cb low). We only assert R dominates and
        // the value is plausibly saturated, not an exact triple, to stay robust
        // to rounding.
        let rgba = palette_entry_to_rgba(PaletteEntry {
            y: 81,
            cr: 240,
            cb: 90,
            t: 255,
        });
        assert!(
            rgba[0] > rgba[1] && rgba[0] > rgba[2],
            "a BT.601 red entry should have the red channel dominate, got {rgba:?}"
        );
        assert!(
            rgba[0] > 200,
            "a BT.601 red entry should produce a near-saturated red, got {rgba:?}"
        );
    }

    #[test]
    fn rle_decodes_simple_runs() {
        // Row 0: 2 px of index 1, 2 px of index 2, end-of-line. Width 4, height 1.
        let img = [
            RleElem { len: 2, color: 1 },
            RleElem { len: 2, color: 2 },
            RleElem { len: 0, color: 0 },
        ];
        let idx = decode_rle_indices(&img, 4, 1);
        assert_eq!(
            idx,
            vec![1, 1, 2, 2],
            "two runs of length 2 should fill a 4-wide row, got {idx:?}"
        );
    }

    #[test]
    fn rle_end_of_line_pads_remaining_columns_with_zero() {
        // Row 0: 1 px of index 5, then end-of-line before width 4 is reached.
        let img = [RleElem { len: 1, color: 5 }, RleElem { len: 0, color: 0 }];
        let idx = decode_rle_indices(&img, 4, 1);
        assert_eq!(
            idx,
            vec![5, 0, 0, 0],
            "an early end-of-line should zero-pad the rest of the row, got {idx:?}"
        );
    }

    #[test]
    fn rle_clips_overlong_run_to_row_width() {
        // A run of length 10 on a 4-wide row must not bleed into the next row.
        let img = [
            RleElem { len: 10, color: 7 },
            RleElem { len: 0, color: 0 },
            RleElem { len: 1, color: 9 },
            RleElem { len: 0, color: 0 },
        ];
        let idx = decode_rle_indices(&img, 4, 2);
        assert_eq!(
            idx,
            vec![7, 7, 7, 7, 9, 0, 0, 0],
            "an overlong run must clip to its own row, leaving row 1 intact, got {idx:?}"
        );
    }

    #[test]
    fn rle_short_input_zero_pads_remaining_rows() {
        // Only one row supplied for a 2-row bitmap: row 1 stays zero.
        let img = [RleElem { len: 2, color: 3 }, RleElem { len: 0, color: 0 }];
        let idx = decode_rle_indices(&img, 2, 2);
        assert_eq!(
            idx,
            vec![3, 3, 0, 0],
            "missing rows should be zero-filled, got {idx:?}"
        );
    }

    #[test]
    fn draw_writes_opaque_pixels_into_plane() {
        // 2x1 red bitmap drawn at (1, 1) on a 4x4 plane.
        let pal = palette(&[(
            1,
            PaletteEntry {
                y: 81,
                cr: 240,
                cb: 90,
                t: 255,
            },
        )]);
        let img = [RleElem { len: 2, color: 1 }, RleElem { len: 0, color: 0 }];
        let mut plane = Plane::new(4, 4);
        plane
            .draw(&DrawCommand {
                x: 1,
                y: 1,
                w: 2,
                h: 1,
                palette: &pal,
                img: &img,
            })
            .expect("drawing an in-bounds bitmap should succeed");

        // Pixel (1,1) and (2,1) should be opaque; (0,0) untouched/transparent.
        let px = |x: u32, y: u32| {
            let base = (y * plane.width() + x) as usize * 4;
            plane.pixels()[base + 3]
        };
        assert_eq!(
            px(0, 0),
            0,
            "pixel outside the draw region should be transparent"
        );
        assert_ne!(
            px(1, 1),
            0,
            "pixel (1,1) inside the draw region should be opaque"
        );
        assert_ne!(
            px(2, 1),
            0,
            "pixel (2,1) inside the draw region should be opaque"
        );
    }

    #[test]
    fn draw_skips_transparent_source_pixels() {
        // Index 0 is transparent; a transparent draw must not clobber existing
        // opaque content beneath it.
        let pal = palette(&[(
            1,
            PaletteEntry {
                y: 235,
                cr: 128,
                cb: 128,
                t: 255,
            },
        )]);
        let mut plane = Plane::new(2, 1);
        // First lay down opaque white across the whole 2x1 plane.
        plane
            .draw(&DrawCommand {
                x: 0,
                y: 0,
                w: 2,
                h: 1,
                palette: &pal,
                img: &[RleElem { len: 2, color: 1 }, RleElem { len: 0, color: 0 }],
            })
            .expect("opaque fill should succeed");
        // Now draw an all-transparent (index 0) region over it.
        plane
            .draw(&DrawCommand {
                x: 0,
                y: 0,
                w: 2,
                h: 1,
                palette: &pal,
                img: &[RleElem { len: 2, color: 0 }, RleElem { len: 0, color: 0 }],
            })
            .expect("transparent draw should succeed");
        assert_eq!(
            plane.pixels()[3],
            255,
            "a transparent source pixel must not overwrite opaque content beneath it"
        );
    }

    #[test]
    fn draw_clips_pixels_outside_plane() {
        // A 4-wide bitmap drawn at x=2 on a 4-wide plane: only 2 px land.
        let pal = palette(&[(
            1,
            PaletteEntry {
                y: 235,
                cr: 128,
                cb: 128,
                t: 255,
            },
        )]);
        let img = [RleElem { len: 4, color: 1 }, RleElem { len: 0, color: 0 }];
        let mut plane = Plane::new(4, 1);
        plane
            .draw(&DrawCommand {
                x: 2,
                y: 0,
                w: 4,
                h: 1,
                palette: &pal,
                img: &img,
            })
            .expect("partially out-of-bounds draw should still succeed for the in-bounds part");
        let alpha = |x: u32| plane.pixels()[(x as usize) * 4 + 3];
        assert_eq!(
            alpha(0),
            0,
            "pixel left of the draw origin should be untouched"
        );
        assert_eq!(
            alpha(1),
            0,
            "pixel left of the draw origin should be untouched"
        );
        assert_ne!(alpha(2), 0, "pixel at the draw origin should be drawn");
        assert_ne!(
            alpha(3),
            0,
            "pixel within the clipped region should be drawn"
        );
    }

    #[test]
    fn draw_out_of_range_palette_index_errors() {
        // A run references index 5 but the palette is empty of meaning there.
        let pal = vec![
            PaletteEntry {
                y: 16,
                cr: 128,
                cb: 128,
                t: 0
            };
            2
        ];
        let img = [RleElem { len: 1, color: 5 }, RleElem { len: 0, color: 0 }];
        let mut plane = Plane::new(1, 1);
        let result = plane.draw(&DrawCommand {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
            palette: &pal,
            img: &img,
        });
        assert_eq!(
            result,
            Err(RleError::PaletteIndexOutOfRange { index: 5, len: 2 }),
            "an out-of-range palette index should produce a PaletteIndexOutOfRange error, got {result:?}"
        );
    }

    #[test]
    fn clear_resets_plane_to_transparent() {
        let pal = palette(&[(
            1,
            PaletteEntry {
                y: 235,
                cr: 128,
                cb: 128,
                t: 255,
            },
        )]);
        let mut plane = Plane::new(2, 1);
        plane
            .draw(&DrawCommand {
                x: 0,
                y: 0,
                w: 2,
                h: 1,
                palette: &pal,
                img: &[RleElem { len: 2, color: 1 }, RleElem { len: 0, color: 0 }],
            })
            .expect("fill should succeed");
        plane.clear();
        assert!(
            plane.pixels().iter().all(|&b| b == 0),
            "clear() should reset every byte of the plane to zero"
        );
    }

    #[test]
    fn wipe_clears_only_the_given_rectangle() {
        let pal = palette(&[(
            1,
            PaletteEntry {
                y: 235,
                cr: 128,
                cb: 128,
                t: 255,
            },
        )]);
        let mut plane = Plane::new(4, 1);
        plane
            .draw(&DrawCommand {
                x: 0,
                y: 0,
                w: 4,
                h: 1,
                palette: &pal,
                img: &[RleElem { len: 4, color: 1 }, RleElem { len: 0, color: 0 }],
            })
            .expect("fill should succeed");
        // Wipe the middle 2 px.
        plane.wipe(1, 0, 2, 1);
        let alpha = |x: u32| plane.pixels()[(x as usize) * 4 + 3];
        assert_ne!(alpha(0), 0, "pixel outside the wipe rect should remain");
        assert_eq!(alpha(1), 0, "pixel inside the wipe rect should be cleared");
        assert_eq!(alpha(2), 0, "pixel inside the wipe rect should be cleared");
        assert_ne!(alpha(3), 0, "pixel outside the wipe rect should remain");
    }

    #[test]
    fn wipe_clips_to_plane_bounds() {
        // A wipe rectangle extending past the plane must not panic or write OOB.
        let mut plane = Plane::new(2, 2);
        plane.wipe(1, 1, 100, 100);
        // Just reaching here without panicking is the assertion; verify the
        // in-bounds corner was cleared.
        let base = (plane.width() + 1) as usize * 4;
        assert_eq!(
            plane.pixels()[base + 3],
            0,
            "the in-bounds part of an oversized wipe should still be cleared"
        );
    }

    #[test]
    fn ctx_init_allocates_a_plane_and_snapshot_reports_size() {
        let ctx = OverlayCtx::new();
        assert!(
            ctx.snapshot().is_none(),
            "a fresh context should have no plane until INIT"
        );
        ctx.apply(&OverlayEvent::Init { w: 8, h: 4 })
            .expect("INIT should not error");
        let (rgba, w, h) = ctx.snapshot().expect("snapshot should exist after INIT");
        assert_eq!((w, h), (8, 4), "snapshot should report the INIT dimensions");
        assert_eq!(
            rgba.len(),
            8 * 4 * 4,
            "snapshot RGBA length should be width*height*4"
        );
    }

    #[test]
    fn ctx_flush_bumps_generation() {
        let ctx = OverlayCtx::new();
        let g0 = ctx.generation();
        ctx.apply(&OverlayEvent::Flush)
            .expect("FLUSH should not error");
        assert_eq!(
            ctx.generation(),
            g0 + 1,
            "FLUSH should increment the frame generation by one"
        );
    }

    #[test]
    fn ctx_close_drops_the_plane() {
        let ctx = OverlayCtx::new();
        ctx.apply(&OverlayEvent::Init { w: 2, h: 2 })
            .expect("INIT should not error");
        assert!(ctx.snapshot().is_some(), "plane should exist after INIT");
        ctx.apply(&OverlayEvent::Close)
            .expect("CLOSE should not error");
        assert!(
            ctx.snapshot().is_none(),
            "CLOSE should drop the plane so snapshot is None"
        );
    }

    #[test]
    fn ctx_draw_before_init_is_a_noop_not_a_panic() {
        // A DRAW with no plane allocated must be silently ignored, not panic.
        let ctx = OverlayCtx::new();
        let pal = vec![
            PaletteEntry {
                y: 235,
                cr: 128,
                cb: 128,
                t: 255
            };
            256
        ];
        let result = ctx.apply(&OverlayEvent::Draw {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
            palette: pal,
            img: vec![RleElem { len: 1, color: 0 }, RleElem { len: 0, color: 0 }],
        });
        assert!(
            result.is_ok(),
            "a DRAW with no plane should be a no-op, got {result:?}"
        );
    }

    #[test]
    fn ctx_draw_composites_into_the_plane() {
        let ctx = OverlayCtx::new();
        ctx.apply(&OverlayEvent::Init { w: 2, h: 1 })
            .expect("INIT should not error");
        let mut pal = vec![
            PaletteEntry {
                y: 16,
                cr: 128,
                cb: 128,
                t: 0
            };
            256
        ];
        pal[1] = PaletteEntry {
            y: 235,
            cr: 128,
            cb: 128,
            t: 255,
        };
        ctx.apply(&OverlayEvent::Draw {
            x: 0,
            y: 0,
            w: 2,
            h: 1,
            palette: pal,
            img: vec![RleElem { len: 2, color: 1 }, RleElem { len: 0, color: 0 }],
        })
        .expect("DRAW should not error");
        let (rgba, _, _) = ctx.snapshot().expect("plane should exist");
        assert_eq!(
            rgba[3], 255,
            "the composited opaque pixel should be visible in the snapshot"
        );
    }

    #[test]
    fn lone_draw_records_the_selected_button_rect() {
        // Init -> single Draw -> Flush is the clean lone-draw: that draw's rect
        // becomes the selected-button rect.
        let ctx = OverlayCtx::new();
        assert!(
            ctx.selected_button_rect().is_none(),
            "a fresh context has no selected-button rect"
        );
        ctx.apply(&OverlayEvent::Init { w: 100, h: 100 })
            .expect("INIT should not error");
        ctx.apply(&draw_event(10, 20, 30, 8))
            .expect("DRAW should not error");
        ctx.apply(&OverlayEvent::Flush)
            .expect("FLUSH should not error");
        assert_eq!(
            ctx.selected_button_rect(),
            Some(Rect {
                x: 10,
                y: 20,
                w: 30,
                h: 8
            }),
            "a FLUSH preceded by exactly one DRAW should record that draw's rect"
        );
    }

    #[test]
    fn two_draws_before_flush_is_not_isolable() {
        // The top-menu deselect-old + select-new pattern: two draws before the
        // flush. Not a clean lone-draw, so the rect stays None (no prior value).
        let ctx = OverlayCtx::new();
        ctx.apply(&OverlayEvent::Init { w: 100, h: 100 })
            .expect("INIT should not error");
        ctx.apply(&draw_event(10, 20, 30, 8))
            .expect("first DRAW should not error");
        ctx.apply(&draw_event(10, 40, 30, 8))
            .expect("second DRAW should not error");
        ctx.apply(&OverlayEvent::Flush)
            .expect("FLUSH should not error");
        assert!(
            ctx.selected_button_rect().is_none(),
            "a two-draw frame is not a clean lone-draw, so no rect should be recorded"
        );
    }

    #[test]
    fn many_draw_redraw_leaves_the_rect_unchanged() {
        // First establish a rect via a lone-draw, then a full page redraw (many
        // draws) must NOT overwrite it — only a clean lone-draw updates it.
        let ctx = OverlayCtx::new();
        ctx.apply(&OverlayEvent::Init { w: 100, h: 100 })
            .expect("INIT should not error");
        ctx.apply(&draw_event(1, 2, 3, 4))
            .expect("DRAW should not error");
        ctx.apply(&OverlayEvent::Flush)
            .expect("FLUSH should not error");
        let established = ctx.selected_button_rect();
        assert!(
            established.is_some(),
            "a lone-draw should establish a rect first"
        );

        // Now a five-draw frame (a page redraw). Note: no CLEAR here, so the
        // prior rect is what must be preserved.
        for i in 0..5u16 {
            ctx.apply(&draw_event(i, i, 5, 5))
                .expect("redraw DRAW should not error");
        }
        ctx.apply(&OverlayEvent::Flush)
            .expect("FLUSH should not error");
        assert_eq!(
            ctx.selected_button_rect(),
            established,
            "a many-draw page redraw must leave the recorded lone-draw rect unchanged"
        );
    }

    #[test]
    fn last_clean_lone_draw_wins_across_frames() {
        // Multiple lone-draw frames: the most recent one is the recorded rect.
        let ctx = OverlayCtx::new();
        ctx.apply(&OverlayEvent::Init { w: 200, h: 200 })
            .expect("INIT should not error");
        ctx.apply(&draw_event(5, 5, 10, 10))
            .expect("DRAW should not error");
        ctx.apply(&OverlayEvent::Flush)
            .expect("FLUSH should not error");
        ctx.apply(&draw_event(50, 60, 20, 12))
            .expect("DRAW should not error");
        ctx.apply(&OverlayEvent::Flush)
            .expect("FLUSH should not error");
        assert_eq!(
            ctx.selected_button_rect(),
            Some(Rect {
                x: 50,
                y: 60,
                w: 20,
                h: 12
            }),
            "the last clean lone-draw should win"
        );
    }

    #[test]
    fn clear_invalidates_the_recorded_rect() {
        // Establish a rect, then a CLEAR (preceding a page redraw) must reset it
        // to None so it never points at a stale page.
        let ctx = OverlayCtx::new();
        ctx.apply(&OverlayEvent::Init { w: 100, h: 100 })
            .expect("INIT should not error");
        ctx.apply(&draw_event(10, 20, 30, 8))
            .expect("DRAW should not error");
        ctx.apply(&OverlayEvent::Flush)
            .expect("FLUSH should not error");
        assert!(
            ctx.selected_button_rect().is_some(),
            "a lone-draw should establish a rect first"
        );
        ctx.apply(&OverlayEvent::Clear)
            .expect("CLEAR should not error");
        assert!(
            ctx.selected_button_rect().is_none(),
            "a CLEAR should invalidate the recorded rect to avoid a stale page reference"
        );
    }

    #[test]
    fn flush_resets_draw_count_between_frames() {
        // A single draw across two separate frames each counts as a lone-draw —
        // the per-frame counter must reset at each FLUSH. Frame 1: one draw.
        // Frame 2: one draw at a new rect. The second frame must still register
        // as lone (proving the counter reset rather than accumulating to two).
        let ctx = OverlayCtx::new();
        ctx.apply(&OverlayEvent::Init { w: 100, h: 100 })
            .expect("INIT should not error");
        ctx.apply(&draw_event(1, 1, 4, 4))
            .expect("DRAW should not error");
        ctx.apply(&OverlayEvent::Flush)
            .expect("FLUSH should not error");
        ctx.apply(&draw_event(9, 9, 4, 4))
            .expect("DRAW should not error");
        ctx.apply(&OverlayEvent::Flush)
            .expect("FLUSH should not error");
        assert_eq!(
            ctx.selected_button_rect(),
            Some(Rect {
                x: 9,
                y: 9,
                w: 4,
                h: 4
            }),
            "the second single-draw frame must register as a lone-draw, proving the counter reset"
        );
    }

    #[test]
    fn crop_extracts_the_expected_subrect() {
        // A 4x4 plane with a unique value per pixel (encoded into the R channel)
        // cropped to the inner 2x2 at (1,1) should yield exactly those pixels.
        let plane_w = 4u32;
        let plane_h = 4u32;
        let mut rgba = vec![0u8; (plane_w * plane_h * 4) as usize];
        for y in 0..plane_h {
            for x in 0..plane_w {
                let base = ((y * plane_w + x) * 4) as usize;
                // Encode position so we can assert the exact pixels copied.
                let pos = u8::try_from(y * plane_w + x).expect("position fits in u8");
                rgba[base] = pos; // R
                rgba[base + 3] = 255; // A
            }
        }
        let (out, w, h) = crop_rgba(
            &rgba,
            plane_w,
            plane_h,
            Rect {
                x: 1,
                y: 1,
                w: 2,
                h: 2,
            },
        )
        .expect("an in-bounds crop should succeed");
        assert_eq!((w, h), (2, 2), "crop dimensions should match the rect");
        // Expected R values: positions (1,1)=5, (2,1)=6, (1,2)=9, (2,2)=10.
        assert_eq!(out[0], 5, "top-left of crop should be plane position 5");
        assert_eq!(out[4], 6, "top-right of crop should be plane position 6");
        assert_eq!(out[8], 9, "bottom-left of crop should be plane position 9");
        assert_eq!(
            out[12], 10,
            "bottom-right of crop should be plane position 10"
        );
    }

    #[test]
    fn crop_clips_a_rect_running_off_the_edge() {
        // A 3x3 plane; a rect at (2,2) with size 4x4 overruns the right/bottom
        // edges. It must clip to the in-bounds 1x1 corner, not panic.
        let plane_w = 3u32;
        let plane_h = 3u32;
        let rgba = vec![200u8; (plane_w * plane_h * 4) as usize];
        let (out, w, h) = crop_rgba(
            &rgba,
            plane_w,
            plane_h,
            Rect {
                x: 2,
                y: 2,
                w: 4,
                h: 4,
            },
        )
        .expect("an overrunning crop should clip rather than fail");
        assert_eq!(
            (w, h),
            (1, 1),
            "an overrunning rect should clip to the in-bounds portion"
        );
        assert_eq!(out.len(), 4, "a 1x1 crop should be exactly 4 RGBA bytes");
    }

    #[test]
    fn crop_zero_area_rect_yields_none() {
        let rgba = vec![0u8; 4 * 4 * 4];
        let none = crop_rgba(
            &rgba,
            4,
            4,
            Rect {
                x: 1,
                y: 1,
                w: 0,
                h: 0,
            },
        );
        assert!(none.is_none(), "a zero-area rect should yield None");
    }

    #[test]
    fn crop_fully_out_of_bounds_rect_yields_none() {
        let rgba = vec![0u8; 4 * 4 * 4];
        let none = crop_rgba(
            &rgba,
            4,
            4,
            Rect {
                x: 10,
                y: 10,
                w: 2,
                h: 2,
            },
        );
        assert!(
            none.is_none(),
            "a rect entirely outside the plane should yield None"
        );
    }

    #[test]
    fn cropped_selection_carves_the_lone_draw_region() {
        // End-to-end on the context: init a plane, paint an opaque button via a
        // real DRAW, flush (lone-draw), and confirm cropped_selection returns a
        // tight buffer matching the draw's rect.
        let ctx = OverlayCtx::new();
        ctx.apply(&OverlayEvent::Init { w: 8, h: 8 })
            .expect("INIT should not error");
        let mut pal = vec![
            PaletteEntry {
                y: 16,
                cr: 128,
                cb: 128,
                t: 0
            };
            256
        ];
        pal[1] = PaletteEntry {
            y: 235,
            cr: 128,
            cb: 128,
            t: 255,
        };
        // A 2x2 opaque region at (3,3).
        ctx.apply(&OverlayEvent::Draw {
            x: 3,
            y: 3,
            w: 2,
            h: 2,
            palette: pal,
            img: vec![
                RleElem { len: 2, color: 1 },
                RleElem { len: 0, color: 0 },
                RleElem { len: 2, color: 1 },
                RleElem { len: 0, color: 0 },
            ],
        })
        .expect("DRAW should not error");
        ctx.apply(&OverlayEvent::Flush)
            .expect("FLUSH should not error");
        let (out, w, h) = ctx
            .cropped_selection()
            .expect("a lone-draw should make a crop available");
        assert_eq!(
            (w, h),
            (2, 2),
            "the crop should be the size of the lone-draw rect"
        );
        assert_eq!(
            out.len(),
            2 * 2 * 4,
            "the crop buffer should be width*height*4 bytes"
        );
        assert_eq!(
            out[3], 255,
            "the cropped region should contain the opaque drawn pixel"
        );
    }
}
