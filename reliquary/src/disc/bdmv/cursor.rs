// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Shared byte-level cursor for binary format parsers.
//!
//! Used by the MPLS, CLPI, and IG parsers to walk big-endian binary
//! structures with bounds checking.

use thiserror::Error;

// ── Errors ──────────────────────────────────────────────────────────────

/// Error from a bounds-checked cursor read.
#[derive(Debug, Error)]
#[error("unexpected end of data at offset {offset} (need {needed} bytes, have {available})")]
pub struct CursorError {
    /// Byte offset where the read was attempted.
    pub offset: usize,
    /// Number of bytes requested.
    pub needed: usize,
    /// Number of bytes actually available from that offset.
    pub available: usize,
}

// ── Cursor ──────────────────────────────────────────────────────────────

/// A cursor over a byte slice with bounds-checked reads.
pub struct Cursor<'a> {
    data: &'a [u8],
    /// Current read position.
    pub pos: usize,
}

#[allow(
    clippy::missing_const_for_fn,
    reason = "internal helper — const adds no value"
)]
impl<'a> Cursor<'a> {
    /// Creates a new cursor at position 0.
    #[must_use]
    pub const fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Returns the number of bytes remaining from the current position.
    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    /// Returns an error if fewer than `n` bytes remain.
    ///
    /// # Errors
    ///
    /// Returns [`CursorError`] if the remaining data is shorter than `n`.
    pub fn ensure(&self, n: usize) -> Result<(), CursorError> {
        if self.remaining() < n {
            return Err(CursorError {
                offset: self.pos,
                needed: n,
                available: self.remaining(),
            });
        }
        Ok(())
    }

    /// Advances the cursor by `n` bytes.
    ///
    /// # Errors
    ///
    /// Returns [`CursorError`] if fewer than `n` bytes remain.
    pub fn skip(&mut self, n: usize) -> Result<(), CursorError> {
        self.ensure(n)?;
        self.pos += n;
        Ok(())
    }

    /// Reads a single byte and advances the cursor.
    ///
    /// # Errors
    ///
    /// Returns [`CursorError`] if no bytes remain.
    pub fn read_u8(&mut self) -> Result<u8, CursorError> {
        self.ensure(1)?;
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }

    /// Reads a big-endian `u16` and advances the cursor.
    ///
    /// # Errors
    ///
    /// Returns [`CursorError`] if fewer than 2 bytes remain.
    pub fn read_u16(&mut self) -> Result<u16, CursorError> {
        self.ensure(2)?;
        let v = u16::from_be_bytes([self.data[self.pos], self.data[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    /// Reads a big-endian `u32` and advances the cursor.
    ///
    /// # Errors
    ///
    /// Returns [`CursorError`] if fewer than 4 bytes remain.
    pub fn read_u32(&mut self) -> Result<u32, CursorError> {
        self.ensure(4)?;
        let v = u32::from_be_bytes([
            self.data[self.pos],
            self.data[self.pos + 1],
            self.data[self.pos + 2],
            self.data[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(v)
    }

    /// Reads `n` bytes as a slice and advances the cursor.
    ///
    /// # Errors
    ///
    /// Returns [`CursorError`] if fewer than `n` bytes remain.
    pub fn read_bytes(&mut self, n: usize) -> Result<&'a [u8], CursorError> {
        self.ensure(n)?;
        let slice = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    /// Moves the cursor to an absolute position.
    ///
    /// # Errors
    ///
    /// Returns [`CursorError`] if the position is past the end of the data.
    pub fn seek(&mut self, new_pos: usize) -> Result<(), CursorError> {
        if new_pos > self.data.len() {
            return Err(CursorError {
                offset: new_pos,
                needed: 0,
                available: 0,
            });
        }
        self.pos = new_pos;
        Ok(())
    }
}
