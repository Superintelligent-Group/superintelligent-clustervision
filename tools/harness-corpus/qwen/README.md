# Qwen Code corpus generator

```sh
tools/harness-corpus/qwen/install.sh 0.25.1
tools/harness-corpus/qwen/generate.sh 0.25.1 crates/cv-core/tests/corpus/qwen/0.25.1
CV_REGEN_CORPUS=1 cargo test -p clustervision-core --test corpus
```

**Needs:** npm, python3, git, and `~/.zai-key` (override with `ZAI_KEY_FILE`). ember has no Qwen Code
install; only the latest release is captured.

`HOME` and `QWEN_HOME` point into the throwaway dir. Qwen Code speaks OpenAI chat completions natively, so
`../key_proxy.py --translate chat --thinking harness` translates nothing: it holds the key (Qwen Code gets `OPENAI_API_KEY=corpus-local-shim`
and `OPENAI_BASE_URL=http://127.0.0.1:<port>/v1`), rewrites `model` to `glm-5.3-flash`, counts calls
against `MAX_CALLS` (default 70) and streams the answer back byte for byte.

Background extras that each cost model calls are off: managed auto memory and dreaming, follow-up
suggestions (`settings.json`), and auto titles (`QWEN_DISABLE_AUTO_TITLE=1`, except the chat scenario).
`tools.todoWrite.enabled` turns `todo_write` on; the task scenario turns on `experimental.agentTeam`
(the `task_*` tools register only with it) in a workspace settings file.

## Scenarios

Positional prompts (`-y` where tools must run unattended, `--session-id` then `-r <id>` for resumes):
chat + resume, tools (parallel reads, `edit`, shell, a failing shell command), background shell, an
`@dot.png` image, `todo_write`, `task_create`/`task_list`, plan mode, MCP (`../codex/mcp_server.py`), a
sub-agent (`agent`), a skill, a custom command, a `UserPromptSubmit` hook, `save_memory`, `web_fetch`, a
model switch (`-m glm-4.7-flash`: the name recorded; the proxy still answers with glm-5.3-flash),
`/compress`, `--fork-session`, `cron_create`/`cron_list`, and `/goal`. Plan mode is cut at 90 s
(`exit_plan_mode` is not offered headless and the run waits). `AFFORDANCES.md` says what each left.

## What is copied

`projects/` (session JSONL, sub-agent transcripts and their `.meta.json`, per-project memory and meta)
and `tmp/` (background shell output, `scheduled_tasks.json`, the legacy gemini-cli shape if any). The
project dir name is the sanitized cwd, which spells the throwaway path with dashes: the generator renames
it to `-home-corpus-project` and rewrites that spelling inside the files before the scrub; the `tmp/` project
hash (sha256 of the cwd) becomes the placeholder's hash the same way (0.25.1's dir was renamed after the
fact, its one in-file mention rewritten). Not copied:
settings, installation ids, `usage_record.jsonl`, `usage/`, the extension store, `~/.qwen/memories/`.

## Size

0.25.1's store is about 460 KB: over the 300 KB target, under the 1 MB cap. A third of it is
`ui_telemetry` records (request and tool lifecycle events) and most of the rest is tool output; the
prompts are already one line each.
