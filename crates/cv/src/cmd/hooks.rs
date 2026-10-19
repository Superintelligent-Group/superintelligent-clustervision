//! The cybernetic layer: `cv hooks` installs five Claude Code or Codex hooks (idempotent, reversible) and
//! `cv hook <name>` is each hook's body — it reads the hook JSON on stdin and prints a line or two.
//!
//! Verified against the Claude Code 2.1.296 hooks docs: a `SessionStart` hook's
//! `hookSpecificOutput.additionalContext` is injected into the model's context (10K chars before it
//! spills to a file); `UserPromptSubmit` the same; `PreToolUse` answers
//! `permissionDecision: allow|deny|ask|defer`; `SubagentStart`/`SubagentStop` carry `agent_id`,
//! `agent_type` and (on stop) `agent_transcript_path` + `last_assistant_message`, and with
//! `async: true` nothing waits for them. Every hook is `cv hook <name>` with an absolute binary
//! path; the entries are recognized by that command shape, so `remove` deletes exactly them.
//!
//! Design constraint from the maintainer: agents set this up for themselves; not fancy; no daemon.
//! Every body reads the catalog or the index (never a fleet scan) and must finish well under 100 ms
//! on a cold cache — the latencies are measured in the lane report, not assumed.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Subcommand, ValueEnum};
use cv_core::ir::{home_rel, Harness, SessionRef};
use cv_core::lanes::LaneEvent;
use cv_core::sanitize::sanitize_line;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::short_id;
use crate::util::fmt_int;

/// The skill installed beside the hooks. It lives inside this crate (`crates/cv/skill/SKILL.md`) so the
/// published package carries it; a path outside the crate builds in the repo and nowhere else.
const SKILL: &str = include_str!("../../skill/SKILL.md");

/// A session whose transcript changed within this many minutes is *live* for the edit guard.
const LIVE_MINUTES: i64 = 30;
/// The start pack's hard cap (chars) — well under the harness's 10K inline limit.
const START_PACK_MAX: usize = 4_000;
/// How many of this directory's sessions the start pack lists.
const START_SESSIONS: usize = 6;
/// How far back from the end of a transcript the start pack looks for a `[branch sha]` line.
const COMMIT_TAIL_BYTES: u64 = 512 * 1024;
/// Above this size the compact hook reads the boundary from the transcript's tail instead of
/// streaming the whole file for its message index: the stream costs ~4.5 ms/MB (release, warm;
/// 16.5 MB measured at 70–90 ms), and the hook's budget is 100 ms cold. Below it the full
/// compaction detection runs.
const COMPACT_FULL_SCAN_MAX: u64 = 8 * 1024 * 1024;
/// The tail read on the large-transcript path: the boundary the harness just wrote sits a few
/// records from the end.
const COMPACT_TAIL_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Subcommand)]
pub(crate) enum HooksCmd {
    /// Merge cv's five hooks into the harness's settings file and copy the `cv` skill beside them.
    /// Idempotent: a second run writes nothing.
    Install {
        /// The harness (`claude` or `codex`).
        #[arg(default_value = "claude")]
        harness: String,
        /// `user` writes the harness home; `project` writes `.claude/settings.json` or `.codex/hooks.json`.
        #[arg(long, value_enum, default_value_t = Scope::User)]
        scope: Scope,
    },
    /// Delete exactly the entries `install` wrote (and the copied skill), nothing else.
    Remove {
        #[arg(default_value = "claude")]
        harness: String,
        #[arg(long, value_enum, default_value_t = Scope::User)]
        scope: Scope,
    },
    /// Print what `install` would change: each entry present / missing, the skill, the settings path.
    Show {
        #[arg(default_value = "claude")]
        harness: String,
        #[arg(long, value_enum, default_value_t = Scope::User)]
        scope: Scope,
        /// Emit `{settings_path, installed, missing, skill_path, skill_present, before, after}`.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Scope {
    User,
    Project,
}

/// The five hook bodies. The name is what the installed command ends with (`cv hook compact`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, ValueEnum)]
pub(crate) enum HookName {
    /// `SessionStart` matcher `compact`: what was compacted and the three commands that read it back.
    Compact,
    /// `SessionStart` matcher `startup|resume`: a capped pack of this directory's last sessions.
    Start,
    /// `SubagentStart` / `SubagentStop` (async): a row in the lane ledger `cv lanes` reads.
    Lane,
    /// `UserPromptSubmit`: one line when decisions are owed to you, else nothing.
    Decisions,
    /// `PreToolUse` matcher `Edit|Write`: `ask` when another live session edited the file.
    Edit,
}

impl HookName {
    fn as_str(self) -> &'static str {
        match self {
            HookName::Compact => "compact",
            HookName::Start => "start",
            HookName::Lane => "lane",
            HookName::Decisions => "decisions",
            HookName::Edit => "edit",
        }
    }
    fn parse(s: &str) -> Option<HookName> {
        [
            HookName::Compact,
            HookName::Start,
            HookName::Lane,
            HookName::Decisions,
            HookName::Edit,
        ]
        .into_iter()
        .find(|n| n.as_str() == s)
    }
}

/// One settings entry: the event it hangs under, its matcher, the body, and whether it runs async.
struct Spec {
    event: &'static str,
    matcher: Option<&'static str>,
    name: HookName,
    asynchronous: bool,
}

/// Five hooks, six entries (`lane` serves both sub-agent events).
const SPECS: &[Spec] = &[
    Spec {
        event: "SessionStart",
        matcher: Some("compact"),
        name: HookName::Compact,
        asynchronous: false,
    },
    Spec {
        event: "SessionStart",
        matcher: Some("startup|resume"),
        name: HookName::Start,
        asynchronous: false,
    },
    Spec {
        event: "SubagentStart",
        matcher: None,
        name: HookName::Lane,
        asynchronous: true,
    },
    Spec {
        event: "SubagentStop",
        matcher: None,
        name: HookName::Lane,
        asynchronous: true,
    },
    Spec {
        event: "UserPromptSubmit",
        matcher: None,
        name: HookName::Decisions,
        asynchronous: false,
    },
    Spec {
        event: "PreToolUse",
        matcher: Some("Edit|Write"),
        name: HookName::Edit,
        asynchronous: false,
    },
];

// Older released Codex builds do not expose the newer Claude-compatible matcher aliases.
const CODEX_EDIT: Spec = Spec {
    event: "PreToolUse",
    matcher: Some("apply_patch"),
    name: HookName::Edit,
    asynchronous: false,
};
fn specs(harness: Harness) -> impl Iterator<Item = &'static Spec> {
    SPECS.iter().map(move |spec| {
        if harness == Harness::Codex && spec.name == HookName::Edit {
            &CODEX_EDIT
        } else {
            spec
        }
    })
}

// ───────────────────────────── settings file ─────────────────────────────

/// Claude Code's config dir: `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
fn claude_config_dir() -> Option<PathBuf> {
    cv_core::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::util::dirs_home().map(|h| h.join(".claude")))
}

/// Codex's home: `$CODEX_HOME`, else `~/.codex`.
fn codex_home() -> Option<PathBuf> {
    cv_core::env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::util::dirs_home().map(|h| h.join(".codex")))
}

/// `(settings.json or hooks.json, skills/cv/SKILL.md)` for a harness and scope.
fn paths(harness: Harness, scope: Scope) -> Result<(PathBuf, PathBuf)> {
    let dir = match scope {
        Scope::User if harness == Harness::Claude => {
            claude_config_dir().context("no home directory: cannot locate ~/.claude")?
        }
        Scope::User => codex_home().context("no home directory: cannot locate ~/.codex")?,
        Scope::Project => {
            std::env::current_dir()
                .context("reading the current directory")?
                .join(if harness == Harness::Claude {
                    ".claude"
                } else {
                    ".codex"
                })
        }
    };
    Ok((
        dir.join(if harness == Harness::Claude {
            "settings.json"
        } else {
            "hooks.json"
        }),
        dir.join("skills").join("cv").join("SKILL.md"),
    ))
}

// ───────────────────────────── Codex trust ─────────────────────────────
//
// Codex runs a `hooks.json` handler only when its identity hash is recorded under
// `[hooks.state."<hooks.json>:<event>:<group>:<handler>"] trusted_hash` in `config.toml`
// (`codex-rs/hooks/src/engine/discovery.rs`: `hook_hash`, `hook_trust_status`). The TUI's startup
// hooks review writes that entry; `codex exec --dangerously-bypass-hook-trust` skips the check;
// nothing else runs an untrusted or modified handler, silently. The hash is sha256 over the
// key-sorted compact JSON of the normalized identity: the event's snake_case label, the group's
// matcher, and the one handler with Codex's defaults applied (`timeout` 600 when unset, `async`
// false, `statusMessage`/`additionalContextLimit` only when set and not the default). Checked
// against Codex 0.162.1's `hooks/list` for cv's six entries in every scope.

/// What Codex will do with one of cv's entries: run it, or skip it until reviewed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Trust {
    Trusted,
    Untrusted,
    /// Trusted once, then the entry changed (a moved binary, an edited matcher): skipped again.
    Modified,
}

impl Trust {
    fn as_str(self) -> &'static str {
        match self {
            Trust::Trusted => "trusted",
            Trust::Untrusted => "untrusted",
            Trust::Modified => "modified",
        }
    }
}

/// One of cv's entries as Codex identifies it: its state key, its current hash, and its trust.
struct TrustRow {
    name: HookName,
    key: String,
    hash: String,
    trust: Trust,
}

/// Codex's state key label for a hooks.json event (`hook_event_key_label`).
fn codex_event_label(event: &str) -> &'static str {
    match event {
        "PreToolUse" => "pre_tool_use",
        "SessionStart" => "session_start",
        "UserPromptSubmit" => "user_prompt_submit",
        "SubagentStart" => "subagent_start",
        "SubagentStop" => "subagent_stop",
        _ => "unknown",
    }
}

/// The hash Codex computes for a `type: command` handler under `event` with `matcher`.
fn codex_hook_hash(event: &str, matcher: Option<&str>, handler: &Value) -> String {
    use sha2::Digest;
    let mut hook = Map::new();
    hook.insert("type".into(), json!("command"));
    hook.insert("command".into(), handler.get("command").cloned().unwrap_or(Value::Null));
    hook.insert(
        "timeout".into(),
        json!(handler.get("timeout").and_then(Value::as_u64).unwrap_or(600).max(1)),
    );
    hook.insert(
        "async".into(),
        json!(handler.get("async").and_then(Value::as_bool).unwrap_or(false)),
    );
    if let Some(s) = handler.get("statusMessage").filter(|v| v.is_string()) {
        hook.insert("statusMessage".into(), s.clone());
    }
    if let Some(limit) = handler
        .get("additionalContextLimit")
        .and_then(Value::as_u64)
        .filter(|l| *l != 2500)
    {
        hook.insert("additionalContextLimit".into(), json!(limit));
    }
    let mut identity = Map::new();
    identity.insert("event_name".into(), json!(codex_event_label(event)));
    if let Some(m) = matcher {
        identity.insert("matcher".into(), json!(m));
    }
    identity.insert("hooks".into(), json!([Value::Object(hook)]));
    let mut identity = Value::Object(identity);
    identity.sort_all_objects();
    let digest = sha2::Sha256::digest(serde_json::to_vec(&identity).unwrap_or_default());
    format!("sha256:{digest:x}")
}

/// `[hooks.state]` of Codex's user `config.toml`: state key → `trusted_hash`. The TUI records
/// trust there for project hooks too.
fn codex_trust_store() -> HashMap<String, String> {
    let Some(text) = codex_home().and_then(|h| std::fs::read_to_string(h.join("config.toml")).ok()) else {
        return HashMap::new();
    };
    let Ok(config) = text.parse::<toml::Table>() else {
        return HashMap::new();
    };
    config
        .get("hooks")
        .and_then(|h| h.get("state"))
        .and_then(toml::Value::as_table)
        .into_iter()
        .flatten()
        .filter_map(|(key, state)| {
            let hash = state.get("trusted_hash")?.as_str()?;
            Some((key.clone(), hash.to_string()))
        })
        .collect()
}

/// cv's entries in `settings` (a Codex hooks.json at `path`), each with the trust Codex gives it.
fn codex_trust(path: &Path, settings: &Value) -> Vec<TrustRow> {
    let store = codex_trust_store();
    let Some(hooks) = settings.get("hooks").and_then(Value::as_object) else {
        return Vec::new();
    };
    // Codex keys by the config folder's canonical path (`/var/…` is `/private/var/…` on macOS).
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut rows = Vec::new();
    for (event, groups) in hooks {
        for (gi, group) in groups.as_array().into_iter().flatten().enumerate() {
            let matcher = group.get("matcher").and_then(Value::as_str);
            for (hi, handler) in group
                .get("hooks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                let Some(name) = ours(handler) else { continue };
                let key = format!("{}:{}:{gi}:{hi}", path.display(), codex_event_label(event));
                let hash = codex_hook_hash(event, matcher, handler);
                let trust = match store.get(&key) {
                    None => Trust::Untrusted,
                    Some(h) if *h == hash => Trust::Trusted,
                    Some(_) => Trust::Modified,
                };
                rows.push(TrustRow { name, key, hash, trust });
            }
        }
    }
    rows
}

/// One phrase for a scope's trust: `trusted`, or the counts and what to do about them.
fn trust_summary(rows: &[TrustRow], scope: Scope) -> String {
    let count = |t: Trust| rows.iter().filter(|r| r.trust == t).count();
    let (untrusted, modified) = (count(Trust::Untrusted), count(Trust::Modified));
    if untrusted + modified == 0 {
        return "trusted".to_string();
    }
    let mut parts = Vec::new();
    if untrusted > 0 {
        parts.push(format!("{untrusted} untrusted"));
    }
    if modified > 0 {
        parts.push(format!("{modified} modified since trusted"));
    }
    let project = match scope {
        Scope::Project => " and this directory must be a trusted project",
        Scope::User => "",
    };
    format!(
        "{} — Codex skips them until you start `codex` and accept them in its startup hooks review{project} \
         (automation: `codex exec --dangerously-bypass-hook-trust`)",
        parts.join(", ")
    )
}

fn read_settings(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(json!({})),
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON — not touching it", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_settings(path: &Path, v: &Value) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut text = serde_json::to_string_pretty(v)?;
    text.push('\n');
    publish_hook_file(path, &text)
}

fn publish_hook_file(path: &Path, text: &str) -> Result<()> {
    // Preserve dotfile symlinks, but replace the target inode so existing readers keep
    // complete old bytes and a failed write cannot truncate the user's settings.
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    cv_core::publish::publish_reader(&target, "hooks", text.as_bytes(), cv_core::publish::Claim::Replace)
        .with_context(|| format!("writing {}", path.display()))
}

/// Shell-quote a path for a `command` string (shell form): bare when it is plain, single-quoted otherwise.
fn shell_quote(p: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | ':' | '~');
    if !p.is_empty() && p.chars().all(plain) {
        p.to_string()
    } else {
        format!("'{}'", p.replace('\'', "'\\''"))
    }
}

/// The `command` string for one hook: this binary, absolute, then `hook <name>`.
fn command_for(harness: Harness, exe: &str, name: HookName) -> String {
    format!(
        "{}{} hook {}",
        if harness == Harness::Codex {
            "CV_HOOK_HARNESS=codex "
        } else {
            ""
        },
        shell_quote(exe),
        name.as_str()
    )
}

/// Is this handler one of ours? Recognized by shape: `<something>/cv hook <name>` (quotes allowed
/// around the program), so a reinstall from another binary path replaces rather than duplicates.
fn ours(handler: &Value) -> Option<HookName> {
    let cmd = handler.get("command")?.as_str()?.trim();
    let (prog, rest) = cmd.rsplit_once(" hook ")?;
    let name = HookName::parse(rest.trim())?;
    let prog = prog.trim().trim_matches(|c| c == '\'' || c == '"');
    let base = Path::new(prog).file_name()?.to_str()?;
    (base == "cv" || base == "cv.exe").then_some(name)
}

fn handler_for(harness: Harness, exe: &str, spec: &Spec) -> Value {
    let mut h = Map::new();
    h.insert("type".into(), json!("command"));
    h.insert("command".into(), json!(command_for(harness, exe, spec.name)));
    if spec.asynchronous {
        h.insert("async".into(), json!(true));
    } else {
        h.insert("timeout".into(), json!(10));
    }
    Value::Object(h)
}

/// Strip every handler of ours from `settings.hooks`, dropping groups and events left empty.
fn strip_ours(settings: &mut Value) {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for ev in SPECS.iter().map(|s| s.event).collect::<std::collections::BTreeSet<_>>() {
        if let Some(groups) = hooks.get_mut(ev).and_then(Value::as_array_mut) {
            for g in groups.iter_mut() {
                if let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                    hs.retain(|h| ours(h).is_none());
                }
            }
            groups.retain(|g| {
                g.get("hooks")
                    .and_then(Value::as_array)
                    .is_some_and(|hs| !hs.is_empty())
            });
            if groups.is_empty() {
                hooks.remove(ev);
            }
        }
    }
    if hooks.is_empty() {
        settings.as_object_mut().map(|o| o.remove("hooks"));
    }
}

/// `settings` with our six entries present (one group per spec, appended under its event).
fn with_ours(harness: Harness, mut settings: Value, exe: &str) -> Value {
    strip_ours(&mut settings);
    let obj = settings.as_object_mut().expect("settings is an object");
    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks = hooks.as_object_mut().expect("object");
    for spec in specs(harness) {
        let mut group = Map::new();
        if let Some(m) = spec.matcher {
            group.insert("matcher".into(), json!(m));
        }
        group.insert("hooks".into(), json!([handler_for(harness, exe, spec)]));
        let groups = hooks.entry(spec.event).or_insert_with(|| json!([]));
        if !groups.is_array() {
            *groups = json!([]);
        }
        groups.as_array_mut().expect("array").push(Value::Object(group));
    }
    settings
}

/// Which of our entries are present in `settings`, by `(event, matcher, name)`, with the command found.
fn present(harness: Harness, settings: &Value) -> HashMap<(&'static str, Option<&'static str>, HookName), String> {
    let mut found = HashMap::new();
    let Some(hooks) = settings.get("hooks").and_then(Value::as_object) else {
        return found;
    };
    for spec in specs(harness) {
        let Some(groups) = hooks.get(spec.event).and_then(Value::as_array) else {
            continue;
        };
        for g in groups {
            let matcher = g.get("matcher").and_then(Value::as_str);
            if matcher != spec.matcher {
                continue;
            }
            for h in g.get("hooks").and_then(Value::as_array).into_iter().flatten() {
                if ours(h) == Some(spec.name) {
                    let cmd = h.get("command").and_then(Value::as_str).unwrap_or("").to_string();
                    found.insert((spec.event, spec.matcher, spec.name), cmd);
                }
            }
        }
    }
    found
}

fn hook_harness(harness: &str) -> Result<Harness> {
    match harness {
        "claude" => Ok(Harness::Claude),
        "codex" => Ok(Harness::Codex),
        _ => bail!("`cv hooks` supports verified Claude Code (`claude`) and Codex (`codex`) hooks; {harness:?} has no verified hook contract"),
    }
}

fn this_exe() -> Result<String> {
    let exe = std::env::current_exe().context("locating the cv binary")?;
    Ok(exe.to_string_lossy().into_owned())
}

pub(crate) fn cmd_hooks(cmd: HooksCmd) -> Result<()> {
    match cmd {
        HooksCmd::Install { harness, scope } => {
            let harness = hook_harness(&harness)?;
            let (settings_path, skill_path) = paths(harness, scope)?;
            let exe = this_exe()?;
            let before = read_settings(&settings_path)?;
            let after = with_ours(harness, before.clone(), &exe);
            let mut changed = Vec::new();
            if after != before {
                write_settings(&settings_path, &after)?;
                changed.push(format!("{} entries → {}", SPECS.len(), home_rel(&settings_path)));
            }
            if std::fs::read_to_string(&skill_path).ok().as_deref() != Some(SKILL) {
                if let Some(dir) = skill_path.parent() {
                    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
                }
                publish_hook_file(&skill_path, SKILL)?;
                changed.push(format!("skill → {}", home_rel(&skill_path)));
            }
            if changed.is_empty() {
                println!(
                    "cv hooks: already installed ({} entries in {}); nothing written",
                    SPECS.len(),
                    home_rel(&settings_path)
                );
            } else {
                println!("cv hooks: installed {}", changed.join(" · "));
                println!("  (takes effect in new harness sessions; `cv hooks show` lists the entries, `cv hooks remove` deletes them)");
            }
            if harness == Harness::Codex {
                println!(
                    "  Codex trust: {}",
                    trust_summary(&codex_trust(&settings_path, &after), scope)
                );
            }
            Ok(())
        }
        HooksCmd::Remove { harness, scope } => {
            let harness = hook_harness(&harness)?;
            let (settings_path, skill_path) = paths(harness, scope)?;
            let before = read_settings(&settings_path)?;
            let n = present(harness, &before).len();
            let mut after = before.clone();
            strip_ours(&mut after);
            let mut removed = Vec::new();
            if after != before {
                write_settings(&settings_path, &after)?;
                removed.push(format!("{n} entries from {}", home_rel(&settings_path)));
            }
            if skill_path.exists() {
                std::fs::remove_file(&skill_path).with_context(|| format!("removing {}", skill_path.display()))?;
                if let Some(dir) = skill_path.parent() {
                    let _ = std::fs::remove_dir(dir); // only if empty
                }
                removed.push(format!("skill {}", home_rel(&skill_path)));
            }
            if removed.is_empty() {
                println!(
                    "cv hooks: nothing of cv's in {}; nothing written",
                    home_rel(&settings_path)
                );
            } else {
                println!("cv hooks: removed {}", removed.join(" · "));
            }
            Ok(())
        }
        HooksCmd::Show { harness, scope, json } => {
            let harness = hook_harness(&harness)?;
            let (settings_path, skill_path) = paths(harness, scope)?;
            let exe = this_exe()?;
            let before = read_settings(&settings_path)?;
            let after = with_ours(harness, before.clone(), &exe);
            let have = present(harness, &before);
            let skill_present = std::fs::read_to_string(&skill_path).ok().as_deref() == Some(SKILL);
            // Codex: the trust each present entry has today (the file as it is, not as install would leave it).
            let trust = match harness {
                Harness::Codex => codex_trust(&settings_path, &before),
                _ => Vec::new(),
            };
            let trust_of = |s: &Spec| {
                trust
                    .iter()
                    .find(|r| r.name == s.name && r.key.contains(&format!(":{}:", codex_event_label(s.event))))
            };
            if json {
                let (mut installed, mut missing) = (Vec::new(), Vec::new());
                for s in specs(harness) {
                    let mut row = json!({"event": s.event, "matcher": s.matcher, "hook": s.name.as_str(),
                        "command": command_for(harness, &exe, s.name)});
                    if let Some(t) = trust_of(s) {
                        row["trust"] = json!(t.trust.as_str());
                        row["trust_key"] = json!(t.key);
                        row["current_hash"] = json!(t.hash);
                    }
                    match have.get(&(s.event, s.matcher, s.name)) {
                        Some(cmd) if *cmd == command_for(harness, &exe, s.name) => installed.push(row),
                        _ => missing.push(row),
                    }
                }
                let mut out = json!({
                    "settings_path": settings_path, "installed": installed, "missing": missing,
                    "skill_path": skill_path, "skill_present": skill_present,
                    "before": before, "after": after, "would_change": after != before || !skill_present,
                });
                if harness == Harness::Codex {
                    out["trust"] = json!(trust_summary(&trust, scope));
                }
                println!("{}", serde_json::to_string_pretty(&out)?);
                return Ok(());
            }
            println!("# cv hooks — {} ({:?} scope)\n", home_rel(&settings_path), scope);
            for s in specs(harness) {
                let want = command_for(harness, &exe, s.name);
                let mark = match have.get(&(s.event, s.matcher, s.name)) {
                    Some(cmd) if *cmd == want => "✓",
                    Some(_) => "~", // ours, another binary path: install replaces it
                    None => "+",
                };
                let matcher = s.matcher.map(|m| format!(" [{m}]")).unwrap_or_default();
                let trust = trust_of(s)
                    .map(|t| format!(" · {}", t.trust.as_str()))
                    .unwrap_or_default();
                println!("  {mark} {:<17}{matcher:<18} {want}{trust}", s.event);
            }
            println!(
                "  {} skill {}",
                if skill_present { "✓" } else { "+" },
                home_rel(&skill_path)
            );
            println!(
                "\n{}",
                if after == before && skill_present {
                    format!("installed; `cv hooks install {harness}` would write nothing")
                } else {
                    format!("✓ present · + `cv hooks install {harness}` adds · ~ replaces (another cv binary's path)")
                }
            );
            if harness == Harness::Codex && !trust.is_empty() {
                println!("Codex trust: {}", trust_summary(&trust, scope));
            }
            Ok(())
        }
    }
}

// ───────────────────────────── doctor's line ─────────────────────────────

/// The compaction knobs Claude Code honours, read from the user and project settings (project wins)
/// and the process environment (wins over a settings `env` block). `None` = unset.
pub(crate) struct Policy {
    pub auto_compact_enabled: Option<bool>,
    pub idle_compaction: Option<bool>,
    pub env: Vec<(&'static str, Option<String>)>,
}

const POLICY_ENV: &[&str] = &[
    "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    "DISABLE_AUTO_COMPACT",
];

/// `(installed entries, settings path)` per scope, and the policy in force — what `cv doctor` prints.
pub(crate) fn status() -> (Vec<(Scope, usize, PathBuf)>, Policy) {
    let mut scopes = Vec::new();
    let mut auto = None;
    let mut idle = None;
    let mut env_block: HashMap<String, String> = HashMap::new();
    for scope in [Scope::User, Scope::Project] {
        let Ok((path, _)) = paths(Harness::Claude, scope) else {
            continue;
        };
        let settings = read_settings(&path).unwrap_or(json!({}));
        scopes.push((scope, present(Harness::Claude, &settings).len(), path));
        if let Some(b) = settings.get("autoCompactEnabled").and_then(Value::as_bool) {
            auto = Some(b);
        }
        if let Some(b) = settings.get("idleCompaction").and_then(Value::as_bool) {
            idle = Some(b);
        }
        if let Some(env) = settings.get("env").and_then(Value::as_object) {
            for (k, v) in env {
                if let Some(s) = v.as_str() {
                    env_block.insert(k.clone(), s.to_string());
                }
            }
        }
    }
    let env = POLICY_ENV
        .iter()
        .map(|k| (*k, cv_core::env::var(k).ok().or_else(|| env_block.get(*k).cloned())))
        .collect();
    (
        scopes,
        Policy {
            auto_compact_enabled: auto,
            idle_compaction: idle,
            env,
        },
    )
}

/// Codex's hooks per scope: `(scope, entries present, hooks.json, their trust)`, scopes with entries only.
fn codex_status() -> Vec<(Scope, usize, PathBuf, Vec<TrustRow>)> {
    [Scope::User, Scope::Project]
        .into_iter()
        .filter_map(|scope| {
            let (path, _) = paths(Harness::Codex, scope).ok()?;
            let settings = read_settings(&path).unwrap_or(json!({}));
            let n = present(Harness::Codex, &settings).len();
            (n > 0).then(|| (scope, n, path.clone(), codex_trust(&path, &settings)))
        })
        .collect()
}

/// The three lines `cv doctor` prints under its header.
pub(crate) fn doctor_lines() -> Vec<String> {
    let (scopes, policy) = status();
    let total = SPECS.len();
    let installed: Vec<String> = scopes
        .iter()
        .filter(|(_, n, _)| *n > 0)
        .map(|(scope, n, path)| format!("{n}/{total} in {} ({scope:?})", home_rel(path)))
        .collect();
    let hooks = if installed.is_empty() {
        "not installed — `cv hooks install claude` wires compaction, session start, lanes, decisions and the edit guard"
            .to_string()
    } else {
        installed.join(", ")
    };
    let codex: Vec<String> = codex_status()
        .iter()
        .map(|(scope, n, path, trust)| {
            format!(
                "{n}/{total} in {} ({scope:?}), {}",
                home_rel(path),
                trust_summary(trust, *scope)
            )
        })
        .collect();
    let codex = if codex.is_empty() {
        "not installed — `cv hooks install codex` wires the same five loops for Codex".to_string()
    } else {
        codex.join("; ")
    };
    let onoff = |v: Option<bool>, default_on: bool| match v {
        Some(true) => "true".to_string(),
        Some(false) => "false".to_string(),
        None => format!("unset (default {})", if default_on { "on" } else { "off" }),
    };
    let mut knobs = vec![
        format!("autoCompactEnabled {}", onoff(policy.auto_compact_enabled, true)),
        format!("idleCompaction {}", onoff(policy.idle_compaction, false)),
    ];
    knobs.extend(policy.env.iter().map(|(k, v)| match v {
        Some(v) => format!("{k}={}", sanitize_line(v)),
        None => format!("{k} unset"),
    }));
    vec![
        format!("Hooks:       {hooks}"),
        format!("Codex hooks: {codex}"),
        format!("Policy:      {}", knobs.join(" · ")),
    ]
}

pub(crate) fn doctor_json() -> Value {
    let (scopes, policy) = status();
    json!({
        "installed": scopes.iter().map(|(scope, n, path)| json!({
            "scope": format!("{scope:?}").to_lowercase(), "entries": n, "of": SPECS.len(), "settings_path": path
        })).collect::<Vec<_>>(),
        "codex": codex_status().iter().map(|(scope, n, path, trust)| json!({
            "scope": format!("{scope:?}").to_lowercase(), "entries": n, "of": SPECS.len(), "settings_path": path,
            "trust": trust.iter().map(|r| json!({"hook": r.name.as_str(), "key": r.key, "trust": r.trust.as_str()})).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "policy": {
            "auto_compact_enabled": policy.auto_compact_enabled,
            "idle_compaction": policy.idle_compaction,
            "env": policy.env.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<Map<String, Value>>(),
        }
    })
}

// ───────────────────────────── the hook bodies ─────────────────────────────

/// The fields the five bodies read from the hook JSON (every other field is ignored).
#[derive(Deserialize, Default)]
struct Input {
    #[serde(skip)]
    codex: bool,
    #[serde(default)]
    session_id: String,
    transcript_path: Option<PathBuf>,
    cwd: Option<PathBuf>,
    hook_event_name: Option<String>,
    agent_id: Option<String>,
    agent_type: Option<String>,
    agent_transcript_path: Option<PathBuf>,
    last_assistant_message: Option<String>,
    tool_input: Option<Value>,
}

/// `cv hook <name>`: read stdin, run the body, print its one-line JSON (or nothing). A body that
/// fails prints its error on stderr and still exits 0 — a hook must never block the harness. A
/// terminal on stdin means no harness is calling: usage, exit 2, instead of waiting for JSON.
pub(crate) fn cmd_hook(name: HookName) -> Result<()> {
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return crate::util::usage(format!(
            "cv hook {} reads Claude Code's hook JSON on stdin, and stdin is a terminal; \
             `cv hooks install claude` writes the settings entries that pipe it",
            name.as_str()
        ));
    }
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let mut input: Input = match serde_json::from_str(&raw) {
        Ok(i) => i,
        Err(e) => {
            eprintln!(
                "cv hook {}: stdin is not the hook JSON ({e}); nothing to do",
                name.as_str()
            );
            return Ok(());
        }
    };
    input.codex = cv_core::env::var_os("CV_HOOK_HARNESS").as_deref() == Some(std::ffi::OsStr::new("codex"));
    let out = match name {
        HookName::Compact => hook_compact(&input),
        HookName::Start => hook_start(&input),
        HookName::Lane => hook_lane(&input),
        HookName::Decisions => hook_decisions(&input),
        HookName::Edit => hook_edit(&input),
    };
    match out {
        Ok(Some(v)) => println!("{v}"),
        Ok(None) => {}
        Err(e) => eprintln!("cv hook {}: {e:#}", name.as_str()),
    }
    Ok(())
}

fn context(event: &str, text: String) -> Value {
    json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": text}})
}

fn claude_ref(id: &str, path: &Path) -> SessionRef {
    SessionRef {
        id: id.to_string(),
        harness: Harness::Claude,
        path: path.to_path_buf(),
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        message_count: None,
    }
}

/// Read the last [`COMPACT_TAIL_BYTES`] of `path`.
fn tail_text(path: &Path, bytes: u64) -> Option<String> {
    use std::io::{Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(bytes);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// The newest `compact_boundary` record in a transcript's tail: `(trigger, preTokens)`. Reads
/// one line's JSON, never the file — the large-transcript path of [`hook_compact`].
fn last_boundary_in_tail(path: &Path) -> Option<(Option<String>, Option<u64>)> {
    let text = tail_text(path, COMPACT_TAIL_BYTES)?;
    let v: Value = text
        .lines()
        .rev()
        .filter(|l| l.contains("\"compact_boundary\""))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v.get("subtype").and_then(Value::as_str) == Some("compact_boundary"))?;
    let meta = v.get("compactMetadata");
    Some((
        meta.and_then(|m| m.get("trigger"))
            .and_then(Value::as_str)
            .map(str::to_string),
        meta.and_then(|m| m.get("preTokens")).and_then(Value::as_u64),
    ))
}

fn not_recorded_yet(id: &str) -> Value {
    context(
        "SessionStart",
        format!(
            "context compacted; the transcript has not recorded the boundary yet — `cv show {id} --kind compaction_boundary,compaction_summary` \
             lists every boundary once it has, `cv show {id} --pre-compaction N` reads the span the Nth discarded."
        ),
    )
}

/// `SessionStart` (`compact`): the boundary the harness just wrote and the commands that read what
/// it discarded. A transcript up to [`COMPACT_FULL_SCAN_MAX`] is streamed the way `cv show --kind
/// compaction_boundary` does it (exact message index and ordinal); a larger one is read from its tail (trigger and
/// size exact, the index left to that view), so the hook stays inside its budget however
/// long the session has run.
fn hook_compact(input: &Input) -> Result<Option<Value>> {
    let Some(path) = input.transcript_path.as_deref() else {
        return Ok(None);
    };
    let id = if input.codex {
        format!("codex:{}", input.session_id)
    } else {
        short_id(&input.session_id)
    };
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size > COMPACT_FULL_SCAN_MAX {
        // A tail without a boundary (one far back in a long transcript) still gets the pointers.
        let what = match if input.codex { None } else { last_boundary_in_tail(path) } {
            Some((trigger, pre_tokens)) => format!(
                " ({}, {})",
                trigger.as_deref().unwrap_or("compaction"),
                pre_tokens
                    .map(|t| format!("{} tokens", fmt_int(t)))
                    .unwrap_or_else(|| "size unrecorded".into())
            ),
            None => String::new(),
        };
        return Ok(Some(context(
            "SessionStart",
            format!(
                "context compacted{what}. `cv show {id} --kind compaction_boundary,compaction_summary` lists this session's \
                 boundaries with their message indexes; `cv show {id} --pre-compaction N` reads what the Nth discarded (`cv show \
                 {id} --kind prompt,answer --pre-compaction N` just its prompts); {rewind} \
                 {AFFORDANCE_POINTER}",
                rewind = if input.codex { String::new() } else { format!("`cv rewind {id} --at <msg>` resumes the agent as of a message.") },
                AFFORDANCE_POINTER = affordance_pointer(&id)
            ),
        )));
    }
    let mut reference = claude_ref(&input.session_id, path);
    if input.codex {
        reference.harness = Harness::Codex;
    }
    let comps = cv_core::compaction::detect(&reference, false)?;
    let Some(last) = comps.last() else {
        return Ok(Some(not_recorded_yet(&id)));
    };
    let k = comps.len();
    let trigger = last.trigger.as_deref().unwrap_or("compaction");
    let pre = last
        .pre_tokens
        .map(|t| format!("{} tokens", fmt_int(t)))
        .unwrap_or_else(|| "size unrecorded".into());
    let idx = last.boundary_msg_idx;
    let mut text = format!(
        "context compacted ({trigger}, {pre}) at message {idx} — compaction #{k} of this session. \
         `cv show {id} --pre-compaction {k}` reads what was lost (`cv show {id} --kind prompt,answer --pre-compaction {k}` just its prompts)"
    );
    if idx > 0 && !input.codex {
        text.push_str(&format!(
            "; `cv rewind {id} --at {}` resumes the agent as of before it.",
            idx - 1
        ));
    } else {
        text.push('.');
    }
    text.push(' ');
    text.push_str(&affordance_pointer(&id));
    Ok(Some(context("SessionStart", text)))
}

/// The one command that reads back what a compaction summary tends to flatten: the session's plan
/// (with the task list beside the transcript), its schedules and its background jobs.
fn affordance_pointer(id: &str) -> String {
    format!("`cv show {id} --kind plan,schedule,background_job` lists the plan, schedules and background jobs this session holds.")
}

/// The last `[branch sha] subject` a transcript's tail recorded (a `git commit` result), reading at
/// most [`COMMIT_TAIL_BYTES`] from the end — never a parse of the whole file.
fn last_commit(path: &Path) -> Option<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r#"\[([^\]\s\\"]+)(?: \([^)]*\))? ([0-9a-f]{7,40})\] "#).expect("static regex")
    });
    let text = tail_text(path, COMMIT_TAIL_BYTES)?;
    re.captures_iter(&text)
        .last()
        .map(|c| c[2][..7.min(c[2].len())].to_string())
}

fn ago(t: Option<chrono::DateTime<chrono::Utc>>) -> String {
    t.map(|t| format!("{} ago", cv_core::task::age_short(t, chrono::Utc::now())))
        .unwrap_or_else(|| "undated".into())
}

/// `(who, decisions owed)` from the task log, under the identity `cv task inbox` would use.
fn owed_decisions() -> Result<(String, usize)> {
    let who = cv_core::task::actor(None, "cv");
    let outcome = cv_core::task::replay()?;
    let n = cv_core::task::inbox(&outcome.model, &who)
        .iter()
        .filter(|e| matches!(e.reason, cv_core::task::InboxReason::DecisionOwed))
        .count();
    Ok((who, n))
}

/// `SessionStart` (`startup|resume`): this directory's last sessions from the catalog as it stands
/// (ids, ages, titles, last commit shas) and the decisions owed — capped, nothing else.
fn hook_start(input: &Input) -> Result<Option<Value>> {
    let cwd = match &input.cwd {
        Some(c) => c.clone(),
        None => std::env::current_dir()?,
    };
    // One canonicalize for the hook's cwd; the catalog's cwds are compared as recorded (a
    // per-row canonicalize is thousands of syscalls on a real store, for nothing).
    let canon = std::fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());
    let mut rows: Vec<SessionRef> = cv_core::cataloged_sessions()
        .into_iter()
        .filter(|r| r.id != input.session_id && !r.id.starts_with("agent-"))
        .filter(|r| r.cwd.as_deref().is_some_and(|c| c == cwd || c == canon))
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.updated_at.or(r.created_at)));
    rows.truncate(START_SESSIONS);
    let (who, owed) = owed_decisions().unwrap_or_else(|_| ("cv".into(), 0));
    if rows.is_empty() && owed == 0 {
        return Ok(None);
    }
    let mut lines = Vec::new();
    if !rows.is_empty() {
        lines.push(format!(
            "cv: {} earlier session(s) in this directory (catalog as of the last `cv ls`; newest first). \
             `cv show <id>` reads one; `cv pack \"<task>\"` pulls the relevant parts of several into context.",
            rows.len()
        ));
        for r in &rows {
            // A catalog title that is harness markup (`<local-command-caveat>…`) is no title.
            let title = r
                .title
                .as_deref()
                .filter(|t| !t.trim_start().starts_with('<'))
                .map(|t| cv_core::ir::truncate(&sanitize_line(t), 60))
                .unwrap_or_else(|| "(untitled)".into());
            let commit = last_commit(&r.path)
                .map(|s| format!(" · last commit {s}"))
                .unwrap_or_default();
            let id = match r.harness {
                Harness::Claude => short_id(&r.id),
                Harness::Codex => format!("codex:{}", r.id),
                h => format!("{h}:{}", short_id(&r.id)),
            };
            lines.push(format!(
                "- {id} · {} · {} msgs · {title}{commit}",
                ago(r.updated_at.or(r.created_at)),
                r.message_count.map_or("–".to_string(), |n| n.to_string())
            ));
        }
    }
    if owed > 0 {
        lines.push(format!(
            "- {owed} decision(s) owed to {who}: `cv task inbox` lists them."
        ));
    }
    let mut text = lines.join("\n");
    while text.chars().count() > START_PACK_MAX && lines.len() > 1 {
        lines.pop();
        text = lines.join("\n");
    }
    Ok(Some(context("SessionStart", text)))
}

fn edit_paths(input: &Input) -> Vec<String> {
    let Some(tool) = input.tool_input.as_ref() else {
        return Vec::new();
    };
    if !input.codex {
        return tool
            .get("file_path")
            .or_else(|| tool.get("notebook_path"))
            .and_then(Value::as_str)
            .map(|s| vec![s.to_string()])
            .unwrap_or_default();
    }
    // Codex apply_patch's hook input is command-shaped even for freeform calls.
    let Some(patch) = tool.get("command").and_then(Value::as_str) else {
        return Vec::new();
    };
    if patch.trim().lines().next() != Some("*** Begin Patch") {
        return Vec::new();
    }
    let mut paths: Vec<String> = patch
        .trim()
        .lines()
        .filter_map(|line| {
            [
                "*** Add File: ",
                "*** Update File: ",
                "*** Delete File: ",
                "*** Move to: ",
            ]
            .into_iter()
            .find_map(|prefix| line.strip_prefix(prefix))
            .map(str::to_string)
        })
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// `SubagentStart` / `SubagentStop`: one row in the lane ledger; no output, no model action.
fn hook_lane(input: &Input) -> Result<Option<Value>> {
    let event = match input.hook_event_name.as_deref() {
        Some("SubagentStart") => "start",
        Some("SubagentStop") => "stop",
        _ => return Ok(None),
    };
    let Some(agent_id) = input.agent_id.as_deref() else {
        return Ok(None);
    };
    let agent_id = if input.codex {
        agent_id
    } else {
        agent_id.strip_prefix("agent-").unwrap_or(agent_id)
    }
    .to_string();
    let expand = |p: &Path| -> PathBuf {
        match (p.strip_prefix("~"), crate::util::dirs_home()) {
            (Ok(rest), Some(h)) => h.join(rest),
            _ => p.to_path_buf(),
        }
    };
    let mut transcript_path = input.transcript_path.as_deref().map(expand);
    let agent_transcript_path = input.agent_transcript_path.as_deref().map(expand).or_else(|| {
        if input.codex {
            return (event == "start").then(|| transcript_path.clone()).flatten();
        }
        transcript_path
            .as_deref()
            .and_then(cv_core::harness::claude::sidecar::subagents_dir)
            .map(|d| d.join(format!("agent-{agent_id}.jsonl")))
    });
    let session_id = if input.codex {
        let path = agent_transcript_path
            .as_deref()
            .context("Codex lane hook has no child transcript")?;
        let parent = cv_core::harness::codex::hook_header(path)
            .and_then(|s| s.lineage.parent)
            .context("Codex child transcript has no parent identity")?;
        if event == "start" {
            transcript_path = cv_core::catalog::lookup(&parent, Some(Harness::Codex))
                .into_iter()
                .next()
                .map(|r| r.path);
        }
        parent
    } else {
        input.session_id.clone()
    };
    let last_text = input
        .last_assistant_message
        .as_deref()
        .and_then(|t| t.lines().find(|l| !l.trim().is_empty()))
        .map(|l| cv_core::ir::truncate(l.trim(), 200));
    cv_core::lanes::append_lane_event(&LaneEvent {
        ts: chrono::Utc::now(),
        event: event.into(),
        session_id,
        agent_id,
        agent_type: input.agent_type.clone(),
        transcript_path,
        agent_transcript_path,
        cwd: input.cwd.clone(),
        last_text,
    })
    .context("appending to the lane ledger")?;
    Ok(None)
}

/// `UserPromptSubmit`: one line when decisions are owed, else nothing at all.
fn hook_decisions(_input: &Input) -> Result<Option<Value>> {
    let (who, n) = owed_decisions()?;
    if n == 0 {
        return Ok(None);
    }
    Ok(Some(context(
        "UserPromptSubmit",
        format!("cv: {n} decision(s) owed to {who} — `cv task inbox` lists them; `cv task resolve <id> --accept-default` or `--choice <option>` answers one."),
    )))
}

/// The transcript behind an indexed session, without a scan: the catalog for a top-level id, the
/// parent's `subagents/` dir for an `agent-…` id.
fn transcript_of(t: &cv_core::events::TouchedSession) -> Option<PathBuf> {
    if let Some(bare) = t.session_id.strip_prefix("agent-") {
        let parent = cv_core::catalog::lookup(t.parent_id.as_deref()?, None)
            .into_iter()
            .next()?;
        let dir = cv_core::harness::claude::sidecar::subagents_dir(&parent.path)?;
        let direct = dir.join(format!("agent-{bare}.jsonl"));
        if direct.exists() {
            return Some(direct);
        }
        let wf = t.workflow.as_deref()?;
        return Some(dir.join("workflows").join(wf).join(format!("agent-{bare}.jsonl")));
    }
    cv_core::catalog::lookup(&t.session_id, None)
        .into_iter()
        .find(|r| r.id == t.session_id)
        .map(|r| r.path)
}

fn minutes_since_modified(path: &Path) -> Option<i64> {
    let m = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(std::time::SystemTime::now().duration_since(m).ok()?.as_secs() as i64 / 60)
}

fn is_current_editor(t: &cv_core::events::TouchedSession, session_id: &str, agent_id: Option<&str>) -> bool {
    t.harness == "claude"
        && t.parent_id.as_deref().unwrap_or(&t.session_id) == session_id
        && t.agent_id.as_deref() == agent_id
}

/// The root thread of a Codex session: each rollout's `parent_thread_id` header followed up
/// through the catalog, the lane ledger's `start` rows standing in for a child the catalog has
/// not indexed yet. A thread without a parent is its own root. Bounded, so a cyclic ledger ends.
fn codex_root(id: &str, path: Option<&Path>, ledger: &[LaneEvent]) -> String {
    let mut id = id.to_string();
    let mut path = path.map(Path::to_path_buf);
    for _ in 0..16 {
        let registered = ledger
            .iter()
            .rev()
            .find(|row| row.event == "start" && row.agent_id == id);
        let parent = path
            .as_deref()
            .and_then(cv_core::harness::codex::hook_header)
            .and_then(|s| s.lineage.parent)
            .or_else(|| registered.map(|row| row.session_id.clone()));
        let Some(parent) = parent else { return id };
        path = cv_core::catalog::lookup(&parent, Some(Harness::Codex))
            .into_iter()
            .find(|r| r.id == parent)
            .map(|r| r.path)
            .or_else(|| registered.and_then(|row| row.transcript_path.clone()));
        id = parent;
    }
    id
}

/// `PreToolUse` (`Edit|Write`): `ask` when another live session edited this path — the edit
/// index (`cv touched`) names the sessions, the catalog names their transcripts, one `stat` each
/// says which are live. Nothing printed = allow. A Claude session's own lane is not another
/// session; neither is any thread under a Codex session's root (its parent, its children), since
/// Codex only takes `deny` and a swarm shares its files on purpose.
fn hook_edit(input: &Input) -> Result<Option<Value>> {
    let files = edit_paths(input);
    let mut conflicts = Vec::new();
    let my_agent = input
        .agent_id
        .as_deref()
        .map(|a| a.strip_prefix("agent-").unwrap_or(a).to_string());
    let ledger = if input.codex {
        cv_core::lanes::read_ledger()
    } else {
        Vec::new()
    };
    let my_root = input
        .codex
        .then(|| codex_root(&input.session_id, input.transcript_path.as_deref(), &ledger));
    for file in files {
        let abs = if Path::new(&file).is_absolute() {
            PathBuf::from(&file)
        } else {
            input
                .cwd
                .clone()
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_default()
                .join(file.trim_start_matches("./"))
        };
        let mut live: Vec<(String, i64)> = Vec::new();
        for t in cv_core::events::sessions_editing(&abs.to_string_lossy()) {
            if !input.codex && is_current_editor(&t, &input.session_id, my_agent.as_deref()) {
                continue;
            }
            let Some(path) = transcript_of(&t) else { continue };
            if let Some(root) = my_root.as_deref() {
                if t.harness == "codex" && codex_root(&t.session_id, Some(&path), &ledger) == root {
                    continue;
                }
            }
            if let Some(m) = minutes_since_modified(&path).filter(|m| *m <= LIVE_MINUTES) {
                live.push((
                    if t.harness == "codex" {
                        format!("codex:{}", t.session_id)
                    } else {
                        short_id(&t.session_id)
                    },
                    m,
                ));
            }
        }
        if live.is_empty() {
            continue;
        }
        live.sort_by_key(|(_, m)| *m);
        let (first, mins) = &live[0];
        let more = match live.len() {
            1 => String::new(),
            n => format!(" (and {} more live session{})", n - 1, if n == 2 { "" } else { "s" }),
        };
        let shown = home_rel(&abs);
        let reason = format!(
        "cv: {shown} was edited by live session {} ({mins} min ago){more}. `cv show {} --last 10` shows its hand; proceed only if the edits do not collide.",
        first,
        first
    );
        conflicts.push(reason);
    }
    if conflicts.is_empty() {
        return Ok(None);
    }
    let reason = conflicts.join("\n");
    Ok(Some(json!({"hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": if input.codex { "deny" } else { "ask" },
        "permissionDecisionReason": reason,
    }})))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hashes Codex 0.162.1's `hooks/list` reported for cv's six entries as `with_ours` writes
    /// them (recorded 2026-10-19 in an isolated `CODEX_HOME`; the binary path is part of the hash).
    #[test]
    fn codex_hook_hash_matches_codex_0_162_1() {
        let exe = "/private/tmp/claude-501/-Users-ember-dev-cv/ece21c13-ee7c-4db3-b8a6-e83ade448e6a/scratchpad/target-hooks/debug/cv";
        let settings = with_ours(Harness::Codex, json!({}), exe);
        let recorded = [
            (
                "SessionStart",
                0,
                "sha256:bab06d02d2b21b9460a5e75fe440527fb5133c32ba0bf29307f663ed3e45d1b1",
            ),
            (
                "SessionStart",
                1,
                "sha256:0648814e802b9e8ceaa87e88b9890ea7bfae7c35d03fe595bf8f0f3fdbdf956a",
            ),
            (
                "SubagentStart",
                0,
                "sha256:a374e342343e7317477ef79e00c96e9d54b586c80958b47a766d2e33a5cb48f2",
            ),
            (
                "SubagentStop",
                0,
                "sha256:2213dfa52226988d3a710d6fa6ffa12b83028a189d06968e8386ab9fd397a28a",
            ),
            (
                "UserPromptSubmit",
                0,
                "sha256:72e820dad14de1ced01a772173169e39c3e88bb7d9ac3265e8fb8c838fad1bc5",
            ),
            (
                "PreToolUse",
                0,
                "sha256:aec1ede91be48eb971d19eacb805a8a9b31038fedfcae6c2e460198c8a77b3e6",
            ),
        ];
        for (event, group, hash) in recorded {
            let g = &settings["hooks"][event][group];
            assert_eq!(
                codex_hook_hash(event, g["matcher"].as_str(), &g["hooks"][0]),
                hash,
                "{event}[{group}]"
            );
        }
        // The keys Codex stores trust under, and the three states its `hooks/list` reports.
        let path = Path::new("/home/u/.codex/hooks.json");
        let rows = codex_trust(path, &settings);
        assert_eq!(rows.len(), 6);
        assert!(rows.iter().all(|r| r.trust == Trust::Untrusted));
        assert!(rows
            .iter()
            .any(|r| r.key == "/home/u/.codex/hooks.json:session_start:1:0" && r.name == HookName::Start));
        assert!(rows
            .iter()
            .any(|r| r.key == "/home/u/.codex/hooks.json:pre_tool_use:0:0" && r.hash == recorded[5].2));
    }

    #[test]
    fn codex_patch_guards_all_destinations_without_treating_shell_as_a_patch() {
        let input = Input {
            codex: true,
            tool_input: Some(
                json!({"command": "*** Begin Patch\n*** Update File: from name.rs\n*** Move to: to name.rs\n@@\n-old\n+new\n*** Add File: added.rs\n+hello\n*** Delete File: deleted.rs\n*** End Patch"}),
            ),
            ..Input::default()
        };
        assert_eq!(
            edit_paths(&input),
            ["added.rs", "deleted.rs", "from name.rs", "to name.rs"]
        );
        let shell = Input {
            tool_input: Some(json!({"command": "echo '*** Update File: pretend.rs'"})),
            ..input
        };
        assert!(edit_paths(&shell).is_empty());
    }

    #[test]
    fn hook_install_keeps_existing_readers_on_the_previous_files() {
        use std::io::Read;
        let _world = crate::test_world("hooks-atomic");
        let (settings, skill) = paths(Harness::Claude, Scope::User).unwrap();
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(&settings, "{}").unwrap();
        std::fs::write(&skill, "previous skill").unwrap();
        let mut previous_settings = std::fs::File::open(&settings).unwrap();
        let mut previous_skill = std::fs::File::open(&skill).unwrap();
        cmd_hooks(HooksCmd::Install {
            harness: "claude".into(),
            scope: Scope::User,
        })
        .unwrap();
        let mut held = String::new();
        previous_settings.read_to_string(&mut held).unwrap();
        assert_eq!(held, "{}");
        held.clear();
        previous_skill.read_to_string(&mut held).unwrap();
        assert_eq!(held, "previous skill");
        assert_eq!(
            read_settings(&settings).unwrap(),
            with_ours(Harness::Claude, json!({}), &this_exe().unwrap())
        );
        assert_eq!(std::fs::read_to_string(skill).unwrap(), SKILL);
    }

    #[test]
    fn edit_conflicts_distinguish_parent_sibling_and_reused_agent_identities() {
        let mut touched = cv_core::events::TouchedSession {
            harness: "claude".into(),
            session_id: "parent".into(),
            title: None,
            edits: 1,
            reads: 0,
            last_ts: None,
            parent_id: None,
            agent_id: None,
            workflow: None,
        };
        assert!(is_current_editor(&touched, "parent", None));
        assert!(
            !is_current_editor(&touched, "parent", Some("self")),
            "a child and parent are separate writers"
        );
        touched.session_id = "agent-self".into();
        touched.parent_id = Some("parent".into());
        touched.agent_id = Some("self".into());
        assert!(is_current_editor(&touched, "parent", Some("self")));
        assert!(!is_current_editor(&touched, "parent", Some("sibling")));
        assert!(!is_current_editor(&touched, "parent", None));
        assert!(!is_current_editor(&touched, "another-parent", Some("self")));
        touched.harness = "codex".into();
        assert!(!is_current_editor(&touched, "parent", Some("self")));
    }

    #[test]
    fn install_is_idempotent_and_remove_leaves_the_rest_untouched() {
        let theirs = json!({
            "theme": "dark",
            "hooks": {
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "guard.sh"}]}],
                "Stop": [{"hooks": [{"type": "command", "command": "/usr/bin/notify"}]}]
            }
        });
        let once = with_ours(Harness::Claude, theirs.clone(), "/opt/cv");
        let twice = with_ours(Harness::Claude, once.clone(), "/opt/cv");
        assert_eq!(once, twice, "a second install changes nothing");
        assert_eq!(present(Harness::Claude, &once).len(), SPECS.len());
        // Theirs survived, in place.
        assert_eq!(once["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert_eq!(once["hooks"]["PreToolUse"][1]["matcher"], "Edit|Write");
        assert_eq!(once["hooks"]["SubagentStop"][0]["hooks"][0]["async"], true);
        assert!(once["hooks"]["SessionStart"][0]["hooks"][0]["async"].is_null());
        // A reinstall from another binary path replaces ours rather than adding a seventh.
        let moved = with_ours(Harness::Claude, once.clone(), "/usr/local/bin/cv");
        assert_eq!(present(Harness::Claude, &moved).len(), SPECS.len());
        assert_eq!(moved["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
        let mut back = moved;
        strip_ours(&mut back);
        assert_eq!(back, theirs, "remove restores the original exactly");
        // Nothing of ours in an empty file leaves no `hooks` key behind.
        let mut empty = with_ours(Harness::Claude, json!({}), "/opt/cv");
        strip_ours(&mut empty);
        assert_eq!(empty, json!({}));
    }

    #[test]
    fn ours_recognizes_the_command_shape_only() {
        assert_eq!(
            ours(&json!({"command": "/opt/cv hook compact"})),
            Some(HookName::Compact)
        );
        assert_eq!(
            ours(&json!({"command": "'/Applications/My Tools/cv' hook lane"})),
            Some(HookName::Lane)
        );
        assert_eq!(ours(&json!({"command": "/opt/cv hook nope"})), None);
        assert_eq!(ours(&json!({"command": "/opt/other hook compact"})), None);
        assert_eq!(ours(&json!({"command": "cv-mcp"})), None);
        assert_eq!(shell_quote("/a b/cv"), "'/a b/cv'");
        assert_eq!(shell_quote("/usr/local/bin/cv"), "/usr/local/bin/cv");
    }

    #[test]
    fn the_large_transcript_path_reads_the_newest_boundary_from_the_tail() {
        let dir = std::env::temp_dir().join(format!("cv-hooks-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        let b = |trig: &str, pre: u64| {
            json!({"type": "system", "subtype": "compact_boundary", "compactMetadata": {"trigger": trig, "preTokens": pre}})
                .to_string()
        };
        // A user turn that merely quotes the word is not a boundary record.
        let quote = json!({"type": "user", "message": {"content": "grep \"compact_boundary\" x"}}).to_string();
        std::fs::write(&p, format!("{}\n{}\n{quote}\n", b("manual", 1), b("auto", 180_412))).unwrap();
        assert_eq!(last_boundary_in_tail(&p), Some((Some("auto".into()), Some(180_412))));
        std::fs::write(&p, format!("{}\n{quote}\n", b("auto", 5))).unwrap();
        assert_eq!(last_boundary_in_tail(&p), Some((Some("auto".into()), Some(5))));
        std::fs::write(&p, format!("{quote}\n")).unwrap();
        assert_eq!(last_boundary_in_tail(&p), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn last_commit_reads_the_tail_and_nothing_else() {
        let dir = std::env::temp_dir().join(format!("cv-hooks-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        std::fs::write(
            &p,
            concat!(
                "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"[main 3e6a269] docs\\n 1 file\"}]}}\n",
                "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"[p3-hooks 0abec0fd1] later\\n\"}]}}\n",
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"see [1] and [2]\"}]}}\n",
            ),
        )
        .unwrap();
        assert_eq!(last_commit(&p).as_deref(), Some("0abec0f"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
