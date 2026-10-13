//! Accept one source snapshot before retaining its lazy file bodies for native emission.
use super::*;
use crate::json::JsonSource;
use crate::source::{Probe, ReadSet};
use crate::stream::{Flow, MessageSink};
use std::collections::HashMap;

struct Collect {
    messages: Vec<crate::Message>,
    index: usize,
    start: usize,
    end: Option<usize>,
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
        let index = self.index;
        self.index += 1;
        if index < self.start || self.end.is_some_and(|end| index >= end) {
            return Ok(());
        }
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
    Ok(selected(adapter, reference, options, 0, None)?.0)
}

pub(super) fn selected(
    adapter: &dyn crate::harness::Adapter,
    reference: &SessionRef,
    options: &ParseOptions,
    start: usize,
    end: Option<usize>,
) -> Result<(Session, usize)> {
    let primary = adapter.resume_source(reference);
    let snapshot = Probe::default().snapshot_from(reference, &primary);
    let mut reads = snapshot
        .all_reads()
        .ok_or_else(|| anyhow::anyhow!("cannot capture source inventory"))?;
    reads.record(&primary);
    let mut sink = Collect {
        messages: Vec::new(),
        index: 0,
        start,
        end,
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
    Ok((session, sink.index))
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
    #[test]
    fn selected_preparation_keeps_only_window_and_original_guard() -> Result<()> {
        let world = cv_testkit::World::new("selected-prepare");
        let path = world.base.join("session.jsonl");
        let mut bytes = Vec::new();
        for index in 0..12 {
            let body = if index == 5 {
                "selected λ".repeat(1024)
            } else {
                "discarded".repeat(128 * 1024)
            };
            serde_json::to_writer(
                &mut bytes,
                &serde_json::json!({
                    "type":"user", "uuid":format!("u{index}"), "cwd":"/tmp/selected", "timestamp":"2025-01-01T00:00:00Z",
                    "message":{"role":"user","content":[{"type":"text","text":body}]}
                }),
            )?;
            bytes.push(b'\n');
        }
        fs::write(&path, bytes)?;
        let reference = SessionRef::new("session", Harness::Claude, &path);
        let adapter = crate::harness::claude::Claude::detect();
        let mut old = crate::stream::collect_with(&adapter, &reference, &ParseOptions::full())?;
        old.materialize()?;
        for (start, end) in [(5, Some(6)), (0, Some(0)), (8, Some(2)), (30, None), (11, None)] {
            let (mut selected, total) = super::super::prepare_selected(&adapter, &reference, start, end)?;
            assert_eq!(total, 12);
            let start = start.min(total);
            let end = end.unwrap_or(total).min(total).max(start);
            assert_eq!(selected.messages.len(), end - start);
            assert_eq!(selected.cwd, old.cwd);
            selected.materialize()?;
            assert_eq!(
                serde_json::to_value(&selected.messages)?,
                serde_json::to_value(&old.messages[start..end])?
            );
        }
        let (selected, _) = super::super::prepare_selected(&adapter, &reference, 5, Some(6))?;
        assert!(matches!(
            &selected.messages[0].content[0],
            Block::Text { text: Text::Source(_) }
        ));
        let mut builder = crate::loom::Composition::default();
        builder.push(selected.clone(), 12, 5, Some(6));
        let actual = builder.finish(Some("00000000-0000-0000-0000-000000000002".into()), Harness::Claude);
        let expected = crate::loom::splice(
            &[crate::loom::Span {
                source: &old,
                start: 5,
                end: Some(6),
            }],
            Some(actual.id.clone()),
            Harness::Claude,
        );
        let mut owned_actual = actual.clone();
        owned_actual.materialize()?;
        assert_eq!(serde_json::to_value(&owned_actual)?, serde_json::to_value(&expected)?);
        assert_eq!(
            crate::render::to_markdown(&owned_actual)?,
            crate::render::to_markdown(&expected)?
        );
        for target in [Harness::Claude, Harness::Codex] {
            let options = EmitOptions {
                new_id: Some(actual.id.clone()),
                ..Default::default()
            };
            let old_result = emit(&expected, target, &world.base.join(format!("old-{target}")), &options)?;
            let new_result = emit(&actual, target, &world.base.join(format!("new-{target}")), &options)?;
            assert!(
                fs::read(old_result.path)? == fs::read(new_result.path)?,
                "native {target} output"
            );
        }
        // Retained descriptors continue to reject a changed source after preparation returns.
        fs::write(&path, b"changed")?;
        let mut selected = selected;
        assert!(selected.materialize().is_err());
        Ok(())
    }

    struct Sidecar {
        path: PathBuf,
        mutate: bool,
        range: Option<(u64, u64)>,
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
            let (offset, len) = self.range.unwrap_or((1, len - 1));
            let mut message = Message::new(Role::User);
            message.content.push(Block::Text {
                text: Text::Span(crate::lazy::Span {
                    source: Some(self.path.clone()),
                    offset,
                    len,
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
            range: None,
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
                mutate: true,
                range: None,
            },
            &reference,
            Harness::Codex
        )
        .is_err());
        Ok(())
    }
    #[test]
    fn selected_opensession_defaults_to_underlying_harness_and_provenance() -> Result<()> {
        let world = cv_testkit::World::new("selected-prepare-opensession");
        let path = world.base.join("source.opensession.json");
        let mut original = Session::new("wrapped-codex", Harness::Codex);
        original.cwd = Some("/tmp/wrapped".into());
        for text in ["head", "selected", "tail"] {
            let mut message = Message::new(Role::User);
            message.content.push(Block::Text { text: text.into() });
            original.messages.push(message);
        }
        fs::write(
            &path,
            serde_json::to_vec(&crate::harness::opensession::document(&original))?,
        )?;
        let reference = SessionRef::new("wrapped-codex", Harness::OpenSession, &path);
        let adapter = crate::harness::opensession::OpenSession;
        let mut legacy = crate::stream::collect_with(&adapter, &reference, &ParseOptions::full())?;
        legacy.materialize()?;
        let (selected, total) = super::super::prepare_selected(&adapter, &reference, 1, Some(2))?;
        assert_eq!(selected.harness, Harness::Codex);
        let mut builder = crate::loom::Composition::default();
        builder.push(selected, total, 1, Some(2));
        let default_target = builder.first_harness().unwrap();
        assert_eq!(default_target, legacy.harness);
        let mut actual = builder.finish(Some("fixed".into()), default_target);
        actual.materialize()?;
        let expected = crate::loom::splice(
            &[crate::loom::Span {
                source: &legacy,
                start: 1,
                end: Some(2),
            }],
            Some("fixed".into()),
            legacy.harness,
        );
        assert_eq!(serde_json::to_value(actual)?, serde_json::to_value(expected)?);
        Ok(())
    }

    #[test]
    fn selected_rich_messages_preserve_full_fidelity_and_eof_defaults() -> Result<()> {
        let world = cv_testkit::World::new("selected-prepare-rich");
        let path = world.base.join("rich.jsonl");
        let rows = [
            serde_json::json!({"type":"user","uuid":"u","message":{"role":"user","content":"head"}}),
            serde_json::json!({"type":"assistant","uuid":"a","parentUuid":"u","timestamp":"2025-01-01T00:00:00Z","message":{"role":"assistant","model":"model","usage":{"input_tokens":20,"output_tokens":10},"content":[{"type":"thinking","thinking":"reason","signature":"signature"},{"type":"tool_use","id":"tool","name":"Read","input":{"path":"/tmp/input","large":"x".repeat(8192)}}]}}),
            serde_json::json!({"type":"user","uuid":"r","parentUuid":"a","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool","content":[{"type":"text","text":"result λ".repeat(1024)}],"is_error":false}]},"toolUseResult":{"rich":"metadata"}}),
            serde_json::json!({"type":"user","uuid":"tail","cwd":"/tmp/eof-default","message":{"role":"user","content":"tail"}}),
        ];
        let mut bytes = Vec::new();
        for row in rows {
            serde_json::to_writer(&mut bytes, &row)?;
            bytes.push(b'\n');
        }
        fs::write(&path, bytes)?;
        let reference = SessionRef::new("rich", Harness::Claude, &path);
        let adapter = crate::harness::claude::Claude::detect();
        let mut old = crate::stream::collect_with(&adapter, &reference, &ParseOptions::full())?;
        old.materialize()?;
        let mut builder = crate::loom::Composition::default();
        for (start, end) in [(1, Some(3)), (2, Some(3))] {
            let (source, total) = super::super::prepare_selected(&adapter, &reference, start, end)?;
            assert_eq!(total, old.messages.len());
            assert_eq!(source.cwd, old.cwd);
            builder.push(source, total, start, end);
        }
        let mut actual = builder.finish(Some("fixed".into()), Harness::Claude);
        actual.materialize()?;
        let expected = crate::loom::splice(
            &[
                crate::loom::Span {
                    source: &old,
                    start: 1,
                    end: Some(3),
                },
                crate::loom::Span {
                    source: &old,
                    start: 2,
                    end: Some(3),
                },
            ],
            Some("fixed".into()),
            Harness::Claude,
        );
        assert_eq!(serde_json::to_value(actual)?, serde_json::to_value(expected)?);
        Ok(())
    }

    #[test]
    fn selected_preparation_checks_discarded_dependencies_and_failure() -> Result<()> {
        let world = cv_testkit::World::new("selected-prepare-dependency");
        let primary = world.base.join("primary");
        let sidecar = world.base.join("sidecar");
        fs::write(&primary, b"{}")?;
        fs::write(&sidecar, b"!discarded")?;
        let reference = SessionRef::new("session", Harness::Claude, &primary);
        let adapter = Sidecar {
            path: sidecar,
            mutate: true,
            range: None,
        };
        // Even an empty selection must reject metadata/dependency mutation during the EOF pass.
        assert!(super::super::prepare_selected(&adapter, &reference, 5, None).is_err());
        Ok(())
    }

    #[test]
    fn literal_quoted_text_does_not_become_the_enclosing_json_value() -> Result<()> {
        let world = cv_testkit::World::new("port-prepare-literal-quotes");
        let primary = world.base.join("session.jsonl");
        let sidecar = world.base.join("body.txt");
        fs::write(&primary, b"{}")?;
        fs::write(&sidecar, br#"[ "x" ]"#)?;
        let reference = SessionRef::new("session", Harness::Claude, &primary);
        let adapter = Sidecar {
            path: sidecar,
            mutate: false,
            range: Some((2, 3)),
        };
        let session = super::super::parse_for_emit(&adapter, &reference, Harness::Codex)?;
        let owned = super::super::materialized_session(&session)?;
        assert_eq!(owned.messages[0].text().as_deref(), Some("\"x\""));
        Ok(())
    }
}
