// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! DVD disc analysis — reads IFO files, resolves titles through PGC tables,
//! and identifies the main title.
//!
//! The public surface is [`analyze`], which returns a [`DvdAnalysis`].

mod ifo;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use thiserror::Error;

use super::reader::{DiscReader, ReaderError};

// ── Errors ──────────────────────────────────────────────────────────────

/// Errors that can occur during DVD analysis.
#[derive(Debug, Error)]
pub enum DvdError {
    /// `VIDEO_TS.IFO` could not be read.
    #[error("failed to read VIDEO_TS.IFO: {0}")]
    ReadVmg(#[source] ReaderError),

    /// `VIDEO_TS.IFO` could not be parsed.
    #[error("failed to parse VIDEO_TS.IFO: {0}")]
    ParseVmg(
        /// Parser error (opaque — internal IFO details are not public).
        #[source]
        Box<dyn std::error::Error + Send + Sync>,
    ),

    /// No titles found on the disc.
    #[error("no titles found on disc")]
    NoTitles,
}

/// A per-VTS error encountered during analysis (non-fatal).
#[derive(Debug, Clone, Serialize)]
pub struct VtsWarning {
    /// VTS number that failed.
    pub vts: u16,
    /// Description of the error.
    pub message: String,
}

// ── Analysis types ──────────────────────────────────────────────────────

/// Complete analysis of a DVD disc structure.
#[derive(Debug, Clone, Serialize)]
pub struct DvdAnalysis {
    /// Title set stream information.
    pub title_sets: Vec<AnalyzedTitleSet>,
    /// Resolved titles from the VMG title search pointer table.
    pub titles: Vec<AnalyzedTitle>,
    /// PGCs in the title domain not referenced by any title.
    pub unreferenced_pgcs: Vec<UnreferencedPgc>,
    /// Global title number of the identified main title (1-based).
    pub main_title: Option<u16>,
    /// Warnings from VTS files that could not be read or parsed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<VtsWarning>,
}

/// Stream information for a title set (VTS).
#[derive(Debug, Clone, Serialize)]
pub struct AnalyzedTitleSet {
    /// VTS number (1-based).
    pub number: u16,
    /// Video description (e.g. "MPEG-2 NTSC 16:9 720×480").
    pub video: String,
    /// Audio stream descriptions.
    pub audio: Vec<String>,
    /// Subtitle stream descriptions.
    pub subtitles: Vec<String>,
}

/// A resolved title with computed duration and stream summary.
#[derive(Debug, Clone, Serialize)]
pub struct AnalyzedTitle {
    /// Global title number (1-based, from VMG title search pointer table).
    pub number: u16,
    /// VTS number this title belongs to.
    pub title_set: u16,
    /// Total duration computed from cell playback times.
    #[serde(serialize_with = "serialize_duration")]
    pub duration: Duration,
    /// Number of chapters.
    pub chapters: u16,
    /// Number of angles.
    pub angles: u8,
    /// Streams available for this title (VTS streams filtered by PGC control bits).
    pub streams: StreamSummary,
    /// Titles whose content is contained within this title (composite grouping).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<u16>,
}

/// A PGC in the title domain not referenced by any title's PTT entries.
#[derive(Debug, Clone, Serialize)]
pub struct UnreferencedPgc {
    /// VTS number.
    pub title_set: u16,
    /// PGC index within the VTS (1-based).
    pub pgc_index: u16,
    /// Entry type identifier.
    pub entry_id: u8,
    /// Duration of the PGC.
    #[serde(serialize_with = "serialize_duration")]
    pub duration: Duration,
    /// Number of programs.
    pub programs: u8,
    /// Number of cells.
    pub cells: u8,
    /// Streams available (VTS streams filtered by PGC control bits).
    pub streams: StreamSummary,
}

/// Summary of streams available in a title or PGC.
#[derive(Debug, Clone, Serialize)]
pub struct StreamSummary {
    /// Video stream descriptions.
    pub video: Vec<String>,
    /// Audio stream descriptions.
    pub audio: Vec<String>,
    /// Subtitle stream descriptions.
    pub subtitles: Vec<String>,
}

fn serialize_duration<S: serde::Serializer>(
    duration: &Duration,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_f64(duration.as_secs_f64())
}

// ── Display ─────────────────────────────────────────────────────────────

impl fmt::Display for DvdAnalysis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Title sets
        for ts in &self.title_sets {
            writeln!(f, "VTS {:02}  {}", ts.number, ts.video)?;
            for a in &ts.audio {
                writeln!(f, "        audio: {a}")?;
            }
            for s in &ts.subtitles {
                writeln!(f, "        sub:   {s}")?;
            }
        }

        if !self.title_sets.is_empty() && !self.titles.is_empty() {
            writeln!(f)?;
        }

        // Titles
        for t in &self.titles {
            let is_main = self.main_title == Some(t.number);
            let marker = if is_main { " *" } else { "" };
            let angle_str = if t.angles > 1 {
                format!("  {}ang", t.angles)
            } else {
                String::new()
            };
            writeln!(
                f,
                "Title {:02}  VTS {:02}  {:>10}  {:>3} ch{}  {}{}",
                t.number,
                t.title_set,
                format_duration(t.duration),
                t.chapters,
                angle_str,
                format_streams(&t.streams),
                marker,
            )?;
            if !t.members.is_empty() {
                let member_strs: Vec<String> =
                    t.members.iter().map(|m| format!("{m:02}")).collect();
                writeln!(f, "         contains: {}", member_strs.join(", "))?;
            }
        }

        // Unreferenced PGCs
        if !self.unreferenced_pgcs.is_empty() {
            writeln!(f)?;
            for u in &self.unreferenced_pgcs {
                writeln!(
                    f,
                    "Unreferenced  VTS {:02}  PGC {:02}  {:>10}  {:>3} prog  {} cell  {}",
                    u.title_set,
                    u.pgc_index,
                    format_duration(u.duration),
                    u.programs,
                    u.cells,
                    format_streams(&u.streams),
                )?;
            }
        }

        if let Some(main) = self.main_title {
            writeln!(f, "\n* main title: {main:02}")?;
        }

        // Warnings
        for w in &self.warnings {
            writeln!(f, "warning: VTS {:02}: {}", w.vts, w.message)?;
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
        let sub_summary: Vec<&str> = s.subtitles.iter().map(String::as_str).collect();
        parts.push(format!("subs: {}", sub_summary.join(", ")));
    }

    parts.join(" | ")
}

// ── Analysis entry point ────────────────────────────────────────────────

/// Analyzes a DVD disc structure.
///
/// Takes a [`DiscReader`] for file access — the reader may be backed by a
/// mounted directory or an ISO image. Looks for `VIDEO_TS/` within the reader.
///
/// # Errors
///
/// Returns [`DvdError`] if `VIDEO_TS.IFO` cannot be read or parsed, or
/// no titles are found. Individual VTS failures are reported as warnings
/// in the result rather than aborting the analysis.
pub fn analyze(reader: &DiscReader) -> Result<DvdAnalysis, DvdError> {
    let video_ts = Path::new("VIDEO_TS");

    // Read VIDEO_TS.IFO (with BUP fallback)
    let vmg_data = read_ifo_with_bup(reader, &video_ts.join("VIDEO_TS.IFO"))?;
    let vmg = ifo::parse_vmg(&vmg_data).map_err(|e| DvdError::ParseVmg(Box::new(e)))?;

    // Parse each VTS, collecting results and warnings
    let mut vts_map: HashMap<u8, ifo::Vts> = HashMap::new();
    let mut warnings: Vec<VtsWarning> = Vec::new();

    for vts_nr in 1..=vmg.nr_of_title_sets {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "VTS numbers are 1–99 per DVD spec, always fit in u8"
        )]
        let vts_nr_u8 = vts_nr as u8;
        let filename = format!("VTS_{vts_nr:02}_0.IFO");
        let vts_path = video_ts.join(&filename);

        match read_ifo_with_bup(reader, &vts_path) {
            Ok(data) => match ifo::parse_vts(&data) {
                Ok(vts) => {
                    vts_map.insert(vts_nr_u8, vts);
                }
                Err(e) => {
                    warnings.push(VtsWarning {
                        vts: vts_nr,
                        message: format!("failed to parse {filename}: {e}"),
                    });
                }
            },
            Err(e) => {
                warnings.push(VtsWarning {
                    vts: vts_nr,
                    message: format!("failed to read {filename}: {e}"),
                });
            }
        }
    }

    // Build title set summaries
    let mut title_sets: Vec<AnalyzedTitleSet> = Vec::new();
    let mut sorted_vts: Vec<u8> = vts_map.keys().copied().collect();
    sorted_vts.sort_unstable();

    for &vts_nr in &sorted_vts {
        if let Some(vts) = vts_map.get(&vts_nr) {
            title_sets.push(analyze_title_set(u16::from(vts_nr), vts));
        }
    }

    // Resolve titles
    let titles = resolve_titles(&vmg, &vts_map);

    if titles.is_empty() && warnings.is_empty() {
        return Err(DvdError::NoTitles);
    }

    // Find unreferenced PGCs
    let unreferenced_pgcs = find_unreferenced_pgcs(&vmg, &vts_map);

    // Identify main title (longest duration)
    let main_title = identify_main_title(&titles);

    // Composite grouping
    let titles = apply_composite_grouping(titles, &vmg, &vts_map);

    Ok(DvdAnalysis {
        title_sets,
        titles,
        unreferenced_pgcs,
        main_title,
        warnings,
    })
}

/// Reads an IFO file with BUP fallback.
fn read_ifo_with_bup(reader: &DiscReader, ifo_path: &Path) -> Result<Vec<u8>, DvdError> {
    match reader.read_file(ifo_path) {
        Ok(data) => Ok(data),
        Err(ifo_err) => {
            // Try BUP fallback
            let bup_path = ifo_path.with_extension("BUP");
            reader
                .read_file(&bup_path)
                .map_err(|_| DvdError::ReadVmg(ifo_err))
        }
    }
}

/// Builds an [`AnalyzedTitleSet`] from VTS stream attributes.
fn analyze_title_set(number: u16, vts: &ifo::Vts) -> AnalyzedTitleSet {
    let video = vts.video_attr.description();

    let audio: Vec<String> = vts
        .audio_attrs
        .iter()
        .map(ifo::AudioAttr::description)
        .collect();

    let subtitles: Vec<String> = vts
        .subp_attrs
        .iter()
        .filter(|s| s.lang_code[0] != 0 || s.lang_code[1] != 0)
        .map(ifo::SubpictureAttr::description)
        .collect();

    AnalyzedTitleSet {
        number,
        video,
        audio,
        subtitles,
    }
}

/// Resolves all VMG titles through VTS PTT tables to compute durations.
fn resolve_titles(vmg: &ifo::Vmg, vts_map: &HashMap<u8, ifo::Vts>) -> Vec<AnalyzedTitle> {
    let mut titles = Vec::new();

    for (i, tp) in vmg.titles.iter().enumerate() {
        let Some(vts) = vts_map.get(&tp.title_set_nr) else {
            continue;
        };

        // vts_ttn is 1-based — index into the VTS PTT table
        let ttn_idx = usize::from(tp.vts_ttn).saturating_sub(1);
        let ptts = vts.ptt_table.get(ttn_idx);

        let (duration, streams) = ptts.map_or_else(
            || (Duration::ZERO, empty_streams()),
            |ptts| {
                let dur = compute_title_duration(ptts, &vts.pgc_table);
                let strm = compute_title_streams(ptts, vts);
                (dur, strm)
            },
        );

        #[allow(
            clippy::cast_possible_truncation,
            reason = "title indices are small per DVD spec"
        )]
        titles.push(AnalyzedTitle {
            number: (i + 1) as u16,
            title_set: u16::from(tp.title_set_nr),
            duration,
            chapters: tp.nr_of_ptts,
            angles: tp.nr_of_angles,
            streams,
            members: Vec::new(),
        });
    }

    titles
}

/// Computes the total duration of a title from its PTT entries.
///
/// Groups chapters by PGC, then sums cell durations for the programs
/// this title uses within each PGC. Angle cells are counted once
/// (only the first cell in each angle block contributes).
fn compute_title_duration(ptts: &[ifo::PttEntry], pgc_table: &[ifo::PgcEntry]) -> Duration {
    // Group chapters by PGC: pgcn → set of program numbers
    let mut pgc_programs: HashMap<u16, BTreeSet<u16>> = HashMap::new();
    for ptt in ptts {
        pgc_programs.entry(ptt.pgcn).or_default().insert(ptt.pgn);
    }

    let mut total = Duration::ZERO;

    for (&pgcn, programs) in &pgc_programs {
        let pgc_idx = usize::from(pgcn).saturating_sub(1);
        let Some(pgc_entry) = pgc_table.get(pgc_idx) else {
            continue;
        };
        let pgc = &pgc_entry.pgc;

        // Find cells belonging to the programs this title uses
        for &pgn in programs {
            let cells = cells_for_program(pgc, pgn);
            for &cell_idx in &cells {
                if let Some(cell) = pgc.cells.get(cell_idx) {
                    // Skip duplicate angle cells — only count the first in each block
                    if cell.block_type == 1 && cell.block_mode > 1 {
                        continue;
                    }
                    total += cell.playback_time.to_duration();
                }
            }
        }
    }

    total
}

/// Returns the cell indices (0-based) belonging to a given program number (1-based).
fn cells_for_program(pgc: &ifo::Pgc, pgn: u16) -> Vec<usize> {
    let pgn_idx = usize::from(pgn).saturating_sub(1);

    // Start cell (1-based) from program map
    let start_cell = pgc.program_map.get(pgn_idx).copied().unwrap_or(1);

    // End cell: next program's start - 1, or nr_of_cells
    let end_cell = pgc
        .program_map
        .get(pgn_idx + 1)
        .copied()
        .unwrap_or(pgc.nr_of_cells + 1);

    // Convert to 0-based indices
    let start = usize::from(start_cell).saturating_sub(1);
    let end = usize::from(end_cell).saturating_sub(1);

    (start..end).collect()
}

/// Computes the stream summary for a title by filtering VTS streams
/// through PGC audio/subpicture control bits.
fn compute_title_streams(ptts: &[ifo::PttEntry], vts: &ifo::Vts) -> StreamSummary {
    // Use the first chapter's PGC for stream filtering
    let pgc = ptts.first().and_then(|ptt| {
        let idx = usize::from(ptt.pgcn).saturating_sub(1);
        vts.pgc_table.get(idx)
    });

    let video = vec![vts.video_attr.description()];

    let audio = pgc.map_or_else(
        || {
            vts.audio_attrs
                .iter()
                .map(ifo::AudioAttr::description)
                .collect()
        },
        |pgc_entry| filter_audio_streams(&vts.audio_attrs, &pgc_entry.pgc.audio_control),
    );

    let subtitles = pgc.map_or_else(
        || {
            vts.subp_attrs
                .iter()
                .filter(|s| s.lang_code[0] != 0 || s.lang_code[1] != 0)
                .map(ifo::SubpictureAttr::description)
                .collect()
        },
        |pgc_entry| filter_subp_streams(&vts.subp_attrs, &pgc_entry.pgc.subp_control),
    );

    StreamSummary {
        video,
        audio,
        subtitles,
    }
}

/// Filters audio streams by PGC audio control bits.
fn filter_audio_streams(attrs: &[ifo::AudioAttr], control: &[u16; 8]) -> Vec<String> {
    attrs
        .iter()
        .enumerate()
        .filter(|(i, _)| *i < 8 && control[*i] & 0x8000 != 0)
        .map(|(_, a)| a.description())
        .collect()
}

/// Filters subpicture streams by PGC subpicture control bits.
fn filter_subp_streams(attrs: &[ifo::SubpictureAttr], control: &[u32; 32]) -> Vec<String> {
    attrs
        .iter()
        .enumerate()
        .filter(|(i, a)| {
            *i < 32
                && control[*i] & 0x8000_0000 != 0
                && (a.lang_code[0] != 0 || a.lang_code[1] != 0)
        })
        .map(|(_, s)| s.description())
        .collect()
}

/// Finds PGCs in the title domain not referenced by any title's PTT entries.
fn find_unreferenced_pgcs(vmg: &ifo::Vmg, vts_map: &HashMap<u8, ifo::Vts>) -> Vec<UnreferencedPgc> {
    let mut result = Vec::new();

    for (&vts_nr, vts) in vts_map {
        // Collect all PGC numbers referenced by any title's PTT entries
        let mut referenced: HashSet<u16> = HashSet::new();
        for title in &vmg.titles {
            if title.title_set_nr != vts_nr {
                continue;
            }
            let ttn_idx = usize::from(title.vts_ttn).saturating_sub(1);
            if let Some(ptts) = vts.ptt_table.get(ttn_idx) {
                for ptt in ptts {
                    referenced.insert(ptt.pgcn);
                }
            }
        }

        // Check each PGC in the title domain
        for (i, pgc_entry) in vts.pgc_table.iter().enumerate() {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "PGC index is small per DVD spec"
            )]
            let pgc_nr = (i + 1) as u16;

            if referenced.contains(&pgc_nr) {
                continue;
            }

            // Exclude menu-domain PGCs (entry_id 0x83–0x87)
            if pgc_entry.entry_id >= 0x83 && pgc_entry.entry_id <= 0x87 {
                continue;
            }

            let pgc = &pgc_entry.pgc;
            let duration = compute_pgc_duration(pgc);
            let streams = compute_pgc_streams(pgc, vts);

            result.push(UnreferencedPgc {
                title_set: u16::from(vts_nr),
                pgc_index: pgc_nr,
                entry_id: pgc_entry.entry_id,
                duration,
                programs: pgc.nr_of_programs,
                cells: pgc.nr_of_cells,
                streams,
            });
        }
    }

    // Sort by VTS number, then PGC index
    result.sort_by_key(|u| (u.title_set, u.pgc_index));

    result
}

/// Computes a PGC's total duration from its cells.
fn compute_pgc_duration(pgc: &ifo::Pgc) -> Duration {
    let mut total = Duration::ZERO;
    for cell in &pgc.cells {
        // Skip duplicate angle cells
        if cell.block_type == 1 && cell.block_mode > 1 {
            continue;
        }
        total += cell.playback_time.to_duration();
    }
    total
}

/// Computes stream summary for a PGC, filtered by its control bits.
fn compute_pgc_streams(pgc: &ifo::Pgc, vts: &ifo::Vts) -> StreamSummary {
    StreamSummary {
        video: vec![vts.video_attr.description()],
        audio: filter_audio_streams(&vts.audio_attrs, &pgc.audio_control),
        subtitles: filter_subp_streams(&vts.subp_attrs, &pgc.subp_control),
    }
}

/// Identifies the main title — the one with the longest duration.
fn identify_main_title(titles: &[AnalyzedTitle]) -> Option<u16> {
    titles.iter().max_by_key(|t| t.duration).map(|t| t.number)
}

/// Applies composite grouping: detects titles whose cells are subsets
/// of another title's cells within the same VTS.
fn apply_composite_grouping(
    mut titles: Vec<AnalyzedTitle>,
    vmg: &ifo::Vmg,
    vts_map: &HashMap<u8, ifo::Vts>,
) -> Vec<AnalyzedTitle> {
    // Build sector range sets per title: title_number → Vec<(first, last)>
    let mut title_sectors: HashMap<u16, Vec<(u32, u32)>> = HashMap::new();
    let mut title_vts: HashMap<u16, u8> = HashMap::new();

    for (i, tp) in vmg.titles.iter().enumerate() {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "title indices are small per DVD spec"
        )]
        let title_nr = (i + 1) as u16;
        title_vts.insert(title_nr, tp.title_set_nr);

        let Some(vts) = vts_map.get(&tp.title_set_nr) else {
            continue;
        };
        let ttn_idx = usize::from(tp.vts_ttn).saturating_sub(1);
        let Some(ptts) = vts.ptt_table.get(ttn_idx) else {
            continue;
        };

        let sectors = collect_title_sectors(ptts, &vts.pgc_table);
        title_sectors.insert(title_nr, sectors);
    }

    // Identify navigation stubs (zero playback + zero still on all cells)
    let mut stubs: HashSet<u16> = HashSet::new();
    for (i, tp) in vmg.titles.iter().enumerate() {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "title indices are small per DVD spec"
        )]
        let title_nr = (i + 1) as u16;
        if is_navigation_stub(tp, vts_map) {
            stubs.insert(title_nr);
        }
    }

    // For each pair of titles in the same VTS, check subset relationship
    let title_nrs: Vec<u16> = titles.iter().map(|t| t.number).collect();

    // members[composite_nr] = vec of member title numbers, in sector order
    let mut members_map: HashMap<u16, Vec<(u16, u32)>> = HashMap::new();

    for &a in &title_nrs {
        for &b in &title_nrs {
            if a == b {
                continue;
            }
            // Navigation stubs are not real content — skip as members
            if stubs.contains(&b) {
                continue;
            }
            // Must be in the same VTS
            if title_vts.get(&a) != title_vts.get(&b) {
                continue;
            }
            let Some(a_sectors) = title_sectors.get(&a) else {
                continue;
            };
            let Some(b_sectors) = title_sectors.get(&b) else {
                continue;
            };
            // Check if b's sectors are a strict subset of a's sectors
            if b_sectors.len() >= a_sectors.len() {
                continue;
            }
            if is_sector_subset(b_sectors, a_sectors) {
                // b is a member of composite a
                // Find the position of b's first sector within a
                let pos = b_sectors
                    .first()
                    .and_then(|&(first, _)| a_sectors.iter().position(|&(af, _)| af == first))
                    .unwrap_or(0);
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "sector positions fit in u32"
                )]
                members_map.entry(a).or_default().push((b, pos as u32));
            }
        }
    }

    // Sort members by position within the composite
    for members in members_map.values_mut() {
        members.sort_by_key(|&(_, pos)| pos);
    }

    // Apply to titles
    for title in &mut titles {
        if let Some(members) = members_map.get(&title.number) {
            title.members = members.iter().map(|&(nr, _)| nr).collect();
        }
    }

    titles
}

/// Collects the sector ranges for a title's cells.
fn collect_title_sectors(ptts: &[ifo::PttEntry], pgc_table: &[ifo::PgcEntry]) -> Vec<(u32, u32)> {
    let mut sectors = Vec::new();
    let mut pgc_programs: HashMap<u16, BTreeSet<u16>> = HashMap::new();
    for ptt in ptts {
        pgc_programs.entry(ptt.pgcn).or_default().insert(ptt.pgn);
    }

    for (&pgcn, programs) in &pgc_programs {
        let pgc_idx = usize::from(pgcn).saturating_sub(1);
        let Some(pgc_entry) = pgc_table.get(pgc_idx) else {
            continue;
        };
        let pgc = &pgc_entry.pgc;

        for &pgn in programs {
            let cells = cells_for_program(pgc, pgn);
            for &cell_idx in &cells {
                if let Some(cell) = pgc.cells.get(cell_idx) {
                    if cell.block_type == 1 && cell.block_mode > 1 {
                        continue;
                    }
                    sectors.push((cell.first_sector, cell.last_sector));
                }
            }
        }
    }

    sectors.sort_by_key(|&(first, _)| first);
    sectors
}

/// Checks whether `sub` sectors are all contained in `sup` sectors.
fn is_sector_subset(sub: &[(u32, u32)], sup: &[(u32, u32)]) -> bool {
    if sub.is_empty() {
        return false;
    }
    for &(sf, sl) in sub {
        if !sup.iter().any(|&(pf, pl)| pf <= sf && sl <= pl) {
            return false;
        }
    }
    true
}

/// Maximum duration for a title to be considered a navigation stub.
/// Real content starts at ~7 seconds (shortest episode in the collection).
/// Stubs have at most a few frames of placeholder video (~0.4s at PAL).
const NAVIGATION_STUB_THRESHOLD: Duration = Duration::from_secs(1);

/// Returns true if a title is a navigation stub — total cell playback
/// time is under [`NAVIGATION_STUB_THRESHOLD`] and no cell has a still
/// time, meaning no meaningful content is ever rendered.
fn is_navigation_stub(tp: &ifo::TitlePointer, vts_map: &HashMap<u8, ifo::Vts>) -> bool {
    let Some(vts) = vts_map.get(&tp.title_set_nr) else {
        return false;
    };
    let ttn_idx = usize::from(tp.vts_ttn).saturating_sub(1);
    let Some(ptts) = vts.ptt_table.get(ttn_idx) else {
        return false;
    };

    let mut total = Duration::ZERO;
    let mut has_cells = false;

    for ptt in ptts {
        let pgc_idx = usize::from(ptt.pgcn).saturating_sub(1);
        let Some(pgc_entry) = vts.pgc_table.get(pgc_idx) else {
            continue;
        };
        let pgc = &pgc_entry.pgc;
        let cells = cells_for_program(pgc, ptt.pgn);
        for &cell_idx in &cells {
            if let Some(cell) = pgc.cells.get(cell_idx) {
                has_cells = true;
                if cell.still_time != 0 {
                    return false;
                }
                total += cell.playback_time.to_duration();
            }
        }
    }

    has_cells && total < NAVIGATION_STUB_THRESHOLD
}

/// Returns an empty [`StreamSummary`].
const fn empty_streams() -> StreamSummary {
    StreamSummary {
        video: Vec::new(),
        audio: Vec::new(),
        subtitles: Vec::new(),
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod tests {
    use super::ifo::tests::{PgcBuilder, VtsBuilder};
    use super::*;
    use ifo::FrameRate;

    /// Builds a simple VMG + VTS map for analysis testing.
    fn build_test_disc(
        vmg_titles: &[(u8, u8, u16, u8)], // (vts_nr, vts_ttn, chapters, angles)
        vts_data: &[(u8, Vec<u8>)],       // (vts_nr, binary)
    ) -> (ifo::Vmg, HashMap<u8, ifo::Vts>) {
        let titles: Vec<ifo::TitlePointer> = vmg_titles
            .iter()
            .map(
                |&(title_set_nr, vts_ttn, nr_of_ptts, nr_of_angles)| ifo::TitlePointer {
                    title_set_nr,
                    vts_ttn,
                    nr_of_ptts,
                    nr_of_angles,
                },
            )
            .collect();

        let nr_of_title_sets = vmg_titles.iter().map(|t| t.0).max().unwrap_or(0);

        let vmg = ifo::Vmg {
            nr_of_title_sets: u16::from(nr_of_title_sets),
            titles,
        };

        let mut vts_map = HashMap::new();
        for (vts_nr, data) in vts_data {
            let vts = ifo::parse_vts(data).expect("test VTS should parse");
            vts_map.insert(*vts_nr, vts);
        }

        (vmg, vts_map)
    }

    #[test]
    fn simple_movie_single_title() {
        // a DVD title pattern: 1 VTS, 1 PGC, 7 chapters
        let vts_data = VtsBuilder::new()
            .video(1, 0, 3, 0)
            .audio(0, 1, 0, *b"en")
            .subpicture(*b"en", 1)
            .pgc(
                PgcBuilder::new()
                    .programs(7)
                    .time(1, 36, 50, 8, FrameRate::Ntsc)
                    .cells_simple(8, 500)
                    .audio_available(&[0])
                    .subp_available(&[0]),
            )
            .title_ptts(&[&[(1, 1), (1, 2), (1, 3), (1, 4), (1, 5), (1, 6), (1, 7)]])
            .build();

        let (vmg, vts_map) = build_test_disc(&[(1, 1, 7, 1)], &[(1, vts_data)]);

        let titles = resolve_titles(&vmg, &vts_map);
        assert_eq!(titles.len(), 1, "should have 1 title");
        assert_eq!(titles[0].chapters, 7, "should have 7 chapters");
        assert_eq!(titles[0].title_set, 1, "should be VTS 1");
        assert!(
            titles[0].duration > Duration::ZERO,
            "should have nonzero duration"
        );

        let main = identify_main_title(&titles);
        assert_eq!(main, Some(1), "main title should be title 1");
    }

    #[test]
    fn multi_vts_with_extras() {
        // a DVD title pattern: main feature + short extras in separate VTSs
        let main_chapters: Vec<(u16, u16)> = (1..=32).map(|p| (1u16, p)).collect();
        let main_vts = VtsBuilder::new()
            .video(1, 0, 3, 0)
            .audio(0, 1, 0, *b"en")
            .audio(0, 5, 0, *b"en")
            .pgc(
                PgcBuilder::new()
                    .programs(32)
                    .time(1, 33, 36, 7, FrameRate::Ntsc)
                    .cells_simple(33, 1000)
                    .audio_available(&[0, 1]),
            )
            .title_ptts(&[&main_chapters[..]])
            .build();

        let extra_vts = VtsBuilder::new()
            .video(1, 0, 3, 0)
            .audio(0, 1, 0, *b"en")
            .pgc(
                PgcBuilder::new()
                    .programs(1)
                    .time(0, 0, 37, 0, FrameRate::Ntsc)
                    .cells_simple(1, 100)
                    .audio_available(&[0]),
            )
            .title_ptts(&[&[(1, 1)]])
            .build();

        let (vmg, vts_map) = build_test_disc(
            &[(1, 1, 32, 1), (2, 1, 1, 1)],
            &[(1, main_vts), (2, extra_vts)],
        );

        let titles = resolve_titles(&vmg, &vts_map);
        assert_eq!(titles.len(), 2, "should have 2 titles");

        let main = identify_main_title(&titles);
        assert_eq!(main, Some(1), "main title should be the feature");
        assert!(
            titles[0].duration > titles[1].duration,
            "main feature should be longer than extra"
        );
    }

    #[test]
    fn tv_series_with_play_all() {
        // a TV-series DVD pattern: play-all PGC with N programs + individual PGCs
        let mut pgcs = vec![
            // PGC 1: play-all (3 episodes × 2 cells each = 6 cells, 3 programs)
            PgcBuilder::new()
                .entry_id(0x81)
                .programs(3)
                .cell(0, 7, 0, 0, FrameRate::Ntsc, 0, 499, 0, 0)
                .cell(0, 0, 3, 0, FrameRate::Ntsc, 500, 509, 0, 0)
                .cell(0, 7, 0, 0, FrameRate::Ntsc, 510, 1009, 0, 0)
                .cell(0, 0, 3, 0, FrameRate::Ntsc, 1010, 1019, 0, 0)
                .cell(0, 7, 0, 0, FrameRate::Ntsc, 1020, 1519, 0, 0)
                .cell(0, 0, 3, 0, FrameRate::Ntsc, 1520, 1529, 0, 0)
                .time(0, 21, 9, 0, FrameRate::Ntsc),
        ];

        // PGCs 2-4: individual episodes (2 cells each)
        for ep in 0..3u8 {
            let start = u32::from(ep) * 510;
            pgcs.push(
                PgcBuilder::new()
                    .entry_id(0x82 + ep)
                    .programs(1)
                    .cell(0, 7, 0, 0, FrameRate::Ntsc, start, start + 499, 0, 0)
                    .cell(0, 0, 3, 0, FrameRate::Ntsc, start + 500, start + 509, 0, 0)
                    .time(0, 7, 3, 0, FrameRate::Ntsc),
            );
        }

        let mut builder = VtsBuilder::new().video(1, 0, 3, 0).audio(0, 1, 0, *b"en");

        for pgc in pgcs {
            builder = builder.pgc(pgc);
        }

        // Play-all: chapters 1-3 map to PGC 1 programs 1-3
        // Episodes: each is vts_ttn 2-4 with 1 chapter mapping to PGC 2-4 program 1
        let play_all_ptts: &[(u16, u16)] = &[(1, 1), (1, 2), (1, 3)];
        let ep1_ptts: &[(u16, u16)] = &[(2, 1)];
        let ep2_ptts: &[(u16, u16)] = &[(3, 1)];
        let ep3_ptts: &[(u16, u16)] = &[(4, 1)];
        builder = builder.title_ptts(&[play_all_ptts, ep1_ptts, ep2_ptts, ep3_ptts]);

        let vts_data = builder.build();

        let (vmg, vts_map) = build_test_disc(
            &[
                (1, 1, 3, 1), // play-all
                (1, 2, 1, 1), // ep 1
                (1, 3, 1, 1), // ep 2
                (1, 4, 1, 1), // ep 3
            ],
            &[(1, vts_data)],
        );

        let titles = resolve_titles(&vmg, &vts_map);
        assert_eq!(
            titles.len(),
            4,
            "should have 4 titles (play-all + 3 episodes)"
        );

        let main = identify_main_title(&titles);
        assert_eq!(main, Some(1), "main title should be the play-all");
        assert!(
            titles[0].duration > titles[1].duration,
            "play-all should be longer than individual episodes"
        );
    }

    #[test]
    fn unreferenced_pgcs() {
        // a DVD title pattern: VTS with PGCs not referenced by any title
        let vts_data = VtsBuilder::new()
            .video(1, 0, 3, 0)
            .audio(0, 1, 0, *b"en")
            .pgc(
                PgcBuilder::new()
                    .entry_id(0x81)
                    .programs(1)
                    .time(1, 30, 0, 0, FrameRate::Ntsc)
                    .cells_simple(1, 500)
                    .audio_available(&[0]),
            )
            .pgc(
                PgcBuilder::new()
                    .entry_id(0x01)
                    .programs(1)
                    .time(0, 22, 0, 0, FrameRate::Ntsc)
                    .cells_simple(1, 100)
                    .audio_available(&[0]),
            )
            .pgc(
                PgcBuilder::new()
                    .entry_id(0x01)
                    .programs(1)
                    .time(0, 3, 0, 0, FrameRate::Ntsc)
                    .cells_simple(1, 50)
                    .audio_available(&[0]),
            )
            .title_ptts(&[&[(1, 1)]]) // only PGC 1 is referenced
            .build();

        let (vmg, vts_map) = build_test_disc(&[(1, 1, 1, 1)], &[(1, vts_data)]);

        let unreferenced = find_unreferenced_pgcs(&vmg, &vts_map);
        assert_eq!(unreferenced.len(), 2, "should find 2 unreferenced PGCs");
        assert_eq!(unreferenced[0].pgc_index, 2, "first unreferenced is PGC 2");
        assert_eq!(unreferenced[1].pgc_index, 3, "second unreferenced is PGC 3");
        assert!(
            unreferenced[0].duration > Duration::ZERO,
            "unreferenced PGC should have nonzero duration"
        );
    }

    #[test]
    fn stream_filtering_by_pgc() {
        // VTS has 3 audio streams but PGC only enables 2
        let vts_data = VtsBuilder::new()
            .video(1, 0, 3, 0)
            .audio(0, 1, 0, *b"en")
            .audio(0, 5, 0, *b"en")
            .audio(0, 1, 0, *b"fr")
            .pgc(
                PgcBuilder::new()
                    .programs(1)
                    .time(0, 5, 0, 0, FrameRate::Ntsc)
                    .cells_simple(1, 500)
                    .audio_available(&[0, 1]),
            )
            .title_ptts(&[&[(1, 1)]])
            .build();

        let (vmg, vts_map) = build_test_disc(&[(1, 1, 1, 1)], &[(1, vts_data)]);

        let titles = resolve_titles(&vmg, &vts_map);
        assert_eq!(titles.len(), 1, "should have 1 title");
        assert_eq!(
            titles[0].streams.audio.len(),
            2,
            "should have 2 audio streams (not 3)"
        );
    }

    #[test]
    fn composite_grouping() {
        // Play-all title contains individual episode sectors
        let pa: &[(u16, u16)] = &[(1, 1), (1, 2), (1, 3)];
        let e1: &[(u16, u16)] = &[(2, 1)];
        let e2: &[(u16, u16)] = &[(3, 1)];
        let e3: &[(u16, u16)] = &[(4, 1)];
        let vts_data = VtsBuilder::new()
            .video(1, 0, 3, 0)
            .audio(0, 1, 0, *b"en")
            .pgc(
                PgcBuilder::new()
                    .entry_id(0x81)
                    .programs(3)
                    .cell(0, 7, 0, 0, FrameRate::Ntsc, 0, 499, 0, 0)
                    .cell(0, 7, 0, 0, FrameRate::Ntsc, 500, 999, 0, 0)
                    .cell(0, 7, 0, 0, FrameRate::Ntsc, 1000, 1499, 0, 0)
                    .time(0, 21, 0, 0, FrameRate::Ntsc)
                    .audio_available(&[0]),
            )
            .pgc(
                PgcBuilder::new()
                    .entry_id(0x82)
                    .programs(1)
                    .cell(0, 7, 0, 0, FrameRate::Ntsc, 0, 499, 0, 0)
                    .time(0, 7, 0, 0, FrameRate::Ntsc)
                    .audio_available(&[0]),
            )
            .pgc(
                PgcBuilder::new()
                    .entry_id(0x83)
                    .programs(1)
                    .cell(0, 7, 0, 0, FrameRate::Ntsc, 500, 999, 0, 0)
                    .time(0, 7, 0, 0, FrameRate::Ntsc)
                    .audio_available(&[0]),
            )
            .pgc(
                PgcBuilder::new()
                    .entry_id(0x84)
                    .programs(1)
                    .cell(0, 7, 0, 0, FrameRate::Ntsc, 1000, 1499, 0, 0)
                    .time(0, 7, 0, 0, FrameRate::Ntsc)
                    .audio_available(&[0]),
            )
            .title_ptts(&[pa, e1, e2, e3])
            .build();

        let (vmg, vts_map) = build_test_disc(
            &[(1, 1, 3, 1), (1, 2, 1, 1), (1, 3, 1, 1), (1, 4, 1, 1)],
            &[(1, vts_data)],
        );

        let titles = resolve_titles(&vmg, &vts_map);
        let titles = apply_composite_grouping(titles, &vmg, &vts_map);

        let play_all = &titles[0];
        assert_eq!(
            play_all.members,
            vec![2, 3, 4],
            "play-all should contain episodes 2, 3, 4"
        );

        // Individual episodes should not have members
        for t in &titles[1..] {
            assert!(t.members.is_empty(), "episode should not have members");
        }
    }

    #[test]
    fn pal_disc() {
        // a PAL DVD title pattern: PAL, 720×576
        let vts_data = VtsBuilder::new()
            .video(1, 1, 3, 0) // PAL
            .audio(0, 1, 0, *b"en")
            .pgc(
                PgcBuilder::new()
                    .programs(3)
                    .time(0, 0, 31, 3, FrameRate::Pal)
                    .cells_simple(3, 500)
                    .audio_available(&[0]),
            )
            .title_ptts(&[&[(1, 1), (1, 2), (1, 3)]])
            .build();

        let (_vmg, vts_map) = build_test_disc(&[(1, 1, 3, 1)], &[(1, vts_data)]);

        let title_sets = [analyze_title_set(1, vts_map.get(&1).expect("VTS 1 exists"))];
        assert!(
            title_sets[0].video.contains("PAL"),
            "should identify PAL: {}",
            title_sets[0].video
        );
        assert!(
            title_sets[0].video.contains("720\u{d7}576"),
            "should show PAL resolution: {}",
            title_sets[0].video
        );
    }
}
