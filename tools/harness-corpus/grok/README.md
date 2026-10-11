# Grok CLI corpus generator

```sh
tools/harness-corpus/grok/install.sh 1.0.50
TMPDIR=<your scratch dir> tools/harness-corpus/grok/generate.sh 1.0.50 crates/cv-core/tests/corpus/grok/1.0.50
CV_REGEN_CORPUS=1 cargo test -p clustervision-core --test corpus
```

**Needs:** curl, zstd (optional; the raw binary is the fallback), python3, git, uuidgen, and
`~/.zai-key` (override with `ZAI_KEY_FILE`). No xAI login: nothing from `~/.grok` is read or copied.

`install.sh` downloads the artifact the official installer (`https://x.ai/cli/install.sh`) uses,
`grok-<v>-<os>-<arch>[.zst]` from `https://x.ai/cli` or its GCS fallback, into
`~/tools/harness-versions/grok/<v>/grok`. No checksum is published; the binary must report the
version. 1.0.50 is byte-identical to the copy the official installer put in `~/.grok/downloads/`.
Latest stable on 2026-10-11 was 1.0.50 (ember's version); `https://x.ai/cli/alpha` named 1.0.51.

## The model path

Grok takes custom OpenAI-compatible models as `[model.<name>]` entries (`base_url`, `env_key`).
The generator defines `corpus-glm` (`glm-5.3-flash`) and `corpus-glm-alt` (`glm-5.3`, the model
switch) at `http://127.0.0.1:<free port>` with a dummy key; `../key_proxy.py` holds the Z.ai key
and forwards to `https://api.z.ai/api/coding/paas/v4`. `[models] session_summary` points titles
at the same model. Grok's own `grok-build` models and their Responses-API path (encrypted
reasoning) are not exercised; `reasoning` records carry `summary` text only.

Every run is `env -i` with `HOME`, `GROK_HOME` and `TMPDIR` in the throwaway dir and
`--always-approve`. Per-turn summaries, session recaps and title refreshes cost a model call each,
so `GROK_TURN_SUMMARY`/`GROK_SESSION_RECAP`/`GROK_TITLE_REFRESH` are on for the chat session only.

## Scenarios

`AFFORDANCES.md` lists every affordance, the record it leaves, and whether the corpus has it.
`--only "<names>"`: `chat tools image todo plan agents background schedule mcp skill workflow
interrupt`. A full run made 71 model calls (budget `MAX_CALLS`, default 85) in about 5 minutes.

`--tools` takes internal ids (`run_terminal_cmd`, not the `run_terminal_command` the model sees),
and any allowlist also drops `spawn_subagent`, the background/scheduler tools and `workflow`, so
those scenarios run with the default tool set. A project workflow needs folder trust, so the model
falls back to running the script inline: the corpus has an inline-script run, not a named one.

## What is copied

`sessions/` as Grok wrote it, minus lock files and the FTS index `session_search.sqlite` (cv does
not read it). `events.jsonl` (telemetry, up to 200 KB a session) and `tool_definitions.json` (40 KB
of tool schemas) are kept for the chat session only. Not copied: config, logs, `memtrace/`, the docs
Grok unpacks into `GROK_HOME`. The bundled workflows' build-machine path
(`/Users/admin/buildkite/...`) and the `/Users/me/...` examples in the image tools' descriptions
are rewritten to `/build/buildkite/` and `/home/me/`, since the corpus never holds a `/Users/` path.
