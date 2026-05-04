# Reliquary Agent Context

This file serves as the single point of truth for AI agents working on the Reliquary project.

## Project Grounding
- **Project Goal:** Physical media preservation — track collections, manage capture/extraction, record provenance.
- **Repository:** `TwoWells/Reliquary` on GitHub.
- **Config:** `@./Cargo.toml`
- **Dependency Policy:** `@./deny.toml`
- **Planning:** `~/Projects/ReliquaryInternal`

## Workspace

Four crates, all published on crates.io:

- `reliquary/` — core library (data model, schema, disc analysis, indexing, deployment)
- `reliquary-cli/` — CLI binary (`add`, `inspect`, `identify`, `scribe`, `deploy`, etc.)
- `reliquary-tui/` — TUI frontend
- `reliquary-web/` — web frontend

The library crate produces data. CLI/TUI/web crates own presentation and interaction.

## Coding Standards
- **Edition:** Rust 2024.
- **Safety:** `unsafe` code is strictly forbidden (`forbid(unsafe_code)`).
- **Error Handling:** Use `anyhow` for application logic and `thiserror` for library errors.
- **Strict Denials:** Do NOT use `unwrap()`, `panic!()`, `todo!()`, `unimplemented!()`, `dbg!()`, `println!()`, or `eprintln!()`. Use proper error handling and the `tracing` crate for logging. `expect()` is denied in production code but allowed in `#[cfg(test)]` modules — prefer `expect("reason")` over `anyhow` workarounds in tests.
- **Assertions:** All `assert!()`, `assert_eq!()`, and `assert_ne!()` calls must include a message explaining what failed.
- **Imports:** No wildcard imports (`use crate::*`).
- **Formatting:** Code must be formatted with `rustfmt`.
- **Linting:** Must pass `cargo clippy` with `pedantic`, `nursery`, and `cargo` groups enabled. Every `#[allow(...)]` must include a `reason` string.

## Quality Standards
- **License Compliance:** All new dependencies MUST have permissive licenses (MIT, Apache-2.0, etc.) as specified in `@./deny.toml`. Reliquary is dual-licensed under AGPL-3.0-or-later and a commercial license.
- **Copyright Headers:** Every `.rs` file must start with the SPDX header (enforced by pre-commit hook):
  ```
  // SPDX-License-Identifier: AGPL-3.0-or-later
  // Copyright (C) 2026 Mark Wells <contact@markwells.dev>
  ```
- **Documentation:** All public APIs must have documentation comments.
- **Testing:** All new features must include tests.

## Setup
- **First time:** `make setup` — configures git hooks and checks for required cargo tools.
- **Required tools:** cargo-deny, cargo-machete, cargo-nextest, cargo-semver-checks, cargo-mutants.
- **Install tools:** `cargo binstall cargo-deny cargo-machete cargo-nextest cargo-semver-checks cargo-mutants`

## Development Commands
- **Build:** `cargo build`
- **Check (full):** `make check` — format, lint, deny, machete, and test in one pass.
- **Test (all):** `make test`
- **Test (filtered):** `make test T=<filter>`
- **Test (repeat):** `make test T=<filter> N=<count>`
- **Lint:** `cargo clippy --workspace --tests -- -D warnings`
- **Format:** `cargo fmt`

## Release Workflow
- **Patch Release:** `make release-patch`
- **Minor Release:** `make release-minor`
- **Major Release:** `make release-major`
- **Custom Version:** `make release V=x.y.z`

Release runs: check → semver-checks → mutation tests (library crate) → commit → tag.
