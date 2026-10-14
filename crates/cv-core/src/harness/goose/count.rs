//! Count the codec's actual turns from small content-shape witnesses, never payload trees.
use super::*;
use crate::harness::store::document::{self, Fields, Raw};
use crate::json::JsonView;
use serde_json::json;

#[derive(Default)]
struct Fold {
    any: bool,
    only_results: bool,
    decoded: Decoded,
}
impl Fold {
    fn part(&mut self, part: Value) {
        let decoded = decode_content(&Value::Array(vec![part]), &mut HashMap::new());
        if !decoded.blocks.is_empty() {
            let results = decoded.blocks.iter().all(|b| matches!(b, Block::ToolResult { .. }));
            self.only_results = if self.any {
                self.only_results && results
            } else {
                results
            };
            self.any = true;
        }
        self.decoded.substantive |= decoded.substantive;
        if self.decoded.error.is_none() {
            self.decoded.error = decoded.error;
        }
        if !decoded.control.is_empty() {
            self.decoded.control = vec![Value::Null];
        }
    }
    fn turn(mut self, role: &str, hidden: bool) -> bool {
        if !self.any {
            return false;
        }
        self.decoded.blocks = vec![if self.only_results {
            Block::ToolResult {
                tool_use_id: String::new(),
                content: "".into(),
                is_error: false,
                tool_name: None,
                status: None,
                details: None,
            }
        } else {
            Block::Text { text: "x".into() }
        }];
        let Some(mut message) = build_message(role, None, None, None, self.decoded) else {
            return false;
        };
        if hidden {
            apply_metadata(&mut message, json!({"userVisible":false}), None, false);
        }
        message.is_turn()
    }
}

fn part(ty: Option<String>, text: bool, call: Option<(bool, bool)>, result: bool) -> Value {
    let mut value = json!({"type":ty});
    if text {
        value["text"] = json!("x");
    }
    if let Some((error, has_value)) = call {
        value["toolCall"] = if error {
            json!({"status":"error"})
        } else if has_value {
            json!({"value":null})
        } else {
            Value::Null
        };
    }
    if result {
        value["toolResult"] = Value::Null;
    }
    value
}

/// The legacy checked reader uses the same witness/codec classification as SQLite discovery.
pub(super) fn view(role: &str, content: Option<JsonView<'_>>) -> Result<bool> {
    let Some(content) = content else {
        return Ok(false);
    };
    let mut fold = Fold::default();
    if content.is_string()? {
        fold.any = !content.source_text()?.unwrap().is_empty();
        fold.decoded.substantive = fold.any;
    } else if let Some(items) = content.array()? {
        for item in items {
            let Some(fields) = Fields::parse(item?, &["type", "text", "toolCall", "toolResult"])? else {
                continue;
            };
            let call = fields
                .get("toolCall")
                .map(|call| {
                    let fields = Fields::parse(call, &["status", "value"])?;
                    Ok::<_, anyhow::Error>((
                        fields
                            .as_ref()
                            .map(|f| f.string("status"))
                            .transpose()?
                            .flatten()
                            .as_deref()
                            == Some("error"),
                        fields.as_ref().and_then(|f| f.get("value")).is_some(),
                    ))
                })
                .transpose()?;
            fold.part(part(
                fields.string("type")?,
                fields.get("text").map(|v| v.is_string()).transpose()?.unwrap_or(false),
                call,
                fields.get("toolResult").is_some(),
            ));
        }
    }
    Ok(fold.turn(role, false))
}

fn accepted<'a>(text: &'a str, index: &'a document::Index) -> Option<Raw<'a>> {
    let syntax: &crate::lazy::RawValue = serde_json::from_str(text).ok()?;
    let raw = Raw::new(syntax.get(), index);
    document::accepts_value(raw).then_some(raw)
}
fn string(raw: Option<Raw<'_>>) -> Option<String> {
    raw.and_then(|raw| serde_json::from_str(raw.get()).ok())
}
fn fields<'a>(raw: Raw<'a>, keys: &[&str]) -> Vec<Option<Raw<'a>>> {
    document::select_fields(raw, keys).unwrap_or_else(|| vec![None; keys.len()])
}

/// Borrow a SQLite TEXT cell only for this cursor step. No spool or retained source is needed:
/// the surrounding ReadConnection already guards publication of the discovery operation.
pub(super) fn cell(role: &str, content: &str, metadata: Option<&str>) -> bool {
    let index = document::Index::default();
    let Some(content) = accepted(content, &index) else {
        return false;
    };
    let mut fold = Fold::default();
    if content.get().starts_with('"') {
        fold.any = content.get() != "\"\"";
        fold.decoded.substantive = fold.any;
    } else if let Some(items) = document::array_iter(content) {
        for item in items {
            let selected = fields(item, &["type", "text", "toolCall", "toolResult"]);
            let call = selected[2].map(|call| {
                let selected = fields(call, &["status", "value"]);
                (string(selected[0]).as_deref() == Some("error"), selected[1].is_some())
            });
            fold.part(part(
                string(selected[0]),
                selected[1].is_some_and(|text| text.get().starts_with('"')),
                call,
                selected[3].is_some(),
            ));
        }
    }
    let hidden = metadata
        .and_then(|m| accepted(m, &index))
        .is_some_and(|m| fields(m, &["userVisible"])[0].is_some_and(|flag| flag.get() == "false"));
    fold.turn(role, hidden)
}
