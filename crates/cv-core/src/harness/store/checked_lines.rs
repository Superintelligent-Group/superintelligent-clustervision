//! Grok/Qwen acceptance policy over shared bounded positioned JSONL framing.
use super::jsonl::ranges;
use crate::{
    json::{InvalidJson, JsonSource, JsonView},
    stream::Flow,
};
use anyhow::{Context, Result};

pub(crate) fn for_each(
    file: &std::fs::File,
    offset: u64,
    source: &JsonSource,
    mut accept: impl FnMut(JsonView<'_>, u64, bool, u64) -> Result<Flow>,
) -> Result<u64> {
    for_each_record(file, offset, source, |view, offset, complete, skipped| match view {
        Some(view) => accept(view, offset, complete, skipped),
        None => Ok(Flow::Continue),
    })
}

/// Visit every physical record boundary, including rejected and blank rows. Replay consumers
/// need these boundaries to checkpoint their accumulated skipped-row state exactly.
pub(crate) fn for_each_record(
    file: &std::fs::File,
    offset: u64,
    source: &JsonSource,
    mut accept: impl FnMut(Option<JsonView<'_>>, u64, bool, u64) -> Result<Flow>,
) -> Result<u64> {
    source.check_metadata(file.metadata()?)?;
    source.check()?;
    let len = source.len().checked_sub(offset).context("JSONL offset beyond source")?;
    let mut records = ranges::records(file, ranges::Range { offset, len })?;
    let mut skipped = 0;
    while let Some(record) = records.next() {
        let (range, complete) = record?;
        // The positioned JSON API requires a nonempty range. An empty physical row is a
        // framing boundary, not malformed JSON, and does not increment skipped state.
        if range.len == 0 {
            if accept(None, records.offset(), complete, skipped)? == Flow::Stop {
                break;
            }
            continue;
        }
        let mut read = |range: ranges::Range| {
            source.read_range_value(range.offset, range.len, |view| {
                accept(Some(view), records.offset(), complete, skipped)
            })
        };
        match read(range) {
            Ok(Flow::Stop) => break,
            Ok(Flow::Continue) => {}
            Err(error) if error.downcast_ref::<InvalidJson>().is_some() => {
                // JSON accepts ASCII boundary whitespace directly. Retry Unicode trimming
                // only rejected ranges, preserving the owned reader's tolerant policy.
                let trimmed = ranges::trim(file, range)?;
                if trimmed.len == 0 {
                    if accept(None, records.offset(), complete, skipped)? == Flow::Stop {
                        break;
                    }
                    continue;
                }
                let rejected = if trimmed.offset == range.offset && trimmed.len == range.len {
                    true
                } else {
                    match read(trimmed) {
                        Ok(Flow::Stop) => break,
                        Ok(Flow::Continue) => false,
                        Err(error) if error.downcast_ref::<InvalidJson>().is_some() => true,
                        Err(error) => return Err(error),
                    }
                };
                if rejected {
                    skipped += 1;
                    if accept(None, records.offset(), complete, skipped)? == Flow::Stop {
                        break;
                    }
                }
            }
            Err(error) => return Err(error),
        }
    }
    source.check()?;
    Ok(skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaced_open_descriptor_is_rejected_before_any_record_callback() -> Result<()> {
        let world = crate::test_world("checked_lines_descriptor");
        let path = world.home.join("records.jsonl");
        std::fs::write(&path, "{\"body\":\"old\"}\n")?;
        let file = std::fs::File::open(&path)?;
        std::fs::rename(&path, world.home.join("opened-old.jsonl"))?;
        std::fs::write(&path, "{\"body\":\"new\"}\n")?;
        let source = JsonSource::capture(&path)?;
        assert_eq!(
            file.metadata()?.len(),
            source.len(),
            "extent alone cannot detect replacement"
        );
        let mut callbacks = 0;
        let result = for_each_record(&file, 0, &source, |_, _, _, _| {
            callbacks += 1;
            Ok(Flow::Continue)
        });
        assert!(result.is_err());
        assert_eq!(callbacks, 0);
        let fresh = std::fs::File::open(&path)?;
        assert_eq!(
            for_each_record(&fresh, source.len(), &source, |_, _, _, _| {
                callbacks += 1;
                Ok(Flow::Continue)
            })?,
            0
        );
        assert_eq!(callbacks, 0, "an EOF suffix remains valid");
        Ok(())
    }
}
