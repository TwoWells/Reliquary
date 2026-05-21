// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Diagnostic trace output for `--trace` mode.
//!
//! All functions in this module write to stderr and are gated behind the
//! `--trace` CLI flag. They dump IG clip structure, MOBJ instruction
//! traces, dispatch table resolution, and execution coverage.

use std::collections::HashSet;

use crate::identify::ExtractedButton;

/// Dumps per-clip IG structure: display sets, pages, buttons, and commands.
#[allow(clippy::print_stderr, reason = "diagnostic trace output")]
#[allow(
    clippy::too_many_lines,
    reason = "diagnostic dump with position + command detail"
)]
pub fn trace_ig_clip(clip_id: &str, ig_stream: &reliquary::disc::bdmv::ig::IgStream) {
    use reliquary::disc::bdmv::ig::NavigationCommand;

    let total_buttons: usize = ig_stream
        .display_sets
        .iter()
        .flat_map(|ds| &ds.compositions)
        .flat_map(|c| &c.pages)
        .map(|p| p.buttons.len())
        .sum();

    eprintln!(
        "\n--- clip {clip_id}: {} display sets, {total_buttons} buttons ---",
        ig_stream.display_sets.len()
    );

    for (ds_idx, ds) in ig_stream.display_sets.iter().enumerate() {
        for comp in &ds.compositions {
            for page in &comp.pages {
                // Buttons with navigation commands (SetGpr or Other)
                let nav_buttons: Vec<_> = page
                    .buttons
                    .iter()
                    .filter(|b| {
                        b.commands.iter().any(|c| {
                            matches!(
                                c,
                                NavigationCommand::SetGpr { .. } | NavigationCommand::Other { .. }
                            )
                        })
                    })
                    .collect();

                // Show button positions for spatial debugging
                for b in &page.buttons {
                    let cmd_label = if reliquary::disc::bdmv::mobj::is_nop_anchor(b) {
                        "NOP"
                    } else if b
                        .commands
                        .iter()
                        .any(|c| matches!(c, NavigationCommand::PlayPl { .. }))
                    {
                        "PlayPl"
                    } else {
                        "nav"
                    };
                    eprintln!(
                        "    btn[{:3}]@({:4},{:4}) {cmd_label}",
                        b.button_id, b.x, b.y,
                    );
                }

                if nav_buttons.is_empty() {
                    eprintln!(
                        "  ds[{ds_idx}] page={}: {} buttons (no nav cmds)",
                        page.page_id,
                        page.buttons.len()
                    );
                    continue;
                }

                let setgpr_count = nav_buttons
                    .iter()
                    .filter(|b| {
                        b.commands
                            .iter()
                            .any(|c| matches!(c, NavigationCommand::SetGpr { .. }))
                    })
                    .count();
                let other_only_count = nav_buttons.len() - setgpr_count;
                eprintln!(
                    "  ds[{ds_idx}] page={}: {} buttons ({} SetGpr, {} Other-only)",
                    page.page_id,
                    page.buttons.len(),
                    setgpr_count,
                    other_only_count,
                );

                for button in &nav_buttons {
                    let has_other = button
                        .commands
                        .iter()
                        .any(|c| matches!(c, NavigationCommand::Other { .. }));
                    let has_setgpr = button
                        .commands
                        .iter()
                        .any(|c| matches!(c, NavigationCommand::SetGpr { .. }));

                    if has_other {
                        // Full disassembly for complex buttons
                        eprintln!(
                            "    btn[{:3}] ({} cmds):",
                            button.button_id,
                            button.commands.len(),
                        );
                        for (i, cmd) in button.commands.iter().enumerate() {
                            let insn = reliquary::disc::bdmv::mobj::command_to_instruction(cmd);
                            eprintln!(
                                "      [{i:2}] {}",
                                reliquary::disc::bdmv::mobj::format_instruction(&insn),
                            );
                        }
                    } else if has_setgpr {
                        // Compact summary for simple SetGpr-only buttons
                        let gprs: Vec<String> = button
                            .commands
                            .iter()
                            .filter_map(|c| match c {
                                NavigationCommand::SetGpr { register, value } => {
                                    Some(format!("GPR[{register}]={value}"))
                                }
                                _ => None,
                            })
                            .collect();
                        eprintln!("    btn[{:3}] {}", button.button_id, gprs.join(", "));
                    }
                }
            }
        }
    }
}

/// Dumps MOBJ structure and instruction trace for debugging.
#[allow(clippy::print_stderr, reason = "diagnostic trace output")]
#[allow(
    clippy::too_many_lines,
    reason = "diagnostic dump with inline formatting"
)]
pub fn trace_mobj_structure(
    mobj_file: &reliquary::disc::bdmv::mobj::MovieObjectFile,
    valid_playlists: &HashSet<u32>,
    ig_buttons: &[(
        reliquary::disc::bdmv::ig::Button,
        reliquary::disc::bdmv::mobj::PlayerContext,
    )],
) {
    use reliquary::disc::bdmv::ig::NavigationCommand;

    eprintln!("\n=== MOBJ TRACE ===");
    eprintln!("{} movie objects", mobj_file.objects.len());

    for (idx, mobj) in mobj_file.objects.iter().enumerate() {
        let instrs = &mobj.instructions;
        let play_pls: Vec<(usize, bool)> = instrs
            .iter()
            .enumerate()
            .filter(|(_, i)| i.group == 0 && i.sub_group == 2) // BRANCH_PLAY
            .map(|(pc, i)| (pc, i.imm_op1))
            .collect();

        if play_pls.is_empty() {
            continue;
        }

        let imm_count = play_pls.iter().filter(|(_, imm)| *imm).count();
        let reg_count = play_pls.len() - imm_count;

        eprintln!(
            "\nMOBJ[{idx}]: {} instructions, {} PlayPl ({} immediate, {} register)",
            instrs.len(),
            play_pls.len(),
            imm_count,
            reg_count
        );

        // Show all PlayPl instructions with surrounding context
        for &(pc, is_imm) in &play_pls {
            let insn = &instrs[pc];
            if is_imm {
                let valid = valid_playlists.contains(&insn.dst);
                eprintln!(
                    "  [{pc:4}] PlayPl(imm={}) {}",
                    insn.dst,
                    if valid { "VALID" } else { "non-valid" }
                );
            } else {
                // Show what SET precedes this PlayPl
                let prev_info = if pc > 0 {
                    let prev = &instrs[pc - 1];
                    if prev.group == 2 && prev.sub_group == 0 {
                        // SET
                        if prev.imm_op2 {
                            format!("preceded by SET GPR[{}] = {}", prev.dst, prev.src)
                        } else {
                            format!("preceded by SET GPR[{}] = GPR[{}]", prev.dst, prev.src)
                        }
                    } else {
                        format!("preceded by group={} sub={}", prev.group, prev.sub_group)
                    }
                } else {
                    "first instruction".to_string()
                };
                eprintln!("  [{pc:4}] PlayPl(GPR[{}]) — {prev_info}", insn.dst);
            }
        }

        // Show instructions for MOBJs with register-based PlayPl.
        // For the dispatch table MOBJ, show init + first handler.
        if reg_count > 0 || idx == 0 {
            let first_play_pc = play_pls.first().map_or(30, |(pc, _)| *pc);
            let show = if play_pls.len() > 5 {
                (first_play_pc + 5).min(instrs.len())
            } else {
                instrs.len().min(30)
            };
            eprintln!("  First {show} instructions:");
            for (pc, insn) in instrs.iter().enumerate().take(show) {
                let desc = match (insn.group, insn.sub_group) {
                    (0, 0) => {
                        // GOTO sub-group: NOP(0)/GOTO(1)/BREAK(2)
                        match insn.branch_opt {
                            0 => format!("NOP → {}", insn.dst),
                            1 => format!("GOTO → {}", insn.dst),
                            2 => format!("BREAK → {}", insn.dst),
                            n => format!("GOTO(opt={n}) → {}", insn.dst),
                        }
                    }
                    (0, 1) => format!("JUMP(mobj={})", insn.dst), // GotoMobj
                    (0, 2) => {
                        // PlayPl
                        if insn.imm_op1 {
                            format!("PlayPl(imm={})", insn.dst)
                        } else {
                            format!("PlayPl(GPR[{}])", insn.dst)
                        }
                    }
                    (1, _) => {
                        // CMP
                        let op = match insn.cmp_opt {
                            2 => "==",
                            3 => "!=",
                            4 => ">=",
                            5 => ">",
                            6 => "<=",
                            7 => "<",
                            _ => "??",
                        };
                        let dst = if insn.imm_op1 {
                            format!("{}", insn.dst)
                        } else {
                            format!("GPR[{}]", insn.dst)
                        };
                        let src = if insn.imm_op2 {
                            format!("{}", insn.src)
                        } else {
                            format!("GPR[{}]", insn.src)
                        };
                        format!("CMP {dst} {op} {src}")
                    }
                    (2, 0) => {
                        // SET
                        let op = match insn.set_opt {
                            1 => "=",
                            2 => "<=>",
                            3 => "+=",
                            4 => "-=",
                            9 => "&=",
                            0xA => "|=",
                            0xB => "^=",
                            0xC => "bset",
                            0xD => "bclr",
                            0xE => "<<=",
                            0xF => ">>=",
                            _ => "??=",
                        };
                        let src = if insn.imm_op2 {
                            format!("{}", insn.src)
                        } else {
                            format!("GPR[{}]", insn.src)
                        };
                        format!("SET GPR[{}] {op} {src}", insn.dst)
                    }
                    (2, 1) => {
                        // SETSYSTEM
                        let op_name = match insn.set_opt {
                            0x01 => "SET_STREAM",
                            0x02 => "SET_NV_TIMER",
                            0x03 => "SET_BUTTON_PAGE",
                            0x04 => "ENABLE_BUTTON",
                            0x05 => "DISABLE_BUTTON",
                            0x06 => "SET_SEC_STREAM",
                            0x07 => "POPUP_OFF",
                            0x08 => "STILL_ON",
                            0x09 => "STILL_OFF",
                            0x0A => "SET_OUTPUT_MODE",
                            0x0B => "SET_STREAM_SS",
                            _ => "UNKNOWN",
                        };
                        let dst_s = if insn.imm_op1 {
                            format!("{}", insn.dst)
                        } else {
                            format!("GPR[{}]", insn.dst)
                        };
                        let src_s = if insn.imm_op2 {
                            format!("{}", insn.src)
                        } else {
                            format!("GPR[{}]", insn.src)
                        };
                        format!("SETSYSTEM {op_name} dst={dst_s} src={src_s}")
                    }
                    _ => format!(
                        "??? grp={} sub={} dst={} src={}",
                        insn.group, insn.sub_group, insn.dst, insn.src
                    ),
                };
                eprintln!("    [{pc:4}] {desc}");
            }
        }
    }

    // Search for instructions that write to GPR[3002] and dump context
    eprintln!("\nSearching all MOBJs for writes to GPR[3002]:");
    for (idx, mobj) in mobj_file.objects.iter().enumerate() {
        let instrs = &mobj.instructions;
        for (pc, insn) in instrs.iter().enumerate() {
            if insn.group == 2 && insn.sub_group == 0 && insn.dst == 3002 {
                let src_desc = if insn.imm_op2 {
                    format!("{}", insn.src)
                } else {
                    format!("GPR[{}]", insn.src)
                };
                eprintln!("  MOBJ[{idx}][{pc}]: GPR[3002] = {src_desc} — context:");
                // Show 5 instructions before and after for context
                let start = pc.saturating_sub(5);
                let end = instrs.len().min(pc + 6);
                for (ctx_pc, ci) in instrs.iter().enumerate().take(end).skip(start) {
                    let desc = match (ci.group, ci.sub_group) {
                        (2, 0) => {
                            let op = match ci.set_opt {
                                0 => "=",
                                8 => "&=",
                                9 => "|=",
                                0xA => "^=",
                                _ => "?=",
                            };
                            let s = if ci.imm_op2 {
                                format!("{}", ci.src)
                            } else if ci.src >= 0x8000_0000 {
                                format!("PSR[{}]", ci.src & 0x7FFF_FFFF)
                            } else {
                                format!("GPR[{}]", ci.src)
                            };
                            format!("SET GPR[{}] {op} {s}", ci.dst)
                        }
                        (1, _) => {
                            let d = if ci.imm_op1 {
                                format!("{}", ci.dst)
                            } else {
                                format!("GPR[{}]", ci.dst)
                            };
                            let s = if ci.imm_op2 {
                                format!("{}", ci.src)
                            } else {
                                format!("GPR[{}]", ci.src)
                            };
                            format!("CMP {d} ?? {s}")
                        }
                        (0, 0) => format!("GOTO(opt={}) → {}", ci.branch_opt, ci.dst),
                        (0, 1) => format!("JUMP → MOBJ[{}]", ci.dst),
                        (0, 2) => {
                            if ci.imm_op1 {
                                format!("PlayPl({})", ci.dst)
                            } else {
                                format!("PlayPl(GPR[{}])", ci.dst)
                            }
                        }
                        _ => format!("grp={} sub={}", ci.group, ci.sub_group),
                    };
                    let marker = if ctx_pc == pc { " ★" } else { "" };
                    eprintln!("      [{ctx_pc:4}] {desc}{marker}");
                }
            }
        }
    }

    // Step-by-step VM trace of the dispatch MOBJ with a sample button's state.
    // Find the dispatch MOBJ (most PlayPl) and a non-trivial button.
    let dispatch_mobj_idx = mobj_file
        .objects
        .iter()
        .enumerate()
        .max_by_key(|(_, m)| {
            m.instructions
                .iter()
                .filter(|i| i.group == 0 && i.sub_group == 2)
                .count()
        })
        .map(|(i, _)| i);

    // Find a button with SetGpr commands
    let sample_button = ig_buttons.iter().find(|(b, _)| {
        b.commands
            .iter()
            .any(|c| matches!(c, NavigationCommand::SetGpr { .. }))
    });

    if let (Some(mobj_idx), Some((button, ctx))) = (dispatch_mobj_idx, sample_button) {
        let instrs = &mobj_file.objects[mobj_idx].instructions;

        // Simulate button command execution to get register state
        let mut gprs = std::collections::HashMap::<u32, u32>::new();
        for cmd in &button.commands {
            if let NavigationCommand::SetGpr { register, value } = cmd {
                gprs.insert(*register, *value);
            }
            // We can't simulate the Other commands (register-to-register SET)
            // but we seed what we can
        }
        // Seed PSR context
        gprs.insert(0x8000_0000, u32::from(ctx.ig_stream)); // PSR[0]
        gprs.insert(0x8000_000A, u32::from(ctx.selected_button_id)); // PSR[10]
        gprs.insert(0x8000_000B, u32::from(ctx.page_id)); // PSR[11]

        eprintln!(
            "\n--- VM TRACE: MOBJ[{mobj_idx}] with btn[{}] (GPR seeds: {:?}) ---",
            button.button_id,
            gprs.iter()
                .filter(|&(&k, _)| k < 0x8000_0000)
                .map(|(&k, &v)| format!("GPR[{k}]={v}"))
                .collect::<Vec<_>>()
                .join(", ")
        );

        let mut pc: usize = 0;
        let mut steps: u32 = 0;
        let max_steps = 150;

        while pc < instrs.len() && steps < max_steps {
            steps += 1;
            let insn = &instrs[pc];
            let old_pc = pc;

            match insn.group {
                2 => {
                    // SET or SETSYSTEM
                    if insn.sub_group <= 1 {
                        let dst_reg = insn.dst;
                        let src_val = if insn.imm_op2 {
                            insn.src
                        } else {
                            gprs.get(&insn.src).copied().unwrap_or(0)
                        };
                        let dst_val = gprs.get(&dst_reg).copied().unwrap_or(0);

                        let result = match insn.set_opt {
                            0x01 => Some(src_val), // MOVE
                            0x02 => {
                                // SWAP
                                gprs.insert(insn.src, dst_val);
                                Some(src_val)
                            }
                            0x03 => Some(dst_val.wrapping_add(src_val)), // ADD
                            0x04 => Some(dst_val.wrapping_sub(src_val)), // SUB
                            0x05 => Some(dst_val.wrapping_mul(src_val)), // MUL
                            0x09 => Some(dst_val & src_val),             // AND
                            0x0A => Some(dst_val | src_val),             // OR
                            0x0B => Some(dst_val ^ src_val),             // XOR
                            0x0C => Some(dst_val | (1 << src_val)),      // BITSET
                            0x0D => Some(dst_val & !(1 << src_val)),     // BITCLR
                            0x0E => Some(dst_val << src_val),            // SHL
                            0x0F => Some(dst_val >> src_val),            // SHR
                            _ => None,
                        };

                        if let Some(val) = result {
                            let reg_name = if dst_reg >= 0x8000_0000 {
                                format!("PSR[{}]", dst_reg & 0x7FFF_FFFF)
                            } else {
                                format!("GPR[{dst_reg}]")
                            };
                            let op = match insn.set_opt {
                                1 => "=",
                                9 => "&=",
                                0xA => "|=",
                                0xB => "^=",
                                _ => "?=",
                            };
                            let src_desc = if insn.imm_op2 {
                                format!("{}", insn.src)
                            } else if insn.src >= 0x8000_0000 {
                                format!(
                                    "PSR[{}] (={})",
                                    insn.src & 0x7FFF_FFFF,
                                    gprs.get(&insn.src).copied().unwrap_or(0)
                                )
                            } else {
                                format!(
                                    "GPR[{}] (={})",
                                    insn.src,
                                    gprs.get(&insn.src).copied().unwrap_or(0)
                                )
                            };
                            gprs.insert(dst_reg, val);

                            // Highlight GPR[3002] writes
                            let marker = if dst_reg == 3002 { " ★★★" } else { "" };
                            eprintln!("  [{old_pc:4}] {reg_name} {op} {src_desc} → {val}{marker}");
                        } else {
                            eprintln!("  [{old_pc:4}] SET(opt={}) GPR[{}]", insn.set_opt, dst_reg);
                        }
                    } else {
                        eprintln!("  [{old_pc:4}] SET sub={}", insn.sub_group);
                    }
                    pc += 1;
                }
                1 => {
                    // CMP — skip next instruction if condition is false
                    let dst_val = if insn.imm_op1 {
                        insn.dst
                    } else {
                        gprs.get(&insn.dst).copied().unwrap_or(0)
                    };
                    let src_val = if insn.imm_op2 {
                        insn.src
                    } else {
                        gprs.get(&insn.src).copied().unwrap_or(0)
                    };
                    let condition = match insn.cmp_opt {
                        2 => dst_val == src_val,
                        3 => dst_val != src_val,
                        4 => dst_val >= src_val,
                        5 => dst_val > src_val,
                        6 => dst_val <= src_val,
                        7 => dst_val < src_val,
                        _ => false,
                    };
                    eprintln!("  [{old_pc:4}] CMP {dst_val} vs {src_val} → {condition}");
                    if condition {
                        pc += 1;
                    } else {
                        pc += 2; // skip next instruction
                    }
                }
                0 => {
                    // BRANCH
                    match insn.sub_group {
                        0 => {
                            // GOTO sub-group: NOP(0)/GOTO(1)/BREAK(2)
                            match insn.branch_opt {
                                0x01 => {
                                    // GOTO — unconditional jump
                                    let target = if insn.imm_op1 {
                                        insn.dst as usize
                                    } else {
                                        gprs.get(&insn.dst).copied().unwrap_or(0) as usize
                                    };
                                    eprintln!("  [{old_pc:4}] GOTO → {target} (taken)");
                                    pc = target;
                                }
                                0x02 => {
                                    eprintln!("  [{old_pc:4}] BREAK");
                                    pc = instrs.len(); // terminate
                                }
                                _ => {
                                    // NOP or unknown
                                    pc += 1;
                                }
                            }
                        }
                        2 => {
                            // PlayPl
                            let pl = if insn.imm_op1 {
                                insn.dst
                            } else {
                                gprs.get(&insn.dst).copied().unwrap_or(0)
                            };
                            eprintln!("  [{old_pc:4}] PlayPl({pl}) — stopping trace");
                            break;
                        }
                        _ => {
                            eprintln!("  [{old_pc:4}] BRANCH sub={}", insn.sub_group);
                            pc += 1;
                        }
                    }
                }
                _ => {
                    pc += 1;
                }
            }
        }
        if steps >= max_steps {
            eprintln!("  (trace limit reached at pc={pc})");
        }
        eprintln!("  GPR[3002] = {}", gprs.get(&3002).copied().unwrap_or(0));
        eprintln!("--- END VM TRACE ---");
    }

    // Show button command summary
    let mut gpr_regs: HashSet<u32> = HashSet::new();
    let mut gpr_values: Vec<u32> = Vec::new();
    for (button, _ctx) in ig_buttons {
        for cmd in &button.commands {
            if let NavigationCommand::SetGpr { register, value } = cmd {
                gpr_regs.insert(*register);
                if !gpr_values.contains(value) {
                    gpr_values.push(*value);
                }
            }
        }
    }
    gpr_values.sort_unstable();
    eprintln!(
        "\nButtons: {} total, dispatch registers: {:?}, sample keys: {:?}",
        ig_buttons.len(),
        gpr_regs,
        &gpr_values[..gpr_values.len().min(20)]
    );

    // Show first 10 buttons with their full command lists
    eprintln!("\nSample button commands:");
    for (button, ctx) in ig_buttons.iter().take(10) {
        let cmds: Vec<String> = button
            .commands
            .iter()
            .map(|c| match c {
                NavigationCommand::SetGpr { register, value } => {
                    format!("SetGpr({register}, {value})")
                }
                NavigationCommand::GotoMobj { object_id } => {
                    format!("GotoMobj({object_id})")
                }
                NavigationCommand::PlayPl {
                    playlist,
                    branch_opt,
                    mark_or_pi,
                } => match branch_opt {
                    1 => format!("PlayPlatMK({playlist}, mark={mark_or_pi})"),
                    2 => format!("PlayPlatPI({playlist}, pi={mark_or_pi})"),
                    _ => format!("PlayPl({playlist})"),
                },
                NavigationCommand::Other { opcode, dst, src } => {
                    let grp = (opcode >> 27) & 0x03;
                    let sub = (opcode >> 24) & 0x07;
                    let set_opt = opcode & 0x1F;
                    let imm1 = (opcode >> 23) & 1 != 0;
                    let imm2 = (opcode >> 22) & 1 != 0;
                    if grp == 2 && sub == 1 {
                        // SETSYSTEM
                        let op = match set_opt {
                            1 => "SET_STREAM",
                            2 => "SET_NV_TIMER",
                            3 => "SET_BUTTON_PAGE",
                            4 => "ENABLE_BUTTON",
                            5 => "DISABLE_BUTTON",
                            6 => "SET_SEC_STREAM",
                            7 => "POPUP_OFF",
                            _ => "?",
                        };
                        let d = if imm1 {
                            format!("{dst}")
                        } else {
                            format!("GPR[{dst}]")
                        };
                        let s = if imm2 {
                            format!("{src}")
                        } else {
                            format!("GPR[{src}]")
                        };
                        format!("SETSYSTEM({op}/{set_opt}, {d}, {s})")
                    } else if grp == 2 && sub == 0 {
                        // SET (non-immediate, not parsed as SetGpr)
                        let op = match set_opt {
                            1 => "=",
                            2 => "<=>",
                            3 => "+=",
                            4 => "-=",
                            9 => "&=",
                            0xA => "|=",
                            0xB => "^=",
                            _ => "?=",
                        };
                        let d = if imm1 {
                            format!("{dst}")
                        } else {
                            format!("GPR[{dst}]")
                        };
                        let s = if imm2 {
                            format!("{src}")
                        } else {
                            format!("GPR[{src}]")
                        };
                        format!("SET {d} {op} {s}")
                    } else {
                        format!("Other(grp={grp},sub={sub},dst={dst},src={src})")
                    }
                }
            })
            .collect();
        eprintln!(
            "  btn[{}] page={} ig={}: {}",
            button.button_id,
            ctx.page_id,
            ctx.ig_stream,
            cmds.join("; ")
        );
    }
    eprintln!("=== END TRACE ===\n");
}

/// Traces composite dispatch resolution per button.
///
/// For each button with a `SetGpr` command, shows the `button_id`, key,
/// composite value (`button_id + key`), IG clip, page, and whether
/// the composite matched a dispatch table case.
#[allow(clippy::print_stderr, reason = "diagnostic trace output")]
pub fn trace_composite_dispatch(
    ig_buttons: &[(
        reliquary::disc::bdmv::ig::Button,
        reliquary::disc::bdmv::mobj::PlayerContext,
    )],
    table: &reliquary::disc::bdmv::mobj::DispatchTable,
) {
    use reliquary::disc::bdmv::ig::NavigationCommand;

    eprintln!("\n=== COMPOSITE DISPATCH TRACE ===");
    eprintln!(
        "dispatch table: MOBJ[{}], {} cases on GPR[{}]",
        table.mobj_index,
        table.cases.len(),
        table.dispatch_register
    );

    let mut matched_cases: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut unmatched_count: u32 = 0;

    for (button, ctx) in ig_buttons {
        // Skip buttons with direct PlayPl
        let has_play_pl = button
            .commands
            .iter()
            .any(|c| matches!(c, NavigationCommand::PlayPl { .. }));
        if has_play_pl {
            continue;
        }

        // Find SetGpr assignments
        let gpr_assignments: Vec<(u32, u32)> = button
            .commands
            .iter()
            .filter_map(|c| match c {
                NavigationCommand::SetGpr { register, value } => Some((*register, *value)),
                _ => None,
            })
            .collect();

        if gpr_assignments.is_empty() {
            continue;
        }

        // Skip GotoMobj buttons (pattern 1 — not dispatch table)
        let has_goto_mobj = button
            .commands
            .iter()
            .any(|c| matches!(c, NavigationCommand::GotoMobj { .. }));
        if has_goto_mobj {
            continue;
        }

        for &(register, value) in &gpr_assignments {
            let composite = u32::from(button.button_id) + value;
            let case_match = table.cases.iter().find(|(cv, _)| *cv == composite);

            if let Some(&(_, playlist)) = case_match {
                matched_cases.insert(composite);
                eprintln!(
                    "  btn[{:4}] ig={:05} page={} GPR[{}]={} composite={:3} → playlist {:3}",
                    button.button_id,
                    ctx.ig_stream,
                    ctx.page_id,
                    register,
                    value,
                    composite,
                    playlist
                );
            } else {
                unmatched_count += 1;
                eprintln!(
                    "  btn[{:4}] ig={:05} page={} GPR[{}]={} composite={:3} → NO MATCH",
                    button.button_id, ctx.ig_stream, ctx.page_id, register, value, composite
                );
            }
        }
    }

    // Summary: which table cases were reached, which were not
    let all_cases: std::collections::HashSet<u32> = table.cases.iter().map(|(cv, _)| *cv).collect();
    let unreached: Vec<(u32, u16)> = table
        .cases
        .iter()
        .filter(|(cv, _)| !matched_cases.contains(cv))
        .copied()
        .collect();

    eprintln!(
        "\n  matched: {} of {} cases, {} buttons unmatched",
        matched_cases.len(),
        all_cases.len(),
        unmatched_count
    );
    if !unreached.is_empty() {
        eprintln!("  unreached cases:");
        for (cv, pl) in &unreached {
            eprintln!("    case {cv} → playlist {pl}");
        }
    }
    eprintln!("=== END COMPOSITE DISPATCH TRACE ===\n");
}

/// Traces dispatch handler execution: for each case, runs the handler
/// and dumps the resulting GPR[3xxx] state.
#[allow(clippy::print_stderr, reason = "diagnostic trace output")]
pub fn trace_dispatch_handlers(
    mobj_file: &reliquary::disc::bdmv::mobj::MovieObjectFile,
    table: &reliquary::disc::bdmv::mobj::DispatchTable,
) {
    use reliquary::disc::bdmv::mobj;

    eprintln!("\n=== DISPATCH HANDLER TRACE ===");
    let dispatch_instrs = &mobj_file.objects[table.mobj_index].instructions;

    for &(case_val, playlist) in &table.cases {
        let handler_pc = mobj::find_handler_pc(dispatch_instrs, case_val, table.dispatch_register);
        let Some(pc) = handler_pc else {
            eprintln!("  case {case_val:3} → pl {playlist:3}: handler PC NOT FOUND");
            continue;
        };

        let mut gprs = std::collections::HashMap::<u32, u32>::new();
        let _ = mobj::run_mobj_vm(
            dispatch_instrs,
            pc,
            &mut gprs,
            &std::collections::HashSet::new(),
        );

        // Show only GPR[3xxx] entries (the configuration registers)
        let mut config_gprs: Vec<(u32, u32)> = gprs
            .iter()
            .filter(|&(&k, _)| (3000..4000).contains(&k))
            .map(|(&k, &v)| (k, v))
            .collect();
        config_gprs.sort_by_key(|&(k, _)| k);

        let gpr_summary: Vec<String> = config_gprs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        eprintln!(
            "  case {case_val:3} → pl {playlist:3} (pc={pc:4}): {}",
            if gpr_summary.is_empty() {
                "(no GPR[3xxx] set)".to_string()
            } else {
                gpr_summary.join(", ")
            }
        );
    }
    eprintln!("=== END DISPATCH HANDLER TRACE ===\n");
}

/// Traces the GPR database state populated by MOBJ[0] (First Play).
///
/// Executes MOBJ[0] with BD spec boot PSR defaults and dumps all
/// GPR[3000–3999] registers — the per-content-item configuration
/// database that data-driven button programs read. Highlights known
/// content config registers (3563, 3773, 3774, 3775, 3776).
#[allow(clippy::print_stderr, reason = "diagnostic trace output")]
pub fn trace_mobj0_database(mobj_file: &reliquary::disc::bdmv::mobj::MovieObjectFile) {
    use reliquary::disc::bdmv::mobj;

    eprintln!("\n=== MOBJ[0] DATABASE ===");

    let Some(mobj0) = mobj_file.objects.first() else {
        eprintln!("  (no MOBJ[0])");
        eprintln!("=== END MOBJ[0] DATABASE ===\n");
        return;
    };

    eprintln!("{} instructions", mobj0.instructions.len());

    // Execute MOBJ[0] with BD spec boot PSR defaults — same seeds
    // as resolve_via_execution uses.
    let psr: u32 = 0x8000_0000;
    let mut gprs = std::collections::HashMap::<u32, u32>::new();
    gprs.insert(psr | 0x01, 0xFF); // primary audio
    gprs.insert(psr | 0x02, 0xFFFE); // PG/TextST
    gprs.insert(psr | 0x03, 0xFF); // angle
    gprs.insert(psr | 0x04, 0xFFFF); // title (init guard)
    gprs.insert(psr | 0x0A, 0xFFFF); // selected button
    gprs.insert(psr | 0x0C, 0xFF); // user style
    gprs.insert(psr | 0x0D, 0xFF); // parental level
    gprs.insert(psr | 0x0E, 0xFFFF); // secondary A/V
    gprs.insert(psr | 0x0F, 0x0002_0000); // audio cap
    gprs.insert(psr | 0x1D, 0x0200); // profile 2.0
    gprs.insert(psr | 0x1F, 0x0200); // player version

    let _ = mobj::run_mobj_vm(
        &mobj0.instructions,
        0,
        &mut gprs,
        &std::collections::HashSet::new(),
    );

    // Collect GPR[3xxx] entries (disc database)
    let mut db_entries: Vec<(u32, u32)> = gprs
        .iter()
        .filter(|&(k, _)| (3000..4000).contains(k))
        .map(|(&k, &v)| (k, v))
        .collect();
    db_entries.sort_by_key(|&(k, _)| k);

    if db_entries.is_empty() {
        eprintln!("  (no GPR[3xxx] entries — disc has no database)");
    } else {
        let first = db_entries.first().map_or(0, |&(k, _)| k);
        let last = db_entries.last().map_or(0, |&(k, _)| k);
        eprintln!("  {} registers in GPR[{first}–{last}]", db_entries.len());

        // Highlight known content config registers
        let config_labels: &[(u32, &str)] = &[
            (3563, "content_index"),
            (3773, "page"),
            (3774, "items_per_page"),
            (3775, "parameter"),
            (3776, "button_id"),
        ];
        eprintln!("  content config:");
        for &(reg, label) in config_labels {
            if let Some(&(_, val)) = db_entries.iter().find(|(k, _)| *k == reg) {
                eprintln!("    GPR[{reg}] = {val:6}  ({label})");
            } else {
                eprintln!("    GPR[{reg}] = (unset)  ({label})");
            }
        }

        // Compact dump of all database entries (8 per line)
        eprintln!("  full dump:");
        for chunk in db_entries.chunks(8) {
            let entries: Vec<String> = chunk.iter().map(|(k, v)| format!("{k}={v}")).collect();
            eprintln!("    {}", entries.join("  "));
        }
    }

    // Show GPR[4xxx] working registers (used as scratch by button programs)
    let mut work_entries: Vec<(u32, u32)> = gprs
        .iter()
        .filter(|&(k, _)| (4000..5000).contains(k))
        .map(|(&k, &v)| (k, v))
        .collect();
    work_entries.sort_by_key(|&(k, _)| k);
    if !work_entries.is_empty() {
        let summary: Vec<String> = work_entries
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        eprintln!(
            "  {} GPR[4xxx] working: {}",
            work_entries.len(),
            summary.join(", ")
        );
    }

    eprintln!("=== END MOBJ[0] DATABASE ===\n");
}

/// Traces execution resolution coverage against the dispatch table.
///
/// Shows which dispatch cases were resolved by the BFS executor, and
/// for each unresolved case, reverse-computes which `(button_id, key)`
/// pair could produce it. Checks whether a matching `SetGpr(4075, key)`
/// button exists on any IG page. This identifies whether unresolved
/// extras need new GPR states on existing pages vs entirely new button
/// discovery.
#[allow(clippy::print_stderr, reason = "diagnostic trace output")]
#[allow(
    clippy::too_many_lines,
    reason = "reverse lookup with inventory collection is inherently long"
)]
pub fn trace_execution_coverage(
    exec_resolved: &[reliquary::disc::bdmv::mobj::ResolvedPlaylist],
    table: &reliquary::disc::bdmv::mobj::DispatchTable,
    clip_pages: &[(u16, Vec<reliquary::disc::bdmv::ig::Page>)],
) {
    use reliquary::disc::bdmv::ig::NavigationCommand;

    eprintln!("\n=== EXECUTION COVERAGE ===");
    eprintln!(
        "dispatch table: MOBJ[{}], {} cases on GPR[{}]",
        table.mobj_index,
        table.cases.len(),
        table.dispatch_register
    );

    // Map resolved playlists to dispatch cases
    let mut resolved_cases = std::collections::HashSet::<u32>::new();

    for rp in exec_resolved {
        let btn_label = rp
            .breadcrumb
            .iter()
            .map(|s| format!("{}", s.button_id))
            .collect::<Vec<_>>()
            .join("→");
        let case_match: Vec<u32> = table
            .cases
            .iter()
            .filter(|(_, pl)| *pl == rp.target.playlist)
            .map(|(cv, _)| *cv)
            .collect();
        for &cv in &case_match {
            resolved_cases.insert(cv);
        }
        if case_match.is_empty() {
            eprintln!(
                "  resolved: pl {:3} (btn[{btn_label}]) — not in dispatch table",
                rp.target.playlist,
            );
        } else {
            for cv in &case_match {
                eprintln!(
                    "  resolved: case {:3} -> pl {:3} (btn[{btn_label}])",
                    cv, rp.target.playlist,
                );
            }
        }
    }

    // Find unresolved cases
    let unresolved: Vec<(u32, u16)> = table
        .cases
        .iter()
        .filter(|(cv, _)| !resolved_cases.contains(cv))
        .copied()
        .collect();

    eprintln!(
        "\n  {} of {} dispatch cases resolved",
        resolved_cases.len(),
        table.cases.len()
    );

    if unresolved.is_empty() {
        eprintln!("  all cases covered!");
        eprintln!("=== END EXECUTION COVERAGE ===\n");
        return;
    }

    // Build button inventory from the first clip only — all clips share
    // the same page/button structure (language variants), so using the
    // first avoids N× duplication in the reverse lookup.
    #[allow(
        clippy::items_after_statements,
        reason = "ButtonInfo is local to this trace function"
    )]
    struct ButtonInfo {
        button_id: u16,
        page_id: u8,
        key_4075: Option<u32>,
        has_complex: bool,
    }

    let mut inventory: Vec<ButtonInfo> = Vec::new();
    let mut seen = std::collections::HashSet::<(u16, u8)>::new();
    for (_, pages) in clip_pages {
        for page in pages {
            for button in &page.buttons {
                if !seen.insert((button.button_id, page.page_id)) {
                    continue;
                }
                let key_4075 = button.commands.iter().find_map(|c| {
                    if let NavigationCommand::SetGpr {
                        register: 4075,
                        value,
                    } = c
                    {
                        Some(*value)
                    } else {
                        None
                    }
                });
                let has_complex = button
                    .commands
                    .iter()
                    .any(|c| matches!(c, NavigationCommand::Other { .. }));
                inventory.push(ButtonInfo {
                    button_id: button.button_id,
                    page_id: page.page_id,
                    key_4075,
                    has_complex,
                });
            }
        }
    }

    let unique_pages: std::collections::HashSet<u8> = inventory.iter().map(|b| b.page_id).collect();
    eprintln!(
        "  {} cases unresolved — reverse lookup ({} unique buttons across {} pages):",
        unresolved.len(),
        inventory.len(),
        unique_pages.len()
    );

    for &(case_val, playlist) in &unresolved {
        let mut exact_matches: Vec<String> = Vec::new();
        let mut near_miss_count: u32 = 0;
        // Complex candidates grouped by page: page_id → Vec<(button_id, needed_key)>
        let mut complex_by_page = std::collections::BTreeMap::<u8, Vec<(u16, u32)>>::new();

        for bi in &inventory {
            let Some(needed_key) = case_val.checked_sub(u32::from(bi.button_id)) else {
                continue;
            };

            // Complex buttons may have SetGpr(4075, N) as an early
            // instruction (e.g. items_per_page) that is overwritten by
            // subsequent computed operations. Treat them as complex
            // candidates regardless of the initial SetGpr value.
            if bi.has_complex {
                complex_by_page
                    .entry(bi.page_id)
                    .or_default()
                    .push((bi.button_id, needed_key));
            } else {
                match bi.key_4075 {
                    Some(actual_key) if actual_key == needed_key => {
                        exact_matches.push(format!(
                            "btn[{}] page={} SetGpr(4075, {needed_key})",
                            bi.button_id, bi.page_id
                        ));
                    }
                    Some(_) => {
                        near_miss_count += 1;
                    }
                    None => {}
                }
            }
        }

        if exact_matches.is_empty() && near_miss_count == 0 && complex_by_page.is_empty() {
            eprintln!("    case {case_val:3} -> pl {playlist:3}: (no candidate buttons)");
            continue;
        }

        eprintln!("    case {case_val:3} -> pl {playlist:3}:");
        for m in &exact_matches {
            eprintln!("      match: {m}");
        }
        if near_miss_count > 0 {
            eprintln!("      {near_miss_count} near-miss buttons (wrong key)");
        }
        if !complex_by_page.is_empty() {
            let total: usize = complex_by_page.values().map(Vec::len).sum();
            eprintln!("      {total} complex buttons (computed key):");
            for (page_id, btns) in &complex_by_page {
                let btn_ids: Vec<String> = btns
                    .iter()
                    .map(|(id, key)| format!("btn[{id}]+{key}"))
                    .collect();
                eprintln!("        page {page_id}: {}", btn_ids.join(", "));
            }
        }
    }

    eprintln!("=== END EXECUTION COVERAGE ===\n");
}

/// Traces direct `PlayPl` buttons — those resolved by the IG parser, not
/// the BFS executor.
///
/// Shows which clip each direct `PlayPl` button lives on and whether the
/// execution resolver also covers that playlist. Buttons on non-navigable
/// clips (pop-up menus) show bitmaps from the wrong context; playlists
/// only reachable via direct `PlayPl` on a pop-up clip may be missing
/// from the main menu's navigation graph entirely.
#[allow(clippy::print_stderr, reason = "diagnostic trace output")]
pub fn trace_direct_play_pl(
    buttons: &[ExtractedButton],
    exec_resolved: &[reliquary::disc::bdmv::mobj::ResolvedPlaylist],
) {
    let direct: Vec<&ExtractedButton> = buttons
        .iter()
        .filter(|b| b.playlist.is_some() && b.breadcrumb.is_empty() && !b.orphan)
        .collect();

    if direct.is_empty() {
        return;
    }

    let exec_playlists: std::collections::HashSet<u16> =
        exec_resolved.iter().map(|rp| rp.target.playlist).collect();

    eprintln!("\n=== DIRECT PlayPl BUTTONS ===");
    eprintln!(
        "{} buttons with inline PlayPl (no breadcrumb, not orphan):",
        direct.len()
    );

    // Group by clip_index for readability
    let mut by_clip = std::collections::BTreeMap::<usize, Vec<&ExtractedButton>>::new();
    for b in &direct {
        by_clip.entry(b.clip_index).or_default().push(b);
    }

    for (clip_idx, clip_buttons) in &by_clip {
        eprintln!("  clip[{clip_idx}]:");
        for b in clip_buttons {
            let Some(pl) = b.playlist else {
                continue;
            };
            let also_exec = if exec_playlists.contains(&pl) {
                " (also resolved via execution)"
            } else {
                " (UNIQUE — not in execution results)"
            };
            let variant = match b.branch_opt {
                1 => format!(" @mark {}", b.mark_or_pi),
                2 => format!(" @PI {}", b.mark_or_pi),
                _ => String::new(),
            };
            eprintln!(
                "    btn[{:3}] page={} -> PL {:3}{variant}{also_exec}",
                b.button_id, b.page_id, pl,
            );
        }
    }

    eprintln!("=== END DIRECT PlayPl ===\n");
}
