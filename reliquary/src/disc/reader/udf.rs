// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Minimal UDF (ECMA-167 / OSTA UDF 2.50) reader — directory listing and
//! file extraction for Blu-ray disc images.
//!
//! Implements just enough of the UDF specification to traverse directories
//! and read files: Anchor Volume Descriptor Pointer, Partition Descriptor,
//! Logical Volume Descriptor (including Type 2 Metadata Partition Maps),
//! File Set Descriptor, File Entry / Extended File Entry, File Identifier
//! Descriptors, and short/long allocation descriptors.
//!
//! UDF 2.50 Blu-ray discs use a Metadata Partition that stores the FSD,
//! directory entries, and file entries inside a Metadata File within the
//! physical partition. This reader handles that indirection transparently.
//!
//! File data is read on demand via seek — the ISO is never loaded into
//! memory in full.

use std::cell::RefCell;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::ReaderError;

/// Sector size for UDF volumes (always 2048 for optical media).
const SECTOR_SIZE: usize = 2048;

/// Sector number of the Anchor Volume Descriptor Pointer.
const AVDP_SECTOR: u64 = 256;

// ── ECMA-167 descriptor tag IDs ─────────────────────────────────────────

const TAG_PARTITION: u16 = 5;
const TAG_LOGICAL_VOLUME: u16 = 6;
const TAG_TERMINATING: u16 = 8;
const TAG_FILE_SET: u16 = 256;
const TAG_FILE_IDENTIFIER: u16 = 257;
const TAG_FILE_ENTRY: u16 = 261;
const TAG_EXTENDED_FILE_ENTRY: u16 = 266;

// ── ICB file type constants ─────────────────────────────────────────────

const ICB_FILE_TYPE_DIRECTORY: u8 = 4;

/// UDF Entity Identifier for the Metadata Partition Map.
const UDF_METADATA_ID: &[u8] = b"*UDF Metadata Partition";

// ── Parsed structures ───────────────────────────────────────────────────

/// A parsed descriptor tag (ECMA-167 §7.2).
#[derive(Debug, Clone, Copy)]
struct Tag {
    id: u16,
}

/// An extent: location + length for on-disc data.
#[derive(Debug, Clone, Copy)]
struct Extent {
    location: u32,
    length: u32,
}

/// A long allocation descriptor (ECMA-167 §7.8.14).
#[derive(Debug, Clone, Copy)]
struct LongAd {
    length: u32,
    location: u32,
    partition: u16,
}

/// A short allocation descriptor (ECMA-167 §7.8.8).
#[derive(Debug, Clone, Copy)]
struct ShortAd {
    length: u32,
    position: u32,
}

/// Information needed to resolve logical block addresses to byte offsets.
#[derive(Debug, Clone)]
struct PartitionInfo {
    /// Physical starting sector of the partition.
    start_sector: u32,
    /// Metadata partition mapping for UDF 2.50+ (Blu-ray).
    metadata: Option<MetadataMapping>,
}

/// Mapping for a UDF 2.50 Metadata Partition.
///
/// The Metadata File's allocation descriptors describe where the metadata
/// partition's logical blocks live within the physical partition.
#[derive(Debug, Clone)]
struct MetadataMapping {
    /// Partition reference number that uses this mapping.
    partition_ref: u16,
    /// Allocation extents of the Metadata File — each maps a contiguous
    /// run of metadata logical blocks to a physical sector range.
    extents: Vec<ShortAd>,
}

/// A directory entry from a File Identifier Descriptor.
#[derive(Debug, Clone)]
struct UdfDirEntry {
    name: String,
    icb_location: u32,
    icb_partition: u16,
}

/// Result of parsing the Main Volume Descriptor Sequence.
struct MvdsInfo {
    partition_start: u32,
    fsd_location: u32,
    fsd_partition: u16,
    metadata_file_location: Option<(u16, u32)>,
}

// ── Reader ──────────────────────────────────────────────────────────────

/// Read-only UDF filesystem backed by a seekable file handle.
pub struct UdfReader {
    /// File handle — wrapped in `RefCell` for interior mutability so that
    /// `read_file` / `read_dir` can take `&self`.
    file: RefCell<File>,
    /// Path to the ISO (for error messages only).
    path: String,
    /// Partition mapping info.
    partition: PartitionInfo,
    /// ICB location of the root directory.
    root_icb_location: u32,
    /// Partition index of the root directory ICB.
    root_icb_partition: u16,
}

impl UdfReader {
    /// Opens a UDF image from a file handle.
    ///
    /// Reads only the structural metadata on open (AVDP, MVDS, FSD) —
    /// file contents are accessed on demand via seek.
    ///
    /// # Errors
    ///
    /// Returns [`ReaderError::IsoParse`] if the image doesn't contain
    /// valid UDF structures.
    pub fn new(mut file: File, path: &Path) -> Result<Self, ReaderError> {
        let path_str = path.display().to_string();

        // 1. Read the Anchor Volume Descriptor Pointer at sector 256.
        let avdp = read_sector(&mut file, AVDP_SECTOR, &path_str)?;
        let tag = parse_tag(&avdp);
        if tag.id != 2 {
            return Err(ReaderError::IsoParse {
                path: path_str,
                reason: format!(
                    "expected AVDP tag (2) at sector {AVDP_SECTOR}, got {}",
                    tag.id
                ),
            });
        }

        // Main VDS extent at AVDP offset 16.
        let mvds_extent = parse_extent(&avdp, 16);

        // 2. Parse the Main Volume Descriptor Sequence.
        let mvds = parse_mvds(&mut file, mvds_extent, &path_str)?;

        // 3. Build partition info, handling metadata partitions if present.
        let partition = build_partition_info(&mut file, &mvds, &path_str)?;

        // 4. Read the File Set Descriptor.
        let fsd_offset = resolve_byte_offset(&partition, mvds.fsd_location, mvds.fsd_partition);
        let fsd = read_at(&mut file, fsd_offset, SECTOR_SIZE, &path_str)?;
        let fsd_tag = parse_tag(&fsd);
        if fsd_tag.id != TAG_FILE_SET {
            return Err(ReaderError::IsoParse {
                path: path_str,
                reason: format!("expected File Set Descriptor (256), got {}", fsd_tag.id),
            });
        }

        // Root directory ICB is a long_ad at FSD offset 400.
        let root_icb = parse_long_ad(&fsd, 400);

        Ok(Self {
            file: RefCell::new(file),
            path: path_str,
            partition,
            root_icb_location: root_icb.location,
            root_icb_partition: root_icb.partition,
        })
    }

    /// Reads a file by relative path within the UDF volume.
    pub fn read_file(&self, rel: &Path) -> Result<Vec<u8>, ReaderError> {
        let (icb_loc, icb_part) = self.resolve_path(rel)?;
        let fe = self.read_file_entry(icb_loc, icb_part)?;

        if fe.is_dir {
            return Err(ReaderError::NotFound {
                path: rel.display().to_string(),
            });
        }

        self.read_file_data(&fe, icb_part)
    }

    /// Lists entries in a directory within the UDF volume.
    pub fn read_dir(&self, rel: &Path) -> Result<Vec<String>, ReaderError> {
        let (icb_loc, icb_part) = if rel.as_os_str().is_empty() || rel == Path::new(".") {
            (self.root_icb_location, self.root_icb_partition)
        } else {
            self.resolve_path(rel)?
        };

        let fe = self.read_file_entry(icb_loc, icb_part)?;
        if !fe.is_dir {
            return Err(ReaderError::NotFound {
                path: rel.display().to_string(),
            });
        }

        let dir_data = self.read_file_data(&fe, icb_part)?;
        let entries = parse_file_identifiers(&dir_data);

        let mut names: Vec<String> = entries.into_iter().map(|e| e.name).collect();
        names.sort();
        Ok(names)
    }

    /// Resolves a relative path to an ICB location by walking the directory tree.
    fn resolve_path(&self, rel: &Path) -> Result<(u32, u16), ReaderError> {
        let mut current_loc = self.root_icb_location;
        let mut current_part = self.root_icb_partition;

        let components: Vec<&str> = rel
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(s) => s.to_str(),
                _ => None,
            })
            .collect();

        if components.is_empty() {
            return Ok((current_loc, current_part));
        }

        for component in &components {
            let fe = self.read_file_entry(current_loc, current_part)?;
            if !fe.is_dir {
                return Err(ReaderError::NotFound {
                    path: rel.display().to_string(),
                });
            }

            let dir_data = self.read_file_data(&fe, current_part)?;
            let entries = parse_file_identifiers(&dir_data);

            let entry = entries
                .into_iter()
                .find(|e| e.name.eq_ignore_ascii_case(component));

            match entry {
                Some(e) => {
                    current_loc = e.icb_location;
                    current_part = e.icb_partition;
                }
                None => {
                    return Err(ReaderError::NotFound {
                        path: rel.display().to_string(),
                    });
                }
            }
        }

        Ok((current_loc, current_part))
    }

    /// Reads and parses a File Entry or Extended File Entry.
    ///
    /// Reads one sector for the header, then reads additional sectors if
    /// the allocation descriptor area extends beyond the first sector.
    fn read_file_entry(
        &self,
        location: u32,
        partition_ref: u16,
    ) -> Result<FileEntryInfo, ReaderError> {
        let byte_offset = resolve_byte_offset(&self.partition, location, partition_ref);
        let header = self.read_bytes(byte_offset, SECTOR_SIZE)?;
        let tag = parse_tag(&header);

        // Determine the total file entry size from the header fields,
        // then read more sectors if the entry spans beyond the first.
        let total_size = file_entry_size(tag.id, &header);
        let raw = if total_size > SECTOR_SIZE {
            self.read_bytes(byte_offset, total_size)?
        } else {
            header
        };

        match tag.id {
            TAG_FILE_ENTRY => parse_file_entry(&raw),
            TAG_EXTENDED_FILE_ENTRY => parse_extended_file_entry(&raw),
            _ => Err(ReaderError::IsoParse {
                path: self.path.clone(),
                reason: format!("expected File Entry (261/266), got tag {}", tag.id),
            }),
        }
    }

    /// Reads the data described by a file entry's allocation descriptors.
    ///
    /// `partition_ref` indicates which partition the allocation descriptors
    /// address — for metadata (directories, file entries) this is the
    /// metadata partition; for file content it's the physical partition.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "info_length as usize and read_len are bounded by actual image size"
    )]
    fn read_file_data(
        &self,
        fe: &FileEntryInfo,
        partition_ref: u16,
    ) -> Result<Vec<u8>, ReaderError> {
        // If data is stored inline (allocation type 3), use the embedded bytes.
        if fe.alloc_type == 3 {
            return Ok(fe.inline_data.clone());
        }

        // Short ADs (type 0) are relative to the same partition as the FE.
        // Long ADs (type 1) carry their own partition reference, but for
        // file data on Blu-ray they typically point to the physical partition.
        let ad_partition = if fe.alloc_type == 1 {
            // Long ADs: physical partition (ref 0) for file data.
            0
        } else {
            partition_ref
        };

        let mut result = Vec::with_capacity(fe.info_length as usize);
        let mut remaining = fe.info_length;

        for ad in &fe.alloc_descriptors {
            let extent_len = ad.length & 0x3FFF_FFFF; // Mask off the top 2 type bits.
            let byte_offset = resolve_byte_offset(&self.partition, ad.position, ad_partition);

            let read_len = u64::from(extent_len).min(remaining) as usize;
            let chunk = self.read_bytes(byte_offset, read_len)?;
            result.extend_from_slice(&chunk);
            remaining = remaining.saturating_sub(read_len as u64);

            if remaining == 0 {
                break;
            }
        }

        Ok(result)
    }

    /// Reads `len` bytes from the file at the given byte offset.
    fn read_bytes(&self, offset: u64, len: usize) -> Result<Vec<u8>, ReaderError> {
        let mut file = self.file.borrow_mut();
        read_at(&mut file, offset, len, &self.path)
    }
}

/// Information extracted from a File Entry / Extended File Entry.
#[derive(Debug)]
struct FileEntryInfo {
    is_dir: bool,
    info_length: u64,
    alloc_type: u8,
    inline_data: Vec<u8>,
    alloc_descriptors: Vec<ShortAd>,
}

// ── Low-level parsing ───────────────────────────────────────────────────

fn le_u16(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

fn le_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

fn le_u64(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
        data[offset + 4],
        data[offset + 5],
        data[offset + 6],
        data[offset + 7],
    ])
}

fn parse_tag(data: &[u8]) -> Tag {
    Tag {
        id: le_u16(data, 0),
    }
}

fn parse_extent(data: &[u8], offset: usize) -> Extent {
    Extent {
        length: le_u32(data, offset),
        location: le_u32(data, offset + 4),
    }
}

fn parse_long_ad(data: &[u8], offset: usize) -> LongAd {
    LongAd {
        length: le_u32(data, offset),
        location: le_u32(data, offset + 4),
        partition: le_u16(data, offset + 8),
    }
}

fn parse_short_ad(data: &[u8], offset: usize) -> ShortAd {
    ShortAd {
        length: le_u32(data, offset),
        position: le_u32(data, offset + 4),
    }
}

/// Resolves a logical block address to a byte offset in the image.
///
/// If the partition reference matches a metadata partition, the block
/// is translated through the Metadata File's allocation extents.
/// Otherwise, the block is offset from the physical partition start.
fn resolve_byte_offset(part: &PartitionInfo, logical_block: u32, partition_ref: u16) -> u64 {
    if let Some(meta) = &part.metadata
        && partition_ref == meta.partition_ref
    {
        return metadata_byte_offset(meta, logical_block, part.start_sector);
    }
    // Physical partition: start_sector + logical_block.
    (u64::from(part.start_sector) + u64::from(logical_block)) * SECTOR_SIZE as u64
}

/// Translates a logical block within the metadata partition to a byte offset.
///
/// Walks the Metadata File's allocation extents to find the physical sector
/// corresponding to the given metadata-partition logical block.
#[allow(
    clippy::cast_possible_truncation,
    reason = "extent block counts are bounded by partition size which fits u32"
)]
fn metadata_byte_offset(meta: &MetadataMapping, logical_block: u32, phys_start: u32) -> u64 {
    let mut remaining_blocks = logical_block;
    for ad in &meta.extents {
        let extent_blocks = (ad.length & 0x3FFF_FFFF) / SECTOR_SIZE as u32;
        if remaining_blocks < extent_blocks {
            let physical_sector =
                u64::from(phys_start) + u64::from(ad.position) + u64::from(remaining_blocks);
            return physical_sector * SECTOR_SIZE as u64;
        }
        remaining_blocks -= extent_blocks;
    }
    // Fallback: treat as physical (shouldn't happen with valid metadata).
    (u64::from(phys_start) + u64::from(logical_block)) * SECTOR_SIZE as u64
}

/// Reads a single sector from the file.
fn read_sector(file: &mut File, sector: u64, path: &str) -> Result<Vec<u8>, ReaderError> {
    let offset = sector * SECTOR_SIZE as u64;
    read_at(file, offset, SECTOR_SIZE, path)
}

/// Reads `len` bytes from the file at the given byte offset.
fn read_at(file: &mut File, offset: u64, len: usize, path: &str) -> Result<Vec<u8>, ReaderError> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| ReaderError::Io {
            path: path.to_owned(),
            source: e,
        })?;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf).map_err(|e| ReaderError::Io {
        path: path.to_owned(),
        source: e,
    })?;
    Ok(buf)
}

/// Parses the Main Volume Descriptor Sequence.
fn parse_mvds(file: &mut File, extent: Extent, path: &str) -> Result<MvdsInfo, ReaderError> {
    let mut partition_start: Option<u32> = None;
    let mut fsd_location: Option<u32> = None;
    let mut fsd_partition: Option<u16> = None;
    let mut metadata_file_location: Option<(u16, u32)> = None;

    let start_sector = u64::from(extent.location);
    let sector_count = u64::from(extent.length) / SECTOR_SIZE as u64;

    for i in 0..sector_count {
        let sector_data = read_sector(file, start_sector + i, path)?;
        let tag = parse_tag(&sector_data);

        match tag.id {
            TAG_PARTITION => {
                // Partition starting location at offset 188.
                partition_start = Some(le_u32(&sector_data, 188));
            }
            TAG_LOGICAL_VOLUME => {
                // FSD pointer at offset 248 (long_ad).
                let contents_use = parse_long_ad(&sector_data, 248);
                fsd_location = Some(contents_use.location);
                fsd_partition = Some(contents_use.partition);

                // Parse partition maps to detect metadata partitions.
                metadata_file_location = parse_partition_maps(&sector_data);
            }
            TAG_TERMINATING | 0 => break,
            _ => { /* skip other descriptors */ }
        }
    }

    let start = partition_start.ok_or_else(|| ReaderError::IsoParse {
        path: path.to_owned(),
        reason: "no Partition Descriptor found in MVDS".to_owned(),
    })?;

    let fsd_loc = fsd_location.ok_or_else(|| ReaderError::IsoParse {
        path: path.to_owned(),
        reason: "no Logical Volume Descriptor found in MVDS".to_owned(),
    })?;

    Ok(MvdsInfo {
        partition_start: start,
        fsd_location: fsd_loc,
        fsd_partition: fsd_partition.unwrap_or(0),
        metadata_file_location,
    })
}

/// Parses partition maps from a Logical Volume Descriptor.
///
/// Returns the metadata file location if a Type 2 Metadata Partition Map
/// is found: (`partition_ref_index`, `metadata_file_logical_block`).
#[allow(
    clippy::cast_possible_truncation,
    reason = "partition map index always fits u16"
)]
fn parse_partition_maps(lvd: &[u8]) -> Option<(u16, u32)> {
    if lvd.len() < 444 {
        return None;
    }

    let map_table_length = le_u32(lvd, 264) as usize;
    let num_maps = le_u32(lvd, 436) as usize;
    let maps_start = 440;
    let maps_end = (maps_start + map_table_length).min(lvd.len());

    let mut pos = maps_start;
    for map_index in 0..num_maps {
        if pos + 2 > maps_end {
            break;
        }

        let map_type = lvd[pos];
        let map_length = lvd[pos + 1] as usize;
        if map_length == 0 || pos + map_length > maps_end {
            break;
        }

        // Type 2 partition map with UDF Metadata identifier.
        if map_type == 2 && map_length >= 64 && pos + 64 <= maps_end {
            // Entity Identifier at offset 4, identifier string at offset 5.
            let id_start = pos + 5;
            let id_end = (id_start + UDF_METADATA_ID.len()).min(maps_end);
            if id_end - id_start == UDF_METADATA_ID.len()
                && lvd[id_start..id_end] == *UDF_METADATA_ID
            {
                // Metadata File Location at offset 40 within the map.
                let meta_file_loc = le_u32(lvd, pos + 40);
                return Some((map_index as u16, meta_file_loc));
            }
        }

        pos += map_length;
    }

    None
}

/// Builds the partition info, reading the Metadata File if a metadata
/// partition is present.
fn build_partition_info(
    file: &mut File,
    mvds: &MvdsInfo,
    path: &str,
) -> Result<PartitionInfo, ReaderError> {
    let metadata = match mvds.metadata_file_location {
        Some((partition_ref, meta_file_block)) => {
            // Read the Metadata File Entry from the physical partition.
            let meta_fe_sector = u64::from(mvds.partition_start) + u64::from(meta_file_block);
            let meta_fe_offset = meta_fe_sector * SECTOR_SIZE as u64;
            let header = read_sector(file, meta_fe_sector, path)?;
            let tag = parse_tag(&header);

            let total_size = file_entry_size(tag.id, &header);
            let raw = if total_size > SECTOR_SIZE {
                read_at(file, meta_fe_offset, total_size, path)?
            } else {
                header
            };

            let fe = match tag.id {
                TAG_FILE_ENTRY => parse_file_entry(&raw)?,
                TAG_EXTENDED_FILE_ENTRY => parse_extended_file_entry(&raw)?,
                _ => {
                    return Err(ReaderError::IsoParse {
                        path: path.to_owned(),
                        reason: format!(
                            "metadata file entry: expected tag 261/266, got {}",
                            tag.id,
                        ),
                    });
                }
            };

            Some(MetadataMapping {
                partition_ref,
                extents: fe.alloc_descriptors,
            })
        }
        None => None,
    };

    Ok(PartitionInfo {
        start_sector: mvds.partition_start,
        metadata,
    })
}

/// Computes the total byte size of a File Entry from its header fields.
///
/// Returns the fixed header size + `L_EA` + `L_AD`. Falls back to one sector
/// if the tag is unrecognised or the header is too short.
fn file_entry_size(tag_id: u16, header: &[u8]) -> usize {
    match tag_id {
        TAG_FILE_ENTRY if header.len() >= 176 => {
            let l_ea = le_u32(header, 168) as usize;
            let l_ad = le_u32(header, 172) as usize;
            176 + l_ea + l_ad
        }
        TAG_EXTENDED_FILE_ENTRY if header.len() >= 216 => {
            let l_ea = le_u32(header, 208) as usize;
            let l_ad = le_u32(header, 212) as usize;
            216 + l_ea + l_ad
        }
        _ => SECTOR_SIZE,
    }
}

/// Parses a standard File Entry (tag 261, ECMA-167 §14.9).
fn parse_file_entry(data: &[u8]) -> Result<FileEntryInfo, ReaderError> {
    parse_file_entry_inner(data, 176, 168, 172)
}

/// Parses an Extended File Entry (tag 266, ECMA-167 §14.17).
fn parse_extended_file_entry(data: &[u8]) -> Result<FileEntryInfo, ReaderError> {
    parse_file_entry_inner(data, 216, 208, 212)
}

/// Common implementation for parsing File Entry / Extended File Entry.
///
/// ICB Tag layout (ECMA-167 §14.6, starts at descriptor byte 16):
///   16-19: Prior Recorded Number of Direct Entries
///   20-21: Strategy Type
///   22-23: Strategy Parameter
///   24-25: Maximum Number of Entries
///   26:    Reserved
///   27:    File Type
///   28-33: Parent ICB Location
///   34-35: Flags (bits 0-2 = allocation descriptor type)
fn parse_file_entry_inner(
    data: &[u8],
    header_size: usize,
    l_ea_offset: usize,
    l_ad_offset: usize,
) -> Result<FileEntryInfo, ReaderError> {
    if data.len() < header_size {
        return Err(ReaderError::IsoParse {
            path: String::new(),
            reason: format!(
                "File Entry too short (need {header_size}, got {})",
                data.len()
            ),
        });
    }

    let icb_tag_file_type = data[27];
    let is_dir = icb_tag_file_type == ICB_FILE_TYPE_DIRECTORY;
    let alloc_type = data[34] & 0x07;
    let info_length = le_u64(data, 56);
    let l_ea = le_u32(data, l_ea_offset) as usize;
    let l_ad = le_u32(data, l_ad_offset) as usize;

    let ad_start = header_size + l_ea;

    parse_allocation_data(data, ad_start, l_ad, alloc_type, is_dir, info_length)
}

/// Extracts allocation descriptors or inline data from a file entry.
fn parse_allocation_data(
    data: &[u8],
    ad_start: usize,
    l_ad: usize,
    alloc_type: u8,
    is_dir: bool,
    info_length: u64,
) -> Result<FileEntryInfo, ReaderError> {
    let ad_end = ad_start + l_ad;
    if ad_end > data.len() {
        return Err(ReaderError::IsoParse {
            path: String::new(),
            reason: "allocation descriptor area exceeds file entry".to_owned(),
        });
    }

    match alloc_type {
        // Type 3: data is stored inline in the allocation descriptor area.
        3 => Ok(FileEntryInfo {
            is_dir,
            info_length,
            alloc_type,
            inline_data: data[ad_start..ad_end].to_vec(),
            alloc_descriptors: Vec::new(),
        }),
        // Type 0: short allocation descriptors (8 bytes each).
        0 => {
            let mut ads = Vec::new();
            let mut pos = ad_start;
            while pos + 8 <= ad_end {
                ads.push(parse_short_ad(data, pos));
                pos += 8;
            }
            Ok(FileEntryInfo {
                is_dir,
                info_length,
                alloc_type,
                inline_data: Vec::new(),
                alloc_descriptors: ads,
            })
        }
        // Type 1: long allocation descriptors (16 bytes each).
        1 => {
            let mut descriptors = Vec::new();
            let mut pos = ad_start;
            while pos + 16 <= ad_end {
                let long = parse_long_ad(data, pos);
                descriptors.push(ShortAd {
                    length: long.length,
                    position: long.location,
                });
                pos += 16;
            }
            Ok(FileEntryInfo {
                is_dir,
                info_length,
                alloc_type,
                inline_data: Vec::new(),
                alloc_descriptors: descriptors,
            })
        }
        _ => Err(ReaderError::IsoParse {
            path: String::new(),
            reason: format!("unsupported allocation descriptor type {alloc_type}"),
        }),
    }
}

/// Parses File Identifier Descriptors from a directory's data.
fn parse_file_identifiers(data: &[u8]) -> Vec<UdfDirEntry> {
    let mut entries = Vec::new();
    let mut pos = 0;

    while pos + 38 <= data.len() {
        let tag = parse_tag(&data[pos..]);
        if tag.id != TAG_FILE_IDENTIFIER {
            // Could be padding; stop parsing.
            break;
        }

        let file_chars = data[pos + 18];
        let is_parent = file_chars & 0x08 != 0;

        let l_fi = data[pos + 19] as usize;
        let icb = parse_long_ad(data, pos + 20);
        let l_iu = le_u16(data, pos + 36) as usize;

        let fi_start = pos + 38 + l_iu;
        let fi_end = fi_start + l_fi;

        // Skip parent directory entries.
        if !is_parent && l_fi > 0 && fi_end <= data.len() {
            let name = decode_udf_name(&data[fi_start..fi_end]);
            if !name.is_empty() {
                entries.push(UdfDirEntry {
                    name,
                    icb_location: icb.location,
                    icb_partition: icb.partition,
                });
            }
        }

        // FID length is padded to 4-byte boundary.
        let fid_len = 38 + l_iu + l_fi;
        let padded_len = (fid_len + 3) & !3;
        pos += padded_len;
    }

    entries
}

/// Decodes a UDF file identifier.
///
/// UDF names are prefixed with a compression ID byte:
/// - 8: 8-bit characters (Latin-1 subset)
/// - 16: 16-bit big-endian Unicode (UCS-2)
fn decode_udf_name(raw: &[u8]) -> String {
    if raw.is_empty() {
        return String::new();
    }

    let compression_id = raw[0];
    let payload = &raw[1..];

    match compression_id {
        16 => {
            let chars: Vec<u16> = payload
                .chunks_exact(2)
                .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                .collect();
            String::from_utf16_lossy(&chars)
        }
        // 8 = 8-bit Latin-1 subset; anything else, best-effort UTF-8.
        _ => String::from_utf8_lossy(payload).into_owned(),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
#[allow(
    clippy::cast_possible_truncation,
    reason = "test data is always small enough for u32"
)]
mod tests {
    use std::io::Write;

    use super::*;

    /// Builds a minimal UDF image for testing.
    ///
    /// Layout:
    ///   Sector 0..15    — system area (zeroed)
    ///   Sector 16       — reserved
    ///   Sector 32       — Partition Descriptor (partition starts at sector 288)
    ///   Sector 33       — Logical Volume Descriptor (FSD at logical block 0)
    ///   Sector 34       — Terminating Descriptor
    ///   Sector 256      — AVDP (points MVDS to sector 32, length 3 sectors)
    ///   Sector 288      — File Set Descriptor (root ICB at logical block 1)
    ///   Sector 289      — Root directory File Entry
    ///   Sector 290+     — Root directory data + files
    struct UdfBuilder {
        /// Sparse sector storage: (`sector_number`, data).
        sectors: Vec<(usize, [u8; SECTOR_SIZE])>,
        /// Partition start sector.
        partition_start: usize,
        /// Next available logical block within the partition.
        next_logical_block: u32,
    }

    impl UdfBuilder {
        const PARTITION_START: usize = 288;

        fn new() -> Self {
            Self {
                sectors: Vec::new(),
                partition_start: Self::PARTITION_START,
                next_logical_block: 0,
            }
        }

        /// Allocates the next logical block and returns its index.
        fn alloc_block(&mut self) -> u32 {
            let block = self.next_logical_block;
            self.next_logical_block += 1;
            block
        }

        /// Writes data to a partition-relative logical block.
        fn write_partition_block(&mut self, logical_block: u32, data: &[u8]) {
            let sector = self.partition_start + logical_block as usize;
            let mut buf = [0u8; SECTOR_SIZE];
            let len = data.len().min(SECTOR_SIZE);
            buf[..len].copy_from_slice(&data[..len]);
            self.sectors.push((sector, buf));
        }

        /// Writes data to an absolute sector.
        fn write_sector(&mut self, sector: usize, data: &[u8]) {
            let mut buf = [0u8; SECTOR_SIZE];
            let len = data.len().min(SECTOR_SIZE);
            buf[..len].copy_from_slice(&data[..len]);
            self.sectors.push((sector, buf));
        }

        /// Builds a minimal UDF image with the given root directory entries.
        /// Each entry is (name, `file_data`, `is_dir`).
        /// Returns an anonymous temp file rewound to the start.
        fn build_with_root(mut self, entries: &[(&str, &[u8], bool)]) -> File {
            // Allocate blocks for FSD, root FE, root dir data, and files.
            let fsd_block = self.alloc_block(); // 0
            let root_fe_block = self.alloc_block(); // 1
            let root_dir_block = self.alloc_block(); // 2

            // Allocate blocks for file entries and data.
            let mut file_blocks = Vec::new();
            for (_, data, _) in entries {
                let fe_block = self.alloc_block();
                let data_block = if data.is_empty() {
                    fe_block // No data needed, dummy
                } else {
                    self.alloc_block()
                };
                file_blocks.push((fe_block, data_block, data.len()));
            }

            // Write AVDP at sector 256.
            let mut avdp = [0u8; SECTOR_SIZE];
            write_tag(&mut avdp, 2); // AVDP tag
            // Main VDS extent at offset 16: length = 3 sectors, location = 32.
            write_extent(&mut avdp, 16, 3 * SECTOR_SIZE as u32, 32);
            self.write_sector(AVDP_SECTOR as usize, &avdp);

            // Write Partition Descriptor at sector 32.
            let mut pd = [0u8; SECTOR_SIZE];
            write_tag(&mut pd, TAG_PARTITION);
            pd[188..192].copy_from_slice(&(self.partition_start as u32).to_le_bytes());
            self.write_sector(32, &pd);

            // Write Logical Volume Descriptor at sector 33.
            let mut lvd = [0u8; SECTOR_SIZE];
            write_tag(&mut lvd, TAG_LOGICAL_VOLUME);
            // Logical block size at offset 212.
            lvd[212..216].copy_from_slice(&(SECTOR_SIZE as u32).to_le_bytes());
            // Contents Use (long_ad to FSD) at offset 248.
            write_long_ad(&mut lvd, 248, SECTOR_SIZE as u32, fsd_block, 0);
            // No partition maps (simple layout without metadata partition).
            lvd[264..268].copy_from_slice(&0u32.to_le_bytes()); // Map Table Length = 0
            lvd[436..440].copy_from_slice(&0u32.to_le_bytes()); // Number of Maps = 0
            self.write_sector(33, &lvd);

            // Write Terminating Descriptor at sector 34.
            let mut term = [0u8; SECTOR_SIZE];
            write_tag(&mut term, TAG_TERMINATING);
            self.write_sector(34, &term);

            // Write File Set Descriptor.
            let mut fsd = [0u8; SECTOR_SIZE];
            write_tag(&mut fsd, TAG_FILE_SET);
            // Root directory ICB at offset 400.
            write_long_ad(&mut fsd, 400, SECTOR_SIZE as u32, root_fe_block, 0);
            self.write_partition_block(fsd_block, &fsd);

            // Build root directory data.
            let mut dir_data = Vec::new();
            // Parent FID entry.
            write_fid(&mut dir_data, "", root_fe_block, 0, true, true);

            for (i, (name, _, is_dir)) in entries.iter().enumerate() {
                let (fe_block, _, _) = file_blocks[i];
                write_fid(&mut dir_data, name, fe_block, 0, *is_dir, false);
            }

            // Write root directory File Entry.
            let root_dir_size = dir_data.len() as u32;
            let root_fe = build_file_entry(true, root_dir_size, 0, root_dir_block, root_dir_size);
            self.write_partition_block(root_fe_block, &root_fe);

            // Write root directory data.
            self.write_partition_block(root_dir_block, &dir_data);

            // Write file entries and data.
            for (i, (_, data, is_dir)) in entries.iter().enumerate() {
                let (fe_block, data_block, _) = file_blocks[i];
                let size = data.len() as u32;

                if *is_dir {
                    let fe = build_file_entry(true, 0, 3, 0, 0);
                    self.write_partition_block(fe_block, &fe);
                } else if data.is_empty() {
                    let fe = build_file_entry(false, 0, 3, 0, 0);
                    self.write_partition_block(fe_block, &fe);
                } else {
                    let fe = build_file_entry(false, size, 0, data_block, size);
                    self.write_partition_block(fe_block, &fe);
                    self.write_partition_block(data_block, data);
                }
            }

            // Assemble the image into a temp file.
            let max_sector = self.sectors.iter().map(|(s, _)| *s).max().unwrap_or(0);

            let mut file = tempfile::tempfile().expect("should create temp file");
            let blank = [0u8; SECTOR_SIZE];
            for _ in 0..=max_sector {
                file.write_all(&blank).expect("should write blank sector");
            }
            for (sector, data) in &self.sectors {
                let offset = (*sector as u64) * SECTOR_SIZE as u64;
                file.seek(SeekFrom::Start(offset))
                    .expect("should seek in temp file");
                file.write_all(data).expect("should write sector data");
            }
            file.seek(SeekFrom::Start(0))
                .expect("should rewind temp file");

            file
        }
    }

    fn write_tag(buf: &mut [u8], id: u16) {
        buf[0..2].copy_from_slice(&id.to_le_bytes());
    }

    fn write_extent(buf: &mut [u8], offset: usize, length: u32, location: u32) {
        buf[offset..offset + 4].copy_from_slice(&length.to_le_bytes());
        buf[offset + 4..offset + 8].copy_from_slice(&location.to_le_bytes());
    }

    fn write_long_ad(buf: &mut [u8], offset: usize, length: u32, location: u32, partition: u16) {
        buf[offset..offset + 4].copy_from_slice(&length.to_le_bytes());
        buf[offset + 4..offset + 8].copy_from_slice(&location.to_le_bytes());
        buf[offset + 8..offset + 10].copy_from_slice(&partition.to_le_bytes());
    }

    /// Builds a File Identifier Descriptor.
    fn write_fid(
        buf: &mut Vec<u8>,
        name: &str,
        icb_location: u32,
        icb_partition: u16,
        is_dir: bool,
        is_parent: bool,
    ) {
        let encoded_name = if is_parent || name.is_empty() {
            Vec::new()
        } else {
            let mut enc = vec![8u8]; // compression ID = 8 (8-bit chars)
            enc.extend_from_slice(name.as_bytes());
            enc
        };

        let l_fi = encoded_name.len() as u8;
        let l_iu: u16 = 0;

        let fid_len = 38 + l_iu as usize + l_fi as usize;
        let padded_len = (fid_len + 3) & !3;

        let start = buf.len();
        buf.resize(start + padded_len, 0);

        write_tag(&mut buf[start..], TAG_FILE_IDENTIFIER);

        let mut chars = 0u8;
        if is_dir {
            chars |= 0x02;
        }
        if is_parent {
            chars |= 0x08;
        }
        buf[start + 18] = chars;
        buf[start + 19] = l_fi;

        write_long_ad(
            &mut buf[start..],
            20,
            SECTOR_SIZE as u32,
            icb_location,
            icb_partition,
        );

        buf[start + 36..start + 38].copy_from_slice(&l_iu.to_le_bytes());

        if !encoded_name.is_empty() {
            buf[start + 38..start + 38 + encoded_name.len()].copy_from_slice(&encoded_name);
        }
    }

    /// Builds a File Entry (tag 261) with a single short allocation descriptor.
    fn build_file_entry(
        is_dir: bool,
        info_length: u32,
        alloc_type: u8,
        ad_position: u32,
        ad_length: u32,
    ) -> Vec<u8> {
        let mut fe = vec![0u8; SECTOR_SIZE];

        write_tag(&mut fe, TAG_FILE_ENTRY);
        // ICB Tag File Type at byte 27.
        fe[27] = if is_dir {
            ICB_FILE_TYPE_DIRECTORY
        } else {
            5 // regular file
        };
        // ICB Tag Flags (alloc type in bits 0-2) at bytes 34-35.
        fe[34] = alloc_type & 0x07;
        fe[56..64].copy_from_slice(&u64::from(info_length).to_le_bytes());
        fe[168..172].copy_from_slice(&0u32.to_le_bytes());

        if alloc_type == 3 {
            fe[172..176].copy_from_slice(&0u32.to_le_bytes());
        } else {
            fe[172..176].copy_from_slice(&8u32.to_le_bytes());
            fe[176..180].copy_from_slice(&ad_length.to_le_bytes());
            fe[180..184].copy_from_slice(&ad_position.to_le_bytes());
        }

        fe
    }

    /// Builds a UDF 2.50 image with a metadata partition, like a real Blu-ray.
    ///
    /// Layout:
    ///   Sector 32       — Partition Descriptor (physical starts at sector 288)
    ///   Sector 33       — LVD with Type 1 + Type 2 (metadata) partition maps
    ///   Sector 34       — Terminating Descriptor
    ///   Sector 256      — AVDP
    ///   Sector 288      — Metadata File Entry (physical partition block 0)
    ///                      → its extent points to sector 289+ (metadata content)
    ///   Sector 289+     — Metadata partition content (FSD, FEs, dir data)
    ///   After metadata  — File data blocks (in physical partition)
    ///
    /// The FSD and root ICB use partition ref 1 (the metadata partition).
    /// File data ADs use partition ref 0 (the physical partition).
    struct MetadataUdfBuilder {
        sectors: Vec<(usize, [u8; SECTOR_SIZE])>,
        partition_start: usize,
        /// Next metadata partition logical block (for FSD, FEs, dirs).
        next_meta_block: u32,
        /// Physical partition block where the metadata content starts.
        meta_content_start: u32,
    }

    impl MetadataUdfBuilder {
        const PARTITION_START: usize = 288;
        // Block 0 of the physical partition = the Metadata File Entry.
        // Metadata content starts at physical block 1.
        const META_CONTENT_START: u32 = 1;

        fn new() -> Self {
            Self {
                sectors: Vec::new(),
                partition_start: Self::PARTITION_START,
                next_meta_block: 0,
                meta_content_start: Self::META_CONTENT_START,
            }
        }

        fn alloc_meta_block(&mut self) -> u32 {
            let block = self.next_meta_block;
            self.next_meta_block += 1;
            block
        }

        fn write_meta_block(&mut self, meta_block: u32, data: &[u8]) {
            // Metadata block N maps to physical sector:
            //   partition_start + meta_content_start + N
            let sector =
                self.partition_start + self.meta_content_start as usize + meta_block as usize;
            let mut buf = [0u8; SECTOR_SIZE];
            let len = data.len().min(SECTOR_SIZE);
            buf[..len].copy_from_slice(&data[..len]);
            self.sectors.push((sector, buf));
        }

        fn write_phys_block(&mut self, phys_block: u32, data: &[u8]) {
            let sector = self.partition_start + phys_block as usize;
            let mut buf = [0u8; SECTOR_SIZE];
            let len = data.len().min(SECTOR_SIZE);
            buf[..len].copy_from_slice(&data[..len]);
            self.sectors.push((sector, buf));
        }

        fn write_sector(&mut self, sector: usize, data: &[u8]) {
            let mut buf = [0u8; SECTOR_SIZE];
            let len = data.len().min(SECTOR_SIZE);
            buf[..len].copy_from_slice(&data[..len]);
            self.sectors.push((sector, buf));
        }

        /// Builds the image. Each entry: (name, `file_data`, `is_dir`).
        #[allow(
            clippy::too_many_lines,
            reason = "test builder assembles a complex image"
        )]
        fn build_with_root(mut self, entries: &[(&str, &[u8], bool)]) -> File {
            // ── Allocate metadata blocks ──
            let fsd_mblock = self.alloc_meta_block(); // 0
            let root_fe_mblock = self.alloc_meta_block(); // 1
            let root_dir_mblock = self.alloc_meta_block(); // 2

            let mut file_info = Vec::new();
            for (_, data, is_dir) in entries {
                let fe_mblock = self.alloc_meta_block();
                // File data goes in the physical partition, not metadata.
                let data_phys_block = if data.is_empty() || *is_dir {
                    0 // unused
                } else {
                    // Allocate physical blocks after metadata area.
                    self.meta_content_start + self.next_meta_block + file_info.len() as u32
                };
                file_info.push((fe_mblock, data_phys_block, *data, *is_dir));
            }

            let total_meta_sectors = self.next_meta_block;

            // ── AVDP ──
            let mut avdp = [0u8; SECTOR_SIZE];
            write_tag(&mut avdp, 2);
            write_extent(&mut avdp, 16, 3 * SECTOR_SIZE as u32, 32);
            self.write_sector(AVDP_SECTOR as usize, &avdp);

            // ── Partition Descriptor ──
            let mut pd = [0u8; SECTOR_SIZE];
            write_tag(&mut pd, TAG_PARTITION);
            pd[188..192].copy_from_slice(&(self.partition_start as u32).to_le_bytes());
            self.write_sector(32, &pd);

            // ── Logical Volume Descriptor with partition maps ──
            let mut lvd = [0u8; SECTOR_SIZE];
            write_tag(&mut lvd, TAG_LOGICAL_VOLUME);
            lvd[212..216].copy_from_slice(&(SECTOR_SIZE as u32).to_le_bytes());
            // FSD at metadata block 0, partition ref 1 (metadata partition).
            write_long_ad(&mut lvd, 248, SECTOR_SIZE as u32, fsd_mblock, 1);

            // Partition maps: Type 1 (6 bytes) + Type 2 metadata (64 bytes).
            let map_table_length: u32 = 6 + 64;
            lvd[264..268].copy_from_slice(&map_table_length.to_le_bytes());
            lvd[436..440].copy_from_slice(&2u32.to_le_bytes()); // 2 maps

            // Map 0: Type 1 (physical), 6 bytes.
            let map0_start = 440;
            lvd[map0_start] = 1; // Type 1
            lvd[map0_start + 1] = 6; // Length
            // Partition number 0 at offset 4.
            lvd[map0_start + 4..map0_start + 6].copy_from_slice(&0u16.to_le_bytes());

            // Map 1: Type 2 (metadata), 64 bytes.
            let map1_start = map0_start + 6;
            lvd[map1_start] = 2; // Type 2
            lvd[map1_start + 1] = 64; // Length
            // Entity Identifier: "*UDF Metadata Partition" at offset 5.
            lvd[map1_start + 5..map1_start + 5 + UDF_METADATA_ID.len()]
                .copy_from_slice(UDF_METADATA_ID);
            // Metadata File Location at offset 40: physical block 0.
            lvd[map1_start + 40..map1_start + 44].copy_from_slice(&0u32.to_le_bytes());

            self.write_sector(33, &lvd);

            // ── Terminating Descriptor ──
            let mut term = [0u8; SECTOR_SIZE];
            write_tag(&mut term, TAG_TERMINATING);
            self.write_sector(34, &term);

            // ── Metadata File Entry (physical block 0) ──
            // Points to the metadata content area.
            let meta_size = total_meta_sectors * SECTOR_SIZE as u32;
            let meta_fe = build_file_entry(
                false, // not a directory — it's the metadata file
                meta_size,
                0, // short ADs
                Self::META_CONTENT_START,
                meta_size,
            );
            // ICB file type 250 = metadata file (overwrite the default 5).
            let mut meta_fe_mut = meta_fe;
            meta_fe_mut[27] = 250;
            self.write_phys_block(0, &meta_fe_mut);

            // ── FSD (metadata block 0) ──
            let mut fsd = [0u8; SECTOR_SIZE];
            write_tag(&mut fsd, TAG_FILE_SET);
            // Root ICB: partition ref 1 (metadata partition).
            write_long_ad(&mut fsd, 400, SECTOR_SIZE as u32, root_fe_mblock, 1);
            self.write_meta_block(fsd_mblock, &fsd);

            // ── Root directory FE + data ──
            let mut dir_data = Vec::new();
            write_fid(&mut dir_data, "", root_fe_mblock, 1, true, true);
            for (fe_mblock, _, _, is_dir) in &file_info {
                // Look up name from entries by position.
                let idx = file_info
                    .iter()
                    .position(|(b, _, _, _)| b == fe_mblock)
                    .unwrap_or(0);
                let (name, _, _) = entries[idx];
                write_fid(&mut dir_data, name, *fe_mblock, 1, *is_dir, false);
            }

            let root_dir_size = dir_data.len() as u32;
            let root_fe = build_file_entry(true, root_dir_size, 0, root_dir_mblock, root_dir_size);
            self.write_meta_block(root_fe_mblock, &root_fe);
            self.write_meta_block(root_dir_mblock, &dir_data);

            // ── File entries and data ──
            for (i, (fe_mblock, data_phys_block, file_data, is_dir)) in file_info.iter().enumerate()
            {
                let _ = i;
                let size = file_data.len() as u32;

                if *is_dir {
                    let fe = build_file_entry(true, 0, 3, 0, 0);
                    self.write_meta_block(*fe_mblock, &fe);
                } else if file_data.is_empty() {
                    let fe = build_file_entry(false, 0, 3, 0, 0);
                    self.write_meta_block(*fe_mblock, &fe);
                } else {
                    // File FE in metadata partition, data in physical partition.
                    // Use long ADs (type 1) so partition ref is explicit.
                    let mut fe = vec![0u8; SECTOR_SIZE];
                    write_tag(&mut fe, TAG_FILE_ENTRY);
                    fe[27] = 5; // regular file
                    fe[34] = 1; // alloc type 1 (long ADs)
                    fe[56..64].copy_from_slice(&u64::from(size).to_le_bytes());
                    fe[168..172].copy_from_slice(&0u32.to_le_bytes()); // L_EA
                    // L_AD: one long_ad = 16 bytes
                    fe[172..176].copy_from_slice(&16u32.to_le_bytes());
                    // Long AD at offset 176: partition ref 0 (physical).
                    write_long_ad(&mut fe, 176, size, *data_phys_block, 0);
                    self.write_meta_block(*fe_mblock, &fe);
                    self.write_phys_block(*data_phys_block, file_data);
                }
            }

            // ── Assemble ──
            let max_sector = self.sectors.iter().map(|(s, _)| *s).max().unwrap_or(0);
            let mut file = tempfile::tempfile().expect("should create temp file");
            let blank = [0u8; SECTOR_SIZE];
            for _ in 0..=max_sector {
                file.write_all(&blank).expect("should write");
            }
            for (sector, data) in &self.sectors {
                let offset = (*sector as u64) * SECTOR_SIZE as u64;
                file.seek(SeekFrom::Start(offset)).expect("should seek");
                file.write_all(data).expect("should write");
            }
            file.seek(SeekFrom::Start(0)).expect("should rewind");
            file
        }
    }

    // ── Tests ───────────────────────────────────────────────────────────

    #[test]
    fn parse_udf_root_directory() {
        let file =
            UdfBuilder::new().build_with_root(&[("BDMV", &[], true), ("CERTIFICATE", &[], true)]);

        let reader = UdfReader::new(file, Path::new("test.udf")).expect("should parse UDF image");

        let entries = reader.read_dir(Path::new("")).expect("should list root");
        assert_eq!(
            entries,
            vec!["BDMV", "CERTIFICATE"],
            "root should contain expected directories"
        );
    }

    #[test]
    fn read_file_from_udf() {
        let file = UdfBuilder::new().build_with_root(&[("TEST.TXT", b"hello udf", false)]);

        let reader = UdfReader::new(file, Path::new("test.udf")).expect("should parse UDF image");

        let data = reader
            .read_file(Path::new("TEST.TXT"))
            .expect("should read file");
        assert_eq!(data, b"hello udf", "file contents should match");
    }

    #[test]
    fn case_insensitive_lookup() {
        let file = UdfBuilder::new().build_with_root(&[("BDMV", &[], true)]);

        let reader = UdfReader::new(file, Path::new("test.udf")).expect("should parse UDF image");

        let entries = reader.read_dir(Path::new("bdmv"));
        assert!(
            entries.is_ok(),
            "case-insensitive directory lookup should succeed"
        );
    }

    #[test]
    fn udf_name_decoding_8bit() {
        let raw = [8, b'H', b'E', b'L', b'L', b'O'];
        assert_eq!(decode_udf_name(&raw), "HELLO", "8-bit name should decode");
    }

    #[test]
    fn udf_name_decoding_16bit() {
        let raw = [16, 0x00, b'A', 0x00, b'B'];
        assert_eq!(decode_udf_name(&raw), "AB", "16-bit name should decode");
    }

    #[test]
    fn invalid_image_rejected() {
        let mut file = tempfile::tempfile().expect("should create temp file");
        file.write_all(&vec![0u8; SECTOR_SIZE * 10])
            .expect("should write");
        file.seek(SeekFrom::Start(0)).expect("should rewind");
        let result = UdfReader::new(file, Path::new("bad.udf"));
        assert!(result.is_err(), "too-small image should fail");
    }

    // ── Metadata partition tests (Blu-ray layout) ───────────────────────

    #[test]
    fn metadata_partition_root_directory() {
        let file = MetadataUdfBuilder::new()
            .build_with_root(&[("BDMV", &[], true), ("CERTIFICATE", &[], true)]);

        let reader =
            UdfReader::new(file, Path::new("test.udf")).expect("should parse UDF with metadata");

        let entries = reader.read_dir(Path::new("")).expect("should list root");
        assert_eq!(
            entries,
            vec!["BDMV", "CERTIFICATE"],
            "root via metadata partition should contain expected directories"
        );
    }

    #[test]
    fn metadata_partition_read_file() {
        let file =
            MetadataUdfBuilder::new().build_with_root(&[("DATA.BIN", b"bluray data", false)]);

        let reader =
            UdfReader::new(file, Path::new("test.udf")).expect("should parse UDF with metadata");

        let data = reader
            .read_file(Path::new("DATA.BIN"))
            .expect("should read file via metadata partition");
        assert_eq!(
            data, b"bluray data",
            "file data should be read from physical partition"
        );
    }

    #[test]
    fn metadata_partition_case_insensitive() {
        let file = MetadataUdfBuilder::new().build_with_root(&[("BDMV", &[], true)]);

        let reader =
            UdfReader::new(file, Path::new("test.udf")).expect("should parse UDF with metadata");

        assert!(
            reader.read_dir(Path::new("bdmv")).is_ok(),
            "case-insensitive lookup should work through metadata partition"
        );
    }
}
