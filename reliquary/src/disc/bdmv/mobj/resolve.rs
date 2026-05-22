// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! BFS resolver, dispatch table extraction, and legacy pattern-matching resolver.

use super::super::ig::{Button, NavigationCommand, Page};
use super::vm::{
    ButtonEffect, collect_cmp_play_pairs, execute_button_commands, execute_from,
    find_visible_button_for_nop, is_nop_anchor, run_mobj_vm, seed_gpr_state, trace_play_pls,
};
use super::{
    BRANCH_GOTO, BRANCH_PLAY, BreadcrumbStep, DispatchEntry, DispatchTable, GRP_BRANCH, GRP_CMP,
    GRP_SET, Instruction, MovieObject, MovieObjectFile, NavClipInput, PSR_FLAG, PlayTarget,
    PlayerContext, ResolvedButton, ResolvedPlaylist,
};

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

/// Resolves playlists reachable from disc menus via BFS navigation.
///
/// Builds a navigation graph via BFS over all (clip, page) nodes.
/// At each node, every button's command program is executed through the
/// mini-VM with the current GPR state:
///
/// - `PlayPl` → terminal (direct playlist resolution with breadcrumb)
/// - `GotoMobj` → follows the target MOBJ to reach `PlayPl` (terminal)
/// - `SET_BUTTON_PAGE` → navigation edge to the target page. GPR state
///   and the breadcrumb are propagated along navigation edges so that
///   content buttons on downstream pages execute with the register
///   values set by upstream navigation buttons.
///
/// Each resolved playlist carries a breadcrumb: the ordered sequence of
/// button IDs pressed to reach it from the root menu. The first arrival
/// per playlist is kept (BFS guarantees shortest path).
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
) -> Vec<ResolvedPlaylist> {
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
    // Per-clip dedup: the same playlist from different clips is NOT a
    // duplicate — it's the same content accessible from different menus.
    // Each menu provides different visual context (e.g. scene selection
    // thumbnails vs. special features thumbnails). The CLI decides which
    // clip's resolution to present.
    let mut resolved_set = std::collections::HashSet::<(usize, PlayTarget)>::new();

    // Page index: (clip_index, page_id) → (page ref, ig_pid)
    let mut page_lookup = std::collections::HashMap::<(usize, u8), (&Page, u16)>::new();
    for (clip_idx, clip) in clips.iter().enumerate() {
        for page in &clip.pages {
            page_lookup.insert((clip_idx, page.page_id), (page, clip.ig_pid));
        }
    }

    // BFS queue: (clip_index, page_id, gprs, breadcrumb)
    // The breadcrumb is the ordered sequence of buttons pressed to
    // reach this node from the root menu, with clip/page context.
    let mut queue: VecDeque<(usize, u8, GprState, Vec<BreadcrumbStep>)> = VecDeque::new();

    // Visited: (clip_index, page_id) → set of GPR state hashes processed.
    let mut visited =
        std::collections::HashMap::<(usize, u8), std::collections::HashSet<u64>>::new();

    // SetButtonPage composites seen during BFS: composite → first breadcrumb.
    // Used as a fallback to resolve dispatch cases that have no NOP anchor.
    // Only the first occurrence per composite is kept (BFS shortest-path).
    let mut nav_composites =
        std::collections::HashMap::<u32, (usize, u8, u16, Vec<BreadcrumbStep>)>::new();

    // Execute MOBJ[0] (First Play) to collect GPR[3xxx] configuration
    // state. WB authoring stores per-content-item configuration in a
    // GPR database (registers 3000–3999) initialized by MOBJ[0]. The
    // complex button programs read from this database to compute
    // dispatch values. Without it, buttons follow default branches.
    let init_gprs: GprState = seed_gpr_state(mobj_file);

    // Seed BFS with root pages only (smallest page_id per clip).
    // Sub-pages are discovered via navigation edges, which gives them
    // proper breadcrumbs. Seeding all pages would give sub-page content
    // single-element breadcrumbs, losing navigation context.
    let init_hash = hash_gpr_state(&init_gprs);
    for (clip_idx, clip) in clips.iter().enumerate() {
        if let Some(root_page) = clip.pages.iter().min_by_key(|p| p.page_id) {
            visited
                .entry((clip_idx, root_page.page_id))
                .or_default()
                .insert(init_hash);
            queue.push_back((clip_idx, root_page.page_id, init_gprs.clone(), Vec::new()));
        }
    }

    let mut iterations: u32 = 0;

    while let Some((clip_idx, page_id, gprs, breadcrumb)) = queue.pop_front() {
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
            // The breadcrumb records the corresponding visible button
            // (the one with the selected-state bitmap showing the
            // content label), not the NOP anchor itself. This ensures
            // the CLI highlights the correct thumbnail/label position.
            if is_nop_anchor(button) {
                if let Some(table) = dispatch_table {
                    let bid = u32::from(button.button_id);
                    if let Some(&(_, pl)) = table.cases.iter().find(|(cv, _)| *cv == bid) {
                        let target = PlayTarget {
                            playlist: pl,
                            branch_opt: 0,
                            mark_or_pi: 0,
                        };
                        if resolved_set.insert((clip_idx, target)) {
                            let visible_bid =
                                find_visible_button_for_nop(page, button, ig_pid, page_id, &gprs)
                                    .unwrap_or(button.button_id);
                            let mut crumb = breadcrumb.clone();
                            crumb.push(BreadcrumbStep {
                                clip_index: clip_idx,
                                page_id,
                                button_id: visible_bid,
                            });
                            resolved.push(ResolvedPlaylist {
                                breadcrumb: crumb,
                                orphan: false,
                                target,
                            });
                        }
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
                    let target = PlayTarget {
                        playlist: pl,
                        branch_opt: bo,
                        mark_or_pi: mpi,
                    };
                    let is_valid = pl != 0
                        && pl != 0xFFFF
                        && (valid_playlists.is_empty() || valid_playlists.contains(&u32::from(pl)));
                    if is_valid && resolved_set.insert((clip_idx, target)) {
                        let mut crumb = breadcrumb.clone();
                        crumb.push(BreadcrumbStep {
                            clip_index: clip_idx,
                            page_id,
                            button_id: button.button_id,
                        });
                        resolved.push(ResolvedPlaylist {
                            breadcrumb: crumb,
                            orphan: false,
                            target,
                        });
                    }
                }
                ButtonEffect::GotoMobj(object_id) => {
                    if let Some(mobj) = mobj_file.objects.get(object_id as usize) {
                        let mut mobj_gprs = new_gprs;
                        if let Some(target) =
                            run_mobj_vm(&mobj.instructions, 0, &mut mobj_gprs, valid_playlists)
                            && resolved_set.insert((clip_idx, target))
                        {
                            let mut crumb = breadcrumb.clone();
                            crumb.push(BreadcrumbStep {
                                clip_index: clip_idx,
                                page_id,
                                button_id: button.button_id,
                            });
                            resolved.push(ResolvedPlaylist {
                                breadcrumb: crumb,
                                orphan: false,
                                target,
                            });
                        }
                    }
                }
                ButtonEffect::SetButtonPage {
                    composite,
                    page: target_page,
                } => {
                    // SET_BUTTON_PAGE is navigation — it doesn't play
                    // anything. The BFS follows the edge; content buttons
                    // on the target page resolve themselves.

                    // Record the composite for fallback dispatch resolution.
                    // Only the first occurrence per composite is kept (BFS
                    // guarantees shortest path). Used after the BFS to
                    // resolve dispatch cases that have no NOP anchor.
                    nav_composites.entry(composite).or_insert_with(|| {
                        let mut crumb = breadcrumb.clone();
                        crumb.push(BreadcrumbStep {
                            clip_index: clip_idx,
                            page_id,
                            button_id: button.button_id,
                        });
                        (clip_idx, page_id, button.button_id, crumb)
                    });

                    // Navigation edge: propagate button GPR state and
                    // breadcrumb to the target page. Pushed BEFORE handler
                    // propagation so the BFS visits the target page with
                    // the correct breadcrumb first. Includes same-page
                    // edges (WB authoring: navigation buttons set GPR
                    // state and loop back to the current page).
                    #[allow(clippy::cast_possible_truncation, reason = "page IDs fit in u8")]
                    let target_page_id = (target_page & 0xFF) as u8;
                    if page_lookup.contains_key(&(clip_idx, target_page_id)) {
                        let propagated: GprState = new_gprs
                            .into_iter()
                            .filter(|(k, _)| k & PSR_FLAG == 0)
                            .collect();

                        let mut nav_crumb = breadcrumb.clone();
                        nav_crumb.push(BreadcrumbStep {
                            clip_index: clip_idx,
                            page_id,
                            button_id: button.button_id,
                        });

                        let key = hash_gpr_state(&propagated);
                        let states = visited.entry((clip_idx, target_page_id)).or_default();
                        if states.insert(key) {
                            queue.push_back((clip_idx, target_page_id, propagated, nav_crumb));
                        }
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

                        // Propagate handler state to already-visited pages
                        // in this clip. Not pushed to unvisited pages to
                        // avoid preempting navigation breadcrumbs.
                        let handler_state: GprState = handler_gprs
                            .into_iter()
                            .filter(|(k, _)| k & PSR_FLAG == 0)
                            .collect();

                        let key = hash_gpr_state(&handler_state);
                        for &(ci, pid) in page_lookup.keys() {
                            if ci == clip_idx
                                && let Some(states) = visited.get_mut(&(ci, pid))
                                && states.insert(key)
                            {
                                queue.push_back((ci, pid, handler_state.clone(), Vec::new()));
                            }
                        }
                    }
                }
                ButtonEffect::None => {}
            }
        }
    }

    // Orphan sweep: resolve content on pages never reached via navigation.
    // These are valid content items (they have bitmaps and playlists) but
    // the user cannot navigate to them from the main menu.
    for (clip_idx, clip) in clips.iter().enumerate() {
        for page in &clip.pages {
            if visited.contains_key(&(clip_idx, page.page_id)) {
                continue;
            }

            let Some(&(_, ig_pid)) = page_lookup.get(&(clip_idx, page.page_id)) else {
                continue;
            };

            for button in &page.buttons {
                // NOP anchor orphans — same visible-button resolution
                // as the main BFS, but with orphan=true.
                if is_nop_anchor(button) {
                    if let Some(table) = dispatch_table {
                        let bid = u32::from(button.button_id);
                        if let Some(&(_, pl)) = table.cases.iter().find(|(cv, _)| *cv == bid) {
                            let target = PlayTarget {
                                playlist: pl,
                                branch_opt: 0,
                                mark_or_pi: 0,
                            };
                            if resolved_set.insert((clip_idx, target)) {
                                let visible_bid = find_visible_button_for_nop(
                                    page,
                                    button,
                                    ig_pid,
                                    page.page_id,
                                    &init_gprs,
                                )
                                .unwrap_or(button.button_id);
                                resolved.push(ResolvedPlaylist {
                                    breadcrumb: vec![BreadcrumbStep {
                                        clip_index: clip_idx,
                                        page_id: page.page_id,
                                        button_id: visible_bid,
                                    }],
                                    orphan: true,
                                    target,
                                });
                            }
                        }
                    }
                    continue;
                }

                // Skip direct PlayPl buttons (handled by IG parser)
                let has_direct_play_pl = button
                    .commands
                    .iter()
                    .any(|c| matches!(c, NavigationCommand::PlayPl { .. }));
                if has_direct_play_pl {
                    continue;
                }

                // Execute orphan button commands with initial GPR state
                let ctx = PlayerContext {
                    ig_stream: ig_pid,
                    selected_button_id: button.button_id,
                    page_id: page.page_id,
                };
                let (effect, new_gprs) =
                    execute_button_commands(&button.commands, &ctx, &init_gprs);

                match effect {
                    ButtonEffect::Playlist {
                        playlist: pl,
                        branch_opt: bo,
                        mark_or_pi: mpi,
                    } => {
                        let target = PlayTarget {
                            playlist: pl,
                            branch_opt: bo,
                            mark_or_pi: mpi,
                        };
                        let is_valid = pl != 0
                            && pl != 0xFFFF
                            && (valid_playlists.is_empty()
                                || valid_playlists.contains(&u32::from(pl)));
                        if is_valid && resolved_set.insert((clip_idx, target)) {
                            resolved.push(ResolvedPlaylist {
                                breadcrumb: vec![BreadcrumbStep {
                                    clip_index: clip_idx,
                                    page_id: page.page_id,
                                    button_id: button.button_id,
                                }],
                                orphan: true,
                                target,
                            });
                        }
                    }
                    ButtonEffect::GotoMobj(object_id) => {
                        if let Some(mobj) = mobj_file.objects.get(object_id as usize) {
                            let mut mobj_gprs = new_gprs;
                            if let Some(target) =
                                run_mobj_vm(&mobj.instructions, 0, &mut mobj_gprs, valid_playlists)
                                && resolved_set.insert((clip_idx, target))
                            {
                                resolved.push(ResolvedPlaylist {
                                    breadcrumb: vec![BreadcrumbStep {
                                        clip_index: clip_idx,
                                        page_id: page.page_id,
                                        button_id: button.button_id,
                                    }],
                                    orphan: true,
                                    target,
                                });
                            }
                        }
                    }
                    ButtonEffect::SetButtonPage { .. } | ButtonEffect::None => {}
                }
            }
        }
    }

    // Dispatch composite pass: resolve dispatch cases from SetButtonPage
    // composites observed during the BFS. This covers two scenarios:
    //
    // 1. **New resolutions**: dispatch cases with no NOP anchor (e.g. WB
    //    "PLAY MOVIE" / "SCENE SELECTION" buttons whose SET_BUTTON_PAGE
    //    composite IS the dispatch case value).
    //
    // 2. **Shorter breadcrumbs**: when the BFS resolved a playlist via a
    //    deep NOP anchor path (e.g. Special Features → sub-page → btn[9])
    //    but a top-level navigation button also produces the same dispatch
    //    case via its composite. The shorter breadcrumb is more natural —
    //    it matches what the user sees on the main menu.
    if let Some(table) = dispatch_table {
        for &(case_val, pl) in &table.cases {
            if let Some((_, _, _, crumb)) = nav_composites.get(&case_val) {
                let target = PlayTarget {
                    playlist: pl,
                    branch_opt: 0,
                    mark_or_pi: 0,
                };
                let is_valid = pl != 0
                    && pl != 0xFFFF
                    && (valid_playlists.is_empty() || valid_playlists.contains(&u32::from(pl)));
                if !is_valid {
                    continue;
                }

                let crumb_clip = crumb.last().map_or(0, |s| s.clip_index);
                if resolved_set.insert((crumb_clip, target)) {
                    // New resolution — no prior path existed.
                    resolved.push(ResolvedPlaylist {
                        breadcrumb: crumb.clone(),
                        orphan: false,
                        target,
                    });
                }
                // NOTE: do NOT replace existing BFS breadcrumbs with
                // shorter composite paths. SetButtonPage composites are
                // navigation keys, not content selectors. The button
                // that produced the composite (e.g. "SPECIAL FEATURES")
                // may be visually unrelated to the playlist the dispatch
                // table maps it to (e.g. PL 100, the main movie). The
                // deeper NOP anchor path, while longer, highlights the
                // correct content button on the correct sub-page.
            }
        }
    }

    resolved
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

// ── Lifecycle simulation ──────────────────────────────────────────────

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

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod tests {
    use super::super::test_helpers::{
        InsnSpec, MobjBuilder, breadcrumb_ids, build_dispatch_mobj, make_button, make_button_at,
        make_page, spec_to_other,
    };
    use super::super::{NavClipInput, PlayerContext};
    use super::*;

    fn parse(data: &[u8]) -> Result<MovieObjectFile, super::super::MobjError> {
        super::super::parse::parse(data)
    }

    // ── Resolver tests ──────────────────────────────────────────────

    #[test]
    fn resolve_set_gpr_goto_mobj_with_positional_pairing() {
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
        let mobj_data = MobjBuilder::new().object(&[InsnSpec::Nop]).build();
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
        assert_eq!(resolved[0].target.playlist, 201, "button 10 → playlist 201");
        assert_eq!(resolved[1].target.playlist, 202, "button 11 → playlist 202");
        assert_eq!(resolved[2].target.playlist, 203, "button 12 → playlist 203");
    }

    #[test]
    fn resolve_unconditional_single_play_pl() {
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
                NavigationCommand::GotoMobj { object_id: 99 },
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

    // ── VM-based resolver tests ────────────────────────────────────

    #[test]
    fn vm_resolves_gpr_dispatch_pattern() {
        let mobj_data = MobjBuilder::new()
            .object(&[
                InsnSpec::SetGpr(4076, 1),
                InsnSpec::CmpEqReg(4075, 4076),
                InsnSpec::GotoIf(6),
                InsnSpec::SetGpr(4076, 5),
                InsnSpec::CmpEqReg(4075, 4076),
                InsnSpec::GotoIf(9),
                InsnSpec::SetGpr(4075, 201),
                InsnSpec::PlayPlReg(4075),
                InsnSpec::Nop,
                InsnSpec::SetGpr(4075, 205),
                InsnSpec::PlayPlReg(4075),
            ])
            .build();

        let mobj_file = parse(&mobj_data).expect("should parse");

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
        let mobj_data = MobjBuilder::new()
            .object(&[
                InsnSpec::SetGpr(4076, 1),      // 0
                InsnSpec::CmpEqReg(4075, 4076), // 1
                InsnSpec::GotoIf(10),           // 2
                InsnSpec::SetGpr(4076, 2),      // 3
                InsnSpec::CmpEqReg(4075, 4076), // 4
                InsnSpec::GotoIf(13),           // 5
                InsnSpec::SetGpr(4076, 3),      // 6
                InsnSpec::CmpEqReg(4075, 4076), // 7
                InsnSpec::GotoIf(16),           // 8
                InsnSpec::Goto(18),             // 9
                InsnSpec::SetGpr(4075, 201),    // 10
                InsnSpec::PlayPlReg(4075),      // 11
                InsnSpec::Goto(18),             // 12
                InsnSpec::SetGpr(4075, 202),    // 13
                InsnSpec::PlayPlReg(4075),      // 14
                InsnSpec::Goto(18),             // 15
                InsnSpec::SetGpr(4075, 203),    // 16
                InsnSpec::PlayPlReg(4075),      // 17
                InsnSpec::Nop,                  // 18
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
        let mobj_data = MobjBuilder::new()
            .object(&[InsnSpec::PlayPl(999)])
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
        let mobj_data = MobjBuilder::new()
            .object(&[
                InsnSpec::SetGpr(4076, 1),
                InsnSpec::CmpEqReg(4075, 4076),
                InsnSpec::GotoIf(4),
                InsnSpec::Goto(6),
                InsnSpec::SetGpr(4075, 201),
                InsnSpec::PlayPlReg(4075),
                InsnSpec::Nop,
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
        let mobj_data = MobjBuilder::new()
            .object(&[InsnSpec::SetGprReg(100, 4075), InsnSpec::PlayPlReg(100)])
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
        let mobj_data = MobjBuilder::new()
            .object(&[
                InsnSpec::Nop,
                InsnSpec::Nop,
                InsnSpec::PlayPl(800),
                InsnSpec::Nop,
                InsnSpec::PlayPlReg(4075),
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
        let mobj_data = MobjBuilder::new()
            .object(&[InsnSpec::PlayPl(800), InsnSpec::Nop])
            .object(&[InsnSpec::Nop, InsnSpec::PlayPl(801), InsnSpec::Nop])
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
        let mobj_data = MobjBuilder::new()
            .object(&[
                InsnSpec::SetGpr(4075, 0),
                InsnSpec::SetGpr(4076, 0),
                InsnSpec::SetGpr(4077, 0),
                InsnSpec::Nop,
                InsnSpec::Nop,
                InsnSpec::PlayPl(800),
                InsnSpec::SetGpr(4076, 1),
                InsnSpec::CmpEqReg(4075, 4076),
                InsnSpec::GotoIf(17),
                InsnSpec::SetGpr(4076, 2),
                InsnSpec::CmpEqReg(4075, 4076),
                InsnSpec::GotoIf(20),
                InsnSpec::SetGpr(4076, 3),
                InsnSpec::CmpEqReg(4075, 4076),
                InsnSpec::GotoIf(23),
                InsnSpec::Goto(25),
                InsnSpec::Nop,
                InsnSpec::SetGpr(4075, 201),
                InsnSpec::PlayPlReg(4075),
                InsnSpec::Goto(25),
                InsnSpec::SetGpr(4075, 202),
                InsnSpec::PlayPlReg(4075),
                InsnSpec::Goto(25),
                InsnSpec::SetGpr(4075, 203),
                InsnSpec::PlayPlReg(4075),
                InsnSpec::Nop,
            ])
            .build();

        let mobj_file = parse(&mobj_data).expect("should parse");
        let valid_playlists: std::collections::HashSet<u32> = [201, 202, 203].into();

        // Without dispatch entries — lifecycle simulation
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
            &[],
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

        // With dispatch entries
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
        let mobj_data = MobjBuilder::new()
            .object(&[
                InsnSpec::SetGpr(4075, 0),
                InsnSpec::PlayPl(800),
                InsnSpec::SetGpr(4076, 10),
                InsnSpec::CmpEqReg(4075, 4076),
                InsnSpec::GotoIf(11),
                InsnSpec::SetGpr(4076, 20),
                InsnSpec::CmpEqReg(4075, 4076),
                InsnSpec::GotoIf(14),
                InsnSpec::SetGpr(4076, 30),
                InsnSpec::CmpEqReg(4075, 4076),
                InsnSpec::GotoIf(17),
                InsnSpec::SetGpr(4075, 301),
                InsnSpec::PlayPlReg(4075),
                InsnSpec::Goto(19),
                InsnSpec::SetGpr(4075, 302),
                InsnSpec::PlayPlReg(4075),
                InsnSpec::Goto(19),
                InsnSpec::SetGpr(4075, 303),
                InsnSpec::PlayPlReg(4075),
                InsnSpec::Nop,
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
        let mobj_data = MobjBuilder::new()
            .object(&[
                InsnSpec::SetGpr(4075, 0),
                InsnSpec::PlayPl(800),
                InsnSpec::SetGpr(4076, 5),
                InsnSpec::CmpEqReg(4075, 4076),
                InsnSpec::GotoIf(7),
                InsnSpec::Nop,
                InsnSpec::Nop,
                InsnSpec::SetGpr(4075, 205),
                InsnSpec::PlayPlReg(4075),
            ])
            .build();

        let mobj_file = parse(&mobj_data).expect("should parse");
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
        let mobj_data = MobjBuilder::new()
            .object(&[InsnSpec::PlayPl(800), InsnSpec::Nop])
            .object(&[InsnSpec::PlayPl(201)])
            .build();

        let mobj_file = parse(&mobj_data).expect("should parse");
        let dispatch_entries = vec![DispatchEntry {
            mobj_index: 0,
            dispatch_pc: 1,
        }];

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
        let mut instrs = vec![
            InsnSpec::SetGpr(200, 0),
            InsnSpec::SetGpr(100, 0),
            InsnSpec::Nop,
        ];
        let cases: [(u32, u16); 3] = [(5, 301), (10, 302), (15, 303)];
        let switch_start = instrs.len();
        for (i, &(case_val, _)) in cases.iter().enumerate() {
            let handler_pc = switch_start + cases.len() * 4 + i * 3;
            instrs.push(InsnSpec::SetGprReg(100, 200));
            instrs.push(InsnSpec::SetGpr(101, case_val));
            instrs.push(InsnSpec::CmpEqReg(100, 101));
            #[allow(clippy::cast_possible_truncation, reason = "test data")]
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
        let dispatch_mobj = build_dispatch_mobj(&[(5, 205), (8, 208), (11, 211)]);
        let mobj_data = MobjBuilder::new().object(&dispatch_mobj).build();
        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

        let buttons = vec![
            make_button(
                6,
                vec![NavigationCommand::SetGpr {
                    register: 4075,
                    value: 5,
                }],
            ),
            make_button(
                3,
                vec![NavigationCommand::SetGpr {
                    register: 4075,
                    value: 5,
                }],
            ),
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
        let dispatch_mobj = build_dispatch_mobj(&[(10, 201), (11, 202), (12, 203)]);
        let mobj_data = MobjBuilder::new().object(&dispatch_mobj).build();
        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

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
        let goto_target = vec![InsnSpec::PlayPl(500)];
        let dispatch_mobj = build_dispatch_mobj(&[(20, 301), (22, 302), (24, 303)]);

        let mobj_data = MobjBuilder::new()
            .object(&goto_target)
            .object(&dispatch_mobj)
            .build();
        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

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
        assert_eq!(resolved[0].target.playlist, 500, "GotoMobj → 500");
        assert_eq!(resolved[1].target.playlist, 302, "composite 20+2=22 → 302");
    }

    #[test]
    fn dispatch_table_button_id_zero_matches_raw_key() {
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
    fn set_button_page_resolves_via_dispatch_fallback() {
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

        assert_eq!(resolved.len(), 1, "dispatch fallback resolves composite 8");
        assert_eq!(resolved[0].target.playlist, 208, "composite 8 → PL 208");
        assert_eq!(
            breadcrumb_ids(&resolved[0].breadcrumb),
            vec![3],
            "breadcrumb is the navigation button"
        );
    }

    #[test]
    fn nav_breadcrumb_play_pl_via_navigation() {
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
                NavigationCommand::SetGpr {
                    register: 0,
                    value: 42,
                },
                NavigationCommand::GotoMobj { object_id: 1 },
            ],
        );
        let page1 = make_page(1, vec![content_button]);

        let mobj_data = MobjBuilder::new()
            .object(&[InsnSpec::Nop])
            .object(&[
                InsnSpec::CmpEq(0, 42),
                InsnSpec::Goto(3),
                InsnSpec::Nop,
                InsnSpec::PlayPl(301),
            ])
            .build();
        let mobj_file = parse(&mobj_data).expect("should parse");

        let mut valid = std::collections::HashSet::new();
        valid.insert(301);

        let clips = vec![NavClipInput {
            ig_pid: 0x1200,
            pages: vec![&page0, &page1],
        }];
        let resolved = resolve_via_execution(&clips, &mobj_file, None, &valid);

        assert_eq!(resolved.len(), 1, "one playlist resolved");
        assert_eq!(resolved[0].target.playlist, 301, "GotoMobj → 301");
        assert_eq!(
            breadcrumb_ids(&resolved[0].breadcrumb),
            vec![1, 5],
            "breadcrumb: nav button 1 → content button 5"
        );
    }

    #[test]
    fn exec_goto_mobj_resolves_playlist() {
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
            .object(&[InsnSpec::Nop])
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
        assert_eq!(
            breadcrumb_ids(&resolved[0].breadcrumb),
            vec![5],
            "direct content → single-element breadcrumb"
        );
    }

    #[test]
    fn exec_direct_play_pl_skipped() {
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
    fn nav_breadcrumb_two_level_navigation() {
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
                    value: 2,
                },
                spec_to_other(&InsnSpec::SetButtonPage(50, 51)),
            ],
        );
        let page1 = make_page(1, vec![nav1]);

        let content = make_button(
            3,
            vec![
                NavigationCommand::SetGpr {
                    register: 0,
                    value: 1,
                },
                NavigationCommand::GotoMobj { object_id: 1 },
            ],
        );
        let page2 = make_page(2, vec![content]);

        let mobj_data = MobjBuilder::new()
            .object(&[InsnSpec::Nop])
            .object(&[InsnSpec::PlayPl(401)])
            .build();
        let mobj_file = parse(&mobj_data).expect("should parse");

        let mut valid = std::collections::HashSet::new();
        valid.insert(401);

        let clips = vec![NavClipInput {
            ig_pid: 0x1200,
            pages: vec![&page0, &page1, &page2],
        }];
        let resolved = resolve_via_execution(&clips, &mobj_file, None, &valid);

        assert_eq!(resolved.len(), 1, "one playlist resolved");
        assert_eq!(resolved[0].target.playlist, 401, "three-level → 401");
        assert_eq!(
            breadcrumb_ids(&resolved[0].breadcrumb),
            vec![1, 2, 3],
            "three-level breadcrumb"
        );
    }

    // ── Navigation graph tests ────────────────────────────────────────

    #[test]
    fn nav_graph_propagates_gpr_state() {
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
                spec_to_other(&InsnSpec::CmpEq(100, 42)),
                NavigationCommand::GotoMobj { object_id: 2 },
                NavigationCommand::GotoMobj { object_id: 1 },
            ],
        );
        let page1 = make_page(1, vec![content_button]);

        let mobj_data = MobjBuilder::new()
            .object(&[InsnSpec::Nop])
            .object(&[InsnSpec::PlayPl(301)])
            .object(&[InsnSpec::PlayPl(302)])
            .build();
        let mobj_file = parse(&mobj_data).expect("should parse");

        let mut valid = std::collections::HashSet::new();
        valid.insert(301);
        valid.insert(302);

        let clips = vec![NavClipInput {
            ig_pid: 0x1200,
            pages: vec![&page0, &page1],
        }];
        let resolved = resolve_via_execution(&clips, &mobj_file, None, &valid);

        assert_eq!(resolved.len(), 1, "one playlist resolved via navigation");
        assert_eq!(
            resolved[0].target.playlist, 302,
            "GPR[100]=42 → MOBJ[2] → 302"
        );
        assert_eq!(
            breadcrumb_ids(&resolved[0].breadcrumb),
            vec![1, 5],
            "via navigation: breadcrumb [nav, content]"
        );
    }

    #[test]
    fn nav_graph_multiple_paths_to_same_page() {
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
                spec_to_other(&InsnSpec::CmpEq(100, 10)),
                NavigationCommand::GotoMobj { object_id: 1 },
                spec_to_other(&InsnSpec::CmpEq(100, 20)),
                NavigationCommand::GotoMobj { object_id: 2 },
                NavigationCommand::GotoMobj { object_id: 3 },
            ],
        );
        let page1 = make_page(1, vec![content]);

        let mobj_data = MobjBuilder::new()
            .object(&[InsnSpec::Nop])
            .object(&[InsnSpec::PlayPl(301)])
            .object(&[InsnSpec::PlayPl(302)])
            .object(&[InsnSpec::PlayPl(303)])
            .build();
        let mobj_file = parse(&mobj_data).expect("should parse");

        let mut valid = std::collections::HashSet::new();
        valid.insert(301);
        valid.insert(302);
        valid.insert(303);

        let clips = vec![NavClipInput {
            ig_pid: 0x1200,
            pages: vec![&page0, &page1],
        }];
        let resolved = resolve_via_execution(&clips, &mobj_file, None, &valid);

        let playlists: std::collections::HashSet<u16> =
            resolved.iter().map(|r| r.target.playlist).collect();
        assert!(playlists.contains(&301), "path A: GPR[100]=10 → 301");
        assert!(playlists.contains(&302), "path B: GPR[100]=20 → 302");
        assert_eq!(playlists.len(), 2, "exactly two playlists");
    }

    #[test]
    fn nav_graph_handles_cycle() {
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
        let resolved =
            resolve_via_execution(&clips, &mobj_file, None, &std::collections::HashSet::new());
        assert!(
            resolved.is_empty(),
            "cycle with no dispatch table produces no resolutions"
        );
    }

    #[test]
    fn nav_graph_same_page_terminates() {
        let nav_button = make_button(
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
                spec_to_other(&InsnSpec::SetButtonPage(4077, 60)),
            ],
        );
        let page = make_page(0, vec![nav_button]);

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

        assert_eq!(resolved.len(), 1, "same-page navigation terminates");
        assert_eq!(resolved[0].target.playlist, 208, "composite 8 → PL 208");
    }

    // ── NOP anchor resolution tests ──────────────────────────────────

    #[test]
    fn exec_nop_anchor_resolves_via_button_id() {
        let anchor_12 = make_button(12, vec![]);
        let anchor_15 = make_button(15, vec![spec_to_other(&InsnSpec::Nop)]);
        let anchor_20 = make_button(20, vec![]);

        let dispatch_mobj =
            build_dispatch_mobj(&[(5, 205), (12, 212), (15, 215), (20, 220), (32, 232)]);
        let mobj_data = MobjBuilder::new().object(&dispatch_mobj).build();
        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

        let page = make_page(0, vec![anchor_12, anchor_15, anchor_20]);
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

        for rp in &resolved {
            assert_eq!(
                rp.breadcrumb.len(),
                1,
                "root-page anchor has single-element breadcrumb"
            );
        }
    }

    #[test]
    fn exec_nop_anchor_skipped_without_dispatch_table() {
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
    fn exec_nop_anchors_coexist_with_navigation_buttons() {
        let nav_button = make_button_at(
            3,
            1080,
            949,
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
        let thumbnail = make_button_at(
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
        let anchor = make_button_at(15, 199, 668, vec![]);

        let dispatch_mobj = build_dispatch_mobj(&[(8, 208), (15, 215), (20, 220)]);
        let mobj_data = MobjBuilder::new().object(&dispatch_mobj).build();
        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

        let page = make_page(0, vec![nav_button, thumbnail, anchor]);
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

        assert_eq!(
            resolved.len(),
            2,
            "NOP anchor and navigation composite both resolve"
        );
        let pls: std::collections::HashSet<u16> =
            resolved.iter().map(|r| r.target.playlist).collect();
        assert!(pls.contains(&215), "anchor 15 → 215");
        assert!(pls.contains(&208), "nav composite 8 → 208");

        let anchor_resolution = resolved
            .iter()
            .find(|r| r.target.playlist == 215)
            .expect("should find anchor resolution");
        assert_eq!(
            breadcrumb_ids(&anchor_resolution.breadcrumb),
            vec![5],
            "NOP anchor breadcrumb remapped to visible thumbnail btn[5]"
        );
    }

    #[test]
    fn exec_nop_anchor_deduplicates() {
        let anchor_p0 = make_button_at(12, 199, 668, vec![]);
        let thumbnail = make_button_at(
            7,
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
        let nav_a = make_button_at(
            1,
            400,
            949,
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
        let nav_b = make_button_at(
            2,
            800,
            949,
            vec![
                NavigationCommand::SetGpr {
                    register: 50,
                    value: 0,
                },
                NavigationCommand::SetGpr {
                    register: 52,
                    value: 1,
                },
                spec_to_other(&InsnSpec::SetButtonPage(50, 52)),
            ],
        );
        let anchor_p1 = make_button(12, vec![]);

        let dispatch_mobj = build_dispatch_mobj(&[(12, 212), (15, 215), (20, 220)]);
        let mobj_data = MobjBuilder::new().object(&dispatch_mobj).build();
        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

        let page0 = make_page(0, vec![anchor_p0, thumbnail, nav_a, nav_b]);
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

        let matching: Vec<_> = resolved
            .iter()
            .filter(|r| r.target.playlist == 212)
            .collect();
        assert_eq!(matching.len(), 1, "same PlayTarget → deduplicated to one");
        assert_eq!(
            breadcrumb_ids(&matching[0].breadcrumb),
            vec![7],
            "shortest breadcrumb (root page) wins, remapped to visible thumbnail"
        );
    }

    #[test]
    fn nav_breadcrumb_nop_anchor_via_navigation() {
        let nav = make_button(
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
        let page0 = make_page(0, vec![nav]);

        let anchor = make_button(15, vec![]);
        let page1 = make_page(1, vec![anchor]);

        let dispatch_mobj = build_dispatch_mobj(&[(10, 210), (15, 215), (20, 220)]);
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

        assert_eq!(resolved.len(), 1, "one anchor resolved");
        assert_eq!(resolved[0].target.playlist, 215, "anchor 15 → 215");
        assert_eq!(
            breadcrumb_ids(&resolved[0].breadcrumb),
            vec![1, 15],
            "sub-page anchor has navigation prefix"
        );
    }

    #[test]
    fn nav_breadcrumb_shortest_path_wins() {
        let direct = make_button(5, vec![NavigationCommand::GotoMobj { object_id: 1 }]);
        let nav = make_button(
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
        let page0 = make_page(0, vec![direct, nav]);

        let dup_content = make_button(5, vec![NavigationCommand::GotoMobj { object_id: 1 }]);
        let page1 = make_page(1, vec![dup_content]);

        let mobj_data = MobjBuilder::new()
            .object(&[InsnSpec::Nop])
            .object(&[InsnSpec::PlayPl(301)])
            .build();
        let mobj_file = parse(&mobj_data).expect("should parse");

        let mut valid = std::collections::HashSet::new();
        valid.insert(301);

        let clips = vec![NavClipInput {
            ig_pid: 0x1200,
            pages: vec![&page0, &page1],
        }];
        let resolved = resolve_via_execution(&clips, &mobj_file, None, &valid);

        assert_eq!(resolved.len(), 1, "deduplicated to one resolution");
        assert_eq!(resolved[0].target.playlist, 301, "playlist 301");
        assert_eq!(
            breadcrumb_ids(&resolved[0].breadcrumb),
            vec![5],
            "shortest path (direct on page 0) wins"
        );
    }

    #[test]
    fn orphan_sweep_resolves_unreachable_pages() {
        let root_content = make_button(5, vec![NavigationCommand::GotoMobj { object_id: 1 }]);
        let page0 = make_page(0, vec![root_content]);

        let orphan_anchor = make_button(15, vec![]);
        let page2 = make_page(2, vec![orphan_anchor]);

        let dispatch_mobj = build_dispatch_mobj(&[(10, 210), (15, 215), (20, 220)]);
        let mobj_data = MobjBuilder::new()
            .object(&dispatch_mobj)
            .object(&[InsnSpec::PlayPl(301)])
            .build();
        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

        let mut valid = std::collections::HashSet::new();
        valid.insert(301);

        let clips = vec![NavClipInput {
            ig_pid: 0x1200,
            pages: vec![&page0, &page2],
        }];
        let resolved = resolve_via_execution(&clips, &mobj_file, Some(&table), &valid);

        assert_eq!(resolved.len(), 2, "root content + orphan anchor");

        let root = resolved
            .iter()
            .find(|r| r.target.playlist == 301)
            .expect("should find root content");
        assert!(!root.orphan, "root page content is not orphan");

        let orphan = resolved
            .iter()
            .find(|r| r.target.playlist == 215)
            .expect("should find orphan content");
        assert!(orphan.orphan, "unreachable page content is orphan");
        assert_eq!(
            orphan.breadcrumb[0].page_id, 2,
            "orphan step carries page context"
        );
    }

    #[test]
    fn goto_mobj_pattern_unaffected_by_visible_resolution() {
        let button_a = make_button(
            1,
            vec![
                NavigationCommand::SetGpr {
                    register: 0,
                    value: 1,
                },
                NavigationCommand::GotoMobj { object_id: 1 },
            ],
        );
        let button_b = make_button(
            2,
            vec![
                NavigationCommand::SetGpr {
                    register: 0,
                    value: 2,
                },
                NavigationCommand::GotoMobj { object_id: 2 },
            ],
        );
        let page = make_page(0, vec![button_a, button_b]);

        let mobj_data = MobjBuilder::new()
            .object(&[InsnSpec::Nop])
            .object(&[InsnSpec::PlayPl(301)])
            .object(&[InsnSpec::PlayPl(302)])
            .build();
        let mobj_file = parse(&mobj_data).expect("should parse");

        let mut valid = std::collections::HashSet::new();
        valid.insert(301);
        valid.insert(302);

        let clips = vec![NavClipInput {
            ig_pid: 0x1200,
            pages: vec![&page],
        }];
        let resolved = resolve_via_execution(&clips, &mobj_file, None, &valid);

        assert_eq!(resolved.len(), 2, "two GotoMobj buttons resolved");
        let pls: std::collections::HashSet<u16> =
            resolved.iter().map(|r| r.target.playlist).collect();
        assert!(pls.contains(&301), "btn[1] → 301");
        assert!(pls.contains(&302), "btn[2] → 302");
    }

    #[test]
    fn exec_cross_clip_nop_anchors_both_preserved() {
        let anchor_a = make_button_at(15, 199, 668, vec![]);
        let thumb_a = make_button_at(
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

        let anchor_b = make_button_at(15, 199, 668, vec![]);
        let thumb_b = make_button_at(
            7,
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

        let dispatch_mobj = build_dispatch_mobj(&[(10, 210), (15, 215), (20, 220)]);
        let mobj_data = MobjBuilder::new().object(&dispatch_mobj).build();
        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

        let page_a = make_page(0, vec![anchor_a, thumb_a]);
        let page_b = make_page(0, vec![anchor_b, thumb_b]);
        let clips = vec![
            NavClipInput {
                ig_pid: 0x1200,
                pages: vec![&page_a],
            },
            NavClipInput {
                ig_pid: 0x1201,
                pages: vec![&page_b],
            },
        ];

        let resolved = resolve_via_execution(
            &clips,
            &mobj_file,
            Some(&table),
            &std::collections::HashSet::new(),
        );

        let matching: Vec<_> = resolved
            .iter()
            .filter(|r| r.target.playlist == 215)
            .collect();
        assert_eq!(
            matching.len(),
            2,
            "same playlist from two clips → both preserved"
        );

        let clip_indices: std::collections::HashSet<usize> = matching
            .iter()
            .map(|r| {
                r.breadcrumb
                    .last()
                    .expect("breadcrumb should not be empty")
                    .clip_index
            })
            .collect();
        assert!(clip_indices.contains(&0), "clip 0 resolution preserved");
        assert!(clip_indices.contains(&1), "clip 1 resolution preserved");
    }

    #[test]
    fn exec_same_clip_still_deduplicates() {
        let anchor_p0 = make_button_at(12, 199, 668, vec![]);
        let thumb_p0 = make_button_at(
            7,
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
        let anchor_p1 = make_button(12, vec![]);

        let dispatch_mobj = build_dispatch_mobj(&[(12, 212), (15, 215), (20, 220)]);
        let mobj_data = MobjBuilder::new().object(&dispatch_mobj).build();
        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file).expect("should extract table");

        let page0 = make_page(0, vec![anchor_p0, thumb_p0]);
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

        let count = resolved.iter().filter(|r| r.target.playlist == 212).count();
        assert_eq!(
            count, 1,
            "same clip, same PlayTarget → still deduplicated to one"
        );
    }

    // ── Register-computed page navigation tests ─────────────────────

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "end-to-end test with multi-page disc structure is inherently long"
    )]
    fn register_computed_page_via_seeded_gprs() {
        // End-to-end: MOBJ[0] writes GPR[3051] = 6 after a PlayPl.
        // btn[4] on page 1 reads GPR[3051] to compute the page target.
        //
        // Before fix: seed_gpr_state terminated at the PlayPl, GPR[3051]
        // was never captured, and btn[4]'s SET_BUTTON_PAGE got page=0.
        // After fix: PlayPl is skipped during seeding, GPR[3051]=6 is
        // captured, and btn[4] navigates to page 6.

        // MOBJ[0]: init block with PlayPl before GPR[3051] write
        let mobj0_instrs = vec![
            InsnSpec::SetGpr(3000, 42),
            InsnSpec::PlayPl(100), // menu start — previously terminated seeding
            InsnSpec::SetGpr(3051, 6), // page config for special features
        ];

        // MOBJ[1]: dispatch table for NOP anchors (need 3+ cases)
        let dispatch_mobj = build_dispatch_mobj(&[(30, 301), (31, 302), (32, 303)]);

        let mobj_data = MobjBuilder::new()
            .object(&mobj0_instrs)
            .object(&dispatch_mobj)
            .build();
        let mobj_file = parse(&mobj_data).expect("should parse");
        let table = extract_dispatch_table(&mobj_file);

        // Page 0: root → page 1
        let root_nav = make_button(
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
        let page0 = make_page(0, vec![root_nav]);

        // Page 1: btn[4] with register-computed SET_BUTTON_PAGE to page 6
        let special_features_btn = make_button(
            4,
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
                spec_to_other(&InsnSpec::SetGprReg(4075, 3051)),
                spec_to_other(&InsnSpec::SetButtonPage(4077, 4075)),
            ],
        );
        let page1 = make_page(1, vec![special_features_btn]);

        // Page 6: special features (NOP anchors with text labels)
        let extras_a = make_button(31, vec![]);
        let extras_b = make_button(32, vec![]);
        let extras_c = make_button(30, vec![]);
        let page6 = make_page(6, vec![extras_a, extras_b, extras_c]);

        let clips = vec![NavClipInput {
            ig_pid: 0x1200,
            pages: vec![&page0, &page1, &page6],
        }];

        let resolved = resolve_via_execution(
            &clips,
            &mobj_file,
            table.as_ref(),
            &valid_set(&[301, 302, 303]),
        );

        // Extras should resolve via page 6 through btn[4]
        let extras: Vec<_> = resolved
            .iter()
            .filter(|r| !r.orphan && r.target.playlist == 302)
            .collect();
        assert_eq!(extras.len(), 1, "extras PL 302 resolved via navigation");

        let crumb = breadcrumb_ids(&extras[0].breadcrumb);
        assert!(
            crumb.contains(&4),
            "extras breadcrumb includes btn[4]: {crumb:?}"
        );

        let last_step = extras[0].breadcrumb.last().expect("non-empty breadcrumb");
        assert_eq!(
            last_step.page_id, 6,
            "extras breadcrumb ends on page 6 (text labels, not filmstrips)"
        );
    }

    /// Builds a valid playlist set from a slice of playlist numbers.
    fn valid_set(playlists: &[u32]) -> std::collections::HashSet<u32> {
        playlists.iter().copied().collect()
    }
}
