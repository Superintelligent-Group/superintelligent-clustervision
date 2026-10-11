# Goose corpus generator

```sh
tools/harness-corpus/goose/install.sh 1.54.0
tools/harness-corpus/goose/generate.sh 1.54.0 crates/cv-core/tests/corpus/goose/1.54.0
CV_REGEN_CORPUS=1 cargo test -p clustervision-core --test corpus
```

**Needs:** gh (or curl), tar, python3, sqlite3, git, and `~/.zai-key` (a Z.ai coding-plan key;
override with `ZAI_KEY_FILE`).

`install.sh` unpacks the official release tarball (`goose-<target>.tar.bz2` from `block/goose`) into
`~/tools/harness-versions/goose/<v>/bin/goose`. Nothing system-wide is installed or read.

## Isolation

`HOME`, `GOOSE_PATH_ROOT` (config, state and data together) and `TMPDIR` all point into the throwaway
dir and pass `corpus_guard`; `GOOSE_DISABLE_KEYRING=1` keeps Goose out of the keychain. `HOME` matters
too: skills and plugins are discovered under `~/.agents/`.

## The model path

Goose speaks OpenAI chat completions, so `chat_shim.py` translates nothing. It forwards each request
to Z.ai's coding endpoint with the key and GLM `thinking` added, and streams the bytes back. Goose's
`openai` provider points at `127.0.0.1:$PORT` with a dummy key. The model is whatever Goose asked for
(`glm-5.3-flash`, and `glm-4.7-flash` for the model-switch turn), so every `requestedModel` in the
store is the model that answered. `MAX_CALLS` (default 60) caps the calls, and `calls.jsonl` records
roles and tool names only.

## Scenarios

`AFFORDANCES.md` lists every Goose affordance: where it persists, whether a scenario covers it, and
why not when one does not. In short, one `goose run` (or `--resume`) per step covers: chat and resume,
the developer tools, a tool error, `read_image`, auto-compaction, a stdio MCP extension (the Codex
corpus's `mcp_server.py`), a `delegate` sub-agent, todo, a project skill, a PreToolUse deny hook,
`--max-turns`, a model switch, a parameterized recipe, a scheduled job run now, chat recall,
`approve` mode with no terminal, fork, import and rename. A scenario that a version cannot run
(1.51.0 has no `goose session rename`), or that the model did not do (parallel calls), lands in
`MANIFEST.toml` `skipped` instead of `scenarios`.

Compaction is forced with `GOOSE_CONTEXT_LIMIT=8000 GOOSE_AUTO_COMPACT_THRESHOLD=0.3`. Goose
compares the previous turn's `total_tokens` with the limit, and the system prompt alone is about 4k
tokens. A tiny threshold with the real 128k limit does not trigger it.

## What is copied

Copied: `data/sessions/sessions.db`, via `VACUUM INTO` (one file with the WAL folded in), to
`<out>/sessions/sessions.db`. Not copied: config, `state/logs` (`llm_request.*.jsonl` holds whole
requests), the model catalog, `data/apps`, or `schedule.json`.

## Budget

1.51.0 used 50 calls and 1.54.0 used 36. The extra affordance scenarios need more than the usual 40.
The older version spends more because the todo extension is on by default there, and GLM calls it on
most turns.
