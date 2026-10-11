# Devin CLI (3000.11.3): affordances and where they persist

Store: `~/.local/share/devin/cli/sessions.db` (`sessions`, `message_nodes` forest, `prompt_history`,
`tool_call_state`, `subagent_heads`, `rendered_commits`, `app_state`), `summaries/<agent>/history_<hex>.md`,
`transcripts/<id>.json`.

| affordance | tool / record | persists? | in 3000.11.3 corpus | cv IR today |
|---|---|---|---|---|
| read / search | `read`, `find_file_by_name`, grep tools | assistant `tool_calls[]` + `tool` rows | read, find_file_by_name | ToolUse / ToolResult |
| edit | `edit` | as above, plus `chisel/undo` on the result row | yes | ToolResult `details` |
| shell | `exec` | as above, plus `chisel/terminal_output` | yes, including a failure | ToolResult `is_error` from `chisel/tool_result_meta.success` |
| tool permission states | ACP ToolCall / ToolCallUpdate | `tool_call_state` (14 rows here) | yes | ignored (duplicates the assistant row) |
| todo list | `todo_write` | tool call; `compact/todo_list` on a compaction wrapper | yes | ToolUse |
| plan mode | `/plan`, `--permission-mode` | `sessions.agent_mode` (`plan`, `bypass`, ...) | yes | `extra.devin.agent_mode` |
| sub-agents (run_subagent) | `run_subagent` (profiles such as `Explore`) | the child chain is a second root in `message_nodes`; the result row's `subagent/agent_id`, `subagent/chain_node_id`, `subagent/profile_name`, `subagent/model` | yes | child session `<id>/<agent_id>` with `lineage.parent`, `spawned_by_tool_use` |
| Fusion sidekick | lead + sidekick pairing | `subagent_heads` rows | no (paid lead) | child session (fixture-tested) |
| compaction | auto, `/compact` | `summarized_from` node pairs + `summaries/` history file | no (see README) | CompactionBoundary + CompactionSummary (fixture-tested) |
| MCP | `mcp_list_tools`, `mcp_call_tool` | tool calls | yes | ToolUse |
| web | `webfetch` (and web search) | tool calls | webfetch | ToolUse |
| typed prompt history | the prompt box | `prompt_history` (12 rows) | yes | not read |
| rules / skills | AGENTS.md rules, skills | `agent-ext/rules-loaded`, `agent-ext/skills-loaded` on system rows | skills-loaded | `extra.devin.rules_loaded` / `skills_loaded` |
| question dialog | ask-user tool | `chisel/user_question_answers` | no | carried verbatim |
| revert / fork | `/revert`, `/fork` | forest branches (`compact/prior_node_ids`), `chisel/undo` | no | main chain only; branches counted |
| commits | `rendered_commits` | table (empty here) | no | ignored |
| model / usage | `generation_model`, `metrics` per assistant row | yes | yes | Message.model, Usage |
| cloud handoff | `/handoff` | not in the local store | n/a | none |
