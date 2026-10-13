//! The OpenAI chat / Responses content shape: parts (`text | input_text | output_text |
//! image_url | input_image | …`) and `tool_calls[]` whose `function.arguments` is a JSON
//! **string**. Codex, Kimi, Kimi Code, Hermes, Continue and Grok all persist a dialect of it; a
//! [`Quirks`] constant names the dialect.

use super::str_field;
use crate::ir::Block;
use crate::lazy::Text;
use serde_json::Value;

/// How a store spells the shape. [`OPENAI`] is the plain chat-completions spelling.
#[derive(Debug, Clone, Copy)]
pub struct Quirks {
    /// Drop a text part whose text is empty (Codex, Continue); Hermes and Kimi keep it.
    pub skip_empty_text: bool,
    /// A `data:` image URL is recorded as the marker `base64:inline` with its media type lifted
    /// out of the URL (Codex); otherwise the URL is kept as the reference.
    pub data_url_as_marker: bool,
    /// Keep at most this many chars of an image reference (Hermes: 120).
    pub image_ref_truncate: Option<usize>,
    /// The reasoning part's `type` and text key (Kimi: `("think", "think")`); `None` for a store
    /// with no reasoning part.
    pub thinking: Option<(&'static str, &'static str)>,
    /// Mirror the reasoning part's `encrypted` blob into `signature` too (kimi-cli).
    pub thinking_signature_mirrors_encrypted: bool,
    /// Drop a reasoning part with neither text nor blob (kimi-cli and Kimi Code: kosong ≥ 0.55 writes
    /// `{"think":"","encrypted":null}` for an empty stream — nothing to show, nothing to re-send).
    pub skip_empty_thinking: bool,
}

/// Plain chat completions: Hermes.
pub const OPENAI: Quirks = Quirks {
    skip_empty_text: false,
    data_url_as_marker: false,
    image_ref_truncate: None,
    thinking: None,
    thinking_signature_mirrors_encrypted: false,
    skip_empty_thinking: false,
};
/// Hermes: plain, with long image references cut at 120 chars.
pub const HERMES: Quirks = Quirks {
    image_ref_truncate: Some(120),
    ..OPENAI
};
/// Codex rollouts: empty text dropped, `data:` images as the inline marker.
pub const CODEX: Quirks = Quirks {
    skip_empty_text: true,
    data_url_as_marker: true,
    ..OPENAI
};
/// Continue: empty text dropped, `imageUrl` parts.
pub const CONTINUE: Quirks = Quirks {
    skip_empty_text: true,
    ..OPENAI
};
/// kimi-cli: `think` parts whose blob doubles as the signature; empty ones dropped (1.49+, kosong
/// ≥ 0.55, writes `{"think":"","encrypted":null}` for every step that streamed no reasoning).
pub const KIMI: Quirks = Quirks {
    thinking: Some(("think", "think")),
    thinking_signature_mirrors_encrypted: true,
    skip_empty_thinking: true,
    ..OPENAI
};
/// Kimi Code: `think` parts, empty ones dropped.
pub const KIMI_CODE: Quirks = Quirks {
    thinking: Some(("think", "think")),
    skip_empty_thinking: true,
    ..OPENAI
};

/// Every block of a `content` field: a bare string is one text block (none when empty), an array
/// is its parts through [`part_to_block`].
pub fn blocks_from(content: Option<&Value>, q: &Quirks) -> Vec<Block> {
    match content {
        Some(Value::String(s)) if !s.is_empty() => vec![Block::Text { text: s.clone().into() }],
        Some(Value::Array(parts)) => parts.iter().filter_map(|p| part_to_block(p, q)).collect(),
        _ => Vec::new(),
    }
}

/// Selected counterpart of [`blocks_from`], retaining checked text and argument backing.
pub(crate) fn blocks_view(content: Option<crate::json::JsonView<'_>>, q: &Quirks) -> anyhow::Result<Vec<Block>> {
    let Some(content) = content else { return Ok(Vec::new()) };
    if let Some(text) = content.source_text()? {
        return Ok(if text.is_empty() {
            Vec::new()
        } else {
            vec![Block::Text { text }]
        });
    }
    let mut blocks = Vec::new();
    if let Some(parts) = content.array()? {
        for part in parts {
            if let Some(block) = part_view(part?, q)? {
                blocks.push(block);
            }
        }
    }
    Ok(blocks)
}

/// Apply the same dialect rules while selecting only the part's semantic fields.
pub(crate) fn part_view(part: crate::json::JsonView<'_>, q: &Quirks) -> anyhow::Result<Option<Block>> {
    use crate::harness::store::document::Fields;
    let Some(fields) = Fields::parse(part, &["type", "text", "think", "encrypted"])? else {
        return Ok(None);
    };
    let ty = fields.string("type")?.unwrap_or_default();
    if let Some((think_ty, text_key)) = q.thinking {
        if ty == think_ty {
            let text = part
                .get(text_key)?
                .map(|v| v.source_text())
                .transpose()?
                .flatten()
                .unwrap_or_default();
            let encrypted = fields.string("encrypted")?;
            if q.skip_empty_thinking && text.is_empty() && encrypted.is_none() {
                return Ok(None);
            }
            return Ok(Some(Block::Thinking {
                text,
                signature: if q.thinking_signature_mirrors_encrypted {
                    encrypted.clone()
                } else {
                    None
                },
                encrypted,
                redacted: false,
            }));
        }
    }
    if super::is_image_part(&ty) {
        let nested = |key| -> anyhow::Result<Option<crate::json::JsonView<'_>>> {
            part.get(key)?.map(|v| v.get("url")).transpose().map(Option::flatten)
        };
        let url = nested("image_url")?
            .or(part.get("image_url")?)
            .or(nested("imageUrl")?)
            .or(part.get("url")?);
        let url = match url {
            Some(url) if url.is_string()? => Some(url),
            _ => None,
        };
        let (media_type, data_ref) = match url {
            None => (None, None),
            Some(url) if q.data_url_as_marker => {
                let text = url.source_text()?.unwrap();
                let resolver = crate::lazy::Resolver::new(None);
                let (prefix, _) = crate::harness::store::document::text_head(&text, &resolver, 5, false)?;
                if prefix == "data:" {
                    #[derive(Debug, thiserror::Error)]
                    #[error("data URL media header complete")]
                    struct HeaderComplete;
                    let mut skip = 5;
                    let mut media = String::new();
                    match text.try_for_each_chunk(&resolver, 4096, |chunk| {
                        for c in chunk.chars() {
                            if skip > 0 {
                                skip -= 1;
                                continue;
                            }
                            if c == ';' {
                                return Err(HeaderComplete.into());
                            }
                            media.push(c);
                        }
                        Ok(())
                    }) {
                        Ok(()) => {}
                        Err(error) if error.is::<HeaderComplete>() => {}
                        Err(error) => return Err(error),
                    }
                    (media.contains('/').then_some(media), Some("base64:inline".into()))
                } else {
                    (None, Some(url.resolve()?.as_str().unwrap().to_owned()))
                }
            }
            Some(url) => {
                let value = if let Some(max) = q.image_ref_truncate {
                    let text = url.source_text()?.unwrap();
                    let (prefix, _) = crate::harness::store::document::text_head(
                        &text,
                        &crate::lazy::Resolver::new(None),
                        max.saturating_add(1),
                        false,
                    )?;
                    crate::ir::truncate(&prefix, max)
                } else {
                    url.resolve()?.as_str().unwrap().to_owned()
                };
                (None, Some(value))
            }
        };
        return Ok(Some(Block::Image { media_type, data_ref }));
    }
    if matches!(ty.as_str(), "audio_url" | "video_url") {
        let source = part
            .get(&ty)?
            .map(|v| v.get("url"))
            .transpose()?
            .flatten()
            .map(|v| v.resolve().map(|v| v.as_str().map(str::to_owned)))
            .transpose()?
            .flatten();
        return Ok(Some(Block::File {
            mime: None,
            path: None,
            source,
        }));
    }
    Ok(fields
        .get("text")
        .map(|v| v.source_text())
        .transpose()?
        .flatten()
        .filter(|text| !q.skip_empty_text || !text.is_empty())
        .map(|text| Block::Text { text }))
}

/// Selected chat/Responses call, including checked JSON-encoded argument handling.
pub(crate) fn tool_call_view(call: crate::json::JsonView<'_>) -> anyhow::Result<Block> {
    use crate::harness::store::document::Fields;
    let fields = Fields::parse(call, &["id", "call_id", "name", "arguments", "function"])?;
    let function = fields
        .as_ref()
        .and_then(|f| f.get("function"))
        .map(|v| Fields::parse(v, &["name", "arguments"]))
        .transpose()?
        .flatten();
    let get = |key| {
        function
            .as_ref()
            .and_then(|f| f.get(key))
            .or_else(|| fields.as_ref().and_then(|f| f.get(key)))
    };
    let name = get("name")
        .map(|v| v.resolve().map(|v| v.as_str().unwrap_or("").to_owned()))
        .transpose()?
        .unwrap_or_default();
    let id = match &fields {
        Some(fields) => {
            if fields.get("id").is_some() {
                fields.string("id")?
            } else {
                fields.string("call_id")?
            }
        }
        None => None,
    }
    .unwrap_or_default();
    let input = match get("arguments") {
        Some(v) => v.encoded_argument()?,
        None => Value::Object(Default::default()).into(),
    };
    Ok(Block::ToolUse {
        id,
        name,
        input,
        namespace: None,
    })
}

/// One part → one [`Block`]. Any part carrying a string `text` is text (`text`, `input_text`,
/// `output_text`, `summary_text`, …); the image types become an [`Block::Image`] reference; a
/// reasoning part (when the dialect has one) a [`Block::Thinking`]; Kimi's `audio_url`/`video_url`
/// a [`Block::File`]. `None` for anything else.
pub fn part_to_block(p: &Value, q: &Quirks) -> Option<Block> {
    let ty = p.get("type").and_then(Value::as_str).unwrap_or("");
    if let Some((think_ty, text_key)) = q.thinking {
        if ty == think_ty {
            let text = p.get(text_key).and_then(Value::as_str).unwrap_or("");
            let encrypted = p.get("encrypted").and_then(Value::as_str).map(str::to_string);
            if q.skip_empty_thinking && text.is_empty() && encrypted.is_none() {
                return None;
            }
            return Some(Block::Thinking {
                text: Text::from(text),
                signature: if q.thinking_signature_mirrors_encrypted {
                    encrypted.clone()
                } else {
                    None
                },
                encrypted,
                redacted: false,
            });
        }
    }
    if super::is_image_part(ty) {
        return Some(image_from(p, q));
    }
    if matches!(ty, "audio_url" | "video_url") {
        return Some(Block::File {
            mime: None,
            path: None,
            source: nested_url(p, ty),
        });
    }
    let t = p.get("text").and_then(Value::as_str)?;
    if q.skip_empty_text && t.is_empty() {
        return None;
    }
    Some(Block::Text { text: Text::from(t) })
}

/// `{<key>: {url}}` — media parts nest the URL under a key named after the type.
fn nested_url(p: &Value, key: &str) -> Option<String> {
    p.get(key)
        .and_then(|u| u.get("url"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// An image part's URL: `image_url.url` (chat), `image_url` as a string (Responses `input_image`),
/// `imageUrl.url` (Continue), or a bare `url`.
fn image_url(p: &Value) -> Option<&str> {
    p.pointer("/image_url/url")
        .or_else(|| p.get("image_url"))
        .or_else(|| p.pointer("/imageUrl/url"))
        .or_else(|| p.get("url"))
        .and_then(Value::as_str)
}

fn image_from(p: &Value, q: &Quirks) -> Block {
    let url = image_url(p);
    if q.data_url_as_marker {
        // Never inline the bytes: a `data:` URL becomes the marker, its media type lifted out;
        // an `http(s)`/`file:` URL is kept as a reference.
        let media_type = url.and_then(|u| {
            u.strip_prefix("data:")
                .and_then(|rest| rest.split(';').next())
                .filter(|m| m.contains('/'))
                .map(str::to_string)
        });
        let data_ref = url.map(|u| {
            if u.starts_with("data:") {
                "base64:inline".to_string()
            } else {
                u.to_string()
            }
        });
        return Block::Image { media_type, data_ref };
    }
    Block::Image {
        media_type: None,
        data_ref: url.map(|u| match q.image_ref_truncate {
            Some(n) => crate::ir::truncate(u, n),
            None => u.to_string(),
        }),
    }
}

/// A tool call's `arguments`: a JSON-encoded **string** on the wire (possibly streamed/partial).
/// A string goes through [`structured_arguments`]; absent/null is the empty object; an
/// already-structured value is itself.
pub fn parse_arguments(v: Option<&Value>) -> Value {
    match v {
        Some(Value::String(s)) => structured_arguments(s),
        Some(Value::Null) | None => Value::Object(Default::default()),
        Some(other) => other.clone(),
    }
}

/// The JSON-encoded `arguments` string decoded — but only when it decodes to an object or array.
/// A bare scalar (the literal argument `"42"` or `"true"`) and a partial/unparseable string stay
/// a JSON string verbatim, so a call's payload is never silently retyped and nothing is lost;
/// a blank string is the empty object.
pub fn structured_arguments(s: &str) -> Value {
    if s.trim().is_empty() {
        return Value::Object(Default::default());
    }
    match serde_json::from_str::<Value>(s) {
        Ok(v @ Value::Object(_)) | Ok(v @ Value::Array(_)) => v,
        _ => Value::String(s.to_string()),
    }
}

/// One `tool_calls[]` entry → a [`Block::ToolUse`]: `{id, function: {name, arguments}}` (chat) or
/// the flattened `{id, name, arguments}` (Grok), `call_id` as the id's Responses spelling.
pub fn tool_call(c: &Value) -> Block {
    let func = c.get("function");
    let name = func
        .and_then(|f| f.get("name"))
        .or_else(|| c.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let id = if c.get("id").is_some() {
        str_field(c, "id")
    } else {
        str_field(c, "call_id")
    };
    Block::ToolUse {
        id,
        name,
        input: parse_arguments(func.and_then(|f| f.get("arguments")).or_else(|| c.get("arguments"))).into(),
        namespace: None,
    }
}

/// Every entry of a `tool_calls` array (a JSON array value, or `None`).
pub fn tool_calls(v: Option<&Value>) -> Vec<Block> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().map(tool_call).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn checked_views_match_owned_parts_and_all_argument_shapes() {
        use crate::json::JsonSource;
        let world = cv_testkit::World::in_process("openai_checked_views");
        let path = world.home.join("selected.json");
        let long = "é\n\"\\".repeat(10_000);
        let parts = json!([
            {"type":"text","text":long},
            {"type":"think","think":long,"encrypted":"opaque"},
            {"type":"image_url","image_url":{"url":format!("data:image/png;base64,{long}")},"ignored":long},
            {"type":"input_image","image_url":format!("https://example/{long}")},
            {"type":"audio_url","audio_url":{"url":"file:audio"},"ignored":long},
            {"type":"image_url","image_url":{"url":null},"url":"fallback-must-not-win"},
            {"type":"image_url","imageUrl":{"url":"nested"}},
            {"type":"think","think":"","encrypted":null},
            {"text":""},
            null
        ]);
        let bytes = serde_json::to_vec(&parts).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let source = JsonSource::capture(&path).unwrap();
        for q in [&KIMI, &KIMI_CODE, &HERMES, &CODEX] {
            let selected = source
                .read_range_value(0, bytes.len() as u64, |view| blocks_view(Some(view), q))
                .unwrap();
            assert_eq!(
                serde_json::to_value(selected).unwrap(),
                serde_json::to_value(blocks_from(Some(&parts), q)).unwrap()
            );
        }
        drop(source);
        for arguments in [
            json!({"payload":long}),
            json!([long]),
            Value::Null,
            json!(42),
            json!(true),
            json!(format!("{{\"payload\":{}}}", serde_json::to_string(&long).unwrap())),
            json!(format!("{{unfinished{long}")),
            json!("42"),
            json!(" "),
        ] {
            let call = json!({"id":"call","function":{"name":"Read","arguments":arguments},"ignored":long});
            let bytes = serde_json::to_vec(&call).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            let source = JsonSource::capture(&path).unwrap();
            let selected = source.read_range_value(0, bytes.len() as u64, tool_call_view).unwrap();
            assert_eq!(
                serde_json::to_value(&selected).unwrap(),
                serde_json::to_value(tool_call(&call)).unwrap()
            );
        }
        let call = json!({"call_id":"missing","name":"Read"});
        assert_eq!(
            serde_json::to_value(tool_call_view((&call).into()).unwrap()).unwrap(),
            serde_json::to_value(tool_call(&call)).unwrap()
        );
    }

    #[test]
    fn parts_in_every_dialect() {
        let parts = json!([
            {"type": "input_text", "text": "hi"},
            {"type": "text", "text": ""},
            {"type": "summary_text", "text": "s"},
            {"type": "input_image", "image_url": "data:image/png;base64,AAA"},
            {"type": "image_url", "image_url": {"url": "https://x/y.png"}},
            {"type": "imageUrl", "imageUrl": {"url": "https://x/z.png"}},
            {"type": "refusal"}
        ]);
        let plain = blocks_from(Some(&parts), &OPENAI);
        assert_eq!(plain.len(), 6);
        assert!(matches!(&plain[1], Block::Text { text } if text.is_empty()));
        assert!(
            matches!(&plain[3], Block::Image { media_type: None, data_ref: Some(u) } if u == "data:image/png;base64,AAA")
        );
        assert!(matches!(&plain[5], Block::Image { data_ref: Some(u), .. } if u == "https://x/z.png"));
        let codex = blocks_from(Some(&parts), &CODEX);
        assert_eq!(codex.len(), 5, "Codex drops the empty text");
        assert!(
            matches!(&codex[2], Block::Image { media_type: Some(m), data_ref: Some(r) } if m == "image/png" && r == "base64:inline")
        );
        let long = json!([{"type": "image_url", "image_url": {"url": format!("https://x/{}", "a".repeat(200))}}]);
        assert!(
            matches!(&blocks_from(Some(&long), &HERMES)[0], Block::Image { data_ref: Some(r), .. } if r.chars().count() == 120)
        );
        assert_eq!(blocks_from(Some(&json!("bare")), &OPENAI).len(), 1);
        assert!(blocks_from(Some(&json!("")), &OPENAI).is_empty());
    }

    #[test]
    fn kimi_think_parts() {
        let parts = json!([
            {"type": "think", "think": "why", "encrypted": "BLOB"},
            {"type": "think", "think": "", "encrypted": null},
            {"type": "audio_url", "audio_url": {"url": "file:///a.wav"}}
        ]);
        let kimi = blocks_from(Some(&parts), &KIMI);
        assert_eq!(
            kimi.len(),
            2,
            "kimi-cli 1.49+ writes an empty think per step; it is dropped"
        );
        assert!(
            matches!(&kimi[0], Block::Thinking { signature: Some(s), encrypted: Some(e), .. } if s == "BLOB" && e == "BLOB")
        );
        assert!(matches!(&kimi[1], Block::File { source: Some(u), .. } if u == "file:///a.wav"));
        let code = blocks_from(Some(&parts), &KIMI_CODE);
        assert_eq!(code.len(), 2, "Kimi Code drops the empty think");
        assert!(matches!(
            &code[0],
            Block::Thinking {
                signature: None,
                encrypted: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn arguments_and_tool_calls() {
        assert_eq!(parse_arguments(None), json!({}));
        assert_eq!(parse_arguments(Some(&json!(""))), json!({}));
        assert_eq!(parse_arguments(Some(&json!(null))), json!({}));
        assert_eq!(parse_arguments(Some(&json!("{\"a\":1}"))), json!({"a": 1}));
        assert_eq!(parse_arguments(Some(&json!("{partial"))), json!("{partial"));
        assert_eq!(
            parse_arguments(Some(&json!("42"))),
            json!("42"),
            "a scalar is not retyped"
        );
        assert_eq!(parse_arguments(Some(&json!("[1]"))), json!([1]));
        assert_eq!(parse_arguments(Some(&json!({"b": 2}))), json!({"b": 2}));
        let calls = json!([
            {"id": "c1", "type": "function", "function": {"name": "shell", "arguments": "{\"cmd\":\"ls\"}"}},
            {"call_id": "c2", "name": "grep", "arguments": "{}"}
        ]);
        let b = tool_calls(Some(&calls));
        assert!(
            matches!(&b[0], Block::ToolUse { id, name, input, .. } if id == "c1" && name == "shell" && input.resolve().unwrap()["cmd"] == "ls")
        );
        assert!(
            matches!(&b[1], Block::ToolUse { id, name, input, .. } if id == "c2" && name == "grep" && input.resolve().unwrap().as_ref() == &json!({}))
        );
        assert!(tool_calls(Some(&json!("not an array"))).is_empty());
    }
}
