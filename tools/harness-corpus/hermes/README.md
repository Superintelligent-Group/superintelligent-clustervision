# Hermes corpus generator

```sh
tools/harness-corpus/hermes/install.sh 0.21.6
tools/harness-corpus/hermes/generate.sh 0.21.6 crates/cv-core/tests/corpus/hermes/0.21.6
CV_REGEN_CORPUS=1 cargo test -p clustervision-core --test corpus
```

**Needs:** a `~/pug/hermes-agent` checkout with the release tags (`HERMES_SRC` overrides), `uv`,
python3, git, sqlite3, and `~/.zai-key` (`ZAI_KEY_FILE` overrides).

## Install

Hermes's `setup.py` refuses to build a wheel outside Nix, and its PyPI releases stop at 0.19. So
`install.sh` uses the layout Hermes supports for a source checkout: `git archive v<version>` into
`~/tools/harness-versions/hermes/<v>/src`, then `uv sync --frozen --no-dev --extra mcp` into
`<v>/venv`. That is an editable install from the release's own `uv.lock`, plus the MCP client.
Without the `mcp` extra Hermes connects zero MCP servers and says so only in `agent.log`. The
release tags are `v2026.8.31` (Homebrew's version, which reports v0.21.0) and `v0.21.6` (the new
version scheme). 0.21.6 reports `vunknown` because it has no install stamp; the commit is in
`<v>/COMMIT` and in `MANIFEST.toml`. The Homebrew `hermes` and `~/.hermes` are never touched.

## Isolation

`HOME`, `HERMES_HOME` and `TMPDIR` all point into the throwaway dir, and `corpus_guard` checks every
one. `HERMES_HOME` is the critical one: when it is unset, Hermes does not refuse to run. It warns
and falls back to `~/.hermes`.

## The model path

Hermes speaks chat completions natively. `chat_shim.py` is a pass-through on `127.0.0.1:$PORT` that
puts the real Z.ai key on each request, so Hermes's `config.yaml` only ever holds the dummy key
`corpus-local-shim`. The shim also pins the model to `glm-5.3-flash` (one `--also` model,
`glm-4.5-air`, passes through for the model-switch scenario), turns GLM thinking on, and swaps
image parts for a text placeholder, because the model is text-only and Hermes still persists the
image. It counts calls, refuses any past `MAX_CALLS` (default 95), and logs request shapes only.

## Two drivers

- `hq`: `hermes chat -Q --yolo -q ...`. A one-shot turn submits its text literally, so slash
  commands never run in it. `--resume <id>` continues a session.
- `htty`: `drive_tty.py`, which runs the interactive CLI in a pseudo-terminal. Each line is sent as
  one bracketed paste, then Enter, once the screen has been quiet for 8 s. A paste matters on 0.21:
  typed keystrokes there go into the slash-completion menu, which eats spaces and Enter. This is
  the only way to reach `/model`, `/compress`, `/branch`, `/undo`, `/retry`, `/queue`, `/title`,
  `/goal`, `/loop`, `/heartbeat`, `/btw` and `/bg`, and background `delegate_task`, which 0.21.6
  refuses in one-shot mode. `--watch` stops the drive if a path in the throwaway dir disappears.

## Scenarios

What each one exercises and how it lands in `state.db` is in [AFFORDANCES.md](AFFORDANCES.md).
Making `/compress` actually compact took three settings. `compression.protect_first_n: 0` and
`protect_last_n: 2` are needed because the default 20-message tail leaves a short session nothing to
summarize. Three long model replies (essays of about 3,000 words each) are needed because Hermes
refuses a compaction whose summary is longer than what it replaces, and GLM's summaries run to about
3K to 5K tokens. 0.21's "lean" tail also keeps the last 10K tokens verbatim whatever
`protect_last_n` says, so the oldest essay is the only one that gets summarized. Tool output is
pruned before summarizing, so a big `read_file` result does not help. The line sent is `/compress
here 1`. 2026.8.31 was generated before the third essay was added, and compacted with two.
`approvals.destructive_slash_confirm: false` stops `/undo` from opening a confirm modal.
`telemetry.shared_metrics` is answered in the config, because 0.21's first-run consent prompt would
otherwise swallow the first typed line.

## What is copied

Only `state.db`, copied with the sqlite backup API (recent rows are in the WAL). Every
`profiles/*/state.db` is copied too, though this corpus has none. Not copied: `config.yaml`,
`.env`, auth, logs, caches, `cron/`, `memories/`, `skills/` or `sessions/`. cv reads none of them.

After `scrub.py`, the FTS5 search indexes (`messages_fts`, `messages_fts_trigram`) are emptied with
`'delete-all'`. They are derived from `messages`, and the trigram index alone was about 4x the
text it indexes, which pushed one version past the 1 MB cap. Their tables and schema stay, and
`INSERT INTO messages_fts(messages_fts) VALUES('rebuild')` restores them. Every message and
session row is exactly as Hermes wrote it.

## Budget

70 to 95 calls per version, above the usual 40 because the affordance list is long. The interactive
drives cost the most: each `/goal` turn adds a judge call, `/compress` adds a summary call, and
every new session adds a title call. `MANIFEST.toml` records the count.