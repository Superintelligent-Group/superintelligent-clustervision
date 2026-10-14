//! Preserve derived envelope schema while injecting an operation-owned compact JSON array.
use serde::Serialize;
use serde_json::ser::{CharEscape, CompactFormatter, Formatter};
use std::io::{self, Read, Write};

/// Serialize `envelope`, replacing its empty root `field` array with staged comma-separated
/// complete JSON values. The frames must come from checked serializers; stage before publishing.
pub fn write_json_array(
    out: &mut impl Write,
    envelope: &impl Serialize,
    field: &str,
    frames: &crate::bytes::ByteSource,
) -> anyhow::Result<()> {
    anyhow::ensure!(field.len() <= 128, "JSON frame field exceeds key budget");
    let mut formatter = Array {
        field,
        frames: frames.reader()?,
        depth: 0,
        key: String::new(),
        reading_key: false,
        selected: false,
        injected: false,
    };
    envelope.serialize(&mut serde_json::Serializer::with_formatter(out, &mut formatter))?;
    anyhow::ensure!(formatter.injected, "JSON frame field is absent or not a root array");
    Ok(())
}
struct Array<'a> {
    field: &'a str,
    frames: Box<dyn Read + 'a>,
    depth: usize,
    key: String,
    reading_key: bool,
    selected: bool,
    injected: bool,
}
impl Formatter for &mut Array<'_> {
    fn begin_object<W: ?Sized + Write>(&mut self, out: &mut W) -> io::Result<()> {
        self.depth += 1;
        out.write_all(b"{")
    }
    fn end_object<W: ?Sized + Write>(&mut self, out: &mut W) -> io::Result<()> {
        self.depth -= 1;
        out.write_all(b"}")
    }
    fn begin_object_key<W: ?Sized + Write>(&mut self, out: &mut W, first: bool) -> io::Result<()> {
        self.reading_key = self.depth == 1;
        self.key.clear();
        if !first {
            out.write_all(b",")?;
        }
        Ok(())
    }
    fn end_object_key<W: ?Sized + Write>(&mut self, _: &mut W) -> io::Result<()> {
        self.selected = self.reading_key && self.key == self.field;
        self.reading_key = false;
        Ok(())
    }
    fn write_string_fragment<W: ?Sized + Write>(&mut self, out: &mut W, part: &str) -> io::Result<()> {
        if self.reading_key {
            if self.key.len().saturating_add(part.len()) <= 128 {
                self.key.push_str(part);
            } else {
                self.reading_key = false;
            }
        }
        out.write_all(part.as_bytes())
    }
    fn write_char_escape<W: ?Sized + Write>(&mut self, out: &mut W, escape: CharEscape) -> io::Result<()> {
        if self.reading_key {
            let character = match &escape {
                CharEscape::Quote => '"',
                CharEscape::ReverseSolidus => '\\',
                CharEscape::Solidus => '/',
                CharEscape::Backspace => '\u{08}',
                CharEscape::FormFeed => '\u{0c}',
                CharEscape::LineFeed => '\n',
                CharEscape::CarriageReturn => '\r',
                CharEscape::Tab => '\t',
                CharEscape::AsciiControl(byte) => char::from(*byte),
            };
            if self.key.len() + character.len_utf8() <= 128 {
                self.key.push(character);
            } else {
                self.reading_key = false;
            }
        }
        CompactFormatter.write_char_escape(out, escape)
    }
    fn begin_array<W: ?Sized + Write>(&mut self, out: &mut W) -> io::Result<()> {
        let inject = self.depth == 1 && self.selected;
        self.depth += 1;
        out.write_all(b"[")?;
        if inject {
            if self.injected {
                return Err(io::Error::other("duplicate staged JSON array"));
            }
            self.injected = true;
            io::copy(&mut self.frames, out)?;
        }
        Ok(())
    }
    fn begin_array_value<W: ?Sized + Write>(&mut self, out: &mut W, first: bool) -> io::Result<()> {
        if self.depth == 2 && self.selected && self.injected {
            return Err(io::Error::other("staged envelope array must be empty"));
        }
        if !first {
            out.write_all(b",")?;
        }
        Ok(())
    }
    fn end_array<W: ?Sized + Write>(&mut self, out: &mut W) -> io::Result<()> {
        if self.depth == 2 {
            self.selected = false;
        }
        self.depth -= 1;
        out.write_all(b"]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escaped_root_keys_match_decoded_names_without_aliasing_other_fields() {
        let frames = crate::bytes::ByteSource::read(b"1".as_slice()).unwrap();
        for key in ["mes\"sages", "mes\\sages", "mes\nsages", "mes\u{01}sages"] {
            let mut envelope = serde_json::Map::new();
            envelope.insert(key.into(), serde_json::json!([]));
            envelope.insert("messages".into(), serde_json::json!([]));
            let mut actual = Vec::new();
            write_json_array(&mut actual, &envelope, "messages", &frames).unwrap();
            let mut expected = envelope.clone();
            expected.insert("messages".into(), serde_json::json!([1]));
            assert_eq!(actual, serde_json::to_vec(&expected).unwrap());
            actual.clear();
            write_json_array(&mut actual, &envelope, key, &frames).unwrap();
            let mut expected = envelope.clone();
            expected.insert(key.into(), serde_json::json!([1]));
            assert_eq!(actual, serde_json::to_vec(&expected).unwrap());
        }
    }

    #[test]
    fn injected_root_array_preserves_schema_and_nested_arrays() {
        let envelope = serde_json::json!({"head":{"messages":[1]},"messages":[],"tail":[3]});
        let frames = crate::bytes::ByteSource::read("{\"text\":\"月🌙é\"},2".as_bytes()).unwrap();
        let mut actual = Vec::new();
        write_json_array(&mut actual, &envelope, "messages", &frames).unwrap();
        let mut expected = envelope.clone();
        expected["messages"] = serde_json::json!([{"text":"月🌙é"},2]);
        assert_eq!(actual, serde_json::to_vec(&expected).unwrap());
        assert!(write_json_array(
            &mut Vec::new(),
            &serde_json::json!({"messages":[1]}),
            "messages",
            &frames
        )
        .is_err());
        assert!(write_json_array(&mut Vec::new(), &serde_json::json!({"other":[]}), "messages", &frames).is_err());
    }
}
