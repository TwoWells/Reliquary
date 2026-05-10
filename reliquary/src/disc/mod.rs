// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Disc analysis — detects disc format and dispatches to the appropriate parser.

pub mod bdmv;
pub mod dvd;
pub mod reader;

use std::path::Path;

use thiserror::Error;

use reader::{DiscReader, ReaderError};

/// Errors from disc inspection.
#[derive(Debug, Error)]
pub enum InspectError {
    /// The path does not point to a recognised disc structure.
    #[error("unrecognised disc format at {path} (expected BDMV/ or VIDEO_TS/ directory)")]
    UnrecognisedFormat {
        /// The path that was inspected.
        path: String,
    },

    /// BDMV analysis failed.
    #[error(transparent)]
    Bdmv(#[from] bdmv::BdmvError),

    /// DVD analysis failed.
    #[error(transparent)]
    Dvd(#[from] dvd::DvdError),

    /// Failed to open the disc reader.
    #[error(transparent)]
    Reader(#[from] ReaderError),
}

/// Result of inspecting a disc.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "format")]
pub enum InspectResult {
    /// Blu-ray disc (BDMV structure).
    #[serde(rename = "bdmv")]
    Bdmv(bdmv::BdmvAnalysis),
    /// DVD disc (`VIDEO_TS` structure).
    #[serde(rename = "dvd")]
    Dvd(dvd::DvdAnalysis),
}

impl std::fmt::Display for InspectResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bdmv(analysis) => write!(f, "{analysis}"),
            Self::Dvd(analysis) => write!(f, "{analysis}"),
        }
    }
}

/// Inspects a disc at the given path.
///
/// `path` can be a mounted ISO, extracted disc folder, or an ISO image
/// file. The reader auto-detects the source and provides unified file
/// access to the analysis pipeline.
///
/// # Errors
///
/// Returns [`InspectError`] if the format is unrecognised, unsupported,
/// or analysis fails.
pub fn inspect(path: &Path) -> Result<InspectResult, InspectError> {
    let reader = DiscReader::open(path)?;
    inspect_reader(&reader, path)
}

/// Inspects a disc using an already-opened reader.
fn inspect_reader(reader: &DiscReader, path: &Path) -> Result<InspectResult, InspectError> {
    // Check for BDMV structure by probing through the reader.
    let has_bdmv = reader.read_dir(Path::new("BDMV")).is_ok();
    let has_playlist = reader.read_dir(Path::new("PLAYLIST")).is_ok();

    if has_bdmv || has_playlist {
        let analysis = bdmv::analyze(reader)?;
        return Ok(InspectResult::Bdmv(analysis));
    }

    // Check for DVD structure
    if reader.read_dir(Path::new("VIDEO_TS")).is_ok() {
        let analysis = dvd::analyze(reader)?;
        return Ok(InspectResult::Dvd(analysis));
    }

    Err(InspectError::UnrecognisedFormat {
        path: path.display().to_string(),
    })
}
