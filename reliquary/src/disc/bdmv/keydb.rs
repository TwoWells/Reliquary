// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! KEYDB.cfg parser — looks up a VUK by disc ID from the libaacs community
//! key database.
//!
//! The parser reads the file line-by-line (streaming) to avoid loading the
//! full 60 MB file into memory.

use std::io::BufRead;
use std::path::{Path, PathBuf};

use thiserror::Error;

use super::aacs::DiscId;

// ── Errors ─────────────────────────────────────────────────────────────────

/// Errors from KEYDB.cfg lookup.
#[derive(Debug, Error)]
pub enum KeydbError {
    /// KEYDB.cfg not found at the expected path.
    #[error("KEYDB.cfg not found at {path}")]
    NotFound {
        /// The path that was checked.
        path: PathBuf,
    },

    /// KEYDB.cfg could not be read.
    #[error("failed to read KEYDB.cfg at {path}: {source}")]
    ReadError {
        /// The path that was read.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },

    /// A matched line had no valid VUK field.
    #[error("disc ID {disc_id} found in KEYDB.cfg but VUK field is missing or malformed")]
    MalformedEntry {
        /// The disc ID that matched.
        disc_id: String,
    },
}

// ── Public API ─────────────────────────────────────────────────────────────

/// Returns the default KEYDB.cfg path (`$XDG_CONFIG_HOME/aacs/KEYDB.cfg`).
///
/// Falls back to `$HOME/.config/aacs/KEYDB.cfg` if `XDG_CONFIG_HOME` is
/// not set.
pub fn default_keydb_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME").map_or_else(
        || {
            let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("~"), PathBuf::from);
            home.join(".config")
        },
        PathBuf::from,
    );
    base.join("aacs").join("KEYDB.cfg")
}

/// Looks up a VUK from KEYDB.cfg by disc ID.
///
/// Opens the file and reads line-by-line (streaming). Returns `Ok(Some(vuk))`
/// on the first match, `Ok(None)` if the disc ID is not found.
///
/// # Errors
///
/// Returns [`KeydbError::NotFound`] if the file doesn't exist,
/// [`KeydbError::ReadError`] on I/O failure, or [`KeydbError::MalformedEntry`]
/// if the disc ID is found but the VUK field can't be parsed.
pub fn lookup_keydb(keydb_path: &Path, disc_id: &DiscId) -> Result<Option<[u8; 16]>, KeydbError> {
    let file = std::fs::File::open(keydb_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            KeydbError::NotFound {
                path: keydb_path.to_path_buf(),
            }
        } else {
            KeydbError::ReadError {
                path: keydb_path.to_path_buf(),
                source: e,
            }
        }
    })?;

    let reader = std::io::BufReader::new(file);

    for line in reader.lines() {
        let line = line.map_err(|e| KeydbError::ReadError {
            path: keydb_path.to_path_buf(),
            source: e,
        })?;

        let trimmed = line.trim();

        // Skip empty lines and comments
        if trimmed.is_empty() || trimmed.starts_with(';') {
            continue;
        }

        // Skip device keys, processing keys, and host certificates
        if trimmed.starts_with("| DK |")
            || trimmed.starts_with("| PK |")
            || trimmed.starts_with("| HC |")
        {
            continue;
        }

        // Check if line starts with our disc ID (case-insensitive)
        if trimmed.len() >= disc_id.id.len()
            && trimmed[..disc_id.id.len()].eq_ignore_ascii_case(&disc_id.id)
        {
            return extract_vuk(trimmed, &disc_id.id).map(Some);
        }
    }

    Ok(None)
}

// ── Internal ───────────────────────────────────────────────────────────────

/// Extracts the VUK from a matched KEYDB.cfg line.
///
/// Looks for `| V |` followed by `0x` and 32 hex characters.
fn extract_vuk(line: &str, disc_id: &str) -> Result<[u8; 16], KeydbError> {
    let vuk_marker = "| V |";
    let marker_pos = line
        .find(vuk_marker)
        .ok_or_else(|| KeydbError::MalformedEntry {
            disc_id: disc_id.to_owned(),
        })?;

    let after_marker = &line[marker_pos + vuk_marker.len()..];
    let after_marker = after_marker.trim_start();

    // Strip optional 0x prefix
    let hex_start = if after_marker.starts_with("0x") || after_marker.starts_with("0X") {
        &after_marker[2..]
    } else {
        after_marker
    };

    if hex_start.len() < 32 {
        return Err(KeydbError::MalformedEntry {
            disc_id: disc_id.to_owned(),
        });
    }

    let hex_str = &hex_start[..32];
    parse_hex_vuk(hex_str).ok_or_else(|| KeydbError::MalformedEntry {
        disc_id: disc_id.to_owned(),
    })
}

/// Parses 32 hex characters into a 16-byte VUK.
fn parse_hex_vuk(hex: &str) -> Option<[u8; 16]> {
    if hex.len() != 32 {
        return None;
    }
    let mut vuk = [0u8; 16];
    for (i, byte) in vuk.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(vuk)
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod tests {
    use super::*;

    /// Builds a synthetic KEYDB.cfg file with the given entries.
    fn write_keydb(dir: &Path, content: &str) -> PathBuf {
        let path = dir.join("KEYDB.cfg");
        std::fs::write(&path, content).expect("should write KEYDB.cfg");
        path
    }

    fn disc_id(hex: &str) -> DiscId {
        DiscId { id: hex.to_owned() }
    }

    #[test]
    fn lookup_finds_correct_vuk() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let content = "\
; comment line
0000000000000000000000000000000000000001 = Title A | D | 2020-01-01 | M | 00000000000000000000000000000000 | I | 00000000000000000000000000000000 | V | 0xaabbccdd11223344aabbccdd11223344 | U | 00000000000000000000000000000000 ; comment
0000000000000000000000000000000000000002 = Title B | D | 2020-01-01 | M | 00000000000000000000000000000000 | I | 00000000000000000000000000000000 | V | 0x11111111111111111111111111111111 | U | 00000000000000000000000000000000 ; comment
0000000000000000000000000000000000000003 = Title C | D | 2020-01-01 | M | 00000000000000000000000000000000 | I | 00000000000000000000000000000000 | V | 0x22222222222222222222222222222222 | U | 00000000000000000000000000000000 ; comment
";

        let keydb_path = write_keydb(dir.path(), content);
        let id = disc_id("0000000000000000000000000000000000000002");

        let result = lookup_keydb(&keydb_path, &id).expect("should succeed");
        assert_eq!(result, Some([0x11; 16]), "should find VUK for disc ID 2");
    }

    #[test]
    fn lookup_not_found() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let content = "\
0000000000000000000000000000000000000001 = Title A | D | 2020-01-01 | M | 00000000000000000000000000000000 | I | 00000000000000000000000000000000 | V | 0xaabbccdd11223344aabbccdd11223344 | U | 00000000000000000000000000000000 ; comment
";

        let keydb_path = write_keydb(dir.path(), content);
        let id = disc_id("ffffffffffffffffffffffffffffffffffffffff");

        let result = lookup_keydb(&keydb_path, &id).expect("should succeed");
        assert_eq!(result, None, "should return None for missing disc ID");
    }

    #[test]
    fn lookup_skips_comments_and_key_lines() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let content = "\
; This is a comment
| DK | AACS_DK_1234 | 00000000000000000000000000000000
| PK | AACS_PK_5678 | 00000000000000000000000000000000
| HC | AACS_HC_9abc | 00000000000000000000000000000000 | 00000000000000000000000000000000
aabbccddaabbccddaabbccddaabbccddaabbccdd = My Disc | D | 2020-01-01 | M | 00000000000000000000000000000000 | I | 00000000000000000000000000000000 | V | 0x99887766554433221100ffeeddccbbaa | U | 00000000000000000000000000000000 ; comment
";

        let keydb_path = write_keydb(dir.path(), content);
        let id = disc_id("aabbccddaabbccddaabbccddaabbccddaabbccdd");

        let result = lookup_keydb(&keydb_path, &id).expect("should succeed");
        let expected: [u8; 16] = [
            0x99, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x00, 0xFF, 0xEE, 0xDD, 0xCC,
            0xBB, 0xAA,
        ];
        assert_eq!(
            result,
            Some(expected),
            "should find VUK after skipping non-entry lines"
        );
    }

    #[test]
    fn lookup_case_insensitive_disc_id() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        // Disc ID in uppercase in file, lowercase in query
        let content = "\
AABBCCDDAABBCCDDAABBCCDDAABBCCDDAABBCCDD = Title | D | 2020-01-01 | M | 00000000000000000000000000000000 | I | 00000000000000000000000000000000 | V | 0x11223344556677889900aabbccddeeff | U | 00000000000000000000000000000000 ; comment
";

        let keydb_path = write_keydb(dir.path(), content);
        let id = disc_id("aabbccddaabbccddaabbccddaabbccddaabbccdd");

        let result = lookup_keydb(&keydb_path, &id).expect("should succeed");
        let expected: [u8; 16] = [
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0x00, 0xAA, 0xBB, 0xCC, 0xDD,
            0xEE, 0xFF,
        ];
        assert_eq!(result, Some(expected), "should match case-insensitively");
    }

    #[test]
    fn lookup_malformed_line_no_vuk_marker() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        // Line matches disc ID but has no | V | marker
        let content = "\
aabbccddaabbccddaabbccddaabbccddaabbccdd = Title | D | 2020-01-01 ; no VUK field
";

        let keydb_path = write_keydb(dir.path(), content);
        let id = disc_id("aabbccddaabbccddaabbccddaabbccddaabbccdd");

        let result = lookup_keydb(&keydb_path, &id);
        assert!(
            matches!(result, Err(KeydbError::MalformedEntry { .. })),
            "should return MalformedEntry for line without VUK marker"
        );
    }

    #[test]
    fn lookup_file_not_found() {
        let id = disc_id("0000000000000000000000000000000000000000");
        let result = lookup_keydb(Path::new("/nonexistent/KEYDB.cfg"), &id);
        assert!(
            matches!(result, Err(KeydbError::NotFound { .. })),
            "should return NotFound for missing file"
        );
    }

    #[test]
    fn default_keydb_path_uses_xdg() {
        // This test just verifies the path ends with the expected suffix.
        // The actual XDG_CONFIG_HOME value varies by environment.
        let path = default_keydb_path();
        assert!(
            path.ends_with("aacs/KEYDB.cfg"),
            "default path should end with aacs/KEYDB.cfg, got: {}",
            path.display()
        );
    }

    #[test]
    fn lookup_vuk_without_0x_prefix() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let content = "\
aabbccddaabbccddaabbccddaabbccddaabbccdd = Title | D | 2020-01-01 | M | 00000000000000000000000000000000 | I | 00000000000000000000000000000000 | V | aabbccddeeff00112233445566778899 | U | 00000000000000000000000000000000 ; comment
";

        let keydb_path = write_keydb(dir.path(), content);
        let id = disc_id("aabbccddaabbccddaabbccddaabbccddaabbccdd");

        let result = lookup_keydb(&keydb_path, &id).expect("should succeed");
        let expected: [u8; 16] = [
            0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99,
        ];
        assert_eq!(result, Some(expected), "should parse VUK without 0x prefix");
    }
}
