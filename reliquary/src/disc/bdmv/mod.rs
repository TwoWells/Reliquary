// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! BDMV disc analysis — reads MPLS playlists, filters, deduplicates,
//! and identifies the main title.
//!
//! The public surface is [`analyze`], which returns a [`BdmvAnalysis`].

pub mod mpls;

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use thiserror::Error;

use super::reader::{DiscReader, ReaderError};
use mpls::{PTS_CLOCK_HZ, Playlist};

// ── Errors ──────────────────────────────────────────────────────────────

/// Errors that can occur during BDMV analysis.
#[derive(Debug, Error)]
pub enum BdmvError {
    /// The `BDMV/PLAYLIST/` directory could not be read.
    #[error("failed to read playlist directory: {0}")]
    PlaylistDir(#[source] ReaderError),

    /// An individual MPLS file could not be read.
    #[error("failed to read {path}: {source}")]
    ReadFile {
        /// Path to the MPLS file.
        path: String,
        /// Underlying reader error.
        source: ReaderError,
    },

    /// An MPLS file could not be parsed.
    #[error("failed to parse {path}: {source}")]
    Parse {
        /// Path to the MPLS file.
        path: String,
        /// Parser error.
        source: mpls::MplsError,
    },

    /// No valid playlists found on the disc after filtering.
    #[error("no valid playlists found after filtering")]
    NoPlaylists,
}

// ── Analysis types ──────────────────────────────────────────────────────

/// Complete analysis of a BDMV disc structure.
#[derive(Debug, Clone, Serialize)]
pub struct BdmvAnalysis {
    /// Analyzed playlists after filtering and deduplication.
    pub playlists: Vec<AnalyzedPlaylist>,
    /// Playlist number of the identified main title, if any.
    pub main_title: Option<u32>,
}

/// An analyzed playlist with computed duration and segment grouping.
#[derive(Debug, Clone, Serialize)]
pub struct AnalyzedPlaylist {
    /// Playlist number (from the MPLS filename).
    pub number: u32,
    /// Total duration of all segments.
    #[serde(serialize_with = "serialize_duration")]
    pub duration: Duration,
    /// Number of chapter marks.
    pub chapters: u32,
    /// Segments grouped by connection condition.
    pub segments: Vec<Segment>,
    /// Stream summary from the first play item.
    pub streams: StreamSummary,
}

/// A segment within a playlist — one or more seamlessly connected play items.
#[derive(Debug, Clone, Serialize)]
pub struct Segment {
    /// Clip identifiers in this segment.
    pub clips: Vec<String>,
    /// Duration of this segment.
    #[serde(serialize_with = "serialize_duration")]
    pub duration: Duration,
}

/// Summary of streams available in a playlist.
#[derive(Debug, Clone, Serialize)]
pub struct StreamSummary {
    /// Video stream descriptions (e.g. `"H.264 1080p 23.976"`).
    pub video: Vec<String>,
    /// Audio stream descriptions (e.g. `"AC-3 2.0 eng"`).
    pub audio: Vec<String>,
    /// Subtitle stream descriptions (e.g. `"PGS eng"`).
    pub subtitles: Vec<String>,
}

fn serialize_duration<S: serde::Serializer>(
    duration: &Duration,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_f64(duration.as_secs_f64())
}

// ── Display ─────────────────────────────────────────────────────────────

impl fmt::Display for BdmvAnalysis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for pl in &self.playlists {
            let is_main = self.main_title == Some(pl.number);
            let marker = if is_main { " *" } else { "" };
            writeln!(
                f,
                "MPLS {:05}  {:>10}  {:>3} ch  {}{}",
                pl.number,
                format_duration(pl.duration),
                pl.chapters,
                format_streams(&pl.streams),
                marker,
            )?;

            if pl.segments.len() > 1 {
                for (i, seg) in pl.segments.iter().enumerate() {
                    writeln!(
                        f,
                        "  segment {}: {} ({})",
                        i + 1,
                        seg.clips.join(" + "),
                        format_duration(seg.duration),
                    )?;
                }
            }
        }

        if let Some(main) = self.main_title {
            writeln!(f, "\n* main title: {main:05}")?;
        }

        Ok(())
    }
}

/// Formats a `Duration` as `HH:MM:SS`.
fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    format!("{hours}:{minutes:02}:{seconds:02}")
}

/// Formats a `StreamSummary` as a compact single-line description.
fn format_streams(s: &StreamSummary) -> String {
    let mut parts = Vec::new();

    for v in &s.video {
        parts.push(v.clone());
    }

    if !s.audio.is_empty() {
        let audio_summary: Vec<&str> = s.audio.iter().map(String::as_str).collect();
        parts.push(audio_summary.join(", "));
    }

    if !s.subtitles.is_empty() {
        let sub_langs: Vec<&str> = s.subtitles.iter().map(String::as_str).collect();
        parts.push(format!("subs: {}", sub_langs.join(", ")));
    }

    parts.join(" | ")
}

// ── Analysis entry point ────────────────────────────────────────────────

/// Analyzes a BDMV disc structure.
///
/// Takes a [`DiscReader`] for file access — the reader may be backed by a
/// mounted directory or an ISO image. Looks for playlists at either
/// `PLAYLIST/` or `BDMV/PLAYLIST/` within the reader.
///
/// # Errors
///
/// Returns [`BdmvError`] if the playlist directory cannot be read, any MPLS
/// file fails to parse, or no valid playlists remain after filtering.
pub fn analyze(reader: &DiscReader) -> Result<BdmvAnalysis, BdmvError> {
    let playlist_dir = if reader.read_dir(Path::new("PLAYLIST")).is_ok() {
        Path::new("PLAYLIST").to_path_buf()
    } else {
        Path::new("BDMV").join("PLAYLIST")
    };

    let mut playlists = read_playlists(reader, &playlist_dir)?;

    // Filter looping menus
    playlists.retain(|pl| !is_looping(pl));

    // Deduplicate
    let mut playlists = dedup_playlists(&playlists);

    if playlists.is_empty() {
        return Err(BdmvError::NoPlaylists);
    }

    // Sort by playlist number for deterministic output
    playlists.sort_by_key(|pl| pl.number);

    // Identify main title
    let main_title = identify_main_title(&playlists);

    // Convert to analyzed playlists
    let analyzed: Vec<AnalyzedPlaylist> = playlists.iter().map(analyze_playlist).collect();

    Ok(BdmvAnalysis {
        playlists: analyzed,
        main_title,
    })
}

/// Reads all `.mpls` files from the playlist directory.
fn read_playlists(reader: &DiscReader, dir: &Path) -> Result<Vec<Playlist>, BdmvError> {
    let entries = reader.read_dir(dir).map_err(BdmvError::PlaylistDir)?;

    let mut playlists = Vec::new();
    let mut mpls_names: Vec<_> = entries
        .into_iter()
        .filter(|name| name.to_ascii_lowercase().ends_with(".mpls"))
        .collect();
    mpls_names.sort();

    for name in mpls_names {
        let number = name
            .strip_suffix(".mpls")
            .or_else(|| name.strip_suffix(".MPLS"))
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);

        let file_path = dir.join(&name);
        let data = reader
            .read_file(&file_path)
            .map_err(|e| BdmvError::ReadFile {
                path: file_path.display().to_string(),
                source: e,
            })?;

        let playlist = mpls::parse(&data, number).map_err(|e| BdmvError::Parse {
            path: file_path.display().to_string(),
            source: e,
        })?;

        playlists.push(playlist);
    }

    Ok(playlists)
}

/// Returns `true` if a playlist is a looping menu.
///
/// A playlist is considered looping if any `(clip_id, in_time, out_time)`
/// tuple appears more than twice.
fn is_looping(playlist: &Playlist) -> bool {
    let mut counts: HashMap<(&str, u32, u32), u32> = HashMap::new();
    for item in &playlist.play_items {
        let key = (item.clip_id.as_str(), item.in_time, item.out_time);
        let count = counts.entry(key).or_insert(0);
        *count += 1;
        if *count > 2 {
            return true;
        }
    }
    false
}

/// Deduplicates playlists by their clip signature.
///
/// Two playlists are duplicates if they reference the same clips in the
/// same order with the same IN/OUT timestamps. Among duplicates, prefer
/// the one with the most chapter marks, then the most streams.
fn dedup_playlists(playlists: &[Playlist]) -> Vec<Playlist> {
    let mut groups: HashMap<Vec<(&str, u32, u32)>, Vec<&Playlist>> = HashMap::new();

    for pl in playlists {
        let sig: Vec<(&str, u32, u32)> = pl
            .play_items
            .iter()
            .map(|item| (item.clip_id.as_str(), item.in_time, item.out_time))
            .collect();
        groups.entry(sig).or_default().push(pl);
    }

    let mut result: Vec<Playlist> = Vec::new();
    for (_sig, mut group) in groups {
        group.sort_by(|a, b| {
            // Most marks first
            let marks = b.marks.len().cmp(&a.marks.len());
            if marks != std::cmp::Ordering::Equal {
                return marks;
            }
            // Most streams first
            let a_streams = a.play_items.first().map_or(0, |i| {
                i.streams.video.len() + i.streams.audio.len() + i.streams.subtitles.len()
            });
            let b_streams = b.play_items.first().map_or(0, |i| {
                i.streams.video.len() + i.streams.audio.len() + i.streams.subtitles.len()
            });
            let streams = b_streams.cmp(&a_streams);
            if streams != std::cmp::Ordering::Equal {
                return streams;
            }
            // Lowest number as tiebreaker
            a.number.cmp(&b.number)
        });
        result.push(group[0].clone());
    }

    result
}

/// Identifies the main title playlist.
///
/// Selects the playlist with the most play items. If all playlists have
/// the same number of play items, falls back to the longest duration.
fn identify_main_title(playlists: &[Playlist]) -> Option<u32> {
    if playlists.is_empty() {
        return None;
    }

    let max_items = playlists
        .iter()
        .map(|pl| pl.play_items.len())
        .max()
        .unwrap_or(0);

    let candidates: Vec<&Playlist> = playlists
        .iter()
        .filter(|pl| pl.play_items.len() == max_items)
        .collect();

    if candidates.len() == 1 {
        return Some(candidates[0].number);
    }

    // Fall back to longest duration
    candidates
        .iter()
        .max_by_key(|pl| playlist_duration(pl))
        .map(|pl| pl.number)
}

/// Computes the total duration of a raw playlist in PTS ticks.
fn playlist_duration(playlist: &Playlist) -> u64 {
    playlist
        .play_items
        .iter()
        .map(|item| u64::from(item.out_time.saturating_sub(item.in_time)))
        .sum()
}

/// Converts PTS ticks to a `Duration`.
#[allow(
    clippy::cast_precision_loss,
    reason = "PTS ticks fit well within f64 mantissa range for any real disc"
)]
fn pts_to_duration(ticks: u64) -> Duration {
    Duration::from_secs_f64(ticks as f64 / f64::from(PTS_CLOCK_HZ))
}

/// Analyzes a single playlist into the public analysis struct.
fn analyze_playlist(playlist: &Playlist) -> AnalyzedPlaylist {
    let segments = group_segments(playlist);

    let total_ticks: u64 = playlist
        .play_items
        .iter()
        .map(|item| u64::from(item.out_time.saturating_sub(item.in_time)))
        .sum();

    // Count only entry marks (mark_type == 1, chapter marks)
    #[allow(
        clippy::cast_possible_truncation,
        reason = "no disc has more than u32::MAX chapter marks"
    )]
    let chapters = playlist.marks.iter().filter(|m| m.mark_type == 1).count() as u32;

    let streams = playlist.play_items.first().map_or_else(
        || StreamSummary {
            video: Vec::new(),
            audio: Vec::new(),
            subtitles: Vec::new(),
        },
        |item| summarize_streams(&item.streams),
    );

    AnalyzedPlaylist {
        number: playlist.number,
        duration: pts_to_duration(total_ticks),
        chapters,
        segments,
        streams,
    }
}

/// Groups play items into segments by connection condition.
///
/// A new segment starts at `connection_condition` = 1. Items with
/// `connection_condition` >= 5 are appended to the current segment.
fn group_segments(playlist: &Playlist) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut current_clips = Vec::new();
    let mut current_ticks: u64 = 0;

    for item in &playlist.play_items {
        if item.connection_condition < 5 && !current_clips.is_empty() {
            segments.push(Segment {
                clips: current_clips,
                duration: pts_to_duration(current_ticks),
            });
            current_clips = Vec::new();
            current_ticks = 0;
        }
        current_clips.push(item.clip_id.clone());
        current_ticks += u64::from(item.out_time.saturating_sub(item.in_time));
    }

    if !current_clips.is_empty() {
        segments.push(Segment {
            clips: current_clips,
            duration: pts_to_duration(current_ticks),
        });
    }

    segments
}

/// Summarizes streams from an STN table into human-readable strings.
fn summarize_streams(stn: &mpls::StnTable) -> StreamSummary {
    let video = stn
        .video
        .iter()
        .map(|v| {
            format!(
                "{} {} {}fps",
                mpls::video_codec_name(v.coding_type),
                mpls::video_resolution(v.video_format),
                mpls::video_frame_rate(v.frame_rate),
            )
        })
        .collect();

    let audio = stn
        .audio
        .iter()
        .map(|a| {
            format!(
                "{} {} {}",
                mpls::audio_codec_name(a.coding_type),
                mpls::audio_channels(a.audio_format),
                a.language,
            )
        })
        .collect();

    let subtitles = stn
        .subtitles
        .iter()
        .map(|s| {
            let codec = if s.coding_type == 0x92 { "Text" } else { "PGS" };
            format!("{codec} {}", s.language)
        })
        .collect();

    StreamSummary {
        video,
        audio,
        subtitles,
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod tests {
    use super::*;
    use crate::disc::bdmv::mpls::tests::MplsBuilder;

    /// Creates a `Playlist` from builder data for analysis tests.
    fn build_playlist(number: u32, data: &[u8]) -> Playlist {
        mpls::parse(data, number).expect("test playlist should parse")
    }

    #[test]
    fn filter_looping_menu() {
        // 82 items, clip 00013 repeated 81 times — Pattern E
        let mut builder = MplsBuilder::new().play_item("00012", 524_295, 3_474_720);
        for _ in 0..81 {
            builder = builder.play_item_seamless("00013", 524_295, 3_227_480);
        }
        let data = builder.build();
        let pl = build_playlist(10, &data);
        assert!(is_looping(&pl), "looping menu should be detected");
    }

    #[test]
    fn non_looping_playlist_not_filtered() {
        let data = MplsBuilder::new()
            .play_item("00004", 27_000_000, 59_040_000)
            .play_item("00005", 27_000_000, 59_175_000)
            .play_item("00006", 27_000_000, 59_310_000)
            .build();
        let pl = build_playlist(1, &data);
        assert!(!is_looping(&pl), "normal playlist should not be filtered");
    }

    #[test]
    fn dedup_keeps_most_chapters() {
        // Two playlists with the same clips but different mark counts
        let data1 = MplsBuilder::new()
            .play_item("00004", 27_000_000, 59_040_000)
            .mark(0, 27_000_000)
            .build();
        let data2 = MplsBuilder::new()
            .play_item("00004", 27_000_000, 59_040_000)
            .mark(0, 27_000_000)
            .mark(0, 28_000_000)
            .mark(0, 29_000_000)
            .build();

        let pl1 = build_playlist(800, &data1);
        let pl2 = build_playlist(850, &data2);

        let result = dedup_playlists(&[pl1, pl2]);
        assert_eq!(result.len(), 1, "should dedup to one playlist");
        assert_eq!(
            result[0].number, 850,
            "should keep the playlist with more marks"
        );
    }

    #[test]
    fn main_title_most_items() {
        let main_data = MplsBuilder::new()
            .play_item("00004", 27_000_000, 59_040_000)
            .play_item("00005", 27_000_000, 59_175_000)
            .play_item("00006", 27_000_000, 59_310_000)
            .build();
        let extra_data = MplsBuilder::new()
            .play_item("00010", 27_000_000, 30_000_000)
            .build();

        let main = build_playlist(1, &main_data);
        let extra = build_playlist(20, &extra_data);

        let result = identify_main_title(&[main, extra]);
        assert_eq!(result, Some(1), "main title should be playlist 1");
    }

    #[test]
    fn main_title_falls_back_to_duration() {
        let long_data = MplsBuilder::new()
            .play_item("00004", 27_000_000, 270_000_000)
            .build();
        let short_data = MplsBuilder::new()
            .play_item("00010", 27_000_000, 30_000_000)
            .build();

        let long = build_playlist(200, &long_data);
        let short = build_playlist(100, &short_data);

        let result = identify_main_title(&[long, short]);
        assert_eq!(
            result,
            Some(200),
            "should pick longest when item counts match"
        );
    }

    #[test]
    fn segments_grouped_by_connection() {
        let data = MplsBuilder::new()
            .play_item("00100", 188_955_000, 222_570_000)
            .play_item_seamless("00102", 222_525_000, 498_645_000)
            .play_item_seamless("00103", 498_375_000, 511_155_000)
            .build();
        let pl = build_playlist(200, &data);
        let analyzed = analyze_playlist(&pl);

        assert_eq!(
            analyzed.segments.len(),
            1,
            "all seamless items form one segment"
        );
        assert_eq!(
            analyzed.segments[0].clips,
            vec!["00100", "00102", "00103"],
            "segment contains all clips"
        );
    }

    #[test]
    fn segments_split_at_non_seamless() {
        // Simulating Pattern C variant with 2 episodes × 3 segments
        let data = MplsBuilder::new()
            .play_item("00100", 27_000_000, 28_350_000)
            .play_item_seamless("00003", 27_000_000, 85_500_000)
            .play_item_seamless("00101", 27_000_000, 29_700_000)
            .play_item("00100", 27_000_000, 28_350_000)
            .play_item_seamless("00004", 27_000_000, 84_600_000)
            .play_item_seamless("00101", 27_000_000, 29_700_000)
            .build();
        let pl = build_playlist(1, &data);
        let analyzed = analyze_playlist(&pl);

        assert_eq!(analyzed.segments.len(), 2, "two episodes/segments");
        assert_eq!(
            analyzed.segments[0].clips,
            vec!["00100", "00003", "00101"],
            "episode 1 clips"
        );
        assert_eq!(
            analyzed.segments[1].clips,
            vec!["00100", "00004", "00101"],
            "episode 2 clips"
        );
    }
}
