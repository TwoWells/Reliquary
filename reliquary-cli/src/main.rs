// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Reliquary CLI — command-line interface for physical media preservation.

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
        /// Path to a mounted ISO, extracted disc folder, or BDMV root.
        path: PathBuf,

        /// Output as JSON instead of a human-readable table.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Command::Inspect { path, json } => run_inspect(&path, json),
    }
}

/// Runs the `inspect` subcommand.
fn run_inspect(path: &std::path::Path, json: bool) -> ExitCode {
    match reliquary::disc::inspect(path) {
        Ok(result) => {
            if json {
                match serde_json::to_string_pretty(&result) {
                    Ok(output) => {
                        // Use write! to stdout — print_stdout is denied by clippy config.
                        // This is the CLI crate's presentation layer, so stdout is correct.
                        #[allow(clippy::print_stdout, reason = "CLI output to stdout")]
                        {
                            println!("{output}");
                        }
                    }
                    Err(e) => {
                        #[allow(clippy::print_stderr, reason = "CLI error output")]
                        {
                            eprintln!("error: failed to serialize JSON: {e}");
                        }
                        return ExitCode::FAILURE;
                    }
                }
            } else {
                #[allow(clippy::print_stdout, reason = "CLI output to stdout")]
                {
                    print!("{result}");
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            #[allow(clippy::print_stderr, reason = "CLI error output")]
            {
                eprintln!("error: {e}");
            }
            ExitCode::FAILURE
        }
    }
}
