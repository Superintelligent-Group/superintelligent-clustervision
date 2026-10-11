# Cline corpus generator

```sh
tools/harness-corpus/cline/install.sh 3.0.70
tools/harness-corpus/cline/generate.sh 3.0.70 crates/cv-core/tests/corpus/cline/3.0.70
CV_REGEN_CORPUS=1 cargo test -p clustervision-core --test corpus
```

**Needs:** npm, python3, git, sqlite3, `~/.zai-key`.

**The writer.** The npm `cline` package (3.x) is the Cline CLI built on the Cline SDK
(`@cline/core`). Cline's 4.x VS Code extension is built on the same SDK and writes the same store,
`~/.cline/data/` (`sessions/<sid>/{<sid>.json,<sid>.messages.json,...}` plus `db/sessions.db`). The
3.x extension's globalStorage `tasks/` layout is the older format; cv's existing fixtures cover it.
ember has no Cline installed, so 3.0.70 (2026-10-08) is the only version.

`CLINE_SESSION_BACKEND_MODE=local` keeps the CLI in-process. Otherwise it starts a background "hub"
daemon that outlives the run. `CLINE_NO_AUTO_UPDATE=1` stops self-updates.

**The model path.** `cline auth -p openai-compatible -b <key_proxy URL> -k <dummy>` writes the
provider into the throwaway `settings/providers.json`. The proxy adds the Z.ai key.

## Scenarios

Every scenario is one `cline --json "<prompt>"`. `--id` (resume) always opens the TUI, which needs a
TTY. Under a pty the TUI neither sent the prompt nor kept to the local backend.

| scenario | how |
|---|---|
| chat, reasoning | one prompt with `--thinking low` |
| read/edit/shell, parallel, tool error | one prompt: `read_files` twice in parallel, `editor`, `run_commands` with a missing file |
| sub-agent | `spawn_agent`: the child's `agent_<uuid>.messages.json` in the lead's dir |
| team | `team_spawn_teammate`, `team_run_task`, `team_await_runs`, `team_cleanup`: a teammate file and `db/teams.db` |
| plan mode | `-p` (the editor tool is withdrawn) |
| MCP | `corpus__add` from `settings/cline_mcp_settings.json` |
| web fetch | `fetch_web_content` |
| ask question | `ask_question` (non-interactive: Cline answers with the first option) |

**Not covered:** multi-turn chat and resume (TUI only, see above), image input (no attachment flag),
compaction (`/compact` is a TUI command; a prompt of `/compact` is rejected; `--acp` mode requires a
Cline account sign-in), checkpoints, `/undo` and `/fork` (TUI only), and schedules (the
`schedules` table exists; making one needs the TUI or the kanban app).

**What is copied:** `sessions/`, and `db/sessions.db` and `db/teams.db` (backup API, then
`journal_mode=DELETE`). Not copied: `settings/` (provider config), `logs/`, `cache/`, `locks/`, and
the empty `connectors`, `cron` and `tasks` databases. The corpus is about 350 KB, over the 300 KB
target: every `<sid>.messages.json` carries the full system prompt.
