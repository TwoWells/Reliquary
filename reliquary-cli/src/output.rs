// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Output formatting — dump, text, and JSON report modes.

use crate::identify::{ExtractedButton, NamedItem};

/// Dumps resolved button→playlist mappings as JSON without prompting.
#[allow(clippy::print_stdout, reason = "CLI result output to stdout")]
#[allow(clippy::print_stderr, reason = "CLI error output")]
pub fn output_dump(
    path: &std::path::Path,
    buttons: &[&ExtractedButton],
    analysis: &reliquary::disc::bdmv::BdmvAnalysis,
) {
    let items: Vec<serde_json::Value> = buttons
        .iter()
        .filter_map(|b| {
            let pl = b.playlist?;
            let breadcrumb_json: Vec<serde_json::Value> = b
                .breadcrumb
                .iter()
                .map(|step| {
                    serde_json::json!({
                        "clip": step.clip_index,
                        "page": step.page_id,
                        "button": step.button_id,
                    })
                })
                .collect();
            let mut entry = serde_json::json!({
                "playlist": pl,
                "button_id": b.button_id,
                "breadcrumb": breadcrumb_json,
                "orphan": b.orphan,
            });

            if b.branch_opt != 0 {
                entry["branch_opt"] = serde_json::json!(b.branch_opt);
                entry["mark_or_pi"] = serde_json::json!(b.mark_or_pi);
            }

            if let Some(info) = analysis
                .playlists
                .iter()
                .find(|p| p.number == u32::from(pl))
            {
                entry["duration"] = serde_json::json!(info.duration.as_secs_f64());
                entry["chapters"] = serde_json::json!(info.chapters);
            }

            Some(entry)
        })
        .collect();

    let mut output = serde_json::json!({
        "path": path.display().to_string(),
        "items": items,
    });

    if !analysis.title_playlists.is_empty() {
        let mut sorted: Vec<u32> = analysis.title_playlists.iter().copied().collect();
        sorted.sort_unstable();
        output["title_playlists"] = serde_json::json!(sorted);
    }

    if !analysis.partially_used_clips.is_empty() {
        let partial: Vec<serde_json::Value> = analysis
            .partially_used_clips
            .iter()
            .map(|p| {
                serde_json::json!({
                    "clip_id": p.clip_id,
                    "estimated_duration": p.estimated_duration.as_secs_f64(),
                    "used_duration": p.used_duration.as_secs_f64(),
                    "playlist": p.playlist,
                })
            })
            .collect();
        output["partially_used_clips"] = serde_json::json!(partial);
    }

    match serde_json::to_string_pretty(&output) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("error: failed to serialize JSON: {e}"),
    }
}

/// Outputs the text report to stdout.
#[allow(clippy::print_stdout, reason = "CLI result output to stdout")]
pub fn output_text(path: &std::path::Path, items: &[NamedItem]) {
    println!("# reliquary identify: {}", path.display());
    for item in items {
        let variant = match item.branch_opt {
            1 => format!(" @mark {}", item.mark_or_pi),
            2 => format!(" @PI {}", item.mark_or_pi),
            _ => String::new(),
        };
        if item.name.is_empty() {
            println!("playlist {:03}{variant}: (skipped)", item.playlist);
        } else {
            println!("playlist {:03}{variant}: {}", item.playlist, item.name);
        }
    }
}

/// Outputs the JSON report to stdout.
#[allow(clippy::print_stdout, reason = "CLI result output to stdout")]
#[allow(clippy::print_stderr, reason = "CLI error output")]
pub fn output_json(
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

            if item.branch_opt != 0 {
                entry["branch_opt"] = serde_json::json!(item.branch_opt);
                entry["mark_or_pi"] = serde_json::json!(item.mark_or_pi);
            }

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
