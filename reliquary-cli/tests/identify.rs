// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! End-to-end tests for `reliquary identify`.
//!
//! These tests build synthetic disc structures with IG data, run the
//! `reliquary-cli` binary against them, and assert on the output.

#![allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
#![allow(
    clippy::cast_possible_truncation,
    reason = "test builder values are small known constants"
)]

use std::collections::HashSet;
use std::fs;
use std::io::Write as _;
use std::process::{Command, Stdio};

use tempfile::TempDir;

// ── Test helpers ──────────────────────────────────────────────────────

/// Returns the path to the built `reliquary-cli` binary.
fn cli_bin() -> std::path::PathBuf {
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
fn build_mpls(clip_id: &str, in_time: u32, out_time: u32, marks: &[(u16, u32)]) -> Vec<u8> {
    let mut buf = Vec::new();

    // Header
    buf.extend_from_slice(b"MPLS");
    buf.extend_from_slice(b"0200");
    buf.extend_from_slice(&[0u8; 4]); // playlist_offset placeholder
    buf.extend_from_slice(&[0u8; 4]); // mark_offset placeholder
    buf.extend_from_slice(&[0u8; 4]); // extension_offset
    buf.extend_from_slice(&[0u8; 20]); // reserved

    // AppInfoPlayList
    buf.extend_from_slice(&0x0000_000E_u32.to_be_bytes());
    buf.extend_from_slice(&[0u8; 14]);

    // Playlist section
    let playlist_offset = buf.len() as u32;
    buf[8..12].copy_from_slice(&playlist_offset.to_be_bytes());

    let item_data = build_play_item(clip_id, in_time, out_time);
    let item_length = item_data.len() as u16;
    let section_length = (6 + 2 + item_data.len()) as u32;
    buf.extend_from_slice(&section_length.to_be_bytes());
    buf.extend_from_slice(&[0u8; 2]); // reserved
    buf.extend_from_slice(&1u16.to_be_bytes()); // num_play_items
    buf.extend_from_slice(&0u16.to_be_bytes()); // num_sub_paths
    buf.extend_from_slice(&item_length.to_be_bytes());
    buf.extend_from_slice(&item_data);

    // Mark section
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

fn build_play_item(clip_id: &str, in_time: u32, out_time: u32) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut id = [b'0'; 5];
    for (i, b) in clip_id.bytes().take(5).enumerate() {
        id[i] = b;
    }
    buf.extend_from_slice(&id);
    buf.extend_from_slice(b"M2TS");
    buf.extend_from_slice(&1u16.to_be_bytes()); // connection_condition=1
    buf.push(0); // stc_id
    buf.extend_from_slice(&in_time.to_be_bytes());
    buf.extend_from_slice(&out_time.to_be_bytes());
    buf.extend_from_slice(&[0u8; 12]); // UO_mask + flags2 + still_mode + still_time

    // STN table: 1 video (H.264 1080p) + 1 audio (AC-3 2.0 eng)
    let mut entries = Vec::new();
    entries.push(3u8); // entry_length
    entries.push(1); // stream_type
    entries.extend_from_slice(&0x1011_u16.to_be_bytes());
    entries.push(2); // attrs_length
    entries.push(0x1b); // H.264
    entries.push((6 << 4) | 1); // 1080p + 23.976

    entries.push(3u8);
    entries.push(1);
    entries.extend_from_slice(&0x1100_u16.to_be_bytes());
    entries.push(5);
    entries.push(0x81); // AC-3
    entries.push((3 << 4) | 1); // stereo + 48kHz
    entries.extend_from_slice(b"eng");

    let table_length = 2 + 7 + 5 + entries.len();
    buf.extend_from_slice(&(table_length as u16).to_be_bytes());
    buf.extend_from_slice(&[0u8; 2]);
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

/// Builds a minimal CLPI binary.
fn build_clpi(application_type: u8, num_source_packets: u32, ig_pid: Option<u16>) -> Vec<u8> {
    let mut buf = Vec::new();

    // Header (40 bytes)
    buf.extend_from_slice(b"HDMV");
    buf.extend_from_slice(b"0200");
    buf.extend_from_slice(&[0u8; 4]); // sequence_info_addr
    buf.extend_from_slice(&[0u8; 4]); // program_info_addr
    buf.extend_from_slice(&[0u8; 4]); // cpi_addr
    buf.extend_from_slice(&[0u8; 4]); // clip_mark_addr
    buf.extend_from_slice(&[0u8; 4]); // ext_data_addr
    buf.extend_from_slice(&[0u8; 12]); // reserved

    // ClipInfo section
    let clip_info_length: u32 = 144;
    buf.extend_from_slice(&clip_info_length.to_be_bytes());
    buf.extend_from_slice(&[0u8; 2]);
    buf.push(1); // clip_stream_type
    buf.push(application_type);
    buf.extend_from_slice(&[0u8; 4]); // flags
    buf.extend_from_slice(&6_000_000_u32.to_be_bytes()); // ts_recording_rate
    buf.extend_from_slice(&num_source_packets.to_be_bytes());
    buf.extend_from_slice(&[0u8; 128]); // reserved

    // SequenceInfo
    let seq_info_addr = buf.len() as u32;
    buf[0x08..0x0C].copy_from_slice(&seq_info_addr.to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes());

    // ProgramInfo
    let program_info_addr = buf.len() as u32;
    buf[0x0C..0x10].copy_from_slice(&program_info_addr.to_be_bytes());

    let mut stream_data = Vec::new();
    if let Some(pid) = ig_pid {
        // IG stream entry
        stream_data.extend_from_slice(&pid.to_be_bytes());
        stream_data.push(4); // attrs_length
        stream_data.push(0x91); // coding_type = IG
        stream_data.extend_from_slice(b"eng");
    }

    let num_streams = u8::from(ig_pid.is_some());
    let program_body_len = 1 + 1 + 4 + 2 + 1 + 1 + stream_data.len();
    buf.extend_from_slice(&(program_body_len as u32).to_be_bytes());
    buf.push(0);
    buf.push(1); // num_programs
    buf.extend_from_slice(&0u32.to_be_bytes()); // spn_program_seq_start
    buf.extend_from_slice(&0x0100_u16.to_be_bytes()); // program_map_pid
    buf.push(num_streams);
    buf.push(0); // num_groups
    buf.extend_from_slice(&stream_data);

    buf
}

// ── IG binary builders ───────────────────────────────────────────────

/// Builds a complete m2ts file with IG data.
///
/// Creates an m2ts containing one PES packet on the given PID,
/// carrying a display set with a palette, objects, and buttons
/// with `PlayPl` commands for the given playlists.
fn build_ig_m2ts(ig_pid: u16, playlists: &[u16]) -> Vec<u8> {
    let ig_payload = build_ig_segments(playlists);
    let pes = build_pes_packet(&ig_payload);
    build_m2ts_from_pes(ig_pid, &pes)
}

/// Builds IG segment data: palette + objects + composition + end-of-display.
fn build_ig_segments(playlists: &[u16]) -> Vec<u8> {
    let mut data = Vec::new();

    // Palette segment (type=0x14)
    let palette_body = build_palette_body();
    data.push(0x14);
    data.extend_from_slice(&(palette_body.len() as u16).to_be_bytes());
    data.extend_from_slice(&palette_body);

    // Object segments (one per button)
    let mut seen_objects = HashSet::new();
    for (i, _) in playlists.iter().enumerate() {
        let object_id = (i + 1) as u16;
        if seen_objects.insert(object_id) {
            let obj_body = build_object_body(object_id);
            data.push(0x15);
            data.extend_from_slice(&(obj_body.len() as u16).to_be_bytes());
            data.extend_from_slice(&obj_body);
        }
    }

    // Composition segment (type=0x18)
    let comp_body = build_composition_body(playlists);
    data.push(0x18);
    data.extend_from_slice(&(comp_body.len() as u16).to_be_bytes());
    data.extend_from_slice(&comp_body);

    // End of display segment (type=0x80)
    data.extend_from_slice(&[0x80, 0x00, 0x00]);

    data
}

/// Builds a palette body with two entries: transparent black and opaque white.
fn build_palette_body() -> Vec<u8> {
    vec![
        0x00, 0x00, // palette_id=0, version=0
        // Entry 0: transparent black (Y=16, Cr=128, Cb=128, Alpha=0)
        0x00, 0x10, 0x80, 0x80, 0x00,
        // Entry 1: opaque white (Y=235, Cr=128, Cb=128, Alpha=255)
        0x01, 0xEB, 0x80, 0x80, 0xFF,
    ]
}

/// Builds an object body for a 2x1 pixel white bitmap.
fn build_object_body(object_id: u16) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&object_id.to_be_bytes());
    body.push(0x00); // version
    body.push(0xC0); // sequence_flag: first and last

    // object_data_length (u24) = width(2) + height(2) + rle(4) = 8
    body.extend_from_slice(&[0x00, 0x00, 0x08]);

    body.extend_from_slice(&2u16.to_be_bytes()); // width=2
    body.extend_from_slice(&1u16.to_be_bytes()); // height=1

    // RLE: two pixels of color 1 + end-of-line
    body.extend_from_slice(&[0x01, 0x01, 0x00, 0x00]);

    body
}

/// Builds an Interactive Composition body with one page containing buttons.
fn build_composition_body(playlists: &[u16]) -> Vec<u8> {
    let mut body = Vec::new();

    // Segment header (9 bytes)
    body.extend_from_slice(&1920u16.to_be_bytes()); // width
    body.extend_from_slice(&1080u16.to_be_bytes()); // height
    body.push(0x10); // frame_rate_reserved
    body.extend_from_slice(&0u16.to_be_bytes()); // composition_number
    body.push(0x00); // composition_state_reserved
    body.push(0xC0); // sequence_descriptor: first and last

    // IC body
    body.extend_from_slice(&[0x00, 0x00, 0x00]); // data_length (unused by parser)
    body.push(0x00); // model_byte: stream_model=0, ui_model=0

    // stream_model=0 fields
    body.push(0x00); // uo_mask_table_flag
    body.extend_from_slice(&[0x00; 5]); // composition_timeout_pts
    body.extend_from_slice(&[0x00; 5]); // selection_timeout_pts

    body.extend_from_slice(&[0x00, 0x00, 0x00]); // user_timeout_duration
    body.push(playlists.len() as u8); // num_pages — one page per button

    // One page per button (simpler than one page with multiple BOGs
    // since we just need each button to be discoverable)
    for (i, &playlist) in playlists.iter().enumerate() {
        let button_id = (i + 1) as u16;
        let object_id = (i + 1) as u16;

        // Page header
        body.push(i as u8); // page_id
        body.push(0x00); // page_version
        body.extend_from_slice(&[0x00; 8]); // UO_mask_table
        body.push(0x00); // in_effects: num_windows=0
        body.push(0x00); // in_effects: num_effects=0
        body.push(0x00); // out_effects: num_windows=0
        body.push(0x00); // out_effects: num_effects=0
        body.push(0x00); // animation_frame_rate_code
        body.extend_from_slice(&button_id.to_be_bytes()); // default_selected_button_id
        body.extend_from_slice(&0xFFFFu16.to_be_bytes()); // default_activated_button_id
        body.push(0x00); // palette_id
        body.push(1); // num_bogs

        // BOG
        body.extend_from_slice(&button_id.to_be_bytes()); // default_valid_button_id
        body.push(1); // num_buttons

        // Button
        body.extend_from_slice(&button_id.to_be_bytes()); // button_id
        body.extend_from_slice(&button_id.to_be_bytes()); // numeric_value
        body.push(0x00); // auto_action
        body.extend_from_slice(&0u16.to_be_bytes()); // x
        body.extend_from_slice(&0u16.to_be_bytes()); // y
        // Neighbors (up, down, left, right)
        body.extend_from_slice(&button_id.to_be_bytes());
        body.extend_from_slice(&button_id.to_be_bytes());
        body.extend_from_slice(&button_id.to_be_bytes());
        body.extend_from_slice(&button_id.to_be_bytes());
        // Normal state
        body.extend_from_slice(&object_id.to_be_bytes()); // normal_start_object_id
        body.extend_from_slice(&0xFFFFu16.to_be_bytes()); // normal_end_object_id
        body.push(0x00); // normal_repeat_reserved
        body.push(0xFF); // selected_sound_id_ref
        // Selected state
        body.extend_from_slice(&object_id.to_be_bytes()); // selected_start_object_id
        body.extend_from_slice(&0xFFFFu16.to_be_bytes()); // selected_end_object_id
        body.push(0x00); // selected_repeat_reserved
        body.push(0xFF); // activated_sound_id_ref
        // Activated state
        body.extend_from_slice(&0xFFFFu16.to_be_bytes()); // activated_start
        body.extend_from_slice(&0xFFFFu16.to_be_bytes()); // activated_end
        // Commands
        body.extend_from_slice(&1u16.to_be_bytes()); // num_commands=1
        // PlayPl command (12 bytes)
        // grp=0 (BRANCH), sub_grp=2 (PLAY), op_cnt=1, imm_op1=1
        body.extend_from_slice(&0x2280_0000u32.to_be_bytes()); // insn
        body.extend_from_slice(&u32::from(playlist).to_be_bytes()); // dst: playlist number
        body.extend_from_slice(&0u32.to_be_bytes()); // src: unused
    }

    body
}

/// Wraps IG payload in a PES packet.
fn build_pes_packet(ig_payload: &[u8]) -> Vec<u8> {
    let pes_data_len = 3 + ig_payload.len(); // flags1 + flags2 + header_data_length + payload
    let mut pes = Vec::new();
    pes.extend_from_slice(&[0x00, 0x00, 0x01]); // start code
    pes.push(0xBD); // stream_id: private_stream_1
    pes.extend_from_slice(&(pes_data_len as u16).to_be_bytes());
    pes.push(0x80); // flags1: marker bits
    pes.push(0x00); // flags2: no PTS
    pes.push(0x00); // header_data_length
    pes.extend_from_slice(ig_payload);
    pes
}

/// Wraps PES data into 192-byte m2ts packets for the given PID.
fn build_m2ts_from_pes(pid: u16, pes_data: &[u8]) -> Vec<u8> {
    const M2TS_PACKET_LEN: usize = 192;
    const TS_HEADER_LEN: usize = 4;
    const TP_EXTRA_LEN: usize = 4;
    const MAX_PAYLOAD: usize = M2TS_PACKET_LEN - TP_EXTRA_LEN - TS_HEADER_LEN;

    let mut result = Vec::new();
    let mut offset = 0;
    let mut cc: u8 = 0;
    let mut first = true;

    while offset < pes_data.len() {
        let remaining = pes_data.len() - offset;

        let (adaptation_field, payload_len) = if remaining >= MAX_PAYLOAD {
            (Vec::new(), MAX_PAYLOAD)
        } else {
            // Need adaptation field for stuffing
            let stuff_needed = MAX_PAYLOAD - remaining;
            let mut af = Vec::new();
            if stuff_needed == 1 {
                af.push(0x00); // length=0
            } else {
                af.push((stuff_needed - 1) as u8); // af_length
                af.push(0x00); // flags
                af.extend(std::iter::repeat_n(0xFF, stuff_needed.saturating_sub(2)));
            }
            (af, remaining)
        };

        let mut packet = vec![0u8; M2TS_PACKET_LEN];

        // TP_extra_header
        packet[0..4].copy_from_slice(&[0x00, 0x00, 0x00, 0x00]);

        // TS header
        packet[4] = 0x47; // sync
        let pusi_bit = if first { 0x40 } else { 0x00 };
        packet[5] = pusi_bit | ((pid >> 8) as u8 & 0x1F);
        packet[6] = (pid & 0xFF) as u8;

        let afc = if adaptation_field.is_empty() {
            0x01 // payload only
        } else {
            0x03 // adaptation + payload
        };
        packet[7] = (afc << 4) | (cc & 0x0F);

        let mut pos = 8;
        if !adaptation_field.is_empty() {
            packet[pos..pos + adaptation_field.len()].copy_from_slice(&adaptation_field);
            pos += adaptation_field.len();
        }

        packet[pos..pos + payload_len].copy_from_slice(&pes_data[offset..offset + payload_len]);

        result.extend_from_slice(&packet);
        offset += payload_len;
        cc = cc.wrapping_add(1);
        first = false;
    }

    result
}

/// Creates a synthetic BDMV directory with MPLS, CLPI, and optional m2ts.
fn setup_identify_disc(
    mpls_files: &[(u32, Vec<u8>)],
    clpi_files: &[(&str, Vec<u8>)],
    m2ts_files: &[(&str, Vec<u8>)],
) -> TempDir {
    let dir = TempDir::new().expect("should create temp dir");
    let playlist_dir = dir.path().join("BDMV").join("PLAYLIST");
    fs::create_dir_all(&playlist_dir).expect("should create PLAYLIST dir");

    for (number, data) in mpls_files {
        let filename = format!("{number:05}.mpls");
        fs::write(playlist_dir.join(filename), data).expect("should write MPLS");
    }

    if !clpi_files.is_empty() {
        let clipinf_dir = dir.path().join("BDMV").join("CLIPINF");
        fs::create_dir_all(&clipinf_dir).expect("should create CLIPINF dir");
        for (clip_id, data) in clpi_files {
            fs::write(clipinf_dir.join(format!("{clip_id}.clpi")), data)
                .expect("should write CLPI");
        }
    }

    if !m2ts_files.is_empty() {
        let stream_dir = dir.path().join("BDMV").join("STREAM");
        fs::create_dir_all(&stream_dir).expect("should create STREAM dir");
        for (clip_id, data) in m2ts_files {
            fs::write(stream_dir.join(format!("{clip_id}.m2ts")), data).expect("should write m2ts");
        }
    }

    dir
}

// ── Tests ─────────────────────────────────────────────────────────────

#[test]
fn identify_no_ig_clips() {
    // Disc with a content playlist but no IG clips
    let mpls = build_mpls("00100", 27_000_000, 59_040_000, &[(0, 27_000_000)]);
    let dir = setup_identify_disc(&[(203, mpls)], &[], &[]);

    let output = Command::new(cli_bin())
        .args(["identify", "--no-images"])
        .arg(dir.path())
        .output()
        .expect("should run CLI");

    assert!(
        !output.status.success(),
        "should fail when no IG clips found"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no IG clips found"),
        "should report no IG clips: {stderr}"
    );
}

#[test]
fn identify_full_pipeline_with_piped_input() {
    let ig_pid: u16 = 0x1200;

    // Content playlist 203 referencing clip 00100
    let mpls = build_mpls("00100", 27_000_000, 65_000_000, &[(0, 27_000_000)]);

    // IG clip 00061 with IG stream on PID 0x1200
    let clpi = build_clpi(1, 1, Some(ig_pid));

    // m2ts with one button: PlayPl → playlist 203
    let m2ts = build_ig_m2ts(ig_pid, &[203]);

    let dir = setup_identify_disc(&[(203, mpls)], &[("00061", clpi)], &[("00061", m2ts)]);

    let mut child = Command::new(cli_bin())
        .args(["identify", "--no-images"])
        .arg(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("should spawn CLI");

    // Provide a name via piped stdin
    child
        .stdin
        .take()
        .expect("should have stdin")
        .write_all(b"Test Feature\n")
        .expect("should write to stdin");

    let output = child.wait_with_output().expect("should wait for output");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "identify should succeed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    assert!(
        stdout.contains("playlist 203: Test Feature"),
        "should contain named playlist in output.\nstdout: {stdout}"
    );
}

#[test]
fn identify_skipped_entry() {
    let ig_pid: u16 = 0x1200;
    let mpls = build_mpls("00100", 27_000_000, 65_000_000, &[(0, 27_000_000)]);
    let clpi = build_clpi(1, 1, Some(ig_pid));
    let m2ts = build_ig_m2ts(ig_pid, &[203]);

    let dir = setup_identify_disc(&[(203, mpls)], &[("00061", clpi)], &[("00061", m2ts)]);

    let mut child = Command::new(cli_bin())
        .args(["identify", "--no-images"])
        .arg(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("should spawn CLI");

    // Empty line = skip
    child
        .stdin
        .take()
        .expect("should have stdin")
        .write_all(b"\n")
        .expect("should write to stdin");

    let output = child.wait_with_output().expect("should wait for output");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "identify should succeed with skipped entry"
    );
    assert!(
        stdout.contains("playlist 203: (skipped)"),
        "should mark skipped playlist.\nstdout: {stdout}"
    );
}

#[test]
fn identify_json_output() {
    let ig_pid: u16 = 0x1200;
    let mpls = build_mpls("00100", 27_000_000, 65_000_000, &[(0, 27_000_000)]);
    let clpi = build_clpi(1, 1, Some(ig_pid));
    let m2ts = build_ig_m2ts(ig_pid, &[203]);

    let dir = setup_identify_disc(&[(203, mpls)], &[("00061", clpi)], &[("00061", m2ts)]);

    let mut child = Command::new(cli_bin())
        .args(["identify", "--no-images", "--json"])
        .arg(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("should spawn CLI");

    child
        .stdin
        .take()
        .expect("should have stdin")
        .write_all(b"Beach Battle\n")
        .expect("should write to stdin");

    let output = child.wait_with_output().expect("should wait for output");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success(), "identify --json should succeed");

    let json: serde_json::Value = serde_json::from_str(&stdout).expect("should parse JSON output");

    let items = json["items"].as_array().expect("items should be an array");
    assert_eq!(items.len(), 1, "should have one item");
    assert_eq!(items[0]["playlist"], 203, "should target playlist 203");
    assert_eq!(items[0]["name"], "Beach Battle", "should have correct name");
}

#[test]
fn identify_multiple_buttons_deduplicates() {
    let ig_pid: u16 = 0x1200;

    // Two playlists
    let mpls_203 = build_mpls("00100", 27_000_000, 65_000_000, &[(0, 27_000_000)]);
    let mpls_204 = build_mpls("00101", 27_000_000, 50_000_000, &[(0, 27_000_000)]);

    let clpi = build_clpi(1, 2, Some(ig_pid));

    // Two buttons targeting different playlists
    let m2ts = build_ig_m2ts(ig_pid, &[203, 204]);

    let dir = setup_identify_disc(
        &[(203, mpls_203), (204, mpls_204)],
        &[("00061", clpi)],
        &[("00061", m2ts)],
    );

    let mut child = Command::new(cli_bin())
        .args(["identify", "--no-images"])
        .arg(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("should spawn CLI");

    child
        .stdin
        .take()
        .expect("should have stdin")
        .write_all(b"Hidden Island\nBeach Battle\n")
        .expect("should write to stdin");

    let output = child.wait_with_output().expect("should wait for output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "identify should succeed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    assert!(
        stdout.contains("playlist 203: Hidden Island"),
        "should contain first named playlist.\nstdout: {stdout}"
    );
    assert!(
        stdout.contains("playlist 204: Beach Battle"),
        "should contain second named playlist.\nstdout: {stdout}"
    );

    assert!(
        stderr.contains("found 2 buttons"),
        "should report button count.\nstderr: {stderr}"
    );
}
