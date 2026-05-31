// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! `MovieObject.bdmv` parser and button→playlist resolver.
//!
//! Parses the HDMV navigation programs and traces register-based button
//! commands through movie objects to resolve indirect playlist mappings.
//!
//! Reference: libbluray `src/libbluray/bdnav/mobj_parse.c`,
//! `src/libbluray/decoders/hdmv_insn.h`.

mod parse;
mod resolve;
mod vm;

pub use parse::{command_to_instruction, format_instruction, parse};
pub use resolve::{
    extract_dispatch_table, find_dispatch_entries, find_handler_pc, resolve_buttons,
    resolve_via_execution,
};
pub use vm::{ButtonEffect, execute_button_commands, is_nop_anchor, run_mobj_vm, seed_gpr_state};

use thiserror::Error;

use super::cursor::CursorError;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// A single step in a navigation breadcrumb.
///
/// Identifies a specific button on a specific page within a specific clip,
/// so the CLI can look up the correct bitmap (not a same-ID button from a
/// different clip or page).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BreadcrumbStep {
    /// Index into the `clips` slice passed to [`resolve_via_execution`].
    pub clip_index: usize,
    /// Page ID within the clip.
    pub page_id: u8,
    /// Button ID within the page.
    pub button_id: u16,
}

/// A resolved playlist with its navigation breadcrumb.
///
/// Produced by [`resolve_via_execution`]. The breadcrumb is the ordered
/// sequence of buttons pressed from the root menu page to the content
/// button. The last element is the button that plays the playlist; earlier
/// elements are navigation buttons pressed to reach it.
#[derive(Debug, PartialEq, Eq)]
pub struct ResolvedPlaylist {
    /// Ordered navigation steps from root to leaf.
    /// The last element is the content button that plays the playlist.
    /// Earlier elements are navigation buttons pressed to reach it.
    pub breadcrumb: Vec<BreadcrumbStep>,
    /// `true` when the content lives on a page not reachable from the
    /// root menu via navigation. Orphans are still valid content (they
    /// have bitmaps and playlists) but cannot be navigated to from the
    /// main menu. The breadcrumb contains only the content button.
    pub orphan: bool,
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

/// A clip's page structure for execution-based resolution.
pub struct NavClipInput<'a> {
    /// IG stream PID (for PSR\[0\] seeding).
    pub ig_pid: u16,
    /// All pages across display sets in this clip.
    pub pages: Vec<&'a super::ig::Page>,
}

// ── Instruction group constants ─────────────────────────────────────────

/// BRANCH group — goto, jump to MOBJ, play playlist.
pub(crate) const GRP_BRANCH: u8 = 0;
/// CMP group — compare and conditionally branch.
pub(crate) const GRP_CMP: u8 = 1;
/// SET group — register operations.
pub(crate) const GRP_SET: u8 = 2;

/// BRANCH sub-groups.
pub(crate) const BRANCH_GOTO: u8 = 0;
pub(crate) const BRANCH_JUMP: u8 = 1;
pub(crate) const BRANCH_PLAY: u8 = 2;

/// `SET_BUTTON_PAGE` operation within the SETSYSTEM sub-group (`sub_group=1`).
///
/// Terminates IG execution and navigates to a new button/page. The
/// player runtime packs `button_id = dst & 0xFFFF` and selects that
/// button on the target page, updating `PSR[10]`. The dispatch MOBJ
/// reads `PSR[10]` into its switch register on resume.
pub(crate) const SET_BUTTON_PAGE_OPT: u8 = 3;

/// Maximum instructions the mini-VM will execute before giving up.
/// Prevents infinite loops in malformed or exotic MOBJ bytecode.
/// Set high enough to cover WB First Play MOBJs (up to ~3000
/// instructions with ~2 steps per CMP/GOTO pair ≈ 6000 effective
/// steps for the disc configuration database initialization).
pub(crate) const VM_STEP_LIMIT: u32 = 10_000;

/// PSR (Player Status Register) bit flag. Register references with
/// bit 31 set address PSRs rather than GPRs.
pub(crate) const PSR_FLAG: u32 = 0x8000_0000;

// ── Test helpers ────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod test_helpers {
    use super::super::ig::{Button, NavigationCommand, Page};

    pub fn make_button(button_id: u16, commands: Vec<NavigationCommand>) -> Button {
        Button {
            button_id,
            x: 0,
            y: 0,
            upper_button_id: 0,
            lower_button_id: 0,
            left_button_id: 0,
            right_button_id: 0,
            normal_object_id: 0,
            selected_object_id: 0,
            commands,
            bog_id: 0,
        }
    }

    pub fn make_button_at(
        button_id: u16,
        x: u16,
        y: u16,
        commands: Vec<NavigationCommand>,
    ) -> Button {
        Button {
            button_id,
            x,
            y,
            upper_button_id: 0,
            lower_button_id: 0,
            left_button_id: 0,
            right_button_id: 0,
            normal_object_id: 0,
            selected_object_id: 0,
            commands,
            bog_id: 0,
        }
    }

    /// Creates a button with explicit neighbor navigation fields.
    pub fn make_button_with_neighbors(
        button_id: u16,
        x: u16,
        y: u16,
        neighbors: [u16; 4],
        commands: Vec<NavigationCommand>,
    ) -> Button {
        Button {
            button_id,
            x,
            y,
            upper_button_id: neighbors[0],
            lower_button_id: neighbors[1],
            left_button_id: neighbors[2],
            right_button_id: neighbors[3],
            normal_object_id: 0,
            selected_object_id: 0,
            commands,
            bog_id: 0,
        }
    }

    pub fn make_page(page_id: u8, buttons: Vec<Button>) -> Page {
        Page { page_id, buttons }
    }

    /// Extracts button IDs from a breadcrumb for concise assertions.
    pub fn breadcrumb_ids(crumb: &[super::BreadcrumbStep]) -> Vec<u16> {
        crumb.iter().map(|s| s.button_id).collect()
    }

    /// Builds a synthetic `MovieObject.bdmv` binary for testing.
    pub struct MobjBuilder {
        objects: Vec<Vec<InsnSpec>>,
    }

    /// Specification for a test instruction.
    #[derive(Clone)]
    pub enum InsnSpec {
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
        pub fn new() -> Self {
            Self {
                objects: Vec::new(),
            }
        }

        pub fn object(mut self, instructions: &[InsnSpec]) -> Self {
            self.objects.push(instructions.to_vec());
            self
        }

        pub fn build(self) -> Vec<u8> {
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

    pub fn build_instruction(buf: &mut Vec<u8>, spec: &InsnSpec) {
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

    /// Converts an `InsnSpec` to raw `(opcode, dst, src)` for building
    /// `NavigationCommand::Other` from test instruction specs.
    pub fn spec_to_raw(spec: &InsnSpec) -> (u32, u32, u32) {
        let mut buf = Vec::new();
        build_instruction(&mut buf, spec);
        let opcode = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let dst = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let src = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
        (opcode, dst, src)
    }

    /// Builds a `NavigationCommand::Other` from an `InsnSpec`.
    pub fn spec_to_other(spec: &InsnSpec) -> NavigationCommand {
        let (opcode, dst, src) = spec_to_raw(spec);
        NavigationCommand::Other { opcode, dst, src }
    }

    /// Builds a synthetic dispatch MOBJ with the CMP/GOTO switch pattern.
    ///
    /// Structure:
    /// - Instructions 0..cases*4: switch table (4 instructions per case)
    /// - Instructions cases*4..: handler blocks (SET + `PlayPl` + GOTO per case)
    pub fn build_dispatch_mobj(cases: &[(u32, u16)]) -> Vec<InsnSpec> {
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
}
