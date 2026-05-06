// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Disc reader — abstracts file access so analysis code works on both
//! mounted directories and ISO image files.
//!
//! The public surface is [`DiscReader`], constructed via [`DiscReader::open`].

mod iso9660;
mod udf;

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

// ── Errors ──────────────────────────────────────────────────────────────

/// Errors from disc reader operations.
#[derive(Debug, Error)]
pub enum ReaderError {
    /// The path does not exist or is not accessible.
    #[error("path not found: {path}")]
    NotFound {
        /// The path that was not found.
        path: String,
    },

    /// The ISO image could not be parsed.
    #[error("failed to parse ISO image at {path}: {reason}")]
    IsoParse {
        /// Path to the ISO file.
        path: String,
        /// Description of the parse failure.
        reason: String,
    },

    /// An I/O error occurred during file access.
    #[error("I/O error accessing {path}: {source}")]
    Io {
        /// Path that triggered the error.
        path: String,
        /// Underlying I/O error.
        source: std::io::Error,
    },
}

// ── Reader ──────────────────────────────────────────────────────────────

/// Access files within a disc structure (mounted directory or ISO image).
///
/// Constructed via [`DiscReader::open`], which auto-detects the source type.
pub enum DiscReader {
    /// A mounted disc or extracted folder — delegates to `std::fs`.
    Directory(PathBuf),
    /// An ISO 9660 image (typically DVD).
    Iso9660(iso9660::Iso9660Reader),
    /// A UDF image (typically Blu-ray).
    Udf(udf::UdfReader),
}

impl DiscReader {
    /// Opens a disc from a path.
    ///
    /// If `path` is a directory, uses the directory backend. If it's a
    /// file, attempts to open it as an ISO image (trying UDF first, then
    /// falling back to ISO 9660).
    ///
    /// # Errors
    ///
    /// Returns [`ReaderError`] if the path doesn't exist, isn't a
    /// recognised format, or can't be read.
    pub fn open(path: &Path) -> Result<Self, ReaderError> {
        if path.is_dir() {
            return Ok(Self::Directory(path.to_path_buf()));
        }

        if path.is_file() {
            return Self::open_iso(path);
        }

        Err(ReaderError::NotFound {
            path: path.display().to_string(),
        })
    }

    /// Reads a file by relative path within the disc.
    ///
    /// # Errors
    ///
    /// Returns [`ReaderError`] if the file doesn't exist or can't be read.
    pub fn read_file(&self, rel: &Path) -> Result<Vec<u8>, ReaderError> {
        match self {
            Self::Directory(root) => {
                let full = root.join(rel);
                fs::read(&full).map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        ReaderError::NotFound {
                            path: full.display().to_string(),
                        }
                    } else {
                        ReaderError::Io {
                            path: full.display().to_string(),
                            source: e,
                        }
                    }
                })
            }
            Self::Iso9660(reader) => reader.read_file(rel),
            Self::Udf(reader) => reader.read_file(rel),
        }
    }

    /// Lists entries in a directory within the disc.
    ///
    /// Returns filenames (not full paths) sorted alphabetically.
    ///
    /// # Errors
    ///
    /// Returns [`ReaderError`] if the directory doesn't exist or can't be read.
    pub fn read_dir(&self, rel: &Path) -> Result<Vec<String>, ReaderError> {
        match self {
            Self::Directory(root) => {
                let full = root.join(rel);
                let entries = fs::read_dir(&full).map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        ReaderError::NotFound {
                            path: full.display().to_string(),
                        }
                    } else {
                        ReaderError::Io {
                            path: full.display().to_string(),
                            source: e,
                        }
                    }
                })?;

                let mut names: Vec<String> = Vec::new();
                for entry in entries {
                    let entry = entry.map_err(|e| ReaderError::Io {
                        path: full.display().to_string(),
                        source: e,
                    })?;
                    if let Some(name) = entry.file_name().to_str() {
                        names.push(name.to_owned());
                    }
                }
                names.sort();
                Ok(names)
            }
            Self::Iso9660(reader) => reader.read_dir(rel),
            Self::Udf(reader) => reader.read_dir(rel),
        }
    }

    /// Opens an ISO image file, trying UDF first (Blu-ray), then ISO 9660 (DVD).
    fn open_iso(path: &Path) -> Result<Self, ReaderError> {
        // Try UDF first — Blu-ray discs require it, and DVDs with UDF bridge
        // are better served by the UDF view.
        let file = Self::open_file(path)?;
        let udf_err = match udf::UdfReader::new(file, path) {
            Ok(reader) => return Ok(Self::Udf(reader)),
            Err(e) => e,
        };

        // Fall back to ISO 9660 (re-open since UDF consumed the handle).
        let file = Self::open_file(path)?;
        match iso9660::Iso9660Reader::new(file, path) {
            Ok(reader) => Ok(Self::Iso9660(reader)),
            // Both failed — report the UDF error since it's tried first and
            // more likely to be the intended format for an ISO file.
            Err(_iso_err) => Err(udf_err),
        }
    }

    /// Opens a file handle, mapping I/O errors to [`ReaderError`].
    fn open_file(path: &Path) -> Result<fs::File, ReaderError> {
        fs::File::open(path).map_err(|e| ReaderError::Io {
            path: path.display().to_string(),
            source: e,
        })
    }
}

impl std::fmt::Debug for DiscReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Directory(path) => f.debug_tuple("Directory").field(path).finish(),
            Self::Iso9660(_) => f.debug_tuple("Iso9660").field(&"..").finish(),
            Self::Udf(_) => f.debug_tuple("Udf").field(&"..").finish(),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod tests {
    use super::*;

    #[test]
    fn open_directory_backend() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let reader = DiscReader::open(dir.path()).expect("should open directory");
        assert!(
            matches!(reader, DiscReader::Directory(_)),
            "directory path should produce Directory variant"
        );
    }

    #[test]
    fn directory_read_file() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, b"hello").expect("should write test file");

        let reader = DiscReader::open(dir.path()).expect("should open directory");
        let data = reader
            .read_file(Path::new("test.txt"))
            .expect("should read file");
        assert_eq!(data, b"hello", "file contents should match");
    }

    #[test]
    fn directory_read_file_not_found() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let reader = DiscReader::open(dir.path()).expect("should open directory");
        let result = reader.read_file(Path::new("nonexistent.txt"));
        assert!(result.is_err(), "missing file should return error");
    }

    #[test]
    fn directory_read_dir() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        fs::write(dir.path().join("b.txt"), b"").expect("should write file");
        fs::write(dir.path().join("a.txt"), b"").expect("should write file");
        fs::create_dir(dir.path().join("subdir")).expect("should create subdir");

        let reader = DiscReader::open(dir.path()).expect("should open directory");
        let entries = reader
            .read_dir(Path::new(""))
            .expect("should list directory");
        assert_eq!(
            entries,
            vec!["a.txt", "b.txt", "subdir"],
            "entries should be sorted alphabetically"
        );
    }

    #[test]
    fn open_nonexistent_path() {
        let result = DiscReader::open(Path::new("/nonexistent/path"));
        assert!(result.is_err(), "nonexistent path should return error");
    }
}
