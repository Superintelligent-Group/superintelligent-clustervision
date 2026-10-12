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
`std::fs::read` of the same bytes in the same run, ~8 GB/s warm): claude 310–400 MB/s, codex 340–600 MB/s, i.e. 14–29x a
plain read. Where it goes:

- **claude**: 68% building a `serde_json::Value` per record line (every string allocated and
  unescaped), then a second `RawValue` scan of `message.content` for the lazy spans.
- **codex**: by bytes (every 100th codex file by size, 43 files, 1.4 GB), `compacted` records are 43% (their
  `replacement_history` is the whole prior conversation; the parser reads only its length) and
  `item_completed` 28%. A giant `function_call_output` line is scanned three times (outer
  `RawValue`, inner struct, escape search).
- **mmap**: the span path maps each file and faults it in 16 KB pages, one fault per page.

## Budgets

| what | budget | gate |
|---|---|---|
| parse, counting pass | ≥ 1 GB/s per core on the largest real sessions; on the committed corpus, no slower than today's ratio to a plain read | `tests/perf_budget.rs` (ratio, CI step) |
| `cv index`, nothing changed | ≤ 1 s | manual |
| `cv index` after an append | O(new bytes): ≤ 0.5 s + 10 ms per new MB | manual |
| `cv index`, full recount | ≤ 60 s on 12 cores, peak RSS ≤ 2 GB | manual |
| `cv ls`, first run | ≤ 300 ms (stretch; discovery has to read every session head once) | manual |
| `cv ls`, warm / `has:` | ≤ 100 ms | manual |
| `cv show --last N` | ≤ the counting pass of that session | manual |

The gate compares throughput as a ratio to a `std::fs::read` of the same bytes in the same run,
so a slow CI box moves both sides and the budget measures cv, not the runner.
