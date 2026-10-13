//! Legacy discovery retains header metadata and the exact conversational count.
use super::*;
use crate::harness::store::{document::Fields, jsonl::ranges};
use crate::json::{InvalidJson, JsonSource, JsonView};

pub(super) fn discover(name: &str, path: &Path) -> Option<SessionRef> {
    discover_checked(name, path).ok()
}
fn discover_checked(name: &str, path: &Path) -> Result<SessionRef> {
    let source = JsonSource::capture(path)?;
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    source.check_metadata(metadata.clone())?;
    let mut records = ranges::records(
        &file,
        ranges::Range {
            offset: 0,
            len: metadata.len(),
        },
    )?;
    records.keep_small_records(64 * 1024);
    let mut session = SessionRef::new(name, Harness::Goose, path);
    let mut header = true;
    while let Some(record) = records.next() {
        let (range, _) = record?;
        let result = if let Some(bytes) = records.small_record() {
            // read_to_string rejected invalid UTF-8 in malformed and ignored records too.
            let text = std::str::from_utf8(bytes)?;
            match serde_json::from_str::<Value>(text) {
                Ok(value) => note((&value).into(), header, &mut session),
                Err(_) => Ok(()),
            }
        } else {
            source.read_range_value(range.offset, range.len, |view| note(view, header, &mut session))
        };
        match result {
            Ok(()) => {}
            Err(error) if error.downcast_ref::<InvalidJson>().is_some() => {
                // Failed syntax can stop before an invalid UTF-8 suffix; it cannot hide it.
                anyhow::ensure!(
                    ranges::valid_utf8(&mut ranges::reader(&file, range)?)?,
                    "invalid Goose legacy UTF-8"
                );
            }
            Err(error) => return Err(error),
        }
        header = false;
    }
    source.check_metadata(file.metadata()?)?;
    source.check()?;
    Ok(session)
}
fn note(view: JsonView<'_>, header: bool, session: &mut SessionRef) -> Result<()> {
    if header {
        let Some(fields) = Fields::parse(view, &["description", "working_dir", "created_at", "updated_at"])? else {
            return Ok(());
        };
        if let Some(description) = fields.get("description") {
            if description.is_string()? {
                let mut prefix = String::new();
                let mut chars = 0;
                let mut nonwhite = false;
                description.for_each_str_chunk(|part| {
                    nonwhite |= part.chars().any(|c| !c.is_whitespace());
                    for c in part.chars().take(81 - chars) {
                        prefix.push(c);
                        chars += 1;
                    }
                    Ok(())
                })?;
                session.title = nonwhite.then(|| crate::ir::truncate(&prefix, 80));
            }
        }
        session.cwd = fields
            .string("working_dir")?
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        for (key, output) in [
            ("created_at", &mut session.created_at),
            ("updated_at", &mut session.updated_at),
        ] {
            if let Some(value) = fields.get(key) {
                if value.is_string()? || value.as_number()?.is_some() {
                    *output = parse_ts_value(value.resolve()?.as_ref());
                }
            }
        }
        return Ok(());
    }
    let Some(fields) = Fields::parse(view, &["role", "content"])? else {
        // Value::get on scalars/arrays defaults the role to user.
        session.message_count += 1;
        return Ok(());
    };
    let conversational = match fields.get("role") {
        Some(role) if role.is_string()? => equals(role, "user")? || equals(role, "assistant")?,
        _ => true,
    };
    if !conversational {
        return Ok(());
    }
    let mut only_results = false;
    if let Some(items) = fields.get("content").map(|v| v.array()).transpose()?.flatten() {
        for item in items {
            let result = match Fields::parse(item?, &["type"])? {
                Some(fields) => fields
                    .get("type")
                    .map(|v| equals(v, "toolResponse"))
                    .transpose()?
                    .unwrap_or(false),
                None => false,
            };
            if !result {
                only_results = false;
                break;
            }
            only_results = true;
        }
    }
    session.message_count += usize::from(!only_results);
    Ok(())
}
fn equals(view: JsonView<'_>, expected: &str) -> Result<bool> {
    let mut offset = 0usize;
    let mut equal = true;
    let string = view.for_each_str_chunk(|part| {
        let end = offset.saturating_add(part.len());
        equal &= expected.as_bytes().get(offset..end) == Some(part.as_bytes());
        offset = end;
        Ok(())
    })?;
    Ok(string && equal && offset == expected.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projected_discovery_preserves_header_count_and_whole_utf8_policy() {
        let world = cv_testkit::World::in_process("goose-legacy-discovery");
        let path = world.base.join("legacy.jsonl");
        let observe = |r: Option<SessionRef>| serde_json::to_value(r).unwrap();
        let cases: Vec<Vec<u8>> = vec![
  vec![], b"\n".to_vec(), b"{bad}\nnull\n[1]\n3\ntrue\n{}\n".to_vec(),
  br#"{"description":"old","description":"new","working_dir":"/repo","created_at":1704067200,"updated_at":"2024-01-01 12:00:00"}
{"role":"assistant","content":[{"type":"toolResponse"}]}
{"role":"assistant","content":[{"type":"toolResponse"},{"type":"text","text":"x"}]}
{"role":false,"content":[]}
{"role":"system","content":[{"type":"text","text":"x"}]}
{"role":"user","role":"tool","content":null}
{"role":"user","content":[{"type":"text","type":"toolResponse"}]}
{"role":"user","content":[{"type":"toolResponse"},{"type":null}]}
"#.to_vec(),
  br#"{"description":null,"working_dir":false,"created_at":[],"updated_at":true}
{"role":"user","ignored":1e400}
{"role":"user","ignored":"\ud800"}
{"role":"user","ignored":1e400,"ignored":1}
{"role":"user","ignored":"\ud800","ignored":"ok"}
{"role":"user","content":[{"type":"toolResponse","ignored":1e400}]}
{"role":"assistant","content":"end"}"#.to_vec(),
  "\u{2003}{}\n{\"role\":\"user\"}\r\n".as_bytes().to_vec(),
  [b"{}\n{bad}\n".as_slice(),b"\xff\n"].concat(),
  [b"{bad}\n".as_slice(),b"\xff"].concat(),
 ];
        let mut cases = cases;
        cases.push([b"{}\n{bad".as_slice(), &vec![b'x'; 70_000], b"\xff\n{}\n"].concat());
        cases.push([b"{}\n{bad".as_slice(), &vec![b'x'; 70_000], b"\n{}\n"].concat());
        for bytes in cases {
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(
                observe(discover("legacy", &path)),
                observe(legacy_oracle("legacy", &path))
            );
        }
        for escaped in [false, true] {
            let body = if escaped { "\\n雪" } else { "x雪" }.repeat(100_000);
            let header = serde_json::json!({"description":" \nTitle","working_dir":"/repo","created_at":1704067200000_i64,"unused":body});
            let row = serde_json::json!({"role":"user","content":[{"type":"text","text":body}]});
            let text = format!("{header}\n{row}\n{{\"role\":\"assistant\",\"content\":[]}}");
            std::fs::write(&path, text).unwrap();
            assert_eq!(
                observe(discover("legacy", &path)),
                observe(legacy_oracle("legacy", &path))
            );
        }
        for title in [
            " ".repeat(100_000),
            format!("{}Meaning", " \u{2003}".repeat(100_000)),
            "雪\n".repeat(100_000),
        ] {
            std::fs::write(&path, serde_json::json!({"description":title}).to_string()).unwrap();
            assert_eq!(
                observe(discover("legacy", &path)),
                observe(legacy_oracle("legacy", &path))
            );
        }
        let deep = format!("{{}}\n{}0{}\n{{\"role\":\"user\"}}\n", "[".repeat(129), "]".repeat(129));
        std::fs::write(&path, deep).unwrap();
        assert_eq!(
            observe(discover("legacy", &path)),
            observe(legacy_oracle("legacy", &path))
        );
    }

    fn legacy_oracle(name: &str, path: &Path) -> Option<SessionRef> {
        let text = std::fs::read_to_string(path).ok()?;
        let mut lines = text.lines();
        let header: Value = lines
            .next()
            .and_then(|l| serde_json::from_str(l).ok())
            .unwrap_or(Value::Null);
        // The contract count (`SessionRef::message_count`): user + assistant turns, with a user line
        // that is only tool results read as the Tool turn `stream_legacy` makes of it.
        let count = lines
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|m| {
                let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
                let only_tool_results = m.get("content").and_then(Value::as_array).is_some_and(|items| {
                    !items.is_empty()
                        && items
                            .iter()
                            .all(|i| i.get("type").and_then(Value::as_str) == Some("toolResponse"))
                });
                matches!(role, "user" | "assistant") && !only_tool_results
            })
            .count();
        let title = header
            .get("description")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(|s| crate::ir::truncate(s, 80));
        let cwd = header
            .get("working_dir")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        Some(SessionRef {
            id: name.to_string(),
            harness: Harness::Goose,
            path: path.to_path_buf(),
            cwd,
            title,
            created_at: header.get("created_at").and_then(parse_ts_value),
            updated_at: header.get("updated_at").and_then(parse_ts_value),
            message_count: count,
        })
    }
}
