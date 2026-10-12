# Store probe

This diagnostic example performs one read-only operation per process on an explicit source.
It supports Claude, Codex, Gemini and OpenClaw through their explicit-root constructors. It does not
detect installed homes or discover to resolve IDs. Direct stream operations never update the catalog.
The composed `affordance-read` operation may persist counts in the baseline implementation: it requires
an existing scratch `CLUSTERVISION_HOME` and refuses the real catalog directory. Claude fork detection
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
`--materialize`. The default mode is lazy. The root defaults to the input's parent; for discovery
pass the config/runtime root explicitly (`projects/`, `sessions/`, or `tmp/` lives beneath it).

| Operation | Work measured |
|---|---|
| `baseline` | Adapter/input setup has completed; empty stage |
| `discard` | Direct stream, count callbacks, immediately discard every message |
| `count` | Same stream plus transcript/session/store affordance counts |
| `collect` | Direct stream into a retained message vector; optional materialization |
| `window` | Stream to EOF, retain only `--last N` messages (default 20), optionally materialize |
| `affordances` | Full collection and materialization, classify and retain affordance rows |
| `affordance-read` | Production `affordance::read`; requires scratch `CLUSTERVISION_HOME` |
| `hash` | Resolve each message, sort owned JSON maps recursively, hash streamed JSON and final metadata |
| `discover` | Explicit-root adapter discovery |
| `chunks` | Direct span decoding; `--span OFFSET:LENGTH [--escaped] --chunk-bytes N` |

`hash` emits independent SHA-256 digests of newline-delimited canonical message JSON and final
session metadata. It resolves spans and sorts input/details/extra maps in place, avoiding a second
whole-message Value tree or serialized string. It excludes session `source_path` (transport
location) only; all other serialized metadata remains. Compare **the same mode** before/after;
full/complete extras and carriers intentionally differ from lean modes. Hashing materializes one
message and must not be included in throughput/heap claims for a discard/count consumer.

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

For standalone escaped decoder allocation testing, `chunks` bypasses the parser entirely. A
file containing `x\\n` repeated N times is a valid raw JSON string body: use offset 0, length 3N,
`--escaped` and a small chunk. Build with `--no-default-features --features probe-heap` to measure
the positioned-read fallback separately from mmap. Check decoded bytes and maximum emitted chunk
alongside allocation growth; output hashing is a different experiment.
