// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! `MovieObject.bdmv` parser and button→playlist resolver.
//!
//! Parses the HDMV navigation programs and traces register-based button
//! commands through movie objects to resolve indirect playlist mappings.
//!
//! Reference: libbluray `src/libbluray/bdnav/mobj_parse.c`,
//! `src/libbluray/decoders/hdmv_insn.h`.

use thiserror::Error;

use super::cursor::{Cursor, CursorError};
use super::ig::{Button, NavigationCommand, Page};

// ── Errors ──────────────────────────────────────────────────────────────

/// Errors that can occur while parsing `MovieObject.bdmv`.
#[derive(Debug, Error)]
pub enum MobjError {
    /// File is smaller than the minimum header size.
    #[error("file too small ({size} bytes, need at least 50)")]
    TooSmall {
        /// Actual file size.
        size: usize,
    },

    /// Magic bytes are not `"MOBJ"`.
    #[error("invalid magic: expected \"MOBJ\", got {found:?}")]
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

impl From<CursorError> for MobjError {
    fn from(e: CursorError) -> Self {
        Self::UnexpectedEof {
            offset: e.offset,
            needed: e.needed,
            available: e.available,
        }
    }
}

// ── Types ───────────────────────────────────────────────────────────────

/// A parsed `MovieObject.bdmv` file.
#[derive(Debug)]
pub struct MovieObjectFile {
    /// Movie objects in file order (0-indexed).
    pub objects: Vec<MovieObject>,
}

/// A single movie object (navigation program).
#[derive(Debug)]
pub struct MovieObject {
    /// Instructions in execution order.
    pub instructions: Vec<Instruction>,
}

/// A decoded HDMV instruction (12 bytes on disc).
///
/// Preserves all decoded fields so the resolver can pattern-match on
/// instruction sequences without re-parsing raw bytes.
#[derive(Debug, Clone)]
pub struct Instruction {
    /// Operand count (0, 1, or 2).
    pub op_cnt: u8,
    /// Instruction group — 0=BRANCH, 1=CMP, 2=SET.
    pub group: u8,
    /// Sub-group within the instruction group.
    pub sub_group: u8,
    /// Destination operand is an immediate value (not a register reference).
    pub imm_op1: bool,
    /// Source operand is an immediate value (not a register reference).
    pub imm_op2: bool,
    /// Branch option (BRANCH and CMP groups).
    pub branch_opt: u8,
    /// Comparison type (CMP group).
    pub cmp_opt: u8,
    /// Set operation type (SET group) — 1=move, 3=add, etc.
    pub set_opt: u8,
    /// Raw destination operand (bytes 4-7).
    pub dst: u32,
    /// Raw source operand (bytes 8-11).
    pub src: u32,
}

/// A playlist target resolved from MOBJ VM execution.
///
/// Carries the `PlayPl` variant fields so callers can distinguish between
/// `PlayPL` (from start), `PlayPLatMK` (at mark), and `PlayPLatPI` (at PI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayTarget {
    /// Playlist number.
    pub playlist: u16,
    /// `PlayPl` variant: 0=from start, 1=at mark, 2=at play item.
    pub branch_opt: u8,
    /// Mark index or play item index (meaningful when `branch_opt > 0`).
    pub mark_or_pi: u32,
}

/// A resolved button → playlist mapping.
#[derive(Debug, PartialEq, Eq)]
pub struct ResolvedButton {
    /// Button identifier from the IG data.
    pub button_id: u16,
    /// Resolved playlist target.
    pub target: PlayTarget,
}

/// A dispatch entry point found in an MOBJ for GPR dispatch resolution.
///
/// In the GPR dispatch pattern (Warner Bros. authoring), the MOBJ calls
/// `PlayPl(menu_playlist)` which suspends the MOBJ. After a button is
/// activated, the MOBJ resumes at `dispatch_pc` (the instruction after
/// the menu `PlayPl`) and reads the button's GPR to dispatch to the
/// correct content playlist.
#[derive(Debug, Clone)]
pub struct DispatchEntry {
    /// Index of the MOBJ containing the dispatch logic.
    pub mobj_index: usize,
    /// Instruction index where dispatch resumes (pc+1 after the menu `PlayPl`).
    pub dispatch_pc: usize,
}

/// A dispatch table extracted from a central dispatch MOBJ.
///
/// In the `SET_BUTTON_PAGE` dispatch pattern (Warner Bros. authoring),
/// a central MOBJ contains a CMP/GOTO switch table that dispatches
/// on a register value (typically `GPR[3002]`) to handler blocks, each
/// ending with `PlayPl(playlist)`. This table maps case values directly
/// to playlist numbers, bypassing the player runtime.
///
/// **Derivation (from libbluray `hdmv_vm.c:837` and
/// `graphics_controller.c:1644`):** `SET_BUTTON_PAGE` in an IG context
/// packs `button_id = dst & 0xFFFF` into the event param. The graphics
/// controller calls `_select_button(gc, button_id)` which writes
/// `PSR[10] = button_id`. The dispatch MOBJ copies PSR\[10\] into the
/// switch register at its resume point.
///
/// Two resolution paths:
/// - **Buttons with commands:** bytecode computes a composite (e.g.
///   `(PSR[10] & 0xFFFF) + key`) and calls `SET_BUTTON_PAGE`. The
///   composite value is the dispatch case.
/// - **NOP anchor buttons:** buttons with no commands whose
///   `button_id` directly matches a dispatch case. When the player
///   selects an anchor (via cursor navigation or `SET_BUTTON_PAGE`
///   page transition), `PSR[10] = button_id` becomes the case.
#[derive(Debug)]
pub struct DispatchTable {
    /// MOBJ index containing the dispatch table.
    pub mobj_index: usize,
    /// The register read by the switch (e.g. `GPR[3002]`).
    pub dispatch_register: u32,
    /// Mapping from case value to playlist number.
    pub cases: Vec<(u32, u16)>,
}

// ── Instruction group constants ─────────────────────────────────────────

/// BRANCH group — goto, jump to MOBJ, play playlist.
const GRP_BRANCH: u8 = 0;
/// CMP group — compare and conditionally branch.
const GRP_CMP: u8 = 1;
/// SET group — register operations.
const GRP_SET: u8 = 2;

/// BRANCH sub-groups.
const BRANCH_GOTO: u8 = 0;
const BRANCH_JUMP: u8 = 1;
const BRANCH_PLAY: u8 = 2;

/// `SET_BUTTON_PAGE` operation within the SETSYSTEM sub-group (`sub_group=1`).
///
/// Terminates IG execution and navigates to a new button/page. The
/// player runtime packs `button_id = dst & 0xFFFF` and selects that
/// button on the target page, updating `PSR[10]`. The dispatch MOBJ
/// reads `PSR[10]` into its switch register on resume.
const SET_BUTTON_PAGE_OPT: u8 = 3;

/// Maximum instructions the mini-VM will execute before giving up.
/// Prevents infinite loops in malformed or exotic MOBJ bytecode.
/// Set high enough to cover WB First Play MOBJs (up to ~3000
/// instructions with ~2 steps per CMP/GOTO pair ≈ 6000 effective
/// steps for the disc configuration database initialization).
const VM_STEP_LIMIT: u32 = 10_000;

/// PSR (Player Status Register) bit flag. Register references with
/// bit 31 set address PSRs rather than GPRs.
const PSR_FLAG: u32 = 0x8000_0000;

/// Player context for VM execution — known PSR values derived from
/// the IG structure at extraction time.
#[derive(Debug, Clone, Default)]
pub struct PlayerContext {
    /// IG stream number (PSR 0).
    pub ig_stream: u16,
    /// Selected button ID (PSR 10).
    pub selected_button_id: u16,
    /// Current page ID (PSR 11).
    pub page_id: u8,
}

// ── Parser ──────────────────────────────────────────────────────────────

/// Parses `MovieObject.bdmv` from raw bytes.
///
/// # Errors
///
/// Returns [`MobjError`] if the file has an invalid header or is truncated.
pub fn parse(data: &[u8]) -> Result<MovieObjectFile, MobjError> {
    // Minimum size: 40-byte header + 4 length + 4 reserved + 2 num_objects
    if data.len() < 50 {
        return Err(MobjError::TooSmall { size: data.len() });
    }

    // Magic: "MOBJ"
    let magic: [u8; 4] = [data[0], data[1], data[2], data[3]];
    if &magic != b"MOBJ" {
        return Err(MobjError::InvalidMagic { found: magic });
    }

    // Skip version (bytes 4-7), extension_data_start (bytes 8-11),
    // reserved (bytes 12-39), length (bytes 40-43), reserved (bytes 44-47).
    // num_objects at bytes 48-49.
    let mut r = Cursor::new(data);
    r.seek(48)?;
    let num_objects = r.read_u16()?;

    let mut objects = Vec::with_capacity(num_objects as usize);

    for _ in 0..num_objects {
        objects.push(parse_object(&mut r)?);
    }

    Ok(MovieObjectFile { objects })
}

/// Parses a single movie object: flags + instruction list.
fn parse_object(r: &mut Cursor<'_>) -> Result<MovieObject, MobjError> {
    // From libbluray mobj_parse.c:
    //   resume_intention_flag (1 bit) + menu_call_mask (1 bit) +
    //   title_search_mask (1 bit) + 13 reserved bits = 2 bytes
    //   num_nav_cmds (u16 BE) = 2 bytes
    // Total per-object header: 4 bytes
    r.skip(2)?; // flags + reserved
    let num_commands = r.read_u16()?;

    let mut instructions = Vec::with_capacity(num_commands as usize);

    for _ in 0..num_commands {
        instructions.push(parse_instruction(r)?);
    }

    Ok(MovieObject { instructions })
}

/// Decodes a single 12-byte HDMV instruction.
fn parse_instruction(r: &mut Cursor<'_>) -> Result<Instruction, MobjError> {
    let insn = r.read_u32()?;
    let dst = r.read_u32()?;
    let src = r.read_u32()?;

    #[allow(
        clippy::cast_possible_truncation,
        reason = "bit fields are small known widths (2-5 bits)"
    )]
    Ok(Instruction {
        op_cnt: ((insn >> 29) & 0x07) as u8,
        group: ((insn >> 27) & 0x03) as u8,
        sub_group: ((insn >> 24) & 0x07) as u8,
        imm_op1: (insn >> 23) & 1 != 0,
        imm_op2: (insn >> 22) & 1 != 0,
        branch_opt: ((insn >> 16) & 0x0F) as u8,
        cmp_opt: ((insn >> 8) & 0x0F) as u8,
        set_opt: (insn & 0x1F) as u8,
        dst,
        src,
    })
}

/// Converts a [`NavigationCommand`] to an [`Instruction`] for VM execution.
///
/// Typed variants (`PlayPl`, `SetGpr`, `GotoMobj`) are reconstructed into
/// their instruction encoding. `Other` variants are decoded from the raw
/// instruction word, identical to [`parse_instruction`].
#[must_use]
pub fn command_to_instruction(cmd: &NavigationCommand) -> Instruction {
    match cmd {
        NavigationCommand::PlayPl {
            playlist,
            branch_opt,
            mark_or_pi,
        } => Instruction {
            op_cnt: if *branch_opt == 0 { 1 } else { 2 },
            group: GRP_BRANCH,
            sub_group: BRANCH_PLAY,
            imm_op1: true,
            imm_op2: *branch_opt != 0,
            branch_opt: *branch_opt,
            cmp_opt: 0,
            set_opt: 0,
            dst: u32::from(*playlist),
            src: *mark_or_pi,
        },
        NavigationCommand::SetGpr { register, value } => Instruction {
            op_cnt: 2,
            group: GRP_SET,
            sub_group: 0,
            imm_op1: false,
            imm_op2: true,
            branch_opt: 0,
            cmp_opt: 0,
            set_opt: 0x01,
            dst: *register,
            src: *value,
        },
        NavigationCommand::GotoMobj { object_id } => Instruction {
            op_cnt: 1,
            group: GRP_BRANCH,
            sub_group: BRANCH_JUMP,
            imm_op1: true,
            imm_op2: false,
            branch_opt: 0x01,
            cmp_opt: 0,
            set_opt: 0,
            dst: *object_id,
            src: 0,
        },
        NavigationCommand::Other { opcode, dst, src } =>
        {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "bit fields are small known widths (2-5 bits)"
            )]
            Instruction {
                op_cnt: ((opcode >> 29) & 0x07) as u8,
                group: ((opcode >> 27) & 0x03) as u8,
                sub_group: ((opcode >> 24) & 0x07) as u8,
                imm_op1: (opcode >> 23) & 1 != 0,
                imm_op2: (opcode >> 22) & 1 != 0,
                branch_opt: ((opcode >> 16) & 0x0F) as u8,
                cmp_opt: ((opcode >> 8) & 0x0F) as u8,
                set_opt: (opcode & 0x1F) as u8,
                dst: *dst,
                src: *src,
            }
        }
    }
}

/// Formats an [`Instruction`] as a human-readable disassembly string.
///
/// Used by `--trace` to dump full button programs including `Other`
/// variants (CMP, GOTO, AND, ADD, `SET_BUTTON_PAGE`) that the typed
/// [`NavigationCommand`] variants don't expose.
#[must_use]
pub fn format_instruction(insn: &Instruction) -> String {
    let fmt_op = |is_imm: bool, raw: u32| -> String {
        if is_imm {
            format!("{raw}")
        } else if raw & PSR_FLAG != 0 {
            format!("PSR[{}]", raw & !PSR_FLAG)
        } else {
            format!("GPR[{raw}]")
        }
    };

    match insn.group {
        GRP_BRANCH => match insn.sub_group {
            BRANCH_GOTO => match insn.branch_opt {
                0x00 => "NOP".to_string(),
                0x01 => format!("GOTO {}", fmt_op(insn.imm_op1, insn.dst)),
                0x02 => "BREAK".to_string(),
                _ => format!("BRANCH(opt={})", insn.branch_opt),
            },
            BRANCH_JUMP => {
                format!("JumpMobj({})", fmt_op(insn.imm_op1, insn.dst))
            }
            BRANCH_PLAY => match insn.branch_opt {
                0 => format!("PlayPl({})", fmt_op(insn.imm_op1, insn.dst)),
                1 => format!(
                    "PlayPlatMK({}, mark={})",
                    fmt_op(insn.imm_op1, insn.dst),
                    fmt_op(insn.imm_op2, insn.src)
                ),
                2 => format!(
                    "PlayPlatPI({}, pi={})",
                    fmt_op(insn.imm_op1, insn.dst),
                    fmt_op(insn.imm_op2, insn.src)
                ),
                _ => format!(
                    "PlayPl({}, opt={})",
                    fmt_op(insn.imm_op1, insn.dst),
                    insn.branch_opt
                ),
            },
            _ => format!("BRANCH(sub={}, opt={})", insn.sub_group, insn.branch_opt),
        },
        GRP_CMP => {
            let op = match insn.cmp_opt {
                0x02 => "==",
                0x03 => "!=",
                0x04 => ">=",
                0x05 => ">",
                0x06 => "<=",
                0x07 => "<",
                _ => "?",
            };
            format!(
                "CMP {} {} {}",
                fmt_op(insn.imm_op1, insn.dst),
                op,
                fmt_op(insn.imm_op2, insn.src),
            )
        }
        GRP_SET if insn.sub_group == 1 => {
            if insn.set_opt == SET_BUTTON_PAGE_OPT {
                format!(
                    "SET_BUTTON_PAGE({}, {})",
                    fmt_op(insn.imm_op1, insn.dst),
                    fmt_op(insn.imm_op2, insn.src),
                )
            } else {
                format!(
                    "SETSYSTEM(opt={}, {}, {})",
                    insn.set_opt,
                    fmt_op(insn.imm_op1, insn.dst),
                    fmt_op(insn.imm_op2, insn.src),
                )
            }
        }
        GRP_SET => {
            let dst = fmt_op(false, insn.dst);
            let src = fmt_op(insn.imm_op2, insn.src);
            match insn.set_opt {
                0x01 => format!("{dst} = {src}"),
                0x02 => format!("{dst} <=> {src}"),
                0x03 => format!("{dst} += {src}"),
                0x04 => format!("{dst} -= {src}"),
                0x05 => format!("{dst} *= {src}"),
                0x06 => format!("{dst} /= {src}"),
                0x07 => format!("{dst} %= {src}"),
                0x08 => format!("{dst} = rand({src})"),
                0x09 => format!("{dst} &= {src}"),
                0x0A => format!("{dst} |= {src}"),
                0x0B => format!("{dst} ^= {src}"),
                0x0C => format!("{dst} |= (1 << {src})"),
                0x0D => format!("{dst} &= ~(1 << {src})"),
                0x0E => format!("{dst} <<= {src}"),
                0x0F => format!("{dst} >>= {src}"),
                _ => format!("SET(opt={}, {dst}, {src})", insn.set_opt),
            }
        }
        _ => format!("UNKNOWN(grp={}, sub={})", insn.group, insn.sub_group),
    }
}

// ── Dispatch entry detection ───────────────────────────────────────────

/// Finds dispatch entry points for GPR dispatch resolution.
///
/// Scans all MOBJs for immediate `PlayPl` instructions whose operand
/// matches a menu playlist number. The dispatch entry point is `pc + 1`
/// — where the MOBJ resumes after the menu playlist completes and a
/// button has been activated (`PlayPl` suspend/resume lifecycle).
///
/// For discs where IG clips aren't referenced by any MPLS playlist
/// (e.g. Warner Bros. authoring), this returns empty and the resolver
/// falls through to [`resolve_via_vm_probing`] which finds entry points
/// per-button by probing register-based `PlayPl` locations.
///
/// Reference: libbluray `hdmv_vm.c` — `_suspend_for_play_pl` (line 440)
/// saves the MOBJ and PC when `PlayPl` executes; `_resume_from_play_pl`
/// (line 455) resumes at `playing_pc + 1`.
#[must_use]
#[allow(
    clippy::implicit_hasher,
    reason = "only called from CLI with std HashMap"
)]
pub fn find_dispatch_entries(
    mobj_file: &MovieObjectFile,
    menu_playlists: &std::collections::HashSet<u32>,
) -> Vec<DispatchEntry> {
    let mut entries = Vec::new();

    for (mobj_idx, mobj) in mobj_file.objects.iter().enumerate() {
        for (pc, insn) in mobj.instructions.iter().enumerate() {
            if insn.group == GRP_BRANCH
                && insn.sub_group == BRANCH_PLAY
                && insn.imm_op1
                && menu_playlists.contains(&insn.dst)
            {
                let dispatch_pc = pc + 1;
                if dispatch_pc < mobj.instructions.len() {
                    entries.push(DispatchEntry {
                        mobj_index: mobj_idx,
                        dispatch_pc,
                    });
                }
            }
        }
    }

    entries
}

// ── Dispatch table extraction ──────────────────────────────────────────

/// Minimum number of switch cases required to identify a dispatch table.
const MIN_DISPATCH_CASES: usize = 3;

/// Maximum instructions to scan forward from a handler PC when looking
/// for the `SET GPR[R] = imm; PlayPl(GPR[R])` pair.
const HANDLER_SCAN_LIMIT: usize = 50;

/// Extracts a dispatch table from the MOBJ file.
///
/// Identifies the dispatch MOBJ (the one with the most
/// `SET GPR[R] = imm; PlayPl(GPR[R])` handler pairs) and extracts the
/// CMP/GOTO switch table mapping case values to playlist numbers.
///
/// Returns `None` if no dispatch table pattern is found (e.g. on discs
/// using the `GotoMobj` pattern instead).
#[must_use]
pub fn extract_dispatch_table(mobj_file: &MovieObjectFile) -> Option<DispatchTable> {
    let mut best: Option<(usize, usize)> = None;

    for (idx, mobj) in mobj_file.objects.iter().enumerate() {
        let count = count_handler_play_pls(&mobj.instructions);
        if count >= MIN_DISPATCH_CASES && best.is_none_or(|(_, best_count)| count > best_count) {
            best = Some((idx, count));
        }
    }

    let (mobj_index, _) = best?;
    extract_switch_table(&mobj_file.objects[mobj_index].instructions, mobj_index)
}

/// Counts `SET GPR[R] = imm; PlayPl(GPR[R])` pairs (handler endpoints).
fn count_handler_play_pls(instrs: &[Instruction]) -> usize {
    instrs
        .windows(2)
        .filter(|w| {
            let set = &w[0];
            let play = &w[1];
            set.group == GRP_SET
                && set.sub_group == 0
                && set.set_opt == 0x01
                && set.imm_op2
                && play.group == GRP_BRANCH
                && play.sub_group == BRANCH_PLAY
                && !play.imm_op1
                && play.dst == set.dst
        })
        .count()
}

/// Extracts the CMP/GOTO switch table from a dispatch MOBJ.
///
/// Scans for the case pattern (SET, SET, CMP + GOTO chain) in two
/// variants:
///
/// **7-instruction pattern** (a WB Blu-ray title, a WB Blu-ray title):
/// ```text
/// SET GPR[R1] = GPR[R2]     // load dispatch register
/// SET GPR[R3] = N            // case value (immediate)
/// CMP GPR[R1] == GPR[R3]    // equality check
/// GOTO → (pc+5)             // CMP true → jump to handler GOTO
/// GOTO → next_case          // CMP false → next case
/// GOTO → handler_pc         // actual handler jump
/// GOTO → next_case          // fallthrough
/// ```
///
/// **4-instruction pattern** (synthetic/other authoring tools):
/// ```text
/// SET GPR[R1] = GPR[R2]     // load dispatch register
/// SET GPR[R3] = N            // case value (immediate)
/// CMP GPR[R1] == GPR[R3]    // equality check
/// GOTO handler_pc            // branch (skipped if CMP false)
/// ```
///
/// Then resolves each handler to its playlist number.
fn extract_switch_table(instrs: &[Instruction], mobj_index: usize) -> Option<DispatchTable> {
    let mut case_targets: Vec<(u32, u32)> = Vec::new(); // (case_value, handler_pc)
    let mut dispatch_register: Option<u32> = None;

    let mut i = 0;
    while i + 3 < instrs.len() {
        if let Some((case_val, handler_pc, reg, case_len)) = match_case_pattern(instrs, i) {
            match dispatch_register {
                None => dispatch_register = Some(reg),
                Some(dr) if dr != reg => {
                    i += 1;
                    continue;
                }
                _ => {}
            }
            case_targets.push((case_val, handler_pc));
            i += case_len;
        } else {
            i += 1;
        }
    }

    if case_targets.len() < MIN_DISPATCH_CASES {
        return None;
    }

    let dispatch_reg = dispatch_register?;

    let cases: Vec<(u32, u16)> = case_targets
        .iter()
        .filter_map(|&(case_val, handler_pc)| {
            find_handler_playlist(instrs, handler_pc as usize).map(|playlist| (case_val, playlist))
        })
        .collect();

    if cases.is_empty() {
        return None;
    }

    Some(DispatchTable {
        mobj_index,
        dispatch_register: dispatch_reg,
        cases,
    })
}

/// Matches a CMP/GOTO case pattern starting at `pc`.
///
/// Returns `(case_value, handler_pc, dispatch_register, case_length)`
/// on match. Tries the 7-instruction pattern first, then falls back to
/// the 4-instruction pattern.
#[allow(
    clippy::cast_possible_truncation,
    reason = "instruction indices fit in u32"
)]
fn match_case_pattern(instrs: &[Instruction], pc: usize) -> Option<(u32, u32, u32, usize)> {
    let load = &instrs[pc];
    let set_case = &instrs[pc + 1];
    let cmp = &instrs[pc + 2];

    // SET GPR[R1] = GPR[R2] (register-to-register MOVE)
    if load.group != GRP_SET || load.sub_group != 0 || load.set_opt != 0x01 || load.imm_op2 {
        return None;
    }
    let r1 = load.dst;
    let dispatch_reg = load.src;

    // SET GPR[R3] = N (immediate MOVE)
    if set_case.group != GRP_SET
        || set_case.sub_group != 0
        || set_case.set_opt != 0x01
        || !set_case.imm_op2
    {
        return None;
    }
    let r3 = set_case.dst;
    let case_value = set_case.src;

    // CMP GPR[R1] == GPR[R3]
    if cmp.group != GRP_CMP
        || cmp.cmp_opt != 0x02
        || cmp.imm_op1
        || cmp.imm_op2
        || cmp.dst != r1
        || cmp.src != r3
    {
        return None;
    }

    // 7-instruction pattern: CMP → GOTO(pc+5) → GOTO(next) → GOTO(handler) → GOTO(next)
    if pc + 6 < instrs.len() {
        let goto_true = &instrs[pc + 3];
        let goto_handler = &instrs[pc + 5];

        if is_unconditional_goto(goto_true)
            && goto_true.dst == (pc + 5) as u32
            && is_unconditional_goto(goto_handler)
        {
            return Some((case_value, goto_handler.dst, dispatch_reg, 7));
        }
    }

    // 4-instruction pattern: GOTO(handler) directly after CMP
    let goto = &instrs[pc + 3];
    if is_unconditional_goto(goto) {
        return Some((case_value, goto.dst, dispatch_reg, 4));
    }

    None
}

/// Returns `true` if the instruction is an unconditional immediate GOTO.
const fn is_unconditional_goto(insn: &Instruction) -> bool {
    insn.group == GRP_BRANCH
        && insn.sub_group == BRANCH_GOTO
        && insn.branch_opt == 0x01
        && insn.imm_op1
}

/// Finds the playlist number in a handler block starting at `handler_pc`.
///
/// Scans forward for a `SET GPR[R] = imm; PlayPl(GPR[R])` pair within
/// [`HANDLER_SCAN_LIMIT`] instructions.
fn find_handler_playlist(instrs: &[Instruction], handler_pc: usize) -> Option<u16> {
    let limit = (handler_pc + HANDLER_SCAN_LIMIT).min(instrs.len().saturating_sub(1));
    let mut pc = handler_pc;

    while pc < limit {
        let set_insn = &instrs[pc];
        let play_insn = &instrs[pc + 1];

        if set_insn.group == GRP_SET
            && set_insn.sub_group == 0
            && set_insn.set_opt == 0x01
            && set_insn.imm_op2
            && play_insn.group == GRP_BRANCH
            && play_insn.sub_group == BRANCH_PLAY
            && !play_insn.imm_op1
            && play_insn.dst == set_insn.dst
        {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "playlist numbers are u16 values"
            )]
            return Some((set_insn.src & 0xFFFF) as u16);
        }

        pc += 1;
    }

    None
}

/// Finds the handler PC for a specific dispatch case value.
///
/// Re-scans the switch table to locate the GOTO target for the given
/// `case_value`. Returns `None` if the case isn't found or the dispatch
/// register doesn't match.
#[must_use]
pub fn find_handler_pc(
    instrs: &[Instruction],
    case_value: u32,
    dispatch_register: u32,
) -> Option<usize> {
    let mut i = 0;
    while i + 3 < instrs.len() {
        if let Some((cv, handler_pc, reg, case_len)) = match_case_pattern(instrs, i) {
            if reg == dispatch_register && cv == case_value {
                return Some(handler_pc as usize);
            }
            i += case_len;
        } else {
            i += 1;
        }
    }
    None
}

// ── Execution-based resolver ───────────────────────────────────────────

/// The terminal effect of executing a button's command program.
///
/// Button commands are HDMV bytecode that modify registers and eventually
/// reach a terminal instruction. This enum captures what the program does
/// without prescribing how to interpret it.
#[derive(Debug)]
enum ButtonEffect {
    /// Play a playlist directly (`PlayPl`).
    Playlist {
        /// Playlist number.
        playlist: u16,
        /// `PlayPl` variant: 0=from start, 1=at mark, 2=at play item.
        branch_opt: u8,
        /// Mark index or play item index (meaningful when `branch_opt > 0`).
        mark_or_pi: u32,
    },
    /// Jump to a movie object (`GotoMobj`).
    GotoMobj(u32),
    /// Navigate to a button/page via `SET_BUTTON_PAGE` (SETSYSTEM `set_opt=3`).
    SetButtonPage {
        /// Composite dispatch value (typically `button_id + key`).
        composite: u32,
        /// Target page from the `src` operand. Used by the navigation
        /// graph to follow internal page edges and propagate GPR state.
        page: u32,
    },
    /// No terminal action reached within the step limit.
    None,
}

/// A clip's page structure for execution-based resolution.
pub struct NavClipInput<'a> {
    /// IG stream PID (for PSR\[0\] seeding).
    pub ig_pid: u16,
    /// All pages across display sets in this clip.
    pub pages: Vec<&'a Page>,
}

/// Executes MOBJ\[0\] (First Play) and returns the resulting GPR state.
///
/// WB authoring stores per-content-item configuration in a GPR database
/// (registers 3000–3999) initialized by the First Play object. Title
/// MOBJs and button programs read from this database to select playlists.
/// Without seeding, conditional `PlayPl` instructions take default
/// branches and fail to resolve.
///
/// PSR entries are seeded with BD spec first-boot defaults so that
/// MOBJ\[0\]'s initialization guard enters the configuration block.
/// The returned map contains only GPR entries (PSRs are stripped).
#[must_use]
pub fn seed_gpr_state(mobj_file: &MovieObjectFile) -> std::collections::HashMap<u32, u32> {
    let Some(mobj0) = mobj_file.objects.first() else {
        return std::collections::HashMap::new();
    };

    let mut gprs = std::collections::HashMap::new();
    // Seed PSRs with first-boot defaults so MOBJ[0]'s init guard enters
    // the initialization block. PSR[4] (title number) must be 0xFFFF
    // ("unconfigured") — the guard `CMP PSR[4] != 0xFFFF` skips init
    // when the title is already set. Other PSRs use BD spec defaults.
    gprs.insert(PSR_FLAG | 0x01, 0xFF); // primary audio
    gprs.insert(PSR_FLAG | 0x02, 0xFFFE); // PG/TextST
    gprs.insert(PSR_FLAG | 0x03, 0xFF); // angle
    gprs.insert(PSR_FLAG | 0x04, 0xFFFF); // title (init guard)
    gprs.insert(PSR_FLAG | 0x0A, 0xFFFF); // selected button
    gprs.insert(PSR_FLAG | 0x0C, 0xFF); // user style
    gprs.insert(PSR_FLAG | 0x0D, 0xFF); // parental level
    gprs.insert(PSR_FLAG | 0x0E, 0xFFFF); // secondary A/V
    gprs.insert(PSR_FLAG | 0x0F, 0x0002_0000); // audio cap
    gprs.insert(PSR_FLAG | 0x1D, 0x0200); // profile 2.0
    gprs.insert(PSR_FLAG | 0x1F, 0x0200); // player version
    let _ = run_mobj_vm(
        &mobj0.instructions,
        0,
        &mut gprs,
        &std::collections::HashSet::new(),
    );
    // Keep only GPR entries (no PSR) — the disc configuration database
    // that title MOBJs and button programs read.
    gprs.into_iter()
        .filter(|(k, _)| k & PSR_FLAG == 0)
        .collect()
}

/// Resolves button→playlist mappings by executing button programs.
///
/// Builds a navigation graph via BFS over all (clip, page) nodes.
/// At each node, every button's command program is executed through the
/// mini-VM with the current GPR state:
///
/// - `PlayPl` → terminal edge (direct playlist resolution)
/// - `GotoMobj` → follows the target MOBJ to reach `PlayPl`
/// - `SET_BUTTON_PAGE` → dispatch table lookup on the composite value,
///   plus a navigation edge to the target page when the page differs
///   from the current one. GPR state is propagated along navigation
///   edges so that content buttons on downstream pages execute with
///   the register values set by upstream navigation buttons.
///
/// This subsumes the five pattern-matching strategies from tickets 06–10
/// into a single execution + graph-traversal pass.
///
/// Duplicate (button\_id, playlist) pairs are deduplicated internally.
#[must_use]
#[allow(
    clippy::implicit_hasher,
    reason = "only called from CLI with std HashMap"
)]
#[allow(
    clippy::too_many_lines,
    reason = "BFS loop with three effect branches is inherently long"
)]
pub fn resolve_via_execution(
    clips: &[NavClipInput<'_>],
    mobj_file: &MovieObjectFile,
    dispatch_table: Option<&DispatchTable>,
    valid_playlists: &std::collections::HashSet<u32>,
) -> Vec<ResolvedButton> {
    use std::collections::VecDeque;
    use std::hash::{Hash, Hasher};

    type GprState = std::collections::HashMap<u32, u32>;

    /// Maximum BFS iterations to prevent runaway execution on cyclic
    /// or pathological graphs.
    const NAV_GRAPH_LIMIT: u32 = 50_000;

    /// Computes a deterministic hash of a GPR state map for visited-set
    /// lookups. Uses sorted key-value pairs so iteration order doesn't
    /// affect the result.
    #[allow(
        clippy::collection_is_never_read,
        reason = "pairs is read via Hash::hash"
    )]
    fn hash_gpr_state(state: &GprState) -> u64 {
        // Collect and sort for determinism (HashMap iteration is random)
        let mut pairs: Vec<(u32, u32)> = state
            .iter()
            .filter(|(k, _)| *k & PSR_FLAG == 0)
            .map(|(&k, &v)| (k, v))
            .collect();
        pairs.sort_unstable();
        let mut hasher = std::hash::DefaultHasher::new();
        pairs.hash(&mut hasher);
        hasher.finish()
    }

    let mut resolved = Vec::new();
    let mut resolved_set = std::collections::HashSet::<(u16, u16)>::new();

    // Page index: (clip_index, page_id) → (page ref, ig_pid)
    let mut page_lookup = std::collections::HashMap::<(usize, u8), (&Page, u16)>::new();
    for (clip_idx, clip) in clips.iter().enumerate() {
        for page in &clip.pages {
            page_lookup.insert((clip_idx, page.page_id), (page, clip.ig_pid));
        }
    }

    // BFS queue: (clip_index, page_id, gprs)
    let mut queue: VecDeque<(usize, u8, GprState)> = VecDeque::new();

    // Visited: (clip_index, page_id) → set of GPR state hashes processed.
    let mut visited =
        std::collections::HashMap::<(usize, u8), std::collections::HashSet<u64>>::new();

    // Execute MOBJ[0] (First Play) to collect GPR[3xxx] configuration
    // state. WB authoring stores per-content-item configuration in a
    // GPR database (registers 3000–3999) initialized by MOBJ[0]. The
    // complex button programs read from this database to compute
    // dispatch values. Without it, buttons follow default branches.
    let init_gprs: GprState = seed_gpr_state(mobj_file);

    // Seed BFS with all pages using MOBJ[0]'s GPR state.
    let init_hash = hash_gpr_state(&init_gprs);
    for (clip_idx, clip) in clips.iter().enumerate() {
        for page in &clip.pages {
            visited
                .entry((clip_idx, page.page_id))
                .or_default()
                .insert(init_hash);
            queue.push_back((clip_idx, page.page_id, init_gprs.clone()));
        }
    }

    let mut iterations: u32 = 0;

    while let Some((clip_idx, page_id, gprs)) = queue.pop_front() {
        iterations += 1;
        if iterations > NAV_GRAPH_LIMIT {
            break;
        }

        let Some(&(page, ig_pid)) = page_lookup.get(&(clip_idx, page_id)) else {
            continue;
        };

        for button in &page.buttons {
            // Skip buttons with a direct immediate PlayPl
            let has_direct_play_pl = button
                .commands
                .iter()
                .any(|c| matches!(c, NavigationCommand::PlayPl { .. }));
            if has_direct_play_pl {
                continue;
            }

            // NOP anchor resolution: buttons whose only commands are
            // NOPs are navigation anchors whose button_id IS the
            // dispatch case.
            //
            // In the WB SET_BUTTON_PAGE pattern, the player runtime
            // sets PSR[10] to the selected button_id. When a NOP
            // anchor is selected (via SET_BUTTON_PAGE navigation or
            // direct user cursor navigation) and activated, PSR[10]
            // becomes the dispatch case. The dispatch MOBJ copies
            // PSR[10] into its switch register and dispatches.
            //
            // WB authoring emits a single NOP instruction (all-zero
            // 12-byte command) on anchor buttons rather than a true
            // zero-command button.
            //
            // Reference: libbluray `graphics_controller.c`
            // `_select_button()` → writes PSR[10] = button_id.
            let is_nop_anchor = button.commands.is_empty()
                || button.commands.iter().all(|c| {
                    matches!(c, NavigationCommand::Other { opcode, dst, src }
                        if *opcode == 0 && *dst == 0 && *src == 0)
                });
            if is_nop_anchor {
                if let Some(table) = dispatch_table {
                    let bid = u32::from(button.button_id);
                    if let Some(&(_, pl)) = table.cases.iter().find(|(cv, _)| *cv == bid)
                        && resolved_set.insert((button.button_id, pl))
                    {
                        resolved.push(ResolvedButton {
                            button_id: button.button_id,
                            target: PlayTarget {
                                playlist: pl,
                                branch_opt: 0,
                                mark_or_pi: 0,
                            },
                        });
                    }
                }
                continue;
            }

            let ctx = PlayerContext {
                ig_stream: ig_pid,
                selected_button_id: button.button_id,
                page_id,
            };

            let (effect, new_gprs) = execute_button_commands(&button.commands, &ctx, &gprs);

            match effect {
                ButtonEffect::Playlist {
                    playlist: pl,
                    branch_opt: bo,
                    mark_or_pi: mpi,
                } => {
                    let is_valid = pl != 0
                        && pl != 0xFFFF
                        && (valid_playlists.is_empty() || valid_playlists.contains(&u32::from(pl)));
                    if is_valid && resolved_set.insert((button.button_id, pl)) {
                        resolved.push(ResolvedButton {
                            button_id: button.button_id,
                            target: PlayTarget {
                                playlist: pl,
                                branch_opt: bo,
                                mark_or_pi: mpi,
                            },
                        });
                    }
                }
                ButtonEffect::GotoMobj(object_id) => {
                    if let Some(mobj) = mobj_file.objects.get(object_id as usize) {
                        let mut mobj_gprs = new_gprs;
                        if let Some(target) =
                            run_mobj_vm(&mobj.instructions, 0, &mut mobj_gprs, valid_playlists)
                            && resolved_set.insert((button.button_id, target.playlist))
                        {
                            resolved.push(ResolvedButton {
                                button_id: button.button_id,
                                target,
                            });
                        }
                    }
                }
                ButtonEffect::SetButtonPage {
                    composite,
                    page: target_page,
                } => {
                    // Terminal resolution via dispatch table lookup.
                    if let Some(table) = dispatch_table
                        && let Some(&(_, pl)) = table.cases.iter().find(|(cv, _)| *cv == composite)
                        && resolved_set.insert((button.button_id, pl))
                    {
                        resolved.push(ResolvedButton {
                            button_id: button.button_id,
                            target: PlayTarget {
                                playlist: pl,
                                branch_opt: 0,
                                mark_or_pi: 0,
                            },
                        });
                    }

                    // Execute the dispatch MOBJ handler for this case to
                    // collect the GPR[3xxx] configuration state it sets.
                    // WB authoring: each handler populates registers like
                    // GPR[3563] (content index), GPR[3776] (expected
                    // button_id) etc. before calling PlayPl to re-enter
                    // the menu. Data-driven button programs on the menu
                    // pages read these to compute their dispatch values.
                    //
                    // Start from the handler PC (not instruction 0) to
                    // skip the MOBJ initialization code which clears
                    // the dispatch register.
                    if let Some(table) = dispatch_table
                        && let Some(handler_pc) = find_handler_pc(
                            &mobj_file.objects[table.mobj_index].instructions,
                            composite,
                            table.dispatch_register,
                        )
                    {
                        let mut handler_gprs = GprState::new();

                        // Run handler until PlayPl (accept any playlist).
                        let _ = run_mobj_vm(
                            &mobj_file.objects[table.mobj_index].instructions,
                            handler_pc,
                            &mut handler_gprs,
                            &std::collections::HashSet::new(),
                        );

                        // Propagate handler state to all pages in this clip.
                        let handler_state: GprState = handler_gprs
                            .into_iter()
                            .filter(|(k, _)| k & PSR_FLAG == 0)
                            .collect();

                        let key = hash_gpr_state(&handler_state);
                        for &(ci, pid) in page_lookup.keys() {
                            if ci == clip_idx {
                                let states = visited.entry((ci, pid)).or_default();
                                if states.insert(key) {
                                    queue.push_back((ci, pid, handler_state.clone()));
                                }
                            }
                        }
                    }

                    // Navigation edge: propagate button GPR state to
                    // the target page. Includes same-page edges (WB
                    // authoring: navigation buttons set GPR state and
                    // loop back to the current page).
                    #[allow(clippy::cast_possible_truncation, reason = "page IDs fit in u8")]
                    let target_page_id = (target_page & 0xFF) as u8;
                    if page_lookup.contains_key(&(clip_idx, target_page_id)) {
                        let propagated: GprState = new_gprs
                            .into_iter()
                            .filter(|(k, _)| k & PSR_FLAG == 0)
                            .collect();

                        let key = hash_gpr_state(&propagated);
                        let states = visited.entry((clip_idx, target_page_id)).or_default();
                        if states.insert(key) {
                            queue.push_back((clip_idx, target_page_id, propagated));
                        }
                    }
                }
                ButtonEffect::None => {}
            }
        }
    }

    resolved
}

/// Executes a button's navigation commands and returns the terminal effect
/// along with the register state at the point of termination.
///
/// Converts all [`NavigationCommand`]s to [`Instruction`]s and runs them
/// through the mini-VM. Recognizes three terminal actions: `PlayPl`,
/// `GotoMobj` (BRANCH\_JUMP), and `SET_BUTTON_PAGE` (SETSYSTEM set\_opt=3).
fn execute_button_commands(
    commands: &[NavigationCommand],
    ctx: &PlayerContext,
    initial_gprs: &std::collections::HashMap<u32, u32>,
) -> (ButtonEffect, std::collections::HashMap<u32, u32>) {
    let instrs: Vec<Instruction> = commands.iter().map(command_to_instruction).collect();

    let mut gprs = initial_gprs.clone();
    gprs.insert(PSR_FLAG, u32::from(ctx.ig_stream));
    gprs.insert(PSR_FLAG | 0x0A, u32::from(ctx.selected_button_id));
    gprs.insert(PSR_FLAG | 0x0B, u32::from(ctx.page_id));

    let mut pc: usize = 0;
    let mut steps: u32 = 0;

    while pc < instrs.len() && steps < VM_STEP_LIMIT {
        steps += 1;
        let insn = &instrs[pc];

        match insn.group {
            GRP_SET => {
                // Intercept SET_BUTTON_PAGE before the generic SET handler
                if insn.sub_group == 1 && insn.set_opt == SET_BUTTON_PAGE_OPT {
                    let composite = fetch_operand(insn.imm_op1, insn.dst, &gprs);
                    let page = fetch_operand(insn.imm_op2, insn.src, &gprs);
                    return (ButtonEffect::SetButtonPage { composite, page }, gprs);
                }
                execute_set(insn, &mut gprs);
                pc += 1;
            }
            GRP_CMP => {
                if execute_cmp(insn, &gprs) {
                    pc += 1;
                } else {
                    pc += 2;
                }
            }
            GRP_BRANCH => match insn.sub_group {
                BRANCH_PLAY => {
                    let playlist = fetch_operand(insn.imm_op1, insn.dst, &gprs);
                    let mark_or_pi = fetch_operand(insn.imm_op2, insn.src, &gprs);
                    #[allow(
                        clippy::cast_possible_truncation,
                        reason = "playlist numbers are u16 values"
                    )]
                    return (
                        ButtonEffect::Playlist {
                            playlist: (playlist & 0xFFFF) as u16,
                            branch_opt: insn.branch_opt,
                            mark_or_pi,
                        },
                        gprs,
                    );
                }
                BRANCH_JUMP => {
                    let object_id = fetch_operand(insn.imm_op1, insn.dst, &gprs);
                    return (ButtonEffect::GotoMobj(object_id), gprs);
                }
                BRANCH_GOTO => {
                    if !execute_goto(insn, &gprs, &mut pc) {
                        pc += 1;
                    }
                }
                _ => {
                    pc += 1;
                }
            },
            _ => {
                pc += 1;
            }
        }
    }

    (ButtonEffect::None, gprs)
}

// ── Button resolver (legacy pattern-matching) ──────────────────────────

/// Resolves button → playlist mappings by tracing through movie objects.
///
/// For each button that has a [`SetGpr`](NavigationCommand::SetGpr) +
/// [`GotoMobj`](NavigationCommand::GotoMobj) command pair (but no direct
/// [`PlayPl`](NavigationCommand::PlayPl)), traces the target movie object
/// to find the conditional `PlayPl` that matches the GPR value.
///
/// `valid_playlists` constrains the VM-based resolver: when tracing
/// GPR dispatch patterns, only playlist numbers in this set are accepted.
/// Pass an empty set to accept any non-zero playlist number.
///
/// `dispatch_entries` provides entry points for GPR dispatch resolution
/// (from [`find_dispatch_entries`]). When non-empty, GPR dispatch buttons
/// are resolved by executing the MOBJ from the dispatch entry point
/// rather than from instruction 0. Pass an empty slice to use the
/// fallback (execute from instruction 0).
///
/// `dispatch_table` provides a statically extracted dispatch table (from
/// [`extract_dispatch_table`]). When present, buttons with `SetGpr` values
/// matching a case in the table are resolved directly without VM execution.
///
/// Returns only the buttons that were successfully resolved. Buttons with
/// direct `PlayPl`, missing commands, or unresolvable control flow are
/// omitted.
#[must_use]
#[allow(
    clippy::implicit_hasher,
    reason = "only called from CLI with std HashMap"
)]
pub fn resolve_buttons(
    buttons: &[(Button, PlayerContext)],
    mobj_file: &MovieObjectFile,
    valid_playlists: &std::collections::HashSet<u32>,
    dispatch_entries: &[DispatchEntry],
    dispatch_table: Option<&DispatchTable>,
) -> Vec<ResolvedButton> {
    let mut resolved = Vec::new();

    for (button, ctx) in buttons {
        // Skip buttons that already have a direct PlayPl
        let has_play_pl = button
            .commands
            .iter()
            .any(|c| matches!(c, NavigationCommand::PlayPl { .. }));
        if has_play_pl {
            continue;
        }

        if let Some(playlist) = trace_button(
            button,
            mobj_file,
            valid_playlists,
            ctx,
            dispatch_entries,
            dispatch_table,
        ) {
            resolved.push(ResolvedButton {
                button_id: button.button_id,
                target: PlayTarget {
                    playlist,
                    branch_opt: 0,
                    mark_or_pi: 0,
                },
            });
        }
    }

    resolved
}

/// Traces a single button's commands through the movie object file.
///
/// Five resolution strategies, tried in order:
/// 1. **`GotoMobj` pattern:** button has `SetGpr` + `GotoMobj` → look up
///    the target MOBJ and scan for matching `PlayPl`.
/// 2. **GPR dispatch via entry points:** button has `SetGpr` but no
///    `GotoMobj`, and dispatch entries are available → execute the MOBJ
///    from the dispatch entry point (`PlayPl` suspend/resume lifecycle).
/// 3. **Dispatch table lookup:** a `SetGpr` value matches a case in the
///    statically extracted dispatch table → return the corresponding
///    playlist directly (no VM execution).
/// 4. **Lifecycle simulation:** run the mini-VM from instruction 0 with
///    `PlayPl` suspend/resume, compare against baseline.
/// 5. **Direct execution fallback:** run the mini-VM on candidate MOBJs
///    from instruction 0.
fn trace_button(
    button: &Button,
    mobj_file: &MovieObjectFile,
    valid_playlists: &std::collections::HashSet<u32>,
    ctx: &PlayerContext,
    dispatch_entries: &[DispatchEntry],
    dispatch_table: Option<&DispatchTable>,
) -> Option<u16> {
    // Collect all SetGpr assignments and optional GotoMobj
    let mut gpr_assignments: Vec<(u32, u32)> = Vec::new();
    let mut goto_mobj: Option<u32> = None;

    for cmd in &button.commands {
        match cmd {
            NavigationCommand::SetGpr { register, value } => {
                gpr_assignments.push((*register, *value));
            }
            NavigationCommand::GotoMobj { object_id } => {
                goto_mobj = Some(*object_id);
            }
            _ => {}
        }
    }

    if gpr_assignments.is_empty() {
        return None;
    }

    // Pattern 1: GotoMobj — button jumps to a specific MOBJ
    if let Some(object_id) = goto_mobj {
        let mobj = mobj_file.objects.get(object_id as usize)?;
        // Use the last SetGpr as the primary assignment
        let &(register, value) = gpr_assignments.last()?;
        return resolve_in_mobj_static(mobj, register, value);
    }

    // Pattern 2: GPR dispatch via known entry points (PlayPl suspend/resume)
    if let Some(result) = resolve_via_dispatch(
        mobj_file,
        &gpr_assignments,
        valid_playlists,
        ctx,
        dispatch_entries,
    ) {
        return Some(result);
    }

    // Pattern 3: Dispatch table lookup — match button_id + key (composite
    // dispatch case) against the statically extracted case→playlist table.
    // The button bytecode computes (PSR[10] & 0xFFFF) + GPR[4075] and
    // passes it to SET_BUTTON_PAGE; PSR[10] at activation = button_id.
    if let Some(table) = dispatch_table {
        for &(_, value) in &gpr_assignments {
            let composite = u32::from(button.button_id) + value;
            if let Some(&(_, playlist)) = table.cases.iter().find(|(cv, _)| *cv == composite) {
                return Some(playlist);
            }
        }
    }

    // Pattern 4: GPR dispatch via lifecycle simulation — execute from
    // instruction 0 with PlayPl suspend/resume, comparing against a
    // baseline to find the dispatch result.
    if let Some(result) = resolve_via_lifecycle(mobj_file, &gpr_assignments, valid_playlists, ctx) {
        return Some(result);
    }

    // Pattern 5: direct execution from instruction 0 — works for simple
    // MOBJs without initialization that clobbers the dispatch register.
    for mobj in &mobj_file.objects {
        let instrs = &mobj.instructions;
        let has_reg_play_pl = instrs
            .iter()
            .any(|i| i.group == GRP_BRANCH && i.sub_group == BRANCH_PLAY && !i.imm_op1);
        if !has_reg_play_pl {
            continue;
        }
        if let Some(target) = execute_from(instrs, 0, &gpr_assignments, valid_playlists, ctx) {
            return Some(target.playlist);
        }
    }

    None
}

/// Static resolution: scans a MOBJ for an immediate `PlayPl` matching
/// a GPR value (`GotoMobj` pattern — a Blu-ray series style).
fn resolve_in_mobj_static(mobj: &MovieObject, register: u32, value: u32) -> Option<u16> {
    let instrs = &mobj.instructions;

    // Pair CMP instructions with PlayPl instructions by position or
    // branch target.
    let pairs = collect_cmp_play_pairs(instrs, register);

    for (cmp_value, playlist) in &pairs {
        if *cmp_value == value {
            return Some(*playlist);
        }
    }

    // Fallback: single unconditional PlayPl with an immediate operand.
    if pairs.is_empty() {
        let imm_play_pls: Vec<u16> = instrs
            .iter()
            .filter(|i| i.group == GRP_BRANCH && i.sub_group == BRANCH_PLAY && i.imm_op1)
            .map(|i| {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "playlist numbers are u16 values"
                )]
                let pl = (i.dst & 0xFFFF) as u16;
                pl
            })
            .collect();

        if imm_play_pls.len() == 1 {
            return Some(imm_play_pls[0]);
        }
    }

    None
}

// ── GPR dispatch via entry points ──────────────────────────────────────

/// Resolves a button's playlist using dispatch entry points.
///
/// Executes the MOBJ from each dispatch entry point (the instruction
/// after the menu `PlayPl`) with the button's register state. Returns
/// the first valid playlist number reached.
fn resolve_via_dispatch(
    mobj_file: &MovieObjectFile,
    gpr_assignments: &[(u32, u32)],
    valid_playlists: &std::collections::HashSet<u32>,
    ctx: &PlayerContext,
    dispatch_entries: &[DispatchEntry],
) -> Option<u16> {
    for entry in dispatch_entries {
        let Some(mobj) = mobj_file.objects.get(entry.mobj_index) else {
            continue;
        };

        if let Some(target) = execute_from(
            &mobj.instructions,
            entry.dispatch_pc,
            gpr_assignments,
            valid_playlists,
            ctx,
        ) {
            return Some(target.playlist);
        }
    }

    None
}

// ── Mini-VM for GPR dispatch resolution ─────────────────────────────────

/// Resolves a button's playlist by simulating the `PlayPl` suspend/resume
/// lifecycle and comparing against a baseline.
///
/// Runs each candidate MOBJ twice: once with the button's GPR values,
/// once without (baseline). Both simulate the suspend/resume lifecycle
/// — on every `PlayPl`, GPR values are re-applied and execution
/// continues. The first valid `PlayPl` where the button run diverges
/// from the baseline is the dispatch result.
fn resolve_via_lifecycle(
    mobj_file: &MovieObjectFile,
    gpr_assignments: &[(u32, u32)],
    valid_playlists: &std::collections::HashSet<u32>,
    ctx: &PlayerContext,
) -> Option<u16> {
    for mobj in &mobj_file.objects {
        let instrs = &mobj.instructions;

        // Skip MOBJs without register-based PlayPl
        let has_reg_play_pl = instrs
            .iter()
            .any(|i| i.group == GRP_BRANCH && i.sub_group == BRANCH_PLAY && !i.imm_op1);
        if !has_reg_play_pl {
            continue;
        }

        // Trace with button's GPR values
        let with_button = trace_play_pls(instrs, gpr_assignments, valid_playlists, ctx);
        // Trace with no button GPR values (baseline)
        let baseline = trace_play_pls(instrs, &[], valid_playlists, ctx);

        // Find first divergence — that's the dispatch result
        for (btn_pl, base_pl) in with_button.iter().zip(baseline.iter()) {
            if btn_pl != base_pl {
                return Some(*btn_pl);
            }
        }

        // No divergence — button trace has more entries than baseline
        if with_button.len() > baseline.len() {
            return with_button.get(baseline.len()).copied();
        }
    }

    None
}

/// Traces all valid `PlayPl` playlists encountered during lifecycle
/// simulation of an MOBJ.
///
/// On every `PlayPl`, the provided GPR assignments are re-applied
/// (simulating button activation during a suspended playlist) and
/// execution continues. Returns the ordered list of valid playlist
/// numbers encountered.
fn trace_play_pls(
    instrs: &[Instruction],
    gpr_assignments: &[(u32, u32)],
    valid_playlists: &std::collections::HashSet<u32>,
    ctx: &PlayerContext,
) -> Vec<u16> {
    let mut gprs = std::collections::HashMap::<u32, u32>::new();
    for &(reg, val) in gpr_assignments {
        gprs.insert(reg, val);
    }
    gprs.insert(PSR_FLAG, u32::from(ctx.ig_stream)); // PSR[0]
    gprs.insert(PSR_FLAG | 0x0A, u32::from(ctx.selected_button_id)); // PSR[10]
    gprs.insert(PSR_FLAG | 0x0B, u32::from(ctx.page_id)); // PSR[11]

    let mut pc: usize = 0;
    let mut steps: u32 = 0;
    let mut playlists = Vec::new();

    while pc < instrs.len() && steps < VM_STEP_LIMIT {
        steps += 1;
        let insn = &instrs[pc];

        match insn.group {
            GRP_SET => {
                execute_set(insn, &mut gprs);
                pc += 1;
            }
            GRP_CMP => {
                // libbluray CMP model: if condition is false, skip next instruction
                if execute_cmp(insn, &gprs) {
                    pc += 1;
                } else {
                    pc += 2;
                }
            }
            GRP_BRANCH => match insn.sub_group {
                BRANCH_PLAY => {
                    let playlist = fetch_operand(insn.imm_op1, insn.dst, &gprs);
                    let is_valid = playlist != 0
                        && playlist != 0xFFFF
                        && (valid_playlists.is_empty() || valid_playlists.contains(&playlist));
                    if is_valid {
                        #[allow(
                            clippy::cast_possible_truncation,
                            reason = "playlist numbers are u16 values"
                        )]
                        playlists.push((playlist & 0xFFFF) as u16);
                    }
                    for &(reg, val) in gpr_assignments {
                        gprs.insert(reg, val);
                    }
                    pc += 1;
                }
                BRANCH_GOTO => {
                    if !execute_goto(insn, &gprs, &mut pc) {
                        pc += 1;
                    }
                }
                _ => {
                    pc += 1;
                }
            },
            _ => {
                pc += 1;
            }
        }
    }

    playlists
}

/// Executes MOBJ instructions starting at `start_pc` with the given
/// register state and player context.
fn execute_from(
    instrs: &[Instruction],
    start_pc: usize,
    gpr_assignments: &[(u32, u32)],
    valid_playlists: &std::collections::HashSet<u32>,
    ctx: &PlayerContext,
) -> Option<PlayTarget> {
    let mut gprs = std::collections::HashMap::<u32, u32>::new();
    for &(reg, val) in gpr_assignments {
        gprs.insert(reg, val);
    }
    gprs.insert(PSR_FLAG, u32::from(ctx.ig_stream));
    gprs.insert(PSR_FLAG | 0x0A, u32::from(ctx.selected_button_id));
    gprs.insert(PSR_FLAG | 0x0B, u32::from(ctx.page_id));

    run_mobj_vm(instrs, start_pc, &mut gprs, valid_playlists)
}

/// Core MOBJ VM loop — executes instructions until a valid `PlayPl` is
/// reached or the step limit is exceeded.
///
/// Shared by [`execute_from`] (legacy resolver) and the BFS
/// navigation graph (`GotoMobj` follow-through, handler execution).
#[allow(
    clippy::implicit_hasher,
    reason = "called from both library internals and CLI with std HashMap"
)]
pub fn run_mobj_vm(
    instrs: &[Instruction],
    start_pc: usize,
    gprs: &mut std::collections::HashMap<u32, u32>,
    valid_playlists: &std::collections::HashSet<u32>,
) -> Option<PlayTarget> {
    let mut pc: usize = start_pc;
    let mut steps: u32 = 0;

    while pc < instrs.len() && steps < VM_STEP_LIMIT {
        steps += 1;
        let insn = &instrs[pc];

        match insn.group {
            GRP_SET => {
                execute_set(insn, gprs);
                pc += 1;
            }
            GRP_CMP => {
                if execute_cmp(insn, gprs) {
                    pc += 1;
                } else {
                    pc += 2;
                }
            }
            GRP_BRANCH => match insn.sub_group {
                BRANCH_PLAY => {
                    let playlist = fetch_operand(insn.imm_op1, insn.dst, gprs);
                    let is_valid = playlist != 0
                        && playlist != 0xFFFF
                        && (valid_playlists.is_empty() || valid_playlists.contains(&playlist));
                    if is_valid {
                        let mark_or_pi = fetch_operand(insn.imm_op2, insn.src, gprs);
                        #[allow(
                            clippy::cast_possible_truncation,
                            reason = "playlist numbers are u16 values"
                        )]
                        return Some(PlayTarget {
                            playlist: (playlist & 0xFFFF) as u16,
                            branch_opt: insn.branch_opt,
                            mark_or_pi,
                        });
                    }
                    pc += 1;
                }
                BRANCH_GOTO => {
                    if !execute_goto(insn, gprs, &mut pc) {
                        pc += 1;
                    }
                }
                _ => {
                    pc += 1;
                }
            },
            _ => {
                pc += 1;
            }
        }
    }

    None
}

/// Fetches an operand value: immediate or from the register file.
///
/// For register-mode operands, the raw value encodes the register address.
/// Bit 31 ([`PSR_FLAG`]) distinguishes PSRs from GPRs. When a PSR-flagged
/// reference is not found (e.g. firmware-specific PSR indices above 127),
/// this falls back to the GPR with the same index. This handles the
/// aliasing seen in Warner Bros. authoring where SETSYSTEM operands
/// reference PSR\[N\] but the value was stored in GPR\[N\] by regular SET
/// instructions.
fn fetch_operand(is_immediate: bool, raw: u32, gprs: &std::collections::HashMap<u32, u32>) -> u32 {
    if is_immediate {
        raw
    } else if raw & PSR_FLAG != 0 {
        // PSR reference — try PSR key first, fall back to GPR alias.
        // This fallback is also how SET_BUTTON_PAGE operands resolve:
        // libbluray's `_read_setbuttonpage_reg` extracts bits 0-11 as
        // the register number, but we receive the full raw operand
        // (e.g. 0x80000FED). The PSR lookup misses, the fallback
        // strips PSR_FLAG and finds GPR[4077].
        gprs.get(&raw)
            .or_else(|| gprs.get(&(raw & !PSR_FLAG)))
            .copied()
            .unwrap_or(0)
    } else {
        gprs.get(&raw).copied().unwrap_or(0)
    }
}

/// Executes a SET group instruction: updates the destination register.
///
/// Handles both general SET (`sub_group=0`) and SETSYSTEM (`sub_group=1`).
/// SETSYSTEM reads/writes PSRs, which are stored in the same register
/// map with the [`PSR_FLAG`] bit set.
fn execute_set(insn: &Instruction, gprs: &mut std::collections::HashMap<u32, u32>) {
    if insn.sub_group > 1 {
        return;
    }

    let dst_reg = insn.dst;
    let src_val = fetch_operand(insn.imm_op2, insn.src, gprs);
    let dst_val = gprs.get(&dst_reg).copied().unwrap_or(0);

    let result = match insn.set_opt {
        0x01 => src_val, // MOVE (assignment)
        0x02 => {
            // SWAP
            gprs.insert(insn.src, dst_val);
            src_val
        }
        0x03 => dst_val.wrapping_add(src_val),     // ADD
        0x04 => dst_val.wrapping_sub(src_val),     // SUB
        0x05 => dst_val.wrapping_mul(src_val),     // MUL
        0x06 if src_val != 0 => dst_val / src_val, // DIV
        0x07 if src_val != 0 => dst_val % src_val, // MOD
        0x08 if src_val != 0 => 1,                 // RND: deterministic (always 1)
        0x09 => dst_val & src_val,                 // AND
        0x0A => dst_val | src_val,                 // OR
        0x0B => dst_val ^ src_val,                 // XOR
        0x0C => dst_val | (1 << src_val),          // BITSET
        0x0D => dst_val & !(1 << src_val),         // BITCLR
        0x0E => dst_val << src_val,                // SHL
        0x0F => dst_val >> src_val,                // SHR
        _ => return,                               // Unknown or unsafe (div/mod by zero) — skip
    };

    gprs.insert(dst_reg, result);
}

/// Executes a CMP group instruction: returns comparison result.
fn execute_cmp(insn: &Instruction, gprs: &std::collections::HashMap<u32, u32>) -> bool {
    let dst_val = fetch_operand(insn.imm_op1, insn.dst, gprs);
    let src_val = fetch_operand(insn.imm_op2, insn.src, gprs);

    match insn.cmp_opt {
        0x02 => dst_val == src_val, // EQ (==)
        0x03 => dst_val != src_val, // NE (!=)
        0x04 => dst_val >= src_val, // GE (>=)
        0x05 => dst_val > src_val,  // GT (>)
        0x06 => dst_val <= src_val, // LE (<=)
        0x07 => dst_val < src_val,  // LT (<)
        _ => false,                 // 0x00/0x01 or unknown — no match
    }
}

/// Executes a GOTO instruction. Returns `true` if the branch was taken
/// (and `pc` was updated), `false` if execution should fall through.
///
/// `branch_opt` encoding (libbluray `hdmv_insn.h`):
/// - `0x00`: NOP — no operation
/// - `0x01`: GOTO — unconditional jump to destination
/// - `0x02`: BREAK — terminate execution
///
/// Conditional branching is handled by CMP, which skips the next
/// instruction when the comparison is false. A CMP+GOTO pair gives
/// conditional behavior: CMP true → GOTO executes → jump; CMP false
/// → GOTO is skipped → fall through.
fn execute_goto(
    insn: &Instruction,
    gprs: &std::collections::HashMap<u32, u32>,
    pc: &mut usize,
) -> bool {
    match insn.branch_opt {
        0x01 => {
            // GOTO — unconditional jump
            let target = fetch_operand(insn.imm_op1, insn.dst, gprs);
            *pc = target as usize;
            true
        }
        0x02 => {
            // BREAK — terminate execution
            *pc = usize::MAX;
            true
        }
        _ => false, // NOP (0x00) and unknown — no branch
    }
}

// ── Static resolution helpers (GotoMobj pattern) ────────────────────────

/// Collects `(comparison_value, playlist)` pairs from a movie object.
///
/// Scans for CMP instructions that reference the given GPR, then finds
/// the associated `PlayPl` instruction. Handles two patterns:
///
/// 1. **CMP + branch target:** the CMP instruction branches to a `PlayPl`
///    at a specific instruction index.
/// 2. **Positional pairing:** CMP instructions and `PlayPl` instructions
///    are paired in order (Nth CMP → Nth `PlayPl`).
fn collect_cmp_play_pairs(instrs: &[Instruction], register: u32) -> Vec<(u32, u16)> {
    let mut cmps: Vec<(usize, u32)> = Vec::new();
    let mut play_pls: Vec<(usize, u16)> = Vec::new();

    for (i, insn) in instrs.iter().enumerate() {
        if insn.group == GRP_CMP {
            if !insn.imm_op1 && insn.dst == register && insn.imm_op2 {
                cmps.push((i, insn.src));
            } else if insn.imm_op1 && !insn.imm_op2 && insn.src == register {
                cmps.push((i, insn.dst));
            }
        }

        #[allow(
            clippy::cast_possible_truncation,
            reason = "playlist numbers are u16 values"
        )]
        if insn.group == GRP_BRANCH && insn.sub_group == BRANCH_PLAY && insn.imm_op1 {
            play_pls.push((i, (insn.dst & 0xFFFF) as u16));
        }
    }

    if cmps.is_empty() || play_pls.is_empty() {
        return Vec::new();
    }

    let pairs = try_branch_target_pairing(instrs, &cmps, &play_pls);
    if !pairs.is_empty() {
        return pairs;
    }

    cmps.iter()
        .zip(play_pls.iter())
        .map(|(&(_, cmp_val), &(_, playlist))| (cmp_val, playlist))
        .collect()
}

/// Tries to pair CMP instructions with `PlayPl` via branch targets.
///
/// For each CMP, looks at the instruction immediately following it.
/// If it's a GOTO (branch group, goto sub-group) targeting a `PlayPl`
/// instruction, creates the pair.
fn try_branch_target_pairing(
    instrs: &[Instruction],
    cmps: &[(usize, u32)],
    play_pls: &[(usize, u16)],
) -> Vec<(u32, u16)> {
    let play_pl_map: std::collections::HashMap<usize, u16> = play_pls.iter().copied().collect();

    let mut pairs = Vec::new();

    for &(cmp_idx, cmp_value) in cmps {
        let next_idx = cmp_idx + 1;
        if next_idx < instrs.len() {
            let next = &instrs[next_idx];
            if next.group == GRP_BRANCH && next.sub_group == BRANCH_GOTO && next.imm_op1 {
                let target = next.dst as usize;
                if let Some(&playlist) = play_pl_map.get(&target) {
                    pairs.push((cmp_value, playlist));
                }
            }
        }
    }

    if pairs.len() == cmps.len() {
        pairs
    } else {
        Vec::new()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod tests {
    use super::*;

    // ── MOBJ file builder ───────────────────────────────────────────

    /// Builds a synthetic `MovieObject.bdmv` binary for testing.
    struct MobjBuilder {
        objects: Vec<Vec<InsnSpec>>,
    }

    /// Specification for a test instruction.
    #[derive(Clone)]
    enum InsnSpec {
        /// `PlayPl` with immediate playlist number.
        PlayPl(u32),
        /// `PlayPl` with register operand — `PlayPl(GPR[reg])`.
        PlayPlReg(u32),
        /// `SetGpr`: register = immediate value.
        SetGpr(u32, u32),
        /// `SetGpr`: dst register = src register (register-to-register move).
        SetGprReg(u32, u32),
        /// CMP: compare `GPR[register]` == immediate value.
        CmpEq(u32, u32),
        /// CMP: compare `GPR[dst]` == `GPR[src]` (register-to-register).
        CmpEqReg(u32, u32),
        /// GOTO: unconditional branch to instruction index.
        Goto(u32),
        /// GOTO: conditional branch (taken if last CMP was true).
        GotoIf(u32),
        /// `GotoMobj`: jump to movie object (`BRANCH_JUMP`, `imm_op1=1`).
        #[allow(dead_code, reason = "used via spec_to_other for button commands")]
        GotoMobj(u32),
        /// AND: `GPR[dst] &= GPR[src]` (register-to-register).
        AndReg(u32, u32),
        /// ADD: `GPR[dst] += GPR[src]` (register-to-register).
        AddReg(u32, u32),
        /// SETSYSTEM `SET_BUTTON_PAGE(GPR[dst], GPR[src])`.
        SetButtonPage(u32, u32),
        /// Nop (all zeros).
        Nop,
    }

    impl MobjBuilder {
        fn new() -> Self {
            Self {
                objects: Vec::new(),
            }
        }

        fn object(mut self, instructions: &[InsnSpec]) -> Self {
            self.objects.push(instructions.to_vec());
            self
        }

        fn build(self) -> Vec<u8> {
            let mut data = Vec::new();

            // Header: "MOBJ" + version "0200" + extension_data_start(4)
            // + reserved(28) + length(4) + reserved(4) + num_objects(2)
            data.extend_from_slice(b"MOBJ");
            data.extend_from_slice(b"0200"); // version
            data.extend_from_slice(&0u32.to_be_bytes()); // extension_data_start
            data.extend_from_slice(&[0u8; 28]); // reserved
            // length — we'll fill this later
            let length_offset = data.len();
            data.extend_from_slice(&0u32.to_be_bytes()); // placeholder
            data.extend_from_slice(&0u32.to_be_bytes()); // reserved

            #[allow(
                clippy::cast_possible_truncation,
                reason = "test builder — object count is small"
            )]
            let num_objects = self.objects.len() as u16;
            data.extend_from_slice(&num_objects.to_be_bytes());

            for obj_instrs in &self.objects {
                // Object header: 2 bytes flags/reserved + 2 bytes num_commands
                data.extend_from_slice(&[0u8; 2]); // flags + reserved
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "test builder — instruction count is small"
                )]
                let num_cmds = obj_instrs.len() as u16;
                data.extend_from_slice(&num_cmds.to_be_bytes());

                for insn in obj_instrs {
                    build_instruction(&mut data, insn);
                }
            }

            // Fill in length field (remaining data after offset 44)
            #[allow(clippy::cast_possible_truncation, reason = "test data is small")]
            let length = (data.len() - length_offset - 4) as u32;
            data[length_offset..length_offset + 4].copy_from_slice(&length.to_be_bytes());

            data
        }
    }

    fn build_instruction(buf: &mut Vec<u8>, spec: &InsnSpec) {
        match spec {
            InsnSpec::PlayPl(playlist) => {
                // grp=0 (BRANCH), sub_grp=2 (PLAY), op_cnt=1, imm_op1=1
                let insn: u32 = 0x2280_0000;
                buf.extend_from_slice(&insn.to_be_bytes());
                buf.extend_from_slice(&playlist.to_be_bytes()); // dst
                buf.extend_from_slice(&0u32.to_be_bytes()); // src
            }
            InsnSpec::SetGpr(register, value) => {
                // grp=2 (SET), sub_grp=0, op_cnt=2, imm_op2=1, set_opt=1 (MOVE)
                let insn: u32 = 0x5040_0001;
                buf.extend_from_slice(&insn.to_be_bytes());
                buf.extend_from_slice(&register.to_be_bytes());
                buf.extend_from_slice(&value.to_be_bytes());
            }
            InsnSpec::CmpEq(register, value) => {
                // grp=1 (CMP), sub_grp=0, op_cnt=2, imm_op1=0 (dst=GPR),
                // imm_op2=1 (src=immediate), cmp_opt=2 (EQ)
                let insn: u32 = 0x4840_0200;
                buf.extend_from_slice(&insn.to_be_bytes());
                buf.extend_from_slice(&register.to_be_bytes()); // dst = GPR ref
                buf.extend_from_slice(&value.to_be_bytes()); // src = immediate
            }
            InsnSpec::PlayPlReg(register) => {
                // grp=0 (BRANCH), sub_grp=2 (PLAY), op_cnt=1, imm_op1=0
                let insn: u32 = 0x2200_0000;
                buf.extend_from_slice(&insn.to_be_bytes());
                buf.extend_from_slice(&register.to_be_bytes()); // dst = GPR ref
                buf.extend_from_slice(&0u32.to_be_bytes());
            }
            InsnSpec::SetGprReg(dst_reg, src_reg) => {
                // grp=2 (SET), sub_grp=0, op_cnt=2, imm_op1=0, imm_op2=0, set_opt=1 (MOVE)
                let insn: u32 = 0x5000_0001;
                buf.extend_from_slice(&insn.to_be_bytes());
                buf.extend_from_slice(&dst_reg.to_be_bytes());
                buf.extend_from_slice(&src_reg.to_be_bytes());
            }
            InsnSpec::CmpEqReg(dst_reg, src_reg) => {
                // grp=1 (CMP), sub_grp=0, op_cnt=2, imm_op1=0, imm_op2=0, cmp_opt=2 (EQ)
                let insn: u32 = 0x4800_0200;
                buf.extend_from_slice(&insn.to_be_bytes());
                buf.extend_from_slice(&dst_reg.to_be_bytes());
                buf.extend_from_slice(&src_reg.to_be_bytes());
            }
            InsnSpec::Goto(target) => {
                // grp=0 (BRANCH), sub_grp=0 (GOTO), op_cnt=1, imm_op1=1,
                // branch_opt=1 (GOTO — unconditional jump)
                let insn: u32 = 0x2081_0000;
                buf.extend_from_slice(&insn.to_be_bytes());
                buf.extend_from_slice(&target.to_be_bytes());
                buf.extend_from_slice(&0u32.to_be_bytes());
            }
            InsnSpec::GotoIf(target) => {
                // grp=0 (BRANCH), sub_grp=0 (GOTO), op_cnt=1, imm_op1=1,
                // branch_opt=1 (GOTO — conditionality comes from CMP skip)
                let insn: u32 = 0x2081_0000;
                buf.extend_from_slice(&insn.to_be_bytes());
                buf.extend_from_slice(&target.to_be_bytes());
                buf.extend_from_slice(&0u32.to_be_bytes());
            }
            InsnSpec::GotoMobj(object_id) => {
                // grp=0 (BRANCH), sub_grp=1 (JUMP), op_cnt=1, imm_op1=1,
                // branch_opt=1 (GOTO)
                let insn: u32 = 0x2181_0000;
                buf.extend_from_slice(&insn.to_be_bytes());
                buf.extend_from_slice(&object_id.to_be_bytes());
                buf.extend_from_slice(&0u32.to_be_bytes());
            }
            InsnSpec::AndReg(dst_reg, src_reg) => {
                // grp=2 (SET), sub_grp=0, op_cnt=2, imm_op2=0, set_opt=9 (AND)
                let insn: u32 = 0x5000_0009;
                buf.extend_from_slice(&insn.to_be_bytes());
                buf.extend_from_slice(&dst_reg.to_be_bytes());
                buf.extend_from_slice(&src_reg.to_be_bytes());
            }
            InsnSpec::AddReg(dst_reg, src_reg) => {
                // grp=2 (SET), sub_grp=0, op_cnt=2, imm_op2=0, set_opt=3 (ADD)
                let insn: u32 = 0x5000_0003;
                buf.extend_from_slice(&insn.to_be_bytes());
                buf.extend_from_slice(&dst_reg.to_be_bytes());
                buf.extend_from_slice(&src_reg.to_be_bytes());
            }
            InsnSpec::SetButtonPage(dst_reg, src_reg) => {
                // grp=2 (SET), sub_grp=1 (SETSYSTEM), op_cnt=2,
                // imm_op1=0, imm_op2=0, set_opt=3 (SET_BUTTON_PAGE)
                let insn: u32 = 0x5100_0003;
                buf.extend_from_slice(&insn.to_be_bytes());
                buf.extend_from_slice(&dst_reg.to_be_bytes());
                buf.extend_from_slice(&src_reg.to_be_bytes());
            }
            InsnSpec::Nop => {
                buf.extend_from_slice(&[0u8; 12]);
            }
        }
    }

    // ── Fake buttons and pages for resolver tests ──────────────────

    fn make_button(button_id: u16, commands: Vec<NavigationCommand>) -> Button {
        Button {
            button_id,
            x: 0,
            y: 0,
            normal_object_id: 0,
            selected_object_id: 0,
            commands,
        }
    }

    fn make_page(page_id: u8, buttons: Vec<Button>) -> Page {
        Page { page_id, buttons }
    }

    /// Converts an `InsnSpec` to raw `(opcode, dst, src)` for building
    /// `NavigationCommand::Other` from test instruction specs.
    fn spec_to_raw(spec: &InsnSpec) -> (u32, u32, u32) {
        let mut buf = Vec::new();
        build_instruction(&mut buf, spec);
        let opcode = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let dst = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let src = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
        (opcode, dst, src)
    }

    /// Builds a `NavigationCommand::Other` from an `InsnSpec`.
    fn spec_to_other(spec: &InsnSpec) -> NavigationCommand {
        let (opcode, dst, src) = spec_to_raw(spec);
        NavigationCommand::Other { opcode, dst, src }
    }

    // ── Parser tests ────────────────────────────────────────────────

    #[test]
    fn parse_single_object() {
        let data = MobjBuilder::new().object(&[InsnSpec::PlayPl(203)]).build();

        let file = parse(&data).expect("should parse single object");
        assert_eq!(file.objects.len(), 1, "one object");
        assert_eq!(file.objects[0].instructions.len(), 1, "one instruction");

        let insn = &file.objects[0].instructions[0];
        assert_eq!(insn.group, GRP_BRANCH, "group is BRANCH");
        assert_eq!(insn.sub_group, BRANCH_PLAY, "sub_group is PLAY");
        assert_eq!(insn.dst, 203, "dst holds playlist number");
    }

    #[test]
    fn parse_multiple_objects() {
        let data = MobjBuilder::new()
            .object(&[InsnSpec::PlayPl(201), InsnSpec::PlayPl(202)])
            .object(&[InsnSpec::Nop])
            .object(&[
                InsnSpec::CmpEq(0, 1),
                InsnSpec::Goto(4),
                InsnSpec::CmpEq(0, 2),
                InsnSpec::Goto(5),
                InsnSpec::PlayPl(301),
                InsnSpec::PlayPl(302),
            ])
            .build();

        let file = parse(&data).expect("should parse multiple objects");
        assert_eq!(file.objects.len(), 3, "three objects");
        assert_eq!(
            file.objects[0].instructions.len(),
            2,
            "object 0: two instructions"
        );
        assert_eq!(
            file.objects[1].instructions.len(),
            1,
            "object 1: one instruction"
        );
        assert_eq!(
            file.objects[2].instructions.len(),
            6,
            "object 2: six instructions"
        );
    }

    #[test]
    fn parse_rejects_invalid_magic() {
        let mut data = MobjBuilder::new().object(&[InsnSpec::Nop]).build();
        data[0..4].copy_from_slice(b"NOPE");

        let err = parse(&data).expect_err("should reject invalid magic");
        assert!(
            err.to_string().contains("invalid magic"),
            "error mentions invalid magic: {err}"
        );
    }

    #[test]
    fn parse_rejects_truncated_file() {
        let data = b"MOBJ0200";

        let err = parse(data).expect_err("should reject truncated file");
        assert!(
            err.to_string().contains("too small"),
            "error mentions size: {err}"
        );
    }

    #[test]
    fn instruction_fields_decoded_correctly() {
        let data = MobjBuilder::new()
            .object(&[InsnSpec::SetGpr(3, 42)])
            .build();

        let file = parse(&data).expect("should parse SetGpr");
        let insn = &file.objects[0].instructions[0];

        assert_eq!(insn.group, GRP_SET, "group is SET");
        assert_eq!(insn.sub_group, 0, "sub_group is 0");
        assert_eq!(insn.op_cnt, 2, "op_cnt is 2");
        assert!(!insn.imm_op1, "dst is GPR reference");
        assert!(insn.imm_op2, "src is immediate");
        assert_eq!(insn.dst, 3, "dst is GPR 3");
        assert_eq!(insn.src, 42, "src is 42");
    }

    // ── Resolver tests ──────────────────────────────────────────────

    #[test]
    fn resolve_set_gpr_goto_mobj_with_positional_pairing() {
        // MOBJ 2 has: CmpEq(GPR[0], 1), CmpEq(GPR[0], 3), CmpEq(GPR[0], 5)
        // followed by PlayPl(201), PlayPl(203), PlayPl(205).
        // Button sets GPR[0]=3, Goto MOBJ 2 → should resolve to playlist 203.
        let mobj_data = MobjBuilder::new()
            .object(&[InsnSpec::Nop]) // MOBJ 0
            .object(&[InsnSpec::Nop]) // MOBJ 1
            .object(&[
                InsnSpec::CmpEq(0, 1),
                InsnSpec::CmpEq(0, 3),
                InsnSpec::CmpEq(0, 5),
                InsnSpec::PlayPl(201),
                InsnSpec::PlayPl(203),
                InsnSpec::PlayPl(205),
            ])
            .build();

        let mobj_file = parse(&mobj_data).expect("should parse MOBJ file");

        let button = make_button(
            7,
            vec![
                NavigationCommand::SetGpr {
                    register: 0,
                    value: 3,
                },
                NavigationCommand::GotoMobj { object_id: 2 },
            ],
        );

        let resolved = resolve_buttons(
            &[(button, PlayerContext::default())],
            &mobj_file,
            &std::collections::HashSet::new(),
            &[],
            None,
        );
        assert_eq!(resolved.len(), 1, "one button resolved");
        assert_eq!(resolved[0].button_id, 7, "button id");
        assert_eq!(resolved[0].target.playlist, 203, "resolved to playlist 203");
    }

    #[test]
    fn resolve_set_gpr_goto_mobj_with_branch_target_pairing() {
        // MOBJ 0 has: CmpEq(GPR[0], 1) → Goto 4, CmpEq(GPR[0], 5) → Goto 5,
        // then PlayPl(201) at index 4, PlayPl(205) at index 5.
        let mobj_data = MobjBuilder::new()
            .object(&[
                InsnSpec::CmpEq(0, 1),
                InsnSpec::Goto(4),
                InsnSpec::CmpEq(0, 5),
                InsnSpec::Goto(5),
                InsnSpec::PlayPl(201),
                InsnSpec::PlayPl(205),
            ])
            .build();

        let mobj_file = parse(&mobj_data).expect("should parse MOBJ file");

        let button = make_button(
            3,
            vec![
                NavigationCommand::SetGpr {
                    register: 0,
                    value: 5,
                },
                NavigationCommand::GotoMobj { object_id: 0 },
            ],
        );

        let resolved = resolve_buttons(
            &[(button, PlayerContext::default())],
            &mobj_file,
            &std::collections::HashSet::new(),
            &[],
            None,
        );
        assert_eq!(resolved.len(), 1, "one button resolved");
        assert_eq!(resolved[0].target.playlist, 205, "resolved to playlist 205");
    }

    #[test]
    fn resolve_skips_button_with_direct_play_pl() {
        let mobj_data = MobjBuilder::new().object(&[InsnSpec::PlayPl(100)]).build();
        let mobj_file = parse(&mobj_data).expect("should parse");

        let button = make_button(
            1,
            vec![NavigationCommand::PlayPl {
                playlist: 100,
                branch_opt: 0,
                mark_or_pi: 0,
            }],
        );

        let resolved = resolve_buttons(
            &[(button, PlayerContext::default())],
            &mobj_file,
            &std::collections::HashSet::new(),
            &[],
            None,
        );
        assert!(resolved.is_empty(), "direct PlayPl button skipped");
    }

    #[test]
    fn resolve_returns_empty_for_unresolvable_button() {
        let mobj_data = MobjBuilder::new()
            .object(&[InsnSpec::Nop]) // MOBJ 0 has no PlayPl
            .build();
        let mobj_file = parse(&mobj_data).expect("should parse");

        let button = make_button(
            1,
            vec![
                NavigationCommand::SetGpr {
                    register: 0,
                    value: 5,
                },
                NavigationCommand::GotoMobj { object_id: 0 },
            ],
        );

        let resolved = resolve_buttons(
            &[(button, PlayerContext::default())],
            &mobj_file,
            &std::collections::HashSet::new(),
            &[],
            None,
        );
        assert!(resolved.is_empty(), "unresolvable button not in output");
    }

    #[test]
    fn resolve_multiple_buttons_same_mobj() {
        // Three buttons all target MOBJ 0, each setting GPR[0] to a
        // different value.
        let mobj_data = MobjBuilder::new()
            .object(&[
                InsnSpec::CmpEq(0, 1),
                InsnSpec::CmpEq(0, 2),
                InsnSpec::CmpEq(0, 3),
                InsnSpec::PlayPl(201),
                InsnSpec::PlayPl(202),
                InsnSpec::PlayPl(203),
            ])
            .build();

        let mobj_file = parse(&mobj_data).expect("should parse");

        let buttons = vec![
            make_button(
                10,
                vec![
                    NavigationCommand::SetGpr {
                        register: 0,
                        value: 1,
                    },
                    NavigationCommand::GotoMobj { object_id: 0 },
                ],
            ),
            make_button(
                11,
                vec![
                    NavigationCommand::SetGpr {
                        register: 0,
                        value: 2,
                    },
                    NavigationCommand::GotoMobj { object_id: 0 },
                ],
            ),
            make_button(
                12,
                vec![
                    NavigationCommand::SetGpr {
                        register: 0,
                        value: 3,
                    },
                    NavigationCommand::GotoMobj { object_id: 0 },
                ],
            ),
        ];

        let resolved = resolve_buttons(
            &buttons
                .into_iter()
                .map(|b| (b, PlayerContext::default()))
                .collect::<Vec<_>>(),
            &mobj_file,
            &std::collections::HashSet::new(),
            &[],
            None,
        );
        assert_eq!(resolved.len(), 3, "all three buttons resolved");

        assert_eq!(resolved[0].button_id, 10, "button 10");
        assert_eq!(resolved[0].target.playlist, 201, "button 10 → playlist 201");

        assert_eq!(resolved[1].button_id, 11, "button 11");
        assert_eq!(resolved[1].target.playlist, 202, "button 11 → playlist 202");

        assert_eq!(resolved[2].button_id, 12, "button 12");
        assert_eq!(resolved[2].target.playlist, 203, "button 12 → playlist 203");
    }

    #[test]
    fn resolve_unconditional_single_play_pl() {
        // MOBJ with a single PlayPl and no CMP — unconditional play.
        let mobj_data = MobjBuilder::new().object(&[InsnSpec::PlayPl(500)]).build();
        let mobj_file = parse(&mobj_data).expect("should parse");

        let button = make_button(
            1,
            vec![
                NavigationCommand::SetGpr {
                    register: 0,
                    value: 99,
                },
                NavigationCommand::GotoMobj { object_id: 0 },
            ],
        );

        let resolved = resolve_buttons(
            &[(button, PlayerContext::default())],
            &mobj_file,
            &std::collections::HashSet::new(),
            &[],
            None,
        );
        assert_eq!(resolved.len(), 1, "resolved via unconditional fallback");
        assert_eq!(resolved[0].target.playlist, 500, "playlist 500");
    }

    #[test]
    fn resolve_out_of_bounds_mobj_returns_empty() {
        let mobj_data = MobjBuilder::new().object(&[InsnSpec::Nop]).build();
        let mobj_file = parse(&mobj_data).expect("should parse");

        let button = make_button(
            1,
            vec![
                NavigationCommand::SetGpr {
                    register: 0,
                    value: 1,
                },
                NavigationCommand::GotoMobj { object_id: 99 }, // out of bounds
            ],
        );

        let resolved = resolve_buttons(
            &[(button, PlayerContext::default())],
            &mobj_file,
            &std::collections::HashSet::new(),
            &[],
            None,
        );
        assert!(resolved.is_empty(), "out-of-bounds MOBJ not resolved");
    }

    // ── VM-based resolver tests (GPR dispatch pattern) ──────────────

    #[test]
    fn vm_resolves_gpr_dispatch_pattern() {
        // Warner Bros. style: button sets GPR[4075]=5, MOBJ compares
        // GPR[4075] against GPR[4076], then overwrites GPR[4075] with
        // the actual playlist number and calls PlayPl(GPR[4075]).
        let mobj_data = MobjBuilder::new()
            .object(&[
                // Case 1: GPR[4076]=1, compare, set playlist 201
                InsnSpec::SetGpr(4076, 1),
                InsnSpec::CmpEqReg(4075, 4076),
                InsnSpec::GotoIf(6),
                // Case 2: GPR[4076]=5, compare, set playlist 205
                InsnSpec::SetGpr(4076, 5),
                InsnSpec::CmpEqReg(4075, 4076),
                InsnSpec::GotoIf(9),
                // PlayPl for case 1
                InsnSpec::SetGpr(4075, 201),
                InsnSpec::PlayPlReg(4075),
                InsnSpec::Nop, // unreachable
                // PlayPl for case 2
                InsnSpec::SetGpr(4075, 205),
                InsnSpec::PlayPlReg(4075),
            ])
            .build();

        let mobj_file = parse(&mobj_data).expect("should parse");

        // Button sets GPR[4075]=5 → should resolve to playlist 205
        let button = make_button(
            7,
            vec![NavigationCommand::SetGpr {
                register: 4075,
                value: 5,
            }],
        );

        let resolved = resolve_buttons(
            &[(button, PlayerContext::default())],
            &mobj_file,
            &std::collections::HashSet::new(),
            &[],
            None,
        );
        assert_eq!(resolved.len(), 1, "one button resolved");
        assert_eq!(resolved[0].target.playlist, 205, "resolved to playlist 205");
    }

    #[test]
    fn vm_resolves_multiple_buttons_gpr_dispatch() {
        // Three buttons with different dispatch keys, all resolved by
        // the same MOBJ.
        let mobj_data = MobjBuilder::new()
            .object(&[
                InsnSpec::SetGpr(4076, 1),      // 0
                InsnSpec::CmpEqReg(4075, 4076), // 1
                InsnSpec::GotoIf(10),           // 2: → handler 1
                InsnSpec::SetGpr(4076, 2),      // 3
                InsnSpec::CmpEqReg(4075, 4076), // 4
                InsnSpec::GotoIf(13),           // 5: → handler 2
                InsnSpec::SetGpr(4076, 3),      // 6
                InsnSpec::CmpEqReg(4075, 4076), // 7
                InsnSpec::GotoIf(16),           // 8: → handler 3
                InsnSpec::Goto(18),             // 9: no match → end
                InsnSpec::SetGpr(4075, 201),    // 10: handler 1
                InsnSpec::PlayPlReg(4075),      // 11
                InsnSpec::Goto(18),             // 12: → end
                InsnSpec::SetGpr(4075, 202),    // 13: handler 2
                InsnSpec::PlayPlReg(4075),      // 14
                InsnSpec::Goto(18),             // 15: → end
                InsnSpec::SetGpr(4075, 203),    // 16: handler 3
                InsnSpec::PlayPlReg(4075),      // 17
                InsnSpec::Nop,                  // 18: end
            ])
            .build();

        let mobj_file = parse(&mobj_data).expect("should parse");
        let valid_playlists: std::collections::HashSet<u32> = [201, 202, 203].into();

        let buttons = vec![
            make_button(
                10,
                vec![NavigationCommand::SetGpr {
                    register: 4075,
                    value: 1,
                }],
            ),
            make_button(
                11,
                vec![NavigationCommand::SetGpr {
                    register: 4075,
                    value: 2,
                }],
            ),
            make_button(
                12,
                vec![NavigationCommand::SetGpr {
                    register: 4075,
                    value: 3,
                }],
            ),
        ];

        let resolved = resolve_buttons(
            &buttons
                .into_iter()
                .map(|b| (b, PlayerContext::default()))
                .collect::<Vec<_>>(),
            &mobj_file,
            &valid_playlists,
            &[],
            None,
        );
        assert_eq!(resolved.len(), 3, "all three buttons resolved");
        assert_eq!(resolved[0].target.playlist, 201, "button 10 → 201");
        assert_eq!(resolved[1].target.playlist, 202, "button 11 → 202");
        assert_eq!(resolved[2].target.playlist, 203, "button 12 → 203");
    }

    #[test]
    fn vm_skips_mobj_without_register_play_pl() {
        // MOBJ 0 has only immediate PlayPl (no register-based).
        // MOBJ 1 has the register-based dispatch.
        // The VM should skip MOBJ 0 and find the match in MOBJ 1.
        let mobj_data = MobjBuilder::new()
            .object(&[InsnSpec::PlayPl(999)]) // immediate only
            .object(&[
                InsnSpec::SetGpr(4076, 5),
                InsnSpec::CmpEqReg(4075, 4076),
                InsnSpec::GotoIf(5),
                InsnSpec::Nop,
                InsnSpec::Nop,
                InsnSpec::SetGpr(4075, 300),
                InsnSpec::PlayPlReg(4075),
            ])
            .build();

        let mobj_file = parse(&mobj_data).expect("should parse");

        let button = make_button(
            1,
            vec![NavigationCommand::SetGpr {
                register: 4075,
                value: 5,
            }],
        );

        let resolved = resolve_buttons(
            &[(button, PlayerContext::default())],
            &mobj_file,
            &std::collections::HashSet::new(),
            &[],
            None,
        );
        assert_eq!(resolved.len(), 1, "resolved via MOBJ 1");
        assert_eq!(resolved[0].target.playlist, 300, "playlist 300");
    }

    #[test]
    fn vm_no_match_returns_empty() {
        // Button sets GPR[4075]=99 but MOBJ only handles value 1.
        // When CMP fails, execution skips past the PlayPl block.
        let mobj_data = MobjBuilder::new()
            .object(&[
                InsnSpec::SetGpr(4076, 1),      // 0: expected key
                InsnSpec::CmpEqReg(4075, 4076), // 1: compare
                InsnSpec::GotoIf(4),            // 2: if match → 4
                InsnSpec::Goto(6),              // 3: else skip past PlayPl
                InsnSpec::SetGpr(4075, 201),    // 4: set playlist
                InsnSpec::PlayPlReg(4075),      // 5: play
                InsnSpec::Nop,                  // 6: end
            ])
            .build();

        let mobj_file = parse(&mobj_data).expect("should parse");

        let button = make_button(
            1,
            vec![NavigationCommand::SetGpr {
                register: 4075,
                value: 99,
            }],
        );

        let resolved = resolve_buttons(
            &[(button, PlayerContext::default())],
            &mobj_file,
            &std::collections::HashSet::new(),
            &[],
            None,
        );
        assert!(resolved.is_empty(), "unmatched dispatch key not resolved");
    }

    #[test]
    fn vm_handles_register_to_register_set() {
        // MOBJ copies a value between registers before PlayPl.
        let mobj_data = MobjBuilder::new()
            .object(&[
                InsnSpec::SetGprReg(100, 4075), // GPR[100] = GPR[4075]
                InsnSpec::PlayPlReg(100),       // PlayPl(GPR[100])
            ])
            .build();

        let mobj_file = parse(&mobj_data).expect("should parse");

        let button = make_button(
            1,
            vec![NavigationCommand::SetGpr {
                register: 4075,
                value: 203,
            }],
        );

        let resolved = resolve_buttons(
            &[(button, PlayerContext::default())],
            &mobj_file,
            &std::collections::HashSet::new(),
            &[],
            None,
        );
        assert_eq!(resolved.len(), 1, "resolved via register copy");
        assert_eq!(resolved[0].target.playlist, 203, "playlist 203");
    }

    // ── Dispatch entry detection tests ─────────────────────────────

    #[test]
    fn find_dispatch_entries_finds_menu_play_pl() {
        // MOBJ 0: PlayPl(800) at index 2, where 800 is a menu playlist.
        // Dispatch entry should be (mobj=0, pc=3).
        let mobj_data = MobjBuilder::new()
            .object(&[
                InsnSpec::Nop,             // 0: init
                InsnSpec::Nop,             // 1: init
                InsnSpec::PlayPl(800),     // 2: play menu playlist
                InsnSpec::Nop,             // 3: dispatch starts here
                InsnSpec::PlayPlReg(4075), // 4: content PlayPl
            ])
            .build();

        let mobj_file = parse(&mobj_data).expect("should parse");
        let menu_playlists: std::collections::HashSet<u32> = [800].into();

        let entries = find_dispatch_entries(&mobj_file, &menu_playlists);
        assert_eq!(entries.len(), 1, "one dispatch entry");
        assert_eq!(entries[0].mobj_index, 0, "mobj index");
        assert_eq!(entries[0].dispatch_pc, 3, "dispatch pc is 3");
    }

    #[test]
    fn find_dispatch_entries_ignores_non_menu_play_pl() {
        // MOBJ has PlayPl(201) but 201 is not a menu playlist.
        let mobj_data = MobjBuilder::new()
            .object(&[InsnSpec::PlayPl(201), InsnSpec::Nop])
            .build();

        let mobj_file = parse(&mobj_data).expect("should parse");
        let menu_playlists: std::collections::HashSet<u32> = [800].into();

        let entries = find_dispatch_entries(&mobj_file, &menu_playlists);
        assert!(entries.is_empty(), "no entries for non-menu PlayPl");
    }

    #[test]
    fn find_dispatch_entries_ignores_last_instruction() {
        // PlayPl(800) is the last instruction — no room for dispatch_pc.
        let mobj_data = MobjBuilder::new().object(&[InsnSpec::PlayPl(800)]).build();

        let mobj_file = parse(&mobj_data).expect("should parse");
        let menu_playlists: std::collections::HashSet<u32> = [800].into();

        let entries = find_dispatch_entries(&mobj_file, &menu_playlists);
        assert!(
            entries.is_empty(),
            "no entry when PlayPl is last instruction"
        );
    }

    #[test]
    fn find_dispatch_entries_multiple_mobjs() {
        // Two MOBJs each with a menu PlayPl.
        let mobj_data = MobjBuilder::new()
            .object(&[
                InsnSpec::PlayPl(800), // 0: menu
                InsnSpec::Nop,         // 1: dispatch
            ])
            .object(&[
                InsnSpec::Nop,         // 0: init
                InsnSpec::PlayPl(801), // 1: menu
                InsnSpec::Nop,         // 2: dispatch
            ])
            .build();

        let mobj_file = parse(&mobj_data).expect("should parse");
        let menu_playlists: std::collections::HashSet<u32> = [800, 801].into();

        let entries = find_dispatch_entries(&mobj_file, &menu_playlists);
        assert_eq!(entries.len(), 2, "two dispatch entries");
        assert_eq!(entries[0].mobj_index, 0, "first entry: mobj 0");
        assert_eq!(entries[0].dispatch_pc, 1, "first entry: pc 1");
        assert_eq!(entries[1].mobj_index, 1, "second entry: mobj 1");
        assert_eq!(entries[1].dispatch_pc, 2, "second entry: pc 2");
    }

    // ── GPR dispatch via entry points tests ────────────────────────

    #[test]
    fn dispatch_entry_resolves_warner_bros_pattern() {
        // Simulates the Warner Bros. authoring pattern:
        // - Initialization code (instructions 0–4) that clobbers GPR[4075]
        // - PlayPl(800) at instruction 5 (menu playlist, suspends MOBJ)
        // - Dispatch logic at instruction 6+ (resumes after button press)
        let mobj_data = MobjBuilder::new()
            .object(&[
                // Initialization — clobbers GPR[4075]
                InsnSpec::SetGpr(4075, 0), // 0: clear dispatch key
                InsnSpec::SetGpr(4076, 0), // 1: clear scratch
                InsnSpec::SetGpr(4077, 0), // 2: clear scratch
                InsnSpec::Nop,             // 3: init
                InsnSpec::Nop,             // 4: init
                InsnSpec::PlayPl(800),     // 5: play menu (suspends)
                // Dispatch logic — resumes here after button activation
                InsnSpec::SetGpr(4076, 1),      // 6: case key = 1
                InsnSpec::CmpEqReg(4075, 4076), // 7: GPR[4075] == 1?
                InsnSpec::GotoIf(17),           // 8: → handler 1
                InsnSpec::SetGpr(4076, 2),      // 9: case key = 2
                InsnSpec::CmpEqReg(4075, 4076), // 10: GPR[4075] == 2?
                InsnSpec::GotoIf(20),           // 11: → handler 2
                InsnSpec::SetGpr(4076, 3),      // 12: case key = 3
                InsnSpec::CmpEqReg(4075, 4076), // 13: GPR[4075] == 3?
                InsnSpec::GotoIf(23),           // 14: → handler 3
                InsnSpec::Goto(25),             // 15: no match → end
                InsnSpec::Nop,                  // 16: pad
                InsnSpec::SetGpr(4075, 201),    // 17: handler 1
                InsnSpec::PlayPlReg(4075),      // 18: play
                InsnSpec::Goto(25),             // 19: → end
                InsnSpec::SetGpr(4075, 202),    // 20: handler 2
                InsnSpec::PlayPlReg(4075),      // 21: play
                InsnSpec::Goto(25),             // 22: → end
                InsnSpec::SetGpr(4075, 203),    // 23: handler 3
                InsnSpec::PlayPlReg(4075),      // 24: play
                InsnSpec::Nop,                  // 25: end
            ])
            .build();

        let mobj_file = parse(&mobj_data).expect("should parse");

        // Only content playlists are valid — menu playlist 800 is excluded.
        let valid_playlists: std::collections::HashSet<u32> = [201, 202, 203].into();

        // Without dispatch entries, the lifecycle simulation still works:
        // it executes from instruction 0, hits PlayPl(800) (non-valid),
        // re-applies button GPR[4075]=2, and continues to the dispatch.
        let button_lifecycle = make_button(
            1,
            vec![NavigationCommand::SetGpr {
                register: 4075,
                value: 2,
            }],
        );
        let lifecycle_result = resolve_buttons(
            &[(button_lifecycle, PlayerContext::default())],
            &mobj_file,
            &valid_playlists,
            &[], // no dispatch entries — lifecycle simulation handles it
            None,
        );
        assert_eq!(
            lifecycle_result.len(),
            1,
            "lifecycle simulation resolves without dispatch entries"
        );
        assert_eq!(
            lifecycle_result[0].target.playlist, 202,
            "lifecycle: button key=2 → playlist 202"
        );

        // With dispatch entries (PlayPl(800) is the menu playlist),
        // the VM starts at instruction 6, skipping initialization.
        let dispatch_entries = vec![DispatchEntry {
            mobj_index: 0,
            dispatch_pc: 6,
        }];

        let button = make_button(
            1,
            vec![NavigationCommand::SetGpr {
                register: 4075,
                value: 2,
            }],
        );

        let resolved = resolve_buttons(
            &[(button, PlayerContext::default())],
            &mobj_file,
            &valid_playlists,
            &dispatch_entries,
            None,
        );
        assert_eq!(resolved.len(), 1, "resolved with dispatch entry");
        assert_eq!(
            resolved[0].target.playlist, 202,
            "button key=2 → playlist 202"
        );
    }

    #[test]
    fn dispatch_entry_resolves_multiple_buttons() {
        // Three buttons with different dispatch keys, resolved via the
        // same dispatch entry point.
        let mobj_data = MobjBuilder::new()
            .object(&[
                InsnSpec::SetGpr(4075, 0), // 0: init clobbers
                InsnSpec::PlayPl(800),     // 1: menu
                // Dispatch at pc=2
                InsnSpec::SetGpr(4076, 10),     // 2: case 10
                InsnSpec::CmpEqReg(4075, 4076), // 3
                InsnSpec::GotoIf(11),           // 4: → handler 10
                InsnSpec::SetGpr(4076, 20),     // 5: case 20
                InsnSpec::CmpEqReg(4075, 4076), // 6
                InsnSpec::GotoIf(14),           // 7: → handler 20
                InsnSpec::SetGpr(4076, 30),     // 8: case 30
                InsnSpec::CmpEqReg(4075, 4076), // 9
                InsnSpec::GotoIf(17),           // 10: → handler 30
                InsnSpec::SetGpr(4075, 301),    // 11: handler 10
                InsnSpec::PlayPlReg(4075),      // 12
                InsnSpec::Goto(19),             // 13: → end
                InsnSpec::SetGpr(4075, 302),    // 14: handler 20
                InsnSpec::PlayPlReg(4075),      // 15
                InsnSpec::Goto(19),             // 16: → end
                InsnSpec::SetGpr(4075, 303),    // 17: handler 30
                InsnSpec::PlayPlReg(4075),      // 18
                InsnSpec::Nop,                  // 19: end
            ])
            .build();

        let mobj_file = parse(&mobj_data).expect("should parse");

        let dispatch_entries = vec![DispatchEntry {
            mobj_index: 0,
            dispatch_pc: 2,
        }];

        let buttons = vec![
            make_button(
                1,
                vec![NavigationCommand::SetGpr {
                    register: 4075,
                    value: 10,
                }],
            ),
            make_button(
                2,
                vec![NavigationCommand::SetGpr {
                    register: 4075,
                    value: 20,
                }],
            ),
            make_button(
                3,
                vec![NavigationCommand::SetGpr {
                    register: 4075,
                    value: 30,
                }],
            ),
        ];

        let resolved = resolve_buttons(
            &buttons
                .into_iter()
                .map(|b| (b, PlayerContext::default()))
                .collect::<Vec<_>>(),
            &mobj_file,
            &std::collections::HashSet::new(),
            &dispatch_entries,
            None,
        );
        assert_eq!(resolved.len(), 3, "all three buttons resolved");
        assert_eq!(resolved[0].target.playlist, 301, "key 10 → 301");
        assert_eq!(resolved[1].target.playlist, 302, "key 20 → 302");
        assert_eq!(resolved[2].target.playlist, 303, "key 30 → 303");
    }

    #[test]
    fn dispatch_entry_end_to_end_with_find() {
        // End-to-end: build an MOBJ, find dispatch entries, resolve buttons.
        let mobj_data = MobjBuilder::new()
            .object(&[
                InsnSpec::SetGpr(4075, 0), // 0: init
                InsnSpec::PlayPl(800),     // 1: menu playlist
                // Dispatch at pc=2
                InsnSpec::SetGpr(4076, 5),      // 2
                InsnSpec::CmpEqReg(4075, 4076), // 3
                InsnSpec::GotoIf(7),            // 4
                InsnSpec::Nop,                  // 5
                InsnSpec::Nop,                  // 6
                InsnSpec::SetGpr(4075, 205),    // 7
                InsnSpec::PlayPlReg(4075),      // 8
            ])
            .build();

        let mobj_file = parse(&mobj_data).expect("should parse");

        // Find dispatch entries using the menu playlist set
        let menu_playlists: std::collections::HashSet<u32> = [800].into();
        let dispatch_entries = find_dispatch_entries(&mobj_file, &menu_playlists);
        assert_eq!(dispatch_entries.len(), 1, "one dispatch entry found");
        assert_eq!(dispatch_entries[0].dispatch_pc, 2, "dispatch_pc = 2");

        let button = make_button(
            7,
            vec![NavigationCommand::SetGpr {
                register: 4075,
                value: 5,
            }],
        );

        let resolved = resolve_buttons(
            &[(button, PlayerContext::default())],
            &mobj_file,
            &std::collections::HashSet::new(),
            &dispatch_entries,
            None,
        );
        assert_eq!(resolved.len(), 1, "resolved via dispatch entry");
        assert_eq!(resolved[0].target.playlist, 205, "key 5 → playlist 205");
    }

    #[test]
    fn dispatch_entry_goto_mobj_unaffected() {
        // GotoMobj buttons should still work when dispatch entries exist.
        let mobj_data = MobjBuilder::new()
            .object(&[
                InsnSpec::PlayPl(800), // 0: menu PlayPl
                InsnSpec::Nop,         // 1: dispatch
            ])
            .object(&[InsnSpec::PlayPl(201)]) // MOBJ 1: target
            .build();

        let mobj_file = parse(&mobj_data).expect("should parse");

        let dispatch_entries = vec![DispatchEntry {
            mobj_index: 0,
            dispatch_pc: 1,
        }];

        // GotoMobj button — should resolve via static path, not dispatch
        let button = make_button(
            1,
            vec![
                NavigationCommand::SetGpr {
                    register: 0,
                    value: 1,
                },
                NavigationCommand::GotoMobj { object_id: 1 },
            ],
        );

        let resolved = resolve_buttons(
            &[(button, PlayerContext::default())],
            &mobj_file,
            &std::collections::HashSet::new(),
            &dispatch_entries,
            None,
        );
        assert_eq!(resolved.len(), 1, "GotoMobj still works");
        assert_eq!(resolved[0].target.playlist, 201, "resolved via GotoMobj");
    }

    // ── Dispatch table extraction tests ───────────────────────────────

    /// Builds a synthetic dispatch MOBJ with the CMP/GOTO switch pattern.
    ///
    /// Structure:
    /// - Instructions 0..cases*4: switch table (4 instructions per case)
    /// - Instructions cases*4..: handler blocks (SET + `PlayPl` + GOTO per case)
    fn build_dispatch_mobj(cases: &[(u32, u16)]) -> Vec<InsnSpec> {
        let switch_end = cases.len() * 4;
        // +1 for exit GOTO between switch and handlers
        let handlers_start = switch_end + 1;
        let exit_pc = handlers_start + cases.len() * 3;
        let mut instrs = Vec::new();

        // Switch table: 4 instructions per case
        for (i, &(case_val, _)) in cases.iter().enumerate() {
            let handler_pc = handlers_start + i * 3;
            instrs.push(InsnSpec::SetGprReg(100, 200)); // load dispatch reg
            instrs.push(InsnSpec::SetGpr(101, case_val)); // case value
            instrs.push(InsnSpec::CmpEqReg(100, 101)); // compare
            #[allow(
                clippy::cast_possible_truncation,
                reason = "test data — handler_pc fits in u32"
            )]
            instrs.push(InsnSpec::Goto(handler_pc as u32)); // → handler
        }

        // Exit GOTO: no match → skip all handlers (real discs have this)
        #[allow(
            clippy::cast_possible_truncation,
            reason = "test data — exit_pc fits in u32"
        )]
        instrs.push(InsnSpec::Goto(exit_pc as u32));

        // Handler blocks: SET playlist + PlayPl + GOTO loop
        for &(_, playlist) in cases {
            instrs.push(InsnSpec::SetGpr(100, u32::from(playlist)));
            instrs.push(InsnSpec::PlayPlReg(100));
            instrs.push(InsnSpec::Goto(0)); // loop back
        }

        // Exit target
        instrs.push(InsnSpec::Nop);

        instrs
    }

    #[test]
    fn extract_dispatch_table_three_cases() {
        let dispatch_mobj = build_dispatch_mobj(&[(0, 201), (1, 202), (2, 203)]);
        let mobj_data = MobjBuilder::new().object(&dispatch_mobj).build();

        let mobj_file = parse(&mobj_data).expect("should parse dispatch MOBJ");
        let table = extract_dispatch_table(&mobj_file).expect("should extract dispatch table");

        assert_eq!(table.mobj_index, 0, "dispatch MOBJ is index 0");
        assert_eq!(
            table.dispatch_register, 200,
            "dispatch register is GPR[200]"
        );
        assert_eq!(table.cases.len(), 3, "three cases");
        assert_eq!(table.cases[0], (0, 201), "case 0 → playlist 201");
        assert_eq!(table.cases[1], (1, 202), "case 1 → playlist 202");
        assert_eq!(table.cases[2], (2, 203), "case 2 → playlist 203");
    }

    #[test]
    fn extract_dispatch_table_with_init_code() {
        // Init code before the switch table — should still extract correctly.
        let mut instrs = vec![
            InsnSpec::SetGpr(200, 0), // init: clear dispatch register
            InsnSpec::SetGpr(100, 0), // init: clear scratch
            InsnSpec::Nop,            // init: pad
        ];
        // 3 init instructions, then switch at 3..15, handlers at 15..
        let cases: [(u32, u16); 3] = [(5, 301), (10, 302), (15, 303)];
        let switch_start = instrs.len();
        for (i, &(case_val, _)) in cases.iter().enumerate() {
            let handler_pc = switch_start + cases.len() * 4 + i * 3;
            instrs.push(InsnSpec::SetGprReg(100, 200));
            instrs.push(InsnSpec::SetGpr(101, case_val));
            instrs.push(InsnSpec::CmpEqReg(100, 101));
            #[allow(
                clippy::cast_possible_truncation,
                reason = "test data — handler_pc fits in u32"
            )]
            instrs.push(InsnSpec::Goto(handler_pc as u32));
        }
        for &(_, playlist) in &cases {
            instrs.push(InsnSpec::SetGpr(100, u32::from(playlist)));
            instrs.push(InsnSpec::PlayPlReg(100));
            instrs.push(InsnSpec::Goto(0));
        }

        let mobj_data = MobjBuilder::new().object(&instrs).build();
        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

        assert_eq!(table.cases.len(), 3, "three cases despite init code");
        assert_eq!(table.cases[0], (5, 301), "case 5 → 301");
        assert_eq!(table.cases[1], (10, 302), "case 10 → 302");
        assert_eq!(table.cases[2], (15, 303), "case 15 → 303");
    }

    #[test]
    fn extract_dispatch_table_picks_mobj_with_most_handlers() {
        // MOBJ 0: small (1 PlayPl), MOBJ 1: dispatch table (4 cases)
        let small_mobj = vec![InsnSpec::PlayPl(999)];
        let dispatch_mobj = build_dispatch_mobj(&[(0, 201), (1, 202), (2, 203), (3, 204)]);

        let mobj_data = MobjBuilder::new()
            .object(&small_mobj)
            .object(&dispatch_mobj)
            .build();

        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

        assert_eq!(table.mobj_index, 1, "picked MOBJ 1 (more handlers)");
        assert_eq!(table.cases.len(), 4, "four cases");
    }

    #[test]
    fn extract_dispatch_table_none_for_goto_mobj_disc() {
        // GotoMobj-style disc: no CMP/GOTO switch pattern, only simple MOBJs.
        let mobj_data = MobjBuilder::new()
            .object(&[InsnSpec::PlayPl(201)])
            .object(&[InsnSpec::PlayPl(202)])
            .object(&[
                InsnSpec::CmpEq(0, 1),
                InsnSpec::Goto(4),
                InsnSpec::CmpEq(0, 2),
                InsnSpec::Goto(5),
                InsnSpec::PlayPl(301),
                InsnSpec::PlayPl(302),
            ])
            .build();

        let mobj_file = parse(&mobj_data).expect("should parse");
        assert!(
            extract_dispatch_table(&mobj_file).is_none(),
            "no dispatch table for GotoMobj-style disc"
        );
    }

    #[test]
    fn dispatch_table_resolves_buttons_composite() {
        // Composite dispatch: case = button_id + key.
        // button_id=6, key=5 → case 11; button_id=3, key=5 → case 8;
        // button_id=0, key=5 → case 5 (backwards compatible with raw key).
        let dispatch_mobj = build_dispatch_mobj(&[(5, 205), (8, 208), (11, 211)]);
        let mobj_data = MobjBuilder::new().object(&dispatch_mobj).build();

        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

        let buttons = vec![
            // button_id=6, key=5 → composite 11 → playlist 211
            make_button(
                6,
                vec![NavigationCommand::SetGpr {
                    register: 4075,
                    value: 5,
                }],
            ),
            // button_id=3, key=5 → composite 8 → playlist 208
            make_button(
                3,
                vec![NavigationCommand::SetGpr {
                    register: 4075,
                    value: 5,
                }],
            ),
            // button_id=0, key=5 → composite 5 → playlist 205
            make_button(
                0,
                vec![NavigationCommand::SetGpr {
                    register: 4075,
                    value: 5,
                }],
            ),
        ];

        let resolved = resolve_buttons(
            &buttons
                .into_iter()
                .map(|b| (b, PlayerContext::default()))
                .collect::<Vec<_>>(),
            &mobj_file,
            &std::collections::HashSet::new(),
            &[],
            Some(&table),
        );
        assert_eq!(resolved.len(), 3, "all three buttons resolved via table");
        assert_eq!(
            resolved[0].target.playlist, 211,
            "button_id=6, key=5 → case 11 → 211"
        );
        assert_eq!(
            resolved[1].target.playlist, 208,
            "button_id=3, key=5 → case 8 → 208"
        );
        assert_eq!(
            resolved[2].target.playlist, 205,
            "button_id=0, key=5 → case 5 → 205"
        );
    }

    #[test]
    fn dispatch_table_composite_outside_range_not_resolved() {
        // Cases are 10–12. Button composite = button_id(1) + key(99) = 100,
        // which is outside the table range.
        let dispatch_mobj = build_dispatch_mobj(&[(10, 201), (11, 202), (12, 203)]);
        let mobj_data = MobjBuilder::new().object(&dispatch_mobj).build();

        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

        // Composite 1 + 99 = 100, not in {10, 11, 12}
        let button = make_button(
            1,
            vec![NavigationCommand::SetGpr {
                register: 4075,
                value: 99,
            }],
        );

        let resolved = resolve_buttons(
            &[(button, PlayerContext::default())],
            &mobj_file,
            &std::collections::HashSet::new(),
            &[],
            Some(&table),
        );
        assert!(resolved.is_empty(), "unmatched key not resolved via table");
    }

    #[test]
    fn dispatch_table_coexists_with_goto_mobj() {
        // Mixed disc: MOBJ 0 is a simple GotoMobj target, MOBJ 1 is a
        // dispatch table. GotoMobj buttons use pattern 1, dispatch buttons
        // use the table. Composite = button_id(20) + key(2) = 22.
        let goto_target = vec![InsnSpec::PlayPl(500)];
        let dispatch_mobj = build_dispatch_mobj(&[(20, 301), (22, 302), (24, 303)]);

        let mobj_data = MobjBuilder::new()
            .object(&goto_target)
            .object(&dispatch_mobj)
            .build();

        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

        // GotoMobj button
        let goto_button = make_button(
            10,
            vec![
                NavigationCommand::SetGpr {
                    register: 0,
                    value: 1,
                },
                NavigationCommand::GotoMobj { object_id: 0 },
            ],
        );

        // Dispatch table button: composite = 20 + 2 = 22 → case 22 → 302
        let dispatch_button = make_button(
            20,
            vec![NavigationCommand::SetGpr {
                register: 4075,
                value: 2,
            }],
        );

        let resolved = resolve_buttons(
            &[
                (goto_button, PlayerContext::default()),
                (dispatch_button, PlayerContext::default()),
            ],
            &mobj_file,
            &std::collections::HashSet::new(),
            &[],
            Some(&table),
        );
        assert_eq!(resolved.len(), 2, "both buttons resolved");
        assert_eq!(resolved[0].button_id, 10, "GotoMobj button");
        assert_eq!(resolved[0].target.playlist, 500, "GotoMobj → 500");
        assert_eq!(resolved[1].button_id, 20, "dispatch table button");
        assert_eq!(resolved[1].target.playlist, 302, "composite 20+2=22 → 302");
    }

    #[test]
    fn dispatch_table_button_id_zero_matches_raw_key() {
        // button_id=0 means composite = 0 + key = key, so the lookup is
        // backwards compatible with the raw-key case.
        let dispatch_mobj = build_dispatch_mobj(&[(0, 201), (5, 205), (10, 210)]);
        let mobj_data = MobjBuilder::new().object(&dispatch_mobj).build();

        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

        let button = make_button(
            0,
            vec![NavigationCommand::SetGpr {
                register: 4075,
                value: 10,
            }],
        );

        let resolved = resolve_buttons(
            &[(button, PlayerContext::default())],
            &mobj_file,
            &std::collections::HashSet::new(),
            &[],
            Some(&table),
        );
        assert_eq!(resolved.len(), 1, "button_id=0 resolved");
        assert_eq!(resolved[0].target.playlist, 210, "composite 0+10=10 → 210");
    }

    // ── Execution-based resolver tests ─────────────────────────────

    #[test]
    fn exec_simple_set_button_page() {
        // Simulates a simple WB extras button:
        //   SetGpr(4075, 5)           -- dispatch key
        //   SetGpr(4076, 0xFFFF)      -- mask
        //   SET GPR[4077] = PSR[10]   -- button_id
        //   GPR[4077] &= GPR[4076]   -- mask to u16
        //   GPR[4077] += GPR[4075]   -- composite = button_id + key
        //   SET_BUTTON_PAGE(GPR[4077], GPR[0])
        let button = make_button(
            3,
            vec![
                NavigationCommand::SetGpr {
                    register: 4075,
                    value: 5,
                },
                NavigationCommand::SetGpr {
                    register: 4076,
                    value: 0xFFFF,
                },
                spec_to_other(&InsnSpec::SetGprReg(4077, PSR_FLAG | 0x0A)),
                spec_to_other(&InsnSpec::AndReg(4077, 4076)),
                spec_to_other(&InsnSpec::AddReg(4077, 4075)),
                spec_to_other(&InsnSpec::SetButtonPage(4077, 0)),
            ],
        );

        // Dispatch table: case 8 (3+5) → playlist 208
        let dispatch_mobj = build_dispatch_mobj(&[(6, 206), (7, 207), (8, 208), (9, 209)]);
        let mobj_data = MobjBuilder::new().object(&dispatch_mobj).build();
        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

        let page = make_page(0, vec![button]);
        let clips = vec![NavClipInput {
            ig_pid: 0x1200,
            pages: vec![&page],
        }];

        let resolved = resolve_via_execution(
            &clips,
            &mobj_file,
            Some(&table),
            &std::collections::HashSet::new(),
        );

        assert_eq!(resolved.len(), 1, "one button resolved");
        assert_eq!(resolved[0].button_id, 3, "button_id matches");
        assert_eq!(resolved[0].target.playlist, 208, "composite 3+5=8 → 208");
    }

    #[test]
    fn exec_complex_button_branches_on_page_id() {
        // Simulates a complex WB button that computes different dispatch
        // keys based on PSR[11] (page_id):
        //   SetGpr(4075, 5)            -- default key
        //   CMP PSR[11] == 2
        //   GOTO skip1                 -- if page 2, jump ahead
        //   SetGpr(4075, 10)           -- key for page != 2
        //   skip1:
        //   SET GPR[4077] = PSR[10]
        //   GPR[4077] &= 0xFFFF (via GPR[4076])
        //   GPR[4077] += GPR[4075]
        //   SET_BUTTON_PAGE(GPR[4077], GPR[0])
        let commands = vec![
            NavigationCommand::SetGpr {
                register: 4075,
                value: 5,
            },
            spec_to_other(&InsnSpec::CmpEq(PSR_FLAG | 0x0B, 2)),
            spec_to_other(&InsnSpec::Goto(4)),
            NavigationCommand::SetGpr {
                register: 4075,
                value: 10,
            },
            // skip1 (pc=4):
            NavigationCommand::SetGpr {
                register: 4076,
                value: 0xFFFF,
            },
            spec_to_other(&InsnSpec::SetGprReg(4077, PSR_FLAG | 0x0A)),
            spec_to_other(&InsnSpec::AndReg(4077, 4076)),
            spec_to_other(&InsnSpec::AddReg(4077, 4075)),
            spec_to_other(&InsnSpec::SetButtonPage(4077, 0)),
        ];

        // Dispatch table
        let dispatch_mobj = build_dispatch_mobj(&[(12, 212), (15, 215), (17, 217)]);
        let mobj_data = MobjBuilder::new().object(&dispatch_mobj).build();
        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

        // Button 7 on page 2: CMP is true → keeps key=5 → composite=12
        let btn_page2 = make_button(7, commands.clone());
        let page2 = make_page(2, vec![btn_page2]);
        let clips_page2 = vec![NavClipInput {
            ig_pid: 0x1200,
            pages: vec![&page2],
        }];

        let resolved_p2 = resolve_via_execution(
            &clips_page2,
            &mobj_file,
            Some(&table),
            &std::collections::HashSet::new(),
        );
        assert_eq!(resolved_p2.len(), 1, "page 2 resolved");
        assert_eq!(
            resolved_p2[0].target.playlist, 212,
            "page 2: composite 7+5=12 → 212"
        );

        // Same button on page 3: CMP is false → key becomes 10 → composite=17
        let btn_page3 = make_button(7, commands);
        let page3 = make_page(3, vec![btn_page3]);
        let clips_page3 = vec![NavClipInput {
            ig_pid: 0x1200,
            pages: vec![&page3],
        }];

        let resolved_p3 = resolve_via_execution(
            &clips_page3,
            &mobj_file,
            Some(&table),
            &std::collections::HashSet::new(),
        );
        assert_eq!(resolved_p3.len(), 1, "page 3 resolved");
        assert_eq!(
            resolved_p3[0].target.playlist, 217,
            "page 3: composite 7+10=17 → 217"
        );
    }

    #[test]
    fn exec_goto_mobj_resolves_playlist() {
        // GotoMobj pattern: SetGpr(0, 42) + GotoMobj(1)
        // MOBJ[1] has: CMP GPR[0]==42, GOTO 3, Nop, PlayPl(301)
        let button = make_button(
            5,
            vec![
                NavigationCommand::SetGpr {
                    register: 0,
                    value: 42,
                },
                NavigationCommand::GotoMobj { object_id: 1 },
            ],
        );

        let mobj_data = MobjBuilder::new()
            .object(&[InsnSpec::Nop]) // MOBJ[0] — unused
            .object(&[
                InsnSpec::CmpEq(0, 42),
                InsnSpec::Goto(3),
                InsnSpec::Nop,
                InsnSpec::PlayPl(301),
            ])
            .build();
        let mobj_file = parse(&mobj_data).expect("should parse");

        let page = make_page(0, vec![button]);
        let clips = vec![NavClipInput {
            ig_pid: 0x1200,
            pages: vec![&page],
        }];

        let mut valid = std::collections::HashSet::new();
        valid.insert(301);

        let resolved = resolve_via_execution(&clips, &mobj_file, None, &valid);
        assert_eq!(resolved.len(), 1, "GotoMobj resolved");
        assert_eq!(resolved[0].target.playlist, 301, "GotoMobj → MOBJ[1] → 301");
    }

    #[test]
    fn exec_direct_play_pl_skipped() {
        // Buttons with direct PlayPl are skipped (they're already resolved
        // by the IG parser's typed variant).
        let button = make_button(
            1,
            vec![NavigationCommand::PlayPl {
                playlist: 200,
                branch_opt: 0,
                mark_or_pi: 0,
            }],
        );

        let mobj_data = MobjBuilder::new().object(&[InsnSpec::Nop]).build();
        let mobj_file = parse(&mobj_data).expect("should parse");

        let page = make_page(0, vec![button]);
        let clips = vec![NavClipInput {
            ig_pid: 0x1200,
            pages: vec![&page],
        }];

        let resolved =
            resolve_via_execution(&clips, &mobj_file, None, &std::collections::HashSet::new());
        assert!(resolved.is_empty(), "direct PlayPl buttons are skipped");
    }

    #[test]
    fn exec_step_limit_returns_none() {
        // Infinite loop: GOTO 0
        let button = make_button(1, vec![spec_to_other(&InsnSpec::Goto(0))]);

        let mobj_data = MobjBuilder::new().object(&[InsnSpec::Nop]).build();
        let mobj_file = parse(&mobj_data).expect("should parse");

        let page = make_page(0, vec![button]);
        let clips = vec![NavClipInput {
            ig_pid: 0x1200,
            pages: vec![&page],
        }];

        let resolved =
            resolve_via_execution(&clips, &mobj_file, None, &std::collections::HashSet::new());
        assert!(resolved.is_empty(), "infinite loop produces no result");
    }

    #[test]
    fn exec_multiple_pages_multiple_clips() {
        // Two clips, each with one page, each with one button.
        // Verifies that resolve_via_execution iterates all clips and pages.
        let dispatch_mobj = build_dispatch_mobj(&[(5, 205), (8, 208), (99, 299)]);
        let mobj_data = MobjBuilder::new().object(&dispatch_mobj).build();
        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

        // Clip 0: button_id=3, key=5 → composite=8 → 208
        let btn0 = make_button(
            3,
            vec![
                NavigationCommand::SetGpr {
                    register: 4075,
                    value: 5,
                },
                NavigationCommand::SetGpr {
                    register: 4076,
                    value: 0xFFFF,
                },
                spec_to_other(&InsnSpec::SetGprReg(4077, PSR_FLAG | 0x0A)),
                spec_to_other(&InsnSpec::AndReg(4077, 4076)),
                spec_to_other(&InsnSpec::AddReg(4077, 4075)),
                spec_to_other(&InsnSpec::SetButtonPage(4077, 0)),
            ],
        );
        let page0 = make_page(0, vec![btn0]);

        // Clip 1: button_id=5, key=0 → composite=5 → 205
        let btn1 = make_button(
            5,
            vec![
                NavigationCommand::SetGpr {
                    register: 4075,
                    value: 0,
                },
                NavigationCommand::SetGpr {
                    register: 4076,
                    value: 0xFFFF,
                },
                spec_to_other(&InsnSpec::SetGprReg(4077, PSR_FLAG | 0x0A)),
                spec_to_other(&InsnSpec::AndReg(4077, 4076)),
                spec_to_other(&InsnSpec::AddReg(4077, 4075)),
                spec_to_other(&InsnSpec::SetButtonPage(4077, 0)),
            ],
        );
        let page1 = make_page(0, vec![btn1]);

        let clips = vec![
            NavClipInput {
                ig_pid: 0x1200,
                pages: vec![&page0],
            },
            NavClipInput {
                ig_pid: 0x1201,
                pages: vec![&page1],
            },
        ];

        let resolved = resolve_via_execution(
            &clips,
            &mobj_file,
            Some(&table),
            &std::collections::HashSet::new(),
        );

        assert_eq!(resolved.len(), 2, "two buttons resolved across clips");
        let playlists: std::collections::HashSet<u16> =
            resolved.iter().map(|r| r.target.playlist).collect();
        assert!(playlists.contains(&205), "clip 1 button resolved to 205");
        assert!(playlists.contains(&208), "clip 0 button resolved to 208");
    }

    #[test]
    fn command_to_instruction_roundtrip() {
        // Verify that command_to_instruction produces correct Instruction
        // fields for each NavigationCommand variant.

        // PlayPl
        let play = command_to_instruction(&NavigationCommand::PlayPl {
            playlist: 42,
            branch_opt: 0,
            mark_or_pi: 0,
        });
        assert_eq!(play.group, GRP_BRANCH, "PlayPl group");
        assert_eq!(play.sub_group, BRANCH_PLAY, "PlayPl sub_group");
        assert!(play.imm_op1, "PlayPl imm_op1");
        assert_eq!(play.dst, 42, "PlayPl dst");

        // SetGpr
        let set = command_to_instruction(&NavigationCommand::SetGpr {
            register: 100,
            value: 999,
        });
        assert_eq!(set.group, GRP_SET, "SetGpr group");
        assert_eq!(set.sub_group, 0, "SetGpr sub_group");
        assert_eq!(set.set_opt, 0x01, "SetGpr MOVE");
        assert!(set.imm_op2, "SetGpr imm_op2");
        assert_eq!(set.dst, 100, "SetGpr dst");
        assert_eq!(set.src, 999, "SetGpr src");

        // GotoMobj
        let goto = command_to_instruction(&NavigationCommand::GotoMobj { object_id: 7 });
        assert_eq!(goto.group, GRP_BRANCH, "GotoMobj group");
        assert_eq!(goto.sub_group, BRANCH_JUMP, "GotoMobj sub_group");
        assert!(goto.imm_op1, "GotoMobj imm_op1");
        assert_eq!(goto.dst, 7, "GotoMobj dst");

        // Other (AND instruction)
        let (opcode, dst, src) = spec_to_raw(&InsnSpec::AndReg(4077, 4076));
        let other = command_to_instruction(&NavigationCommand::Other { opcode, dst, src });
        assert_eq!(other.group, GRP_SET, "Other AND group");
        assert_eq!(other.set_opt, 0x09, "Other AND set_opt");
        assert_eq!(other.dst, 4077, "Other AND dst");
        assert_eq!(other.src, 4076, "Other AND src");
    }

    // ── Navigation graph tests ────────────────────────────────────────

    #[test]
    fn nav_graph_propagates_gpr_state() {
        // Page 0: navigation button sets GPR[100]=42, navigates to page 1.
        // Page 1: content button computes composite = GPR[100] + button_id.
        //
        // Without propagation (empty state): GPR[100]=0 → composite = 0+5 = 5 → 205
        // With propagation: GPR[100]=42 → composite = 42+5 = 47 → 247
        let nav_button = make_button(
            1,
            vec![
                NavigationCommand::SetGpr {
                    register: 100,
                    value: 42,
                },
                NavigationCommand::SetGpr {
                    register: 50,
                    value: 0,
                },
                NavigationCommand::SetGpr {
                    register: 51,
                    value: 1,
                },
                spec_to_other(&InsnSpec::SetButtonPage(50, 51)),
            ],
        );
        let page0 = make_page(0, vec![nav_button]);

        let content_button = make_button(
            5,
            vec![
                spec_to_other(&InsnSpec::SetGprReg(4077, 100)),
                spec_to_other(&InsnSpec::AddReg(4077, PSR_FLAG | 0x0A)),
                NavigationCommand::SetGpr {
                    register: 200,
                    value: 1,
                },
                spec_to_other(&InsnSpec::SetButtonPage(4077, 200)),
            ],
        );
        let page1 = make_page(1, vec![content_button]);

        let dispatch_mobj = build_dispatch_mobj(&[(5, 205), (42, 242), (47, 247)]);
        let mobj_data = MobjBuilder::new().object(&dispatch_mobj).build();
        let mobj_file = parse(&mobj_data).expect("should parse dispatch MOBJ");
        let table = extract_dispatch_table(&mobj_file).expect("should extract dispatch table");

        let clips = vec![NavClipInput {
            ig_pid: 0x1200,
            pages: vec![&page0, &page1],
        }];

        let resolved = resolve_via_execution(
            &clips,
            &mobj_file,
            Some(&table),
            &std::collections::HashSet::new(),
        );

        let playlists: std::collections::HashSet<u16> =
            resolved.iter().map(|r| r.target.playlist).collect();
        assert!(
            playlists.contains(&247),
            "propagated GPR[100]=42 → composite 47 → 247; got {resolved:?}"
        );
        assert!(
            playlists.contains(&205),
            "empty GPR state → composite 5 → 205; got {resolved:?}"
        );
    }

    #[test]
    fn nav_graph_multiple_paths_to_same_page() {
        // Page 0: two navigation buttons, each setting GPR[100] to a
        // different value and navigating to page 1.
        // Page 1: content button reads GPR[100].
        //
        // Expected: both propagated states produce distinct resolutions.
        let nav_a = make_button(
            1,
            vec![
                NavigationCommand::SetGpr {
                    register: 100,
                    value: 10,
                },
                NavigationCommand::SetGpr {
                    register: 50,
                    value: 0,
                },
                NavigationCommand::SetGpr {
                    register: 51,
                    value: 1,
                },
                spec_to_other(&InsnSpec::SetButtonPage(50, 51)),
            ],
        );
        let nav_b = make_button(
            2,
            vec![
                NavigationCommand::SetGpr {
                    register: 100,
                    value: 20,
                },
                NavigationCommand::SetGpr {
                    register: 50,
                    value: 0,
                },
                NavigationCommand::SetGpr {
                    register: 51,
                    value: 1,
                },
                spec_to_other(&InsnSpec::SetButtonPage(50, 51)),
            ],
        );
        let page0 = make_page(0, vec![nav_a, nav_b]);

        let content = make_button(
            5,
            vec![
                spec_to_other(&InsnSpec::SetGprReg(4077, 100)),
                spec_to_other(&InsnSpec::AddReg(4077, PSR_FLAG | 0x0A)),
                NavigationCommand::SetGpr {
                    register: 200,
                    value: 1,
                },
                spec_to_other(&InsnSpec::SetButtonPage(4077, 200)),
            ],
        );
        let page1 = make_page(1, vec![content]);

        // Composites: 10+5=15, 20+5=25, 0+5=5 (empty state)
        let dispatch_mobj = build_dispatch_mobj(&[(5, 205), (15, 215), (25, 225)]);
        let mobj_data = MobjBuilder::new().object(&dispatch_mobj).build();
        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

        let clips = vec![NavClipInput {
            ig_pid: 0x1200,
            pages: vec![&page0, &page1],
        }];

        let resolved = resolve_via_execution(
            &clips,
            &mobj_file,
            Some(&table),
            &std::collections::HashSet::new(),
        );

        let playlists: std::collections::HashSet<u16> =
            resolved.iter().map(|r| r.target.playlist).collect();
        assert!(
            playlists.contains(&215),
            "path A: GPR[100]=10 → composite 15 → 215; got {resolved:?}"
        );
        assert!(
            playlists.contains(&225),
            "path B: GPR[100]=20 → composite 25 → 225; got {resolved:?}"
        );
        assert!(
            playlists.contains(&205),
            "empty state: composite 5 → 205; got {resolved:?}"
        );
    }

    #[test]
    fn nav_graph_handles_cycle() {
        // Page 0 navigates to page 1, page 1 navigates back to page 0.
        // BFS must terminate without hanging.
        let nav0 = make_button(
            1,
            vec![
                NavigationCommand::SetGpr {
                    register: 50,
                    value: 0,
                },
                NavigationCommand::SetGpr {
                    register: 51,
                    value: 1,
                },
                spec_to_other(&InsnSpec::SetButtonPage(50, 51)),
            ],
        );
        let page0 = make_page(0, vec![nav0]);

        let nav1 = make_button(
            2,
            vec![
                NavigationCommand::SetGpr {
                    register: 50,
                    value: 0,
                },
                NavigationCommand::SetGpr {
                    register: 51,
                    value: 0,
                },
                spec_to_other(&InsnSpec::SetButtonPage(50, 51)),
            ],
        );
        let page1 = make_page(1, vec![nav1]);

        let mobj_data = MobjBuilder::new().object(&[InsnSpec::Nop]).build();
        let mobj_file = parse(&mobj_data).expect("should parse");

        let clips = vec![NavClipInput {
            ig_pid: 0x1200,
            pages: vec![&page0, &page1],
        }];

        // Should terminate — visited set prevents re-processing same state
        let resolved =
            resolve_via_execution(&clips, &mobj_file, None, &std::collections::HashSet::new());
        assert!(
            resolved.is_empty(),
            "cycle with no dispatch table produces no resolutions"
        );
    }

    #[test]
    fn nav_graph_same_page_terminates() {
        // Single page where the button's SET_BUTTON_PAGE targets the
        // current page (same-page edge). Should resolve via dispatch
        // table and terminate without infinite looping.
        let button = make_button(
            3,
            vec![
                NavigationCommand::SetGpr {
                    register: 4075,
                    value: 5,
                },
                NavigationCommand::SetGpr {
                    register: 4076,
                    value: 0xFFFF,
                },
                spec_to_other(&InsnSpec::SetGprReg(4077, PSR_FLAG | 0x0A)),
                spec_to_other(&InsnSpec::AndReg(4077, 4076)),
                spec_to_other(&InsnSpec::AddReg(4077, 4075)),
                // page register GPR[60] = 0 = current page → same-page edge
                spec_to_other(&InsnSpec::SetButtonPage(4077, 60)),
            ],
        );
        let page = make_page(0, vec![button]);

        let dispatch_mobj = build_dispatch_mobj(&[(7, 207), (8, 208), (9, 209)]);
        let mobj_data = MobjBuilder::new().object(&dispatch_mobj).build();
        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

        let clips = vec![NavClipInput {
            ig_pid: 0x1200,
            pages: vec![&page],
        }];

        let resolved = resolve_via_execution(
            &clips,
            &mobj_file,
            Some(&table),
            &std::collections::HashSet::new(),
        );

        assert_eq!(resolved.len(), 1, "one button resolved");
        assert_eq!(resolved[0].target.playlist, 208, "composite 3+5=8 → 208");
    }

    // ── NOP anchor resolution tests ──────────────────────────────────

    #[test]
    fn exec_nop_anchor_resolves_via_button_id() {
        // NOP anchor buttons resolve by matching their button_id
        // directly against the dispatch table. This models the WB
        // SET_BUTTON_PAGE pattern where anchor button_ids on extras
        // pages are the dispatch cases (PSR[10] = button_id).
        //
        // Tests both true empty commands and single-NOP commands
        // (WB authoring emits a single all-zero 12-byte NOP).
        let anchor_12 = make_button(12, vec![]);
        let anchor_15 = make_button(15, vec![spec_to_other(&InsnSpec::Nop)]);
        let anchor_20 = make_button(20, vec![]);

        let dispatch_mobj =
            build_dispatch_mobj(&[(5, 205), (12, 212), (15, 215), (20, 220), (32, 232)]);
        let mobj_data = MobjBuilder::new().object(&dispatch_mobj).build();
        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

        let page = make_page(2, vec![anchor_12, anchor_15, anchor_20]);
        let clips = vec![NavClipInput {
            ig_pid: 0x1200,
            pages: vec![&page],
        }];

        let resolved = resolve_via_execution(
            &clips,
            &mobj_file,
            Some(&table),
            &std::collections::HashSet::new(),
        );

        assert_eq!(resolved.len(), 3, "three anchors resolved");
        let playlists: std::collections::HashSet<u16> =
            resolved.iter().map(|r| r.target.playlist).collect();
        assert!(playlists.contains(&212), "anchor 12 → 212");
        assert!(playlists.contains(&215), "anchor 15 → 215");
        assert!(playlists.contains(&220), "anchor 20 → 220");
    }

    #[test]
    fn exec_nop_anchor_skipped_without_dispatch_table() {
        // Without a dispatch table, NOP anchors cannot resolve.
        let anchor = make_button(12, vec![]);

        let mobj_data = MobjBuilder::new().object(&[InsnSpec::Nop]).build();
        let mobj_file = parse(&mobj_data).expect("should parse");

        let page = make_page(0, vec![anchor]);
        let clips = vec![NavClipInput {
            ig_pid: 0x1200,
            pages: vec![&page],
        }];

        let resolved =
            resolve_via_execution(&clips, &mobj_file, None, &std::collections::HashSet::new());
        assert!(
            resolved.is_empty(),
            "no dispatch table → no anchor resolution"
        );
    }

    #[test]
    fn exec_nop_anchor_unmatched_id_skipped() {
        // NOP anchor whose button_id doesn't match any dispatch case.
        let anchor = make_button(99, vec![]);

        let dispatch_mobj = build_dispatch_mobj(&[(5, 205), (10, 210), (15, 215)]);
        let mobj_data = MobjBuilder::new().object(&dispatch_mobj).build();
        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

        let page = make_page(0, vec![anchor]);
        let clips = vec![NavClipInput {
            ig_pid: 0x1200,
            pages: vec![&page],
        }];

        let resolved = resolve_via_execution(
            &clips,
            &mobj_file,
            Some(&table),
            &std::collections::HashSet::new(),
        );
        assert!(
            resolved.is_empty(),
            "button_id 99 not in dispatch table → no resolution"
        );
    }

    #[test]
    fn exec_nop_anchors_coexist_with_command_buttons() {
        // Mix of NOP anchors and command buttons on the same page.
        // Both should resolve independently.
        let command_button = make_button(
            3,
            vec![
                NavigationCommand::SetGpr {
                    register: 4075,
                    value: 5,
                },
                NavigationCommand::SetGpr {
                    register: 4076,
                    value: 0xFFFF,
                },
                spec_to_other(&InsnSpec::SetGprReg(4077, PSR_FLAG | 0x0A)),
                spec_to_other(&InsnSpec::AndReg(4077, 4076)),
                spec_to_other(&InsnSpec::AddReg(4077, 4075)),
                spec_to_other(&InsnSpec::SetButtonPage(4077, 0)),
            ],
        );
        let anchor = make_button(15, vec![]);

        let dispatch_mobj = build_dispatch_mobj(&[(8, 208), (15, 215), (20, 220)]);
        let mobj_data = MobjBuilder::new().object(&dispatch_mobj).build();
        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

        let page = make_page(0, vec![command_button, anchor]);
        let clips = vec![NavClipInput {
            ig_pid: 0x1200,
            pages: vec![&page],
        }];

        let resolved = resolve_via_execution(
            &clips,
            &mobj_file,
            Some(&table),
            &std::collections::HashSet::new(),
        );

        assert_eq!(resolved.len(), 2, "command button + anchor both resolved");
        let playlists: std::collections::HashSet<u16> =
            resolved.iter().map(|r| r.target.playlist).collect();
        assert!(playlists.contains(&208), "command button 3+5=8 → 208");
        assert!(playlists.contains(&215), "anchor 15 → 215");
    }

    #[test]
    fn exec_nop_anchor_deduplicates() {
        // Same anchor on multiple pages should only produce one resolution.
        let anchor_p0 = make_button(12, vec![]);
        let anchor_p1 = make_button(12, vec![]);

        let dispatch_mobj = build_dispatch_mobj(&[(12, 212), (15, 215), (20, 220)]);
        let mobj_data = MobjBuilder::new().object(&dispatch_mobj).build();
        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

        let page0 = make_page(0, vec![anchor_p0]);
        let page1 = make_page(1, vec![anchor_p1]);
        let clips = vec![NavClipInput {
            ig_pid: 0x1200,
            pages: vec![&page0, &page1],
        }];

        let resolved = resolve_via_execution(
            &clips,
            &mobj_file,
            Some(&table),
            &std::collections::HashSet::new(),
        );

        let matching = resolved
            .iter()
            .filter(|r| r.button_id == 12 && r.target.playlist == 212)
            .count();
        assert_eq!(
            matching, 1,
            "same anchor on two pages → deduplicated to one"
        );
    }
}
