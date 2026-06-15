// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Reliquary CLI — command-line interface for physical media preservation.

mod inspect;

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
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Command::Inspect { path, json } => inspect::run_inspect(&path, json),
    }
}
