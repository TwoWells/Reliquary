// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! BDMV disc analysis — reads MPLS playlists, filters, deduplicates,
//! and identifies the main title.
//!
//! The public surface is [`analyze`], which returns a [`BdmvAnalysis`].

pub mod clpi;
pub mod mpls;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::Range;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use thiserror::Error;

use super::reader::{DiscReader, ReaderError};
use clpi::ClipInfo;
use mpls::{PTS_CLOCK_HZ, Playlist};

// ── Errors ──────────────────────────────────────────────────────────────

/// Errors that can occur during BDMV analysis.
#[derive(Debug, Error)]
pub enum BdmvError {
    /// The `BDMV/PLAYLIST/` directory could not be read.
    #[error("failed to read playlist directory: {0}")]
    PlaylistDir(#[source] ReaderError),

    /// No valid playlists found on the disc after filtering.
    #[error("no valid playlists found after filtering")]
    NoPlaylists,
}

/// A per-clip error encountered during analysis (non-fatal).
#[derive(Debug, Clone, Serialize)]
pub struct ClipWarning {
    /// Clip ID that failed (from the CLPI filename).
    pub clip_id: String,
    /// Description of the error.
    pub message: String,
}

/// A per-playlist error encountered during analysis (non-fatal).
#[derive(Debug, Clone, Serialize)]
pub struct PlaylistWarning {
    /// Playlist number that failed (from the MPLS filename).
    pub playlist: u32,
    /// Description of the error.
    pub message: String,
}

// ── Analysis types ──────────────────────────────────────────────────────

/// Complete analysis of a BDMV disc structure.
#[derive(Debug, Clone, Serialize)]
pub struct BdmvAnalysis {
    /// Analyzed playlists after filtering and deduplication.
    pub playlists: Vec<AnalyzedPlaylist>,
    /// Playlist number of the identified main title, if any.
    pub main_title: Option<u32>,
    /// Clips with IG streams (menu clips for `identify`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ig_clips: Vec<IgClip>,
    /// Clips not referenced by any playlist.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unreferenced_clips: Vec<UnreferencedClip>,
    /// Warnings from playlists that could not be read or parsed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<PlaylistWarning>,
    /// Warnings from clips that could not be read or parsed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub clip_warnings: Vec<ClipWarning>,
}

/// Clip with IG streams — a menu clip for `identify`.
#[derive(Debug, Clone, Serialize)]
pub struct IgClip {
    /// Clip ID (e.g. `"00291"`).
    pub clip_id: String,
    /// Application type from `ClipInfo`.
    pub application_type: u8,
    /// Estimated m2ts file size in bytes.
    pub file_size: u64,
    /// IG stream PIDs and languages.
    pub ig_streams: Vec<IgStream>,
}

/// An IG stream within a clip.
#[derive(Debug, Clone, Serialize)]
pub struct IgStream {
    /// MPEG-TS PID.
    pub pid: u16,
    /// Three-letter ISO 639-2 language code.
    pub language: String,
}

/// A clip present in CLIPINF but not referenced by any MPLS playlist.
#[derive(Debug, Clone, Serialize)]
pub struct UnreferencedClip {
    /// Clip ID (e.g. `"00291"`).
    pub clip_id: String,
    /// Whether this clip has IG streams.
    pub has_ig: bool,
    /// Stream summary (coding type names).
    pub streams: Vec<String>,
    /// Estimated m2ts file size in bytes.
    pub file_size: u64,
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
    /// Playlists whose content is contained within this playlist (composite grouping).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<CompositeMember>,
}

/// A member of a composite playlist group.
#[derive(Debug, Clone, Serialize)]
pub struct CompositeMember {
    /// Playlist number of the member.
    pub playlist: u32,
    /// 1-based position of this member within the composite's segment list.
    pub segment_index: u32,
    /// Start of the range used by the composite, relative to the member's own timeline.
    #[serde(serialize_with = "serialize_duration")]
    pub range_start: Duration,
    /// End of the range used by the composite, relative to the member's own timeline.
    #[serde(serialize_with = "serialize_duration")]
    pub range_end: Duration,
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
        // Collect playlist numbers that appear as composite members
        let member_set: HashSet<u32> = self
            .playlists
            .iter()
            .flat_map(|pl| pl.members.iter().map(|m| m.playlist))
            .collect();

        // Index playlists by number for member lookups
        let by_number: HashMap<u32, &AnalyzedPlaylist> =
            self.playlists.iter().map(|pl| (pl.number, pl)).collect();

        for pl in &self.playlists {
            // Skip playlists that only appear as composite members
            if member_set.contains(&pl.number) {
                continue;
            }

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

            if !pl.members.is_empty() {
                for member in &pl.members {
                    let (dur, chs) = by_number.get(&member.playlist).map_or_else(
                        || (String::from("?"), String::from("?")),
                        |m| (format_duration(m.duration), format!("{}", m.chapters)),
                    );
                    writeln!(
                        f,
                        "  {:02}: MPLS {:05}  {:>10}  {:>3} ch  {}\u{2013}{}",
                        member.segment_index,
                        member.playlist,
                        dur,
                        chs,
                        format_duration(member.range_start),
                        format_duration(member.range_end),
                    )?;
                }
            } else if pl.segments.len() > 1 {
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

        // IG clips
        if !self.ig_clips.is_empty() {
            writeln!(f)?;
            for ig in &self.ig_clips {
                let langs: Vec<&str> = ig.ig_streams.iter().map(|s| s.language.as_str()).collect();
                writeln!(
                    f,
                    "IG {:>5}  app_type={} {:>8}  {}",
                    ig.clip_id,
                    ig.application_type,
                    format_file_size(ig.file_size),
                    langs.join(", "),
                )?;
            }
        }

        // Unreferenced clips
        if !self.unreferenced_clips.is_empty() {
            writeln!(f)?;
            for clip in &self.unreferenced_clips {
                let ig_marker = if clip.has_ig { " [IG]" } else { "" };
                writeln!(
                    f,
                    "unreferenced {:>5}  {:>8}  {}{}",
                    clip.clip_id,
                    format_file_size(clip.file_size),
                    clip.streams.join(", "),
                    ig_marker,
                )?;
            }
        }

        // Warnings
        for w in &self.warnings {
            writeln!(f, "warning: MPLS {:05}: {}", w.playlist, w.message)?;
        }
        for w in &self.clip_warnings {
            writeln!(f, "warning: CLPI {}: {}", w.clip_id, w.message)?;
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

/// Formats a byte size as a human-readable string (KB, MB, GB).
#[allow(
    clippy::cast_precision_loss,
    reason = "file sizes fit well within f64 mantissa range for any real disc"
)]
fn format_file_size(bytes: u64) -> String {
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

/// Returns a human-readable name for a stream coding type.
const fn stream_coding_name(coding_type: u8) -> &'static str {
    match coding_type {
        0x01 => "MPEG-1",
        0x02 => "MPEG-2",
        0x03 | 0x04 => "MPEG Audio",
        0xea => "VC-1",
        0x1b => "H.264",
        0x24 => "HEVC",
        0x80 => "LPCM",
        0x81 => "AC-3",
        0x82 => "DTS",
        0x83 => "TrueHD",
        0x84 => "E-AC-3",
        0x85 => "DTS-HD HR",
        0x86 => "DTS-HD MA",
        0x90 => "PGS",
        0x91 => "IG",
        0x92 => "Text",
        0xa1 => "E-AC-3 2nd",
        0xa2 => "DTS-HD 2nd",
        _ => "Unknown",
    }
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
/// Returns [`BdmvError`] if the playlist directory cannot be read or no
/// valid playlists remain after filtering. Individual MPLS read/parse
/// failures are reported as warnings in the result rather than aborting
/// the analysis.
pub fn analyze(reader: &DiscReader) -> Result<BdmvAnalysis, BdmvError> {
    let playlist_dir = if reader.read_dir(Path::new("PLAYLIST")).is_ok() {
        Path::new("PLAYLIST").to_path_buf()
    } else {
        Path::new("BDMV").join("PLAYLIST")
    };

    let (mut playlists, warnings) = read_playlists(reader, &playlist_dir)?;

    // Filter looping menus
    playlists.retain(|pl| !is_looping(pl));

    // Deduplicate
    let mut playlists = dedup_playlists(&playlists);

    if playlists.is_empty() && warnings.is_empty() {
        return Err(BdmvError::NoPlaylists);
    }

    // Sort by playlist number for deterministic output
    playlists.sort_by_key(|pl| pl.number);

    // Identify main title
    let main_title = identify_main_title(&playlists);

    // Convert to analyzed playlists
    let mut analyzed: Vec<AnalyzedPlaylist> = playlists.iter().map(analyze_playlist).collect();

    // Composite grouping
    apply_composite_grouping(&playlists, &mut analyzed);

    // Read CLPI files
    let clipinf_dir = if reader.read_dir(Path::new("CLIPINF")).is_ok() {
        Some(Path::new("CLIPINF").to_path_buf())
    } else if reader.read_dir(&Path::new("BDMV").join("CLIPINF")).is_ok() {
        Some(Path::new("BDMV").join("CLIPINF"))
    } else {
        None
    };

    let (ig_clips, unreferenced_clips, clip_warnings) = clipinf_dir.map_or_else(
        || (Vec::new(), Vec::new(), Vec::new()),
        |dir| {
            let (clips, clip_warns) = read_clips(reader, &dir);
            let ig = identify_ig_clips(&clips);
            let unreferenced = find_unreferenced_clips(&clips, &playlists);
            (ig, unreferenced, clip_warns)
        },
    );

    Ok(BdmvAnalysis {
        playlists: analyzed,
        main_title,
        ig_clips,
        unreferenced_clips,
        warnings,
        clip_warnings,
    })
}

/// Reads all `.mpls` files from the playlist directory.
///
/// Returns the successfully parsed playlists and any per-file warnings.
/// The playlist directory itself must be readable (fatal error), but
/// individual file read or parse failures are collected as warnings.
fn read_playlists(
    reader: &DiscReader,
    dir: &Path,
) -> Result<(Vec<Playlist>, Vec<PlaylistWarning>), BdmvError> {
    let entries = reader.read_dir(dir).map_err(BdmvError::PlaylistDir)?;

    let mut playlists = Vec::new();
    let mut warnings = Vec::new();
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

        let data = match reader.read_file(&file_path) {
            Ok(data) => data,
            Err(e) => {
                warnings.push(PlaylistWarning {
                    playlist: number,
                    message: format!("failed to read {}: {e}", file_path.display()),
                });
                continue;
            }
        };

        match mpls::parse(&data, number) {
            Ok(playlist) => playlists.push(playlist),
            Err(e) => {
                warnings.push(PlaylistWarning {
                    playlist: number,
                    message: format!("failed to parse {}: {e}", file_path.display()),
                });
            }
        }
    }

    Ok((playlists, warnings))
}

/// Reads all `.clpi` files from the CLIPINF directory.
///
/// Returns the successfully parsed clips and any per-file warnings.
/// Individual file read or parse failures are collected as warnings
/// rather than aborting analysis.
fn read_clips(reader: &DiscReader, dir: &Path) -> (Vec<ClipInfo>, Vec<ClipWarning>) {
    let Ok(entries) = reader.read_dir(dir) else {
        return (Vec::new(), Vec::new());
    };

    let mut clips = Vec::new();
    let mut warnings = Vec::new();
    let mut clpi_names: Vec<_> = entries
        .into_iter()
        .filter(|name| name.to_ascii_lowercase().ends_with(".clpi"))
        .collect();
    clpi_names.sort();

    for name in clpi_names {
        let clip_id = name
            .strip_suffix(".clpi")
            .or_else(|| name.strip_suffix(".CLPI"))
            .unwrap_or(&name)
            .to_string();

        let file_path = dir.join(&name);

        let data = match reader.read_file(&file_path) {
            Ok(data) => data,
            Err(e) => {
                warnings.push(ClipWarning {
                    clip_id,
                    message: format!("failed to read {}: {e}", file_path.display()),
                });
                continue;
            }
        };

        match clpi::parse(&data, clip_id.clone()) {
            Ok(clip) => clips.push(clip),
            Err(e) => {
                warnings.push(ClipWarning {
                    clip_id,
                    message: format!("failed to parse {}: {e}", file_path.display()),
                });
            }
        }
    }

    (clips, warnings)
}

/// Identifies clips that contain IG streams (menu clips).
fn identify_ig_clips(clips: &[ClipInfo]) -> Vec<IgClip> {
    let mut ig_clips = Vec::new();

    for clip in clips {
        let ig_streams: Vec<IgStream> = clip
            .streams
            .iter()
            .filter(|s| s.coding_type == clpi::CODING_TYPE_IG)
            .map(|s| {
                let language = match &s.attrs {
                    clpi::StreamAttrs::Ig { language } => language.clone(),
                    _ => String::new(),
                };
                IgStream {
                    pid: s.pid,
                    language,
                }
            })
            .collect();

        if !ig_streams.is_empty() {
            ig_clips.push(IgClip {
                clip_id: clip.clip_id.clone(),
                application_type: clip.application_type,
                file_size: u64::from(clip.num_source_packets) * 192,
                ig_streams,
            });
        }
    }

    ig_clips
}

/// Finds clips not referenced by any MPLS playlist.
fn find_unreferenced_clips(clips: &[ClipInfo], playlists: &[Playlist]) -> Vec<UnreferencedClip> {
    // Collect all clip IDs referenced by any playlist
    let referenced: HashSet<&str> = playlists
        .iter()
        .flat_map(|pl| {
            pl.play_items.iter().flat_map(|item| {
                std::iter::once(item.clip_id.as_str())
                    .chain(item.angle_clip_ids.iter().map(String::as_str))
            })
        })
        .collect();

    clips
        .iter()
        .filter(|clip| !referenced.contains(clip.clip_id.as_str()))
        .map(|clip| {
            let has_ig = clip
                .streams
                .iter()
                .any(|s| s.coding_type == clpi::CODING_TYPE_IG);
            let streams = clip
                .streams
                .iter()
                .map(|s| stream_coding_name(s.coding_type).to_string())
                .collect();
            UnreferencedClip {
                clip_id: clip.clip_id.clone(),
                has_ig,
                streams,
                file_size: u64::from(clip.num_source_packets) * 192,
            }
        })
        .collect()
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
        members: Vec::new(),
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

// ── Composite grouping ────────────────────────────────────────────────

/// Groups play items into segment ranges by connection condition.
///
/// Returns index ranges into the playlist's `play_items` vec. Each range
/// covers one segment (a group of seamlessly connected play items).
fn segment_ranges(playlist: &Playlist) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;

    for (i, item) in playlist.play_items.iter().enumerate() {
        if i > 0 && item.connection_condition < 5 {
            ranges.push(start..i);
            start = i;
        }
    }
    ranges.push(start..playlist.play_items.len());
    ranges
}

/// Collects clip IDs from a range of play items.
fn clip_ids_for_range<'a>(playlist: &'a Playlist, range: &Range<usize>) -> Vec<&'a str> {
    playlist.play_items[range.clone()]
        .iter()
        .map(|item| item.clip_id.as_str())
        .collect()
}

/// Computes the member's time range as used by a composite's segment.
///
/// The range is relative to the member's own timeline — `range_start` is
/// how far into the member the composite begins, `range_end` is where it
/// ends. When the composite uses the full member, this is `0..duration`.
fn compute_member_range(
    composite: &Playlist,
    seg_range: &Range<usize>,
    member: &Playlist,
) -> (Duration, Duration) {
    let member_start_pts = member.play_items.first().map_or(0, |item| item.in_time);
    let composite_start_pts = composite.play_items[seg_range.start].in_time;

    let offset_pts = u64::from(composite_start_pts.saturating_sub(member_start_pts));
    let range_start = pts_to_duration(offset_pts);

    let seg_duration_pts: u64 = composite.play_items[seg_range.clone()]
        .iter()
        .map(|item| u64::from(item.out_time.saturating_sub(item.in_time)))
        .sum();
    let range_end = pts_to_duration(offset_pts + seg_duration_pts);

    (range_start, range_end)
}

/// Identifies composite playlists and populates their member lists.
///
/// A composite is a multi-segment playlist whose segments correspond to
/// other playlists on the disc. Matching is by clip ID list: a playlist
/// whose full clip sequence matches one segment of the composite is a
/// member of that composite at that segment's position.
fn apply_composite_grouping(playlists: &[Playlist], analyzed: &mut [AnalyzedPlaylist]) {
    // Precompute segment ranges and full clip lists for each playlist
    let all_seg_ranges: Vec<Vec<Range<usize>>> = playlists.iter().map(segment_ranges).collect();
    let all_clip_lists: Vec<Vec<&str>> = playlists
        .iter()
        .map(|pl| {
            pl.play_items
                .iter()
                .map(|item| item.clip_id.as_str())
                .collect()
        })
        .collect();

    for (ci, composite_segs) in all_seg_ranges.iter().enumerate() {
        if composite_segs.len() <= 1 {
            continue;
        }

        let mut members = Vec::new();

        for (seg_idx, seg_range) in composite_segs.iter().enumerate() {
            let seg_clips = clip_ids_for_range(&playlists[ci], seg_range);

            // Find a playlist whose full clip list matches this segment
            for (mi, member_clips) in all_clip_lists.iter().enumerate() {
                if mi == ci {
                    continue;
                }
                if member_clips.as_slice() == seg_clips.as_slice() {
                    let (range_start, range_end) =
                        compute_member_range(&playlists[ci], seg_range, &playlists[mi]);
                    #[allow(
                        clippy::cast_possible_truncation,
                        reason = "segment index fits in u32 for any real disc"
                    )]
                    members.push(CompositeMember {
                        playlist: playlists[mi].number,
                        segment_index: (seg_idx + 1) as u32,
                        range_start,
                        range_end,
                    });
                    break;
                }
            }
        }

        if !members.is_empty() {
            analyzed[ci].members = members;
        }
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

    // ── Composite grouping tests ──────────────────────────────────────

    #[test]
    fn composite_groups_single_clip_members() {
        // Play-all: 3 episodes, each a single clip with non-seamless boundaries
        let play_all_data = MplsBuilder::new()
            .play_item("00004", 27_000_000, 59_040_000)
            .play_item("00005", 27_000_000, 59_175_000)
            .play_item("00006", 27_000_000, 59_310_000)
            .build();
        // Individual episode playlists
        let ep1_data = MplsBuilder::new()
            .play_item("00004", 27_000_000, 59_040_000)
            .build();
        let ep2_data = MplsBuilder::new()
            .play_item("00005", 27_000_000, 59_175_000)
            .build();
        let ep3_data = MplsBuilder::new()
            .play_item("00006", 27_000_000, 59_310_000)
            .build();

        let playlists = vec![
            build_playlist(14, &play_all_data),
            build_playlist(4, &ep1_data),
            build_playlist(5, &ep2_data),
            build_playlist(6, &ep3_data),
        ];
        let mut analyzed: Vec<AnalyzedPlaylist> = playlists.iter().map(analyze_playlist).collect();

        apply_composite_grouping(&playlists, &mut analyzed);

        assert_eq!(
            analyzed[0].members.len(),
            3,
            "play-all should have 3 members"
        );
        assert_eq!(
            analyzed[0].members[0].playlist, 4,
            "first member is episode 1"
        );
        assert_eq!(
            analyzed[0].members[1].playlist, 5,
            "second member is episode 2"
        );
        assert_eq!(
            analyzed[0].members[2].playlist, 6,
            "third member is episode 3"
        );

        // Episodes should not be composites themselves
        assert!(
            analyzed[1].members.is_empty(),
            "episode should not be a composite"
        );
    }

    #[test]
    fn composite_groups_multi_clip_members() {
        // Play-all: 2 episodes, each with intro + content + outro (seamless)
        let play_all_data = MplsBuilder::new()
            .play_item("00100", 27_000_000, 28_350_000)
            .play_item_seamless("00003", 27_000_000, 85_500_000)
            .play_item_seamless("00101", 27_000_000, 29_700_000)
            .play_item("00100", 27_000_000, 28_350_000)
            .play_item_seamless("00004", 27_000_000, 84_600_000)
            .play_item_seamless("00101", 27_000_000, 29_700_000)
            .build();
        // Individual episode playlists with same structure
        let ep1_data = MplsBuilder::new()
            .play_item("00100", 27_000_000, 28_350_000)
            .play_item_seamless("00003", 27_000_000, 85_500_000)
            .play_item_seamless("00101", 27_000_000, 29_700_000)
            .build();
        let ep2_data = MplsBuilder::new()
            .play_item("00100", 27_000_000, 28_350_000)
            .play_item_seamless("00004", 27_000_000, 84_600_000)
            .play_item_seamless("00101", 27_000_000, 29_700_000)
            .build();

        let playlists = vec![
            build_playlist(14, &play_all_data),
            build_playlist(4, &ep1_data),
            build_playlist(5, &ep2_data),
        ];
        let mut analyzed: Vec<AnalyzedPlaylist> = playlists.iter().map(analyze_playlist).collect();

        apply_composite_grouping(&playlists, &mut analyzed);

        assert_eq!(
            analyzed[0].members.len(),
            2,
            "play-all should have 2 members"
        );
        assert_eq!(
            analyzed[0].members[0].playlist, 4,
            "first member is episode 1"
        );
        assert_eq!(
            analyzed[0].members[1].playlist, 5,
            "second member is episode 2"
        );
    }

    #[test]
    fn composite_range_full_member() {
        // Composite uses the full member — range should be 0..duration
        let play_all_data = MplsBuilder::new()
            .play_item("00004", 27_000_000, 72_000_000)
            .play_item("00005", 27_000_000, 72_000_000)
            .build();
        let ep1_data = MplsBuilder::new()
            .play_item("00004", 27_000_000, 72_000_000)
            .build();

        let playlists = vec![
            build_playlist(14, &play_all_data),
            build_playlist(4, &ep1_data),
        ];
        let mut analyzed: Vec<AnalyzedPlaylist> = playlists.iter().map(analyze_playlist).collect();

        apply_composite_grouping(&playlists, &mut analyzed);

        let member = &analyzed[0].members[0];
        assert_eq!(member.range_start, Duration::ZERO, "range starts at 0");
        assert_eq!(
            member.range_end, analyzed[1].duration,
            "range end equals member duration"
        );
    }

    #[test]
    fn no_composite_for_single_segment() {
        // A single-segment playlist should not become a composite
        let data = MplsBuilder::new()
            .play_item("00004", 27_000_000, 59_040_000)
            .build();

        let playlists = vec![build_playlist(1, &data)];
        let mut analyzed: Vec<AnalyzedPlaylist> = playlists.iter().map(analyze_playlist).collect();

        apply_composite_grouping(&playlists, &mut analyzed);

        assert!(
            analyzed[0].members.is_empty(),
            "single-segment playlist has no members"
        );
    }

    #[test]
    fn composite_display_suppresses_members() {
        // Play-all with 2 episodes + an ungrouped extra
        let play_all_data = MplsBuilder::new()
            .play_item("00004", 27_000_000, 59_040_000)
            .play_item("00005", 27_000_000, 59_175_000)
            .build();
        let ep1_data = MplsBuilder::new()
            .play_item("00004", 27_000_000, 59_040_000)
            .build();
        let ep2_data = MplsBuilder::new()
            .play_item("00005", 27_000_000, 59_175_000)
            .build();
        let extra_data = MplsBuilder::new()
            .play_item("00010", 27_000_000, 30_000_000)
            .build();

        let mut playlists = vec![
            build_playlist(14, &play_all_data),
            build_playlist(4, &ep1_data),
            build_playlist(5, &ep2_data),
            build_playlist(20, &extra_data),
        ];
        playlists.sort_by_key(|pl| pl.number);
        let mut analyzed: Vec<AnalyzedPlaylist> = playlists.iter().map(analyze_playlist).collect();

        apply_composite_grouping(&playlists, &mut analyzed);

        let analysis = BdmvAnalysis {
            playlists: analyzed,
            main_title: Some(14),
            ig_clips: Vec::new(),
            unreferenced_clips: Vec::new(),
            warnings: Vec::new(),
            clip_warnings: Vec::new(),
        };

        let output = format!("{analysis}");
        // Members should not appear as top-level entries
        assert!(
            !output.contains("\nMPLS 00004"),
            "member 00004 should be suppressed from top level"
        );
        assert!(
            !output.contains("\nMPLS 00005"),
            "member 00005 should be suppressed from top level"
        );
        // Composite and ungrouped extra should appear
        assert!(
            output.contains("MPLS 00014"),
            "composite should appear at top level"
        );
        assert!(
            output.contains("MPLS 00020"),
            "ungrouped extra should appear at top level"
        );
        // Members should appear indented under the composite
        assert!(
            output.contains("  01: MPLS 00004"),
            "member 00004 should appear indented under composite"
        );
        assert!(
            output.contains("  02: MPLS 00005"),
            "member 00005 should appear indented under composite"
        );
    }

    // ── Warning display test ──────────────────────────────────────────

    #[test]
    fn warnings_appear_in_display() {
        let ep_data = MplsBuilder::new()
            .play_item("00004", 27_000_000, 59_040_000)
            .build();
        let analyzed = vec![analyze_playlist(&build_playlist(4, &ep_data))];

        let analysis = BdmvAnalysis {
            playlists: analyzed,
            main_title: Some(4),
            ig_clips: Vec::new(),
            unreferenced_clips: Vec::new(),
            warnings: vec![PlaylistWarning {
                playlist: 99,
                message: "failed to parse PLAYLIST/00099.mpls: bad magic".into(),
            }],
            clip_warnings: Vec::new(),
        };

        let output = format!("{analysis}");
        assert!(
            output.contains("warning: MPLS 00099"),
            "warning should appear in output: {output}"
        );
    }

    // ── CLPI integration tests ───────────────────────────────────────

    use crate::disc::bdmv::clpi::tests::ClpiBuilder;

    #[test]
    fn ig_clips_identified() {
        let clip_content = ClpiBuilder::new()
            .application_type(1)
            .num_source_packets(1_000_000)
            .video(0x1011, 0x1b, 6, 1)
            .audio(0x1100, 0x81, 3, 1, *b"eng")
            .build();
        let clip_ig = ClpiBuilder::new()
            .application_type(5)
            .num_source_packets(500)
            .ig(0x1400, *b"eng")
            .build();

        let clips = vec![
            clpi::parse(&clip_content, "00004".into()).expect("should parse content clip"),
            clpi::parse(&clip_ig, "00291".into()).expect("should parse IG clip"),
        ];

        let ig_clips = identify_ig_clips(&clips);
        assert_eq!(ig_clips.len(), 1, "should find one IG clip");
        assert_eq!(ig_clips[0].clip_id, "00291", "IG clip id");
        assert_eq!(ig_clips[0].application_type, 5, "IG application type");
        assert_eq!(ig_clips[0].file_size, 500 * 192, "IG file size");
        assert_eq!(ig_clips[0].ig_streams.len(), 1, "IG stream count");
        assert_eq!(ig_clips[0].ig_streams[0].pid, 0x1400, "IG stream PID");
        assert_eq!(
            ig_clips[0].ig_streams[0].language, "eng",
            "IG stream language"
        );
    }

    #[test]
    fn no_ig_clips_when_absent() {
        let clip = ClpiBuilder::new()
            .application_type(1)
            .num_source_packets(1_000_000)
            .video(0x1011, 0x1b, 6, 1)
            .audio(0x1100, 0x81, 3, 1, *b"eng")
            .build();

        let clips = vec![clpi::parse(&clip, "00004".into()).expect("should parse")];

        let ig_clips = identify_ig_clips(&clips);
        assert!(ig_clips.is_empty(), "should find no IG clips");
    }

    #[test]
    fn unreferenced_clips_detected() {
        let clip_content = ClpiBuilder::new()
            .num_source_packets(1_000_000)
            .video(0x1011, 0x1b, 6, 1)
            .build();
        let clip_ig = ClpiBuilder::new()
            .application_type(5)
            .num_source_packets(500)
            .ig(0x1400, *b"eng")
            .build();

        let clips = vec![
            clpi::parse(&clip_content, "00004".into()).expect("should parse content"),
            clpi::parse(&clip_ig, "00291".into()).expect("should parse IG"),
        ];

        // Only clip 00004 is referenced by a playlist
        let mpls_data = MplsBuilder::new()
            .play_item("00004", 27_000_000, 59_040_000)
            .build();
        let playlists = vec![build_playlist(100, &mpls_data)];

        let unreferenced = find_unreferenced_clips(&clips, &playlists);
        assert_eq!(unreferenced.len(), 1, "should find one unreferenced clip");
        assert_eq!(unreferenced[0].clip_id, "00291", "unreferenced clip id");
        assert!(unreferenced[0].has_ig, "unreferenced clip has IG");
        assert_eq!(
            unreferenced[0].file_size,
            500 * 192,
            "unreferenced file size"
        );
    }

    #[test]
    fn all_clips_referenced() {
        let clip = ClpiBuilder::new()
            .num_source_packets(1_000_000)
            .video(0x1011, 0x1b, 6, 1)
            .build();

        let clips = vec![clpi::parse(&clip, "00004".into()).expect("should parse")];

        let mpls_data = MplsBuilder::new()
            .play_item("00004", 27_000_000, 59_040_000)
            .build();
        let playlists = vec![build_playlist(100, &mpls_data)];

        let unreferenced = find_unreferenced_clips(&clips, &playlists);
        assert!(unreferenced.is_empty(), "all clips should be referenced");
    }
}
