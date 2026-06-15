// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Reliquary CLI — command-line interface for physical media preservation.

mod identify;
mod inspect;
mod output;
mod prompt;
mod render;
mod snapshot;
mod trace;
mod util;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Reliquary — physical media preservation toolkit.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Available subcommands.
#[derive(Subcommand)]
enum Command {
    /// Inspect a disc — show structure, playlists, streams, and main title.
    Inspect {
        /// Path to an ISO image or extracted disc folder.
        path: PathBuf,

        /// Output as JSON instead of a human-readable table.
        #[arg(long)]
        json: bool,
    },

    /// Identify disc content — extract menu button bitmaps and name extras.
    Identify {
        /// Path to an ISO image or extracted disc folder.
        path: PathBuf,

        /// Volume Unique Key as a 32-character hex string (overrides KEYDB.cfg lookup).
        #[arg(long)]
        vuk: Option<String>,

        /// Path to `KEYDB.cfg` (default: `$XDG_CONFIG_HOME/aacs/KEYDB.cfg`).
        #[arg(long)]
        keydb: Option<PathBuf>,

        /// Skip KEYDB.cfg lookup.
        #[arg(long)]
        no_keydb: bool,

        /// Dump resolved button→playlist mappings without the interactive
        /// naming prompt. Outputs JSON to stdout and exits.
        #[arg(long)]
        dump: bool,

        /// Output as JSON instead of a text report.
        #[arg(long)]
        json: bool,

        /// Skip bitmap rendering (text-only mode).
        #[arg(long)]
        no_images: bool,

        /// Dump MOBJ instruction trace for debugging GPR dispatch resolution.
        #[arg(long)]
        trace: bool,

        /// Write composited page images as PPM files to the given directory.
        #[arg(long)]
        dump_pages: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Command::Inspect { path, json } => inspect::run_inspect(&path, json),
        Command::Identify {
            path,
            vuk,
            keydb,
            no_keydb,
            dump,
            json,
            no_images,
            trace,
            dump_pages,
        } => identify::run_identify(
            &path,
            vuk.as_deref(),
            keydb.as_deref(),
            no_keydb,
            dump,
            json,
            no_images,
            trace,
            dump_pages.as_deref(),
        ),
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod tests {
    use crate::util::parse_vuk;

    #[test]
    fn invalid_vuk_format() {
        assert!(parse_vuk("0x1234").is_err(), "short VUK should be rejected");
        assert!(
            parse_vuk("ZZZZ0000000000000000000000000000").is_err(),
            "non-hex VUK should be rejected"
        );
        assert!(
            parse_vuk("00000000000000000000000000000000").is_ok(),
            "valid 32-char hex should parse"
        );
        assert!(
            parse_vuk("0x00000000000000000000000000000000").is_ok(),
            "0x-prefixed 32-char hex should parse"
        );
    }
}
