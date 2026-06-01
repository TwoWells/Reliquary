// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Disc loading pipeline — reads IG data and builds compositable pages.
//!
//! Split into two layers:
//! - [`DiscState`]: disc-level state (reader, analysis, MOBJ file) loaded
//!   once and shared across clip switches.
//! - [`LoadedClip`]: one IG clip's decoded data, rebuilt on cross-clip
//!   navigation.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use reliquary::disc::bdmv::compose::{ButtonComposition, PageComposition};
use reliquary::disc::bdmv::ig::{self, NavigationCommand};
use reliquary::disc::bdmv::mobj::{self, MovieObjectFile};
use reliquary::disc::bdmv::{BdmvAnalysis, read_clip, rle, ts};
use reliquary::disc::reader::DiscReader;
use reliquary::disc::{self, InspectResult};

pub use mobj::{ButtonEffect, PlayTarget, PlayerContext, execute_button_commands, run_mobj_chain};

// ── Disc-level state ─────────────────────────────────────────────────

/// Disc-level state shared across clip switches.
///
/// Loaded once by [`load_disc`] and kept alive for the navigator's
/// lifetime. Provides the reader, analysis, MOBJ infrastructure, and
/// initial GPR state needed for dispatch and cross-clip navigation.
pub struct DiscState {
    /// Disc reader for loading clips on demand.
    pub reader: DiscReader,
    /// Full disc analysis (IG clips, menu playlists, video clip mapping).
    pub analysis: BdmvAnalysis,
    /// Parsed movie objects for VM execution.
    pub mobj_file: MovieObjectFile,
    /// Title number → MOBJ index mapping (from `index.bdmv`).
    pub title_to_mobj: HashMap<u32, usize>,
    /// Menu playlist numbers as a set (for fast lookup).
    pub menu_playlists: HashSet<u32>,
    /// Title MOBJ resume point for `SET_BUTTON_PAGE` dispatch.
    ///
    /// `(mobj_index, resume_pc)` — the instruction after the title
    /// MOBJ's menu `PlayPl`, where execution resumes when IG terminates.
    pub resume_point: Option<(usize, usize)>,
    /// VUK for AACS-encrypted discs.
    pub vuk: Option<[u8; 16]>,
    /// Initial GPR state from MOBJ\[0\] + title MOBJ chain.
    pub init_gprs: HashMap<u32, u32>,
}

/// Loads disc-level state: reader, analysis, MOBJ file, and GPR seed.
///
/// # Errors
///
/// Returns an error string if the disc cannot be read or is not a BDMV disc.
pub fn load_disc(path: &Path, vuk: Option<[u8; 16]>) -> Result<DiscState, String> {
    let reader = DiscReader::open(path).map_err(|e| format!("failed to open disc: {e}"))?;

    let analysis = match disc::inspect(path).map_err(|e| format!("inspect failed: {e}"))? {
        InspectResult::Bdmv(a) => a,
        InspectResult::Dvd(_) => return Err("DVD discs do not have IG menus".into()),
    };

    let mobj_file = load_mobj_file(&reader);
    let title_to_mobj = load_title_to_mobj_map(&reader);
    let menu_playlists: HashSet<u32> = analysis.menu_playlists.iter().copied().collect();

    let resume_point = mobj::find_title_resume_point(&mobj_file, &menu_playlists, &title_to_mobj);

    let init_gprs = seed_gprs(&mobj_file, &menu_playlists, &title_to_mobj);

    Ok(DiscState {
        reader,
        analysis,
        mobj_file,
        title_to_mobj,
        menu_playlists,
        resume_point,
        vuk,
        init_gprs,
    })
}

// ── Clip-level state ─────────────────────────────────────────────────

/// All decoded data for one IG clip — supports building any page on demand.
pub struct LoadedClip {
    /// Clip index within the disc's IG clip list.
    clip_index: usize,
    /// Canvas width from the IC descriptor.
    canvas_width: u16,
    /// Canvas height from the IC descriptor.
    canvas_height: u16,
    /// Decoded button bitmaps keyed by object ID, shared across pages.
    bitmaps: Vec<DecodedObject>,
    /// All pages in presentation order from the IC.
    pages: Vec<ig::Page>,
    /// Video background RGBA buffer (if extracted), matching canvas dimensions.
    background: Option<Vec<u8>>,
}

/// A decoded bitmap object, identified by its object ID.
struct DecodedObject {
    object_id: u16,
    bitmap: rle::Bitmap,
}

/// A fully loaded page ready for rendering, with navigation command access.
pub struct LoadedPage {
    /// The compositable page data (positions + bitmaps).
    pub page: PageComposition,
    /// Navigation commands for each button, indexed by position in
    /// `page.buttons` (same order, same length).
    pub button_commands: Vec<Vec<NavigationCommand>>,
    /// Whether each button has the auto-action flag set (parallel to
    /// `button_commands`). Auto-action buttons activate immediately
    /// when selected — used with `default_selected_button_id` to
    /// implement bootstrap page routing.
    pub auto_action: Vec<bool>,
    /// Button selected by default when this page loads (0xFFFF = none).
    pub default_selected_button_id: u16,
    /// Button activated directly when this page loads (0xFFFF = none).
    pub default_activated_button_id: u16,
}

impl LoadedPage {
    /// Returns the navigation commands for the given button ID.
    pub fn commands_for_button(&self, button_id: u16) -> Option<&[NavigationCommand]> {
        let idx = self
            .page
            .buttons
            .iter()
            .position(|b| b.button_id == button_id)?;
        self.button_commands.get(idx).map(Vec::as_slice)
    }
}

impl LoadedClip {
    /// Returns the background as a slice reference, if available.
    pub fn background(&self) -> Option<&[u8]> {
        self.background.as_deref()
    }

    /// Returns the clip index within the disc's IG clip list.
    pub const fn clip_index(&self) -> usize {
        self.clip_index
    }

    /// Number of pages in this clip.
    pub const fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Builds a [`LoadedPage`] for the given page index.
    ///
    /// Returns `None` if the page index is out of range.
    pub fn build_page(&self, page_index: usize) -> Option<LoadedPage> {
        let page = self.pages.get(page_index)?;

        let mut btn_comps = Vec::with_capacity(page.buttons.len());
        let mut button_commands = Vec::with_capacity(page.buttons.len());
        let mut auto_action = Vec::with_capacity(page.buttons.len());

        for button in &page.buttons {
            let normal = self.find_bitmap(button.normal_object_id);
            let selected = self.find_bitmap(button.selected_object_id);

            btn_comps.push(ButtonComposition {
                button_id: button.button_id,
                x: button.x,
                y: button.y,
                normal,
                selected,
            });
            button_commands.push(button.commands.clone());
            auto_action.push(button.auto_action);
        }

        let page_comp = PageComposition {
            clip_index: self.clip_index,
            page_id: page.page_id,
            canvas_width: self.canvas_width,
            canvas_height: self.canvas_height,
            buttons: btn_comps,
        };

        Some(LoadedPage {
            page: page_comp,
            button_commands,
            auto_action,
            default_selected_button_id: page.default_selected_button_id,
            default_activated_button_id: page.default_activated_button_id,
        })
    }

    /// Finds the page index for a given page ID.
    ///
    /// Page IDs are assigned by the disc author and may not be sequential
    /// starting from 0. This maps from the ID in `SetButtonPage` commands
    /// to the index used by [`build_page`](Self::build_page).
    pub fn page_index_for_id(&self, page_id: u8) -> Option<usize> {
        self.pages.iter().position(|p| p.page_id == page_id)
    }

    /// Clones a bitmap for the given object ID, if it was successfully decoded.
    fn find_bitmap(&self, object_id: u16) -> Option<rle::Bitmap> {
        self.bitmaps
            .iter()
            .find(|o| o.object_id == object_id)
            .map(|o| o.bitmap.clone())
    }
}

/// Loads an entire IG clip from the disc state.
///
/// Parses all pages, decodes all button bitmaps, and optionally extracts
/// a video background frame. The returned [`LoadedClip`] can produce
/// a [`LoadedPage`] for any page via [`build_page`](LoadedClip::build_page).
///
/// # Errors
///
/// Returns an error string if the clip index is out of range or IG
/// parsing fails.
pub fn load_clip(disc: &DiscState, clip_index: usize) -> Result<LoadedClip, String> {
    let ig_clip = disc.analysis.ig_clips.get(clip_index).ok_or_else(|| {
        format!(
            "clip index {clip_index} out of range (have {} clips)",
            disc.analysis.ig_clips.len()
        )
    })?;

    // Read and decrypt the IG clip
    let data = read_clip(&disc.reader, disc.vuk.as_ref(), &ig_clip.clip_id)
        .map_err(|e| format!("failed to read clip {}: {e}", ig_clip.clip_id))?;

    // Demux MPEG-TS to get PES packets
    let pes_packets =
        ts::demux(&data).map_err(|e| format!("failed to demux clip {}: {e}", ig_clip.clip_id))?;

    // Get IG stream PID
    let ig_pid = ig_clip
        .ig_streams
        .first()
        .ok_or_else(|| format!("clip {} has no IG streams", ig_clip.clip_id))?
        .pid;

    // Concatenate IG PES payloads
    let mut ig_payload = Vec::new();
    for pes in &pes_packets {
        if pes.pid == ig_pid
            && let Ok(parsed) = ts::parse_pes(&pes.data)
        {
            ig_payload.extend_from_slice(&parsed.payload);
        }
    }

    if ig_payload.is_empty() {
        return Err(format!("no IG data in clip {}", ig_clip.clip_id));
    }

    // Parse IG segments
    let ig_stream = ig::parse(&ig_payload)
        .map_err(|e| format!("failed to parse IG in clip {}: {e}", ig_clip.clip_id))?;

    // Get the first display set
    let ds = ig_stream
        .display_sets
        .into_iter()
        .next()
        .ok_or("no display sets in IG stream")?;

    let palette = ds.palettes.first().ok_or("no palettes in display set")?;

    let comp = ds
        .compositions
        .into_iter()
        .next()
        .ok_or("no compositions in display set")?;

    // Decode all bitmap objects up front (shared across pages)
    let mut bitmaps = Vec::with_capacity(ds.objects.len());
    for obj in &ds.objects {
        if let Ok(bitmap) = rle::decode(obj, palette) {
            bitmaps.push(DecodedObject {
                object_id: obj.object_id,
                bitmap,
            });
        }
    }

    // Extract video background
    let background = extract_background(
        &disc.reader,
        &disc.analysis,
        ig_clip,
        disc.vuk.as_ref(),
        comp.width,
        comp.height,
    );

    Ok(LoadedClip {
        clip_index,
        canvas_width: comp.width,
        canvas_height: comp.height,
        bitmaps,
        pages: comp.pages,
        background,
    })
}

// ── Internal helpers ─────────────────────────────────────────────────

/// Seeds GPR state from MOBJ\[0\] + title MOBJ chain.
fn seed_gprs(
    mobj_file: &MovieObjectFile,
    menu_playlists: &HashSet<u32>,
    title_to_mobj: &HashMap<u32, usize>,
) -> HashMap<u32, u32> {
    let mut init_gprs = mobj::seed_gpr_state(mobj_file);

    if !menu_playlists.is_empty() {
        let title_gprs = mobj::seed_title_gprs(mobj_file, menu_playlists, title_to_mobj);
        for (k, v) in title_gprs {
            init_gprs.insert(k, v);
        }
    }

    init_gprs
}

/// Parses `MovieObject.bdmv`. Returns an empty file if missing or invalid.
fn load_mobj_file(reader: &DiscReader) -> MovieObjectFile {
    let mobj_path = std::path::Path::new("BDMV/MovieObject.bdmv");
    let mobj_alt = std::path::Path::new("MovieObject.bdmv");

    let data = reader
        .read_file(mobj_path)
        .or_else(|_| reader.read_file(mobj_alt));

    data.ok()
        .and_then(|d| mobj::parse(&d).ok())
        .unwrap_or(MovieObjectFile {
            objects: Vec::new(),
        })
}

/// Reads `index.bdmv` and builds a title-number → MOBJ-index mapping.
///
/// Title 0 = First Playback, title 0xFFFF = Top Menu, titles 1..N =
/// regular titles (1-based).
fn load_title_to_mobj_map(reader: &DiscReader) -> HashMap<u32, usize> {
    use reliquary::disc::bdmv::index;

    let index_path = std::path::Path::new("BDMV/index.bdmv");
    let index_alt = std::path::Path::new("index.bdmv");

    let index_data = reader
        .read_file(index_path)
        .or_else(|_| reader.read_file(index_alt));

    let Ok(data) = index_data else {
        return HashMap::new();
    };

    let Ok(disc_index) = index::parse(&data) else {
        return HashMap::new();
    };

    let mut map = HashMap::new();

    // Title 0 = First Playback
    if let index::ObjectType::Hdmv { id_ref, .. } = &disc_index.first_play.object {
        map.insert(0, usize::from(*id_ref));
    }

    // Title 0xFFFF = Top Menu
    if let index::ObjectType::Hdmv { id_ref, .. } = &disc_index.top_menu.object {
        map.insert(0xFFFF, usize::from(*id_ref));
    }

    // Regular titles (1-based)
    for (i, title) in disc_index.titles.iter().enumerate() {
        if let index::ObjectType::Hdmv { id_ref, .. } = &title.object {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "disc title count is small (u16 in index.bdmv)"
            )]
            let title_num = (i as u32) + 1;
            map.insert(title_num, usize::from(*id_ref));
        }
    }

    map
}

/// Attempts to extract a video background frame for the IG clip.
fn extract_background(
    reader: &DiscReader,
    analysis: &BdmvAnalysis,
    ig_clip: &reliquary::disc::bdmv::IgClip,
    vuk: Option<&[u8; 16]>,
    width: u16,
    height: u16,
) -> Option<Vec<u8>> {
    /// Maximum video clip size to read for background extraction (50 MB).
    const MAX_VIDEO_CLIP_SIZE: usize = 50 * 1024 * 1024;

    let video_clip_id = analysis
        .ig_video_clips
        .get(&ig_clip.clip_id)
        .map_or_else(|| ig_clip.clip_id.clone(), Clone::clone);

    let clip_data = read_clip(reader, vuk, &video_clip_id).ok()?;
    if clip_data.len() > MAX_VIDEO_CLIP_SIZE {
        return None;
    }

    extract_video_frame(&clip_data, width, height)
}

/// Extracts the first video frame from an m2ts clip as RGBA pixel data.
fn extract_video_frame(clip_data: &[u8], width: u16, height: u16) -> Option<Vec<u8>> {
    use std::io::Write;

    let temp_dir = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .map(|h| h.join("tmp"))
        .filter(|d| d.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let temp_path = temp_dir.join(format!("reliquary_nav_{}.m2ts", std::process::id()));

    std::fs::File::create(&temp_path)
        .and_then(|mut f| f.write_all(clip_data))
        .ok()?;

    let output = std::process::Command::new("ffmpeg")
        .arg("-v")
        .arg("quiet")
        .arg("-i")
        .arg(&temp_path)
        .arg("-vframes")
        .arg("1")
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg("rgba")
        .arg("-s")
        .arg(format!("{width}x{height}"))
        .arg("pipe:1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok();

    let _ = std::fs::remove_file(&temp_path);

    let output = output?;
    let expected = usize::from(width) * usize::from(height) * 4;
    if output.status.success() && output.stdout.len() == expected {
        Some(output.stdout)
    } else {
        None
    }
}
