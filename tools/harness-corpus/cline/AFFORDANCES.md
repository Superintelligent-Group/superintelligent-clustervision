# Cline (CLI 3.0.70, SDK store): affordances and where they persist

Store: `~/.cline/data/sessions/<sid>/` (`<sid>.json` session row, `<sid>.messages.json`, child
`*.messages.json`), `db/sessions.db` (`sessions`, `subagent_spawn_queue`, `schedules`,
`schedule_executions`), `db/teams.db` (`team_events`, `team_tasks`, `team_runs`, `team_outcomes`,
`team_outcome_fragments`, `team_runtime_snapshot`), `db/cron.db`, `db/tasks.db`, `db/connectors.db`.

| affordance | tool / record | persists? | in 3.0.70 corpus | cv IR today |
|---|---|---|---|---|
| read / search | `read_files`, `search_codebase` | messages `tool_use`/`tool_result` (array results) | read_files | ToolUse / ToolResult (array as JSON text) |
| edit | `editor` | as above | yes | ToolUse / ToolResult |
| shell | `run_commands` | as above (`{query, result, success}` per command) | yes | ToolResult |
| web fetch | `fetch_web_content` | as above | yes | ToolUse |
| question to the user | `ask_question` | tool call + the answer | yes | ToolUse / ToolResult |
| sub-agents | `spawn_agent` | child `agent_<uuid>.messages.json` (`origin.parentThreadId`, `subagent`); `sessions` row `is_subagent=1`, `parent_agent_id`; `subagent_spawn_queue` | yes | child session `<sid>/agent_<uuid>`, `lineage.parent` + `agent_path`; first prompt `origin: subagent` |
| agent teams | `team_spawn_teammate`, `team_run_task`, `team_await_runs`, `team_send_message`, `team_broadcast`, `team_read_mailbox`, `team_task`, `team_status`, `team_mission_log`, `team_*_outcome*`, `team_cleanup`, `team_shutdown_teammate`, `team_cancel_run`, `team_list_runs` | teammate `<name>__<id>.messages.json`; `teams.db` events, tasks, runs, outcomes; `team_name` on the session row | spawn, run, await, cleanup | teammate = child session; `teams.db` `team_events`: one `subagent_spawn`/`state` row per run (teammate, task, status, result) on the lead |
| plan mode | `-p` / plan vs act | not on the session row; the tool set changes (no `editor`) | yes | none |
| MCP | `<server>__<tool>` | tool call | yes | ToolUse `corpus__add` |
| reasoning | `thinking` blocks | yes | yes | Block::Thinking |
| usage / model | `metrics`, `modelInfo` per assistant turn; totals in `metadata.usage` | yes | yes | Usage, Message.model |
| run errors | assistant `metadata.displayOnly` + `displayRole: error` | yes | not in this run | MessageKind::Error |
| compaction | agentic/basic compaction, `/compact` | a compaction state (`SessionCompactionState`: messages, source hash) | no (TUI only) | not mapped |
| checkpoints / undo | `/undo` (TUI), hook-driven checkpoints | per-session state | no | not mapped |
| fork | `/fork` (named fork) | a new session | no | not mapped |
| schedules / cron | agent-created schedules, `~/.cline/schedules`, `~/.cline/cron` | `sessions.db` `schedules`, `schedule_executions`; `cron.db` `cron_specs`, `cron_runs` | no | an execution / cron run's `session_id` → that session's `schedule`/`fire` row (unit test on the DDL); a schedule names no maker session |
| hooks | `~/.cline/hooks`, `--hooks-dir` | `sessions.hook_path`; hook events in `--json` output, not in messages | no | not read |
| connectors | Slack, Telegram, ... (`cline connect`) | `connectors.db` | no | not read |
| approvals | `--auto-approve`, tool approval mode | not persisted | n/a | none |
| status / exit | `<sid>.json` `status`, `exit_code` | yes | yes | `extra.cline.status`, `exit_code` |
