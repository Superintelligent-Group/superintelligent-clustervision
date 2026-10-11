# Zed (store as of app 1.16.1): affordances and where they persist

From the snapshot in `crates/cv-core/tests/corpus/zed/1.16.1/` and cv's adapter (`harness/zed.rs`).
Nothing here is exercised by a generated corpus (see README.md).

| affordance | record | persists? | cv IR today |
|---|---|---|---|
| tool calls / results | 0.3.0 `Agent.content[].ToolUse`, `Agent.tool_results{}`; 0.2.0 `tool_uses[]`, `tool_results[]` (outputs: edit `old_text/new_text/raw_output`, search `results[]`, `paths[]`) | yes | ToolUse / ToolResult |
| thinking | `Thinking{text, signature}`, `RedactedThinking`; segments `type: thinking` | yes | Block::Thinking |
| attachments / mentions | `User.content[].Mention{uri, content}`, `Image`; 0.2.0 `context` + `creases` | yes | Block::File (URI); context in extra |
| sub-agents | `threads.parent_id`; blob `subagent_context{parent_thread_id, depth}` | yes | `lineage.parent`; `extra.zed.subagent_depth` |
| compaction | `{"Compaction": {"Summary": ...}}` message; `detailed_summary(_state)` | yes | CompactionSummary |
| resume | bare `"Resume"` message | yes | dropped (no content) |
| profile / mode | blob `profile`, `completion_mode` | yes | extra |
| thinking settings | `thinking_enabled`, `thinking_effort`, `speed` | yes | not mapped |
| usage | `cumulative_token_usage`, `request_token_usage` | yes (per thread / per request) | cumulative in extra; per-request not mapped |
| model | blob `model{provider, model}` | yes | Session.model |
| tool-use limit | `tool_use_limit_reached` | yes | not mapped |
| worktree / branch | `worktree_branch`, `folder_paths`, `initial_project_snapshot.git_state` | yes | cwd, GitInfo |
| external (ACP) agent threads | `db/0-stable/db.sqlite` `sidebar_threads(session_id, agent_id, title, archived, ...)` | metadata only; transcripts live with the agent | not read |
| archived worktrees | `archived_git_worktrees`, `thread_archived_worktrees` | yes | not read |
| terminal threads | `sidebar_terminal_threads` | yes | not read |
| imported threads | blob `imported` | yes | extra |
| approvals | not in the store | no | none |
