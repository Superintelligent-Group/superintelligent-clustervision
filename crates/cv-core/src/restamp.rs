//! The one session-id restamp for Claude JSONL records, as byte surgery.
//!
//! Every record-level reshape of a Claude transcript (`prune`, `rewind`, `adopt`) writes a NEW
//! session: the same lines with a new `sessionId` (and `session_id`, the other spelling Claude Code
//! writes on most records since ~2.1.25x). This module replaces exactly those two top-level string
//! values, located by `RawValue` offsets, and leaves every other byte of the line as it was — key
//! order, whitespace, number spellings, non-ASCII text. A `sessionId` nested inside content (a tool
//! output quoting a transcript) is content and is not touched. The property, pinned below:
//! `restamp(restamp(x, a), b) == x` when `b` is the id `x` carried.

use serde::Deserialize;
use serde_json::value::RawValue;

#[derive(Deserialize)]
struct TopIds<'a> {
    #[serde(borrow, rename = "sessionId")]
    session_id: Option<&'a RawValue>,
    #[serde(borrow, rename = "session_id")]
    session_id_snake: Option<&'a RawValue>,
}

/// Raw transform output ids also become filenames/resource directory names. Keep them within the
/// selected output directory; this does not restrict ids stored in arbitrary IR or restamped text.
pub(crate) fn validate_output_id(id: &str) -> anyhow::Result<()> {
    let mut components = std::path::Path::new(id).components();
    anyhow::ensure!(
        !id.is_empty()
            && !id.contains(['/', '\\', '\0'])
            && matches!(components.next(), Some(std::path::Component::Normal(_)))
            && components.next().is_none(),
        "new session id must be a nonempty filename component, not a path: {id:?}"
    );
    Ok(())
}

/// Restamp one transcript line: replace the top-level `sessionId` / `session_id` string values with
/// `new_id`, leaving every other byte as it was. `None` when the line carries no top-level session
/// id (non-JSON, or a record without one): the caller keeps it verbatim.
pub fn restamp_line(line: &str, new_id: &str) -> Option<String> {
    let ids: TopIds = serde_json::from_str(line).ok()?;
    let mut spans: Vec<(usize, usize)> = [ids.session_id, ids.session_id_snake]
        .into_iter()
        .flatten()
        .map(RawValue::get)
        .filter(|raw| raw.starts_with('"'))
        .map(|raw| {
            let start = raw.as_ptr() as usize - line.as_ptr() as usize;
            (start, start + raw.len())
        })
        .collect();
    if spans.is_empty() {
        return None;
    }
    spans.sort();
    let quoted = serde_json::to_string(new_id).expect("a string serializes");
    let mut out = String::with_capacity(line.len() + 8);
    let mut at = 0;
    for (s, e) in spans {
        out.push_str(&line[at..s]);
        out.push_str(&quoted);
        at = e;
    }
    out.push_str(&line[at..]);
    Some(out)
}

/// Stream a captured UTF-8 record with the same top-level id surgery as `restamp_line`.
/// The producer chooses UTF-8/JSON acceptance and owns staging of partial writes. Malformed JSON
/// and duplicate known id fields are copied verbatim, matching the borrowed serde struct above.
#[cfg(test)]
pub(crate) fn write_restamped_source(
    source: &crate::json::JsonSource,
    file: &std::fs::File,
    offset: u64,
    len: u64,
    new_id: &str,
    output: &mut impl std::io::Write,
) -> anyhow::Result<bool> {
    use crate::harness::store::document::InvalidStruct;
    source.check_metadata(file.metadata()?)?;
    source.check()?;
    let mut spans = if len == 0 {
        Vec::new()
    } else {
        match source.read_range_syntax(offset, len, source_id_spans) {
            Ok(spans) => spans,
            Err(e) if e.is::<crate::json::InvalidJson>() || e.is::<InvalidStruct>() => Vec::new(),
            Err(e) => return Err(e),
        }
    };
    write_restamped_spans(source, file, offset, len, new_id, &mut spans, output)
}

/// Selected during strict producer validation; preserve derived-struct id acceptance.
pub(crate) fn source_id_spans(view: crate::json::JsonView<'_>) -> anyhow::Result<Vec<(u64, u64)>> {
    use crate::harness::store::document::{Fields, InvalidStruct};
    let ids = match Fields::parse_struct(view, &["sessionId", "session_id"]) {
        Ok(Some(ids)) => ids,
        Ok(None) => return Ok(Vec::new()),
        Err(e) if e.is::<InvalidStruct>() => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut spans = Vec::new();
    for key in ["sessionId", "session_id"] {
        if let Some(value) = ids.get(key) {
            if value.is_string()? {
                let (offset, len) = value
                    .source_range()?
                    .ok_or_else(|| anyhow::anyhow!("expected source id range"))?;
                spans.push((offset, offset + len));
            }
        }
    }
    Ok(spans)
}

/// Copy captured, already selected id spans without repeating source JSON scans.
pub(crate) fn write_restamped_spans(
    source: &crate::json::JsonSource,
    file: &std::fs::File,
    offset: u64,
    len: u64,
    new_id: &str,
    spans: &mut [(u64, u64)],
    output: &mut impl std::io::Write,
) -> anyhow::Result<bool> {
    use crate::harness::store::jsonl::ranges::{self, Range};
    source.check_metadata(file.metadata()?)?;
    source.check()?;
    spans.sort_unstable();
    let stamped = !spans.is_empty();
    let mut at = offset;
    for &(start, end) in spans.iter() {
        std::io::copy(
            &mut ranges::reader(
                file,
                Range {
                    offset: at,
                    len: start - at,
                },
            )?,
            output,
        )?;
        serde_json::to_writer(&mut *output, new_id)?;
        at = end;
    }
    std::io::copy(
        &mut ranges::reader(
            file,
            Range {
                offset: at,
                len: offset + len - at,
            },
        )?,
        output,
    )?;
    source.check_metadata(file.metadata()?)?;
    source.check()?;
    Ok(stamped)
}

/// Counts from the shared streaming transcript surgery; no transcript-sized body is retained.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RestampCounts {
    pub lines: usize,
    pub stamped: usize,
    pub bytes: usize,
}

/// Restamp a captured UTF-8 transcript through the existing line/source surgery. Small physical
/// rows reuse framing scratch; large rows remain positioned ranges. Malformed JSON passes through,
/// but invalid UTF-8 is fatal, matching `read_to_string` followed by `restamp`. Callers must stage
/// output because a later invalid row or source mutation can fail after earlier writes.
pub(crate) fn write_restamped_transcript(
    source: &crate::json::JsonSource,
    file: &std::fs::File,
    new_id: &str,
    output: &mut impl std::io::Write,
) -> anyhow::Result<RestampCounts> {
    use crate::harness::store::jsonl::ranges::{self, Range};
    use std::io::Write;
    struct Counted<'a, W> {
        output: &'a mut W,
        bytes: usize,
    }
    impl<W: Write> Write for Counted<'_, W> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let count = self.output.write(bytes)?;
            self.bytes = self
                .bytes
                .checked_add(count)
                .ok_or_else(|| std::io::Error::other("restamped size overflow"))?;
            Ok(count)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.output.flush()
        }
    }
    source.check_metadata(file.metadata()?)?;
    source.check()?;
    let mut output = Counted { output, bytes: 0 };
    let mut counts = RestampCounts::default();
    let mut rows = ranges::records(
        file,
        Range {
            offset: 0,
            len: source.len(),
        },
    )?;
    rows.keep_small_records(64 * 1024);
    while let Some(row) = rows.next() {
        let (range, newline) = row?;
        if let Some(bytes) = rows.small_record() {
            let line = std::str::from_utf8(bytes).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "stream did not contain valid UTF-8")
            })?;
            counts.lines += usize::from(!line.trim().is_empty());
            if let Some(restamped) = restamp_line(line, new_id) {
                counts.stamped += 1;
                output.write_all(restamped.as_bytes())?;
            } else {
                output.write_all(bytes)?;
            }
        } else {
            if !ranges::valid_utf8(&mut ranges::reader(file, range)?)? {
                return Err(
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "stream did not contain valid UTF-8").into(),
                );
            }
            counts.lines += usize::from(ranges::trim(file, range)?.len != 0);
            let mut spans = match source.read_range_syntax(range.offset, range.len, source_id_spans) {
                Ok(spans) => spans,
                Err(error)
                    if error.is::<crate::json::InvalidJson>()
                        || error.is::<crate::harness::store::document::InvalidStruct>() =>
                {
                    Vec::new()
                }
                Err(error) => return Err(error),
            };
            counts.stamped += usize::from(write_restamped_spans(
                source,
                file,
                range.offset,
                range.len,
                new_id,
                &mut spans,
                &mut output,
            )?);
        }
        if newline {
            output.write_all(b"\n")?;
        }
    }
    source.check_metadata(file.metadata()?)?;
    source.check()?;
    counts.bytes = output.bytes;
    Ok(counts)
}

/// A restamped transcript and what the restamp touched.
pub struct Restamped {
    pub body: String,
    /// Non-blank lines seen.
    pub lines: usize,
    /// Lines that carried a top-level id and were restamped.
    pub stamped: usize,
}

/// Restamp a whole transcript. Line endings and blank lines are preserved as they were.
pub fn restamp(text: &str, new_id: &str) -> Restamped {
    let mut body = String::with_capacity(text.len() + 64);
    let (mut lines, mut stamped) = (0, 0);
    for piece in text.split_inclusive('\n') {
        let (line, end) = match piece.strip_suffix('\n') {
            Some(l) => (l, "\n"),
            None => (piece, ""),
        };
        lines += usize::from(!line.trim().is_empty());
        match restamp_line(line, new_id) {
            Some(new) => {
                stamped += 1;
                body.push_str(&new);
            }
            None => body.push_str(line),
        }
        body.push_str(end);
    }
    Restamped { body, lines, stamped }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_writer_matches_old_restamp_bytes_and_counts_for_mixed_rows() {
        let world = cv_testkit::World::new("streaming-restamp-transcript");
        let path = world.base.join("rows.jsonl");
        let small = concat!(
            "\r\n  \n",
            " { \"sessionId\" : \"dead\", \"n\":1.50, \"nested\":{\"sessionId\":\"keep\"} }\r\n",
            "{\"sessionId\":\"dead\",\"sessionId\":\"duplicate\"}\n",
            "not json ☃\n[\"dead\",\"snake\"]\n",
            "{\"session_id\":12}\n{\"sessionId\":\"dead\",\"ignored\":1e9999}"
        );
        let large = format!(
            "{small}\n{{\"sessionId\":\"dead\",\"text\":\"{}\",\"session_id\":\"dead\"}}\r\n{}\n{} trailing malformed ☃\n{{\"sessionId\":\"dead\"}}",
            "λ\\n".repeat(30000), " ".repeat(100000), "x".repeat(100000)
        );
        for original in ["", small, large.as_str()] {
            std::fs::write(&path, original).unwrap();
            let source = crate::json::JsonSource::capture(&path).unwrap();
            let file = std::fs::File::open(&path).unwrap();
            let expected = restamp(original, "new\"id\\λ");
            let mut actual = Vec::new();
            let counts = write_restamped_transcript(&source, &file, "new\"id\\λ", &mut actual).unwrap();
            assert_eq!(actual, expected.body.as_bytes());
            assert_eq!(
                counts,
                RestampCounts {
                    lines: expected.lines,
                    stamped: expected.stamped,
                    bytes: expected.body.len()
                }
            );
        }
    }

    #[test]
    fn source_writer_matches_borrowed_struct_bytes_and_rejections() {
        let world = cv_testkit::World::new("source-restamp");
        let path = world.base.join("rows.jsonl");
        for line in [
            "",
            "not json",
            "null",
            "{}",
            r#"{"sessionId":"x","session_id":"x","nested":{"sessionId":"keep"}}"#,
            r#" { "sessionId" : "x", "n":1.50, "session_id":true } "#,
            r#"{"sessionId":"x","sessionId":"duplicate"}"#,
            r#"{"sessionId":null,"session_id":12}"#,
            r#"["x","y"]"#,
            r#"["x","y","extra"]"#,
            r#"{"ignored":1e9999,"sessionId":"x"}"#,
        ] {
            std::fs::write(&path, line).unwrap();
            let source = crate::json::JsonSource::capture(&path).unwrap();
            let file = std::fs::File::open(&path).unwrap();
            let mut output = Vec::new();
            let stamped = write_restamped_source(&source, &file, 0, line.len() as u64, "new\"☃", &mut output).unwrap();
            let expected = restamp_line(line, "new\"☃");
            assert_eq!(stamped, expected.is_some(), "{line}");
            assert_eq!(
                String::from_utf8(output).unwrap(),
                expected.unwrap_or_else(|| line.to_owned()),
                "{line}"
            );
        }
    }

    #[test]
    fn restamp_line_ignores_lines_without_a_top_level_id() {
        assert_eq!(restamp_line("not json", "y"), None);
        assert_eq!(restamp_line(r#"{"a":{"sessionId":"x"}}"#, "y"), None);
        assert_eq!(restamp_line(r#"{"sessionId":null}"#, "y"), None);
        assert_eq!(
            restamp_line(r#"{"sessionId":"x","session_id":"x","k":"x"}"#, "y").unwrap(),
            r#"{"sessionId":"y","session_id":"y","k":"x"}"#
        );
    }

    /// Only the two top-level ids move; spacing, number spellings, nested ids and non-ASCII text
    /// are the source's bytes, so restamping back gives the source.
    #[test]
    fn restamp_is_byte_surgery_and_undoes_itself() {
        let src = concat!(
            "{\"parentUuid\":null,\"type\":\"user\",\"message\":{\"content\":\"brief — ☃\"},\"uuid\":\"u1\",\"sessionId\":\"dead\"}\n",
            "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"{\\\"sessionId\\\":\\\"dead\\\"}\"}]},\"uuid\":\"u2\",\"sessionId\": \"dead\" , \"n\":1.50, \"session_id\":\"dead\"}\n",
            "\n",
            "{\"type\":\"summary\",\"summary\":\"no session id here\"}"
        );
        let r = restamp(src, "live");
        assert_eq!((r.lines, r.stamped), (3, 2));
        assert_eq!(r.body.matches("live").count(), 3, "two ids on line 2, one on line 1");
        assert!(
            r.body.contains("{\\\"sessionId\\\":\\\"dead\\\"}"),
            "nested id is content"
        );
        assert!(r
            .body
            .contains("\"sessionId\": \"live\" , \"n\":1.50, \"session_id\":\"live\"}"));
        assert!(r.body.contains("brief — ☃"));
        assert!(r.body.ends_with("\"no session id here\"}"));
        assert_eq!(restamp(&r.body, "dead").body, src);
    }
}
