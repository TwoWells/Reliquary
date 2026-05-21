// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! The `decrypt` subcommand — AACS decryption of Blu-ray discs and clips.

use std::process::ExitCode;

use crate::util::{format_size, parse_vuk};

/// Runs the `decrypt` subcommand — resolves VUK then dispatches.
#[allow(clippy::print_stderr, reason = "CLI status and error output")]
pub fn run_decrypt(
    path: &std::path::Path,
    vuk_hex: Option<&str>,
    clip: Option<&str>,
    keydb: Option<&std::path::Path>,
    no_keydb: bool,
    output: &std::path::Path,
) -> ExitCode {
    let (vuk, uk_data) = match resolve_vuk(path, vuk_hex, keydb, no_keydb) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };

    clip.map_or_else(
        || run_decrypt_disc(path, &vuk, uk_data.as_deref(), output),
        |clip_id| run_decrypt_clip(path, &vuk, clip_id, uk_data.as_deref(), output),
    )
}

/// Resolves the VUK from the CLI flag or KEYDB.cfg lookup.
///
/// Returns the VUK and, when disc ID computation was needed, the raw
/// `Unit_Key_RO.inf` bytes so callers can reuse them for key parsing.
///
/// Resolution order:
/// 1. `--vuk` flag → use directly (no unit key data read).
/// 2. Compute disc ID, search KEYDB.cfg.
/// 3. No match → error with disc ID for manual lookup.
#[allow(clippy::print_stderr, reason = "CLI status output")]
fn resolve_vuk(
    path: &std::path::Path,
    vuk_hex: Option<&str>,
    keydb: Option<&std::path::Path>,
    no_keydb: bool,
) -> Result<([u8; 16], Option<Vec<u8>>), String> {
    // Direct VUK from --vuk flag — no need to read the disc
    if let Some(hex) = vuk_hex {
        return Ok((parse_vuk(hex)?, None));
    }

    // Read Unit_Key_RO.inf once and compute disc ID
    let reader = reliquary::disc::reader::DiscReader::open(path).map_err(|e| e.to_string())?;
    let uk_data =
        reliquary::disc::bdmv::aacs::read_unit_key_data(&reader).map_err(|e| e.to_string())?;
    let disc_id = reliquary::disc::bdmv::aacs::disc_id_from_data(&uk_data);
    eprintln!("disc ID: {disc_id}");

    if no_keydb {
        return Err(format!(
            "VUK not provided and KEYDB.cfg lookup disabled — use --vuk <hex> (disc ID: {disc_id})"
        ));
    }

    // Determine KEYDB.cfg path
    let keydb_path = keydb.map_or_else(
        reliquary::disc::bdmv::keydb::default_keydb_path,
        std::path::PathBuf::from,
    );

    match reliquary::disc::bdmv::keydb::lookup_keydb(&keydb_path, &disc_id) {
        Ok(Some(vuk)) => {
            eprintln!("VUK found in KEYDB.cfg");
            Ok((vuk, Some(uk_data)))
        }
        Ok(None) => Err(format!(
            "VUK not found in KEYDB.cfg — use --vuk <hex> to provide manually (disc ID: {disc_id})"
        )),
        Err(e) => Err(e.to_string()),
    }
}

/// Decrypts a single m2ts clip.
///
/// When `uk_data` is provided (from VUK resolution), reuses it to avoid
/// re-reading `Unit_Key_RO.inf`.
#[allow(clippy::print_stderr, reason = "CLI status and error output")]
fn run_decrypt_clip(
    path: &std::path::Path,
    vuk: &[u8; 16],
    clip_id: &str,
    uk_data: Option<&[u8]>,
    output: &std::path::Path,
) -> ExitCode {
    let reader = match reliquary::disc::reader::DiscReader::open(path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let keys = match uk_data.map_or_else(
        || reliquary::disc::bdmv::aacs::AacsKeys::from_disc(&reader, vuk),
        |data| reliquary::disc::bdmv::aacs::AacsKeys::from_unit_key_data(data, vuk),
    ) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    match keys.decrypt_clip(&reader, clip_id) {
        Ok(data) => match std::fs::write(output, &data) {
            Ok(()) => {
                eprintln!("decrypted {} bytes to {}", data.len(), output.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: failed to write output: {e}");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// An m2ts file to decrypt, with its location within the ISO or filesystem.
struct M2tsTarget {
    name: String,
    size: u64,
    extents: Vec<reliquary::disc::reader::FileExtent>,
}

/// Decrypts a whole disc — copies input then decrypts m2ts files in-place.
///
/// When `uk_data` is provided (from VUK resolution), reuses it to avoid
/// re-reading `Unit_Key_RO.inf` from the output copy.
#[allow(clippy::print_stderr, reason = "CLI status and error output")]
fn run_decrypt_disc(
    path: &std::path::Path,
    vuk: &[u8; 16],
    uk_data: Option<&[u8]>,
    output: &std::path::Path,
) -> ExitCode {
    // 1. Copy input to output
    if let Err(e) = copy_input(path, output) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }

    // 2. Open the copy, parse keys, and collect targets
    let (keys, targets, is_iso) = match prepare_targets(output, vuk, uk_data) {
        Ok(result) => result,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };

    // 3. Decrypt each m2ts
    let (mut files_decrypted, mut files_skipped) = (0u32, 0u32);

    if is_iso {
        let mut file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(output)
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!("error: failed to open output ISO: {e}");
                return ExitCode::FAILURE;
            }
        };

        for target in &targets {
            match decrypt_iso_target(&keys, &mut file, target) {
                Ok(was_encrypted) => {
                    report_target(target, was_encrypted);
                    if was_encrypted {
                        files_decrypted += 1;
                    } else {
                        files_skipped += 1;
                    }
                }
                Err(e) => {
                    eprintln!("error: failed to decrypt {}: {e}", target.name);
                    return ExitCode::FAILURE;
                }
            }
        }
    } else {
        for target in &targets {
            match decrypt_dir_target(&keys, output, target) {
                Ok(was_encrypted) => {
                    report_target(target, was_encrypted);
                    if was_encrypted {
                        files_decrypted += 1;
                    } else {
                        files_skipped += 1;
                    }
                }
                Err(e) => {
                    eprintln!("error: failed to decrypt {}: {e}", target.name);
                    return ExitCode::FAILURE;
                }
            }
        }
    }

    eprintln!("done: {files_decrypted} m2ts decrypted, {files_skipped} skipped (not encrypted)");
    ExitCode::SUCCESS
}

/// Copies input (ISO or directory) to output.
#[allow(clippy::print_stderr, reason = "CLI status output")]
fn copy_input(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
    if src.is_dir() {
        eprintln!("copying directory...");
        copy_dir_recursive(src, dst).map_err(|e| format!("failed to copy directory: {e}"))
    } else {
        let size = std::fs::metadata(src).map_err(|e| e.to_string())?.len();
        eprintln!("copying ISO ({})...", format_size(size));
        std::fs::copy(src, dst)
            .map(|_| ())
            .map_err(|e| format!("failed to copy ISO: {e}"))
    }
}

/// Opens the output copy, parses AACS keys, and collects m2ts targets.
///
/// When `uk_data` is provided, uses it directly instead of re-reading
/// `Unit_Key_RO.inf` from the output.
#[allow(clippy::print_stderr, reason = "CLI status output")]
fn prepare_targets(
    output: &std::path::Path,
    vuk: &[u8; 16],
    uk_data: Option<&[u8]>,
) -> Result<(reliquary::disc::bdmv::aacs::AacsKeys, Vec<M2tsTarget>, bool), String> {
    let reader = reliquary::disc::reader::DiscReader::open(output).map_err(|e| e.to_string())?;

    eprintln!("parsing AACS keys...");
    let keys = uk_data
        .map_or_else(
            || reliquary::disc::bdmv::aacs::AacsKeys::from_disc(&reader, vuk),
            |data| reliquary::disc::bdmv::aacs::AacsKeys::from_unit_key_data(data, vuk),
        )
        .map_err(|e| e.to_string())?;

    let stream_dir = std::path::Path::new("BDMV/STREAM");
    let entries = reader
        .read_dir(stream_dir)
        .map_err(|e| format!("failed to list BDMV/STREAM: {e}"))?;

    let m2ts_names: Vec<&str> = entries
        .iter()
        .filter(|n| {
            std::path::Path::new(n)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("m2ts"))
        })
        .map(String::as_str)
        .collect();

    let is_iso = !output.is_dir();
    let mut targets = Vec::new();

    if is_iso {
        for name in &m2ts_names {
            let rel = stream_dir.join(name);
            match reader.file_extents(&rel) {
                Some(Ok(extents)) => {
                    let size: u64 = extents.iter().map(|e| e.length).sum();
                    targets.push(M2tsTarget {
                        name: (*name).to_owned(),
                        size,
                        extents,
                    });
                }
                Some(Err(e)) => {
                    return Err(format!("failed to get extents for {name}: {e}"));
                }
                None => {
                    return Err(format!("file extents not available for {name}"));
                }
            }
        }
    } else {
        for name in &m2ts_names {
            let m2ts_path = output.join("BDMV/STREAM").join(name);
            let size = std::fs::metadata(&m2ts_path)
                .map_err(|e| e.to_string())?
                .len();
            targets.push(M2tsTarget {
                name: (*name).to_owned(),
                size,
                extents: Vec::new(),
            });
        }
    }

    Ok((keys, targets, is_iso))
}

/// Decrypts an m2ts within an ISO via file extents. Returns whether it was encrypted.
fn decrypt_iso_target(
    keys: &reliquary::disc::bdmv::aacs::AacsKeys,
    file: &mut std::fs::File,
    target: &M2tsTarget,
) -> Result<bool, reliquary::disc::bdmv::aacs::AacsError> {
    let mut total_decrypted: u64 = 0;
    for extent in &target.extents {
        let stats = keys.decrypt_stream(file, extent.offset, extent.length)?;
        total_decrypted += stats.blocks_decrypted;
    }
    Ok(total_decrypted > 0)
}

/// Decrypts an m2ts file within a directory. Returns whether it was encrypted.
fn decrypt_dir_target(
    keys: &reliquary::disc::bdmv::aacs::AacsKeys,
    output: &std::path::Path,
    target: &M2tsTarget,
) -> Result<bool, reliquary::disc::bdmv::aacs::AacsError> {
    let m2ts_path = output.join("BDMV/STREAM").join(&target.name);
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&m2ts_path)?;
    let stats = keys.decrypt_stream(&mut file, 0, target.size)?;
    Ok(stats.blocks_decrypted > 0)
}

/// Prints a status line for a decrypted or skipped m2ts file.
#[allow(clippy::print_stderr, reason = "CLI status output")]
fn report_target(target: &M2tsTarget, was_encrypted: bool) {
    if was_encrypted {
        eprintln!(
            "decrypted BDMV/STREAM/{} ({})",
            target.name,
            format_size(target.size)
        );
    } else {
        eprintln!("skipped BDMV/STREAM/{} (not encrypted)", target.name);
    }
}

/// Recursively copies a directory tree.
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}
