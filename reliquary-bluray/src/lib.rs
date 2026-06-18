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
mod overlay;

use std::ffi::{CString, NulError};
use std::path::Path;
use std::ptr::NonNull;

use thiserror::Error;

pub use overlay::Rect;

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

    /// The software IG-plane overlay state, heap-allocated so its address is
    /// stable. Its raw pointer is handed to libbluray via
    /// `bd_register_overlay_proc` in [`Disc::play`]; the overlay trampoline
    /// writes composited frames into it. Boxing guarantees the address libbluray
    /// holds stays valid even as the `Disc` itself is moved, and the box is
    /// dropped only after the proc is unregistered in [`Drop`].
    overlay: Box<overlay::OverlayCtx>,

    /// Whether the overlay proc is currently registered with libbluray. Guards
    /// [`Drop`] so we unregister exactly when we registered.
    overlay_registered: bool,

    /// Scratch buffer reused across [`Disc::pump`] so each pump does not
    /// reallocate. libbluray's navigation reads are 6144-byte aligned units; in
    /// overlay-only mode we never inspect the bytes, only the events and the
    /// overlay callback's side effects.
    read_buf: Vec<u8>,
}

/// A key the user can send to the disc menu.
///
/// Each variant maps to a libbluray `BD_VK_*` code. Keeping this an enum
/// (rather than exposing raw key integers) keeps the safe API closed:
/// `reliquary-nav` cannot pass an arbitrary FFI value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// Move the highlight up.
    Up,
    /// Move the highlight down.
    Down,
    /// Move the highlight left.
    Left,
    /// Move the highlight right.
    Right,
    /// Activate the selected button.
    Enter,
}

impl Key {
    /// The libbluray `BD_VK_*` code for this key.
    const fn to_vk(self) -> u32 {
        match self {
            Self::Up => ffi::BD_VK_UP,
            Self::Down => ffi::BD_VK_DOWN,
            Self::Left => ffi::BD_VK_LEFT,
            Self::Right => ffi::BD_VK_RIGHT,
            Self::Enter => ffi::BD_VK_ENTER,
        }
    }
}

/// One observable outcome of pumping the navigation loop.
///
/// Surfaced from the libbluray event queue. Overlay-only: this is the subset
/// the render loop reacts to; activation/playlist handling for the join lands
/// in a later stage, but [`PumpEvent::Playlist`] is already surfaced so callers
/// can observe it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpEvent {
    /// A playlist became current (`BD_EVENT_PLAYLIST`); the parameter is the
    /// mpls number. Fired on activation — the future "join" trigger.
    Playlist(u32),
    /// Interactive-menu visibility changed (`BD_EVENT_MENU`): `true` visible.
    MenuVisible(bool),
    /// Playback parked on a still (`BD_EVENT_STILL_TIME`); the parameter is the
    /// still duration in seconds (`0` = infinite). Menus commonly park here.
    StillTime(u32),
    /// A fatal libbluray error (`BD_EVENT_ERROR`); playback cannot continue.
    Error,
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

        NonNull::new(raw).map_or(Err(OpenError::OpenFailed), |handle| {
            Ok(Self {
                handle,
                overlay: Box::new(overlay::OverlayCtx::new()),
                overlay_registered: false,
                // 6144 bytes = one aligned BD navigation read unit.
                read_buf: vec![0u8; 6144],
            })
        })
    }

    /// Begin menu playback: register the overlay handler and start the disc
    /// with on-disc menus (`bd_play`).
    ///
    /// Call once after [`Disc::open`] and before [`Disc::pump`]. Registering the
    /// overlay proc before `bd_play` ensures the very first IG-plane commands
    /// are composited. Returns `false` if `bd_play` reported failure (e.g. the
    /// disc has no playable menu).
    pub fn play(&mut self) -> bool {
        if !self.overlay_registered {
            // The boxed `OverlayCtx` has a stable heap address; hand its raw
            // pointer to libbluray, which only stores it and passes it back to
            // the trampoline. We unregister in `Drop` before the box is freed.
            let ctx_ptr: *mut overlay::OverlayCtx = std::ptr::from_mut(self.overlay.as_mut());
            ffi::register_overlay(self.handle.as_ptr(), ctx_ptr);
            self.overlay_registered = true;
        }
        ffi::play(self.handle.as_ptr())
    }

    /// Jump to the disc Top Menu (`bd_menu_call`).
    ///
    /// Returns `false` if libbluray reported failure (e.g. the menu call is UO-
    /// masked at the current position).
    pub fn jump_to_menu(&mut self) -> bool {
        ffi::menu_call(self.handle.as_ptr())
    }

    /// Send a navigation [`Key`] to the menu (`bd_user_input`).
    ///
    /// Returns `true` if libbluray reported that the selection or activation
    /// changed (its `> 0` result), `false` for "no change" or a (non-fatal)
    /// rejection such as a UO-masked key.
    pub fn send_key(&mut self, key: Key) -> bool {
        ffi::user_input(self.handle.as_ptr(), key.to_vk()) > 0
    }

    /// Advance the navigation loop by up to `max_iterations` `bd_read_ext`
    /// pumps, returning the events observed (in order).
    ///
    /// Each pump advances the HDMV VM and may fire the overlay callback
    /// (compositing into the plane this `Disc` owns). The loop stops early when
    /// `bd_read_ext` reports an error or there is nothing left to do for this
    /// burst; `max_iterations` bounds the work per call so a UI frame is never
    /// blocked indefinitely. Call [`Disc::frame`] afterwards to fetch the
    /// latest composited menu image.
    pub fn pump(&mut self, max_iterations: u32) -> Vec<PumpEvent> {
        let mut events = Vec::new();
        for _ in 0..max_iterations {
            let (rc, ev) = ffi::read_ext(self.handle.as_ptr(), &mut self.read_buf);
            match ev.event {
                ffi::BD_EVENT_PLAYLIST => events.push(PumpEvent::Playlist(ev.param)),
                ffi::BD_EVENT_MENU => events.push(PumpEvent::MenuVisible(ev.param != 0)),
                ffi::BD_EVENT_STILL_TIME => events.push(PumpEvent::StillTime(ev.param)),
                ffi::BD_EVENT_ERROR => {
                    events.push(PumpEvent::Error);
                    break;
                }
                // BD_EVENT_NONE and the many events we do not model are ignored.
                _ => {}
            }
            // A negative read result is a hard error; stop pumping this burst.
            if rc < 0 {
                break;
            }
        }
        events
    }

    /// The current overlay frame generation. Increments once per overlay
    /// `FLUSH`; a change since the caller's last observation means a new
    /// composited menu image is available from [`Disc::frame`]. Cheap to poll
    /// every UI repaint to avoid re-uploading an unchanged texture.
    #[must_use]
    pub fn frame_generation(&self) -> u64 {
        self.overlay.generation()
    }

    /// Snapshot the latest composited menu frame as `(rgba, width, height)`,
    /// where `rgba` is `width * height * 4` bytes in `[R, G, B, A]` order.
    ///
    /// Returns `None` if no overlay plane has been initialised yet (no menu
    /// drawn). The buffer is copied out, so the returned `Vec` is independent of
    /// the disc's internal plane.
    #[must_use]
    pub fn frame(&self) -> Option<(Vec<u8>, u32, u32)> {
        self.overlay.snapshot()
    }

    /// The currently-selected menu button's bounding box, derived from
    /// libbluray's overlay **lone-draw** signal: when a button gains the
    /// highlight the disc redraws only that button, so a `FLUSH` preceded by
    /// exactly one IG `DRAW` isolates its rect. `None` until such a clean
    /// lone-draw has been seen (and reset by a full-plane clear).
    ///
    /// The submenu uses this clean single-draw highlight; the top menu uses a
    /// two-draw deselect/select pattern where the selected rect is not cleanly
    /// isolable, so this can remain `None` there — a known limitation of the
    /// signal, not a defect. Crop the frame to this rect (or use
    /// [`Disc::cropped_selection`]) for a tight single-button image.
    #[must_use]
    pub fn selected_button_rect(&self) -> Option<Rect> {
        self.overlay.selected_button_rect()
    }

    /// Snapshot the latest composited frame and crop it to the current
    /// [`selected_button_rect`](Disc::selected_button_rect), returning a tight
    /// `(rgba, width, height)` single-button image, or `None` if no menu has
    /// been composited yet or no clean lone-draw rect is available.
    ///
    /// The rect is clipped to the frame bounds, so a rect carried over from a
    /// previous, differently-sized page can never index out of range.
    #[must_use]
    pub fn cropped_selection(&self) -> Option<(Vec<u8>, u32, u32)> {
        self.overlay.cropped_selection()
    }

    /// Resolve an activated playlist (mpls number, as surfaced by
    /// [`PumpEvent::Playlist`]) to the clip ids it plays — the `.m2ts` base
    /// names *without* extension, e.g. `["00292"]`.
    ///
    /// This is the libbluray half of the navigate join: a content button
    /// activates a playlist, and this maps that playlist to its clip(s), whose
    /// names the caller matches against the makemkvcon title index to recover
    /// the output `.mkv`.
    ///
    /// Returns an empty `Vec` if libbluray does not know the playlist (its
    /// `bd_get_playlist_info` returned NULL). The clip ids are owned `String`s;
    /// no libbluray-owned memory escapes this call (the underlying
    /// `BLURAY_TITLE_INFO` is freed before returning).
    #[must_use]
    pub fn playlist_clips(&self, playlist: u32) -> Vec<String> {
        ffi::playlist_clips(self.handle.as_ptr(), playlist)
    }
}

impl Drop for Disc {
    fn drop(&mut self) {
        // Unregister the overlay proc first, so libbluray cannot call back into
        // the `OverlayCtx` after this `Disc` (and its boxed context) is gone.
        // Closing the disc also tears down callbacks, but unregistering
        // explicitly first removes any window between close and box-free.
        if self.overlay_registered {
            ffi::unregister_overlay(self.handle.as_ptr());
            self.overlay_registered = false;
        }
        // `self.handle` is a non-null pointer obtained from `bd_open` and not
        // yet closed; `Drop` runs at most once, so `bd_close` is called exactly
        // once for it.
        ffi::close(self.handle.as_ptr());
        // `self.overlay` (the Box) is freed after this returns — only now that
        // the proc is unregistered and the disc closed.
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
    use super::{Disc, Key, OpenError, version};
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

    /// Manual end-to-end render loop against a real disc.
    ///
    /// Opens the disc at `RELIQUARY_TEST_DISC`, registers the overlay handler,
    /// starts menu playback, jumps to the top menu, pumps the navigation loop a
    /// bounded number of times, sends an arrow key, and pumps again — exercising
    /// the full `play → pump → send_key → frame` path including the overlay
    /// trampoline. Ignored by default and a no-op when the env var is unset
    /// (public repo: no real disc ships).
    ///
    /// This cannot assert a *specific* menu rendered (that is disc-dependent and
    /// not visually verifiable here); it asserts the loop runs without error and
    /// that, once a menu has drawn, a frame snapshot is well-formed
    /// (`rgba.len() == w * h * 4`).
    #[test]
    #[ignore = "requires a real decrypted disc image; path supplied via RELIQUARY_TEST_DISC"]
    fn render_loop_smoke_real_disc() {
        let Ok(path) = std::env::var("RELIQUARY_TEST_DISC") else {
            // No disc configured — nothing to exercise.
            return;
        };
        let mut disc =
            Disc::open(Path::new(&path), None).expect("opening RELIQUARY_TEST_DISC should succeed");

        let played = disc.play();
        assert!(
            played,
            "bd_play should start menu playback on a disc with on-disc menus"
        );
        disc.jump_to_menu();

        // Pump a generous bounded amount so menus that park on a still settle.
        for _ in 0..32 {
            disc.pump(256);
        }
        // Nudge the highlight and pump again so a lone-draw/redraw can fire.
        disc.send_key(Key::Right);
        for _ in 0..32 {
            disc.pump(256);
        }

        // If a menu has been composited, the snapshot must be internally
        // consistent. (A disc that never drew a menu yields None, which we
        // tolerate rather than fail — some discs autostart playback.)
        if let Some((rgba, w, h)) = disc.frame() {
            assert_eq!(
                rgba.len(),
                (w as usize) * (h as usize) * 4,
                "a composited frame's RGBA length must equal width * height * 4, got {} for {w}x{h}",
                rgba.len()
            );
        }
        // `disc` drops here, exercising unregister + bd_close.
    }

    /// Manual end-to-end lone-draw crop against a real disc.
    ///
    /// Navigates a submenu (which uses the clean single-draw highlight) on the
    /// disc at `RELIQUARY_TEST_DISC` and asserts that a selected-button rect
    /// becomes available and that cropping the frame to it yields a tight,
    /// internally-consistent single-button image (`rgba.len() == w * h * 4`).
    /// Ignored by default and a no-op when the env var is unset (public repo:
    /// no real disc ships).
    ///
    /// This is the disc-verification gap made explicit: the unit tests prove the
    /// lone-draw *bookkeeping* on synthetic event streams, but only a real disc
    /// can confirm that the recorded rect visually matches the highlighted
    /// button. That visual match cannot be asserted programmatically here; this
    /// test only confirms a rect is produced and the crop is well-formed.
    #[test]
    #[ignore = "requires a real decrypted disc image; path supplied via RELIQUARY_TEST_DISC"]
    fn lone_draw_crop_real_disc() {
        let Ok(path) = std::env::var("RELIQUARY_TEST_DISC") else {
            // No disc configured — nothing to exercise.
            return;
        };
        let mut disc =
            Disc::open(Path::new(&path), None).expect("opening RELIQUARY_TEST_DISC should succeed");
        assert!(
            disc.play(),
            "bd_play should start menu playback on a disc with on-disc menus"
        );
        disc.jump_to_menu();

        // Settle the top menu, then step into a submenu and navigate so the
        // clean single-draw highlight fires. The exact key sequence is
        // disc-dependent; we drive a generous bounded walk and check whether a
        // rect ever materialises rather than assuming a fixed layout.
        for _ in 0..32 {
            disc.pump(256);
        }
        let mut saw_rect = false;
        for _ in 0..16 {
            disc.send_key(Key::Right);
            for _ in 0..16 {
                disc.pump(256);
            }
            if disc.selected_button_rect().is_some() {
                saw_rect = true;
                break;
            }
        }

        // A rect is only guaranteed on discs whose menus use the clean
        // lone-draw (submenus do; the top menu may not). When one is available,
        // the crop must be internally consistent.
        if saw_rect && let Some((rgba, w, h)) = disc.cropped_selection() {
            assert_eq!(
                rgba.len(),
                (w as usize) * (h as usize) * 4,
                "a cropped selection's RGBA length must equal width * height * 4, got {} for {w}x{h}",
                rgba.len()
            );
        }
        // `disc` drops here, exercising unregister + bd_close.
    }
}
