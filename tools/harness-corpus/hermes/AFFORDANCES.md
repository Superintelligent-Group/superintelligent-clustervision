# Hermes affordances: what persists, what the corpus exercises, how cv reads it

The sources were Hermes's tool list (what it sends the model), `hermes_cli/commands.py`
(`CommandDef`), `website/docs/user-guide/features/`, and the `state.db` both releases wrote.
"Captured" means a row for it is in the committed store: **8.31** = `corpus/hermes/2026.8.31`,
**0.21** = `corpus/hermes/0.21.6`. Everything cv reads lives in `state.db`. Files under
`HERMES_HOME` (`cron/`, `memories/`, `skills/`, `checkpoints/`, logs) are not copied, so the
affordances that persist only there show up in the store as their tool call and result.

| affordance | Hermes record / tool name | persists in state.db as | 8.31 | 0.21 | cv IR today |
|---|---|---|---|---|---|
| multi-turn chat, resume | `hermes chat -q`, `--resume` | `sessions` row + `messages` (`user`/`assistant`) | yes | yes | Prompt / Reply |
| reasoning | GLM thinking | `messages.reasoning`, `reasoning_content` | yes | yes | `Block::Thinking`; raw columns in `extra.hermes` |
| file read / edit / shell | `read_file`, `patch`, `terminal` | assistant `tool_calls` JSON + `tool` row (`tool_call_id`, `tool_name`) | yes | yes | `ToolUse` / `ToolResult` |
| parallel tool calls | two `read_file` in one turn | two calls in one `tool_calls`, then two `tool` rows | yes | yes | two `ToolUse` in one Reply |
| tool error | `terminal` exit 1 | `tool` row content `{"exit_code":1,…}` | yes | yes | `ToolResult`, `is_error: false` (Hermes has no error flag) |
| planning / todo | `todo` (8.31), `todo_list` (0.21) | tool call + result only | yes | yes | ToolUse / ToolResult |
| memory | `memory` | tool call + result (the memory itself goes to `memories/`) | yes | yes | ToolUse / ToolResult |
| cron jobs | `cronjob` (8.31), `cronjob_manage` (0.21) | tool call + result (the job goes to `cron/`) | yes | yes | ToolUse / ToolResult |
| cron runs | the scheduler's own sessions | `sessions.source='cron'` | no: needs a scheduler tick | no | (would be ordinary sessions) |
| background process | `terminal(background=true)` + `process` / `process_manage` | tool calls; `display_kind='process_complete'` notice when notified | calls only | calls only | ToolUse; `process_complete` → System Notice |
| code execution | `execute_code` | tool call + result | yes | yes | ToolUse / ToolResult |
| session search | `session_search` | tool call + result | yes | yes | ToolUse / ToolResult |
| deferred-tool search | `tool_search`, `tool_describe`, `tool_call` | tool calls; a deferred tool runs as `tool_call{name,arguments}` | `tool_describe` / `tool_call` (for MCP) | yes, for most tools | ToolUse named `tool_call`; the result row's `tool_name` is the real tool |
| MCP tool | `mcp__corpus__add` via `tool_call` | as above | yes | yes | ToolUse `tool_call` + ToolResult `tool_name: mcp__corpus__add` |
| skills | `skill_manage` (create), `-s` preload | tool call; the skill goes to `skills/`; a preload has no row of its own | yes | yes | ToolUse / ToolResult |
| `/skill` invocation | slash `/<skill>` | user row `display_kind='skill_invocation'` | no | no | not reached by an arm: stays Prompt, kind in `extra.hermes.display_kind` |
| sub-agent (sync) | `delegate_task` | parent tool call; child `sessions` row, `source='subagent'`, `model_config._delegate_from`, `parent_session_id` | yes | yes | parent ToolUse (a Reply, not `SubagentSpawn`); child excluded from discovery, `lineage.parent` when parsed |
| sub-agent (background) | `delegate_task(background=true)` (interactive only on 0.21) | `async_delegations` row; on delivery a `user` row `display_kind='async_delegation_complete'` | job only | yes | System `SubagentReturn`, origin Subagent |
| image input | `--image` (one-shot) | text `[Image attached at: <path>]` in the user row | yes, as text | yes, as text | Text (no `Block::Image`) |
| `@file:` reference | `@file:a.txt` | expanded inline under `--- Attached Context ---` | yes | yes | Text in the Prompt |
| hooks | `pre_tool_call` block, `pre_llm_call` context | blocked call → `tool` row with the hook's reason; injected context only in what was sent, never stored | yes | yes | ToolResult text |
| provider error | shim 500 | 8.31: nothing (the user row stays unanswered); 0.21: assistant row `display_kind='failed_turn'` | yes | yes | 0.21: System `Error`, origin Harness |
| model switch | `/model <m>` | no row; next user row `api_content` starts `[Note: model was just switched …]` | yes | yes | Prompt; note in `extra.hermes.api_content` |
| title | `/title`, auto title | `sessions.title`, `title_source` | yes | yes | `Session.title` |
| retry | `/retry` | superseded rows get `active=0, compacted=0` | yes | yes | hidden (lean); `complete` tags `active:false` |
| undo / rewind | `/undo` | same flags as retry; the composer is refilled with the undone prompt | yes | yes | hidden (lean) |
| queue | `/queue <prompt>` | next user row once it runs (may merge into a neighbouring user row) | yes | yes | Prompt |
| steer | `/steer` while a turn runs | user row `display_kind='steer'` | no: needs a turn in flight | no | Prompt; kind in extra |
| compaction | `/compress`, `/compress here 1` | in place: old rows `active=0, compacted=1`; summary row `_compressed_summary=1`; tail cloned | yes | no: refused by the anti-growth guard | `CompactionBoundary` + `CompactionSummary` |
| branch | `/branch` | new session, `model_config._branched_from`; parent `end_reason='branched'` | yes | yes | separate session, `lineage.forked_from` |
| goal | `/goal` | `state_meta` key `goal:<session>` (`{goal, status, turns_used, …}`) + its turns; a continuation nudge would be an `auto_continue` row (none written: the judge said done after one turn) | yes | yes | turns are Prompt / Reply; the goal state is not read; `auto_continue` → Notice |
| loop | `/loop 1m …` | `state_meta` key `loop:<session>` + each wakeup as an ordinary user turn | yes | yes | Prompt / Reply; loop state not read |
| heartbeat | `/heartbeat every …` | `state_meta` key `heartbeat:<session>`; a fire is an ordinary user turn | set and cleared, never fired | same | heartbeat state not read |
| btw | `/btw` | side answer printed, not stored | not persisted | not persisted | — |
| bg | `/bg <prompt>` | a separate session, id `bg_<HHMMSS>_<hex>` | yes | yes | its own session |
| foreign import | `hermes sessions import <claude jsonl>` | session `source='claude-code'`, `origin_json.imported_from` | yes | yes | every message `Origin::Import` |
| pin / rename / archive | `hermes sessions pin/rename/archive` | `sessions.pinned`, `title`, `archived` | yes | yes | `extra.hermes.session.{pinned,archived}` |
| auto-archive | archive sweep | `sessions.auto_archived` (v30) | no (sweep not run) | column present | session extra |
| message identity | every row (v31) | `message_uid`, `absorbed_message_uids`, `tool_call_uids`, `tool_call_uid` | — | yes | `extra.hermes` |
| clarify | `clarify` | tool call + answer | no: needs a user to answer | no | (ToolUse) |
| web search / fetch | `web_search`, `web_extract` | tool call + result | no: no search backend key | no | (ToolUse) |
| checkpoints / rollback | `--checkpoints`, `/rollback` | `checkpoints/` shadow repos, not state.db | no | no | — |
| kanban | `hermes kanban` | `kanban.db` (separate store) | no | no | — |
| plan | `/plan` | a markdown file in `.hermes/plans/` | no | no | — |
| personality / reasoning / yolo toggles | slash toggles | config, or `display_kind='personality_switch'` | no | no | undocumented kind: keeps row role |
