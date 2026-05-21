// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Interactive naming prompts for content buttons.

use std::collections::{HashMap, HashSet};

use crate::identify::{ExtractedButton, NamedItem, PageComposition};
use crate::render::render_bitmap;
use crate::snapshot::{composite_page, draw_highlight_border};
use crate::util::{format_identify_duration, format_identify_streams};

/// Returns the leaf page key for a content button.
///
/// The leaf page is where the content button lives — the last breadcrumb
/// step's page, or the button's own page if no breadcrumb exists.
fn leaf_page(button: &ExtractedButton) -> (usize, u8) {
    button
        .breadcrumb
        .last()
        .map_or((button.clip_index, button.page_id), |step| {
            (step.clip_index, step.page_id)
        })
}

/// Presents content buttons grouped by menu page.
///
/// For each unique page that contains content buttons:
/// 1. Renders the full page (video background + IG overlay, no highlight)
///    so the user sees all labels and thumbnails at once.
/// 2. For each content button on that page, renders the page again with
///    that button highlighted and prompts for a name.
///
/// Pages are sorted so sub-pages (with visible button highlights) appear
/// before the main menu page. Orphan content appears last.
#[allow(clippy::print_stderr, reason = "CLI interactive output")]
#[allow(
    clippy::too_many_lines,
    reason = "page-grouped rendering with overview + per-item highlights"
)]
pub fn prompt_content_buttons(
    buttons: &[&ExtractedButton],
    pages: &[PageComposition],
    backgrounds: &HashMap<usize, Vec<u8>>,
    analysis: &reliquary::disc::bdmv::BdmvAnalysis,
    no_images: bool,
) -> Vec<NamedItem> {
    let mut items = Vec::new();
    let stdin = std::io::stdin();

    // Group content buttons by their leaf page (clip_index, page_id).
    let mut groups: HashMap<(usize, u8), Vec<&ExtractedButton>> = HashMap::new();
    for &button in buttons {
        groups.entry(leaf_page(button)).or_default().push(button);
    }

    // Sort groups: deeper sub-pages first (more likely to have visible
    // button highlights), orphans last.
    let mut sorted: Vec<((usize, u8), Vec<&ExtractedButton>)> = groups.into_iter().collect();
    sorted.sort_by(|ga, gb| {
        let a_orphan = ga.1.first().is_some_and(|btn| btn.orphan);
        let b_orphan = gb.1.first().is_some_and(|btn| btn.orphan);
        let a_depth = ga.1.first().map_or(0, |btn| btn.breadcrumb.len());
        let b_depth = gb.1.first().map_or(0, |btn| btn.breadcrumb.len());
        a_orphan
            .cmp(&b_orphan)
            .then_with(|| b_depth.cmp(&a_depth))
            .then_with(|| ga.0.cmp(&gb.0))
    });

    let multiple_clips = sorted
        .iter()
        .map(|(key, _)| key.0)
        .collect::<HashSet<_>>()
        .len()
        > 1;

    for ((clip_index, page_id), group_buttons) in &sorted {
        // Find the page composition for this group
        let page = pages
            .iter()
            .find(|p| p.clip_index == *clip_index && p.page_id == *page_id);

        // Page header
        let orphan_label = if group_buttons.first().is_some_and(|btn| btn.orphan) {
            " [orphan]"
        } else {
            ""
        };
        if multiple_clips {
            eprintln!(
                "── clip {clip_index} page {page_id}{orphan_label} ({} items) ──",
                group_buttons.len()
            );
        } else {
            eprintln!(
                "── page {page_id}{orphan_label} ({} items) ──",
                group_buttons.len()
            );
        }

        // Render page overview (no highlight) — shows all labels/thumbnails
        if !no_images && let Some(p) = page {
            let bg = backgrounds.get(&p.clip_index).map(Vec::as_slice);
            let canvas = composite_page(p, None, bg);
            render_bitmap(p.canvas_width, p.canvas_height, &canvas);
        }
        eprintln!();

        // Each content button with its own highlighted image
        for button in group_buttons {
            let Some(playlist) = button.playlist else {
                continue;
            };

            let pl_info = analysis
                .playlists
                .iter()
                .find(|p| p.number == u32::from(playlist));

            let variant_suffix = match button.branch_opt {
                1 => format!(" @mark {}", button.mark_or_pi),
                2 => format!(" @PI {}", button.mark_or_pi),
                _ => String::new(),
            };

            if let Some(info) = pl_info {
                let duration = format_identify_duration(info.duration);
                let streams = format_identify_streams(&info.streams);
                eprintln!(
                    "  Playlist {playlist:03}{variant_suffix}: {duration}  {streams}  {} ch",
                    info.chapters
                );
            } else {
                eprintln!("  Playlist {playlist:03}{variant_suffix}:");
            }

            // Render page with this button highlighted + border
            if !no_images && let Some(p) = page {
                let highlight_id = button
                    .breadcrumb
                    .last()
                    .map_or(button.button_id, |s| s.button_id);
                let bg = backgrounds.get(&p.clip_index).map(Vec::as_slice);
                let mut canvas = composite_page(p, Some(highlight_id), bg);

                // Draw a visible border so the user can locate the button
                // even when the selected-state bitmap is invisible.
                if let Some(btn_comp) = p.buttons.iter().find(|b| b.button_id == highlight_id) {
                    let bmp = btn_comp.selected.as_ref().or(btn_comp.normal.as_ref());
                    if let Some(bmp) = bmp {
                        draw_highlight_border(
                            &mut canvas,
                            usize::from(p.canvas_width),
                            usize::from(p.canvas_height),
                            usize::from(btn_comp.x),
                            usize::from(btn_comp.y),
                            usize::from(bmp.width),
                            usize::from(bmp.height),
                        );
                    }
                }

                render_bitmap(p.canvas_width, p.canvas_height, &canvas);
            }

            // Prompt for name
            eprint!("  Name: ");
            let mut name = String::new();
            if stdin.read_line(&mut name).is_err() {
                return items;
            }
            let name = name.trim().to_string();

            if name == "q" {
                return items;
            }

            items.push(NamedItem {
                playlist,
                branch_opt: button.branch_opt,
                mark_or_pi: button.mark_or_pi,
                name,
            });

            eprintln!();
        }
    }

    items
}

/// Presents all buttons (fallback mode — no `PlayPl`) and prompts for names.
///
/// In this mode the user sees each bitmap and enters a playlist number
/// and name manually, correlating with the inspect output printed above.
#[allow(clippy::print_stderr, reason = "CLI interactive output")]
pub fn prompt_fallback_buttons(
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

        items.push(NamedItem {
            playlist,
            branch_opt: 0,
            mark_or_pi: 0,
            name,
        });

        eprintln!();
    }

    items
}
