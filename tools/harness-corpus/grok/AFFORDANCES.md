# Grok CLI affordances and what the corpus captures (1.0.50)

Enumerated from the README and user guide Grok unpacks into `$GROK_HOME` (`README.md`,
`docs/user-guide/*.md`), `grok --help`, and the tool list Grok sent (`tool_definitions.json` of a
session run with the default set: 24 tools). "Headless" means `grok -p` with a BYOK model.
The IR column is how cv reads it after this lane's adapter changes.

| affordance | tool / record | persisted as | in corpus | cv IR today |
|---|---|---|---|---|
| prompt, reply | `user` / `assistant` lines | `chat_history.jsonl` (`<user_query>` wrapper, `prompt_index`) + `updates.jsonl` chunks | yes | `Prompt`/`Human`, `Reply` |
| reasoning | `reasoning` record | its own line before the assistant line (`summary[]`, `encrypted_content?`) | yes | `Thinking` block on the next turn |
| resume | `-r <id>` | same session dir | yes | same session |
| fork | `-r <id> --fork-session` | new dir; `summary.json` `parent_session_id`, `forked_at` | yes | `lineage.forked_from` |
| model switch | `-m` on resume | `assistant.model_id`, `summary.current_model_id`, `usage.json` `modelUsage` | yes | per-message `model`; no `ModelChange` |
| compaction | `/compact` | `chat_history` rewritten; `compaction/`, `compaction_checkpoints/`, `compaction_requests/`; `auto_compact_completed` update | yes | `CompactionSummary` (no boundary: the history before it is gone) |
| session title / summaries / recap | `models.session_summary` | `summary.json` `generated_title`, `session_summary`, `title_refresh_idx` | yes (chat session) | session title |
| AGENTS.md rules | project `AGENTS.md` | `<rules>` in the `<user_info>` user line, `prompt_context.json` | yes | `InjectedContext` |
| system prompt | — | `system_prompt.txt` + leading `system` line | yes | `Session::system_prompt` |
| hooks | `[[hooks.<Event>]]` in config | `updates.jsonl` `hook_execution` (name, status, elapsed); output not kept | yes | not read |
| shell, read, edit, write, list | `run_terminal_command`, `read_file`, `search_replace`, `write`, `list_dir` | `assistant.tool_calls[]` + `tool_result` lines; `tool_call`/`tool_call_update` updates | yes | `ToolUse`/`ToolResult` (error from updates) |
| parallel calls | two `tool_calls` in one line | — | yes | two `ToolUse` |
| tool error | failed command | `tool_result` + `tool_call_update status: failed` | yes | `ToolResult` error |
| output cap | `[toolset.bash] output_byte_limit` | truncated result | yes | `ToolResult` |
| rewind points | automatic | `rewind_points.jsonl` (file snapshots per prompt) | yes | `checkpoint/create` per line (store) |
| image | `read_file` on a png, `@dot.png` mention | text only with a text model (no image block persisted) | yes (as text) | `Text` |
| todo list | `todo_write` | `plan.json`, `plan` update | yes | `ToolUse`; `plan.json` not read |
| plan mode | `enter_plan_mode` / `exit_plan_mode` | `plan_mode.json`, `current_mode_update` | yes | `ToolUse` |
| sub-agent | `spawn_subagent`, `get_command_or_subagent_output` | child is a normal session (`session_kind: subagent`); parent keeps `subagents/<child>/{meta.json,output.json}`; `subagent_spawned`/`finished` updates | yes | child: `lineage.parent`, `agent_path`; no `SubagentSpawn` message |
| background command | `run_terminal_command` background + `get_command_or_subagent_output` | `terminal/`, `background_tasks`, `task_backgrounded`/`task_completed` updates | yes | `ToolUse`/`ToolResult` |
| monitor | `monitor` | tool call + result | yes | `ToolUse` |
| kill | `kill_command_or_subagent` | — | no (nothing to kill in budget) | — |
| scheduler | `scheduler_create` / `scheduler_list` / `scheduler_delete` | tool calls; `scheduled_task_created`/`deleted` updates | yes | `ToolUse` |
| MCP | `search_tool` + `use_tool` (`corpus__add`) | tool calls | yes | `ToolUse` named `search_tool`/`use_tool` |
| skills | `/corpus-greet` | `<skill_information>` appended to the user line | yes | `Prompt` |
| workflow | `workflow` (Rhai) | `workflows/wf_<run>/{state.json,journal.jsonl,script.rhai,args.json,scripts/}`; agents are `subagent` sessions; `workflow_updated` | yes (inline script; a named project workflow needs folder trust) | children: `lineage.parent` = the launching session; run state not read |
| interrupt | SIGINT | only the prompt line survives | yes | `Prompt` |
| ask_user_question | tool | — | no (offered, but nobody answers headless) | — |
| image/video generation | `image_gen`, `image_edit`, `image_to_video`, `reference_to_video` | — | no (xAI Imagine API) | — |
| web search / fetch | `web_search` (xAI Responses), `web_fetch` (off by default) | — | no | — |
| memory | `memory_search`/`memory_get`, `/memory`, `/flush` (`GROK_MEMORY=1`) | `~/.grok/memory/` | no (off by default; would need its own scenario) | not read |
| worktree | `--worktree` | `worktrees.db` | no | not read |
| LSP | `lsp` (`features.lsp_tools`) | — | no | — |
| per-session usage | — | `usage.json`, `signals.json` | yes | not read (no per-message usage in `chat_history`) |
| prompt history | — | `<cwd group>/prompt_history.jsonl` | yes | not read |
| search index | — | `sessions/session_search.sqlite` (FTS5) | no (not copied) | not read |
