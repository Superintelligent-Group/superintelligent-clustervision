# Continue (cn 1.5.47): affordances and where they persist

The store is `~/.continue/sessions/<id>.json`: `history[]` of `{message, contextItems, toolCallStates?,
conversationSummary?}`. "Persists" below means it lands in that file.

| affordance | tool / record | persists? | exercised in 1.5.47 corpus | cv IR today |
|---|---|---|---|---|
| file read / list / search | `Read`, `List`, `Search` | yes: `message.toolCalls[]` + `toolCallStates[]` | yes | ToolUse; ToolResult from `toolCallStates` |
| edit / write | `Edit`, `MultiEdit`, `Write` | yes, as above | Edit | ToolUse / ToolResult |
| shell | `Bash` | yes, as above | yes, including an error | ToolResult `is_error` from `status: "errored"` |
| background jobs | `CheckBackgroundJob` (Bash has no background flag in 1.5.47) | the check call would persist; the job table is in memory | no | ToolUse |
| todo / plan list | `Checklist` (markdown checklist argument) | yes, as a tool call | yes | ToolUse `Checklist` (no plan type) |
| plan mode | `--readonly` (tool policy) | no: the mode is not recorded in the session | yes (run, unrecorded) | none |
| status | `Status` (`--beta-status-tool`) | tool call | no: not offered in plan mode | ToolUse |
| question to the user | `AskQuestion` | tool call | no | ToolUse |
| sub-agents | `Subagent` (`--beta-subagent-tool`; agents are models with the `subagent` role) | the call and its result; the child's history is not a separate session file | yes: the call fails in `-p` mode | ToolUse + ToolResult (error) |
| fork | `--fork <id>` | a new session file holding the copied history | yes | a separate session; no lineage recorded by cn |
| resume | `--resume` | appends to the same file | yes | one session |
| compaction | `/compact`, auto-compaction (`conversationSummary` on an item) | only in interactive mode; headless `/compact` is not saved | no | not mapped yet (no `conversationSummary` in any corpus) |
| MCP | `mcpServers` tools (`add`) | tool call + state | yes | ToolUse |
| web fetch | `Fetch` | tool call + state | yes | ToolUse |
| skills | `Skills` | tool call | no | ToolUse |
| attachments / context | `contextItems[]` | yes | no (no IDE context in cn) | Block::File |
| usage, model | `message.usage` (+ `model`, `cost_cents`) | yes | yes | Usage, Message.model |
| approvals | `permissions.yaml`, `--allow/--ask/--exclude` | no: config only | n/a | none |
| hooks | Claude-style hooks (`PreToolUse` ... `SubagentStop`) | not in the session file | no | none |
| ending the run | `Exit`, `ReportFailure`, `UploadArtifact` | tool calls | no | ToolUse |
