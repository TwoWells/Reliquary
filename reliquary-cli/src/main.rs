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
        /// Path to an ISO image or extracted disc folder.
        path: PathBuf,

        /// Output as JSON instead of a human-readable table.
        #[arg(long)]
        json: bool,
    },

    /// Decrypt an AACS-encrypted m2ts clip from a Blu-ray disc.
    Decrypt {
        /// Path to an ISO image or extracted disc folder.
        path: PathBuf,

        /// Volume Unique Key as a 32-character hex string.
        #[arg(long)]
        vuk: String,

        /// Clip ID to decrypt (e.g. "00100").
        #[arg(long)]
        clip: String,

        /// Output file path for the decrypted m2ts.
        #[arg(short, long)]
        output: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Command::Inspect { path, json } => run_inspect(&path, json),
        Command::Decrypt {
            path,
            vuk,
            clip,
            output,
        } => run_decrypt(&path, &vuk, &clip, &output),
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

/// Runs the `decrypt` subcommand.
fn run_decrypt(
    path: &std::path::Path,
    vuk_hex: &str,
    clip_id: &str,
    output: &std::path::Path,
) -> ExitCode {
    let vuk = match parse_vuk(vuk_hex) {
        Ok(v) => v,
        Err(msg) => {
            #[allow(clippy::print_stderr, reason = "CLI error output")]
            {
                eprintln!("error: {msg}");
            }
            return ExitCode::FAILURE;
        }
    };

    let reader = match reliquary::disc::reader::DiscReader::open(path) {
        Ok(r) => r,
        Err(e) => {
            #[allow(clippy::print_stderr, reason = "CLI error output")]
            {
                eprintln!("error: {e}");
            }
            return ExitCode::FAILURE;
        }
    };

    match reliquary::disc::bdmv::aacs::decrypt_clip(&reader, &vuk, clip_id) {
        Ok(data) => match std::fs::write(output, &data) {
            Ok(()) => {
                #[allow(clippy::print_stderr, reason = "CLI status output")]
                {
                    eprintln!("decrypted {} bytes to {}", data.len(), output.display());
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                #[allow(clippy::print_stderr, reason = "CLI error output")]
                {
                    eprintln!("error: failed to write output: {e}");
                }
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            #[allow(clippy::print_stderr, reason = "CLI error output")]
            {
                eprintln!("error: {e}");
            }
            ExitCode::FAILURE
        }
    }
}

/// Parses a 32-character hex string into a 16-byte VUK.
fn parse_vuk(hex: &str) -> Result<[u8; 16], String> {
    let hex = hex.strip_prefix("0x").unwrap_or(hex);

    if hex.len() != 32 {
        return Err(format!("VUK must be 32 hex characters, got {}", hex.len()));
    }

    let mut vuk = [0u8; 16];
    for (i, byte) in vuk.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|_| format!("invalid hex at position {}", i * 2))?;
    }
    Ok(vuk)
}
