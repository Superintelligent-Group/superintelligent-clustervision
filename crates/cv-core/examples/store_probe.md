# Store probe

This diagnostic example performs one read-only operation per process on an explicit source.
It supports Claude, Codex, Gemini, OpenClaw, Continue, LM Studio, Cline, Kimi, Kimi Code, Grok, Qwen,
OpenSession, ChatGPT/Claude account exports and (with SQLite enabled) Devin/Zed through explicit-root constructors. It does not
detect installed homes or discover to resolve IDs. Direct stream operations never update the catalog.
The composed `affordance-read` operation may persist counts, and `discover` initializes the catalog's
scan cache: both require an existing scratch `CLUSTERVISION_HOME` and refuse the real catalog directory. Claude fork detection
and adapter side-store reads still examine siblings of the supplied source, so use a frozen
scratch directory. `discover` explicitly scans `--root`; the other operations read `PATH`.

Build clean timing and requested Rust heap versions separately, with your own target directory:

```sh
CARGO_TARGET_DIR=/tmp/my-probe-target CARGO_BUILD_JOBS=2 cargo build --release \
  -p clustervision-core --example store_probe
cp /tmp/my-probe-target/release/examples/store_probe /tmp/my-probe-clean
CARGO_TARGET_DIR=/tmp/my-probe-target CARGO_BUILD_JOBS=2 cargo build --release \
  -p clustervision-core --example store_probe --features probe-heap
cp /tmp/my-probe-target/release/examples/store_probe /tmp/my-probe-heap
```

Run `OP HARNESS ID PATH`, optionally `--root DIR`, `--mode lazy|bulk|full|complete` and
`--materialize`; `--mode lazy-extra` retains extras alongside lazy content. The default mode is lazy. The root defaults to the input's parent; for discovery
pass the config/runtime root explicitly (`projects/`, `sessions/`, or `tmp/` lives beneath it).
Continue expects its global directory containing `sessions/`; LM Studio expects the conversations
directory containing `*.conversation.json` itself. OpenClaw expects its state directory.
Cline expects a home containing `.cline/`; Kimi, Kimi Code and Grok expect their native homes
containing `sessions/`. Devin/Zed expect the directory containing `sessions.db`/`threads.db`. Directory session
paths (Kimi, Kimi Code and Grok) report directory metadata size, not total transcript/sidecar bytes;
use the generator manifest for total generated physical source sizes.
OpenSession and account exports scan only the explicit root, excluding configured exports and
`CV_EXPORTS`; their direct stream operations select the supplied ID from the supplied document.

| Operation | Work measured |
|---|---|
| `baseline` | Adapter/input setup has completed; empty stage |
| `discard` | Direct stream, count callbacks, immediately discard every message |
| `count` | Same stream plus transcript/session/store affordance counts |
| `checkpoint` | Count through the resumable pipeline; return last checkpoint in `detail.checkpoint`, every `--chunk-bytes N` bytes |
| `resume` | Continue fixture checkpoint from `--resume-from FILE`; report suffix callbacks and whole-session counts |
| `doctor` | Production context attribution report, using the requested parse mode |
| `token-stats` | Production token statistics for the explicit session and its subagent forest; scratch catalog required |
| `collect` | Direct stream into a retained message vector; optional materialization |
| `window` | Stream to EOF, retain only `--last N` messages (default 20), optionally materialize |
| `affordances` | Full collection and materialization, classify and retain affordance rows |
| `affordance-read` | Production `affordance::read`; requires scratch `CLUSTERVISION_HOME` |
| `hash` | Resolve each message, sort owned JSON maps recursively, hash streamed JSON and final metadata |
| `discover` | Explicit-root discovery; requires scratch `CLUSTERVISION_HOME` |
| `chunks` | Direct span decoding; `--span OFFSET:LENGTH [--escaped] --chunk-bytes N` |
| `resolve` | Direct span materialization into a retained owned string; `--span OFFSET:LENGTH [--escaped]` |
| `resolved-len` | Direct decoded byte counting without owning text; same span flags |

`token-stats` calls `TokenStats::compute` on the explicit session reference. Its parse policy is
production-defined; `--mode` is ignored. The complete statistics are in `detail`; `messages`
stays zero because the operation does not expose callback counts. Child detection can examine
sibling files, so isolate the entire supplied source directory. Aggregation and JSON report
construction are both included in the stage.

For append diagnostics, save `checkpoint`'s `detail.checkpoint` as JSON, append to the same
scratch transcript, then run `resume --resume-from FILE`. These operations never save marks or
counts in the catalog. The diagnostic calls the adapter directly: **it does not validate inode,
sampled prefix, prior consumer output or dependency freshness**. Use only an unchanged fixture
prefix and matching parse mode. Production FTS performs those checks before reuse; the composed
regression tests compare search documents, events and counts with a whole read. `messages` counts
only callbacks in the resumed suffix, while `detail.turns` and `detail.counts` include the prefix.
A partial EOF record is replayed from the last complete safe checkpoint.

`--hash-output` explicitly enables consumer fidelity digests. Discovery hashes all returned
`SessionRef` metadata, sorted by ID/path (paths included); both affordance operations hash their
ordered rows, including summaries/items and store rows. Leave it off for throughput and allocation
measurements; compare output digests in separate fidelity runs.
`--warm-discover` runs the explicit-root adapter's discovery during setup and drops its returned
refs before timing/heap capture. It requires a scratch catalog. The report labels this setup and
includes retained cache metadata in baseline live heap; released source buffers are excluded
from the warm selected-read stage. Compare cold and warm separately, especially for no-mmap
account archives where cold admission can own the archive but warm reads own only a selected range.
For `resolve`, `--hash-output` adds a SHA-256 digest of the decoded UTF-8 bytes; the default
reports only decoded length. The owned result remains live through the heap snapshot, so its
requested output size is part of the measured heap and is not subject to the chunk scratch bound.

`json-len`, `json-write` and `json-prefix` exercise source-backed `Json` directly with `--span OFFSET:LENGTH`.
The range contains one complete JSON value, including string quotes or container delimiters;
`--escaped` is unused. All include source construction and strict validation in the measured
stage. `json-len` reports the canonical encoded byte count; `json-write` writes canonical bytes
to a counting discard sink, or with `--hash-output` to a streaming SHA-256 sink. Neither retains
an owned output or invokes the adapter. `json-prefix --prefix-bytes N` retains a canonical prefix
through the heap snapshot, reporting its byte count and truncation; the default budget is 64 KiB
and zero is allowed. `--hash-output` additionally hashes only the retained prefix. Compare hashes in separate fidelity runs so hashing
does not affect ordinary CPU or heap measurements.

`hash` emits independent SHA-256 digests of newline-delimited canonical message JSON and final
session metadata. It resolves spans and sorts input/details/extra maps in place, avoiding a second
whole-message Value tree or serialized string. It excludes session `source_path` (transport
location) only; all other serialized metadata remains. Compare **the same mode** before/after;
full/complete extras and carriers intentionally differ from lean modes. Hashing materializes one
message and must not be included in throughput/heap claims for a discard/count consumer.
`hash --hash-detail` also emits each message's fingerprint/serialized size and canonical session
metadata, to locate a mismatch without dumping all message text. It performs extra serialization
and retains fingerprint rows, so use it only for fidelity diagnosis. The production
`affordance-read` reports its parse/materialization policy as production-defined/unknown because
the same diagnostic must describe baseline and candidate library implementations truthfully.

The heap build reports successful Rust allocation requests, baseline/peak/end live requested
bytes and cumulative requested bytes. Its hooks include realloc requests and run throughout
process startup; stage boundaries never reset live bytes. Collected sessions/affordance rows
remain alive at the end snapshot. This excludes native allocations, mmap, stacks, allocator
overhead and fragmentation. Hook atomics affect timings: use the clean build for timing.

Use `/usr/bin/time -l` (macOS) or `/usr/bin/time -v` (Linux) around each clean process for total
wall/user/system CPU, RSS and faults. JSON `wall_seconds` measures the stage only; process RSS
includes startup and setup. For macOS file-backed versus private residency also inspect
`vmmap --summary`/`footprint` on a long enough run. A directory/whole DB size is not selected
session bytes; this tool labels metadata size `physical_input_bytes` and reports traversed bytes
as unknown. It never primes input with a whole-file read. Record explicitly warmed versus
unprimed input and catalog state, and do not claim a cold OS cache without controlling it.

Generate fresh representative fixtures outside the measured process:

```sh
python3 tools/perf-fixture.py gemini /tmp/my-gemini-fixture \
  --payload-bytes 100000000 --records 1 --escaped
```

`PERF-FIXTURE.json` records the explicit root/path/id, expected message count, decoded payload
bytes, encoded physical bytes and shape. The generator refuses an existing destination; it writes
unique IDs and parent links rather than repeating identical records. Use many records with the
same total payload to distinguish per-record overhead from giant fields, then repeat for Claude
and Codex. It creates synthetic scratch inputs, not new committed harness corpus versions.
The generator also supports Continue (unindexed history) and LM Studio (two versions per reply,
selected version first) for discovery scaling. Use fixed record size while increasing total bytes;
escaped discarded strings may still require scratch proportional to the largest field.
The native-format generator also supports `cline-sdk`, `opensession`, `chatgpt-export`,
`claude-export` and `kimi`. `--shape history` scales the selected transcript; account exports'
`--shape unrelated` puts a growing unrelated conversation before a tiny selected conversation.
Kimi's `--shape sidecar-unmatched` grows unmatched wire results and extras while preserving a
tiny selected last-wins result; `sidecar-selected` grows the output actually requested instead.
Manifests distinguish total generated payload, selected payload and each physical input file.
These cases derive their envelopes from corpus schemas, but are synthetic fixtures and do not
replace or regenerate corpus versions. Use `--records TOTAL_BYTES/262144` for fixed-size fields.
`zed` uses the corpus thread schema and the native level-3 zstd codec (the generator needs `zstd`
on PATH). Its manifest separates decoded JSON size, compressed blob size and SQLite file size;
generation and compression run outside the measured process.

For standalone escaped decoder allocation testing, `chunks` bypasses the parser entirely. A
file containing `x\\n` repeated N times is a valid raw JSON string body: use offset 0, length 3N,
`--escaped` and a small chunk. Build with `--no-default-features --features probe-heap` to measure
the positioned-read fallback separately from mmap. Check decoded bytes and maximum emitted chunk
alongside allocation growth; output hashing is a different experiment.

CI's live Rust heap gate is `tools/check-heap-budget.py MMAP_HEAP_PROBE --no-mmap-probe RANGED_HEAP_PROBE`.
It validates the binaries' reported build features and rejects missing allocation counters. The
16/64 MiB scratch cases check lazy Gemini/Claude/Codex giant fields and Claude/Codex stores with
fixed-size records in mmap builds. Valid/malformed escaped spans run with both mmap and positioned
reads; this no-mmap guarantee applies to the resolver, not whole-record adapter buffering. It checks expected message
or decoded byte counts, absolute heap ceilings and growth, without a wall-time threshold. Full
collection, deliberately owned tool arguments and native SQLite heap are outside these promises.
`--only stores` / `--only chunks` supports a scoped local check. CI's exact build commands are in
`.github/workflows/ci.yml`; the driver always uses a temporary catalog and deletes only its own inputs.

CI's separate native `--only active` pass extends the same 8 MiB absolute / 2 MiB growth budgets to discarded
fixed-size Cline SDK/OpenSession history, account archives' unrelated conversations, and unmatched
Kimi wire bodies. Every 16/64 MiB discovery probe gets a fresh catalog, preventing cache hits from
masking scan work. `--format FORMAT` narrows this campaign; `--baseline-probe HEAP_PROBE` also checks
small resolved-message/metadata and fresh discovery digests in separate fidelity runs. Selected
giant outputs and whole selected ChatGPT mappings are not included in these bounded claims.
`--only doctor` checks giant Claude/Codex/Gemini decoded attribution in lazy-extra mode against
separate full-mode processes, optionally using the supplied old baseline. Default CI's suite
includes stores/chunks, the five native active formats and the composed doctor pass. The doctor
gate compares lazy-extra with the full report oracle in separate processes. Portable resolver promises are checked
separately; cold no-mmap document readers may still own the source bytes and are not admitted
to the native adapter limits.
`--only active --format zed` explicitly opts into compressed-thread discovery/streaming with
fixed-size messages, retaining the same budgets; native SQLite allocations remain excluded.

`query --query EXPR` invokes the production predicate reducer; `query-collect --query EXPR`
uses the independent lazy collector followed by `matches_full`. Both require a scratch catalog,
report the same boolean result, and force lazy parsing. The collected session stays alive through
the heap snapshot. Use rich tool arguments and repeated events to measure retained history;
large text spans alone do not exercise that distinction. Callback counts are not observed by
these composed APIs. Compaction predicates have a separate validation pass and should be
measured separately from ordinary tool/model/path queries.

`perf-fixture.py claude|codex NEW_DIR --shape tool-arguments --records N --payload-bytes B`
generates N Edit calls with distinct absolute paths and model `test`; B is the total decoded
argument content. Fixed-size arguments across larger N expose query retention. Setting B to
zero and growing N isolates event/histogram retention without a demanded giant argument.

`check-heap-budget.py ... --only query` compares the production reducer and collector on
16/64 MiB Claude/Codex histories with fixed 256 KiB arguments. Queries demand the last unique
target, verify the boolean oracle, and enforce the same 8 MiB / 2 MiB native heap limits.
The collector's peak is reported as a negative control, without applying the reducer budget
to its deliberately retained IR. A single giant argument still requires a record-sized Value;
this gate promises bounded history retention, not lazy argument materialization.

`tools` streams through the production `ToolHistogram::from_stream` used by forest tools;
`tools-collect` uses the previous EventSink collection followed by `from_events`. Both force
lazy parsing and report the complete small histogram as `detail`, so exact output parity needs
no additional digest or transcript materialization. Neither updates the catalog. Callback counts
are not observed by the composed histogram API. Use fixed-size tool calls with distinct targets
and one tool name to distinguish retained event history from demanded histogram output. The old
path includes EventSink's per-record checkpoint requests; the histogram consumer never resumes
from or stores them.

The event operations include extraction and SQL publication. `events` reports the logical temporary
file bytes written and read once, excluding filesystem block rounding and catalog IO; small batches
stay in memory. Temporary files are exclusively created with private permissions and removed on
success or ordinary errors. A process crash can leave its private temporary file for OS cleanup,
but the catalog transaction rolls back. Distinct targets are capped in the publication cache, and
ordered batches retain deduplication keys only for one message. A single enormous message or target
remains a largest-record cost. Compare fixed-size records at increasing message counts; separate
heap and clean timing runs, and state whether the scratch catalog is new or already populated.

`tools/check-heap-budget.py MMAP_HEAP_PROBE --no-mmap-probe RANGED_HEAP_PROBE --only events`
is the event retention gate run in CI. It generates 20,000 and 200,000
small Claude Edit records with distinct targets, verifies every persisted row and the source stamp,
checks temporary-file cleanup and I/O accounting, and bounds Rust heap at 1 MiB with 256 KiB growth.
The retained `events-collected` oracle must produce identical rows and demonstrate growing heap.
The focused event case uses only the mmap binary. Native SQLite memory, giant tool arguments and
crash cleanup remain outside this heap guarantee.

`hash --raw-order` resolves content but preserves IR map insertion order in message and metadata
digests. It detects serialization-order drift that the default canonical digest deliberately
normalizes. It does not compare original harness JSON bytes or whitespace. The report marks
canonical JSON version as null and serialization order as preserved; ordinary hash output stays
compatible with canonical version 1. Use the same switch on both compared binaries.

CI also runs `--only active --format zed` with zstd installed for generated compressed rowid fixtures. This caps requested Rust heap; separate process-footprint measurements cover SQLite native memory.
