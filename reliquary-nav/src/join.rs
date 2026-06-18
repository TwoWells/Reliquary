// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! The navigate **join**: mapping an activated menu button to its output mkv.
//!
//! A content button activates a playlist; `reliquary-bluray` resolves that
//! playlist to its clip ids (the `.m2ts` base names). This module reconciles
//! those clip ids against the makemkvcon [`TitleIndex`] built from the same
//! source, recovering the [`TitleRecord`] — and therefore the output `.mkv`
//! makemkvcon would write.
//!
//! Two halves live here, both disc-free and unit-testable:
//!
//! - [`resolve`] — the pure match: clip id → `index.by_source_file("<id>.m2ts")`.
//! - [`source_for_path`] — the disc path → makemkvcon [`Source`] mapping, plus
//!   [`spawn_scan`], which runs `title_index` on a worker thread and reports the
//!   result over a channel so the UI thread is never blocked on makemkvcon.

use std::path::Path;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::thread;

use reliquary_makemkv::{DriverError, Source, TitleIndex, TitleRecord, title_index};

/// Resolve a playlist's clip ids to the makemkvcon title that ripped them.
///
/// libbluray yields clip ids without an extension (`"00292"`); the makemkvcon
/// index is keyed on the full source file name (`"00292.m2ts"`), so each id has
/// `.m2ts` appended before lookup. The first clip (`clip_ids[0]`) is tried
/// first — for a single-clip playlist that is the only candidate — and the
/// remaining clips are tried as a fallback for multi-clip playlists, returning
/// the first title that matches. Returns `None` if no clip resolves (or the
/// list is empty).
#[must_use]
pub fn resolve<'i>(clip_ids: &[String], index: &'i TitleIndex) -> Option<&'i TitleRecord> {
    clip_ids
        .iter()
        .find_map(|clip_id| index.by_source_file(&format!("{clip_id}.m2ts")))
}

/// Map a disc path to the makemkvcon [`Source`] that enumerates the same media.
///
/// - A **directory** is treated as a decrypted BDMV folder → [`Source::File`]
///   (makemkvcon's `file:` MRL accepts a decrypted backup folder).
/// - A **file** is treated as an ISO image → [`Source::Iso`] (makemkvcon's
///   `iso:` MRL).
///
/// Returns `None` if the path does not exist (neither a file nor a directory),
/// so the caller can skip the scan rather than hand makemkvcon a bad MRL.
///
/// A physical optical drive (`disc:N`) is intentionally **not** inferred from a
/// path: nav opens an ISO or a decrypted BDMV folder — the same inputs
/// libbluray opens. Driving an optical drive directly would need an explicit
/// drive index from the operator and is left as future work.
#[must_use]
pub fn source_for_path(path: &Path) -> Option<Source> {
    if path.is_dir() {
        Some(Source::File(path.into()))
    } else if path.is_file() {
        Some(Source::Iso(path.into()))
    } else {
        // TODO: a `disc:N` optical-drive source would need an explicit drive
        // index supplied by the operator; an arbitrary path cannot imply one.
        None
    }
}

/// Spawn a background worker that builds the makemkvcon [`TitleIndex`] for
/// `source`, returning the [`Receiver`] the result is delivered on.
///
/// The worker touches only the makemkvcon source MRL (a `Send` value); it never
/// sees the `!Send` libbluray `Disc`, which stays on the UI thread. The UI
/// thread polls the returned receiver with [`poll_scan`] each frame and stores
/// the index once it arrives. A scan that fails (makemkvcon missing, or a
/// non-zero exit) is delivered as an `Err` so the caller can log it and leave
/// the join unavailable without crashing.
#[must_use]
pub fn spawn_scan(source: Source) -> Receiver<Result<TitleIndex, DriverError>> {
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let result = title_index(&source);
        // The UI thread may have exited (window closed) before the scan
        // finished; a send failure then is expected and ignored.
        let _ = tx.send(result);
    });
    rx
}

/// The state of the background makemkvcon scan, as held by the UI.
///
/// Drives the lightweight status the navigator surfaces: "scanning titles…"
/// until the index arrives, then either a ready index or an unavailable join.
pub enum ScanState {
    /// The worker is still running `makemkvcon info`.
    Scanning(Receiver<Result<TitleIndex, DriverError>>),
    /// The index is built and the join is available.
    Ready(TitleIndex),
    /// The scan failed (e.g. makemkvcon is not installed); the join is
    /// unavailable, but navigation and crop still work.
    Unavailable,
}

impl ScanState {
    /// Begin scanning `path`'s media in the background, or report the join
    /// unavailable if the path maps to no makemkvcon source.
    #[must_use]
    pub fn start(path: &Path) -> Self {
        source_for_path(path).map_or_else(
            || {
                tracing::warn!(
                    path = %path.display(),
                    "no makemkvcon source for this path; the join is unavailable"
                );
                Self::Unavailable
            },
            |source| Self::Scanning(spawn_scan(source)),
        )
    }

    /// Poll the worker once (non-blocking). When the scan finishes, transitions
    /// `Scanning` to either `Ready` (logging the title count) or `Unavailable`
    /// (logging the driver error). A no-op in the `Ready`/`Unavailable` states.
    pub fn poll(&mut self) {
        let Self::Scanning(rx) = self else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(index)) => {
                tracing::info!(titles = index.len(), "makemkvcon title index ready");
                *self = Self::Ready(index);
            }
            Ok(Err(err)) => {
                tracing::warn!(%err, "makemkvcon scan failed; the join is unavailable");
                *self = Self::Unavailable;
            }
            Err(TryRecvError::Empty) => {
                // Still scanning — nothing to do this frame.
            }
            Err(TryRecvError::Disconnected) => {
                tracing::warn!(
                    "makemkvcon scan worker disconnected without a result; \
                     the join is unavailable"
                );
                *self = Self::Unavailable;
            }
        }
    }

    /// The built index if the scan has finished successfully, else `None`.
    #[must_use]
    pub const fn index(&self) -> Option<&TitleIndex> {
        match self {
            Self::Ready(index) => Some(index),
            Self::Scanning(_) | Self::Unavailable => None,
        }
    }

    /// Whether the scan is still in progress.
    #[must_use]
    pub const fn is_scanning(&self) -> bool {
        matches!(self, Self::Scanning(_))
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod tests {
    use std::path::Path;

    use reliquary_makemkv::{Source, parse};

    use super::{resolve, source_for_path};

    /// A generic two-title fixture: a feature on `00800.m2ts` and a bare-`.m2ts`
    /// extra on `00292.m2ts`. No real disc/movie titles (public repo).
    const FIXTURE: &str = r#"TINFO:0,2,0,"Main Feature"
TINFO:0,9,0,"1:42:18"
TINFO:0,16,0,"00800.m2ts"
TINFO:0,27,0,"Feature_t00.mkv"
TINFO:1,2,0,"Bonus"
TINFO:1,9,0,"0:02:41"
TINFO:1,16,0,"00292.m2ts"
TINFO:1,27,0,"Extra_t01.mkv"
"#;

    #[test]
    fn resolves_first_clip_hit() {
        let index = parse(FIXTURE).expect("fixture should parse");
        // A single-clip playlist whose clip is the feature's source.
        let clips = vec!["00800".to_string()];
        let title = resolve(&clips, &index).expect("clips[0] should resolve to the feature title");
        assert_eq!(
            title.output_file_name.as_deref(),
            Some("Feature_t00.mkv"),
            "clips[0] match should resolve to the feature's output mkv"
        );
    }

    #[test]
    fn appends_m2ts_before_lookup() {
        let index = parse(FIXTURE).expect("fixture should parse");
        // The clip id arrives without an extension; resolve must append `.m2ts`.
        let clips = vec!["00292".to_string()];
        let title = resolve(&clips, &index)
            .expect("a bare clip id should resolve once `.m2ts` is appended");
        assert_eq!(
            title.title_id, 1,
            "the bare `00292` id should resolve to title 1 via `00292.m2ts`"
        );
    }

    #[test]
    fn falls_back_to_a_later_clip() {
        let index = parse(FIXTURE).expect("fixture should parse");
        // clips[0] is unknown to the index; the second clip is the real match.
        let clips = vec!["99999".to_string(), "00292".to_string()];
        let title = resolve(&clips, &index)
            .expect("a later clip should resolve when clips[0] does not match");
        assert_eq!(
            title.output_file_name.as_deref(),
            Some("Extra_t01.mkv"),
            "the fallback clip should resolve to its output mkv"
        );
    }

    #[test]
    fn no_match_returns_none() {
        let index = parse(FIXTURE).expect("fixture should parse");
        let clips = vec!["12345".to_string(), "67890".to_string()];
        assert!(
            resolve(&clips, &index).is_none(),
            "clip ids absent from the index should resolve to None"
        );
    }

    #[test]
    fn empty_clip_list_returns_none() {
        let index = parse(FIXTURE).expect("fixture should parse");
        let clips: Vec<String> = Vec::new();
        assert!(
            resolve(&clips, &index).is_none(),
            "an empty clip list should resolve to None"
        );
    }

    #[test]
    fn empty_index_returns_none() {
        let index = parse("").expect("empty input should parse to an empty index");
        let clips = vec!["00292".to_string()];
        assert!(
            resolve(&clips, &index).is_none(),
            "no clip can resolve against an empty index"
        );
    }

    #[test]
    fn directory_path_maps_to_file_source() {
        // A directory models a decrypted BDMV folder → file: MRL. Use the
        // crate's own manifest dir, which is guaranteed to exist as a directory.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let source = source_for_path(dir).expect("an existing directory should map to a source");
        assert!(
            matches!(source, Source::File(_)),
            "a directory should map to Source::File, got {source:?}"
        );
    }

    #[test]
    fn file_path_maps_to_iso_source() {
        // A regular file models an ISO image → iso: MRL. This crate's own
        // Cargo.toml is guaranteed to exist as a regular file; build its
        // absolute path from the manifest dir so the test is CWD-independent.
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let file = manifest.join("Cargo.toml");
        let source = source_for_path(&file).expect("an existing file should map to a source");
        assert!(
            matches!(source, Source::Iso(_)),
            "a file should map to Source::Iso, got {source:?}"
        );
    }

    #[test]
    fn nonexistent_path_maps_to_no_source() {
        let missing = Path::new("/nonexistent/reliquary-nav/no-such-disc");
        assert!(
            source_for_path(missing).is_none(),
            "a path that is neither a file nor a directory should map to no source"
        );
    }
}
