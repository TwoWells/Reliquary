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
//! This module covers disc **lifecycle** (`bd_open`, `bd_close`,
//! `bd_get_version`) and the menu **render + interaction loop**: `bd_play`,
//! `bd_read_ext` (the navigation pump), `bd_menu_call`, `bd_user_input`, and
//! `bd_register_overlay_proc` with its IG-plane overlay callback. The
//! playlist-info / activation prototypes land in later stages.
//!
//! ## ABI guard
//!
//! Every libbluray struct mirrored here — [`BdEvent`], [`BdOverlay`],
//! [`BdPgPaletteEntry`], [`BdPgRleElem`] — is matched by a `size_of` plus
//! key-field [`core::mem::offset_of!`] assertion test against the layout
//! documented in the installed headers, so an ABI change in libbluray fails a
//! test here rather than silently corrupting memory when we dereference an
//! overlay event.
//!
//! ## No unwinding across the FFI boundary
//!
//! The overlay callback ([`overlay_trampoline`]) is an `extern "C"` function
//! libbluray invokes synchronously from inside our `bd_read_ext` calls. Its
//! entire body runs inside [`std::panic::catch_unwind`]: a panic must never
//! unwind into libbluray's C frames, which is undefined behaviour. A caught
//! panic is swallowed (logged at most) and the callback returns normally,
//! leaving the plane untouched for that event.

#![allow(
    unsafe_code,
    reason = "this crate is the sole libbluray FFI boundary; the raw extern \
              \"C\" prototypes here are the workspace's only unsafe surface, \
              wrapped by a 100% safe API in the crate root"
)]

use core::ffi::{c_char, c_int, c_void};

use crate::overlay::{OverlayCtx, OverlayEvent};

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

// --- Struct mirrors (dereferenced — guarded by the ABI tests below) ---

/// Mirror of `BD_EVENT` (`bluray.h`): an event-queue entry.
///
/// Two 32-bit fields, no padding. We read `event`/`param` out of the value
/// `bd_read_ext` writes through our out-pointer.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct BdEvent {
    /// Event type (`bd_event_e`).
    pub event: u32,
    /// Event-specific data.
    pub param: u32,
}

/// Mirror of `BD_PG_PALETTE_ENTRY` (`overlay.h`): one BT.601 limited-range
/// palette entry. Four `uint8_t`, packed.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BdPgPaletteEntry {
    /// Luma `Y` (16…235).
    pub y: u8,
    /// `Cr` chroma (16…240).
    pub cr: u8,
    /// `Cb` chroma (16…240).
    pub cb: u8,
    /// Transparency `T` (0 transparent … 255 opaque).
    pub t: u8,
}

/// Mirror of `BD_PG_RLE_ELEM` (`overlay.h`): one RLE run. Two `uint16_t`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BdPgRleElem {
    /// Run length; `0` marks end-of-line.
    pub len: u16,
    /// Palette index.
    pub color: u16,
}

/// Mirror of `BD_OVERLAY` (`overlay.h`): a compressed-YUV overlay event.
///
/// Field order and padding follow the C struct exactly (guarded below): the
/// three `uint8_t` flags sit between the leading `int64_t pts` and the four
/// `uint16_t` geometry fields, with the two trailing pointers 8-byte aligned.
/// We only ever read fields out of a `*const BdOverlay` that libbluray owns;
/// we never construct one to hand back.
#[repr(C)]
pub struct BdOverlay {
    /// Presentation timestamp on the video grid.
    pub pts: i64,
    /// Overlay plane (`bd_overlay_plane_e`: 0 = PG, 1 = IG).
    pub plane: u8,
    /// Overlay command (`bd_overlay_cmd_e`).
    pub cmd: u8,
    /// Set if only the palette changed.
    pub palette_update_flag: u8,
    /// Region top-left x.
    pub x: u16,
    /// Region top-left y.
    pub y: u16,
    /// Region width.
    pub w: u16,
    /// Region height.
    pub h: u16,
    /// Pointer to 256 palette entries (may be NULL for non-DRAW commands).
    pub palette: *const BdPgPaletteEntry,
    /// Pointer to the RLE image (may be NULL for non-DRAW commands).
    pub img: *const BdPgRleElem,
}

/// Mirror of `BLURAY_CLIP_INFO` (`bluray.h`): one clip of a playlist.
///
/// We dereference only `clip_id` (the `.m2ts` base name) from this struct; the
/// rest of the fields exist purely to make the layout — and therefore the array
/// stride for `clips[i]` — byte-exact. The six `BLURAY_STREAM_INFO *` fields are
/// pointers we never follow, mirrored as opaque `*const c_void` so we do not
/// have to model `BLURAY_STREAM_INFO` itself. The trailing `clip_id[6]` lands at
/// offset 88 on LP64 (guarded by the ABI tests below); a wrong overall size here
/// would silently read garbage `clip_id` for any clip past the first, so its
/// `size_of` is the sharpest ABI assertion in this module.
#[repr(C)]
pub struct BluRayClipInfo {
    /// Number of mpeg-ts packets.
    pub pkt_count: u32,
    /// Clip still mode (`bd_still_mode_e`).
    pub still_mode: u8,
    /// Still time (seconds) if `still_mode == BD_STILL_TIME`.
    pub still_time: u16,
    /// Number of video streams.
    pub video_stream_count: u8,
    /// Number of audio streams.
    pub audio_stream_count: u8,
    /// Number of PG (Presentation Graphics) streams.
    pub pg_stream_count: u8,
    /// Number of IG (Interactive Graphics) streams.
    pub ig_stream_count: u8,
    /// Number of secondary audio streams.
    pub sec_audio_stream_count: u8,
    /// Number of secondary video streams.
    pub sec_video_stream_count: u8,
    /// `BLURAY_STREAM_INFO *video_streams` — opaque; never followed.
    pub video_streams: *const c_void,
    /// `BLURAY_STREAM_INFO *audio_streams` — opaque; never followed.
    pub audio_streams: *const c_void,
    /// `BLURAY_STREAM_INFO *pg_streams` — opaque; never followed.
    pub pg_streams: *const c_void,
    /// `BLURAY_STREAM_INFO *ig_streams` — opaque; never followed.
    pub ig_streams: *const c_void,
    /// `BLURAY_STREAM_INFO *sec_audio_streams` — opaque; never followed.
    pub sec_audio_streams: *const c_void,
    /// `BLURAY_STREAM_INFO *sec_video_streams` — opaque; never followed.
    pub sec_video_streams: *const c_void,
    /// Start media time, 90 kHz ("playlist time").
    pub start_time: u64,
    /// Start timestamp, 90 kHz.
    pub in_time: u64,
    /// End timestamp, 90 kHz.
    pub out_time: u64,
    /// Clip identifier (`.m2ts` file name), NUL-padded ASCII. The one field we
    /// read.
    pub clip_id: [u8; 6],
}

/// Mirror of `BLURAY_TITLE_INFO` (`bluray.h`): a playlist's clip list and
/// counts.
///
/// We dereference `clip_count` and the `clips` pointer (then stride into the
/// `BluRayClipInfo` array). The `chapters`/`marks` pointers are mirrored as
/// opaque `*const c_void` — we never follow them — so the layout stays exact
/// without modelling the chapter/mark structs. `duration` (90 kHz) is read for
/// the surfaced selection line.
#[repr(C)]
pub struct BluRayTitleInfo {
    /// Playlist index number (filled only by `bd_get_title_info`).
    pub idx: u32,
    /// Playlist ID (mpls file name).
    pub playlist: u32,
    /// Playlist duration, 90 kHz.
    pub duration: u64,
    /// Number of clips — the bound for iterating `clips`.
    pub clip_count: u32,
    /// Number of angles.
    pub angle_count: u8,
    /// Number of chapters.
    pub chapter_count: u32,
    /// Number of playmarks.
    pub mark_count: u32,
    /// `BLURAY_CLIP_INFO *clips` — the clip array we stride into.
    pub clips: *const BluRayClipInfo,
    /// `BLURAY_TITLE_CHAPTER *chapters` — opaque; never followed.
    pub chapters: *const c_void,
    /// `BLURAY_TITLE_MARK *marks` — opaque; never followed.
    pub marks: *const c_void,
    /// MVC base view (0 = left, 1 = right).
    pub mvc_base_view_r_flag: u8,
}

// --- Constants (transcribed from keys.h / overlay.h / bluray.h) ---

/// `BD_OVERLAY_IG` — the Interactive Graphics plane (HDMV menus).
pub const BD_OVERLAY_IG: u8 = 1;

/// `BD_OVERLAY_INIT` — allocate the plane.
pub const BD_OVERLAY_INIT: u8 = 0;
/// `BD_OVERLAY_CLOSE` — release the plane.
pub const BD_OVERLAY_CLOSE: u8 = 1;
/// `BD_OVERLAY_CLEAR` — clear the whole plane.
pub const BD_OVERLAY_CLEAR: u8 = 2;
/// `BD_OVERLAY_DRAW` — draw an RLE bitmap.
pub const BD_OVERLAY_DRAW: u8 = 3;
/// `BD_OVERLAY_WIPE` — clear a rectangle.
pub const BD_OVERLAY_WIPE: u8 = 4;
/// `BD_OVERLAY_HIDE` — the plane is empty and may be hidden.
pub const BD_OVERLAY_HIDE: u8 = 5;
/// `BD_OVERLAY_FLUSH` — present accumulated changes.
pub const BD_OVERLAY_FLUSH: u8 = 6;

/// Number of palette entries libbluray supplies per overlay (`overlay.h`).
pub const BD_OVERLAY_PALETTE_LEN: usize = 256;

/// `BD_VK_UP` — arrow up.
pub const BD_VK_UP: u32 = 12;
/// `BD_VK_DOWN` — arrow down.
pub const BD_VK_DOWN: u32 = 13;
/// `BD_VK_LEFT` — arrow left.
pub const BD_VK_LEFT: u32 = 14;
/// `BD_VK_RIGHT` — arrow right.
pub const BD_VK_RIGHT: u32 = 15;
/// `BD_VK_ENTER` — select / activate.
pub const BD_VK_ENTER: u32 = 16;

/// `BD_EVENT_ERROR` — fatal playback error.
pub const BD_EVENT_ERROR: u32 = 1;
/// `BD_EVENT_PLAYLIST` — a playlist became current (the activation/join trigger).
pub const BD_EVENT_PLAYLIST: u32 = 6;
/// `BD_EVENT_STILL_TIME` — parked on a still (param: 0 = infinite, else seconds).
pub const BD_EVENT_STILL_TIME: u32 = 26;
/// `BD_EVENT_MENU` — interactive menu visibility (0 = no, 1 = yes).
pub const BD_EVENT_MENU: u32 = 30;

/// libbluray's YUV overlay handler type:
/// `void (*)(void *handle, const BD_OVERLAY * const event)`.
pub type OverlayProc = Option<unsafe extern "C" fn(handle: *mut c_void, event: *const BdOverlay)>;

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

    /// `int bd_play(BLURAY *bd);`
    ///
    /// Start playing the disc with on-disc menus, from the First Play title.
    /// Returns 1 on success, 0 on error.
    fn bd_play(bd: *mut Bluray) -> c_int;

    /// `int bd_read_ext(BLURAY *bd, unsigned char *buf, int len, BD_EVENT *event);`
    ///
    /// The navigation-mode pump: advances the HDMV VM, fires registered
    /// callbacks (including the overlay proc), and writes the next queued event
    /// through `event`. Returns bytes read, `-1` on error, `0` when an event
    /// must be handled first or at end of title.
    fn bd_read_ext(bd: *mut Bluray, buf: *mut u8, len: c_int, event: *mut BdEvent) -> c_int;

    /// `int bd_menu_call(BLURAY *bd, int64_t pts);`
    ///
    /// Open the disc Top Menu. `pts` is the current playback position
    /// (1/90000 s) or `-1`. Returns 1 on success, 0 on error.
    fn bd_menu_call(bd: *mut Bluray, pts: i64) -> c_int;

    /// `int bd_user_input(BLURAY *bd, int64_t pts, uint32_t key);`
    ///
    /// Pass a user key (a `BD_VK_*` code) to the graphics controller. `pts` is
    /// `-1` for "now". Returns `<0` on error, `0` on success, `>0` if the
    /// selection/activation changed.
    fn bd_user_input(bd: *mut Bluray, pts: i64, key: u32) -> c_int;

    /// `void bd_register_overlay_proc(BLURAY *bd, void *handle, bd_overlay_proc_f func);`
    ///
    /// Register the compressed-YUV (IG/PG) overlay handler. `handle` is passed
    /// back to `func` on every event. Pass a NULL `func` to unregister.
    fn bd_register_overlay_proc(bd: *mut Bluray, handle: *mut c_void, func: OverlayProc);

    /// `BLURAY_TITLE_INFO* bd_get_playlist_info(BLURAY *bd, uint32_t playlist, unsigned angle);`
    ///
    /// Resolve a playlist (mpls) to its clip list. Returns a freshly allocated,
    /// owned `BLURAY_TITLE_INFO*` whose `clips[i].clip_id` is the `.m2ts` base
    /// name — the join's match key — or `NULL` on error. The caller must release
    /// a non-null result exactly once with [`bd_free_title_info`].
    fn bd_get_playlist_info(
        bd: *mut Bluray,
        playlist: u32,
        angle: core::ffi::c_uint,
    ) -> *mut BluRayTitleInfo;

    /// `void bd_free_title_info(BLURAY_TITLE_INFO *title_info);`
    ///
    /// Free a `BLURAY_TITLE_INFO*` returned by [`bd_get_playlist_info`] (or
    /// `bd_get_title_info`). Safe to call exactly once per non-null result.
    fn bd_free_title_info(title_info: *mut BluRayTitleInfo);
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

/// Start playing the disc with on-disc menus. Returns `true` on success.
///
/// `bd` must be a live, non-null handle from [`open`].
pub fn play(bd: *mut Bluray) -> bool {
    // SAFETY: the caller guarantees `bd` is a live, non-null `BLURAY*` from
    // `bd_open`; `bd_play` only reads and mutates state behind that handle.
    unsafe { bd_play(bd) == 1 }
}

/// Jump to the disc Top Menu (`bd_menu_call(bd, -1)`). Returns `true` on
/// success.
///
/// `bd` must be a live, non-null handle from [`open`].
pub fn menu_call(bd: *mut Bluray) -> bool {
    // SAFETY: the caller guarantees `bd` is a live, non-null `BLURAY*`. `-1`
    // means "use the current pts" per the header contract.
    unsafe { bd_menu_call(bd, -1) == 1 }
}

/// Pass a `BD_VK_*` key to the graphics controller (`bd_user_input(bd, -1,
/// key)`). Returns the raw result: `<0` error, `0` no change, `>0` selection
/// or activation changed.
///
/// `bd` must be a live, non-null handle from [`open`].
pub fn user_input(bd: *mut Bluray, key: u32) -> c_int {
    // SAFETY: the caller guarantees `bd` is a live, non-null `BLURAY*`; `-1`
    // means "now"; `key` is an arbitrary key code which libbluray validates.
    unsafe { bd_user_input(bd, -1, key) }
}

/// Pump the navigation loop once: advance the VM, fire callbacks, and return
/// the next queued event (`BD_EVENT_NONE` if none) plus the bytes-read result
/// from `bd_read_ext` (`-1` on error).
///
/// `bd` must be a live, non-null handle from [`open`]. `buf` is a scratch
/// buffer libbluray may write decoded data into; in overlay-only mode we
/// discard its contents and only act on events + the overlay callback.
pub fn read_ext(bd: *mut Bluray, buf: &mut [u8]) -> (c_int, BdEvent) {
    let mut event = BdEvent::default();
    let len = c_int::try_from(buf.len()).unwrap_or(c_int::MAX);
    // SAFETY: the caller guarantees `bd` is a live, non-null `BLURAY*`. `buf`
    // is a valid, writable slice of `len` bytes whose pointer and length we
    // pass faithfully; `event` is a valid, writable out-pointer that outlives
    // the call. `bd_read_ext` writes at most `len` bytes into `buf` and one
    // event into `event`.
    let rc = unsafe { bd_read_ext(bd, buf.as_mut_ptr(), len, &raw mut event) };
    (rc, event)
}

/// Register `handle` (an `OverlayCtx` pointer) with the IG/PG overlay
/// trampoline. Pass the same `handle` that was boxed and kept alive by the
/// caller for the lifetime of the registration.
///
/// `bd` must be a live, non-null handle from [`open`]. `handle` must point to a
/// valid [`OverlayCtx`] that outlives every subsequent `bd_read_ext` call and
/// the eventual unregister/close.
pub fn register_overlay(bd: *mut Bluray, handle: *mut OverlayCtx) {
    // SAFETY: the caller guarantees `bd` is a live, non-null `BLURAY*` and that
    // `handle` points to an `OverlayCtx` living at least until the proc is
    // unregistered or the disc closed. libbluray only stores the pointer and
    // passes it back to `overlay_trampoline`; it never dereferences it itself.
    unsafe {
        bd_register_overlay_proc(bd, handle.cast::<c_void>(), Some(overlay_trampoline));
    }
}

/// Unregister the overlay proc (NULL func), so libbluray stops calling back
/// before the [`OverlayCtx`] is dropped.
///
/// `bd` must be a live, non-null handle from [`open`].
pub fn unregister_overlay(bd: *mut Bluray) {
    // SAFETY: the caller guarantees `bd` is a live, non-null `BLURAY*`. A NULL
    // func unregisters the handler; the `handle` is then irrelevant.
    unsafe { bd_register_overlay_proc(bd, core::ptr::null_mut(), None) };
}

/// Resolve a playlist (mpls) to the list of its clip ids (the `.m2ts` base
/// names, without extension), e.g. `["00292"]`.
///
/// Calls `bd_get_playlist_info`, reads `clip_count` and each `clips[i].clip_id`
/// into an owned `Vec<String>`, then frees the `BLURAY_TITLE_INFO` — no raw
/// pointer escapes the call. Returns an empty `Vec` if `bd_get_playlist_info`
/// returns NULL (unknown playlist or error).
///
/// `bd` must be a live, non-null handle from [`open`].
pub fn playlist_clips(bd: *mut Bluray, playlist: u32) -> Vec<String> {
    // SAFETY: the caller guarantees `bd` is a live, non-null `BLURAY*`. Angle 0
    // is always valid. `bd_get_playlist_info` returns either NULL or a freshly
    // allocated, owned `BLURAY_TITLE_INFO*` we are responsible for freeing.
    let info = unsafe { bd_get_playlist_info(bd, playlist, 0) };
    let Some(info) = core::ptr::NonNull::new(info) else {
        return Vec::new();
    };

    // SAFETY: `info` is a non-null, freshly allocated `BLURAY_TITLE_INFO` owned
    // by us until `bd_free_title_info`. We only read its scalar fields and the
    // `clips` pointer; the struct layout is guarded by the ABI tests below.
    let title = unsafe { info.as_ref() };
    let clip_count = title.clip_count as usize;

    let mut clips = Vec::with_capacity(clip_count);
    if !title.clips.is_null() {
        for i in 0..clip_count {
            // SAFETY: `clips` points to `clip_count` valid `BluRayClipInfo`
            // elements (libbluray's invariant for a non-null clip array); `i`
            // stays in `0..clip_count`. The element's stride is `BluRayClipInfo`'s
            // size, asserted exact by the ABI tests, so `add(i)` addresses the
            // i-th clip and not garbage.
            let clip = unsafe { &*title.clips.add(i) };
            clips.push(clip_id_to_string(clip.clip_id));
        }
    }

    // SAFETY: `info` is the non-null result of `bd_get_playlist_info`, not yet
    // freed and not used after this call; `bd_free_title_info` releases it
    // exactly once. All data we needed is already copied into owned `String`s.
    unsafe { bd_free_title_info(info.as_ptr()) };

    clips
}

/// Decode a libbluray `clip_id` (`[u8; 6]`, NUL-padded ASCII like `b"00292\0"`)
/// into an owned `String` with no extension, e.g. `"00292"`.
///
/// Stops at the first NUL; bytes are interpreted as Latin-1 so a (never-expected)
/// non-ASCII byte cannot panic or be lost. clip ids are always ASCII digits in
/// practice.
fn clip_id_to_string(clip_id: [u8; 6]) -> String {
    clip_id
        .into_iter()
        .take_while(|&b| b != 0)
        .map(char::from)
        .collect()
}

/// The `extern "C"` overlay callback libbluray invokes for each IG/PG command.
///
/// libbluray calls this **synchronously** from inside our `bd_read_ext` calls,
/// on the same thread, passing back the `handle` we registered (an
/// `*mut OverlayCtx`) and a borrowed `*const BdOverlay`. The entire body runs
/// inside [`std::panic::catch_unwind`]: a panic must never unwind into the C
/// frame above us (that is undefined behaviour), so any panic is caught and
/// swallowed, leaving the plane unchanged for that event.
///
/// Only IG-plane commands are composited; PG-plane (subtitle) events are
/// ignored. The raw palette/RLE pointers are copied into owned `Vec`s before
/// the borrow ends, then dispatched through the safe [`OverlayCtx::apply`].
unsafe extern "C" fn overlay_trampoline(handle: *mut c_void, event: *const BdOverlay) {
    // Catch any panic so it cannot cross back into libbluray's C stack.
    let result = std::panic::catch_unwind(|| {
        if handle.is_null() || event.is_null() {
            return;
        }
        // SAFETY: libbluray passes back exactly the `*mut OverlayCtx` we
        // registered (kept alive by the `Disc` for the registration's
        // lifetime) and a non-null `*const BD_OVERLAY` it owns for the duration
        // of this call. We only read through `event`; we hold `&*ctx` (shared)
        // and never alias it mutably across threads — calls are serialized on
        // this thread by libbluray.
        let ctx: &OverlayCtx = unsafe { &*handle.cast::<OverlayCtx>() };
        // SAFETY: as above — `event` is a valid, readable `BD_OVERLAY` for this
        // call. Reading the scalar fields is sound; the pointer fields are
        // validated before dereference in `decoded_overlay_event`.
        let ev = unsafe { &*event };

        // Only the Interactive Graphics plane carries menu graphics.
        if ev.plane != BD_OVERLAY_IG {
            return;
        }

        // SAFETY: `ev` is a valid `BD_OVERLAY`; `decoded_overlay_event` only
        // dereferences `ev.palette`/`ev.img` after null-checking and walks the
        // RLE image within the `w`/`h` bounds the event declares.
        let Some(decoded) = (unsafe { decoded_overlay_event(ev) }) else {
            return;
        };

        // Apply the command. A DRAW with a malformed palette index returns an
        // error we deliberately ignore here (the frame just misses that draw);
        // there is no safe way to surface it across the C boundary.
        let _ = ctx.apply(&decoded);
    });
    if result.is_err() {
        // A panic was unwound to here and stopped; log via tracing without
        // touching the (possibly poisoned) overlay state.
        tracing::error!("overlay callback panicked; event dropped to preserve FFI soundness");
    }
}

/// Translate a borrowed raw `BD_OVERLAY` into an owned [`OverlayEvent`].
///
/// Returns `None` for commands we do not composite (e.g. an unknown `cmd`).
/// For `DRAW`, this copies the 256-entry palette and walks the RLE image —
/// which carries no explicit length — by counting `h` end-of-line markers,
/// producing an owned `Vec<RleElem>` so the borrow into libbluray's memory ends
/// before [`OverlayCtx::apply`] runs.
///
/// # Safety
///
/// `ev` must reference a valid `BD_OVERLAY`. For a `DRAW`, if `ev.palette` /
/// `ev.img` are non-null they must point to, respectively, at least
/// [`BD_OVERLAY_PALETTE_LEN`] palette entries and a NUL-line-terminated RLE
/// image spanning `ev.h` rows, as libbluray guarantees.
unsafe fn decoded_overlay_event(ev: &BdOverlay) -> Option<OverlayEvent> {
    match ev.cmd {
        BD_OVERLAY_INIT => Some(OverlayEvent::Init { w: ev.w, h: ev.h }),
        BD_OVERLAY_CLOSE => Some(OverlayEvent::Close),
        BD_OVERLAY_CLEAR => Some(OverlayEvent::Clear),
        BD_OVERLAY_HIDE => Some(OverlayEvent::Hide),
        BD_OVERLAY_FLUSH => Some(OverlayEvent::Flush),
        BD_OVERLAY_WIPE => Some(OverlayEvent::Wipe {
            x: ev.x,
            y: ev.y,
            w: ev.w,
            h: ev.h,
        }),
        BD_OVERLAY_DRAW => {
            if ev.palette.is_null() || ev.img.is_null() {
                return None;
            }
            // SAFETY: per the function's safety contract, a DRAW's `palette`
            // points to BD_OVERLAY_PALETTE_LEN valid entries. We copy them into
            // an owned, converted Vec so no borrow escapes.
            let raw_palette =
                unsafe { core::slice::from_raw_parts(ev.palette, BD_OVERLAY_PALETTE_LEN) };
            let palette = raw_palette
                .iter()
                .map(|e| crate::overlay::PaletteEntry {
                    y: e.y,
                    cr: e.cr,
                    cb: e.cb,
                    t: e.t,
                })
                .collect();

            // SAFETY: per the contract, `ev.img` points to a valid RLE image
            // spanning `ev.h` rows, each ended by a `len == 0` element. We walk
            // it element-by-element, copying into an owned Vec, stopping once
            // `ev.h` end-of-line markers have been seen (or a generous element
            // cap is hit, defending against a missing terminator).
            let img = unsafe { copy_rle_image(ev.img, ev.h) };
            Some(OverlayEvent::Draw {
                x: ev.x,
                y: ev.y,
                w: ev.w,
                h: ev.h,
                palette,
                img,
            })
        }
        _ => None,
    }
}

/// Copy a libbluray RLE image (`*const BD_PG_RLE_ELEM`, NUL-line-terminated)
/// into an owned `Vec<RleElem>`, reading exactly `rows` scanlines.
///
/// # Safety
///
/// `img` must be non-null and point to a valid RLE image of at least `rows`
/// scanlines, each terminated by a `len == 0` element, as libbluray
/// guarantees for a `DRAW`.
unsafe fn copy_rle_image(img: *const BdPgRleElem, rows: u16) -> Vec<crate::overlay::RleElem> {
    // Defensive cap: even a pathological row is bounded; 2^24 elements is far
    // beyond any real menu bitmap and stops a runaway walk if a terminator is
    // somehow missing.
    const MAX_ELEMS: usize = 1 << 24;

    let mut out = Vec::new();
    let mut lines_done = 0u32;
    let mut idx = 0usize;
    let target_rows = u32::from(rows);
    while lines_done < target_rows && idx < MAX_ELEMS {
        // SAFETY: `idx` advances one element at a time and we stop after
        // `target_rows` end-of-line markers, staying within the image libbluray
        // guarantees spans `rows` scanlines. Each read is of a valid element.
        let elem = unsafe { *img.add(idx) };
        out.push(crate::overlay::RleElem {
            len: elem.len,
            color: elem.color,
        });
        if elem.len == 0 {
            lines_done += 1;
        }
        idx += 1;
    }
    out
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod abi_tests {
    //! ABI guards: assert our `#[repr(C)]` mirrors match the C struct layouts
    //! documented in the installed libbluray headers. Any drift in libbluray's
    //! ABI — a reordered field, a changed type, inserted padding — fails a test
    //! here rather than corrupting memory when the trampoline dereferences a
    //! `BD_OVERLAY`. Offsets are the standard System V layout for the field
    //! sequence transcribed from `overlay.h` / `bluray.h`.

    use super::{
        BdEvent, BdOverlay, BdPgPaletteEntry, BdPgRleElem, BluRayClipInfo, BluRayTitleInfo,
    };
    use core::mem::{align_of, offset_of, size_of};

    #[test]
    fn bd_event_layout() {
        // BD_EVENT { uint32_t event; uint32_t param; } — 8 bytes, 4-aligned.
        assert_eq!(size_of::<BdEvent>(), 8, "BD_EVENT should be 8 bytes");
        assert_eq!(align_of::<BdEvent>(), 4, "BD_EVENT should be 4-aligned");
        assert_eq!(
            offset_of!(BdEvent, event),
            0,
            "BD_EVENT.event should be at offset 0"
        );
        assert_eq!(
            offset_of!(BdEvent, param),
            4,
            "BD_EVENT.param should be at offset 4"
        );
    }

    #[test]
    fn bd_pg_palette_entry_layout() {
        // BD_PG_PALETTE_ENTRY { uint8_t Y, Cr, Cb, T; } — 4 bytes, 1-aligned.
        assert_eq!(
            size_of::<BdPgPaletteEntry>(),
            4,
            "BD_PG_PALETTE_ENTRY should be 4 bytes"
        );
        assert_eq!(
            align_of::<BdPgPaletteEntry>(),
            1,
            "BD_PG_PALETTE_ENTRY should be 1-aligned"
        );
        assert_eq!(
            offset_of!(BdPgPaletteEntry, y),
            0,
            "Y should be at offset 0"
        );
        assert_eq!(
            offset_of!(BdPgPaletteEntry, cr),
            1,
            "Cr should be at offset 1"
        );
        assert_eq!(
            offset_of!(BdPgPaletteEntry, cb),
            2,
            "Cb should be at offset 2"
        );
        assert_eq!(
            offset_of!(BdPgPaletteEntry, t),
            3,
            "T should be at offset 3"
        );
    }

    #[test]
    fn bd_pg_rle_elem_layout() {
        // BD_PG_RLE_ELEM { uint16_t len; uint16_t color; } — 4 bytes, 2-aligned.
        assert_eq!(
            size_of::<BdPgRleElem>(),
            4,
            "BD_PG_RLE_ELEM should be 4 bytes"
        );
        assert_eq!(
            align_of::<BdPgRleElem>(),
            2,
            "BD_PG_RLE_ELEM should be 2-aligned"
        );
        assert_eq!(offset_of!(BdPgRleElem, len), 0, "len should be at offset 0");
        assert_eq!(
            offset_of!(BdPgRleElem, color),
            2,
            "color should be at offset 2"
        );
    }

    #[test]
    fn bd_overlay_layout() {
        // BD_OVERLAY:
        //   int64_t pts;                 // 0
        //   uint8_t plane, cmd, palette_update_flag; // 8, 9, 10
        //   (1 byte pad)                 // 11
        //   uint16_t x, y, w, h;         // 12, 14, 16, 18
        //   (4 bytes pad to 8-align ptr) // 20
        //   const BD_PG_PALETTE_ENTRY *palette; // 24
        //   const BD_PG_RLE_ELEM *img;          // 32
        // total: 40 bytes, 8-aligned (on LP64).
        assert_eq!(
            size_of::<BdOverlay>(),
            40,
            "BD_OVERLAY should be 40 bytes on LP64"
        );
        assert_eq!(align_of::<BdOverlay>(), 8, "BD_OVERLAY should be 8-aligned");
        assert_eq!(offset_of!(BdOverlay, pts), 0, "pts should be at offset 0");
        assert_eq!(
            offset_of!(BdOverlay, plane),
            8,
            "plane should be at offset 8"
        );
        assert_eq!(offset_of!(BdOverlay, cmd), 9, "cmd should be at offset 9");
        assert_eq!(
            offset_of!(BdOverlay, palette_update_flag),
            10,
            "palette_update_flag should be at offset 10"
        );
        assert_eq!(offset_of!(BdOverlay, x), 12, "x should be at offset 12");
        assert_eq!(offset_of!(BdOverlay, y), 14, "y should be at offset 14");
        assert_eq!(offset_of!(BdOverlay, w), 16, "w should be at offset 16");
        assert_eq!(offset_of!(BdOverlay, h), 18, "h should be at offset 18");
        assert_eq!(
            offset_of!(BdOverlay, palette),
            24,
            "palette pointer should be at offset 24"
        );
        assert_eq!(
            offset_of!(BdOverlay, img),
            32,
            "img pointer should be at offset 32"
        );
    }

    #[test]
    fn copy_rle_image_reads_exactly_the_requested_rows() {
        // Two rows of RLE: row 0 = [3,end], row 1 = [1,1,end]; a trailing
        // element after row 1 must NOT be read (we stop at `rows` terminators).
        let elems = [
            BdPgRleElem { len: 3, color: 7 },
            BdPgRleElem { len: 0, color: 0 }, // end row 0
            BdPgRleElem { len: 1, color: 1 },
            BdPgRleElem { len: 0, color: 0 }, // end row 1
            BdPgRleElem { len: 9, color: 9 }, // belongs to a 3rd row we won't read
        ];
        // SAFETY: `elems` is a valid array of >= 2 terminated scanlines; we ask
        // for exactly 2 rows, which `copy_rle_image` stops at.
        let copied = unsafe { super::copy_rle_image(elems.as_ptr(), 2) };
        assert_eq!(
            copied.len(),
            4,
            "reading 2 rows should copy exactly the 4 elements of those rows, got {}",
            copied.len()
        );
        assert_eq!(copied[0].len, 3, "first element length should be preserved");
        assert_eq!(
            copied[3].len, 0,
            "the last copied element should be the row-1 terminator"
        );
    }

    #[test]
    fn bd_clip_info_layout() {
        // BLURAY_CLIP_INFO (LP64):
        //   uint32_t pkt_count;                 // 0
        //   uint8_t  still_mode;                // 4
        //   (1 byte pad)                        // 5
        //   uint16_t still_time;                // 6
        //   uint8_t  {video,audio,pg,ig,sec_audio,sec_video}_stream_count; // 8..14
        //   (2 bytes pad to 8-align pointers)   // 14
        //   BLURAY_STREAM_INFO *x6;             // 16,24,32,40,48,56
        //   uint64_t start_time, in_time, out_time; // 64,72,80
        //   char     clip_id[6];                // 88
        //   (2 bytes pad to 8-align struct)     // 94
        // total: 96 bytes, 8-aligned. The size is load-bearing: it is the
        // array stride for clips[i], so a wrong size reads garbage clip_id.
        assert_eq!(
            size_of::<BluRayClipInfo>(),
            96,
            "BLURAY_CLIP_INFO should be 96 bytes on LP64 (the clips[i] array stride)"
        );
        assert_eq!(
            align_of::<BluRayClipInfo>(),
            8,
            "BLURAY_CLIP_INFO should be 8-aligned"
        );
        assert_eq!(
            offset_of!(BluRayClipInfo, pkt_count),
            0,
            "pkt_count should be at offset 0"
        );
        assert_eq!(
            offset_of!(BluRayClipInfo, still_time),
            6,
            "still_time should be at offset 6"
        );
        assert_eq!(
            offset_of!(BluRayClipInfo, video_streams),
            16,
            "the first stream pointer should be at offset 16"
        );
        assert_eq!(
            offset_of!(BluRayClipInfo, start_time),
            64,
            "start_time should be at offset 64"
        );
        assert_eq!(
            offset_of!(BluRayClipInfo, clip_id),
            88,
            "clip_id should be at offset 88 — the field the join reads"
        );
    }

    #[test]
    fn bd_title_info_layout() {
        // BLURAY_TITLE_INFO (LP64):
        //   uint32_t idx;            // 0
        //   uint32_t playlist;       // 4
        //   uint64_t duration;       // 8
        //   uint32_t clip_count;     // 16
        //   uint8_t  angle_count;    // 20
        //   (3 bytes pad)            // 21
        //   uint32_t chapter_count;  // 24
        //   uint32_t mark_count;     // 28
        //   BLURAY_CLIP_INFO *clips; // 32
        //   BLURAY_TITLE_CHAPTER *chapters; // 40
        //   BLURAY_TITLE_MARK *marks;       // 48
        //   uint8_t  mvc_base_view_r_flag;  // 56
        //   (7 bytes pad)            // 57
        // total: 64 bytes, 8-aligned.
        assert_eq!(
            size_of::<BluRayTitleInfo>(),
            64,
            "BLURAY_TITLE_INFO should be 64 bytes on LP64"
        );
        assert_eq!(
            align_of::<BluRayTitleInfo>(),
            8,
            "BLURAY_TITLE_INFO should be 8-aligned"
        );
        assert_eq!(
            offset_of!(BluRayTitleInfo, duration),
            8,
            "duration should be at offset 8"
        );
        assert_eq!(
            offset_of!(BluRayTitleInfo, clip_count),
            16,
            "clip_count should be at offset 16 — the iteration bound"
        );
        assert_eq!(
            offset_of!(BluRayTitleInfo, clips),
            32,
            "clips pointer should be at offset 32 — the array we stride into"
        );
    }

    #[test]
    fn clip_id_to_string_stops_at_nul() {
        // A typical 5-digit clip id NUL-padded to 6 bytes decodes without the
        // padding or any extension.
        let id = *b"00292\0";
        assert_eq!(
            super::clip_id_to_string(id),
            "00292",
            "a NUL-padded clip id should decode to its digits with no extension"
        );
    }

    #[test]
    fn clip_id_to_string_full_six_chars() {
        // A clip id that fills all six bytes (no NUL) is decoded in full.
        let id = *b"123456";
        assert_eq!(
            super::clip_id_to_string(id),
            "123456",
            "a full six-byte clip id should decode all six characters"
        );
    }
}
