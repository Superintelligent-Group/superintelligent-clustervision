# Handoff: make every store fast and small (2026-10-12)

Status: a brief for the next agent, written by the agent that ran the perf-2 lane's integration.
Read `docs/design/PERF.md` (measured numbers, budgets) and `AGENTS.md` (repo rules) first. Delete
this file when its work list is done or superseded.

## The goal, in ember's words

"We can't be blowing up to GBs of heap. We need to find a way to have this being hella fast."
cv reads ember's real fleet: ~6,750 sessions, 97 GB of transcripts across 17 harnesses on a
12-core, 96 GB machine. The aim is the platonic ideal of cv for concrete efficiency: stream, never
materialize a session or a store you don't need whole; never re-read bytes already seen; use all
cores; keep heap bounded by work in flight, not by file size; every index or mark over a file
verifies itself on use, so staleness costs a re-read and never a wrong answer.

## Where it stands (main, 0.16.1 + perf-2)

Done by perf-2 (see PERF.md "After"): parallel index sweep (read on 12 cores, write on one);
verified resume marks for claude `.jsonl` and plain codex `.jsonl` (an append costs its own bytes);
one FTS meta doc per session + body chunks; memchr line splitting and `madvise(SEQUENTIAL)` in
codex; lean parses that skip records a lean reader never opens; a CI parse gate
(`crates/cv-core/tests/perf_budget.rs`, throughput as a ratio to a plain read of the same bytes).

Measured 2026-10-12 on the largest real session per harness (`/usr/bin/time -l`, release build;
"show" = `cv show <h:id> --kind affordances`, a full streaming pass; "doctor" = `cv doctor <h:id>`):

| harness | largest | show time | show peak RSS | doctor peak RSS |
|---|---|---|---|---|
| gemini | 299 MB | 1.14 s | **1.12 GB** | **1.12 GB** |
| codex | 675 MB | 0.94 s | 574 MB (mostly the file-backed mmap) | 129 MB |
| claude | 181 MB | 0.53 s | 158 MB | 25 MB |
| devin | 172 MB db | 0.28 s | 39 MB | 38 MB |
| all others | small on this machine | ≤ 0.06 s | ≤ 40 MB | ≤ 40 MB |

Budgets still missed: live full recount 61–68 s / 2.1–2.4 GB (budget 60 s / 2 GB; the excess is
non-claude/codex stores, with gemini the likely driver: 12 workers × a 1 GB tree); claude parse
400–620 MB/s (budget 1 GB/s/core); first-run `cv ls` 5.7–7.8 s (budget 300 ms, untouched); an
append 0.7 s (budget 0.5 s; the excess is discovery re-scanning grown files).

## Clearing up one confusion

cv already has borrowed parsing for **content**: `crates/cv-core/src/lazy.rs` and the claude and
codex adapters keep large text as `Text::Span` into the file (via `serde_json::value::RawValue`), so
a 700 MB tool output costs 16 bytes until a consumer resolves it (commits `051b77c`, `3eeb8de`).
What it does **not** have is typed **records**: claude still builds a whole `serde_json::Value` per
line (68% of its parse time) and then re-scans `message.content` with `RawValue` for spans. The
work below is typed, borrowed record shapes for the hot lines, in one pass, not borrowing from
scratch.

## Work list, in order

1. **Gemini: stream the document.** Its older sessions are one JSON document (`messages: [...]`),
   parsed into a `Value` tree about 4x the file. Deserialize with a serde visitor over the messages
   array (`Deserializer::deserialize_map` + a `SeqAccess` that emits one message at a time to the
   `MessageSink`), borrowing strings, spans for large content as claude does. Target: the 299 MB
   session under 50 MB peak, ≥ 500 MB/s. The jsonl-era Gemini format gets the resume marks.
2. **Claude: typed borrowed records.** `#[derive(Deserialize)] struct Line<'a> { #[serde(rename =
   "type")] kind: &'a str, uuid: Option<&'a str>, #[serde(borrow)] message: Option<Msg<'a>>, ... }`
   with `Cow<'a, str>` where escapes force an owned string and `&'a RawValue` for content (spans
   produced in the same pass). Keep the `Value` path only for record types the typed shape doesn't
   cover, counted in `extra["cv"]` so drift is visible. Target ≥ 1 GB/s on the largest sessions.
3. **Bound parallel memory by bytes, not sessions.** The sweep's 12 workers can each hold a big
   file's working set; admit work by estimated bytes in flight (file size or a per-harness factor)
   so peak RSS stays ≤ 2 GB regardless of which 12 sessions run together.
4. **Resume marks for every append-only store.** Extend `crates/cv-core/src/stream.rs` checkpoints
   and the index resume (`a93317a`) to kimi, kimi-code, qwen, grok, gemini (jsonl era), cline,
   continue, codex `.zst` rollouts and the 2025 single-JSON codex format (or document why one can't
   resume). Sub-agent `meta.json` changes must invalidate a resume.
5. **SQLite stores** (opencode, cursor, devin, hermes, goose, openclaw, zed): per-session prepared
   statements that stream rows; never load a table; `PRAGMA query_only=1`, a sane `mmap_size`;
   discovery cached by a key covering the db and its `-wal` (cursor/devin/opencode dominate warm
   discovery at 0.1–0.2 s each).
6. **First-run `cv ls`** (5.7–7.8 s): gemini and kimi-code discover serially; claude/codex scans
   read every head and tail. Parallelize discovery per harness and per file, read heads only, and
   persist the scan cache early.
7. **Scale fixtures.** Most stores are small on this machine, so their worst case is unmeasured.
   Add `tools/harness-corpus/scale.py` that builds a large synthetic session per store by
   replicating real records from `crates/cv-core/tests/corpus/<h>/<v>/` (ids rewritten, valid
   format) into a scratch dir, never committed, and measure every store at 100 MB and 1 GB.
8. **Make the budgets gates.** Extend `perf_budget.rs` with a peak-heap check (count allocations or
   cap with a counting global allocator in the test) per store on the scale fixtures, so "no GBs of
   heap" fails CI instead of being re-discovered by hand.

## How to measure without lying

- Never touch `~/.clustervision`: copy the catalog and index into a scratch dir and point
  `CLUSTERVISION_HOME` at the copy. Transcripts are read-only.
- For before/after comparisons on the real fleet, make a frozen APFS clone (`cp -c -R`) of the
  stores under test so live sessions don't move between runs; delete it after.
- `/usr/bin/time -l`, release build, 3 runs; report real, user+sys and peak RSS. For mmap-heavy
  harnesses also report `footprint` (`/usr/bin/footprint <pid>` or `vmmap --summary`), since RSS
  counts file-backed pages.
- Output must be byte-identical: hash every message of every session in lazy, bulk and full mode
  before and after (perf-2 did this for 924 claude and 4,225 codex sessions), run
  `cargo +1.99.0 test -p clustervision-core --test corpus`, and compare `cv show --json` on a sample.
- The machine is shared with other agents; interleave before/after runs and say the load.

## Gates before pushing (all of them; CI runs the same)

`cargo +1.99.0 fmt --all --check`; `cargo +1.99.0 clippy --workspace --all-targets -- -D warnings`;
`cargo +1.99.0 build --workspace --all-targets`; `cargo +1.99.0 build -p clustervision-core
--no-default-features`; `cargo +1.99.0 check -p clustervision-core --no-default-features --target
wasm32-unknown-unknown`; `cargo +1.99.0 test --workspace` (once; ~1 min); `cargo +1.99.0 package
--workspace --exclude cv-web --exclude cv-testkit`; `bash tools/check-manual-names.sh
target/debug/cv`; `python3 tools/harness-corpus/test_tools.py`.

## Decisions ember may want to make

- Whether a once-per-upgrade full-text rebuild is acceptable again if a schema change helps (perf-2
  already spent one in the next release).
- Whether the seek store comes back: perf-2 found it unjustified after the parse work (worst window
  1.0 s); the resume marks already hold a verified anchor if it's wanted.
- Whether to ask harness authors for in-stream skip/index records (see `HARNESS-ASKS.md`): the real
  fix for jsonl's lack of seekability is the writer's.
