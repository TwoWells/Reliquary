// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! `index.bdmv` parser — disc title table.
//!
//! Parses the top-level navigation table that maps title numbers to movie
//! objects (HDMV) or BD-J applications. Used to determine which playlists
//! the disc author registered as content titles.
//!
//! Reference: libbluray `src/libbluray/bdnav/index_parse.c`.

use thiserror::Error;

use super::cursor::{Cursor, CursorError};

// ── Errors ──────────────────────────────────────────────────────────────

/// Errors that can occur while parsing `index.bdmv`.
#[derive(Debug, Error)]
pub enum IndexError {
    /// File is smaller than the minimum header size.
    #[error("file too small ({size} bytes, need at least 16)")]
    TooSmall {
        /// Actual file size.
        size: usize,
    },

    /// Magic bytes are not `"INDX"`.
    #[error("invalid magic: expected \"INDX\", got {found:?}")]
    InvalidMagic {
        /// The four bytes actually found.
        found: [u8; 4],
    },

    /// Data is truncated during parsing.
    #[error("unexpected end of data at offset {offset} (need {needed} bytes, have {available})")]
    UnexpectedEof {
        /// Byte offset where the read was attempted.
        offset: usize,
        /// Number of bytes requested.
        needed: usize,
        /// Number of bytes actually available from that offset.
        available: usize,
    },
}

impl From<CursorError> for IndexError {
    fn from(e: CursorError) -> Self {
        Self::UnexpectedEof {
            offset: e.offset,
            needed: e.needed,
            available: e.available,
        }
    }
}

// ── Types ───────────────────────────────────────────────────────────────

/// A parsed `index.bdmv` file.
#[derive(Debug)]
pub struct DiscIndex {
    /// First Play object.
    pub first_play: PlayItem,
    /// Top Menu object.
    pub top_menu: PlayItem,
    /// Title entries.
    pub titles: Vec<Title>,
}

/// Object type in the index table (wire values: HDMV=1, BD-J=2).
#[derive(Debug, Clone)]
pub enum ObjectType {
    /// HDMV movie object — `id_ref` is an MOBJ index.
    Hdmv {
        /// Playback type (0=movie, 1=interactive).
        playback_type: u8,
        /// Movie object index.
        id_ref: u16,
    },
    /// BD-J application — `name` is a 5-char BDJO filename.
    Bdj {
        /// Playback type.
        playback_type: u8,
        /// 5-character application name.
        name: String,
    },
}

/// A `first_play` or `top_menu` entry.
#[derive(Debug, Clone)]
pub struct PlayItem {
    /// Object reference.
    pub object: ObjectType,
}

/// A numbered title entry.
#[derive(Debug, Clone)]
pub struct Title {
    /// Access type (0=normal, 1=title search only, etc.).
    pub access_type: u8,
    /// Object reference.
    pub object: ObjectType,
}

// ── Parser ──────────────────────────────────────────────────────────────

/// Size of the HDMV/BD-J body within a `PLAY_ITEM` or `TITLE` entry.
const OBJECT_BODY_SIZE: usize = 8;

/// Object type values from libbluray `indx_object_type` enum.
#[cfg(test)]
const OBJECT_TYPE_HDMV: u32 = 1;
const OBJECT_TYPE_BDJ: u32 = 2;

/// Parses `index.bdmv` from raw bytes.
///
/// # Errors
///
/// Returns [`IndexError`] if the file has an invalid header or is truncated.
pub fn parse(data: &[u8]) -> Result<DiscIndex, IndexError> {
    if data.len() < 16 {
        return Err(IndexError::TooSmall { size: data.len() });
    }

    // Magic: "INDX"
    let magic: [u8; 4] = [data[0], data[1], data[2], data[3]];
    if &magic != b"INDX" {
        return Err(IndexError::InvalidMagic { found: magic });
    }

    // index_start at bytes 8-11
    let mut r = Cursor::new(data);
    r.seek(8)?;
    let index_start = r.read_u32()? as usize;

    // Seek to the index section
    r.seek(index_start)?;

    // index_length (4 bytes) — we skip it, parse by structure
    r.skip(4)?;

    // first_play
    let first_play = parse_play_item(&mut r)?;

    // top_menu
    let top_menu = parse_play_item(&mut r)?;

    // num_titles
    let num_titles = r.read_u16()?;

    let mut titles = Vec::with_capacity(num_titles as usize);
    for _ in 0..num_titles {
        titles.push(parse_title(&mut r)?);
    }

    Ok(DiscIndex {
        first_play,
        top_menu,
        titles,
    })
}

/// Parses a `PLAY_ITEM` (`first_play` or `top_menu`): 4 bytes type/reserved + 8 bytes body.
fn parse_play_item(r: &mut Cursor<'_>) -> Result<PlayItem, IndexError> {
    let object = parse_object_ref(r)?;
    Ok(PlayItem { object })
}

/// Parses a TITLE entry: 4 bytes type/access/reserved + 8 bytes body.
fn parse_title(r: &mut Cursor<'_>) -> Result<Title, IndexError> {
    let header = r.read_u32()?;

    #[allow(
        clippy::cast_possible_truncation,
        reason = "access_type is a 2-bit field"
    )]
    let access_type = ((header >> 28) & 0x03) as u8;

    let object = parse_object_body(r, header)?;
    Ok(Title {
        access_type,
        object,
    })
}

/// Parses the 4-byte header + 8-byte object body shared by `PLAY_ITEM` and `TITLE`.
fn parse_object_ref(r: &mut Cursor<'_>) -> Result<ObjectType, IndexError> {
    let header = r.read_u32()?;
    parse_object_body(r, header)
}

/// Parses the 8-byte HDMV/BD-J body given the already-read 4-byte header word.
fn parse_object_body(r: &mut Cursor<'_>, header: u32) -> Result<ObjectType, IndexError> {
    let object_type = (header >> 30) & 0x03;

    r.ensure(OBJECT_BODY_SIZE)?;

    if object_type == OBJECT_TYPE_BDJ {
        // BD-J: 2 bits playback_type + 14 bits reserved (2 bytes) +
        //        5 bytes name + 1 byte reserved (8 bytes total)
        let pb_word = r.read_u16()?;
        #[allow(
            clippy::cast_possible_truncation,
            reason = "playback_type is a 2-bit field"
        )]
        let playback_type = ((pb_word >> 14) & 0x03) as u8;

        let name_bytes = r.read_bytes(5)?;
        let name = String::from_utf8_lossy(name_bytes).into_owned();
        r.skip(1)?; // reserved

        Ok(ObjectType::Bdj {
            playback_type,
            name,
        })
    } else {
        // HDMV (object_type == OBJECT_TYPE_HDMV, or treat unknown as HDMV)
        // Body: 2 bits playback_type + 14 bits reserved + 16 bits id_ref + 32 bits reserved
        let word1 = r.read_u32()?;
        r.skip(4)?; // 32 bits reserved

        #[allow(
            clippy::cast_possible_truncation,
            reason = "playback_type is a 2-bit field"
        )]
        let playback_type = ((word1 >> 30) & 0x03) as u8;

        #[allow(clippy::cast_possible_truncation, reason = "id_ref is a 16-bit field")]
        let id_ref = (word1 & 0xFFFF) as u16;

        Ok(ObjectType::Hdmv {
            playback_type,
            id_ref,
        })
    }
}

// ── Title resolution ────────────────────────────────────────────────────

/// Resolves title table entries to playlist numbers.
///
/// For each HDMV title, runs the target MOBJ through the mini-VM to find
/// the terminal `PlayPl`. The VM is seeded with MOBJ\[0\]'s GPR database
/// so that conditional title MOBJs (WB authoring) resolve correctly.
/// BD-J titles are skipped (no MOBJ to execute). `first_play` and
/// `top_menu` entries are excluded — these are typically the FBI warning
/// and menu loop, not content titles.
///
/// When a dispatch table is provided and any title MOBJ fails to resolve
/// (e.g. because it plays a menu clip that requires IG interaction), all
/// dispatch table playlist targets are included. On WB discs, these are
/// the extras playlists reachable through the central dispatch MOBJ.
///
/// Returns the set of playlist numbers that the disc author registered
/// as titles.
#[must_use]
pub fn resolve_title_playlists(
    index: &DiscIndex,
    mobj_file: &super::mobj::MovieObjectFile,
    dispatch_table: Option<&super::mobj::DispatchTable>,
) -> std::collections::HashSet<u32> {
    use super::mobj::{run_mobj_vm, seed_gpr_state};

    let empty_valid = std::collections::HashSet::new();
    let init_gprs = seed_gpr_state(mobj_file);
    let mut result = std::collections::HashSet::new();
    let mut has_unresolved = false;

    for title in &index.titles {
        if let ObjectType::Hdmv { id_ref, .. } = &title.object {
            let idx = usize::from(*id_ref);
            if let Some(mobj) = mobj_file.objects.get(idx) {
                let mut gprs = init_gprs.clone();
                if let Some(target) = run_mobj_vm(&mobj.instructions, 0, &mut gprs, &empty_valid) {
                    result.insert(u32::from(target.playlist));
                } else {
                    has_unresolved = true;
                }
            }
        }
    }

    // Include dispatch table targets when any title MOBJ could not be
    // resolved. On WB discs, unresolved titles are menu entry points
    // that dispatch to content playlists via the central MOBJ switch.
    if has_unresolved && let Some(table) = dispatch_table {
        for &(_, pl) in &table.cases {
            result.insert(u32::from(pl));
        }
    }

    result
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod tests {
    use super::*;

    /// Builds a synthetic `index.bdmv` for testing.
    struct IndexBuilder {
        first_play: ObjectType,
        top_menu: ObjectType,
        titles: Vec<(u8, ObjectType)>,
    }

    impl IndexBuilder {
        fn new() -> Self {
            Self {
                first_play: ObjectType::Hdmv {
                    playback_type: 0,
                    id_ref: 0,
                },
                top_menu: ObjectType::Hdmv {
                    playback_type: 1,
                    id_ref: 1,
                },
                titles: Vec::new(),
            }
        }

        fn first_play_hdmv(mut self, id_ref: u16) -> Self {
            self.first_play = ObjectType::Hdmv {
                playback_type: 0,
                id_ref,
            };
            self
        }

        fn top_menu_hdmv(mut self, id_ref: u16) -> Self {
            self.top_menu = ObjectType::Hdmv {
                playback_type: 1,
                id_ref,
            };
            self
        }

        fn hdmv_title(mut self, access_type: u8, id_ref: u16) -> Self {
            self.titles.push((
                access_type,
                ObjectType::Hdmv {
                    playback_type: 0,
                    id_ref,
                },
            ));
            self
        }

        fn bdj_title(mut self, access_type: u8, name: &str) -> Self {
            self.titles.push((
                access_type,
                ObjectType::Bdj {
                    playback_type: 0,
                    name: name.to_owned(),
                },
            ));
            self
        }

        fn build(&self) -> Vec<u8> {
            let mut data = Vec::new();

            // Header (16 bytes)
            data.extend_from_slice(b"INDX"); // magic
            data.extend_from_slice(b"0200"); // version
            let index_start: u32 = 40; // after 40-byte app info area
            data.extend_from_slice(&index_start.to_be_bytes());
            data.extend_from_slice(&0u32.to_be_bytes()); // extension_data_start

            // AppInfo padding (40 - 16 = 24 bytes)
            data.resize(40, 0);

            // Index section
            let index_body_start = data.len();

            // index_length placeholder (4 bytes)
            data.extend_from_slice(&0u32.to_be_bytes());

            // first_play (12 bytes: 4 header + 8 body)
            Self::write_play_item(&mut data, &self.first_play);

            // top_menu (12 bytes: 4 header + 8 body)
            Self::write_play_item(&mut data, &self.top_menu);

            // num_titles
            #[allow(
                clippy::cast_possible_truncation,
                reason = "test builder won't exceed u16"
            )]
            let num_titles = self.titles.len() as u16;
            data.extend_from_slice(&num_titles.to_be_bytes());

            // titles
            for (access_type, object) in &self.titles {
                Self::write_title(&mut data, *access_type, object);
            }

            // Patch index_length
            #[allow(
                clippy::cast_possible_truncation,
                reason = "test data won't exceed u32"
            )]
            let index_length = (data.len() - index_body_start - 4) as u32;
            data[index_body_start..index_body_start + 4]
                .copy_from_slice(&index_length.to_be_bytes());

            data
        }

        fn write_play_item(data: &mut Vec<u8>, object: &ObjectType) {
            match object {
                ObjectType::Hdmv {
                    playback_type,
                    id_ref,
                } => {
                    // Header: object_type=HDMV(1) in top 2 bits + 30 bits reserved
                    let header = OBJECT_TYPE_HDMV << 30;
                    data.extend_from_slice(&header.to_be_bytes());
                    // Body: playback_type (2 bits) + reserved (14 bits) + id_ref (16 bits)
                    let word = (u32::from(*playback_type) << 30) | u32::from(*id_ref);
                    data.extend_from_slice(&word.to_be_bytes());
                    // 4 bytes reserved
                    data.extend_from_slice(&0u32.to_be_bytes());
                }
                ObjectType::Bdj {
                    playback_type,
                    name,
                } => {
                    // Header: object_type=BDJ(2) in top 2 bits + 30 bits reserved
                    let header = OBJECT_TYPE_BDJ << 30;
                    data.extend_from_slice(&header.to_be_bytes());
                    // Body: playback_type (2 bits) + reserved (14 bits) = 2 bytes
                    let pb_bytes = u16::from(*playback_type) << 14;
                    data.extend_from_slice(&pb_bytes.to_be_bytes());
                    // 5-byte name
                    let mut name_bytes = [0u8; 5];
                    for (i, b) in name.as_bytes().iter().take(5).enumerate() {
                        name_bytes[i] = *b;
                    }
                    data.extend_from_slice(&name_bytes);
                    // 1 byte reserved
                    data.push(0);
                }
            }
        }

        fn write_title(data: &mut Vec<u8>, access_type: u8, object: &ObjectType) {
            match object {
                ObjectType::Hdmv {
                    playback_type,
                    id_ref,
                } => {
                    // Header: object_type=HDMV(1) (2 bits) + access_type (2 bits) + 28 bits reserved
                    let header = (OBJECT_TYPE_HDMV << 30) | (u32::from(access_type) << 28);
                    data.extend_from_slice(&header.to_be_bytes());
                    // Body: playback_type (2 bits) + reserved (14 bits) + id_ref (16 bits)
                    let word = (u32::from(*playback_type) << 30) | u32::from(*id_ref);
                    data.extend_from_slice(&word.to_be_bytes());
                    // 4 bytes reserved
                    data.extend_from_slice(&0u32.to_be_bytes());
                }
                ObjectType::Bdj {
                    playback_type,
                    name,
                } => {
                    // Header: object_type=BDJ(2) (2 bits) + access_type (2 bits) + 28 bits reserved
                    let header = (OBJECT_TYPE_BDJ << 30) | (u32::from(access_type) << 28);
                    data.extend_from_slice(&header.to_be_bytes());
                    // Body: playback_type (2 bits) + reserved (14 bits) = 2 bytes
                    let pb_bytes = u16::from(*playback_type) << 14;
                    data.extend_from_slice(&pb_bytes.to_be_bytes());
                    // 5-byte name
                    let mut name_bytes = [0u8; 5];
                    for (i, b) in name.as_bytes().iter().take(5).enumerate() {
                        name_bytes[i] = *b;
                    }
                    data.extend_from_slice(&name_bytes);
                    // 1 byte reserved
                    data.push(0);
                }
            }
        }
    }

    // ── Parser tests ────────────────────────────────────────────────────

    #[test]
    fn parse_three_hdmv_titles() {
        let data = IndexBuilder::new()
            .first_play_hdmv(0)
            .top_menu_hdmv(1)
            .hdmv_title(0, 2)
            .hdmv_title(0, 3)
            .hdmv_title(0, 4)
            .build();

        let index = parse(&data).expect("should parse valid index");

        assert_eq!(index.titles.len(), 3, "should have 3 titles");

        // Verify first_play
        assert!(
            matches!(&index.first_play.object, ObjectType::Hdmv { id_ref: 0, .. }),
            "first_play should be HDMV with id_ref 0"
        );

        // Verify top_menu
        assert!(
            matches!(&index.top_menu.object, ObjectType::Hdmv { id_ref: 1, .. }),
            "top_menu should be HDMV with id_ref 1"
        );

        // Verify titles
        assert!(
            matches!(&index.titles[0].object, ObjectType::Hdmv { id_ref: 2, .. }),
            "title 0 should be HDMV with id_ref 2"
        );
        assert!(
            matches!(&index.titles[1].object, ObjectType::Hdmv { id_ref: 3, .. }),
            "title 1 should be HDMV with id_ref 3"
        );
        assert!(
            matches!(&index.titles[2].object, ObjectType::Hdmv { id_ref: 4, .. }),
            "title 2 should be HDMV with id_ref 4"
        );
    }

    #[test]
    fn parse_bdj_title() {
        let data = IndexBuilder::new().bdj_title(0, "00001").build();

        let index = parse(&data).expect("should parse index with BD-J title");

        assert_eq!(index.titles.len(), 1, "should have 1 title");
        assert!(
            matches!(&index.titles[0].object, ObjectType::Bdj { name, .. } if name == "00001"),
            "title should be BD-J with name '00001'"
        );
    }

    #[test]
    fn parse_mixed_hdmv_and_bdj() {
        let data = IndexBuilder::new()
            .hdmv_title(0, 2)
            .bdj_title(0, "00003")
            .hdmv_title(1, 5)
            .build();

        let index = parse(&data).expect("should parse mixed index");
        assert_eq!(index.titles.len(), 3, "should have 3 titles");

        // Title 0: HDMV
        assert!(
            matches!(&index.titles[0].object, ObjectType::Hdmv { id_ref: 2, .. }),
            "title 0 should be HDMV with id_ref 2"
        );

        // Title 1: BD-J
        assert!(
            matches!(&index.titles[1].object, ObjectType::Bdj { name, .. } if name == "00003"),
            "title 1 should be BD-J with name '00003'"
        );

        // Title 2: HDMV with access_type 1
        assert_eq!(
            index.titles[2].access_type, 1,
            "title 2 should have access_type 1"
        );
        assert!(
            matches!(&index.titles[2].object, ObjectType::Hdmv { id_ref: 5, .. }),
            "title 2 should be HDMV with id_ref 5"
        );
    }

    #[test]
    fn parse_truncated_file() {
        let result = parse(&[0x49, 0x4E, 0x44, 0x58, 0x30, 0x31]);
        assert!(result.is_err(), "truncated file should fail");
    }

    #[test]
    fn parse_wrong_magic() {
        let mut data = IndexBuilder::new().build();
        data[0..4].copy_from_slice(b"MOBJ");

        let result = parse(&data);
        assert!(result.is_err(), "wrong magic should fail");
        let err = result.expect_err("should be InvalidMagic");
        assert!(
            err.to_string().contains("INDX"),
            "error should mention the expected magic"
        );
    }

    #[test]
    fn parse_empty_title_table() {
        let data = IndexBuilder::new().build();
        let index = parse(&data).expect("should parse index with no titles");
        assert!(index.titles.is_empty(), "should have no titles");
    }

    // ── Title resolution tests ──────────────────────────────────────────

    /// Builds a minimal MOBJ file with objects whose instructions are
    /// `PlayPl(immediate)` for the given playlist numbers.
    fn build_mobj_file(play_pls: &[u16]) -> super::super::mobj::MovieObjectFile {
        use super::super::mobj::{Instruction, MovieObject, MovieObjectFile};

        // BRANCH group = 0, PLAY sub-group = 2 (private consts in mobj)
        const GRP_BRANCH: u8 = 0;
        const BRANCH_PLAY: u8 = 2;

        let objects: Vec<MovieObject> = play_pls
            .iter()
            .map(|&pl| {
                // Encode PlayPl(immediate): group=BRANCH, sub_group=PLAY,
                // imm_op1=true, dst=playlist number
                MovieObject {
                    instructions: vec![Instruction {
                        op_cnt: 1,
                        group: GRP_BRANCH,
                        sub_group: BRANCH_PLAY,
                        imm_op1: true,
                        imm_op2: true,
                        branch_opt: 0,
                        cmp_opt: 0,
                        set_opt: 0,
                        dst: u32::from(pl),
                        src: 0,
                    }],
                }
            })
            .collect();

        MovieObjectFile { objects }
    }

    #[test]
    fn resolve_titles_to_playlists() {
        // MOBJs: 0→FBI (PL 999), 1→menu (PL 998), 2→PL 100, 3→PL 201, 4→PL 202
        let mobj_file = build_mobj_file(&[999, 998, 100, 201, 202]);

        let data = IndexBuilder::new()
            .first_play_hdmv(0)
            .top_menu_hdmv(1)
            .hdmv_title(0, 2)
            .hdmv_title(0, 3)
            .hdmv_title(0, 4)
            .build();

        let index = parse(&data).expect("should parse");
        let resolved = resolve_title_playlists(&index, &mobj_file, None);

        assert_eq!(resolved.len(), 3, "should resolve 3 title playlists");
        assert!(resolved.contains(&100), "should contain PL 100");
        assert!(resolved.contains(&201), "should contain PL 201");
        assert!(resolved.contains(&202), "should contain PL 202");
        // first_play and top_menu should NOT be included
        assert!(
            !resolved.contains(&999),
            "first_play PL should not be included"
        );
        assert!(
            !resolved.contains(&998),
            "top_menu PL should not be included"
        );
    }

    #[test]
    fn resolve_skips_bdj_titles() {
        let mobj_file = build_mobj_file(&[999, 998, 100]);

        let data = IndexBuilder::new()
            .hdmv_title(0, 2)
            .bdj_title(0, "00003")
            .build();

        let index = parse(&data).expect("should parse");
        let resolved = resolve_title_playlists(&index, &mobj_file, None);

        assert_eq!(resolved.len(), 1, "should resolve only HDMV title");
        assert!(resolved.contains(&100), "should contain PL 100");
    }

    #[test]
    fn resolve_handles_out_of_bounds_mobj() {
        // Only 2 MOBJs but title references MOBJ 5
        let mobj_file = build_mobj_file(&[100, 200]);

        let data = IndexBuilder::new().hdmv_title(0, 5).build();

        let index = parse(&data).expect("should parse");
        let resolved = resolve_title_playlists(&index, &mobj_file, None);

        assert!(
            resolved.is_empty(),
            "out-of-bounds MOBJ reference should produce empty set"
        );
    }
}
