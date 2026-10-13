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
    source.check()?;
    let len = file
        .metadata()?
        .len()
        .checked_sub(offset)
        .context("JSONL offset beyond source")?;
    let mut records = ranges::records(file, ranges::Range { offset, len })?;
    let mut skipped = 0;
    while let Some(record) = records.next() {
        let (range, complete) = record?;
        if range.len == 0 {
            continue;
        }
        let mut read = |range: ranges::Range| {
            source.read_range_value(range.offset, range.len, |view| {
                accept(view, records.offset(), complete, skipped)
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
                    continue;
                }
                if trimmed.offset == range.offset && trimmed.len == range.len {
                    skipped += 1;
                    continue;
                }
                match read(trimmed) {
                    Ok(Flow::Stop) => break,
                    Ok(Flow::Continue) => {}
                    Err(error) if error.downcast_ref::<InvalidJson>().is_some() => skipped += 1,
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
    }
    source.check()?;
    Ok(skipped)
}
