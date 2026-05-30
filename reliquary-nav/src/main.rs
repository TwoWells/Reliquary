// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Interactive disc menu navigator.
//!
//! Opens a window rendering a Blu-ray IG menu page at full resolution.
//! Mouse hover highlights buttons by swapping their bitmap state (normal ↔
//! selected). Validates the compositing pipeline and button bounding boxes.

mod pipeline;

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

use reliquary::disc::bdmv::compose::composite_page;

use crate::pipeline::LoadedPage;

/// Interactive disc menu navigator — visual validation of IG compositing.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Path to disc ISO or extracted directory.
    path: PathBuf,

    /// IG clip index (0-based). Defaults to the first IG clip.
    #[arg(long, default_value_t = 0)]
    clip: usize,

    /// Page index within the clip (0-based). Defaults to the first page.
    #[arg(long, default_value_t = 0)]
    page: usize,

    /// VUK hex string for AACS-encrypted discs.
    #[arg(long)]
    vuk: Option<String>,
}

#[allow(
    clippy::print_stderr,
    reason = "binary crate uses stderr for error reporting"
)]
fn main() -> ExitCode {
    let cli = Cli::parse();

    let vuk = match cli.vuk.as_deref().map(parse_vuk) {
        Some(Ok(v)) => Some(v),
        Some(Err(e)) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
        None => None,
    };

    let loaded = match pipeline::load_page(&cli.path, cli.clip, cli.page, vuk.as_ref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let event_loop = match EventLoop::new() {
        Ok(el) => el,
        Err(e) => {
            eprintln!("error: failed to create event loop: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut app = App::new(loaded);
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("error: event loop failed: {e}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

/// Parses a hex string into a 16-byte VUK.
fn parse_vuk(s: &str) -> Result<[u8; 16], String> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if s.len() != 32 {
        return Err(format!("VUK must be 32 hex characters, got {}", s.len()));
    }
    let mut key = [0u8; 16];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|_| format!("invalid hex at position {}", i * 2))?;
    }
    Ok(key)
}

// ── Application state ──────────────────────────────────────────────────

struct App {
    loaded: LoadedPage,
    state: Option<WindowState>,
}

struct WindowState {
    /// Window handle — kept alive for the surface and used for redraw requests.
    window: Arc<Window>,
    surface: softbuffer::Surface<Arc<Window>, Arc<Window>>,
    /// Currently highlighted button ID (None = no highlight).
    highlight: Option<u16>,
}

impl App {
    const fn new(loaded: LoadedPage) -> Self {
        Self {
            loaded,
            state: None,
        }
    }

    #[allow(
        clippy::many_single_char_names,
        reason = "r/g/b and w/h are conventional and clear in a pixel-blitting loop"
    )]
    fn render(&mut self) {
        let Some(state) = &mut self.state else {
            return;
        };

        let canvas = composite_page(&self.loaded.page, state.highlight, self.loaded.background());

        let width = u32::from(self.loaded.page.canvas_width);
        let height = u32::from(self.loaded.page.canvas_height);

        let _ = state.surface.resize(
            NonZeroU32::new(width).unwrap_or(NonZeroU32::MIN),
            NonZeroU32::new(height).unwrap_or(NonZeroU32::MIN),
        );

        if let Ok(mut buffer) = state.surface.buffer_mut() {
            for (i, pixel) in buffer.iter_mut().enumerate() {
                let off = i * 4;
                let r = u32::from(canvas[off]);
                let g = u32::from(canvas[off + 1]);
                let b = u32::from(canvas[off + 2]);
                *pixel = (r << 16) | (g << 8) | b;
            }
            let _ = buffer.present();
        }
    }

    /// Hit-test: find which button (if any) the cursor is over.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "cursor coordinates clamped to u16 canvas bounds"
    )]
    #[allow(
        clippy::cast_sign_loss,
        reason = "negative cursor values become 0 via saturating conversion"
    )]
    fn hit_test(&self, cursor_x: f64, cursor_y: f64) -> Option<u16> {
        let px = if cursor_x < 0.0 {
            0u16
        } else if cursor_x > f64::from(u16::MAX) {
            u16::MAX
        } else {
            cursor_x as u16
        };
        let py = if cursor_y < 0.0 {
            0u16
        } else if cursor_y > f64::from(u16::MAX) {
            u16::MAX
        } else {
            cursor_y as u16
        };

        for btn in &self.loaded.page.buttons {
            let Some(bmp) = btn.normal.as_ref().or(btn.selected.as_ref()) else {
                continue;
            };
            let bx = btn.x;
            let by = btn.y;
            let bw = bmp.width;
            let bh = bmp.height;

            if px >= bx && px < bx.saturating_add(bw) && py >= by && py < by.saturating_add(bh) {
                return Some(btn.button_id);
            }
        }
        None
    }
}

#[allow(
    clippy::print_stderr,
    reason = "binary crate uses stderr for error reporting"
)]
impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }

        let width = u32::from(self.loaded.page.canvas_width);
        let height = u32::from(self.loaded.page.canvas_height);

        let attrs = Window::default_attributes()
            .with_title("Reliquary Navigator")
            .with_inner_size(LogicalSize::new(width, height))
            .with_resizable(false);

        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("error: failed to create window: {e}");
                event_loop.exit();
                return;
            }
        };

        let context = match softbuffer::Context::new(Arc::clone(&window)) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: failed to create softbuffer context: {e}");
                event_loop.exit();
                return;
            }
        };

        let surface = match softbuffer::Surface::new(&context, Arc::clone(&window)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: failed to create surface: {e}");
                event_loop.exit();
                return;
            }
        };

        self.state = Some(WindowState {
            window,
            surface,
            highlight: None,
        });

        self.render();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }
            WindowEvent::CursorMoved {
                position: PhysicalPosition { x, y },
                ..
            } => {
                let new_highlight = self.hit_test(x, y);
                if let Some(state) = &mut self.state
                    && new_highlight != state.highlight
                {
                    state.highlight = new_highlight;
                    state.window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                self.render();
            }
            _ => {}
        }
    }
}
