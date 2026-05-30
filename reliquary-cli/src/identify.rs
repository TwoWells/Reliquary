// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! The `identify` subcommand — IG pipeline with interactive naming.

use std::collections::{HashMap, HashSet};
use std::process::ExitCode;

use crate::output::{output_dump, output_json, output_text};
use crate::prompt::{prompt_content_buttons, prompt_fallback_buttons};
use crate::snapshot::{dump_page_images, extract_video_frame};
use crate::trace;
use crate::util::{format_identify_duration, format_size, parse_vuk};

// ── Shared types ────────────────────────────────────────────────────

/// A button extracted from the IG stream with its decoded bitmap.
pub struct ExtractedButton {
    /// Playlist number if the button has a `PlayPl` command.
    pub playlist: Option<u16>,
    /// `PlayPl` variant: 0=from start, 1=at mark, 2=at play item.
    pub branch_opt: u8,
    /// Mark index or play item index (meaningful when `branch_opt > 0`).
    pub mark_or_pi: u32,
    /// Index into the clips list (identifies the IG clip).
    pub clip_index: usize,
    /// Page ID within the clip.
    pub page_id: u8,
    /// Button identifier from the IG data.
    pub button_id: u16,
    /// Navigation breadcrumb — ordered steps from root to this button.
    /// Empty for direct `PlayPl` buttons (single-step resolution).
    pub breadcrumb: Vec<reliquary::disc::bdmv::mobj::BreadcrumbStep>,
    /// `true` when the content is on a page not reachable from the root menu.
    pub orphan: bool,
    /// Bitmap width in pixels.
    pub width: u16,
    /// Bitmap height in pixels.
    pub height: u16,
    /// RGBA pixel data (4 bytes per pixel, row-major).
    pub rgba: Vec<u8>,
}

/// A named content item — user's response for one button.
pub struct NamedItem {
    /// Playlist number.
    pub playlist: u16,
    /// `PlayPl` variant: 0=from start, 1=at mark, 2=at play item.
    pub branch_opt: u8,
    /// Mark index or play item index (meaningful when `branch_opt > 0`).
    pub mark_or_pi: u32,
    /// User-provided name (empty string means skipped).
    pub name: String,
}

/// All decoded button bitmaps for one IG page, with positions and canvas size.
///
/// Used for full-page rendering: composite all buttons at their `(x, y)`
/// coordinates onto a canvas at the composition window dimensions, then
/// highlight the active button by overwriting its region with the selected-
/// state bitmap.
pub struct PageComposition {
    /// Index into the clips list (matches [`ExtractedButton::clip_index`]).
    pub clip_index: usize,
    /// Page identifier.
    pub page_id: u8,
    /// Canvas width in pixels (from the IG composition descriptor).
    pub canvas_width: u16,
    /// Canvas height in pixels.
    pub canvas_height: u16,
    /// Decoded button bitmaps with positions.
    pub buttons: Vec<ButtonComposition>,
}

/// A single button's position and decoded bitmaps (both states).
pub struct ButtonComposition {
    /// Button identifier.
    pub button_id: u16,
    /// Horizontal position on the canvas.
    pub x: u16,
    /// Vertical position on the canvas.
    pub y: u16,
    /// Normal (unselected) state bitmap, if decodable.
    pub normal: Option<reliquary::disc::bdmv::rle::Bitmap>,
    /// Selected (highlighted) state bitmap, if decodable.
    pub selected: Option<reliquary::disc::bdmv::rle::Bitmap>,
}

// ── Pipeline ────────────────────────────────────────────────────────

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
#[allow(
    clippy::too_many_arguments,
    reason = "CLI flag pass-through, not a public API"
)]
pub fn run_identify(
    path: &std::path::Path,
    vuk_hex: Option<&str>,
    keydb: Option<&std::path::Path>,
    no_keydb: bool,
    dump: bool,
    json: bool,
    no_images: bool,
    trace: bool,
    dump_pages: Option<&std::path::Path>,
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

    let (buttons, page_compositions, clip_backgrounds) =
        match extract_buttons(&reader, &analysis, vuk.as_ref(), trace) {
            Ok(b) => b,
            Err(msg) => {
                eprintln!("error: {msg}");
                return ExitCode::FAILURE;
            }
        };

    // 5. Build content inventory (filtering + dedup in the library)
    let menu_buttons: Vec<reliquary::disc::bdmv::inventory::MenuButton> = buttons
        .iter()
        .filter_map(|b| {
            let playlist = b.playlist?;
            Some(reliquary::disc::bdmv::inventory::MenuButton {
                playlist,
                branch_opt: b.branch_opt,
                mark_or_pi: b.mark_or_pi,
                clip_index: b.clip_index,
                page_id: b.page_id,
                button_id: b.button_id,
                breadcrumb: b.breadcrumb.clone(),
                orphan: b.orphan,
            })
        })
        .collect();

    let inventory = reliquary::disc::bdmv::inventory::build_inventory(&analysis, &menu_buttons);

    // Map navigable content back to ExtractedButtons for rendering.
    // The snapshot metadata identifies the winning button by
    // (clip_index, page_id, button_id).
    let content_buttons: Vec<&ExtractedButton> = inventory
        .navigable
        .iter()
        .filter_map(|nav| {
            buttons.iter().find(|b| {
                b.playlist == Some(nav.playlist)
                    && b.clip_index == nav.snapshot.clip_index
                    && b.page_id == nav.snapshot.page_id
                    && b.button_id == nav.snapshot.button_id
            })
        })
        .collect();

    eprintln!(
        "found {} buttons with playlist targets",
        content_buttons.len()
    );

    // 6. Dump page images (diagnostic)
    if let Some(dir) = dump_pages {
        dump_page_images(dir, &content_buttons, &page_compositions, &clip_backgrounds);
    }

    // 7. Dump mode: output resolved mappings without interactive prompt
    if dump {
        output_dump(path, &content_buttons, &analysis);
        return ExitCode::SUCCESS;
    }

    // 7. Present buttons and collect names
    eprintln!();
    let items = if content_buttons.is_empty() {
        eprintln!("no buttons with playlist targets — showing all buttons");
        eprintln!();
        // Print playlist table for manual correlation
        eprintln!("{analysis}");
        prompt_fallback_buttons(&buttons, &analysis, no_images)
    } else {
        prompt_content_buttons(
            &content_buttons,
            &page_compositions,
            &clip_backgrounds,
            &analysis,
            no_images,
        )
    };

    // 8. Show partially used clips (summary after interactive prompt)
    if !analysis.partially_used_clips.is_empty() {
        eprintln!("partially used clips:");
        for puc in &analysis.partially_used_clips {
            let est = format_identify_duration(puc.estimated_duration);
            let used = format_identify_duration(puc.used_duration);
            #[allow(clippy::cast_sign_loss, reason = "coverage is always 0-100%")]
            #[allow(clippy::cast_possible_truncation, reason = "coverage is always 0-100%")]
            let pct = if puc.estimated_duration.as_secs() > 0 {
                (puc.used_duration.as_secs_f64() / puc.estimated_duration.as_secs_f64() * 100.0)
                    as u32
            } else {
                0
            };
            eprintln!(
                "  {clip}  {est}  used {used} ({pct}%)  by MPLS {pl:05}",
                clip = puc.clip_id,
                pl = puc.playlist,
            );
        }
        eprintln!();
    }

    // 9. Output results
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
///
/// Returns the extracted buttons, per-page composition data, and
/// per-clip video background frames (decoded from the menu clip's
/// video stream via `ffmpeg`). The backgrounds are keyed by clip
/// index and used to composite IG overlays on the actual menu art.
#[allow(clippy::print_stderr, reason = "CLI warning output")]
#[allow(clippy::too_many_lines, reason = "per-clip trace adds lines")]
#[allow(
    clippy::type_complexity,
    reason = "returns buttons + page compositions + video backgrounds together"
)]
fn extract_buttons(
    reader: &reliquary::disc::reader::DiscReader,
    analysis: &reliquary::disc::bdmv::BdmvAnalysis,
    vuk: Option<&[u8; 16]>,
    do_trace: bool,
) -> Result<
    (
        Vec<ExtractedButton>,
        Vec<PageComposition>,
        HashMap<usize, Vec<u8>>,
    ),
    String,
> {
    use reliquary::disc::bdmv::{ig, read_clip, rle, ts};

    use reliquary::disc::bdmv::mobj::PlayerContext;

    /// Maximum video clip size to read for background extraction (50 MB).
    /// Menu clips are short still-frames; anything larger is content.
    const MAX_VIDEO_CLIP_SIZE: usize = 50 * 1024 * 1024;

    let mut buttons = Vec::new();
    let mut page_compositions = Vec::new();
    // Per-clip video background RGBA frames (clip_index → RGBA data)
    let mut clip_backgrounds: HashMap<usize, Vec<u8>> = HashMap::new();
    // Cache: video_clip_id → extracted RGBA frame (avoids re-reading/re-decoding)
    let mut frame_cache: HashMap<String, Option<Vec<u8>>> = HashMap::new();
    // Collect raw IG buttons with player context for legacy MOBJ resolution
    let mut ig_buttons: Vec<(ig::Button, PlayerContext)> = Vec::new();
    // Parallel origin tracking: (clip_index, page_id) for each ig_button entry
    let mut ig_button_origins: Vec<(usize, u8)> = Vec::new();
    // Collect page structure per clip for execution-based resolution
    let mut clip_pages: Vec<(u16, Vec<ig::Page>)> = Vec::new();
    // Track clip_index for breadcrumb matching (indexes into clip_pages)
    let mut next_clip_index: usize = 0;
    // Track whether ffmpeg was found (only warn once if missing)
    let mut ffmpeg_warned = false;

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

        if do_trace {
            trace::trace_ig_clip(&ig_clip.clip_id, &ig_stream);
        }

        // Collect buttons from the first display set only. Language
        // variants share identical button programs but have different
        // bitmaps. Using the first display set matches what the resolver
        // uses for page structure and avoids cross-language bitmap
        // collisions when looking up buttons by (clip, page, button_id).
        for ds in ig_stream.display_sets.iter().take(1) {
            let Some(palette) = ds.palettes.first() else {
                continue;
            };

            for comp in &ds.compositions {
                for page in &comp.pages {
                    let mut btn_comps = Vec::new();

                    for button in &page.buttons {
                        // Decode both bitmap states for page composition.
                        let normal_bmp = ds
                            .objects
                            .iter()
                            .find(|o| o.object_id == button.normal_object_id)
                            .and_then(|o| rle::decode(o, palette).ok());
                        let selected_bmp = ds
                            .objects
                            .iter()
                            .find(|o| o.object_id == button.selected_object_id)
                            .and_then(|o| rle::decode(o, palette).ok());

                        btn_comps.push(ButtonComposition {
                            button_id: button.button_id,
                            x: button.x,
                            y: button.y,
                            normal: normal_bmp,
                            selected: selected_bmp,
                        });

                        // For ExtractedButton, prefer selected state bitmap.
                        let bitmap_ref = btn_comps
                            .last()
                            .and_then(|bc| bc.selected.as_ref().or(bc.normal.as_ref()));
                        let Some(bitmap) = bitmap_ref else { continue };

                        // Find PlayPl command if any
                        let play_pl = button.commands.iter().find_map(|cmd| {
                            if let ig::NavigationCommand::PlayPl {
                                playlist,
                                branch_opt,
                                mark_or_pi,
                            } = cmd
                            {
                                Some((*playlist, *branch_opt, *mark_or_pi))
                            } else {
                                None
                            }
                        });

                        let (playlist, branch_opt, mark_or_pi) = match play_pl {
                            Some((pl, bo, mpi)) => (Some(pl), bo, mpi),
                            None => (None, 0, 0),
                        };

                        buttons.push(ExtractedButton {
                            playlist,
                            branch_opt,
                            mark_or_pi,
                            clip_index: next_clip_index,
                            page_id: page.page_id,
                            button_id: button.button_id,
                            breadcrumb: Vec::new(),
                            orphan: false,
                            width: bitmap.width,
                            height: bitmap.height,
                            rgba: bitmap.data.clone(),
                        });

                        // Clone the IG button for MOBJ resolution (only if
                        // no direct PlayPl — avoids unnecessary cloning)
                        if playlist.is_none() {
                            ig_buttons.push((
                                ig::Button {
                                    button_id: button.button_id,
                                    x: button.x,
                                    y: button.y,
                                    upper_button_id: button.upper_button_id,
                                    lower_button_id: button.lower_button_id,
                                    left_button_id: button.left_button_id,
                                    right_button_id: button.right_button_id,
                                    normal_object_id: button.normal_object_id,
                                    selected_object_id: button.selected_object_id,
                                    commands: button.commands.clone(),
                                    bog_id: button.bog_id,
                                },
                                PlayerContext {
                                    ig_stream: ig_pid,
                                    selected_button_id: button.button_id,
                                    page_id: page.page_id,
                                },
                            ));
                            ig_button_origins.push((next_clip_index, page.page_id));
                        }
                    }

                    page_compositions.push(PageComposition {
                        clip_index: next_clip_index,
                        page_id: page.page_id,
                        canvas_width: comp.width,
                        canvas_height: comp.height,
                        buttons: btn_comps,
                    });
                }
            }
        }

        // Extract video background for this clip. The IG clip only
        // contains the overlay — the video background is in a separate
        // clip identified by the MPLS sub-path mapping.
        let canvas_dims = ig_stream
            .display_sets
            .first()
            .and_then(|ds| ds.compositions.first())
            .map(|c| (c.width, c.height));

        // Collect pages for execution-based resolution (one copy per clip,
        // using the first display set's compositions — language variants
        // share identical button programs).
        let pages: Vec<ig::Page> = ig_stream
            .display_sets
            .into_iter()
            .take(1)
            .flat_map(|ds| ds.compositions)
            .flat_map(|c| c.pages)
            .collect();
        if !pages.is_empty() {
            if let Some((w, h)) = canvas_dims {
                // Look up the video clip for this IG clip via sub-path mapping.
                // If no mapping exists, try the IG clip itself (some discs
                // bundle video and IG in the same clip).
                let video_clip_id = analysis
                    .ig_video_clips
                    .get(&ig_clip.clip_id)
                    .map_or_else(|| ig_clip.clip_id.clone(), Clone::clone);

                // Use cached frame if we've already processed this video clip.
                let frame = frame_cache.entry(video_clip_id.clone()).or_insert_with(|| {
                    let clip_data = if video_clip_id == ig_clip.clip_id {
                        if data.len() <= MAX_VIDEO_CLIP_SIZE {
                            Some(data.clone())
                        } else {
                            None
                        }
                    } else {
                        read_clip(reader, vuk, &video_clip_id)
                            .ok()
                            .filter(|d| d.len() <= MAX_VIDEO_CLIP_SIZE)
                    };
                    clip_data.and_then(|d| extract_video_frame(&d, w, h))
                });

                if let Some(bg) = frame {
                    clip_backgrounds.insert(next_clip_index, bg.clone());
                } else if !ffmpeg_warned {
                    eprintln!(
                        "note: could not extract video background \
                         (is ffmpeg installed?)"
                    );
                    ffmpeg_warned = true;
                }
            }
            clip_pages.push((ig_pid, pages));
            next_clip_index += 1;
        }
    }

    // Resolve indirect button → playlist mappings via MovieObject.bdmv
    if !ig_buttons.is_empty() || !clip_pages.is_empty() {
        let valid_playlists: HashSet<u32> = analysis.playlists.iter().map(|p| p.number).collect();
        resolve_mobj_buttons(
            reader,
            &ig_buttons,
            &ig_button_origins,
            &clip_pages,
            &mut buttons,
            &valid_playlists,
            &analysis.menu_playlists,
            do_trace,
        );
    }

    Ok((buttons, page_compositions, clip_backgrounds))
}

/// Resolves indirect button→playlist mappings via `MovieObject.bdmv`.
///
/// Uses execution-based resolution (running button programs through the VM)
/// as the primary strategy, with the legacy pattern-matching resolver as
/// fallback for any buttons the executor doesn't resolve.
#[allow(clippy::print_stderr, reason = "CLI status output")]
#[allow(
    clippy::too_many_lines,
    reason = "pipeline orchestration with two resolver passes"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "origin tracking adds one parameter to the existing pipeline"
)]
fn resolve_mobj_buttons(
    reader: &reliquary::disc::reader::DiscReader,
    ig_buttons: &[(
        reliquary::disc::bdmv::ig::Button,
        reliquary::disc::bdmv::mobj::PlayerContext,
    )],
    ig_button_origins: &[(usize, u8)],
    clip_pages: &[(u16, Vec<reliquary::disc::bdmv::ig::Page>)],
    buttons: &mut Vec<ExtractedButton>,
    valid_playlists: &HashSet<u32>,
    menu_playlists: &[u32],
    do_trace: bool,
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

    if do_trace {
        trace::trace_mobj_structure(&mobj_file, valid_playlists, ig_buttons);
    }

    // Extract dispatch table from central dispatch MOBJ (SET_BUTTON_PAGE pattern)
    let dispatch_table = mobj::extract_dispatch_table(&mobj_file);

    if let Some(ref table) = dispatch_table {
        eprintln!(
            "extracted dispatch table from MOBJ[{}]: {} cases on GPR[{}]",
            table.mobj_index,
            table.cases.len(),
            table.dispatch_register
        );
    }

    if do_trace && let Some(ref table) = dispatch_table {
        trace::trace_composite_dispatch(ig_buttons, table);
        trace::trace_dispatch_handlers(&mobj_file, table);
    }

    if do_trace {
        trace::trace_mobj0_database(&mobj_file);
    }

    // Primary: execution-based resolution (runs ALL button commands
    // through the VM, including CMP/GOTO/AND/ADD in Other variants)
    let nav_clips: Vec<mobj::NavClipInput<'_>> = clip_pages
        .iter()
        .map(|(pid, pages)| mobj::NavClipInput {
            ig_pid: *pid,
            pages: pages.iter().collect(),
        })
        .collect();

    let exec_resolved = mobj::resolve_via_execution(
        &nav_clips,
        &mobj_file,
        dispatch_table.as_ref(),
        valid_playlists,
    );

    if !exec_resolved.is_empty() {
        eprintln!(
            "resolved {} button playlist mappings via execution",
            exec_resolved.len()
        );
    }

    // Fill in execution-resolved playlists. The last breadcrumb step
    // identifies the content button by (clip_index, page_id, button_id);
    // match it against the extracted buttons to fill in the playlist.
    //
    // Fallback: when the exact match fails (e.g. the navigation button
    // that produced a dispatch composite has no bitmap), try any
    // unresolved button on the same clip and page.
    for rp in &exec_resolved {
        let Some(content_step) = rp.breadcrumb.last() else {
            continue;
        };
        let idx = buttons.iter().position(|b| {
            b.clip_index == content_step.clip_index
                && b.page_id == content_step.page_id
                && b.button_id == content_step.button_id
                && b.playlist.is_none()
        });
        if let Some(i) = idx {
            buttons[i].playlist = Some(rp.target.playlist);
            buttons[i].branch_opt = rp.target.branch_opt;
            buttons[i].mark_or_pi = rp.target.mark_or_pi;
            buttons[i].breadcrumb.clone_from(&rp.breadcrumb);
            buttons[i].orphan = rp.orphan;
        } else {
            // Button already occupied by a different playlist — clone
            // with the new resolution so all dispatch composites get
            // breadcrumbs (ticket 06b: multi-composite matchback).
            let source = buttons.iter().position(|b| {
                b.clip_index == content_step.clip_index
                    && b.page_id == content_step.page_id
                    && b.button_id == content_step.button_id
            });
            if let Some(i) = source {
                let width = buttons[i].width;
                let height = buttons[i].height;
                let clip_index = buttons[i].clip_index;
                let page_id = buttons[i].page_id;
                let button_id = buttons[i].button_id;
                let rgba = buttons[i].rgba.clone();
                buttons.push(ExtractedButton {
                    playlist: Some(rp.target.playlist),
                    branch_opt: rp.target.branch_opt,
                    mark_or_pi: rp.target.mark_or_pi,
                    clip_index,
                    page_id,
                    button_id,
                    breadcrumb: rp.breadcrumb.clone(),
                    orphan: rp.orphan,
                    width,
                    height,
                    rgba,
                });
            }
        }
    }

    if do_trace && let Some(ref table) = dispatch_table {
        trace::trace_execution_coverage(&exec_resolved, table, clip_pages);
    }

    if do_trace {
        trace::trace_direct_play_pl(buttons, &exec_resolved);
    }

    // Fallback: legacy pattern-matching resolver for any remaining buttons
    let unresolved_count = buttons.iter().filter(|b| b.playlist.is_none()).count();

    if unresolved_count > 0 && !ig_buttons.is_empty() {
        let menu_set: HashSet<u32> = menu_playlists.iter().copied().collect();
        let dispatch_entries = mobj::find_dispatch_entries(&mobj_file, &menu_set);

        let legacy_resolved = mobj::resolve_buttons(
            ig_buttons,
            &mobj_file,
            valid_playlists,
            &dispatch_entries,
            dispatch_table.as_ref(),
        );

        // Build origin lookup: button_id → [(clip_index, page_id)]
        let mut origin_lookup: HashMap<u16, Vec<(usize, u8)>> = HashMap::new();
        for ((b, _), &(clip_idx, page_id)) in ig_buttons.iter().zip(ig_button_origins) {
            origin_lookup
                .entry(b.button_id)
                .or_default()
                .push((clip_idx, page_id));
        }

        let mut legacy_count = 0u32;
        for rb in &legacy_resolved {
            let Some(origins) = origin_lookup.get(&rb.button_id) else {
                continue;
            };
            for &(clip_index, page_id) in origins {
                if let Some(eb) = buttons.iter_mut().find(|b| {
                    b.button_id == rb.button_id
                        && b.clip_index == clip_index
                        && b.page_id == page_id
                        && b.playlist.is_none()
                }) {
                    eb.playlist = Some(rb.target.playlist);
                    eb.branch_opt = rb.target.branch_opt;
                    eb.mark_or_pi = rb.target.mark_or_pi;
                    legacy_count += 1;
                    break;
                }
            }
        }

        if legacy_count > 0 {
            eprintln!("resolved {legacy_count} additional buttons via legacy pattern matching");
        }
    }
}
