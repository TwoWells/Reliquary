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

    /// Decrypt an AACS-encrypted Blu-ray disc or single clip.
    Decrypt {
        /// Path to an ISO image or extracted disc folder.
        path: PathBuf,

        /// Volume Unique Key as a 32-character hex string.
        #[arg(long)]
        vuk: String,

        /// Decrypt a single clip instead of the whole disc (e.g. "00100").
        #[arg(long)]
        clip: Option<String>,

        /// Output path (ISO/directory for whole-disc, file for per-clip).
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
        } => run_decrypt(&path, &vuk, clip.as_deref(), &output),
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

/// Runs the `decrypt` subcommand — dispatches to per-clip or whole-disc.
fn run_decrypt(
    path: &std::path::Path,
    vuk_hex: &str,
    clip: Option<&str>,
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

    clip.map_or_else(
        || run_decrypt_disc(path, &vuk, output),
        |clip_id| run_decrypt_clip(path, &vuk, clip_id, output),
    )
}

/// Decrypts a single m2ts clip (existing per-clip behavior).
#[allow(clippy::print_stderr, reason = "CLI status and error output")]
fn run_decrypt_clip(
    path: &std::path::Path,
    vuk: &[u8; 16],
    clip_id: &str,
    output: &std::path::Path,
) -> ExitCode {
    let reader = match reliquary::disc::reader::DiscReader::open(path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    match reliquary::disc::bdmv::aacs::decrypt_clip(&reader, vuk, clip_id) {
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
#[allow(clippy::print_stderr, reason = "CLI status and error output")]
fn run_decrypt_disc(path: &std::path::Path, vuk: &[u8; 16], output: &std::path::Path) -> ExitCode {
    // 1. Copy input to output
    if let Err(e) = copy_input(path, output) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }

    // 2. Open the copy, parse keys, and collect targets
    let (keys, targets, is_iso) = match prepare_targets(output, vuk) {
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
#[allow(clippy::print_stderr, reason = "CLI status output")]
fn prepare_targets(
    output: &std::path::Path,
    vuk: &[u8; 16],
) -> Result<(reliquary::disc::bdmv::aacs::AacsKeys, Vec<M2tsTarget>, bool), String> {
    let reader = reliquary::disc::reader::DiscReader::open(output).map_err(|e| e.to_string())?;

    eprintln!("parsing AACS keys...");
    let keys = reliquary::disc::bdmv::aacs::AacsKeys::from_disc(&reader, vuk)
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

/// Formats a byte count as a human-readable string.
#[allow(
    clippy::cast_precision_loss,
    reason = "file sizes fit within f64 precision for any real disc"
)]
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else {
        format!("{:.1} KB", bytes as f64 / KB as f64)
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

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod tests {
    use aes::Aes128;
    use cbc::Encryptor as CbcEncryptor;
    use cipher::block_padding::NoPadding;
    use cipher::{BlockEncrypt, BlockEncryptMut, KeyInit, KeyIvInit};

    use super::*;

    const ALIGNED_UNIT_LEN: usize = 6144;
    const KEY_TABLE_HEADER_LEN: usize = 48;
    const KEY_ENTRY_STRIDE: usize = 48;
    const AACS_IV: [u8; 16] = [
        0x0B, 0xA0, 0xF8, 0xDD, 0xFE, 0xA6, 0x1F, 0xB3, 0xD8, 0xDF, 0x9F, 0x56, 0x6A, 0x05, 0x0F,
        0x78,
    ];

    // ── Test helpers ─────────────────────────────────────────────────────

    /// Encrypts a unit key with the VUK (as stored on disc).
    fn encrypt_unit_key(plaintext_key: &[u8; 16], vuk: &[u8; 16]) -> [u8; 16] {
        let cipher = Aes128::new(vuk.into());
        let mut block = aes::Block::clone_from_slice(plaintext_key);
        cipher.encrypt_block(&mut block);
        let mut result = [0u8; 16];
        result.copy_from_slice(&block);
        result
    }

    /// Builds a valid `Unit_Key_RO.inf` binary with the given encrypted keys.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "test helper — key counts and offsets are small positive values"
    )]
    fn build_unit_key_file(encrypted_keys: &[[u8; 16]]) -> Vec<u8> {
        let title_section_size: usize = 6 + 4; // 1 title
        let uk_pos: u32 = (20 + title_section_size) as u32;
        let key_table_size = KEY_TABLE_HEADER_LEN + encrypted_keys.len() * KEY_ENTRY_STRIDE;
        let total = uk_pos as usize + key_table_size;

        let mut data = vec![0u8; total];
        data[0..4].copy_from_slice(&uk_pos.to_be_bytes());

        // Title mapping: first_play=1, top_menu=1, num_titles=1, title[0].cps_unit=1
        data[20..22].copy_from_slice(&1u16.to_be_bytes());
        data[22..24].copy_from_slice(&1u16.to_be_bytes());
        data[24..26].copy_from_slice(&1u16.to_be_bytes());
        data[28..30].copy_from_slice(&1u16.to_be_bytes());

        let uk_start = uk_pos as usize;
        let num_uk = encrypted_keys.len() as u16;
        data[uk_start..uk_start + 2].copy_from_slice(&num_uk.to_be_bytes());

        for (i, key) in encrypted_keys.iter().enumerate() {
            let offset = uk_start + KEY_TABLE_HEADER_LEN + i * KEY_ENTRY_STRIDE;
            data[offset..offset + 16].copy_from_slice(key);
        }

        data
    }

    /// Builds an encrypted 6144-byte aligned unit.
    fn build_encrypted_block(unit_key: &[u8; 16], seed: [u8; 16]) -> [u8; ALIGNED_UNIT_LEN] {
        let mut block = [0u8; ALIGNED_UNIT_LEN];
        block[..16].copy_from_slice(&seed);
        for i in 0..32 {
            block[i * 192 + 4] = 0x47;
        }
        block[0] |= 0xC0;
        block[4] = 0x47;

        let cipher = Aes128::new(unit_key.into());
        let mut derived_key_block = aes::Block::clone_from_slice(&block[..16]);
        cipher.encrypt_block(&mut derived_key_block);
        let mut derived_key = [0u8; 16];
        for (i, byte) in derived_key.iter_mut().enumerate() {
            *byte = derived_key_block[i] ^ block[i];
        }

        let encryptor = CbcEncryptor::<Aes128>::new(&derived_key.into(), &AACS_IV.into());
        encryptor
            .encrypt_padded_mut::<NoPadding>(&mut block[16..], ALIGNED_UNIT_LEN - 16)
            .expect("encryption should succeed");

        block
    }

    /// Builds an unencrypted 6144-byte aligned unit with valid TS sync.
    fn build_unencrypted_block() -> [u8; ALIGNED_UNIT_LEN] {
        let mut block = [0u8; ALIGNED_UNIT_LEN];
        for i in 0..32 {
            block[i * 192 + 4] = 0x47;
        }
        block
    }

    /// Creates a synthetic disc directory for testing.
    struct SyntheticDisc {
        dir: tempfile::TempDir,
        vuk: [u8; 16],
    }

    impl SyntheticDisc {
        /// Creates a disc with 2 encrypted + 1 unencrypted m2ts + metadata.
        fn new() -> Self {
            let vuk = [0x42; 16];
            let plaintext_key = [0x77; 16];
            let stored_key = encrypt_unit_key(&plaintext_key, &vuk);
            let uk_data = build_unit_key_file(&[stored_key]);

            let dir = tempfile::tempdir().expect("should create temp dir");
            let aacs_dir = dir.path().join("AACS");
            let stream_dir = dir.path().join("BDMV").join("STREAM");
            let playlist_dir = dir.path().join("BDMV").join("PLAYLIST");
            std::fs::create_dir_all(&aacs_dir).expect("should create AACS dir");
            std::fs::create_dir_all(&stream_dir).expect("should create STREAM dir");
            std::fs::create_dir_all(&playlist_dir).expect("should create PLAYLIST dir");

            // Unit key file
            std::fs::write(aacs_dir.join("Unit_Key_RO.inf"), &uk_data)
                .expect("should write unit key file");

            // Encrypted m2ts files
            let block1 = build_encrypted_block(&plaintext_key, [0x01; 16]);
            let block2 = build_encrypted_block(&plaintext_key, [0x02; 16]);
            std::fs::write(stream_dir.join("00000.m2ts"), block1).expect("should write m2ts 00000");
            std::fs::write(stream_dir.join("00001.m2ts"), block2).expect("should write m2ts 00001");

            // Unencrypted m2ts
            let unenc = build_unencrypted_block();
            std::fs::write(stream_dir.join("00100.m2ts"), unenc).expect("should write m2ts 00100");

            // Non-m2ts metadata
            std::fs::write(playlist_dir.join("00000.mpls"), b"fake playlist data")
                .expect("should write playlist");

            Self { dir, vuk }
        }

        fn path(&self) -> &std::path::Path {
            self.dir.path()
        }

        fn vuk_hex(&self) -> String {
            self.vuk
                .iter()
                .fold(String::with_capacity(32), |mut acc, b| {
                    use std::fmt::Write as _;
                    let _ = write!(acc, "{b:02x}");
                    acc
                })
        }
    }

    // ── Tests ────────────────────────────────────────────────────────────

    #[test]
    fn whole_disc_decrypt_directory() {
        let disc = SyntheticDisc::new();
        let output = tempfile::tempdir().expect("should create output dir");
        let output_path = output.path().join("decrypted");

        let result = run_decrypt(disc.path(), &disc.vuk_hex(), None, &output_path);
        assert_eq!(
            result,
            ExitCode::SUCCESS,
            "whole-disc decrypt should succeed"
        );

        // Output structure matches input
        assert!(
            output_path.join("AACS/Unit_Key_RO.inf").exists(),
            "Unit_Key_RO.inf should be copied"
        );
        assert!(
            output_path.join("BDMV/STREAM/00000.m2ts").exists(),
            "00000.m2ts should exist"
        );
        assert!(
            output_path.join("BDMV/STREAM/00001.m2ts").exists(),
            "00001.m2ts should exist"
        );
        assert!(
            output_path.join("BDMV/STREAM/00100.m2ts").exists(),
            "00100.m2ts should exist"
        );
        assert!(
            output_path.join("BDMV/PLAYLIST/00000.mpls").exists(),
            "playlist should be copied"
        );

        // Non-m2ts files copied byte-for-byte
        let playlist = std::fs::read(output_path.join("BDMV/PLAYLIST/00000.mpls"))
            .expect("should read playlist");
        assert_eq!(
            playlist, b"fake playlist data",
            "non-m2ts files should be byte-identical"
        );

        // Encrypted m2ts files should have valid TS sync (decrypted)
        for name in &["00000.m2ts", "00001.m2ts"] {
            let data = std::fs::read(output_path.join("BDMV/STREAM").join(name))
                .expect("should read m2ts");
            assert_eq!(
                data.len(),
                ALIGNED_UNIT_LEN,
                "{name} size should be unchanged"
            );
            // Encryption flag cleared
            assert_eq!(
                data[0] & 0xC0,
                0,
                "{name} encryption flag should be cleared"
            );
            // TS sync bytes present
            for pkt in 0..4 {
                let offset = pkt * 192 + 4;
                assert_eq!(data[offset], 0x47, "{name} TS sync at packet {pkt}");
            }
        }

        // Unencrypted m2ts should be identical to source
        let src_unenc =
            std::fs::read(disc.path().join("BDMV/STREAM/00100.m2ts")).expect("should read source");
        let dst_unenc =
            std::fs::read(output_path.join("BDMV/STREAM/00100.m2ts")).expect("should read output");
        assert_eq!(
            src_unenc, dst_unenc,
            "unencrypted m2ts should be byte-identical"
        );

        // Output size matches input for each m2ts
        for name in &["00000.m2ts", "00001.m2ts", "00100.m2ts"] {
            let src_len = std::fs::metadata(disc.path().join("BDMV/STREAM").join(name))
                .expect("should stat source")
                .len();
            let dst_len = std::fs::metadata(output_path.join("BDMV/STREAM").join(name))
                .expect("should stat output")
                .len();
            assert_eq!(src_len, dst_len, "{name} size should be unchanged");
        }
    }

    #[test]
    fn whole_disc_wrong_vuk() {
        let disc = SyntheticDisc::new();
        let output = tempfile::tempdir().expect("should create output dir");
        let output_path = output.path().join("decrypted");

        let wrong_vuk = "ff".repeat(16);
        let result = run_decrypt(disc.path(), &wrong_vuk, None, &output_path);
        assert_eq!(
            result,
            ExitCode::FAILURE,
            "wrong VUK should produce failure"
        );
    }

    #[test]
    fn whole_disc_no_unit_key_file() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let stream_dir = dir.path().join("BDMV").join("STREAM");
        std::fs::create_dir_all(&stream_dir).expect("should create STREAM dir");
        std::fs::write(stream_dir.join("00000.m2ts"), build_unencrypted_block())
            .expect("should write m2ts");

        let output = tempfile::tempdir().expect("should create output dir");
        let output_path = output.path().join("decrypted");

        let vuk = "42".repeat(16);
        let result = run_decrypt(dir.path(), &vuk, None, &output_path);
        assert_eq!(
            result,
            ExitCode::FAILURE,
            "missing Unit_Key_RO.inf should produce failure"
        );
    }

    #[test]
    fn per_clip_still_works() {
        let disc = SyntheticDisc::new();
        let output = tempfile::tempdir().expect("should create output dir");
        let output_path = output.path().join("00000.m2ts");

        let result = run_decrypt(disc.path(), &disc.vuk_hex(), Some("00000"), &output_path);
        assert_eq!(result, ExitCode::SUCCESS, "per-clip decrypt should succeed");

        let data = std::fs::read(&output_path).expect("should read output");
        assert_eq!(data.len(), ALIGNED_UNIT_LEN, "output size should match");
        assert_eq!(data[0] & 0xC0, 0, "encryption flag should be cleared");
        for pkt in 0..4 {
            let offset = pkt * 192 + 4;
            assert_eq!(data[offset], 0x47, "TS sync at packet {pkt}");
        }
    }
}
