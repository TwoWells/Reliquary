// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! AACS decryption — decrypts Blu-ray m2ts streams given a known VUK.
//!
//! Reference: `reference/AACS.md` in the planning repository.
//! Implements the VUK-to-content decryption path: parse `Unit_Key_RO.inf`,
//! decrypt unit keys (AES-128-ECB), decrypt aligned units (AES-128-CBC with
//! per-block key derivation).

use aes::Aes128;
use cbc::Decryptor as CbcDecryptor;
use cipher::block_padding::NoPadding;
use cipher::{BlockDecrypt, BlockDecryptMut, BlockEncrypt, KeyInit, KeyIvInit};
use sha1::{Digest, Sha1};
use thiserror::Error;

use super::super::reader::{DiscReader, ReaderError};

use std::fs::File;
use std::io::{Read as _, Seek, SeekFrom, Write as _};

// ── Constants ──────────────────────────────────────────────────────────────

/// Size of one AACS aligned unit in bytes (32 × 192-byte TS packets).
const ALIGNED_UNIT_LEN: usize = 6144;

/// Hardcoded IV for AACS CBC decryption.
const AACS_IV: [u8; 16] = [
    0x0B, 0xA0, 0xF8, 0xDD, 0xFE, 0xA6, 0x1F, 0xB3, 0xD8, 0xDF, 0x9F, 0x56, 0x6A, 0x05, 0x0F, 0x78,
];

/// Byte stride between key entries in `Unit_Key_RO.inf`.
const KEY_ENTRY_STRIDE: usize = 48;

/// Offset from `uk_pos` to the first key entry.
const KEY_TABLE_HEADER_LEN: usize = 48;

/// Number of TS sync bytes to check when verifying decryption.
const SYNC_CHECK_COUNT: usize = 4;

// ── Errors ─────────────────────────────────────────────────────────────────

/// Errors from AACS decryption.
#[derive(Debug, Error)]
pub enum AacsError {
    /// `AACS/Unit_Key_RO.inf` not found (disc may not be encrypted).
    #[error("AACS/Unit_Key_RO.inf not found — disc may not be encrypted")]
    NoUnitKeyFile,

    /// `Unit_Key_RO.inf` is malformed or truncated.
    #[error("invalid Unit_Key_RO.inf: {reason}")]
    InvalidUnitKeyFile {
        /// Description of the parse failure.
        reason: String,
    },

    /// The m2ts has no encrypted aligned units.
    #[error("clip {clip_id} has no encrypted aligned units — use read_file directly")]
    NotEncrypted {
        /// Clip ID that was not encrypted.
        clip_id: String,
    },

    /// No unit key successfully decrypted the content (wrong VUK).
    #[error("decryption failed for clip {clip_id} — no unit key produced valid TS sync")]
    DecryptionFailed {
        /// Clip ID that could not be decrypted.
        clip_id: String,
    },

    /// The m2ts file could not be read from the disc.
    #[error("failed to read m2ts: {0}")]
    ReadError(#[from] ReaderError),

    /// An I/O error occurred during streaming decryption.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

// ── Internal types ─────────────────────────────────────────────────────────

/// Parsed content of `AACS/Unit_Key_RO.inf`.
struct UnitKeyFile {
    /// Encrypted unit keys (16 bytes each).
    encrypted_keys: Vec<[u8; 16]>,
}

// ── Public types ──────────────────────────────────────────────────────────

/// Statistics from a streaming decryption operation.
#[derive(Debug, Clone, Copy)]
pub struct DecryptStats {
    /// Number of aligned units that were decrypted.
    pub blocks_decrypted: u64,
    /// Number of aligned units that were skipped (already unencrypted).
    pub blocks_skipped: u64,
}

/// A disc's AACS identity — the SHA-1 hash of `AACS/Unit_Key_RO.inf`.
///
/// This is the disc ID used to look up the VUK in KEYDB.cfg.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscId {
    /// SHA-1 hash as 40 lowercase hex characters.
    pub id: String,
}

impl std::fmt::Display for DiscId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.id)
    }
}

/// Parsed and decrypted unit keys, ready for m2ts decryption.
///
/// Pre-parses `Unit_Key_RO.inf` and decrypts unit keys once, so they
/// can be reused across multiple m2ts files during whole-disc decryption.
pub struct AacsKeys {
    unit_keys: Vec<[u8; 16]>,
}

impl AacsKeys {
    /// Parse `Unit_Key_RO.inf` and decrypt unit keys with the VUK.
    ///
    /// Reads the unit key file from the disc. To avoid re-reading when
    /// the data is already available (e.g. from disc ID computation),
    /// use [`Self::from_unit_key_data`].
    ///
    /// # Errors
    ///
    /// Returns [`AacsError`] if the unit key file is missing or malformed.
    pub fn from_disc(reader: &DiscReader, vuk: &[u8; 16]) -> Result<Self, AacsError> {
        let uk_data = read_unit_key_data(reader)?;
        Self::from_unit_key_data(&uk_data, vuk)
    }

    /// Parse pre-read `Unit_Key_RO.inf` data and decrypt unit keys.
    ///
    /// Use this when the raw `Unit_Key_RO.inf` bytes are already
    /// available (e.g. saved from [`read_unit_key_data`] during disc ID
    /// computation) to avoid reading the file twice.
    ///
    /// # Errors
    ///
    /// Returns [`AacsError::InvalidUnitKeyFile`] if the data is malformed.
    pub fn from_unit_key_data(uk_data: &[u8], vuk: &[u8; 16]) -> Result<Self, AacsError> {
        let uk_file = parse_unit_key_file(uk_data)?;
        let unit_keys = decrypt_unit_keys(&uk_file, vuk);
        Ok(Self { unit_keys })
    }

    /// Decrypt a single m2ts clip using these keys.
    ///
    /// Reads the full m2ts into memory, decrypts, and returns the
    /// plaintext. For large files, prefer [`Self::decrypt_stream`].
    ///
    /// # Errors
    ///
    /// Returns [`AacsError`] if the clip cannot be read, is not
    /// encrypted, or no unit key produces valid TS sync.
    pub fn decrypt_clip(&self, reader: &DiscReader, clip_id: &str) -> Result<Vec<u8>, AacsError> {
        let m2ts_path = format!("BDMV/STREAM/{clip_id}.m2ts");
        let mut data = reader
            .read_file(std::path::Path::new(&m2ts_path))
            .map_err(AacsError::ReadError)?;
        self.decrypt_data(&mut data, clip_id)?;
        Ok(data)
    }

    /// Decrypt m2ts data in-place.
    ///
    /// The `clip_id` is used only for error messages.
    ///
    /// # Errors
    ///
    /// Returns [`AacsError::NotEncrypted`] if no blocks are encrypted,
    /// or [`AacsError::DecryptionFailed`] if no unit key works.
    pub fn decrypt_data(&self, data: &mut [u8], clip_id: &str) -> Result<(), AacsError> {
        let first_encrypted = find_first_encrypted_block(data);
        let Some(first_offset) = first_encrypted else {
            return Err(AacsError::NotEncrypted {
                clip_id: clip_id.to_owned(),
            });
        };

        let key_index = find_unit_key(
            &data[first_offset..first_offset + ALIGNED_UNIT_LEN],
            &self.unit_keys,
        )
        .ok_or_else(|| AacsError::DecryptionFailed {
            clip_id: clip_id.to_owned(),
        })?;

        decrypt_m2ts(data, &self.unit_keys[key_index]);
        Ok(())
    }

    /// Decrypt m2ts content by seeking within a file.
    ///
    /// Reads 6144-byte aligned units from `file` starting at `offset`
    /// for `length` bytes, decrypts encrypted blocks in-place, and
    /// writes them back. Uses constant memory regardless of file size.
    ///
    /// Auto-detects the correct unit key on the first encrypted block.
    /// If no blocks are encrypted, returns [`DecryptStats`] with zero
    /// decrypted and all skipped (no error).
    ///
    /// # Errors
    ///
    /// Returns [`AacsError::DecryptionFailed`] if encrypted blocks are
    /// found but no unit key produces valid TS sync, or [`AacsError::Io`]
    /// on I/O failures.
    pub fn decrypt_stream(
        &self,
        file: &mut File,
        offset: u64,
        length: u64,
    ) -> Result<DecryptStats, AacsError> {
        let mut blocks_decrypted: u64 = 0;
        let mut blocks_skipped: u64 = 0;
        let mut unit_key_idx: Option<usize> = None;
        let mut block = [0u8; ALIGNED_UNIT_LEN];
        let mut pos: u64 = 0;

        while pos + ALIGNED_UNIT_LEN as u64 <= length {
            let file_offset = offset + pos;
            file.seek(SeekFrom::Start(file_offset))?;
            file.read_exact(&mut block)?;

            if block[0] & 0xC0 != 0 {
                let key_idx = if let Some(idx) = unit_key_idx {
                    idx
                } else {
                    let idx = find_unit_key(&block, &self.unit_keys).ok_or_else(|| {
                        AacsError::DecryptionFailed {
                            clip_id: format!("stream at offset {file_offset:#x}"),
                        }
                    })?;
                    unit_key_idx = Some(idx);
                    idx
                };

                decrypt_block(&mut block, &self.unit_keys[key_idx]);

                file.seek(SeekFrom::Start(file_offset))?;
                file.write_all(&block)?;

                blocks_decrypted += 1;
            } else {
                blocks_skipped += 1;
            }

            pos += ALIGNED_UNIT_LEN as u64;
        }

        Ok(DecryptStats {
            blocks_decrypted,
            blocks_skipped,
        })
    }
}

// ── Public API ─────────────────────────────────────────────────────────────

/// Decrypts an m2ts clip from a Blu-ray disc.
///
/// Convenience wrapper around [`AacsKeys::from_disc`] and
/// [`AacsKeys::decrypt_clip`]. For decrypting multiple clips from the
/// same disc, use [`AacsKeys`] directly to avoid re-parsing keys.
///
/// # Errors
///
/// Returns [`AacsError`] if:
/// - `Unit_Key_RO.inf` is missing ([`AacsError::NoUnitKeyFile`])
/// - `Unit_Key_RO.inf` is malformed ([`AacsError::InvalidUnitKeyFile`])
/// - The clip has no encrypted blocks ([`AacsError::NotEncrypted`])
/// - No unit key produces valid TS sync ([`AacsError::DecryptionFailed`])
/// - The m2ts cannot be read ([`AacsError::ReadError`])
pub fn decrypt_clip(
    reader: &DiscReader,
    vuk: &[u8; 16],
    clip_id: &str,
) -> Result<Vec<u8>, AacsError> {
    AacsKeys::from_disc(reader, vuk)?.decrypt_clip(reader, clip_id)
}

/// Reads `AACS/Unit_Key_RO.inf` from the disc.
///
/// Returns the raw file bytes. These can be passed to
/// [`disc_id_from_data`] and [`AacsKeys::from_unit_key_data`] to avoid
/// reading the file multiple times.
///
/// # Errors
///
/// Returns [`AacsError::NoUnitKeyFile`] if the file is not found,
/// or [`AacsError::ReadError`] on I/O failure.
pub fn read_unit_key_data(reader: &DiscReader) -> Result<Vec<u8>, AacsError> {
    let path = std::path::Path::new("AACS/Unit_Key_RO.inf");
    match reader.read_file(path) {
        Ok(data) => Ok(data),
        Err(ReaderError::NotFound { .. }) => Err(AacsError::NoUnitKeyFile),
        Err(e) => Err(AacsError::ReadError(e)),
    }
}

/// Computes the disc ID from raw `Unit_Key_RO.inf` data.
///
/// The disc ID is the SHA-1 hash of the file, returned as 40 lowercase
/// hex characters. This is the standard AACS identifier used to look up
/// the VUK in KEYDB.cfg.
#[must_use]
pub fn disc_id_from_data(data: &[u8]) -> DiscId {
    let hash = Sha1::digest(data);
    let id = hash.iter().fold(String::with_capacity(40), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    });
    DiscId { id }
}

/// Computes the disc ID (SHA-1 of `AACS/Unit_Key_RO.inf`).
///
/// Convenience wrapper that reads the file and hashes it. When the raw
/// data is already available, use [`disc_id_from_data`] instead.
///
/// # Errors
///
/// Returns [`AacsError::NoUnitKeyFile`] if `AACS/Unit_Key_RO.inf` is
/// not found, or [`AacsError::ReadError`] on I/O failure.
pub fn disc_id(reader: &DiscReader) -> Result<DiscId, AacsError> {
    let data = read_unit_key_data(reader)?;
    Ok(disc_id_from_data(&data))
}

// ── Internal functions ─────────────────────────────────────────────────────

/// Parses `Unit_Key_RO.inf` to extract encrypted unit keys.
fn parse_unit_key_file(data: &[u8]) -> Result<UnitKeyFile, AacsError> {
    // Minimum: 4 bytes for uk_pos
    if data.len() < 4 {
        return Err(AacsError::InvalidUnitKeyFile {
            reason: "file too short for header".to_owned(),
        });
    }

    // uk_pos: u32 BE at offset 0
    let uk_pos = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;

    // Key table starts at uk_pos, needs at least 2 bytes for num_uk
    if uk_pos + 2 > data.len() {
        return Err(AacsError::InvalidUnitKeyFile {
            reason: format!(
                "uk_pos ({uk_pos}) points beyond file length ({})",
                data.len()
            ),
        });
    }

    let num_uk = u16::from_be_bytes([data[uk_pos], data[uk_pos + 1]]) as usize;

    if num_uk == 0 {
        return Err(AacsError::InvalidUnitKeyFile {
            reason: "no unit keys in file".to_owned(),
        });
    }

    // Keys start at uk_pos + KEY_TABLE_HEADER_LEN, stride KEY_ENTRY_STRIDE
    let keys_start = uk_pos + KEY_TABLE_HEADER_LEN;
    let keys_end = keys_start + num_uk * KEY_ENTRY_STRIDE;

    if keys_end > data.len() {
        return Err(AacsError::InvalidUnitKeyFile {
            reason: format!(
                "key table requires {keys_end} bytes, file has {}",
                data.len()
            ),
        });
    }

    let mut encrypted_keys = Vec::with_capacity(num_uk);
    for i in 0..num_uk {
        let offset = keys_start + i * KEY_ENTRY_STRIDE;
        let mut key = [0u8; 16];
        key.copy_from_slice(&data[offset..offset + 16]);
        encrypted_keys.push(key);
    }

    Ok(UnitKeyFile { encrypted_keys })
}

/// Decrypts all unit keys using the VUK (AES-128-ECB, one block each).
fn decrypt_unit_keys(file: &UnitKeyFile, vuk: &[u8; 16]) -> Vec<[u8; 16]> {
    let cipher = Aes128::new(vuk.into());
    file.encrypted_keys
        .iter()
        .map(|encrypted| {
            let mut block = aes::Block::clone_from_slice(encrypted);
            cipher.decrypt_block(&mut block);
            let mut key = [0u8; 16];
            key.copy_from_slice(&block);
            key
        })
        .collect()
}

/// Finds the byte offset of the first encrypted aligned unit, or `None`.
fn find_first_encrypted_block(data: &[u8]) -> Option<usize> {
    let mut offset = 0;
    while offset + ALIGNED_UNIT_LEN <= data.len() {
        if data[offset] & 0xC0 != 0 {
            return Some(offset);
        }
        offset += ALIGNED_UNIT_LEN;
    }
    None
}

/// Tries each unit key on a block, returns the index of the one that
/// produces valid MPEG-TS sync bytes.
fn find_unit_key(block: &[u8], keys: &[[u8; 16]]) -> Option<usize> {
    for (i, key) in keys.iter().enumerate() {
        let mut trial = [0u8; ALIGNED_UNIT_LEN];
        trial.copy_from_slice(block);
        decrypt_block(&mut trial, key);
        if verify_ts_sync(&trial) {
            return Some(i);
        }
    }
    None
}

/// Decrypts all encrypted aligned units in `data` in-place.
fn decrypt_m2ts(data: &mut [u8], unit_key: &[u8; 16]) {
    let mut offset = 0;
    while offset + ALIGNED_UNIT_LEN <= data.len() {
        if data[offset] & 0xC0 != 0 {
            let block = &mut data[offset..offset + ALIGNED_UNIT_LEN];
            decrypt_block(block, unit_key);
        }
        offset += ALIGNED_UNIT_LEN;
    }
}

/// Decrypts a single 6144-byte aligned unit in-place.
///
/// 1. Derive per-block key: `AES-ECB-encrypt(unit_key, block[0..16]) XOR block[0..16]`
/// 2. CBC-decrypt `block[16..6144]` with derived key and `AACS_IV`.
/// 3. Clear encryption flag.
fn decrypt_block(block: &mut [u8], unit_key: &[u8; 16]) {
    // Derive per-block key
    let cipher = Aes128::new(unit_key.into());
    let mut derived_key_block = aes::Block::clone_from_slice(&block[..16]);
    cipher.encrypt_block(&mut derived_key_block);

    let mut derived_key = [0u8; 16];
    for (i, byte) in derived_key.iter_mut().enumerate() {
        *byte = derived_key_block[i] ^ block[i];
    }

    // CBC-decrypt the content portion (bytes 16..6144).
    // Input is always (ALIGNED_UNIT_LEN - 16) bytes = a multiple of the AES
    // block size, so NoPadding unpadding cannot fail.
    let decryptor = CbcDecryptor::<Aes128>::new(&derived_key.into(), &AACS_IV.into());
    let _ = decryptor.decrypt_padded_mut::<NoPadding>(&mut block[16..ALIGNED_UNIT_LEN]);

    // Clear encryption flag
    block[0] &= !0xC0;
}

/// Verifies MPEG-TS sync bytes in a decrypted aligned unit.
///
/// Each 192-byte packet has a 4-byte `TP_extra_header` followed by a
/// 188-byte TS packet. The sync byte (0x47) is at offset 4 within each
/// 192-byte packet.
fn verify_ts_sync(block: &[u8]) -> bool {
    for i in 0..SYNC_CHECK_COUNT {
        let offset = i * 192 + 4;
        if offset >= block.len() || block[offset] != 0x47 {
            return false;
        }
    }
    true
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
pub(crate) mod tests {
    use cbc::Encryptor as CbcEncryptor;
    use cipher::BlockEncryptMut;
    use std::io::{Read as _, Seek, SeekFrom, Write as _};

    use super::*;

    impl AacsKeys {
        fn from_keys(unit_keys: Vec<[u8; 16]>) -> Self {
            Self { unit_keys }
        }
    }

    // ── Test helpers ───────────────────────────────────────────────────

    /// Builds a valid `Unit_Key_RO.inf` binary.
    pub struct UnitKeyBuilder {
        keys: Vec<[u8; 16]>,
        first_play_unit: u16,
        top_menu_unit: u16,
        title_units: Vec<u16>,
    }

    impl UnitKeyBuilder {
        pub fn new() -> Self {
            Self {
                keys: Vec::new(),
                first_play_unit: 1,
                top_menu_unit: 1,
                title_units: Vec::new(),
            }
        }

        pub fn unit_key(mut self, key: [u8; 16]) -> Self {
            self.keys.push(key);
            self
        }

        pub fn first_play(mut self, unit: u16) -> Self {
            self.first_play_unit = unit;
            self
        }

        pub fn top_menu(mut self, unit: u16) -> Self {
            self.top_menu_unit = unit;
            self
        }

        pub fn title(mut self, cps_unit: u16) -> Self {
            self.title_units.push(cps_unit);
            self
        }

        #[allow(
            clippy::cast_possible_truncation,
            reason = "test builder — values are always small"
        )]
        pub fn build(&self) -> Vec<u8> {
            // Title mapping section size
            let title_section_size = 6 + self.title_units.len() * 4;
            // uk_pos is after: 20-byte header + title mapping section
            let uk_pos: u32 = (20 + title_section_size) as u32;
            let key_table_size = KEY_TABLE_HEADER_LEN + self.keys.len() * KEY_ENTRY_STRIDE;
            let total = uk_pos as usize + key_table_size;

            let mut data = vec![0u8; total];

            // uk_pos at offset 0
            data[0..4].copy_from_slice(&uk_pos.to_be_bytes());

            // Title mapping at offset 20
            data[20..22].copy_from_slice(&self.first_play_unit.to_be_bytes());
            data[22..24].copy_from_slice(&self.top_menu_unit.to_be_bytes());
            let num_titles = self.title_units.len() as u16;
            data[24..26].copy_from_slice(&num_titles.to_be_bytes());
            for (i, &unit) in self.title_units.iter().enumerate() {
                let offset = 26 + i * 4 + 2; // CPS unit at +2 within entry
                data[offset..offset + 2].copy_from_slice(&unit.to_be_bytes());
            }

            // Key table at uk_pos
            let uk_start = uk_pos as usize;
            let num_uk = self.keys.len() as u16;
            data[uk_start..uk_start + 2].copy_from_slice(&num_uk.to_be_bytes());

            // Key entries at uk_pos + 48, stride 48
            for (i, key) in self.keys.iter().enumerate() {
                let offset = uk_start + KEY_TABLE_HEADER_LEN + i * KEY_ENTRY_STRIDE;
                data[offset..offset + 16].copy_from_slice(key);
            }

            data
        }
    }

    /// Builds an encrypted 6144-byte aligned unit from known plaintext.
    struct EncryptedBlockBuilder {
        unit_key: [u8; 16],
        /// 32 TS packets of 192 bytes each, with valid sync bytes.
        plaintext: [u8; ALIGNED_UNIT_LEN],
    }

    impl EncryptedBlockBuilder {
        fn new(unit_key: [u8; 16]) -> Self {
            // Build valid TS packet data: sync byte at offset 4 of each
            // 192-byte packet
            let mut plaintext = [0u8; ALIGNED_UNIT_LEN];
            // Set TP_extra_header byte 0 with encryption flag (will be set
            // during build)
            for i in 0..32 {
                plaintext[i * 192 + 4] = 0x47; // TS sync byte
            }
            Self {
                unit_key,
                plaintext,
            }
        }

        /// Sets the first 16 bytes (the plaintext seed used for key derivation).
        fn seed(mut self, seed: [u8; 16]) -> Self {
            self.plaintext[..16].copy_from_slice(&seed);
            self
        }

        fn build(&self) -> [u8; ALIGNED_UNIT_LEN] {
            let mut block = self.plaintext;

            // Set encryption flag and ensure valid TS structure
            block[0] |= 0xC0;
            block[4] = 0x47; // TS sync byte must survive seed

            // Derive per-block key (same algorithm as decrypt, but we encrypt
            // to produce the ciphertext)
            let cipher = Aes128::new((&self.unit_key).into());
            let mut derived_key_block = aes::Block::clone_from_slice(&block[..16]);
            cipher.encrypt_block(&mut derived_key_block);

            let mut derived_key = [0u8; 16];
            for (i, byte) in derived_key.iter_mut().enumerate() {
                *byte = derived_key_block[i] ^ block[i];
            }

            // CBC-encrypt the content portion (bytes 16..6144)
            let encryptor = CbcEncryptor::<Aes128>::new(&derived_key.into(), &AACS_IV.into());
            encryptor
                .encrypt_padded_mut::<NoPadding>(&mut block[16..], ALIGNED_UNIT_LEN - 16)
                .expect("encryption should succeed on aligned data");

            block
        }
    }

    // ── Unit_Key_RO.inf parser tests ──────────────────────────────────

    #[test]
    fn parse_single_key() {
        let key = [0xAA; 16];
        let data = UnitKeyBuilder::new()
            .unit_key(key)
            .first_play(1)
            .top_menu(1)
            .title(1)
            .build();

        let file = parse_unit_key_file(&data).expect("should parse valid file");
        assert_eq!(file.encrypted_keys.len(), 1, "should have 1 key");
        assert_eq!(file.encrypted_keys[0], key, "key bytes should match");
    }

    #[test]
    fn parse_multiple_keys() {
        let data = UnitKeyBuilder::new()
            .unit_key([0x11; 16])
            .unit_key([0x22; 16])
            .unit_key([0x33; 16])
            .title(1)
            .title(2)
            .build();

        let file = parse_unit_key_file(&data).expect("should parse valid file");
        assert_eq!(file.encrypted_keys.len(), 3, "should have 3 keys");
        assert_eq!(file.encrypted_keys[0], [0x11; 16], "key 0 should match");
        assert_eq!(file.encrypted_keys[1], [0x22; 16], "key 1 should match");
        assert_eq!(file.encrypted_keys[2], [0x33; 16], "key 2 should match");
    }

    #[test]
    fn reject_truncated_file() {
        let result = parse_unit_key_file(&[0x00, 0x01]);
        assert!(result.is_err(), "should reject file shorter than header");
    }

    #[test]
    fn reject_uk_pos_beyond_eof() {
        // uk_pos points to offset 1000, but file is only 100 bytes
        let mut data = vec![0u8; 100];
        data[0..4].copy_from_slice(&1000u32.to_be_bytes());

        let result = parse_unit_key_file(&data);
        assert!(result.is_err(), "should reject uk_pos beyond EOF");
    }

    // ── Unit key decryption tests ─────────────────────────────────────

    #[test]
    fn decrypt_unit_key_round_trip() {
        let vuk = [0x42; 16];
        let plaintext_key = [0xDE; 16];

        // Encrypt the plaintext key with the VUK to get what would be
        // stored on disc
        let cipher = Aes128::new((&vuk).into());
        let mut encrypted = aes::Block::clone_from_slice(&plaintext_key);
        cipher.encrypt_block(&mut encrypted);

        let mut stored_key = [0u8; 16];
        stored_key.copy_from_slice(&encrypted);

        let file = UnitKeyFile {
            encrypted_keys: vec![stored_key],
        };

        let decrypted = decrypt_unit_keys(&file, &vuk);
        assert_eq!(decrypted.len(), 1, "should have 1 decrypted key");
        assert_eq!(
            decrypted[0], plaintext_key,
            "decrypted key should match original"
        );
    }

    // ── Aligned unit decryption tests ─────────────────────────────────

    #[test]
    fn decrypt_block_round_trip() {
        let unit_key = [0x55; 16];
        let seed = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
            0x0F, 0x10,
        ];

        let builder = EncryptedBlockBuilder::new(unit_key).seed(seed);
        let expected_plaintext = builder.plaintext;
        let mut encrypted = builder.build();

        // Encryption flag should be set
        assert_ne!(
            encrypted[0] & 0xC0,
            0,
            "encryption flag should be set before decryption"
        );

        decrypt_block(&mut encrypted, &unit_key);

        // Encryption flag should be cleared
        assert_eq!(
            encrypted[0] & 0xC0,
            0,
            "encryption flag should be cleared after decryption"
        );

        // Content (bytes 16..) should match original plaintext
        assert_eq!(
            &encrypted[16..],
            &expected_plaintext[16..],
            "decrypted content should match original plaintext"
        );
    }

    #[test]
    fn skip_unencrypted_block() {
        let unit_key = [0x55; 16];
        let mut block = [0u8; ALIGNED_UNIT_LEN];
        // Set TS sync bytes but no encryption flag
        for i in 0..32 {
            block[i * 192 + 4] = 0x47;
        }

        let original = block;
        decrypt_m2ts(&mut block, &unit_key);

        assert_eq!(block, original, "unencrypted block should not be modified");
    }

    #[test]
    fn partial_final_block_untouched() {
        let unit_key = [0x55; 16];
        // Data that's 6144 + 100 bytes (one full block + partial)
        let mut data = vec![0u8; ALIGNED_UNIT_LEN + 100];
        // Fill the trailing bytes with a marker
        for byte in &mut data[ALIGNED_UNIT_LEN..] {
            *byte = 0xBE;
        }

        decrypt_m2ts(&mut data, &unit_key);

        // Trailing bytes should be untouched
        for (i, &byte) in data[ALIGNED_UNIT_LEN..].iter().enumerate() {
            assert_eq!(
                byte, 0xBE,
                "trailing byte at offset {i} should be untouched"
            );
        }
    }

    // ── TS sync verification tests ────────────────────────────────────

    #[test]
    fn valid_ts_sync() {
        let mut block = [0u8; ALIGNED_UNIT_LEN];
        for i in 0..32 {
            block[i * 192 + 4] = 0x47;
        }
        assert!(verify_ts_sync(&block), "valid sync bytes should pass");
    }

    #[test]
    fn invalid_ts_sync() {
        let block = [0u8; ALIGNED_UNIT_LEN]; // no sync bytes
        assert!(!verify_ts_sync(&block), "missing sync bytes should fail");
    }

    // ── Auto-detection tests ──────────────────────────────────────────

    #[test]
    fn find_correct_key_among_candidates() {
        let correct_key = [0x77; 16];
        let wrong_key_1 = [0x11; 16];
        let wrong_key_2 = [0x22; 16];

        let encrypted = EncryptedBlockBuilder::new(correct_key)
            .seed([0x01; 16])
            .build();

        let keys = vec![wrong_key_1, wrong_key_2, correct_key];
        let result = find_unit_key(&encrypted, &keys);
        assert_eq!(result, Some(2), "should find the correct key at index 2");
    }

    #[test]
    fn no_valid_key() {
        let real_key = [0x77; 16];
        let wrong_key_1 = [0x11; 16];
        let wrong_key_2 = [0x22; 16];

        let encrypted = EncryptedBlockBuilder::new(real_key)
            .seed([0x01; 16])
            .build();

        let keys = vec![wrong_key_1, wrong_key_2];
        let result = find_unit_key(&encrypted, &keys);
        assert_eq!(result, None, "should find no valid key");
    }

    // ── Integration tests (synthetic disc) ────────────────────────────

    #[test]
    fn decrypt_clip_full_round_trip() {
        let vuk = [0x42; 16];
        let plaintext_key = [0x77; 16];

        // Encrypt the unit key with the VUK (as stored on disc)
        let cipher = Aes128::new((&vuk).into());
        let mut encrypted_uk = aes::Block::clone_from_slice(&plaintext_key);
        cipher.encrypt_block(&mut encrypted_uk);
        let mut stored_key = [0u8; 16];
        stored_key.copy_from_slice(&encrypted_uk);

        // Build Unit_Key_RO.inf
        let uk_data = UnitKeyBuilder::new().unit_key(stored_key).title(1).build();

        // Build encrypted m2ts (2 blocks)
        let block1 = EncryptedBlockBuilder::new(plaintext_key)
            .seed([0x01; 16])
            .build();
        let block2 = EncryptedBlockBuilder::new(plaintext_key)
            .seed([0x02; 16])
            .build();

        let mut m2ts = Vec::with_capacity(ALIGNED_UNIT_LEN * 2);
        m2ts.extend_from_slice(&block1);
        m2ts.extend_from_slice(&block2);

        // Write to a temp directory
        let dir = tempfile::tempdir().expect("should create temp dir");
        let aacs_dir = dir.path().join("AACS");
        let stream_dir = dir.path().join("BDMV").join("STREAM");
        std::fs::create_dir_all(&aacs_dir).expect("should create AACS dir");
        std::fs::create_dir_all(&stream_dir).expect("should create STREAM dir");
        std::fs::write(aacs_dir.join("Unit_Key_RO.inf"), &uk_data)
            .expect("should write unit key file");
        std::fs::write(stream_dir.join("00100.m2ts"), &m2ts).expect("should write m2ts file");

        let reader = DiscReader::open(dir.path()).expect("should open disc");
        let decrypted = decrypt_clip(&reader, &vuk, "00100").expect("should decrypt");

        // Verify TS sync in both blocks
        assert_eq!(
            decrypted.len(),
            ALIGNED_UNIT_LEN * 2,
            "output size should match input"
        );
        for block_idx in 0..2 {
            let base = block_idx * ALIGNED_UNIT_LEN;
            for pkt in 0..32 {
                let offset = base + pkt * 192 + 4;
                assert_eq!(
                    decrypted[offset], 0x47,
                    "TS sync at block {block_idx} packet {pkt} should be 0x47"
                );
            }
        }
    }

    #[test]
    fn decrypt_clip_wrong_vuk() {
        let real_vuk = [0x42; 16];
        let wrong_vuk = [0xFF; 16];
        let plaintext_key = [0x77; 16];

        // Encrypt unit key with real VUK
        let cipher = Aes128::new((&real_vuk).into());
        let mut encrypted_uk = aes::Block::clone_from_slice(&plaintext_key);
        cipher.encrypt_block(&mut encrypted_uk);
        let mut stored_key = [0u8; 16];
        stored_key.copy_from_slice(&encrypted_uk);

        let uk_data = UnitKeyBuilder::new().unit_key(stored_key).title(1).build();
        let block = EncryptedBlockBuilder::new(plaintext_key)
            .seed([0x01; 16])
            .build();

        let dir = tempfile::tempdir().expect("should create temp dir");
        let aacs_dir = dir.path().join("AACS");
        let stream_dir = dir.path().join("BDMV").join("STREAM");
        std::fs::create_dir_all(&aacs_dir).expect("should create AACS dir");
        std::fs::create_dir_all(&stream_dir).expect("should create STREAM dir");
        std::fs::write(aacs_dir.join("Unit_Key_RO.inf"), &uk_data)
            .expect("should write unit key file");
        std::fs::write(stream_dir.join("00100.m2ts"), block).expect("should write m2ts file");

        let reader = DiscReader::open(dir.path()).expect("should open disc");
        let result = decrypt_clip(&reader, &wrong_vuk, "00100");
        assert!(
            matches!(result, Err(AacsError::DecryptionFailed { .. })),
            "wrong VUK should produce DecryptionFailed"
        );
    }

    #[test]
    fn decrypt_clip_no_unit_key_file() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let stream_dir = dir.path().join("BDMV").join("STREAM");
        std::fs::create_dir_all(&stream_dir).expect("should create STREAM dir");
        std::fs::write(stream_dir.join("00100.m2ts"), [0u8; ALIGNED_UNIT_LEN])
            .expect("should write m2ts file");

        let reader = DiscReader::open(dir.path()).expect("should open disc");
        let result = decrypt_clip(&reader, &[0u8; 16], "00100");
        assert!(
            matches!(result, Err(AacsError::NoUnitKeyFile)),
            "missing Unit_Key_RO.inf should produce NoUnitKeyFile"
        );
    }

    #[test]
    fn decrypt_clip_not_encrypted() {
        let vuk = [0x42; 16];
        let plaintext_key = [0x77; 16];

        // Encrypt unit key with VUK
        let cipher = Aes128::new((&vuk).into());
        let mut encrypted_uk = aes::Block::clone_from_slice(&plaintext_key);
        cipher.encrypt_block(&mut encrypted_uk);
        let mut stored_key = [0u8; 16];
        stored_key.copy_from_slice(&encrypted_uk);

        let uk_data = UnitKeyBuilder::new().unit_key(stored_key).title(1).build();

        // m2ts with no encryption flag set
        let mut m2ts = vec![0u8; ALIGNED_UNIT_LEN];
        for i in 0..32 {
            m2ts[i * 192 + 4] = 0x47;
        }

        let dir = tempfile::tempdir().expect("should create temp dir");
        let aacs_dir = dir.path().join("AACS");
        let stream_dir = dir.path().join("BDMV").join("STREAM");
        std::fs::create_dir_all(&aacs_dir).expect("should create AACS dir");
        std::fs::create_dir_all(&stream_dir).expect("should create STREAM dir");
        std::fs::write(aacs_dir.join("Unit_Key_RO.inf"), &uk_data)
            .expect("should write unit key file");
        std::fs::write(stream_dir.join("00100.m2ts"), &m2ts).expect("should write m2ts file");

        let reader = DiscReader::open(dir.path()).expect("should open disc");
        let result = decrypt_clip(&reader, &vuk, "00100");
        assert!(
            matches!(result, Err(AacsError::NotEncrypted { .. })),
            "unencrypted clip should produce NotEncrypted"
        );
    }

    // ── AacsKeys tests ──────────────────────────────────────────────────

    #[test]
    fn aacs_keys_from_disc_and_decrypt_clip() {
        let vuk = [0x42; 16];
        let plaintext_key = [0x77; 16];

        let cipher = Aes128::new((&vuk).into());
        let mut encrypted_uk = aes::Block::clone_from_slice(&plaintext_key);
        cipher.encrypt_block(&mut encrypted_uk);
        let mut stored_key = [0u8; 16];
        stored_key.copy_from_slice(&encrypted_uk);

        let uk_data = UnitKeyBuilder::new().unit_key(stored_key).title(1).build();
        let block = EncryptedBlockBuilder::new(plaintext_key)
            .seed([0x01; 16])
            .build();

        let dir = tempfile::tempdir().expect("should create temp dir");
        let aacs_dir = dir.path().join("AACS");
        let stream_dir = dir.path().join("BDMV").join("STREAM");
        std::fs::create_dir_all(&aacs_dir).expect("should create AACS dir");
        std::fs::create_dir_all(&stream_dir).expect("should create STREAM dir");
        std::fs::write(aacs_dir.join("Unit_Key_RO.inf"), &uk_data)
            .expect("should write unit key file");
        std::fs::write(stream_dir.join("00100.m2ts"), block).expect("should write m2ts file");

        let reader = DiscReader::open(dir.path()).expect("should open disc");
        let keys = AacsKeys::from_disc(&reader, &vuk).expect("should parse keys");
        let decrypted = keys
            .decrypt_clip(&reader, "00100")
            .expect("should decrypt via AacsKeys");

        assert_eq!(
            decrypted.len(),
            ALIGNED_UNIT_LEN,
            "output size should match input"
        );
        for pkt in 0..32 {
            let offset = pkt * 192 + 4;
            assert_eq!(
                decrypted[offset], 0x47,
                "TS sync at packet {pkt} should be 0x47"
            );
        }
    }

    // ── decrypt_stream tests ────────────────────────────────────────────

    #[test]
    fn decrypt_stream_encrypted_blocks() {
        let unit_key = [0x55; 16];
        let keys = AacsKeys::from_keys(vec![unit_key]);

        let block1 = EncryptedBlockBuilder::new(unit_key)
            .seed([0x01; 16])
            .build();
        let block2 = EncryptedBlockBuilder::new(unit_key)
            .seed([0x02; 16])
            .build();

        let mut file = tempfile::tempfile().expect("should create temp file");
        file.write_all(&block1).expect("should write block1");
        file.write_all(&block2).expect("should write block2");

        let length = (ALIGNED_UNIT_LEN * 2) as u64;
        let stats = keys
            .decrypt_stream(&mut file, 0, length)
            .expect("should decrypt");

        assert_eq!(stats.blocks_decrypted, 2, "should decrypt 2 blocks");
        assert_eq!(stats.blocks_skipped, 0, "should skip 0 blocks");

        // Verify file contents
        file.seek(SeekFrom::Start(0)).expect("should seek");
        let mut result = vec![0u8; ALIGNED_UNIT_LEN * 2];
        file.read_exact(&mut result).expect("should read");

        for block_idx in 0..2 {
            let base = block_idx * ALIGNED_UNIT_LEN;
            assert_eq!(
                result[base] & 0xC0,
                0,
                "encryption flag should be cleared in block {block_idx}"
            );
            for pkt in 0..32 {
                let offset = base + pkt * 192 + 4;
                assert_eq!(
                    result[offset], 0x47,
                    "TS sync at block {block_idx} packet {pkt} should be 0x47"
                );
            }
        }
    }

    #[test]
    fn decrypt_stream_mixed_blocks() {
        let unit_key = [0x55; 16];
        let keys = AacsKeys::from_keys(vec![unit_key]);

        // Block 1: unencrypted
        let mut unencrypted = [0u8; ALIGNED_UNIT_LEN];
        for i in 0..32 {
            unencrypted[i * 192 + 4] = 0x47;
        }
        // Block 2: encrypted
        let encrypted = EncryptedBlockBuilder::new(unit_key)
            .seed([0x01; 16])
            .build();

        let mut file = tempfile::tempfile().expect("should create temp file");
        file.write_all(&unencrypted)
            .expect("should write unencrypted");
        file.write_all(&encrypted).expect("should write encrypted");

        let length = (ALIGNED_UNIT_LEN * 2) as u64;
        let stats = keys
            .decrypt_stream(&mut file, 0, length)
            .expect("should decrypt");

        assert_eq!(stats.blocks_decrypted, 1, "should decrypt 1 block");
        assert_eq!(stats.blocks_skipped, 1, "should skip 1 block");
    }

    #[test]
    fn decrypt_stream_all_unencrypted() {
        let unit_key = [0x55; 16];
        let keys = AacsKeys::from_keys(vec![unit_key]);

        let mut unencrypted = [0u8; ALIGNED_UNIT_LEN];
        for i in 0..32 {
            unencrypted[i * 192 + 4] = 0x47;
        }

        let mut file = tempfile::tempfile().expect("should create temp file");
        file.write_all(&unencrypted).expect("should write block");

        let stats = keys
            .decrypt_stream(&mut file, 0, ALIGNED_UNIT_LEN as u64)
            .expect("should not error on unencrypted");

        assert_eq!(stats.blocks_decrypted, 0, "should decrypt 0 blocks");
        assert_eq!(stats.blocks_skipped, 1, "should skip 1 block");
    }

    #[test]
    fn decrypt_stream_file_size_unchanged() {
        let unit_key = [0x55; 16];
        let keys = AacsKeys::from_keys(vec![unit_key]);

        let block = EncryptedBlockBuilder::new(unit_key)
            .seed([0x01; 16])
            .build();
        let original_len = block.len() as u64;

        let mut file = tempfile::tempfile().expect("should create temp file");
        file.write_all(&block).expect("should write block");

        keys.decrypt_stream(&mut file, 0, original_len)
            .expect("should decrypt");

        let new_len = file.seek(SeekFrom::End(0)).expect("should seek to end");
        assert_eq!(new_len, original_len, "file size should not change");
    }

    // ── Disc ID tests ──────────────────────────────────────────────────

    #[test]
    fn disc_id_from_synthetic_unit_key_file() {
        let uk_data = UnitKeyBuilder::new().unit_key([0xAA; 16]).title(1).build();

        // Compute expected SHA-1
        let expected_hash = sha1::Sha1::digest(&uk_data);
        let expected_id: String = expected_hash.iter().fold(String::new(), |mut acc, b| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{b:02x}");
            acc
        });

        // Write to a synthetic disc
        let dir = tempfile::tempdir().expect("should create temp dir");
        let aacs_dir = dir.path().join("AACS");
        std::fs::create_dir_all(&aacs_dir).expect("should create AACS dir");
        std::fs::write(aacs_dir.join("Unit_Key_RO.inf"), &uk_data)
            .expect("should write unit key file");

        let reader = DiscReader::open(dir.path()).expect("should open disc");
        let result = disc_id(&reader).expect("should compute disc ID");

        assert_eq!(
            result.id, expected_id,
            "disc ID should match SHA-1 of Unit_Key_RO.inf"
        );
        assert_eq!(result.id.len(), 40, "disc ID should be 40 hex characters");
    }

    #[test]
    fn disc_id_no_unit_key_file() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        std::fs::create_dir_all(dir.path().join("BDMV/STREAM")).expect("should create STREAM dir");

        let reader = DiscReader::open(dir.path()).expect("should open disc");
        let result = disc_id(&reader);
        assert!(
            matches!(result, Err(AacsError::NoUnitKeyFile)),
            "should return NoUnitKeyFile when Unit_Key_RO.inf is missing"
        );
    }
}
