# Performance budget (2026-10-12)

Where the time and bytes go per command, measured, and what each command may cost. The fleet:
ember's machine, 12 cores, 96 GB RAM, ~6,750 cataloged sessions; claude + codex transcripts are
5,195 files and 95 GB (codex 88 GB of it), the rest are sqlite stores. Every figure is
`/usr/bin/time -l`, release build, best of 3 unless noted, against a copy of the real catalog
and index (`CLUSTERVISION_HOME`), page cache warm except where it cannot be (95 GB).

## Today (0.16.1)

| command | real | RSS | where it goes (sampled with `sample`) |
|---|---|---|---|
| `cv index`, full counts recount | 329 s | 1.8 GB | one thread: 171 s user + 36 s sys, the other ~120 s waiting on 5.2 M mmap page faults. 98% under `stream_counted`, 95% of that in the codex adapter (88 of the 95 GB) |
| `cv index`, nothing changed | 0.66 s | 145 MB | discovery (0.4 s, see `ls --fresh`) + one stored-field scan of the tantivy index |
| `cv index` after a 1 MB append to each of 3 sessions (1.6 GB) | 26 s | 3.3 GB | every changed session is re-read whole and its FTS docs rewritten: 1.6 GB parsed and re-tokenized for 3 MB of news |
| `cv ls`, first run (empty home) | 5.7 s | 1.7–2.5 GB | discovery with a cold scan cache, 38 s CPU over 12 threads: claude and codex `scan` read every transcript's head/tail; gemini and kimi-code discover serially and are the critical path |
| `cv ls`, warm | 0.07 s | 32 MB | catalog read + freshness probe |
| `cv ls --fresh` (= one `cv watch` poll) | 0.41 s | 120 MB | `discover_all` with a warm scan cache: stat the fleet, cursor/devin/opencode sqlite reads |
| `cv ls -q has:plan` | 0.05 s | 24 MB | catalog counts; never parses when counts are fresh |
| `cv show <claude 139 MB> --last 20` | 0.36 s | 23 MB | one full lazy parse to count, then the window |
| `cv show <codex 708 MB, giant lines> --last 20` | 2.0 s | 690 MB (file-backed mmap) | same; the giant `function_call_output` lines are scanned three times |
| `cv show <codex 512 MB, 92k msgs> --last 20` | 1.24 s | 530 MB (mmap) | same |
| `cv search tantivy` | 0.27 s | 24 MB | tantivy query + live snippets re-read from the top hits |

Parse throughput of the counting pass (a lazy stream with every message counted, timed against a
`std::fs::read` of the same bytes in the same run, ~8 GB/s warm): claude 310–400 MB/s, codex
340–600 MB/s, i.e. 14–29x a plain read. Where it goes:

- **claude**: 68% building a `serde_json::Value` per record line (every string allocated and
  unescaped), then a second `RawValue` scan of `message.content` for the lazy spans.
- **codex**: by bytes (every 100th codex file by size, 43 files, 1.4 GB), `compacted` records
  are 43% (their `replacement_history` is the whole prior conversation; the parser reads only its
  length) and
  `item_completed` 28%. A giant `function_call_output` line is scanned three times (outer
  `RawValue`, inner struct, escape search).
- **mmap**: the span path maps each file and faults it in 16 KB pages, one fault per page.

## Budgets

| what | budget | gate |
|---|---|---|
| parse, counting pass | ≥ 1 GB/s per core on the largest real sessions; on the corpus, ≤ x90 a plain read | `tests/perf_budget.rs`, CI step |
| `cv index`, nothing changed | ≤ 1 s | manual |
| `cv index` after an append | O(new bytes): ≤ 0.5 s + 10 ms per new MB | manual |
| `cv index`, full recount | ≤ 60 s on 12 cores, peak RSS ≤ 2 GB | manual |
| `cv ls`, first run | ≤ 300 ms (stretch; discovery has to read every session head once) | manual |
| `cv ls`, warm / `has:` | ≤ 100 ms | manual |
| `cv show --last N` | ≤ the counting pass of that session | manual |

The gate compares throughput as a ratio to a `std::fs::read` of the same bytes in the same run,
so a slow CI box moves both sides and the budget measures cv, not the runner.

## After (branch perf-2, 2026-10-12)

Same method; "frozen" is an APFS-clone copy of the claude+codex fleet (95 GB) so runs compare
byte-for-byte. The machine carried load 20–80 from other work; before/after runs were interleaved.

| command | before | after | budget |
|---|---|---|---|
| counts recount, live fleet | 329 s, 1.6 GB footprint | 61 / 64 / 68 s, 2.1–2.4 GB | missed (60 s, 2 GB) |
| counts recount, frozen | 335 / 379 / 355 s | 41–44 s, 140 MB | met |
| full rebuild, frozen | 499 s | 225 s | — |
| full rebuild, live fleet (the one-time schema rebuild) | — | 275 s, 3.2 GB footprint | — |
| append ~1 MB to 3 sessions (1.6 GB) | 23–36 s, 2.3 GB | 0.69–0.72 s, 50–70 MB | missed by ~0.2 s (discovery) |
| `cv index`, nothing changed | 0.66 s | 0.03–0.05 s scratch fleet; ~0.9 s live | met |
| `cv show <codex 708 MB> --last 20` | 2.0 s | 1.0 s | met |
| `cv show <claude 139 MB> --last 20` | 0.37 s | 0.28–0.34 s | met |
| `cv ls -q has:plan`, `cv ls` warm | 0.05–0.08 s | same | met |
| `cv ls`, first run | 5.7 s | 7.5–7.8 s (load) | missed (not worked on) |

Counting pass, largest sessions: codex 0.66–0.93 s per 700 MB (0.76–1.08 GB/s), 250 MB at
1.9 GB/s; claude 0.26–0.39 s per 140–190 MB (400–620 MB/s). Every claude and codex session of the
frozen fleet streams identically (lazy, bulk, full: every message hashed) and the catalog
(counts, events, sync rows) is byte-identical before and after. Search returns the same matching
sessions (one of 2,728 differs for "page fault"); top-40 order moved (meta docs now count in
BM25's averages, chunks end at message boundaries).

What did it: parallel sweep (read on 12 cores, write on one); lean parses that skip what lean
readers never open (codex `compacted` histories, claude `toolUseResult`/`attachment`); memchr line
splitting; `madvise(SEQUENTIAL)`; checkpoints + verified resume marks; one meta doc per session.

Not done, and why:
- **Seek anchors for `show --range/--last`**: not justified. After the parse work the worst real
  session is 1.0 s and only three sessions exceed 0.5 s. If wanted, the resume marks are the
  anchor: verified offset, message count, parser version.
- **corpus throughput**: the committed corpus is small records (~1.3 KB/message) and runs at
  ~170 MB/s before and after; its cost is per-record `serde_json::Value` building. The next lever
  is typed, borrowed deserialization of the hot claude/codex record shapes.
- **first-run `cv ls`, footprint on the live fleet**: discovery (gemini and kimi-code discover
  serially; claude/codex scans read every head) and the 2 GB+ footprint from non-claude/codex
  stores were not worked on.


## Store datapaths pass (2026-10-12)

Source baseline: `c0f0044`; implementation and remaining work: `DATAPATHS.md`. These are
process-isolated operations over frozen copies or generated fixtures, with a scratch catalog.
Peak heap is requested live Rust allocation, not RSS, native SQLite allocation, or fragmentation.
Throughput runs use a clean release binary; allocation and fidelity instrumentation run separately.
The shared machine carried roughly 150–180 load early in this pass and 17–20 during the final
Gemini/affordance pairs. CPU comparisons below use interleaved baseline/candidate medians, not a
promise about interactive wall latency.

| Operation/input | Baseline peak Rust heap | Updated peak Rust heap |
|---|---:|---:|
| Gemini lazy count, frozen 313.8 MB / 199 messages | 841,082,713 B | 42,730 B |
| Gemini production affordance read, same source | 841,530,697 B | 41,702 B |
| Gemini one hook with late summary, 64 MiB plain text | 201,381,306 B | 137,865 B |
| Gemini one hook with late summary, 64 MiB escaped text | 201,381,306 B | 399,854 B |
| Codex tool-only lazy count, 16 → 64 MiB plain payloads | 12.4 → 49.7 MB | 4,263 B, flat |
| Codex tool-only lazy count, 16 → 64 MiB escaped payloads | 18.4 → 73.5 MB | 4,261 B, flat |
| Claude lazy count, frozen 189.8 MB | 5,138,961 B | 203,285 B |
| Claude lazy count, generated 100 MB plain text | 230,658,294 B | 10,914 B |
| Claude lazy count, generated 100 MB decoded escaped text | 364,875,160 B | 10,926 B |
| Continue discovery, 16 → 64 MiB plain body data | 33.6 → 134.4 MB | 1,242 B, flat |
| LM Studio discovery, 16 → 64 MiB plain body data | 33.7 → 134.7 MB | 1,392 B, flat |
| OpenClaw SQLite flat/tree reads, 64 MiB payloads | 67.9 MB | 1.05 MB |
| OpenClaw registry read, 128 MiB payloads | 135.6 MB | 2.1 MB |

The real Gemini affordance recording emits zero rows; the giant hook fixtures each emit one row
with the same ordered serialized digest before and after, including the late summary. Discovery
metadata digests match for all eight Continue/LM Studio scale cases and four indexed Continue cases.
Escaped discovery fields still require serde scratch: at these fixed record sizes Continue peaks
at 262,478 B and LM Studio at 131,395 B, independent of total source size.

Claude lazy count CPU medians (three interleaved pairs): frozen 0.307 → 0.273 s;
100 MB plain 0.0885 → 0.0512 s; escaped 0.718 → 0.404 s; 30,000 small records
0.0856 → 0.0417 s. Typed borrowed records avoid the old Value tree, and mmap line traversal
removes the giant-line buffer. Rich extras and uncommon fallback shapes remain owned.

Codex's event-authority prepass now combines syntax checking and discriminator projection, then
checks strict Value-compatible acceptance. On generated 64 MiB tool-only inputs, three paired
clean runs give median child CPU 0.0706 → 0.0508 s plain and 0.3487 → 0.3354 s escaped.
Retired instructions fall from 624.6M → 559.5M plain and 3.414B → 2.497B escaped. Load was
67–71 with integration builds running; caches were warm/uncontrolled, and an escaped wall-time
outlier remains in the raw results. The earlier redundant-scan prototype's slowdown is eliminated;
escaped CPU is best described as approximately unchanged, with constant heap at both input sizes.

Gemini lazy count CPU medians (three final interleaved pairs): 1.05 → 0.72 s, about 31% less
CPU; source-stage wall medians 1.124 → 0.813 s. The baseline's first 3.567 s outlier is retained
in the raw results; OS caches were not controlled. Production affordance reading on the same
source takes median 0.87 → 0.64 s user+system CPU and 0.894 → 0.653 s wall. The escaped
64 MiB late-summary fixture instead costs about 1.45× CPU: the bounded reducer trades a second
validation pass for dramatically lower peak allocation. No hashing runs contribute to these times.

Actual CLI JSON-window checks used paired debug builds, isolated catalogs and four 4 MiB Gemini
replies. All four first/last plain/escaped output hashes match the full-reader oracle. Selecting
only the first message lowers peak native footprint from 15–20 MB to about 6.8 MB. Selecting a
giant escaped tail remains output-sized and overlaps the full-reader footprint; no tail or
throughput improvement is claimed. Large selected escaped spans avoid whole quoted temporary
copies; spans at most 64 KiB retain the direct one-pass decoder.

Cursor's composer discovery query on 20,000 unrelated bubble rows falls from 80,043 to 52
SQLite VM steps, with zero full-scan steps. OpenClaw's bounded row/tree replay trades roughly
1.5× CPU on the measured flat/tree fixtures for the lower heap; registry CPU is approximately
unchanged. It should not be described as an across-the-board throughput improvement.

Escaped span chunk decoding now has a source-size-independent working set, with and without
mmap, including malformed tails. The decoder validates before emission to preserve whole-field
raw fallback semantics, then emits bounded serde-decoded windows. Dense escaped text takes about
2.1–2.3× the CPU of the old whole-field decoder; unescaped mmap text remains borrowed. CI gates
16/64 MiB cases by peak heap and growth, with no noisy wall-time threshold. A baseline reader
fails the same gate on source scaling, demonstrating that the gate catches the original defect.

These figures do not establish a new full-fleet recount time, first-run listing time, or total
process memory ceiling. Full output, rich metadata, selected SQLite blobs, non-mmap document
fallbacks, event queues and other stores still have separate bounds recorded in `DATAPATHS.md`.


## Composed-path follow-up (2026-10-12, in progress)

Baseline for this follow-up is `28daf7c`, the completed first datapaths pass. These results
cover individual changes; the combined implementation has not yet completed its final gates or
whole-fleet after run. Requested Rust heap excludes native SQLite allocations and mapped input.

| Operation/input | First-pass baseline | Follow-up |
|---|---:|---:|
| Claude metadata discovery, four outputs totaling 64 MiB | 49.54 MB plain / 68.43 MB escaped heap | approximately 96.4 KB, flat |
| Event extraction and SQL publication, 20,000 events | 7,211,499 B heap | 358,005 B |
| Event extraction and SQL publication, 200,000 events | 58,878,796 B heap | 358,007 B |
| Native SDK, OpenSession and account-export count/discovery, 16 → 64 MiB fixtures | approximately 33–60 → 135–241 MB heap | under 78 KB in tested cases |
| Selected owned text, 136 MB ordinary escaped source | 0.263 s user CPU | 0.133 s |
| Selected owned text, 136 MB surrogate-heavy source | 0.551 s user CPU | 0.282 s |
| Decoded-length count, same ordinary / surrogate sources | 0.244 / 0.594 s user CPU | 0.126 / 0.289 s |

The event spool writes and rereads approximately 2.42 / 24.58 MB of temporary data at the two
sizes. Clean median CPU was 0.110 → 0.119 s and 1.224 → 1.207 s respectively: the demonstrated
improvement is bounded retention, not a throughput multiplier. Temporary files are exclusive,
private and removed by their owner. SQL failures retain retryable state.

Selected-text timings are six alternating hash-disabled release pairs. Valid inputs use about
half the prior CPU; malformed-tail timing ranges overlap. The owned result still determines peak
memory, saving only about 65 KB for large output. A length-only consumer instead stays below
314 KiB in both mmap and portable builds. Independent hashes cover valid escapes, surrogate
pairs, compact Unicode escapes and malformed raw fallback; all match.

Claude discovery hashes match on 924 frozen references covering 6.8 GB. Paired median CPU on the
189.8 MB session fell from 0.508 to 0.250 s. Native document-reader resolved message and metadata
hashes match in lazy, full and complete modes on four escaped fixtures. Their portable cold path
still owns encoded source bytes: approximately 16.8 → 67.3 MB plain and 21.4 → 85.9 MB escaped.
Warm portable exports seek only the selected conversation. No universal document heap bound is
claimed. Rich-query reducers match the old collector on 282 sessions across 15 harnesses,
including exact affordance counts; unmatched relative paths can remain until final cwd is known.

The live whole-CLI baseline used isolated catalogs and write-denied original stores, with 12
logical workers and substantial shared load (roughly 80–105 during full indexing). It indexed
6,826 sessions with zero reported errors in 421.54 s wall / 592.80 s CPU. Peak RSS was 3.39 GB;
peak physical footprint was 823 MB. Those are different metrics. Samples caught both repeated
catalog open/schema/checkpoint work during publication and a later wait for Tantivy's final
merge. They do not establish either phase's percentage of total runtime.

The same baseline's first empty-catalog listing took 14.97 s, quiet warm listings 0.03–0.04 s,
and fresh listings a 1.14 s median. The fleet remained live and changed between operations, so
these are observations rather than frozen before/after claims. An isolated 16.29 MB prefix plus
623-byte append did resume and matched a complete rebuild's search results, counts and events.
The candidate replay remains pending.


Further composed checks, still before the final combined gate run:

| Operation | Baseline | Updated |
|---|---:|---:|
| Doctor, 16/64 MiB plain giant fields, Claude/Codex/Gemini | full reference scales to 33–67 MB | approximately 4.6–5.7 KB requested heap, flat |
| Doctor, same escaped fields | full reference scales with decoded fields | approximately 199–201 KB requested heap, flat |
| Tool histogram, 20k → 200k calls | 4.4 → 36.2 MB requested heap | Claude 2.6 KB / Codex 6.2 KB, flat |
| Rich query, 16 → 64 MiB fixed-size argument histories | collected 16.9–17.4 → 67.5–68.1 MB | approximately 0.27–1.13 MB, flat per case |
| Zed discovery, 16 → 64 MiB low-compressibility plain payload | 172 → 680 MB process footprint | 14.1 → 14.3 MB |
| Zed read, same payload | 113 → 444 MB process footprint | 14.2 → 16.4 MB |
| Semantic head projection, 64 MiB owned inline / tool input | added heap 192 / 128 MiB | approximately 64 KiB |

Doctor reports and rich-query booleans match their reference collectors. Tool histogram detail
objects match exactly on both Claude and Codex. The histogram allocation measurements use the
same debug instrumented binary for both paths; no throughput claim follows. Query fixtures use
256 KiB arguments, so they establish bounded history retention, not lazy giant arguments.

The final Zed test uses its canonical rowid schema and compressed blobs growing from 12.6 to
50.4 MB (escaped 13.1 to 52.5 MB). Rust heap remains about 6.68 MB, including decoder scratch;
escaped process footprints remain roughly 13–16.5 MB. Resolved messages and session metadata match
on all four fixtures. The WITHOUT ROWID/failed-blob-open fallback still retains one native cell.
No clean throughput improvement is claimed for the two-pass decoder.

Semantic projection measurements start after owned input construction. FTS's isolated production
read/extract/body-emission stage on one 100 MB input falls from 504 MB to 204 MB footprint after
streamed formatting and capped body documents; it excludes tokenization, queueing and final merge.
The remaining input Value and Tee clone still dominate. Concatenated canonical body bytes and
extracted edit events match. This is a bounded serialization improvement, not a bound on the
whole structured-input path.
