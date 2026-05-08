// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Reliquary CLI — command-line interface for physical media preservation.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Reliquary — physical media preservation toolkit.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Available subcommands.
#[derive(Subcommand)]
enum Command {
    /// Inspect a disc — show structure, playlists, streams, and main title.
    Inspect {
        /// Path to an ISO image or extracted disc folder.
        path: PathBuf,

        /// Output as JSON instead of a human-readable table.
        #[arg(long)]
        json: bool,
    },

    /// Identify disc content — extract menu button bitmaps and name extras.
    Identify {
        /// Path to an ISO image or extracted disc folder.
        path: PathBuf,

        /// Volume Unique Key as a 32-character hex string (overrides KEYDB.cfg lookup).
        #[arg(long)]
        vuk: Option<String>,

        /// Path to `KEYDB.cfg` (default: `$XDG_CONFIG_HOME/aacs/KEYDB.cfg`).
        #[arg(long)]
        keydb: Option<PathBuf>,

        /// Skip KEYDB.cfg lookup.
        #[arg(long)]
        no_keydb: bool,

        /// Output as JSON instead of a text report.
        #[arg(long)]
        json: bool,

        /// Skip bitmap rendering (text-only mode).
        #[arg(long)]
        no_images: bool,

        /// Dump MOBJ instruction trace for debugging GPR dispatch resolution.
        #[arg(long)]
        trace: bool,
    },

    /// Decrypt an AACS-encrypted Blu-ray disc or single clip.
    Decrypt {
        /// Path to an ISO image or extracted disc folder.
        path: PathBuf,

        /// Volume Unique Key as a 32-character hex string (overrides lookup).
        #[arg(long)]
        vuk: Option<String>,

        /// Decrypt a single clip instead of the whole disc (e.g. "00100").
        #[arg(long)]
        clip: Option<String>,

        /// Path to `KEYDB.cfg` (default: `$XDG_CONFIG_HOME/aacs/KEYDB.cfg`).
        #[arg(long)]
        keydb: Option<PathBuf>,

        /// Skip KEYDB.cfg lookup.
        #[arg(long)]
        no_keydb: bool,

        /// Output path (ISO/directory for whole-disc, file for per-clip).
        #[arg(short, long)]
        output: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Command::Inspect { path, json } => run_inspect(&path, json),
        Command::Identify {
            path,
            vuk,
            keydb,
            no_keydb,
            json,
            no_images,
            trace,
        } => run_identify(
            &path,
            vuk.as_deref(),
            keydb.as_deref(),
            no_keydb,
            json,
            no_images,
            trace,
        ),
        Command::Decrypt {
            path,
            vuk,
            clip,
            keydb,
            no_keydb,
            output,
        } => run_decrypt(
            &path,
            vuk.as_deref(),
            clip.as_deref(),
            keydb.as_deref(),
            no_keydb,
            &output,
        ),
    }
}

/// Runs the `inspect` subcommand.
fn run_inspect(path: &std::path::Path, json: bool) -> ExitCode {
    match reliquary::disc::inspect(path) {
        Ok(result) => {
            if json {
                match serde_json::to_string_pretty(&result) {
                    Ok(output) => {
                        // Use write! to stdout — print_stdout is denied by clippy config.
                        // This is the CLI crate's presentation layer, so stdout is correct.
                        #[allow(clippy::print_stdout, reason = "CLI output to stdout")]
                        {
                            println!("{output}");
                        }
                    }
                    Err(e) => {
                        #[allow(clippy::print_stderr, reason = "CLI error output")]
                        {
                            eprintln!("error: failed to serialize JSON: {e}");
                        }
                        return ExitCode::FAILURE;
                    }
                }
            } else {
                #[allow(clippy::print_stdout, reason = "CLI output to stdout")]
                {
                    print!("{result}");
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            #[allow(clippy::print_stderr, reason = "CLI error output")]
            {
                eprintln!("error: {e}");
            }
            ExitCode::FAILURE
        }
    }
}

// ── Identify ──────────────────────────────────────────────────────────────

/// A button extracted from the IG stream with its decoded bitmap.
struct ExtractedButton {
    /// Playlist number if the button has a `PlayPl` command.
    playlist: Option<u16>,
    /// Button identifier from the IG data.
    button_id: u16,
    /// Bitmap width in pixels.
    width: u16,
    /// Bitmap height in pixels.
    height: u16,
    /// RGBA pixel data (4 bytes per pixel, row-major).
    rgba: Vec<u8>,
}

/// A named content item — user's response for one button.
struct NamedItem {
    /// Playlist number.
    playlist: u16,
    /// User-provided name (empty string means skipped).
    name: String,
}

/// Runs the `identify` subcommand — full IG pipeline with interactive naming.
#[allow(clippy::print_stderr, reason = "CLI status and error output")]
#[allow(
    clippy::too_many_lines,
    reason = "pipeline orchestration is inherently sequential"
)]
#[allow(
    clippy::fn_params_excessive_bools,
    reason = "CLI flag pass-through, not a public API"
)]
fn run_identify(
    path: &std::path::Path,
    vuk_hex: Option<&str>,
    keydb: Option<&std::path::Path>,
    no_keydb: bool,
    json: bool,
    no_images: bool,
    trace: bool,
) -> ExitCode {
    // Auto-disable images when stderr is not a terminal (images render to stderr)
    let no_images = no_images || !std::io::IsTerminal::is_terminal(&std::io::stderr());

    // 1. Open disc
    let reader = match reliquary::disc::reader::DiscReader::open(path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    // 2. Analyze disc structure (playlists, streams, IG clips)
    eprintln!("analyzing disc...");
    let analysis = match reliquary::disc::bdmv::analyze(&reader) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    if analysis.ig_clips.is_empty() {
        eprintln!("no IG clips found — disc has no HDMV menus to identify");
        return ExitCode::FAILURE;
    }

    // 3. Resolve VUK (optional — absent for unencrypted discs)
    let vuk = match resolve_vuk_for_identify(&reader, vuk_hex, keydb, no_keydb) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };

    // 4. Extract buttons from all IG clips
    let clip_summary: Vec<String> = analysis
        .ig_clips
        .iter()
        .map(|c| format!("{} ({})", c.clip_id, format_size(c.file_size)))
        .collect();
    eprintln!(
        "found {} IG clips: {}",
        analysis.ig_clips.len(),
        clip_summary.join(", ")
    );

    let buttons = match extract_buttons(&reader, &analysis, vuk.as_ref(), trace) {
        Ok(b) => b,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };

    // 5. Filter: content buttons (have PlayPl targeting a valid playlist)
    let valid_playlists: HashSet<u32> = analysis.playlists.iter().map(|p| p.number).collect();

    let content_buttons: Vec<&ExtractedButton> = {
        let mut seen = HashSet::new();
        buttons
            .iter()
            .filter(|b| {
                b.playlist
                    .is_some_and(|pl| valid_playlists.contains(&u32::from(pl)) && seen.insert(pl))
            })
            .collect()
    };

    // 6. Present buttons and collect names
    let items = if content_buttons.is_empty() {
        eprintln!("no buttons with playlist targets found — showing all buttons");
        eprintln!();
        // Print playlist table for manual correlation
        eprintln!("{analysis}");
        prompt_fallback_buttons(&buttons, &analysis, no_images)
    } else {
        eprintln!(
            "found {} buttons with playlist targets",
            content_buttons.len()
        );
        eprintln!();
        prompt_content_buttons(&content_buttons, &analysis, no_images)
    };

    // 7. Output results
    if json {
        output_json(path, &items, &analysis);
    } else {
        output_text(path, &items);
    }

    ExitCode::SUCCESS
}

/// Resolves the VUK for the identify command.
///
/// Returns `None` when the disc is not AACS-encrypted (no `Unit_Key_RO.inf`).
/// Errors when AACS is detected but no VUK can be found.
#[allow(clippy::print_stderr, reason = "CLI status output")]
fn resolve_vuk_for_identify(
    reader: &reliquary::disc::reader::DiscReader,
    vuk_hex: Option<&str>,
    keydb: Option<&std::path::Path>,
    no_keydb: bool,
) -> Result<Option<[u8; 16]>, String> {
    if let Some(hex) = vuk_hex {
        return Ok(Some(parse_vuk(hex)?));
    }

    // Try to read Unit_Key_RO.inf — if absent, disc is not AACS encrypted
    let Ok(uk_data) = reliquary::disc::bdmv::aacs::read_unit_key_data(reader) else {
        return Ok(None);
    };

    let disc_id = reliquary::disc::bdmv::aacs::disc_id_from_data(&uk_data);
    eprintln!("disc ID: {disc_id}");

    if no_keydb {
        return Err(format!(
            "VUK not provided and KEYDB.cfg lookup disabled — use --vuk <hex> (disc ID: {disc_id})"
        ));
    }

    let keydb_path = keydb.map_or_else(
        reliquary::disc::bdmv::keydb::default_keydb_path,
        std::path::PathBuf::from,
    );

    match reliquary::disc::bdmv::keydb::lookup_keydb(&keydb_path, &disc_id) {
        Ok(Some(vuk)) => {
            eprintln!("VUK found in KEYDB.cfg");
            Ok(Some(vuk))
        }
        Ok(None) => Err(format!(
            "VUK not found in KEYDB.cfg — use --vuk <hex> (disc ID: {disc_id})"
        )),
        Err(e) => Err(e.to_string()),
    }
}

/// Extracts all buttons with decoded bitmaps from IG clips.
///
/// Runs the full pipeline for each IG clip: read → demux → parse PES →
/// parse IG segments → decode RLE bitmaps. Then resolves indirect
/// button→playlist mappings via `MovieObject.bdmv` tracing.
#[allow(clippy::print_stderr, reason = "CLI warning output")]
fn extract_buttons(
    reader: &reliquary::disc::reader::DiscReader,
    analysis: &reliquary::disc::bdmv::BdmvAnalysis,
    vuk: Option<&[u8; 16]>,
    trace: bool,
) -> Result<Vec<ExtractedButton>, String> {
    use reliquary::disc::bdmv::{ig, read_clip, rle, ts};

    use reliquary::disc::bdmv::mobj::PlayerContext;

    let mut buttons = Vec::new();
    // Collect raw IG buttons with player context for MOBJ resolution
    let mut ig_buttons: Vec<(ig::Button, PlayerContext)> = Vec::new();

    for ig_clip in &analysis.ig_clips {
        // Read clip (decrypting if VUK available)
        let data = read_clip(reader, vuk, &ig_clip.clip_id)
            .map_err(|e| format!("failed to read clip {}: {e}", ig_clip.clip_id))?;

        // Demux MPEG-TS
        let pes_packets = ts::demux(&data)
            .map_err(|e| format!("failed to demux clip {}: {e}", ig_clip.clip_id))?;

        // Process each IG stream PID in this clip
        let Some(ig_stream_info) = ig_clip.ig_streams.first() else {
            continue;
        };
        let ig_pid = ig_stream_info.pid;

        // Filter PES by IG PID, parse headers, concatenate payloads
        let mut ig_payload = Vec::new();
        for pes in &pes_packets {
            if pes.pid == ig_pid {
                match ts::parse_pes(&pes.data) {
                    Ok(parsed) => ig_payload.extend_from_slice(&parsed.payload),
                    Err(e) => {
                        eprintln!("warning: PES parse error in clip {}: {e}", ig_clip.clip_id);
                    }
                }
            }
        }

        if ig_payload.is_empty() {
            continue;
        }

        // Parse IG segments
        let ig_stream = ig::parse(&ig_payload)
            .map_err(|e| format!("failed to parse IG in clip {}: {e}", ig_clip.clip_id))?;

        // Collect buttons from all display sets
        for ds in &ig_stream.display_sets {
            let Some(palette) = ds.palettes.first() else {
                continue;
            };

            for comp in &ds.compositions {
                for page in &comp.pages {
                    for button in &page.buttons {
                        // Decode bitmap
                        let obj = ds
                            .objects
                            .iter()
                            .find(|o| o.object_id == button.normal_object_id);

                        let Some(obj) = obj else { continue };

                        let bitmap = match rle::decode(obj, palette) {
                            Ok(b) => b,
                            Err(e) => {
                                eprintln!(
                                    "warning: RLE decode failed for object {}: {e}",
                                    button.normal_object_id
                                );
                                continue;
                            }
                        };

                        // Find PlayPl command if any
                        let playlist = button.commands.iter().find_map(|cmd| {
                            if let ig::NavigationCommand::PlayPl { playlist } = cmd {
                                Some(*playlist)
                            } else {
                                None
                            }
                        });

                        buttons.push(ExtractedButton {
                            playlist,
                            button_id: button.button_id,
                            width: bitmap.width,
                            height: bitmap.height,
                            rgba: bitmap.data,
                        });

                        // Clone the IG button for MOBJ resolution (only if
                        // no direct PlayPl — avoids unnecessary cloning)
                        if playlist.is_none() {
                            ig_buttons.push((
                                ig::Button {
                                    button_id: button.button_id,
                                    x: button.x,
                                    y: button.y,
                                    normal_object_id: button.normal_object_id,
                                    selected_object_id: button.selected_object_id,
                                    commands: button.commands.clone(),
                                },
                                PlayerContext {
                                    ig_stream: ig_pid,
                                    selected_button_id: button.button_id,
                                    page_id: page.page_id,
                                },
                            ));
                        }
                    }
                }
            }
        }
    }

    // Resolve indirect button → playlist mappings via MovieObject.bdmv
    if !ig_buttons.is_empty() {
        let valid_playlists: HashSet<u32> = analysis.playlists.iter().map(|p| p.number).collect();
        resolve_mobj_buttons(
            reader,
            &ig_buttons,
            &mut buttons,
            &valid_playlists,
            &analysis.menu_playlists,
            trace,
        );
    }

    Ok(buttons)
}

/// Resolves indirect button→playlist mappings via `MovieObject.bdmv`.
///
/// Reads and parses the MOBJ file, finds dispatch entry points from
/// menu playlist references, runs the resolver, and fills in `playlist`
/// fields on `ExtractedButton`s that were `None`.
#[allow(clippy::print_stderr, reason = "CLI status output")]
fn resolve_mobj_buttons(
    reader: &reliquary::disc::reader::DiscReader,
    ig_buttons: &[(
        reliquary::disc::bdmv::ig::Button,
        reliquary::disc::bdmv::mobj::PlayerContext,
    )],
    buttons: &mut [ExtractedButton],
    valid_playlists: &HashSet<u32>,
    menu_playlists: &[u32],
    trace: bool,
) {
    use reliquary::disc::bdmv::mobj;

    // Try both paths for MovieObject.bdmv
    let mobj_path = std::path::Path::new("BDMV/MovieObject.bdmv");
    let mobj_alt = std::path::Path::new("MovieObject.bdmv");

    let mobj_data = match reader.read_file(mobj_path) {
        Ok(data) => data,
        Err(_) => {
            if let Ok(data) = reader.read_file(mobj_alt) {
                data
            } else {
                eprintln!("warning: MovieObject.bdmv not found — skipping MOBJ resolution");
                return;
            }
        }
    };

    let mobj_file = match mobj::parse(&mobj_data) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("warning: failed to parse MovieObject.bdmv: {e}");
            return;
        }
    };

    if trace {
        trace_mobj_structure(&mobj_file, valid_playlists, ig_buttons);
    }

    // Find dispatch entry points for GPR dispatch resolution
    let menu_set: HashSet<u32> = menu_playlists.iter().copied().collect();
    let dispatch_entries = mobj::find_dispatch_entries(&mobj_file, &menu_set);

    if !dispatch_entries.is_empty() {
        eprintln!(
            "found {} MOBJ dispatch entry points from menu playlists",
            dispatch_entries.len()
        );
    }

    let resolved =
        mobj::resolve_buttons(ig_buttons, &mobj_file, valid_playlists, &dispatch_entries);

    if !resolved.is_empty() {
        eprintln!(
            "resolved {} button playlist mappings via MovieObject.bdmv",
            resolved.len()
        );
    }

    // Fill in resolved playlists on the extracted buttons
    for rb in &resolved {
        if let Some(eb) = buttons
            .iter_mut()
            .find(|b| b.button_id == rb.button_id && b.playlist.is_none())
        {
            eb.playlist = Some(rb.playlist);
        }
    }
}

/// Dumps MOBJ structure and instruction trace for debugging.
#[allow(clippy::print_stderr, reason = "diagnostic trace output")]
#[allow(
    clippy::too_many_lines,
    reason = "diagnostic dump with inline formatting"
)]
fn trace_mobj_structure(
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
                for ctx_pc in start..end {
                    let ci = &instrs[ctx_pc];
                    let desc = match (ci.group, ci.sub_group) {
                        (2, 0) => {
                            let op = match ci.set_opt {
                                0 => "=", 8 => "&=", 9 => "|=", 0xA => "^=", _ => "?=",
                            };
                            let s = if ci.imm_op2 { format!("{}", ci.src) } else if ci.src >= 0x8000_0000 { format!("PSR[{}]", ci.src & 0x7FFF_FFFF) } else { format!("GPR[{}]", ci.src) };
                            format!("SET GPR[{}] {op} {s}", ci.dst)
                        }
                        (1, _) => {
                            let d = if ci.imm_op1 { format!("{}", ci.dst) } else { format!("GPR[{}]", ci.dst) };
                            let s = if ci.imm_op2 { format!("{}", ci.src) } else { format!("GPR[{}]", ci.src) };
                            format!("CMP {d} ?? {s}")
                        }
                        (0, 0) => format!("GOTO(opt={}) → {}", ci.branch_opt, ci.dst),
                        (0, 1) => format!("JUMP → MOBJ[{}]", ci.dst),
                        (0, 2) => if ci.imm_op1 { format!("PlayPl({})", ci.dst) } else { format!("PlayPl(GPR[{}])", ci.dst) },
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
                            0x01 => Some(src_val),                          // MOVE
                            0x02 => {                                       // SWAP
                                gprs.insert(insn.src, dst_val);
                                Some(src_val)
                            }
                            0x03 => Some(dst_val.wrapping_add(src_val)),    // ADD
                            0x04 => Some(dst_val.wrapping_sub(src_val)),    // SUB
                            0x05 => Some(dst_val.wrapping_mul(src_val)),    // MUL
                            0x09 => Some(dst_val & src_val),                // AND
                            0x0A => Some(dst_val | src_val),                // OR
                            0x0B => Some(dst_val ^ src_val),                // XOR
                            0x0C => Some(dst_val | (1 << src_val)),         // BITSET
                            0x0D => Some(dst_val & !(1 << src_val)),        // BITCLR
                            0x0E => Some(dst_val << src_val),               // SHL
                            0x0F => Some(dst_val >> src_val),               // SHR
                            _ => None,
                        };

                        if let Some(val) = result {
                            let reg_name = if dst_reg >= 0x8000_0000 {
                                format!("PSR[{}]", dst_reg & 0x7FFF_FFFF)
                            } else {
                                format!("GPR[{dst_reg}]")
                            };
                            let op = match insn.set_opt {
                                1 => "=", 9 => "&=", 0xA => "|=", 0xB => "^=",
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
                            eprintln!(
                                "  [{old_pc:4}] {reg_name} {op} {src_desc} → {val}{marker}"
                            );
                        } else {
                            eprintln!("  [{old_pc:4}] SET(opt={}) GPR[{}]", insn.set_opt, dst_reg);
                        }
                        pc += 1;
                    } else {
                        eprintln!("  [{old_pc:4}] SET sub={}", insn.sub_group);
                        pc += 1;
                    }
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
                    eprintln!(
                        "  [{old_pc:4}] CMP {dst_val} vs {src_val} → {condition}"
                    );
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
                                    eprintln!(
                                        "  [{old_pc:4}] GOTO → {target} (taken)"
                                    );
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
                            eprintln!(
                                "  [{old_pc:4}] PlayPl({pl}) — stopping trace"
                            );
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
        eprintln!(
            "  GPR[3002] = {}",
            gprs.get(&3002).copied().unwrap_or(0)
        );
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
                NavigationCommand::PlayPl { playlist } => {
                    format!("PlayPl({playlist})")
                }
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
                        let d = if imm1 { format!("{dst}") } else { format!("GPR[{dst}]") };
                        let s = if imm2 { format!("{src}") } else { format!("GPR[{src}]") };
                        format!("SETSYSTEM({op}/{set_opt}, {d}, {s})")
                    } else if grp == 2 && sub == 0 {
                        // SET (non-immediate, not parsed as SetGpr)
                        let op = match set_opt {
                            1 => "=", 2 => "<=>", 3 => "+=", 4 => "-=",
                            9 => "&=", 0xA => "|=", 0xB => "^=", _ => "?=",
                        };
                        let d = if imm1 { format!("{dst}") } else { format!("GPR[{dst}]") };
                        let s = if imm2 { format!("{src}") } else { format!("GPR[{src}]") };
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

/// Presents content buttons (with `PlayPl`) and prompts for names.
#[allow(clippy::print_stderr, reason = "CLI interactive output")]
fn prompt_content_buttons(
    buttons: &[&ExtractedButton],
    analysis: &reliquary::disc::bdmv::BdmvAnalysis,
    no_images: bool,
) -> Vec<NamedItem> {
    let mut items = Vec::new();
    let stdin = std::io::stdin();

    for button in buttons {
        let Some(playlist) = button.playlist else {
            continue;
        };

        // Look up playlist metadata
        let pl_info = analysis
            .playlists
            .iter()
            .find(|p| p.number == u32::from(playlist));

        // Print playlist metadata
        if let Some(info) = pl_info {
            let duration = format_identify_duration(info.duration);
            let streams = format_identify_streams(&info.streams);
            eprintln!(
                "Playlist {playlist:03}: {duration}  {streams}  {} ch",
                info.chapters
            );
        } else {
            eprintln!("Playlist {playlist:03}:");
        }

        // Render bitmap
        if !no_images {
            render_bitmap(button.width, button.height, &button.rgba);
        }

        // Prompt for name
        eprint!("  Name: ");
        let mut name = String::new();
        if stdin.read_line(&mut name).is_err() {
            break;
        }
        let name = name.trim().to_string();

        if name == "q" {
            break;
        }

        items.push(NamedItem { playlist, name });

        eprintln!();
    }

    items
}

/// Presents all buttons (fallback mode — no `PlayPl`) and prompts for names.
///
/// In this mode the user sees each bitmap and enters a playlist number
/// and name manually, correlating with the inspect output printed above.
#[allow(clippy::print_stderr, reason = "CLI interactive output")]
fn prompt_fallback_buttons(
    buttons: &[ExtractedButton],
    _analysis: &reliquary::disc::bdmv::BdmvAnalysis,
    no_images: bool,
) -> Vec<NamedItem> {
    let mut items = Vec::new();
    let stdin = std::io::stdin();

    for button in buttons {
        eprintln!("Button {}:", button.button_id);

        if !no_images {
            render_bitmap(button.width, button.height, &button.rgba);
        }

        // Prompt for playlist number
        eprint!("  Playlist (or Enter to skip): ");
        let mut pl_input = String::new();
        if stdin.read_line(&mut pl_input).is_err() {
            break;
        }
        let pl_input = pl_input.trim();

        if pl_input == "q" {
            break;
        }

        if pl_input.is_empty() {
            eprintln!();
            continue;
        }

        let Ok(playlist) = pl_input.parse::<u16>() else {
            eprintln!("  (invalid playlist number, skipping)");
            eprintln!();
            continue;
        };

        // Prompt for name
        eprint!("  Name: ");
        let mut name = String::new();
        if stdin.read_line(&mut name).is_err() {
            break;
        }
        let name = name.trim().to_string();

        if name == "q" {
            break;
        }

        items.push(NamedItem { playlist, name });

        eprintln!();
    }

    items
}

// ── Terminal image rendering ───────────────────────────────────────────

/// Graphics protocol for inline image rendering.
enum GraphicsProtocol {
    /// Kitty graphics protocol (Kitty, `WezTerm`, Ghostty, Konsole 5.30+).
    Kitty,
    /// Sixel (foot, xterm, mlterm, Windows Terminal 1.22+, mintty).
    Sixel,
    /// Halfblock characters — universal fallback.
    Halfblock,
}

/// Detects the best available terminal graphics protocol.
fn detect_graphics_protocol() -> GraphicsProtocol {
    if let Ok(term_program) = std::env::var("TERM_PROGRAM") {
        match term_program.to_ascii_lowercase().as_str() {
            "kitty" | "wezterm" | "ghostty" => return GraphicsProtocol::Kitty,
            "foot" | "mlterm" | "mintty" => return GraphicsProtocol::Sixel,
            _ => {}
        }
    }

    if let Ok(term) = std::env::var("TERM") {
        if term.contains("kitty") {
            return GraphicsProtocol::Kitty;
        }
        // foot sets TERM=foot-extra or foot
        if term.starts_with("foot") {
            return GraphicsProtocol::Sixel;
        }
    }

    // Konsole 5.30+ supports Kitty graphics
    if std::env::var("KONSOLE_VERSION")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .is_some_and(|v| v >= 230_000)
    {
        return GraphicsProtocol::Kitty;
    }

    // Windows Terminal supports Sixel (1.22+)
    if std::env::var_os("WT_SESSION").is_some() {
        return GraphicsProtocol::Sixel;
    }

    GraphicsProtocol::Halfblock
}

/// Renders an RGBA bitmap inline in the terminal.
///
/// Auto-detects the terminal graphics protocol and uses the best
/// available: Kitty > Sixel > halfblock.
#[allow(clippy::print_stderr, reason = "CLI bitmap rendering")]
fn render_bitmap(width: u16, height: u16, rgba: &[u8]) {
    match detect_graphics_protocol() {
        GraphicsProtocol::Kitty => render_kitty(width, height, rgba),
        GraphicsProtocol::Sixel => render_sixel(width, height, rgba),
        GraphicsProtocol::Halfblock => render_halfblock(width, height, rgba),
    }
}

// ── Kitty graphics protocol ───────────────────────────────────────────

/// Renders via the Kitty graphics protocol (APC escape sequences).
///
/// Sends raw RGBA data base64-encoded. Chunks at 4096 bytes to stay
/// within protocol limits.
#[allow(clippy::print_stderr, reason = "CLI bitmap rendering")]
fn render_kitty(width: u16, height: u16, rgba: &[u8]) {
    let encoded = base64_encode(rgba);
    let chunks: Vec<&[u8]> = encoded.as_bytes().chunks(4096).collect();

    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i < chunks.len() - 1);
        let chunk_str = std::str::from_utf8(chunk).unwrap_or("");
        if i == 0 {
            eprint!("\x1b_Gf=32,s={width},v={height},a=T,m={more};{chunk_str}\x1b\\");
        } else {
            eprint!("\x1b_Gm={more};{chunk_str}\x1b\\");
        }
    }
    eprintln!();
}

/// Base64-encodes a byte slice (RFC 4648, no line breaks).
#[allow(
    clippy::cast_possible_truncation,
    reason = "index masked to 0-63 always fits in usize"
)]
fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut result = String::with_capacity(data.len().div_ceil(3) * 4);

    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let n = (b0 << 16) | (b1 << 8) | b2;

        result.push(char::from(CHARS[(n >> 18 & 0x3F) as usize]));
        result.push(char::from(CHARS[(n >> 12 & 0x3F) as usize]));
        if chunk.len() > 1 {
            result.push(char::from(CHARS[(n >> 6 & 0x3F) as usize]));
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(char::from(CHARS[(n & 0x3F) as usize]));
        } else {
            result.push('=');
        }
    }

    result
}

// ── Sixel graphics ────────────────────────────────────────────────────

/// Renders via the Sixel graphics protocol (DCS escape sequences).
///
/// Quantizes the image to ≤256 colors and encodes as sixel bands
/// (6-pixel-high rows) with RLE compression.
#[allow(clippy::print_stderr, reason = "CLI bitmap rendering")]
fn render_sixel(width: u16, height: u16, rgba: &[u8]) {
    let w = usize::from(width);
    let h = usize::from(height);
    let (palette, indices) = build_sixel_palette(rgba, w, h);

    // DCS introducer + raster attributes
    eprint!("\x1bP0;0;0q\"1;1;{w};{h}");

    // Color registers (RGB percentages 0-100)
    for (i, &[r, g, b]) in palette.iter().enumerate() {
        let rp = u32::from(r) * 100 / 255;
        let gp = u32::from(g) * 100 / 255;
        let bp = u32::from(b) * 100 / 255;
        eprint!("#{i};2;{rp};{gp};{bp}");
    }

    // Sixel data: process in 6-row bands
    let num_bands = h.div_ceil(6);
    for band in 0..num_bands {
        let band_y = band * 6;
        let mut first_color = true;

        for (color_idx, _) in palette.iter().enumerate() {
            // Skip colors absent from this band
            if !band_has_color(&indices, w, h, band_y, color_idx) {
                continue;
            }

            if first_color {
                first_color = false;
            } else {
                eprint!("$");
            }
            eprint!("#{color_idx}");

            // Encode columns with RLE
            let mut run_char: Option<u8> = None;
            let mut run_len: usize = 0;

            for x in 0..w {
                let mut bits: u8 = 0;
                for row_off in 0..6u8 {
                    let y = band_y + usize::from(row_off);
                    if y < h && indices[y * w + x] == Some(color_idx) {
                        bits |= 1 << row_off;
                    }
                }
                let ch = bits + 0x3F;

                if run_char == Some(ch) {
                    run_len += 1;
                } else {
                    emit_sixel_run(run_char, run_len);
                    run_char = Some(ch);
                    run_len = 1;
                }
            }
            emit_sixel_run(run_char, run_len);
        }

        if band < num_bands - 1 {
            eprint!("-");
        }
    }

    // String terminator
    eprint!("\x1b\\");
    eprintln!();
}

/// Checks whether a color appears in a given 6-row band.
fn band_has_color(
    indices: &[Option<usize>],
    w: usize,
    h: usize,
    band_y: usize,
    color_idx: usize,
) -> bool {
    for row_off in 0..6 {
        let y = band_y + row_off;
        if y >= h {
            break;
        }
        for x in 0..w {
            if indices[y * w + x] == Some(color_idx) {
                return true;
            }
        }
    }
    false
}

/// Emits a sixel run (single character or `!count<char>` for repeats).
#[allow(clippy::print_stderr, reason = "sixel output fragment")]
fn emit_sixel_run(ch: Option<u8>, len: usize) {
    let Some(ch) = ch else { return };
    if len == 1 {
        eprint!("{}", char::from(ch));
    } else if len > 1 {
        eprint!("!{len}{}", char::from(ch));
    }
}

/// Builds a color palette and per-pixel index map for sixel encoding.
///
/// Collects unique RGB values (ignoring transparent pixels). If more
/// than 256 unique colors exist, quantizes to a 6×6×6 RGB cube.
fn build_sixel_palette(rgba: &[u8], w: usize, h: usize) -> (Vec<[u8; 3]>, Vec<Option<usize>>) {
    let total = w * h;
    let mut color_map: HashMap<[u8; 3], usize> = HashMap::new();
    let mut palette = Vec::new();
    let mut indices = Vec::with_capacity(total);

    for pixel in rgba.chunks_exact(4) {
        if pixel[3] == 0 {
            indices.push(None);
            continue;
        }
        let rgb = [pixel[0], pixel[1], pixel[2]];
        let idx = *color_map.entry(rgb).or_insert_with(|| {
            let i = palette.len();
            palette.push(rgb);
            i
        });
        indices.push(Some(idx));
    }

    // If under the 256-color limit, we're done
    if palette.len() <= 256 {
        return (palette, indices);
    }

    // Quantize to 6×6×6 RGB cube (216 colors)
    color_map.clear();
    palette.clear();
    indices.clear();

    for pixel in rgba.chunks_exact(4) {
        if pixel[3] == 0 {
            indices.push(None);
            continue;
        }
        let qr = pixel[0] / 43;
        let qg = pixel[1] / 43;
        let qb = pixel[2] / 43;
        let rgb = [qr * 51, qg * 51, qb * 51];
        let idx = *color_map.entry(rgb).or_insert_with(|| {
            let i = palette.len();
            palette.push(rgb);
            i
        });
        indices.push(Some(idx));
    }

    (palette, indices)
}

// ── Halfblock fallback ────────────────────────────────────────────────

/// Renders via Unicode halfblock characters with truecolor ANSI escapes.
///
/// Each pair of pixel rows becomes one terminal row: top pixel sets the
/// background color, bottom pixel sets the foreground, using `▄`.
/// Scales to fit terminal width via nearest-neighbor sampling.
#[allow(clippy::print_stderr, reason = "CLI bitmap rendering")]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "pixel coordinates and scaling factors are small positive values"
)]
fn render_halfblock(width: u16, height: u16, rgba: &[u8]) {
    let term_cols = terminal_columns();
    let w = usize::from(width);
    let h = usize::from(height);

    let (scaled_w, scaled_h) = if w > term_cols {
        let scale = term_cols as f64 / w as f64;
        (
            (w as f64 * scale) as usize,
            (h as f64 * scale).max(1.0) as usize,
        )
    } else {
        (w, h)
    };

    for row in (0..scaled_h).step_by(2) {
        eprint!("  ");
        for col in 0..scaled_w {
            let top = sample_pixel(rgba, w, h, scaled_w, scaled_h, col, row);
            let bot = if row + 1 < scaled_h {
                sample_pixel(rgba, w, h, scaled_w, scaled_h, col, row + 1)
            } else {
                [0, 0, 0, 0]
            };

            if top[3] == 0 && bot[3] == 0 {
                eprint!(" ");
            } else if bot[3] == 0 {
                eprint!("\x1b[38;2;{};{};{}m\u{2580}\x1b[0m", top[0], top[1], top[2]);
            } else if top[3] == 0 {
                eprint!("\x1b[38;2;{};{};{}m\u{2584}\x1b[0m", bot[0], bot[1], bot[2]);
            } else {
                eprint!(
                    "\x1b[48;2;{};{};{}m\x1b[38;2;{};{};{}m\u{2584}\x1b[0m",
                    top[0], top[1], top[2], bot[0], bot[1], bot[2]
                );
            }
        }
        eprintln!();
    }
}

/// Samples a pixel from the source bitmap using nearest-neighbor scaling.
fn sample_pixel(
    rgba: &[u8],
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
    col: usize,
    row: usize,
) -> [u8; 4] {
    let src_x = (col * src_w / dst_w).min(src_w - 1);
    let src_y = (row * src_h / dst_h).min(src_h - 1);
    let offset = (src_y * src_w + src_x) * 4;
    if offset + 3 < rgba.len() {
        [
            rgba[offset],
            rgba[offset + 1],
            rgba[offset + 2],
            rgba[offset + 3],
        ]
    } else {
        [0, 0, 0, 0]
    }
}

// ── Terminal helpers ──────────────────────────────────────────────────

/// Returns the terminal width in columns.
fn terminal_columns() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(80)
        .saturating_sub(4) // leave room for "  " indent and margin
}

/// Formats a `Duration` as `H:MM:SS` for identify output.
fn format_identify_duration(d: std::time::Duration) -> String {
    let total_secs = d.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    format!("{hours}:{minutes:02}:{seconds:02}")
}

/// Formats a `StreamSummary` as a compact description for identify output.
fn format_identify_streams(s: &reliquary::disc::bdmv::StreamSummary) -> String {
    let mut parts = Vec::new();
    for v in &s.video {
        parts.push(v.clone());
    }
    if let Some(a) = s.audio.first() {
        parts.push(a.clone());
    }
    parts.join("  ")
}

/// Outputs the text report to stdout.
#[allow(clippy::print_stdout, reason = "CLI result output to stdout")]
fn output_text(path: &std::path::Path, items: &[NamedItem]) {
    println!("# reliquary identify: {}", path.display());
    for item in items {
        if item.name.is_empty() {
            println!("playlist {:03}: (skipped)", item.playlist);
        } else {
            println!("playlist {:03}: {}", item.playlist, item.name);
        }
    }
}

/// Outputs the JSON report to stdout.
#[allow(clippy::print_stdout, reason = "CLI result output to stdout")]
#[allow(clippy::print_stderr, reason = "CLI error output")]
fn output_json(
    path: &std::path::Path,
    items: &[NamedItem],
    analysis: &reliquary::disc::bdmv::BdmvAnalysis,
) {
    let json_items: Vec<serde_json::Value> = items
        .iter()
        .map(|item| {
            let mut entry = serde_json::json!({
                "playlist": item.playlist,
                "name": item.name,
            });

            if let Some(info) = analysis
                .playlists
                .iter()
                .find(|p| p.number == u32::from(item.playlist))
            {
                entry["duration"] = serde_json::json!(info.duration.as_secs_f64());
                entry["chapters"] = serde_json::json!(info.chapters);
            }

            entry
        })
        .collect();

    let output = serde_json::json!({
        "path": path.display().to_string(),
        "items": json_items,
    });

    match serde_json::to_string_pretty(&output) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("error: failed to serialize JSON: {e}"),
    }
}

// ── Decrypt ──────────────────────────────────────────────────────────────

/// Runs the `decrypt` subcommand — resolves VUK then dispatches.
#[allow(clippy::print_stderr, reason = "CLI status and error output")]
fn run_decrypt(
    path: &std::path::Path,
    vuk_hex: Option<&str>,
    clip: Option<&str>,
    keydb: Option<&std::path::Path>,
    no_keydb: bool,
    output: &std::path::Path,
) -> ExitCode {
    let (vuk, uk_data) = match resolve_vuk(path, vuk_hex, keydb, no_keydb) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };

    clip.map_or_else(
        || run_decrypt_disc(path, &vuk, uk_data.as_deref(), output),
        |clip_id| run_decrypt_clip(path, &vuk, clip_id, uk_data.as_deref(), output),
    )
}

/// Resolves the VUK from the CLI flag or KEYDB.cfg lookup.
///
/// Returns the VUK and, when disc ID computation was needed, the raw
/// `Unit_Key_RO.inf` bytes so callers can reuse them for key parsing.
///
/// Resolution order:
/// 1. `--vuk` flag → use directly (no unit key data read).
/// 2. Compute disc ID, search KEYDB.cfg.
/// 3. No match → error with disc ID for manual lookup.
#[allow(clippy::print_stderr, reason = "CLI status output")]
fn resolve_vuk(
    path: &std::path::Path,
    vuk_hex: Option<&str>,
    keydb: Option<&std::path::Path>,
    no_keydb: bool,
) -> Result<([u8; 16], Option<Vec<u8>>), String> {
    // Direct VUK from --vuk flag — no need to read the disc
    if let Some(hex) = vuk_hex {
        return Ok((parse_vuk(hex)?, None));
    }

    // Read Unit_Key_RO.inf once and compute disc ID
    let reader = reliquary::disc::reader::DiscReader::open(path).map_err(|e| e.to_string())?;
    let uk_data =
        reliquary::disc::bdmv::aacs::read_unit_key_data(&reader).map_err(|e| e.to_string())?;
    let disc_id = reliquary::disc::bdmv::aacs::disc_id_from_data(&uk_data);
    eprintln!("disc ID: {disc_id}");

    if no_keydb {
        return Err(format!(
            "VUK not provided and KEYDB.cfg lookup disabled — use --vuk <hex> (disc ID: {disc_id})"
        ));
    }

    // Determine KEYDB.cfg path
    let keydb_path = keydb.map_or_else(
        reliquary::disc::bdmv::keydb::default_keydb_path,
        std::path::PathBuf::from,
    );

    match reliquary::disc::bdmv::keydb::lookup_keydb(&keydb_path, &disc_id) {
        Ok(Some(vuk)) => {
            eprintln!("VUK found in KEYDB.cfg");
            Ok((vuk, Some(uk_data)))
        }
        Ok(None) => Err(format!(
            "VUK not found in KEYDB.cfg — use --vuk <hex> to provide manually (disc ID: {disc_id})"
        )),
        Err(e) => Err(e.to_string()),
    }
}

/// Decrypts a single m2ts clip.
///
/// When `uk_data` is provided (from VUK resolution), reuses it to avoid
/// re-reading `Unit_Key_RO.inf`.
#[allow(clippy::print_stderr, reason = "CLI status and error output")]
fn run_decrypt_clip(
    path: &std::path::Path,
    vuk: &[u8; 16],
    clip_id: &str,
    uk_data: Option<&[u8]>,
    output: &std::path::Path,
) -> ExitCode {
    let reader = match reliquary::disc::reader::DiscReader::open(path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let keys = match uk_data.map_or_else(
        || reliquary::disc::bdmv::aacs::AacsKeys::from_disc(&reader, vuk),
        |data| reliquary::disc::bdmv::aacs::AacsKeys::from_unit_key_data(data, vuk),
    ) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    match keys.decrypt_clip(&reader, clip_id) {
        Ok(data) => match std::fs::write(output, &data) {
            Ok(()) => {
                eprintln!("decrypted {} bytes to {}", data.len(), output.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: failed to write output: {e}");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// An m2ts file to decrypt, with its location within the ISO or filesystem.
struct M2tsTarget {
    name: String,
    size: u64,
    extents: Vec<reliquary::disc::reader::FileExtent>,
}

/// Decrypts a whole disc — copies input then decrypts m2ts files in-place.
///
/// When `uk_data` is provided (from VUK resolution), reuses it to avoid
/// re-reading `Unit_Key_RO.inf` from the output copy.
#[allow(clippy::print_stderr, reason = "CLI status and error output")]
fn run_decrypt_disc(
    path: &std::path::Path,
    vuk: &[u8; 16],
    uk_data: Option<&[u8]>,
    output: &std::path::Path,
) -> ExitCode {
    // 1. Copy input to output
    if let Err(e) = copy_input(path, output) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }

    // 2. Open the copy, parse keys, and collect targets
    let (keys, targets, is_iso) = match prepare_targets(output, vuk, uk_data) {
        Ok(result) => result,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };

    // 3. Decrypt each m2ts
    let (mut files_decrypted, mut files_skipped) = (0u32, 0u32);

    if is_iso {
        let mut file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(output)
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!("error: failed to open output ISO: {e}");
                return ExitCode::FAILURE;
            }
        };

        for target in &targets {
            match decrypt_iso_target(&keys, &mut file, target) {
                Ok(was_encrypted) => {
                    report_target(target, was_encrypted);
                    if was_encrypted {
                        files_decrypted += 1;
                    } else {
                        files_skipped += 1;
                    }
                }
                Err(e) => {
                    eprintln!("error: failed to decrypt {}: {e}", target.name);
                    return ExitCode::FAILURE;
                }
            }
        }
    } else {
        for target in &targets {
            match decrypt_dir_target(&keys, output, target) {
                Ok(was_encrypted) => {
                    report_target(target, was_encrypted);
                    if was_encrypted {
                        files_decrypted += 1;
                    } else {
                        files_skipped += 1;
                    }
                }
                Err(e) => {
                    eprintln!("error: failed to decrypt {}: {e}", target.name);
                    return ExitCode::FAILURE;
                }
            }
        }
    }

    eprintln!("done: {files_decrypted} m2ts decrypted, {files_skipped} skipped (not encrypted)");
    ExitCode::SUCCESS
}

/// Copies input (ISO or directory) to output.
#[allow(clippy::print_stderr, reason = "CLI status output")]
fn copy_input(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
    if src.is_dir() {
        eprintln!("copying directory...");
        copy_dir_recursive(src, dst).map_err(|e| format!("failed to copy directory: {e}"))
    } else {
        let size = std::fs::metadata(src).map_err(|e| e.to_string())?.len();
        eprintln!("copying ISO ({})...", format_size(size));
        std::fs::copy(src, dst)
            .map(|_| ())
            .map_err(|e| format!("failed to copy ISO: {e}"))
    }
}

/// Opens the output copy, parses AACS keys, and collects m2ts targets.
///
/// When `uk_data` is provided, uses it directly instead of re-reading
/// `Unit_Key_RO.inf` from the output.
#[allow(clippy::print_stderr, reason = "CLI status output")]
fn prepare_targets(
    output: &std::path::Path,
    vuk: &[u8; 16],
    uk_data: Option<&[u8]>,
) -> Result<(reliquary::disc::bdmv::aacs::AacsKeys, Vec<M2tsTarget>, bool), String> {
    let reader = reliquary::disc::reader::DiscReader::open(output).map_err(|e| e.to_string())?;

    eprintln!("parsing AACS keys...");
    let keys = uk_data
        .map_or_else(
            || reliquary::disc::bdmv::aacs::AacsKeys::from_disc(&reader, vuk),
            |data| reliquary::disc::bdmv::aacs::AacsKeys::from_unit_key_data(data, vuk),
        )
        .map_err(|e| e.to_string())?;

    let stream_dir = std::path::Path::new("BDMV/STREAM");
    let entries = reader
        .read_dir(stream_dir)
        .map_err(|e| format!("failed to list BDMV/STREAM: {e}"))?;

    let m2ts_names: Vec<&str> = entries
        .iter()
        .filter(|n| {
            std::path::Path::new(n)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("m2ts"))
        })
        .map(String::as_str)
        .collect();

    let is_iso = !output.is_dir();
    let mut targets = Vec::new();

    if is_iso {
        for name in &m2ts_names {
            let rel = stream_dir.join(name);
            match reader.file_extents(&rel) {
                Some(Ok(extents)) => {
                    let size: u64 = extents.iter().map(|e| e.length).sum();
                    targets.push(M2tsTarget {
                        name: (*name).to_owned(),
                        size,
                        extents,
                    });
                }
                Some(Err(e)) => {
                    return Err(format!("failed to get extents for {name}: {e}"));
                }
                None => {
                    return Err(format!("file extents not available for {name}"));
                }
            }
        }
    } else {
        for name in &m2ts_names {
            let m2ts_path = output.join("BDMV/STREAM").join(name);
            let size = std::fs::metadata(&m2ts_path)
                .map_err(|e| e.to_string())?
                .len();
            targets.push(M2tsTarget {
                name: (*name).to_owned(),
                size,
                extents: Vec::new(),
            });
        }
    }

    Ok((keys, targets, is_iso))
}

/// Decrypts an m2ts within an ISO via file extents. Returns whether it was encrypted.
fn decrypt_iso_target(
    keys: &reliquary::disc::bdmv::aacs::AacsKeys,
    file: &mut std::fs::File,
    target: &M2tsTarget,
) -> Result<bool, reliquary::disc::bdmv::aacs::AacsError> {
    let mut total_decrypted: u64 = 0;
    for extent in &target.extents {
        let stats = keys.decrypt_stream(file, extent.offset, extent.length)?;
        total_decrypted += stats.blocks_decrypted;
    }
    Ok(total_decrypted > 0)
}

/// Decrypts an m2ts file within a directory. Returns whether it was encrypted.
fn decrypt_dir_target(
    keys: &reliquary::disc::bdmv::aacs::AacsKeys,
    output: &std::path::Path,
    target: &M2tsTarget,
) -> Result<bool, reliquary::disc::bdmv::aacs::AacsError> {
    let m2ts_path = output.join("BDMV/STREAM").join(&target.name);
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&m2ts_path)?;
    let stats = keys.decrypt_stream(&mut file, 0, target.size)?;
    Ok(stats.blocks_decrypted > 0)
}

/// Prints a status line for a decrypted or skipped m2ts file.
#[allow(clippy::print_stderr, reason = "CLI status output")]
fn report_target(target: &M2tsTarget, was_encrypted: bool) {
    if was_encrypted {
        eprintln!(
            "decrypted BDMV/STREAM/{} ({})",
            target.name,
            format_size(target.size)
        );
    } else {
        eprintln!("skipped BDMV/STREAM/{} (not encrypted)", target.name);
    }
}

/// Recursively copies a directory tree.
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Formats a byte count as a human-readable string.
#[allow(
    clippy::cast_precision_loss,
    reason = "file sizes fit within f64 precision for any real disc"
)]
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    }
}

/// Parses a 32-character hex string into a 16-byte VUK.
fn parse_vuk(hex: &str) -> Result<[u8; 16], String> {
    let hex = hex.strip_prefix("0x").unwrap_or(hex);

    if hex.len() != 32 {
        return Err(format!("VUK must be 32 hex characters, got {}", hex.len()));
    }

    let mut vuk = [0u8; 16];
    for (i, byte) in vuk.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|_| format!("invalid hex at position {}", i * 2))?;
    }
    Ok(vuk)
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod tests {
    use aes::Aes128;
    use cbc::Encryptor as CbcEncryptor;
    use cipher::block_padding::NoPadding;
    use cipher::{BlockEncrypt, BlockEncryptMut, KeyInit, KeyIvInit};

    use super::*;

    const ALIGNED_UNIT_LEN: usize = 6144;
    const KEY_TABLE_HEADER_LEN: usize = 48;
    const KEY_ENTRY_STRIDE: usize = 48;
    const AACS_IV: [u8; 16] = [
        0x0B, 0xA0, 0xF8, 0xDD, 0xFE, 0xA6, 0x1F, 0xB3, 0xD8, 0xDF, 0x9F, 0x56, 0x6A, 0x05, 0x0F,
        0x78,
    ];

    // ── Test helpers ─────────────────────────────────────────────────────

    /// Encrypts a unit key with the VUK (as stored on disc).
    fn encrypt_unit_key(plaintext_key: &[u8; 16], vuk: &[u8; 16]) -> [u8; 16] {
        let cipher = Aes128::new(vuk.into());
        let mut block = aes::Block::clone_from_slice(plaintext_key);
        cipher.encrypt_block(&mut block);
        let mut result = [0u8; 16];
        result.copy_from_slice(&block);
        result
    }

    /// Builds a valid `Unit_Key_RO.inf` binary with the given encrypted keys.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "test helper — key counts and offsets are small positive values"
    )]
    fn build_unit_key_file(encrypted_keys: &[[u8; 16]]) -> Vec<u8> {
        let title_section_size: usize = 6 + 4; // 1 title
        let uk_pos: u32 = (20 + title_section_size) as u32;
        let key_table_size = KEY_TABLE_HEADER_LEN + encrypted_keys.len() * KEY_ENTRY_STRIDE;
        let total = uk_pos as usize + key_table_size;

        let mut data = vec![0u8; total];
        data[0..4].copy_from_slice(&uk_pos.to_be_bytes());

        // Title mapping: first_play=1, top_menu=1, num_titles=1, title[0].cps_unit=1
        data[20..22].copy_from_slice(&1u16.to_be_bytes());
        data[22..24].copy_from_slice(&1u16.to_be_bytes());
        data[24..26].copy_from_slice(&1u16.to_be_bytes());
        data[28..30].copy_from_slice(&1u16.to_be_bytes());

        let uk_start = uk_pos as usize;
        let num_uk = encrypted_keys.len() as u16;
        data[uk_start..uk_start + 2].copy_from_slice(&num_uk.to_be_bytes());

        for (i, key) in encrypted_keys.iter().enumerate() {
            let offset = uk_start + KEY_TABLE_HEADER_LEN + i * KEY_ENTRY_STRIDE;
            data[offset..offset + 16].copy_from_slice(key);
        }

        data
    }

    /// Builds an encrypted 6144-byte aligned unit.
    fn build_encrypted_block(unit_key: &[u8; 16], seed: [u8; 16]) -> [u8; ALIGNED_UNIT_LEN] {
        let mut block = [0u8; ALIGNED_UNIT_LEN];
        block[..16].copy_from_slice(&seed);
        for i in 0..32 {
            block[i * 192 + 4] = 0x47;
        }
        block[0] |= 0xC0;
        block[4] = 0x47;

        let cipher = Aes128::new(unit_key.into());
        let mut derived_key_block = aes::Block::clone_from_slice(&block[..16]);
        cipher.encrypt_block(&mut derived_key_block);
        let mut derived_key = [0u8; 16];
        for (i, byte) in derived_key.iter_mut().enumerate() {
            *byte = derived_key_block[i] ^ block[i];
        }

        let encryptor = CbcEncryptor::<Aes128>::new(&derived_key.into(), &AACS_IV.into());
        encryptor
            .encrypt_padded_mut::<NoPadding>(&mut block[16..], ALIGNED_UNIT_LEN - 16)
            .expect("encryption should succeed");

        block
    }

    /// Builds an unencrypted 6144-byte aligned unit with valid TS sync.
    fn build_unencrypted_block() -> [u8; ALIGNED_UNIT_LEN] {
        let mut block = [0u8; ALIGNED_UNIT_LEN];
        for i in 0..32 {
            block[i * 192 + 4] = 0x47;
        }
        block
    }

    /// Creates a synthetic disc directory for testing.
    struct SyntheticDisc {
        dir: tempfile::TempDir,
        vuk: [u8; 16],
    }

    impl SyntheticDisc {
        /// Creates a disc with 2 encrypted + 1 unencrypted m2ts + metadata.
        fn new() -> Self {
            let vuk = [0x42; 16];
            let plaintext_key = [0x77; 16];
            let stored_key = encrypt_unit_key(&plaintext_key, &vuk);
            let uk_data = build_unit_key_file(&[stored_key]);

            let dir = tempfile::tempdir().expect("should create temp dir");
            let aacs_dir = dir.path().join("AACS");
            let stream_dir = dir.path().join("BDMV").join("STREAM");
            let playlist_dir = dir.path().join("BDMV").join("PLAYLIST");
            std::fs::create_dir_all(&aacs_dir).expect("should create AACS dir");
            std::fs::create_dir_all(&stream_dir).expect("should create STREAM dir");
            std::fs::create_dir_all(&playlist_dir).expect("should create PLAYLIST dir");

            // Unit key file
            std::fs::write(aacs_dir.join("Unit_Key_RO.inf"), &uk_data)
                .expect("should write unit key file");

            // Encrypted m2ts files
            let block1 = build_encrypted_block(&plaintext_key, [0x01; 16]);
            let block2 = build_encrypted_block(&plaintext_key, [0x02; 16]);
            std::fs::write(stream_dir.join("00000.m2ts"), block1).expect("should write m2ts 00000");
            std::fs::write(stream_dir.join("00001.m2ts"), block2).expect("should write m2ts 00001");

            // Unencrypted m2ts
            let unenc = build_unencrypted_block();
            std::fs::write(stream_dir.join("00100.m2ts"), unenc).expect("should write m2ts 00100");

            // Non-m2ts metadata
            std::fs::write(playlist_dir.join("00000.mpls"), b"fake playlist data")
                .expect("should write playlist");

            Self { dir, vuk }
        }

        fn path(&self) -> &std::path::Path {
            self.dir.path()
        }

        fn vuk_hex(&self) -> String {
            self.vuk
                .iter()
                .fold(String::with_capacity(32), |mut acc, b| {
                    use std::fmt::Write as _;
                    let _ = write!(acc, "{b:02x}");
                    acc
                })
        }
    }

    // ── Tests ────────────────────────────────────────────────────────────

    #[test]
    fn whole_disc_decrypt_directory() {
        let disc = SyntheticDisc::new();
        let output = tempfile::tempdir().expect("should create output dir");
        let output_path = output.path().join("decrypted");

        let result = run_decrypt(
            disc.path(),
            Some(&disc.vuk_hex()),
            None,
            None,
            false,
            &output_path,
        );
        assert_eq!(
            result,
            ExitCode::SUCCESS,
            "whole-disc decrypt should succeed"
        );

        // Output structure matches input
        assert!(
            output_path.join("AACS/Unit_Key_RO.inf").exists(),
            "Unit_Key_RO.inf should be copied"
        );
        assert!(
            output_path.join("BDMV/STREAM/00000.m2ts").exists(),
            "00000.m2ts should exist"
        );
        assert!(
            output_path.join("BDMV/STREAM/00001.m2ts").exists(),
            "00001.m2ts should exist"
        );
        assert!(
            output_path.join("BDMV/STREAM/00100.m2ts").exists(),
            "00100.m2ts should exist"
        );
        assert!(
            output_path.join("BDMV/PLAYLIST/00000.mpls").exists(),
            "playlist should be copied"
        );

        // Non-m2ts files copied byte-for-byte
        let playlist = std::fs::read(output_path.join("BDMV/PLAYLIST/00000.mpls"))
            .expect("should read playlist");
        assert_eq!(
            playlist, b"fake playlist data",
            "non-m2ts files should be byte-identical"
        );

        // Encrypted m2ts files should have valid TS sync (decrypted)
        for name in &["00000.m2ts", "00001.m2ts"] {
            let data = std::fs::read(output_path.join("BDMV/STREAM").join(name))
                .expect("should read m2ts");
            assert_eq!(
                data.len(),
                ALIGNED_UNIT_LEN,
                "{name} size should be unchanged"
            );
            // Encryption flag cleared
            assert_eq!(
                data[0] & 0xC0,
                0,
                "{name} encryption flag should be cleared"
            );
            // TS sync bytes present
            for pkt in 0..4 {
                let offset = pkt * 192 + 4;
                assert_eq!(data[offset], 0x47, "{name} TS sync at packet {pkt}");
            }
        }

        // Unencrypted m2ts should be identical to source
        let src_unenc =
            std::fs::read(disc.path().join("BDMV/STREAM/00100.m2ts")).expect("should read source");
        let dst_unenc =
            std::fs::read(output_path.join("BDMV/STREAM/00100.m2ts")).expect("should read output");
        assert_eq!(
            src_unenc, dst_unenc,
            "unencrypted m2ts should be byte-identical"
        );

        // Output size matches input for each m2ts
        for name in &["00000.m2ts", "00001.m2ts", "00100.m2ts"] {
            let src_len = std::fs::metadata(disc.path().join("BDMV/STREAM").join(name))
                .expect("should stat source")
                .len();
            let dst_len = std::fs::metadata(output_path.join("BDMV/STREAM").join(name))
                .expect("should stat output")
                .len();
            assert_eq!(src_len, dst_len, "{name} size should be unchanged");
        }
    }

    #[test]
    fn whole_disc_wrong_vuk() {
        let disc = SyntheticDisc::new();
        let output = tempfile::tempdir().expect("should create output dir");
        let output_path = output.path().join("decrypted");

        let wrong_vuk = "ff".repeat(16);
        let result = run_decrypt(
            disc.path(),
            Some(&wrong_vuk),
            None,
            None,
            false,
            &output_path,
        );
        assert_eq!(
            result,
            ExitCode::FAILURE,
            "wrong VUK should produce failure"
        );
    }

    #[test]
    fn whole_disc_no_unit_key_file() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let stream_dir = dir.path().join("BDMV").join("STREAM");
        std::fs::create_dir_all(&stream_dir).expect("should create STREAM dir");
        std::fs::write(stream_dir.join("00000.m2ts"), build_unencrypted_block())
            .expect("should write m2ts");

        let output = tempfile::tempdir().expect("should create output dir");
        let output_path = output.path().join("decrypted");

        let vuk = "42".repeat(16);
        let result = run_decrypt(dir.path(), Some(&vuk), None, None, false, &output_path);
        assert_eq!(
            result,
            ExitCode::FAILURE,
            "missing Unit_Key_RO.inf should produce failure"
        );
    }

    #[test]
    fn per_clip_still_works() {
        let disc = SyntheticDisc::new();
        let output = tempfile::tempdir().expect("should create output dir");
        let output_path = output.path().join("00000.m2ts");

        let result = run_decrypt(
            disc.path(),
            Some(&disc.vuk_hex()),
            Some("00000"),
            None,
            false,
            &output_path,
        );
        assert_eq!(result, ExitCode::SUCCESS, "per-clip decrypt should succeed");

        let data = std::fs::read(&output_path).expect("should read output");
        assert_eq!(data.len(), ALIGNED_UNIT_LEN, "output size should match");
        assert_eq!(data[0] & 0xC0, 0, "encryption flag should be cleared");
        for pkt in 0..4 {
            let offset = pkt * 192 + 4;
            assert_eq!(data[offset], 0x47, "TS sync at packet {pkt}");
        }
    }

    // ── VUK lookup integration tests ────────────────────────────────────

    #[test]
    fn decrypt_with_keydb_lookup() {
        use sha1::{Digest, Sha1};

        let disc = SyntheticDisc::new();

        // Compute the disc ID from the Unit_Key_RO.inf
        let uk_data = std::fs::read(disc.path().join("AACS/Unit_Key_RO.inf"))
            .expect("should read unit key file");
        let hash = Sha1::digest(&uk_data);
        let disc_id: String = hash.iter().fold(String::new(), |mut acc, b| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{b:02x}");
            acc
        });

        // Build a synthetic KEYDB.cfg with the computed disc ID and VUK
        let keydb_dir = tempfile::tempdir().expect("should create keydb dir");
        let keydb_path = keydb_dir.path().join("KEYDB.cfg");
        let keydb_content = format!(
            "{disc_id} = Test Disc | D | 2020-01-01 | M | 00000000000000000000000000000000 | I | 00000000000000000000000000000000 | V | 0x{} | U | 00000000000000000000000000000000 ; test\n",
            disc.vuk_hex()
        );
        std::fs::write(&keydb_path, keydb_content).expect("should write KEYDB.cfg");

        // Decrypt without --vuk, using --keydb
        let output = tempfile::tempdir().expect("should create output dir");
        let output_path = output.path().join("decrypted");

        let result = run_decrypt(
            disc.path(),
            None,
            None,
            Some(keydb_path.as_path()),
            false,
            &output_path,
        );
        assert_eq!(
            result,
            ExitCode::SUCCESS,
            "decrypt via KEYDB.cfg lookup should succeed"
        );

        // Verify decryption worked — encrypted files should have valid TS sync
        for name in &["00000.m2ts", "00001.m2ts"] {
            let data = std::fs::read(output_path.join("BDMV/STREAM").join(name))
                .expect("should read m2ts");
            assert_eq!(
                data[0] & 0xC0,
                0,
                "{name} encryption flag should be cleared"
            );
            for pkt in 0..4 {
                let offset = pkt * 192 + 4;
                assert_eq!(data[offset], 0x47, "{name} TS sync at packet {pkt}");
            }
        }
    }

    #[test]
    fn decrypt_no_vuk_no_keydb_fails() {
        let disc = SyntheticDisc::new();
        let output = tempfile::tempdir().expect("should create output dir");
        let output_path = output.path().join("decrypted");

        let result = run_decrypt(disc.path(), None, None, None, true, &output_path);
        assert_eq!(
            result,
            ExitCode::FAILURE,
            "no VUK and --no-keydb should fail"
        );
    }

    #[test]
    fn decrypt_keydb_disc_id_not_found_fails() {
        let disc = SyntheticDisc::new();

        // KEYDB.cfg with a different disc ID
        let keydb_dir = tempfile::tempdir().expect("should create keydb dir");
        let keydb_path = keydb_dir.path().join("KEYDB.cfg");
        let keydb_content = "ffffffffffffffffffffffffffffffffffffffff = Other Disc | D | 2020-01-01 | M | 00000000000000000000000000000000 | I | 00000000000000000000000000000000 | V | 0x00000000000000000000000000000000 | U | 00000000000000000000000000000000 ; test\n";
        std::fs::write(&keydb_path, keydb_content).expect("should write KEYDB.cfg");

        let output = tempfile::tempdir().expect("should create output dir");
        let output_path = output.path().join("decrypted");

        let result = run_decrypt(
            disc.path(),
            None,
            None,
            Some(keydb_path.as_path()),
            false,
            &output_path,
        );
        assert_eq!(
            result,
            ExitCode::FAILURE,
            "disc ID not in KEYDB.cfg should fail"
        );
    }

    // ── Identify tests ──────────────────────────────────────────────────

    #[test]
    fn button_filtering_deduplicates_by_playlist() {
        let buttons = [
            ExtractedButton {
                playlist: Some(203),
                button_id: 1,
                width: 2,
                height: 1,
                rgba: vec![255, 0, 0, 255, 0, 255, 0, 255],
            },
            ExtractedButton {
                playlist: Some(203), // duplicate playlist
                button_id: 2,
                width: 2,
                height: 1,
                rgba: vec![0, 0, 255, 255, 255, 255, 0, 255],
            },
            ExtractedButton {
                playlist: Some(204),
                button_id: 3,
                width: 2,
                height: 1,
                rgba: vec![0, 0, 0, 255, 255, 255, 255, 255],
            },
        ];

        let valid_playlists: HashSet<u32> = [203, 204].into_iter().collect();
        let content_buttons: Vec<&ExtractedButton> = {
            let mut seen = HashSet::new();
            buttons
                .iter()
                .filter(|b| {
                    b.playlist.is_some_and(|pl| {
                        valid_playlists.contains(&u32::from(pl)) && seen.insert(pl)
                    })
                })
                .collect()
        };

        assert_eq!(content_buttons.len(), 2, "should deduplicate playlist 203");
        assert_eq!(
            content_buttons[0].button_id, 1,
            "first button for playlist 203 should win"
        );
        assert_eq!(
            content_buttons[1].playlist,
            Some(204),
            "playlist 204 should be included"
        );
    }

    #[test]
    fn button_filtering_skips_invalid_playlists() {
        let buttons = [
            ExtractedButton {
                playlist: Some(203),
                button_id: 1,
                width: 1,
                height: 1,
                rgba: vec![0, 0, 0, 255],
            },
            ExtractedButton {
                playlist: Some(999), // not in valid set
                button_id: 2,
                width: 1,
                height: 1,
                rgba: vec![0, 0, 0, 255],
            },
            ExtractedButton {
                playlist: None, // no PlayPl
                button_id: 3,
                width: 1,
                height: 1,
                rgba: vec![0, 0, 0, 255],
            },
        ];

        let valid_playlists: HashSet<u32> = [203, 204].into_iter().collect();
        let content_buttons: Vec<&ExtractedButton> = {
            let mut seen = HashSet::new();
            buttons
                .iter()
                .filter(|b| {
                    b.playlist.is_some_and(|pl| {
                        valid_playlists.contains(&u32::from(pl)) && seen.insert(pl)
                    })
                })
                .collect()
        };

        assert_eq!(
            content_buttons.len(),
            1,
            "only playlist 203 should pass filtering"
        );
        assert_eq!(
            content_buttons[0].playlist,
            Some(203),
            "only valid playlist should be included"
        );
    }

    #[test]
    fn output_text_format() {
        use std::fmt::Write as _;

        let items = [
            NamedItem {
                playlist: 203,
                name: "Themyscira: The Hidden Island".to_string(),
            },
            NamedItem {
                playlist: 204,
                name: String::new(),
            },
            NamedItem {
                playlist: 207,
                name: "a WB Blu-ray title at War".to_string(),
            },
        ];

        // Reproduce output_text logic into a string
        let mut output = String::new();
        let _ = writeln!(
            output,
            "# reliquary identify: {}",
            std::path::Path::new("test.iso").display()
        );
        for item in &items {
            if item.name.is_empty() {
                let _ = writeln!(output, "playlist {:03}: (skipped)", item.playlist);
            } else {
                let _ = writeln!(output, "playlist {:03}: {}", item.playlist, item.name);
            }
        }

        assert!(
            output.contains("# reliquary identify: test.iso"),
            "should include header"
        );
        assert!(
            output.contains("playlist 203: Themyscira: The Hidden Island"),
            "should include named item"
        );
        assert!(
            output.contains("playlist 204: (skipped)"),
            "should mark skipped items"
        );
        assert!(
            output.contains("playlist 207: a WB Blu-ray title at War"),
            "should include all named items"
        );
    }

    #[test]
    fn output_json_structure() {
        let items = [
            NamedItem {
                playlist: 203,
                name: "Beach Battle".to_string(),
            },
            NamedItem {
                playlist: 204,
                name: String::new(),
            },
        ];

        let json_items: Vec<serde_json::Value> = items
            .iter()
            .map(|item| {
                serde_json::json!({
                    "playlist": item.playlist,
                    "name": item.name,
                })
            })
            .collect();

        let output = serde_json::json!({
            "path": "test.iso",
            "items": json_items,
        });

        let arr = output["items"]
            .as_array()
            .expect("items should be an array");
        assert_eq!(arr.len(), 2, "should have two items");
        assert_eq!(arr[0]["playlist"], 203, "first item should be playlist 203");
        assert_eq!(
            arr[0]["name"], "Beach Battle",
            "first item should have correct name"
        );
        assert_eq!(arr[1]["name"], "", "skipped item should have empty name");
    }

    #[test]
    fn identify_duration_formatting() {
        assert_eq!(
            format_identify_duration(std::time::Duration::from_secs(296)),
            "0:04:56",
            "4 min 56 sec"
        );
        assert_eq!(
            format_identify_duration(std::time::Duration::from_secs(3661)),
            "1:01:01",
            "1 hour 1 min 1 sec"
        );
        assert_eq!(
            format_identify_duration(std::time::Duration::from_secs(0)),
            "0:00:00",
            "zero duration"
        );
    }
}
