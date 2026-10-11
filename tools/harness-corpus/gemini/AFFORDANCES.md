# Gemini CLI affordances and what they persist

From gemini-cli's source at v0.63.0 (`packages/core/src/tools/tool-names.ts`, `config/storage.ts`,
`services/chatRecordingService.ts`, `hooks/types.ts`) and the corpus runs of 0.46.0 and 0.63.0. "Recording"
is `~/.gemini/tmp/<project>/chats/session-*.jsonl`. "cv" is how cv's IR represents it today.

| affordance | tool / record | persisted as | corpus | cv |
|---|---|---|---|---|
| prompt, reply | `user` / `gemini` records | recording | yes | Prompt / Reply |
| session preamble | first `user` record `<session_context>` (via `$set.messages`) | recording | yes | InjectedContext (Harness) |
| thoughts | `gemini.thoughts[]` (+ `thought` parts after a resume re-sync) | recording | yes | Thinking |
| tool calls and results | `gemini.toolCalls[]` (+ a `user` record of `functionResponse` parts) | recording | yes | ToolUse; Tool/ToolResult (one copy) |
| parallel tool calls | several `toolCalls[]` on one record | recording | yes | several ToolUse in one Reply |
| tool error | `toolCalls[].status = "error"` | recording | yes | ToolResult `is_error` |
| token usage | `gemini.tokens` | recording | yes | Usage |
| model switch | per-record `model` | recording | yes | `Message::model` when it differs |
| resume | same file, `$set.sessionId` + `$set.messages` (0.46.0 also leaves an empty twin file) | recording | yes | one session (twin ignored) |
| image / `@file` | `inlineData` part; `displayContent` keeps the typed text | recording | yes | Image block; `extra.gemini.display_text` |
| shell, edit, read, glob, grep, ls | `run_shell_command`, `replace`, `write_file`, `read_file`, `read_many_files`, `glob`, `grep_search`, `list_directory` | recording | yes (most) | ToolUse/ToolResult |
| background shell | `run_shell_command` `is_background: true`; output in `tmp/background-processes/background-<pid>.log` | recording + log file | yes | ToolUse; log file not read |
| todo list | `write_todos` | recording (call args) | yes | ToolUse |
| task tracker | `tracker_create_task` … `tracker_visualize` (`experimental.taskTracker`); `tmp/<project>/<session>/tracker/<id>.json` | recording + files | yes | ToolUse; files not read |
| plan mode | `--approval-mode plan`, `enter_plan_mode`/`exit_plan_mode`; plan at `tmp/<project>/<session>/plans/*.md` | recording + file | yes | ToolUse; file not read |
| topic | `update_topic` | recording | yes (the model called it) | ToolUse |
| sub-agents | `invoke_agent` (`toolCalls[].agentId`); child recording `chats/<parent>/<agentId>.jsonl` (`kind: subagent`, `summary`) | recording | yes | SubagentSpawn / SubagentReturn; child `lineage.parent` |
| sub-agent completion | `complete_task` (in the child) | child recording | yes | ToolUse |
| skills | `activate_skill`; skill files in `~/.gemini/skills/` | recording | yes | ToolUse |
| custom commands | `~/.gemini/commands/*.toml`, expanded into the prompt; typed text in `displayContent` | recording | yes | Prompt; `display_text` |
| hooks | BeforeAgent `additionalContext` → trailing `<hook_context>` part | recording | yes | InjectedContext (Hook) |
| MCP tools | `mcp_<server>_<tool>` | recording | yes | ToolUse (`mcp_corpus_add`) |
| web | `web_fetch`, `google_web_search` | recording | yes | ToolUse |
| compression | automatic (`model.compressionThreshold`) | **not persisted** (the API history is rewritten in memory; the recording keeps every turn) | forced, no trace | none |
| `/compress` | interactive-only | headless sends it to the model as text | n/a | Prompt |
| `/chat save <tag>` | `tmp/<project>/checkpoint-<tag>.json` (`{history: Content[], authType}`) | file | yes | its own session `checkpoint-<tag>` |
| rewind | `$rewindTo` control record (`/rewind`, interactive) | recording | no (interactive only) | Branch |
| info / warning / error notices | `info`/`warning`/`error` records (interactive UI only) | recording | no | Notice / Error |
| file checkpoints (restore) | `general.checkpointing`: shadow git under `history/<project>/`, `tmp/<project>/checkpoints/` | TUI only; headless wrote the shadow repo, no checkpoint files | no | none |
| memory | `/memory add` writes `GEMINI.md`; `experimental.autoMemory` writes `.patch` inbox files | files, not the recording | no | none |
| ask_user | `ask_user` (needs a person) | recording | no | — |
| logs.json | `tmp/<project>/logs.json` (prompt log; `[]` headless) | file | yes (empty) | per-session prompts |
| session summary | `$set.summary` on sub-agent recordings | recording | yes | title |
| approvals | `--approval-mode`, policies; no per-call approval record | not persisted | — | — |
| interrupts | no record (a killed run just ends) | — | no | — |
