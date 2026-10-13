//! Strict read-only JSONL projection, sharing document source slices and navigation indexes.

pub(crate) mod ranges;

use super::document::{accepts_value, Index, Raw};
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
