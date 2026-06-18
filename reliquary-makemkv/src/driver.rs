// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Thin subprocess driver around makemkvcon.
//!
//! Runs `makemkvcon -r --minlength=0 info <source>`, captures its stdout, and
//! hands the text to the [pure parser](crate::parse::parse) to build a
//! [`TitleIndex`]. The driver performs the I/O; all the format knowledge lives
//! in the parser, which is exercised by fixtures rather than a live tool.
//!
//! makemkvcon requires a registered key and media, so the live path is not
//! available in CI; see the ignored, env-gated integration test.

use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

use thiserror::Error;

use crate::parse::{ParseError, TitleIndex, parse};

/// The default makemkvcon executable name, resolved via `PATH`.
const DEFAULT_PROGRAM: &str = "makemkvcon";

/// A makemkvcon source MRL — the `<source>` argument to `info`.
///
/// makemkvcon accepts three source forms: an optical drive index, an ISO image
/// file, or a file/decrypted-backup folder. This enum renders each to the
/// `disc:N` / `iso:/path` / `file:/path` string makemkvcon expects.
#[derive(Debug, Clone)]
pub enum Source {
    /// An optical drive by its makemkvcon index — renders to `disc:N`.
    Disc(u32),
    /// An ISO image file — renders to `iso:/path.iso`.
    Iso(Box<Path>),
    /// A file or a decrypted backup folder — renders to `file:/path`.
    File(Box<Path>),
}

impl Source {
    /// Renders the source to its makemkvcon MRL string.
    ///
    /// For [`Self::Iso`] / [`Self::File`] the path is rendered with
    /// [`Path::display`]; a path whose bytes are not valid UTF-8 is rendered
    /// lossily, which is acceptable for the MRL but cannot round-trip exotic
    /// paths.
    #[must_use]
    fn to_mrl(&self) -> String {
        match self {
            Self::Disc(n) => format!("disc:{n}"),
            Self::Iso(path) => format!("iso:{}", path.display()),
            Self::File(path) => format!("file:{}", path.display()),
        }
    }
}

/// Errors from driving makemkvcon and parsing its output.
#[derive(Debug, Error)]
pub enum DriverError {
    /// Spawning the makemkvcon process failed (e.g. the binary is not on
    /// `PATH`).
    #[error("failed to spawn makemkvcon ({program}): {source}")]
    Spawn {
        /// The program name that could not be spawned.
        program: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// makemkvcon exited with a non-zero status. Carries the status text and
    /// the captured stderr (lossily decoded) for diagnosis.
    #[error("makemkvcon exited with {status}: {stderr}")]
    NonZeroExit {
        /// The process exit status, formatted.
        status: String,
        /// The captured standard error, lossily decoded.
        stderr: String,
    },

    /// makemkvcon's stdout was not valid UTF-8 and could not be parsed.
    #[error("makemkvcon produced non-UTF-8 output")]
    NonUtf8Output(#[source] std::string::FromUtf8Error),

    /// The captured output could not be parsed into a title index.
    #[error(transparent)]
    Parse(#[from] ParseError),
}

/// Drives makemkvcon to produce a [`TitleIndex`] for `source`.
///
/// Runs `makemkvcon -r --minlength=0 info <source>` (the `makemkvcon` binary
/// resolved via `PATH`), captures stdout, and parses it. `--minlength=0` keeps
/// short extras and trailers that makemkvcon would otherwise drop.
///
/// # Errors
///
/// Returns [`DriverError`] if the process cannot be spawned
/// ([`DriverError::Spawn`]), exits non-zero ([`DriverError::NonZeroExit`]),
/// produces non-UTF-8 stdout ([`DriverError::NonUtf8Output`]), or its output
/// fails to parse ([`DriverError::Parse`]).
pub fn title_index(source: &Source) -> Result<TitleIndex, DriverError> {
    title_index_with_program(DEFAULT_PROGRAM, source)
}

/// Drives a specific makemkvcon executable to produce a [`TitleIndex`].
///
/// Like [`title_index`], but runs `program` instead of the `PATH`-resolved
/// `makemkvcon`. Useful when the binary lives outside `PATH`.
///
/// # Errors
///
/// See [`title_index`].
pub fn title_index_with_program<P: AsRef<OsStr>>(
    program: P,
    source: &Source,
) -> Result<TitleIndex, DriverError> {
    let program = program.as_ref();
    let mrl = source.to_mrl();

    tracing::debug!(
        program = %program.to_string_lossy(),
        source = %mrl,
        "running makemkvcon info"
    );

    let output = Command::new(program)
        .args(["-r", "--minlength=0", "info"])
        .arg(&mrl)
        .output()
        .map_err(|source| DriverError::Spawn {
            program: program.to_string_lossy().into_owned(),
            source,
        })?;

    if !output.status.success() {
        return Err(DriverError::NonZeroExit {
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    let stdout = String::from_utf8(output.stdout).map_err(DriverError::NonUtf8Output)?;
    Ok(parse(&stdout)?)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod tests {
    use std::path::Path;

    use super::{DriverError, Source, title_index, title_index_with_program};

    #[test]
    fn source_renders_disc_mrl() {
        assert_eq!(
            Source::Disc(0).to_mrl(),
            "disc:0",
            "a drive index should render as disc:N"
        );
    }

    #[test]
    fn source_renders_iso_mrl() {
        let src = Source::Iso(Path::new("/images/backup.iso").into());
        assert_eq!(
            src.to_mrl(),
            "iso:/images/backup.iso",
            "an ISO path should render as iso:/path"
        );
    }

    #[test]
    fn source_renders_file_mrl() {
        let src = Source::File(Path::new("/backups/decrypted").into());
        assert_eq!(
            src.to_mrl(),
            "file:/backups/decrypted",
            "a folder path should render as file:/path"
        );
    }

    #[test]
    fn missing_program_is_a_spawn_error() {
        // A program name that cannot exist on PATH should surface as a Spawn
        // error rather than panicking — no makemkvcon required.
        let src = Source::Disc(0);
        let err = title_index_with_program("reliquary-makemkv-no-such-program-xyzzy", &src)
            .expect_err("spawning a nonexistent program should fail");
        assert!(
            matches!(err, DriverError::Spawn { .. }),
            "a missing executable should yield a Spawn error, got {err:?}"
        );
    }

    /// Manual end-to-end run against a real makemkvcon and source.
    ///
    /// Ignored by default: makemkvcon needs a registered key plus media, which
    /// we deliberately do not ship (public repo — no real titles or AACS keys).
    /// Provide a source MRL component via the environment to exercise it, e.g.
    /// `RELIQUARY_TEST_MAKEMKV_SOURCE=disc:0 cargo test -- --ignored`. The value
    /// is the MRL string passed straight through (`disc:N`, `iso:/path`, or
    /// `file:/path`). When the variable is unset (the default, including
    /// `make check`'s `--run-ignored all`) the test no-ops rather than failing —
    /// there is simply no source to enumerate.
    #[test]
    #[ignore = "requires a registered makemkvcon and media; source supplied via RELIQUARY_TEST_MAKEMKV_SOURCE"]
    fn title_index_real_source() {
        let Ok(mrl) = std::env::var("RELIQUARY_TEST_MAKEMKV_SOURCE") else {
            // No source configured — nothing to enumerate.
            return;
        };

        // Reconstruct a Source from the MRL prefix the operator supplied.
        let (scheme, rest) = mrl
            .split_once(':')
            .expect("RELIQUARY_TEST_MAKEMKV_SOURCE must be an MRL of the form scheme:value");
        let source = match scheme {
            "disc" => {
                let index: u32 = rest
                    .parse()
                    .expect("disc: MRL should carry a numeric index");
                Some(Source::Disc(index))
            }
            "iso" => Some(Source::Iso(Path::new(rest).into())),
            "file" => Some(Source::File(Path::new(rest).into())),
            _ => None,
        }
        .expect("RELIQUARY_TEST_MAKEMKV_SOURCE scheme must be disc:/iso:/file:");

        let index = title_index(&source).expect("driving real makemkvcon should succeed");
        assert!(
            !index.is_empty(),
            "a real source should enumerate at least one title"
        );
    }
}
