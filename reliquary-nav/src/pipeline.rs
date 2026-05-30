// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Disc loading pipeline — reads IG data and builds a compositable page.
//!
//! Extracts the IG stream from a disc, decodes button bitmaps, and
//! optionally extracts a video background frame via ffmpeg.

use std::path::Path;

use reliquary::disc::bdmv::compose::{ButtonComposition, PageComposition};
use reliquary::disc::bdmv::{ig, read_clip, rle, ts};
use reliquary::disc::reader::DiscReader;
use reliquary::disc::{self, InspectResult};

/// A fully loaded page ready for rendering.
pub struct LoadedPage {
    /// The compositable page data.
    pub page: PageComposition,
    /// Video background RGBA buffer (if extracted), matching canvas dimensions.
    background: Option<Vec<u8>>,
}

impl LoadedPage {
    /// Returns the background as a slice reference, if available.
    pub fn background(&self) -> Option<&[u8]> {
        self.background.as_deref()
    }
}

/// Loads a single page from a disc for rendering.
///
/// # Errors
///
/// Returns an error string if the disc cannot be read, the clip index
/// or page index is out of range, or IG parsing fails.
pub fn load_page(
    path: &Path,
    clip_index: usize,
    page_index: usize,
    vuk: Option<&[u8; 16]>,
) -> Result<LoadedPage, String> {
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

    // Find the requested page
    let comp = ds
        .compositions
        .first()
        .ok_or("no compositions in display set")?;

    let page = comp.pages.get(page_index).ok_or_else(|| {
        format!(
            "page index {page_index} out of range (have {} pages)",
            comp.pages.len()
        )
    })?;

    // Decode button bitmaps
    let mut btn_comps = Vec::new();
    for button in &page.buttons {
        let normal = ds
            .objects
            .iter()
            .find(|o| o.object_id == button.normal_object_id)
            .and_then(|o| rle::decode(o, palette).ok());
        let selected = ds
            .objects
            .iter()
            .find(|o| o.object_id == button.selected_object_id)
            .and_then(|o| rle::decode(o, palette).ok());

        btn_comps.push(ButtonComposition {
            button_id: button.button_id,
            x: button.x,
            y: button.y,
            normal,
            selected,
        });
    }

    let page_comp = PageComposition {
        clip_index,
        page_id: page.page_id,
        canvas_width: comp.width,
        canvas_height: comp.height,
        buttons: btn_comps,
    };

    // Extract video background
    let background = extract_background(&reader, &analysis, ig_clip, vuk, comp.width, comp.height);

    Ok(LoadedPage {
        page: page_comp,
        background,
    })
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
