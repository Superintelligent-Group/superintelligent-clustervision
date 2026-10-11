# kimi-cli (legacy): what it persists, and what the corpus covers

Sources: the tool list kimi-cli 1.48.0/1.51.0 sends (`Agent`, `AskUserQuestion`, `EnterPlanMode`,
`ExitPlanMode`, `FetchURL`, `Glob`, `Grep`, `ReadFile`, `ReadMediaFile`, `SetTodoList`, `Shell`,
`StrReplaceFile`, `TaskList`, `TaskOutput`, `TaskStop`, `WriteFile`, plus MCP tools by bare name),
`~/pug/kimi-cli` at 1.48.0/1.51.0, and the stores in `crates/cv-core/tests/corpus/kimi/`. A session
is `sessions/<md5(cwd)>/<uuid>/`.

| affordance | persisted as | captured | cv IR |
|---|---|---|---|
| multi-turn chat, resume (`-S`) | `context.jsonl` `user` / `assistant` | yes | `prompt`/Human, `reply`/Model |
| read, edit, write, shell | `assistant.tool_calls` + `tool` records; `wire.jsonl` `ToolResult` | yes | `ToolUse` / `ToolResult` |
| parallel tool calls | several `tool_calls` | yes | one `reply` |
| tool error | `wire.jsonl` `ToolResult.return_value.is_error` | yes | `ToolResult{is_error}` |
| thinking | `think` parts; 1.49+ also an empty one per step | yes | `Thinking` (empty dropped) |
| image input (`ReadMediaFile`) | `image_url` part in the `tool` record | yes | `Image` inside the `ToolResult` |
| todo list (`SetTodoList`) | tool call + `state.json.todos` | yes | `ToolUse`; `state.json` not read |
| plan mode | tool calls + `state.json.plan_mode/plan_slug`; a plan-mode reminder in `context.jsonl` | yes | `ToolUse`; reminder `injected_context` |
| background shell, `TaskOutput` | `tasks/<id>/{spec,runtime,control,consumer}.json`, `output.log`; `notifications/<id>/{event,delivery}.json`; a `<notification>` user record | yes | the `<notification>` record `injected_context`/Scheduler; task files not read |
| sub-agent (`Agent`) | `subagents/<agent_id>/{context.jsonl,wire.jsonl,meta.json,prompt.txt,output}` | yes | child `Session` (`lineage.parent`, `agent_path`; `meta.json` in `extra["kimi"]["subagent"]`) |
| skills (`/skill:<name>`) | the SKILL.md body as a plain `user` record | yes | `prompt`/Human: no marker tells it from typed text |
| MCP (`--mcp-config-file`) | tool call by bare name (`add`) | yes | `ToolUse` named `add` |
| web fetch | tool call | yes | `ToolUse` |
| web search (`SearchWeb`) | needs the Moonshot search service login | **no** | |
| `AskUserQuestion` | tool call, auto-dismissed in `--afk` | yes | `ToolUse` / `ToolResult` |
| Ralph loop (`--max-ralph-iterations`) | the repeated prompt with an "automated loop" suffix as a `user` record | yes | `prompt`/Human |
| checkpoints | `_checkpoint{id}` every step | yes | `carrier` under `complete` |
| compaction (`/compact`) | rotation to `context_N.jsonl`; new segment opens with the `<system>Previous context has been compacted` summary; `wire.jsonl` `CompactionBegin/End` | yes | `compaction_boundary` + `compaction_summary` |
| `/clear` | rotation to `context_N.jsonl` | yes | `branch` (synthetic) |
| rewind (`revert_to`, D-Mail) | rotation; the kept prefix is copied into the new file | **no**: no headless trigger, `SendDMail` not in the default tool set | `branch`; the copied prefix read once |
| hooks (`[[hooks]]`) | not persisted (no record in `context.jsonl` or `wire.jsonl`) | ran, nothing to capture | |
| approvals | `state.json.approval{yolo, afk, auto_approve_actions}`; `--afk` approves everything | mode only | not read |
| interrupt | print mode has none | **no** | |
| usage | `_usage{token_count}`, `wire.jsonl` `StatusUpdate.token_usage` | yes | `Message::usage` |
| MCP loading | `wire.jsonl` `MCPLoadingBegin/End` | yes | ignored |
