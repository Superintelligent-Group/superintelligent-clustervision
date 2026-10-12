//! Discovery reads only titles, cwd, timestamps and whether an assistant is synthetic. Avoid
//! constructing conversation bodies and sidecars while retaining the owned codec's acceptance.

use super::*;
use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::borrow::Cow;
use std::fmt;

#[derive(Default)]
struct Fields<'a> {
    kind: Option<&'a RawValue>,
    cwd: Option<&'a RawValue>,
    timestamp: Option<&'a RawValue>,
    custom_title: Option<&'a RawValue>,
    ai_title: Option<&'a RawValue>,
    api_error: Option<&'a RawValue>,
    message: Body<'a>,
}

#[derive(Default)]
struct Body<'a> {
    model: Option<&'a RawValue>,
}

#[derive(Deserialize)]
#[serde(field_identifier)]
enum Key {
    #[serde(rename = "type")]
    Kind,
    #[serde(rename = "cwd")]
    Cwd,
    #[serde(rename = "timestamp")]
    Timestamp,
    #[serde(rename = "customTitle")]
    CustomTitle,
    #[serde(rename = "aiTitle")]
    AiTitle,
    #[serde(rename = "isApiErrorMessage")]
    ApiError,
    #[serde(rename = "message")]
    Message,
    #[serde(rename = "model")]
    Model,
    #[serde(other)]
    Other,
}

// Value accepts scalars and arrays too. These contain no demanded metadata; duplicate object
// fields overwrite earlier values, including null or a non-object replacing the message.
macro_rules! ignored_shapes {
    ($output:ty) => {
        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(<$output>::default())
        }
        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok(<$output>::default())
        }
        fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
            Ok(<$output>::default())
        }
        fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
            Ok(<$output>::default())
        }
        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok(<$output>::default())
        }
        fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
            Ok(<$output>::default())
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok(<$output>::default())
        }
    };
}

impl<'de> Deserialize<'de> for Fields<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Fields<'de>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a record value")
            }
            ignored_shapes!(Fields<'de>);
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = Fields::default();
                while let Some(key) = map.next_key::<Key>()? {
                    match key {
                        Key::Kind => out.kind = Some(map.next_value()?),
                        Key::Cwd => out.cwd = Some(map.next_value()?),
                        Key::Timestamp => out.timestamp = Some(map.next_value()?),
                        Key::CustomTitle => out.custom_title = Some(map.next_value()?),
                        Key::AiTitle => out.ai_title = Some(map.next_value()?),
                        Key::ApiError => out.api_error = Some(map.next_value()?),
                        Key::Message => out.message = map.next_value()?,
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(out)
            }
        }
        d.deserialize_any(V)
    }
}

impl<'de> Deserialize<'de> for Body<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Body<'de>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a message value")
            }
            ignored_shapes!(Body<'de>);
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = Body::default();
                while let Some(key) = map.next_key::<Key>()? {
                    match key {
                        Key::Model => out.model = Some(map.next_value()?),
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(out)
            }
        }
        d.deserialize_any(V)
    }
}

fn string(raw: Option<&RawValue>) -> Option<Cow<'_, str>> {
    let text = raw?.get();
    if !text.starts_with('"') {
        return None;
    }
    if !text.as_bytes().contains(&b'\\') {
        Some(Cow::Borrowed(&text[1..text.len() - 1]))
    } else {
        serde_json::from_str::<String>(text).ok().map(Cow::Owned)
    }
}

pub(super) struct Scan {
    session: SessionRef,
    custom_title: bool,
}

impl Scan {
    pub(super) fn new(path: &Path) -> Self {
        Self {
            session: SessionRef::new(
                path.file_stem().and_then(|s| s.to_str()).unwrap_or(""),
                Harness::Claude,
                path,
            ),
            custom_title: false,
        }
    }

    pub(super) fn line(&mut self, bytes: &[u8]) {
        let Ok(text) = std::str::from_utf8(bytes) else {
            return;
        };
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let Ok(fields) = serde_json::from_str::<Fields<'_>>(text) else {
            return;
        };
        if !super::super::store::json::value_compatible(text.as_bytes()) {
            return;
        }
        let kind = string(fields.kind);
        let ty = kind.as_deref().unwrap_or("");
        if ty == "custom-title" {
            if let Some(title) = string(fields.custom_title) {
                self.session.title = Some(title.into_owned());
                self.custom_title = true;
            }
        }
        if ty == "ai-title" && !self.custom_title {
            if let Some(title) = string(fields.ai_title) {
                self.session.title = Some(title.into_owned());
            }
        }
        if self.session.cwd.is_none() {
            self.session.cwd = string(fields.cwd).map(|cwd| PathBuf::from(cwd.as_ref()));
        }
        if let Some(ts) = string(fields.timestamp).as_deref().and_then(parse_ts) {
            self.session.created_at = Some(self.session.created_at.map_or(ts, |old| old.min(ts)));
            self.session.updated_at = Some(self.session.updated_at.map_or(ts, |old| old.max(ts)));
        }
        let synthetic = ty == "assistant"
            && (fields.api_error.is_some_and(|v| v.get() == "true")
                || string(fields.message.model).as_deref() == Some(SYNTHETIC_MODEL));
        if matches!(ty, "user" | "assistant") && !synthetic {
            self.session.message_count += 1;
        }
    }

    pub(super) fn reader<R: BufRead>(&mut self, reader: R) {
        for line in reader.lines() {
            let Ok(line) = line else {
                continue;
            };
            self.line(line.as_bytes());
        }
    }

    #[cfg(feature = "mmap")]
    pub(super) fn bytes(&mut self, bytes: &[u8]) {
        let mut start = 0;
        for end in memchr::memchr_iter(b'\n', bytes) {
            self.line(&bytes[start..end]);
            start = end + 1;
        }
        if start < bytes.len() {
            self.line(&bytes[start..]);
        }
    }

    pub(super) fn finish(self) -> SessionRef {
        self.session
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The former discovery reducer is the oracle: its count deliberately does not parse content,
    // and its cwd/timestamps include metadata-only records unlike the conversational parser.
    fn owned(path: &Path, bytes: &[u8]) -> SessionRef {
        let mut out = Scan::new(path);
        super::super::super::for_each_json_line(std::io::Cursor::new(bytes), |v| {
            let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
            if ty == "custom-title" {
                if let Some(t) = v.get("customTitle").and_then(Value::as_str) {
                    out.session.title = Some(t.into());
                    out.custom_title = true;
                }
            }
            if ty == "ai-title" && !out.custom_title {
                if let Some(t) = v.get("aiTitle").and_then(Value::as_str) {
                    out.session.title = Some(t.into());
                }
            }
            if out.session.cwd.is_none() {
                out.session.cwd = v.get("cwd").and_then(Value::as_str).map(PathBuf::from);
            }
            if let Some(ts) = v.get("timestamp").and_then(Value::as_str).and_then(parse_ts) {
                out.session.created_at = Some(out.session.created_at.map_or(ts, |c| c.min(ts)));
                out.session.updated_at = Some(out.session.updated_at.map_or(ts, |u| u.max(ts)));
            }
            let synthetic = ty == "assistant" && is_synthetic_assistant(&v, v.get("message").unwrap_or(&Value::Null));
            if matches!(ty, "user" | "assistant") && !synthetic {
                out.session.message_count += 1;
            }
            Flow::Continue
        });
        out.finish()
    }

    fn check(path: &Path, bytes: &[u8]) {
        let expected = serde_json::to_value(owned(path, bytes)).unwrap();
        let mut buffered = Scan::new(path);
        buffered.reader(std::io::Cursor::new(bytes));
        assert_eq!(
            serde_json::to_value(buffered.finish()).unwrap(),
            expected,
            "{}",
            path.display()
        );
        #[cfg(feature = "mmap")]
        {
            let mut mapped = Scan::new(path);
            mapped.bytes(bytes);
            assert_eq!(
                serde_json::to_value(mapped.finish()).unwrap(),
                expected,
                "{}",
                path.display()
            );
        }
    }

    #[test]
    fn discovery_projection_preserves_owned_record_semantics() {
        let mut cases = vec![
            "null".into(),
            "[]".into(),
            "123".into(),
            "\"text\"".into(),
            r#"{"type":"user","message":null}"#.into(),
            r#"{"type":"assistant","message":{"model":"<synthetic>"}}"#.into(),
            r#"{"type":"assistant","isApiErrorMessage":true,"message":null}"#.into(),
            r#"{"type":"assistant","isApiErrorMessage":"true","message":null}"#.into(),
            r#"{"type":"assistant","message":{"model":"<synthetic>"},"message":[]}"#.into(),
            r#"{"type":"assistant","message":{"model":"<synthetic>","model":null}}"#.into(),
            r#"{"type":"user","type":"assistant","isApiErrorMessage":true,"isApiErrorMessage":false}"#.into(),
            r#"{"type":"user","type":{},"cwd":null,"cwd":"/cwd"}"#.into(),
            r#"{"type":"custom-title","customTitle":"a","customTitle":[]}"#.into(),
            r#"{"\u0074ype":"assistant","message":{"model":"\u003csynthetic>"}}"#.into(),
            r#"{"type":"user","cwd":"/a\u0020b","timestamp":"2026-01-01T00:00:00Z"}"#.into(),
            r#"{"type":"user","unknown":1e400,"unknown":0}"#.into(),
            r#"{"type":"user","unknown":"\ud800","unknown":0}"#.into(),
            r#"{"type":"user","unknown":{"\ud800":0}}"#.into(),
            r#"{"type":"user","message":{"model":"normal","unknown":"\udc00"}}"#.into(),
            "\u{a0}{\"type\":\"user\"}\u{a0}".into(),
            "{\"type\":\"user\"} trailing".into(),
        ];
        for depth in 125..=129 {
            cases.push(format!(
                "{{\"type\":\"user\",\"unknown\":{}0{}}}",
                "[".repeat(depth),
                "]".repeat(depth)
            ));
        }
        for case in &cases {
            check(Path::new("oracle.jsonl"), case.as_bytes());
        }
        cases.extend([
            r#"{"type":"summary","summary":"ignored","cwd":"/metadata","timestamp":"2026-01-02T00:00:00Z"}"#.into(),
            r#"{"type":"ai-title","aiTitle":"AI"}"#.into(),
            r#"{"type":"custom-title","customTitle":null}"#.into(),
            r#"{"type":"ai-title","aiTitle":"new AI","timestamp":"2026-01-01T00:00:00Z"}"#.into(),
            r#"{"type":"custom-title","customTitle":"\u2603"}"#.into(),
            r#"{"type":"custom-title","customTitle":false}"#.into(),
            r#"{"type":"ai-title","aiTitle":"blocked"}"#.into(),
            r#"{"type":"attachment","timestamp":"2026-01-03T00:00:00Z"}"#.into(),
        ]);
        let bytes = cases.join("\r\n");
        check(Path::new("combined.jsonl"), bytes.as_bytes());
        check(
            Path::new("invalid-utf8.jsonl"),
            b"{\"type\":\"user\",\"unknown\":\"\xff\"}\n{\"type\":\"user\"}",
        );
    }

    #[test]
    fn discovery_projection_matches_owned_corpus_and_file_transport() {
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/claude");
        for entry in WalkDir::new(corpus).into_iter().filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                check(path, &fs::read(path).unwrap());
            }
        }
        let dir = std::env::temp_dir().join(format!("cv-claude-metadata-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let bytes = b"\n{\"type\":\"user\"}\r\n{\"type\":\"assistant\",\"message\":{\"model\":\"<synthetic>\"}}\n{\"type\":\"custom-title\",\"customTitle\":\"tail\"}";
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            serde_json::to_value(scan(&path).unwrap()).unwrap(),
            serde_json::to_value(owned(&path, bytes)).unwrap()
        );
        fs::write(&path, []).unwrap();
        assert_eq!(scan(&path).unwrap().message_count, 0);
        fs::remove_dir_all(dir).unwrap();
    }
}
