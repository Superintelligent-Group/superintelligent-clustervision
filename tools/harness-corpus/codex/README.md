# Codex corpus generator

```sh
tools/harness-corpus/codex/install.sh 0.162.1
tools/harness-corpus/codex/generate.sh 0.162.1 crates/cv-core/tests/corpus/codex/0.162.1
CV_REGEN_CORPUS=1 cargo test -p clustervision-core --test corpus
```

**Needs:** npm, python3, git, and `~/.zai-key` (a Z.ai coding-plan key; override with `ZAI_KEY_FILE`).

`install.sh` installs into `~/tools/harness-versions/codex/<v>/` with `npm --prefix`. The Homebrew
`codex` and `~/.codex` are never read or written.

## The model path

Codex 0.16x speaks only the Responses API (`wire_api = "responses"` is the only variant left).
Z.ai does not serve `/responses`, and the OpenRouter dev key had expired on 2026-10-11. So
`responses_shim.py` is a small local Responses endpoint that forwards each request to Z.ai's chat
completions (`glm-5.3-flash`, `thinking` on) and streams the answer back as Responses SSE: a
`reasoning` item from `reasoning_content`, then `message`, `function_call` or `custom_tool_call`
items, and usage. Only the shim gets the key; Codex gets a dummy key for a provider at
`127.0.0.1:$PORT`.

Two things in the store come from this setup rather than from a real OpenAI backend:

- The model slug. Codex is told `model = "gpt-6-luna"` (`CODEX_MODEL_PROFILE`) so that its bundled
  metadata gives the real tool set: freeform `apply_patch`, unified exec, responses-lite tool
  prefixes, reasoning summaries. With an unknown slug Codex falls back to a reduced tool set. The
  model that actually answered is in `MANIFEST.toml`.
- No `encrypted_content`. Reasoning arrives as summary text only.

The record shapes, the persistence policy, the file layout, and the item ordering are all Codex's.

## Scenarios

| scenario | how |
|---|---|
| multi-turn chat, resume | `codex exec` once, then `codex exec resume <id>` twice on the same thread |
| read/edit/shell, parallel calls, tool error | one thread: read two files in parallel, `apply_patch` an edit then `cat`, `cat` a missing file |
| reasoning | every turn (GLM thinking → `reasoning` items) |
| image input | `codex exec --image dot.png` (persisted as `input_image`; the text model sees a placeholder) |
| compaction | resume with `-c model_auto_compact_token_limit=200`, which forces a local compaction |
| MCP | `mcp_server.py`, a stdio server with one `add` tool, in `config.toml` |
| sub-agents | `--enable multi_agent`, which spawns one child thread (its own rollout) |

`codex exec` has no `/compact`, so compaction is forced through the token limit instead.

## What is copied

Copied: `sessions/` (and `archived_sessions/` if present), `session_index.jsonl`, and
`history.jsonl`. Not copied: the sqlite projections (`state_5`, `thread_history_1`, `logs_2`,
`goals_1`, `memories_1`, `queue_1`). cv reads none of them, and `state_5` alone is about 250 KB
after VACUUM. Config, `installation_id`, shell snapshots and locks are not copied either.

## Budget

The shim refuses calls past `MAX_CALLS` (default 40) and logs every one to `$WORK/calls.jsonl`.
That log holds only item and tool types, never content or headers. A full run takes 20 to 30
calls, and the count goes into `MANIFEST.toml`.
