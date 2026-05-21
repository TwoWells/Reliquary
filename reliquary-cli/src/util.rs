// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Shared utilities used across CLI subcommands.

/// Returns the terminal width in columns.
pub fn terminal_columns() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(80)
        .saturating_sub(4) // leave room for "  " indent and margin
}

/// Formats a `Duration` as `H:MM:SS` for identify output.
pub fn format_identify_duration(d: std::time::Duration) -> String {
    let total_secs = d.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    format!("{hours}:{minutes:02}:{seconds:02}")
}

/// Formats a `StreamSummary` as a compact description for identify output.
pub fn format_identify_streams(s: &reliquary::disc::bdmv::StreamSummary) -> String {
    let mut parts = Vec::new();
    for v in &s.video {
        parts.push(v.clone());
    }
    if let Some(a) = s.audio.first() {
        parts.push(a.clone());
    }
    parts.join("  ")
}

/// Formats a byte count as a human-readable string.
#[allow(
    clippy::cast_precision_loss,
    reason = "file sizes fit within f64 precision for any real disc"
)]
pub fn format_size(bytes: u64) -> String {
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

/// Parses a 32-character hex string into a 16-byte VUK.
pub fn parse_vuk(hex: &str) -> Result<[u8; 16], String> {
    let hex = hex.strip_prefix("0x").unwrap_or(hex);

    if hex.len() != 32 {
        return Err(format!("VUK must be 32 hex characters, got {}", hex.len()));
    }

    let mut vuk = [0u8; 16];
    for (i, byte) in vuk.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|_| format!("invalid hex at position {}", i * 2))?;
    }
    Ok(vuk)
}
