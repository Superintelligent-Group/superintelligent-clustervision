//! Portable navigation over strictly accepted record ranges, without encoded line retention.
use super::super::store::document::range::{Range, RangeSource};
use super::*;

fn string(source: &RangeSource, value: Option<Range>) -> Result<Option<String>> {
    match value {
        Some(value) if source.kind(value)? == Some(b'"') => source.materialize(value).map(Some),
        _ => Ok(None),
    }
}
fn key(source: &RangeSource, id: Option<Range>, ty: Option<Range>) -> Result<Option<String>> {
    let Some(id) = string(source, id)? else { return Ok(None) };
    Ok(Some(format!("{}\u{1f}{id}", string(source, ty)?.unwrap_or_default())))
}

pub(super) fn ingest(replay: &mut RecordingReplay<RangeBody>, source: &RangeSource, range: Range) -> Result<()> {
    let Some(fields) = source.select_fields(
        range,
        &["$rewindTo", "$set", "id", "type", "content", "sessionId", "projectHash"],
    )?
    else {
        return Ok(());
    };
    if let Some(target) = string(source, fields[0])? {
        replay.rewind(&target, RangeBody::Marker);
        return Ok(());
    }
    if let Some(set) = fields[1] {
        if source.kind(set)? == Some(b'{') {
            let mut roster = RECORD_METADATA.to_vec();
            roster.push("messages");
            let values = source.select_fields(set, &roster)?.unwrap();
            if let Some(messages) = values[RECORD_METADATA.len()] {
                replay.clear_messages();
                if let Some(messages) = source.array(messages)? {
                    for message in messages {
                        let message = message?;
                        if let Some(fields) = source.select_fields(message, &["id", "type"])? {
                            if let Some(key) = key(source, fields[0], fields[1])? {
                                replay.insert(key, RangeBody::Range(message));
                            }
                        }
                    }
                }
            }
            for (name, value) in RECORD_METADATA.iter().zip(values) {
                if let Some(value) = value {
                    replay.metadata.insert((*name).into(), source.materialize(value)?);
                }
            }
            return Ok(());
        }
    }
    if let Some(key) = key(source, fields[2], fields[3])? {
        if fields[3].is_some() || fields[4].is_some() {
            replay.insert(key, RangeBody::Range(range));
        }
        return Ok(());
    }
    if fields[5].is_some() && fields[6].is_some() {
        replay.saw_meta = true;
        let values = source.select_fields(range, RECORD_METADATA)?.unwrap();
        for (name, value) in RECORD_METADATA.iter().zip(values) {
            if let Some(value) = value {
                replay.metadata.insert((*name).into(), source.materialize(value)?);
            }
        }
    }
    Ok(())
}

pub(super) enum RangeBody {
    Range(Range),
    Marker(Value),
}

/// Match the old UTF-8 line decoder and Unicode trim while retaining only a small boundary tail.
pub(super) fn trim(source: &RangeSource, range: Range) -> Result<Option<Range>> {
    use std::io::Read;
    let mut cursor = source.cursor(range)?;
    let mut bytes = [0u8; 64 * 1024 + 3];
    let mut pending = 0;
    let mut position = range.offset;
    let mut start = None;
    let mut end = position;
    loop {
        let read = cursor.read(&mut bytes[pending..])?;
        if read == 0 {
            return Ok((pending == 0)
                .then_some(start.map(|offset| Range {
                    offset,
                    len: end - offset,
                }))
                .flatten());
        }
        let available = pending + read;
        let valid = match std::str::from_utf8(&bytes[..available]) {
            Ok(_) => available,
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => return Ok(None),
        };
        let text = std::str::from_utf8(&bytes[..valid])?;
        if start.is_none() {
            let body = text.trim_start();
            if !body.is_empty() {
                start = Some(position + (text.len() - body.len()) as u64);
            }
        }
        let tail = text.trim_end();
        if !tail.is_empty() {
            end = position + tail.len() as u64;
        }
        position += valid as u64;
        pending = available - valid;
        bytes.copy_within(valid..available, 0);
    }
}
