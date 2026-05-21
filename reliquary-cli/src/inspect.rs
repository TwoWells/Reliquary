// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! The `inspect` subcommand — disc structure and playlist listing.

use std::process::ExitCode;

/// Runs the `inspect` subcommand.
pub fn run_inspect(path: &std::path::Path, json: bool) -> ExitCode {
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
