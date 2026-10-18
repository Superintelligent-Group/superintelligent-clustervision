# PLAN-0.18 — ClusterVision (read at origin/main e9db5a1d = v0.17.0, 2026-10-18)

The release starts from 0.17 and includes its unfinished reader, reshape and store work.
The decisions in §e govern the whole plan. Measured sizes and timings explain engineering
choices; they are not quotas. Implementation and test evidence belongs in commits, with live
coordination state outside the product tree.

## (a) Theme: the release an agent feels, and can trust

0.18 makes cv trustworthy where an agent meets it daily. **Power:** exercise the five hook loops
on real Codex, and expose observed writer-version coverage in `cv doctor`. **Performance:** make
first discovery useful without counting every transcript, with counts supplied by indexing or a
full read. **Maturity:** check the stability promise, run macOS CI, encode safe remote operation,
and verify upgrades from the actual released artifact. **Simplicity:** share the native reshape
writer, JSONL lifecycle, wire grammars and SQLite selections where their semantics agree.

Nothing compiles on the laptop. Builds, tests, packaging, manual and heap checks run on persvati;
macOS checks run in CI. All stores used by checks are isolated scratch copies or explicitly
authorized frozen sources, never writable real harness homes.

## (b) Release work

| # | Required result | Evidence / boundary |
|---|---|---|
| C1 | First listing discovers without exact counting; `message_count: 0` means uncounted; index/full reads publish exact counts | Common codec demands; source-validated count reuse; measure cold and warm paths separately. Optional metadata fidelity is an explicit design decision, below |
| C2 | Reduce repeated stat inventory for `has:` where the sweep already captured the same source identity | Preserve dependency invalidation; measure the Claude tool-results inventory after J |
| C3 | Investigate fixed indexing costs when measurements justify a change | No separate lane from the current small-fleet profile; retain fleet-scale validation |
| C4 | Two unchanged index passes perform no transcript reread at representative fleet scale, including compressed Codex | The original 4,893-session reread needs broader proof than the 76-rollout regression; no synthetic or small-fleet result stands in for real-fleet coverage |
| H1 | `cv hooks install codex` supports the five loops, native child lineage and code-mode edits | Real isolated Codex execution on persvati, failure/block semantics, bounded correlation and output hashes; Gemini/Qwen hooks excluded |
| H2 | One `cv doctor` row compares newest observed writer versions with verified corpus coverage, including unknowns | Observations ride existing reads; one catalog column/shared epoch; no dedicated scans, cron or network |
| A1 | Machine-checked stability promise for row/message JSON, CLI exits, HTTP routes and MCP names | Compare actual emitted schemas/defaults and surfaces with the stable release; explicit exceptions only; one minor of removal pointers |
| A2 | macOS nightly, rc and manual CI exercises filesystem/SQLite lifecycle and mapped heap paths | Actual hosted execution remains distinct from validating workflow syntax |
| A3 | Safe remote preflight and exact-candidate CI landing tools | Isolated worktrees/targets/homes; propagate failures; bind CI and fast-forward landing to captured candidate and main SHAs |
| A4 | Released 0.16.1 upgrades over every session-bearing corpus version; repeat index reports zero changed | Checksum-pinned published binary, upgraded versus clean candidate rows/events/show hashes; rerun on final release candidate |
| R1 | One checked prune view path; shared native `derive` writer for prune, rewind and adopt; separate retrieval | Preserve untouched bytes, sidecars, resumability, token estimates and public compatibility; selected large rewind payloads stay bounded |
| J1 | One JSONL read lifecycle and discovery as a codec demand for Grok, Claude, Codex, Kimi, Kimi Code and Qwen | Delete the second reader in harness/mod.rs; preserve malformed/partial-record, source-recheck and checkpoint rules; reuse existing source/dependency authorities |
| J2 | Share actual Qwen/Gemini parts grammar and the Anthropic writer mapping | Preserve native quirks and owned Span versus streamed resolved-text contracts; no general emitter framework |
| S1 | Shared SQLite lifecycle and indexed selections for OpenClaw window groups, Hermes generation dedup, Qwen task inventory, Goose fork inventory and export cache | Preserve read-only native stores, WAL/source guards, ordering and fallback schemas; re-prove each affected heap path and measure cost |
| Z1 | Deferred: broad Span/Resolver rewrite | Existing source-check and serialization contracts remain; measurement did not justify this rewrite in 0.18 |
| E1 | No separate emitters-as-codec lane | Literal wire twins belong to J2 |
| X1 | Retain exact bounded spill unless a simpler replacement serves all consumers | Hermes replay, whole-operation TokenFold and unmatched native event IDs have different lifetimes; rationale in §e |
| L1 | Shared meaningful titles, Cursor timestamp wrapper, Qwen project schedule freshness, evidenced AFFORDANCES leaves | Cover Kimi todo/legacy agents, Roo mixed returns and Claude slash skills; do not infer ownership or completion from unmarked prose |
| K1 | Check whether Claude offers writable pre-compaction summary output | Implement only a real upstream capability; documented contract check in HARNESS-ASKS.md |
| G1 | No command silently waits when used by an agent | One-shot non-TTY defaults, explicit/announced follow, daemon readiness, hook TTY exit2 and piped command contract |

### C1 metadata boundary

Count deferral is approved. It does not automatically authorize changing other JSON values.
Claude can record title overrides anywhere in a transcript and timestamps out of order; compressed
Codex needs EOF to establish its final activity timestamp. Bounded sampling cannot certify these
as final metadata. Native summary fields and unchanged validated catalog facts can be reused.
Never substitute filesystem mtime for native time or a copied parent's prompt for a child's title.
The choice between unknown optional metadata until a full read and an exact cold metadata scan
remains explicit; until resolved, keep exact fallback behavior while removing count-only work.

## (c) Lanes and integration

Astra owns implementation, review and integration with normal Sol collaboration peers, at most
three active lanes. Reuse the peer holding context; leave a continuation file when needed.
Each lane owns a worktree, remote target and scratch home, with sccache and scoped checks.
Root lands the reviewed branch through GitHub CI; no lane pushes main or tags.

1. Foundations: A3, A2, A1 and measured PERF documentation.
2. Hooks/coverage: H1, H2 and K1. **I1:** discovery interpretation and consumer source stamps
   share the one 0.18 epoch transition, invalidating old counts, events and search receipts.
3. Correctness: L1 and the X1 design review.
4. Reshape: R1 with real replay hashes and affected heap checks.
5. **`v18-integration`:** J1/C1 (Grok first, then the other JSONL adapters), J2 and S1 compose on
   one branch. Qwen's reader belongs to J, its task inventory to S. **I2:** the existing
   `Adapter::resume_source` owns primary paths and `dependencies` owns auxiliary inputs; add no
   duplicate `transcript` authority. Review the shared lifecycle before broad adapter ports.
   **I3:** finish C2/C3 after discovery changes, then C4 at representative scale.
6. Release: independent peer review, complete composed gates, final A4 run, rc artifacts,
   actual macOS smoke and 0.18.0 landing/release through root.

Regenerate the informational size snapshot when product code changes and explain meaningful
module growth. Hash selected real outputs instead of dumping transcripts. Run the narrowest
refuting tests during development, then the complete composed gate before release. Missing
prerequisites and unavailable platforms are gaps, never passes.

## (d) Measurement questions

- Which cold/warm operations actually dominate work? Distinguish discovery, count publication,
  unchanged indexing, appends and full recounts; record bytes/instructions and conditions.
- C4: on an authorized frozen representative fleet, index once and require zero changed/resumed
  work on both following passes. Verify no transcript reread, including compressed sessions.
- C2/C3: attribute catalog, dependency inventory, baseline decoding and Tantivy fixed costs.
- X1: examine every consumer's required exactness and lifetime, not only sampled Hermes runs.
- H1: verify actual Codex event names, payloads, output interpretation and all five loops.
- J2/R1: identify real duplicate grammar/writers, preserve their differing policies, and compare
  native outputs before removing the old implementation.

## (d′) Lane M results (2026-10-18, persvati, release build, load 4-10 on 24 cores)

| # | number | mark |
|---|---|---|
| d1 | stubbing Resolver leaves 230 dead lines in lazy.rs (393 gross incl. the Resolver family) | **Z1 NO-GO**: limited simplification; deferred |
| d2 | per prompt: `hook decisions` 12.6 M insn, `hook edit` 13.9 M, `hook start` 16.7 M, `hook compact` 50 M on a 96 MB transcript; `has:plan` warm 35 M, `search` warm 59 M, `ls` warm 20 M; **first `ls` 9.9 G / 1.35 s, first `has:plan` 40 G / 6.2 s, first `index` 64 G** (qual-home, 419 sessions) | On this sample, per-prompt work < 40 ms; **the first-run path motivates C1** |
| d4 | `index` unchanged = 86 M cv instructions (57 ms): alloc 19 %, dependency/path hashing 17 %, tantivy open 15 %, sqlite 13 %, FileId decode 13 % | **C3 not a lane**; PERF's live 1.0-1.6 s is fleet scale. C2's 2.8 G is the Claude tool-results stat inventory |
| d5 | max trailing user single-text run: corpus 8,204 B, real Hermes 758 B | Historical Hermes-only sample; not a bound on fleet-wide TokenFold or native-event correlation (see X1 review below) |
| d6 | Codex: 12 events, `~/.codex/hooks.json` (+ repo-local), Claude-shaped snake_case stdin with `transcript_path`, exit 2 = block, `async` handlers; Gemini: 11 events, `~/.gemini/settings.json`, any exit ∉ {0,1} = deny, sync only, **HTML-escapes `additionalContext`**; Qwen: no checkout | Historical contract survey; Codex alone is in scope and requires live verification |
| d7 | `block_to` vs `anthropic/write::block`: 29 LCS-equal lines (14 non-trivial); qwen vs gemini parts: 25 / 14 | **E1 not a lane**; share only actual common grammar |
| d8 | prune owned halves 603 raw, 474-561 cleanly deletable; view halves (918) stay; `retrieve` 163 is a move | Replace the twins; a retrieval move is not a deletion |

These measurements identify the first-run path as the priority on this sample, not every fleet.

## (e) ember's decisions (2026-10-18)

1. Theme order as in §a: maturity and power first, parsimony where deletion is literal.
2. No numerological targets. The §f weights, the per-10K-session budgets and the ratchet
   percentages are gone: we pursue excellence and speed, measure what we ship, and say what we
   measured. The size gate stays only as a drift alarm (a module growing is a question, not a
   failure); the heap gates stay because they catch regressions.
3. PERF.md states no targets. It keeps measured facts with their dates and conditions.
4. Hooks only where they can be tested for real: Codex (ember uses it; the corpus key proxy runs it
   on persvati). Gemini and Qwen hooks are out of 0.18.
5. The "drift sensor" is one `cv doctor` row: newest writer version seen in the stores vs the
   newest version the corpus has verified, so a harness changing its format is noticed on ember's
   machine instead of silently misread. No cron, no network.
6. Everything else: do our best. Span (Q4) stays a no-go on d1's count; emitters-as-codec is not
   a lane; ExactStrings goes if the replacement is simpler; the snapshot reaper stays.

### X1 review after live Codex hooks

Retain the shared exact spill set for 0.18. Hermes clears it at substantive assistant turns,
but TokenFold deduplicates across a whole stats operation, and native Codex edit correlation
must remember every unmatched call ID. Neither has a trailing-run bound. A capped or evicting
set would miscount; an unbounded set failed the event heap gate during H1. The existing
in-memory fast path plus exact disk spill serves all three, including giant checked text keys,
without a new dependency or another dedup implementation. Simplification remains worthwhile
only if it preserves those composed requirements; sampled Hermes runs alone do not justify
removing the spill path. The failure and corrected measurements are recorded with H1's commit.

## (f) Estimating

0.17 estimated savings it could not get: moves counted as deletions, "unify N" counted as
(N−1)×size, output-changing deletions treated as byte-identical, lanes dying at their context
ceiling before deleting. 0.18 does not estimate lines. A lane says what it deleted and what it
added after the fact, and a reader judges the change on its shape.
