// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! End-to-end tests for `reliquary inspect`.
//!
//! These tests build synthetic disc structures (BDMV or `VIDEO_TS`) from
//! binary fixtures, run the `reliquary-cli` binary against them, and
//! assert on the exact stdout output.

#![allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
#![allow(
    clippy::cast_possible_truncation,
    reason = "test builder values are small known constants"
)]

use std::fs;
use std::process::Command;

use tempfile::TempDir;

/// Returns the path to the built `reliquary-cli` binary.
fn cli_bin() -> std::path::PathBuf {
    // cargo-nextest sets this; fall back to target/debug
    std::env::var("CARGO_BIN_EXE_reliquary-cli").map_or_else(
        |_| {
            let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            path.pop();
            path.push("target");
            path.push("debug");
            path.push("reliquary-cli");
            path
        },
        std::path::PathBuf::from,
    )
}

/// Builds a minimal MPLS binary for a single play item.
///
/// Produces a valid MPLS file with one play item (H.264 1080p 23.976,
/// AC-3 2.0 eng) and the specified chapter marks.
fn build_mpls(
    clip_id: &str,
    in_time: u32,
    out_time: u32,
    conn: u8,
    marks: &[(u16, u32)],
) -> Vec<u8> {
    let mut buf = Vec::new();

    // ── Header ──
    buf.extend_from_slice(b"MPLS");
    buf.extend_from_slice(b"0200");
    buf.extend_from_slice(&[0u8; 4]); // playlist_offset placeholder (8)
    buf.extend_from_slice(&[0u8; 4]); // mark_offset placeholder (12)
    buf.extend_from_slice(&[0u8; 4]); // extension_offset
    buf.extend_from_slice(&[0u8; 20]); // reserved

    // ── AppInfoPlayList ──
    buf.extend_from_slice(&0x0000_000E_u32.to_be_bytes());
    buf.extend_from_slice(&[0u8; 14]);

    // ── Playlist section ──
    let playlist_offset = buf.len() as u32;
    buf[8..12].copy_from_slice(&playlist_offset.to_be_bytes());

    // Build the single play item
    let item_data = build_play_item(clip_id, in_time, out_time, conn);
    let item_length = item_data.len() as u16;

    let section_length = (6 + 2 + item_data.len()) as u32;
    buf.extend_from_slice(&section_length.to_be_bytes());
    buf.extend_from_slice(&[0u8; 2]); // reserved
    buf.extend_from_slice(&1u16.to_be_bytes()); // num_play_items
    buf.extend_from_slice(&0u16.to_be_bytes()); // num_sub_paths
    buf.extend_from_slice(&item_length.to_be_bytes());
    buf.extend_from_slice(&item_data);

    // ── Mark section ──
    let mark_offset = buf.len() as u32;
    buf[12..16].copy_from_slice(&mark_offset.to_be_bytes());

    let marks_length = (2 + marks.len() * 14) as u32;
    buf.extend_from_slice(&marks_length.to_be_bytes());
    buf.extend_from_slice(&(marks.len() as u16).to_be_bytes());
    for &(play_item_ref, timestamp) in marks {
        buf.push(0); // reserved
        buf.push(1); // mark_type = entry
        buf.extend_from_slice(&play_item_ref.to_be_bytes());
        buf.extend_from_slice(&timestamp.to_be_bytes());
        buf.extend_from_slice(&0xFFFF_u16.to_be_bytes()); // entry_ES_PID
        buf.extend_from_slice(&0u32.to_be_bytes()); // duration
    }

    buf
}

/// Builds a multi-item MPLS binary.
fn build_multi_item_mpls(items: &[(&str, u32, u32, u8)], marks: &[(u16, u32)]) -> Vec<u8> {
    let mut buf = Vec::new();

    buf.extend_from_slice(b"MPLS");
    buf.extend_from_slice(b"0200");
    buf.extend_from_slice(&[0u8; 4]); // playlist_offset
    buf.extend_from_slice(&[0u8; 4]); // mark_offset
    buf.extend_from_slice(&[0u8; 4]); // extension_offset
    buf.extend_from_slice(&[0u8; 20]); // reserved

    buf.extend_from_slice(&0x0000_000E_u32.to_be_bytes());
    buf.extend_from_slice(&[0u8; 14]);

    let playlist_offset = buf.len() as u32;
    buf[8..12].copy_from_slice(&playlist_offset.to_be_bytes());

    let mut items_buf = Vec::new();
    for &(clip_id, in_time, out_time, conn) in items {
        let item_data = build_play_item(clip_id, in_time, out_time, conn);
        let item_length = item_data.len() as u16;
        items_buf.extend_from_slice(&item_length.to_be_bytes());
        items_buf.extend_from_slice(&item_data);
    }

    let section_length = (6 + items_buf.len()) as u32;
    buf.extend_from_slice(&section_length.to_be_bytes());
    buf.extend_from_slice(&[0u8; 2]);
    buf.extend_from_slice(&(items.len() as u16).to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes());
    buf.extend_from_slice(&items_buf);

    let mark_offset = buf.len() as u32;
    buf[12..16].copy_from_slice(&mark_offset.to_be_bytes());

    let marks_length = (2 + marks.len() * 14) as u32;
    buf.extend_from_slice(&marks_length.to_be_bytes());
    buf.extend_from_slice(&(marks.len() as u16).to_be_bytes());
    for &(play_item_ref, timestamp) in marks {
        buf.push(0);
        buf.push(1);
        buf.extend_from_slice(&play_item_ref.to_be_bytes());
        buf.extend_from_slice(&timestamp.to_be_bytes());
        buf.extend_from_slice(&0xFFFF_u16.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
    }

    buf
}

fn build_play_item(clip_id: &str, in_time: u32, out_time: u32, conn: u8) -> Vec<u8> {
    let mut buf = Vec::new();

    // clip_id (5 ASCII)
    let mut id = [b'0'; 5];
    for (i, b) in clip_id.bytes().take(5).enumerate() {
        id[i] = b;
    }
    buf.extend_from_slice(&id);
    buf.extend_from_slice(b"M2TS"); // codec_id
    buf.extend_from_slice(&(u16::from(conn)).to_be_bytes()); // flags
    buf.push(0); // stc_id
    buf.extend_from_slice(&in_time.to_be_bytes());
    buf.extend_from_slice(&out_time.to_be_bytes());
    buf.extend_from_slice(&[0u8; 12]); // UO_mask + flags2 + still_mode + still_time

    // STN table: 1 video (H.264 1080p 23.976) + 1 audio (AC-3 2.0 eng)
    let mut entries = Vec::new();
    // Video
    entries.push(3u8); // entry_length
    entries.push(1); // stream_type
    entries.extend_from_slice(&0x1011_u16.to_be_bytes());
    entries.push(2); // attrs_length
    entries.push(0x1b); // H.264
    entries.push((6 << 4) | 1); // 1080p + 23.976

    // Audio
    entries.push(3u8);
    entries.push(1);
    entries.extend_from_slice(&0x1100_u16.to_be_bytes());
    entries.push(5); // attrs_length
    entries.push(0x81); // AC-3
    entries.push((3 << 4) | 1); // stereo + 48kHz
    entries.extend_from_slice(b"eng");

    let table_length = 2 + 7 + 5 + entries.len();
    buf.extend_from_slice(&(table_length as u16).to_be_bytes());
    buf.extend_from_slice(&[0u8; 2]); // reserved
    buf.push(1); // num_video
    buf.push(1); // num_audio
    buf.push(0); // num_pg
    buf.push(0); // num_ig
    buf.push(0); // num_sec_audio
    buf.push(0); // num_sec_video
    buf.push(0); // num_pip_pg
    buf.extend_from_slice(&[0u8; 5]); // reserved
    buf.extend_from_slice(&entries);

    buf
}

/// Creates a synthetic BDMV directory with the given MPLS files.
fn setup_bdmv(mpls_files: &[(u32, Vec<u8>)]) -> TempDir {
    let dir = TempDir::new().expect("should create temp dir");
    let playlist_dir = dir.path().join("BDMV").join("PLAYLIST");
    fs::create_dir_all(&playlist_dir).expect("should create BDMV/PLAYLIST");

    for (number, data) in mpls_files {
        let filename = format!("{number:05}.mpls");
        fs::write(playlist_dir.join(filename), data).expect("should write MPLS file");
    }

    dir
}

#[test]
fn inspect_single_episode_disc() {
    // Pattern A: single episode playlist
    let mpls = build_mpls("00004", 27_000_000, 59_040_000, 1, &[(0, 27_000_000)]);
    let dir = setup_bdmv(&[(100, mpls)]);

    let output = Command::new(cli_bin())
        .args(["inspect", &dir.path().display().to_string()])
        .output()
        .expect("should run inspect");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "inspect should succeed: {stdout}");
    assert!(
        stdout.contains("MPLS 00100"),
        "should show playlist 00100: {stdout}"
    );
    assert!(stdout.contains('*'), "should mark main title: {stdout}");
}

#[test]
fn inspect_multi_episode_disc() {
    // Pattern A: 3-episode play-all + 1 extra
    let play_all = build_multi_item_mpls(
        &[
            ("00004", 27_000_000, 59_040_000, 1),
            ("00005", 27_000_000, 59_175_000, 1),
            ("00006", 27_000_000, 59_310_000, 1),
        ],
        &[(0, 27_000_000), (1, 27_000_000), (2, 27_000_000)],
    );
    let extra = build_mpls("00010", 27_000_000, 30_600_000, 1, &[(0, 27_000_000)]);
    let dir = setup_bdmv(&[(1, play_all), (20, extra)]);

    let output = Command::new(cli_bin())
        .args(["inspect", &dir.path().display().to_string()])
        .output()
        .expect("should run inspect");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "inspect should succeed: {stdout}");
    // Play-all (3 items) should be main title
    assert!(
        stdout.contains("MPLS 00001") && stdout.contains('*'),
        "playlist 00001 should be main title: {stdout}"
    );
    assert!(
        stdout.contains("MPLS 00020"),
        "should show extras playlist: {stdout}"
    );
}

#[test]
fn inspect_json_output() {
    let mpls = build_mpls(
        "00004",
        27_000_000,
        59_040_000,
        1,
        &[(0, 27_000_000), (0, 28_144_890)],
    );
    let dir = setup_bdmv(&[(100, mpls)]);

    let output = Command::new(cli_bin())
        .args(["inspect", "--json", &dir.path().display().to_string()])
        .output()
        .expect("should run inspect --json");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "inspect --json should succeed: {stdout}"
    );

    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("output should be valid JSON");

    assert_eq!(json["format"], "bdmv", "format should be bdmv");
    assert_eq!(json["main_title"], 100, "main_title should be 100");
    assert_eq!(
        json["playlists"][0]["number"], 100,
        "playlist number should be 100"
    );
    assert_eq!(
        json["playlists"][0]["chapters"], 2,
        "should have 2 chapters"
    );
}

#[test]
fn inspect_unrecognised_format() {
    let dir = TempDir::new().expect("should create temp dir");

    let output = Command::new(cli_bin())
        .args(["inspect", &dir.path().display().to_string()])
        .output()
        .expect("should run inspect");

    assert!(
        !output.status.success(),
        "inspect should fail for unrecognised format"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unrecognised disc format"),
        "error message should mention format: {stderr}"
    );
}

// ── DVD helpers ──────────────────────────────────────────────────────────

const DVD_SECTOR: usize = 2048;

/// Encodes a BCD time value (hours, minutes, seconds, frames, rate).
const fn bcd_time(h: u8, m: u8, s: u8, f: u8, pal: bool) -> [u8; 4] {
    let rate: u8 = if pal { 1 } else { 3 };
    [
        (h / 10) << 4 | (h % 10),
        (m / 10) << 4 | (m % 10),
        (s / 10) << 4 | (s % 10),
        (rate << 6) | ((f / 10) << 4) | (f % 10),
    ]
}

/// Builds a minimal VMG (`VIDEO_TS.IFO`) with the given title pointers.
/// Each entry is `(title_set_nr, vts_ttn, nr_of_ptts, nr_of_angles)`.
fn build_vmg(titles: &[(u8, u8, u16, u8)], nr_title_sets: u16) -> Vec<u8> {
    let mut buf = vec![0u8; DVD_SECTOR * 2 + titles.len() * 12 + 8];

    buf[..12].copy_from_slice(b"DVDVIDEO-VMG");
    buf[0x3E..0x40].copy_from_slice(&nr_title_sets.to_be_bytes());
    buf[0xC4..0xC8].copy_from_slice(&1u32.to_be_bytes()); // tt_srpt at sector 1

    let tt = DVD_SECTOR;
    buf[tt..tt + 2].copy_from_slice(&(titles.len() as u16).to_be_bytes());
    let last_byte = (8 + titles.len() * 12 - 1) as u32;
    buf[tt + 4..tt + 8].copy_from_slice(&last_byte.to_be_bytes());

    for (i, &(vts, ttn, ptts, angles)) in titles.iter().enumerate() {
        let b = tt + 8 + i * 12;
        buf[b + 1] = angles;
        buf[b + 2..b + 4].copy_from_slice(&ptts.to_be_bytes());
        buf[b + 6] = vts;
        buf[b + 7] = ttn;
    }

    buf
}

/// Builds a minimal VTS IFO with a single PGC and PTT table.
fn build_vts(
    video_raw: u16,
    audio: &[(u8, u8, [u8; 2])],
    pgc_time: [u8; 4],
    cells: &[([u8; 4], u32, u32)],
    ptts: &[&[(u16, u16)]],
    audio_control: &[u16; 8],
) -> Vec<u8> {
    let nr_programs = cells.len() as u8;
    let nr_cells = cells.len() as u8;

    // PGC body: 236 fixed + program_map + cell_playback
    let pgc_prog_offset: u16 = 236;
    let pgc_cell_offset: u16 = pgc_prog_offset + u16::from(nr_programs);
    let pgc_size = usize::from(pgc_cell_offset) + usize::from(nr_cells) * 24;

    let pgci_header = 8;
    let pgci_srp = 8; // 1 PGC
    let pgcit_total = pgci_header + pgci_srp + pgc_size;

    let ptt_header = 8;
    let ptt_offsets = ptts.len() * 4;
    let ptt_entries: usize = ptts.iter().map(|p| p.len() * 4).sum();
    let ptt_total = ptt_header + ptt_offsets + ptt_entries;

    let total = 3 * DVD_SECTOR + ptt_total + pgcit_total;
    let mut buf = vec![0u8; total];

    // Header
    buf[..12].copy_from_slice(b"DVDVIDEO-VTS");
    buf[0x200..0x202].copy_from_slice(&video_raw.to_be_bytes());

    // Audio
    buf[0x203] = audio.len() as u8;
    for (i, &(fmt, ch, lang)) in audio.iter().enumerate() {
        let b = 0x204 + i * 8;
        buf[b] = fmt << 5;
        buf[b + 1] = ch;
        buf[b + 2] = lang[0];
        buf[b + 3] = lang[1];
    }

    // PTT at sector 1, PGC at sector 2
    buf[0xC8..0xCC].copy_from_slice(&1u32.to_be_bytes());
    buf[0xCC..0xD0].copy_from_slice(&2u32.to_be_bytes());

    // PTT table
    let ptt_base = DVD_SECTOR;
    buf[ptt_base..ptt_base + 2].copy_from_slice(&(ptts.len() as u16).to_be_bytes());
    buf[ptt_base + 4..ptt_base + 8].copy_from_slice(&((ptt_total - 1) as u32).to_be_bytes());

    let mut entry_off = ptt_offsets + ptt_header;
    for (i, ptt) in ptts.iter().enumerate() {
        let off_pos = ptt_base + 8 + i * 4;
        buf[off_pos..off_pos + 4].copy_from_slice(&(entry_off as u32).to_be_bytes());
        let abs = ptt_base + entry_off;
        for (j, &(pgcn, pgn)) in ptt.iter().enumerate() {
            let e = abs + j * 4;
            buf[e..e + 2].copy_from_slice(&pgcn.to_be_bytes());
            buf[e + 2..e + 4].copy_from_slice(&pgn.to_be_bytes());
        }
        entry_off += ptt.len() * 4;
    }

    // PGC table
    let pgcit_base = DVD_SECTOR * 2;
    buf[pgcit_base..pgcit_base + 2].copy_from_slice(&1u16.to_be_bytes()); // 1 PGC
    buf[pgcit_base + 4..pgcit_base + 8].copy_from_slice(&((pgcit_total - 1) as u32).to_be_bytes());

    // PGC search pointer
    let srp = pgcit_base + 8;
    buf[srp] = 0x81; // entry_id
    let pgc_data_off = (pgci_header + pgci_srp) as u32;
    buf[srp + 4..srp + 8].copy_from_slice(&pgc_data_off.to_be_bytes());

    // PGC body
    let pgc = pgcit_base + pgci_header + pgci_srp;
    buf[pgc + 2] = nr_programs;
    buf[pgc + 3] = nr_cells;
    buf[pgc + 4..pgc + 8].copy_from_slice(&pgc_time);

    // audio_control
    for (i, &ac) in audio_control.iter().enumerate() {
        let off = pgc + 12 + i * 2;
        buf[off..off + 2].copy_from_slice(&ac.to_be_bytes());
    }

    buf[pgc + 230..pgc + 232].copy_from_slice(&pgc_prog_offset.to_be_bytes());
    buf[pgc + 232..pgc + 234].copy_from_slice(&pgc_cell_offset.to_be_bytes());

    // Program map
    for i in 0..nr_programs {
        buf[pgc + usize::from(pgc_prog_offset) + usize::from(i)] = i + 1;
    }

    // Cell playback
    for (i, &(time, first, last)) in cells.iter().enumerate() {
        let cb = pgc + usize::from(pgc_cell_offset) + i * 24;
        buf[cb + 4..cb + 8].copy_from_slice(&time);
        buf[cb + 8..cb + 12].copy_from_slice(&first.to_be_bytes());
        buf[cb + 20..cb + 24].copy_from_slice(&last.to_be_bytes());
    }

    buf
}

/// Creates a synthetic `VIDEO_TS` directory with VMG and VTS IFO files.
fn setup_dvd(vmg_data: &[u8], vts_files: &[(u8, Vec<u8>)]) -> TempDir {
    let dir = TempDir::new().expect("should create temp dir");
    let video_ts = dir.path().join("VIDEO_TS");
    fs::create_dir_all(&video_ts).expect("should create VIDEO_TS");

    fs::write(video_ts.join("VIDEO_TS.IFO"), vmg_data).expect("should write VMG");
    for &(vts_nr, ref data) in vts_files {
        let filename = format!("VTS_{vts_nr:02}_0.IFO");
        fs::write(video_ts.join(filename), data).expect("should write VTS IFO");
    }

    dir
}

// ── DVD tests ───────────────────────────────────────────────────────────

#[test]
fn inspect_dvd_simple_movie() {
    // Single VTS, 1 PGC, 7 chapters — a DVD title pattern
    let time = bcd_time(1, 36, 50, 8, false);
    let mut ac = [0u16; 8];
    ac[0] = 0x8000;

    let vts = build_vts(
        0x4E00, // MPEG-2 NTSC 16:9 720×480
        &[(0, 1, *b"en")],
        time,
        &[
            (time, 0, 999),
            (time, 1000, 1999),
            (time, 2000, 2999),
            (time, 3000, 3999),
            (time, 4000, 4999),
            (time, 5000, 5999),
            (time, 6000, 6999),
        ],
        &[&[(1, 1), (1, 2), (1, 3), (1, 4), (1, 5), (1, 6), (1, 7)]],
        &ac,
    );
    let vmg = build_vmg(&[(1, 1, 7, 1)], 1);
    let dir = setup_dvd(&vmg, &[(1, vts)]);

    let output = Command::new(cli_bin())
        .args(["inspect", &dir.path().display().to_string()])
        .output()
        .expect("should run inspect");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "inspect should succeed: {stdout}");
    assert!(
        stdout.contains("Title 01"),
        "should show title 01: {stdout}"
    );
    assert!(stdout.contains("VTS 01"), "should show VTS 01: {stdout}");
    assert!(stdout.contains("7 ch"), "should show 7 chapters: {stdout}");
    assert!(stdout.contains('*'), "should mark main title: {stdout}");
}

#[test]
fn inspect_dvd_json_output() {
    let time = bcd_time(0, 30, 0, 0, false);
    let mut ac = [0u16; 8];
    ac[0] = 0x8000;

    let vts = build_vts(
        0x4E00,
        &[(0, 1, *b"en")],
        time,
        &[(time, 0, 999)],
        &[&[(1, 1)]],
        &ac,
    );
    let vmg = build_vmg(&[(1, 1, 1, 1)], 1);
    let dir = setup_dvd(&vmg, &[(1, vts)]);

    let output = Command::new(cli_bin())
        .args(["inspect", "--json", &dir.path().display().to_string()])
        .output()
        .expect("should run inspect --json");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "inspect --json should succeed: {stdout}"
    );

    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("output should be valid JSON");

    assert_eq!(json["format"], "dvd", "format should be dvd");
    assert_eq!(json["main_title"], 1, "main_title should be 1");
    assert_eq!(json["titles"][0]["number"], 1, "title number should be 1");
    assert_eq!(json["titles"][0]["chapters"], 1, "should have 1 chapter");
    assert_eq!(
        json["title_sets"][0]["number"], 1,
        "title set number should be 1"
    );
}

#[test]
fn inspect_dvd_missing_vmg() {
    let dir = TempDir::new().expect("should create temp dir");
    fs::create_dir_all(dir.path().join("VIDEO_TS")).expect("should create VIDEO_TS");

    let output = Command::new(cli_bin())
        .args(["inspect", &dir.path().display().to_string()])
        .output()
        .expect("should run inspect");

    assert!(
        !output.status.success(),
        "inspect should fail for DVD without VIDEO_TS.IFO"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("VIDEO_TS.IFO"),
        "error should mention missing IFO: {stderr}"
    );
}
