//! Discovery/enrichment over the existing mapped or positioned document readers. The fallback
//! frames ranges rather than retaining physical lines; reducers share their field semantics.
use super::*;
use crate::harness::store::{
    document::range::{Range, RangeSource},
    jsonl::ranges,
};
use crate::json::JsonSource;
#[cfg(feature = "mmap")]
use crate::lazy::Span;

#[derive(Clone, Copy)]
pub(super) enum Row<'a> {
    #[cfg(feature = "mmap")]
    Mapped { raw: Raw<'a>, offset: u64 },
    Positioned {
        range: Range,
        context: &'a RangeSource,
        captured: &'a JsonSource,
    },
}
impl<'a> Row<'a> {
    pub(super) fn get(self, key: &str) -> Result<Option<Self>> {
        Ok(match self {
            #[cfg(feature = "mmap")]
            Self::Mapped { raw, offset } => fields(raw).get(key).map(|child| Self::Mapped {
                raw: child,
                offset: offset + (child.get().as_ptr() as usize - raw.get().as_ptr() as usize) as u64,
            }),
            Self::Positioned {
                range,
                context,
                captured,
            } => context.get(range, key)?.map(|range| Self::Positioned {
                range,
                context,
                captured,
            }),
        })
    }
    pub(super) fn number(self) -> Result<Option<serde_json::Number>> {
        match self {
            #[cfg(feature = "mmap")]
            Self::Mapped { raw, .. } => Ok(serde_json::from_str(raw.get()).ok()),
            Self::Positioned { range, context, .. } => {
                if matches!(context.kind(range)?, Some(b'-' | b'0'..=b'9')) {
                    Ok(Some(context.materialize(range)?))
                } else {
                    Ok(None)
                }
            }
        }
    }
    pub(super) fn number_field(self, key: &str) -> Result<Option<serde_json::Number>> {
        self.get(key)?.map(Self::number).transpose().map(Option::flatten)
    }
    pub(super) fn ordered_keys(self) -> Result<Vec<String>> {
        match self {
            #[cfg(feature = "mmap")]
            Self::Mapped { raw, .. } => Ok(super::ordered_keys(raw)),
            Self::Positioned { range, context, .. } => {
                let mut keys = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for entry in context.object(range)?.into_iter().flatten() {
                    let (key, _) = entry?;
                    let key: String = context.materialize(key)?;
                    if seen.insert(key.clone()) {
                        keys.push(key)
                    }
                }
                Ok(keys)
            }
        }
    }
    pub(super) fn string(self) -> Result<Option<String>> {
        match self {
            #[cfg(feature = "mmap")]
            Self::Mapped { raw, .. } => Ok(serde_json::from_str(raw.get()).ok()),
            Self::Positioned { range, context, .. } => {
                if context.kind(range)? == Some(b'"') {
                    Ok(Some(context.materialize(range)?))
                } else {
                    Ok(None)
                }
            }
        }
    }
    pub(super) fn string_field(self, key: &str) -> Result<Option<String>> {
        self.get(key)?.map(Self::string).transpose().map(Option::flatten)
    }
    pub(super) fn array(self) -> Result<Box<dyn Iterator<Item = Result<Self>> + 'a>> {
        match self {
            #[cfg(feature = "mmap")]
            Self::Mapped { raw, offset } => Ok(Box::new(document::array_iter(raw).into_iter().flatten().map(
                move |child| {
                    Ok(Self::Mapped {
                        raw: child,
                        offset: offset + (child.get().as_ptr() as usize - raw.get().as_ptr() as usize) as u64,
                    })
                },
            ))),
            Self::Positioned {
                range,
                context,
                captured,
            } => Ok(Box::new(context.array(range)?.into_iter().flatten().map(
                move |range| {
                    range
                        .map(|range| Self::Positioned {
                            range,
                            context,
                            captured,
                        })
                        .map_err(Into::into)
                },
            ))),
        }
    }
    fn text(self) -> Result<Option<Text>> {
        match self {
            #[cfg(feature = "mmap")]
            Self::Mapped { raw, offset } => {
                let bytes = raw.get().as_bytes();
                if bytes.first() != Some(&b'"') {
                    return Ok(None);
                }
                Ok(if bytes.len() - 2 > INLINE_MAX {
                    Some(Text::Span(Span {
                        source: None,
                        offset: offset + 1,
                        len: bytes.len() as u64 - 2,
                        escaped: bytes.contains(&b'\\'),
                    }))
                } else {
                    serde_json::from_str(raw.get()).ok().map(Text::Inline)
                })
            }
            Self::Positioned {
                range,
                context,
                captured,
            } => {
                if context.kind(range)? != Some(b'"') {
                    return Ok(None);
                }
                if range.len > INLINE_MAX as u64 {
                    let crate::Json::Source(range) = captured.validated_range(range.offset, range.len)? else {
                        unreachable!()
                    };
                    Ok(Some(Text::Source(crate::lazy::SourceText::new(range))))
                } else {
                    Ok(Some(Text::Inline(context.materialize(range)?)))
                }
            }
        }
    }
    pub(super) fn title(self, resolver: &crate::lazy::Resolver) -> Result<Option<String>> {
        if let Some(text) = self.text()? {
            return Ok(document::text_summary(std::iter::once(text), resolver, true));
        }
        let mut failure = None;
        let texts = self.array()?.filter_map(|part| {
            match part.and_then(|part| part.get("text")?.map(Self::text).transpose().map(Option::flatten)) {
                Ok(text) => text,
                Err(error) => {
                    failure = Some(error);
                    None
                }
            }
        });
        let title = document::text_summary(texts, resolver, true);
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(title)
    }
}

pub(super) fn scan(path: &Path, visit: impl FnMut(Row<'_>) -> Result<()>) -> Option<()> {
    scan_selected_with(path, false, false, visit).ok()
}
pub(super) fn scan_hooks(path: &Path, visit: impl FnMut(Row<'_>) -> Result<()>) -> Option<()> {
    scan_selected_with(path, false, true, visit).ok()
}
#[cfg(test)]
fn scan_with(path: &Path, force_positioned: bool, visit: impl FnMut(Row<'_>) -> Result<()>) -> Result<()> {
    scan_selected_with(path, force_positioned, false, visit)
}
fn scan_selected_with(
    path: &Path,
    force_positioned: bool,
    hooks_only: bool,
    mut visit: impl FnMut(Row<'_>) -> Result<()>,
) -> Result<()> {
    let captured = JsonSource::capture(path)?;
    let file = std::fs::File::open(path)?;
    captured.check_metadata(file.metadata()?)?;
    #[cfg(feature = "mmap")]
    if !force_positioned {
        if let Ok(source) = document::Source::map_file(&file) {
            let text = std::str::from_utf8(source.bytes())?;
            for line in text.lines() {
                let line = line.trim();
                if hooks_only && !line.contains(HOOK_LITERAL) {
                    continue;
                }
                let Ok(validated) = serde_json::from_str::<&crate::lazy::RawValue>(line) else {
                    continue;
                };
                let offset = (validated.get().as_ptr() as usize - text.as_ptr() as usize) as u64;
                let raw = source
                    .slice(offset, validated.get().len() as u64)
                    .ok_or_else(|| anyhow::anyhow!("JSONL mapped range unavailable"))?;
                if document::accepts_value(raw) {
                    visit(Row::Mapped { raw, offset })?;
                }
            }
            captured.check_metadata(file.metadata()?)?;
            captured.check()?;
            return Ok(());
        }
    }
    #[cfg(not(feature = "mmap"))]
    let _ = force_positioned;
    // The context shares the opened descriptor with framing, and keeps original identity guards.
    let context = RangeSource::from_file(file, captured.clone())?;
    let file = context
        .file()
        .ok_or_else(|| anyhow::anyhow!("JSONL context has no descriptor"))?;
    let extent = context.range();
    let framed = ranges::Range {
        offset: extent.offset,
        len: extent.len,
    };
    anyhow::ensure!(
        ranges::valid_utf8(&mut ranges::reader(file, framed)?)?,
        "JSONL source contains invalid UTF-8"
    );
    for record in ranges::records(file, framed)? {
        let (range, _) = record?;
        let range = ranges::trim(file, range)?;
        if range.len == 0 || (hooks_only && !contains_hook(file, range)?) {
            continue;
        }
        let range = Range {
            offset: range.offset,
            len: range.len,
        };
        if !context.validate(range)? {
            continue;
        }
        let range = context.cursor(range)?.value()?;
        visit(Row::Positioned {
            range,
            context: &context,
            captured: &captured,
        })?;
    }
    context.check_identity()?;
    captured.check()?;
    Ok(())
}

const HOOK_LITERAL: &str = "\"hook_execution\"";
/// Preserve literal encoded-byte selection across fixed read windows; this does not inspect JSON.
fn contains_hook(file: &std::fs::File, range: ranges::Range) -> Result<bool> {
    use std::io::Read;
    let mut input = ranges::reader(file, range)?;
    let mut window = [0u8; 64 * 1024 + 16];
    let mut carry = 0;
    loop {
        let read = input.read(&mut window[carry..])?;
        if read == 0 {
            return Ok(false);
        }
        let end = carry + read;
        if memchr::memmem::find(&window[..end], HOOK_LITERAL.as_bytes()).is_some() {
            return Ok(true);
        }
        carry = (HOOK_LITERAL.len() - 1).min(end);
        window.copy_within(end - carry..end, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checkpoint_keys_and_literal_hook_selection_match_both_backends() -> Result<()> {
        let world = crate::test_world("grok-positioned-affordances");
        let path = world.base.join("rows.jsonl");
        let body = "ignored ".repeat(20000);
        fs::write(&path,format!("{{\"prompt_index\":7,\"file_snapshots\":{{\"b\":\"{body}\",\"a\":false,\"\\u0062\":null}},\"after_snapshots\":\"{body}\"}}"))?;
        for force in [false, true] {
            let mut keys = Vec::new();
            let mut prompt = None;
            scan_selected_with(&path, force, false, |row| {
                keys = row.get("file_snapshots")?.unwrap().ordered_keys()?;
                prompt = row.number_field("prompt_index")?.and_then(|n| n.as_u64());
                Ok(())
            })?;
            assert_eq!(keys, ["b", "a"]);
            assert_eq!(prompt, Some(7));
        }
        let text=concat!(
            "{\"params\":{\"update\":{\"sessionUpdate\":\"hook_execution\",\"event_name\":\"literal\"}}}\n",
            "{\"params\":{\"update\":{\"sessionUpdate\":\"hook\\u005fexecution\",\"event_name\":\"skip\"}}}\n",
            "{\"params\":{\"update\":{\"sessionUpdate\":\"hook\\u005fexecution\",\"event_name\":\"selected\"}},\"ignored\":\"hook_execution\"}\n",
            "{\"params\":{\"update\":{\"sessionUpdate\":\"hook_execution\"}},\"ignored\":\"\\ud800\"}\n"
        );
        fs::write(&path, text)?;
        for force in [false, true] {
            let mut events = Vec::new();
            scan_selected_with(&path, force, true, |row| {
                let update = row.get("params")?.unwrap().get("update")?.unwrap();
                events.push(update.string_field("event_name")?);
                Ok(())
            })?;
            assert_eq!(events, [Some("literal".into()), Some("selected".into())]);
        }
        // Exercise every placement of a split marker at the fixed window boundary.
        for split in 1..HOOK_LITERAL.len() {
            let text = format!("{}{} suffix", "x".repeat(64 * 1024 + 16 - split), HOOK_LITERAL);
            fs::write(&path, &text)?;
            let file = std::fs::File::open(&path)?;
            assert!(contains_hook(
                &file,
                ranges::Range {
                    offset: 0,
                    len: text.len() as u64
                }
            )?);
        }
        fs::write(&path, b"{\"ignored\":1}\n\xff")?;
        assert!(scan_selected_with(&path, true, true, |_| Ok(())).is_err());
        Ok(())
    }
    #[test]
    fn positioned_metadata_matches_mapped_fields_titles_and_whole_utf8_policy() -> Result<()> {
        let world = crate::test_world("grok-positioned-metadata");
        let path = world.base.join("history.jsonl");
        let body = "雪\n\"body\"".repeat(30000);
        let cases = [
            serde_json::json!({"type":"user","content":body,"ignored":body}).to_string(),
            r#"{"type":"user","ty\u0070e":"assistant","content":"old"}"#.into(),
            serde_json::json!({"type":"user","content":[{"text":""},{"ignored":body},{"text":"answer"}]}).to_string(),
            r#"{"type":"user","content":"skip","ignored":"\ud800"}"#.into(),
            r#"{"type":"assistant","ignored":1e400}"#.into(),
            "null".into(),
            "not JSON".into(),
        ];
        let text = format!("\u{2003}{}\r\n{}", cases.join("\n"), "\u{2002}");
        fs::write(&path, &text)?;
        let resolver = crate::lazy::Resolver::new(Some(path.clone()));
        let mut projected = Vec::new();
        scan_with(&path, true, |row| {
            projected.push((
                row.string_field("type")?,
                row.get("content")?.map(|v| v.title(&resolver)).transpose()?.flatten(),
            ));
            Ok(())
        })?;
        let mut mapped = Vec::new();
        scan_with(&path, false, |row| {
            mapped.push((
                row.string_field("type")?,
                row.get("content")?.map(|v| v.title(&resolver)).transpose()?.flatten(),
            ));
            Ok(())
        })?;
        assert_eq!(projected, mapped);
        let expected = text
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
            .map(|value| {
                let title = text_parts(value.get("content"));
                (
                    value.get("type").and_then(Value::as_str).map(str::to_owned),
                    (!title.trim().is_empty()).then(|| crate::ir::truncate(&title, 80)),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(projected, expected);
        fs::write(
            &path,
            format!(
                "{}\n",
                serde_json::json!({"params":{"update":{"sessionUpdate":"tool_call_update","toolCallId":"call","title":"rich","status":"failed","locations":[{"path":"a"},{"path":"b"}],"ignored":body}}})
            ),
        )?;
        let mut statuses = Vec::new();
        for force in [false, true] {
            let mut status = None;
            scan_with(&path, force, |row| {
                let update = row.get("params")?.unwrap().get("update")?.unwrap();
                status = Some((
                    update.string_field("sessionUpdate")?,
                    update.string_field("status")?,
                    update
                        .get("locations")?
                        .unwrap()
                        .array()?
                        .map(|v| v.and_then(|v| v.string_field("path")))
                        .collect::<Result<Vec<_>>>()?,
                ));
                Ok(())
            })?;
            statuses.push(status);
        }
        assert_eq!(statuses[0], statuses[1]);
        let mut invalid = text.into_bytes();
        invalid.extend_from_slice(b"\n\xff");
        fs::write(&path, invalid)?;
        assert!(scan_with(&path, true, |_| Ok(())).is_err());
        assert!(scan_with(&path, false, |_| Ok(())).is_err());
        fs::write(&path, r#"{"type":"user","content":"accepted"}"#)?;
        assert!(scan_with(&path, true, |_| {
            fs::write(&path, b"changed")?;
            Ok(())
        })
        .is_err());
        Ok(())
    }
}
