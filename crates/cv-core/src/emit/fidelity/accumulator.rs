//! Keep only facts used by the existing fidelity reducer, never payload bodies.
use super::*;
use crate::stream::{Flow, MessageSink};

pub(super) struct Accumulator {
    target: Harness,
    thinking: ThinkingMode,
    features: Features,
    // Claude's answered-call policy needs future IDs, but not argument/output bodies.
    claude: Vec<Message>,
    answered: std::collections::HashSet<String>,
    resolver: crate::lazy::Resolver,
    error: Option<anyhow::Error>,
}
impl Accumulator {
    pub(super) fn new(target: Harness, thinking: ThinkingMode, resolver: crate::lazy::Resolver) -> Self {
        Self {
            target,
            thinking,
            features: Features::default(),
            claude: Vec::new(),
            answered: Default::default(),
            resolver,
            error: None,
        }
    }
    pub(super) fn note(&mut self, message: &Message) -> Result<()> {
        let facts = project(message, &self.resolver, self.target)?;
        if self.target == Harness::Claude {
            for block in &facts.content {
                if let Block::ToolResult { tool_use_id, .. } = block {
                    self.answered.insert(tool_use_id.clone());
                }
            }
        }
        if self.target == Harness::Claude && facts.content.iter().any(|b| matches!(b, Block::ToolUse { .. })) {
            self.claude.push(facts);
        } else {
            self.features
                .note(self.target, self.thinking, &facts, false, &|_| false);
        }
        Ok(())
    }
    pub(super) fn finish(mut self, metadata: &Session) -> Features {
        if self.target == Harness::Claude {
            for message in &self.claude {
                self.features.note(
                    self.target,
                    self.thinking,
                    message,
                    metadata.model.is_some(),
                    &|b| matches!(b, Block::ToolUse { id, .. } if !self.answered.contains(id.as_str())),
                );
            }
        }
        if metadata.model.is_some() {
            // Adapters can establish the session default at EOF. It applies to every eligible turn.
            self.features.model_eff = self.features.role_counts.values().sum();
        }
        self.features
    }
}
impl MessageSink for Accumulator {
    fn message(&mut self, message: Message) -> Flow {
        match self.note(&message) {
            Ok(()) => Flow::Continue,
            Err(error) => {
                self.error = Some(error);
                Flow::Stop
            }
        }
    }
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.error.take()
    }
    fn meta(&mut self, session: &Session) {
        self.resolver = session.resolver();
    }
}

pub(in crate::emit) fn source_features(target: Harness, thinking: ThinkingMode, session: &Session) -> Result<Features> {
    let mut accumulator = Accumulator::new(target, thinking, session.resolver());
    for message in &session.messages {
        accumulator.note(message)?;
    }
    Ok(accumulator.finish(session))
}

pub(super) fn marker(text: &crate::lazy::Text, resolver: &crate::lazy::Resolver) -> Result<String> {
    if let Some(inline) = text.inline_str() {
        return Ok(match inline.trim_start().chars().next() {
            Some(ch @ ('/' | '?')) => ch.to_string(),
            Some(_) => "x".into(),
            None => String::new(),
        });
    }
    let mut first = None;
    text.try_for_each_chunk(resolver, 64 * 1024, |chunk| {
        if first.is_none() {
            first = chunk.chars().find(|ch| !ch.is_whitespace());
        }
        Ok(())
    })?;
    Ok(match first {
        Some('/' | '?') => first.unwrap().to_string(),
        Some(_) => "x".into(),
        None => String::new(),
    })
}
fn project(message: &Message, resolver: &crate::lazy::Resolver, target: Harness) -> Result<Message> {
    let mut facts = Message::new(message.role);
    facts.id = message.id.as_ref().map(|_| String::new());
    facts.kind = message.kind;
    facts.origin = message.origin;
    facts.timestamp = message.timestamp;
    facts.model = message.model.as_ref().map(|_| String::new());
    facts.usage = message.usage.clone();
    if is_carrier(message) {
        facts
            .extra
            .insert(crate::harness::claude::CARRIER_KEY.into(), Value::Null);
    }
    let mut text_started = false;
    for block in &message.content {
        facts.content.push(match block {
            Block::Text { text } => {
                let mut value = if reroles_client_commands(target) && message.kind == MessageKind::Prompt {
                    marker(text, resolver)?
                } else {
                    validate_text(text, resolver)?;
                    String::new()
                };
                // Only the first nonblank text character determines the client-command re-role.
                if text_started {
                    value.clear();
                } else {
                    text_started = !value.is_empty();
                }
                Block::Text { text: value.into() }
            }
            Block::Thinking {
                text,
                signature,
                encrypted,
                redacted,
            } => Block::Thinking {
                text: marker(text, resolver)?.into(),
                signature: signature.as_ref().map(|_| String::new()),
                encrypted: encrypted.as_ref().map(|_| String::new()),
                redacted: *redacted,
            },
            Block::ToolUse { id, input, .. } => {
                // Preserve the old source-error boundary even though feature counts ignore args.
                input.with_view(|_| Ok(()))?;
                Block::ToolUse {
                    id: if target == Harness::Claude {
                        id.clone()
                    } else {
                        String::new()
                    },
                    name: String::new(),
                    input: Value::Null.into(),
                    namespace: None,
                }
            }
            Block::ToolResult {
                tool_use_id,
                content,
                tool_name,
                is_error,
                details,
                ..
            } => {
                validate_text(content, resolver)?;
                Block::ToolResult {
                    tool_use_id: if target == Harness::Claude {
                        tool_use_id.clone()
                    } else {
                        String::new()
                    },
                    content: "".into(),
                    tool_name: tool_name
                        .as_ref()
                        .map(|name| if name.is_empty() { String::new() } else { "x".into() }),
                    is_error: *is_error,
                    status: None,
                    details: details.as_ref().map(|_| Value::Null),
                }
            }
            Block::Image { .. } => Block::Image {
                media_type: None,
                data_ref: None,
            },
            Block::File { .. } => Block::File {
                mime: None,
                path: None,
                source: None,
            },
        });
    }
    Ok(facts)
}

fn validate_text(text: &crate::lazy::Text, resolver: &crate::lazy::Resolver) -> Result<()> {
    match text {
        crate::lazy::Text::Inline(_) => Ok(()),
        crate::lazy::Text::Source(source) => source.json().with_view(|view| {
            anyhow::ensure!(view.is_string()?, "expected source text string");
            Ok(())
        }),
        crate::lazy::Text::Span(_) => text.try_for_each_chunk(resolver, 64 * 1024, |_| Ok(())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projection_preserves_future_answers_final_model_and_client_commands() -> Result<()> {
        let mut session = Session::new("projection", Harness::Claude);
        let mut call = Message::new(Role::Assistant);
        call.content.push(Block::ToolUse {
            id: "later".into(),
            name: "tool".into(),
            input: serde_json::json!({"large": "ignored"}).into(),
            namespace: None,
        });
        let mut orphan = call.clone();
        if let Block::ToolUse { id, .. } = &mut orphan.content[0] {
            *id = "orphan".into();
        }
        let mut result = Message::new(Role::Tool);
        result.content.push(Block::ToolResult {
            tool_use_id: "later".into(),
            content: "output".into(),
            tool_name: Some("tool".into()),
            is_error: true,
            status: None,
            details: Some(Value::Null),
        });
        let mut command = Message::new(Role::User);
        command.content = vec![
            Block::Text { text: " \n".into() },
            Block::Text { text: " ?help".into() },
        ];
        command.model = Some("explicit".into());
        let mut signed = Message::new(Role::Assistant);
        signed.content.push(Block::Thinking {
            text: "\u{2003}".into(),
            signature: Some(String::new()),
            encrypted: None,
            redacted: true,
        });
        let mut carrier = Message::new(Role::System);
        carrier.kind = MessageKind::Carrier;
        session.messages = vec![result, orphan, command, signed, carrier, call];
        for model in [None, Some("late-default".into())] {
            session.model = model;
            for target in [
                Harness::Claude,
                Harness::Codex,
                Harness::Grok,
                Harness::Gemini,
                Harness::Qwen,
                Harness::LmStudio,
                Harness::OpenCode,
            ] {
                for thinking in [ThinkingMode::Native, ThinkingMode::Text, ThinkingMode::Drop] {
                    assert_eq!(
                        source_features(target, thinking, &session)?,
                        super::super::features(target, thinking, &session),
                        "{target}/{thinking}"
                    );
                }
            }
        }
        Ok(())
    }
}
