//! Accept one source snapshot before retaining its lazy file bodies for native emission.
use super::*;
use crate::json::JsonSource;
use crate::source::{Probe, ReadSet};
use crate::stream::{Flow, MessageSink};
use std::collections::HashMap;

struct Collect {
    messages: Vec<crate::Message>,
    primary: PathBuf,
    implicit: PathBuf,
    reads: ReadSet,
    sources: HashMap<PathBuf, JsonSource>,
    error: Option<anyhow::Error>,
}
impl Collect {
    fn capture(&mut self, path: &Path) -> Result<()> {
        let path = std::path::absolute(path)?;
        self.reads.record(&path);
        if !self.sources.contains_key(&path) {
            self.sources.insert(path.clone(), JsonSource::capture(&path)?);
        }
        Ok(())
    }
    fn note(&mut self, message: crate::Message) -> Result<()> {
        crate::source::record_message_reads(&message, &self.primary, &mut self.reads);
        for block in &message.content {
            if let Some(crate::Text::Span(span)) = text(block) {
                let path = span.source.clone().unwrap_or_else(|| self.implicit.clone());
                self.capture(&path)?;
            }
        }
        self.messages.push(message);
        Ok(())
    }
}
impl MessageSink for Collect {
    fn message(&mut self, message: crate::Message) -> Flow {
        if let Err(error) = self.note(message) {
            self.error = Some(error);
            Flow::Stop
        } else {
            Flow::Continue
        }
    }
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.error.take()
    }
    fn meta(&mut self, metadata: &Session) {
        if let Some(path) = &metadata.source_path {
            self.implicit = path.clone();
        }
    }
    fn dependency(&mut self, path: &Path) {
        self.reads.record(path);
        // Absence/directory membership are read-set guards, not retained body files.
        if fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
            if let Err(error) = self.capture(path) {
                self.error = Some(error);
            }
        }
    }
}
fn text(block: &crate::Block) -> Option<&crate::Text> {
    match block {
        crate::Block::Text { text } | crate::Block::Thinking { text, .. } => Some(text),
        crate::Block::ToolResult { content, .. } => Some(content),
        _ => None,
    }
}
fn text_mut(block: &mut crate::Block) -> Option<&mut crate::Text> {
    match block {
        crate::Block::Text { text } | crate::Block::Thinking { text, .. } => Some(text),
        crate::Block::ToolResult { content, .. } => Some(content),
        _ => None,
    }
}
pub(super) fn parse(
    adapter: &dyn crate::harness::Adapter,
    reference: &SessionRef,
    options: &ParseOptions,
) -> Result<Session> {
    let primary = adapter.resume_source(reference);
    let snapshot = Probe::default().snapshot_from(reference, &primary);
    let mut reads = snapshot
        .all_reads()
        .ok_or_else(|| anyhow::anyhow!("cannot capture source inventory"))?;
    reads.record(&primary);
    let mut sink = Collect {
        messages: Vec::new(),
        primary: primary.clone(),
        implicit: reference.path.clone(),
        reads,
        sources: HashMap::new(),
        error: None,
    };
    if fs::metadata(&primary).is_ok_and(|metadata| metadata.is_file()) {
        sink.capture(&primary)?;
    }
    let (mut session, counted) = crate::stream::stream_counting(adapter, reference, options, &mut sink)?;
    if let Some(counted) = &counted {
        sink.reads.merge(&counted.dependencies);
    }
    let guard = snapshot.with_reads(&sink.reads);
    anyhow::ensure!(
        sink.reads.fresh(&mut Probe::default()) && crate::source::unchanged_from(reference, &primary, &guard),
        "source changed during port preparation"
    );
    let resolver = session.resolver();
    for message in &mut sink.messages {
        for block in &mut message.content {
            let Some(value) = text_mut(block) else { continue };
            let crate::Text::Span(span) = value else { continue };
            let span = span.clone();
            let path = std::path::absolute(
                span.source
                    .as_ref()
                    .or(session.source_path.as_ref())
                    .unwrap_or(&primary),
            )?;
            let source = sink
                .sources
                .get(&path)
                .ok_or_else(|| anyhow::anyhow!("uncaptured retained span source: {}", path.display()))?;
            source.check()?;
            let mut literal_backslash = false;
            if !span.escaped {
                value.try_for_each_chunk(&resolver, 64 * 1024, |chunk| {
                    literal_backslash |= chunk.contains('\\');
                    Ok(())
                })?;
            }
            let quoted = if !literal_backslash {
                span.offset
                    .checked_sub(1)
                    .zip(span.len.checked_add(2))
                    .filter(|(offset, len)| {
                        offset
                            .checked_add(*len)
                            .is_some_and(|end| fs::metadata(&path).is_ok_and(|metadata| end <= metadata.len()))
                    })
                    .map(|(offset, len)| {
                        source.read_range_value(offset, len, |view| {
                            if view.source_range()? == Some((offset, len)) {
                                view.source_text()
                            } else {
                                Ok(None)
                            }
                        })
                    })
            } else {
                None
            };
            let bound = match quoted {
                Some(Ok(Some(text))) => text,
                Some(Err(error)) if error.downcast_ref::<crate::json::InvalidJson>().is_none() => return Err(error),
                _ => crate::harness::store::text::append(source, &source.decoded_backing()?, |builder| {
                    value.try_for_each_chunk(&resolver, 64 * 1024, |chunk| builder.push(chunk))
                })?,
            };
            source.check()?;
            *value = bound;
        }
    }

    session.messages = sink.messages;
    if let Some(counted) = counted {
        counted.record_with_source(reference, &guard);
    }
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Block, Harness, Message, Role, Text};
    #[test]
    fn actual_claude_preparation_retains_original_body_identity() -> Result<()> {
        let world = cv_testkit::World::new("port-prepare-claude");
        let path = world.base.join("session.jsonl");
        let body = "plain retained body λ".repeat(512);
        fs::write(
            &path,
            serde_json::to_vec(
                &serde_json::json!({"type":"user","uuid":"u","message":{"role":"user","content":[{"type":"text","text":body}]}}),
            )?,
        )?;
        let reference = SessionRef::new("session", Harness::Claude, &path);
        let session =
            super::super::parse_for_emit(&crate::harness::claude::Claude::detect(), &reference, Harness::Codex)?;
        assert!(matches!(
            &session.messages[0].content[0],
            Block::Text { text: Text::Source(_) }
        ));
        let output = world.base.join("output");
        let options = EmitOptions {
            new_id: Some("00000000-0000-0000-0000-000000000001".into()),
            ..Default::default()
        };
        let result = emit(&session, Harness::Codex, &output, &options)?;
        let bytes = fs::read(&result.path)?;
        let mut changed = fs::read(&path)?;
        let at = changed.windows(5).position(|bytes| bytes == b"plain").unwrap();
        changed[at] = b'P';
        fs::write(&path, changed)?;
        assert!(emit(&session, Harness::Codex, &output, &options).is_err());
        assert_eq!(fs::read(&result.path)?, bytes);
        Ok(())
    }
    struct Sidecar {
        path: PathBuf,
        mutate: bool,
    }
    impl crate::harness::Adapter for Sidecar {
        fn harness(&self) -> Harness {
            Harness::Claude
        }
        fn roots(&self) -> Vec<PathBuf> {
            Vec::new()
        }
        fn discover(&self) -> Result<Vec<SessionRef>> {
            Ok(Vec::new())
        }
        fn stream(&self, reference: &SessionRef, _: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
            let mut session = Session::new(&reference.id, Harness::Claude);
            session.source_path = Some(reference.path.clone());
            sink.meta(&session);
            sink.dependency(&self.path);
            let len = fs::metadata(&self.path)?.len();
            let mut message = Message::new(Role::User);
            message.content.push(Block::Text {
                text: Text::Span(crate::lazy::Span {
                    source: Some(self.path.clone()),
                    offset: 1,
                    len: len - 1,
                    escaped: false,
                }),
            });
            sink.try_message(message)?;
            if self.mutate {
                fs::write(&self.path, b"changed after source callback")?;
            }
            Ok(session)
        }
    }
    #[test]
    fn literal_sidecar_spans_are_bounded_snapshots_with_original_identity() -> Result<()> {
        let world = cv_testkit::World::new("port-prepare-sidecar");
        let primary = world.base.join("session.jsonl");
        let sidecar = world.base.join("body.txt");
        fs::write(&primary, b"{}")?;
        let body = "literal \\n body λ".repeat(1024);
        fs::write(&sidecar, format!("!{body}"))?;
        let reference = SessionRef::new("session", Harness::Claude, &primary);
        let adapter = Sidecar {
            path: sidecar.clone(),
            mutate: false,
        };
        let session = super::super::parse_for_emit(&adapter, &reference, Harness::Codex)?;
        assert!(matches!(
            &session.messages[0].content[0],
            Block::Text { text: Text::Source(_) }
        ));
        let materialized = super::super::materialized_session(&session)?;
        assert_eq!(materialized.messages[0].text().as_deref(), Some(body.as_str()));
        fs::write(&sidecar, b"!changed")?;
        assert!(emit(
            &session,
            Harness::Codex,
            &world.base.join("output"),
            &EmitOptions::default()
        )
        .is_err());
        fs::write(&sidecar, format!("!{body}"))?;
        assert!(super::super::parse_for_emit(
            &Sidecar {
                path: sidecar,
                mutate: true
            },
            &reference,
            Harness::Codex
        )
        .is_err());
        Ok(())
    }
}
