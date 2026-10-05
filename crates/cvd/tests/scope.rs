use std::fs;
use std::process::Command;

#[test]
fn archived_listing_respects_project_scope() {
    let home = std::env::temp_dir().join(format!(
        "cvd-scope-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&home).unwrap();
    let rows: String = [
        ("inside", Some("/work/proj")),
        ("outside", Some("/elsewhere")),
        ("unknown", None),
    ]
    .iter()
    .map(|(id, cwd)| {
        format!(
            "{}\n",
            serde_json::json!({
                "harness": "codex", "id": id, "cwd": cwd, "message_count": 1,
                "archived_at": "2026-01-01T00:00:00Z", "path": format!("archive/codex/{id}.json")
            })
        )
    })
    .collect();
    fs::write(home.join("catalog.jsonl"), rows).unwrap();
    let run = |scope: Option<&str>| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cvd"));
        cmd.args(["--home", home.to_str().unwrap(), "ls"])
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env_remove("CV_ALLOW_ROOTS");
        if let Some(scope) = scope {
            cmd.env("CV_ALLOW_ROOTS", scope);
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    let all = run(None);
    assert!(all.contains("outside") && all.contains("unknown"), "{all}");
    let scoped = run(Some("/work/proj"));
    assert!(scoped.contains("inside"), "{scoped}");
    assert!(!scoped.contains("outside") && !scoped.contains("unknown"), "{scoped}");
    fs::remove_dir_all(home).unwrap();
}
