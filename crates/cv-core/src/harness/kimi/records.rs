//! Checked positioned context records with the established rotated-prefix continuation policy.
use super::*;
use crate::{
    harness::store::jsonl::ranges,
    json::{InvalidJson, JsonSource, JsonView},
};
use anyhow::Context;
use std::{collections::hash_map::DefaultHasher, hash::Hasher, io::BufRead};

struct Fingerprint<'a> {
    reader: BufReader<ranges::Reader<'a>>,
    hash: DefaultHasher,
}
impl Read for Fingerprint<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let n = self.reader.read(bytes)?;
        self.hash.write(&bytes[..n]);
        Ok(n)
    }
}
impl BufRead for Fingerprint<'_> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.reader.fill_buf()
    }
    fn consume(&mut self, amount: usize) {
        self.hash.write(&self.reader.buffer()[..amount]);
        self.reader.consume(amount);
    }
}

pub(super) fn visit(
    segments: &[PathBuf],
    at: Option<u64>,
    mut accept: impl FnMut(JsonView<'_>, &JsonSource, Option<&str>, Option<u64>, u64) -> Result<Flow>,
) -> Result<u64> {
    let mut skipped = 0;
    let mut prev = Vec::new();
    for (index, path) in segments.iter().enumerate() {
        let live = index + 1 == segments.len();
        if at.is_some() && !live {
            continue;
        }
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let source = JsonSource::capture(path)?;
        source.check_metadata(file.metadata()?)?;
        let offset = if live { at.unwrap_or(0) } else { 0 };
        let len = source
            .len()
            .checked_sub(offset)
            .context("context offset beyond source")?;
        let rows = ranges::records(&file, ranges::Range { offset, len })?;
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
        let mut cur = Vec::new();
        let mut line_index = 0;
        let mut shared = at.is_none() && index > 0;
        let mut announce = shared;
        for row in rows {
            let (raw, complete) = row?;
            let range = ranges::trim(&file, raw)?;
            // Older segments need fingerprints for their successor. The live segment only
            // needs them until its copied prefix diverges; accepted JSON validation already
            // checks UTF-8 afterwards, so an unused hash would reread every remaining body.
            if !live || shared {
                let mut fingerprint = Fingerprint {
                    reader: ranges::reader(&file, range)?,
                    hash: DefaultHasher::new(),
                };
                if !ranges::valid_utf8(&mut fingerprint)? {
                    skipped += 1;
                    continue;
                }
                // str::hash appends this delimiter; chunked writes retain the same fingerprint.
                fingerprint.hash.write_u8(0xff);
                let hash = fingerprint.hash.finish();
                if !live {
                    cur.push(hash);
                }
                let matches = shared && prev.get(line_index) == Some(&hash);
                line_index += 1;
                if matches {
                    continue;
                }
                shared = false;
            }
            if range.len == 0 {
                continue;
            }
            let position = (live && complete).then_some(raw.offset);
            let result = source.read_range_value(range.offset, range.len, |view| {
                let starts = announce.then_some(name);
                announce = false;
                accept(view, &source, starts, position, skipped)
            });
            match result {
                Ok(Flow::Stop) => {
                    source.check()?;
                    return Ok(skipped);
                }
                Ok(Flow::Continue) => {}
                Err(error) if error.is::<InvalidJson>() => skipped += 1,
                Err(error) => return Err(error),
            }
        }
        source.check()?;
        prev = cur;
    }
    Ok(skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn positioned_context_visit_matches_owned_rotation_offsets_and_failures() -> Result<()> {
        let world = crate::test_world("kimi-positioned-records");
        let a = world.base.join("context_1.jsonl");
        let b = world.base.join("context.jsonl");
        let prefix = "\u{2003}{\"role\":\"user\",\"content\":\"prompt λ\"}\r\n\ninvalid-json\n";
        let mut first = prefix.as_bytes().to_vec();
        first.extend_from_slice(b"\xff\n{\"role\":\"assistant\",\"content\":\"old\"}\n");
        fs::write(&a, first)?;
        let mut second = prefix.as_bytes().to_vec();
        second.extend_from_slice(b"\xff\n{\"ignored\":1e999}\n{\"role\":\"assistant\",\"content\":\"new\"}\n{\"role\":\"user\",\"content\":\"partial\"}");
        fs::write(&b, second)?;
        let segments = vec![a, b.clone()];
        for at in [None, Some(prefix.len() as u64)] {
            let mut old = Vec::new();
            let skipped = super::super::for_each_segment_record_at(&segments, at, |value, name, offset, skipped| {
                old.push((value, name.map(str::to_string), offset, skipped));
                Flow::Continue
            });
            let mut new = Vec::new();
            let actual = visit(&segments, at, |view, _, name, offset, skipped| {
                new.push((view.resolve()?.into_owned(), name.map(str::to_string), offset, skipped));
                Ok(Flow::Continue)
            })?;
            assert_eq!(actual, skipped);
            assert_eq!(new, old);
        }
        let error = visit(&segments, None, |_, source, _, _, _| {
            fs::write(source.path(), "changed")?;
            Ok(Flow::Stop)
        });
        assert!(error.is_err());
        Ok(())
    }
}
