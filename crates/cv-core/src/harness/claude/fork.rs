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
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
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

fn head(path: &Path) -> Option<Head> {
    let reader = BufReader::new(fs::File::open(path).ok()?);
    let mut newest: Option<DateTime<Utc>> = None;
    for line in reader.lines().take(HEAD_LINES) {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let at = v.get("timestamp").and_then(Value::as_str).and_then(super::parse_ts);
        if let Some(uuid) = v.get("uuid").and_then(Value::as_str) {
            let boundary = v.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
                && v.get("parentUuid").is_none_or(Value::is_null);
            let older = matches!((at, newest), (Some(a), Some(n)) if a < n);
            return Some(Head {
                uuid: uuid.to_string(),
                at,
                boundary,
                copied: boundary || older,
            });
        }
        if let Some(a) = at {
            newest = Some(newest.map_or(a, |n| n.max(a)));
        }
    }
    None
}

fn modified(path: &Path) -> Option<DateTime<Utc>> {
    fs::metadata(path).ok()?.modified().ok().map(DateTime::<Utc>::from)
}

/// Whether `path` holds `needle` anywhere (line by line, so peak memory is one line).
fn holds(path: &Path, needle: &str) -> bool {
    let Ok(f) = fs::File::open(path) else {
        return false;
    };
    BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .any(|l| l.contains(needle))
}

/// The session id `path` was forked from, when its head says it is a fork and a sibling holds the
/// records it copied.
pub(super) fn source(path: &Path) -> Option<String> {
    let me = head(path)?;
    if !me.copied {
        return None;
    }
    let at = me.at?;
    let dir = path.parent()?;
    let mut candidates: Vec<(DateTime<Utc>, PathBuf, Head)> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p != path && p.is_file())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".jsonl") && !n.ends_with(".flat.jsonl"))
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
