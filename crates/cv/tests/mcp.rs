//! Protocol-level integration tests for `cv mcp`, the stdio JSON-RPC server: spawn the real binary,
//! run the MCP handshake, list tools, and call them against a temp-home fixture corpus.
//!
//! Hermetic: a fake `$HOME` carries the claude fixtures and `$CLUSTERVISION_HOME` the index/task
//! state; both are passed only to the child process's environment (and from it to the `cv` it
//! re-runs for a generated tool). The argv a generated call builds is pinned by `mcp_tools`' unit
//! tests; these check the round trip through the real commands.

use cv_testkit::World;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};

use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// A running `cv mcp` server over stdio, with a line reader on a side thread so a hung server
/// fails the test with a timeout instead of wedging the run.
struct Server {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    world: World,
}

impl Server {
    fn spawn(tag: &str) -> Server {
        let world = World::new(tag);

        // Fixture corpus: two tiny claude sessions.
        let alpha = [
            json!({"type": "ai-title", "aiTitle": "alpha adventures"}),
            json!({"type": "user", "uuid": "u1", "timestamp": "2026-01-01T10:00:00Z",
                   "cwd": "/work/proj",
                   "message": {"role": "user", "content": "please fix the zebrafish migration"}}),
            json!({"type": "assistant", "uuid": "a1", "timestamp": "2026-01-02T10:00:00Z",
                   "message": {"role": "assistant", "content": [
                       {"type": "text", "text": "done — the migration is fixed"}]}}),
        ];
        let beta = [
            json!({"type": "user", "uuid": "b1", "timestamp": "2026-03-01T09:00:00Z",
                   "cwd": "/work/proj",
                   "message": {"role": "user", "content": "how many quokkas in the census"}}),
            json!({"type": "assistant", "uuid": "b2", "timestamp": "2026-03-01T09:05:00Z",
                   "message": {"role": "assistant", "content": [
                       {"type": "text", "text": "seventeen quokkas"}]}}),
        ];
        world.write_session("alphasess", &alpha);
        world.write_session("betasess", &beta);

        let mut child = world
            .cv_command()
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("cv mcp should spawn");

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Server {
            child,
            stdin,
            lines: rx,
            world,
        }
    }

    /// Send one raw line to the server.
    fn send_raw(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").expect("write to cv mcp stdin");
        self.stdin.flush().unwrap();
    }

    fn send(&mut self, v: &Value) {
        self.send_raw(&v.to_string());
    }

    /// Next response line as JSON. The timeout exists to turn a WEDGE into a failure, not to gate
    /// on speed: 10s was tight enough that `observe_stream_bounded_read_only_tail` failed at
    /// exactly 10.018s under a loaded full-suite run while passing in 2.5s on its own, and a
    /// generated tool now shells out to the `cv` binary, which is not free. A minute still fails a
    /// genuine hang promptly and stops reporting the machine's load as a bug.
    fn recv(&mut self) -> Value {
        let line = self
            .lines
            .recv_timeout(Duration::from_secs(60))
            .expect("timed out waiting for a cv mcp response");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("non-JSON response {line:?}: {e}"))
    }

    /// Round-trip a request and assert the JSON-RPC envelope (version + echoed id).
    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let resp = self.recv();
        assert_eq!(resp["jsonrpc"], "2.0", "envelope: {resp}");
        assert_eq!(resp["id"], json!(id), "id must echo: {resp}");
        resp
    }

    /// Call a tool and return `(text, is_error)` from the MCP tool result.
    fn call_tool(&mut self, id: u64, name: &str, args: Value) -> (String, bool) {
        let resp = self.request(id, "tools/call", json!({"name": name, "arguments": args}));
        let result = &resp["result"];
        assert!(result.is_object(), "tools/call must return a result: {resp}");
        let content = result["content"]
            .as_array()
            .unwrap_or_else(|| panic!("tool result must carry content: {resp}"));
        assert_eq!(content[0]["type"], "text", "{resp}");
        let text = content[0]["text"].as_str().unwrap_or_default().to_string();
        let is_error = result["isError"].as_bool().unwrap_or(false);
        (text, is_error)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn handshake_and_tool_schemas() {
    let mut s = Server::spawn("handshake");

    // initialize: protocol version, server info, capabilities.
    let resp = s.request(1, "initialize", json!({"protocolVersion": "2025-06-18"}));
    let r = &resp["result"];
    assert!(r["protocolVersion"].is_string(), "{resp}");
    assert_eq!(r["serverInfo"]["name"], "clustervision", "{resp}");
    assert!(r["capabilities"]["tools"].is_object(), "{resp}");

    // The initialized notification must produce no response — the next reply must be for id 2.
    s.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let resp = s.request(2, "ping", json!({}));
    assert!(resp["result"].is_object(), "{resp}");

    // tools/list: every tool is named, described, and carries a valid object schema whose
    // `required` names actually exist in `properties`.
    let resp = s.request(3, "tools/list", json!({}));
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    assert!(tools.len() >= 5, "expected the full toolset, got {}", tools.len());
    let mut names = std::collections::HashSet::new();
    for t in tools {
        let name = t["name"].as_str().unwrap_or_else(|| panic!("unnamed tool: {t}"));
        assert!(names.insert(name.to_string()), "duplicate tool name {name}");
        assert!(
            !t["description"].as_str().unwrap_or("").is_empty(),
            "{name} needs a description"
        );
        let schema = &t["inputSchema"];
        assert_eq!(schema["type"], "object", "{name}: inputSchema must be type:object");
        let props = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{name}: properties must be an object"));
        for (pname, p) in props {
            assert!(p["type"].is_string(), "{name}.{pname}: every property needs a type");
            assert!(
                !p["description"].as_str().unwrap_or("").is_empty(),
                "{name}.{pname}: every property needs a description"
            );
        }
        if let Some(req) = schema["required"].as_array() {
            for r in req {
                let r = r.as_str().expect("required entries are strings");
                assert!(props.contains_key(r), "{name}: required {r:?} missing from properties");
            }
        }
    }
    // Generated from the CLI's Read group…
    for expected in ["ls", "show", "cat", "search"] {
        assert!(
            names.contains(expected),
            "missing CLI-generated tool {expected}: {names:?}"
        );
    }
    // …the task subcommands, generated the same way…
    for expected in ["task_open", "task_claim", "task_resolve", "task_inbox", "task_events"] {
        assert!(names.contains(expected), "missing CLI-generated tool {expected}");
    }
    assert!(!names.contains("task"), "`task` is a dispatcher, not a tool: {names:?}");
    // …and the two hand-written tools, which block or hold a cursor.
    for expected in ["observe_stream", "await_omen"] {
        assert!(names.contains(expected), "missing MCP-only tool {expected}");
    }
    // `resolve` answers with one of choice / accept_default / confirm: none is required alone.
    let resolve = tools.iter().find(|t| t["name"] == "task_resolve").unwrap();
    assert_eq!(resolve["inputSchema"]["required"], json!(["id"]), "{resolve}");
    // Fleet & live commands (`serve`, `watch`, `mcp`) are never tools: they block or hold a socket.
    for gone in ["serve", "watch", "archive_sync", "mcp", "scry"] {
        assert!(!names.contains(gone), "{gone} must not be a generated tool: {names:?}");
    }
    // The 0.11.0 removals stay removed: their replacements are show/search/ls/pack/prune/cat.
    for gone in [
        "list_sessions",
        "search_sessions",
        "read_session",
        "project_sessions",
        "recall",
        "prune_session",
        "prune_retrieve",
    ] {
        assert!(
            !names.contains(gone),
            "{gone} was removed in 0.11.0 but is still advertised"
        );
    }

    // `show` advertises its MCP window defaults, and `--json` is never the caller's to set.
    let show = tools.iter().find(|t| t["name"] == "show").unwrap();
    let desc = show["description"].as_str().unwrap();
    assert!(desc.contains("LAST 50") && desc.contains("200000"), "{desc}");
    let props = &show["inputSchema"]["properties"];
    assert!(props.get("json").is_none(), "json must not be a property: {props}");
    assert_eq!(props["last"]["type"], "integer", "{props}");
    assert_eq!(show["inputSchema"]["required"], json!(["id"]), "{show}");
}

/// The generated tools run the real commands: `show` with no window gets the MCP default and
/// returns the IR as JSON, `cat` (no JSON form) returns text, a non-zero exit is a tool error
/// carrying stderr, and a protocol mistake is -32602 before anything runs.
#[test]
fn generated_tools_run_the_real_commands() {
    let mut s = Server::spawn("generated");
    s.request(1, "initialize", json!({}));

    let (text, is_err) = s.call_tool(2, "show", json!({"id": "alphasess"}));
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).expect("show returns the IR as JSON");
    assert_eq!(v["id"], "alphasess", "{text}");
    assert_eq!(v["messages"].as_array().map(Vec::len), Some(2), "{text}");

    // An explicit window is passed through (a clap id becomes its kebab-case flag).
    let (text, is_err) = s.call_tool(3, "show", json!({"id": "alphasess", "first": 1}));
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["messages"].as_array().map(Vec::len), Some(1), "{text}");

    // `ls` filters through to the same rows `cv ls --json` prints.
    let (text, is_err) = s.call_tool(4, "ls", json!({"cwd": "/work/proj"}));
    assert!(!is_err, "{text}");
    let rows: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(rows.as_array().map(Vec::len), Some(2), "{text}");

    // A non-zero exit becomes an isError tool result carrying stderr.
    let (text, is_err) = s.call_tool(5, "show", json!({"id": "nosuch"}));
    assert!(is_err, "a failing `cv` must be a tool error: {text}");
    assert!(text.contains("nosuch"), "stderr must reach the caller: {text}");

    // A missing required positional and an unknown argument are the caller's protocol mistakes.
    let resp = s.request(8, "tools/call", json!({"name": "cat", "arguments": {"session": "a"}}));
    assert_eq!(resp["error"]["code"], -32602, "{resp}");
    assert!(
        resp["error"]["message"].as_str().unwrap().contains("tool_use_id"),
        "{resp}"
    );
    let resp = s.request(
        9,
        "tools/call",
        json!({"name": "show", "arguments": {"id": "a", "limit": 5}}),
    );
    assert_eq!(resp["error"]["code"], -32602, "{resp}");
    assert!(resp["error"]["message"].as_str().unwrap().contains("limit"), "{resp}");
}

#[test]
fn tool_calls_against_fixture_corpus() {
    let mut s = Server::spawn("tools");
    s.request(1, "initialize", json!({}));

    // The board tools went away in 0.15.0: a pointer at the task log, never a silent unknown-tool.
    for name in ["board_post", "board_read", "board_await", "board_claim"] {
        let (text, is_err) = s.call_tool(12, name, json!({"channel": "testchan", "body": "hello fleet"}));
        assert!(is_err, "{name}: {text}");
        assert!(
            text.contains("removed in 0.15.0") && text.contains("task_note"),
            "{name} must point at the replacement: {text}"
        );
    }
    assert!(!s.world.cv_home.join("board").exists(), "nothing wrote a board dir");
}

/// The task tools are `cv task <sub>` and drive the durable store end-to-end over the protocol:
/// open → two claimants race (first writer wins; the loser gets the store's rejection, not a
/// duplicate) → done. Also pins the identity-bearing-verb refusal, the `--state` vocabulary
/// rejection, the inbox, and the list's default (non-terminal) scope.
#[test]
fn task_tools_open_claim_race_done() {
    let mut s = Server::spawn("tasks");
    s.request(1, "initialize", json!({}));

    // Open: the CLI's confirmation line, then the full id.
    let (text, is_err) = s.call_tool(
        2,
        "task_open",
        json!({"title": "carve the totem", "from": "agent:alpha"}),
    );
    assert!(!is_err, "{text}");
    assert!(text.contains("opened"), "{text}");
    let id = text.lines().last().expect("the id line").trim().to_string();

    // First claimant wins…
    let (text, is_err) = s.call_tool(3, "task_claim", json!({"id": id, "from": "agent:alpha"}));
    assert!(!is_err, "{text}");
    assert!(text.contains("→ claimed"), "{text}");

    // …the second gets a rejection from the store's locked CAS, not a duplicate claim.
    let (text, is_err) = s.call_tool(4, "task_claim", json!({"id": id, "from": "agent:beta"}));
    assert!(is_err, "the second claimant must lose: {text}");
    assert!(text.contains("rejected"), "{text}");

    // Identity-bearing verbs refuse to act namelessly (no `from`, no CV_ENDPOINT in the env).
    let (text, is_err) = s.call_tool(5, "task_release", json!({"id": id}));
    assert!(is_err, "{text}");
    assert!(text.contains("CV_ENDPOINT"), "the error must name the cure: {text}");

    // The winner's inbox carries the claim; the loser's stays empty.
    let (text, is_err) = s.call_tool(6, "task_inbox", json!({"who": "agent:alpha"}));
    assert!(!is_err, "{text}");
    // The one inbox row (`cv_core::task::InboxItem`), on the page `cv serve` serves too.
    let page: Value = serde_json::from_str(&text).expect("inbox --json");
    assert_eq!(page["who"], "agent:alpha", "{text}");
    let inbox = page["items"].as_array().expect("inbox items");
    assert_eq!(inbox.len(), 1, "{text}");
    assert_eq!(inbox[0]["id"], json!(id), "{text}");
    assert_eq!(inbox[0]["reason"], "claimed_by_you", "{text}");
    assert_eq!(inbox[0]["effective_state"], "claimed", "{text}");
    assert!(inbox[0]["since"].is_string() && inbox[0]["age"].is_string(), "{text}");
    let (text, _) = s.call_tool(7, "task_inbox", json!({"who": "agent:beta"}));
    let page: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(page["items"], json!([]), "{text}");

    // An unknown state filter errors naming the vocabulary — never a silent empty list.
    let (text, is_err) = s.call_tool(8, "task_list", json!({"state": "redy"}));
    assert!(is_err, "{text}");
    assert!(
        text.contains("redy") && text.contains("claimed"),
        "the error names the typo and the vocabulary: {text}"
    );

    // Done closes it: hidden from the default list, visible with `all`.
    let (text, is_err) = s.call_tool(9, "task_done", json!({"id": id, "from": "agent:alpha"}));
    assert!(!is_err, "{text}");
    assert!(text.contains("→ done"), "{text}");
    let (text, _) = s.call_tool(10, "task_list", json!({}));
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap(),
        json!([]),
        "terminal task hidden: {text}"
    );
    let (text, _) = s.call_tool(11, "task_list", json!({"all": true}));
    let rows: Value = serde_json::from_str(&text).unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 1, "{text}");
    assert_eq!(rows[0]["task_id"], json!(id), "{text}");
    assert_eq!(rows[0]["state"], "done", "{text}");

    // The retired land-facet tools error with a pointer, never a bare "unknown tool".
    let (text, is_err) = s.call_tool(12, "task_debt", json!({}));
    assert!(is_err, "{text}");
    assert!(
        text.contains("removed in 0.15.0") && text.contains("task_done"),
        "{text}"
    );
}

#[test]
fn protocol_error_handling() {
    let mut s = Server::spawn("errors");
    s.request(1, "initialize", json!({}));

    // Unknown method with an id → -32601.
    let resp = s.request(2, "wizardry/cast", json!({}));
    assert_eq!(resp["error"]["code"], -32601, "{resp}");

    // Unparseable line → -32700 with a null id (and the server survives).
    s.send_raw("this is not json {");
    let resp = s.recv();
    assert_eq!(resp["error"]["code"], -32700, "{resp}");
    assert!(resp["id"].is_null(), "{resp}");

    // Unknown tool → a tool-level error result, not a protocol error.
    let (text, is_err) = s.call_tool(3, "tools_that_do_not_exist", json!({}));
    assert!(is_err, "{text}");
    assert!(text.contains("unknown tool"), "{text}");

    // A *notification* (no id) must never get a response — even for a known method. The next
    // line on the wire after the notifications must be the reply to the request that follows.
    s.send(&json!({"jsonrpc": "2.0", "method": "ping"}));
    s.send(&json!({"jsonrpc": "2.0", "method": "tools/list"}));
    let resp = s.request(4, "ping", json!({}));
    assert_eq!(
        resp["id"],
        json!(4),
        "a notification (no id) must not produce a response; got an extra reply: {resp}"
    );

    // Still alive and well after all of that.
    let resp = s.request(5, "tools/list", json!({}));
    assert!(resp["result"]["tools"].is_array(), "{resp}");
}

/// Requests are dispatched concurrently: a parked long-poll (await_omen on a regex nothing
/// emits) must not head-of-line-block a fast request issued after it. Responses come back out
/// of order — the fast one first — and both complete well under the long-poll's timeout budget.
#[test]
fn slow_long_poll_does_not_block_other_requests() {
    let mut s = Server::spawn("concurrent");
    s.request(1, "initialize", json!({}));

    let start = std::time::Instant::now();
    s.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
        "name": "await_omen",
        "arguments": {"regex": "NEVER_MATCHES", "cwd_contains": "/nowhere", "timeout_secs": 6, "interval_secs": 1}}}));
    s.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
        "name": "task_list", "arguments": {}}}));

    let first = s.recv();
    let elapsed = start.elapsed();
    assert_eq!(first["id"], json!(3), "the fast request must answer first: {first}");
    assert!(
        elapsed < Duration::from_secs(4),
        "task_list took {elapsed:?} — blocked behind the 6s long-poll?"
    );
    assert_eq!(first["result"]["isError"], false, "{first}");

    // The long-poll still completes on its own schedule (timed out, unmatched).
    let second = s.recv();
    assert_eq!(second["id"], json!(2), "{second}");
    let text = second["result"]["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("\"timed_out\": true"), "{second}");
}

/// A present-but-malformed numeric argument is the caller's protocol mistake: JSON-RPC `-32602
/// Invalid params`, not a silent fallback to the default (and not an isError tool result). An
/// integral float is tolerated — clients often serialize `1` as `1.0`.
#[test]
fn malformed_numeric_args_are_invalid_params() {
    let mut s = Server::spawn("badparams");
    s.request(1, "initialize", json!({}));

    for (id, bad) in [(2u64, json!("ten")), (3, json!(-3)), (4, json!(1.5))] {
        let resp = s.request(
            id,
            "tools/call",
            json!({"name": "observe_stream", "arguments": {"cwd_contains": "/nowhere", "max_messages": bad}}),
        );
        assert_eq!(resp["error"]["code"], -32602, "max_messages={bad}: {resp}");
        assert!(
            resp["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("max_messages"),
            "the error must name the offending argument: {resp}"
        );
        assert!(resp["result"].is_null(), "an error response carries no result: {resp}");
    }

    // Integral float: accepted, behaves like the integer — on the hand-written guard…
    let (text, is_err) = s.call_tool(
        5,
        "observe_stream",
        json!({"cwd_contains": "/nowhere", "max_messages": 1.0}),
    );
    assert!(!is_err, "{text}");
    // …and on the generated path, which renders it for clap without the `.0`.
    let (text, is_err) = s.call_tool(9, "show", json!({"id": "alphasess", "last": 20.0}));
    assert!(!is_err, "{text}");
    assert!(serde_json::from_str::<Value>(&text).is_ok(), "{text}");

    // Other tools route through the same guard (spot-check a long-poll's timeout).
    let resp = s.request(
        6,
        "tools/call",
        json!({"name": "await_omen", "arguments": {"regex": ".", "timeout_secs": "soon"}}),
    );
    assert_eq!(resp["error"]["code"], -32602, "{resp}");

    // Still alive afterwards.
    let resp = s.request(7, "ping", json!({}));
    assert!(resp["result"].is_object(), "{resp}");
}

/// `observe_stream` is the non-blocking tail of `await_omen`: bounded, read-only, cursor-driven.
/// Covers the spec's required cases — an absent/empty corpus yields a bounded EMPTY result, the
/// baseline call emits no backlog, the cursor drains only newly-appended messages, max_messages
/// bounds the batch (more_pending), and nothing is written anywhere.
#[test]
fn observe_stream_bounded_read_only_tail() {
    let mut s = Server::spawn("observe");
    s.request(1, "initialize", json!({}));

    // 1) Empty corpus (a filter that matches NO session) → a bounded, empty, baseline result.
    //    No crash, no error, an empty `messages`, and a usable cursor.
    let (text, is_err) = s.call_tool(2, "observe_stream", json!({"cwd_contains": "/no/such/dir"}));
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).expect("observe_stream returns JSON");
    assert_eq!(v["baseline"], true, "first call is a baseline: {text}");
    assert_eq!(v["count"], 0, "absent corpus → zero messages: {text}");
    assert_eq!(v["messages"].as_array().unwrap().len(), 0, "{text}");
    assert_eq!(v["more_pending"], false, "{text}");
    assert!(
        v["cursor"].as_str().unwrap().contains("\"v\":1"),
        "cursor is versioned: {text}"
    );

    // 2) Baseline over the REAL fixture corpus emits no backlog (only future activity is tailed),
    //    and hands back a cursor recording the current tail.
    let (text, is_err) = s.call_tool(3, "observe_stream", json!({"cwd_contains": "/work/proj"}));
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["baseline"], true, "{text}");
    assert_eq!(v["count"], 0, "baseline emits no history: {text}");
    let cursor = v["cursor"].as_str().unwrap().to_string();

    // 3) Replaying that cursor with no new activity drains nothing (and is no longer a baseline).
    let (text, is_err) = s.call_tool(
        4,
        "observe_stream",
        json!({"cwd_contains": "/work/proj", "since_cursor": cursor}),
    );
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["baseline"], false, "a cursor call is not a baseline: {text}");
    assert_eq!(v["count"], 0, "no new activity → nothing drained: {text}");

    // 4) Append a NEW message to the alpha session on disk; the next cursor call drains exactly it.
    let proj = s.world.proj.clone();
    let extra = json!({"type": "assistant", "uuid": "a2",
                       "timestamp": "2026-06-19T12:00:00Z",
                       "message": {"role": "assistant", "content": [
                           {"type": "text", "text": "ZEBRA_TAIL_MARKER appended later"}]}});
    let mut body = fs::read_to_string(proj.join("alphasess.jsonl")).unwrap();
    body.push_str(&format!("{extra}\n"));
    fs::write(proj.join("alphasess.jsonl"), body).unwrap();

    let (text, is_err) = s.call_tool(
        5,
        "observe_stream",
        json!({"cwd_contains": "/work/proj", "since_cursor": cursor}),
    );
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    let msgs = v["messages"].as_array().unwrap();
    assert!(
        msgs.iter()
            .any(|m| m["text"].as_str().unwrap_or("").contains("ZEBRA_TAIL_MARKER")),
        "the appended message must be drained: {text}"
    );
    assert_eq!(
        v["count"], 1,
        "exactly the one appended message, nothing from before: {text}"
    );
    assert!(
        msgs.iter()
            .all(|m| m["text"].as_str().unwrap_or("").contains("ZEBRA_TAIL_MARKER")
                || !m["text"].as_str().unwrap_or("").contains("zebrafish")),
        "previously-seen messages must NOT re-emit: {text}"
    );

    // 5) max_messages bounds the batch. Append several messages, baseline-reset, then drain with a
    //    cap of 1 and assert more_pending is flagged.
    let mut body = fs::read_to_string(proj.join("alphasess.jsonl")).unwrap();
    for i in 0..3 {
        let m = json!({"type": "assistant", "uuid": format!("burst{i}"),
                       "timestamp": "2026-06-19T13:00:00Z",
                       "message": {"role": "assistant", "content": [
                           {"type": "text", "text": format!("burst message {i}")}]}});
        body.push_str(&format!("{m}\n"));
    }
    fs::write(proj.join("alphasess.jsonl"), &body).unwrap();
    // Fresh baseline so we know exactly what's pending, then append AFTER baselining.
    let (text, _) = s.call_tool(6, "observe_stream", json!({"cwd_contains": "/work/proj"}));
    let base2 = serde_json::from_str::<Value>(&text).unwrap()["cursor"]
        .as_str()
        .unwrap()
        .to_string();
    let mut body = fs::read_to_string(proj.join("alphasess.jsonl")).unwrap();
    for i in 0..3 {
        let m = json!({"type": "assistant", "uuid": format!("after{i}"),
                       "timestamp": "2026-06-19T14:00:00Z",
                       "message": {"role": "assistant", "content": [
                           {"type": "text", "text": format!("after message {i}")}]}});
        body.push_str(&format!("{m}\n"));
    }
    fs::write(proj.join("alphasess.jsonl"), body).unwrap();
    let (text, is_err) = s.call_tool(
        7,
        "observe_stream",
        json!({"cwd_contains": "/work/proj", "since_cursor": base2, "max_messages": 1}),
    );
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["count"], 1, "max_messages=1 bounds the batch: {text}");
    assert_eq!(v["more_pending"], true, "the rest must be flagged pending: {text}");

    // ...and polling again with the returned cursor drains the rest, one per call, in order —
    // with nothing new appended in between.
    let mut cursor = v["cursor"].as_str().unwrap().to_string();
    let mut drained = vec![v["messages"][0]["text"].as_str().unwrap().trim().to_string()];
    for id in 100..110 {
        let (text, is_err) = s.call_tool(
            id,
            "observe_stream",
            json!({"cwd_contains": "/work/proj", "since_cursor": cursor, "max_messages": 1}),
        );
        assert!(!is_err, "{text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        cursor = v["cursor"].as_str().unwrap().to_string();
        drained.extend(
            v["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["text"].as_str().unwrap().trim().to_string()),
        );
        if v["more_pending"] == false {
            break;
        }
    }
    assert_eq!(
        drained,
        ["after message 0", "after message 1", "after message 2"],
        "more_pending must be drainable by polling the cursor"
    );

    // 6) READ-ONLY: observe_stream writes nothing under $CLUSTERVISION_HOME but the index.
    assert!(
        !s.world.cv_home.join("tasks").exists(),
        "observe_stream must not write the task log"
    );
}

/// A `since_cursor` this build cannot use (garbage, or another schema version) is a fresh
/// baseline, as `observe_stream`'s doc says: it records where every session stands and emits
/// nothing. Treating it as "no prior positions" instead makes every matching session look new,
/// and the call dumps the whole history of each one.
#[test]
fn observe_stream_unusable_cursor_is_a_fresh_baseline() {
    let mut s = Server::spawn("observe-badcursor");
    s.request(1, "initialize", json!({}));
    let mut id = 2;
    for bad in ["not a cursor", r#"{"v":99,"o":{}}"#, r#"{"v":1}"#] {
        let (text, is_err) = s.call_tool(
            id,
            "observe_stream",
            json!({"cwd_contains": "/work/proj", "since_cursor": bad}),
        );
        id += 1;
        assert!(!is_err, "{text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            v["count"], 0,
            "an unusable cursor must not dump history ({bad}): {text}"
        );
        assert_eq!(v["baseline"], true, "an unusable cursor is a baseline ({bad}): {text}");
        // ...and the cursor it hands back follows from there: nothing new, nothing drained.
        let cursor = v["cursor"].as_str().unwrap().to_string();
        let (text, _) = s.call_tool(
            id,
            "observe_stream",
            json!({"cwd_contains": "/work/proj", "since_cursor": cursor}),
        );
        id += 1;
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["count"], 0, "{text}");
    }
}
