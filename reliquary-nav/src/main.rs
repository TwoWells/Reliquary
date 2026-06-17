// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Reliquary Nav — disc menu navigator for physical media preservation.
//!
//! Opens a Blu-ray (decrypted BDMV directory, mounted disc, or ISO) via
//! `reliquary-bluray`, renders its HDMV menu at full resolution as an egui
//! image, and forwards arrow-key / Enter input to libbluray. Overlay-only:
//! no background video is decoded — the menu *is* the IG overlay plane.
//!
//! Usage: `reliquary-nav <disc-path>`, or set `RELIQUARY_TEST_DISC` and run
//! with no argument. No real titles or keys are embedded anywhere; the path is
//! supplied entirely at runtime.

mod app;

use std::path::PathBuf;
use std::process::ExitCode;

use reliquary_bluray::Disc;
use tracing_subscriber::EnvFilter;

use crate::app::NavApp;

/// Resolve the disc path from the first CLI argument, falling back to the
/// `RELIQUARY_TEST_DISC` environment variable. Returns `None` if neither is
/// provided.
fn resolve_disc_path() -> Option<PathBuf> {
    if let Some(arg) = std::env::args_os().nth(1) {
        return Some(PathBuf::from(arg));
    }
    std::env::var_os("RELIQUARY_TEST_DISC").map(PathBuf::from)
}

/// Install the process-wide tracing subscriber so the binary's diagnostics are
/// visible on stderr.
///
/// Verbosity is controlled by `RUST_LOG`; when it is unset we default to `info`
/// for our own crates and quiet the chatty GUI/windowing dependencies to `warn`
/// so a stray `info` from them does not bury the app's own messages. The fmt
/// layer writes to stderr (this is the `tracing` machinery, not the
/// `eprintln!` macro).
fn init_tracing() {
    // `RUST_LOG` wins when set; otherwise default to `info` for our own crates
    // and quiet the chatty GUI/windowing dependencies to `warn`. `EnvFilter`
    // parses this directive string and ignores any individual directive it
    // cannot understand, so it never fails here.
    const DEFAULT_FILTER: &str = "info,wgpu=warn,wgpu_core=warn,wgpu_hal=warn,winit=warn,eframe=warn,egui_glow=warn,calloop=warn,sctk=warn";
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

fn main() -> ExitCode {
    init_tracing();

    let Some(path) = resolve_disc_path() else {
        tracing::error!(
            "no disc path given; pass one as the first argument or set RELIQUARY_TEST_DISC"
        );
        return ExitCode::FAILURE;
    };

    // Open the disc (None defers to libaacs' KEYDB for encrypted discs).
    let mut disc = match Disc::open(&path, None) {
        Ok(disc) => disc,
        Err(err) => {
            tracing::error!(%err, path = %path.display(), "failed to open disc");
            return ExitCode::FAILURE;
        }
    };

    // Register the overlay handler and start menu playback.
    if !disc.play() {
        tracing::error!(
            path = %path.display(),
            "bd_play failed; the disc may have no playable on-disc menu"
        );
        return ExitCode::FAILURE;
    }
    // Jump straight to the top menu so the user lands on it immediately.
    disc.jump_to_menu();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_fullscreen(true)
            .with_title("Reliquary Nav"),
        ..eframe::NativeOptions::default()
    };

    match eframe::run_native(
        "Reliquary Nav",
        native_options,
        Box::new(|_cc| Ok(Box::new(NavApp::new(disc)))),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(%err, "eframe exited with an error");
            ExitCode::FAILURE
        }
    }
}
