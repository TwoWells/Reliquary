// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Interactive Graphics (IG) segment parser — extracts button→playlist
//! mappings and bitmap object definitions from raw PES payload bytes.
//!
//! IG segments share header format with PGS (Presentation Graphics).
//! The parser iterates segment headers, dispatches by type, and
//! accumulates results into display sets.
//!
//! Reference: libbluray `src/libbluray/decoders/ig_decode.c`, `ig.h`,
//! `hdmv_insn.h`.

use std::collections::HashMap;

use thiserror::Error;

use super::cursor::{Cursor, CursorError};

// ── Segment type constants ──────────────────────────────────────────────
//
// IG segment types differ from PGS. In PGS, 0x17 is the Presentation
// Composition Segment and 0x18 is End of Display Set. In IG streams,
// 0x18 is the Interactive Composition Segment and 0x80 is End of Display.
// Reference: libbluray `ig.h`.

/// Palette Definition Segment.
const SEG_PALETTE: u8 = 0x14;
/// Object Definition Segment.
const SEG_OBJECT: u8 = 0x15;
/// Interactive Composition Segment (IG-specific; 0x17 is PGS only).
const SEG_COMPOSITION: u8 = 0x18;
/// End of Display (IG end marker).
const SEG_END_OF_DISPLAY: u8 = 0x80;

// ── Errors ──────────────────────────────────────────────────────────────

/// Errors that can occur while parsing an IG stream.
#[derive(Debug, Error)]
pub enum IgError {
    /// Segment data is truncated.
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

impl From<CursorError> for IgError {
    fn from(e: CursorError) -> Self {
        Self::UnexpectedEof {
            offset: e.offset,
            needed: e.needed,
            available: e.available,
        }
    }
}

// ── Public types ────────────────────────────────────────────────────────

/// A parsed IG stream — all display sets from the PES payloads.
#[derive(Debug)]
pub struct IgStream {
    /// Display sets in presentation order.
    pub display_sets: Vec<DisplaySet>,
}

/// A single display set (complete menu state).
#[derive(Debug)]
pub struct DisplaySet {
    /// Color palettes for bitmap rendering.
    pub palettes: Vec<Palette>,
    /// RLE-compressed bitmap objects.
    pub objects: Vec<ObjectDefinition>,
    /// Interactive compositions (menu screens with buttons).
    pub compositions: Vec<InteractiveComposition>,
}

/// An Interactive Composition — one menu screen with buttons.
#[derive(Debug)]
pub struct InteractiveComposition {
    /// Video width in pixels.
    pub width: u16,
    /// Video height in pixels.
    pub height: u16,
    /// Menu pages within this composition.
    pub pages: Vec<Page>,
}

/// A menu page within a composition.
#[derive(Debug)]
pub struct Page {
    /// Page identifier.
    pub page_id: u8,
    /// Buttons on this page.
    pub buttons: Vec<Button>,
}

/// A menu button with bitmap references and navigation commands.
#[derive(Debug)]
pub struct Button {
    /// Button identifier.
    pub button_id: u16,
    /// Horizontal position in pixels.
    pub x: u16,
    /// Vertical position in pixels.
    pub y: u16,
    /// Object ID for the normal (unselected) state bitmap.
    pub normal_object_id: u16,
    /// Object ID for the selected state bitmap.
    pub selected_object_id: u16,
    /// Navigation commands — filtered to `PlayPl` commands where
    /// identifiable, others wrapped as `Other`.
    pub commands: Vec<NavigationCommand>,
}

/// An HDMV navigation command on a button.
#[derive(Debug, PartialEq, Eq)]
pub enum NavigationCommand {
    /// Play a playlist (the mapping we care about).
    PlayPl {
        /// Playlist number (e.g. 203 → `00203.mpls`).
        playlist: u16,
    },
    /// Any other command (opaque, not parsed further).
    Other {
        /// Raw opcode word.
        opcode: u32,
    },
}

/// An RLE-compressed bitmap object.
#[derive(Debug)]
pub struct ObjectDefinition {
    /// Object identifier (referenced by button states).
    pub object_id: u16,
    /// Bitmap width in pixels.
    pub width: u16,
    /// Bitmap height in pixels.
    pub height: u16,
    /// Raw RLE data (reassembled from multi-segment objects).
    pub rle_data: Vec<u8>,
}

/// A color palette.
#[derive(Debug)]
pub struct Palette {
    /// Palette identifier.
    pub palette_id: u8,
    /// Palette entries.
    pub entries: Vec<PaletteEntry>,
}

/// A single palette entry in `YCrCbA` color space.
#[derive(Debug)]
pub struct PaletteEntry {
    /// Palette index.
    pub index: u8,
    /// Luminance.
    pub y: u8,
    /// Red chrominance.
    pub cr: u8,
    /// Blue chrominance.
    pub cb: u8,
    /// Alpha (opacity).
    pub alpha: u8,
}

// ── Public API ──────────────────────────────────────────────────────────

/// Parses IG segments from raw PES payload bytes.
///
/// `data` is the concatenated payloads from all IG PES packets
/// in a single clip (from `demux` + `parse_pes`).
///
/// # Errors
///
/// Returns [`IgError`] if a segment header or required body is truncated.
/// Unknown segment types are skipped with a warning.
pub fn parse(data: &[u8]) -> Result<IgStream, IgError> {
    let mut r = Cursor::new(data);
    let mut display_sets = Vec::new();
    let mut current = CurrentDisplaySet::default();
    // Accumulator for multi-segment objects, keyed by object_id
    let mut object_acc: HashMap<u16, ObjectAccumulator> = HashMap::new();

    while r.remaining() >= 3 {
        let seg_type = r.read_u8()?;
        let seg_length = r.read_u16()? as usize;

        if r.remaining() < seg_length {
            return Err(IgError::UnexpectedEof {
                offset: r.pos,
                needed: seg_length,
                available: r.remaining(),
            });
        }

        // Read the segment body as a bounded sub-slice so individual
        // parsers cannot accidentally consume data from later segments.
        let seg_data = r.read_bytes(seg_length)?;

        match seg_type {
            SEG_PALETTE => {
                current
                    .palettes
                    .push(parse_palette(&mut Cursor::new(seg_data))?);
            }
            SEG_OBJECT => {
                parse_object_segment(
                    &mut Cursor::new(seg_data),
                    &mut object_acc,
                    &mut current.objects,
                )?;
            }
            SEG_COMPOSITION => {
                current
                    .compositions
                    .push(parse_interactive_composition(&mut Cursor::new(seg_data))?);
            }
            SEG_END_OF_DISPLAY => {
                // Finalize any in-progress multi-segment objects
                finalize_objects(&mut object_acc, &mut current.objects);
                display_sets.push(current.into_display_set());
                current = CurrentDisplaySet::default();
            }
            _ => {
                // Unknown segment type — skip silently for forward compatibility.
            }
        }
    }

    // If there's an unterminated display set with content, include it
    if current.has_content() {
        finalize_objects(&mut object_acc, &mut current.objects);
        display_sets.push(current.into_display_set());
    }

    Ok(IgStream { display_sets })
}

// ── Internal types ──────────────────────────────────────────────────────

/// Accumulates segments for a display set being built.
#[derive(Default)]
struct CurrentDisplaySet {
    palettes: Vec<Palette>,
    objects: Vec<ObjectDefinition>,
    compositions: Vec<InteractiveComposition>,
}

impl CurrentDisplaySet {
    #[allow(
        clippy::missing_const_for_fn,
        reason = "Vec::is_empty is not const-stable"
    )]
    fn has_content(&self) -> bool {
        !self.palettes.is_empty() || !self.objects.is_empty() || !self.compositions.is_empty()
    }

    fn into_display_set(self) -> DisplaySet {
        DisplaySet {
            palettes: self.palettes,
            objects: self.objects,
            compositions: self.compositions,
        }
    }
}

/// Accumulates RLE data across multi-segment objects.
struct ObjectAccumulator {
    object_id: u16,
    width: u16,
    height: u16,
    rle_data: Vec<u8>,
}

// ── Palette parsing ─────────────────────────────────────────────────────

/// Parses a Palette Definition Segment.
fn parse_palette(r: &mut Cursor<'_>) -> Result<Palette, IgError> {
    let palette_id = r.read_u8()?;
    let _palette_version = r.read_u8()?;

    // Entry count derived from remaining segment data
    let num_entries = r.remaining() / 5;

    let mut entries = Vec::with_capacity(num_entries);
    for _ in 0..num_entries {
        let index = r.read_u8()?;
        let y = r.read_u8()?;
        let cr = r.read_u8()?;
        let cb = r.read_u8()?;
        let alpha = r.read_u8()?;
        entries.push(PaletteEntry {
            index,
            y,
            cr,
            cb,
            alpha,
        });
    }

    Ok(Palette {
        palette_id,
        entries,
    })
}

// ── Object parsing ──────────────────────────────────────────────────────

/// Sequence flags for Object Definition Segments.
const SEQ_FIRST_AND_LAST: u8 = 0xC0;
const SEQ_FIRST: u8 = 0x80;
const SEQ_LAST: u8 = 0x40;

/// Parses an Object Definition Segment.
///
/// Multi-segment objects are accumulated by object ID. Complete objects
/// are pushed to `objects`.
fn parse_object_segment(
    r: &mut Cursor<'_>,
    object_acc: &mut HashMap<u16, ObjectAccumulator>,
    objects: &mut Vec<ObjectDefinition>,
) -> Result<(), IgError> {
    let object_id = r.read_u16()?;
    let _object_version = r.read_u8()?;
    let sequence_flag = r.read_u8()?;

    match sequence_flag {
        SEQ_FIRST_AND_LAST => {
            // Complete single-segment object
            // object_data_length (3 bytes) — includes width/height/rle
            let _data_length = read_u24(r)?;
            let width = r.read_u16()?;
            let height = r.read_u16()?;
            let rle_data = r.read_bytes(r.remaining())?.to_vec();
            objects.push(ObjectDefinition {
                object_id,
                width,
                height,
                rle_data,
            });
        }
        SEQ_FIRST => {
            // First segment of a multi-segment object
            let _data_length = read_u24(r)?;
            let width = r.read_u16()?;
            let height = r.read_u16()?;
            let rle_data = r.read_bytes(r.remaining())?.to_vec();
            object_acc.insert(
                object_id,
                ObjectAccumulator {
                    object_id,
                    width,
                    height,
                    rle_data,
                },
            );
        }
        SEQ_LAST => {
            // Last segment
            let rle_chunk = r.read_bytes(r.remaining())?;
            if let Some(mut acc) = object_acc.remove(&object_id) {
                acc.rle_data.extend_from_slice(rle_chunk);
                objects.push(ObjectDefinition {
                    object_id: acc.object_id,
                    width: acc.width,
                    height: acc.height,
                    rle_data: acc.rle_data,
                });
            }
        }
        _ => {
            // Middle segment (0x00)
            let rle_chunk = r.read_bytes(r.remaining())?;
            if let Some(acc) = object_acc.get_mut(&object_id) {
                acc.rle_data.extend_from_slice(rle_chunk);
            }
        }
    }

    Ok(())
}

/// Finalize any remaining multi-segment objects into the objects list.
fn finalize_objects(
    object_acc: &mut HashMap<u16, ObjectAccumulator>,
    objects: &mut Vec<ObjectDefinition>,
) {
    for (_, acc) in object_acc.drain() {
        objects.push(ObjectDefinition {
            object_id: acc.object_id,
            width: acc.width,
            height: acc.height,
            rle_data: acc.rle_data,
        });
    }
}

/// Reads a 3-byte big-endian unsigned integer.
fn read_u24(r: &mut Cursor<'_>) -> Result<u32, IgError> {
    let bytes = r.read_bytes(3)?;
    Ok(u32::from(bytes[0]) << 16 | u32::from(bytes[1]) << 8 | u32::from(bytes[2]))
}

// ── Interactive Composition parsing ─────────────────────────────────────

/// Parses an Interactive Composition Segment (0x18).
///
/// The segment body has a 9-byte segment-level header (video descriptor,
/// composition descriptor, sequence descriptor) followed by the IC body.
/// The IC body starts with a `data_length` (u24) and model flags.
///
/// For multi-segment ICs, the `sequence_descriptor` indicates first
/// (0x80), middle (0x00), or last (0x40) segments. Continuation
/// segments repeat the 9-byte segment header. Page data that spans
/// segment boundaries is parsed as far as each segment allows.
///
/// Reference: libbluray `ig_decode.c`, `graphics_processor.c`.
fn parse_interactive_composition(r: &mut Cursor<'_>) -> Result<InteractiveComposition, IgError> {
    // ── Segment-level header (9 bytes) ──
    let width = r.read_u16()?;
    let height = r.read_u16()?;
    // frame_rate_id (4 bits) + reserved (4 bits)
    r.skip(1)?;
    // composition_number (u16)
    r.skip(2)?;
    // composition_state (2 bits) + reserved (6 bits)
    r.skip(1)?;
    // sequence_descriptor
    r.skip(1)?;

    // ── IC body ──
    // data_length (u24) — total IC body size across all segments
    let _data_length = read_u24(r)?;

    // stream_model (1 bit) + ui_model (1 bit) + reserved (6 bits)
    let model_byte = r.read_u8()?;
    let stream_model = (model_byte >> 7) & 1;

    if stream_model == 0 {
        // 7 bits reserved + 1 bit uo_mask_table_flag (unused here)
        r.skip(1)?;
        // composition_timeout_pts (33 bits → 5 bytes)
        r.skip(5)?;
        // selection_timeout_pts (33 bits → 5 bytes)
        r.skip(5)?;
    }

    // user_timeout_duration (u24)
    r.skip(3)?;

    let num_pages = r.read_u8()?;

    // The IC may be split across multiple segments. Parse as many pages
    // as fit in this segment; continuation segments contribute their own
    // pages as separate compositions that are merged by the caller.
    let mut pages = Vec::with_capacity(num_pages as usize);
    for _ in 0..num_pages {
        if r.remaining() == 0 {
            break;
        }
        match parse_page(r) {
            Ok(page) => pages.push(page),
            Err(_) => break,
        }
    }

    Ok(InteractiveComposition {
        width,
        height,
        pages,
    })
}

/// Parses a single page within an Interactive Composition.
///
/// Reference: libbluray `_decode_page`.
fn parse_page(r: &mut Cursor<'_>) -> Result<Page, IgError> {
    let page_id = r.read_u8()?;
    let _page_version = r.read_u8()?;

    // UO_mask_table (8 bytes)
    r.skip(8)?;

    // in_effects (effect sequence)
    skip_effect_sequence(r)?;
    // out_effects (effect sequence)
    skip_effect_sequence(r)?;

    // animation_frame_rate_code (u8)
    r.skip(1)?;

    // default_selected_button_id (u16) + default_activated_button_id (u16)
    r.skip(4)?;

    // palette_id (u8)
    r.skip(1)?;

    let num_bogs = r.read_u8()?;

    let mut buttons = Vec::new();
    for _ in 0..num_bogs {
        parse_bog(r, &mut buttons)?;
    }

    Ok(Page { page_id, buttons })
}

/// Skips an effect sequence (windows + effects).
///
/// Reference: libbluray `_decode_effect_sequence`.
fn skip_effect_sequence(r: &mut Cursor<'_>) -> Result<(), IgError> {
    // Windows
    let num_windows = r.read_u8()?;
    for _ in 0..num_windows {
        // pg_decode_window: id(1) + x(2) + y(2) + w(2) + h(2) = 9 bytes
        r.skip(9)?;
    }
    // Effects
    let num_effects = r.read_u8()?;
    for _ in 0..num_effects {
        skip_effect(r)?;
    }
    Ok(())
}

/// Skips a single effect.
///
/// Reference: libbluray `_decode_effect`.
fn skip_effect(r: &mut Cursor<'_>) -> Result<(), IgError> {
    // effect_duration (u24)
    r.skip(3)?;
    // palette_id_ref (u8)
    r.skip(1)?;
    let num_composition_objects = r.read_u8()?;
    for _ in 0..num_composition_objects {
        skip_composition_object(r)?;
    }
    Ok(())
}

/// Skips a composition object within an effect.
///
/// Reference: libbluray `pg_decode_composition_object`.
fn skip_composition_object(r: &mut Cursor<'_>) -> Result<(), IgError> {
    // object_id_ref (u16) + window_id_ref (u8)
    r.skip(3)?;
    // crop_flag (1 bit) + forced_on_flag (1 bit) + reserved (6 bits)
    let flags = r.read_u8()?;
    let crop_flag = (flags >> 7) & 1;
    // x (u16) + y (u16)
    r.skip(4)?;
    if crop_flag == 1 {
        // crop_x (u16) + crop_y (u16) + crop_w (u16) + crop_h (u16)
        r.skip(8)?;
    }
    Ok(())
}

/// Parses a Button Overlap Group (BOG) and appends its buttons.
fn parse_bog(r: &mut Cursor<'_>, buttons: &mut Vec<Button>) -> Result<(), IgError> {
    // default_valid_button_id (u16)
    let _default_button = r.read_u16()?;
    let num_buttons = r.read_u8()?;

    for _ in 0..num_buttons {
        buttons.push(parse_button(r)?);
    }

    Ok(())
}

/// Parses a single button definition.
///
/// Reference: libbluray `_decode_button`.
fn parse_button(r: &mut Cursor<'_>) -> Result<Button, IgError> {
    let button_id = r.read_u16()?;
    let _numeric_value = r.read_u16()?;
    // auto_action_flag (1 bit) + reserved (7 bits)
    let _auto_action = r.read_u8()?;

    let x = r.read_u16()?;
    let y = r.read_u16()?;

    // neighbor navigation: up, down, left, right (u16 each)
    r.skip(8)?;

    // Normal state
    let normal_start_object_id = r.read_u16()?;
    let _normal_end_object_id = r.read_u16()?;
    // normal_repeat_flag (1 bit) + reserved (7 bits)
    r.skip(1)?;

    // selected_sound_id_ref (u8)
    r.skip(1)?;

    // Selected state
    let selected_start_object_id = r.read_u16()?;
    let _selected_end_object_id = r.read_u16()?;
    // selected_repeat_flag (1 bit) + reserved (7 bits)
    r.skip(1)?;

    // activated_sound_id_ref (u8)
    r.skip(1)?;

    // Activated state
    let _activated_start = r.read_u16()?;
    let _activated_end = r.read_u16()?;

    let num_commands = r.read_u16()?;

    let mut commands = Vec::with_capacity(num_commands as usize);
    for _ in 0..num_commands {
        commands.push(parse_navigation_command(r)?);
    }

    Ok(Button {
        button_id,
        x,
        y,
        normal_object_id: normal_start_object_id,
        selected_object_id: selected_start_object_id,
        commands,
    })
}

// ── Navigation command parsing ──────────────────────────────────────────

/// Parses a single HDMV navigation command (12 bytes).
///
/// HDMV commands are encoded as bytecode:
/// - Bytes 0-3: instruction word (opcode + operand count + flags)
/// - Bytes 4-7: destination operand
/// - Bytes 8-11: source operand
///
/// `PlayPL`: `group=0x2`, `sub_group=0x1`. The playlist number is in the
/// source operand (bytes 8-11).
fn parse_navigation_command(r: &mut Cursor<'_>) -> Result<NavigationCommand, IgError> {
    let insn = r.read_u32()?;
    let _dst = r.read_u32()?;
    let src = r.read_u32()?;

    // Instruction word layout:
    //   bits 31-28: group
    //   bits 27-24: sub_group
    let group = (insn >> 28) & 0x0F;
    let sub_group = (insn >> 24) & 0x0F;

    if group == 0x02 && sub_group == 0x01 {
        // PlayPL — playlist number from source operand (low 16 bits)
        #[allow(
            clippy::cast_possible_truncation,
            reason = "playlist numbers are u16 values"
        )]
        let playlist = (src & 0xFFFF) as u16;
        Ok(NavigationCommand::PlayPl { playlist })
    } else {
        Ok(NavigationCommand::Other { opcode: insn })
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
#[allow(
    clippy::cast_possible_truncation,
    reason = "test builder values are small known constants"
)]
#[allow(
    clippy::redundant_pub_crate,
    reason = "pub(crate) needed for cross-module test access"
)]
pub(crate) mod tests {
    use super::*;

    // ── IgBuilder ───────────────────────────────────────────────────

    /// Builds a valid IG segment byte stream for testing.
    pub(crate) struct IgBuilder {
        segments: Vec<Vec<u8>>,
    }

    impl IgBuilder {
        pub(crate) fn new() -> Self {
            Self {
                segments: Vec::new(),
            }
        }

        /// Adds a Palette Definition Segment.
        pub(crate) fn palette(mut self, palette_id: u8, entries: &[(u8, u8, u8, u8, u8)]) -> Self {
            let mut body = Vec::new();
            body.push(palette_id);
            body.push(0); // version
            for &(index, y, cr, cb, alpha) in entries {
                body.push(index);
                body.push(y);
                body.push(cr);
                body.push(cb);
                body.push(alpha);
            }
            self.segments.push(build_segment(SEG_PALETTE, &body));
            self
        }

        /// Adds a single-segment Object Definition.
        pub(crate) fn object(
            mut self,
            object_id: u16,
            width: u16,
            height: u16,
            rle_data: &[u8],
        ) -> Self {
            let mut body = Vec::new();
            body.extend_from_slice(&object_id.to_be_bytes());
            body.push(0); // version
            body.push(SEQ_FIRST_AND_LAST); // sequence_flag
            // object_data_length (3 bytes): width(2) + height(2) + rle
            let data_len = 4 + rle_data.len();
            body.push((data_len >> 16) as u8);
            body.push((data_len >> 8) as u8);
            body.push(data_len as u8);
            body.extend_from_slice(&width.to_be_bytes());
            body.extend_from_slice(&height.to_be_bytes());
            body.extend_from_slice(rle_data);
            self.segments.push(build_segment(SEG_OBJECT, &body));
            self
        }

        /// Adds the first segment of a multi-segment object.
        pub(crate) fn object_first(
            mut self,
            object_id: u16,
            width: u16,
            height: u16,
            total_rle_len: usize,
            rle_data: &[u8],
        ) -> Self {
            let mut body = Vec::new();
            body.extend_from_slice(&object_id.to_be_bytes());
            body.push(0); // version
            body.push(SEQ_FIRST);
            let data_len = 4 + total_rle_len;
            body.push((data_len >> 16) as u8);
            body.push((data_len >> 8) as u8);
            body.push(data_len as u8);
            body.extend_from_slice(&width.to_be_bytes());
            body.extend_from_slice(&height.to_be_bytes());
            body.extend_from_slice(rle_data);
            self.segments.push(build_segment(SEG_OBJECT, &body));
            self
        }

        /// Adds a middle segment of a multi-segment object.
        pub(crate) fn object_middle(mut self, object_id: u16, rle_data: &[u8]) -> Self {
            let mut body = Vec::new();
            body.extend_from_slice(&object_id.to_be_bytes());
            body.push(0); // version
            body.push(0x00); // middle
            body.extend_from_slice(rle_data);
            self.segments.push(build_segment(SEG_OBJECT, &body));
            self
        }

        /// Adds the last segment of a multi-segment object.
        pub(crate) fn object_last(mut self, object_id: u16, rle_data: &[u8]) -> Self {
            let mut body = Vec::new();
            body.extend_from_slice(&object_id.to_be_bytes());
            body.push(0); // version
            body.push(SEQ_LAST);
            body.extend_from_slice(rle_data);
            self.segments.push(build_segment(SEG_OBJECT, &body));
            self
        }

        /// Adds an Interactive Composition Segment with the given buttons.
        pub(crate) fn composition(mut self, width: u16, height: u16, pages: &[PageSpec]) -> Self {
            let mut body = Vec::new();
            // ── Segment-level header (9 bytes) ──
            body.extend_from_slice(&width.to_be_bytes());
            body.extend_from_slice(&height.to_be_bytes());
            body.push(0); // frame_rate_id + reserved
            body.extend_from_slice(&0u16.to_be_bytes()); // composition_number
            body.push(0); // composition_state + reserved
            body.push(0xC0); // sequence_descriptor: first + last
            // ── IC body ──
            body.extend_from_slice(&[0u8; 3]); // data_length
            body.push(0); // stream_model=0, ui_model=0
            body.push(0); // uo_mask_flag=0 + reserved
            body.extend_from_slice(&[0u8; 10]); // timeout PTSes (5+5)
            body.extend_from_slice(&[0u8; 3]); // user_timeout_duration

            body.push(pages.len() as u8);
            for page in pages {
                build_page(&mut body, page);
            }

            self.segments.push(build_segment(SEG_COMPOSITION, &body));
            self
        }

        /// Adds an End of Display marker.
        pub(crate) fn end_of_display(mut self) -> Self {
            self.segments.push(build_segment(SEG_END_OF_DISPLAY, &[]));
            self
        }

        /// Builds the complete byte stream.
        pub(crate) fn build(self) -> Vec<u8> {
            let mut data = Vec::new();
            for seg in self.segments {
                data.extend_from_slice(&seg);
            }
            data
        }
    }

    /// Specification for a page in the test builder.
    pub(crate) struct PageSpec {
        /// Page ID.
        pub page_id: u8,
        /// Buttons on this page, grouped into BOGs (one button per BOG).
        pub buttons: Vec<ButtonSpec>,
    }

    /// Specification for a button in the test builder.
    pub(crate) struct ButtonSpec {
        /// Button ID.
        pub button_id: u16,
        /// Horizontal position.
        pub x: u16,
        /// Vertical position.
        pub y: u16,
        /// Normal state object ID.
        pub normal_object_id: u16,
        /// Selected state object ID.
        pub selected_object_id: u16,
        /// Commands to attach.
        pub commands: Vec<CommandSpec>,
    }

    /// Specification for a navigation command in the test builder.
    pub(crate) enum CommandSpec {
        /// `PlayPL` command with a playlist number.
        PlayPl(u16),
        /// Some other command with an arbitrary opcode.
        Other(u32),
    }

    fn build_segment(seg_type: u8, body: &[u8]) -> Vec<u8> {
        let mut seg = Vec::new();
        seg.push(seg_type);
        seg.extend_from_slice(&(body.len() as u16).to_be_bytes());
        seg.extend_from_slice(body);
        seg
    }

    fn build_page(buf: &mut Vec<u8>, page: &PageSpec) {
        buf.push(page.page_id);
        buf.push(0); // page_version
        // UO_mask_table (8 bytes)
        buf.extend_from_slice(&[0u8; 8]);
        // in_effect_sequence: num_windows=0, num_effects=0
        buf.push(0);
        buf.push(0);
        // out_effect_sequence: num_windows=0, num_effects=0
        buf.push(0);
        buf.push(0);
        // animation_frame_rate_code
        buf.push(0);
        // default_selected_button_id + default_activated_button_id
        buf.extend_from_slice(&[0u8; 4]);
        // palette_id
        buf.push(0);
        // num_bogs — one BOG per button for simplicity
        buf.push(page.buttons.len() as u8);
        for button in &page.buttons {
            build_bog(buf, button);
        }
    }

    fn build_bog(buf: &mut Vec<u8>, button: &ButtonSpec) {
        // default_valid_button_id
        buf.extend_from_slice(&button.button_id.to_be_bytes());
        // num_buttons = 1 (one button per BOG)
        buf.push(1);
        build_button(buf, button);
    }

    fn build_button(buf: &mut Vec<u8>, button: &ButtonSpec) {
        buf.extend_from_slice(&button.button_id.to_be_bytes());
        buf.extend_from_slice(&0u16.to_be_bytes()); // numeric_value
        buf.push(0); // auto_action
        buf.extend_from_slice(&button.x.to_be_bytes());
        buf.extend_from_slice(&button.y.to_be_bytes());
        // neighbor navigation: up, down, left, right
        buf.extend_from_slice(&[0u8; 8]);
        // Normal state
        buf.extend_from_slice(&button.normal_object_id.to_be_bytes()); // start
        buf.extend_from_slice(&button.normal_object_id.to_be_bytes()); // end
        buf.push(0); // repeat_flag + reserved
        // selected_sound_id_ref
        buf.push(0xFF);
        // Selected state
        buf.extend_from_slice(&button.selected_object_id.to_be_bytes()); // start
        buf.extend_from_slice(&button.selected_object_id.to_be_bytes()); // end
        buf.push(0); // repeat_flag + reserved
        // activated_sound_id_ref
        buf.push(0xFF);
        // Activated state
        buf.extend_from_slice(&button.selected_object_id.to_be_bytes()); // start
        buf.extend_from_slice(&button.selected_object_id.to_be_bytes()); // end
        // Commands
        buf.extend_from_slice(&(button.commands.len() as u16).to_be_bytes());
        for cmd in &button.commands {
            build_command(buf, cmd);
        }
    }

    fn build_command(buf: &mut Vec<u8>, cmd: &CommandSpec) {
        match cmd {
            CommandSpec::PlayPl(playlist) => {
                // group=0x2, sub_group=0x1 → insn = 0x2100_0000
                // plus operand count bits: we need imm operand mode
                // From hdmv_insn.h: PlayPL uses branch group (0x2), sub=0x1
                // op_cnt=1 (one source operand)
                // bit layout: group(4) sub(4) op_cnt(3) ...
                // Full instruction: 0x2110_0000 (group=2, sub=1, op_cnt=1, imm=1)
                let insn: u32 = 0x2110_0000;
                buf.extend_from_slice(&insn.to_be_bytes());
                buf.extend_from_slice(&0u32.to_be_bytes()); // dst
                buf.extend_from_slice(&u32::from(*playlist).to_be_bytes()); // src
            }
            CommandSpec::Other(opcode) => {
                buf.extend_from_slice(&opcode.to_be_bytes());
                buf.extend_from_slice(&0u32.to_be_bytes()); // dst
                buf.extend_from_slice(&0u32.to_be_bytes()); // src
            }
        }
    }

    // ── Segment iteration tests ─────────────────────────────────────

    #[test]
    fn single_display_set() {
        let rle = vec![0xAA, 0xBB];
        let data = IgBuilder::new()
            .palette(0, &[(0, 255, 128, 128, 255)])
            .object(0, 100, 30, &rle)
            .composition(
                1920,
                1080,
                &[PageSpec {
                    page_id: 0,
                    buttons: vec![ButtonSpec {
                        button_id: 0,
                        x: 100,
                        y: 200,
                        normal_object_id: 0,
                        selected_object_id: 0,
                        commands: vec![CommandSpec::PlayPl(203)],
                    }],
                }],
            )
            .end_of_display()
            .build();

        let stream = parse(&data).expect("should parse single display set");
        assert_eq!(stream.display_sets.len(), 1, "one display set");
        let ds = &stream.display_sets[0];
        assert_eq!(ds.palettes.len(), 1, "one palette");
        assert_eq!(ds.objects.len(), 1, "one object");
        assert_eq!(ds.compositions.len(), 1, "one composition");
    }

    #[test]
    fn multiple_display_sets() {
        let rle = vec![0xAA];
        let data = IgBuilder::new()
            .palette(0, &[(0, 255, 128, 128, 255)])
            .object(0, 50, 20, &rle)
            .composition(1920, 1080, &[])
            .end_of_display()
            .palette(1, &[(0, 0, 0, 0, 255)])
            .object(1, 60, 25, &rle)
            .composition(1920, 1080, &[])
            .end_of_display()
            .build();

        let stream = parse(&data).expect("should parse multiple display sets");
        assert_eq!(stream.display_sets.len(), 2, "two display sets");
        assert_eq!(
            stream.display_sets[0].palettes[0].palette_id, 0,
            "first display set palette id"
        );
        assert_eq!(
            stream.display_sets[1].palettes[0].palette_id, 1,
            "second display set palette id"
        );
    }

    #[test]
    fn unknown_segment_type_skipped() {
        // Build a stream with an unknown segment type (0x99) between valid segments
        let rle = vec![0xAA];
        let mut data = IgBuilder::new()
            .palette(0, &[(0, 255, 128, 128, 255)])
            .build();
        // Insert unknown segment (u8 type + u16 length)
        let unknown_body = vec![0x01, 0x02, 0x03];
        data.push(0x99); // unknown type
        data.extend_from_slice(&(unknown_body.len() as u16).to_be_bytes());
        data.extend_from_slice(&unknown_body);
        // Add object + end
        let rest = IgBuilder::new()
            .object(0, 50, 20, &rle)
            .end_of_display()
            .build();
        data.extend_from_slice(&rest);

        let stream = parse(&data).expect("should skip unknown segment type");
        assert_eq!(stream.display_sets.len(), 1, "one display set");
        assert_eq!(
            stream.display_sets[0].objects.len(),
            1,
            "object parsed after unknown segment"
        );
    }

    // ── Interactive Composition tests ───────────────────────────────

    #[test]
    fn buttons_with_play_pl_commands() {
        let data = IgBuilder::new()
            .composition(
                1920,
                1080,
                &[PageSpec {
                    page_id: 0,
                    buttons: vec![
                        ButtonSpec {
                            button_id: 0,
                            x: 100,
                            y: 200,
                            normal_object_id: 0,
                            selected_object_id: 1,
                            commands: vec![CommandSpec::PlayPl(203)],
                        },
                        ButtonSpec {
                            button_id: 1,
                            x: 100,
                            y: 260,
                            normal_object_id: 2,
                            selected_object_id: 3,
                            commands: vec![CommandSpec::PlayPl(204)],
                        },
                    ],
                }],
            )
            .end_of_display()
            .build();

        let stream = parse(&data).expect("should parse buttons with PlayPL");
        let comp = &stream.display_sets[0].compositions[0];
        assert_eq!(comp.width, 1920, "composition width");
        assert_eq!(comp.height, 1080, "composition height");
        assert_eq!(comp.pages.len(), 1, "one page");

        let page = &comp.pages[0];
        assert_eq!(page.buttons.len(), 2, "two buttons");

        assert_eq!(page.buttons[0].button_id, 0, "button 0 id");
        assert_eq!(page.buttons[0].x, 100, "button 0 x");
        assert_eq!(page.buttons[0].y, 200, "button 0 y");
        assert_eq!(page.buttons[0].normal_object_id, 0, "button 0 normal obj");
        assert_eq!(
            page.buttons[0].selected_object_id, 1,
            "button 0 selected obj"
        );
        assert_eq!(
            page.buttons[0].commands,
            vec![NavigationCommand::PlayPl { playlist: 203 }],
            "button 0 PlayPL command"
        );

        assert_eq!(
            page.buttons[1].commands,
            vec![NavigationCommand::PlayPl { playlist: 204 }],
            "button 1 PlayPL command"
        );
    }

    #[test]
    fn multi_page_composition() {
        let data = IgBuilder::new()
            .composition(
                1920,
                1080,
                &[
                    PageSpec {
                        page_id: 0,
                        buttons: vec![ButtonSpec {
                            button_id: 0,
                            x: 10,
                            y: 20,
                            normal_object_id: 0,
                            selected_object_id: 1,
                            commands: vec![CommandSpec::PlayPl(100)],
                        }],
                    },
                    PageSpec {
                        page_id: 1,
                        buttons: vec![ButtonSpec {
                            button_id: 1,
                            x: 30,
                            y: 40,
                            normal_object_id: 2,
                            selected_object_id: 3,
                            commands: vec![CommandSpec::PlayPl(101)],
                        }],
                    },
                ],
            )
            .end_of_display()
            .build();

        let stream = parse(&data).expect("should parse multi-page composition");
        let comp = &stream.display_sets[0].compositions[0];
        assert_eq!(comp.pages.len(), 2, "two pages");
        assert_eq!(comp.pages[0].page_id, 0, "page 0 id");
        assert_eq!(comp.pages[1].page_id, 1, "page 1 id");
        assert_eq!(comp.pages[0].buttons.len(), 1, "page 0 button count");
        assert_eq!(comp.pages[1].buttons.len(), 1, "page 1 button count");
    }

    #[test]
    fn button_with_no_commands() {
        let data = IgBuilder::new()
            .composition(
                1920,
                1080,
                &[PageSpec {
                    page_id: 0,
                    buttons: vec![ButtonSpec {
                        button_id: 0,
                        x: 10,
                        y: 20,
                        normal_object_id: 0,
                        selected_object_id: 1,
                        commands: vec![],
                    }],
                }],
            )
            .end_of_display()
            .build();

        let stream = parse(&data).expect("should parse button with no commands");
        let button = &stream.display_sets[0].compositions[0].pages[0].buttons[0];
        assert!(button.commands.is_empty(), "empty commands list");
    }

    #[test]
    fn button_with_non_play_pl_commands() {
        let other_opcode: u32 = 0x1000_0000; // some non-PlayPL opcode
        let data = IgBuilder::new()
            .composition(
                1920,
                1080,
                &[PageSpec {
                    page_id: 0,
                    buttons: vec![ButtonSpec {
                        button_id: 0,
                        x: 10,
                        y: 20,
                        normal_object_id: 0,
                        selected_object_id: 1,
                        commands: vec![CommandSpec::Other(other_opcode)],
                    }],
                }],
            )
            .end_of_display()
            .build();

        let stream = parse(&data).expect("should parse non-PlayPL commands");
        let button = &stream.display_sets[0].compositions[0].pages[0].buttons[0];
        assert_eq!(
            button.commands,
            vec![NavigationCommand::Other {
                opcode: other_opcode
            }],
            "command wrapped as Other"
        );
    }

    // ── Object Definition tests ─────────────────────────────────────

    #[test]
    fn single_segment_object() {
        let rle = vec![0x01, 0x02, 0x03, 0x04];
        let data = IgBuilder::new()
            .object(42, 100, 30, &rle)
            .end_of_display()
            .build();

        let stream = parse(&data).expect("should parse single-segment object");
        let obj = &stream.display_sets[0].objects[0];
        assert_eq!(obj.object_id, 42, "object id");
        assert_eq!(obj.width, 100, "object width");
        assert_eq!(obj.height, 30, "object height");
        assert_eq!(obj.rle_data, rle, "RLE data");
    }

    #[test]
    fn multi_segment_object() {
        let chunk1 = vec![0x01, 0x02];
        let chunk2 = vec![0x03, 0x04];
        let chunk3 = vec![0x05, 0x06];
        let total_rle_len = chunk1.len() + chunk2.len() + chunk3.len();

        let data = IgBuilder::new()
            .object_first(7, 200, 50, total_rle_len, &chunk1)
            .object_middle(7, &chunk2)
            .object_last(7, &chunk3)
            .end_of_display()
            .build();

        let stream = parse(&data).expect("should parse multi-segment object");
        let obj = &stream.display_sets[0].objects[0];
        assert_eq!(obj.object_id, 7, "object id");
        assert_eq!(obj.width, 200, "object width");
        assert_eq!(obj.height, 50, "object height");
        assert_eq!(
            obj.rle_data,
            vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06],
            "reassembled RLE data"
        );
    }

    // ── Palette Definition tests ────────────────────────────────────

    #[test]
    fn palette_with_multiple_entries() {
        let data = IgBuilder::new()
            .palette(
                5,
                &[
                    (0, 235, 128, 128, 255), // white, opaque
                    (1, 16, 128, 128, 255),  // black, opaque
                    (2, 81, 90, 240, 128),   // blue, semi-transparent
                ],
            )
            .end_of_display()
            .build();

        let stream = parse(&data).expect("should parse palette");
        let pal = &stream.display_sets[0].palettes[0];
        assert_eq!(pal.palette_id, 5, "palette id");
        assert_eq!(pal.entries.len(), 3, "entry count");

        assert_eq!(pal.entries[0].index, 0, "entry 0 index");
        assert_eq!(pal.entries[0].y, 235, "entry 0 Y");
        assert_eq!(pal.entries[0].cr, 128, "entry 0 Cr");
        assert_eq!(pal.entries[0].cb, 128, "entry 0 Cb");
        assert_eq!(pal.entries[0].alpha, 255, "entry 0 alpha");

        assert_eq!(pal.entries[2].index, 2, "entry 2 index");
        assert_eq!(pal.entries[2].alpha, 128, "entry 2 alpha");
    }

    // ── Integration test ────────────────────────────────────────────

    #[test]
    fn complete_ig_stream_button_to_playlist_mapping() {
        let rle_normal = vec![0xAA; 10];
        let rle_selected = vec![0xBB; 10];

        let data = IgBuilder::new()
            .palette(0, &[(0, 235, 128, 128, 255), (1, 16, 128, 128, 0)])
            .object(0, 120, 30, &rle_normal)
            .object(1, 120, 30, &rle_selected)
            .object(2, 120, 30, &rle_normal)
            .object(3, 120, 30, &rle_selected)
            .composition(
                1920,
                1080,
                &[PageSpec {
                    page_id: 0,
                    buttons: vec![
                        ButtonSpec {
                            button_id: 0,
                            x: 100,
                            y: 200,
                            normal_object_id: 0,
                            selected_object_id: 1,
                            commands: vec![CommandSpec::PlayPl(203)],
                        },
                        ButtonSpec {
                            button_id: 1,
                            x: 100,
                            y: 260,
                            normal_object_id: 2,
                            selected_object_id: 3,
                            commands: vec![CommandSpec::PlayPl(204)],
                        },
                    ],
                }],
            )
            .end_of_display()
            .build();

        let stream = parse(&data).expect("should parse complete IG stream");
        assert_eq!(stream.display_sets.len(), 1, "one display set");

        let ds = &stream.display_sets[0];

        // Verify palette
        assert_eq!(ds.palettes.len(), 1, "one palette");
        assert_eq!(ds.palettes[0].entries.len(), 2, "two palette entries");

        // Verify objects
        assert_eq!(ds.objects.len(), 4, "four objects");
        for obj in &ds.objects {
            assert_eq!(obj.width, 120, "object width");
            assert_eq!(obj.height, 30, "object height");
        }

        // Verify button → playlist mapping
        let comp = &ds.compositions[0];
        let buttons = &comp.pages[0].buttons;
        assert_eq!(buttons.len(), 2, "two buttons");

        // Button 0 → playlist 203
        assert_eq!(buttons[0].normal_object_id, 0, "button 0 normal obj");
        assert_eq!(buttons[0].selected_object_id, 1, "button 0 selected obj");
        assert_eq!(
            buttons[0].commands,
            vec![NavigationCommand::PlayPl { playlist: 203 }],
            "button 0 plays playlist 203"
        );

        // Button 1 → playlist 204
        assert_eq!(buttons[1].normal_object_id, 2, "button 1 normal obj");
        assert_eq!(buttons[1].selected_object_id, 3, "button 1 selected obj");
        assert_eq!(
            buttons[1].commands,
            vec![NavigationCommand::PlayPl { playlist: 204 }],
            "button 1 plays playlist 204"
        );

        // Verify object IDs referenced by buttons exist in the display set
        let obj_ids: Vec<u16> = ds.objects.iter().map(|o| o.object_id).collect();
        for button in buttons {
            assert!(
                obj_ids.contains(&button.normal_object_id),
                "normal object {} exists in display set",
                button.normal_object_id
            );
            assert!(
                obj_ids.contains(&button.selected_object_id),
                "selected object {} exists in display set",
                button.selected_object_id
            );
        }
    }
}
