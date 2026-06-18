// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! makemkvcon robot-mode title index for Reliquary.
//!
//! This crate enumerates a disc's titles via makemkvcon's machine-readable
//! `info` output and builds a [`TitleIndex`] keyed for source-filename lookup.
//! That index is the data the navigate join matches against: a menu button
//! activates a playlist, libbluray resolves the playlist to a clip
//! (`MMMMM.m2ts`), and the join looks that clip up here to recover the
//! makemkvcon title — and therefore the output `.mkv` makemkvcon would write.
//!
//! The crate is split into a pure [`parse`]r and a thin [`driver`]:
//!
//! - [`parse::parse`] consumes robot-mode text and is fully unit-testable
//!   against fixtures, with no subprocess and no disc.
//! - [`driver::title_index`] runs `makemkvcon -r --minlength=0 info <source>`
//!   and feeds its stdout to the parser.
//!
//! It drives makemkvcon as a subprocess and contains no FFI or `unsafe`, so it
//! sits outside the `reliquary-bluray` boundary.

pub mod driver;
pub mod parse;

pub use driver::{DriverError, Source, title_index, title_index_with_program};
pub use parse::{ParseError, TitleIndex, TitleRecord, parse};
