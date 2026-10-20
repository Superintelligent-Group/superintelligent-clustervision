//! The sidecar tree Claude Code keeps beside a session transcript. For `<dir>/<sid>.jsonl` it is
//! `<dir>/<sid>/`, holding:
//!
//! - `subagents/agent-<id>.jsonl` (+ `agent-<id>.meta.json`) — directly spawned `Agent`/`Task`
//!   sub-agents;
//! - `subagents/workflows/<run>/agent-<id>.jsonl` + `journal.jsonl` — one dir per `Workflow`
//!   run: its agents' transcripts and the orchestrator's journaled per-agent results;
//! - `workflows/wf_<run>.json` + `workflows/scripts/<name>-wf_<run>.js` — the run's state file
//!   and driving script.
//!
//! Every one of those paths is spelled here and nowhere else, and the one `journal.jsonl`
//! parser lives here: [`super`] (the sub-agent forest) and [`super::super::claude_workflow`]
//! (the Workflow objects) used to each carry their own copy of both.

use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The journal a `Workflow` run writes into its run dir.
pub const JOURNAL: &str = "journal.jsonl";

/// A JSON-object sidecar (a sub-agent's `agent-<id>.meta.json`) as its object: empty when the
/// file is missing, unreadable, not JSON, or not an object. A sidecar is optional, so a bad one
/// reads as none, the same way for every reader and writer that consults it.
pub fn read_or_default(path: &Path) -> serde_json::Map<String, Value> {
    match std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    {
        Some(Value::Object(object)) => object,
        _ => serde_json::Map::new(),
    }
}

/// `<dir>/<stem>/` for a transcript at `<dir>/<stem>.jsonl`: the session's sidecar root.
pub fn session_dir(transcript: &Path) -> Option<PathBuf> {
    let stem = transcript.file_stem()?.to_str()?;
    Some(transcript.parent()?.join(stem))
}

/// `subagents/`: directly spawned sub-agent transcripts and their `.meta.json` sidecars.
pub fn subagents_dir(transcript: &Path) -> Option<PathBuf> {
    session_dir(transcript).map(|d| d.join("subagents"))
}

/// `subagents/workflows/`: one dir per `Workflow` run.
pub fn workflow_runs_dir(transcript: &Path) -> Option<PathBuf> {
    subagents_dir(transcript).map(|d| d.join("workflows"))
}

/// `subagents/workflows/<run_id>/`: a run's agent transcripts and its [`JOURNAL`].
pub fn workflow_run_dir(transcript: &Path, run_id: &str) -> Option<PathBuf> {
    workflow_runs_dir(transcript).map(|d| d.join(run_id))
}

/// `workflows/`: the `wf_<run>.json` state files and the `scripts/` dir.
pub fn workflows_dir(transcript: &Path) -> Option<PathBuf> {
    session_dir(transcript).map(|d| d.join("workflows"))
}

/// A run dir's `journal.jsonl` reduced to `agentId → full result Value`. The LAST `result` record
/// per agent wins: a `result` supersedes the earlier `started`, and a re-issued agent journals more
/// than once. Empty when the journal is absent or unreadable; a corrupt line is skipped the way
/// every adapter skips one (the shared JSONL lifecycle).
pub fn journal_results(run_dir: &Path) -> BTreeMap<String, Value> {
    let mut map = BTreeMap::new();
    let read = crate::harness::store::jsonl::for_each_record(&run_dir.join(JOURNAL), |view| {
        let Some(fields) = crate::harness::store::document::Fields::parse(view, &["type", "agentId", "result"])? else {
            return Ok(());
        };
        if fields.string("type")?.as_deref() == Some("result") {
            if let (Some(id), Some(result)) = (fields.string("agentId")?, fields.get("result")) {
                map.insert(id, result.resolve()?.into_owned());
            }
        }
        Ok(())
    });
    if read.is_err() {
        map.clear();
    }
    map
}

/// The `(status, summary)` a journaled result carries. Workflows write two shapes: a structured
/// object `{status, summary, …}` (status vocabularies are per-workflow: `done`/`partial`/`GREEN`/
/// `proven`/`blocked`/…; with no `summary` key the object's compact JSON stands in so the outcome
/// is not lost) or a plain string (the agent's whole freeform return, with no separate status).
pub fn result_summary(result: &Value) -> (Option<String>, Option<String>) {
    match result {
        Value::Object(obj) => {
            let status = obj.get("status").and_then(Value::as_str).map(str::to_string);
            let summary = obj
                .get("summary")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| Some(result.to_string()));
            (status, summary)
        }
        Value::String(s) => (None, Some(s.clone())),
        _ => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    #[test]
    fn paths_hang_off_the_transcript_stem() {
        let t = Path::new("/p/enc/abc.jsonl");
        assert_eq!(session_dir(t), Some(PathBuf::from("/p/enc/abc")));
        assert_eq!(subagents_dir(t), Some(PathBuf::from("/p/enc/abc/subagents")));
        assert_eq!(
            workflow_run_dir(t, "wf_1"),
            Some(PathBuf::from("/p/enc/abc/subagents/workflows/wf_1"))
        );
        assert_eq!(workflows_dir(t), Some(PathBuf::from("/p/enc/abc/workflows")));
    }

    /// Object and string results, a started-only agent, a re-issued agent (last result wins) and
    /// one corrupt line.
    #[test]
    fn journal_keeps_the_last_result_per_agent_and_reads_both_shapes() {
        let dir = std::env::temp_dir().join(format!("cv-sidecar-journal-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let mut f = fs::File::create(dir.join(JOURNAL)).unwrap();
        writeln!(f, r#"{{"type":"started","key":"k1","agentId":"aaa"}}"#).unwrap();
        writeln!(
            f,
            r#"{{"type":"result","key":"k1","agentId":"aaa","result":{{"status":"RED","summary":"first try"}}}}"#
        )
        .unwrap();
        writeln!(f, "{{not json").unwrap();
        writeln!(
            f,
            r#"{{"type":"result","key":"k1","agentId":"aaa","result":{{"status":"done","summary":"closed the lane"}}}}"#
        )
        .unwrap();
        writeln!(f, r#"{{"type":"started","key":"k2","agentId":"bbb"}}"#).unwrap();
        writeln!(
            f,
            r#"{{"type":"result","key":"k3","agentId":"ccc","result":"freeform return text"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"result","key":"k4","agentId":"ddd","result":{{"try":2}}}}"#
        )
        .unwrap();
        drop(f);

        let map = journal_results(&dir);
        assert_eq!(map["aaa"]["status"], "done", "the last result wins");
        assert!(!map.contains_key("bbb"), "a started-only agent has no result");
        assert_eq!(
            result_summary(&map["aaa"]),
            (Some("done".into()), Some("closed the lane".into()))
        );
        assert_eq!(result_summary(&map["ccc"]), (None, Some("freeform return text".into())));
        assert_eq!(
            result_summary(&map["ddd"]),
            (None, Some(r#"{"try":2}"#.into())),
            "no summary key: the compact object stands in"
        );
        assert!(journal_results(&dir.join("absent")).is_empty());
        fs::remove_dir_all(&dir).ok();
    }
}
