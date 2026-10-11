# Gemini CLI corpus generator

```sh
tools/harness-corpus/gemini/install.sh 0.63.0
tools/harness-corpus/gemini/generate.sh 0.63.0 crates/cv-core/tests/corpus/gemini/0.63.0
CV_REGEN_CORPUS=1 cargo test -p clustervision-core --test corpus
```

**Needs:** npm, python3, git, and `~/.zai-key` (a Z.ai coding-plan key; override with `ZAI_KEY_FILE`).

`install.sh` installs into `~/tools/harness-versions/gemini/<v>/` with `npm --prefix`. The Homebrew
`gemini` and `~/.gemini` are never read or written: `HOME` and `GEMINI_CLI_HOME` point into the
throwaway dir.

## The model path

Google's free OAuth tier refuses Gemini CLI since 2026 (`IneligibleTierError: UNSUPPORTED_CLIENT`,
"This client is no longer supported for Gemini Code Assist for individuals", seen on 0.63.0 with ember's
OAuth file on 2026-10-11; the copy was deleted at once). Gemini CLI has no OpenAI-compatible provider,
but every version since 0.46 honours `GOOGLE_GEMINI_BASE_URL` with API-key auth. So
`generate_content_shim.py` is a small local Gemini API (`generateContent`, `streamGenerateContent`,
`countTokens` answered locally) that forwards each request to Z.ai's chat completions
(`glm-5.3-flash`) and answers in Gemini's shape: `thought` parts from `reasoning_content`, `text`,
`functionCall`, and `usageMetadata`. Only the shim holds the key; Gemini CLI gets a dummy API key.

Gemini CLI is told `model.name = gemini-2.5-flash` (so its model config enables thoughts); 0.46.0
records its own alias (`gemini-3.5-flash`) for that name, 0.63.0 records it as given. Sub-agents run
on `gemini-2.5-pro` (their own config). Every answer is GLM's; `MANIFEST.toml` says so.

## Scenarios

Each is one or more `gemini -p` runs (`-y` where tools must run unattended, `--session-id` then
`-r <id>` for resumes); see `AFFORDANCES.md` for what each leaves on disk.

| scenario | how |
|---|---|
| chat, resume | three turns on one session id |
| tools, parallel, tool error, thoughts | read two files at once, `replace` then `cat`, `cat` a missing file |
| background shell | `run_shell_command` with `is_background` |
| image | `@dot.png` in the prompt (inlined as `inlineData`) |
| write_todos, task tracker | `write_todos`; `tracker_*` with a workspace `experimental.taskTracker` |
| plan mode | `--approval-mode plan`, `exit_plan_mode` |
| MCP | `../codex/mcp_server.py` (one `add` tool) in `mcpServers` |
| sub-agent | `invoke_agent` → `codebase_investigator`, its own recording |
| skill, custom command, hook | `~/.gemini/skills/corpus-greeting`, `~/.gemini/commands/corpus-hello.toml`, a BeforeAgent hook |
| web | `web_fetch`, `google_web_search` (answered by GLM through the shim) |
| model switch | resume with `-m gemini-2.5-flash-lite` |
| compression | resume under a workspace `model.compressionThreshold` of 0.0001 (`/compress` is interactive-only) |
| `/chat save` | writes `checkpoint-corpus.json`, then exits 42 (its overwrite prompt is unsupported headless) |

Not exercisable headless: `/rewind` (`$rewindTo`), `ask_user`, `info`/`error`/`warning` records (the
interactive UI writes them), restore checkpoints (`general.checkpointing` snapshots only in the TUI).

## What is copied

`tmp/` (recordings, `checkpoint-*.json`, `logs.json`, plans, tracker, background-process logs),
`projects.json`, and `history/*/.project_root`. Not settings, the shadow git repos under `history/`,
installation ids, or the hook/skill/command inputs.

## Budget

The shim refuses calls past `MAX_CALLS` (default 70) and logs each to `$WORK/calls.jsonl` (part
kinds and tool names only). A full run is 50 to 60 calls; the count is in `MANIFEST.toml`. Each run
is cut at `RUN_TIMEOUT` (300 s).
