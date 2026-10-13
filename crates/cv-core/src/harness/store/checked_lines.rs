//! Offset-only JSONL framing: selected producers never allocate the physical record body.
use crate::{
    json::{InvalidJson, JsonSource, JsonView},
    stream::Flow,
};
use anyhow::Result;
use std::io::{BufRead, Read, Seek};

pub(crate) fn for_each(
    mut reader: impl BufRead,
    mut offset: u64,
    source: &JsonSource,
    mut accept: impl FnMut(JsonView<'_>, u64, bool, u64) -> Result<Flow>,
) -> Result<u64> {
    let mut start = offset;
    let mut skipped = 0;
    let mut nonwhite = false;
    loop {
        let bytes = reader.fill_buf()?;
        let eof = bytes.is_empty();
        let n = if eof {
            0
        } else {
            memchr::memchr(b'\n', bytes).map_or(bytes.len(), |i| i + 1)
        };
        let complete = n != 0 && bytes[n - 1] == b'\n';
        nonwhite |= bytes[..n].iter().any(|byte| !byte.is_ascii_whitespace());
        offset += n as u64;
        reader.consume(n);
        if complete || eof {
            if nonwhite {
                match source.read_range_value(start, offset - start, |view| accept(view, offset, complete, skipped)) {
                    Ok(Flow::Stop) => break,
                    Ok(Flow::Continue) => {}
                    Err(error) if error.downcast_ref::<InvalidJson>().is_some() => {
                        // Preserve the tolerant reader's Unicode trim without owning rejected rows.
                        match trim_range(source, start, offset)? {
                            Some((_, 0)) => {}
                            Some((trimmed, len)) if trimmed != start || len != offset - start => {
                                match source
                                    .read_range_value(trimmed, len, |view| accept(view, offset, complete, skipped))
                                {
                                    Ok(Flow::Stop) => break,
                                    Ok(Flow::Continue) => {}
                                    Err(error) if error.downcast_ref::<InvalidJson>().is_some() => skipped += 1,
                                    Err(error) => return Err(error),
                                }
                            }
                            _ => skipped += 1,
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
            start = offset;
            nonwhite = false;
        }
        if eof {
            break;
        }
    }
    source.check()?;
    Ok(skipped)
}

fn trim_range(source: &JsonSource, mut start: u64, mut end: u64) -> Result<Option<(u64, u64)>> {
    let mut file = std::fs::File::open(source.path())?;
    let mut bytes = [0u8; 4096];
    while start < end {
        file.seek(std::io::SeekFrom::Start(start))?;
        let n = usize::try_from((end - start).min(bytes.len() as u64))?;
        file.read_exact(&mut bytes[..n])?;
        let text = match std::str::from_utf8(&bytes[..n]) {
            Ok(text) => text,
            Err(error) => std::str::from_utf8(&bytes[..error.valid_up_to()])?,
        };
        if let Some((index, _)) = text.char_indices().find(|(_, c)| !c.is_whitespace()) {
            start += index as u64;
            break;
        }
        if text.is_empty() {
            return Ok(None);
        }
        start += text.len() as u64;
    }
    while start < end {
        let n = usize::try_from((end - start).min(bytes.len() as u64))?;
        let at = end - n as u64;
        file.seek(std::io::SeekFrom::Start(at))?;
        file.read_exact(&mut bytes[..n])?;
        let skip = bytes[..n].iter().take_while(|b| **b & 0xc0 == 0x80).count();
        let Ok(text) = std::str::from_utf8(&bytes[skip..n]) else {
            return Ok(None);
        };
        if let Some((index, c)) = text.char_indices().rev().find(|(_, c)| !c.is_whitespace()) {
            end = at + skip as u64 + index as u64 + c.len_utf8() as u64;
            break;
        }
        end = at + skip as u64;
    }
    Ok(Some((start, end - start)))
}
