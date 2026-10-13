//! Bounded text construction in a checked shared source arena.
use super::bytes::ByteSource;
use crate::{
    json::{JsonSource, JsonView},
    lazy::Text,
};
use anyhow::{Context, Result};
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
    let (offset, len) = arena.append(|writer| {
        writer.write_all(b"\"")?;
        produce(&mut Builder(writer))?;
        writer.write_all(b"\"")?;
        Ok(())
    })?;
    source.read_backing_syntax(arena.clone(), offset, len, |view| {
        view.source_text()?.context("constructed text")
    })
}

fn write_quoted_json(view: crate::json::JsonView<'_>, writer: &mut dyn std::io::Write) -> Result<()> {
    use std::io::Write;
    struct Escaped<'a>(&'a mut dyn Write);
    impl Write for Escaped<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let mut rest = bytes;
            while let Some(index) = memchr::memchr2(b'"', b'\\', rest) {
                self.0.write_all(&rest[..index])?;
                self.0.write_all(&[b'\\', rest[index]])?;
                rest = &rest[index + 1..];
            }
            self.0.write_all(rest)?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.0.flush()
        }
    }
    let mut buffered = std::io::BufWriter::with_capacity(64 * 1024, writer);
    view.write_canonical(&mut Escaped(&mut buffered))?;
    buffered.flush()?;
    Ok(())
}
