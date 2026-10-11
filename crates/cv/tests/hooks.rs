//! The cybernetic layer end to end: `cv hooks install/show/remove` against a fake `$HOME`'s
//! Claude Code settings (idempotent, reversible, leaves other hooks alone), and each `cv hook
//! <name>` driven with the payload shape the Claude Code 2.1.296 hooks docs give for its event.
//! Hermetic through `cv_testkit::World`: own `$HOME` + `$CLUSTERVISION_HOME`, the real binary.

use cv_testkit::claude::{self, compact_summary, tool_use, user, with};
use cv_testkit::World;
use serde_json::json;
use std::fs;
use std::path::PathBuf;

fn write_agent(w: &World, sid: &str, agent_id: &str, lines: &[serde_json::Value]) {
    let meta = json!({"agentType": "general-purpose", "description": "LANE", "toolUseId": "toolu_1"});
    w.write_agent(sid, agent_id, &meta, lines);
}

fn cv_ok(w: &World, args: &[&str]) -> String {
    w.cv_ok(args).0
}

/// `cv hook <name>` fed one payload; the hook must exit 0 whatever happens.
fn hook(w: &World, name: &str, payload: &serde_json::Value, env: &[(&str, &str)]) -> String {
    let (ok, code, out, err) = w.cv_full(&["hook", name], env, Some(&payload.to_string()));
    assert!(ok, "cv hook {name} exited {code}\nstdout:\n{out}\nstderr:\n{err}");
    out
}

fn settings_path(w: &World) -> PathBuf {
    w.home.join(".claude/settings.json")
}

fn settings(w: &World) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(settings_path(w)).unwrap()).unwrap()
}

const CWD: &str = "/work/proj";

fn assistant(uuid: &str, ts: &str, text: &str) -> serde_json::Value {
    with(
        claude::reply(
            uuid,
            ts,
            json!([{"type": "text", "text": text}]),
            json!({"input_tokens": 100, "output_tokens": 50}),
        ),
        json!({"cwd": CWD}),
    )
}

fn assistant_tool(uuid: &str, ts: &str, name: &str, input: serde_json::Value) -> serde_json::Value {
    with(
        claude::reply(
            uuid,
            ts,
            json!([tool_use(&format!("toolu_{uuid}"), name, input)]),
            json!({"input_tokens": 10, "output_tokens": 5}),
        ),
        json!({"cwd": CWD}),
    )
}

fn tool_result(uuid: &str, ts: &str, tool_use_id: &str, content: &str) -> serde_json::Value {
    with(claude::tool_result(uuid, ts, tool_use_id, content), json!({"cwd": CWD}))
}

fn boundary(uuid: &str, ts: &str) -> serde_json::Value {
    claude::boundary(uuid, ts, "auto", 180_412)
}

/// The common fields every event carries (the docs' "common input fields").
fn common(w: &World, sid: &str, event: &str) -> serde_json::Value {
    serde_json::json!({
        "session_id": sid,
        "transcript_path": w.transcript(sid),
        "cwd": "/work/proj",
        "permission_mode": "default",
        "hook_event_name": event,
    })
}

fn context_of(out: &str) -> String {
    let v: serde_json::Value =
        serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("hook stdout is not JSON ({e}):\n{out}"));
    v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_else(|| panic!("no additionalContext in {out}"))
        .to_string()
}

#[test]
fn install_is_idempotent_show_reports_and_remove_restores_the_file() {
    let w = World::new("install");
    fs::create_dir_all(w.home.join(".claude")).unwrap();
    let theirs = serde_json::json!({
        "theme": "dark",
        "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/usr/bin/notify"}]}],
                  "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "guard.sh"}]}]}
    });
    fs::write(settings_path(&w), serde_json::to_string_pretty(&theirs).unwrap()).unwrap();

    let out = cv_ok(&w, &["hooks", "install", "claude"]);
    assert!(out.contains("installed 6 entries"), "{out}");
    let once = fs::read_to_string(settings_path(&w)).unwrap();
    let s = settings(&w);
    assert_eq!(s["theme"], "dark");
    assert_eq!(
        s["hooks"]["Stop"][0]["hooks"][0]["command"], "/usr/bin/notify",
        "theirs survives"
    );
    assert_eq!(s["hooks"]["PreToolUse"][0]["matcher"], "Bash", "theirs stays first");
    assert_eq!(s["hooks"]["PreToolUse"][1]["matcher"], "Edit|Write");
    let cmd = s["hooks"]["SessionStart"][0]["hooks"][0]["command"].as_str().unwrap();
    assert!(cmd.ends_with("/cv hook compact"), "absolute binary path: {cmd}");
    assert_eq!(s["hooks"]["SessionStart"][1]["matcher"], "startup|resume");
    assert_eq!(s["hooks"]["SubagentStart"][0]["hooks"][0]["async"], true);
    assert_eq!(s["hooks"]["SubagentStop"][0]["hooks"][0]["async"], true);
    assert!(s["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .ends_with(" hook decisions"));
    let skill = w.home.join(".claude/skills/cv/SKILL.md");
    assert!(
        fs::read_to_string(&skill).unwrap().starts_with("---\nname: cv\n"),
        "skill copied"
    );

    // Idempotent: the second run writes nothing and the bytes are identical.
    let out = cv_ok(&w, &["hooks", "install", "claude"]);
    assert!(out.contains("already installed"), "{out}");
    assert_eq!(fs::read_to_string(settings_path(&w)).unwrap(), once);

    let out = cv_ok(&w, &["hooks", "show", "claude", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["installed"].as_array().unwrap().len(), 6);
    assert_eq!(v["missing"].as_array().unwrap().len(), 0);
    assert_eq!(v["would_change"], false);
    assert_eq!(v["skill_present"], true);
    let out = cv_ok(&w, &["hooks", "show", "claude"]);
    assert!(
        out.contains("✓ SessionStart") && out.contains("would write nothing"),
        "{out}"
    );

    // Remove: exactly ours go; the file equals what was there before.
    let out = cv_ok(&w, &["hooks", "remove", "claude"]);
    assert!(out.contains("removed 6 entries"), "{out}");
    assert_eq!(settings(&w), theirs);
    assert!(!skill.exists(), "skill removed");
    let out = cv_ok(&w, &["hooks", "remove", "claude"]);
    assert!(out.contains("nothing written"), "{out}");

    // Project scope writes under the cwd, and only Claude is a known harness.
    let out = cv_ok(&w, &["hooks", "install", "claude", "--scope", "project"]);
    assert!(out.contains("installed"), "{out}");
    assert!(w.base.join(".claude/settings.json").exists());
    assert!(w.base.join(".claude/skills/cv/SKILL.md").exists());
    let (ok, code, _, err) = w.cv(&["hooks", "install", "codex"]);
    assert!(!ok && code == 1, "unknown harness errors");
    assert!(err.contains("Claude Code only"), "{err}");

    // doctor says whether the hooks are installed (project scope here) and the policy in force.
    w.write_session(
        "docsess",
        &[
            user("u0", "2026-10-01T10:00:00Z", "hi"),
            assistant("a0", "2026-10-01T10:00:05Z", "hello"),
        ],
    );
    let (ok, _, out, err) = w.cv_env(&["doctor", "docsess"], &[("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE", "70")]);
    assert!(ok, "{err}");
    assert!(out.contains("Hooks:       6/6 in"), "{out}");
    assert!(out.contains("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=70"), "{out}");
    assert!(out.contains("autoCompactEnabled unset (default on)"), "{out}");
}

#[test]
fn each_hook_answers_the_docs_payload_for_its_event() {
    let w = World::new("bodies");
    // alphasess: edited a file, made a commit, has a title-worthy first prompt.
    w.write_session(
        "alphasess",
        &[
            serde_json::json!({"type": "ai-title", "aiTitle": "fix the widget gate", "sessionId": "s"}),
            user("u0", "2026-10-01T10:00:00Z", "fix the widget gate"),
            assistant_tool(
                "a0",
                "2026-10-01T10:00:05Z",
                "Edit",
                serde_json::json!({"file_path": "/work/proj/src/widget.rs", "old_string": "a", "new_string": "b"}),
            ),
            tool_result("r0", "2026-10-01T10:00:06Z", "toolu_a0", "edited ok"),
            assistant_tool(
                "a1",
                "2026-10-01T10:00:07Z",
                "Bash",
                serde_json::json!({"command": "git commit -am 'gate fix'"}),
            ),
            tool_result(
                "r1",
                "2026-10-01T10:00:08Z",
                "toolu_a1",
                "[main 63cef47] gate fix\n 1 file changed",
            ),
            assistant("a2", "2026-10-01T10:00:09Z", "landed."),
        ],
    );
    // hooksess: the session the hooks run in — compacted once, one lane still running.
    w.write_session(
        "hooksess",
        &[
            user("u0", "2026-10-01T11:00:00Z", "start the swarm"),
            assistant("a0", "2026-10-01T11:00:05Z", "starting"),
            boundary("b1", "2026-10-01T11:30:00Z"),
            compact_summary("s1", "2026-10-01T11:30:01Z", "SUMMARY: the early context"),
            user("u1", "2026-10-01T11:31:00Z", "carry on"),
        ],
    );
    write_agent(
        &w,
        "hooksess",
        "aaa1",
        &[
            user("x0", "2026-10-01T11:00:10Z", "You are lane A."),
            assistant_tool(
                "x1",
                "2026-10-01T11:00:20Z",
                "Bash",
                serde_json::json!({"command": "cargo build"}),
            ),
            tool_result("x2", "2026-10-01T11:01:00Z", "toolu_x1", "ok"),
            // Ends mid tool call: `running` until something records a stop.
            assistant_tool(
                "x3",
                "2026-10-01T11:02:00Z",
                "Bash",
                serde_json::json!({"command": "cargo test"}),
            ),
        ],
    );
    w.write_session("elsewhere", &[
        serde_json::json!({"type": "user", "uuid": "e0", "sessionId": "s", "timestamp": "2026-10-01T12:00:00Z", "cwd": "/other",
            "message": {"role": "user", "content": "unrelated"}}),
    ]);
    // The catalog and the edit index are what the hooks read; populate them as a user would.
    cv_ok(&w, &["ls"]);
    cv_ok(&w, &["events", "alphasess"]);

    // ── compact: SessionStart with source=compact ──
    let out = hook(
        &w,
        "compact",
        &with(
            common(&w, "hooksess", "SessionStart"),
            serde_json::json!({"source": "compact"}),
        ),
        &[],
    );
    let ctx = context_of(&out);
    assert!(
        ctx.contains("context compacted (auto, 180,412 tokens) at message 2 — compaction #1"),
        "{ctx}"
    );
    assert!(ctx.contains("`cv show hooksess --pre-compaction 1`"), "{ctx}");
    assert!(ctx.contains("`cv rewind hooksess --at 1`"), "{ctx}");
    // A session whose transcript has no boundary yet: honest, still a pointer.
    let out = hook(
        &w,
        "compact",
        &with(
            common(&w, "alphasess", "SessionStart"),
            serde_json::json!({"source": "compact"}),
        ),
        &[],
    );
    assert!(context_of(&out).contains("has not recorded the boundary yet"), "{out}");

    // ── start: SessionStart with source=startup, from hooksess, in /work/proj ──
    let out = hook(
        &w,
        "start",
        &with(
            common(&w, "hooksess", "SessionStart"),
            serde_json::json!({"source": "startup"}),
        ),
        &[],
    );
    let ctx = context_of(&out);
    assert!(ctx.starts_with("cv: 1 earlier session(s) in this directory"), "{ctx}");
    assert!(
        ctx.contains("- alphases · ") && ctx.contains("fix the widget gate") && ctx.contains("last commit 63cef47"),
        "{ctx}"
    );
    assert!(!ctx.contains("hooksess"), "the running session is not listed:\n{ctx}");
    assert!(
        !ctx.contains("elsewher"),
        "another directory's session is not listed:\n{ctx}"
    );
    assert!(ctx.chars().count() <= 4000);
    // Nothing to say, nothing printed.
    let out = hook(
        &w,
        "start",
        &with(
            common(&w, "x", "SessionStart"),
            serde_json::json!({"source": "startup", "cwd": "/nowhere"}),
        ),
        &[],
    );
    assert_eq!(out.trim(), "");

    // ── lane: SubagentStart then SubagentStop (the docs' shapes: agent-prefixed id on start, bare on stop) ──
    let out = hook(
        &w,
        "lane",
        &with(
            common(&w, "hooksess", "SubagentStart"),
            serde_json::json!({"agent_id": "agent-aaa1", "agent_type": "general-purpose"}),
        ),
        &[],
    );
    assert_eq!(out.trim(), "", "async hook prints nothing");
    let before = cv_ok(&w, &["lanes", "hooksess", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&before).unwrap();
    assert_eq!(v[0]["status"], "running", "{before}");
    let out = hook(
        &w,
        "lane",
        &with(
            common(&w, "hooksess", "SubagentStop"),
            serde_json::json!({
        "stop_hook_active": false, "agent_id": "aaa1", "agent_type": "general-purpose",
        "agent_transcript_path": w.proj.join("hooksess/subagents/agent-aaa1.jsonl"),
        "last_assistant_message": "Lane A is done.\nmore", "background_tasks": [], "session_crons": []}),
        ),
        &[],
    );
    assert_eq!(out.trim(), "");
    let ledger = fs::read_to_string(w.cv_home.join("lanes.jsonl")).unwrap();
    let rows: Vec<serde_json::Value> = ledger.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["event"], "start");
    assert_eq!(rows[0]["agent_id"], "aaa1", "agent- prefix stripped");
    assert!(
        rows[0]["agent_transcript_path"]
            .as_str()
            .unwrap()
            .ends_with("subagents/agent-aaa1.jsonl"),
        "derived on start"
    );
    assert_eq!(rows[1]["event"], "stop");
    assert_eq!(rows[1]["last_text"], "Lane A is done.");
    let after = cv_ok(&w, &["lanes", "hooksess", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&after).unwrap();
    assert_eq!(v[0]["status"], "stopped", "{after}");
    assert_eq!(v[0]["status_source"], "hook_ledger", "{after}");

    // ── decisions: UserPromptSubmit — silent with nothing owed, one line otherwise ──
    let prompt = with(
        common(&w, "hooksess", "UserPromptSubmit"),
        serde_json::json!({"prompt": "hi"}),
    );
    assert_eq!(hook(&w, "decisions", &prompt, &[("CV_ENDPOINT", "lane:me")]).trim(), "");
    cv_ok(
        &w,
        &[
            "task",
            "decide",
            "which door",
            "--for",
            "lane:me",
            "--default",
            "left",
            "--option",
            "right",
        ],
    );
    let out = hook(&w, "decisions", &prompt, &[("CV_ENDPOINT", "lane:me")]);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
    assert!(
        context_of(&out).contains("1 decision(s) owed to lane:me — `cv task inbox`"),
        "{out}"
    );
    assert_eq!(
        hook(&w, "decisions", &prompt, &[("CV_ENDPOINT", "lane:other")]).trim(),
        "",
        "not owed to this endpoint"
    );

    // ── edit: PreToolUse Edit on a file a live session (alphasess, written seconds ago) edited ──
    let edit = |sid: &str, file: &str| {
        with(
            common(&w, sid, "PreToolUse"),
            serde_json::json!({
            "tool_name": "Edit", "tool_use_id": "toolu_01ABC123",
            "tool_input": {"file_path": file, "old_string": "a", "new_string": "b"}}),
        )
    };
    let out = hook(&w, "edit", &edit("hooksess", "/work/proj/src/widget.rs"), &[]);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "ask");
    let reason = v["hookSpecificOutput"]["permissionDecisionReason"].as_str().unwrap();
    assert!(
        reason.contains("edited by live session alphases (0 min ago)"),
        "{reason}"
    );
    assert!(reason.contains("`cv show alphases --last 10`"), "{reason}");
    // A relative path resolves against the hook's cwd.
    let out = hook(&w, "edit", &edit("hooksess", "src/widget.rs"), &[]);
    assert!(out.contains("\"ask\""), "{out}");
    // The editing session itself, and a file nobody edited: allow by silence.
    assert_eq!(
        hook(&w, "edit", &edit("alphasess", "/work/proj/src/widget.rs"), &[]).trim(),
        ""
    );
    assert_eq!(
        hook(&w, "edit", &edit("hooksess", "/work/proj/src/other.rs"), &[]).trim(),
        ""
    );

    // Garbage on stdin never fails the harness.
    let (ok, _, out, err) = w.cv_stdin(&["hook", "edit"], "not json");
    assert!(ok && out.is_empty(), "{out}");
    assert!(err.contains("not the hook JSON"), "{err}");
}
