//! Bounded count/title projection over strict shared JSON ranges.
use super::*;

/// Count/title metadata through the shared range grammar. Scalar validation and discarded
/// bodies stay bounded even for escaped strings, unlike serde's whole-string scratch.
pub(super) fn view(view: crate::json::JsonView<'_>) -> Result<Value> {
    use crate::harness::store::document::Fields;
    anyhow::ensure!(view.accepts_value()?, "Cursor CLI message is not Value-compatible JSON");
    let Some(fields) = Fields::parse(view, &["role", "content", "providerOptions"])? else {
        return Ok(Value::Null);
    };
    let mut out = Map::new();
    out.insert(
        "role".into(),
        fields
            .get("role")
            .map(|v| prefix(v, 32, false))
            .transpose()?
            .flatten()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    if let Some(provider) = fields.get("providerOptions") {
        if let Some(cursor) = provider.get("cursor")? {
            let context = cursor
                .get("requestContextCompleteness")?
                .map(|v| v.is_null())
                .transpose()?
                == Some(false);
            let summary = cursor.get("isSummary")?.map(|v| v.as_bool()).transpose()?.flatten() == Some(true);
            out.insert("providerOptions".into(),serde_json::json!({"cursor":{"requestContextCompleteness":if context {Value::Bool(true)} else {Value::Null},"isSummary":summary}}));
        }
    }
    if let Some(content) = fields.get("content") {
        if let Some(text) = prefix(content, 96, true)? {
            out.insert("content".into(), text.into());
        } else if let Some(parts) = content.array()? {
            let mut title = String::new();
            let mut title_chars = 0;
            let mut material = None;
            for part in parts {
                let Some(fields) = Fields::parse(part?, &["type", "text"])? else {
                    continue;
                };
                let Some(kind) = fields.get("type").map(|v| prefix(v, 32, false)).transpose()?.flatten() else {
                    continue;
                };
                if kind == "text" {
                    if let Some(text) = fields
                        .get("text")
                        .map(|v| prefix(v, 96usize.saturating_sub(title_chars), true))
                        .transpose()?
                        .flatten()
                        .filter(|text| !text.is_empty())
                    {
                        if !title.is_empty() && title_chars < 96 {
                            title.push('\n');
                            title_chars += 1;
                        }
                        for ch in text.chars().take(96usize.saturating_sub(title_chars)) {
                            title.push(ch);
                            title_chars += 1;
                        }
                        material.get_or_insert_with(|| "text".to_owned());
                    }
                } else if matches!(
                    kind.as_str(),
                    "reasoning" | "redacted-reasoning" | "tool-call" | "tool-result" | "image" | "file"
                ) {
                    material.get_or_insert(kind);
                }
            }
            let mut content = Vec::new();
            if let Some(kind) = material.filter(|kind| kind != "text") {
                content.push(serde_json::json!({"type":kind}));
            }
            if !title.is_empty() {
                content.push(serde_json::json!({"type":"text","text":title}));
            }
            out.insert("content".into(), content.into());
        }
    }
    Ok(Value::Object(out))
}

#[derive(Debug, thiserror::Error)]
#[error("Cursor CLI bounded title prefix complete")]
struct PrefixComplete;
fn prefix(view: crate::json::JsonView<'_>, max_chars: usize, trim: bool) -> Result<Option<String>> {
    if !view.is_string()? {
        return Ok(None);
    }
    let mut text = String::new();
    let mut chars = 0;
    let mut trimming = trim;
    let mut present = false;
    let result = view.for_each_str_chunk(|chunk| {
        present |= !chunk.is_empty();
        for ch in chunk.chars() {
            if trimming && ch.is_whitespace() {
                continue;
            }
            trimming = false;
            if chars == max_chars {
                return Err(PrefixComplete.into());
            }
            text.push(ch);
            chars += 1;
        }
        Ok(())
    });
    match result {
        Err(error) if error.downcast_ref::<PrefixComplete>().is_some() => {}
        other => {
            other?;
        }
    }
    if text.is_empty() && present {
        text.push(' ');
    }
    Ok(Some(text))
}

#[cfg(test)]
pub(super) fn read(input: impl Read) -> Result<Value> {
    let backing = std::sync::Arc::new(ByteSource::read(input)?);
    let path = std::env::temp_dir().join(format!("cv-cursor-cli-projection-{}", uuid::Uuid::new_v4()));
    struct Remove(PathBuf);
    impl Drop for Remove {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _cleanup = Remove(path.clone());
    let mut file = std::fs::File::create(&path)?;
    backing.copy_to(&mut file)?;
    let captured = crate::json::JsonSource::capture(&path)?;
    let length = backing.len()?;
    captured.read_backing_syntax(backing, 0, length, view)
}
