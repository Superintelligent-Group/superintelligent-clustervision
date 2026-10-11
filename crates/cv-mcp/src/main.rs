//! # cv-mcp — clustervision as an MCP server
//!
//! A stdio [Model Context Protocol](https://modelcontextprotocol.io) server that lets a *running*
//! coding agent (Claude Code, Codex, Gemini, …) search and read the sessions of **other** agents —
//! within the same project, back through time, and across harnesses. "Let agents read other agents'
//! minds." It is a thin front-end over the [`cv_core`] library.
//!
//! ## Registering with a host
//!
//! Build it (`cargo build -p cv-mcp --release`) and point your agent at the binary. For Claude Code:
//!
//! ```text
//! claude mcp add clustervision -- /absolute/path/to/cv-mcp
//! ```
//!
//! It speaks line-delimited JSON-RPC 2.0 over stdin/stdout. **stdout is the protocol channel**, so
//! all diagnostics go to stderr.
//!
//! ## Tools exposed
//!
//! Two families, and the split is the whole design (docs/INTERFACE-V2.md §6):
//!
//! 1. **Generated from the CLI** — one tool per `cv` command in the Read / Reshape / Export /
//!    System groups (`ls`, `show`, `cat`, `search`, `events`, `tools`, `workflow`, `prune`,
//!    `export`, `pack`, `doctor`, …). At startup the server runs `cv schema --commands --json`
//!    once and turns clap's own introspection into MCP schemas, so tool names, flags, harness
//!    lists and output shapes cannot drift from the binary. A call shells out to
//!    `cv <command> <args…> --json`. See [`cli_tools`].
//! 2. **Hand-written, MCP-only** — the tools with no CLI equivalent, because they block, hold a
//!    cursor, or arbitrate between agents:
//!    - `await_omen(regex, …)` — block until another agent's session emits a matching message.
//!    - `observe_stream(cwd_contains?, harness?, since_cursor?, max_messages=50, char_cap?)` — the
//!      non-blocking sibling: drains the *newly-appended* messages since an opaque cursor and
//!      returns immediately with a fresh cursor, so a senior agent can poll a junior's activity on
//!      its own cadence. Bounded + read-only.
//!    - `task_*` — durable fleet tasks, mirroring `cv task <sub>`.
//!
//! `show` is windowed by default over MCP (`--last 50 --max-bytes 200000`) so a whole transcript is
//! never what an agent accidentally pulls into its context.

mod cli_tools;

use anyhow::Context as _;
use cli_tools::CliTools;
use cv_core::watch::{Cursor, Filter, Watcher};
use cv_core::{Block, Harness};
use serde_json::{json, Value};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// The CLI-generated tools, resolved once at startup. `None` means no usable `cv` was found — the
/// server still serves the hand-written tools rather than refusing to start.
static CLI: OnceLock<Option<CliTools>> = OnceLock::new();

fn cli() -> Option<&'static CliTools> {
    CLI.get().and_then(Option::as_ref)
}

const PROTOCOL_VERSION: &str = "2025-06-18";

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    eprintln!("cv-mcp: clustervision MCP server starting (stdio JSON-RPC)");

    // Generate the command tools from the CLI itself. A failure here is loud but not fatal: the
    // hand-written coordination tools (task/omen) don't need `cv` on disk.
    let generated = match CliTools::discover() {
        Ok(c) => {
            eprintln!(
                "cv-mcp: generated {} tools from {}",
                c.commands().len(),
                c.bin().display()
            );
            Some(c)
        }
        Err(e) => {
            eprintln!(
                "cv-mcp: no CLI-generated tools ({e:#}); serving the MCP-only tools. Set $CV_BIN to a `cv` binary."
            );
            None
        }
    };
    let _ = CLI.set(generated);

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut buf: Vec<u8> = Vec::new();

    // Every response funnels through one writer task: requests are handled *concurrently* (a
    // parked long-poll like await_omen must not head-of-line-block an `ls`
    // issued after it), but stdout is a shared byte stream, so a single consumer serializes the
    // frames. Responses may therefore complete out of order — JSON-RPC ties a reply to its request
    // by `id`, so that's fine.
    let (resp_tx, mut resp_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(msg) = resp_rx.recv().await {
            if let Err(e) = write_message(&mut stdout, &msg).await {
                eprintln!("cv-mcp: stdout write failed: {e:#}");
                break;
            }
            // The frame (and the tool call that built it) is done: hand its freed heap back.
            drop(msg);
            tokio::task::spawn_blocking(release_freed_heap);
        }
    });

    loop {
        buf.clear();
        // Read raw bytes, not into a String: a single non-UTF-8 byte on stdin would make `read_line`
        // error out. Lossy-decode instead so one bad frame is tolerated rather than fatal, and keep
        // transient read errors non-fatal too — only EOF (or a dead stdin) ends the server.
        let n = match reader.read_until(b'\n', &mut buf).await {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("cv-mcp: stdin read error, shutting down: {e:#}");
                break;
            }
        };
        if n == 0 {
            break; // EOF
        }
        let line = String::from_utf8_lossy(&buf);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                // Parse error: id unknown, reply with null id per JSON-RPC.
                let _ = resp_tx.send(error_response(Value::Null, -32700, &format!("Parse error: {e}")));
                continue;
            }
        };

        // A notification (no `id`) gets no response; just acknowledge by ignoring.
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        let params = req.get("params").cloned();

        // Dispatch each request on its own task so slow handlers never block the read loop.
        let tx = resp_tx.clone();
        tokio::spawn(async move {
            if let Some(resp) = handle(&method, params, id).await {
                let _ = tx.send(resp);
            }
        });
    }

    // Flush responses already queued (or about to land) before exiting; don't hang around for the
    // full length of an in-flight long-poll — the host that would read its answer is gone.
    drop(resp_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;

    Ok(())
}

async fn handle(method: &str, params: Option<Value>, id: Option<Value>) -> Option<Value> {
    // A request without an `id` is a JSON-RPC *notification*: the method still runs (a
    // notification tools/call may have side effects worth performing), but per spec the server
    // MUST NOT reply — not even with `id: null`.
    let is_notification = id.is_none();
    let id = id.unwrap_or(Value::Null);

    let response = match method {
        "initialize" => Some(ok_response(id, initialize_result())),
        "ping" => Some(ok_response(id, json!({}))),
        "notifications/initialized" | "notifications/cancelled" => None,
        "tools/list" => Some(ok_response(id, json!({ "tools": tool_list() }))),
        "tools/call" => {
            let params = params.unwrap_or(Value::Null);
            let name = params.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

            // Run the (synchronous, fs-bound) tool body off the async reactor.
            let result = tokio::task::spawn_blocking(move || call_tool(&name, &args)).await;

            match result {
                Ok(Ok(text)) => Some(ok_response(id, tool_text_result(&text, false))),
                // A malformed *argument* (wrong type, negative count, …) is the caller's protocol
                // mistake: JSON-RPC -32602 Invalid params, not a tool-execution failure.
                Ok(Err(e)) if e.downcast_ref::<InvalidParams>().is_some() => {
                    Some(error_response(id, -32602, &format!("{e}")))
                }
                Ok(Err(e)) => {
                    // Tool execution errors are reported via the result's isError flag, not a
                    // protocol-level error, per the MCP spec.
                    Some(ok_response(id, tool_text_result(&format!("error: {e:#}"), true)))
                }
                Err(join_err) => Some(error_response(id, -32603, &format!("internal task error: {join_err}"))),
            }
        }
        _ => Some(error_response(id, -32601, &format!("Method not found: {method}"))),
    };
    if is_notification {
        None
    } else {
        response
    }
}

// ---------------------------------------------------------------------------
// MCP boilerplate
// ---------------------------------------------------------------------------

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "clustervision",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": instructions(),
    })
}

/// The server blurb a host shows the model. It names the tools that actually exist (they are
/// generated from `cv`, so this text is the only hand-written place they could go stale) and
/// enumerates harnesses from `Harness::ALL` rather than a frozen list of five.
fn instructions() -> String {
    let harnesses: Vec<&str> = Harness::ALL.iter().map(|h| h.as_str()).collect();
    format!(
        "clustervision lets you read OTHER agents' sessions across harnesses and time. \
         Start with `ls` (add `cwd` to see what happened — or is happening — in the current project, \
         `query` for the filter calculus) and `search` for a past conversation by content; then `show` \
         a transcript (windowed by default: `last` 50 messages, `max_bytes` 200000 — pass `first`, \
         `last`, `range`, `around`+`context` or `max_bytes` to move the window) and `cat` one tool \
         call's full output by its tool_use_id. `pack` builds task-relevant context out of the whole \
         corpus; `doctor` explains why a session's context window keeps filling; `workflow`, `tree` \
         and `show --subagents` open up sub-agent forests. `prune`, `rewind`, `splice`, `loom`, \
         `port` and `redact` produce NEW sessions from existing ones without touching the source \
         (`rewind` = an agent as of a past message or commit, resumable). \
         To coordinate with sibling agents live, use `await_omen`/`observe_stream` (watch another \
         agent's output) and the `task_*` tools. \
         Harnesses cv reads: {}.",
        harnesses.join(", ")
    )
}

/// Every tool: the CLI-generated commands first (they are the ones an agent reaches for), then the
/// hand-written MCP-only tools. A generated name never shadows a hand-written one.
fn tool_list() -> Value {
    // Several json! blocks concatenated: one macro invocation over the whole list blows the macro
    // recursion limit.
    let mut tools = base_tool_list();
    if let (Value::Array(list), Value::Array(tasks)) = (&mut tools, task_tool_list()) {
        list.extend(tasks);
    }
    let Value::Array(list) = &mut tools else {
        return tools;
    };
    let hand_written: std::collections::HashSet<String> = list
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect();
    if let Some(cli) = cli() {
        let mut generated: Vec<Value> = cli
            .tool_list()
            .into_iter()
            .filter(|t| !hand_written.contains(t["name"].as_str().unwrap_or("")))
            .collect();
        generated.append(list);
        *list = generated;
    }
    tools
}

fn task_tool_list() -> Value {
    json!([
        {
            "name": "task_open",
            "description": "Open a durable fleet task (dispatch object): a lifecycle open → claimed → done, notes, tags, blockers; completion is self-reported and labeled so. Returns the new task.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Short imperative title." },
                    "body": { "type": "string", "description": "Details / acceptance criteria." },
                    "repo": { "type": "string", "description": "Absolute path of the git repo the work happens in (`task_list` filters on it; `cv task sweep` reads it)." },
                    "issue": { "type": "string", "description": "External issue/work handle, free-form." },
                    "assignee": { "type": "string", "description": "Endpoint this task is assigned to, if pre-assigned." },
                    "from": { "type": "string", "description": "Who's opening. Defaults to $CV_ENDPOINT, else 'agent'." }
                },
                "required": ["title"]
            }
        },
        {
            "name": "task_list",
            "description": "List fleet tasks (non-terminal by default). Filter by state (open|claimed|resolved|done|abandoned|superseded), assignee, or repo.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "state": { "type": "string", "description": "Effective-state filter." },
                    "assignee": { "type": "string", "description": "Assignee endpoint filter." },
                    "repo": { "type": "string", "description": "Repo path filter." },
                    "tag": { "type": "string", "description": "Only tasks carrying this tag." },
                    "all": { "type": "boolean", "description": "Include terminal tasks (default false)." }
                }
            }
        },
        {
            "name": "task_show",
            "description": "Show one task's full projection: state, assignee, notes, tags, blockers, the decision facet, the pinned STATUS.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Task id (unique prefix ok)." }
                },
                "required": ["id"]
            }
        },
        {
            "name": "task_claim",
            "description": "Claim an open task for yourself. First writer wins — a durable, race-free claim (losers get a rejection, not a duplicate).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Task id (prefix ok)." },
                    "from": { "type": "string", "description": "Your endpoint. Defaults to $CV_ENDPOINT; ERROR when neither is set (identity-bearing events must record who acted)." }
                },
                "required": ["id"]
            }
        },
        {
            "name": "task_release",
            "description": "Release your claim on a task back to open.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Task id (prefix ok)." },
                    "from": { "type": "string", "description": "Your endpoint. Defaults to $CV_ENDPOINT; ERROR when neither is set (identity-bearing events must record who acted)." }
                },
                "required": ["id"]
            }
        },
        {
            "name": "task_note",
            "description": "Record a progress note on a task (never changes state).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Task id (prefix ok)." },
                    "text": { "type": "string", "description": "The note." },
                    "session_ref": { "type": "string", "description": "Your cv session id, for provenance." },
                    "from": { "type": "string", "description": "Your endpoint. Defaults to $CV_ENDPOINT, else 'agent'." }
                },
                "required": ["id", "text"]
            }
        },
        {
            "name": "task_done",
            "description": "Complete a task, optionally pointing at observable evidence. Completion is SELF-REPORTED and labeled so — `observed` is free text cv records, never a claim cv checked. Refused on a decision (answer it with `cv task resolve`).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Task id (prefix ok)." },
                    "observed": { "type": "string", "description": "Pointer to observable evidence (URL, path, session id)." },
                    "from": { "type": "string", "description": "Your endpoint. Defaults to $CV_ENDPOINT, else 'agent'." }
                },
                "required": ["id"]
            }
        },
        {
            "name": "task_abandon",
            "description": "Kill a task (always allowed on a non-terminal task).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Task id (prefix ok)." },
                    "reason": { "type": "string", "description": "Why." },
                    "from": { "type": "string", "description": "Your endpoint. Defaults to $CV_ENDPOINT, else 'agent'." }
                },
                "required": ["id"]
            }
        },
        {
            "name": "task_inbox",
            "description": "What needs `who`: decisions they owe, decisions made for them provisionally, tasks assigned to them, tasks they claimed. Stalest first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "who": { "type": "string", "description": "Endpoint to compute the inbox for. Defaults to $CV_ENDPOINT; ERROR when neither is given." }
                }
            }
        }
    ])
}

fn base_tool_list() -> Value {
    json!([
        {
            "name": "await_omen",
            "description": "Block until another agent's session emits a message matching a regex, then return it. Watches live for new/appended messages (text, thinking, or tool results) across harnesses. Use to wait on a sibling agent — e.g. await_omen(regex='BUILD (PASSED|FAILED)', cwd_contains='/myproj'). Returns when matched or when timeout_secs elapses.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "regex": { "type": "string", "description": "Rust regex to match against newly-emitted message text." },
                    "harness": { "type": "string", "description": "Only watch this harness." },
                    "cwd_contains": { "type": "string", "description": "Only watch sessions whose cwd contains this substring." },
                    "timeout_secs": { "type": "number", "description": "Give up after this many seconds (default 120)." },
                    "interval_secs": { "type": "number", "description": "Poll interval (default 2)." }
                },
                "required": ["regex"]
            }
        },
        {
            "name": "observe_stream",
            "description": "The NON-BLOCKING sibling of await_omen: drain the newly-appended messages from another agent's session(s) since an opaque cursor and return IMMEDIATELY with a fresh cursor — so a senior orchestrator can poll a junior agent's live activity on its own cadence (await_omen blocks until ONE regex matches; observe_stream tails the incremental stream of everything). The first call (no cursor) records a baseline and returns no backlog — pass the returned cursor to the next call to get only what was appended since. Read-only (never writes the board); bounded by max_messages + char_cap. Scope it with cwd_contains and/or harness, e.g. observe_stream(cwd_contains='/junior-proj').",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "cwd_contains": { "type": "string", "description": "Only observe sessions whose recorded cwd contains this substring." },
                    "harness": { "type": "string", "description": "Only observe this harness: claude, codex, grok, opencode, gemini." },
                    "since_cursor": { "type": "string", "description": "Opaque cursor from a previous observe_stream call. Omit on the first call to start from the current tail (baseline; returns no messages)." },
                    "max_messages": { "type": "number", "description": "Max messages to return this call (default 50). Remaining unread messages are reported as more_pending; poll again with the returned cursor to drain them." },
                    "char_cap": { "type": "number", "description": "Optional cap on total characters of message text returned; the last message is truncated to fit (default 16000)." }
                }
            }
        }
    ])
}

fn ok_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_text_result(text: &str, is_error: bool) -> Value {
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": is_error
    })
}

/// Return freed heap pages to the OS after a request.
///
/// cv-mcp lives as long as the host session (days), and the in-process tools (`observe_stream`,
/// `await_omen`, `task_*`) parse whole transcripts, so one call can transiently allocate ~1 GB
/// across several blocking-pool threads. glibc frees that memory but keeps it mapped in each
/// thread's arena: without a trim, RSS stays at the high-water mark of the largest call ever made,
/// times however many arenas were touched. `malloc_trim(0)` releases the free pages of every arena
/// (measured: 938 MB → 77 MB after one call mix). The live data is untouched, so behaviour is
/// unchanged; the cost is a walk of the free lists, small next to a tool call. Other allocators
/// (musl, macOS) return memory on their own, so this is a no-op there.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn release_freed_heap() {
    extern "C" {
        fn malloc_trim(pad: usize) -> std::os::raw::c_int;
    }
    // SAFETY: glibc's malloc_trim takes only a padding size, touches no caller memory, and
    // locks each arena while it trims, so it is safe to call from any thread at any time.
    unsafe {
        malloc_trim(0);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn release_freed_heap() {}

async fn write_message(stdout: &mut tokio::io::Stdout, msg: &Value) -> anyhow::Result<()> {
    let mut buf = serde_json::to_vec(msg)?;
    buf.push(b'\n');
    stdout.write_all(&buf).await?;
    stdout.flush().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tool implementations (synchronous; called via spawn_blocking)
// ---------------------------------------------------------------------------

fn call_tool(name: &str, args: &Value) -> anyhow::Result<String> {
    match name {
        "await_omen" => await_omen(args),
        "observe_stream" => observe_stream(args),
        // Removed in 0.15.0 (the coordination board): a pointer, never a silent "unknown tool".
        n if n.starts_with("board_") => anyhow::bail!(
            "{n} was removed in 0.15.0 with the coordination board — the task log is the fleet's record: \
             `task_note` to say something, `task_claim` to hold work, `await_omen`/`observe_stream` to wait on a sibling"
        ),
        "task_open" => task_open(args),
        "task_list" => task_list(args),
        "task_show" => task_show(args),
        "task_claim" => task_simple(args, |from| cv_core::task::TaskEventKind::Claimed { assignee: from }),
        "task_release" => task_simple(args, |_| cv_core::task::TaskEventKind::Released {}),
        "task_note" => task_note(args),
        "task_done" => task_done(args),
        "task_abandon" => task_abandon(args),
        "task_inbox" => task_inbox(args),
        // Removed in 0.15.0 with the land facet: a pointer, never a silent "unknown tool".
        "task_propose" | "task_pass" | "task_refute" | "task_verify" | "task_debt" | "task_stats" => {
            anyhow::bail!(
                "{name} was removed in 0.15.0 with the task land facet — completion is self-reported \
                 and labeled: `task_done` with `observed`, `task_note` for evidence"
            )
        }
        // Everything else is a CLI-generated command tool: shell out to `cv`.
        other => match cli() {
            Some(c) if c.find(other).is_some() => c.call(other, args),
            Some(_) => anyhow::bail!("unknown tool: {other}"),
            None => anyhow::bail!(
                "unknown tool: {other} (no `cv` binary was found at startup, so the command tools \
                 are not registered — set $CV_BIN)"
            ),
        },
    }
}

// ---------------------------------------------------------------------------
// Board coordination primitives (request/reply, claim/lease, presence, ack)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// task_* tools — thin wrappers over cv_core::task.
// ---------------------------------------------------------------------------

/// Identity resolution (G4) for verbs where the actor is bookkeeping, not semantics
/// (open/note/done/abandon): explicit `from` param, else the spawner-set
/// `CV_ENDPOINT` (the MCP server inherits the agent's environment), else the legacy "agent"
/// sink. Shared logic lives in [`cv_core::task::actor`]; only MCP's default sink is chosen here.
fn task_from_or_agent(args: &Value) -> String {
    cv_core::task::actor(arg_str(args, "from").map(String::from), "agent")
}

/// Identity for identity-BEARING tools (task_claim/release), whose endpoint string keys inbox
/// semantics: unresolvable identity is a hard error, never a silent fall-through to the shared
/// "agent" sink ([`cv_core::task::require_actor`]).
fn require_task_from(args: &Value) -> anyhow::Result<String> {
    cv_core::task::require_actor(arg_str(args, "from").map(String::from), "`from`")
}

/// Replay the default task store; error on unreadable log, surface warnings inline.
fn task_replay() -> anyhow::Result<(cv_core::task::ReplayOutcome, Vec<String>)> {
    let outcome = cv_core::task::replay()?;
    let warnings = outcome.warnings.clone();
    Ok((outcome, warnings))
}

/// Resolve an id prefix against the replayed model.
fn task_resolve(outcome: &cv_core::task::ReplayOutcome, prefix: &str) -> anyhow::Result<String> {
    cv_core::task::resolve_id(&outcome.model, prefix)
        .map(String::from)
        .map_err(|e| anyhow::anyhow!(e))
}

/// Append an agent event through the shared core path; return a JSON report of the event and
/// new state.
fn task_append(
    task_id: Option<&str>,
    from: &str,
    kind: cv_core::task::TaskEventKind,
    warnings: Vec<String>,
) -> anyhow::Result<String> {
    let store = cv_core::task::TaskStore::default_store();
    let report = cv_core::task::append(&store, task_id, from, kind, warnings)?;
    Ok(serde_json::to_string_pretty(&json!({
        "event": report.event,
        "effective_state": report.effective_state,
        "warnings": report.warnings,
    }))?)
}

fn task_open(args: &Value) -> anyhow::Result<String> {
    let title = arg_str(args, "title").context("`title` is required")?.to_string();
    if args.get("channel").is_some() {
        anyhow::bail!(
            "`channel` was removed in 0.15.0 with the coordination board — task events are mirrored nowhere; \
             follow them with `cv task events --since <id>`"
        );
    }
    let repo = match arg_str(args, "repo") {
        Some(r) => Some(
            std::path::Path::new(r)
                .canonicalize()
                .with_context(|| format!("repo {r} not found"))?,
        ),
        None => None,
    };
    task_append(
        None,
        &task_from_or_agent(args),
        cv_core::task::TaskEventKind::Opened {
            title,
            body: arg_str(args, "body").unwrap_or("").to_string(),
            repo,
            issue: arg_str(args, "issue").map(String::from),
            channel: None,
            assignee: arg_str(args, "assignee").map(String::from),
        },
        Vec::new(),
    )
}

fn task_list(args: &Value) -> anyhow::Result<String> {
    let (outcome, warnings) = task_replay()?;
    let filter = cv_core::task::TaskFilter {
        state: arg_str(args, "state").map(String::from),
        assignee: arg_str(args, "assignee").map(String::from),
        repo: arg_str(args, "repo").map(std::path::PathBuf::from),
        include_terminal: args.get("all").and_then(Value::as_bool).unwrap_or(false),
        tag: arg_str(args, "tag").map(String::from),
        // MCP/HTTP see the whole store: scoping is the shell's default, not the wire's.
        ..Default::default()
    };
    let tasks: Vec<cv_core::task::TaskRow> = cv_core::task::list(&outcome.model, &filter)
        .map_err(|e| anyhow::anyhow!(e))?
        .into_iter()
        .map(cv_core::task::TaskRow::brief)
        .collect();
    Ok(serde_json::to_string_pretty(
        &json!({ "tasks": tasks, "warnings": warnings }),
    )?)
}

fn task_show(args: &Value) -> anyhow::Result<String> {
    let (outcome, warnings) = task_replay()?;
    let id = task_resolve(&outcome, arg_str(args, "id").context("`id` is required")?)?;
    Ok(serde_json::to_string_pretty(&json!({
        "task": outcome.model.tasks[&id],
        "effective_state": cv_core::task::effective_display(&outcome.model.tasks[&id]),
        "warnings": warnings,
    }))?)
}

/// Shared shape for claim/release: resolve id, build the kind from `from`, append.
fn task_simple(args: &Value, kind: impl FnOnce(String) -> cv_core::task::TaskEventKind) -> anyhow::Result<String> {
    let (outcome, warnings) = task_replay()?;
    let id = task_resolve(&outcome, arg_str(args, "id").context("`id` is required")?)?;
    let from = require_task_from(args)?;
    task_append(Some(&id), &from.clone(), kind(from), warnings)
}

fn task_note(args: &Value) -> anyhow::Result<String> {
    let (outcome, warnings) = task_replay()?;
    let id = task_resolve(&outcome, arg_str(args, "id").context("`id` is required")?)?;
    task_append(
        Some(&id),
        &task_from_or_agent(args),
        cv_core::task::TaskEventKind::Noted {
            text: arg_str(args, "text").context("`text` is required")?.to_string(),
            session_ref: arg_str(args, "session_ref").map(String::from),
        },
        warnings,
    )
}

fn task_done(args: &Value) -> anyhow::Result<String> {
    let (outcome, warnings) = task_replay()?;
    let id = task_resolve(&outcome, arg_str(args, "id").context("`id` is required")?)?;
    for retired in ["check_cmd", "check_file", "check_http"] {
        if args.get(retired).is_some() {
            anyhow::bail!(
                "`{retired}` was removed in 0.15.0 — completion is self-reported; put the evidence in `observed`"
            );
        }
    }
    task_append(
        Some(&id),
        &task_from_or_agent(args),
        cv_core::task::TaskEventKind::Done {
            observed: arg_str(args, "observed").map(String::from),
            check: None,
        },
        warnings,
    )
}

fn task_abandon(args: &Value) -> anyhow::Result<String> {
    let (outcome, warnings) = task_replay()?;
    let id = task_resolve(&outcome, arg_str(args, "id").context("`id` is required")?)?;
    task_append(
        Some(&id),
        &task_from_or_agent(args),
        cv_core::task::TaskEventKind::Abandoned {
            reason: arg_str(args, "reason").unwrap_or("no reason given").to_string(),
        },
        warnings,
    )
}

fn task_inbox(args: &Value) -> anyhow::Result<String> {
    let (outcome, warnings) = task_replay()?;
    // `who` still wins when given; a bare call means "my inbox" via the spawner-set CV_ENDPOINT.
    let who = arg_str(args, "who")
        .map(String::from)
        .or_else(cv_core::task::default_endpoint)
        .context("`who` is required (or set CV_ENDPOINT)")?;
    let entries = cv_core::task::InboxRow::compute(&outcome.model, &who);
    Ok(serde_json::to_string_pretty(
        &json!({ "inbox": entries, "warnings": warnings }),
    )?)
}

/// Block until a newly-emitted message matches `regex` (or `timeout_secs` elapses). The polling loop
/// runs on a blocking worker thread (tools are dispatched via spawn_blocking), so blocking is fine.
fn await_omen(args: &Value) -> anyhow::Result<String> {
    let pattern = arg_str(args, "regex").context("`regex` is required")?;
    let re = regex::Regex::new(pattern).with_context(|| format!("invalid regex: {pattern:?}"))?;
    let filter = Filter {
        harness: parse_harness(args)?,
        cwd_contains: arg_str(args, "cwd_contains").map(str::to_string),
    };
    let timeout = Duration::from_secs(arg_usize(args, "timeout_secs", 120)? as u64);
    let interval = Duration::from_secs(arg_usize(args, "interval_secs", 2)?.max(1) as u64);

    // Only react to activity from now on (not the existing backlog).
    let mut watcher = Watcher::new(filter, false);
    let start = Instant::now();
    loop {
        for ev in watcher.poll() {
            for m in &ev.new_messages {
                let text = message_text(m);
                if let Some(hit) = re.find(&text) {
                    let snippet_start = hit.start().saturating_sub(80);
                    let snippet_end = (hit.end() + 80).min(text.len());
                    return Ok(serde_json::to_string_pretty(&json!({
                        "matched": true,
                        "harness": ev.reference.harness.as_str(),
                        "id": ev.reference.id,
                        "cwd": ev.reference.cwd.as_ref().map(|p| p.to_string_lossy()),
                        "role": cv_core::render::role_label(m.role),
                        "matched_text": &text[char_floor(&text, snippet_start)..char_ceil(&text, snippet_end)],
                        "event": match ev.kind { cv_core::watch::EventKind::New => "new_session", _ => "appended" },
                    }))?);
                }
            }
        }
        if start.elapsed() >= timeout {
            return Ok(serde_json::to_string_pretty(&json!({
                "matched": false,
                "timed_out": true,
                "waited_secs": start.elapsed().as_secs(),
            }))?);
        }
        std::thread::sleep(interval);
    }
}

// ---------------------------------------------------------------------------
// observe_stream — the non-blocking tail of await_omen
// ---------------------------------------------------------------------------

/// Non-blocking incremental tail: the messages appended to matching sessions since `since_cursor`
/// plus a fresh cursor, returned at once. It is one `Watcher` poll with the watcher's state carried
/// by the caller ([`cv_core::watch::Cursor`]) instead of by a process: the first call (no cursor)
/// records a baseline and emits nothing, so a caller follows from "now" rather than dumping
/// history. Read-only. Sessions come from the catalog fast path (`cv_core::sessions`, ~ms) rather
/// than a stat-the-fleet scan (~s), because this is *polled*; the catalog's staleness probe
/// re-stats the most recently updated transcripts, and an actively tailed session is one of them.
fn observe_stream(args: &Value) -> anyhow::Result<String> {
    let filter = Filter {
        harness: parse_harness(args)?,
        cwd_contains: arg_str(args, "cwd_contains").map(str::to_string),
    };
    let max_messages = arg_usize(args, "max_messages", 50)?.max(1);
    let char_cap = arg_usize(args, "char_cap", 16_000)?.max(1);
    let prev = arg_str(args, "since_cursor").and_then(Cursor::decode);
    let baseline = prev.is_none();
    let mut watcher = match prev {
        Some(c) => Watcher::from_cursor(filter, c, cv_core::sessions),
        None => Watcher::with_discovery(filter, false, cv_core::sessions),
    };

    let mut out_msgs: Vec<Value> = Vec::new();
    let mut chars_used = 0usize;
    let more_pending = !baseline
        && watcher.poll_bounded(max_messages, |ev| {
            let r = &ev.reference;
            let mut taken = 0;
            for m in &ev.new_messages {
                let text = message_text(m);
                let room = char_cap.saturating_sub(chars_used);
                if room == 0 {
                    break;
                }
                let text = if text.len() > room {
                    format!("{}…", &text[..floor_char_boundary(&text, room)])
                } else {
                    text
                };
                chars_used += text.len();
                out_msgs.push(json!({
                    "harness": r.harness.as_str(),
                    "session_id": r.id,
                    "cwd": r.cwd.as_ref().map(|p| p.to_string_lossy().to_string()),
                    "role": cv_core::render::role_label(m.role),
                    "timestamp": m.timestamp.map(|t| t.to_rfc3339()),
                    "text": text,
                }));
                taken += 1;
            }
            taken
        });

    Ok(serde_json::to_string_pretty(&json!({
        "baseline": baseline,
        "messages": out_msgs,
        "count": out_msgs.len(),
        "more_pending": more_pending,
        "cursor": watcher.to_cursor().encode(),
    }))?)
}

/// All matchable text in a message: text + thinking + tool-result content.
fn message_text(m: &cv_core::Message) -> String {
    let mut out = String::new();
    for b in &m.content {
        match b {
            Block::Text { text } | Block::Thinking { text, .. } => {
                out.push_str(text);
                out.push('\n');
            }
            Block::ToolResult { content, .. } => {
                out.push_str(content);
                out.push('\n');
            }
            Block::ToolUse { name, input, .. } => {
                out.push_str(name);
                out.push(' ');
                out.push_str(&input.to_string());
                out.push('\n');
            }
            Block::File { path, source, .. } => {
                if let Some(p) = path.as_deref().or(source.as_deref()) {
                    out.push_str(p);
                    out.push('\n');
                }
            }
            Block::Image { .. } => {}
        }
    }
    out
}

fn char_floor(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
fn char_ceil(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// A malformed caller-supplied argument. Mapped to JSON-RPC `-32602 Invalid params` at the
/// protocol layer (unlike tool *execution* failures, which surface as `isError` tool results).
#[derive(Debug)]
struct InvalidParams(String);

impl std::fmt::Display for InvalidParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Invalid params: {}", self.0)
    }
}

impl std::error::Error for InvalidParams {}

/// Read an optional non-negative-integer argument. Absent (or JSON null) yields `default`; a
/// present-but-malformed value (a string, a negative, a fractional float) is an [`InvalidParams`]
/// error rather than a silent fallback to the default. Integral floats are tolerated — some
/// clients serialize `20` as `20.0`.
fn arg_usize(args: &Value, key: &str, default: usize) -> anyhow::Result<usize> {
    let v = match args.get(key) {
        None | Some(Value::Null) => return Ok(default),
        Some(v) => v,
    };
    if let Some(n) = v.as_u64() {
        return Ok(n as usize);
    }
    if let Some(f) = v.as_f64() {
        if f >= 0.0 && f.fract() == 0.0 && f <= usize::MAX as f64 {
            return Ok(f as usize);
        }
    }
    Err(InvalidParams(format!("`{key}` must be a non-negative integer, got {v}")).into())
}

fn parse_harness(args: &Value) -> anyhow::Result<Option<Harness>> {
    match arg_str(args, "harness") {
        None => Ok(None),
        Some(s) => match Harness::parse(s) {
            Some(h) => Ok(Some(h)),
            None => anyhow::bail!(
                "unknown harness: {s:?} (expected one of {})",
                Harness::ALL.iter().map(|h| h.as_str()).collect::<Vec<_>>().join(", ")
            ),
        },
    }
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
