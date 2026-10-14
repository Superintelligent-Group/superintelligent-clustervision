//! Portable Claude replay projects final wire fields, including raw carrier/extra overrides.
//! Model labels cannot authenticate signatures. Native emission remains archival fidelity.
use crate::harness::store::{
    bytes::{ByteSource, ByteStage},
    document::range::{Range, RangeSource},
};
use anyhow::Result;
use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, BufReader, Write},
    sync::Arc,
};

#[derive(Default, Debug)]
pub(crate) struct Changes {
    pub ids: usize,
    pub thinking: usize,
}
fn legal(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}
fn string(source: &RangeSource, range: Option<Range>) -> Result<Option<String>> {
    match range {
        Some(range) if source.kind(range)? == Some(b'"') => Ok(Some(source.materialize(range)?)),
        _ => Ok(None),
    }
}
fn blocks(source: &RangeSource, row: Range, mut visit: impl FnMut(Range) -> Result<()>) -> Result<()> {
    let mut cursor = source.cursor(row)?;
    cursor.whitespace()?;
    if cursor.position() == row.offset + row.len {
        return Ok(());
    }
    let Some(message) = source.get(row, "message")? else {
        return Ok(());
    };
    let Some(content) = source.get(message, "content")? else {
        return Ok(());
    };
    if let Some(array) = source.array(content)? {
        for block in array {
            visit(block?)?;
        }
    }
    Ok(())
}
/// Bounded framing for both memory and private spills; never retain an entire JSONL line.
fn rows(source: &ByteSource, mut visit: impl FnMut(Range) -> Result<()>) -> Result<()> {
    let mut reader = BufReader::with_capacity(64 * 1024, source.reader()?);
    let (mut start, mut at) = (0u64, 0u64);
    loop {
        let bytes = reader.fill_buf()?;
        if bytes.is_empty() {
            if at > start {
                visit(Range {
                    offset: start,
                    len: at - start,
                })?;
            }
            break;
        }
        let n = bytes.len();
        let mut position = 0;
        while let Some(delta) = memchr::memchr(b'\n', &bytes[position..]) {
            position += delta + 1;
            let end = at + position as u64;
            visit(Range {
                offset: start,
                len: end - start,
            })?;
            start = end;
        }
        at += n as u64;
        reader.consume(n);
    }
    Ok(())
}
fn copy(source: &RangeSource, range: Range, output: &mut ByteStage) -> Result<()> {
    std::io::copy(&mut source.cursor(range)?, output)?;
    Ok(())
}
enum Replacement {
    Id(Vec<u8>),
    Thinking(Option<Range>),
}

/// Two passes reserve all valid IDs before allocation. Metadata scales with IDs and each record's
/// selected spans. Unknown fields and tool payloads copy unchanged with bounded positioned reads.
pub(crate) fn normalize(bytes: ByteSource, mode: crate::emit::ThinkingMode) -> Result<(ByteStage, Changes)> {
    let bytes = Arc::new(bytes);
    let source = RangeSource::private(bytes.clone())?;
    let mut reserved = HashSet::new();
    let mut invalid = Vec::new();
    let mut seen = HashSet::new();
    rows(&bytes, |row| {
        // A portable output must not silently carry invalid JSON around its repaired fields.
        let mut cursor = source.cursor(row)?;
        cursor.whitespace()?;
        if cursor.position() == row.offset + row.len {
            return Ok(());
        }
        anyhow::ensure!(source.validate(row)?, "portable Claude replay encountered invalid JSON");
        blocks(&source, row, |block| {
            let ty = string(&source, source.get(block, "type")?)?;
            let key = match ty.as_deref() {
                Some("tool_use") => "id",
                Some("tool_result") => "tool_use_id",
                _ => return Ok(()),
            };
            let id = string(&source, source.get(block, key)?)?
                .ok_or_else(|| anyhow::anyhow!("portable Claude replay: {key} must be a string"))?;
            if legal(&id) {
                reserved.insert(id);
            } else if seen.insert(id.clone()) {
                invalid.push(id);
            }
            Ok(())
        })
    })?;
    let mut mapping = HashMap::new();
    let mut next = 0usize;
    for original in invalid {
        loop {
            let replacement = format!("cv_tool_{next}");
            next += 1;
            if reserved.insert(replacement.clone()) {
                mapping.insert(original, serde_json::to_vec(&replacement)?);
                break;
            }
        }
    }
    let mut output = ByteStage::new();
    let mut changes = Changes {
        ids: mapping.len(),
        thinking: 0,
    };
    rows(&bytes, |row| {
        let mut patches = Vec::new();
        blocks(&source, row, |block| {
            let ty = string(&source, source.get(block, "type")?)?;
            match ty.as_deref() {
                Some("tool_use") | Some("tool_result") => {
                    let key = if ty.as_deref() == Some("tool_use") {
                        "id"
                    } else {
                        "tool_use_id"
                    };
                    let range = source.get(block, key)?;
                    if let Some(id) = string(&source, range)? {
                        if let Some(replacement) = mapping.get(&id) {
                            patches.push((range.unwrap(), Replacement::Id(replacement.clone())));
                        }
                    }
                }
                Some("thinking") | Some("redacted_thinking") if mode == crate::emit::ThinkingMode::Portable => {
                    changes.thinking += 1;
                    let text = if ty.as_deref() == Some("redacted_thinking") {
                        None
                    } else {
                        match source.get(block, "thinking")? {
                            Some(range) if source.kind(range)? == Some(b'"') && range.len > 2 => Some(range),
                            _ => None,
                        }
                    };
                    patches.push((block, Replacement::Thinking(text)));
                }
                _ => {}
            }
            Ok(())
        })?;
        patches.sort_by_key(|p| p.0.offset);
        let mut position = row.offset;
        for (range, replacement) in patches {
            copy(
                &source,
                Range {
                    offset: position,
                    len: range.offset - position,
                },
                &mut output,
            )?;
            match replacement {
                Replacement::Id(id) => output.write_all(&id)?,
                Replacement::Thinking(text) => {
                    output.write_all(b"{\"type\":\"text\",\"text\":")?;
                    if let Some(text) = text {
                        copy(&source, text, &mut output)?;
                    } else {
                        output.write_all(b"\"[thinking: opaque provider reasoning omitted for portable replay]\"")?;
                    }
                    output.write_all(b"}")?;
                }
            }
            position = range.offset + range.len;
        }
        copy(
            &source,
            Range {
                offset: position,
                len: row.offset + row.len - position,
            },
            &mut output,
        )
    })?;
    Ok((output, changes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn portable_preserves_payloads_and_maps_collisions_without_model_guesses() -> Result<()> {
        let body = "giant \\ \" 🌙\n".repeat(100_000);
        let calls = json!({"type":"assistant","message":{"role":"assistant","model":"claude-alias","content":[
            {"type":"thinking","thinking":body,"signature":"foreign"},
            {"type":"thinking","thinking":"","signature":"opaque"},
            {"type":"redacted_thinking","data":"opaque","thinking":"do not interpret this"},
            {"type":"tool_use","id":"a:b","name":"Read","input":{"body":body}},
            {"type":"tool_use","id":"a#b","name":"Read","input":{}},
            {"type":"tool_use","id":"cv_tool_0","name":"Read","input":{}},
            {"type":"tool_use","id":"","name":"Read","input":{}},
            {"type":"tool_use","id":"☃","name":"Read","input":{}}
        ]},"unknown":{"keep":"a:b"}});
        let results = json!({"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"a:b","content":body},
            {"type":"tool_result","tool_use_id":"a#b","content":"ok"},
            {"type":"tool_result","tool_use_id":"cv_tool_0","content":"ok"},
            {"type":"tool_result","tool_use_id":"","content":"ok"},
            {"type":"tool_result","tool_use_id":"☃","content":"ok"}
        ]}});
        let mut stage = ByteStage::new();
        // Results-before-calls must use the same mapping as ordinary call/result ordering.
        writeln!(stage, "{results}")?;
        writeln!(stage, "{calls}")?;
        let (stage, changes) = normalize(stage.finish()?, crate::emit::ThinkingMode::Portable)?;
        assert_eq!((changes.ids, changes.thinking), (4, 3));
        let output = stage.finish()?;
        let rows: Vec<serde_json::Value> = BufReader::new(output.reader()?)
            .lines()
            .map(|row| serde_json::from_str(&row.unwrap()).unwrap())
            .collect();
        let content = rows[1]["message"]["content"].as_array().unwrap();
        assert_eq!(content[0]["text"], body);
        assert!(content[1]["text"].as_str().unwrap().contains("opaque"));
        assert!(content[2]["text"].as_str().unwrap().contains("opaque"));
        let ids: Vec<_> = content[3..].iter().map(|b| b["id"].as_str().unwrap()).collect();
        assert!(ids.iter().all(|id| legal(id)));
        assert_eq!(ids.iter().collect::<HashSet<_>>().len(), 5);
        assert_eq!(ids[2], "cv_tool_0");
        for (result, id) in rows[0]["message"]["content"].as_array().unwrap().iter().zip(ids) {
            assert_eq!(result["tool_use_id"], id);
        }
        assert_eq!(content[3]["input"], calls["message"]["content"][3]["input"]);
        assert_eq!(rows[0]["message"]["content"][0]["content"], body);
        assert_eq!(rows[1]["unknown"], calls["unknown"]);
        Ok(())
    }
    #[test]
    fn portable_rejects_missing_ids_and_invalid_json() -> Result<()> {
        for line in [
            r#"{"message":{"content":[{"type":"tool_use","name":"Read"}]}}"#,
            r#"{"message":{"content":[{"type":"tool_result","tool_use_id":null}]}}"#,
            r#"{"invalid":"\ud800"}"#,
        ] {
            let mut stage = ByteStage::new();
            stage.write_all(line.as_bytes())?;
            assert!(normalize(stage.finish()?, crate::emit::ThinkingMode::Portable).is_err());
        }
        Ok(())
    }
}
