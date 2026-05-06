// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Disc analysis — detects disc format and dispatches to the appropriate parser.

pub mod bdmv;

use std::path::Path;

use thiserror::Error;

/// Errors from disc inspection.
#[derive(Debug, Error)]
pub enum InspectError {
    /// The path does not point to a recognised disc structure.
    #[error("unrecognised disc format at {path} (expected BDMV/ or VIDEO_TS/ directory)")]
    UnrecognisedFormat {
        /// The path that was inspected.
        path: String,
    },

    /// DVD format is not yet supported.
    #[error("DVD format detected but not yet supported")]
    DvdNotSupported,

    /// BDMV analysis failed.
    #[error(transparent)]
    Bdmv(#[from] bdmv::BdmvError),
}

/// Result of inspecting a disc.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "format")]
pub enum InspectResult {
    /// Blu-ray disc (BDMV structure).
    #[serde(rename = "bdmv")]
    Bdmv(bdmv::BdmvAnalysis),
}

impl std::fmt::Display for InspectResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bdmv(analysis) => write!(f, "{analysis}"),
        }
    }
}

/// Inspects a disc at the given path.
///
/// `path` should be a mounted ISO, extracted disc folder, or a path
/// containing a `BDMV/` or `VIDEO_TS/` directory.
///
/// # Errors
///
/// Returns [`InspectError`] if the format is unrecognised, unsupported,
/// or analysis fails.
pub fn inspect(path: &Path) -> Result<InspectResult, InspectError> {
    // Check for BDMV structure
    if path.join("BDMV").is_dir() || path.join("PLAYLIST").is_dir() {
        let analysis = bdmv::analyze(path)?;
        return Ok(InspectResult::Bdmv(analysis));
    }

    // Check for DVD structure
    if path.join("VIDEO_TS").is_dir() {
        return Err(InspectError::DvdNotSupported);
    }

    Err(InspectError::UnrecognisedFormat {
        path: path.display().to_string(),
    })
}
