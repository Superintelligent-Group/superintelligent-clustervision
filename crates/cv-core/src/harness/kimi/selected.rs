//! Selected context bodies use the same dialect mapping as owned Kimi records.
use super::*;
use crate::harness::store::document::Fields;
use crate::json::{JsonSource, JsonView};

pub(super) fn message(
    view: JsonView<'_>,
    source: &JsonSource,
    enrich: &WireEnrich,
    complete: bool,
    names: &HashMap<String, String>,
) -> Result<Option<Message>> {
    let Some(fields) = Fields::parse(view, &["role", "content", "tool_calls", "tool_call_id"])? else {
        return Ok(None);
    };
    let role = fields.string("role")?.unwrap_or_default();
    let m = match role.as_str() {
        "_system_prompt" => {
            let mut m = Message::of_kind(Role::System, MessageKind::SystemPrompt, Origin::Harness);
            m.content.push(Block::Text {
                text: text_parts_view(fields.get("content"), source)?,
            });
            m
        }
        "user" | "assistant" => {
            let mut m = if role == "user" {
                Message::of_kind(Role::User, MessageKind::Prompt, Origin::Human)
            } else {
                Message::of_kind(Role::Assistant, MessageKind::Reply, Origin::Model)
            };
            m.content = openai::blocks_from_view(fields.get("content"), &openai::KIMI, &mut |v| v.source_text())?;
            if role == "assistant" {
                if let Some(calls) = fields.get("tool_calls").map(|v| v.array()).transpose()?.flatten() {
                    for call in calls {
                        m.content.push(openai::tool_call_from_view(call?, true)?);
                    }
                }
            } else {
                classify(&mut m)?;
            }
            if m.content.is_empty() {
                return Ok(None);
            }
            m
        }
        "tool" => {
            let id = fields.string("tool_call_id")?.unwrap_or_default();
            let (content, is_error, details) = match enrich.tool_results.get(&id) {
                Some(result) => (enrich.output(result), result.is_error, enrich.extras(result)),
                None => (text_parts_view(fields.get("content"), source)?, false, None),
            };
            let mut m = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness);
            m.content.push(Block::ToolResult {
                tool_name: names.get(&id).cloned(),
                tool_use_id: id,
                content,
                is_error,
                status: Some(if is_error { "error" } else { "completed" }.into()),
                details,
            });
            m
        }
        // Checkpoint carriers have no ordinary body contract to defer.
        "_checkpoint" if complete => {
            return Ok(context_message_inner(view.resolve()?.as_ref(), enrich, complete, names))
        }
        _ => return Ok(None),
    };
    Ok(Some(m))
}

fn text_parts_view(content: Option<JsonView<'_>>, source: &JsonSource) -> Result<Text> {
    let Some(content) = content else {
        return Ok(Text::default());
    };
    if let Some(text) = content.source_text()? {
        return Ok(text);
    }
    let Some(parts) = content.array()? else {
        return Ok(Text::default());
    };
    let arena = source.decoded_backing()?;
    super::super::store::text::append(source, &arena, |writer| {
        let mut first = true;
        for part in parts {
            let part = part?;
            let Some(text) = part.get("text")? else { continue };
            if !text.is_string()? {
                continue;
            }
            if !first {
                writer.push("\n")?;
            }
            first = false;
            writer.push_string(text)?;
        }
        Ok(())
    })
}

/// Only the first non-whitespace marker prefix decides the kind; retain no transcript body.
fn classify(message: &mut Message) -> Result<()> {
    let Some(Block::Text { text }) = message.content.first() else {
        return Ok(());
    };
    let (head, _) = crate::harness::store::document::text_head(
        text,
        &crate::lazy::Resolver::new(None),
        COMPACTION_SUMMARY_TAG.chars().count(),
        true,
    )?;
    let (kind, origin) = if head.starts_with(COMPACTION_SUMMARY_TAG) {
        (MessageKind::CompactionSummary, Origin::Harness)
    } else if head.starts_with("<notification") {
        (MessageKind::InjectedContext, Origin::Scheduler)
    } else if head.starts_with("<system>") || head.starts_with("<system-reminder>") {
        (MessageKind::InjectedContext, Origin::Harness)
    } else {
        return Ok(());
    };
    message.kind = kind;
    message.origin = origin;
    Ok(())
}

pub(super) fn fold_usage(message: &mut Message, view: JsonView<'_>) -> Result<()> {
    let Some(token) = view.get("token_count")? else {
        return Ok(());
    };
    if let Some(number) = token.as_number()? {
        let value = Value::Number(number);
        message
            .harness_extra_mut(Harness::Kimi)
            .insert("token_count".into(), value.clone());
        if message.kind == MessageKind::Reply && message.usage.is_none() {
            message.usage = Some(Usage {
                input_tokens: value.as_u64(),
                ..Usage::default()
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::kimi::tests::{old_context_oracle, ResumeKeep};

    #[test]
    fn checked_bodies_match_owned_rotation_usage_and_source_replay() {
        let world = cv_testkit::World::in_process("kimi_selected_bodies");
        let home = world.home.join(".kimi");
        let dir = home.join("sessions/hash/session");
        fs::create_dir_all(&dir).unwrap();
        let payload = "é\n\"\\".repeat(20_000);
        let prefix = serde_json::json!({"role":"_system_prompt","content":"system"}).to_string() + "\n";
        let old = format!("{prefix}{}\n", serde_json::json!({"role":"user","content":"old"}));
        fs::write(dir.join("context_1.jsonl"), &old).unwrap();
        let body = format!(
            "{prefix}{}\n{}\n{}\n{}\n{}\n{}\nbad json\n",
            serde_json::json!({"role":"user","content":[{"type":"text","text":format!("{}{} {payload}", "\u{2003}".repeat(2_000), COMPACTION_SUMMARY_TAG)}]}),
            serde_json::json!({"role":"_checkpoint","id":3}),
            serde_json::json!({"role":"assistant","content":[{"type":"think","think":payload,"encrypted":"opaque"},{"type":"text","text":payload}],"tool_calls":[{"id":"call","function":{"name":"Read","arguments":serde_json::json!({"payload":payload}).to_string()}}]}),
            serde_json::json!({"role":"_usage","token_count":37}),
            serde_json::json!({"role":"tool","tool_call_id":"call","content":[{"text":payload},{"type":"image_url","image_url":{"url":"file:image"}},{"text":""},{"text":payload}]}),
            serde_json::json!({"role":"assistant","content":[{"type":"think","think":"","encrypted":null},{"text":"tail"}]})
        );
        let path = dir.join("context.jsonl");
        fs::write(&path, &body).unwrap();
        let adapter = Kimi::with_home(&home);
        let r = SessionRef::new("session", Harness::Kimi, &dir);
        for opts in [
            ParseOptions::lazy(),
            ParseOptions {
                spans: true,
                ..ParseOptions::complete()
            },
        ] {
            let expected = old_context_oracle(&adapter, &r, &opts);
            let mut kept = ResumeKeep::default();
            let mut shell = adapter.stream(&r, &opts, &mut kept).unwrap();
            assert!(kept.messages.iter().flat_map(|m| &m.content).any(|b| matches!(
                b,
                Block::Text { text: Text::Source(_) }
                    | Block::Thinking {
                        text: Text::Source(_),
                        ..
                    }
            )));
            shell.messages = kept.messages.clone();
            shell.materialize().unwrap();
            assert_eq!(
                serde_json::to_value(shell).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
            let deferred = kept
                .messages
                .iter()
                .find(|m| m.content.iter().any(|b| matches!(b, Block::Thinking { .. })))
                .unwrap()
                .clone();
            fs::write(&path, format!("{body}\n")).unwrap();
            assert!(serde_json::to_value(&deferred).is_err());
            fs::write(&path, &body).unwrap();
        }
    }
}
