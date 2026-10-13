//! Bounded text construction in a checked shared source arena.
use super::bytes::ByteSource;
use crate::{
    json::{JsonSource, JsonView},
    lazy::Text,
};
use anyhow::Result;
use std::{io::Write, sync::Arc};

pub(crate) struct Builder<'a>(&'a mut dyn Write);
impl Builder<'_> {
    /// Append decoded text, bounding re-encoding scratch even for giant borrowed chunks.
    pub(crate) fn push(&mut self, mut text: &str) -> Result<()> {
        while !text.is_empty() {
            let mut end = text.len().min(64 * 1024);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let encoded = serde_json::to_string(&text[..end])?;
            self.0.write_all(&encoded.as_bytes()[1..encoded.len() - 1])?;
            text = &text[end..];
        }
        Ok(())
    }
    /// Append a string view without decoding and re-encoding its already valid interior.
    pub(crate) fn push_string(&mut self, view: JsonView<'_>) -> Result<bool> {
        if !view.is_string()? {
            return Ok(false);
        }
        struct Interior<'a> {
            writer: &'a mut dyn Write,
            first: bool,
            last: Option<u8>,
        }
        impl Write for Interior<'_> {
            fn write(&mut self, mut bytes: &[u8]) -> std::io::Result<usize> {
                let length = bytes.len();
                if bytes.is_empty() {
                    return Ok(0);
                }
                if self.first {
                    if bytes[0] != b'"' {
                        return Err(std::io::Error::other("missing string quote"));
                    }
                    self.first = false;
                    bytes = &bytes[1..];
                }
                if !bytes.is_empty() {
                    if let Some(last) = self.last.take() {
                        self.writer.write_all(&[last])?;
                    }
                    self.writer.write_all(&bytes[..bytes.len() - 1])?;
                    self.last = bytes.last().copied();
                }
                Ok(length)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.writer.flush()
            }
        }
        let mut interior = Interior {
            writer: self.0,
            first: true,
            last: None,
        };
        view.write_canonical(&mut interior)?;
        anyhow::ensure!(interior.last == Some(b'"'), "missing closing string quote");
        Ok(true)
    }
    /// Append a value's canonical JSON document as decoded text.
    pub(crate) fn push_json(&mut self, view: JsonView<'_>) -> Result<()> {
        write_quoted_json(view, self.0)
    }
}

pub(crate) fn append(
    source: &JsonSource,
    arena: &Arc<ByteSource>,
    produce: impl FnOnce(&mut Builder<'_>) -> Result<()>,
) -> Result<Text> {
    let (offset, len) = arena.append(|writer| generate_string(writer, produce))?;
    source.generated_string_text(arena.clone(), offset, len)
}

// Only a successful complete quoted string may reach generated_string_text; writer and producer
// failures leave unpublished append bytes. Builder fields prevent callers from emitting raw bytes.
fn generate_string(writer: &mut dyn Write, produce: impl FnOnce(&mut Builder<'_>) -> Result<()>) -> Result<()> {
    writer.write_all(b"\"")?;
    produce(&mut Builder(writer))?;
    writer.write_all(b"\"")?;
    Ok(())
}

fn write_quoted_json(view: crate::json::JsonView<'_>, writer: &mut dyn std::io::Write) -> Result<()> {
    use std::io::Write;
    struct Escaped<'a> {
        writer: &'a mut dyn Write,
        scratch: Vec<u8>,
    }
    impl Write for Escaped<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            for chunk in bytes.chunks(64 * 1024) {
                self.scratch.clear();
                self.scratch.reserve(chunk.len().saturating_mul(2));
                for &byte in chunk {
                    if matches!(byte, b'"' | b'\\') {
                        self.scratch.push(b'\\');
                    }
                    self.scratch.push(byte);
                }
                self.writer.write_all(&self.scratch)?;
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.writer.flush()
        }
    }
    let mut buffered = std::io::BufWriter::with_capacity(64 * 1024, writer);
    view.write_canonical(&mut Escaped {
        writer: &mut buffered,
        scratch: Vec::new(),
    })?;
    buffered.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lazy::Resolver;

    #[test]
    fn generated_string_matches_full_codec_and_keeps_original_provenance() {
        let world = cv_testkit::World::in_process("generated-string-source");
        let path = world.base.join("original");
        std::fs::write(&path, "snapshot provenance").unwrap();
        let source = JsonSource::capture(&path).unwrap();
        let arena = Arc::new(ByteSource::arena().unwrap());
        let body = "🌙\n\"\\\t".repeat(8192);
        let input = serde_json::json!({"body":body, "scalar":42});
        let wire = serde_json::to_vec(&input).unwrap();
        let (offset, len) = arena
            .append(|out| {
                out.write_all(&wire)?;
                Ok(())
            })
            .unwrap();
        let text = source
            .read_backing_value(arena.clone(), offset, len, |view| {
                append(&source, &arena, |builder| {
                    builder.push("prefix\n")?;
                    assert!(builder.push_string(view.get("body")?.unwrap())?);
                    assert!(!builder.push_string(view.get("scalar")?.unwrap())?);
                    builder.push("\n")?;
                    builder.push_json(view)
                })
            })
            .unwrap();
        assert!(matches!(text, Text::Source(_)));
        let expected = format!("prefix\n{body}\n{}", serde_json::to_string(&input).unwrap());
        assert_eq!(text.try_resolve(&Resolver::new(None)).unwrap(), expected);
        let small = append(&source, &arena, |builder| builder.push("small🌙")).unwrap();
        assert_eq!(small.inline_str(), Some("small🌙"));
        std::fs::write(&path, "changed snapshot").unwrap();
        assert!(text.try_resolve_prefix(&Resolver::new(None), 8).is_err());
        assert!(append(&source, &arena, |builder| builder.push("fresh")).is_err());
    }

    #[test]
    fn malformed_syntax_only_strings_and_failed_writers_never_publish_a_carrier() {
        let world = cv_testkit::World::in_process("generated-string-errors");
        let path = world.base.join("original");
        std::fs::write(&path, "snapshot provenance").unwrap();
        let source = JsonSource::capture(&path).unwrap();
        let arena = Arc::new(ByteSource::arena().unwrap());
        let accepted = append(&source, &arena, |builder| builder.push(&"keep🌙".repeat(8192))).unwrap();
        for suffix in ["\\uD800", "\\uDC00"] {
            let wire = format!("\"{}{}\"", "prefix".repeat(8192), suffix);
            let (offset, len) = arena
                .append(|out| {
                    out.write_all(wire.as_bytes())?;
                    Ok(())
                })
                .unwrap();
            for string_only in [true, false] {
                let mut visited = false;
                let result = source.read_backing_syntax(arena.clone(), offset, len, |view| {
                    visited = true;
                    append(&source, &arena, |builder| {
                        if string_only {
                            builder.push_string(view)?;
                            Ok(())
                        } else {
                            builder.push_json(view)
                        }
                    })
                });
                assert!(
                    visited,
                    "syntax-only view must exercise canonical strict string checking"
                );
                assert!(result.is_err(), "{suffix} string_only={string_only}");
            }
        }
        assert_eq!(
            accepted.try_resolve(&Resolver::new(None)).unwrap(),
            "keep🌙".repeat(8192)
        );
        assert!(append(&source, &arena, |builder| {
            builder.push("unpublished")?;
            anyhow::bail!("producer failure")
        })
        .is_err());
        struct Fails {
            remaining: usize,
        }
        impl Write for Fails {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.remaining == 0 {
                    return Err(std::io::Error::other("writer failure"));
                }
                let len = bytes.len().min(self.remaining);
                self.remaining -= len;
                Ok(len)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let error = generate_string(&mut Fails { remaining: 8 }, |builder| builder.push("unpublished")).unwrap_err();
        assert!(error.to_string().contains("writer failure"));
        assert_eq!(
            accepted.try_decoded_len(&Resolver::new(None)).unwrap(),
            "keep🌙".len() * 8192
        );
    }
}
