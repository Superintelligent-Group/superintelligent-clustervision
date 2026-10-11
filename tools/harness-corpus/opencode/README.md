# OpenCode corpus generator

```sh
tools/harness-corpus/opencode/install.sh 1.18.20        # opencode-ai on npm
tools/harness-corpus/opencode/install.sh 2.0.25         # @opencode/cli on npm (2.x is not a GitHub release)
tools/harness-corpus/opencode/generate.sh 1.18.20 crates/cv-core/tests/corpus/opencode/1.18.20
tools/harness-corpus/opencode/generate.sh 2.0.25 crates/cv-core/tests/corpus/opencode/2.0.25
tools/harness-corpus/opencode/migrate.sh crates/cv-core/tests/corpus/opencode/1.18.20 2.0.25 \
    crates/cv-core/tests/corpus/opencode/1.18.20-migrated-2.0.25
CV_REGEN_CORPUS=1 cargo test -p clustervision-core --test corpus
```

**Needs:** npm, python3, sqlite3, curl, git, and `~/.zai-key` (override with `ZAI_KEY_FILE`).
`~/.opencode`, the Homebrew install and `~/.local/share/opencode` are never read or written; even
`--version` writes a log under the data dir, so `install.sh` probes in a throwaway home too.

## The model path

OpenCode speaks chat completions, so `chat_proxy.py` only holds the key: it forwards each request to
Z.ai's coding endpoint (`glm-5.3-flash`, `thinking` on), counts it, and streams the answer back. The
config names three models at the proxy: the real one, `glm-alt` (the same model under a second id, for
the model-switch scenario) and `corpus-error` (answered by the proxy with a 400, for the
provider-error scenario, never sent upstream or counted).

## How it drives OpenCode

One `opencode serve` per run, in the throwaway home, and every prompt is `opencode run --attach`
(1.x) or `run --server` (2.x) against it. The server also takes the affordances `run` has no flag for,
through its HTTP API: compaction, a user shell command, revert, interrupt. A server that outlives each
`run` is what lets 2.x background work finish and report back. 2.x requires a server password; the
generator sets a fixed loopback-only one.

Every new session gets `--title`, or OpenCode spends a model call generating one.

## Scenarios

See `AFFORDANCES.md` for the full list of what OpenCode persists, what is exercised, and why the rest
is not. `MANIFEST.toml` in each corpus dir lists the scenarios that run actually covered.

## What is copied

`opencode.db` only, through the sqlite backup API (the WAL folded in), by `thin.py`, which also
empties two tables cv never reads: `kv` entries `models-dev:*` (the ~6 MB models.dev catalog 2.x caches)
and `event` (the sync log, a replay of the same updates). Their schemas stay. Not copied: config, auth,
logs, the `snapshot/` git store, 2.x's `shell/` output files, caches.

## The 1.x → 2.x migration

2.x migrates a 1.x database in place the first time its server starts. `migrate.sh` copies a committed
1.x corpus database into a throwaway data dir, starts the 2.x server over it, waits for
`GET /api/experimental/migration/v1` to report `completed`, and commits what it left (both table sets).
No model calls.

## Budget

`MAX_CALLS` (default 60) caps the proxy. A full run takes about 40 to 45 calls.
