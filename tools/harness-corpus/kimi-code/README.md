# Kimi Code corpus generator

```sh
tools/harness-corpus/kimi-code/install.sh 2.1.1
tools/harness-corpus/kimi-code/generate.sh 2.1.1 crates/cv-core/tests/corpus/kimi-code/2.1.1
# what a newer version does to an older store (needs the older run's --keep dir):
tools/harness-corpus/kimi-code/generate.sh 0.39.1 crates/cv-core/tests/corpus/kimi-code/0.39.1 --keep
tools/harness-corpus/kimi-code/migrate.sh 0.39.1 <kept dir> 2.1.1 crates/cv-core/tests/corpus/kimi-code/0.39.1-then-2.1.1
CV_REGEN_CORPUS=1 cargo test -p clustervision-core --test corpus
```

**Needs:** npm, python3, git, and `~/.kimikey` (a Kimi Code membership key, `sk-kimi-…`, for
`https://api.kimi.com/coding/v1`; override with `KIMI_KEY_FILE`). The key does not work on
`api.moonshot.ai`. `install.sh` uses `npm --prefix` into `~/tools/harness-versions/kimi-code/<v>/`;
the Homebrew `kimi-code` and `~/.kimi-code` are never touched.

## The model path

Kimi Code takes a whole provider from `KIMI_MODEL_NAME` / `KIMI_MODEL_API_KEY` /
`KIMI_MODEL_BASE_URL` and never writes it to config.toml. The base URL is `../key_proxy.py` (passthrough) on a
free local port with a dummy key; the proxy alone holds the real key, forwards bytes to the coding
endpoint, and stops at `MAX_CALLS` (default 60). So the model alias in the store is
`__kimi_env_model__`, Kimi Code's name for an env-defined model; `llm.request.model` says
`kimi-for-coding`.

## Scenarios

`AFFORDANCES.md` lists every affordance and what the corpus has of it. In short: one long session
(`-p`, then `-S <id> -p` for each scenario) carries chat, tools, parallel calls, a tool error, todo,
plan mode, a background shell, cron, goals, a skill, MCP, fetch, `Agent`, `AgentSwarm`, and an
automatic compaction (a 60k window for one turn: compaction fires at `used + 50k reserved >= window`).
A second, tiny session is driven through the TUI by `tui_driver.py` with `tui.json`: `/undo`, a
prompt interrupted with Esc while the model URL points at a closed port (so 2.x records its API
retries and no call is made), and `/clear` (which starts a third session). In 2.x that tiny session
is then `kimi fork`ed.

Prompt mode runs in the "auto" permission mode, so `AskUserQuestion` and approval prompts never
happen; `--plan` cannot be combined with `-p`, so plan mode goes through its tools.

## Size

Every agent's first step writes an `llm.tools_snapshot` record of ~70 KB (55 to 75 KB for a
sub-agent), the full tool schema. That is why the single-agent scenarios share one session, and
why a version still lands at 700 to 900 KB, over the 300 KB target and under the 1 MB cap.

## What is copied

`sessions/` without `logs/` and the `.index-dirty` / `.index-cache` bookkeeping, plus
`session_index.jsonl` and `workspaces.json`. Not config.toml, mcp.json, skills, `device_id`,
`cache/`, `user-history/` or logs.
