// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! The eframe/egui menu-navigator application.
//!
//! ## Threading model
//!
//! `reliquary_bluray::Disc` is intentionally `!Send`/`!Sync`: libbluray is a
//! single-threaded, in-process FFI library whose overlay/event callbacks fire
//! synchronously on the thread that calls `bd_*`. We therefore **own the
//! `Disc` directly inside this eframe `App`**, on the UI thread, and pump it
//! from inside [`eframe::App::update`] each frame — the handle never crosses a
//! thread boundary, so its `!Send` bound is honoured by construction rather
//! than worked around. A bounded number of `bd_read_ext` iterations per frame
//! keeps the UI responsive while letting navigation advance and overlay
//! callbacks composite the menu. The latest composited ARGB frame is uploaded
//! as an egui texture only when the disc reports a new overlay generation, so
//! an idle menu costs no texture churn.

use eframe::egui;
use reliquary_bluray::{Disc, Key, PumpEvent, Rect};

/// How many `bd_read_ext` pumps to run per UI frame. Enough to drain a burst of
/// overlay commands after a key press without starving the repaint; the loop
/// stops early when there is nothing to do.
const PUMP_ITERATIONS_PER_FRAME: u32 = 64;

/// The menu-navigator application: owns the disc, pumps it each frame, and
/// shows the composited overlay as a fullscreen image.
pub struct NavApp {
    /// The libbluray disc handle, driven entirely on this (UI) thread.
    disc: Disc,
    /// The uploaded overlay texture, re-uploaded only when the frame generation
    /// changes. `None` until the first menu overlay has been composited.
    texture: Option<egui::TextureHandle>,
    /// The overlay generation last uploaded into [`Self::texture`]. Used to skip
    /// redundant uploads of an unchanged menu image.
    last_generation: u64,
    /// Set once we have observed at least one composited frame, so the UI can
    /// distinguish "still loading the menu" from "menu is genuinely empty".
    seen_frame: bool,
    /// The plane dimensions of the last uploaded frame, used to map the
    /// selected-button rect (in plane coordinates) onto the on-screen image.
    frame_size: Option<[f32; 2]>,
}

impl NavApp {
    /// Build the app around an already-opened, already-`play()`ed [`Disc`].
    #[must_use]
    pub const fn new(disc: Disc) -> Self {
        Self {
            disc,
            texture: None,
            last_generation: 0,
            seen_frame: false,
            frame_size: None,
        }
    }

    /// Translate egui key events into disc navigation keys and forward them.
    fn handle_input(&mut self, ctx: &egui::Context) {
        // Map the arrow keys + Enter; collect first to avoid borrowing input
        // across the `send_key` calls.
        let keys: Vec<Key> = ctx.input(|i| {
            let mut out = Vec::new();
            for (egui_key, nav_key) in [
                (egui::Key::ArrowUp, Key::Up),
                (egui::Key::ArrowDown, Key::Down),
                (egui::Key::ArrowLeft, Key::Left),
                (egui::Key::ArrowRight, Key::Right),
                (egui::Key::Enter, Key::Enter),
            ] {
                if i.key_pressed(egui_key) {
                    out.push(nav_key);
                }
            }
            out
        });
        for key in keys {
            let changed = self.disc.send_key(key);
            tracing::debug!(?key, changed, "sent navigation key to disc");
        }
    }

    /// Upload the latest composited frame as a texture if the disc reports a
    /// newer generation than we last uploaded.
    fn refresh_texture(&mut self, ctx: &egui::Context) {
        let generation = self.disc.frame_generation();
        if self.texture.is_some() && generation == self.last_generation {
            // Nothing new since the last upload.
            return;
        }
        let Some((rgba, width, height)) = self.disc.frame() else {
            // No overlay plane initialised yet.
            return;
        };
        if width == 0 || height == 0 {
            return;
        }
        let image =
            egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &rgba);
        let options = egui::TextureOptions::LINEAR;
        match &mut self.texture {
            Some(handle) => handle.set(image, options),
            None => {
                self.texture = Some(ctx.load_texture("menu-overlay", image, options));
            }
        }
        self.last_generation = generation;
        self.seen_frame = true;
        #[allow(
            clippy::cast_precision_loss,
            reason = "plane dimensions are video resolutions (<= a few thousand px), well within f32's exact-integer range"
        )]
        {
            self.frame_size = Some([width as f32, height as f32]);
        }
    }

    /// Draw the selected-button rect's outline over the displayed menu image, so
    /// the lone-draw signal can be confirmed by eye against the highlighted
    /// button. `image_rect` is the on-screen rectangle the menu image was
    /// painted into; the button rect (in plane coordinates) is mapped into it.
    /// A no-op until both a frame and a clean lone-draw rect are available.
    fn draw_selection_outline(&self, ui: &egui::Ui, image_rect: egui::Rect) {
        let Some([plane_w, plane_h]) = self.frame_size else {
            return;
        };
        if plane_w <= 0.0 || plane_h <= 0.0 {
            return;
        }
        let Some(Rect { x, y, w, h }) = self.disc.selected_button_rect() else {
            return;
        };
        // Map plane coordinates into the painted image's screen rectangle. The
        // image preserves aspect ratio, so x and y share a single scale factor.
        let scale = (image_rect.width() / plane_w).min(image_rect.height() / plane_h);
        let painted_w = plane_w * scale;
        let painted_h = plane_h * scale;
        // The image is centred within `image_rect`; recover its top-left origin.
        let origin = egui::pos2(
            image_rect.center().x - painted_w / 2.0,
            image_rect.center().y - painted_h / 2.0,
        );
        let rect = egui::Rect::from_min_size(
            egui::pos2(
                f32::from(x).mul_add(scale, origin.x),
                f32::from(y).mul_add(scale, origin.y),
            ),
            egui::vec2(f32::from(w) * scale, f32::from(h) * scale),
        );
        ui.painter().rect_stroke(
            rect,
            0.0,
            egui::Stroke::new(2.0, egui::Color32::from_rgb(0, 200, 255)),
            egui::StrokeKind::Outside,
        );
    }
}

impl eframe::App for NavApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 1. Forward any key presses to the disc.
        self.handle_input(ctx);

        // 2. Advance navigation / fire overlay callbacks a bounded amount.
        for event in self.disc.pump(PUMP_ITERATIONS_PER_FRAME) {
            match event {
                PumpEvent::Error => {
                    tracing::error!("libbluray reported a fatal playback error");
                }
                other => tracing::debug!(?other, "pump event"),
            }
        }

        // 3. Upload the composited menu if it changed.
        self.refresh_texture(ctx);

        // 4. Draw the fullscreen menu image (or a placeholder while it loads).
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(egui::Color32::BLACK))
            .show(ctx, |ui| {
                if let Some(texture) = &self.texture {
                    let available = ui.available_size();
                    let response = ui
                        .centered_and_justified(|ui| {
                            ui.add(
                                egui::Image::new(texture)
                                    .maintain_aspect_ratio(true)
                                    .fit_to_exact_size(available),
                            )
                        })
                        .inner;
                    // Outline the selected button (lone-draw rect) over the menu
                    // image as a visual confirmation of the lone-draw signal.
                    self.draw_selection_outline(ui, response.rect);
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new("Loading disc menu…").color(egui::Color32::GRAY),
                        );
                    });
                }
            });

        // 5. Keep the loop live: menus animate and we must keep pumping even
        // without user input, so request another repaint unconditionally.
        ctx.request_repaint();
    }
}
