//! JSON output which preserves the derived IR schema without materializing lazy payloads.
//!
//! The serde derives visit message and block fields. This visitor replaces only their lazy
//! leaves; ordinary fields still use serde_json. Output may be partial on any error, so callers
//! publishing files must stage them first.
use crate::{
    ir::{Block, Message},
    json::Json,
    lazy::{Resolver, Text},
};
use serde::{
    Serialize, Serializer,
    ser::{Impossible, SerializeStruct},
};
use std::io::{self, Write};

type Result<T> = std::result::Result<T, serde_json::Error>;

impl Message {
    /// Write complete IR JSON with bounded scratch space for source-backed text and arguments.
    /// Structural object keys and already-owned extras retain their existing representation.
    pub fn write_json(&self, writer: &mut impl Write, resolver: &Resolver) -> anyhow::Result<()> {
        self.write_json_format(writer, resolver, false)
    }

    /// As [`Self::write_json`], with byte-identical serde_json default pretty formatting.
    pub fn write_json_pretty(&self, writer: &mut impl Write, resolver: &Resolver) -> anyhow::Result<()> {
        self.write_json_format(writer, resolver, true)
    }

    fn write_json_format(&self, writer: &mut impl Write, resolver: &Resolver, pretty: bool) -> anyhow::Result<()> {
        self.serialize(Fields {
            writer,
            resolver,
            pretty,
            depth: 0,
            first: true,
            context: Context::Message(self),
        })?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Context<'a> {
    Message(&'a Message),
    Block(&'a Block),
}
struct Fields<'a, W> {
    writer: &'a mut W,
    resolver: &'a Resolver,
    pretty: bool,
    depth: usize,
    first: bool,
    context: Context<'a>,
}
fn error(error: impl std::fmt::Display) -> serde_json::Error {
    serde::ser::Error::custom(error)
}
fn indent(writer: &mut impl Write, depth: usize) -> io::Result<()> {
    for _ in 0..depth {
        writer.write_all(b"  ")?;
    }
    Ok(())
}
impl<W: Write> Fields<'_, W> {
    fn value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<()> {
        if self.pretty {
            serde_json::to_writer_pretty(
                BaseIndent {
                    writer: &mut *self.writer,
                    depth: self.depth + 1,
                },
                value,
            )
        } else {
            serde_json::to_writer(&mut *self.writer, value)
        }
    }
    fn text(&mut self, text: &Text) -> Result<()> {
        if let Text::Inline(text) = text {
            // A string's compact and pretty representations are identical.
            return serde_json::to_writer(&mut *self.writer, text);
        }
        let Text::Span(span) = text else { unreachable!() };
        self.writer.write_all(b"\"").map_err(error)?;
        let mut escaped = Vec::new();
        self.resolver
            .try_for_each_chunk(span, 64 * 1024, |chunk| {
                escaped.clear();
                serde_json::to_writer(&mut escaped, chunk)?;
                self.writer.write_all(&escaped[1..escaped.len() - 1])?;
                Ok(())
            })
            .map_err(error)?;
        self.writer.write_all(b"\"").map_err(error)
    }
    fn json(&mut self, value: &Json) -> Result<()> {
        if self.pretty {
            value
                .write_canonical(&mut PrettyJson {
                    writer: &mut *self.writer,
                    base: self.depth + 1,
                    stack: Vec::new(),
                    string: false,
                    escape: false,
                })
                .map_err(error)
        } else {
            value.write_canonical(self.writer).map_err(error)
        }
    }
    fn content(&mut self, blocks: &[Block]) -> Result<()> {
        self.writer.write_all(b"[").map_err(error)?;
        for (i, block) in blocks.iter().enumerate() {
            if i != 0 {
                self.writer.write_all(b",").map_err(error)?;
            }
            if self.pretty {
                self.writer.write_all(b"\n").map_err(error)?;
                indent(self.writer, self.depth + 2).map_err(error)?;
            }
            block.serialize(Fields {
                writer: &mut *self.writer,
                resolver: self.resolver,
                pretty: self.pretty,
                depth: self.depth + 2,
                first: true,
                context: Context::Block(block),
            })?;
        }
        if self.pretty && !blocks.is_empty() {
            self.writer.write_all(b"\n").map_err(error)?;
            indent(self.writer, self.depth + 1).map_err(error)?;
        }
        self.writer.write_all(b"]").map_err(error)
    }
}
impl<W: Write> SerializeStruct for Fields<'_, W> {
    type Ok = ();
    type Error = serde_json::Error;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, key: &'static str, value: &T) -> Result<()> {
        if !self.first {
            self.writer.write_all(b",").map_err(error)?;
        }
        self.first = false;
        if self.pretty {
            self.writer.write_all(b"\n").map_err(error)?;
            indent(self.writer, self.depth + 1).map_err(error)?;
        }
        serde_json::to_writer(&mut *self.writer, key)?;
        self.writer
            .write_all(if self.pretty { b": " } else { b":" })
            .map_err(error)?;
        match (self.context, key) {
            (Context::Message(message), "content") => self.content(&message.content),
            (Context::Block(block), _) => match block {
                Block::Text { text } | Block::Thinking { text, .. } => {
                    if key == "text" {
                        self.text(text)
                    } else {
                        self.value(value)
                    }
                }
                Block::ToolResult { content, .. } => {
                    if key == "content" {
                        self.text(content)
                    } else {
                        self.value(value)
                    }
                }
                Block::ToolUse { input, .. } => {
                    if key == "input" {
                        self.json(input)
                    } else {
                        self.value(value)
                    }
                }
                Block::Image { .. } | Block::File { .. } => self.value(value),
            },
            _ => self.value(value),
        }
    }
    fn end(self) -> Result<()> {
        if self.pretty && !self.first {
            self.writer.write_all(b"\n").map_err(error)?;
            indent(self.writer, self.depth).map_err(error)?;
        }
        self.writer.write_all(b"}").map_err(error)
    }
}
macro_rules! unsupported {
    ($($name:ident($($arg:ident: $ty:ty),*);)*) => { $(
        fn $name(self, $($arg: $ty),*) -> Result<Self::Ok> {
            $(let _ = $arg;)* Err(error("IR JSON field visitor expected a struct"))
        }
    )* };
}
impl<W: Write> Serializer for Fields<'_, W> {
    type Ok = ();
    type Error = serde_json::Error;
    type SerializeSeq = Impossible<(), Self::Error>;
    type SerializeTuple = Impossible<(), Self::Error>;
    type SerializeTupleStruct = Impossible<(), Self::Error>;
    type SerializeTupleVariant = Impossible<(), Self::Error>;
    type SerializeMap = Impossible<(), Self::Error>;
    type SerializeStruct = Self;
    type SerializeStructVariant = Impossible<(), Self::Error>;
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self> {
        self.writer.write_all(b"{").map_err(error)?;
        Ok(self)
    }
    unsupported! {
        serialize_bool(v: bool); serialize_i8(v: i8); serialize_i16(v: i16);
        serialize_i32(v: i32); serialize_i64(v: i64); serialize_u8(v: u8);
        serialize_u16(v: u16); serialize_u32(v: u32); serialize_u64(v: u64);
        serialize_f32(v: f32); serialize_f64(v: f64); serialize_char(v: char);
        serialize_str(v: &str); serialize_bytes(v: &[u8]); serialize_none();
        serialize_unit(); serialize_unit_struct(name: &'static str);
        serialize_unit_variant(name: &'static str, index: u32, variant: &'static str);
    }
    fn serialize_some<T: ?Sized + Serialize>(self, _: &T) -> Result<()> {
        Err(error("expected struct"))
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(self, _: &'static str, _: &T) -> Result<()> {
        Err(error("expected struct"))
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<()> {
        Err(error("expected struct"))
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq> {
        Err(error("expected struct"))
    }
    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple> {
        Err(error("expected struct"))
    }
    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<Self::SerializeTupleStruct> {
        Err(error("expected struct"))
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant> {
        Err(error("expected struct"))
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap> {
        Err(error("expected struct"))
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant> {
        Err(error("expected struct"))
    }
}

// Nested ordinary fields use serde_json's own pretty serializer. Its indentation starts at
// zero, so add the containing field's indentation after each formatter newline.
struct BaseIndent<'a, W> {
    writer: &'a mut W,
    depth: usize,
}
impl<W: Write> Write for BaseIndent<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut start = 0;
        for end in memchr::memchr_iter(b'\n', bytes) {
            self.writer.write_all(&bytes[start..=end])?;
            indent(self.writer, self.depth)?;
            start = end + 1;
        }
        self.writer.write_all(&bytes[start..])?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

// Whitespace-only transform of already canonical JSON. Delimiters inside escaped string
// chunks are untouched; the stack holds only a nonempty bit for each nesting level.
struct PrettyJson<'a, W> {
    writer: &'a mut W,
    base: usize,
    stack: Vec<bool>,
    string: bool,
    escape: bool,
}
impl<W: Write> PrettyJson<'_, W> {
    fn newline(&mut self) -> io::Result<()> {
        self.writer.write_all(b"\n")?;
        indent(self.writer, self.base + self.stack.len())
    }
    fn token(&mut self) -> io::Result<()> {
        if let Some(nonempty) = self.stack.last_mut() {
            if !*nonempty {
                *nonempty = true;
                self.newline()?;
            }
        }
        Ok(())
    }
}
impl<W: Write> Write for PrettyJson<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut start = 0;
        let mut i = 0;
        while i < bytes.len() {
            if self.string {
                if self.escape {
                    self.escape = false;
                    i += 1;
                    continue;
                }
                let Some(offset) = memchr::memchr2(b'"', b'\\', &bytes[i..]) else {
                    break;
                };
                i += offset;
                if bytes[i] == b'\\' {
                    self.escape = true;
                } else {
                    self.string = false;
                }
                i += 1;
                continue;
            }
            self.writer.write_all(&bytes[start..i])?;
            let byte = bytes[i];
            match byte {
                b'{' | b'[' => {
                    self.token()?;
                    self.writer.write_all(&[byte])?;
                    self.stack.push(false);
                }
                b'}' | b']' => {
                    let nonempty = self.stack.pop().unwrap_or(false);
                    if nonempty {
                        self.newline()?;
                    }
                    self.writer.write_all(&[byte])?;
                }
                b',' => {
                    self.writer.write_all(b",")?;
                    self.newline()?;
                }
                b':' => self.writer.write_all(b": ")?,
                b'"' => {
                    self.token()?;
                    self.string = true;
                    self.writer.write_all(b"\"")?;
                }
                _ => {
                    self.token()?;
                    start = i;
                    // Canonical numbers, booleans and null end only at a container delimiter.
                    i += memchr::memchr3(b',', b']', b'}', &bytes[i..]).unwrap_or(bytes.len() - i);
                    continue;
                }
            }
            i += 1;
            start = i;
        }
        self.writer.write_all(&bytes[start..])?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ir::Role, lazy::Span};
    use serde_json::json;
    use std::path::PathBuf;

    struct Source(PathBuf);
    impl Source {
        fn new(bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!("cv-ir-json-{}", uuid::Uuid::new_v4()));
            std::fs::write(&path, bytes).unwrap();
            Self(path)
        }
    }
    impl Drop for Source {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn oracle(message: &Message, resolver: &Resolver) {
        let mut materialized = message.clone();
        materialized.materialize(resolver).unwrap();
        let mut compact = Vec::new();
        message.write_json(&mut compact, resolver).unwrap();
        assert_eq!(compact, serde_json::to_vec(&materialized).unwrap());
        let mut pretty = Vec::new();
        message.write_json_pretty(&mut pretty, resolver).unwrap();
        assert_eq!(pretty, serde_json::to_vec_pretty(&materialized).unwrap());
    }
    #[test]
    fn ir_json_matches_derived_schema_all_block_variants_and_skips() {
        let resolver = Resolver::new(None);
        oracle(&Message::new(Role::User), &resolver);
        let blocks = json!([
            {"type":"text","text":""},
            {"type":"text","text":"hello\n \"☺\\"},
            {"type":"thinking","text":"reason"},
            {"type":"thinking","text":"reason","signature":"sig","encrypted":"enc","redacted":true},
            {"type":"tool_use","id":"id","name":"tool","input":null},
            {"type":"tool_use","id":"id","name":"tool","namespace":"mcp","input":{"a":[],"b":{},"c":[{},[],{"unicode":"☺","escaped":"\n\"\\","bool":false,"number":-0.0,"e":1e40}]}},
            {"type":"tool_result","tool_use_id":"id","content":""},
            {"type":"tool_result","tool_use_id":"id","content":"result\n","is_error":true,"tool_name":"tool","status":"error","details":{"x":[1,2],"y":{}}},
            {"type":"image"},
            {"type":"image","media_type":"image/png","data_ref":"opaque"},
            {"type":"file"},
            {"type":"file","mime":"text/plain","path":"/file","source":"reference"}
        ]);
        let mut message: Message = serde_json::from_value(json!({
            "id":"msg","parent_id":"parent","role":"assistant","timestamp":"2026-10-12T00:00:00Z","model":"model",
            "content":blocks,
            "usage":{"input_tokens":1,"output_tokens":2,"cache_read_tokens":3,"cache_creation_tokens":4,"reasoning_tokens":5},
            "extra":{"claude":{"field":[1,{"a":true}]},"_record":{"type":"record"}}
        })).unwrap();
        oracle(&message, &resolver);
        message.id = None;
        message.parent_id = None;
        message.timestamp = None;
        message.model = None;
        message.usage = None;
        message.extra.clear();
        oracle(&message, &resolver);
    }
    #[test]
    fn ir_json_source_arguments_and_text_match_owned_bytes() {
        let raw = r#"{"z":0,"a":[{},[],{"x":"\u0061\n\"\\☺","zero":-0,"float":1e2}],"z":2}"#.as_bytes();
        let source = Source::new(raw);
        let body = "☺a\\n\\t\\uD83D\\uDE00\\\"\\\\".repeat(17000);
        let text_source = Source::new(body.as_bytes());
        let literal = Source::new("☺\n\"\\plain".repeat(17000).as_bytes());
        let mut message = Message::new(Role::Assistant);
        message.content = vec![
            Block::ToolUse {
                id: "id".into(),
                name: "tool".into(),
                namespace: None,
                input: Json::source(&source.0, 0, raw.len() as u64).unwrap(),
            },
            Block::Text {
                text: Text::Span(Span {
                    source: Some(text_source.0.clone()),
                    offset: 0,
                    len: body.len() as u64,
                    escaped: true,
                }),
            },
            Block::ToolResult {
                tool_use_id: "id".into(),
                content: Text::Span(Span {
                    source: Some(literal.0.clone()),
                    offset: 0,
                    len: std::fs::metadata(&literal.0).unwrap().len(),
                    escaped: false,
                }),
                is_error: false,
                tool_name: None,
                status: None,
                details: None,
            },
        ];
        oracle(&message, &Resolver::new(None));
    }
    #[test]
    fn ir_json_text_preserves_lossy_utf8_and_malformed_escape_fallback() {
        for (raw, escaped) in [
            (b"a\xffb".as_slice(), false),
            (b"a\\uD800tail".as_slice(), true),
            (b"a\\qtail".as_slice(), true),
        ] {
            let source = Source::new(raw);
            let mut message = Message::new(Role::User);
            message.content = vec![Block::Text {
                text: Text::Span(Span {
                    source: Some(source.0.clone()),
                    offset: 0,
                    len: raw.len() as u64,
                    escaped,
                }),
            }];
            oracle(&message, &Resolver::new(None));
        }
    }
    #[test]
    fn ir_json_pretty_transform_is_independent_of_write_boundaries() {
        let value = json!({"a":[{},[],0,true,null,-1.25,"\"\\\n☺",{"b":[]}],"c":{}});
        let canonical = serde_json::to_vec(&value).unwrap();
        for chunk in 1..=canonical.len() {
            let mut output = Vec::new();
            let mut writer = PrettyJson {
                writer: &mut output,
                base: 0,
                stack: Vec::new(),
                string: false,
                escape: false,
            };
            for bytes in canonical.chunks(chunk) {
                writer.write_all(bytes).unwrap();
            }
            assert_eq!(output, serde_json::to_vec_pretty(&value).unwrap(), "chunk={chunk}");
        }
    }
    #[test]
    fn ir_json_propagates_errors_during_streamed_text_and_arguments() {
        struct BudgetWriter(usize);
        impl Write for BudgetWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if bytes.len() > self.0 {
                    return Err(io::Error::other("payload output failure"));
                }
                self.0 -= bytes.len();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let text = Source::new(&vec![b'x'; 10000]);
        let args = Source::new(format!("{{\"arg\":\"{}\"}}", "x".repeat(10000)).as_bytes());
        let resolver = Resolver::new(None);
        let mut message = Message::new(Role::Assistant);
        for block in [
            Block::Text {
                text: Text::Span(Span {
                    source: Some(text.0.clone()),
                    offset: 0,
                    len: 10000,
                    escaped: false,
                }),
            },
            Block::ToolUse {
                id: "id".into(),
                name: "tool".into(),
                namespace: None,
                input: Json::source(&args.0, 0, std::fs::metadata(&args.0).unwrap().len()).unwrap(),
            },
        ] {
            message.content = vec![block];
            let compact = message.write_json(&mut BudgetWriter(300), &resolver).unwrap_err();
            assert!(compact.to_string().contains("payload output failure"), "{compact}");
            let pretty = message
                .write_json_pretty(&mut BudgetWriter(300), &resolver)
                .unwrap_err();
            assert!(pretty.to_string().contains("payload output failure"), "{pretty}");
        }
    }
    #[test]
    fn ir_json_propagates_missing_text_changed_arguments_and_output_errors() {
        let resolver = Resolver::new(None);
        let source = Source::new(b"text");
        let mut message = Message::new(Role::User);
        message.content.push(Block::Text {
            text: Text::Span(Span {
                source: Some(source.0.clone()),
                offset: 0,
                len: 100,
                escaped: false,
            }),
        });
        assert!(message.write_json(&mut Vec::new(), &resolver).is_err());
        let json_source = Source::new(b"{\"a\":1}");
        message.content = vec![Block::ToolUse {
            id: "id".into(),
            name: "tool".into(),
            namespace: None,
            input: Json::source(&json_source.0, 0, 7).unwrap(),
        }];
        std::fs::write(&json_source.0, b"{\"b\":2}").unwrap();
        assert!(message.write_json_pretty(&mut Vec::new(), &resolver).is_err());
        struct Failing;
        impl Write for Failing {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("output failure"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(Message::new(Role::User).write_json(&mut Failing, &resolver).is_err());
    }
}
