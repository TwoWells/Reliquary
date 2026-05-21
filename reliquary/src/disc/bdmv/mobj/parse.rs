// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! MOBJ file parser and instruction encoding.

use super::super::cursor::Cursor;
use super::super::ig::NavigationCommand;
use super::{
    BRANCH_GOTO, BRANCH_JUMP, BRANCH_PLAY, GRP_BRANCH, GRP_CMP, GRP_SET, Instruction, MobjError,
    MovieObject, MovieObjectFile, PSR_FLAG, SET_BUTTON_PAGE_OPT,
};

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

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod tests {
    use super::super::test_helpers::{InsnSpec, MobjBuilder, spec_to_raw};
    use super::*;

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
}
