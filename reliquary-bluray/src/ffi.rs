// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Raw, hand-written FFI declarations for `libbluray`.
//!
//! This module is the **sole** carrier of the workspace's `unsafe` exception.
//! Every `extern "C"` prototype below is transcribed by hand from the installed
//! `libbluray/bluray.h` header — no `bindgen`, so we keep
//! the `unsafe` surface tiny, auditable, and free of a `libclang` build
//! dependency. Nothing in this module is part of the public API; the crate
//! root re-exposes a 100% safe wrapper.
//!
//! ## Scope
//!
//! This module currently covers disc **lifecycle only** — `bd_open` and
//! `bd_close`. The playback, key-input, overlay-callback, and playlist-info
//! prototypes land in later stages.
//!
//! ## ABI guard (deferred)
//!
//! We bind no libbluray *structs* yet; `BLURAY` is an opaque pointer we never
//! dereference, so there is nothing whose layout could drift. Once we start
//! dereferencing `BLURAY_TITLE_INFO` and the overlay structs, this module MUST
//! gain `size_of` + field-offset assertion tests against the header to catch
//! ABI drift. None are needed yet.
//!
//! ## No unwinding across the FFI boundary
//!
//! No Rust callback is registered with libbluray yet. When the overlay/ARGB C
//! callbacks are added (`bd_register_overlay_proc`), the Rust callback bodies
//! MUST wrap their logic in [`std::panic::catch_unwind`] and never let a panic
//! unwind into libbluray's C frames — unwinding across an `extern "C"` boundary
//! is undefined behaviour. The rule is recorded here now so that work inherits
//! it.

#![allow(
    unsafe_code,
    reason = "this crate is the sole libbluray FFI boundary; the raw extern \
              \"C\" prototypes here are the workspace's only unsafe surface, \
              wrapped by a 100% safe API in the crate root"
)]

use core::ffi::{c_char, c_int};

/// Opaque libbluray instance handle (`BLURAY` in `bluray.h`).
///
/// libbluray declares `typedef struct bluray BLURAY;` and never exposes the
/// struct's fields, so we model it as an opaque, zero-field type that can only
/// ever be referred to behind a pointer. We never construct, move, or
/// dereference a value of this type in Rust.
#[repr(C)]
pub struct Bluray {
    // A single private zero-sized member makes the type both opaque and
    // impossible to construct outside libbluray, matching the C `struct bluray`
    // whose definition is hidden. `PhantomData` over the never-Send/Sync raw
    // pointer also makes the handle correctly `!Send`/`!Sync` by default.
    _opaque: [u8; 0],
    _marker: core::marker::PhantomData<(*mut u8, core::marker::PhantomPinned)>,
}

unsafe extern "C" {
    /// `BLURAY *bd_open(const char *device_path, const char *keyfile_path);`
    ///
    /// Shortcut for `bd_open_disc(bd_init(), device_path, keyfile_path)`.
    /// `device_path` may be a decrypted BDMV folder, a mounted disc/device, or
    /// (via libudfread) an ISO image file directly. A `NULL` `keyfile_path`
    /// lets libaacs auto-load its `KEYDB.cfg`. Returns an allocated `BLURAY`
    /// object, or `NULL` on error.
    fn bd_open(device_path: *const c_char, keyfile_path: *const c_char) -> *mut Bluray;

    /// `void bd_close(BLURAY *bd);`
    ///
    /// Closes the disc and frees the `BLURAY` object. Safe to call exactly once
    /// per non-null handle returned by [`bd_open`]; never call it twice.
    fn bd_close(bd: *mut Bluray);

    /// `void bd_get_version(int *major, int *minor, int *micro);`
    ///
    /// Reports the runtime libbluray version. Bound here purely as a
    /// side-effect-free smoke test that the link directive and the `extern "C"`
    /// ABI are wired up correctly.
    fn bd_get_version(major: *mut c_int, minor: *mut c_int, micro: *mut c_int);
}

// Thin safe-to-call wrappers. Every `unsafe` block in the crate lives here,
// under this module's single `#![allow(unsafe_code, …)]`, so the crate root
// (`lib.rs`) calls only safe Rust and carries no `unsafe`. These are not safe
// in the abstract — callers must uphold the documented contracts — but they
// concentrate the auditable FFI surface in one place.

/// Open a disc handle.
///
/// `device_path` must be a valid, NUL-terminated C string pointer.
/// `keyfile_path` must be either NULL or a valid NUL-terminated C string
/// pointer. Returns NULL on failure; a non-null result is an owned `BLURAY*`
/// that the caller must release exactly once with [`close`].
pub fn open(device_path: *const c_char, keyfile_path: *const c_char) -> *mut Bluray {
    // SAFETY: `bd_open` reads two NUL-terminated C strings (the second may be
    // NULL) and returns either a freshly allocated, owned `BLURAY*` or NULL. The
    // caller guarantees both pointers satisfy those preconditions and keeps the
    // backing storage alive across the call.
    unsafe { bd_open(device_path, keyfile_path) }
}

/// Close a disc handle previously returned non-null by [`open`].
///
/// `bd` must be a non-null `BLURAY*` from [`open`] that has not already been
/// closed; it must not be used afterward.
pub fn close(bd: *mut Bluray) {
    // SAFETY: the caller guarantees `bd` is a live, non-null handle from
    // `bd_open` that has not yet been closed, and is closed exactly once here.
    unsafe { bd_close(bd) };
}

/// Query the runtime libbluray version as `(major, minor, micro)`.
pub fn version() -> (c_int, c_int, c_int) {
    let mut major: c_int = 0;
    let mut minor: c_int = 0;
    let mut micro: c_int = 0;
    // SAFETY: `bd_get_version` writes three valid `int` out-pointers and has no
    // other effect; the locals outlive the call.
    unsafe { bd_get_version(&raw mut major, &raw mut minor, &raw mut micro) };
    (major, minor, micro)
}
