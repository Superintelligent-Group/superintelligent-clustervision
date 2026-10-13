//! # `cv mcp` — clustervision as an MCP server
//!
//! A stdio [Model Context Protocol](https://modelcontextprotocol.io) server that lets a *running*
//! coding agent (Claude Code, Codex, Gemini, …) search and read the sessions of **other** agents —
//! within the same project, back through time, and across harnesses. "Let agents read other agents'
//! minds." It is a thin front-end over the [`cv_core`] library.
//!
//! ## Registering with a host
//!
//! Point your agent at `cv mcp`. For Claude Code:
//!
//! ```text
//! claude mcp add clustervision -- /absolute/path/to/cv mcp
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
//!    `export`, `pack`, `doctor`, …) and per `cv task` subcommand (`task_open`, `task_claim`, …),
//!    built at startup from the in-process clap tree (the same `commands_json`
//!    `cv schema --commands --json` prints), so tool names, flags, harness lists
//!    and output shapes cannot drift from the binary. A call runs this executable as
//!    `cv <command> <args…> --json`. See [`super::mcp_tools`].
//! 2. **Hand-written, MCP-only** — the two tools with no CLI equivalent, because they block or
//!    hold a cursor:
//!    - `await_omen(regex, …)` — block until another agent's session emits a matching message.
//!    - `observe_stream(cwd_contains?, harness?, since_cursor?, max_messages=50, char_cap?)` — the
//!      non-blocking sibling: drains the *newly-appended* messages since an opaque cursor and
//!      returns immediately with a fresh cursor, so a senior agent can poll a junior's activity on
//!      its own cadence. Bounded + read-only.
//!
//! `show` is windowed by default over MCP (`--last 50 --max-bytes 200000`) so a whole transcript is
//! never what an agent accidentally pulls into its context.

use super::mcp_tools::CliTools;
use anyhow::Context as _;
use cv_core::scan::{ceil_char, floor_char};
use cv_core::watch::{Cursor, Filter, Watcher};
use cv_core::Harness;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// The CLI-generated tools, built once at startup from the clap tree.
static CLI: OnceLock<Option<CliTools>> = OnceLock::new();

fn cli() -> Option<&'static CliTools> {
    CLI.get().and_then(Option::as_ref)
}

const PROTOCOL_VERSION: &str = "2025-06-18";

/// A request handler's stack. Tool bodies parse whole transcripts; give them main's room.
const HANDLER_STACK: usize = 8 * 1024 * 1024;

/// Serve MCP over stdio until stdin closes.
pub(crate) fn cmd_mcp(tree: &clap::Command) -> anyhow::Result<()> {
    eprintln!("cv mcp: clustervision MCP server starting (stdio JSON-RPC)");
    let generated = match CliTools::from_tree(tree) {
        Ok(c) => {
            eprintln!("cv mcp: generated {} tools from the CLI", c.commands().len());
            Some(c)
        }
        Err(e) => {
            eprintln!("cv mcp: no CLI-generated tools ({e:#}); serving the MCP-only tools");
            None
        }
    };
    let _ = CLI.set(generated);

    // Every response funnels through one writer thread: requests are handled *concurrently* (a
    // parked long-poll like await_omen must not head-of-line-block an `ls` issued after it), but
    // stdout is a shared byte stream, so a single consumer serializes the frames. Responses may
    // complete out of order — JSON-RPC ties a reply to its request by `id`, so that's fine.
    let cancelled = Arc::new(AtomicBool::new(false));
    let writer_cancelled = cancelled.clone();
    let (resp_tx, resp_rx) = mpsc::channel::<Response>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        let mut stdout = std::io::BufWriter::with_capacity(64 * 1024, std::io::stdout().lock());
        for msg in resp_rx {
            if let Err(e) = write_message(&mut stdout, &msg) {
                eprintln!("cv mcp: stdout write failed: {e:#}");
                break;
            }
            // The frame (and the tool call that built it) is done: hand its freed heap back.
            drop(msg);
            release_freed_heap();
        }
        writer_cancelled.store(true, Ordering::Relaxed);
        let _ = done_tx.send(());
    });

    let mut reader = std::io::stdin().lock();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        // Read raw bytes, not into a String: a single non-UTF-8 byte on stdin would make
        // `read_line` error out. Lossy-decode instead so one bad frame is tolerated rather than
        // fatal, and keep transient read errors non-fatal too — only EOF ends the server.
        let n = match reader.read_until(b'\n', &mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("cv mcp: stdin read error, shutting down: {e:#}");
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
                let _ = resp_tx.send(Response::Json(error_response(
                    Value::Null,
                    -32700,
                    &format!("Parse error: {e}"),
                )));
                continue;
            }
        };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        let params = req.get("params").cloned();

        // Each request on its own thread so slow handlers never block the read loop.
        let tx = resp_tx.clone();
        let cancelled = cancelled.clone();
        let spawned = std::thread::Builder::new().stack_size(HANDLER_STACK).spawn(move || {
            if let Some(resp) = handle(&method, params, id, &cancelled) {
                let _ = tx.send(resp);
            }
        });
        if let Err(e) = spawned {
            eprintln!("cv mcp: could not start a request thread: {e}");
        }
    }

    // Flush responses already queued (or about to land) before exiting; don't hang around for the
    // full length of an in-flight long-poll — the host that would read its answer is gone.
    drop(resp_tx);
    if done_rx.recv_timeout(Duration::from_secs(2)).is_err() {
        cancelled.store(true, Ordering::Relaxed);
        let _ = done_rx.recv_timeout(Duration::from_secs(1));
    }
    Ok(())
}

fn handle(method: &str, params: Option<Value>, id: Option<Value>, cancelled: &AtomicBool) -> Option<Response> {
    let _reads = cv_core::source::read_scope();
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

            // A panicking tool costs its own reply, not the server.
            let result = std::panic::catch_unwind(move || call_tool(&name, &args, cancelled));

            match result {
                Ok(Ok(ToolOutput::Text(text))) => Some(ok_response(id, tool_text_result(&text, false))),
                Ok(Ok(ToolOutput::Command(output))) => {
                    return (!is_notification).then_some(Response::Command { id, output });
                }
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
                Err(_) => Some(error_response(id, -32603, "internal error: the tool panicked")),
            }
        }
        _ => Some(error_response(id, -32601, &format!("Method not found: {method}"))),
    };
    if is_notification {
        None
    } else {
        response.map(Response::Json)
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
         corpus; `doctor` explains why a session's context window keeps filling; `workflow`, `show` \
         with `tree` or `subagents` open up sub-agent forests, and `show` with `kind` reads only what \
         the person said (`prompt,answer`) or the compaction seams. `prune`, `rewind`, `distill`, `splice` \
         and `port` produce NEW sessions from existing ones without touching the source \
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
    let mut tools = base_tool_list();
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

pub(crate) fn base_tool_list() -> Value {
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
                    "harness": { "type": "string", "description": "Only observe this harness (any name `cv ls --harness` takes)." },
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
/// `cv mcp` lives as long as the host session (days), and the in-process tools (`observe_stream`,
/// `await_omen`, the generated tools' children) parse whole transcripts, so one call can transiently allocate ~1 GB
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

enum Response {
    Json(Value),
    Command {
        id: Value,
        output: super::mcp_tools::CommandOutput,
    },
}
fn write_message(stdout: &mut impl Write, msg: &Response) -> anyhow::Result<()> {
    match msg {
        Response::Json(value) => serde_json::to_writer(&mut *stdout, value)?,
        Response::Command { id, output } => write_command_response(stdout, id, output)?,
    }
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

/// Publish the fixed JSON-RPC envelope through the server's single writer. Staged text is
/// immutable; serde handles the arbitrary request id and the shared writer handles escaping.
fn write_command_response(
    out: &mut impl Write,
    id: &Value,
    output: &super::mcp_tools::CommandOutput,
) -> anyhow::Result<()> {
    use std::io::Read;
    out.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":")?;
    serde_json::to_writer(&mut *out, id)?;
    out.write_all(b",\"result\":{\"content\":[{\"type\":\"text\",\"text\":")?;
    if let Some(prefix) = &output.error_prefix {
        let detail = if output.stderr.trimmed.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        };
        cv_core::projection::write_json_string(
            out,
            std::io::Cursor::new(prefix.as_bytes()).chain(detail.reader(true)?),
        )?;
    } else {
        cv_core::projection::write_json_string(out, output.stdout.reader(false)?)?;
        if output.stderr.len > 0 {
            out.write_all(b"},{\"type\":\"text\",\"text\":")?;
            cv_core::projection::write_json_string(out, output.stderr.reader(false)?)?;
        }
    }
    out.write_all(if output.error_prefix.is_some() {
        b"}],\"isError\":true}}"
    } else {
        b"}],\"isError\":false}}"
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tool implementations (synchronous; each call runs on its request's thread)
// ---------------------------------------------------------------------------

enum ToolOutput {
    Text(String),
    Command(super::mcp_tools::CommandOutput),
}
fn call_tool(name: &str, args: &Value, cancelled: &AtomicBool) -> anyhow::Result<ToolOutput> {
    match name {
        "await_omen" => await_omen(args).map(ToolOutput::Text),
        "observe_stream" => observe_stream(args).map(ToolOutput::Text),
        // Removed in 0.15.0 (the coordination board): a pointer, never a silent "unknown tool".
        n if n.starts_with("board_") => anyhow::bail!(
            "{n} was removed in 0.15.0 with the coordination board — the task log is the fleet's record: \
             `task_note` to say something, `task_claim` to hold work, `await_omen`/`observe_stream` to wait on a sibling"
        ),
        // Removed in 0.15.0 with the land facet: a pointer, never a silent "unknown tool".
        "task_propose" | "task_pass" | "task_refute" | "task_verify" | "task_debt" | "task_stats" => {
            anyhow::bail!(
                "{name} was removed in 0.15.0 with the task land facet — completion is self-reported \
                 and labeled: `task_done` with `observed`, `task_note` for evidence"
            )
        }
        // Everything else is a CLI-generated command tool: shell out to `cv`.
        other => match cli() {
            Some(c) if c.find(other).is_some() => c.call(other, args, cancelled).map(ToolOutput::Command),
            Some(_) => anyhow::bail!("unknown tool: {other}"),
            None => anyhow::bail!("unknown tool: {other} (the command tools failed to register at startup)"),
        },
    }
}

/// Block until a newly-emitted message matches `regex` (or `timeout_secs` elapses). The polling loop
/// runs on its request's own thread, so blocking is fine.
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
                let text = message_text(m)?;
                if let Some(hit) = re.find(&text) {
                    let snippet_start = hit.start().saturating_sub(80);
                    let snippet_end = (hit.end() + 80).min(text.len());
                    return Ok(serde_json::to_string_pretty(&json!({
                        "matched": true,
                        "harness": ev.reference.harness.as_str(),
                        "id": ev.reference.id,
                        "cwd": ev.reference.cwd.as_ref().map(|p| p.to_string_lossy()),
                        "role": cv_core::render::role_label(m.role),
                        "matched_text": &text[floor_char(&text, snippet_start)..ceil_char(&text, snippet_end)],
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

    observe_poll(
        &mut watcher,
        baseline,
        max_messages,
        char_cap,
        |message, resolver, room| {
            let (mut text, truncated) = cv_core::projection::searchable_prefix(message, resolver, room)?;
            if truncated {
                text.push('…');
            }
            Ok(text)
        },
    )
}

fn observe_poll(
    watcher: &mut Watcher,
    baseline: bool,
    max_messages: usize,
    char_cap: usize,
    mut project: impl FnMut(&cv_core::Message, &cv_core::Resolver, usize) -> anyhow::Result<String>,
) -> anyhow::Result<String> {
    let mut out_msgs: Vec<Value> = Vec::new();
    let mut chars_used = 0usize;
    let mut projection_error = None;
    let more_pending = !baseline
        && watcher.try_poll_bounded_retained(max_messages, |ev| {
            if projection_error.is_some() {
                return 0;
            }
            let r = &ev.reference;
            let resolver = cv_core::Resolver::new(Some(r.path.clone()));
            let mut taken = 0;
            for m in &ev.new_messages {
                let room = char_cap.saturating_sub(chars_used);
                if room == 0 {
                    break;
                }
                let text = match project(m, &resolver, room) {
                    Ok(text) => text,
                    Err(error) => {
                        projection_error = Some(error);
                        break;
                    }
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
        })?;
    if let Some(error) = projection_error {
        return Err(error);
    }

    Ok(serde_json::to_string_pretty(&json!({
        "baseline": baseline,
        "messages": out_msgs,
        "count": out_msgs.len(),
        "more_pending": more_pending,
        "cursor": watcher.to_cursor().encode(),
    }))?)
}

/// All matchable text in a message: text + thinking + tool-result content.
fn message_text(m: &cv_core::Message) -> anyhow::Result<String> {
    let mut output = String::new();
    m.append_searchable(&mut output)?;
    Ok(output)
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// A malformed caller-supplied argument. Mapped to JSON-RPC `-32602 Invalid params` at the
/// protocol layer (unlike tool *execution* failures, which surface as `isError` tool results).
#[derive(Debug)]
pub(crate) struct InvalidParams(pub String);

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

#[cfg(test)]
mod projection_tests {
    use super::*;
    use cv_core::{Block, Json, Message, Role, SessionRef};

    fn discover() -> Vec<SessionRef> {
        cv_core::harness::for_harness(Harness::Claude)
            .unwrap()
            .discover()
            .unwrap()
    }

    // Independent pre-change owned observer: whole searchable strings, then truncation.
    fn owned_observe_oracle(watcher: &mut Watcher, cap: usize) -> anyhow::Result<String> {
        let mut rows = Vec::new();
        let mut used = 0usize;
        let more = watcher.try_poll_bounded(50, |event| {
            let r=&event.reference; let mut taken=0;
            for m in event.new_messages {
                let full=message_text(&m).unwrap(); let room=cap.saturating_sub(used);
                if room==0 { break; }
                let text=if full.len()>room {format!("{}…",&full[..floor_char(&full,room)])} else {full};
                used+=text.len();
                rows.push(json!({"harness":r.harness.as_str(),"session_id":r.id,"cwd":r.cwd.as_ref().map(|p|p.to_string_lossy().to_string()),"role":cv_core::render::role_label(m.role),"timestamp":m.timestamp.map(|t|t.to_rfc3339()),"text":text}));taken+=1;
            } taken
        })?;
        Ok(serde_json::to_string_pretty(
            &json!({"baseline":false,"messages":rows,"count":rows.len(),"more_pending":more,"cursor":watcher.to_cursor().encode()}),
        )?)
    }

    #[test]
    #[ignore = "isolated clean release CPU control; CV_OBSERVE_CONTROL=old|new"]
    fn observe_tiny_actual_consumer_cpu() {
        let world = cv_testkit::World::in_process("observe-tiny-cpu");
        world.write_session("observe-tiny", &[json!({"type":"assistant", "sessionId":"observe-tiny", "uuid":"a", "message":{"role":"assistant","content":"tiny 月 text"}})]);
        let retained = std::env::var("CV_OBSERVE_CONTROL").unwrap() == "new";
        let start = std::time::Instant::now();
        for _ in 0..2000 {
            let mut watcher = Watcher::with_discovery(Filter::default(), true, discover);
            if retained {
                std::hint::black_box(
                    observe_poll(&mut watcher, false, 50, 16000, |m, r, cap| {
                        let (mut text, truncated) = cv_core::projection::searchable_prefix(m, r, cap)?;
                        if truncated {
                            text.push('…');
                        }
                        Ok(text)
                    })
                    .unwrap(),
                );
            } else {
                std::hint::black_box(owned_observe_oracle(&mut watcher, 16000).unwrap());
            }
        }
        println!("observe tiny retained={retained} wall={:?}", start.elapsed());
    }

    #[test]
    fn retained_observation_rejects_callback_mutation_before_acceptance() {
        let world = cv_testkit::World::in_process("observe-prefix-mutation");
        let path = world.write_session("observe-mutation", &[json!({"type":"assistant", "sessionId":"observe-mutation", "uuid":"a", "message":{"role":"assistant","content":"hello"}})]);
        let mut watcher = Watcher::with_discovery(Filter::default(), true, discover);
        let before = watcher.to_cursor().encode();
        let result = watcher.try_poll_bounded_retained(50, |event| {
            assert_eq!(event.new_messages.len(), 1);
            std::fs::write(&path, b"changed").unwrap();
            1
        });
        assert!(result.is_err());
        assert_eq!(watcher.to_cursor().encode(), before);
    }

    #[cfg(feature = "probe-heap")]
    #[test]
    #[ignore = "isolated allocation evidence; run one test thread with probe-heap"]
    fn observe_giant_actual_consumer_heap() {
        let world = cv_testkit::World::in_process("observe-giant-heap");
        for kind in ["text", "arguments"] {
            for mib in [16, 64] {
                let body = "x\n月".repeat(mib * 1024 * 1024 / 5);
                let block = if kind == "text" {
                    json!({"type":"text","text":body})
                } else {
                    json!({"type":"tool_use","id":"c","name":"tool","input":{"payload":body}})
                };
                world.write_session("observe-giant", &[json!({"type":"assistant", "sessionId":"observe-giant", "uuid":"a", "message":{"role":"assistant","content":[block]}})]);
                let mut oracle = None;
                for retained in [false, true] {
                    let mut watcher = Watcher::with_discovery(Filter::default(), true, discover);
                    let baseline = cv_testkit::heap::begin();
                    let output = if retained {
                        observe_poll(&mut watcher, false, 50, 16000, |m, r, cap| {
                            let (mut text, truncated) = cv_core::projection::searchable_prefix(m, r, cap)?;
                            if truncated {
                                text.push('…');
                            }
                            Ok(text)
                        })
                        .unwrap()
                    } else {
                        owned_observe_oracle(&mut watcher, 16000).unwrap()
                    };
                    let heap = cv_testkit::heap::end(baseline);
                    println!(
                        "{}",
                        json!({"kind":kind,"mib":mib,"retained":retained,"heap":heap,"output_bytes":output.len()})
                    );
                    if retained {
                        assert!(heap.peak_above_baseline_bytes < 2 * 1024 * 1024);
                    }
                    if retained {
                        assert_eq!(
                            oracle.as_deref(),
                            Some(output.as_str()),
                            "complete giant response oracle"
                        );
                    } else {
                        oracle = Some(output.clone());
                    }
                    drop(output);
                }
            }
        }
    }

    #[test]
    fn observe_retained_prefix_matches_owned_oracle() {
        let world = cv_testkit::World::in_process("observe-prefix-oracle");
        let body = "月🌙é\n\"\\".repeat(4000);
        world.write_session("observe-prefix", &[json!({"type":"assistant", "sessionId":"observe-prefix", "uuid":"a", "message":{"role":"assistant","content":[{"type":"text","text":body},{"type":"tool_use","id":"c","name":"tool","input":{"payload":body}}]}})]);
        for cap in [1usize, 2, 3, 4, 7, 19, 16000, 200000] {
            let mut old = Watcher::with_discovery(Filter::default(), true, discover);
            let oracle = owned_observe_oracle(&mut old, cap).unwrap();
            let mut watcher = Watcher::with_discovery(Filter::default(), true, discover);
            let output = observe_poll(&mut watcher, false, 50, cap, |message, resolver, room| {
                let (mut text, truncated) = cv_core::projection::searchable_prefix(message, resolver, room)?;
                if truncated {
                    text.push('…');
                }
                Ok(text)
            })
            .unwrap();
            assert_eq!(output, oracle, "cap={cap}");
        }
    }

    #[test]
    fn observe_read_failure_returns_error_without_advancing_cursor() {
        let _world = cv_testkit::World::in_process("mcp-unreadable-boundary");
        fn unreadable() -> Vec<SessionRef> {
            vec![SessionRef::new(
                "unreadable",
                Harness::Claude,
                std::env::temp_dir().join(format!("cv-mcp-missing-{}.jsonl", uuid::Uuid::new_v4())),
            )]
        }
        let mut watcher = Watcher::with_discovery(Filter::default(), true, unreadable);
        let before = watcher.to_cursor().encode();
        assert!(observe_poll(&mut watcher, false, 50, 16_000, |m, _, _| message_text(m)).is_err());
        assert_eq!(
            watcher.to_cursor().encode(),
            before,
            "a demanded read failure leaves cursor position unchanged"
        );
    }

    #[test]
    fn observe_projection_failure_returns_error_without_a_success_cursor() {
        let world = cv_testkit::World::in_process("mcp-json-boundary");
        world.write_session("observe-json", &[json!({"type":"user", "sessionId":"observe-json", "uuid":"u", "message":{"role":"user","content":"new activity"}})]);
        let path = world.base.join("arguments.json");
        let value = json!({"argument":"月🌙é\n\"\\".repeat(300)});
        let bytes = serde_json::to_vec(&value).unwrap();
        let message = |input| {
            let mut message = Message::new(Role::Assistant);
            message.content.push(Block::ToolUse {
                id: "call".into(),
                name: "custom".into(),
                input,
                namespace: None,
            });
            message
        };
        let owned_text = message_text(&message(value.into())).unwrap();
        for deleted in [false, true] {
            std::fs::write(&path, &bytes).unwrap();
            let source = message(Json::source(&path, 0, bytes.len() as u64).unwrap());
            assert_eq!(message_text(&source).unwrap(), owned_text);
            if deleted {
                std::fs::remove_file(&path).unwrap();
            } else {
                std::fs::write(&path, b"{} ").unwrap();
            }
            let mut watcher = Watcher::with_discovery(Filter::default(), true, discover);
            let mut demanded = false;
            let result = observe_poll(&mut watcher, false, 50, 16_000, |_, _, _| {
                demanded = true;
                message_text(&source)
            });
            assert!(demanded, "must exercise a demanded callback projection");
            assert!(
                result.is_err(),
                "cannot serialize a success cursor after failed projection"
            );
            let mut recovered = 0;
            watcher.poll_bounded(50, |event| {
                recovered += event.new_messages.len();
                event.new_messages.len()
            });
            assert_eq!(recovered, 1, "failed projection must leave offered messages unread");
        }
    }
}

#[cfg(test)]
mod transport_tests {
    use super::super::mcp_tools::{CapturedText, CommandOutput};
    use super::*;
    #[test]
    fn staged_protocol_matches_old_value_oracle_and_keeps_success_stderr() {
        let body = "λ🌙\"\\\n".repeat(12000).into_bytes();
        for stdout in [b"".as_slice(), b"ok\n", b"\xff\xe2\x82", body.as_slice()] {
            for stderr in [b"".as_slice(), " warning λ\n".as_bytes(), b" \xff\n".as_slice()] {
                for failure in [false, true] {
                    let prefix = failure.then(|| "error: cv show s exited 2: ".to_owned());
                    let output = CommandOutput {
                        stdout: CapturedText::capture(stdout).unwrap(),
                        stderr: CapturedText::capture(stderr).unwrap(),
                        error_prefix: prefix.clone(),
                    };
                    let id = json!({"nested":{"deeper":{"text":"do not replace id"}}});
                    let stdout = String::from_utf8_lossy(stdout);
                    let stderr = String::from_utf8_lossy(stderr);
                    let mut expected = if let Some(prefix) = prefix {
                        let detail = if stderr.trim().is_empty() {
                            stdout.trim()
                        } else {
                            stderr.trim()
                        };
                        tool_text_result(&format!("{prefix}{detail}"), true)
                    } else {
                        tool_text_result(&stdout, false)
                    };
                    if !failure && !stderr.is_empty() {
                        expected["content"]
                            .as_array_mut()
                            .unwrap()
                            .push(json!({"type":"text","text":stderr}));
                    }
                    let mut expected = serde_json::to_vec(&ok_response(id.clone(), expected)).unwrap();
                    expected.push(b'\n');
                    let mut actual = Vec::new();
                    write_message(&mut actual, &Response::Command { id, output }).unwrap();
                    assert_eq!(actual, expected);
                }
            }
        }
    }
    #[test]
    fn staged_writer_failure_and_protocol_classification() {
        struct Failed;
        impl Write for Failed {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let output = CommandOutput {
            stdout: CapturedText::capture(b"body".as_slice()).unwrap(),
            stderr: CapturedText::capture(b"".as_slice()).unwrap(),
            error_prefix: None,
        };
        assert!(write_message(&mut Failed, &Response::Command { id: json!(1), output }).is_err());
        let cancelled = AtomicBool::new(false);
        let params = json!({"name":"observe_stream","arguments":{"max_messages":"invalid"}});
        let Some(Response::Json(value)) = handle("tools/call", Some(params), Some(json!(1)), &cancelled) else {
            panic!("error frame");
        };
        assert_eq!(value["error"]["code"], -32602);
        let params = json!({"name":"board_removed","arguments":{}});
        let Some(Response::Json(value)) = handle("tools/call", Some(params.clone()), Some(json!(1)), &cancelled) else {
            panic!("tool error frame");
        };
        assert_eq!(value["result"]["isError"], true);
        assert!(handle("tools/call", Some(params), None, &cancelled).is_none());
    }
}

#[cfg(all(test, feature = "probe-heap"))]
mod transport_heap_probe {
    use super::*;
    #[test]
    #[ignore = "isolated whole generated-tool transport allocation control"]
    fn staged_mcp_whole_transport_heap() {
        struct Count(u64);
        impl Write for Count {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 += bytes.len() as u64;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let bin = std::env::var("CV_MCP_PROBE_BIN").expect("actual cv binary");
        let source = std::env::var("CV_MCP_PROBE_SOURCE").expect("scratch source");
        let range = std::env::var("CV_MCP_PROBE_RANGE").unwrap_or_else(|_| "0..1".into());
        let mode = std::env::var("CV_MCP_PROBE_MODE").expect("owned or staged");
        let argv = vec!["show".to_string(), source, "--range".into(), range, "--json".into()];
        let mut sink = Count(0);
        let baseline = cv_testkit::heap::begin();
        if mode == "owned" {
            let child = std::process::Command::new(&bin)
                .args(&argv)
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            assert!(child.status.success());
            let text = String::from_utf8_lossy(&child.stdout).into_owned();
            let value = ok_response(json!(1), tool_text_result(&text, false));
            let mut frame = serde_json::to_vec(&value).unwrap();
            frame.push(b'\n');
            sink.write_all(&frame).unwrap();
        } else {
            let output =
                super::super::mcp_tools::run_command(std::path::Path::new(&bin), &argv, &AtomicBool::new(false))
                    .unwrap();
            assert!(output.error_prefix.is_none());
            write_message(&mut sink, &Response::Command { id: json!(1), output }).unwrap();
        }
        let heap = cv_testkit::heap::end(baseline);
        eprintln!(
            "MCP_TRANSPORT_HEAP {}",
            json!({"mode":mode,"wire_bytes":sink.0,"heap":heap})
        );
    }
}
