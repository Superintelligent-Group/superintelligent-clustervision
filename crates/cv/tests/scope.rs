//! Real CLI coverage of scoped discovery, cached lookup, and pre-existing indexes.

use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct World(PathBuf);

impl World {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!(
            "cv-scope-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let sessions = base.join("home/.codex/sessions");
        fs::create_dir_all(&sessions).unwrap();
        for (id, cwd) in [
            ("inside", Some("/work/proj/src")),
            ("outside", Some("/work/proj-other")),
            ("unknown", None),
        ] {
            let mut payload = json!({"id": id});
            if let Some(cwd) = cwd {
                payload["cwd"] = json!(cwd);
            }
            let lines = [
                json!({"type": "session_meta", "timestamp": "2026-01-01T00:00:00Z", "payload": payload}),
                json!({"type": "response_item", "payload": {"type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": "scopefixture needle"}]}}),
            ];
            fs::write(
                sessions.join(format!("rollout-{id}.jsonl")),
                lines.iter().map(|v| format!("{v}\n")).collect::<String>(),
            )
            .unwrap();
        }
        Self(base)
    }

    fn cv(&self, args: &[&str], roots: Option<&str>) -> Output {
        let home = self.0.join("home");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cv"));
        cmd.args(args)
            .current_dir(&self.0)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("CODEX_HOME", home.join(".codex"))
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("CLUSTERVISION_HOME", self.0.join("state"))
            .env_remove("CV_ALLOW_ROOTS");
        if let Some(roots) = roots {
            cmd.env("CV_ALLOW_ROOTS", roots);
        }
        cmd.output().unwrap()
    }

    fn output(&self, args: &[&str], roots: Option<&str>) -> String {
        let out = self.cv(args, roots);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn scope_hides_sessions_from_live_cached_and_indexed_reads() {
    let w = World::new();
    let all = w.output(&["ls", "--json", "--fresh", "--harness", "codex"], None);
    for id in ["inside", "outside", "unknown"] {
        assert!(all.contains(id), "{all}");
    }
    // Build unrestricted state before narrowing scope: neither cache may leak old rows.
    w.output(&["index"], None);
    for args in [
        vec!["ls", "--json", "--harness", "codex"],
        vec!["ls", "--json", "--fresh", "--harness", "codex"],
        vec!["search", "scopefixture", "--json"],
    ] {
        let visible = w.output(&args, Some("/work/proj"));
        assert!(visible.contains("inside"), "{visible}");
        assert!(!visible.contains("outside"), "{visible}");
        assert!(!visible.contains("unknown"), "{visible}");
    }
    for id in ["outside", "unknown"] {
        let out = w.cv(&["show", id, "--harness", "codex", "--json"], Some("/work/proj"));
        assert!(!out.status.success(), "{id} must be invisible");
        assert!(!String::from_utf8_lossy(&out.stdout).contains("scopefixture"));
    }
    assert!(w.cv(&["show", "inside", "--json"], Some("/work/proj")).status.success());
    // CLI overrides the environment and works after the subcommand.
    let all = w.output(&["ls", "--json", "--scope", "/work/proj-other"], Some("/work/proj"));
    assert!(all.contains("outside"), "{all}");
    assert!(!all.contains("inside"), "{all}");
}

#[test]
fn config_and_environment_scope_live_search() {
    let w = World::new();
    let config = w.0.join("home/.config/clustervision");
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("config.toml"), "allow_roots = ['/work/proj']\n").unwrap();
    let hits = w.output(&["search", "scopefixture", "--json"], None);
    assert!(hits.contains("inside"), "{hits}");
    assert!(!hits.contains("outside"), "{hits}");
    assert!(!hits.contains("unknown"), "{hits}");
    let hits = w.output(&["search", "scopefixture", "--json"], Some("/work/proj-other"));
    assert!(hits.contains("outside"), "{hits}");
    assert!(!hits.contains("inside"), "{hits}");
    let all = w.output(&["ls", "--json"], Some(""));
    assert!(all.contains("unknown"), "{all}");
}
