// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Reliquary CLI — command-line interface for physical media preservation.

mod decrypt;
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

    /// Decrypt an AACS-encrypted Blu-ray disc or single clip.
    Decrypt {
        /// Path to an ISO image or extracted disc folder.
        path: PathBuf,

        /// Volume Unique Key as a 32-character hex string (overrides lookup).
        #[arg(long)]
        vuk: Option<String>,

        /// Decrypt a single clip instead of the whole disc (e.g. "00100").
        #[arg(long)]
        clip: Option<String>,

        /// Path to `KEYDB.cfg` (default: `$XDG_CONFIG_HOME/aacs/KEYDB.cfg`).
        #[arg(long)]
        keydb: Option<PathBuf>,

        /// Skip KEYDB.cfg lookup.
        #[arg(long)]
        no_keydb: bool,

        /// Output path (ISO/directory for whole-disc, file for per-clip).
        #[arg(short, long)]
        output: PathBuf,
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
        Command::Decrypt {
            path,
            vuk,
            clip,
            keydb,
            no_keydb,
            output,
        } => decrypt::run_decrypt(
            &path,
            vuk.as_deref(),
            clip.as_deref(),
            keydb.as_deref(),
            no_keydb,
            &output,
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
    use aes::Aes128;
    use cbc::Encryptor as CbcEncryptor;
    use cipher::block_padding::NoPadding;
    use cipher::{BlockEncrypt, BlockEncryptMut, KeyInit, KeyIvInit};

    use std::process::ExitCode;

    use crate::decrypt;
    use crate::util::parse_vuk;

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

        let result = decrypt::run_decrypt(
            disc.path(),
            Some(&disc.vuk_hex()),
            None,
            None,
            false,
            &output_path,
        );
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
            "playlist should be copied unchanged"
        );
    }

    #[test]
    fn single_clip_decrypt() {
        let disc = SyntheticDisc::new();
        let output = tempfile::tempdir().expect("should create output dir");
        let output_path = output.path().join("clip.m2ts");

        let result = decrypt::run_decrypt(
            disc.path(),
            Some(&disc.vuk_hex()),
            Some("00000"),
            None,
            false,
            &output_path,
        );
        assert_eq!(
            result,
            ExitCode::SUCCESS,
            "single-clip decrypt should succeed"
        );

        let decrypted = std::fs::read(&output_path).expect("should read decrypted clip");
        assert_eq!(
            decrypted.len(),
            ALIGNED_UNIT_LEN,
            "decrypted clip should be one aligned unit"
        );
        // Verify TS sync bytes are in place
        for i in 0..32 {
            assert_eq!(
                decrypted[i * 192 + 4],
                0x47,
                "TS sync byte at packet {i} should be 0x47"
            );
        }
    }

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
