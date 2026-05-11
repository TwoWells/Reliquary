// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Validates MKV output against mkvinfo and ffprobe.
//!
//! These tests write a synthetic MKV to a temp file and run external
//! tools to verify the container structure.  Skipped when the tools
//! are not installed.

#![allow(
    clippy::expect_used,
    reason = "integration tests use expect() per project rules"
)]

use std::io::Cursor;
use std::process::Command;

use reliquary::matroska::mux::{
    AudioTrack, Chapter, ContentTag, Frame, MkvMuxer, SegmentInfo, SubtitleTrack, TrackSpec,
    VideoColour, VideoTrack,
};

/// Returns `true` if the given binary is on `$PATH`.
fn tool_available(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Builds a minimal but structurally complete MKV in memory.
fn build_test_mkv() -> Vec<u8> {
    let info = SegmentInfo {
        duration_ns: Some(2_000_000_000), // 2 seconds
        title: Some("Validation Test".to_string()),
        segment_uid: Some([
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
            0x0F, 0x10,
        ]),
    };

    let mut muxer =
        MkvMuxer::new(Cursor::new(Vec::new()), &info, true, true).expect("create muxer");

    let tracks = [
        TrackSpec::Video(VideoTrack {
            codec_id: "V_MPEG2",
            codec_private: None,
            pixel_width: 720,
            pixel_height: 480,
            display_width: None,
            display_height: None,
            default_duration_ns: Some(33_366_666), // 29.97 fps
            interlaced: Some(true),
            name: Some("Main Video".to_string()),
            colour: None,
        }),
        TrackSpec::Audio(AudioTrack {
            codec_id: "A_AC3",
            codec_private: None,
            sampling_frequency: 48000.0,
            channels: 6,
            bit_depth: None,
            language: "eng".to_string(),
            name: Some("Surround Sound".to_string()),
            is_default: true,
        }),
    ];
    muxer.add_tracks(&tracks).expect("add tracks");

    let chapters = [
        Chapter {
            start_ns: 0,
            end_ns: None,
            title: "Opening".to_string(),
            language: "eng".to_string(),
        },
        Chapter {
            start_ns: 1_000_000_000,
            end_ns: None,
            title: "Main Feature".to_string(),
            language: "eng".to_string(),
        },
    ];
    muxer.write_chapters(&chapters).expect("write chapters");

    let tags = [ContentTag {
        name: "TITLE".to_string(),
        value: "Behind the Scenes - Themyscira".to_string(),
    }];
    muxer.write_tags(&tags).expect("write tags");

    // Two clusters with synthetic frame data.
    muxer.start_cluster(0).expect("start cluster 0");
    muxer
        .write_frame(&Frame {
            track: 1,
            timestamp_ms: 0,
            data: &[0x00; 512],
            keyframe: true,
            discardable: false,
            duration_ms: None,
        })
        .expect("video keyframe 0");
    muxer
        .write_frame(&Frame {
            track: 2,
            timestamp_ms: 0,
            data: &[0x00; 128],
            keyframe: true,
            discardable: false,
            duration_ms: None,
        })
        .expect("audio frame 0");

    muxer.start_cluster(1000).expect("start cluster 1000");
    muxer
        .write_frame(&Frame {
            track: 1,
            timestamp_ms: 1000,
            data: &[0x00; 512],
            keyframe: true,
            discardable: false,
            duration_ms: None,
        })
        .expect("video keyframe 1000");
    muxer
        .write_frame(&Frame {
            track: 2,
            timestamp_ms: 1000,
            data: &[0x00; 128],
            keyframe: true,
            discardable: false,
            duration_ms: None,
        })
        .expect("audio frame 1000");

    muxer.finalize().expect("finalize").into_inner()
}

/// Writes the test MKV to a temp file and returns its path.
fn write_temp_mkv() -> tempfile::NamedTempFile {
    let data = build_test_mkv();
    let mut tmp = tempfile::Builder::new()
        .suffix(".mkv")
        .tempfile()
        .expect("create temp file");
    std::io::Write::write_all(&mut tmp, &data).expect("write MKV data");
    tmp
}

#[test]
fn mkvinfo_validates_structure() {
    if !tool_available("mkvinfo") {
        return;
    }

    let tmp = write_temp_mkv();
    let output = Command::new("mkvinfo")
        .arg(tmp.path())
        .output()
        .expect("run mkvinfo");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "mkvinfo should exit successfully\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // Segment info.
    assert!(
        stdout.contains("Segment information"),
        "should report segment information:\n{stdout}"
    );
    assert!(
        stdout.contains("Validation Test"),
        "should contain segment title:\n{stdout}"
    );

    // Tracks.
    assert!(
        stdout.contains("V_MPEG2"),
        "should contain video codec ID:\n{stdout}"
    );
    assert!(
        stdout.contains("A_AC3"),
        "should contain audio codec ID:\n{stdout}"
    );

    // Chapters.
    assert!(
        stdout.contains("Chapters"),
        "should contain chapters section:\n{stdout}"
    );
    assert!(
        stdout.contains("Opening"),
        "should contain first chapter title:\n{stdout}"
    );
    assert!(
        stdout.contains("Main Feature"),
        "should contain second chapter title:\n{stdout}"
    );

    // Tags.
    assert!(
        stdout.contains("Tags"),
        "should contain tags section:\n{stdout}"
    );
    assert!(
        stdout.contains("Behind the Scenes - Themyscira"),
        "should contain title tag value:\n{stdout}"
    );

    // Clusters.
    assert!(
        stdout.contains("Cluster"),
        "should contain cluster data:\n{stdout}"
    );
}

#[test]
fn ffprobe_validates_structure() {
    if !tool_available("ffprobe") {
        return;
    }

    let tmp = write_temp_mkv();
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_format",
            "-show_streams",
            "-show_chapters",
        ])
        .arg(tmp.path())
        .output()
        .expect("run ffprobe");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "ffprobe should exit successfully\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // No error-level messages on stderr.
    assert!(
        stderr.is_empty(),
        "ffprobe should produce no errors:\n{stderr}"
    );

    // Format.
    assert!(
        stdout.contains("format_name=matroska"),
        "should identify as matroska:\n{stdout}"
    );
    assert!(
        stdout.contains("Behind the Scenes - Themyscira"),
        "should contain title tag:\n{stdout}"
    );

    // Streams — should have 2 (video + audio).
    let stream_count = stdout.matches("[STREAM]").count();
    assert_eq!(stream_count, 2, "should have 2 streams:\n{stdout}");

    // Chapters — should have 2.
    let chapter_count = stdout.matches("[CHAPTER]").count();
    assert_eq!(chapter_count, 2, "should have 2 chapters:\n{stdout}");

    // Chapter titles.
    assert!(
        stdout.contains("Opening"),
        "should contain first chapter title:\n{stdout}"
    );
    assert!(
        stdout.contains("Main Feature"),
        "should contain second chapter title:\n{stdout}"
    );
}

// ---------------------------------------------------------------------------
// Feature-rich MKV — exercises tickets 01–04
// ---------------------------------------------------------------------------

/// Builds an MKV exercising track features from tickets 01–04:
/// display aspect ratio, HDR colour, multiple audio tracks with
/// non-default flags, forced subtitle with `BlockGroup`/`BlockDuration`,
/// and multi-cluster `PrevSize`.
fn full_feature_tracks() -> Vec<TrackSpec> {
    vec![
        // Video: 1920x1080 with display AR, colour metadata, progressive.
        // Uses V_MPEG2 (not HEVC) because ffprobe's extract_extradata
        // BSF rejects all-zero synthetic data as invalid HEVC NAL units.
        TrackSpec::Video(VideoTrack {
            codec_id: "V_MPEG2",
            codec_private: None,
            pixel_width: 1920,
            pixel_height: 1080,
            display_width: Some(1920),
            display_height: Some(1080),
            default_duration_ns: Some(41_708_333), // 23.976 fps
            interlaced: Some(false),
            name: Some("Main Video".to_string()),
            colour: Some(VideoColour {
                matrix_coefficients: 9,       // BT.2020 NCL
                transfer_characteristics: 16, // PQ (HDR10)
                primaries: 9,                 // BT.2020
                range: 1,                     // broadcast/limited
                bits_per_channel: 10,
            }),
        }),
        // Default English audio.
        TrackSpec::Audio(AudioTrack {
            codec_id: "A_AC3",
            codec_private: None,
            sampling_frequency: 48000.0,
            channels: 6,
            bit_depth: None,
            language: "eng".to_string(),
            name: Some("English 5.1".to_string()),
            is_default: true,
        }),
        // Non-default French audio.
        TrackSpec::Audio(AudioTrack {
            codec_id: "A_AC3",
            codec_private: None,
            sampling_frequency: 48000.0,
            channels: 2,
            bit_depth: None,
            language: "fra".to_string(),
            name: Some("French Stereo".to_string()),
            is_default: false,
        }),
        // Forced English subtitle.
        TrackSpec::Subtitle(SubtitleTrack {
            codec_id: "S_HDMV/PGS",
            codec_private: None,
            language: "eng".to_string(),
            name: Some("Forced Signs".to_string()),
            is_default: false,
            is_forced: true,
        }),
    ]
}

fn write_full_feature_clusters(muxer: &mut MkvMuxer<Cursor<Vec<u8>>>, sub_track: u32) {
    // Cluster 0: video keyframe + both audio tracks + subtitle BlockGroup.
    muxer.start_cluster(0).expect("start cluster 0");
    for (track, size) in [(1, 1024), (2, 256), (3, 256)] {
        muxer
            .write_frame(&Frame {
                track,
                timestamp_ms: 0,
                data: &vec![0x00; size],
                keyframe: true,
                discardable: false,
                duration_ms: None,
            })
            .expect("frame at 0");
    }
    // Subtitle with BlockDuration (ticket 04 BlockGroup).
    muxer
        .write_frame(&Frame {
            track: sub_track,
            timestamp_ms: 0,
            data: &[0x00; 64],
            keyframe: true,
            discardable: false,
            duration_ms: Some(2000),
        })
        .expect("subtitle blockgroup 0");

    // Clusters 1000 and 2000: keyframes that test `PrevSize` chaining.
    for ts in [1000, 2000] {
        muxer.start_cluster(ts).expect("start cluster");
        muxer
            .write_frame(&Frame {
                track: 1,
                timestamp_ms: ts,
                data: &[0x00; 1024],
                keyframe: true,
                discardable: false,
                duration_ms: None,
            })
            .expect("video keyframe");
    }
}

fn build_full_feature_mkv() -> Vec<u8> {
    let info = SegmentInfo {
        duration_ns: Some(3_000_000_000),
        title: Some("Full Feature Test".to_string()),
        segment_uid: Some([0xAA; 16]),
    };

    let mut muxer =
        MkvMuxer::new(Cursor::new(Vec::new()), &info, true, true).expect("create muxer");

    let tracks = full_feature_tracks();
    let assigned = muxer.add_tracks(&tracks).expect("add tracks");
    let sub_track = assigned[3];

    let chapters = [Chapter {
        start_ns: 0,
        end_ns: Some(3_000_000_000),
        title: "Full Chapter".to_string(),
        language: "eng".to_string(),
    }];
    muxer.write_chapters(&chapters).expect("write chapters");

    let tags = [ContentTag {
        name: "TITLE".to_string(),
        value: "Full Feature Test".to_string(),
    }];
    muxer.write_tags(&tags).expect("write tags");

    write_full_feature_clusters(&mut muxer, sub_track);

    muxer.finalize().expect("finalize").into_inner()
}

fn write_full_feature_mkv() -> tempfile::NamedTempFile {
    let data = build_full_feature_mkv();
    let mut tmp = tempfile::Builder::new()
        .suffix(".mkv")
        .tempfile()
        .expect("create temp file");
    std::io::Write::write_all(&mut tmp, &data).expect("write MKV data");
    tmp
}

#[test]
fn mkvinfo_validates_full_features() {
    if !tool_available("mkvinfo") {
        return;
    }

    let tmp = write_full_feature_mkv();
    let output = Command::new("mkvinfo")
        .arg(tmp.path())
        .output()
        .expect("run mkvinfo");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "mkvinfo should exit successfully\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // Segment info (ticket 02).
    assert!(
        stdout.contains("Segment information"),
        "should report segment info:\n{stdout}"
    );
    assert!(
        stdout.contains("Full Feature Test"),
        "should contain segment title:\n{stdout}"
    );

    // Video track (ticket 03).
    assert!(
        stdout.contains("V_MPEG2"),
        "should contain MPEG2 codec ID:\n{stdout}"
    );
    assert!(
        stdout.contains("1920"),
        "should contain pixel width:\n{stdout}"
    );
    assert!(
        stdout.contains("1080"),
        "should contain pixel height:\n{stdout}"
    );
    assert!(
        stdout.contains("Main Video"),
        "should contain video track name:\n{stdout}"
    );

    // Colour metadata (ticket 03).
    assert!(
        stdout.contains("color information"),
        "should contain colour section:\n{stdout}"
    );

    // Multiple audio tracks (ticket 03).
    assert!(
        stdout.contains("English 5.1"),
        "should contain default audio track name:\n{stdout}"
    );
    assert!(
        stdout.contains("French Stereo"),
        "should contain non-default audio track name:\n{stdout}"
    );
    assert!(
        stdout.contains("A_AC3"),
        "should contain AC3 codec ID:\n{stdout}"
    );

    // Subtitle track (ticket 03).
    assert!(
        stdout.contains("S_HDMV/PGS"),
        "should contain PGS subtitle codec:\n{stdout}"
    );
    assert!(
        stdout.contains("Forced Signs"),
        "should contain subtitle track name:\n{stdout}"
    );
    assert!(
        stdout.contains("\"Forced display\" flag: 1"),
        "should report forced flag:\n{stdout}"
    );

    // Clusters present (ticket 04).
    assert!(
        stdout.contains("Cluster"),
        "should contain cluster data:\n{stdout}"
    );

    // Chapters (ticket 05).
    assert!(
        stdout.contains("Full Chapter"),
        "should contain chapter title:\n{stdout}"
    );
}

fn assert_ffprobe_streams(stdout: &str) {
    // 4 streams: video + 2 audio + subtitle.
    let stream_count = stdout.matches("[STREAM]").count();
    assert_eq!(stream_count, 4, "should have 4 streams:\n{stdout}");

    // Video stream details (ticket 03).
    assert!(
        stdout.contains("codec_name=mpeg2video"),
        "should identify MPEG2 codec:\n{stdout}"
    );
    assert!(
        stdout.contains("width=1920"),
        "should report video width:\n{stdout}"
    );
    assert!(
        stdout.contains("height=1080"),
        "should report video height:\n{stdout}"
    );

    // Audio stream details (ticket 03).
    assert!(
        stdout.contains("codec_name=ac3"),
        "should identify AC3 codec:\n{stdout}"
    );
    assert!(
        stdout.contains("sample_rate=48000"),
        "should report audio sample rate:\n{stdout}"
    );
    assert!(
        stdout.contains("channels=6"),
        "should report 5.1 channel count:\n{stdout}"
    );
    assert!(
        stdout.contains("channels=2"),
        "should report stereo channel count:\n{stdout}"
    );

    // Language tags (ticket 03).
    assert!(
        stdout.contains("TAG:language=eng"),
        "should report English language:\n{stdout}"
    );
    assert!(
        stdout.contains("TAG:language=fra"),
        "should report French language:\n{stdout}"
    );

    // Track names (ticket 03).
    assert!(
        stdout.contains("TAG:title=English 5.1"),
        "should report default audio track name:\n{stdout}"
    );
    assert!(
        stdout.contains("TAG:title=French Stereo"),
        "should report non-default audio track name:\n{stdout}"
    );

    // Subtitle stream (ticket 03).
    assert!(
        stdout.contains("codec_name=hdmv_pgs_subtitle"),
        "should identify PGS subtitle codec:\n{stdout}"
    );
    // Forced disposition (ticket 03).
    assert!(
        stdout.contains("DISPOSITION:forced=1"),
        "should report forced subtitle disposition:\n{stdout}"
    );

    // Colour metadata (ticket 03).
    assert!(
        stdout.contains("color_transfer=smpte2084"),
        "should report PQ transfer:\n{stdout}"
    );
    assert!(
        stdout.contains("color_primaries=bt2020"),
        "should report BT.2020 primaries:\n{stdout}"
    );
}

#[test]
fn ffprobe_validates_full_features() {
    if !tool_available("ffprobe") {
        return;
    }

    let tmp = write_full_feature_mkv();
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_format",
            "-show_streams",
            "-show_chapters",
        ])
        .arg(tmp.path())
        .output()
        .expect("run ffprobe");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "ffprobe should exit successfully\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // Synthetic zero-fill frame data triggers codec-level parsing
    // warnings (e.g. PGS subtitle parser).  Filter those out —
    // only fail on actual container errors.
    let has_container_errors = stderr.lines().any(|line| !line.contains("[pgssub"));
    assert!(
        !has_container_errors,
        "ffprobe should produce no container errors:\n{stderr}"
    );

    assert_ffprobe_streams(&stdout);

    // Chapter (ticket 05).
    let chapter_count = stdout.matches("[CHAPTER]").count();
    assert_eq!(chapter_count, 1, "should have 1 chapter:\n{stdout}");
    assert!(
        stdout.contains("Full Chapter"),
        "should contain chapter title:\n{stdout}"
    );

    // Format-level metadata (ticket 02 + 05).
    assert!(
        stdout.contains("format_name=matroska"),
        "should identify as matroska:\n{stdout}"
    );
    assert!(
        stdout.contains("TAG:TITLE=Full Feature Test"),
        "should contain title tag:\n{stdout}"
    );
}
