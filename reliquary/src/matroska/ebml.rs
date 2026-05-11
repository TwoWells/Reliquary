// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! EBML primitive encoder — variable-length integers and typed element writers.
//!
//! This module implements the binary encoding layer defined by RFC 8794.
//! It writes EBML elements to an [`io::Write`] output sequentially with no
//! buffering or seeking.  The caller is responsible for nesting master
//! elements correctly.
//!
//! All public functions return the number of bytes written, enabling
//! callers to track byte offsets for `SeekHead` and Cue generation.

use std::io;

/// Maximum VINT width in bytes.
const MAX_VINT_WIDTH: u8 = 8;

/// Void element ID.
const VOID_ID: u32 = 0xEC;

// ---------------------------------------------------------------------------
// VINT encoding
// ---------------------------------------------------------------------------

/// Writes a VINT (variable-length integer) to the output.
///
/// Used for data sizes.  The width marker bits are NOT part of the value
/// (stripped on read).  Uses the shortest encoding unless `min_width`
/// requests a wider one (for reserving patchable space).
///
/// # Errors
///
/// Returns [`io::Error`] if the value exceeds the maximum for an 8-byte
/// VINT, if `min_width` exceeds 8, or if writing fails.
pub fn write_vint(w: &mut impl io::Write, value: u64, min_width: u8) -> io::Result<usize> {
    let natural = vint_width(value)?;
    let width = natural.max(min_width);

    if width > MAX_VINT_WIDTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("VINT min_width {width} exceeds maximum of {MAX_VINT_WIDTH}"),
        ));
    }

    let max_for_width = vint_max(width);
    if value > max_for_width {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("value {value} exceeds maximum {max_for_width} for VINT width {width}"),
        ));
    }

    let mut buf = [0u8; 8];
    for i in 0..usize::from(width) {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "extracting individual bytes from a u64 — truncation is intentional"
        )]
        let byte = (value >> (i * 8)) as u8;
        buf[usize::from(width) - 1 - i] = byte;
    }
    buf[0] |= 1 << (8 - width);

    w.write_all(&buf[..usize::from(width)])?;
    Ok(usize::from(width))
}

/// Writes an element ID to the output.
///
/// Element IDs include the width marker bits as part of the value.
/// Always uses the canonical (shortest) encoding.
///
/// # Errors
///
/// Returns [`io::Error`] if the ID is invalid (zero, or does not match
/// any valid VINT width pattern) or if writing fails.
pub fn write_element_id(w: &mut impl io::Write, id: u32) -> io::Result<usize> {
    let width = element_id_width(id)?;
    let mut buf = [0u8; 4];
    for i in 0..width {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "extracting individual bytes from a u32 — truncation is intentional"
        )]
        let byte = (id >> (i * 8)) as u8;
        buf[width - 1 - i] = byte;
    }
    w.write_all(&buf[..width])?;
    Ok(width)
}

/// Writes the VINT encoding of "unknown size" (all data bits = 1).
///
/// # Errors
///
/// Returns [`io::Error`] if `width` is 0 or exceeds 8, or if writing
/// fails.
pub fn write_unknown_size(w: &mut impl io::Write, width: u8) -> io::Result<usize> {
    if width == 0 || width > MAX_VINT_WIDTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown-size width must be 1..={MAX_VINT_WIDTH}, got {width}"),
        ));
    }

    // Unknown size: marker bit set, all data bits = 1.
    // First byte = (marker << 1) - 1, remaining bytes = 0xFF.
    //   width 1: 0xFF,  width 2: 0x7F 0xFF,  width 8: 0x01 0xFF×7
    let mut buf = [0xFFu8; 8];
    let marker = 1u16 << (8 - width);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "marker is at most 0x100; (marker<<1)-1 always fits in u8"
    )]
    let first = ((marker << 1) - 1) as u8;
    buf[0] = first;

    w.write_all(&buf[..usize::from(width)])?;
    Ok(usize::from(width))
}

// ---------------------------------------------------------------------------
// Typed element writers
// ---------------------------------------------------------------------------

/// Writes a complete unsigned integer element (ID + size + value).
///
/// The value is encoded in the minimum number of big-endian bytes.
/// Value 0 is encoded as an empty element (size = 0).
///
/// # Errors
///
/// Returns [`io::Error`] if writing fails.
pub fn write_uint(w: &mut impl io::Write, id: u32, value: u64) -> io::Result<usize> {
    let data_len = uint_byte_len(value);
    let mut written = write_element_id(w, id)?;
    written += write_vint(w, data_len as u64, 1)?;
    if data_len > 0 {
        let bytes = value.to_be_bytes();
        w.write_all(&bytes[8 - data_len..])?;
        written += data_len;
    }
    Ok(written)
}

/// Writes a complete signed integer element (ID + size + value).
///
/// The value is encoded in the minimum number of two's-complement
/// big-endian bytes, preserving the sign bit.  Value 0 is encoded as
/// an empty element (size = 0).
///
/// # Errors
///
/// Returns [`io::Error`] if writing fails.
pub fn write_int(w: &mut impl io::Write, id: u32, value: i64) -> io::Result<usize> {
    let data_len = int_byte_len(value);
    let mut written = write_element_id(w, id)?;
    written += write_vint(w, data_len as u64, 1)?;
    if data_len > 0 {
        let bytes = value.to_be_bytes();
        w.write_all(&bytes[8 - data_len..])?;
        written += data_len;
    }
    Ok(written)
}

/// Writes a complete float element (8-byte IEEE 754).
///
/// # Errors
///
/// Returns [`io::Error`] if writing fails.
pub fn write_float(w: &mut impl io::Write, id: u32, value: f64) -> io::Result<usize> {
    let mut written = write_element_id(w, id)?;
    written += write_vint(w, 8, 1)?;
    w.write_all(&value.to_be_bytes())?;
    written += 8;
    Ok(written)
}

/// Writes a complete ASCII string element.
///
/// # Errors
///
/// Returns [`io::Error`] if writing fails.
pub fn write_string(w: &mut impl io::Write, id: u32, value: &str) -> io::Result<usize> {
    let data = value.as_bytes();
    let mut written = write_element_id(w, id)?;
    written += write_vint(w, data.len() as u64, 1)?;
    w.write_all(data)?;
    written += data.len();
    Ok(written)
}

/// Writes a complete UTF-8 string element.
///
/// # Errors
///
/// Returns [`io::Error`] if writing fails.
pub fn write_utf8(w: &mut impl io::Write, id: u32, value: &str) -> io::Result<usize> {
    // UTF-8 and ASCII string elements have the same wire format — raw bytes.
    write_string(w, id, value)
}

/// Writes a complete binary element.
///
/// # Errors
///
/// Returns [`io::Error`] if writing fails.
pub fn write_binary(w: &mut impl io::Write, id: u32, data: &[u8]) -> io::Result<usize> {
    let mut written = write_element_id(w, id)?;
    written += write_vint(w, data.len() as u64, 1)?;
    w.write_all(data)?;
    written += data.len();
    Ok(written)
}

/// Writes a complete date element (nanoseconds from 2001-01-01T00:00:00 UTC).
///
/// # Errors
///
/// Returns [`io::Error`] if writing fails.
pub fn write_date(w: &mut impl io::Write, id: u32, nanos: i64) -> io::Result<usize> {
    let mut written = write_element_id(w, id)?;
    written += write_vint(w, 8, 1)?;
    w.write_all(&nanos.to_be_bytes())?;
    written += 8;
    Ok(written)
}

/// Writes a master element header (ID + size).
///
/// The caller writes children, which must total exactly `content_size`
/// bytes.
///
/// # Errors
///
/// Returns [`io::Error`] if writing fails.
pub fn write_master(w: &mut impl io::Write, id: u32, content_size: u64) -> io::Result<usize> {
    let mut written = write_element_id(w, id)?;
    written += write_vint(w, content_size, 1)?;
    Ok(written)
}

/// Writes a master element header with unknown size.
///
/// The element is terminated implicitly by sibling/parent elements or
/// EOF.  Uses an 8-byte unknown-size VINT.
///
/// # Errors
///
/// Returns [`io::Error`] if writing fails.
pub fn write_master_unknown_size(w: &mut impl io::Write, id: u32) -> io::Result<usize> {
    let mut written = write_element_id(w, id)?;
    written += write_unknown_size(w, MAX_VINT_WIDTH)?;
    Ok(written)
}

/// Writes an EBML void element of the given total size (including ID
/// and size fields).
///
/// Used to reserve space for backpatching.
///
/// # Errors
///
/// Returns [`io::Error`] if `total_size` is too small to hold the Void
/// element header (minimum 2 bytes for 1-byte ID + 1-byte size) or if
/// writing fails.
pub fn write_void(w: &mut impl io::Write, total_size: usize) -> io::Result<usize> {
    // Void ID (0xEC) is always 1 byte.
    const ID_LEN: usize = 1;

    if total_size < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("void total_size must be at least 2, got {total_size}"),
        ));
    }

    let available = total_size - ID_LEN;
    let (size_width, padding) = find_void_size_width(available)?;

    write_element_id(w, VOID_ID)?;
    write_vint(w, padding as u64, size_width)?;

    let zeros = vec![0u8; padding];
    w.write_all(&zeros)?;

    Ok(total_size)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Returns the minimum VINT width needed to encode `value`.
fn vint_width(value: u64) -> io::Result<u8> {
    for width in 1..=MAX_VINT_WIDTH {
        if value <= vint_max(width) {
            return Ok(width);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("value {value} exceeds maximum VINT capacity"),
    ))
}

/// Returns the maximum value encodable in a VINT of the given width.
///
/// Max = 2^(7*width) - 2 (all-ones is reserved for unknown size).
const fn vint_max(width: u8) -> u64 {
    (1u64 << (7 * width)) - 2
}

/// Determines the byte width of an element ID from its value.
fn element_id_width(id: u32) -> io::Result<usize> {
    match id {
        0x80..=0xFE => Ok(1),
        0x4000..=0x7FFF => Ok(2),
        0x20_0000..=0x3F_FFFF => Ok(3),
        0x1000_0000..=0x1FFF_FFFF => Ok(4),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid element ID: {id:#010X}"),
        )),
    }
}

/// Returns the minimum number of bytes to encode an unsigned integer
/// value.  Value 0 returns 0 (empty element).
const fn uint_byte_len(value: u64) -> usize {
    if value == 0 {
        return 0;
    }
    ((u64::BITS - value.leading_zeros()) as usize).div_ceil(8)
}

/// Returns the minimum number of bytes to encode a signed integer
/// value in two's complement.  Value 0 returns 0 (empty element).
const fn int_byte_len(value: i64) -> usize {
    if value == 0 {
        return 0;
    }
    if value > 0 {
        // Need enough bytes so that the high bit is 0 (positive sign).
        let bits_needed = u64::BITS - value.cast_unsigned().leading_zeros();
        // +1 for sign bit, then ceil to bytes.
        (bits_needed as usize + 1).div_ceil(8)
    } else {
        // For negative: count leading ones in the two's complement.
        let bits_needed = u64::BITS - value.cast_unsigned().leading_ones();
        (bits_needed as usize + 1).div_ceil(8)
    }
}

/// Finds the VINT width for a Void element's size field such that
/// `size_vint_len + padding = available`, where the VINT encodes
/// `padding`.
fn find_void_size_width(available: usize) -> io::Result<(u8, usize)> {
    for width in 1..=MAX_VINT_WIDTH {
        let padding = available - usize::from(width);
        if padding as u64 <= vint_max(width) {
            return Ok((width, padding));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "void element too large",
    ))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // VINT encoding
    // -----------------------------------------------------------------------

    #[test]
    fn vint_value_0() {
        let mut buf = Vec::new();
        let n = write_vint(&mut buf, 0, 1).expect("write_vint(0)");
        assert_eq!(n, 1, "should write 1 byte");
        assert_eq!(buf, [0x80], "value 0 → 0x80");
    }

    #[test]
    fn vint_value_1() {
        let mut buf = Vec::new();
        let n = write_vint(&mut buf, 1, 1).expect("write_vint(1)");
        assert_eq!(n, 1, "should write 1 byte");
        assert_eq!(buf, [0x81], "value 1 → 0x81");
    }

    #[test]
    fn vint_value_126() {
        let mut buf = Vec::new();
        let n = write_vint(&mut buf, 126, 1).expect("write_vint(126)");
        assert_eq!(n, 1, "should write 1 byte");
        assert_eq!(buf, [0xFE], "value 126 → 0xFE");
    }

    #[test]
    fn vint_value_127_overflows_to_width_2() {
        let mut buf = Vec::new();
        let n = write_vint(&mut buf, 127, 1).expect("write_vint(127)");
        assert_eq!(n, 2, "should write 2 bytes");
        assert_eq!(buf, [0x40, 0x7F], "value 127 → width 2");
    }

    #[test]
    fn vint_value_16382() {
        let mut buf = Vec::new();
        let n = write_vint(&mut buf, 16382, 1).expect("write_vint(16382)");
        assert_eq!(n, 2, "should write 2 bytes");
        assert_eq!(buf, [0x7F, 0xFE], "value 16382 → max for width 2");
    }

    #[test]
    fn vint_value_16383_overflows_to_width_3() {
        let mut buf = Vec::new();
        let n = write_vint(&mut buf, 16383, 1).expect("write_vint(16383)");
        assert_eq!(n, 3, "should write 3 bytes");
        assert_eq!(buf, [0x20, 0x3F, 0xFF], "value 16383 → width 3");
    }

    #[test]
    fn vint_min_width_forcing() {
        let mut buf = Vec::new();
        let n = write_vint(&mut buf, 2, 4).expect("write_vint(2, min_width=4)");
        assert_eq!(n, 4, "should write 4 bytes");
        assert_eq!(buf, [0x10, 0x00, 0x00, 0x02], "value 2 forced to width 4");
    }

    #[test]
    fn unknown_size_width_1() {
        let mut buf = Vec::new();
        let n = write_unknown_size(&mut buf, 1).expect("unknown_size(1)");
        assert_eq!(n, 1, "should write 1 byte");
        assert_eq!(buf, [0xFF], "unknown size width 1 → 0xFF");
    }

    #[test]
    fn unknown_size_width_8() {
        let mut buf = Vec::new();
        let n = write_unknown_size(&mut buf, 8).expect("unknown_size(8)");
        assert_eq!(n, 8, "should write 8 bytes");
        assert_eq!(
            buf,
            [0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
            "unknown size width 8"
        );
    }

    // -----------------------------------------------------------------------
    // Element ID encoding
    // -----------------------------------------------------------------------

    #[test]
    fn element_id_1_byte() {
        let mut buf = Vec::new();
        let n = write_element_id(&mut buf, 0xA3).expect("SimpleBlock ID");
        assert_eq!(n, 1, "1-byte ID");
        assert_eq!(buf, [0xA3], "SimpleBlock ID → [0xA3]");
    }

    #[test]
    fn element_id_2_byte() {
        let mut buf = Vec::new();
        let n = write_element_id(&mut buf, 0x4DBB).expect("Seek ID");
        assert_eq!(n, 2, "2-byte ID");
        assert_eq!(buf, [0x4D, 0xBB], "Seek ID → [0x4D, 0xBB]");
    }

    #[test]
    fn element_id_4_byte() {
        let mut buf = Vec::new();
        let n = write_element_id(&mut buf, 0x1A45_DFA3).expect("EBML header ID");
        assert_eq!(n, 4, "4-byte ID");
        assert_eq!(
            buf,
            [0x1A, 0x45, 0xDF, 0xA3],
            "EBML header ID → [0x1A, 0x45, 0xDF, 0xA3]"
        );
    }

    #[test]
    fn element_id_invalid_zero() {
        let mut buf = Vec::new();
        let err = write_element_id(&mut buf, 0).expect_err("ID 0 should be invalid");
        assert_eq!(
            err.kind(),
            io::ErrorKind::InvalidInput,
            "invalid ID error kind"
        );
    }

    // -----------------------------------------------------------------------
    // Uint element
    // -----------------------------------------------------------------------

    #[test]
    fn uint_value_1() {
        let mut buf = Vec::new();
        let n = write_uint(&mut buf, 0x4286, 1).expect("EBMLVersion = 1");
        assert_eq!(n, 4, "2-byte ID + 1-byte size + 1-byte value");
        assert_eq!(buf, [0x42, 0x86, 0x81, 0x01], "EBMLVersion element");
    }

    #[test]
    fn uint_value_0() {
        let mut buf = Vec::new();
        let n = write_uint(&mut buf, 0xD7, 0).expect("TrackNumber = 0");
        assert_eq!(n, 2, "1-byte ID + 1-byte size(0)");
        assert_eq!(buf, [0xD7, 0x80], "uint 0 → empty body");
    }

    #[test]
    fn uint_value_256() {
        let mut buf = Vec::new();
        let n = write_uint(&mut buf, 0xD7, 256).expect("uint 256");
        assert_eq!(n, 4, "1-byte ID + 1-byte size + 2-byte value");
        assert_eq!(buf, [0xD7, 0x82, 0x01, 0x00], "uint 256");
    }

    // -----------------------------------------------------------------------
    // Int element
    // -----------------------------------------------------------------------

    #[test]
    fn int_value_0() {
        let mut buf = Vec::new();
        let n = write_int(&mut buf, 0xFB, 0).expect("int 0");
        assert_eq!(n, 2, "1-byte ID + 1-byte size(0)");
        assert_eq!(buf, [0xFB, 0x80], "int 0 → empty body");
    }

    #[test]
    fn int_positive() {
        let mut buf = Vec::new();
        let n = write_int(&mut buf, 0xFB, 127).expect("int 127");
        assert_eq!(n, 3, "1-byte ID + 1-byte size + 1-byte value");
        assert_eq!(buf, [0xFB, 0x81, 0x7F], "int 127");
    }

    #[test]
    fn int_positive_needs_sign_byte() {
        let mut buf = Vec::new();
        let n = write_int(&mut buf, 0xFB, 128).expect("int 128");
        assert_eq!(n, 4, "1-byte ID + 1-byte size + 2-byte value");
        assert_eq!(buf, [0xFB, 0x82, 0x00, 0x80], "int 128 needs sign padding");
    }

    #[test]
    fn int_negative() {
        let mut buf = Vec::new();
        let n = write_int(&mut buf, 0xFB, -1).expect("int -1");
        assert_eq!(n, 3, "1-byte ID + 1-byte size + 1-byte value");
        assert_eq!(buf, [0xFB, 0x81, 0xFF], "int -1 → 0xFF");
    }

    #[test]
    fn int_negative_128() {
        let mut buf = Vec::new();
        let n = write_int(&mut buf, 0xFB, -128).expect("int -128");
        assert_eq!(n, 3, "1-byte ID + 1-byte size + 1-byte value");
        assert_eq!(buf, [0xFB, 0x81, 0x80], "int -128 → 0x80");
    }

    #[test]
    fn int_negative_129() {
        let mut buf = Vec::new();
        let n = write_int(&mut buf, 0xFB, -129).expect("int -129");
        assert_eq!(n, 4, "1-byte ID + 1-byte size + 2-byte value");
        assert_eq!(buf, [0xFB, 0x82, 0xFF, 0x7F], "int -129 needs 2 bytes");
    }

    // -----------------------------------------------------------------------
    // Float element
    // -----------------------------------------------------------------------

    #[test]
    fn float_element() {
        let mut buf = Vec::new();
        let n = write_float(&mut buf, 0x4489, 1000.0).expect("Duration = 1000.0");
        assert_eq!(n, 11, "2-byte ID + 1-byte size + 8-byte value");
        assert_eq!(&buf[..3], [0x44, 0x89, 0x88], "float header: ID + size(8)");
        let float_bytes = &buf[3..11];
        let recovered = f64::from_be_bytes(float_bytes.try_into().expect("8 bytes"));
        assert!(
            (recovered - 1000.0).abs() < f64::EPSILON,
            "float round-trip: expected 1000.0, got {recovered}"
        );
    }

    // -----------------------------------------------------------------------
    // String element
    // -----------------------------------------------------------------------

    #[test]
    fn string_element() {
        let mut buf = Vec::new();
        let n = write_string(&mut buf, 0x4282, "matroska").expect("DocType");
        assert_eq!(n, 11, "2-byte ID + 1-byte size + 8-byte string");
        assert_eq!(&buf[..3], [0x42, 0x82, 0x88], "string header: ID + size(8)");
        assert_eq!(&buf[3..], b"matroska", "string body");
    }

    #[test]
    fn string_empty() {
        let mut buf = Vec::new();
        let n = write_string(&mut buf, 0x4282, "").expect("empty string");
        assert_eq!(n, 3, "2-byte ID + 1-byte size(0)");
        assert_eq!(buf, [0x42, 0x82, 0x80], "empty string element");
    }

    // -----------------------------------------------------------------------
    // Binary element
    // -----------------------------------------------------------------------

    #[test]
    fn binary_element() {
        let data = [0xDE, 0xAD, 0xBE, 0xEF];
        let mut buf = Vec::new();
        let n = write_binary(&mut buf, 0x63A2, &data).expect("CodecPrivate");
        assert_eq!(n, 7, "2-byte ID + 1-byte size + 4-byte data");
        assert_eq!(&buf[3..], data, "binary body matches input");
    }

    #[test]
    fn binary_empty() {
        let mut buf = Vec::new();
        let n = write_binary(&mut buf, 0x63A2, &[]).expect("empty binary");
        assert_eq!(n, 3, "2-byte ID + 1-byte size(0)");
    }

    // -----------------------------------------------------------------------
    // Date element
    // -----------------------------------------------------------------------

    #[test]
    fn date_element() {
        let mut buf = Vec::new();
        let nanos: i64 = 1_000_000_000; // 1 second after epoch
        let n = write_date(&mut buf, 0x4461, nanos).expect("DateUTC");
        assert_eq!(n, 11, "2-byte ID + 1-byte size + 8-byte value");
        let date_bytes: [u8; 8] = buf[3..11].try_into().expect("8 bytes");
        let recovered = i64::from_be_bytes(date_bytes);
        assert_eq!(recovered, nanos, "date round-trip");
    }

    // -----------------------------------------------------------------------
    // Master element
    // -----------------------------------------------------------------------

    #[test]
    fn master_element_empty() {
        let mut buf = Vec::new();
        let n = write_master(&mut buf, 0x1A45_DFA3, 0).expect("empty master");
        assert_eq!(n, 5, "4-byte ID + 1-byte size(0)");
        assert_eq!(
            buf,
            [0x1A, 0x45, 0xDF, 0xA3, 0x80],
            "EBML header master with size 0"
        );
    }

    #[test]
    fn master_element_with_children() {
        let mut buf = Vec::new();
        let header_len = write_master(&mut buf, 0x1A45_DFA3, 4).expect("master header");
        let child_len = write_uint(&mut buf, 0x4286, 1).expect("child uint");
        assert_eq!(header_len + child_len, 9, "master + child total");
        assert_eq!(child_len, 4, "child is exactly content_size bytes");
    }

    #[test]
    fn master_unknown_size() {
        let mut buf = Vec::new();
        let n = write_master_unknown_size(&mut buf, 0x1853_8067).expect("Segment unknown size");
        assert_eq!(n, 12, "4-byte ID + 8-byte unknown size VINT");
        assert_eq!(
            &buf[4..],
            [0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
            "unknown size VINT"
        );
    }

    // -----------------------------------------------------------------------
    // Void element
    // -----------------------------------------------------------------------

    #[test]
    fn void_element_4031() {
        let mut buf = Vec::new();
        let n = write_void(&mut buf, 4031).expect("void 4031");
        assert_eq!(n, 4031, "void total size");
        assert_eq!(buf.len(), 4031, "output length matches");
        assert_eq!(buf[0], 0xEC, "void element ID");
        let id_len = 1;
        let size_vint_len = 2;
        let padding_start = id_len + size_vint_len;
        assert!(
            buf[padding_start..].iter().all(|&b| b == 0),
            "all padding bytes should be zero"
        );
    }

    #[test]
    fn void_element_small() {
        let mut buf = Vec::new();
        let n = write_void(&mut buf, 3).expect("void 3");
        assert_eq!(n, 3, "void total size 3");
        assert_eq!(buf.len(), 3, "output length matches");
        assert_eq!(buf[0], 0xEC, "void element ID");
        assert_eq!(buf[2], 0x00, "1 byte of zero padding");
    }

    #[test]
    fn void_element_minimum() {
        let mut buf = Vec::new();
        let n = write_void(&mut buf, 2).expect("void 2");
        assert_eq!(n, 2, "void total size 2");
        assert_eq!(buf.len(), 2, "output length matches");
        assert_eq!(buf[0], 0xEC, "void element ID");
    }

    #[test]
    fn void_element_too_small() {
        let mut buf = Vec::new();
        let err = write_void(&mut buf, 1).expect_err("void size 1 should fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "too small error");
    }

    // -----------------------------------------------------------------------
    // Byte count accuracy
    // -----------------------------------------------------------------------

    type WriteFn = fn(&mut Vec<u8>) -> io::Result<usize>;

    #[test]
    fn return_values_match_output_length() {
        let cases: &[(&str, WriteFn)] = &[
            ("vint", |w| write_vint(w, 1000, 1)),
            ("element_id", |w| write_element_id(w, 0x1A45_DFA3)),
            ("unknown_size", |w| write_unknown_size(w, 4)),
            ("uint", |w| write_uint(w, 0x4286, 42)),
            ("int", |w| write_int(w, 0xFB, -42)),
            ("float", |w| write_float(w, 0x4489, 1000.0)),
            ("string", |w| write_string(w, 0x4282, "test")),
            ("utf8", |w| write_utf8(w, 0x4D80, "héllo")),
            ("binary", |w| write_binary(w, 0x63A2, &[1, 2, 3])),
            ("date", |w| write_date(w, 0x4461, 0)),
            ("master", |w| write_master(w, 0x1A45_DFA3, 100)),
            ("master_unknown", |w| {
                write_master_unknown_size(w, 0x1853_8067)
            }),
            ("void", |w| write_void(w, 50)),
        ];

        for (name, f) in cases {
            let mut buf = Vec::new();
            let n = f(&mut buf).expect(name);
            assert_eq!(
                n,
                buf.len(),
                "{name}: return value {n} ≠ output length {}",
                buf.len()
            );
        }
    }
}
