//! Discovery reads only titles, cwd, timestamps and whether an assistant is synthetic. Avoid
//! constructing conversation bodies and sidecars while retaining the owned codec's acceptance.

use super::*;
use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
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

struct Facts<'a> {
    ty: &'a str,
    custom_title: Option<Cow<'a, str>>,
    ai_title: Option<Cow<'a, str>>,
    cwd: Option<Cow<'a, str>>,
    at: Option<DateTime<Utc>>,
    synthetic: bool,
}

#[cfg(test)]
thread_local! { pub(super) static FRAMED: std::cell::RefCell<Vec<(u64,u64)>> = const { std::cell::RefCell::new(Vec::new()) }; }

pub(super) const CONTINUATION_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "/claude.discovery/1");
const LAYER: &str = "claude.discovery";

#[derive(Clone, Serialize, Deserialize)]
struct State {
    session: SessionRef,
    custom_title: bool,
}

pub(super) struct Scan {
    session: SessionRef,
    custom_title: bool,
    prefix: Option<(u64, State)>,
    readable: bool,
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
            prefix: None,
            readable: true,
        }
    }

    pub(super) fn restore(path: &Path, checkpoint: Option<&Checkpoint>) -> (Self, u64) {
        let mut scan = Self::new(path);
        if let Some(cp) = checkpoint {
            if let Some(state) = cp.get::<State>(LAYER) {
                if state.session.id == scan.session.id
                    && state.session.path == scan.session.path
                    && state.session.harness == Harness::Claude
                    && state.session.message_count as u64 <= cp.offset
                {
                    scan.session = state.session;
                    scan.custom_title = state.custom_title;
                    return (scan, cp.offset);
                }
            }
        }
        (scan, 0)
    }

    fn save_prefix(&mut self, offset: u64) {
        self.prefix = Some((
            offset,
            State {
                session: self.session.clone(),
                custom_title: self.custom_title,
            },
        ));
    }

    pub(super) fn checkpoint(&self) -> Option<Checkpoint> {
        if !self.readable {
            return None;
        }
        let (offset, state) = self.prefix.as_ref()?;
        let mut checkpoint = Checkpoint::new(*offset);
        checkpoint.put(LAYER, state);
        Some(checkpoint)
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
        // Fields validates grammar while skipping unused strings without decoded allocations.
        // IgnoredAny/RawValue do not enforce Value's Unicode, finite-number or nesting acceptance;
        // this separate boundary must also reject invalid ignored and overwritten fields.
        // UTF-8 is already checked above, so reuse it rather than validating the bytes again.
        if !super::super::store::json::value_compatible_str_at_depth_with(text, 0, |_, _| {}) {
            return;
        }
        let kind = string(fields.kind);
        let ty = kind.as_deref().unwrap_or("");
        self.note(Facts {
            ty,
            custom_title: if ty == "custom-title" {
                string(fields.custom_title)
            } else {
                None
            },
            ai_title: if ty == "ai-title" && !self.custom_title {
                string(fields.ai_title)
            } else {
                None
            },
            cwd: if self.session.cwd.is_none() {
                string(fields.cwd)
            } else {
                None
            },
            at: string(fields.timestamp).as_deref().and_then(parse_ts),
            synthetic: ty == "assistant"
                && (fields.api_error.is_some_and(|v| v.get() == "true")
                    || string(fields.message.model).as_deref() == Some(SYNTHETIC_MODEL)),
        });
    }

    fn note(&mut self, facts: Facts<'_>) {
        if facts.ty == "custom-title" {
            if let Some(title) = facts.custom_title {
                self.session.title = Some(title.into_owned());
                self.custom_title = true;
            }
        }
        if facts.ty == "ai-title" && !self.custom_title {
            if let Some(title) = facts.ai_title {
                self.session.title = Some(title.into_owned());
            }
        }
        if self.session.cwd.is_none() {
            self.session.cwd = facts.cwd.map(|cwd| PathBuf::from(cwd.as_ref()));
        }
        if let Some(ts) = facts.at {
            self.session.created_at = Some(self.session.created_at.map_or(ts, |old| old.min(ts)));
            self.session.updated_at = Some(self.session.updated_at.map_or(ts, |old| old.max(ts)));
        }
        if matches!(facts.ty, "user" | "assistant") && !facts.synthetic {
            self.session.message_count += 1;
        }
    }

    #[cfg(all(test, not(feature = "mmap")))]
    pub(super) fn positioned(&mut self, file: &fs::File, source: &JsonSource) -> Result<()> {
        self.positioned_at(file, source, 0)
    }

    #[cfg(not(feature = "mmap"))]
    pub(super) fn positioned_at(&mut self, file: &fs::File, source: &JsonSource, offset: u64) -> Result<()> {
        use crate::harness::store::{document::Fields, jsonl::ranges};
        source.check_metadata(file.metadata()?)?;
        let mut rows = ranges::records(
            file,
            ranges::Range {
                offset,
                len: source
                    .len()
                    .checked_sub(offset)
                    .context("discovery offset outside source")?,
            },
        )?;
        rows.keep_small_records(64 * 1024);
        let mut complete_offset = offset;
        let mut partial = false;
        while let Some(row) = rows.next() {
            let (raw, complete) = row?;
            if complete {
                complete_offset = rows.offset();
            } else {
                self.save_prefix(raw.offset);
                partial = true;
            }
            if let Some(bytes) = rows.small_record() {
                self.line(bytes);
                continue;
            }
            // Discovery historically uses str::trim, unlike the conversational byte driver.
            let range = ranges::trim(file, raw)?;
            if range.len == 0 {
                continue;
            }
            let result = source.read_range_value(range.offset, range.len, |view| {
                let Some(fields) = Fields::parse(
                    view,
                    &[
                        "type",
                        "cwd",
                        "timestamp",
                        "customTitle",
                        "aiTitle",
                        "isApiErrorMessage",
                        "message",
                    ],
                )?
                else {
                    return Ok(());
                };
                let kind = fields.string("type")?;
                let ty = kind.as_deref().unwrap_or("");
                let model = fields
                    .get("message")
                    .map(|v| Fields::parse(v, &["model"]))
                    .transpose()?
                    .flatten();
                let synthetic = ty == "assistant"
                    && (fields
                        .get("isApiErrorMessage")
                        .map(|v| v.as_bool())
                        .transpose()?
                        .flatten()
                        == Some(true)
                        || model
                            .as_ref()
                            .map(|m| m.string("model"))
                            .transpose()?
                            .flatten()
                            .as_deref()
                            == Some(SYNTHETIC_MODEL));
                self.note(Facts {
                    ty,
                    custom_title: if ty == "custom-title" {
                        fields.string("customTitle")?.map(Cow::Owned)
                    } else {
                        None
                    },
                    ai_title: if ty == "ai-title" && !self.custom_title {
                        fields.string("aiTitle")?.map(Cow::Owned)
                    } else {
                        None
                    },
                    cwd: if self.session.cwd.is_none() {
                        fields.string("cwd")?.map(Cow::Owned)
                    } else {
                        None
                    },
                    at: fields.string("timestamp")?.as_deref().and_then(parse_ts),
                    synthetic,
                });
                Ok(())
            });
            match result {
                Ok(()) => {}
                Err(error) if error.downcast_ref::<crate::json::InvalidJson>().is_some() => {}
                Err(error) => return Err(error),
            }
        }
        if !partial {
            self.save_prefix(complete_offset);
        }
        source.check_metadata(file.metadata()?)
    }

    #[cfg(test)]
    pub(super) fn reader<R: BufRead>(&mut self, reader: R) {
        self.reader_at(reader, 0);
    }

    #[cfg(any(feature = "mmap", test))]
    pub(super) fn reader_at<R: BufRead>(&mut self, mut reader: R, mut offset: u64) {
        let mut bytes = Vec::new();
        let mut partial = false;
        loop {
            bytes.clear();
            match reader.read_until(b'\n', &mut bytes) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => {
                    self.readable = false;
                    offset += bytes.len() as u64;
                    continue;
                }
            }
            if bytes.last() != Some(&b'\n') {
                self.save_prefix(offset);
                partial = true;
            }
            self.line(&bytes);
            offset += bytes.len() as u64;
        }
        if !partial {
            self.save_prefix(offset);
        }
    }

    #[cfg(all(test, feature = "mmap"))]
    pub(super) fn bytes(&mut self, bytes: &[u8]) {
        self.bytes_at(bytes, 0);
    }

    #[cfg(feature = "mmap")]
    pub(super) fn bytes_at(&mut self, bytes: &[u8], offset: u64) {
        let mut start = 0;
        for end in memchr::memchr_iter(b'\n', bytes) {
            self.line(&bytes[start..end]);
            start = end + 1;
        }
        self.save_prefix(offset + start as u64);
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
        #[cfg(not(feature = "mmap"))]
        {
            let world = cv_testkit::World::in_process("claude-discovery-positioned-oracle");
            let captured_path = world.base.join("rows.jsonl");
            fs::write(&captured_path, bytes).unwrap();
            let mut positioned = Scan::new(path);
            positioned
                .positioned(
                    &fs::File::open(&captured_path).unwrap(),
                    &JsonSource::capture(&captured_path).unwrap(),
                )
                .unwrap();
            assert_eq!(
                serde_json::to_value(positioned.finish()).unwrap(),
                expected,
                "{}",
                path.display()
            );
        }
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
    fn continuation_replays_partial_and_valid_unterminated_records() {
        let world = cv_testkit::World::in_process("claude-metadata-resume-tails");
        let path = world.base.join("s.jsonl");
        let prefix = "{\"type\":\"summary\",\"cwd\":\"/metadata\",\"timestamp\":\"2026-02-01T00:00:00Z\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"AI\"}\n";
        let tails = [
            (r#"{"type":"user","message":null}"#, "\n{\"type\":\"custom-title\",\"customTitle\":\"renamed\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"blocked\"}\n"),
            (r#"{"type":"user"}"#, " trailing\n{\"type\":\"assistant\",\"isApiErrorMessage\":true}\n"),
            (r#"{"type":"custom-title","customTitle":"part"#, "ial\"}\n{\"type\":\"user\",\"timestamp\":\"2025-01-01T00:00:00Z\"}\n"),
            (r#"{"type":"user","unknown":"\ud800"}"#, "\n{\"type\":\"assistant\",\"message\":{\"model\":\"normal\",\"model\":\"<synthetic>\"}}\n"),
        ];
        for (tail, appended) in tails {
            let old = format!("{prefix}{tail}");
            fs::write(&path, &old).unwrap();
            let (before, cp) = scan_at(&path, None).unwrap();
            assert_eq!(
                serde_json::to_value(before).unwrap(),
                serde_json::to_value(owned(&path, old.as_bytes())).unwrap()
            );
            let cp = cp.unwrap();
            assert_eq!(cp.offset, prefix.len() as u64);
            let grown = format!("{old}{appended}");
            fs::write(&path, &grown).unwrap();
            FRAMED.with(|frames| frames.borrow_mut().clear());
            let (resumed, next) = scan_at(&path, Some(&cp)).unwrap();
            assert_eq!(
                serde_json::to_value(resumed).unwrap(),
                serde_json::to_value(owned(&path, grown.as_bytes())).unwrap()
            );
            assert_eq!(
                FRAMED.with(|frames| frames.borrow().clone()),
                vec![(cp.offset, (grown.len() - prefix.len()) as u64)]
            );
            assert_eq!(next.unwrap().offset, grown.len() as u64);
            #[cfg(all(feature = "sqlite", unix))]
            {
                use std::io::Write;
                fs::write(&path, &old).unwrap();
                cached_metadata(&path, false).unwrap();
                crate::discover_cache::persist(false);
                let conn = crate::catalog::open_db().unwrap();
                crate::discover_cache::invalidate_interpretation(Path::new(conn.path().unwrap()));
                fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap()
                    .write_all(appended.as_bytes())
                    .unwrap();
                FRAMED.with(|frames| frames.borrow_mut().clear());
                assert_eq!(
                    serde_json::to_value(cached_metadata(&path, false).unwrap()).unwrap(),
                    serde_json::to_value(owned(&path, grown.as_bytes())).unwrap()
                );
                assert_eq!(
                    FRAMED.with(|frames| frames.borrow().clone()),
                    vec![(cp.offset, (grown.len() - prefix.len()) as u64)],
                    "persisted continuation must replay the entire provisional EOF record"
                );
            }
        }
    }

    #[cfg(all(feature = "sqlite", unix))]
    #[test]
    fn cached_continuation_is_persisted_and_rejects_rewrites_replacements_and_corruption() {
        use std::io::Write;
        let world = cv_testkit::World::in_process("claude-metadata-cache-resume");
        let path = world.base.join("s.jsonl");
        let prefix = format!("{{\"type\":\"summary\",\"cwd\":\"/metadata\"}}\n{{\"type\":\"assistant\",\"message\":{{\"content\":\"{}\"}}}}\n", "ignored".repeat(32768));
        fs::write(&path, &prefix).unwrap();
        let check = |expected: &str, extent: Option<(u64, u64)>| {
            FRAMED.with(|frames| frames.borrow_mut().clear());
            let actual = cached_metadata(&path, false).unwrap();
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(owned(&path, expected.as_bytes())).unwrap()
            );
            assert_eq!(
                FRAMED.with(|frames| frames.borrow().clone()),
                extent.into_iter().collect::<Vec<_>>()
            );
        };
        check(&prefix, Some((0, prefix.len() as u64)));
        check(&prefix, None);
        crate::discover_cache::persist(false);
        let conn = crate::catalog::open_db().unwrap();
        let saved: String = conn
            .query_row(
                "SELECT continuation FROM scan_cache WHERE path=?1",
                [path.to_string_lossy()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(serde_json::from_str::<crate::discover_cache::ScanContinuation>(&saved).is_ok());
        crate::discover_cache::invalidate_interpretation(Path::new(conn.path().unwrap()));
        let appended = "{\"type\":\"custom-title\",\"customTitle\":\"new title\"}\n{\"type\":\"user\",\"timestamp\":\"2025-01-01T00:00:00Z\"}\n";
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(appended.as_bytes())
            .unwrap();
        let mut text = format!("{prefix}{appended}");
        check(&text, Some((prefix.len() as u64, appended.len() as u64)));
        check(&text, None);
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        text = text.replace("new title", "bad title");
        fs::write(&path, &text).unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(mtime))
            .unwrap();
        check(&text, Some((0, text.len() as u64)));
        let replacement = world.base.join("replacement");
        let more = "{\"type\":\"ai-title\",\"aiTitle\":\"still blocked\"}\n";
        text.push_str(more);
        fs::write(&replacement, &text).unwrap();
        fs::rename(replacement, &path).unwrap();
        check(&text, Some((0, text.len() as u64)));
        crate::discover_cache::persist(false);
        conn.execute(
            "UPDATE scan_cache SET continuation='broken' WHERE path=?1",
            [path.to_string_lossy()],
        )
        .unwrap();
        crate::discover_cache::invalidate_interpretation(Path::new(conn.path().unwrap()));
        check(&text, None); // An ordinary exact hit does not decode continuation JSON.
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"type\":\"user\"}\n")
            .unwrap();
        text.push_str("{\"type\":\"user\"}\n");
        check(&text, Some((0, text.len() as u64)));
    }

    #[cfg(all(feature = "sqlite", unix))]
    #[test]
    fn cached_checkpoint_is_independent_of_counts_and_rejects_invalid_states() {
        use std::io::Write;
        let world = cv_testkit::World::in_process("claude-metadata-state-guards");
        let path = world.base.join("s.jsonl");
        let mut text = String::from("{\"type\":\"ai-title\",\"aiTitle\":\"initial\"}\n");
        fs::write(&path, &text).unwrap();
        cached_metadata(&path, false).unwrap();
        crate::discover_cache::persist(false);
        let conn = crate::catalog::open_db().unwrap();
        crate::discover_cache::fill_count(&conn, &path, Harness::Claude, "s", 99);
        let append = "{\"type\":\"user\"}\n";
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(append.as_bytes())
            .unwrap();
        text.push_str(append);
        assert_eq!(
            cached_metadata(&path, false).unwrap().message_count,
            1,
            "published fallback counts do not alter reducer state"
        );
        for mutation in 0..8 {
            crate::discover_cache::persist(false);
            let encoded: String = conn
                .query_row(
                    "SELECT continuation FROM scan_cache WHERE path=?1",
                    [path.to_string_lossy()],
                    |row| row.get(0),
                )
                .unwrap();
            let mut state: crate::discover_cache::ScanContinuation = serde_json::from_str(&encoded).unwrap();
            match mutation {
                0 => state.head.version = "obsolete".into(),
                1 => state.checkpoint.offset += 1,
                2 => {
                    state.head.offset = text.len() as u64 + 100;
                    state.checkpoint.offset = state.head.offset;
                }
                3 => state.dependencies = crate::source::ReadSet::unrecorded(),
                4 => state.checkpoint = Checkpoint::new(state.checkpoint.offset),
                5 => state.checkpoint.put(
                    LAYER,
                    &serde_json::json!({"session": "wrong-type", "custom_title": false}),
                ),
                6 => state.head.identity = None,
                _ => {
                    let mut reducer = state.checkpoint.get::<State>(LAYER).unwrap();
                    reducer.session.message_count = usize::MAX;
                    state.checkpoint.put(LAYER, &reducer);
                }
            }
            conn.execute(
                "UPDATE scan_cache SET continuation=?1 WHERE path=?2",
                rusqlite::params![serde_json::to_string(&state).unwrap(), path.to_string_lossy()],
            )
            .unwrap();
            crate::discover_cache::invalidate_interpretation(Path::new(conn.path().unwrap()));
            fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(append.as_bytes())
                .unwrap();
            text.push_str(append);
            FRAMED.with(|frames| frames.borrow_mut().clear());
            assert_eq!(
                serde_json::to_value(cached_metadata(&path, false).unwrap()).unwrap(),
                serde_json::to_value(owned(&path, text.as_bytes())).unwrap()
            );
            assert_eq!(
                FRAMED.with(|frames| frames.borrow().clone()),
                vec![(0, text.len() as u64)],
                "invalid state {mutation} falls back to the whole reducer"
            );
        }
        crate::discover_cache::persist(false);
        conn.execute(
            "UPDATE scan_cache SET continuation=NULL WHERE path=?1",
            [path.to_string_lossy()],
        )
        .unwrap();
        crate::discover_cache::invalidate_interpretation(Path::new(conn.path().unwrap()));
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(append.as_bytes())
            .unwrap();
        text.push_str(append);
        FRAMED.with(|frames| frames.borrow_mut().clear());
        cached_metadata(&path, false).unwrap();
        assert_eq!(
            FRAMED.with(|frames| frames.borrow().clone()),
            vec![(0, text.len() as u64)]
        );
        fs::write(&path, "{\"type\":\"user\"}\n").unwrap();
        FRAMED.with(|frames| frames.borrow_mut().clear());
        assert_eq!(cached_metadata(&path, false).unwrap().message_count, 1);
        assert_eq!(
            FRAMED.with(|frames| frames.borrow().clone()),
            vec![(0, append.len() as u64)]
        );
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
