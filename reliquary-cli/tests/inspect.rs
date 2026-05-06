// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! End-to-end tests for `reliquary inspect`.
//!
//! These tests build synthetic BDMV directory structures from binary MPLS
//! fixtures, run the `reliquary-cli` binary against them, and assert on
//! the exact stdout output.

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

#[test]
fn inspect_dvd_not_supported() {
    let dir = TempDir::new().expect("should create temp dir");
    fs::create_dir_all(dir.path().join("VIDEO_TS")).expect("should create VIDEO_TS");

    let output = Command::new(cli_bin())
        .args(["inspect", &dir.path().display().to_string()])
        .output()
        .expect("should run inspect");

    assert!(!output.status.success(), "inspect should fail for DVD");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("DVD") && stderr.contains("not yet supported"),
        "error should mention DVD not supported: {stderr}"
    );
}
