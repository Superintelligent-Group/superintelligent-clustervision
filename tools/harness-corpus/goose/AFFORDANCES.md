# Goose affordances: what persists, and what the corpus exercises

Enumerated from the `block/goose` source at `v1.51.0` (`1a4249ac`) and `v1.54.0` (`cd643d21`):
`crates/goose/src/agents/platform_extensions/` (the built-in extensions), `crates/goose-cli`
(`goose --help`, `goose run --help`), `session/session_manager.rs` (the store), `hooks/`, `plugins/`,
`skills/`, `context_mgmt/`. The tool list is the one Goose 1.54.0 sent on its first request: `analyze,
apps__{create,delete,iterate,list}_app(s), delegate, edit, extensionmanager__{list_resources,
manage_extensions, read_resource, search_available_extensions}, load, load_skill, read_image, shell, tree,
write` (1.51.0 also enables `todo` by default).

The store is one sqlite file, `$GOOSE_PATH_ROOT/data/sessions/sessions.db` (schema 16 in both
versions): `sessions`, `messages` (`content_json`, `metadata_json`), `usage_ledger`, plus
`provider_inventory_*` (a model catalog cache, no conversation). Scenario names are the ones in
`generate.sh` and `MANIFEST.toml`.

| affordance | where it persists | exercised | record / tool name |
|---|---|---|---|
| multi-turn chat, resume | `messages` rows; `goose run --resume --name` appends to the same session | yes (`chat`) | `text` blocks |
| reasoning | `thinking{thinking, signature:""}` block on the assistant row | yes (every GLM turn) | `thinking` |
| per-turn context | an agent-only user row `{userVisible:false, turnContext:true}` holding `<turn-context>` (time, cwd, `<turn-budget>`, todo notes, MOIM text) | yes (every prompt) | `metadata_json.turnContext` |
| shell / write / edit / tree | `toolRequest` on the assistant row, `toolResponse` on the next user row; shell adds `structuredContent{stdout, stderr, exit_code}` | yes (`tools`) | `shell`, `write`, `edit`, `tree` |
| analyze (tree-sitter) | tool call | yes (`tools`) | `analyze` |
| parallel tool calls | several `toolRequest`s in one assistant row | asked; GLM did not batch | — |
| tool error | `toolResult{status:"error", error}` or `value.isError` | yes (`tools`, `hook`) | |
| image input | `read_image` returns an rmcp `image` content item inside the `toolResponse`; an import carries a top-level `image` block | yes (`image`, `import`) | `read_image` |
| auto-compaction | every earlier row's `metadata_json.agentVisible` flipped to `false` in place, then three agent-only rows: the summary (user), a continuation note (assistant), the preserved prompt (user); `usage_ledger.is_compaction = 1`. No `isCompaction` in `metadata_json`, and the "Exceeded auto-compact threshold" notice is not stored | yes (`compact`, forced with `GOOSE_CONTEXT_LIMIT`) | |
| manual `/compact` | same writer (`manual_compact`) | no: interactive only | |
| tool-pair summarization | the pair's rows go agent-invisible, one agent-only summary row is added | no (needs a long tool loop) | |
| MCP stdio extension | tool name `<extension>__<tool>`; `extension_data.enabled_extensions.v0` lists it | yes (`mcp`) | `corpus__add` |
| sub-agents (summon) | `delegate` call; the child is its own session, `session_type = sub_agent`, `parent_session_id` = parent, recipe "Delegated Task" in `recipe_json`; the parent's `toolResponse.value._meta.subagent_session_id` names the child | yes (`subagent`) | `delegate` |
| knowledge loading (summon) | `load` tool call | no | `load` |
| skills | `load_skill` tool call; skills live in `~/.agents/skills` or `<project>/.agents/skills` | yes (`skill`) | `load_skill` |
| todo list | `todo__todo_write` tool call; state in `sessions.extension_data["todo.v0"].content`; echoed into later `<turn-context>` rows | yes (`todo`; default-on in 1.51.0, `--with-builtin todo` in 1.54.0) | `todo__todo_write` |
| hooks (Open Plugins `hooks.json`) | only their effect: a PreToolUse deny becomes a `toolResult` error `-32603: Tool call denied by policy hook \`<plugin>\`: <reason>` | yes (`hook`) | |
| max turns | the next `<turn-context>` carries `<turn-budget>n/n used</turn-budget>`; the run stops after the tool result | yes (`maxturns`) | |
| model switch | `metadata_json.inference.requestedModel` per assistant row; `sessions.model_config_json` is overwritten with the latest model | yes (`switch`) | |
| recipes | `sessions.recipe_json` (1.54 strips parameters the rendered recipe no longer references), `user_recipe_values_json` | yes (`recipe`) | |
| scheduled jobs | the run is a session with `session_type = scheduled` and `schedule_id`; the job itself is in `schedule.json` (not copied) | yes (`schedule`, `goose schedule run-now`) | |
| chat recall | `chatrecall__chatrecall` tool call over past sessions | yes (`recall`) | `chatrecall__chatrecall` |
| approvals | `GOOSE_MODE=approve` headless: Goose refuses before inference, so only the prompt rows persist. Interactive approvals would write `toolConfirmationRequest` / `actionRequired` blocks | the refusal only (`approve`) | |
| fork | `goose session --resume --fork` copies every message into a NEW session with the same name; no link to the source is stored, but the copies keep their `message_id` | yes (`fork`) | `lineage.forked_from`: the earliest-created other session holding the fork's first `message_id` |
| import | `goose session import <claude/codex/pi .jsonl>`: `session_type = user`, no provenance stored | yes (`import`) | |
| rename | `sessions.name`, `user_set_name = 1` | yes in 1.54.0 (`rename`); 1.51.0 has no `goose session rename` | |
| usage and cost | `metadata_json.usage{inputTokens, outputTokens, totalTokens, cacheReadTokens, cost, costSource, elapsedMs, timeToFirstTokenMs}`; `sessions.accumulated_*`; `usage_ledger` per call | yes (every turn) | |
| top of mind (MOIM) | `GOOSE_MOIM_MESSAGE_TEXT/FILE` text inside the `<turn-context>` row | no (same row shape as turn context) | `tom` |
| steer | `metadata_json.steer` on a queued user row | no: interactive queue only | |
| interrupt / cancel | | no: needs a terminal Ctrl-C | |
| `!cmd` bang shell | `BangShellOperation` | no: interactive only; in `goose run -t` the text goes to the model | |
| apps | `apps__*` tool calls; the HTML lands in `data/apps/` | no | `apps__create_app` |
| extension manager | `extensionmanager__*` tool calls; enabling one rewrites `extension_data` | no | |
| orchestrator, code mode, summarize, computer controller (web) | tool calls, default-off | no | |
| archive, project | `sessions.archived_at`, `project_id` | no (desktop/ACP features) | |

Unreleased on main (`3bd85200`, schema 17): `sessions.system_prompt_override`,
`system_prompt_extras_json`, `container_id`. No corpus can carry these until a release ships them.
