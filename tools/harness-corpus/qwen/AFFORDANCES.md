# Qwen Code affordances and what they persist

From qwen-code v0.25.1 (`packages/core/src/tools/tool-names.ts`, `services/chatRecordingService.ts`,
`config/storage.ts`, `agents/agent-transcript.ts`, `hooks/types.ts`) and the corpus run. "Session" is
`~/.qwen/projects/<cwd>/chats/<id>.jsonl`. "cv" is how cv's IR represents it today.

| affordance | tool / record | persisted as | corpus | cv |
|---|---|---|---|---|
| prompt, reply | `user` (`provenance: real_user`) / `assistant` | session | yes | Prompt / Reply |
| reasoning | `thought: true` parts | session | yes | Thinking |
| tool calls, results, errors | `functionCall` parts; `tool_result` records (`toolCallResult.status`) | session | yes | ToolUse; Tool/ToolResult (`is_error`, `status`) |
| parallel tool calls | several `functionCall` parts, one `tool_result` record each | session | yes | several ToolUse |
| deferred tools | `tool_search`, then `tool_call {name, arguments}` | session | yes | ToolUse named for the inner tool, namespace `tool_call` |
| token usage | `usageMetadata` (Gemini shape) | session | yes | Usage (input disjoint from cache, reasoning) |
| model switch | per-record `model`; `session_model` system record from `/model` | session | yes (per record) | `Message::model`; ModelChange |
| resume | same file, appended | session | yes | one session |
| fork | `--fork-session`: a new session, every record `forkedFrom` | session | yes | `lineage.forked_from` |
| title | `custom_title` system record (`/rename`, auto titles) | session | no (no fast model configured, no record written) | `Session::title` |
| image / `@file` | `at_command` system record + `inlineData` part | session | yes | Notice + Image block |
| shell, edit, read | `run_shell_command`, `edit`, `write_file`, `read_file`, `grep_search`, `glob`, `list_directory` | session | yes (most) | ToolUse/ToolResult |
| background shell | `is_background`; `tmp/<hash>/background-shells/<session>/shell-*.{output,status}` | session + files | yes | ToolUse; files not read |
| todo list | `todo_write` (`tools.todoWrite.enabled`) | session | yes | ToolUse |
| task tracker | `task_create`/`task_update`/`task_list` (with `experimental.agentTeam`) | session | yes | ToolUse |
| teams | `team_create`, `send_message`, `team_plan_approval`, `request_shutdown` | session | `team_create` only (the model called it) | ToolUse |
| sub-agents | `agent` (via `tool_call`); `subagents/<parent>/agent-<id>.jsonl` (`isSidechain`) + `.meta.json` | files | yes | SubagentSpawn / SubagentReturn; child `lineage.parent`, `spawned_by_tool_use`, `agent_path` |
| sub-sessions, workflows, arena | `create_sub_session`, `workflow`, `/arena` | session / `projects/<cwd>/workflows/` | no (budget) | — |
| cron / loops | `cron_create`/`cron_list`/`cron_delete`, `loop_wakeup`; `tmp/<hash>/scheduled_tasks.json`; a fired job is a `user` record `subtype: cron` | session + file | create/list yes; no fire (the process exits first) | ToolUse; a fire would be InjectedContext (Scheduler) |
| goals | `/goal` → `goal_state` system records, `goal_runtime` user/tool_result records, `get_goal`/`update_goal` | session | yes | Notice `[goal create] …`; InjectedContext |
| plan mode | `--approval-mode plan`, `exit_plan_mode` | session | the plan turn only; `exit_plan_mode` is not offered headless | Reply |
| skills | `skill`; `~/.qwen/skills/` | session | yes | ToolUse |
| custom commands | `~/.qwen/commands/*.toml`, expanded into the prompt | session (expanded text only; no `slash_command` record headless) | yes | Prompt |
| hooks | `UserPromptSubmit` `additionalContext` → trailing `<qwen:user-prompt-submit-context>` part | session | yes | InjectedContext (Hook) |
| MCP | `mcp__<server>__<tool>` (deferred, via `tool_call`) | session | yes | ToolUse `tool_call/mcp__corpus__add` |
| web | `web_fetch`, `web_search` | session | `web_fetch` yes | ToolUse |
| compression | `/compress` → `chat_compression` system record `{info, compressedHistory}` | session | yes | CompactionBoundary + CompactionSummary |
| memory | `save_memory`/`manage_memory`/`search_memory`; `~/.qwen/memories/`, `projects/<cwd>/memory/` | files | no: not offered headless with managed auto memory off | — |
| rewind / branch | `rewind`, `branch_checkpoint` system records (`/rewind`, `/branch`, interactive) | session | no | Branch; ignored |
| notifications, background completions | `user` `subtype: notification`, `system` `background_task_completed` | session | no | InjectedContext; Notice |
| turn errors | `turn_result` with `error` | session | no | Error |
| telemetry | `ui_telemetry` (request/tool lifecycle) | session (a third of the bytes) | yes | ignored |
| attribution, file history | `attribution_snapshot`, `file_history_snapshot` | session | attribution yes | ignored |
| worktrees | `enter_worktree`/`exit_worktree`, `--worktree` | session | no | — |
| ask user | `ask_user_question` (needs a person) | session | no | — |
| approvals | `--approval-mode`, `executionContext.approvalMode` on each prompt; `session_approval_mode` records | session | yes (field) | ignored |
| notebooks, LSP, image generation, omni media tools | `notebook_edit`, `lsp`, `image_gen`, `omni_*` | session | no | — |
