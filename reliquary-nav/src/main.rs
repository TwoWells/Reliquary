// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Interactive disc menu navigator.
//!
//! Opens a window rendering a Blu-ray IG menu page at full resolution.
//! Mouse hover highlights buttons by swapping their bitmap state (normal ↔
//! selected). Clicking a button executes its HDMV commands and navigates
//! via direct page change, MOBJ dispatch, or `GotoMobj` chain following.

mod pipeline;

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

use reliquary::disc::bdmv::compose::composite_page;
use reliquary::disc::bdmv::mobj::PSR_FLAG;

use crate::pipeline::{
    ButtonEffect, DiscState, LoadedClip, LoadedPage, PlayTarget, PlayerContext,
    execute_button_commands, load_clip, load_disc, run_mobj_chain,
};

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

    let disc = match load_disc(&cli.path, vuk) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let clip = match load_clip(&disc, cli.clip) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let Some(loaded) = clip.build_page(cli.page) else {
        eprintln!(
            "error: page index {} out of range (have {} pages)",
            cli.page,
            clip.page_count()
        );
        return ExitCode::FAILURE;
    };

    let event_loop = match EventLoop::new() {
        Ok(el) => el,
        Err(e) => {
            eprintln!("error: failed to create event loop: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut app = App::new(disc, clip, loaded);
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
    disc: DiscState,
    clip: LoadedClip,
    loaded: LoadedPage,
    state: Option<WindowState>,
    /// Persistent GPR state across button clicks and MOBJ dispatch.
    gprs: HashMap<u32, u32>,
    /// Navigation history for back button (right-click / Escape).
    history: Vec<HistoryEntry>,
}

/// Saved state for back navigation.
struct HistoryEntry {
    clip_index: usize,
    page_id: u8,
    gprs: HashMap<u32, u32>,
}

struct WindowState {
    /// Window handle — kept alive for the surface and used for redraw requests.
    window: Arc<Window>,
    surface: softbuffer::Surface<Arc<Window>, Arc<Window>>,
    /// Currently highlighted button ID (None = no highlight).
    highlight: Option<u16>,
}

impl App {
    fn new(disc: DiscState, clip: LoadedClip, loaded: LoadedPage) -> Self {
        let gprs = disc.init_gprs.clone();
        Self {
            disc,
            clip,
            loaded,
            state: None,
            gprs,
            history: Vec::new(),
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

        let canvas = composite_page(&self.loaded.page, state.highlight, self.clip.background());

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

    // ── Click handling ───────────────────────────────────────────────

    /// Handles a button click — executes commands, dispatches to the
    /// appropriate navigation path based on the terminal effect.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "page IDs are u8 values stored in u32 SetButtonPage fields"
    )]
    #[allow(
        clippy::print_stderr,
        reason = "binary crate uses stderr for debug trace"
    )]
    fn handle_click(&mut self, button_id: u16) {
        let Some(commands) = self.loaded.commands_for_button(button_id) else {
            return;
        };

        let ctx = PlayerContext {
            selected_button_id: button_id,
            page_id: self.loaded.page.page_id,
            ..PlayerContext::default()
        };
        let (effect, new_gprs) = execute_button_commands(commands, &ctx, &self.gprs);
        self.gprs = new_gprs;

        eprintln!(
            "click: button_id={button_id} page_id={} effect={effect:?}",
            self.loaded.page.page_id
        );

        match effect {
            ButtonEffect::SetButtonPage { composite, page } => {
                self.handle_set_button_page(composite, page);
            }
            ButtonEffect::GotoMobj(object_id) => {
                self.handle_goto_mobj(object_id);
            }
            ButtonEffect::Playlist {
                playlist,
                branch_opt,
                mark_or_pi,
            } => {
                self.handle_play_target(PlayTarget {
                    playlist,
                    branch_opt,
                    mark_or_pi,
                });
            }
            ButtonEffect::None => {}
        }
    }

    /// Handles `SET_BUTTON_PAGE` — composite button selection, direct
    /// page navigation, or MOBJ dispatch fallback.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "page IDs and button IDs are u8/u16 values in u32 fields"
    )]
    #[allow(
        clippy::print_stderr,
        reason = "binary crate uses stderr for debug trace"
    )]
    fn handle_set_button_page(&mut self, composite: u32, page: u32) {
        let composite_button_id = if composite > 0 {
            Some((composite & 0xFFFF) as u16)
        } else {
            None
        };

        // Direct page navigation: if the target page exists, navigate
        // there with optional button selection from composite.
        // Page doesn't exist → fall through to button selection.
        // WB uses invalid page IDs (e.g. 120) as a signal that the
        // composite selects a button on the CURRENT page.
        let page_id = (page & 0xFF) as u8;
        if page_id > 0
            && let Some(page_index) = self.clip.page_index_for_id(page_id)
        {
            self.push_history();
            self.navigate_to_page_index(page_index, composite_button_id);
            return;
        }

        // Button selection: composite > 0 selects a button on the
        // current page. If that button has auto_action, activate it
        // immediately (this is how WB navigates to submenu pages).
        if let Some(btn_id) = composite_button_id
            && let Some(idx) = self
                .loaded
                .page
                .buttons
                .iter()
                .position(|b| b.button_id == btn_id)
        {
            if let Some(state) = &mut self.state {
                state.highlight = Some(btn_id);
                state.window.request_redraw();
            }
            // If this button has auto_action, activate it.
            if self.loaded.auto_action.get(idx).copied().unwrap_or(false) {
                self.handle_click(btn_id);
            }
            return;
        }

        // MOBJ dispatch fallback: resume the title MOBJ with updated
        // PSR[10]/PSR[11]. Any non-zero playlist terminates the chain
        // so we can distinguish content playback from menu navigation.
        // Previously this used menu_playlists as the valid set, which
        // caused content PlayPl instructions to be skipped — the chain
        // would continue to the menu-return PlayPl and navigate to
        // page 0 instead of logging the content playlist.
        if let Some((mobj_idx, resume_pc)) = self.disc.resume_point {
            self.gprs.insert(PSR_FLAG | 0x0A, composite); // PSR[10]
            self.gprs.insert(PSR_FLAG | 0x0B, page); // PSR[11]

            let any_playlist = std::collections::HashSet::new();
            let target = run_mobj_chain(
                &self.disc.mobj_file,
                mobj_idx,
                resume_pc,
                &mut self.gprs,
                &self.disc.title_to_mobj,
                &any_playlist,
            );

            if let Some(play_target) = target {
                eprintln!(
                    "mobj_dispatch: psr10={composite:#x} psr11={page:#x} → playlist {}",
                    play_target.playlist
                );
                self.handle_play_target(play_target);
            }
        }
    }

    /// Handles `GotoMobj` — executes the target MOBJ chain to reach a
    /// `PlayPl` and acts on the result.
    #[allow(
        clippy::print_stderr,
        reason = "binary crate uses stderr for debug trace"
    )]
    fn handle_goto_mobj(&mut self, object_id: u32) {
        // Empty valid set = any non-zero PlayPl is terminal.
        let any_playlist = std::collections::HashSet::new();
        let target = run_mobj_chain(
            &self.disc.mobj_file,
            object_id as usize,
            0,
            &mut self.gprs,
            &self.disc.title_to_mobj,
            &any_playlist,
        );

        if let Some(play_target) = target {
            eprintln!(
                "goto_mobj: object={object_id} → playlist {}",
                play_target.playlist
            );
            self.handle_play_target(play_target);
        }
    }

    /// Handles a `PlayTarget` from MOBJ dispatch, `GotoMobj`, or direct
    /// `PlayPl` button effects.
    ///
    /// If the playlist is a menu playlist, navigates using PSR\[11\] page
    /// (set by the MOBJ chain's `SET_BUTTON_PAGE` before `PlayPl`) with
    /// page 0 + auto-action as fallback. Otherwise, logs the content
    /// playlist.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "page IDs are u8 values stored in u32 PSR fields"
    )]
    #[allow(
        clippy::print_stderr,
        reason = "binary crate uses stderr for status messages"
    )]
    fn handle_play_target(&mut self, target: PlayTarget) {
        if self
            .disc
            .menu_playlists
            .contains(&u32::from(target.playlist))
        {
            // Menu playlist — navigate using PSR[11] page if the MOBJ
            // chain set it, otherwise fall back to page 0 + auto-action.
            let psr11 = self.gprs.get(&(PSR_FLAG | 0x0B)).copied().unwrap_or(0);
            let target_page = (psr11 & 0xFF) as u8;
            self.push_history();
            if target_page > 0
                && let Some(page_index) = self.clip.page_index_for_id(target_page)
            {
                self.navigate_to_page_index(page_index, None);
            } else {
                self.navigate_to_page_index(0, None);
            }
        } else {
            // Content playlist — log it.
            eprintln!(
                "content: playlist {} (branch_opt={}, mark_or_pi={})",
                target.playlist, target.branch_opt, target.mark_or_pi
            );
        }
    }

    // ── Navigation helpers ───────────────────────────────────────────

    /// Navigates to a page by index, optionally selecting a specific button.
    ///
    /// After loading the page, checks for auto-action buttons and executes
    /// them — this handles the WB page-0 bootstrap pattern where an
    /// auto-action button reads GPR state and navigates to the real page.
    fn navigate_to_page_index(&mut self, page_index: usize, initial_highlight: Option<u16>) {
        let Some(new_loaded) = self.clip.build_page(page_index) else {
            return;
        };

        self.loaded = new_loaded;

        if let Some(state) = &mut self.state {
            state.highlight = initial_highlight;
            state.window.request_redraw();
        }

        // Auto-action: if this page has a default_activated_button_id,
        // execute that button's commands immediately.
        self.run_auto_action(0);
    }

    /// Executes auto-action buttons on the current page.
    ///
    /// Two BD spec paths for auto-activation:
    /// 1. `default_activated_button_id` — page directly activates a button.
    /// 2. `default_selected_button_id` + per-button `auto_action` flag —
    ///    page selects a button, and that button's `auto_action` flag
    ///    causes it to activate immediately when selected.
    ///
    /// Both paths are used for bootstrap pages (e.g. WB page 0) that
    /// read GPR state and navigate to the correct submenu page.
    ///
    /// `depth` limits recursion — auto-action on the target page may
    /// chain to another auto-action, but we cap at 4 hops.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "page IDs are u8 values stored in u32 SetButtonPage fields"
    )]
    fn run_auto_action(&mut self, depth: u8) {
        if depth >= 4 {
            return;
        }
        // Path 1: page-level direct activation
        let mut activated_id = self.loaded.default_activated_button_id;

        // Path 2: default-selected button with auto_action flag
        if activated_id == 0xFFFF {
            let selected_id = self.loaded.default_selected_button_id;
            if selected_id != 0xFFFF
                && let Some(idx) = self
                    .loaded
                    .page
                    .buttons
                    .iter()
                    .position(|b| b.button_id == selected_id)
                && self.loaded.auto_action.get(idx).copied().unwrap_or(false)
            {
                activated_id = selected_id;
            }
        }

        if activated_id == 0xFFFF {
            return;
        }

        let Some(commands) = self.loaded.commands_for_button(activated_id) else {
            return;
        };

        let ctx = PlayerContext {
            selected_button_id: activated_id,
            page_id: self.loaded.page.page_id,
            ..PlayerContext::default()
        };
        // Clone commands before executing to avoid borrow conflict.
        let commands_owned: Vec<_> = commands.to_vec();
        let (effect, new_gprs) = execute_button_commands(&commands_owned, &ctx, &self.gprs);
        self.gprs = new_gprs;

        // Auto-action typically produces SetButtonPage to the real page.
        if let ButtonEffect::SetButtonPage { page, composite } = effect {
            let page_id = (page & 0xFF) as u8;
            if page_id > 0
                && let Some(page_index) = self.clip.page_index_for_id(page_id)
            {
                let highlight = if composite > 0 {
                    Some((composite & 0xFFFF) as u16)
                } else {
                    None
                };
                let Some(new_loaded) = self.clip.build_page(page_index) else {
                    return;
                };
                self.loaded = new_loaded;
                if let Some(state) = &mut self.state {
                    state.highlight = highlight;
                    state.window.request_redraw();
                }
                // Recursive auto-action on the target page
                self.run_auto_action(depth + 1);
            }
        }
    }

    /// Pushes the current state onto the history stack.
    fn push_history(&mut self) {
        self.history.push(HistoryEntry {
            clip_index: self.clip.clip_index(),
            page_id: self.loaded.page.page_id,
            gprs: self.gprs.clone(),
        });
    }

    /// Pops the history stack and navigates back.
    fn navigate_back(&mut self) {
        let Some(entry) = self.history.pop() else {
            return;
        };

        self.gprs = entry.gprs;

        // Same clip — just switch pages.
        if entry.clip_index == self.clip.clip_index() {
            if let Some(page_index) = self.clip.page_index_for_id(entry.page_id)
                && let Some(new_loaded) = self.clip.build_page(page_index)
            {
                self.loaded = new_loaded;
                if let Some(state) = &mut self.state {
                    state.highlight = None;
                    state.window.request_redraw();
                }
            }
            return;
        }

        // Different clip — reload.
        if let Ok(new_clip) = load_clip(&self.disc, entry.clip_index)
            && let Some(page_index) = new_clip.page_index_for_id(entry.page_id)
            && let Some(new_loaded) = new_clip.build_page(page_index)
        {
            self.clip = new_clip;
            self.loaded = new_loaded;
            if let Some(state) = &mut self.state {
                state.highlight = None;
                state.window.request_redraw();
            }
        }
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

        // Run auto-action on the initial page. On WB, page 0 is a
        // bootstrap stub whose auto-action button reads GPR state and
        // navigates to the main menu page.
        let page_before = self.loaded.page.page_id;
        self.run_auto_action(0);

        // Bootstrap fallback: if auto-action didn't navigate away from
        // page 0 (GPR lifecycle state not yet populated), skip to the
        // first page with visible buttons. This handles the WB pattern
        // where page 0 needs GPR[3807] from MOBJ dispatch feedback.
        if self.loaded.page.page_id == page_before && self.clip.page_count() > 1 {
            let has_visible_buttons = self
                .loaded
                .page
                .buttons
                .iter()
                .any(|b| b.normal.is_some() || b.selected.is_some());
            if !has_visible_buttons {
                for idx in 1..self.clip.page_count() {
                    if let Some(candidate) = self.clip.build_page(idx) {
                        let visible = candidate
                            .page
                            .buttons
                            .iter()
                            .any(|b| b.normal.is_some() || b.selected.is_some());
                        if visible {
                            self.loaded = candidate;
                            break;
                        }
                    }
                }
            }
        }

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
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                if let Some(ws) = &self.state
                    && let Some(button_id) = ws.highlight
                {
                    self.handle_click(button_id);
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Right,
                ..
            }
            | WindowEvent::KeyboardInput {
                event:
                    winit::event::KeyEvent {
                        physical_key:
                            winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::Escape),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => {
                self.navigate_back();
            }
            WindowEvent::RedrawRequested => {
                self.render();
            }
            _ => {}
        }
    }
}
