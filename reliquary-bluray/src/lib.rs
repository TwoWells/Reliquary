// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! # `reliquary-bluray`
//!
//! The safe libbluray FFI boundary for Reliquary. Reliquary drives libbluray
//! in-process to render and navigate disc menus rather than reimplementing the
//! HDMV VM. This crate is the **sole** carrier of the workspace's `unsafe`
//! exception: the raw `extern "C"` prototypes live in the private [`ffi`] module
//! under a single justified `#[allow(unsafe_code, …)]`, and every item exposed
//! here is a 100% safe API. `reliquary-nav` and the rest of the workspace stay
//! `forbid(unsafe_code)` and consume only this surface.
//!
//! ## Scope
//!
//! This is the lifecycle skeleton: [`Disc::open`] acquires a disc handle and
//! [`Disc`]'s [`Drop`] releases it via `bd_close`. Render, key input,
//! overlay/lone-draw, and playlist events land in later stages and will extend
//! this same safe API. The out-of-process subprocess driver is kept in reserve
//! behind this API as a documented fallback; it is not implemented here.
//!
//! ## Decrypted content
//!
//! libbluray opens an **encrypted** BD ISO when libaacs and the `FindVUK` KEYDB
//! are present; with no key path passed, libaacs auto-loads its
//! `~/.config/aacs/KEYDB.cfg`. A decrypted BDMV directory opens with no keys at
//! all. [`Disc::open`] models this with an `Option<&Path>` key argument —
//! `None` defers to libaacs' KEYDB.

mod ffi;

use std::ffi::{CString, NulError};
use std::path::Path;
use std::ptr::NonNull;

use thiserror::Error;

/// Errors from opening a Blu-ray disc handle.
#[derive(Debug, Error)]
pub enum OpenError {
    /// A path could not be converted to a C string because it contained an
    /// interior NUL byte. libbluray takes NUL-terminated C strings, so such a
    /// path can never be represented.
    #[error("path contains an interior NUL byte and cannot be passed to libbluray")]
    PathHasNul(#[source] NulError),

    /// `bd_open` returned `NULL`. The disc could not be opened — e.g. the path
    /// does not exist, is not a recognised BD structure, or required keys
    /// (libaacs/KEYDB) were missing for an encrypted disc. libbluray does not
    /// report a finer-grained reason through `bd_open` itself.
    #[error("libbluray could not open a disc at the given path (bd_open returned NULL)")]
    OpenFailed,
}

/// An open libbluray disc handle.
///
/// Wraps the opaque `BLURAY*` returned by `bd_open`. The handle is released
/// exactly once, in [`Drop`], via `bd_close` — there is no public `close`.
///
/// ## Threading
///
/// libbluray's documentation states overlay/event callbacks are invoked from
/// the thread that calls the `bd_*` functions, and the library is not
/// internally synchronised for concurrent use of one instance. This handle is
/// therefore intentionally **not** `Send`/`Sync` (the opaque FFI type carries a
/// raw-pointer marker that makes it `!Send`/`!Sync` by default). Drive a given
/// `Disc` from a single thread. If future work needs cross-thread handoff, that
/// must be justified against libbluray's actual guarantees rather than assumed
/// here.
#[derive(Debug)]
pub struct Disc {
    /// Non-null `BLURAY*`. Invariant: valid and owned for the lifetime of this
    /// `Disc`, freed exactly once in [`Drop`].
    handle: NonNull<ffi::Bluray>,
}

impl Disc {
    /// Open a Blu-ray disc and acquire a handle to it.
    ///
    /// `path` may be a decrypted BDMV directory, a mounted disc/device, or an
    /// ISO image file (libbluray reads ISOs directly via libudfread).
    ///
    /// `keys` is the path to a libaacs `KEYDB.cfg` keyfile. Pass `None` (the
    /// common case) to let libaacs auto-load its default
    /// `~/.config/aacs/KEYDB.cfg` — required only for encrypted discs;
    /// decrypted BDMV directories need no keys.
    ///
    /// The returned [`Disc`] releases the handle automatically on drop.
    ///
    /// # Errors
    ///
    /// - [`OpenError::PathHasNul`] if `path` or `keys` contains an interior NUL
    ///   byte (cannot be expressed as a C string).
    /// - [`OpenError::OpenFailed`] if libbluray's `bd_open` returns `NULL`
    ///   (path missing/unrecognised, or required keys absent for an encrypted
    ///   disc).
    pub fn open(path: &Path, keys: Option<&Path>) -> Result<Self, OpenError> {
        let device = path_to_cstring(path)?;
        let keyfile = match keys {
            Some(k) => Some(path_to_cstring(k)?),
            None => None,
        };

        // A NULL keyfile pointer tells libbluray to defer to libaacs' KEYDB.
        let keyfile_ptr = keyfile.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());

        // `device` and `keyfile` own the backing storage and outlive the call;
        // the returned pointer is owned and never aliased.
        let raw = ffi::open(device.as_ptr(), keyfile_ptr);

        // Keep the CStrings alive until after `bd_open` has copied what it needs.
        drop(device);
        drop(keyfile);

        NonNull::new(raw).map_or(Err(OpenError::OpenFailed), |handle| Ok(Self { handle }))
    }
}

impl Drop for Disc {
    fn drop(&mut self) {
        // `self.handle` is a non-null pointer obtained from `bd_open` and not
        // yet closed; `Drop` runs at most once, so `bd_close` is called exactly
        // once for it.
        ffi::close(self.handle.as_ptr());
    }
}

/// libbluray runtime version as `(major, minor, micro)`.
///
/// A side-effect-free probe of the linked library, useful as a smoke test that
/// the FFI boundary and link directive are wired up correctly.
#[must_use]
pub fn version() -> (i32, i32, i32) {
    ffi::version()
}

/// Convert a filesystem path to a NUL-terminated C string for libbluray.
fn path_to_cstring(path: &Path) -> Result<CString, OpenError> {
    // On Unix, OS strings are arbitrary bytes; encode them losslessly. (This
    // crate targets the Linux libbluray build per the project's reference.)
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        CString::new(path.as_os_str().as_bytes()).map_err(OpenError::PathHasNul)
    }

    #[cfg(not(unix))]
    {
        // Fall back to a lossy UTF-8 view on non-Unix targets. libbluray's
        // supported deployment here is Linux; this branch exists only so the
        // crate still type-checks elsewhere.
        let s = path.to_string_lossy();
        CString::new(s.as_ref()).map_err(OpenError::PathHasNul)
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod tests {
    use super::{Disc, OpenError, version};
    use std::path::Path;

    #[test]
    fn open_nonexistent_path_returns_open_failed() {
        // A path that does not exist: bd_open returns NULL → OpenFailed.
        let bogus = Path::new("/nonexistent/reliquary-bluray/no-such-disc.iso");
        let result = Disc::open(bogus, None);
        assert!(
            matches!(result, Err(OpenError::OpenFailed)),
            "opening a nonexistent path should yield OpenError::OpenFailed, got {result:?}"
        );
    }

    #[test]
    fn open_path_with_interior_nul_returns_path_has_nul() {
        // Build a path containing an interior NUL byte, which can never be a
        // valid C string. Done via raw bytes so no real path is referenced.
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let bad = Path::new(OsStr::from_bytes(b"/tmp/with\0nul"));
        let result = Disc::open(bad, None);
        assert!(
            matches!(result, Err(OpenError::PathHasNul(_))),
            "a path with an interior NUL should yield OpenError::PathHasNul, got {result:?}"
        );
    }

    #[test]
    fn keys_path_with_interior_nul_returns_path_has_nul() {
        // The keyfile path is also validated; an interior NUL there must error
        // before any FFI call.
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let ok_disc = Path::new("/nonexistent/reliquary-bluray/disc");
        let bad_keys = Path::new(OsStr::from_bytes(b"/tmp/keys\0db"));
        let result = Disc::open(ok_disc, Some(bad_keys));
        assert!(
            matches!(result, Err(OpenError::PathHasNul(_))),
            "a keyfile path with an interior NUL should yield OpenError::PathHasNul, got {result:?}"
        );
    }

    #[test]
    fn version_is_queryable() {
        // Smoke test: the link directive and extern "C" ABI are wired up, and
        // libbluray reports a plausible (non-negative) version triple.
        let (major, minor, micro) = version();
        assert!(
            major >= 0 && minor >= 0 && micro >= 0,
            "libbluray version components should be non-negative, got {major}.{minor}.{micro}"
        );
    }

    /// Manual end-to-end open + drop against a real decrypted BDMV / ISO.
    ///
    /// Ignored by default: it needs a real disc image, which we deliberately do
    /// not ship (public repo — no real titles or AACS keys). Provide a generic
    /// descriptor path via the environment to exercise it, e.g.
    /// `RELIQUARY_TEST_DISC=/path/to/decrypted/bdmv cargo test -- --ignored`.
    /// Nothing sensitive is hardcoded; the path comes from the environment. When
    /// the variable is unset (the default, including `make check`'s
    /// `--run-ignored all`) the test no-ops rather than failing — there is
    /// simply no disc to open.
    #[test]
    #[ignore = "requires a real decrypted disc image; path supplied via RELIQUARY_TEST_DISC"]
    fn open_then_drop_real_disc() {
        let Ok(path) = std::env::var("RELIQUARY_TEST_DISC") else {
            // No disc configured — nothing to exercise.
            return;
        };
        let disc = Disc::open(Path::new(&path), None);
        assert!(
            disc.is_ok(),
            "opening the disc at RELIQUARY_TEST_DISC should succeed, got {disc:?}"
        );
        // `disc` drops here, exercising bd_close.
    }
}
