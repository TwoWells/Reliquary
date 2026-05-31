// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Disc loading pipeline — reads IG data and builds compositable pages.
//!
//! Loads an entire IG clip once (all pages, bitmaps, palette), then builds
//! [`LoadedPage`] views on demand for any page within the clip. This
//! supports page navigation without re-reading the disc.

use std::collections::HashMap;
use std::path::Path;

use reliquary::disc::bdmv::compose::{ButtonComposition, PageComposition};
use reliquary::disc::bdmv::ig::{self, NavigationCommand};
use reliquary::disc::bdmv::mobj;
use reliquary::disc::bdmv::{read_clip, rle, ts};
use reliquary::disc::reader::DiscReader;
use reliquary::disc::{self, InspectResult};

pub use mobj::{ButtonEffect, PlayerContext, execute_button_commands};

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
    /// Initial GPR state from MOBJ\[0\] (First Play) execution.
    ///
    /// WB and similar authoring stores per-content-item configuration in a
    /// GPR database initialized by MOBJ\[0\]. Button commands read from
    /// these registers to compute dispatch values and page targets.
    init_gprs: HashMap<u32, u32>,
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

    /// Returns the initial GPR state from MOBJ\[0\] execution.
    #[allow(
        clippy::missing_const_for_fn,
        reason = "HashMap::new is not const-stable"
    )]
    pub fn init_gprs(&self) -> &HashMap<u32, u32> {
        &self.init_gprs
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

/// Loads an entire IG clip from a disc.
///
/// Parses all pages, decodes all button bitmaps, and optionally extracts
/// a video background frame. The returned [`LoadedClip`] can produce
/// a [`LoadedPage`] for any page via [`build_page`](LoadedClip::build_page).
///
/// # Errors
///
/// Returns an error string if the disc cannot be read, the clip index
/// is out of range, or IG parsing fails.
pub fn load_clip(
    path: &Path,
    clip_index: usize,
    vuk: Option<&[u8; 16]>,
) -> Result<LoadedClip, String> {
    let reader = DiscReader::open(path).map_err(|e| format!("failed to open disc: {e}"))?;

    let analysis = match disc::inspect(path).map_err(|e| format!("inspect failed: {e}"))? {
        InspectResult::Bdmv(a) => a,
        InspectResult::Dvd(_) => return Err("DVD discs do not have IG menus".into()),
    };

    let ig_clip = analysis.ig_clips.get(clip_index).ok_or_else(|| {
        format!(
            "clip index {clip_index} out of range (have {} clips)",
            analysis.ig_clips.len()
        )
    })?;

    // Read and decrypt the IG clip
    let data = read_clip(&reader, vuk, &ig_clip.clip_id)
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
    let background = extract_background(&reader, &analysis, ig_clip, vuk, comp.width, comp.height);

    // Seed GPR state from MOBJ[0] (First Play). Button commands on
    // complex discs read from a GPR database initialized here.
    let init_gprs = load_mobj_gprs(&reader, &analysis.menu_playlists);

    Ok(LoadedClip {
        clip_index,
        canvas_width: comp.width,
        canvas_height: comp.height,
        bitmaps,
        pages: comp.pages,
        background,
        init_gprs,
    })
}

/// Loads `MovieObject.bdmv`, seeds the GPR database from MOBJ\[0\], and
/// executes the title MOBJ chain to capture per-page context registers.
///
/// Phase 1: [`mobj::seed_gpr_state`] runs MOBJ\[0\] to capture the global
/// GPR database (e.g. GPR\[3000+\] on WB).
///
/// Phase 2: [`mobj::seed_title_gprs`] follows the MOBJ chain from MOBJ\[0\]
/// through `JumpObject`/`JumpTitle` instructions to the title MOBJ that
/// plays the menu playlist, capturing per-page context registers (e.g.
/// GPR\[3\] on a Blu-ray series). Title MOBJ registers override MOBJ\[0\]
/// where both set the same register.
///
/// Returns an empty map if the file is missing or cannot be parsed.
fn load_mobj_gprs(reader: &DiscReader, menu_playlists: &[u32]) -> HashMap<u32, u32> {
    let mobj_path = std::path::Path::new("BDMV/MovieObject.bdmv");
    let mobj_alt = std::path::Path::new("MovieObject.bdmv");

    let mobj_data = reader
        .read_file(mobj_path)
        .or_else(|_| reader.read_file(mobj_alt));

    let Ok(data) = mobj_data else {
        return HashMap::new();
    };

    let Ok(mobj_file) = mobj::parse(&data) else {
        return HashMap::new();
    };

    // Phase 1: MOBJ[0] GPR database (global config registers)
    let mut init_gprs = mobj::seed_gpr_state(&mobj_file);

    // Phase 2: Title MOBJ context registers (per-page state)
    if !menu_playlists.is_empty() {
        let menu_set: std::collections::HashSet<u32> = menu_playlists.iter().copied().collect();
        let title_to_mobj = load_title_to_mobj_map(reader);
        let title_gprs = mobj::seed_title_gprs(&mobj_file, &menu_set, &title_to_mobj);
        // Title MOBJ overrides MOBJ[0] where both set the same register
        for (k, v) in title_gprs {
            init_gprs.insert(k, v);
        }
    }

    init_gprs
}

/// Reads `index.bdmv` and builds a title-number → MOBJ-index mapping.
///
/// Used by [`mobj::seed_title_gprs`] to resolve `JumpTitle` instructions.
/// Title 0 = First Playback, title 0xFFFF = Top Menu, titles 1..N =
/// regular titles (1-based, matching libbluray's `_jump_title` semantics).
///
/// Returns an empty map if the index file is missing or cannot be parsed.
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
///
/// Uses ffmpeg to decode the first frame from the video clip mapped
/// to this IG clip. Returns `None` if ffmpeg is unavailable or
/// extraction fails.
fn extract_background(
    reader: &DiscReader,
    analysis: &reliquary::disc::bdmv::BdmvAnalysis,
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
///
/// Uses `ffmpeg` to decode one video frame, scaled to the given dimensions.
/// Returns `None` if `ffmpeg` is not available or frame extraction fails.
fn extract_video_frame(clip_data: &[u8], width: u16, height: u16) -> Option<Vec<u8>> {
    use std::io::Write;

    // Use ~/tmp if it exists (avoids tmpfs/ramdisk pressure for large clips),
    // otherwise fall back to the system temp directory.
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
