# kimi-cli (legacy) corpus generator

```sh
tools/harness-corpus/kimi/install.sh 1.51.0
tools/harness-corpus/kimi/generate.sh 1.51.0 crates/cv-core/tests/corpus/kimi/1.51.0
CV_REGEN_CORPUS=1 cargo test -p clustervision-core --test corpus
```

**Needs:** uv, python3, git, and `~/.kimikey` (the Kimi Code membership key; see
`../kimi-code/README.md`). `install.sh` makes a uv venv at `~/tools/harness-versions/kimi/<v>/`; the
Homebrew `kimi-cli` and `~/.kimi` are never touched.

kimi-cli is archived. **1.52.0, the final release, cannot produce a store**: every entry point prints
a pointer to Kimi Code (and, with no arguments, runs its installer). 1.51.0 is the last version that
writes sessions, so the corpus is 1.48.0 (what ember had installed) and 1.51.0.

## The model path

A `config.toml` in the throwaway dir (outside `KIMI_SHARE_DIR`, so it is never copied) defines one
`kimi` provider at `passthrough_proxy.py` with a dummy key, `default_thinking = true`,
`telemetry = false`, and two hooks. MCP comes from `--mcp-config-file`, the user skill from
`$KIMI_SHARE_DIR/skills/greet/`.

## Scenarios

`AFFORDANCES.md` has the full list. Everything runs in print mode with `--afk` on one session,
resumed with `-S` for each scenario: chat, tools, parallel calls, a tool error, an image through
`ReadMediaFile`, `SetTodoList`, plan mode, a background shell, `/skill:greet`, MCP, fetch,
`AskUserQuestion` (auto-dismissed), `Agent`, one Ralph-loop iteration, `/compact` and `/clear`
(both rotate `context.jsonl` into `context_N.jsonl`). About 38 model calls.

## What is copied

`sessions/` (all segments, `wire.jsonl`, `state.json`, `subagents/`, `tasks/`, `notifications/`) and
`kimi.json`. Not config, skills, `device_id`, logs, or telemetry.
