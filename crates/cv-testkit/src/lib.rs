//! The integration tests' one hermetic world. Every `crates/*/tests/*.rs` file that needs a fake
//! `$HOME` builds it here, so the rules live in one place:
//!
//! - a temp `base/` holding `home/` (with `.claude/projects/-work-proj`) and `cvhome/`, removed on
//!   drop;
//! - the real binary, spawned with `HOME`, `CLUSTERVISION_HOME` and the XDG dirs pointing into it,
//!   and every variable in [`CLEARED_ENV`] removed, so an agent seat's own identity, Claude config
//!   dir or server token never leaks into a test;
//! - error-path honesty: [`World::cv_fails`] asserts a nonzero exit, a non-empty stderr and no
//!   panic.
//!
//! The env is passed only to the child, so tests run in parallel. The two cv-core files that
//! drive the library in-process take [`World::in_process`], which holds a mutex for the test's
//! whole body and sets the same variables on the process.
//!
//! A dev-dependency only: the dependency fence scans `[dependencies]`, and `cargo publish` strips
//! a path-only dev-dependency.

pub mod heap;

use serde_json::Value;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};

/// Ambient variables no test may inherit: identity (`CV_ENDPOINT`), extra Claude roots
/// (`CLAUDE_CONFIG_DIR`, `CLUSTERVISION_CLAUDE_ROOTS`), the catalog's staleness override and
/// Cursor's dir, the pack LLM switch (tests exercise the zero-network digest), and the server
/// tokens (a token in the shell must not gate a test's requests).
pub const CLEARED_ENV: &[&str] = &[
    "CV_ENDPOINT",
    "CLAUDE_CONFIG_DIR",
    "CLUSTERVISION_CLAUDE_ROOTS",
    "CLUSTERVISION_MAX_STALE_SECS",
    "CURSOR_USER_DIR",
    "CV_PACK_LLM",
    "CV_SERVE_TOKEN",
    "CVD_TOKEN",
];

/// `(stdout, stderr)` of a run that succeeded or failed as asserted.
pub type Output = (String, String);

/// The `cv` binary cargo built for this test run: integration tests live in `target/<profile>/deps/`,
/// the binary one directory up.
pub fn cv_bin() -> PathBuf {
    let exe = std::env::current_exe().expect("test executable path");
    let dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile>/deps/<test>");
    let bin = dir.join(format!("cv{}", std::env::consts::EXE_SUFFIX));
    assert!(
        bin.is_file(),
        "no cv binary at {} (is this a crates/cv test?)",
        bin.display()
    );
    bin
}

/// A fresh, unique directory under the system temp dir.
pub fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("cv-test-{tag}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Lines as a JSONL body, one per line, newline-terminated.
pub fn jsonl(lines: &[Value]) -> String {
    lines.iter().map(|l| format!("{l}\n")).collect()
}

static PROCESS_ENV: Mutex<()> = Mutex::new(());

/// One temp world: a fake `$HOME` with a Claude project dir, and a `$CLUSTERVISION_HOME`.
pub struct World {
    pub base: PathBuf,
    pub home: PathBuf,
    pub cv_home: PathBuf,
    /// `home/.claude/projects/-work-proj`, where [`World::write_session`] writes.
    pub proj: PathBuf,
    guard: Option<MutexGuard<'static, ()>>,
}

impl World {
    pub fn new(tag: &str) -> World {
        let base = temp_dir(tag);
        let home = base.join("home");
        let cv_home = base.join("cvhome");
        let proj = home.join(".claude/projects/-work-proj");
        fs::create_dir_all(&proj).unwrap();
        fs::create_dir_all(&cv_home).unwrap();
        World {
            base,
            home,
            cv_home,
            proj,
            guard: None,
        }
    }

    /// A world for tests that call the library in-process: holds a process-wide lock until drop
    /// and sets [`World::env`] on the process, with [`CLEARED_ENV`] removed.
    pub fn in_process(tag: &str) -> World {
        let guard = PROCESS_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let mut w = World::new(tag);
        for (k, v) in w.env() {
            std::env::set_var(k, v);
        }
        for k in CLEARED_ENV {
            std::env::remove_var(k);
        }
        w.guard = Some(guard);
        w
    }

    /// The variables that make a process see this world.
    pub fn env(&self) -> [(&'static str, PathBuf); 5] {
        [
            ("HOME", self.home.clone()),
            ("CLUSTERVISION_HOME", self.cv_home.clone()),
            // Linux fallbacks for dirs::cache_dir / config_dir / data_dir; harmless on macOS.
            ("XDG_CACHE_HOME", self.home.join(".cache")),
            ("XDG_CONFIG_HOME", self.home.join(".config")),
            ("XDG_DATA_HOME", self.home.join(".local/share")),
        ]
    }

    /// `program` in this world: cwd `base`, [`World::env`] set, [`CLEARED_ENV`] removed.
    pub fn command(&self, program: impl AsRef<OsStr>) -> Command {
        let mut cmd = Command::new(program);
        cmd.current_dir(&self.base);
        for k in CLEARED_ENV {
            cmd.env_remove(k);
        }
        cmd.envs(self.env());
        cmd
    }

    /// `cv` in this world (see [`World::command`]).
    pub fn cv_command(&self) -> Command {
        self.command(cv_bin())
    }

    /// Run `cv`; returns (success, exit code or -1 on a signal, stdout, stderr).
    pub fn cv(&self, args: &[&str]) -> (bool, i32, String, String) {
        self.cv_full(args, &[], None)
    }

    /// [`World::cv`] with extra env vars, set after the clears.
    pub fn cv_env(&self, args: &[&str], env: &[(&str, &str)]) -> (bool, i32, String, String) {
        self.cv_full(args, env, None)
    }

    /// [`World::cv`] with `stdin` fed to the process.
    pub fn cv_stdin(&self, args: &[&str], stdin: &str) -> (bool, i32, String, String) {
        self.cv_full(args, &[], Some(stdin))
    }

    pub fn cv_full(&self, args: &[&str], env: &[(&str, &str)], stdin: Option<&str>) -> (bool, i32, String, String) {
        let mut cmd = self.cv_command();
        cmd.args(args).envs(env.iter().copied());
        let out = match stdin {
            None => cmd.stdin(Stdio::null()).output().expect("cv should run"),
            Some(text) => {
                cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
                let mut child = cmd.spawn().expect("cv should spawn");
                child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
                child.wait_with_output().expect("cv should run")
            }
        };
        (
            out.status.success(),
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// Run `cv`, asserting success.
    pub fn cv_ok(&self, args: &[&str]) -> Output {
        let (ok, code, out, err) = self.cv(args);
        assert!(ok, "cv {args:?} exited {code}\nstdout:\n{out}\nstderr:\n{err}");
        (out, err)
    }

    /// Run `cv`, asserting it fails the honest way: nonzero exit, not a signal, an explanation on
    /// stderr, and no panic.
    pub fn cv_fails(&self, args: &[&str]) -> Output {
        let (ok, code, out, err) = self.cv(args);
        assert!(!ok, "cv {args:?} unexpectedly succeeded\nstdout:\n{out}");
        assert_ne!(code, -1, "cv {args:?} died by signal (panic/abort?)\nstderr:\n{err}");
        assert!(
            !err.trim().is_empty(),
            "cv {args:?} failed silently (empty stderr)\nstdout:\n{out}"
        );
        assert!(
            !err.contains("panicked"),
            "cv {args:?} panicked instead of erroring:\n{err}"
        );
        (out, err)
    }

    /// `proj/<sid>.jsonl`, the Claude transcript of session `sid`.
    pub fn transcript(&self, sid: &str) -> PathBuf {
        self.proj.join(format!("{sid}.jsonl"))
    }

    /// Write a Claude session under `proj`; `sid` becomes the session id.
    pub fn write_session(&self, sid: &str, lines: &[Value]) -> PathBuf {
        let path = self.transcript(sid);
        fs::write(&path, jsonl(lines)).unwrap();
        path
    }

    /// A directly spawned sub-agent of `sid`: `<sid>/subagents/agent-<id>.jsonl` and its meta.
    pub fn write_agent(&self, sid: &str, agent_id: &str, meta: &Value, lines: &[Value]) -> PathBuf {
        let dir = self.proj.join(sid).join("subagents");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("agent-{agent_id}.jsonl"));
        fs::write(&path, jsonl(lines)).unwrap();
        fs::write(dir.join(format!("agent-{agent_id}.meta.json")), meta.to_string()).unwrap();
        path
    }

    /// Replace the task log with `lines` after its header (the one way to plant an OLD event:
    /// the CLI stamps `now`).
    pub fn seed_task_log(&self, lines: &[Value]) {
        let dir = self.cv_home.join("tasks");
        fs::create_dir_all(&dir).unwrap();
        let body = format!("{{\"format\":\"cv-task-log\",\"v\":1}}\n{}", jsonl(lines));
        fs::write(dir.join("events.jsonl"), body).unwrap();
    }

    /// Append raw events to the task log, after whatever the CLI wrote: how a test plants a shape
    /// the CLI no longer writes.
    pub fn append_task_log(&self, lines: &[Value]) {
        let dir = self.cv_home.join("tasks");
        fs::create_dir_all(&dir).unwrap();
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("events.jsonl"))
            .unwrap();
        f.write_all(jsonl(lines).as_bytes()).unwrap();
    }
}

impl Drop for World {
    fn drop(&mut self) {
        if self.guard.is_some() {
            for k in CLEARED_ENV {
                std::env::remove_var(k);
            }
        }
        fs::remove_dir_all(&self.base).ok();
    }
}

/// Claude Code transcript records, in the shapes the real harness writes. Every record carries
/// `sessionId: "s"`; [`with`] adds or overrides fields.
pub mod claude {
    use serde_json::{json, Value};

    /// A user prompt, with `cwd` (the field `cv ls` reads the project from).
    pub fn user(uuid: &str, ts: &str, text: &str) -> Value {
        json!({
            "type": "user", "uuid": uuid, "sessionId": "s", "timestamp": ts, "cwd": "/work/proj",
            "message": {"role": "user", "content": text}
        })
    }

    /// An assistant turn of `model` with these content blocks.
    pub fn assistant(uuid: &str, ts: &str, model: &str, blocks: Value) -> Value {
        json!({
            "type": "assistant", "uuid": uuid, "sessionId": "s", "timestamp": ts,
            "message": {"role": "assistant", "model": model, "content": blocks}
        })
    }

    /// An assistant turn as Claude Code writes it for a real model: an API message id
    /// `msg_<uuid>` and a `usage` object.
    pub fn reply(uuid: &str, ts: &str, blocks: Value, usage: Value) -> Value {
        json!({
            "type": "assistant", "uuid": uuid, "sessionId": "s", "timestamp": ts,
            "message": {"role": "assistant", "model": "claude-opus-4-1-20250805", "id": format!("msg_{uuid}"),
                "content": blocks, "usage": usage}
        })
    }

    /// A `tool_use` block.
    pub fn tool_use(id: &str, name: &str, input: Value) -> Value {
        json!({"type": "tool_use", "id": id, "name": name, "input": input})
    }

    /// The user record carrying a tool's result.
    pub fn tool_result(uuid: &str, ts: &str, tool_use_id: &str, content: &str) -> Value {
        json!({
            "type": "user", "uuid": uuid, "sessionId": "s", "timestamp": ts,
            "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tool_use_id, "content": content}]}
        })
    }

    /// A compaction boundary.
    pub fn boundary(uuid: &str, ts: &str, trigger: &str, pre_tokens: u64) -> Value {
        json!({
            "type": "system", "subtype": "compact_boundary", "uuid": uuid, "sessionId": "s", "timestamp": ts,
            "content": "Conversation compacted",
            "compactMetadata": {"trigger": trigger, "preTokens": pre_tokens, "durationMs": 1000}
        })
    }

    /// The compaction summary that seeds the next window.
    pub fn compact_summary(uuid: &str, ts: &str, body: &str) -> Value {
        json!({
            "type": "user", "uuid": uuid, "isCompactSummary": true, "sessionId": "s", "timestamp": ts,
            "message": {"role": "user", "content": body}
        })
    }

    /// The `SubagentStop` hook attachment the harness writes when a child returns.
    pub fn subagent_stop(uuid: &str, ts: &str) -> Value {
        json!({
            "type": "attachment", "uuid": uuid, "sessionId": "s", "timestamp": ts,
            "attachment": {"type": "hook_success", "hookName": "SubagentStop", "hookEvent": "SubagentStop", "content": ""}
        })
    }

    /// The `queue-operation` task notification for a background child.
    pub fn notification(ts: &str, agent_id: &str, status: &str) -> Value {
        json!({
            "type": "queue-operation", "operation": "enqueue", "timestamp": ts, "sessionId": "s",
            "content": format!("<task-notification>\n<task-id>{agent_id}</task-id>\n<status>{status}</status>\n<summary>done</summary>\n</task-notification>")
        })
    }

    /// `base` with `extra`'s fields merged in, recursively for objects.
    pub fn with(mut base: Value, extra: Value) -> Value {
        merge(&mut base, extra);
        base
    }

    fn merge(base: &mut Value, extra: Value) {
        match (base, extra) {
            (Value::Object(b), Value::Object(e)) => {
                for (k, v) in e {
                    match b.get_mut(&k) {
                        Some(slot) if slot.is_object() && v.is_object() => merge(slot, v),
                        _ => {
                            b.insert(k, v);
                        }
                    }
                }
            }
            (b, e) => *b = e,
        }
    }
}
