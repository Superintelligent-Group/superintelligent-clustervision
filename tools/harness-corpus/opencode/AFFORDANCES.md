# OpenCode affordances: what persists, and what the corpus exercises

From the source (`~/pug/opencode`, tags `v1.18.20` and `v2.0.25`), the tool lists each version sends
(1.18.20: `bash edit glob grep read skill task todowrite webfetch write`, plus MCP tools; 2.0.25:
`edit glob grep question read shell skill subagent webfetch websearch write execute`), and the stores
the generator produced. "1.x" means the `message`/`part` tables, "2.x" the `session_message` table
(both in `opencode.db`). Corpus columns: **yes** = in `tests/corpus/opencode/<v>/`.

| affordance | 1.18.20 record | 2.0.25 record | 1.18.20 | 2.0.25 | cv IR |
|---|---|---|---|---|---|
| multi-turn chat, resume (`run -s`) | `message` rows + `text` parts | `user` / `assistant` rows | yes | yes | `Prompt` / `Reply` |
| reasoning | `reasoning` part | `assistant.content[].reasoning` | yes | yes | `Block::Thinking` |
| file/shell tools | `tool` part, `state{status,input,output,metadata,time}` | `assistant.content[].tool`, `state{status,input,content[],error?}` | yes | yes | `ToolUse` + `ToolResult` (Tool message) |
| parallel tool calls | several `tool` parts in one message | several `tool` items in one row | yes | yes | several `ToolUse` |
| tool error | `state.status = error`, `state.error` string | `state.status = error`, `state.error{type,message}` | yes (denied `rm`) | yes (interrupted `sleep`) | `ToolResult.is_error` |
| permission rules (deny) | the denied call is a `tool` error naming the rule | same shape (the turn was then reverted away, see MANIFEST) | yes | no | `ToolResult.is_error`; `approval/deny` |
| saved approvals ("always allow") | `permission` table (per project) | `permission` table; `session_v2.permission` ruleset | no: headless `run` cannot answer a prompt | no | `permission` column → session `extra`; the ruleset is `approval/state` (the table is not read) |
| `question` tool (ask the user) | — | `question` tool call | — | no: needs an interactive answer | `ToolUse` |
| todo list | `todowrite` tool + `todo` table | removed in 2.x | yes | — | `ToolUse`; `todo` table not read |
| plan mode | `agent: "plan"` on the message | `agent-switched` rows + `system` reminder rows | yes | yes | bag `agent`; `Notice` + `InjectedContext` |
| sub-agent (`task` / `subagent`) | child `session.parent_id` + `task` tool | child `session_v2.parent_id` + `subagent` tool | yes | yes | `Lineage::parent`; `ToolUse` |
| background sub-agent | — | `subagent{background:true}`, completion = `synthetic` row `metadata.source = "subagent"` | — | yes | `SubagentReturn`/Subagent |
| background shell | — | `shell{background:true}`, completion = `synthetic` row `metadata.source = "shell"`; output file under `data/shell/` | — | yes (file not copied) | `InjectedContext` |
| `@agent` mention / `subtask` part | `agent` / `subtask` parts | `user.agents[]` | no: TUI syntax | no | `SubagentSpawn` |
| compaction (manual, the TUI's `/compact`) | user message with `compaction` part + assistant `summary: true` | one `compaction` row `{status, reason, summary, recent}` | yes (`POST /session/:id/summarize`) | yes (`POST /api/session/:id/compact`) | `CompactionBoundary` + `CompactionSummary` |
| auto-compaction | same records, `auto: true` | `reason: "auto"` | no: a tiny context limit makes 1.18 loop (compact, "continue", overflow, ...) | no | as above |
| model switch | per-message `modelID` | `model-switched` row | yes | yes | `Message::model`; 2.x `ModelChange` |
| MCP tool | tool named `<server>_<tool>` (`corpus_add`) | called through code mode: `execute` tool, `metadata.toolCalls[]` | yes | yes | `ToolUse` (`corpus_add` / `execute`) |
| skills | `skill` tool | `skill` tool; `skill` row and `user.skills[]` when a skill is attached | yes (tool) | yes (tool); row no | `ToolUse`; row → `InjectedContext` |
| custom command | `run --command` → expanded prompt | `POST /api/session/:id/command` | yes | no: `run` quotes the text | `Prompt` |
| user shell (`!cmd`) | user message + an assistant message holding a `bash` tool part | `shell` row `{command, status, exit, output}` | yes | yes | 1.x `ToolUse`; 2.x `InjectedContext`/Human |
| fork (`run --fork`) | new session, title `(fork #1)`, no link column | `session_v2.fork_session_id`, `fork_boundary` | yes | yes | 2.x `Lineage::forked_from`; 1.x none |
| revert / undo | `session.revert` JSON (rows kept) | staged: `session_v2.revert`; committed: rows deleted | yes | yes (both) | session `extra["opencode"]["revert"]` → `checkpoint/restore`; no `Branch` marker |
| snapshots (checkpoints) | `step-start`/`step-finish` `snapshot`, `patch` parts; `snapshot/` git dir | `assistant.snapshot{start,end,files}` | yes (git dir not copied) | yes | bag `steps` / `patches` / `snapshot`; a patch or file-changing snapshot is `checkpoint/create` |
| interrupt | assistant `error.name = MessageAbortedError` | assistant `error.type = aborted`, tool `tool.interrupted` | yes (`POST /session/:id/abort`) | yes (`POST /api/session/:id/interrupt`) | `Error` |
| provider error | `error.name = APIError` | `error.type = provider.invalid-request` (our 400), `status`, `response.body` | yes | yes | `Error`, bag `error_name` |
| retry | `retry` part | `assistant.retry` | no | no | bag `retries` / `retry` |
| image / file attachment | `file` part, data URL | `user.files[]`, base64 | yes | yes | `Block::Image` / `Block::File` |
| web fetch | `webfetch` tool | `webfetch` tool | yes | yes | `ToolUse` |
| web search | — | `websearch` tool | — | no: needs OpenCode's search provider | `ToolUse` |
| turn end | (none) | `idle` row `{outcome}`; `session_v2.idle_outcome` | — | yes | `Carrier` under `complete` only |
| location switch | — | `location-switched` row | — | no: `session move` | `Notice` |
| instruction snapshots (AGENTS.md etc.) | — | `instruction_entry/blob/state` | — | yes (incidental) | not read |
| queued prompts / steering | `session_input` (empty in 1.18) | `session_inbox`, `session_pending` (drained on delivery) | no | no (empty after delivery) | not read |
| sync event log | `event` (615 rows) | `event` | thinned | thinned | not read |
| share | `session_share`, `share_url` | `share_url` | no: disabled | no | session `extra` |
| archive | `time_archived` | `time_archived` | no | no | session `extra` |
| prompt history | `~/.local/state/opencode/prompt-history.jsonl` (TUI) | same | no: TUI only | no | not read |
| hooks / plugins | JS plugins; no record of their own | same | no | no | — |
| schedules, timers, memory | none in OpenCode | none | — | — | — |
