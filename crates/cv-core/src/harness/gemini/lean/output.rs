//! Selected result text, shared by mapped and positioned recording projections.
use crate::{
    harness::store::{bytes::ByteSource, document::Fields, text},
    json::{JsonSource, JsonView},
    lazy::Text,
};
use anyhow::Result;
use std::sync::Arc;

#[derive(Clone, Copy)]
pub(in crate::harness::gemini) struct Output<'a> {
    pub source: &'a JsonSource,
    pub arena: &'a Arc<ByteSource>,
}

#[derive(Clone, Copy)]
enum Chunk<'a> {
    Empty,
    String(JsonView<'a>),
    Json(JsonView<'a>),
}
impl Chunk<'_> {
    fn present(self) -> Result<bool> {
        Ok(match self {
            Self::Empty => false,
            Self::Json(_) => true,
            Self::String(value) => match value.source_range()? {
                Some((_, len)) => len > 2,
                None => value.resolve()?.as_str().is_some_and(|s| !s.is_empty()),
            },
        })
    }
    fn push(self, writer: &mut text::Builder<'_>) -> Result<()> {
        match self {
            Self::Empty => Ok(()),
            Self::String(value) => writer.push_string(value).map(|_| ()),
            Self::Json(value) => writer.push_json(value),
        }
    }
}
fn value_chunk(view: JsonView<'_>) -> Result<Chunk<'_>> {
    if view.is_string()? {
        return Ok(Chunk::String(view));
    }
    if view.is_null()? {
        return Ok(Chunk::Empty);
    }
    if let Some(fields) = Fields::parse(view, &["output", "error"])? {
        for key in ["output", "error"] {
            if let Some(value) = fields.get(key) {
                if value.is_string()? {
                    return Ok(Chunk::String(value));
                }
            }
        }
    }
    Ok(Chunk::Json(view))
}
fn part_chunk(view: JsonView<'_>, text_part: bool) -> Result<Chunk<'_>> {
    if let Some(fields) = Fields::parse(view, &["functionResponse", "text"])? {
        if let Some(response) = fields.get("functionResponse") {
            return response
                .get("response")?
                .map(value_chunk)
                .transpose()
                .map(|v| v.unwrap_or(Chunk::Empty));
        }
        if text_part {
            if let Some(value) = fields.get("text") {
                if value.is_string()? {
                    return Ok(Chunk::String(value));
                }
            }
        }
    }
    value_chunk(view)
}
impl Output<'_> {
    fn text(self, chunk: Chunk<'_>) -> Result<Text> {
        match chunk {
            Chunk::Empty => Ok(Text::default()),
            Chunk::String(value) => Ok(value.source_text()?.unwrap_or_default()),
            Chunk::Json(value) => text::append(self.source, self.arena, |writer| writer.push_json(value)),
        }
    }
    pub(super) fn thoughts(self, value: JsonView<'_>) -> Result<(Vec<serde_json::Value>, Vec<crate::ir::Block>)> {
        let mut headers = Vec::new();
        let mut blocks = Vec::new();
        if let Some(thoughts) = value.array()? {
            for thought in thoughts {
                let thought = thought?;
                headers.push(serde_json::Value::Null);
                let string = |key| -> Result<Chunk<'_>> {
                    Ok(match thought.get(key)? {
                        Some(value) if value.is_string()? => Chunk::String(value),
                        _ => Chunk::Empty,
                    })
                };
                let subject = string("subject")?;
                let description = string("description")?;
                let text = match (subject.present()?, description.present()?) {
                    (false, false) => continue,
                    (true, false) => self.text(subject)?,
                    (false, true) => self.text(description)?,
                    (true, true) => text::append(self.source, self.arena, |writer| {
                        writer.push("**")?;
                        subject.push(writer)?;
                        writer.push("** ")?;
                        description.push(writer)
                    })?,
                };
                blocks.push(crate::ir::Block::Thinking {
                    text,
                    signature: None,
                    encrypted: None,
                    redacted: false,
                });
            }
        }
        Ok((headers, blocks))
    }
    pub(super) fn value(self, value: JsonView<'_>) -> Result<Text> {
        self.text(value_chunk(value)?)
    }
    pub(super) fn result(self, value: JsonView<'_>) -> Result<Option<Text>> {
        if value.is_string()? {
            return self.text(Chunk::String(value)).map(Some);
        }
        if let Some(parts) = value.array()? {
            let chunks = parts.map(|part| part_chunk(part?, true)).collect::<Result<Vec<_>>>()?;
            let mut present = Vec::new();
            for chunk in chunks {
                if chunk.present()? {
                    present.push(chunk);
                }
            }
            return match present.as_slice() {
                [] => Ok(None),
                [one] => self.text(*one).map(Some),
                many => text::append(self.source, self.arena, |writer| {
                    for (i, chunk) in many.iter().enumerate() {
                        if i != 0 {
                            writer.push("\n")?;
                        }
                        chunk.push(writer)?;
                    }
                    Ok(())
                })
                .map(Some),
            };
        }
        if value.is_object()? {
            self.text(part_chunk(value, false)?).map(Some)
        } else {
            Ok(None)
        }
    }
}
