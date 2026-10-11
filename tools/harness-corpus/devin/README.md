# Devin CLI corpus generator

```sh
tools/harness-corpus/devin/install.sh 3000.11.3
tools/harness-corpus/devin/generate.sh 3000.11.3 crates/cv-core/tests/corpus/devin/3000.11.3
CV_REGEN_CORPUS=1 cargo test -p clustervision-core --test corpus
```

**Needs:** npm, python3, git, sqlite3, and a signed-in Devin account (`devin auth login` done once
on this machine).

**The writer.** The Devin CLI is closed source; npm `devin` ships the native binary. `install.sh`
installs it under `~/tools/harness-versions/devin/<v>/` and never touches the copy ember runs
(`~/.local/share/devin/cli/_versions/`). `devin -p` (print mode) writes the same
`~/.local/share/devin/cli/sessions.db` the interactive CLI writes; `-r <id> -p` resumes a session.
ember's installed version, 3000.11.3, is also the latest release (2026-09-23), so it is the only
version.

**The model path, and why it is not Z.ai.** Devin sends every model call through Cognition's
backend under the account. It has no custom-endpoint or bring-your-own-key option. So the generator
copies **only** `credentials.toml` into the throwaway home, deletes the copy when it exits, and uses
`swe-2-medium`, which `devin models list` shows as Free for this account. Model calls cannot be
counted; the manifest records sessions and turns instead.

## Scenarios

| scenario | how |
|---|---|
| chat, resume | one session, three turns (`-p`, then `-r <id> -p` twice) |
| tools, parallel, tool error, image file, todo, MCP, web fetch | one resumed session of seven turns: `read` x2, `edit` + `exec`, a failing `exec`, `read` of dot.png, `todo_write`, `mcp_list_tools`/`mcp_call_tool`, `webfetch` |
| sub-agent | `run_subagent`. 3000.11.3 puts the child's chain in the parent's `message_nodes` forest with no `subagent_heads` row; cv now finds it through the result row's `subagent/chain_node_id` |
| plan mode | `-p "/plan ..."` (`sessions.agent_mode = plan`) |

The tool scenarios share one session for a reason. Every session row (`cogs_json`, about 27 KB) and
every turn's re-copied system prefix (about 20 KB) cost space. At 756 KB this corpus is over the
300 KB target, under the 1 MB cap.

**Not covered:** the Fusion sidekick (`subagent_heads` rows): it runs only under a lead+sidekick
pairing whose lead models are paid. Compaction: `-p /compact` writes `summaries/history_<hex>.md`
but no summary nodes before print mode exits, and the format of `DEVIN_COMPACTION_THRESHOLDS` is
undocumented (JSON and `apply=,hard=` were both ignored). Image attachments: print mode has no
attachment flag.

**What is copied:** `sessions.db` (backup API, then `journal_mode=DELETE`), plus `transcripts/` and
`summaries/` when present. Not copied: credentials, config, logs, `installation_id`,
`app_state.json`, locks, plugins.
