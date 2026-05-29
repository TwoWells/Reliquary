// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Content inventory — categorizes disc content into navigable, auto-play,
//! orphaned, and partially used buckets.
//!
//! The entry point is [`build_inventory`], which takes structural analysis
//! data and resolved menu buttons, applies filtering and deduplication,
//! and returns a [`ContentInventory`] ready for snapshot construction and
//! presentation.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde::Serialize;

use super::mobj::BreadcrumbStep;
use super::{BdmvAnalysis, PartiallyUsedClip, StreamSummary, UnreferencedClip};

// ── Input ──────────────────────────────────────────────────────────────

/// A resolved menu button for inventory construction.
///
/// Carries the resolution data needed for content categorization and
/// deduplication. Bitmap data is a presentation concern and stays in
/// the CLI.
#[derive(Debug, Clone)]
pub struct MenuButton {
    /// Playlist number.
    pub playlist: u16,
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
    pub breadcrumb: Vec<BreadcrumbStep>,
    /// `true` when the content is on a page unreachable from the root menu.
    pub orphan: bool,
}

// ── Output ─────────────────────────────────────────────────────────────

/// A complete inventory of content on a disc.
///
/// Categorizes every watchable item into one of four buckets:
/// - **Navigable** — reachable via disc menu navigation (has a snapshot).
/// - **Auto-play** — pre-menu playlists from the First Play MOBJ chain.
/// - **Orphaned** — clips present on disc but unreachable from any menu.
/// - **Partially used** — clips where only a segment is referenced.
#[derive(Debug, Clone, Serialize)]
pub struct ContentInventory {
    /// Content reachable via menu navigation (snapshot path).
    pub navigable: Vec<NavigableContent>,
    /// Pre-menu auto-play playlists (First Play MOBJ chain).
    pub auto_play: Vec<AutoPlayContent>,
    /// Clips on disc but unreachable from menus or playlists.
    pub orphaned: Vec<UnreferencedClip>,
    /// Clips with unused segments.
    pub partially_used: Vec<PartiallyUsedClip>,
}

/// A playlist reachable from the disc menu.
///
/// Includes playlist metadata (duration, streams, chapters) and snapshot
/// metadata for constructing the navigation snapshot without re-reading
/// the disc.
#[derive(Debug, Clone, Serialize)]
pub struct NavigableContent {
    /// Playlist number.
    pub playlist: u16,
    /// `PlayPl` variant: 0=from start, 1=at mark, 2=at play item.
    pub branch_opt: u8,
    /// Mark index or play item index (meaningful when `branch_opt > 0`).
    pub mark_or_pi: u32,
    /// Total duration.
    #[serde(serialize_with = "super::serialize_duration")]
    pub duration: Duration,
    /// Stream summary (codecs, channels, languages).
    pub streams: StreamSummary,
    /// Number of chapter marks.
    pub chapters: u32,
    /// Snapshot metadata for constructing the navigation snapshot.
    pub snapshot: SnapshotMeta,
}

/// Metadata for constructing a navigation snapshot.
///
/// Identifies which menu clip, page, and button to composite for the
/// snapshot that shows what the user would see on their TV when
/// selecting this content item.
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotMeta {
    /// Index into the clips list — the clip where the content button lives.
    pub clip_index: usize,
    /// Page ID where the content button lives.
    pub page_id: u8,
    /// Button whose selected-state bitmap to composite.
    pub button_id: u16,
    /// Navigation breadcrumb from root to content button.
    ///
    /// Always non-empty: for direct `PlayPl` buttons, contains a single
    /// step at the button's own location.
    pub breadcrumb: Vec<BreadcrumbStep>,
    /// `true` when the content is on a page unreachable from the root menu.
    pub orphan: bool,
}

/// A pre-menu auto-play playlist from the First Play MOBJ chain.
///
/// These playlists play automatically before the menu loads (FBI warning,
/// studio logo, trailer). No menu button exists — labeled by play order.
#[derive(Debug, Clone, Serialize)]
pub struct AutoPlayContent {
    /// Playlist number.
    pub playlist: u16,
    /// Total duration.
    #[serde(serialize_with = "super::serialize_duration")]
    pub duration: Duration,
    /// Play order (1-based).
    pub order: u32,
}

// ── Builder ────────────────────────────────────────────────────────────

/// Builds a content inventory from analysis data and resolved buttons.
///
/// Applies the following filters and transformations:
/// 1. **Playlist validation** — only buttons targeting known playlists.
/// 2. **Composite parent suppression** — playlists that contain other
///    playlists as members are suppressed (the members are the content).
/// 3. **Title table filter** — when `index.bdmv` provides a title table,
///    only playlists registered as titles are included.
/// 4. **Cross-clip dedup** — when the same playlist appears from multiple
///    clips or pages, prefers the resolution with a navigation breadcrumb,
///    then the page with the most sibling content buttons.
/// 5. **Final dedup** — one entry per `(playlist, branch_opt, mark_or_pi)`.
///
/// Enriches each surviving button with playlist metadata (duration,
/// streams, chapters) from the analysis.
#[must_use]
pub fn build_inventory(analysis: &BdmvAnalysis, buttons: &[MenuButton]) -> ContentInventory {
    let valid_playlists: HashSet<u32> = analysis.playlists.iter().map(|p| p.number).collect();

    let composite_parents: HashSet<u32> = analysis
        .playlists
        .iter()
        .filter(|p| !p.members.is_empty())
        .map(|p| p.number)
        .collect();

    let title_playlists = &analysis.title_playlists;
    let use_title_filter = !title_playlists.is_empty();

    // 1-3. Filter: valid playlist, not composite parent, in title table
    let mut candidates: Vec<&MenuButton> = buttons
        .iter()
        .filter(|b| {
            let pl32 = u32::from(b.playlist);
            valid_playlists.contains(&pl32)
                && !composite_parents.contains(&pl32)
                && (!use_title_filter || title_playlists.contains(&pl32))
        })
        .collect();

    // 4. Cross-clip dedup scoring: count content buttons per page
    let page_content_count = count_page_buttons(&candidates);

    // Sort: prefer breadcrumb-having buttons, then pages with more siblings
    candidates.sort_by(|a, b| {
        let a_no_bc = u8::from(a.breadcrumb.is_empty());
        let b_no_bc = u8::from(b.breadcrumb.is_empty());
        a_no_bc.cmp(&b_no_bc).then_with(|| {
            let a_count = page_content_count
                .get(&content_page(a))
                .copied()
                .unwrap_or(0);
            let b_count = page_content_count
                .get(&content_page(b))
                .copied()
                .unwrap_or(0);
            b_count.cmp(&a_count)
        })
    });

    // 5. Final dedup: first arrival per (playlist, branch_opt, mark_or_pi)
    let mut seen = HashSet::new();
    let deduped: Vec<&MenuButton> = candidates
        .into_iter()
        .filter(|b| seen.insert((b.playlist, b.branch_opt, b.mark_or_pi)))
        .collect();

    // Build navigable content with playlist metadata
    let navigable = deduped
        .into_iter()
        .filter_map(|b| build_navigable(b, analysis))
        .collect();

    ContentInventory {
        navigable,
        auto_play: Vec::new(),
        orphaned: analysis.unreferenced_clips.clone(),
        partially_used: analysis.partially_used_clips.clone(),
    }
}

/// Returns the content page key for a button — the page where the content
/// button lives (breadcrumb's last step, or the button's own page).
fn content_page(b: &MenuButton) -> (usize, u8) {
    b.breadcrumb
        .last()
        .map_or((b.clip_index, b.page_id), |s| (s.clip_index, s.page_id))
}

/// Counts content buttons per (`clip_index`, `page_id`) for dedup scoring.
fn count_page_buttons(candidates: &[&MenuButton]) -> HashMap<(usize, u8), usize> {
    let mut counts: HashMap<(usize, u8), usize> = HashMap::new();
    for b in candidates {
        *counts.entry(content_page(b)).or_default() += 1;
    }
    counts
}

/// Builds a [`NavigableContent`] from a button and the analysis data.
///
/// Returns `None` if the playlist is not found in the analysis (should
/// not happen after validation, but avoids panicking).
fn build_navigable(b: &MenuButton, analysis: &BdmvAnalysis) -> Option<NavigableContent> {
    let info = analysis
        .playlists
        .iter()
        .find(|p| p.number == u32::from(b.playlist))?;

    // Resolve the content button location from the breadcrumb or the
    // button's own fields.
    let (snap_clip, snap_page, snap_button) = b
        .breadcrumb
        .last()
        .map_or((b.clip_index, b.page_id, b.button_id), |s| {
            (s.clip_index, s.page_id, s.button_id)
        });

    // Guarantee a non-empty breadcrumb: for direct PlayPl buttons,
    // create a synthetic single-step breadcrumb.
    let breadcrumb = if b.breadcrumb.is_empty() {
        vec![BreadcrumbStep {
            clip_index: b.clip_index,
            page_id: b.page_id,
            button_id: b.button_id,
        }]
    } else {
        b.breadcrumb.clone()
    };

    Some(NavigableContent {
        playlist: b.playlist,
        branch_opt: b.branch_opt,
        mark_or_pi: b.mark_or_pi,
        duration: info.duration,
        streams: info.streams.clone(),
        chapters: info.chapters,
        snapshot: SnapshotMeta {
            clip_index: snap_clip,
            page_id: snap_page,
            button_id: snap_button,
            breadcrumb,
            orphan: b.orphan,
        },
    })
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod tests {
    use super::*;
    use crate::disc::bdmv::{AnalyzedPlaylist, CompositeMember, Segment};

    /// Creates a minimal `AnalyzedPlaylist` for testing.
    fn test_playlist(number: u32, duration_secs: u64, chapters: u32) -> AnalyzedPlaylist {
        AnalyzedPlaylist {
            number,
            duration: Duration::from_secs(duration_secs),
            chapters,
            segments: vec![Segment {
                clips: vec![format!("{number:05}")],
                duration: Duration::from_secs(duration_secs),
            }],
            streams: StreamSummary {
                video: vec!["H.264 1080p 23.976".to_string()],
                audio: vec!["AC-3 5.1 eng".to_string()],
                subtitles: vec![],
            },
            members: vec![],
        }
    }

    /// Creates a minimal `MenuButton` for testing.
    fn test_button(playlist: u16, clip_index: usize, page_id: u8, button_id: u16) -> MenuButton {
        MenuButton {
            playlist,
            branch_opt: 0,
            mark_or_pi: 0,
            clip_index,
            page_id,
            button_id,
            breadcrumb: Vec::new(),
            orphan: false,
        }
    }

    /// Creates a minimal `BdmvAnalysis` with the given playlists.
    fn test_analysis(playlists: Vec<AnalyzedPlaylist>) -> BdmvAnalysis {
        let title_playlists = playlists.iter().map(|p| p.number).collect();
        BdmvAnalysis {
            playlists,
            ig_clips: vec![],
            menu_playlists: vec![],
            unreferenced_clips: vec![],
            partially_used_clips: vec![],
            warnings: vec![],
            clip_warnings: vec![],
            title_playlists,
            ig_video_clips: HashMap::new(),
        }
    }

    #[test]
    fn basic_inventory() {
        let analysis = test_analysis(vec![
            test_playlist(100, 8476, 13),
            test_playlist(201, 600, 1),
            test_playlist(202, 300, 1),
        ]);

        let buttons = vec![
            test_button(100, 0, 0, 1),
            test_button(201, 0, 0, 2),
            test_button(202, 0, 0, 3),
        ];

        let inv = build_inventory(&analysis, &buttons);

        assert_eq!(
            inv.navigable.len(),
            3,
            "all three playlists should be navigable"
        );
        let pl100 = inv
            .navigable
            .iter()
            .find(|n| n.playlist == 100)
            .expect("pl 100 should be present");
        assert_eq!(pl100.chapters, 13, "pl 100 should have 13 chapters");
        assert_eq!(
            pl100.duration,
            Duration::from_secs(8476),
            "pl 100 duration should match"
        );
    }

    #[test]
    fn composite_parents_suppressed() {
        let mut pl100 = test_playlist(100, 8476, 13);
        pl100.members = vec![CompositeMember {
            playlist: 201,
            segment_index: 1,
            range_start: Duration::ZERO,
            range_end: Duration::from_mins(10),
        }];

        let analysis = test_analysis(vec![pl100, test_playlist(201, 600, 1)]);

        let buttons = vec![test_button(100, 0, 0, 1), test_button(201, 0, 1, 2)];

        let inv = build_inventory(&analysis, &buttons);

        assert_eq!(
            inv.navigable.len(),
            1,
            "composite parent pl 100 should be suppressed"
        );
        assert_eq!(
            inv.navigable[0].playlist, 201,
            "only the member playlist should remain"
        );
    }

    #[test]
    fn title_table_filter() {
        let analysis = {
            let mut a = test_analysis(vec![
                test_playlist(100, 8476, 13),
                test_playlist(201, 600, 1),
            ]);
            // Only pl 100 is in the title table
            a.title_playlists = std::iter::once(100).collect();
            a
        };

        let buttons = vec![test_button(100, 0, 0, 1), test_button(201, 0, 1, 2)];

        let inv = build_inventory(&analysis, &buttons);

        assert_eq!(
            inv.navigable.len(),
            1,
            "only title-table playlists should be navigable"
        );
        assert_eq!(
            inv.navigable[0].playlist, 100,
            "pl 100 is in the title table"
        );
    }

    #[test]
    fn dedup_by_playlist() {
        let analysis = test_analysis(vec![test_playlist(100, 8476, 13)]);

        let buttons = vec![test_button(100, 0, 0, 1), test_button(100, 1, 0, 5)];

        let inv = build_inventory(&analysis, &buttons);

        assert_eq!(
            inv.navigable.len(),
            1,
            "duplicate playlist should be deduped"
        );
    }

    #[test]
    fn prefer_breadcrumb_in_dedup() {
        let analysis = test_analysis(vec![test_playlist(100, 8476, 13)]);

        // Button without breadcrumb (direct PlayPl)
        let btn_no_bc = test_button(100, 0, 0, 1);

        // Button with breadcrumb (resolved via MOBJ)
        let mut btn_with_bc = test_button(100, 1, 2, 5);
        btn_with_bc.breadcrumb = vec![
            BreadcrumbStep {
                clip_index: 0,
                page_id: 0,
                button_id: 3,
            },
            BreadcrumbStep {
                clip_index: 1,
                page_id: 2,
                button_id: 5,
            },
        ];

        // Order: no-breadcrumb first, but dedup should prefer breadcrumb
        let buttons = vec![btn_no_bc, btn_with_bc];

        let inv = build_inventory(&analysis, &buttons);

        assert_eq!(inv.navigable.len(), 1, "duplicate should be deduped");
        assert_eq!(
            inv.navigable[0].snapshot.clip_index, 1,
            "breadcrumb-having resolution should win"
        );
    }

    #[test]
    fn prefer_page_with_more_siblings() {
        let analysis = test_analysis(vec![
            test_playlist(100, 8476, 13),
            test_playlist(201, 600, 1),
            test_playlist(202, 300, 1),
        ]);

        // Page (0, 0): only pl 100
        // Page (0, 1): pl 100, pl 201, pl 202 — more siblings
        let buttons = vec![
            test_button(100, 0, 0, 1),
            test_button(100, 0, 1, 4),
            test_button(201, 0, 1, 5),
            test_button(202, 0, 1, 6),
        ];

        let inv = build_inventory(&analysis, &buttons);

        assert_eq!(inv.navigable.len(), 3, "three unique playlists");
        let pl100 = inv
            .navigable
            .iter()
            .find(|n| n.playlist == 100)
            .expect("pl 100 should be present");
        assert_eq!(
            pl100.snapshot.page_id, 1,
            "page with more siblings should win for pl 100"
        );
    }

    #[test]
    fn snapshot_meta_breadcrumb_guaranteed_nonempty() {
        let analysis = test_analysis(vec![test_playlist(100, 8476, 13)]);
        let buttons = vec![test_button(100, 0, 3, 7)];

        let inv = build_inventory(&analysis, &buttons);

        assert_eq!(inv.navigable.len(), 1, "one navigable item");
        let snap = &inv.navigable[0].snapshot;
        assert!(
            !snap.breadcrumb.is_empty(),
            "breadcrumb should be non-empty even for direct PlayPl"
        );
        assert_eq!(
            snap.breadcrumb[0].clip_index, 0,
            "synthetic breadcrumb clip_index"
        );
        assert_eq!(
            snap.breadcrumb[0].page_id, 3,
            "synthetic breadcrumb page_id"
        );
        assert_eq!(
            snap.breadcrumb[0].button_id, 7,
            "synthetic breadcrumb button_id"
        );
    }

    #[test]
    fn orphaned_and_partially_used_passed_through() {
        let mut analysis = test_analysis(vec![test_playlist(100, 8476, 13)]);
        analysis.unreferenced_clips = vec![UnreferencedClip {
            clip_id: "00999".to_string(),
            has_ig: false,
            streams: vec!["H.264".to_string()],
            file_size: 1_000_000,
        }];
        analysis.partially_used_clips = vec![PartiallyUsedClip {
            clip_id: "00100".to_string(),
            estimated_duration: Duration::from_hours(2),
            used_duration: Duration::from_hours(1),
            playlist: 100,
            in_time: 0,
            out_time: 162_000_000,
        }];

        let buttons = vec![test_button(100, 0, 0, 1)];
        let inv = build_inventory(&analysis, &buttons);

        assert_eq!(inv.orphaned.len(), 1, "orphaned clips should pass through");
        assert_eq!(
            inv.orphaned[0].clip_id, "00999",
            "orphaned clip ID should match"
        );
        assert_eq!(
            inv.partially_used.len(),
            1,
            "partially used clips should pass through"
        );
    }

    #[test]
    fn invalid_playlist_filtered() {
        let analysis = test_analysis(vec![test_playlist(100, 8476, 13)]);

        // Button targeting a playlist that doesn't exist in the analysis
        let buttons = vec![test_button(100, 0, 0, 1), test_button(999, 0, 0, 2)];

        let inv = build_inventory(&analysis, &buttons);

        assert_eq!(
            inv.navigable.len(),
            1,
            "invalid playlist should be filtered"
        );
        assert_eq!(
            inv.navigable[0].playlist, 100,
            "only valid playlist remains"
        );
    }

    #[test]
    fn empty_title_table_skips_filter() {
        let mut analysis = test_analysis(vec![
            test_playlist(100, 8476, 13),
            test_playlist(201, 600, 1),
        ]);
        // Empty title table = no title filter applied
        analysis.title_playlists = HashSet::new();

        let buttons = vec![test_button(100, 0, 0, 1), test_button(201, 0, 1, 2)];

        let inv = build_inventory(&analysis, &buttons);

        assert_eq!(
            inv.navigable.len(),
            2,
            "both playlists should pass when title table is empty"
        );
    }

    #[test]
    fn branch_opt_preserved() {
        let analysis = test_analysis(vec![test_playlist(100, 8476, 13)]);

        let mut btn = test_button(100, 0, 0, 1);
        btn.branch_opt = 1;
        btn.mark_or_pi = 5;

        let buttons = vec![btn];
        let inv = build_inventory(&analysis, &buttons);

        assert_eq!(inv.navigable.len(), 1, "one navigable item");
        assert_eq!(
            inv.navigable[0].branch_opt, 1,
            "branch_opt should be preserved"
        );
        assert_eq!(
            inv.navigable[0].mark_or_pi, 5,
            "mark_or_pi should be preserved"
        );
    }

    #[test]
    fn different_branch_opts_not_deduped() {
        let analysis = test_analysis(vec![test_playlist(100, 8476, 13)]);

        let btn_start = test_button(100, 0, 0, 1);
        let mut btn_mark = test_button(100, 0, 0, 2);
        btn_mark.branch_opt = 1;
        btn_mark.mark_or_pi = 3;

        let buttons = vec![btn_start, btn_mark];
        let inv = build_inventory(&analysis, &buttons);

        assert_eq!(
            inv.navigable.len(),
            2,
            "same playlist with different branch_opt should not be deduped"
        );
    }
}
