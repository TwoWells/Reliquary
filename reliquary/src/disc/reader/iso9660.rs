// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Minimal ISO 9660 (ECMA-119) reader — directory listing and file extraction.
//!
//! Supports only what's needed for disc analysis: read the Primary Volume
//! Descriptor, traverse directory records, and extract file contents.
//! No Joliet, Rock Ridge, or El Torito support.
//!
//! File data is read on demand via seek — the ISO is never loaded into
//! memory in full.

use std::cell::RefCell;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::ReaderError;

/// Sector size in bytes (2048 for Mode 1 data tracks).
const SECTOR_SIZE: usize = 2048;

/// ISO 9660 magic identifier at the start of every volume descriptor.
const ISO_MAGIC: &[u8; 5] = b"CD001";

/// Location of the first volume descriptor (sector 16).
const SYSTEM_AREA_SECTORS: u64 = 16;

// ── Parsed structures ───────────────────────────────────────────────────

/// A directory entry parsed from an ISO 9660 directory record.
#[derive(Debug, Clone)]
struct DirEntry {
    /// File identifier (name), uppercase, with `;1` version suffix stripped.
    name: String,
    /// Logical block address of the extent (file data or subdirectory).
    extent_lba: u32,
    /// Size of the extent in bytes.
    extent_size: u32,
    /// Whether this entry is a directory.
    is_dir: bool,
}

/// Read-only ISO 9660 filesystem backed by a seekable file handle.
pub struct Iso9660Reader {
    /// File handle — wrapped in `RefCell` for interior mutability so that
    /// `read_file` / `read_dir` can take `&self`.
    file: RefCell<File>,
    /// Path to the ISO (for error messages only).
    path: String,
    /// Root directory extent LBA.
    root_lba: u32,
    /// Root directory extent size.
    root_size: u32,
}

impl Iso9660Reader {
    /// Opens an ISO 9660 image from a file handle.
    ///
    /// Reads only the Primary Volume Descriptor (sector 16+) on open —
    /// the rest of the image is accessed on demand.
    ///
    /// # Errors
    ///
    /// Returns [`ReaderError::IsoParse`] if the image doesn't contain a
    /// valid Primary Volume Descriptor.
    pub fn new(mut file: File, path: &Path) -> Result<Self, ReaderError> {
        let path_str = path.display().to_string();
        let (root_lba, root_size) = parse_pvd(&mut file, &path_str)?;
        Ok(Self {
            file: RefCell::new(file),
            path: path_str,
            root_lba,
            root_size,
        })
    }

    /// Reads a file by relative path within the ISO.
    pub fn read_file(&self, rel: &Path) -> Result<Vec<u8>, ReaderError> {
        let entry = self.resolve_path(rel)?;
        if entry.is_dir {
            return Err(ReaderError::NotFound {
                path: rel.display().to_string(),
            });
        }
        self.read_extent(entry.extent_lba, entry.extent_size)
    }

    /// Lists entries in a directory within the ISO.
    pub fn read_dir(&self, rel: &Path) -> Result<Vec<String>, ReaderError> {
        let (lba, size) = if rel.as_os_str().is_empty() || rel == Path::new(".") {
            (self.root_lba, self.root_size)
        } else {
            let entry = self.resolve_path(rel)?;
            if !entry.is_dir {
                return Err(ReaderError::NotFound {
                    path: rel.display().to_string(),
                });
            }
            (entry.extent_lba, entry.extent_size)
        };

        let entries = self.read_directory(lba, size)?;
        let mut names: Vec<String> = entries.into_iter().map(|e| e.name).collect();
        names.sort();
        Ok(names)
    }

    /// Resolves a relative path to a directory entry by walking the tree.
    fn resolve_path(&self, rel: &Path) -> Result<DirEntry, ReaderError> {
        let mut current_lba = self.root_lba;
        let mut current_size = self.root_size;

        let components: Vec<&str> = rel
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(s) => s.to_str(),
                _ => None,
            })
            .collect();

        if components.is_empty() {
            return Ok(DirEntry {
                name: String::new(),
                extent_lba: self.root_lba,
                extent_size: self.root_size,
                is_dir: true,
            });
        }

        let mut found = None;
        for (i, component) in components.iter().enumerate() {
            let entries = self.read_directory(current_lba, current_size)?;
            let upper = component.to_uppercase();

            let entry = entries
                .into_iter()
                .find(|e| e.name.eq_ignore_ascii_case(&upper));

            match entry {
                Some(e) => {
                    if i < components.len() - 1 {
                        // Intermediate component must be a directory.
                        if !e.is_dir {
                            return Err(ReaderError::NotFound {
                                path: rel.display().to_string(),
                            });
                        }
                        current_lba = e.extent_lba;
                        current_size = e.extent_size;
                    } else {
                        found = Some(e);
                    }
                }
                None => {
                    return Err(ReaderError::NotFound {
                        path: rel.display().to_string(),
                    });
                }
            }
        }

        found.ok_or_else(|| ReaderError::NotFound {
            path: rel.display().to_string(),
        })
    }

    /// Reads raw bytes from an extent via seek.
    fn read_extent(&self, lba: u32, size: u32) -> Result<Vec<u8>, ReaderError> {
        let offset = u64::from(lba) * SECTOR_SIZE as u64;
        let mut file = self.file.borrow_mut();
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| ReaderError::Io {
                path: self.path.clone(),
                source: e,
            })?;
        let mut buf = vec![0u8; size as usize];
        file.read_exact(&mut buf).map_err(|e| ReaderError::Io {
            path: self.path.clone(),
            source: e,
        })?;
        Ok(buf)
    }

    /// Parses directory records from an extent.
    fn read_directory(&self, lba: u32, size: u32) -> Result<Vec<DirEntry>, ReaderError> {
        let raw = self.read_extent(lba, size)?;
        Ok(parse_directory_records(&raw))
    }
}

// ── ISO 9660 parsing helpers ────────────────────────────────────────────

/// Reads a little-endian u32 from a byte slice.
fn le_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

/// Parses the Primary Volume Descriptor and returns (root LBA, root size).
///
/// Reads sectors starting at 16, looking for a type-1 descriptor.
fn parse_pvd(file: &mut File, path: &str) -> Result<(u32, u32), ReaderError> {
    let mut sector = SYSTEM_AREA_SECTORS;
    let mut buf = [0u8; SECTOR_SIZE];

    loop {
        let offset = sector * SECTOR_SIZE as u64;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| ReaderError::Io {
                path: path.to_owned(),
                source: e,
            })?;

        if file.read(&mut buf).map_err(|e| ReaderError::Io {
            path: path.to_owned(),
            source: e,
        })? < SECTOR_SIZE
        {
            return Err(ReaderError::IsoParse {
                path: path.to_owned(),
                reason: "no Primary Volume Descriptor found".to_owned(),
            });
        }

        // Check magic at bytes 1..6.
        if buf[1..6] != *ISO_MAGIC {
            return Err(ReaderError::IsoParse {
                path: path.to_owned(),
                reason: format!("invalid volume descriptor magic at sector {sector}"),
            });
        }

        let vd_type = buf[0];

        match vd_type {
            // Type 1 = Primary Volume Descriptor
            1 => {
                // Root directory record is at offset 156, length 34.
                let root_lba = le_u32(&buf, 158);
                let root_size = le_u32(&buf, 166);
                return Ok((root_lba, root_size));
            }
            // Type 255 = Volume Descriptor Set Terminator
            255 => {
                return Err(ReaderError::IsoParse {
                    path: path.to_owned(),
                    reason: "no Primary Volume Descriptor found before terminator".to_owned(),
                });
            }
            // Skip supplementary (2), partition (3), etc.
            _ => {
                sector += 1;
            }
        }
    }
}

/// Parses directory records from raw directory extent data.
fn parse_directory_records(data: &[u8]) -> Vec<DirEntry> {
    let mut entries = Vec::new();
    let mut pos = 0;

    while pos < data.len() {
        let record_len = data[pos] as usize;
        if record_len == 0 {
            // Padding to next sector boundary.
            let next_sector = (pos / SECTOR_SIZE + 1) * SECTOR_SIZE;
            if next_sector >= data.len() {
                break;
            }
            pos = next_sector;
            continue;
        }

        if pos + record_len > data.len() {
            break;
        }

        let record = &data[pos..pos + record_len];
        let extent_lba = le_u32(record, 2);
        let extent_size = le_u32(record, 10);
        let flags = record[25];
        let is_dir = flags & 0x02 != 0;
        let name_len = record[32] as usize;

        if name_len > 0 && pos + 33 + name_len <= pos + record_len {
            let name_bytes = &record[33..33 + name_len];

            // Skip "." (0x00) and ".." (0x01) entries.
            if !(name_len == 1 && (name_bytes[0] == 0x00 || name_bytes[0] == 0x01)) {
                let raw_name = String::from_utf8_lossy(name_bytes);
                // Strip the ";1" version suffix.
                let name = raw_name
                    .split(';')
                    .next()
                    .unwrap_or(&raw_name)
                    .trim_end_matches('.')
                    .to_owned();

                if !name.is_empty() {
                    entries.push(DirEntry {
                        name,
                        extent_lba,
                        extent_size,
                        is_dir,
                    });
                }
            }
        }

        pos += record_len;
    }

    entries
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod tests {
    use std::io::Write;

    use super::*;

    /// Builds a minimal valid ISO 9660 image with a root directory.
    struct Iso9660Builder {
        sectors: Vec<[u8; SECTOR_SIZE]>,
    }

    impl Iso9660Builder {
        fn new() -> Self {
            // 18 sectors minimum: 16 system area + PVD at 16 + terminator at 17.
            Self {
                sectors: vec![[0u8; SECTOR_SIZE]; 18],
            }
        }

        /// Writes a file into the image at a new sector, returns its LBA.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "test images are always small"
        )]
        fn add_file(&mut self, contents: &[u8]) -> (u32, u32) {
            let lba = self.sectors.len() as u32;
            let sectors_needed = contents.len().div_ceil(SECTOR_SIZE);
            for i in 0..sectors_needed {
                let mut sector = [0u8; SECTOR_SIZE];
                let start = i * SECTOR_SIZE;
                let end = (start + SECTOR_SIZE).min(contents.len());
                if start < contents.len() {
                    sector[..end - start].copy_from_slice(&contents[start..end]);
                }
                self.sectors.push(sector);
            }
            (lba, contents.len() as u32)
        }

        /// Creates a directory extent with the given entries.
        /// Each entry is (name, `extent_lba`, `extent_size`, `is_dir`).
        fn add_directory(
            &mut self,
            self_lba_placeholder: u32,
            parent_lba: u32,
            entries: &[(&str, u32, u32, bool)],
        ) -> (u32, u32) {
            let mut dir_data = Vec::new();

            // "." entry
            write_dir_record(&mut dir_data, b"\x00", self_lba_placeholder, 0, true);
            // ".." entry
            write_dir_record(&mut dir_data, b"\x01", parent_lba, 0, true);

            for (name, lba, size, is_dir) in entries {
                write_dir_record(&mut dir_data, name.as_bytes(), *lba, *size, *is_dir);
            }

            self.add_file(&dir_data)
        }

        /// Builds the final image and writes it to an anonymous temp file.
        fn build(mut self, root_lba: u32, root_size: u32) -> File {
            // Write PVD at sector 16
            let pvd = &mut self.sectors[16];
            pvd[0] = 1; // Primary Volume Descriptor type
            pvd[1..6].copy_from_slice(ISO_MAGIC);
            pvd[6] = 1; // Version

            // Root directory record at offset 156
            let root_record = &mut pvd[156..190];
            root_record[0] = 34; // Record length
            root_record[2..6].copy_from_slice(&root_lba.to_le_bytes());
            root_record[10..14].copy_from_slice(&root_size.to_le_bytes());
            root_record[25] = 0x02; // Directory flag
            root_record[32] = 1; // Name length
            root_record[33] = 0x00; // "." identifier

            // Write terminator at sector 17
            let term = &mut self.sectors[17];
            term[0] = 255;
            term[1..6].copy_from_slice(ISO_MAGIC);

            let mut file = tempfile::tempfile().expect("should create temp file");
            for sector in &self.sectors {
                file.write_all(sector).expect("should write sector");
            }
            file.seek(SeekFrom::Start(0))
                .expect("should rewind temp file");
            file
        }
    }

    /// Writes a single ISO 9660 directory record into `buf`.
    fn write_dir_record(buf: &mut Vec<u8>, name: &[u8], lba: u32, size: u32, is_dir: bool) {
        let name_len = name.len();
        let padding = usize::from(name_len.is_multiple_of(2));
        #[allow(
            clippy::cast_possible_truncation,
            reason = "test directory records are always small"
        )]
        let record_len = (33 + name_len + padding) as u8;

        let start = buf.len();
        buf.resize(start + record_len as usize, 0);

        buf[start] = record_len;
        buf[start + 2..start + 6].copy_from_slice(&lba.to_le_bytes());
        buf[start + 6..start + 10].copy_from_slice(&lba.to_be_bytes());
        buf[start + 10..start + 14].copy_from_slice(&size.to_le_bytes());
        buf[start + 14..start + 18].copy_from_slice(&size.to_be_bytes());
        buf[start + 25] = if is_dir { 0x02 } else { 0x00 };
        #[allow(
            clippy::cast_possible_truncation,
            reason = "test names are always short"
        )]
        {
            buf[start + 32] = name_len as u8;
        }
        buf[start + 33..start + 33 + name_len].copy_from_slice(name);
    }

    #[test]
    fn parse_iso9660_root_directory() {
        let mut builder = Iso9660Builder::new();
        let (file_lba, file_size) = builder.add_file(b"test content");
        let (root_lba, root_size) =
            builder.add_directory(0, 0, &[("TESTFILE.TXT", file_lba, file_size, false)]);

        let file = builder.build(root_lba, root_size);
        let reader =
            Iso9660Reader::new(file, Path::new("test.iso")).expect("should parse ISO 9660");

        let entries = reader.read_dir(Path::new("")).expect("should list root");
        assert_eq!(
            entries,
            vec!["TESTFILE.TXT"],
            "root should contain test file"
        );
    }

    #[test]
    fn read_file_from_iso9660() {
        let mut builder = Iso9660Builder::new();
        let (file_lba, file_size) = builder.add_file(b"hello iso");
        let (root_lba, root_size) =
            builder.add_directory(0, 0, &[("HELLO.TXT", file_lba, file_size, false)]);

        let file = builder.build(root_lba, root_size);
        let reader =
            Iso9660Reader::new(file, Path::new("test.iso")).expect("should parse ISO 9660");

        let data = reader
            .read_file(Path::new("HELLO.TXT"))
            .expect("should read file");
        assert_eq!(data, b"hello iso", "file contents should match");
    }

    #[test]
    fn traverse_subdirectory() {
        let mut builder = Iso9660Builder::new();
        let (file_lba, file_size) = builder.add_file(b"nested data");
        let (sub_lba, sub_size) =
            builder.add_directory(0, 0, &[("DATA.BIN", file_lba, file_size, false)]);
        let (root_lba, root_size) =
            builder.add_directory(0, 0, &[("SUBDIR", sub_lba, sub_size, true)]);

        let file = builder.build(root_lba, root_size);
        let reader =
            Iso9660Reader::new(file, Path::new("test.iso")).expect("should parse ISO 9660");

        let entries = reader
            .read_dir(Path::new("SUBDIR"))
            .expect("should list subdir");
        assert_eq!(entries, vec!["DATA.BIN"], "subdir should contain data file");

        let data = reader
            .read_file(Path::new("SUBDIR/DATA.BIN"))
            .expect("should read nested file");
        assert_eq!(data, b"nested data", "nested file contents should match");
    }

    #[test]
    fn case_insensitive_lookup() {
        let mut builder = Iso9660Builder::new();
        let (file_lba, file_size) = builder.add_file(b"content");
        let (root_lba, root_size) =
            builder.add_directory(0, 0, &[("BDMV", file_lba, file_size, false)]);

        let file = builder.build(root_lba, root_size);
        let reader =
            Iso9660Reader::new(file, Path::new("test.iso")).expect("should parse ISO 9660");

        let data = reader
            .read_file(Path::new("bdmv"))
            .expect("should find file case-insensitively");
        assert_eq!(data, b"content", "case-insensitive lookup should work");
    }

    #[test]
    fn invalid_image_rejected() {
        let mut file = tempfile::tempfile().expect("should create temp file");
        file.write_all(&vec![0u8; SECTOR_SIZE * 17])
            .expect("should write garbage");
        file.seek(SeekFrom::Start(0)).expect("should rewind");
        let result = Iso9660Reader::new(file, Path::new("bad.iso"));
        assert!(result.is_err(), "garbage data should fail to parse");
    }
}
