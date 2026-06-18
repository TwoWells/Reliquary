// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Pure parser for makemkvcon robot-mode `info` output.
//!
//! This module consumes the text emitted by
//! `makemkvcon -r --minlength=0 info <source>` and builds a [`TitleIndex`]. It
//! performs no I/O and does not spawn any subprocess, so it is unit-testable
//! against captured fixtures without makemkvcon or a disc.
//!
//! # Robot-mode format
//!
//! Robot mode emits one record per line. The records this parser cares about
//! are `TINFO` lines:
//!
//! ```text
//! TINFO:<title>,<attr>,<code>,"<value>"
//! ```
//!
//! where `<title>` is a zero-based title index, `<attr>` is a numeric
//! attribute id, `<code>` is a numeric message/format code, and `<value>` is a
//! quoted string. The attributes the index captures are the duration (9), the
//! source file name (16), and the output file name (27).
//!
//! All other record kinds (`MSG`, `CINFO`, `SINFO`, `DRV`, `TCOUNT`, …) are
//! ignored, as are lines that do not parse as a well-formed `TINFO` record.

use std::collections::HashMap;

use thiserror::Error;

/// makemkvcon attribute id for a title's duration (e.g. `0:02:41`).
const ATTR_DURATION: u32 = 9;
/// makemkvcon attribute id for a title's source file name (e.g. `00292.m2ts`).
const ATTR_SOURCE_FILE_NAME: u32 = 16;
/// makemkvcon attribute id for a title's output (mkv) file name.
const ATTR_OUTPUT_FILE_NAME: u32 = 27;

/// Errors returned while parsing robot-mode output.
#[derive(Debug, Error)]
pub enum ParseError {
    /// A `TINFO` line was structurally malformed (missing fields, a
    /// non-numeric `title`/`attr`/`code`, or an unterminated quoted value).
    ///
    /// Carries the 1-based line number and the offending line for diagnosis.
    #[error("malformed TINFO record on line {line}: {content:?}")]
    MalformedTinfo {
        /// 1-based line number of the offending record.
        line: usize,
        /// The full text of the offending line.
        content: String,
    },
}

/// A single title's joinable attributes from makemkvcon's `info` output.
///
/// Each field is `Option` because makemkvcon does not guarantee that every
/// attribute is present for every title: a title may legitimately omit any of
/// these. A field is `Some` only when the corresponding `TINFO` attribute was
/// observed for the title; it is `None` otherwise.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TitleRecord {
    /// The zero-based title index (the `<title>` field of the `TINFO` record).
    pub title_id: u32,
    /// The source file name (attr 16) — the BD clip this title was ripped
    /// from, e.g. `00292.m2ts`. For Warner Bros. extras this is a bare
    /// `.m2ts` name rather than a playlist. This is the join's match key.
    pub source_file_name: Option<String>,
    /// The duration (attr 9), as makemkvcon's `H:MM:SS` string, e.g.
    /// `0:02:41`. Kept verbatim rather than parsed into a typed duration.
    pub duration: Option<String>,
    /// The output file name (attr 27) — the `.mkv` makemkvcon would write for
    /// this title.
    pub output_file_name: Option<String>,
}

impl TitleRecord {
    /// Creates an empty record for the given title id, all attributes absent.
    const fn new(title_id: u32) -> Self {
        Self {
            title_id,
            source_file_name: None,
            duration: None,
            output_file_name: None,
        }
    }
}

/// A title index built from makemkvcon robot-mode `info` output.
///
/// The index is keyed for **source-filename lookup** ([`Self::by_source_file`]):
/// given a clip file name such as `00292.m2ts`, it returns the title that
/// makemkvcon ripped from that clip, whose [`TitleRecord::output_file_name`] is
/// the resulting mkv. That is the query the navigate join performs.
///
/// Records that carry no source file name (attr 16 absent) are retained — they
/// are reachable via [`Self::titles`] but cannot be resolved by source-file
/// lookup.
#[derive(Debug, Clone, Default)]
pub struct TitleIndex {
    /// All title records, in the order titles first appeared in the output.
    records: Vec<TitleRecord>,
    /// Source file name → index into `records`. Only titles with a source
    /// file name appear here.
    by_source: HashMap<String, usize>,
}

impl TitleIndex {
    /// Looks a title up by its source clip file name (attr 16), e.g.
    /// `00292.m2ts`.
    ///
    /// Returns the matching [`TitleRecord`], or `None` if no title was ripped
    /// from that clip.
    #[must_use]
    pub fn by_source_file(&self, source_file_name: &str) -> Option<&TitleRecord> {
        self.by_source
            .get(source_file_name)
            .and_then(|&i| self.records.get(i))
    }

    /// Returns all title records, in the order their titles first appeared in
    /// the output.
    #[must_use]
    pub fn titles(&self) -> &[TitleRecord] {
        &self.records
    }

    /// Returns the number of titles in the index.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.records.len()
    }

    /// Returns `true` if the index contains no titles.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// A parsed `TINFO` record: the three leading integer fields plus the decoded
/// (unquoted, unescaped) value.
struct TinfoRecord {
    title: u32,
    attr: u32,
    value: String,
}

/// Parses makemkvcon robot-mode `info` output into a [`TitleIndex`].
///
/// Lines that are not `TINFO` records (`MSG`, `CINFO`, `SINFO`, `DRV`,
/// `TCOUNT`, blank lines, …) are ignored. `TINFO` records for attributes the
/// index does not track are also ignored.
///
/// Titles are created lazily as their first `TINFO` record is seen, preserving
/// first-appearance order.
///
/// # Errors
///
/// Returns [`ParseError::MalformedTinfo`] if a line begins with the `TINFO:`
/// prefix but is not a well-formed record — for example a missing field, a
/// non-numeric `title`/`attr`/`code`, or an unterminated quoted value.
pub fn parse(robot_output: &str) -> Result<TitleIndex, ParseError> {
    let mut records: Vec<TitleRecord> = Vec::new();
    // Title id → position in `records`, so repeated attributes for one title
    // accumulate into a single record.
    let mut position: HashMap<u32, usize> = HashMap::new();

    for (zero_based, line) in robot_output.lines().enumerate() {
        let Some(body) = line.strip_prefix("TINFO:") else {
            // Not a TINFO record — every other record kind is ignored.
            continue;
        };

        let record = parse_tinfo_body(body).ok_or_else(|| ParseError::MalformedTinfo {
            line: zero_based + 1,
            content: line.to_string(),
        })?;

        let idx = *position.entry(record.title).or_insert_with(|| {
            records.push(TitleRecord::new(record.title));
            records.len() - 1
        });

        let Some(target) = records.get_mut(idx) else {
            // Unreachable: `idx` was produced from `records.len()` above.
            continue;
        };

        match record.attr {
            ATTR_DURATION => target.duration = Some(record.value),
            ATTR_SOURCE_FILE_NAME => target.source_file_name = Some(record.value),
            ATTR_OUTPUT_FILE_NAME => target.output_file_name = Some(record.value),
            // An attribute the index does not track — skip it.
            _ => {}
        }
    }

    // Build the source-filename lookup once all records are populated. Later
    // titles win on the rare event of a duplicate source name.
    let mut by_source: HashMap<String, usize> = HashMap::new();
    for (i, record) in records.iter().enumerate() {
        if let Some(name) = &record.source_file_name {
            by_source.insert(name.clone(), i);
        }
    }

    Ok(TitleIndex { records, by_source })
}

/// Parses the body of a `TINFO` line — everything after the `TINFO:` prefix.
///
/// The body is `<title>,<attr>,<code>,"<value>"`. The value is a quoted string
/// that may itself contain commas, so only the first three comma-separated
/// integer fields are split off; the remainder is treated as the quoted value
/// and decoded. Returns `None` on any structural defect.
fn parse_tinfo_body(body: &str) -> Option<TinfoRecord> {
    // Split off exactly the three leading integer fields. `splitn(4, ',')`
    // keeps the value (which may contain commas) intact as the final part.
    let mut fields = body.splitn(4, ',');
    let title: u32 = fields.next()?.trim().parse().ok()?;
    let attr: u32 = fields.next()?.trim().parse().ok()?;
    // `code` is parsed for validation but not retained.
    let _code: u32 = fields.next()?.trim().parse().ok()?;
    let raw_value = fields.next()?;

    let value = decode_quoted_value(raw_value)?;
    Some(TinfoRecord { title, attr, value })
}

/// Decodes a makemkvcon-quoted value.
///
/// The value is surrounded by double quotes. makemkvcon escapes an embedded
/// double quote by doubling it (`""`), so this reverses that: it strips the
/// outer quotes and collapses each `""` back to a single `"`. A lone interior
/// quote that is not part of a `""` pair, or a value not wrapped in quotes at
/// all, is treated as malformed (`None`).
fn decode_quoted_value(raw: &str) -> Option<String> {
    let raw = raw.trim();
    // Must be wrapped in quotes and have at least the two delimiters.
    let inner = raw.strip_prefix('"')?.strip_suffix('"')?;

    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '"' {
            // An interior quote is only valid as the first half of a `""`
            // escape pair; collapse the pair to a single quote.
            match chars.next() {
                Some('"') => out.push('"'),
                _ => return None,
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect() for assertions per project rules"
)]
mod tests {
    use super::{ParseError, parse};

    /// A multi-title fixture exercising attrs 9/16/27 plus ignored record kinds
    /// and ignored attributes. All names are generic placeholders — no real
    /// disc/movie titles appear here (public repo).
    const MULTI_TITLE: &str = r#"MSG:1005,0,1,"MakeMKV started","%1 started","MakeMKV"
DRV:0,2,999,12,"BD-ROM DRIVE","DISC_LABEL","/dev/sr0"
CINFO:1,6209,"Generic Disc"
TCOUNT:2
TINFO:0,2,0,"Main Feature"
TINFO:0,8,0,"16"
TINFO:0,9,0,"1:42:18"
TINFO:0,16,0,"00800.m2ts"
TINFO:0,27,0,"Feature_t00.mkv"
SINFO:0,0,1,6201,"Video"
TINFO:1,2,0,"Bonus"
TINFO:1,9,0,"0:02:41"
TINFO:1,16,0,"00292.m2ts"
TINFO:1,27,0,"Extra_t01.mkv"
"#;

    #[test]
    fn parses_multiple_titles_with_join_attrs() {
        let index = parse(MULTI_TITLE).expect("multi-title fixture should parse");
        assert_eq!(index.len(), 2, "fixture declares exactly two titles");

        let feature = index
            .by_source_file("00800.m2ts")
            .expect("title for 00800.m2ts should resolve by source file");
        assert_eq!(
            feature.title_id, 0,
            "00800.m2ts belongs to title 0 in the fixture"
        );
        assert_eq!(
            feature.duration.as_deref(),
            Some("1:42:18"),
            "attr 9 duration should be captured verbatim"
        );
        assert_eq!(
            feature.output_file_name.as_deref(),
            Some("Feature_t00.mkv"),
            "attr 27 output file name should be captured"
        );

        let bonus = index
            .by_source_file("00292.m2ts")
            .expect("title for 00292.m2ts should resolve by source file");
        assert_eq!(bonus.title_id, 1, "00292.m2ts belongs to title 1");
        assert_eq!(
            bonus.output_file_name.as_deref(),
            Some("Extra_t01.mkv"),
            "source-file lookup resolves to the right output mkv"
        );
        assert_eq!(
            bonus.duration.as_deref(),
            Some("0:02:41"),
            "source-file lookup resolves to the right duration"
        );
    }

    #[test]
    fn quoted_value_with_embedded_comma_is_preserved() {
        // A title name containing a comma must not be split on the comma. The
        // name field (attr 2) is not indexed, but its value must survive the
        // splitn(4) so the trailing attrs still parse — here we confirm the
        // embedded comma does not corrupt parsing of the same title.
        let input = "TINFO:0,2,0,\"Bonus, Extended\"\nTINFO:0,16,0,\"00010.m2ts\"\n";
        let index = parse(input).expect("embedded-comma value should parse");
        let title = index
            .by_source_file("00010.m2ts")
            .expect("title should resolve despite the comma in the name field");
        assert_eq!(
            title.title_id, 0,
            "the comma-bearing name belongs to the same title 0 as its source file"
        );
    }

    #[test]
    fn doubled_quotes_are_unescaped() {
        // makemkvcon escapes an embedded double quote by doubling it.
        let input = "TINFO:0,16,0,\"clip\"\"x.m2ts\"\n";
        let index = parse(input).expect("escaped-quote value should parse");
        assert_eq!(
            index.titles()[0].source_file_name.as_deref(),
            Some("clip\"x.m2ts"),
            "a doubled interior quote should decode to a single quote"
        );
    }

    #[test]
    fn title_missing_an_attr_yields_none() {
        // Title 0 has a source file but no duration and no output name.
        let input = "TINFO:0,16,0,\"00050.m2ts\"\n";
        let index = parse(input).expect("single-attr title should parse");
        let title = index
            .by_source_file("00050.m2ts")
            .expect("the title should still resolve by its source file");
        assert_eq!(
            title.duration, None,
            "an absent duration attribute should be represented as None"
        );
        assert_eq!(
            title.output_file_name, None,
            "an absent output-name attribute should be represented as None"
        );
    }

    #[test]
    fn unknown_and_malformed_non_tinfo_lines_are_ignored() {
        // Mix of unrelated record kinds, a blank line, and free text. None are
        // TINFO records, so all are skipped without error.
        let input = "\nMSG:5085,0,0,\"hash\"\nGarbage line with no prefix\nSINFO:0,0,1,0,\"x\"\nTINFO:0,16,0,\"00099.m2ts\"\n";
        let index = parse(input).expect("non-TINFO noise should be ignored");
        assert_eq!(
            index.len(),
            1,
            "only the single TINFO record should produce a title"
        );
        assert!(
            index.by_source_file("00099.m2ts").is_some(),
            "the lone TINFO record should still be indexed"
        );
    }

    #[test]
    fn wb_extra_bare_m2ts_source_resolves() {
        // Warner Bros. extras surface as a bare `.m2ts` source rather than via
        // a playlist; the join must still resolve them. (Generic clip number.)
        let input = "TINFO:3,9,0,\"0:00:45\"\nTINFO:3,16,0,\"00531.m2ts\"\nTINFO:3,27,0,\"Extra_t03.mkv\"\n";
        let index = parse(input).expect("bare-m2ts extra should parse");
        let extra = index
            .by_source_file("00531.m2ts")
            .expect("a bare .m2ts extra source should resolve");
        assert_eq!(
            extra.output_file_name.as_deref(),
            Some("Extra_t03.mkv"),
            "the bare-m2ts extra should map to its output mkv"
        );
    }

    #[test]
    fn empty_input_yields_empty_index() {
        let index = parse("").expect("empty input should parse to an empty index");
        assert!(index.is_empty(), "empty input should yield no titles");
        assert_eq!(index.len(), 0, "empty index length should be zero");
        assert!(
            index.by_source_file("00001.m2ts").is_none(),
            "no source file should resolve from an empty index"
        );
    }

    #[test]
    fn malformed_tinfo_non_numeric_field_errors() {
        // A TINFO line whose `attr` field is not an integer is malformed.
        let input = "TINFO:0,notanumber,0,\"00001.m2ts\"\n";
        let err = parse(input).expect_err("a non-numeric attr field should error");
        assert!(
            matches!(err, ParseError::MalformedTinfo { line: 1, .. }),
            "the malformed record should report line 1, got {err:?}"
        );
    }

    #[test]
    fn malformed_tinfo_unterminated_quote_errors() {
        // The value is not wrapped in a closing quote.
        let input = "TINFO:0,16,0,\"00001.m2ts\n";
        let err = parse(input).expect_err("an unterminated quoted value should error");
        assert!(
            matches!(err, ParseError::MalformedTinfo { .. }),
            "an unterminated quote should be a MalformedTinfo error, got {err:?}"
        );
    }

    #[test]
    fn malformed_tinfo_missing_value_field_errors() {
        // Only three fields — the quoted value is absent entirely.
        let input = "TINFO:0,16,0\n";
        let err = parse(input).expect_err("a record missing its value should error");
        assert!(
            matches!(err, ParseError::MalformedTinfo { .. }),
            "a missing value field should be a MalformedTinfo error, got {err:?}"
        );
    }

    #[test]
    fn repeated_source_name_last_title_wins() {
        // Two titles claiming the same source clip: the later one wins the
        // lookup, but both remain reachable via titles().
        let input = "TINFO:0,16,0,\"00001.m2ts\"\nTINFO:0,27,0,\"A_t00.mkv\"\nTINFO:1,16,0,\"00001.m2ts\"\nTINFO:1,27,0,\"B_t01.mkv\"\n";
        let index = parse(input).expect("duplicate source names should parse");
        assert_eq!(index.len(), 2, "both titles should be retained");
        let resolved = index
            .by_source_file("00001.m2ts")
            .expect("the shared source name should resolve");
        assert_eq!(
            resolved.title_id, 1,
            "the later title should win a duplicate source-name lookup"
        );
    }
}
