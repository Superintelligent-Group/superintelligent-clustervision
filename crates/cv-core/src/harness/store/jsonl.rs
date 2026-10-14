//! Strict read-only JSONL projection, sharing document source slices and navigation indexes.

pub(crate) mod ranges;

use super::document::{self, accepts_value, Index, Raw};
use crate::lazy::RawValue;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// Visit Value-compatible records with their absolute byte offsets. Malformed JSON lines are
/// skipped; UTF-8/read failure anywhere returns None, matching read_to_string's all-or-none
/// contract. Callbacks may already have run: callers must discard accumulated state on None.
/// Mapped source bytes/index are dropped before this returns the original read-only descriptor;
/// the portable or mapping-failure path retains only a single line's scratch buffer.
pub(crate) fn scan(path: &Path, visit: impl FnMut(Raw<'_>, u64)) -> Option<File> {
    scan_selected(path, |_| true, visit)
}

/// Apply an existing lexical selection before JSON validation, preserving whole-file UTF-8
/// validation. Useful for event readers whose original contract selects a literal marker.
pub(crate) fn scan_selected(
    path: &Path,
    select: impl Fn(&str) -> bool,
    mut visit: impl FnMut(Raw<'_>, u64),
) -> Option<File> {
    let file = File::open(path).ok()?;
    #[cfg(feature = "mmap")]
    if let Ok(source) = super::document::Source::map_file(&file) {
        let text = std::str::from_utf8(source.bytes()).ok()?;
        for line in text.lines() {
            let line = line.trim();
            if !select(line) {
                continue;
            }
            let Ok(validated) = serde_json::from_str::<&RawValue>(line) else {
                continue;
            };
            let offset = (validated.get().as_ptr() as usize - text.as_ptr() as usize) as u64;
            let raw = source.slice(offset, validated.get().len() as u64)?;
            if accepts_value(raw) {
                visit(raw, offset);
            }
        }
        return Some(file);
    }
    let mut reader = BufReader::new(file.try_clone().ok()?);
    let mut line = Vec::new();
    let mut offset = 0;
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line).ok()?;
        if n == 0 {
            break;
        }
        let text = std::str::from_utf8(&line).ok()?;
        let trimmed = text.trim();
        if select(trimmed) {
            if let Ok(validated) = serde_json::from_str::<&RawValue>(trimmed) {
                let index = Index::default();
                let raw = Raw::new(validated.get(), &index);
                if accepts_value(raw) {
                    visit(
                        raw,
                        offset + (trimmed.as_ptr() as usize - text.as_ptr() as usize) as u64,
                    );
                }
            }
        }
        offset += n as u64;
    }
    Some(file)
}

/// One physical JSONL line, as the record reader hands it to a codec.
pub(crate) struct Line<'v> {
    /// Absolute offset of the record after ASCII whitespace trimming.
    pub at: u64,
    /// Absolute offset just past the line: after its LF, or the end of the read.
    pub end: u64,
    /// The writer finished this line (it ends in LF); only such boundaries may checkpoint.
    pub complete: bool,
    /// The Value-compatible record, or None for a blank, non-UTF-8 or rejected line.
    pub view: Option<crate::json::JsonView<'v>>,
}

/// Records of `from..to` in a captured JSONL source, each physical line once. This is the byte
/// source every JSONL codec shares; it is the only place the two builds differ. Mapped builds
/// borrow each line from one mapping. Positioned builds (and a failed mapping) read each small
/// line into one reused buffer and leave a giant line in place as a positioned view, so no
/// private copy of it is ever made. Either way the codec sees the same [`JsonView`] API.
///
/// Records are trimmed of ASCII whitespace only, as JSON itself allows. Read errors end the
/// visit with an error. `file` must be the descriptor the caller checked against `source`.
pub(crate) fn for_each_line(
    source: &crate::json::JsonSource,
    file: &File,
    from: u64,
    to: u64,
    mut visit: impl FnMut(Line<'_>) -> anyhow::Result<crate::stream::Flow>,
) -> anyhow::Result<()> {
    use crate::stream::Flow;
    #[cfg(feature = "mmap")]
    if let Ok(map) = unsafe { memmap2::Mmap::map(file) } {
        #[cfg(unix)]
        let _ = map.advise(memmap2::Advice::Sequential);
        let bytes = usize::try_from(from)
            .ok()
            .zip(usize::try_from(to).ok())
            .and_then(|(from, to)| map.get(from..to))
            .unwrap_or(&[]);
        let mut start = 0;
        for end in memchr::memchr_iter(b'\n', bytes).map(|i| i + 1) {
            let line = &bytes[start..end - 1];
            if in_memory(source, from + start as u64, from + end as u64, true, line, &mut visit)? == Flow::Stop {
                return Ok(());
            }
            start = end;
        }
        if start < bytes.len() {
            in_memory(source, from + start as u64, to, false, &bytes[start..], &mut visit)?;
        }
        return Ok(());
    }
    let mut frames = ranges::records(
        file,
        ranges::Range {
            offset: from,
            len: to.saturating_sub(from),
        },
    )?;
    frames.keep_small_records(64 * 1024);
    let context = document::range::RangeSource::from_captured(source.clone())?;
    while let Some(frame) = frames.next() {
        let (raw, complete) = frame?;
        let end = frames.offset();
        let flow = if let Some(bytes) = frames.small_record() {
            in_memory(source, raw.offset, end, complete, bytes, &mut visit)?
        } else {
            let range = trim_ascii(file, raw)?;
            let at = range.offset;
            let mut visited = false;
            let read = (range.len != 0).then(|| {
                source.read_positioned_range_value(&context, at, range.len, |view| {
                    visited = true;
                    visit(Line {
                        at,
                        end,
                        complete,
                        view: Some(view),
                    })
                })
            });
            match read {
                Some(Err(error)) if !visited && error.downcast_ref::<crate::json::InvalidJson>().is_some() => None,
                Some(flow) => Some(flow?),
                None => None,
            }
            .map_or_else(
                || {
                    visit(Line {
                        at,
                        end,
                        complete,
                        view: None,
                    })
                },
                Ok,
            )?
        };
        if flow == Flow::Stop {
            break;
        }
    }
    Ok(())
}

/// One line whose bytes are in memory (mapped, or read as a small positioned record).
fn in_memory(
    source: &crate::json::JsonSource,
    start: u64,
    end: u64,
    complete: bool,
    bytes: &[u8],
    visit: &mut impl FnMut(Line<'_>) -> anyhow::Result<crate::stream::Flow>,
) -> anyhow::Result<crate::stream::Flow> {
    let lead = bytes.iter().take_while(|b| b.is_ascii_whitespace()).count();
    let tail = bytes[lead..]
        .iter()
        .rev()
        .take_while(|b| b.is_ascii_whitespace())
        .count();
    let trimmed = &bytes[lead..bytes.len() - tail];
    let at = start + lead as u64;
    if let Some(text) = (!trimmed.is_empty())
        .then(|| std::str::from_utf8(trimmed).ok())
        .flatten()
    {
        let mut visited = false;
        match source.read_record(at, text, |view| {
            visited = true;
            visit(Line {
                at,
                end,
                complete,
                view: Some(view),
            })
        }) {
            Err(error) if !visited && error.downcast_ref::<crate::json::InvalidJson>().is_some() => {}
            flow => return flow,
        }
    }
    visit(Line {
        at,
        end,
        complete,
        view: None,
    })
}

/// ASCII-trim a giant positioned record by reading only its boundary windows.
fn trim_ascii(file: &File, mut range: ranges::Range) -> anyhow::Result<ranges::Range> {
    let mut bytes = [0; 64 * 1024];
    while range.len > 0 {
        let n = range.len.min(bytes.len() as u64) as usize;
        ranges::read_exact_at(file, &mut bytes[..n], range.offset)?;
        let lead = bytes[..n].iter().take_while(|b| b.is_ascii_whitespace()).count();
        range.offset += lead as u64;
        range.len -= lead as u64;
        if lead != n {
            break;
        }
    }
    while range.len > 0 {
        let n = range.len.min(bytes.len() as u64) as usize;
        ranges::read_exact_at(file, &mut bytes[..n], range.offset + range.len - n as u64)?;
        let tail = bytes[..n].iter().rev().take_while(|b| b.is_ascii_whitespace()).count();
        range.len -= tail as u64;
        if tail != n {
            break;
        }
    }
    Ok(range)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::store::document::Record;

    #[test]
    fn projected_records_keep_absolute_offsets_and_value_acceptance() {
        let world = cv_testkit::World::new("raw-jsonl");
        let path = world.base.join("wire.jsonl");
        let text = format!(
            "\u{2003}{{\"n\":1,\"body\":\"{}\"}} \r\n\ninvalid\n{{\"n\":2,\"ignored\":1e400}}\n{{\"n\":3}}",
            "雪".repeat(3000)
        );
        std::fs::write(&path, &text).unwrap();
        let mut seen = Vec::new();
        let file = scan(&path, |raw, offset| {
            assert_eq!(&text[offset as usize..offset as usize + raw.get().len()], raw.get());
            seen.push(Record::parse(raw).unwrap().value("n").unwrap());
        })
        .unwrap();
        assert_eq!(seen, [serde_json::json!(1), serde_json::json!(3)]);
        assert_eq!(file.metadata().unwrap().len(), text.len() as u64);
        let mut selected = 0;
        assert!(scan_selected(&path, |line| line.contains("\"n\":3"), |_, _| selected += 1).is_some());
        assert_eq!(selected, 1);
        // Selection does not hide a non-UTF8 suffix: callers must retract a partial result.
        let mut bad = text.into_bytes();
        bad.extend_from_slice(b"\n\xff");
        std::fs::write(&path, bad).unwrap();
        assert!(scan_selected(&path, |_| false, |_, _| panic!("selected nothing")).is_none());
    }
}
