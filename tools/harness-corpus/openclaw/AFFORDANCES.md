# OpenClaw affordances: what persists, what the corpus exercises, how cv reads it

Sources: the tool lists both versions send (2026.1.29: 22 tools in every request; 2026.10.1: a
14-tool surface with 57 more behind `tool_search` / `tool_describe` / `tool_call`), the CLI help,
and the stores in `crates/cv-core/tests/corpus/openclaw/<v>/`. "1.29" and "10.1" are the versions.

| affordance | record / tool name | persisted as | corpus | cv IR |
|---|---|---|---|---|
| multi-turn chat | `message` role `user` / `assistant` | transcript entries | both | `Prompt`/Human, `Reply`/Model |
| resume | same session id, later process | appended entries | 10.1 (1.29: impossible after a reset, see README) | same session |
| file read / edit / write | `read`, `edit`, `write`, `apply_patch` (10.1) | `toolCall` block + `toolResult` message | both (read, edit) | `ToolUse` + `ToolResult` |
| shell | `exec` | toolCall/toolResult | both | `ToolUse` + `ToolResult` |
| parallel tool calls | two `toolCall` blocks in one assistant message | same | both | two `ToolUse` in one `Reply` |
| tool error | `toolResult.isError` / error JSON text | same | both (`cat` of a missing file) | `ToolResult` with error flag |
| background process | `exec` background + `process` poll | toolCall/toolResult | both | `ToolUse` + `ToolResult` |
| background completion notice | 1.29: a `user` message "System: … Exec completed …" + heartbeat turn | user message, no marker | 1.29 | `Prompt`/Human (text-only; no structural marker) |
| thinking | `thinking` block; `thinking_level_change` entry | both | both | `Thinking` block; `ModelChange` |
| image | `read` (1.29) / `view_image` (10.1) result with an `image` block | toolResult content | both | `Image` block in `ToolResult` |
| sub-agent spawn | `sessions_spawn` (1.29 direct; 10.1 via `tool_call`) | parent toolCall/result (`childSessionKey`); child session under `agent:main:subagent:<uuid>` with `spawnedBy` / `spawned_by` | both | parent `ToolUse sessions_spawn` (10.1: `tool_call` + nested-tool `Notice`); child `lineage.parent`, prompts `origin: subagent` |
| sub-agent wait | `sessions_history` (1.29), `sessions_yield` (10.1) | toolCall/result; 10.1 also `custom_message openclaw.sessions_yield` | both | `ToolUse`; `Notice` |
| sub-agent return | 1.29: "[Queued announce messages …]" user text; 10.1: user message with `provenance.kind = inter_session` from the sub-agent key | user message | both | 1.29 `Prompt`/Human (text-only); 10.1 `SubagentReturn`/Subagent |
| sub-agent registry | 1.29 `subagents/runs.json`; 10.1 `state/openclaw.sqlite` `subagent_runs` | side store | both (copied) | one `subagent_spawn`/`state` row per run on the requester / controller session (task, outcome, result) |
| cron / scheduled work | `cron` tool (1.29) / `automations` via `tool_call` (10.1); `openclaw cron run` | job in `cron/jobs.json` (1.29) or `cron_jobs` (10.1); run session `agent:main:cron:<job>[:run:<id>]` | both | run prompt `origin: scheduler` (key; 10.1 also `provenance internal_system/cron`); each job a `schedule`/`state` row on its owner (10.1 `owner.sessionKey`), its run sessions, and a `sessionTarget: main` job's main session |
| heartbeat | heartbeat turn ("HEARTBEAT_OK") | ordinary messages | 1.29 (incidental) | `Prompt`/`Reply` |
| memory | `memory_search`, `memory_get` (1.29 direct; 10.1 via `tool_call`) | toolCall/result | both (1.29: search disabled, no embedding key) | `ToolUse` + `ToolResult` |
| web fetch | `web_fetch` | toolCall/result | both (loopback URL; 10.1 refuses private hosts) | `ToolUse` + `ToolResult` |
| web search | `web_search` | toolCall/result | no (needs a search API key) | — |
| MCP | `mcp.servers` (10.1 only) through `tool_call` | toolCall/result + nested-tool record | 10.1 | `ToolUse tool_call` + `Notice` (`custom_details.toolName`) |
| nested tool record | `custom`, `customType: openclaw.nested-tool.v1` | message role `custom` | 10.1 | `Notice`, details in `extra.openclaw.custom_details` |
| tool discovery | `tool_search`, `tool_describe` | toolCall/result | 10.1 | `ToolUse` + `ToolResult` |
| compaction (auto) | `compaction` entry (`fromHook`) | transcript entry | both (tiny-window model) | `CompactionBoundary` + `CompactionSummary` |
| compaction (explicit) | gateway `sessions.compact` | 10.1: `compaction` entry; 1.29: truncation to `maxLines` + `<id>.jsonl.bak.<ts>` archive | both | 10.1 boundary + summary; 1.29 a transcript that starts mid-session (archive not read) |
| reset / new | gateway `sessions.reset` | 10.1: `reset` entry in the same transcript; 1.29: new session id under the key | both | 10.1 `Branch`; 1.29 a new session |
| model switch | gateway `sessions.patch {model}` | `model_change` entry | both | `ModelChange` |
| cache bookkeeping | `custom openclaw.cache-ttl`, `custom model-snapshot`, `openclaw:bootstrap-context:full` | `custom` entries | both | `Carrier` (complete only) |
| payload compression | `transcript_events.event_zstd` | sqlite column | 10.1 (17 rows) | decoded like `event_json` |
| session title / label | `session_info`, `label`, `sessions.patch {label}` | entries / index | no | title, `extra.openclaw.labels` |
| branch / rewind | `leaf` controls | entries | no (no CLI path) | visible-branch replay |
| inter-session send | `sessions_send` | target session gets an `inter_session` user message | no | `Prompt`/Subagent (10.1 provenance) |
| messaging channels | `message`, channel delivery | delivery records, mirrors | no (needs a channel login) | — |
| browser / canvas / nodes / tts / computer | those tools | toolCall/result | no (need a browser, device or audio) | `ToolUse` |
| exec approvals | `approvals` / exec-policy | approvals tables | no (local mode runs without prompts) | — |
| hooks | internal hooks (`session-memory`, `command-logger`) | workspace files / logs, not the session store | no | — |
| skills | `skills.*`, injected into the system prompt; `SKILL.md` read with `read` | `sessionsJson.skillsSnapshot` (1.29), `session_entry_snapshots` (10.1, not copied) | disabled (`allowBundled: []`) | — |
| slash commands | `/compact`, `/new`, `/model` … | only from a channel or the TUI; the CLI refuses them | via the gateway methods above | — |
| interrupt / abort | `chat.abort`, run timeout | `stopReason: "aborted"` on the assistant message | no | `Reply` with stop reason in extra |
