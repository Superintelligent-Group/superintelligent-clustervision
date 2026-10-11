# The harness corpus generators

`crates/cv-core/tests/corpus/<harness>/<version>/` holds real sessions that one version of a harness
wrote itself, scrubbed and committed, with `summary.json` (what cv read out of them) and
`MANIFEST.toml` (how they were made). `crates/cv-core/tests/corpus.rs` re-reads every version on every
test run. A new version's corpus shows what changed; the old ones, never deleted or regenerated in
place, prove cv still reads them. The design is `CORPUS.md` in the review notes; the rules below are
the binding part.

## Per harness: `tools/harness-corpus/<harness>/`

- `install.sh <version>`: installs exactly that version under `~/tools/harness-versions/<harness>/<version>/`
  (npm `--prefix`, a venv, a release tarball, a `~/pug` build). Idempotent; prints the binary path.
  Never touches the harness ember has installed.
- `generate.sh <version> <out-dir>`: runs the scenarios in a throwaway home and writes the store to
  `<out-dir>` (normally `crates/cv-core/tests/corpus/<harness>/<version>`, which must not exist yet).
  `--keep` leaves the temp dir for inspection.
- `README.md`: what it needs, which scenarios it covers, which it cannot and why.

## Shared pieces: `lib.sh` and `scrub.py`

Every `generate.sh` sources `lib.sh`:

| function | what it does |
|---|---|
| `corpus_work` | sets `WORK` (fresh temp dir) and `CORPUS_HOME=$WORK/home`, with the project at `$CORPUS_HOME/project` |
| `corpus_guard VAR...` | exits unless each env var (`HOME`, `CODEX_HOME`, `TMPDIR`, ...) resolves inside `$WORK` |
| `corpus_load_key VAR FILE` | exports `VAR` from a key file without printing it, and records the file so the scrub can check that literal key |
| `corpus_scrub DIR` | runs `scrub.py` (below) |
| `corpus_size_check DIR` | warns over 300 KB and fails over 1 MB (file bytes, not `du` blocks) |
| `corpus_manifest DIR k=v...` | writes `MANIFEST.toml` |

`scrub.py` rewrites every file, including every TEXT/BLOB cell of every sqlite table (then VACUUM):
the throwaway home becomes `/home/corpus`, so the project becomes `/home/corpus/project`. The rest of
`$WORK` becomes `/home/corpus/.work`, `~/tools/harness-versions` becomes `/opt/harness-versions`, the rest of the real home
becomes `/home/corpus`, the real user name becomes `corpus`, and account ids and
emails become `corpus@example.invalid`. The same paths are also rewritten dash-encoded (Claude Code's
`-home-corpus-project` project dirs) and percent-encoded (Grok's `%2Fhome%2Fcorpus%2Fproject`), in
file and directory names too. It then **refuses**, naming each file but never the match, if
any of these is left anywhere:

- a loaded key's literal value
- a token shape (`sk-`, `ghp_`, `Bearer ...`, a JWT, a Z.ai key, ...)
- `/Users/`, the real user name, or the throwaway path

`corpus.rs` runs the same kind of check over every committed file.

### `drive.sh`, `key_proxy.py`, `schema_snapshot.py`

- `key_proxy.py` serves harnesses that already speak the provider's wire (chat completions, Anthropic
  messages) and need only the key kept out of them. It forwards to Z.ai and adds the key, enforces
  `MAX_CALLS`, and logs offered tool names, never content. `drive.sh` (sourced after `lib.sh`) starts
  it on a free port (`corpus_proxy UPSTREAM bearer|x-api-key [MODEL]`, giving `PROXY_URL`), counts
  calls (`corpus_calls`), sets up the shared scratch project (`corpus_project`), and writes manifest
  lists (`toml_list`). Used by `cline/`, `continue/` and `roo/`; `claude/` and `grok/` start it themselves.
- `schema_snapshot.py` records a store's shape (DDL, merged JSON key paths and types) without its
  content. It is for desktop apps that cannot be driven headless (`zed/`, `lmstudio/`). Their
  `snapshot.sh` writes `tests/corpus/<h>/<app version>/` (`SNAPSHOT.toml` and no sessions), and
  `--check` diffs a later store against that snapshot.
- Each harness dir also has an `AFFORDANCES.md`: every affordance, where the store records it, whether
  the corpus exercises it, and how cv's IR represents it.

## Rules that are easy to break

- **Throwaway dirs live in your lane's own directory.** Run generators with `TMPDIR` set to a directory
  only your lane uses, and never delete by glob in a shared one: `lib.sh` names every work dir
  `cv-corpus.XXXXXX`, so `rm -rf cv-corpus.*` in a shared TMPDIR deletes other lanes' runs.
- **Throwaway home always.** Set `HOME` and the harness's own variable (`CODEX_HOME`, `GOOSE_PATH_ROOT`,
  `HERMES_HOME`, ...) and `TMPDIR` into `$WORK`, and run `corpus_guard` on all of them. Harnesses
  record `$TMPDIR` too: Codex lists it as a sandbox writable root.
- **Keys stay out of the harness.** Load the key in the generator, hand it only to the process that
  calls the model (for Codex, the local `responses_shim.py`), and `unset` it before the harness
  starts. Never put a key in argv, a config file, or a log.
- **Budget**: at most about 40 model calls per (harness, version). The Codex shim enforces
  `MAX_CALLS` and records every call in `calls.jsonl`; `MANIFEST.toml` carries the count.
- **Copy what the harness wrote, not what you configured.** Copy rollouts, databases, and indexes.
  Never copy auth, config, logs, or caches. Copy a live sqlite database with the backup API, not
  `cp`: WAL mode leaves the data in the `-wal` file.
- **Never regenerate a committed version.** Add a new version dir. After adding one, run
  `CV_REGEN_CORPUS=1 cargo test -p clustervision-core --test corpus` to write its `summary.json`, and
  read the other versions' diffs: any change there is a parser change and needs explaining.

## Adding a harness

1. Write `install.sh`, `generate.sh` and a README under `tools/harness-corpus/<harness>/`, copying
   `codex/` as the pattern.
2. Generate one version, then read it with jq/sqlite before trusting cv's summary of it.
3. Add a row to `HARNESSES` in `crates/cv-core/tests/corpus.rs` that roots the adapter at a version
   dir (give the adapter a `with_home`/`with_root` constructor if it lacks one; never use process env).
4. Regenerate summaries, commit the corpus, summary, and manifest together.
