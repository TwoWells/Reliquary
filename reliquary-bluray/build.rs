// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Build script for `reliquary-bluray`.
//!
//! Emits the link directive for the system `libbluray` shared library. We link
//! directly with `-lbluray` rather than probing `pkg-config`, both to avoid a
//! build dependency and because `pkg-config` is unavailable in some build
//! environments. Consumers that install `libbluray` in a non-standard prefix
//! can point the linker at it with the usual `RUSTFLAGS`/`-L` mechanisms.

fn main() {
    // Link against the system libbluray (e.g. /usr/lib/libbluray.so.3).
    println!("cargo:rustc-link-lib=bluray");

    // Only re-run when this script itself changes; the link directive is static.
    println!("cargo:rerun-if-changed=build.rs");
}
