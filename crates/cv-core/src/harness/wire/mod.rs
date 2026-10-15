//! The two wire shapes most harnesses persist, read into [`Block`](crate::ir::Block)s and written
//! back out, each spelled **once**:
//!
//! - [`anthropic`]: the Messages-API content array (`text | thinking | redacted_thinking | tool_use
//!   | tool_result | image | document`) — Claude Code, Cline/Roo, the claude.ai export, and with a
//!   renamed key or two OpenClaw, Goose and Zed's legacy threads.
//! - [`openai`]: chat/Responses parts (`text | input_text | output_text | image_url | input_image`)
//!   plus `tool_calls[].function.arguments` as a JSON **string** — Codex, Kimi, Kimi Code, Hermes,
//!   Continue, Grok.
//!
//! An adapter names its store's spelling with a `Quirks` constant and keeps only what is genuinely
//! its own: where its files are and which records mean prompt, reply, tool, compaction.

use serde_json::Value;

pub mod anthropic;
pub mod openai;
pub(crate) mod write;

/// A part is an image whatever the family calls it.
pub(crate) fn is_image_part(ty: &str) -> bool {
    matches!(ty, "image" | "image_url" | "input_image" | "output_image" | "imageUrl")
}

/// Flatten a `content` field — a plain string or an array of parts — to transcript text: text
/// parts joined by newlines, an image part as the `[image]` placeholder so it is not silently
/// dropped from the searchable transcript. For a tool result's content and a message body.
pub(crate) fn coerce_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| {
                if i.get("type").and_then(Value::as_str).is_some_and(is_image_part) {
                    return Some("[image]".to_string());
                }
                i.get("text").and_then(Value::as_str).map(str::to_string)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The text parts of a `content` field only (no placeholders): for titles and cwd sniffing, where
/// `[image]` would be noise.
pub(crate) fn text_parts(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| i.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Discovery's exact text_parts title, retaining only the demanded head. The caller's checked
/// record view validates the entire row and rechecks its source after this projection.
pub(crate) fn text_title(content: Option<crate::json::JsonView<'_>>) -> anyhow::Result<Option<String>> {
    let Some(content) = content else { return Ok(None) };
    let resolver = crate::lazy::Resolver::new(None);
    if let Some(text) = content.source_text()? {
        return Ok(super::store::document::text_summary(
            std::iter::once(text),
            &resolver,
            true,
        ));
    }
    let Some(parts) = content.array()? else { return Ok(None) };
    let mut failure = None;
    let texts = parts.filter_map(|part| {
        match part.and_then(|part| {
            part.get("text")?
                .map(|value| value.source_text())
                .transpose()
                .map(Option::flatten)
        }) {
            Ok(text) => text,
            Err(error) => {
                failure = Some(error);
                None
            }
        }
    });
    let title = super::store::document::text_summary(texts, &resolver, true);
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(title)
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn selected_title_matches_full_text_join_without_part_roster() -> anyhow::Result<()> {
        let world = crate::test_world("wire-title-parts");
        let path = world.base.join("content.json");
        let mut many = vec![serde_json::json!({"text":""}); 4096];
        many.push(serde_json::json!({"text":"late"}));
        for value in [
            serde_json::json!("large λ\n\"".repeat(20000)),
            serde_json::json!([{ "text":""},{"text":""},{"text":"answer"}]),
            serde_json::json!([{ "text":" \n"},{"ignored":"large"},{"text":"\u{2003}"}]),
            serde_json::json!(many),
            serde_json::json!({"text":"ignored non-array"}),
        ] {
            std::fs::write(&path, serde_json::to_vec(&value)?)?;
            let source = crate::json::JsonSource::capture(&path)?;
            let title = source.read_range_value(0, source.len(), |view| text_title(Some(view)))?;
            let full = text_parts(Some(&value));
            let expected = (!full.trim().is_empty()).then(|| crate::ir::truncate(&full, 80));
            assert_eq!(title, expected);
        }
        Ok(())
    }
    #[test]
    fn coerce_and_text_parts() {
        let arr = json!([
            {"type": "text", "text": "a"},
            {"type": "image", "source": {"type": "base64", "data": "…"}},
            {"type": "input_text", "text": "b"},
            {"type": "tool_use", "id": "x"}
        ]);
        assert_eq!(coerce_text(Some(&arr)), "a\n[image]\nb");
        assert_eq!(text_parts(Some(&arr)), "a\nb");
        assert_eq!(coerce_text(Some(&json!("plain"))), "plain");
        assert_eq!(text_parts(Some(&json!("plain"))), "plain");
        assert_eq!(coerce_text(None), "");
        assert_eq!(coerce_text(Some(&json!({"text": "not an array"}))), "");
    }
}
