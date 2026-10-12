//! Private dataset projection transport: source-backed arguments become opaque descriptors, and
//! all text spans remain spans. No unresolved placeholder can leave this module as a Message.
use crate::ir::{Block, Message};
use crate::json::{Json, SourceDescriptor, SourceRegistry};
use crate::spill::Record;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

#[derive(Serialize, Deserialize)]
pub(super) struct DeferredMessage {
    message: Message,
    sources: Vec<(usize, SourceDescriptor)>,
}
impl DeferredMessage {
    pub(super) fn new(mut message: Message, registry: &mut SourceRegistry) -> Self {
        // The query has already inspected the original message. Only training rendering uses
        // this projection; unconsumed rich sidecars/IDs/usage need not occupy the private spool.
        message.id = None;
        message.parent_id = None;
        message.timestamp = None;
        message.model = None;
        message.usage = None;
        message.extra = serde_json::Map::new();
        let mut sources = Vec::new();
        for (index, block) in message.content.iter_mut().enumerate() {
            match block {
                Block::Thinking {
                    signature, encrypted, ..
                } => {
                    *signature = None;
                    *encrypted = None;
                }
                Block::ToolUse {
                    id, input, namespace, ..
                } => {
                    *id = String::new();
                    *namespace = None;
                    if let Some(source) = registry.detach(input) {
                        sources.push((index, source));
                    }
                }
                Block::ToolResult {
                    tool_use_id,
                    tool_name,
                    status,
                    details,
                    ..
                } => {
                    *tool_use_id = String::new();
                    *tool_name = None;
                    *status = None;
                    *details = None;
                }
                Block::Image { data_ref, .. } => *data_ref = None,
                Block::File { source, .. } => *source = None,
                Block::Text { .. } => {}
            }
        }
        Self { message, sources }
    }

    pub(super) fn restore(mut self, registry: &SourceRegistry) -> Result<Message> {
        for (index, descriptor) in self.sources {
            let block = self
                .message
                .content
                .get_mut(index)
                .context("invalid staged source block index")?;
            let Block::ToolUse { input, .. } = block else {
                anyhow::bail!("invalid staged source block type")
            };
            anyhow::ensure!(
                matches!(input, Json::Owned(serde_json::Value::Null)),
                "invalid staged source placeholder"
            );
            *input = registry.restore(descriptor)?;
        }
        Ok(self.message)
    }
}

fn text_heap(text: &crate::lazy::Text) -> usize {
    match text {
        crate::lazy::Text::Inline(text) => text.capacity(),
        crate::lazy::Text::Span(span) => span.source.as_ref().map_or(0, std::path::PathBuf::capacity),
    }
}
fn value_heap(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(text) => text.capacity(),
        serde_json::Value::Array(values) => {
            values.capacity() * std::mem::size_of::<serde_json::Value>() + values.iter().map(value_heap).sum::<usize>()
        }
        // Conservative node/table accounting covers both BTreeMap and preserve_order layouts.
        serde_json::Value::Object(values) => values
            .iter()
            .map(|(key, value)| 1024 + key.capacity() + value_heap(value))
            .sum(),
        _ => 0,
    }
}
impl Record for DeferredMessage {
    type ReadState = ();
    fn heap_bytes(&self) -> usize {
        self.sources.capacity() * std::mem::size_of::<(usize, SourceDescriptor)>()
            + self.message.content.capacity() * std::mem::size_of::<Block>()
            + self
                .message
                .content
                .iter()
                .map(|block| match block {
                    Block::Text { text } | Block::Thinking { text, .. } => text_heap(text),
                    Block::ToolUse { name, input, .. } => {
                        name.capacity()
                            + match input {
                                Json::Owned(value) => value_heap(value),
                                Json::Source(_) => unreachable!("source values are detached before staging"),
                            }
                    }
                    Block::ToolResult { content, .. } => text_heap(content),
                    Block::Image { media_type, .. } => media_type.as_ref().map_or(0, String::capacity),
                    Block::File { path, mime, .. } => {
                        path.as_ref().map_or(0, String::capacity) + mime.as_ref().map_or(0, String::capacity)
                    }
                })
                .sum::<usize>()
    }
    fn write(&self, writer: &mut impl Write) -> Result<()> {
        serde_json::to_writer(&mut *writer, self).context("writing deferred dataset message")?;
        writer.write_all(b"\n")?;
        Ok(())
    }
    fn read(reader: &mut impl BufRead, _: &mut ()) -> Result<Self> {
        // Deserialize one object directly from the bounded reader; never own an encoded line
        // alongside its decoded inline fields. Do not require EOF after this one spool record.
        let row = Self::deserialize(&mut serde_json::Deserializer::from_reader(&mut *reader))
            .context("reading deferred dataset message")?;
        let mut delimiter = [0];
        reader.read_exact(&mut delimiter)?;
        anyhow::ensure!(delimiter == [b'\n'], "invalid deferred dataset delimiter");
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::{Format, RecordWriter};
    use crate::ir::Role;
    #[test]
    fn spilled_sources_restore_exact_training_bytes_without_serializing_payloads() {
        let world = cv_testkit::World::in_process("deferred-dataset-source-spill");
        let path = world.base.join("source.json");
        let body = serde_json::json!({"rich":"escaped\n\"🌙\"".repeat(8192)}).to_string();
        std::fs::write(&path, &body).unwrap();
        let mut original = Message::new(Role::Assistant);
        original.content.push(Block::ToolUse {
            id: "call".into(),
            name: "Tool".into(),
            namespace: None,
            input: Json::source(&path, 0, body.len() as u64).unwrap(),
        });
        let mut registry = SourceRegistry::default();
        let mut ordered = crate::spill::Ordered::default();
        for _ in 0..1000 {
            ordered
                .push(DeferredMessage::new(original.clone(), &mut registry))
                .unwrap();
        }
        let temporary_bytes = ordered.temporary_bytes().unwrap();
        assert!(temporary_bytes > 0);
        assert!(
            temporary_bytes < body.len() as u64 * 10,
            "payloads were not duplicated into the spool"
        );
        let resolver = crate::lazy::Resolver::new(None);
        let mut expected = Vec::new();
        let mut record = RecordWriter::new(&mut expected, Format::Chatml, None);
        record.message(&original, &resolver).unwrap();
        record.finish().unwrap();
        let mut restored_count = 0;
        for row in ordered.rows().unwrap() {
            let restored = row.unwrap().restore(&registry).unwrap();
            restored_count += 1;
            if !matches!(restored_count, 1 | 1000) {
                continue;
            }
            let mut actual = Vec::new();
            let mut record = RecordWriter::new(&mut actual, Format::Chatml, None);
            record.message(&restored, &resolver).unwrap();
            record.finish().unwrap();
            assert_eq!(actual, expected);
        }
        assert_eq!(restored_count, 1000);
    }

    #[test]
    fn corrupted_descriptors_cannot_escape_as_unresolved_messages() {
        let world = cv_testkit::World::in_process("deferred-dataset-descriptor-corruption");
        let path = world.base.join("source.json");
        std::fs::write(&path, "null").unwrap();
        let mut message = Message::new(Role::Assistant);
        message.content.push(Block::ToolUse {
            id: "call".into(),
            name: "Tool".into(),
            namespace: None,
            input: Json::source(&path, 0, 4).unwrap(),
        });
        let mut registry = SourceRegistry::default();
        let staged = DeferredMessage::new(message, &mut registry);
        let encoded = serde_json::to_value(&staged).unwrap();
        for failure in ["index", "type", "placeholder", "source", "duplicate"] {
            let mut corrupted = encoded.clone();
            match failure {
                "index" => corrupted["sources"][0][0] = 999.into(),
                "type" => corrupted["message"]["content"][0] = serde_json::json!({"type":"text","text":"x"}),
                "placeholder" => corrupted["message"]["content"][0]["input"] = true.into(),
                "source" => corrupted["sources"][0][1]["source"] = 999.into(),
                "duplicate" => {
                    let duplicate = corrupted["sources"][0].clone();
                    corrupted["sources"].as_array_mut().unwrap().push(duplicate);
                }
                _ => unreachable!(),
            }
            let staged: DeferredMessage = serde_json::from_value(corrupted).unwrap();
            assert!(staged.restore(&registry).is_err(), "{failure}");
        }
    }
}
