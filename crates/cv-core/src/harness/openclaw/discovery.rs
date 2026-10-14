//! One compact index inventory per discovery operation, with checked transcript projections.
use super::*;
use crate::harness::store::{document::Fields, jsonl::ranges};
use crate::harness::wire::text_title;
use crate::json::{JsonSource, JsonView};

#[derive(Default, Debug, PartialEq)]
pub(super) struct Entry {
    title: Option<String>,
    cwd: Option<PathBuf>,
    updated_at: Option<DateTime<Utc>>,
}
type Index = HashMap<String, Entry>;

/// Operation-local reuse only: all consulted indexes, including absent ones, are rechecked
/// before discovery publishes. Their unrelated rich descendants never become owned Values.
#[derive(Default)]
pub(super) struct Indexes {
    entries: HashMap<PathBuf, Index>,
    reads: crate::source::ReadSet,
}
impl Indexes {
    pub(super) fn get(&mut self, dir: &Path) -> &Index {
        self.entries.entry(dir.to_path_buf()).or_insert_with(|| {
            let path = dir.join("sessions.json");
            self.reads.record(&path);
            // Missing, unreadable and malformed indexes historically fall back to transcript
            // metadata. Capture before the attempt; final dependency checks reject a race.
            JsonSource::capture(&path)
                .and_then(|source| source.read_document(index))
                .unwrap_or_default()
        })
    }
    pub(super) fn check(&self) -> Result<()> {
        anyhow::ensure!(
            self.reads.fresh(&mut crate::source::Probe::default()),
            "OpenClaw discovery index dependencies changed or unavailable"
        );
        Ok(())
    }
}

fn index(view: JsonView<'_>) -> Result<Index> {
    let mut out = HashMap::new();
    // for_each_entry preserves Value's last-duplicate values and first insertion order,
    // including the winner when several registry keys name the same sessionId.
    view.for_each_entry(|_, entry| {
        let Some(fields) = Fields::parse(
            entry,
            &[
                "sessionId",
                "label",
                "subject",
                "cwd",
                "spawnedCwd",
                "spawnedWorkspaceDir",
                "updatedAt",
            ],
        )?
        else {
            return Ok(());
        };
        let Some(id) = fields.string("sessionId")? else {
            return Ok(());
        };
        // A present invalid/blank label suppresses subject, as entry_title does.
        let title = fields.get("label").or_else(|| fields.get("subject"));
        let title = string_title(title)?;
        let mut cwd = None;
        for key in ["cwd", "spawnedCwd", "spawnedWorkspaceDir"] {
            if let Some(path) = fields.string(key)? {
                cwd = Some(PathBuf::from(path));
                break;
            }
        }
        out.insert(
            id,
            Entry {
                title,
                cwd,
                updated_at: timestamp(fields.get("updatedAt"))?,
            },
        );
        Ok(())
    })?;
    Ok(out)
}
fn string_title(value: Option<JsonView<'_>>) -> Result<Option<String>> {
    match value {
        Some(value) if value.is_string()? => text_title(Some(value)),
        _ => Ok(None),
    }
}
fn timestamp(value: Option<JsonView<'_>>) -> Result<Option<DateTime<Utc>>> {
    match value {
        Some(value) if value.is_string()? || value.as_number()?.is_some() => {
            Ok(ts_from_value(value.resolve()?.as_ref()))
        }
        _ => Ok(None),
    }
}

pub(super) fn scan(path: &Path, index: &Index) -> Result<SessionRef> {
    let source = JsonSource::capture(path)?;
    let file = fs::File::open(path)?;
    source.check_metadata(file.metadata()?)?;
    // The prior read_to_string rejected every invalid byte, even on malformed/ignored rows.
    anyhow::ensure!(
        ranges::valid_utf8(&mut ranges::reader(
            &file,
            ranges::Range {
                offset: 0,
                len: source.len()
            }
        )?)?,
        "OpenClaw discovery transcript did not contain valid UTF-8"
    );
    let mut acc = ScanAcc::default();
    super::super::store::checked_lines::for_each(&file, 0, &source, |view, _, _, _| {
        feed(view, &mut acc)?;
        Ok(Flow::Continue)
    })?;
    source.check_metadata(file.metadata()?)?;
    source.check()?;
    let id = if acc.id.is_empty() {
        session_id_from_filename(path)
    } else {
        acc.id
    };
    let entry = index.get(&id);
    Ok(SessionRef {
        harness: Harness::OpenClaw,
        path: path.to_path_buf(),
        title: entry
            .and_then(|e| e.title.clone())
            .or(acc.session_name)
            .or(acc.first_user),
        cwd: acc.cwd.or_else(|| entry.and_then(|e| e.cwd.clone())),
        created_at: acc.created_at,
        updated_at: entry.and_then(|e| e.updated_at).or(acc.updated_at),
        message_count: acc.message_count,
        id,
    })
}

fn feed(view: JsonView<'_>, acc: &mut ScanAcc) -> Result<()> {
    let Some(fields) = Fields::parse(view, &["type", "id", "cwd", "timestamp", "name", "message"])? else {
        return Ok(());
    };
    match fields.string("type")?.as_deref() {
        Some("session") => {
            if acc.id.is_empty() {
                acc.id = fields.string("id")?.unwrap_or_default();
            }
            if acc.cwd.is_none() {
                acc.cwd = fields.string("cwd")?.map(PathBuf::from);
            }
            if acc.created_at.is_none() {
                acc.created_at = fields.string("timestamp")?.as_deref().and_then(parse_ts);
            }
        }
        Some("message") => {
            let body = fields
                .get("message")
                .map(|value| Fields::parse(value, &["role", "timestamp", "content"]))
                .transpose()?
                .flatten();
            let role = body.as_ref().map(|f| f.string("role")).transpose()?.flatten();
            acc.message_count += usize::from(matches!(role.as_deref(), Some("user" | "assistant")));
            let mut entry_timestamp = fields.string("timestamp")?.as_deref().and_then(parse_ts);
            if entry_timestamp.is_none() {
                entry_timestamp = timestamp(body.as_ref().and_then(|f| f.get("timestamp")))?;
            }
            if let Some(timestamp) = entry_timestamp {
                acc.updated_at = Some(timestamp);
            }
            if acc.first_user.is_none() && role.as_deref() == Some("user") {
                acc.first_user = text_title(body.as_ref().and_then(|f| f.get("content")))?;
            }
        }
        Some("session_info") => {
            if let Some(title) = string_title(fields.get("name"))? {
                acc.session_name = Some(title);
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn owned_index(bytes: &[u8]) -> Index {
        let mut out = HashMap::new();
        if let Ok(Value::Object(entries)) = serde_json::from_slice::<Value>(bytes) {
            for (_, entry) in entries {
                if let Some(id) = entry.get("sessionId").and_then(Value::as_str) {
                    out.insert(
                        id.into(),
                        Entry {
                            title: entry_title(Some(&entry)),
                            cwd: entry_cwd(Some(&entry)),
                            updated_at: entry.get("updatedAt").and_then(ts_from_value),
                        },
                    );
                }
            }
        }
        out
    }

    #[test]
    fn selected_indexes_preserve_value_precedence_and_tolerant_fallback() -> Result<()> {
        let world = cv_testkit::World::in_process("openclaw-discovery-index");
        let dir = world.base.join("sessions");
        fs::create_dir_all(&dir)?;
        let path = dir.join("sessions.json");
        let giant = "雪\n".repeat(30000);
        let mut cases = vec![
            Vec::new(), b"{bad}".to_vec(), b"[]".to_vec(), b"null".to_vec(),
            br#"{"z":{"sessionId":"same","label":"first"},"a":{"sessionId":"same","label":"second"},"z":{"sessionId":"same","label":"replacement"}}"#.to_vec(),
            br#"{"key":{"sessionId":"s","label":null,"subject":"suppressed","cwd":false,"spawnedCwd":"","spawnedWorkspaceDir":"suppressed","updatedAt":false}}"#.to_vec(),
            br#"{"a":{"sessionId":"s","subject":"title","cwd":"/cwd","updatedAt":1704067200000},"ignored":{"unused":1e999}}"#.to_vec(),
            br#"{"a":{"sessionId":"s","subject":"title"},"ignored":{"unused":"\ud800"}}"#.to_vec(),
            b"{\"a\":{\"sessionId\":\"s\",\"subject\":\"title\"},\"ignored\":\"\xff\"}".to_vec(),
        ];
        for title in [
            " \u{2003}".repeat(10000),
            format!("{}end", " \u{2003}".repeat(10000)),
            giant.clone(),
        ] {
            cases.push(serde_json::to_vec(&json!({
                "one":{"sessionId":"s1","label":title,"subject":"fallback","cwd":"/one","spawnedCwd":giant,"updatedAt":"2024-01-01T00:00:00Z","unused":giant},
                "two":{"sessionId":"s2","subject":"two","spawnedWorkspaceDir":"/two","updatedAt":1704067200000_i64},
                "invalid":{"sessionId":false},
            }))?);
        }
        for bytes in cases {
            fs::write(&path, &bytes)?;
            let mut indexes = Indexes::default();
            assert_eq!(indexes.get(&dir), &owned_index(&bytes));
            indexes.check()?;
        }
        Ok(())
    }

    #[test]
    fn discovery_reuses_one_index_snapshot_and_rejects_changed_or_created_indexes() -> Result<()> {
        let world = cv_testkit::World::in_process("openclaw-discovery-index-reuse");
        let dir = world.base.join("sessions");
        fs::create_dir_all(&dir)?;
        let path = dir.join("sessions.json");
        for before in [None, Some("{bad}"), Some(r#"{"key":{"sessionId":"s","label":"old"}}"#)] {
            if let Some(before) = before {
                fs::write(&path, before)?;
            } else if path.exists() {
                fs::remove_file(&path)?;
            }
            let mut indexes = Indexes::default();
            let first = indexes.get(&dir).get("s").and_then(|entry| entry.title.clone());
            indexes.check()?;
            fs::write(&path, r#"{"key":{"sessionId":"s","label":"new"}}"#)?;
            assert_eq!(indexes.get(&dir).get("s").and_then(|entry| entry.title.clone()), first);
            assert!(indexes.check().is_err());
            let mut next_operation = Indexes::default();
            assert_eq!(
                next_operation
                    .get(&dir)
                    .get("s")
                    .and_then(|entry| entry.title.as_deref()),
                Some("new")
            );
            next_operation.check()?;
        }
        Ok(())
    }

    fn owned_scan(path: &Path, index: &Index) -> Result<SessionRef> {
        let text = fs::read_to_string(path)?;
        let mut acc = ScanAcc::default();
        super::super::super::for_each_json_line_str(&text, |value| {
            acc.feed(&value);
            Flow::Continue
        });
        let id = if acc.id.is_empty() {
            session_id_from_filename(path)
        } else {
            acc.id
        };
        let entry = index.get(&id);
        Ok(SessionRef {
            harness: Harness::OpenClaw,
            path: path.to_path_buf(),
            id,
            title: entry
                .and_then(|e| e.title.clone())
                .or(acc.session_name)
                .or(acc.first_user),
            cwd: acc.cwd.or_else(|| entry.and_then(|e| e.cwd.clone())),
            created_at: acc.created_at,
            updated_at: entry.and_then(|e| e.updated_at).or(acc.updated_at),
            message_count: acc.message_count,
        })
    }

    #[test]
    fn projected_transcript_discovery_preserves_owned_scan_and_inventory_metadata() -> Result<()> {
        let world = cv_testkit::World::in_process("openclaw-discovery-transcript");
        let sessions = world.base.join("agents/main/sessions");
        fs::create_dir_all(&sessions)?;
        let index_bytes = serde_json::to_vec(&json!({
            "key":{"sessionId":"named","label":"index title","cwd":"/index","updatedAt":1704067200000_i64},
        }))?;
        fs::write(sessions.join("sessions.json"), &index_bytes)?;
        let index = owned_index(&index_bytes);
        let body = " \n\u{2003}".repeat(10000) + "tail";
        let path = sessions.join("named.jsonl");
        let mut text = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            json!({"type":"session","id":"named","cwd":"/transcript","timestamp":"2024-01-01T01:00:00Z","ignored":body}),
            json!({"type":"message","timestamp":"invalid","message":{"role":"user","timestamp":1704070800000_i64,"content":[{"text":""},{"text":body},{"type":"ignored","text":"last"}],"ignored":body}}),
            json!({"type":"session_info","name":"  \n"}),
            json!({"type":"message","timestamp":"2024-01-01T03:00:00Z","message":{"role":"assistant","timestamp":1704070800000_i64,"content":[{"type":"thinking","thinking":body}]}}),
            json!({"type":"message","message":{"role":"toolResult","timestamp":1704070800000_i64,"content":body}}),
        );
        text.push_str(
            "\u{2003}{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"accepted\"}}\u{2003}\n",
        );
        text.push_str("{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"rejected\"},\"ignored\":1e999}\n{bad}\n");
        for index in [&index, &Index::default()] {
            fs::write(&path, &text)?;
            let expected = owned_scan(&path, index)?;
            assert_eq!(
                serde_json::to_value(scan(&path, index)?)?,
                serde_json::to_value(expected)?
            );
        }
        fs::write(&path, &text)?;
        let refs = OpenClaw::with_state_dir(&world.base).discover()?;
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].title.as_deref(), Some("index title"));
        assert_eq!(refs[0].cwd.as_deref(), Some(Path::new("/transcript")));
        assert_eq!(refs[0].message_count, 3);
        fs::write(&path, [text.as_bytes(), b"\n{bad}\xff"].concat())?;
        assert!(scan(&path, &index).is_err());
        Ok(())
    }
}
