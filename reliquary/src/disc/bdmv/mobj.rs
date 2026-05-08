// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! `MovieObject.bdmv` parser and button→playlist resolver.
//!
//! Parses the HDMV navigation programs and traces register-based button
//! commands through movie objects to resolve indirect playlist mappings.
//!
//! Reference: libbluray `src/libbluray/bdnav/mobj_parse.c`,
//! `src/libbluray/decoders/hdmv_insn.h`.

use thiserror::Error;

use super::cursor::{Cursor, CursorError};
use super::ig::{Button, NavigationCommand};

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

/// A resolved button → playlist mapping.
#[derive(Debug, PartialEq, Eq)]
pub struct ResolvedButton {
    /// Button identifier from the IG data.
    pub button_id: u16,
    /// Resolved playlist number.
    pub playlist: u16,
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

// ── Instruction group constants ─────────────────────────────────────────

/// BRANCH group — goto, jump to MOBJ, play playlist.
const GRP_BRANCH: u8 = 0;
/// CMP group — compare and conditionally branch.
const GRP_CMP: u8 = 1;
/// SET group — register operations.
const GRP_SET: u8 = 2;

/// BRANCH sub-groups.
const BRANCH_GOTO: u8 = 0;
const BRANCH_PLAY: u8 = 2;

/// Maximum instructions the mini-VM will execute before giving up.
/// Prevents infinite loops in malformed or exotic MOBJ bytecode.
const VM_STEP_LIMIT: u32 = 2000;

/// PSR (Player Status Register) bit flag. Register references with
/// bit 31 set address PSRs rather than GPRs.
const PSR_FLAG: u32 = 0x8000_0000;

/// Player context for VM execution — known PSR values derived from
/// the IG structure at extraction time.
#[derive(Debug, Clone, Default)]
pub struct PlayerContext {
    /// IG stream number (PSR 0).
    pub ig_stream: u16,
    /// Current page ID (PSR 10).
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
        reason = "bit fields are small known widths (2-4 bits)"
    )]
    Ok(Instruction {
        op_cnt: ((insn >> 29) & 0x07) as u8,
        group: ((insn >> 27) & 0x03) as u8,
        sub_group: ((insn >> 24) & 0x07) as u8,
        imm_op1: (insn >> 23) & 1 != 0,
        imm_op2: (insn >> 22) & 1 != 0,
        branch_opt: ((insn >> 18) & 0x0F) as u8,
        cmp_opt: ((insn >> 12) & 0x0F) as u8,
        set_opt: ((insn >> 4) & 0x0F) as u8,
        dst,
        src,
    })
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

// ── Button resolver ─────────────────────────────────────────────────────

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

        if let Some(playlist) =
            trace_button(button, mobj_file, valid_playlists, ctx, dispatch_entries)
        {
            resolved.push(ResolvedButton {
                button_id: button.button_id,
                playlist,
            });
        }
    }

    resolved
}

/// Traces a single button's commands through the movie object file.
///
/// Three resolution strategies:
/// 1. **`GotoMobj` pattern:** button has `SetGpr` + `GotoMobj` → look up
///    the target MOBJ and scan for matching `PlayPl`.
/// 2. **GPR dispatch via entry points:** button has `SetGpr` but no
///    `GotoMobj`, and dispatch entries are available → execute the MOBJ
///    from the dispatch entry point (`PlayPl` suspend/resume lifecycle).
/// 3. **GPR dispatch fallback:** no dispatch entries → run the mini-VM
///    on candidate MOBJs from instruction 0.
fn trace_button(
    button: &Button,
    mobj_file: &MovieObjectFile,
    valid_playlists: &std::collections::HashSet<u32>,
    ctx: &PlayerContext,
    dispatch_entries: &[DispatchEntry],
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

    // Pattern 3: GPR dispatch via lifecycle simulation — execute from
    // instruction 0 with PlayPl suspend/resume, comparing against a
    // baseline to find the dispatch result.
    if let Some(result) = resolve_via_lifecycle(mobj_file, &gpr_assignments, valid_playlists, ctx) {
        return Some(result);
    }

    // Pattern 4: direct execution from instruction 0 — works for simple
    // MOBJs without initialization that clobbers the dispatch register.
    for mobj in &mobj_file.objects {
        let instrs = &mobj.instructions;
        let has_reg_play_pl = instrs
            .iter()
            .any(|i| i.group == GRP_BRANCH && i.sub_group == BRANCH_PLAY && !i.imm_op1);
        if !has_reg_play_pl {
            continue;
        }
        if let Some(playlist) = execute_from(instrs, 0, &gpr_assignments, valid_playlists, ctx) {
            return Some(playlist);
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

        if let Some(playlist) = execute_from(
            &mobj.instructions,
            entry.dispatch_pc,
            gpr_assignments,
            valid_playlists,
            ctx,
        ) {
            return Some(playlist);
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
    gprs.insert(PSR_FLAG, u32::from(ctx.ig_stream));
    gprs.insert(PSR_FLAG | 0x0A, u32::from(ctx.page_id));

    let mut pc: usize = 0;
    let mut cmp_flag = false;
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
                cmp_flag = execute_cmp(insn, &gprs);
                pc += 1;
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
                    if !execute_goto(insn, cmp_flag, &mut pc) {
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
) -> Option<u16> {
    // Sparse register file — GPRs and PSRs share the same map.
    // PSR references use bit 31 as a flag (0x80000000 | psr_index).
    let mut gprs = std::collections::HashMap::<u32, u32>::new();
    for &(reg, val) in gpr_assignments {
        gprs.insert(reg, val);
    }
    // Seed known PSR values from player context
    gprs.insert(PSR_FLAG, u32::from(ctx.ig_stream)); // PSR[0]
    gprs.insert(PSR_FLAG | 0x0A, u32::from(ctx.page_id)); // PSR[10]

    let mut pc: usize = start_pc;
    let mut cmp_flag = false;
    let mut steps: u32 = 0;

    while pc < instrs.len() && steps < VM_STEP_LIMIT {
        steps += 1;
        let insn = &instrs[pc];

        match insn.group {
            GRP_SET => {
                execute_set(insn, &mut gprs);
                pc += 1;
            }
            GRP_CMP => {
                cmp_flag = execute_cmp(insn, &gprs);
                pc += 1;
            }
            GRP_BRANCH => {
                match insn.sub_group {
                    BRANCH_PLAY => {
                        // PlayPl — resolve the playlist number
                        let playlist = fetch_operand(insn.imm_op1, insn.dst, &gprs);
                        // Validate: must be non-zero, not 0xFFFF, and
                        // (if the valid set is non-empty) in the valid set.
                        let is_valid = playlist != 0
                            && playlist != 0xFFFF
                            && (valid_playlists.is_empty() || valid_playlists.contains(&playlist));
                        if is_valid {
                            #[allow(
                                clippy::cast_possible_truncation,
                                reason = "playlist numbers are u16 values"
                            )]
                            return Some((playlist & 0xFFFF) as u16);
                        }
                        // Not a valid playlist — keep executing (early
                        // PlayPl calls in MOBJs are often guards/fallbacks
                        // that fire with the raw dispatch key still in the
                        // register, before the real dispatch logic runs)
                        pc += 1;
                    }
                    BRANCH_GOTO => {
                        // Conditional or unconditional goto
                        if execute_goto(insn, cmp_flag, &mut pc) {
                            // Took the branch — pc already updated
                        } else {
                            pc += 1;
                        }
                    }
                    _ => {
                        // BRANCH_JUMP (GotoMobj) or unknown — skip and
                        // continue. We can't follow cross-MOBJ jumps, but
                        // the dispatch logic that handles button results is
                        // typically right after the jump instruction.
                        pc += 1;
                    }
                }
            }
            _ => {
                pc += 1; // Unknown group — skip
            }
        }
    }

    None
}

/// Fetches an operand value: immediate or from the register file.
fn fetch_operand(is_immediate: bool, raw: u32, gprs: &std::collections::HashMap<u32, u32>) -> u32 {
    if is_immediate {
        raw
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
        0x00 => src_val, // move (assignment)
        0x01 => {
            // swap
            gprs.insert(insn.src, dst_val);
            src_val
        }
        0x02 => dst_val.wrapping_add(src_val),     // add
        0x03 => dst_val.wrapping_sub(src_val),     // sub
        0x04 => dst_val.wrapping_mul(src_val),     // mul
        0x05 if src_val != 0 => dst_val / src_val, // div
        0x06 if src_val != 0 => dst_val % src_val, // mod
        0x08 => dst_val & src_val,                 // and
        0x09 => dst_val | src_val,                 // or
        0x0A => dst_val ^ src_val,                 // xor
        _ => return, // Unknown or unsafe (div/mod by zero, rnd) — skip
    };

    gprs.insert(dst_reg, result);
}

/// Executes a CMP group instruction: returns comparison result.
fn execute_cmp(insn: &Instruction, gprs: &std::collections::HashMap<u32, u32>) -> bool {
    let dst_val = fetch_operand(insn.imm_op1, insn.dst, gprs);
    let src_val = fetch_operand(insn.imm_op2, insn.src, gprs);

    match insn.cmp_opt {
        0x01 => dst_val == src_val, // EQ (==)
        0x02 => dst_val != src_val, // NE (!=)
        0x03 => dst_val >= src_val, // GE (>=)
        0x04 => dst_val > src_val,  // GT (>)
        0x05 => dst_val <= src_val, // LE (<=)
        0x06 => dst_val < src_val,  // LT (<)
        _ => false,                 // 0x00 or unknown — no match
    }
}

/// Executes a GOTO instruction. Returns `true` if the branch was taken
/// (and `pc` was updated), `false` if execution should fall through.
///
/// `branch_opt` encoding (observed from real disc data):
/// - `0x00`: conditional — branch if last CMP was true
/// - `0x01`: conditional — branch if last CMP was false (inverted)
///
/// All 3231 GOTO instructions on a WB Blu-ray title use `branch_opt=0`.
/// These are the conditional branches in the switch/case dispatch.
#[allow(clippy::missing_const_for_fn, reason = "mutates pc via &mut")]
fn execute_goto(insn: &Instruction, cmp_flag: bool, pc: &mut usize) -> bool {
    let should_branch = match insn.branch_opt {
        0x00 => cmp_flag,  // branch if comparison was true
        0x01 => !cmp_flag, // branch if comparison was false
        _ => true,         // other values — treat as unconditional
    };

    if should_branch && insn.imm_op1 {
        *pc = insn.dst as usize;
        return true;
    }

    false
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
                // grp=2 (SET), sub_grp=0, op_cnt=2, imm_op2=1, set_opt=0 (move)
                let insn: u32 = 0x5040_0000;
                buf.extend_from_slice(&insn.to_be_bytes());
                buf.extend_from_slice(&register.to_be_bytes());
                buf.extend_from_slice(&value.to_be_bytes());
            }
            InsnSpec::CmpEq(register, value) => {
                // grp=1 (CMP), sub_grp=0, op_cnt=2, imm_op1=0 (dst=GPR),
                // imm_op2=1 (src=immediate), cmp_opt=1 (EQ)
                let insn: u32 = 0x4840_1000;
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
                // grp=2 (SET), sub_grp=0, op_cnt=2, imm_op1=0, imm_op2=0, set_opt=0 (move)
                let insn: u32 = 0x5000_0000;
                buf.extend_from_slice(&insn.to_be_bytes());
                buf.extend_from_slice(&dst_reg.to_be_bytes());
                buf.extend_from_slice(&src_reg.to_be_bytes());
            }
            InsnSpec::CmpEqReg(dst_reg, src_reg) => {
                // grp=1 (CMP), sub_grp=0, op_cnt=2, imm_op1=0, imm_op2=0, cmp_opt=1 (EQ)
                let insn: u32 = 0x4800_1000;
                buf.extend_from_slice(&insn.to_be_bytes());
                buf.extend_from_slice(&dst_reg.to_be_bytes());
                buf.extend_from_slice(&src_reg.to_be_bytes());
            }
            InsnSpec::Goto(target) => {
                // grp=0 (BRANCH), sub_grp=0 (GOTO), op_cnt=1, imm_op1=1,
                // branch_opt=2 (unconditional in our handler)
                let insn: u32 = 0x2088_0000;
                buf.extend_from_slice(&insn.to_be_bytes());
                buf.extend_from_slice(&target.to_be_bytes());
                buf.extend_from_slice(&0u32.to_be_bytes());
            }
            InsnSpec::GotoIf(target) => {
                // grp=0 (BRANCH), sub_grp=0 (GOTO), op_cnt=1, imm_op1=1,
                // branch_opt=0 (conditional — branch if last CMP true)
                let insn: u32 = 0x2080_0000;
                buf.extend_from_slice(&insn.to_be_bytes());
                buf.extend_from_slice(&target.to_be_bytes());
                buf.extend_from_slice(&0u32.to_be_bytes());
            }
            InsnSpec::Nop => {
                buf.extend_from_slice(&[0u8; 12]);
            }
        }
    }

    // ── Fake buttons for resolver tests ─────────────────────────────

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
        );
        assert_eq!(resolved.len(), 1, "one button resolved");
        assert_eq!(resolved[0].button_id, 7, "button id");
        assert_eq!(resolved[0].playlist, 203, "resolved to playlist 203");
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
        );
        assert_eq!(resolved.len(), 1, "one button resolved");
        assert_eq!(resolved[0].playlist, 205, "resolved to playlist 205");
    }

    #[test]
    fn resolve_skips_button_with_direct_play_pl() {
        let mobj_data = MobjBuilder::new().object(&[InsnSpec::PlayPl(100)]).build();
        let mobj_file = parse(&mobj_data).expect("should parse");

        let button = make_button(1, vec![NavigationCommand::PlayPl { playlist: 100 }]);

        let resolved = resolve_buttons(
            &[(button, PlayerContext::default())],
            &mobj_file,
            &std::collections::HashSet::new(),
            &[],
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
        );
        assert_eq!(resolved.len(), 3, "all three buttons resolved");

        assert_eq!(resolved[0].button_id, 10, "button 10");
        assert_eq!(resolved[0].playlist, 201, "button 10 → playlist 201");

        assert_eq!(resolved[1].button_id, 11, "button 11");
        assert_eq!(resolved[1].playlist, 202, "button 11 → playlist 202");

        assert_eq!(resolved[2].button_id, 12, "button 12");
        assert_eq!(resolved[2].playlist, 203, "button 12 → playlist 203");
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
        );
        assert_eq!(resolved.len(), 1, "resolved via unconditional fallback");
        assert_eq!(resolved[0].playlist, 500, "playlist 500");
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
        );
        assert_eq!(resolved.len(), 1, "one button resolved");
        assert_eq!(resolved[0].playlist, 205, "resolved to playlist 205");
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
        );
        assert_eq!(resolved.len(), 3, "all three buttons resolved");
        assert_eq!(resolved[0].playlist, 201, "button 10 → 201");
        assert_eq!(resolved[1].playlist, 202, "button 11 → 202");
        assert_eq!(resolved[2].playlist, 203, "button 12 → 203");
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
        );
        assert_eq!(resolved.len(), 1, "resolved via MOBJ 1");
        assert_eq!(resolved[0].playlist, 300, "playlist 300");
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
        );
        assert_eq!(resolved.len(), 1, "resolved via register copy");
        assert_eq!(resolved[0].playlist, 203, "playlist 203");
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
        );
        assert_eq!(
            lifecycle_result.len(),
            1,
            "lifecycle simulation resolves without dispatch entries"
        );
        assert_eq!(
            lifecycle_result[0].playlist, 202,
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
        );
        assert_eq!(resolved.len(), 1, "resolved with dispatch entry");
        assert_eq!(resolved[0].playlist, 202, "button key=2 → playlist 202");
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
        );
        assert_eq!(resolved.len(), 3, "all three buttons resolved");
        assert_eq!(resolved[0].playlist, 301, "key 10 → 301");
        assert_eq!(resolved[1].playlist, 302, "key 20 → 302");
        assert_eq!(resolved[2].playlist, 303, "key 30 → 303");
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
        );
        assert_eq!(resolved.len(), 1, "resolved via dispatch entry");
        assert_eq!(resolved[0].playlist, 205, "key 5 → playlist 205");
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
        );
        assert_eq!(resolved.len(), 1, "GotoMobj still works");
        assert_eq!(resolved[0].playlist, 201, "resolved via GotoMobj");
    }
}
