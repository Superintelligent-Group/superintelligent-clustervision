# MCP — agents reading each other's minds

Most of clustervision is about *you* reading sessions after the fact. `cv mcp` is the
other direction: it hands the same powers to a **running coding agent** so it can read
*other* agents' sessions — within this project, back through time, and across harnesses
— and coordinate with siblings live. 🔮

It's a [Model Context Protocol](https://modelcontextprotocol.io) server speaking
line-delimited JSON-RPC 2.0 over stdio. Point Claude Code (or Codex, Gemini, …) at it and
the agent gets the whole `cv` command surface as tools, plus a handful of MCP-only tools
for live coordination.

## Motivating use cases

- **"What happened in this project before?"** — `ls(cwd='/myproj')` lists every session
  (any harness) whose recorded working directory is this project, newest first. The agent
  reads the relevant one with `show(id=…)`.
- **"Has anyone solved this before?"** — `search(query=…)` finds it by what was literally
  said; `pack(task=…)` compiles a whole context bundle out of the corpus. See
  [search](search.md).
- **"What's my sibling agent doing right now?"** — `ls` surfaces live sessions;
  `observe_stream` tails their output; `lanes` shows a session's sub-agent forest.
- **"Block until another session prints X."** — `await_omen(regex=…)` watches live
  message streams and returns when one matches.
- **"Why does my own context keep filling up?"** — `doctor(id=…)` attributes the pressure
  by source. Point it at your *own* session id.

## Registering it

The server is a `cv` subcommand. Register it with your host; for Claude Code:

```sh
claude mcp add clustervision -- /absolute/path/to/cv mcp
```

Any MCP host works — give it the absolute path to `cv` as the command and `mcp` as its one
argument. **stdout is the protocol channel**, so all diagnostics go to stderr. It implements
`initialize`, `ping`, `tools/list` and `tools/call` against protocol version `2025-06-18`, and
every tool returns its result as a single text content block. (Until 0.15.0 this was a separate
`cv-mcp` binary; `cv-mcp` is now a one-release shim that runs `cv mcp`.)

## The tools are generated from the CLI

There is no hand-maintained list of what cv can do inside `cv mcp`. At startup the server
reads its own command tree, in-process — exactly what

```sh
cv schema --commands --json
```

prints: clap's own introspection of the command tree — every visible command and
subcommand with its group, `about`, and each argument's kind (`flag` / `option` /
`positional`), value type, possible values, default, help text and required-ness. Each
command in the **Read**, **Reshape**, **Export** and **System** groups, and every `cv task`
subcommand, becomes one MCP tool:

- the **tool name is the command name** (`ls`, `show`, `cat`, …; a subcommand joins with
  an underscore, `task_open`);
- the **tool description is clap's `about`**;
- **every argument becomes a JSON-schema property named after the argument** —
  positionals included, so `cat` takes `session` and `tool_use_id` as properties — with
  clap's help as the property description, `possible_values` as an `enum`, and the CLI
  default as `default`;
- **required arguments land in the schema's `required` list** (an argument in a one-of
  group, like `task resolve`'s `choice` / `accept_default` / `confirm`, is not required alone);
- a **list value** repeats a repeatable option (`blocked_by: ["a", "b"]`) or fills a repeatable
  positional (`splice`'s `specs`).

A call runs the same `cv` executable as `cv <command> <args…> --json`, and its stdout comes
back as the tool result. The `--json` is appended only for commands that have the flag (so `cat`,
`tree`, `export`, `blame`, `resume`, … return text), and `json` is therefore *not* a
property you set. A **non-zero exit is an MCP tool error** (`isError: true`) carrying the
child's stderr, so `cv`'s exit-2 "no session matching …" reaches the agent verbatim. A
malformed or unknown argument never reaches `cv` at all: it comes back as JSON-RPC
`-32602 Invalid params`.

Because the schema is the running binary's own, **tool names, flags, harness lists and
output shapes cannot drift from the CLI** — there is no second binary to find, and so none to
be stale. Add a command or a flag to `cv` and it appears over
MCP with no change here. The page you are reading is the one place that can go stale: if
it and a live `tools/list` ever disagree, the live reply is the authority.

## Windows: `show` never dumps a whole transcript by default

The [window flags](cli.md) are the same five words everywhere in cv, and they are
properties on the `show` tool (and on `export`, which has them too):

| Property | Meaning |
|---|---|
| `first` | the first N messages |
| `last` | the last N messages |
| `range` | `"A..B"` — messages A (inclusive) to B (exclusive); `"A.."`, `"..B"` |
| `around` + `context` | message N with K either side (default 5) |
| `max_bytes` | stop after N bytes of rendered output, then print a `… continue with --range <next>..` line |

Over MCP — and **only** over MCP — `show` gets a default window: when the caller passes
**none** of those five, `cv mcp` adds `--last 50 --max-bytes 200000`. A 400-message session
would otherwise land in the agent's context in one shot, which is exactly the problem
`doctor` exists to diagnose. Naming any selector (including `max_bytes` alone, or
`pre_compaction`, which sets its own window) suppresses the defaults entirely — asking for
the whole thing is allowed, it just has to be asked for. The `ls` / `search` `limit`
defaults are the CLI's, unchanged.

## The generated tools

The CLI's Read / Reshape / Export / System groups, plus the task tools (below). Arguments are listed in full by
the live `tools/list`; only the **required** ones are shown here. "JSON" says whether the
call gets `--json` appended (and so returns machine output rather than rendered text).

### Read

| Tool | Required | JSON | What it does |
|---|---|---|---|
| `ls` | — | yes | List discovered sessions across all harnesses. `cwd` filters to a project; `query` takes the filter calculus. |
| `show` | `id` | yes | Print a single session (by `harness:id` or a unique id prefix). Windowed by default — see above. |
| `cat` | `session`, `tool_use_id` | no | Print one tool call's full output, wherever it lives: inline, in a `prune` sidecar, or in a persisted-output file. `input: true` prints the call's arguments instead. |
| `search` | `query` | yes | Full-text search across all session content. |
| `events` | `id` | yes | What a session DID: its extracted events (file edits/reads, commands, errors). |
| `touched` | `path` | yes | Every session with a file_edit/file_read event on that path. |
| `tools` | `id` | yes | Cross-agent tool analytics across the orchestrator and its whole sub-agent forest. |
| `tree` | `id` | no | A session's message threading (DAG if parent ids exist, else a numbered list). |
| `workflow` | `id` | yes | A `Workflow` run, first-class: phase tree, agents and outcomes, totals, driving script. Without `run_id`, lists the session's runs. |
| `compaction` | `id` | yes | Every compaction boundary, its trigger, pre-compaction size, and the seeding summary. |
| `prompts` | `id` | yes | Only what the person said: human prompts and `AskUserQuestion` answers, with message indices. `pre_compaction` narrows to a discarded span. |
| `lanes` | `id` | yes | The sub-agent forest as a status table: model, duration, tokens, tool calls, status, last line; `stranded` lists lanes parked on a promise. |
| `deferrals` | `id` | yes | Every deferral phrase in the assistant's text with context; `open_tasks` marks each MATCHED/UNMATCHED against the task store. |
| `stats` | — | yes | Fleet analytics over all discovered sessions. |
| `blame` | `file` | no | Which agent session wrote this code, and what was it thinking? A session whose own tool output made the commit ranks first, `exact`. |
| `doctor` | — | yes | Why a context window keeps filling: pressure by source, overhead, compaction frequency. |

### Reshape

Each produces a **new** session id; the source is never touched.

| Tool | Required | JSON | What it does |
|---|---|---|---|
| `prune` | `id` | yes | Lossless compaction into a new resumable session: bulky old tool payloads go to a sidecar behind a `[PRUNED]` marker. Retrieve one with `cat`. |
| `rewind` | `id` | yes | The agent as of a past message or commit (`at`: message index or sha), as a new resumable Claude session with a provenance sidecar; sub-agents come out standalone. |
| `splice` | `specs` | no | Compose a new session from spans of existing ones (`<id>:A..B`): `<id>:..N` forks at message N, `<base>:..N <other>:M..` grafts. |
| `port` | `id` | no | A copy that runs elsewhere — another harness (`harness`), another working directory (`cwd`), or both. |
| `resume` | `id` | no | The resume incantation for a session in its native harness. |

### Export

| Tool | Required | JSON | What it does |
|---|---|---|---|
| `export` | `id` | no | Export a session to markdown or OpenSession JSON (`redact` scrubs it first). Takes the window flags. |
| `dataset` | — | no | The corpus as a fine-tuning dataset (JSONL, one session per line). |
| `pack` | `task` | no | Compile a context bundle for a new task out of the whole corpus. |

### System

| Tool | Required | JSON | What it does |
|---|---|---|---|
| `index` | — | no | Build/refresh the full-text index that makes `search` instant. |
| `config` | — | no | View the user config and manage the export-source index. |
| `schema` | — | yes | The reference: query calculus, and the machine-readable schema of every shape cv emits. |
| `formats` | — | yes | The format census: what recent sessions contain, per harness, and what the manifest does not name. |
| `recipes` | — | no | The agent quickstart: the things agents do with cv, each as one command line. |

## What was removed in 0.11.0

The old hand-written session tools are gone, with no aliases. Each had a CLI command doing
the same job better; now there is one of each.

| Removed tool | Use instead |
|---|---|
| `list_sessions` | `ls` |
| `project_sessions(cwd)` | `ls(cwd=…)` |
| `search_sessions` | `search` |
| `read_session` | `show` (windowed by default) |
| `recall` | `pack` — the one "build context from the corpus" verb. For plain meaning-based hits, `search(semantic=true)`. |
| `prune_session` | `prune` |
| `prune_retrieve` | `cat(session, tool_use_id)` |

The hand-written `doctor` tool was replaced by the generated `cv doctor`, which takes the
same `id` and returns the same report plus everything the CLI has grown since.

## The MCP-only tools

These two have **no CLI equivalent** and stay hand-written, because one blocks and the other
holds a cursor across calls — things a one-shot command cannot do.

### `await_omen`

Block until any other agent's session emits a message matching a regex, then return it.
Watches **live** for new and appended messages — text, thinking, tool results, tool uses
— across harnesses, reacting only to activity from *now on* (not the backlog).

| Arg | Type | Default | Meaning |
|-----|------|---------|---------|
| `regex` | string | **required** | Rust regex matched against newly-emitted message text. |
| `harness` | string | — | Only watch this harness. |
| `cwd_contains` | string | — | Only watch sessions whose cwd contains this substring. |
| `timeout_secs` | number | `120` | Give up after this many seconds. |
| `interval_secs` | number | `2` | Poll interval (minimum 1). |

On a match returns `{ matched: true, harness, id, cwd, role, matched_text, event }`
(`event` is `new_session` or `appended`); on timeout returns
`{ matched: false, timed_out: true, waited_secs }`. *When to use:* wait on a sibling
agent's raw transcript — e.g. `await_omen(regex='BUILD (PASSED|FAILED)', cwd_contains='/myproj')`.
For explicit, structured coordination, use the task tools (below).

### `observe_stream`

The **non-blocking** sibling of `await_omen`: drain the newly-appended messages since an
opaque cursor and return immediately with a fresh one, so a senior orchestrator can poll a
junior's live activity on its own cadence. The first call (no cursor) records a baseline
and returns no backlog.

| Arg | Type | Default | Meaning |
|-----|------|---------|---------|
| `cwd_contains` | string | — | Only observe sessions whose cwd contains this substring. |
| `harness` | string | — | Only observe this harness. |
| `since_cursor` | string | — | Cursor from a previous call. Omit to baseline from the current tail. |
| `max_messages` | number | `50` | Max messages this call; the rest are flagged `more_pending`. |
| `char_cap` | number | `16000` | Cap on total characters returned; the last message is truncated to fit. |

Read-only — it writes nothing.

Requests are dispatched concurrently, so a parked `await_omen` never
head-of-line-blocks a `show` issued after it.

## The task substrate over MCP

The [task substrate](tasks.md) is fully drivable over MCP, and the tools are **generated
from `cv task`** like every other command tool: `task_open`, `task_list`, `task_show`,
`task_claim`, `task_release`, `task_note`, `task_done`, `task_status`, `task_abandon`,
`task_supersede`, `task_tag`, `task_block`, `task_decide`, `task_resolve`, `task_events`,
`task_sweep`, `task_inbox` — the same names, arguments and help as `cv task
<sub> --help`. What comes back is what the CLI prints:

- a write answers with its confirmation line (`✦ claimed 01a1… → claimed`; `task_open` puts the
  new task's full id on its own last line), and a refused one is a tool error carrying the
  reducer's reason (`event rejected: … cannot apply claimed` for the loser of a claim race);
- `task_list` and `task_show` answer with `--json` (the full task projection), and `task_inbox`
  with the one inbox page `cv serve` serves too (`{who, items, closed, hidden, counts}`, each item
  carrying `reason`, `effective_state`, `since` and `age`); `task_list` has the CLI's default scope (non-terminal,
  the last 14 days or involving you), and `all: true` lifts it;
- `task_events` answers with JSON lines, the poll surface.

**Identity.** The server inherits the agent's environment, so a spawner-set
`CV_ENDPOINT=agent:<name>` makes every bare call record the right actor. Identity-*bearing*
tools (`task_claim`, `task_release`, `task_status`, `task_resolve`) **error** when neither
`from` nor `CV_ENDPOINT` is set, naming the cure.

## A typical flow

1. An agent starts work in `/myproj`. It calls `ls(cwd='/myproj')` to see prior history, and
   `task_inbox()` to see what is assigned to it.
2. It `task_claim`s its task so no sibling picks it up, and `task_note`s what it is about to do.
3. Mid-task it hits a wall and calls `pack(task='refresh token rotation')` to compile
   prior work on it from the whole corpus, then `show(id=…, around=…)` on the winner to
   read the span in context — or `cat(session, tool_use_id)` to pull one tool output back
   in full.
4. It hands off: `task_done(id=…, observed='auth done, tests green in <session>')` — recorded as its
   own word, labeled self-reported.
5. A sibling that ran `await_omen(regex='auth done', cwd_contains='/myproj')` unblocks and picks
   up the next task; the orchestrator sees the close in `cv task events --since <id>`.

The thirteen board tools (post, read, await, request, reply, replies, unanswered, claim,
release, claims, who, heartbeat, ack) were removed in 0.15.0 with the coordination board; calling
one errors with a pointer at the task tools.
