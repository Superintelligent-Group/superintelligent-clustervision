# PLAN-0.18 — ClusterVision (read at origin/main e9db5a1d = v0.17.0, 2026-10-18)

Product lines at the tag: cv 18,483 / cv-core 78,590 / cv-search 4,901 (101,974); tests 87.1K.
Wave recovery in 0.17: deletions −586/1.5K (39%), identity −204/2.5K (8%), bytes −718/3.3K (22%),
doors −217/2.5K (9%). Target-design steps 5-8 (lanes D/E/F) never ran. The Span call (lazy.rs)
is open (see wave notes referenced in `cv-0-18-plan` memory and §d1).

## (a) Theme: the release an agent feels, and can trust

0.17 made cv stream and tell the truth about freshness; 0.18 makes cv trustworthy at its edges,
where an agent meets it daily. Four weights, in order. **Power:** hooks for Codex, Gemini and
Qwen (verified shell-hook harnesses with Claude-shaped stdin JSON carrying `transcript_path`:
codex `codex-rs/core/src/hook_runtime.rs`, gemini `packages/core/src/hooks/types.ts`), and a
drift sensor that uses the stores themselves as the instrument (Claude records carry `version`,
Codex `cli_version`; the corpus pins 27 writer versions). **Performance the agent feels:** a
first `cv ls` in a fresh home or sandbox takes minutes (2.36 T instructions, PERF.md "0.17
facts"); listing without counting fixes the one budget that is felt, and it is the same work as
target step 5 (discovery as a codec demand). **Maturity:** a stated stability promise checked by
a schema diff, macOS exercised in CI (ci.yml is ubuntu-only), the no-laptop-build operating model
written into tools/, and the upgrade path verified by machine. **Parsimony continues only where
deletion is literal:** prune's owned/view twins, the second JSONL reader, the heap-gate scenario
paths, the anthropic write twin. Budgets known to be false (300 ms first ls with exact counts,
60 s full recount of 89 GB) get restated so the instrument stops lying. Less code is a result
here, not the goal.

## (b) Candidate items

| # | item | why a user/agent cares | evidence | size (rule §f) | risk | depends on |
|---|---|---|---|---|---|---|
| C1 | first-run `ls` lists without parsing; `message_count: 0` = not counted; exact counts from `cv index`/first full read | minutes → seconds in every fresh home, sandbox, MCP call | PERF.md 0.17 facts (2.36 T); catalog.rs defines "not counted"; DATAPATHS.md: exact first-run counts cannot be cheap | discovery demand per adapter ~+200/−400; shares step 5 | product change (E2) | J |
| C2 | `has:` 0.24-0.3 s: a stat per session + tool-results dirs ≈ 2.8 G insn | felt on every `ls -q has:` in MCP | PERF.md has:plan rows | ~100 (trust `watched` identities refreshed by the sweep) | low | measure d4 |
| C3 | `cv index` unchanged live 1.0-1.6 s (≤1 s), append 0.70 s (≤0.53 s): fixed cost | hook-driven indexing on every stop | PERF.md table | unknown until d4 | medium | measure d4 |
| C4 | fleet-scale check that pass 2 after `cv index` reads nothing (the 4,893 reread) | the 0.17 fix (164d3b95) was proven at 76 rollouts only | PERF.md; codex-baseline notes | measurement, 0 lines | — | ember runs `cv index` once |
| H1 | `cv hooks install codex\|gemini\|qwen`: event-name table + settings path + payload mapping; the five hooks ported | the five loops exist only for Claude Code (hooks.rs `require_claude`) | `~/.codex/hooks.json`; Codex events PreToolUse/Stop/SessionStart/UserPromptSubmit/PostToolUse/SubagentStop/PreCompact/SessionEnd; Gemini BeforeTool/AfterTool/SessionStart/SessionEnd/BeforeAgent/AfterAgent/BeforeModel/AfterModel/PreCompress/Notification | +250..400 in cmd/hooks.rs + corpus `hooks` scenarios | medium: each contract verified live on persvati through the corpus key proxy | d6 |
| H2 | drift sensor: `cv doctor` row per harness "newest writer version in your fleet vs newest corpus version cv is verified against"; weekly persvati cron running tools/harness-drift.sh --pull | silent format drift (three harnesses moved stores without error, 2026-09-19) | claude record `version`; codex `cli_version`; tests/corpus/*/MANIFEST.toml | +100..150 (catalog column `writer_version`, one bump) | low | I1 |
| A1 | stability promise: session row keys, message JSON, exit codes 0/1/2, routes, MCP names; deprecation = one minor of 410/exit-2 pointers; CI gate = schema diff vs the last tag | agents script against `--json`; no promise exists today | INTERFACE-V2 §3-4; `cv schema --commands --json` | +1 doc page, +60 tool | low | — |
| A2 | macOS CI job (macos-14): default-features nextest subset + mmap heap gates + sqlite lifecycle check; nightly and on `v*-rc*` tags | macOS code (APFS, ctime in FileId, F_FULLFSYNC publish, mmap windows) runs only at release; nothing compiles on the laptop | ci.yml all ubuntu | +60 yml | cost: macOS minutes 10× | — |
| A3 | operating model into tools/: remote-gate.sh, land-ci.sh (dispatch CI, 60 s poll, ff-only), `-rc` tag → artifact smoke | survives session loss; 0.17 ran these from a scratchpad | session scripts | +150 sh | low | — |
| A4 | upgrade-path gate: 0.16.1→HEAD over the 27 corpus versions, second pass zero changed, scripted on persvati | the Codex whole-tree review did it by hand once | review report ("284 session instances") | +80 sh/py | low | A3 |
| R1 | prune on one view path for every record size; `retrieve` out; rewind as selector; adopt = derive + meta | the most literal duplication left; prune 3,451, rewind 1,001 | TARGET §10: the five LARGE_RECORD twin pairs | −1,500..−2,000 (twin deletion ×1.0; derive +420) | medium: replay byte-identical + verify_claude_resumable + token_estimates fixtures | measure d8 |
| J1 | one JSONL read lifecycle + `Adapter::transcript`; delete the second reader in harness/mod.rs; discovery as a demand (Codex scan+Turns, Claude metadata, Grok, Kimi, KimiCode) | one checkpoint/recheck/rejected-line rule; the mechanism C1 needs | TARGET §4; CODEC.md | −140 (reader) + unify 6×~60 ×0.3 ≈ −250; discovery −300..−600 | medium | Z1 first on the branch |
| J2 | `wire/genai` for Qwen+Gemini parts; the anthropic write twin | two decoders and two writers of one grammar | TARGET §2 | −400..−600 | medium (Qwen has no local fleet) | d7 |
| S1 | SQLite lifecycle in store/sqlite + `Select`; heap-gate paths → indexed SQL one commit each (OpenClaw window groups, Hermes dedup → `a.content IS b.content`, Qwen TaskInventory, Goose fork inventory, export cache) | bespoke SQL walks nobody can review | TARGET §4; DECISIONS Q2 | lifecycle −300..−500; paths −700..−900 | medium: each re-proves its heap gate AND PERF time budget | — |
| Z1 | Span via checked range; `SourceText` serializes as the span object so `show --json` bytes hold; Resolver/CheckedSource/DemandReader/the unsafe map go | Q4 "every lazy payload checked" unfinished; 17 cfgs | lazy.rs serialization | −300..−450 honest (TARGET said 700) | medium-high: every Resolver consumer | d1 go/no-go |
| E1 | emitters as the codec reversed (Q1) | — | emit modules ≈ 8.3K raw; only the anthropic and genai twins are literal | not a lane; fold twins into J2 | — | — |
| X1 | ExactStrings (Q10): a bounded set over the trailing user run, labeled | 840 raw lines used once (hermes ReplayDedup) | spill/exact.rs | −400 if d5 shows runs small | low | d5 |
| L1 | leftovers: emitter `fallback_title` → `Message::prompt_title`; Cursor `<timestamp>` title wrapper; Qwen per-project schedule staleness; AFFORDANCES.md leaves | correctness of titles and `has:` rows | CHANGELOG 0.17 / AFFORDANCES.md | ±150 | low | — |
| K1 | HARNESS-ASKS: re-read Claude Code docs; if PreCompact `compactSummary` landed, `cv hook pre-compact` emits the distill span | the compaction summary an agent wakes to | docs/design/HARNESS-ASKS.md | +150 only if landed | low | 10-minute check |

## (c) Lanes, order, integration points

Lessons kept: overlapping lanes integrate once on a branch; independent lanes land serially via
dispatched CI (60 s poll); nothing compiles on the laptop (persvati; `-rc` tags for macOS
artifacts); budgets `--write` per commit and every touched module falls or the commit says why;
byte identity by hashed samples over an APFS clone; lanes capped at ~250K tokens with a
continuation file at 200K; delete-first inside a lane.

0. **Lane M — measure first** (persvati + one quiet read-only laptop session): the §d list. Outputs
   become go/no-go marks here; no code.
1. **Lane 0 — root, serial, small:** A3 (scripts into tools/), A2 (macOS nightly job), A1 (promise
   page + schema-diff gate), PERF budgets restated (§e3). Land each on CI.
2. **Lane H — hooks + drift (independent, serial):** H1, H2, K1. **I1:** the ONE 0.18 catalog/stamp
   bump is a root commit on main before H and J start; both rebase on it.
3. **Lane A — leftovers L1, X1** (independent, serial, before the integration branch opens).
4. **Lane R — reshape R1** (independent of the adapters' readers, serial).
5. **Integration branch `v18-integration`** (overlapping lanes, one CI, one fast-forward):
   Z1 first (if d1 says go); J (J1 + J2 + C1), Grok first, then Claude, Codex, Kimi, KimiCode, the
   Qwen reader, C1 as J's product goal; S (S1) in parallel with J, qwen split (J the reader, S the
   inventory). **I2:** harness/mod.rs trait changes (`transcript`, `dependencies`) are a root commit
   at branch open. **I3:** C2/C3 land after J, since `has:` freshness changes with discovery.
6. **Release polish:** Codex peer review on persvati, A4 upgrade gate, `-rc` tag, macOS artifact
   smoke read-only, 0.18.0.

## (d) Measure before committing to the big items

1. **Span prize (Z1):** lines of lazy.rs reachable only via Resolver/CheckedSource/DemandReader
   (stub Resolver on persvati, `-W dead_code`). Go if ≥ 400; else Z1 waits for the Goose-era rewrite.
2. **Which budget an agent feels:** instruction counts (release build, load < 12, the qual-home on
   persvati) of the hook commands that run on every prompt, `ls -q has:plan`, `search` warm, `ls`
   first run. Rank by count × frequency.
3. **C4:** after ember's `cv index`, two `cv index --json` passes on the real home (read-only, quiet
   laptop): changed must be 0 both times. If not, that is 0.18's first fix.
4. **Index fixed cost (C2/C3):** instructions by phase (catalog open, baseline decode, tantivy open,
   sweep) on the qual-home → decides whether C3 is a lane.
5. **ExactStrings:** max bytes of a trailing user single-text run over corpus Hermes + the real
   Hermes sessions (read-only) → X1 go/no-go.
6. **Hook contracts:** read codex hook_runtime.rs and gemini hookRunner.ts for output handling
   (`additionalContext`, decisions, async) → H1 size and which of the five hooks each can host.
7. **Emitter twins:** literal-copy lines between the anthropic block writer twins and qwen vs
   gemini emit → confirms E1 is not a lane.
8. **Prune twins:** count the owned halves of the five LARGE_RECORD pairs → R1's estimate from the
   actual twin size.

## (e) Decisions only ember can make (recommendation in bold)

1. Theme order. **Maturity + power first half (Lanes 0, H, A), parsimony second half (R, then the
   integration branch); Z1 only if d1 ≥ 400.**
2. First `ls` shows uncounted rows and leaves exact counts to `cv index`. **Yes; restate the 300 ms
   budget as "first `ls` ≤ 2 s per 10K sessions without counts".**
3. Restate the known-false budgets (full recount from bytes/(cores × 400 MB/s); the counting pass
   at the measured ~700 MB/s floor, labeled). **Yes: a budget nobody can meet is not an instrument.**
4. Hooks for Codex, Gemini, Qwen in 0.18; OpenClaw/Hermes in-process plugin hooks deferred.
   **Yes; each verified live on persvati via the corpus key proxy.**
5. Drift sensor as a `cv doctor` row plus a weekly persvati cron. **Yes; one catalog column, riding
   the single 0.18 bump (I1).**
6. macOS CI nightly + on `-rc` tags vs per push. **Nightly + rc tags (cost).**
7. Stability promise scope. **Promise only what a test checks: session row keys, message JSON, exit
   codes, routes, MCP names; one minor of pointers before removal; schema-diff gate.**
8. Q1 emitters-as-codec. **No lane; fold the two literal twins into J2.**
9. Q10 ExactStrings. **Replace after d5 unless real runs exceed 1 MiB.**
10. Q8 snapshots reaper. **Keep; the immutable-copy prototype was never measured.**
11. Q4 Span: approve "`SourceText` serializes as the span object" so byte identity holds.
    **Approve; go only with d1.**
12. Lane discipline: 0.17 lanes ran past 500K tokens and died before deleting. **Cap 250K,
    continuation at 200K, ≤ 3 overlapping lanes at once, delete-first ordering.**

## (f) What 0.17's estimates got wrong, and the 0.18 estimation rule

- Moves were counted as savings (`Probe::dependency_entries` → `Adapter::dependencies`: −190
  estimated, ≈ 0 real).
- "Unify N shapes into one" was costed as (N−1)×size, but the survivor absorbs every variant's
  cfg, `Result` plumbing and quirk (identity: 10 shapes → `FileId`, −204 of 2.5K).
- Deletions that need an output change (char-budgeted heads, Span serialization) were counted as
  byte-identical; they are product decisions first.
- Lanes hit context ceilings mid-port: new code landed, old paths stayed (Resolver untouched).
- Bug fixes and hardening rode each step (+0.1K each) and were not netted.

**Rule for 0.18:** estimate = lines that will be deleted whose replacement already exists or is a
literal twin ×1.0; "unify N" ×0.3; "move" ×0; every commit message states estimate vs actual, so
the ratio is visible by the second week and lanes can be stopped on evidence, not hope.
