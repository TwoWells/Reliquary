// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! # Reliquary
//!
//! Media preservation backend. Tracks physical media collections (optical
//! discs, vinyl, tape), manages capture and extraction pipelines, and
//! records the full processing provenance of every output file.
//!
//! ## Concepts
//!
//! - **Collection** — an independent archive root with its own products
//!   and database index
//! - **Product** — a physical media product identified by UPC, described
//!   by a `meta.yaml` manifest
//! - **Provenance** — per-output records of how files were produced,
//!   including extraction settings, encode parameters, and experiments
//!   tried and rejected

pub mod disc;
