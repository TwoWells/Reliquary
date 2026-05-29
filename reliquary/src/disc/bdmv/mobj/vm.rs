// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Mini-VM execution engine for HDMV bytecode.

use super::super::ig::{Button, NavigationCommand, Page};
use super::parse::command_to_instruction;
use super::{
    BRANCH_GOTO, BRANCH_JUMP, BRANCH_PLAY, GRP_BRANCH, GRP_CMP, GRP_SET, Instruction,
    MovieObjectFile, PSR_FLAG, PlayTarget, PlayerContext, SET_BUTTON_PAGE_OPT, VM_STEP_LIMIT,
};

// ── NOP anchor classification ──────────────────────────────────────────

/// Returns `true` if a button is a NOP anchor — a dispatch target with no
/// real commands.
///
/// NOP anchors have either no commands or all commands are NOP instructions
/// (all-zero 12-byte commands). In the WB `SET_BUTTON_PAGE` pattern, NOP
/// anchors exist solely so the MOBJ VM can read their `button_id` from
/// PSR\[10\] to dispatch to the correct playlist. They are not visible to
/// the user.
#[must_use]
pub fn is_nop_anchor(button: &Button) -> bool {
    button.commands.is_empty()
        || button.commands.iter().all(|c| {
            matches!(c, NavigationCommand::Other { opcode, dst, src }
                if *opcode == 0 && *dst == 0 && *src == 0)
        })
}

/// Finds the visible button on a page that corresponds to a NOP anchor.
///
/// Two strategies, tried in order:
/// 1. **Forward trace:** execute each visible button's commands. If a button
///    produces `SET_BUTTON_PAGE` whose target button ID matches the NOP
///    anchor's `button_id`, it navigates to this anchor.
/// 2. **Neighbor graph walk:** check cursor-navigation neighbor pointers in
///    both directions (NOP → visible and visible → NOP) within 1–2 hops.
///    Deterministic when the disc authoring encodes the relationship.
///
/// Returns `None` if the page has no visible (non-NOP) buttons or no
/// structural connection exists. On WB discs, NOP anchors on a page are
/// dispatch targets from other pages — the visible button that activates
/// them is on the originating page, resolved by the dispatch composite
/// pass in [`resolve_via_execution`](super::resolve::resolve_via_execution).
pub fn find_visible_button_for_nop(
    page: &Page,
    nop_button: &Button,
    ig_stream: u16,
    page_id: u8,
    gprs: &std::collections::HashMap<u32, u32>,
) -> Option<u16> {
    let visible: Vec<&Button> = page.buttons.iter().filter(|b| !is_nop_anchor(b)).collect();

    if visible.is_empty() {
        return None;
    }

    // Forward trace: check if any visible button's SET_BUTTON_PAGE targets
    // this NOP anchor. The target button_id is `composite & 0xFFFF`.
    for vis in &visible {
        let ctx = PlayerContext {
            ig_stream,
            selected_button_id: vis.button_id,
            page_id,
        };
        let (effect, _) = execute_button_commands(&vis.commands, &ctx, gprs);
        if let ButtonEffect::SetButtonPage { composite, .. } = effect {
            #[allow(clippy::cast_possible_truncation, reason = "button IDs are u16 values")]
            let target_bid = (composite & 0xFFFF) as u16;
            if target_bid == nop_button.button_id {
                return Some(vis.button_id);
            }
        }
    }

    // Neighbor graph walk: check structural connections between the NOP
    // anchor and visible buttons via cursor-navigation neighbor pointers.
    // Walks both directions: NOP → visible and visible → NOP.
    let nop_neighbors = [
        nop_button.upper_button_id,
        nop_button.lower_button_id,
        nop_button.left_button_id,
        nop_button.right_button_id,
    ];

    let visible_ids: std::collections::HashSet<u16> = visible.iter().map(|v| v.button_id).collect();

    // Forward 1-hop: NOP anchor's neighbor is a visible button.
    for &nid in &nop_neighbors {
        if nid != nop_button.button_id && visible_ids.contains(&nid) {
            return Some(nid);
        }
    }

    // Reverse 1-hop: a visible button's neighbor is the NOP anchor.
    // On WB discs, visible thumbnails have neighbor fields pointing to
    // co-located NOP anchors, but the NOP anchors may not point back.
    for vis in &visible {
        let vis_neighbors = [
            vis.upper_button_id,
            vis.lower_button_id,
            vis.left_button_id,
            vis.right_button_id,
        ];
        if vis_neighbors.contains(&nop_button.button_id) {
            return Some(vis.button_id);
        }
    }

    // 2-hop: neighbor's neighbor is a visible button (either direction).
    let button_map: std::collections::HashMap<u16, &Button> =
        page.buttons.iter().map(|b| (b.button_id, b)).collect();

    for &nid in &nop_neighbors {
        if nid == nop_button.button_id {
            continue;
        }
        let Some(neighbor) = button_map.get(&nid) else {
            continue;
        };
        for &next_id in &[
            neighbor.upper_button_id,
            neighbor.lower_button_id,
            neighbor.left_button_id,
            neighbor.right_button_id,
        ] {
            if next_id != neighbor.button_id
                && next_id != nop_button.button_id
                && visible_ids.contains(&next_id)
            {
                return Some(next_id);
            }
        }
    }

    None
}

// ── Button effect ──────────────────────────────────────────────────────

/// The terminal effect of executing a button's command program.
///
/// Button commands are HDMV bytecode that modify registers and eventually
/// reach a terminal instruction. This enum captures what the program does
/// without prescribing how to interpret it.
#[derive(Debug)]
pub enum ButtonEffect {
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

// ── Execution engine ───────────────────────────────────────────────────

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
    // Use a sentinel playlist set to reject all PlayPl instructions.
    // With an empty set, `valid_playlists.is_empty()` is true and any
    // non-zero playlist terminates execution — potentially before all
    // GPR[3xxx] config registers are written. A non-empty set that
    // matches nothing forces PlayPl to be skipped, allowing the entire
    // init block to execute.
    let reject_all_playlists = std::collections::HashSet::from([u32::MAX]);
    let _ = run_mobj_vm(&mobj0.instructions, 0, &mut gprs, &reject_all_playlists);
    // Keep only GPR entries (no PSR) — the disc configuration database
    // that title MOBJs and button programs read.
    gprs.into_iter()
        .filter(|(k, _)| k & PSR_FLAG == 0)
        .collect()
}

/// Executes a button's navigation commands and returns the terminal effect
/// along with the register state at the point of termination.
///
/// Converts all [`NavigationCommand`]s to [`Instruction`]s and runs them
/// through the mini-VM. Recognizes three terminal actions: `PlayPl`,
/// `GotoMobj` (BRANCH\_JUMP), and `SET_BUTTON_PAGE` (SETSYSTEM set\_opt=3).
pub fn execute_button_commands(
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

/// Executes MOBJ instructions starting at `start_pc` with the given
/// register state and player context.
pub fn execute_from(
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
pub fn fetch_operand(
    is_immediate: bool,
    raw: u32,
    gprs: &std::collections::HashMap<u32, u32>,
) -> u32 {
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
pub fn execute_set(insn: &Instruction, gprs: &mut std::collections::HashMap<u32, u32>) {
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
pub fn execute_cmp(insn: &Instruction, gprs: &std::collections::HashMap<u32, u32>) -> bool {
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
pub fn execute_goto(
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

// ── Lifecycle simulation ──────────────────────────────────────────────

/// Traces all valid `PlayPl` playlists encountered during lifecycle
/// simulation of an MOBJ.
///
/// On every `PlayPl`, the provided GPR assignments are re-applied
/// (simulating button activation during a suspended playlist) and
/// execution continues. Returns the ordered list of valid playlist
/// numbers encountered.
pub fn trace_play_pls(
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
pub fn collect_cmp_play_pairs(instrs: &[Instruction], register: u32) -> Vec<(u32, u16)> {
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
    clippy::panic,
    reason = "tests use expect() and panic!() for assertions per project rules"
)]
mod tests {
    use super::super::NavClipInput;
    use super::super::test_helpers::{
        InsnSpec, MobjBuilder, make_button, make_button_at, spec_to_other,
    };
    use super::*;

    // ── NOP detection tests ─────────────────────────────────────────

    #[test]
    fn nop_detection_empty_commands() {
        let button = make_button(10, vec![]);
        assert!(is_nop_anchor(&button), "empty commands → NOP anchor");
    }

    #[test]
    fn nop_detection_all_nop_instructions() {
        let button = make_button(10, vec![spec_to_other(&InsnSpec::Nop)]);
        assert!(
            is_nop_anchor(&button),
            "single NOP instruction → NOP anchor"
        );

        let button2 = make_button(
            10,
            vec![spec_to_other(&InsnSpec::Nop), spec_to_other(&InsnSpec::Nop)],
        );
        assert!(
            is_nop_anchor(&button2),
            "multiple NOP instructions → NOP anchor"
        );
    }

    #[test]
    fn nop_detection_real_commands_not_nop() {
        let button = make_button(
            10,
            vec![NavigationCommand::SetGpr {
                register: 100,
                value: 42,
            }],
        );
        assert!(!is_nop_anchor(&button), "SetGpr command → not NOP");

        let button2 = make_button(10, vec![NavigationCommand::GotoMobj { object_id: 1 }]);
        assert!(!is_nop_anchor(&button2), "GotoMobj command → not NOP");

        let button3 = make_button(
            10,
            vec![NavigationCommand::PlayPl {
                playlist: 200,
                branch_opt: 0,
                mark_or_pi: 0,
            }],
        );
        assert!(!is_nop_anchor(&button3), "PlayPl command → not NOP");
    }

    // ── Visible button resolution tests ───────────────────────────────

    #[test]
    fn visible_button_forward_trace() {
        // Visible button's SET_BUTTON_PAGE directly targets the NOP
        // anchor's button_id. Forward trace matches without spatial
        // fallback.
        //
        // Visible button computes composite = button_id 19 via register.
        // SetGpr(50, 19) + SetButtonPage(50, 51) → composite = 19.
        let visible = make_button_at(
            5,
            229,
            668,
            vec![
                NavigationCommand::SetGpr {
                    register: 50,
                    value: 19,
                },
                NavigationCommand::SetGpr {
                    register: 51,
                    value: 0,
                },
                spec_to_other(&InsnSpec::SetButtonPage(50, 51)),
            ],
        );
        let page = super::super::test_helpers::make_page(
            0,
            vec![make_button_at(19, 1339, 949, vec![]), visible],
        );
        let nop_ref = make_button_at(19, 1339, 949, vec![]);

        let gprs = std::collections::HashMap::new();
        let result = find_visible_button_for_nop(&page, &nop_ref, 0x1200, 0, &gprs);
        assert_eq!(
            result,
            Some(5),
            "forward trace finds visible btn[5] targeting NOP btn[19]"
        );
    }

    #[test]
    fn visible_button_neighbor_1hop() {
        use super::super::test_helpers::make_button_with_neighbors;

        // NOP anchor btn[21] has btn[5] as its upper neighbor.
        // btn[5] is a visible button — should be found in 1 hop.
        let nop = make_button_with_neighbors(
            21,
            199,
            668,
            [5, 0, 0, 0], // upper=5
            vec![],
        );
        let thumb = make_button_at(
            5,
            229,
            668,
            vec![NavigationCommand::SetGpr {
                register: 50,
                value: 0,
            }],
        );

        let page = super::super::test_helpers::make_page(0, vec![nop.clone(), thumb]);
        let gprs = std::collections::HashMap::new();
        let result = find_visible_button_for_nop(&page, &nop, 0x1200, 0, &gprs);
        assert_eq!(result, Some(5), "1-hop neighbor walk finds visible btn[5]");
    }

    #[test]
    fn visible_button_neighbor_2hop() {
        use super::super::test_helpers::make_button_with_neighbors;

        // NOP anchor btn[21] → neighbor btn[22] (another NOP) → neighbor btn[5] (visible).
        // 2-hop walk required.
        let nop = make_button_with_neighbors(
            21,
            0,
            0,
            [22, 0, 0, 0], // upper=22
            vec![],
        );
        let intermediate = make_button_with_neighbors(
            22,
            0,
            0,
            [5, 0, 0, 0], // upper=5
            vec![],
        );
        let thumb = make_button_at(
            5,
            229,
            668,
            vec![NavigationCommand::SetGpr {
                register: 50,
                value: 0,
            }],
        );

        let page = super::super::test_helpers::make_page(0, vec![nop.clone(), intermediate, thumb]);
        let gprs = std::collections::HashMap::new();
        let result = find_visible_button_for_nop(&page, &nop, 0x1200, 0, &gprs);
        assert_eq!(result, Some(5), "2-hop neighbor walk finds visible btn[5]");
    }

    #[test]
    fn visible_button_no_neighbor_link_returns_none() {
        use super::super::test_helpers::make_button_with_neighbors;

        // NOP anchor btn[21] has no neighbor links to any visible button.
        // On a copy-paste page, the neighbor pointers reference non-existent
        // buttons — should return None to thin the candidate pool.
        let nop = make_button_with_neighbors(
            21,
            0,
            0,
            [99, 98, 97, 96], // all neighbors reference buttons not on this page
            vec![],
        );
        let thumb = make_button_at(
            5,
            229,
            668,
            vec![NavigationCommand::SetGpr {
                register: 50,
                value: 0,
            }],
        );

        let page = super::super::test_helpers::make_page(0, vec![nop.clone(), thumb]);
        let gprs = std::collections::HashMap::new();
        let result = find_visible_button_for_nop(&page, &nop, 0x1200, 0, &gprs);
        assert_eq!(result, None, "no neighbor link to visible button → None");
    }

    #[test]
    fn visible_button_no_visible_returns_none() {
        // Page with only NOP anchors — no visible buttons to map to.
        let page = super::super::test_helpers::make_page(
            0,
            vec![
                make_button(12, vec![]),
                make_button(15, vec![spec_to_other(&InsnSpec::Nop)]),
            ],
        );
        let nop_ref = make_button(12, vec![]);

        let gprs = std::collections::HashMap::new();
        let result = find_visible_button_for_nop(&page, &nop_ref, 0x1200, 0, &gprs);
        assert_eq!(result, None, "no visible buttons → None");
    }

    #[test]
    fn visible_button_breadcrumb_in_resolver() {
        use super::super::resolve::extract_dispatch_table;
        use super::super::resolve::resolve_via_execution;
        use super::super::test_helpers::{build_dispatch_mobj, make_button_with_neighbors};

        // End-to-end: NOP anchor on a page with a neighbor-linked visible
        // thumbnail. The resolver's breadcrumb should record the visible
        // button, not the NOP anchor.
        let nop = make_button_with_neighbors(21, 199, 668, [5, 0, 0, 0], vec![]);
        let thumb = make_button_at(
            5,
            229,
            668,
            vec![
                NavigationCommand::SetGpr {
                    register: 50,
                    value: 0,
                },
                NavigationCommand::SetGpr {
                    register: 51,
                    value: 26,
                },
                spec_to_other(&InsnSpec::SetButtonPage(50, 51)),
            ],
        );
        let page = super::super::test_helpers::make_page(0, vec![nop, thumb]);

        let dispatch_mobj = build_dispatch_mobj(&[(21, 209), (23, 210), (25, 211)]);
        let mobj_data = MobjBuilder::new().object(&dispatch_mobj).build();
        let mobj_file = super::super::parse::parse(&mobj_data).expect("should parse");
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

        assert_eq!(resolved.len(), 1, "one NOP anchor resolved");
        assert_eq!(resolved[0].target.playlist, 209, "NOP btn[21] → PL 209");
        assert_eq!(
            super::super::test_helpers::breadcrumb_ids(&resolved[0].breadcrumb),
            vec![5],
            "breadcrumb records visible btn[5], not NOP btn[21]"
        );
    }

    // ── Register-computed SET_BUTTON_PAGE tests ──────────────────────

    #[test]
    fn register_computed_set_button_page() {
        // WB Special Features button command program (a WB Blu-ray title btn[4]):
        //   1. GPR[4075] = 5              (nav bar offset, immediate)
        //   2. GPR[4076] = 0xFFFF          (mask, immediate)
        //   3. GPR[4077] = PSR[10]         (selected button ID, reg-to-reg)
        //   4. GPR[4077] &= GPR[4076]      (mask to 16 bits)
        //   5. GPR[4077] += GPR[4075]      (offset → composite dispatch key)
        //   6. GPR[4075] = GPR[3051]        (page from config DB, reg-to-reg)
        //   7. SET_BUTTON_PAGE(GPR[4077], GPR[4075])
        //   8. NOP
        //
        // With btn[4] selected (PSR[10]=4) and GPR[3051]=6:
        //   composite = (4 & 0xFFFF) + 5 = 9
        //   page = GPR[3051] = 6
        let commands = vec![
            NavigationCommand::SetGpr {
                register: 4075,
                value: 5,
            },
            NavigationCommand::SetGpr {
                register: 4076,
                value: 0xFFFF,
            },
            spec_to_other(&InsnSpec::SetGprReg(4077, super::PSR_FLAG | 0x0A)),
            spec_to_other(&InsnSpec::AndReg(4077, 4076)),
            spec_to_other(&InsnSpec::AddReg(4077, 4075)),
            spec_to_other(&InsnSpec::SetGprReg(4075, 3051)),
            spec_to_other(&InsnSpec::SetButtonPage(4077, 4075)),
            spec_to_other(&InsnSpec::Nop),
        ];

        let ctx = PlayerContext {
            ig_stream: 0x1200,
            selected_button_id: 4,
            page_id: 1,
        };

        // GPR[3051] = 6 from MOBJ[0] init (page config for special features)
        let mut gprs = std::collections::HashMap::new();
        gprs.insert(3051_u32, 6_u32);

        let (effect, _) = execute_button_commands(&commands, &ctx, &gprs);

        match effect {
            ButtonEffect::SetButtonPage { composite, page } => {
                assert_eq!(composite, 9, "composite = PSR[10](4) & 0xFFFF + 5 = 9");
                assert_eq!(page, 6, "page = GPR[3051] = 6");
            }
            ref other => panic!("expected SetButtonPage, got {other:?}"),
        }
    }

    #[test]
    fn register_computed_set_button_page_without_seed() {
        // Same command program but GPR[3051] is NOT seeded.
        // GPR[3051] defaults to 0, so page = 0 instead of 6.
        // This models the failure when seed_gpr_state doesn't
        // initialize the config register.
        let commands = vec![
            NavigationCommand::SetGpr {
                register: 4075,
                value: 5,
            },
            NavigationCommand::SetGpr {
                register: 4076,
                value: 0xFFFF,
            },
            spec_to_other(&InsnSpec::SetGprReg(4077, super::PSR_FLAG | 0x0A)),
            spec_to_other(&InsnSpec::AndReg(4077, 4076)),
            spec_to_other(&InsnSpec::AddReg(4077, 4075)),
            spec_to_other(&InsnSpec::SetGprReg(4075, 3051)),
            spec_to_other(&InsnSpec::SetButtonPage(4077, 4075)),
            spec_to_other(&InsnSpec::Nop),
        ];

        let ctx = PlayerContext {
            ig_stream: 0x1200,
            selected_button_id: 4,
            page_id: 1,
        };

        let gprs = std::collections::HashMap::new(); // no seed

        let (effect, _) = execute_button_commands(&commands, &ctx, &gprs);

        match effect {
            ButtonEffect::SetButtonPage { composite, page } => {
                assert_eq!(
                    composite, 9,
                    "composite still computed correctly from PSR[10]"
                );
                assert_eq!(
                    page, 0,
                    "page = 0 when GPR[3051] uninitialized (wrong destination)"
                );
            }
            ref other => panic!("expected SetButtonPage, got {other:?}"),
        }
    }

    // ── seed_gpr_state tests ─────────────────────────────────────────

    #[test]
    fn seed_gpr_captures_registers_after_play_pl() {
        // MOBJ[0] writes GPR[3051] = 6 AFTER a PlayPl instruction.
        // Before the fix, PlayPl terminated execution and GPR[3051]
        // was never captured. After the fix, PlayPl is skipped
        // during seeding, so the entire init block executes.
        let mobj_data = MobjBuilder::new()
            .object(&[
                // Init guard: CMP PSR[4] != 0xFFFF → skip init
                InsnSpec::Nop, // simplified: no guard in test
                // GPR database — writes before and after PlayPl
                InsnSpec::SetGpr(3000, 42),
                InsnSpec::SetGpr(3001, 99),
                // Menu start (real discs play the menu playlist here)
                InsnSpec::PlayPl(100),
                // More GPR writes AFTER the PlayPl
                InsnSpec::SetGpr(3051, 6),
                InsnSpec::SetGpr(3052, 7),
            ])
            .build();

        let mobj_file = super::super::parse::parse(&mobj_data).expect("should parse");
        let gprs = seed_gpr_state(&mobj_file);

        assert_eq!(
            gprs.get(&3000).copied(),
            Some(42),
            "GPR[3000] captured before PlayPl"
        );
        assert_eq!(
            gprs.get(&3051).copied(),
            Some(6),
            "GPR[3051] captured AFTER PlayPl (the fix)"
        );
        assert_eq!(
            gprs.get(&3052).copied(),
            Some(7),
            "GPR[3052] captured after PlayPl"
        );
    }
}
