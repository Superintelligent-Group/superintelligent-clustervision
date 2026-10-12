# OpenClaw corpus generator

```sh
tools/harness-corpus/openclaw/install.sh 2026.10.1
TMPDIR=<your scratch dir> tools/harness-corpus/openclaw/generate.sh 2026.10.1 crates/cv-core/tests/corpus/openclaw/2026.10.1
CV_REGEN_CORPUS=1 cargo test -p clustervision-core --test corpus
```

**Needs:** npm, python3 (3.14 or later when the store has zstd cells: `scrub.py` decompresses them to
scrub inside), git, sqlite3, and `~/.zai-key` (override with `ZAI_KEY_FILE`). A free local port for
the gateway, `GW_PORT` (default 18947); the script refuses to start if it is taken. The proxy binds a
free port of its own.

`install.sh` installs into `~/tools/harness-versions/openclaw/<v>/` with `npm --prefix`. The Homebrew
`openclaw` and `~/.openclaw` are never read or written. `HOME`, `OPENCLAW_STATE_DIR`,
`OPENCLAW_CONFIG_PATH` and `TMPDIR` all point into the throwaway dir, the legacy `CLAWDBOT_*` /
`MOLTBOT_*` names are unset, and `corpus_guard` checks them.

## The model path and the gateway

OpenClaw speaks OpenAI chat completions natively (`api: "openai-completions"`), so `../key_proxy.py --translate chat` only
forwards: OpenClaw sends to `127.0.0.1:$PORT` with a dummy key, the proxy adds the Z.ai key and
streams the reply back unchanged. It maps the `developer` role to `system` (Z.ai rejects it: 2026.1
sends `developer` to reasoning models), drops OpenAI-only knobs, and answers both configured model
ids (`glm-5.3-flash` and `glm-5.3-flash-tiny`, the same model under OpenClaw's minimum 16k context
window) with `glm-5.3-flash`. It logs request shape only to `calls.jsonl` and refuses calls past
`MAX_CALLS` (default 60).

The scenarios run against a foreground `openclaw gateway run` in the throwaway home (never
`gateway install`, so no launchd service), because sub-agents, cron, compaction and reset are
gateway features. The gateway token is a fixed local string, not a secret.

## Version differences the script handles

- **Separate sessions.** 2026.10 binds `--session-id <id>` to its own key `agent:main:explicit:<id>`.
  2026.1 collapses every direct CLI turn onto `agent:main:main`, so a new session is a
  `sessions.reset` of main (what `/new` does), and resume of an earlier session is impossible there.
- **Compaction.** 2026.1's `sessions.compact` only truncates (keeps the last `maxLines` lines and
  archives the whole file as `<id>.jsonl.bak.<ts>`); the auto-compaction on the tiny-window model
  writes the `compaction` entries. 2026.10 runs a model compaction when `maxLines` is absent.
- **MCP.** `mcp.servers` does not exist in 2026.1 (an unknown key stops the gateway), so MCP runs only
  where `openclaw --help` lists `mcp`, through `../codex/mcp_server.py`.
- **cron run.** 2026.1 needs `--force` to run a job that is not due.

## What is copied

`agents/<id>/sessions/` (JSONL era: transcripts, `sessions.json`, `.bak` archives), and
`agents/<id>/agent/openclaw-agent.sqlite` (sqlite era, via the backup API) pruned to the tables cv
reads: `schema_meta`, `transcript_events`, `session_windows`, `session_nodes`. The full schema is
about 190 tables and 780 KB with no rows. Also the sub-agent and cron registries: `subagents/runs.json`
and `cron/` (JSONL era), `state/openclaw.sqlite` pruned to `subagent_runs`, `task_runs`, `cron_jobs`,
`cron_run_receipts` (sqlite era). Not copied: config, auth profiles, device identity, logs, caches,
the workspace, memory indexes, FTS tables.

`corpus_scrub` also rewrites the install prefix (`~/tools/harness-versions/openclaw/<v>`, which the
2026.1 `sessions.json` skills snapshot records) to `/opt/openclaw/<v>`.

## Scenarios

See `MANIFEST.toml` in each version dir for what ran and what was skipped, and `AFFORDANCES.md` for
the full list of what OpenClaw persists and how cv reads it. About 40 to 50 model calls per version:
OpenClaw runs post-turn maintenance calls (memory flush, context-engine bookkeeping) on top of the
scenario turns.
