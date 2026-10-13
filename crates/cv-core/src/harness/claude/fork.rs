//! The source of a `--fork-session` session. Claude Code copies the source's active chain into a
//! new file under the new `sessionId` and writes no pointer back; the copied records keep their
//! `uuid`s and timestamps, so the shared `uuid` is the link.
//!
//! A fork is recognized from its own head, so an ordinary session pays one short head read:
//!
//! - its first `uuid` record is older than a record written before it (the fork's own
//!   `queue-operation` for the prompt it was started with precedes the copied history), or
//! - it begins at a `compact_boundary` (the copy starts at the source's last compaction, so the
//!   source holds that boundary mid-file).
//!
//! Its source is a sibling transcript in the same project directory that was live at the copied
//! record's time (modified since, first record no later), and either begins with the same `uuid`
//! without itself looking copied, or holds the boundary `uuid` past its own first record. Only
//! those few candidates are read whole. Siblings that are themselves forks of the same source are
//! never chosen; a fork of a fork resolves to the original.

use chrono::{DateTime, Utc};
#[cfg(feature = "mmap")]
use serde::Deserialize;
#[cfg(feature = "mmap")]
use serde_json::value::RawValue;
#[cfg(test)]
use serde_json::Value;
#[cfg(feature = "mmap")]
use std::borrow::Cow;
use std::fs;
use std::io::BufRead;
#[cfg(any(feature = "mmap", test))]
use std::io::BufReader;
use std::path::{Path, PathBuf};

/// How far a head read goes before giving up on finding the first `uuid` record.
const HEAD_LINES: usize = 64;

/// What a transcript's head says: its first `uuid` record and whether it looks copied.
struct Head {
    uuid: String,
    at: Option<DateTime<Utc>>,
    /// The first record is a `compact_boundary`.
    boundary: bool,
    /// The head looks copied (see the module docs).
    copied: bool,
}

/// Only the fields used to recognize a copy. Raw values skip giant message payloads; the
/// last occurrence wins, exactly as Value's object map does (including non-string values).
#[cfg(feature = "mmap")]
#[derive(Default)]
struct Fields<'a> {
    uuid: Option<&'a RawValue>,
    timestamp: Option<&'a RawValue>,
    subtype: Option<&'a RawValue>,
    parent: Option<&'a RawValue>,
}

#[cfg(feature = "mmap")]
impl<'de> Deserialize<'de> for Fields<'de> {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct Projection;
        impl<'de> serde::de::Visitor<'de> for Projection {
            type Value = Fields<'de>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut out = Fields::default();
                while let Some(key) = map.next_key::<String>()? {
                    let value = map.next_value::<&'de RawValue>()?;
                    match key.as_str() {
                        "uuid" => out.uuid = Some(value),
                        "timestamp" => out.timestamp = Some(value),
                        "subtype" => out.subtype = Some(value),
                        "parentUuid" => out.parent = Some(value),
                        _ => {}
                    }
                }
                Ok(out)
            }
        }
        de.deserialize_map(Projection)
    }
}

#[cfg(feature = "mmap")]
fn string(raw: Option<&RawValue>) -> Option<Cow<'_, str>> {
    let raw = raw?.get();
    if let Ok(s) = serde_json::from_str::<&str>(raw) {
        Some(Cow::Borrowed(s))
    } else {
        serde_json::from_str::<String>(raw).ok().map(Cow::Owned)
    }
}

/// Visit physical UTF-8 lines, stopping on a read/UTF-8 error as BufRead::lines did. Native
/// mapping keeps a giant first record out of the private heap; the portable transport retains
/// just one physical line. Both preserve the head's physical-line budget, including malformed
/// and empty records, and do not Unicode-trim JSON.
#[cfg(feature = "mmap")]
fn lines(path: &Path, limit: usize, mut visit: impl FnMut(&str) -> bool) -> Option<()> {
    let file = fs::File::open(path).ok()?;
    #[cfg(feature = "mmap")]
    if let Ok(map) = unsafe { memmap2::Mmap::map(&file) } {
        let mut start = 0;
        for end in memchr::memchr_iter(b'\n', &map)
            .chain(std::iter::once(map.len()))
            .take(limit)
        {
            let Ok(line) = std::str::from_utf8(&map[start..end]) else {
                break;
            };
            start = end.saturating_add(1);
            if !visit(line) {
                break;
            }
        }
        return Some(());
    }
    for line in BufReader::new(file).lines().take(limit) {
        let Ok(line) = line else { break };
        if !visit(&line) {
            break;
        }
    }
    Some(())
}

#[cfg(feature = "mmap")]
fn head(path: &Path) -> Option<Head> {
    let mut newest: Option<DateTime<Utc>> = None;
    let mut found = None;
    lines(path, HEAD_LINES, |line| {
        let Ok(v) = serde_json::from_str::<Fields<'_>>(line) else {
            return true;
        };
        // Raw ignored values skip checks Value performs; retain its acceptance before using
        // timestamps, including malformed discarded and overwritten fields.
        if !crate::harness::store::json::value_compatible_str_at_depth_with(line, 0, |_, _| {}) {
            return true;
        }
        let at = string(v.timestamp).as_deref().and_then(super::parse_ts);
        if let Some(uuid) = string(v.uuid) {
            let boundary = string(v.subtype).as_deref() == Some("compact_boundary")
                && v.parent.is_none_or(|parent| parent.get() == "null");
            let older = matches!((at, newest), (Some(a), Some(n)) if a < n);
            found = Some(Head {
                uuid: uuid.into_owned(),
                at,
                boundary,
                copied: boundary || older,
            });
            return false;
        }
        if let Some(a) = at {
            newest = Some(newest.map_or(a, |n| n.max(a)));
        }
        true
    })?;
    found
}

fn modified(path: &Path) -> Option<DateTime<Utc>> {
    fs::metadata(path).ok()?.modified().ok().map(DateTime::<Utc>::from)
}

/// Whether `path` holds `needle` anywhere (line by line, so peak memory is one line).
#[cfg(feature = "mmap")]
fn holds(path: &Path, needle: &str) -> bool {
    let mut found = false;
    lines(path, usize::MAX, |line| {
        found = line.contains(needle);
        !found
    });
    found
}

/// The session id `path` was forked from, when its head says it is a fork and a sibling holds the
/// records it copied.
#[cfg(test)]
pub(super) fn source(path: &Path) -> Option<String> {
    source_tracked(path, &mut crate::stream::Discard)
}

pub(super) fn source_tracked(path: &Path, sink: &mut dyn crate::MessageSink) -> Option<String> {
    let me = head(path)?;
    if !me.copied {
        return None;
    }
    let at = me.at?;
    let dir = path.parent()?;
    sink.dependency(dir); // A new/deleted eligible sibling can change the inferred source.
    let mut candidates: Vec<(DateTime<Utc>, PathBuf, Head)> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p != path)
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".jsonl") && !n.ends_with(".flat.jsonl"))
        })
        .filter(|p| {
            sink.dependency(p);
            p.is_file()
        })
        // Live at the copied record's time: written since.
        .filter(|p| modified(p).is_some_and(|m| m >= at))
        .filter_map(|p| {
            let h = head(&p)?;
            let first = h.at?;
            (first <= at).then_some((first, p, h))
        })
        .collect();
    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    candidates
        .into_iter()
        .find(|(_, p, h)| {
            if h.uuid == me.uuid {
                !h.copied
            } else {
                me.boundary && holds(p, &me.uuid)
            }
        })
        .and_then(|(_, p, _)| p.file_stem().and_then(|s| s.to_str()).map(str::to_string))
}

#[cfg(not(feature = "mmap"))]
fn head(path: &Path) -> Option<Head> {
    use crate::{
        harness::store::{
            document::{
                range::{Range, RangeSource},
                Fields,
            },
            jsonl::ranges,
        },
        json::{InvalidJson, JsonSource},
    };
    let source = JsonSource::capture(path).ok()?;
    let file = fs::File::open(path).ok()?;
    source.check_metadata(file.metadata().ok()?).ok()?;
    let utf8 = RangeSource::from_captured(source.clone()).ok()?;
    let frames = ranges::records(
        &file,
        ranges::Range {
            offset: 0,
            len: source.len(),
        },
    )
    .ok()?;
    let mut newest: Option<DateTime<Utc>> = None;
    for row in frames.take(HEAD_LINES) {
        let (raw, _) = row.ok()?;
        if !utf8
            .valid_utf8(Range {
                offset: raw.offset,
                len: raw.len,
            })
            .ok()?
        {
            break;
        }
        if raw.len == 0 {
            continue;
        }
        let facts = source.read_range_value(raw.offset, raw.len, |view| {
            let Some(fields) = Fields::parse(view, &["uuid", "timestamp", "subtype", "parentUuid"])? else {
                return Ok(None);
            };
            let uuid = fields.string("uuid")?;
            let at = fields.string("timestamp")?.as_deref().and_then(super::parse_ts);
            let boundary = fields.string("subtype")?.as_deref() == Some("compact_boundary")
                && fields
                    .get("parentUuid")
                    .map(|v| v.is_null())
                    .transpose()?
                    .unwrap_or(true);
            Ok(Some((uuid, at, boundary)))
        });
        let facts = match facts {
            Ok(Some(facts)) => facts,
            Ok(None) => continue,
            Err(error) if error.downcast_ref::<InvalidJson>().is_some() => continue,
            Err(_) => return None,
        };
        let (uuid, at, boundary) = facts;
        if let Some(uuid) = uuid {
            let copied = boundary || matches!((at,newest), (Some(a),Some(n)) if a<n);
            return Some(Head {
                uuid,
                at,
                boundary,
                copied,
            });
        }
        if let Some(at) = at {
            newest = Some(newest.map_or(at, |n| n.max(at)));
        }
    }
    None
}

#[cfg(not(feature = "mmap"))]
fn holds(path: &Path, needle: &str) -> bool {
    use crate::{
        harness::store::{
            document::range::{Range, RangeSource},
            jsonl::ranges,
        },
        json::JsonSource,
    };
    let run = || -> anyhow::Result<bool> {
        let source = JsonSource::capture(path)?;
        let file = fs::File::open(path)?;
        source.check_metadata(file.metadata()?)?;
        let utf8 = RangeSource::from_captured(source.clone())?;
        let frames = ranges::records(
            &file,
            ranges::Range {
                offset: 0,
                len: source.len(),
            },
        )?;
        let mut pending = Vec::new();
        let mut bridge = Vec::new();
        for row in frames {
            let (mut raw, complete) = row?;
            if !utf8.valid_utf8(Range {
                offset: raw.offset,
                len: raw.len,
            })? {
                break;
            }
            // BufRead::lines removes CR only when followed by LF.
            if complete && raw.len > 0 {
                let mut last = [0];
                ranges::read_exact_at(&file, &mut last, raw.offset + raw.len - 1)?;
                if last[0] == b'\r' {
                    raw.len -= 1;
                }
            }
            if needle.is_empty() {
                return Ok(true);
            }
            pending.clear();
            let mut reader = ranges::reader(&file, raw)?;
            loop {
                let bytes = reader.fill_buf()?;
                if bytes.is_empty() {
                    break;
                }
                bridge.clear();
                bridge.extend_from_slice(&pending);
                bridge.extend_from_slice(bytes);
                if memchr::memmem::find(&bridge, needle.as_bytes()).is_some() {
                    source.check_metadata(file.metadata()?)?;
                    return Ok(true);
                }
                let keep = bridge.len().min(needle.len() - 1);
                pending.clear();
                pending.extend_from_slice(&bridge[bridge.len() - keep..]);
                let n = bytes.len();
                reader.consume(n);
            }
        }
        source.check_metadata(file.metadata()?)?;
        Ok(false)
    };
    run().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(dir: &Path, name: &str, lines: &[Value]) -> PathBuf {
        let p = dir.join(name);
        let text: Vec<String> = lines.iter().map(Value::to_string).collect();
        fs::write(&p, text.join("\n") + "\n").unwrap();
        p
    }

    fn rec(uuid: &str, parent: Option<&str>, ts: &str) -> Value {
        json!({"type": "user", "uuid": uuid, "parentUuid": parent, "timestamp": ts,
               "message": {"role": "user", "content": "hi"}})
    }

    fn queued(ts: &str) -> Value {
        json!({"type": "queue-operation", "operation": "enqueue", "timestamp": ts})
    }

    fn legacy_head(path: &Path) -> Option<Head> {
        let mut newest: Option<DateTime<Utc>> = None;
        for line in BufReader::new(fs::File::open(path).ok()?).lines().take(HEAD_LINES) {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let at = v
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(super::super::parse_ts);
            if let Some(uuid) = v.get("uuid").and_then(Value::as_str) {
                let boundary = v.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
                    && v.get("parentUuid").is_none_or(Value::is_null);
                return Some(Head {
                    uuid: uuid.into(),
                    at,
                    boundary,
                    copied: boundary || matches!((at,newest), (Some(a),Some(n)) if a<n),
                });
            }
            if let Some(a) = at {
                newest = Some(newest.map_or(a, |n| n.max(a)));
            }
        }
        None
    }

    #[test]
    fn projected_heads_match_value_validation_and_physical_line_limits() {
        let w = cv_testkit::World::in_process("fork-head-projection");
        let path = w.base.join("head.jsonl");
        let uuid = r#"{"uuid":"shared","timestamp":"2026-09-19T00:00:00Z"}"#;
        let queued = r#"{"timestamp":"2026-09-19T01:00:00Z"}"#;
        let mut cases: Vec<Vec<u8>> = vec![
            format!("{queued}\n{uuid}\n").into_bytes(),
            format!("{}{uuid}\n", "\n".repeat(63)).into_bytes(),
            format!("{}{uuid}\n", "\n".repeat(64)).into_bytes(),
            format!("malformed\nnull\n[1,2]\n{uuid}\n").into_bytes(),
            format!("\u{a0}{queued}\n{uuid}\n").into_bytes(),
            format!("{{\"uuid\":\"bad\",\"ignored\":1e400}}\n{uuid}\n").into_bytes(),
            format!("{{\"uuid\":\"bad\",\"ignored\":\"\\ud800\"}}\n{uuid}\n").into_bytes(),
            br#"{"uuid":"first","uuid":17}
{"uuid":"last","uuid":"final","subtype":"compact_boundary","parentUuid":null}
"#
            .to_vec(),
            br#"{"uuid":"parent","subtype":"compact_boundary","parentUuid":false}
"#
            .to_vec(),
            br#"{"uuid":"escaped\u0031","timestamp":[],"parentUuid":null}
"#
            .to_vec(),
        ];
        cases.push([b"\xff\n".as_slice(), uuid.as_bytes()].concat());
        cases.push(format!("{{\"uuid\":\"giant\",\"message\":\"{}\"}}\n", "payload".repeat(150_000)).into_bytes());
        cases.push(
            format!(
                "{{\"uuid\":\"escaped\",\"message\":\"{}\"}}\n",
                "\\u0061".repeat(150_000)
            )
            .into_bytes(),
        );
        let observe = |head: Option<Head>| head.map(|h| (h.uuid, h.at, h.boundary, h.copied));
        for (i, bytes) in cases.iter().enumerate() {
            fs::write(&path, bytes).unwrap();
            assert_eq!(observe(head(&path)), observe(legacy_head(&path)), "case {i}");
        }
        fs::write(&path, [b"prefix\n\xff\n".as_slice(), b"needle\n"].concat()).unwrap();
        assert!(!holds(&path, "needle"), "invalid UTF-8 stops sibling traversal");
    }

    /// An uncompacted source: the fork starts with the same `uuid`, after its own newer queue
    /// record. The source, a sibling fork, and an unrelated session are told apart.
    #[test]
    fn a_fork_of_an_uncompacted_session_names_its_source() {
        let dir = std::env::temp_dir().join(format!("cv-claude-fork-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let t0 = "2026-10-11T20:00:00Z";
        let src = write(
            &dir,
            "src.jsonl",
            &[
                queued(t0),
                rec("u0", None, t0),
                rec("u1", Some("u0"), "2026-10-11T20:00:05Z"),
            ],
        );
        let fork_lines = [
            json!({"type": "custom-title", "customTitle": "t"}),
            queued("2026-10-11T21:00:00Z"),
            rec("u0", None, t0),
            rec("u1", Some("u0"), "2026-10-11T20:00:05Z"),
            rec("f1", Some("u1"), "2026-10-11T21:00:01Z"),
        ];
        let fork = write(&dir, "fork.jsonl", &fork_lines);
        let sibling = write(&dir, "sibling.jsonl", &fork_lines);
        write(&dir, "other.jsonl", &[queued(t0), rec("o0", None, t0)]);
        assert_eq!(source(&fork).as_deref(), Some("src"));
        assert_eq!(source(&sibling).as_deref(), Some("src"));
        assert_eq!(source(&src), None, "the source's head is not a copy");
        let _ = fs::remove_dir_all(&dir);
    }
}
