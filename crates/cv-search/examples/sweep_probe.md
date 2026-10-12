# Isolated production sweep oracle

The ignored `production_sweep_heap_probe` drives the actual planning, parser worker pool,
FTS/event/count tee, producer channel, Tantivy indexing pipeline, commits and final compaction.
It uses production 4 MiB body chunks, a fresh temporary catalog/index, four logical giant
sessions and 32 small sessions. Every session's search, event and count publication is checked;
a second sweep must skip all sources. It never discovers ambient harness stores.

`probe-heap` installs the shared testkit allocator in the **test binary only**. Counters measure
requested live Rust allocations; C/SQLite allocation, mmap, stacks and allocator overhead are
outside them. Only run the named ignored oracle with this feature: it selects production chunk
sizes instead of the ordinary tiny test chunks. Timing includes heap instrumentation.

Generate the narrow fixture outside the measured process. This example keeps generator scratch
bounded and writes a synthetic two-message native Claude transcript with one giant Edit argument.
Use a directory owned by this task:

```python
from pathlib import Path

root = Path('/tmp/cv-sweep-fixtures')
root.mkdir(exist_ok=True)
prefix = (b'{"type":"user","sessionId":"perf","uuid":"user","cwd":"/perf",'
          b'"message":{"role":"user","content":"Inspect the file"}}\n'
          b'{"type":"assistant","sessionId":"perf","uuid":"assistant",'
          b'"message":{"role":"assistant","model":"test","content":[{"type":"tool_use",'
          b'"id":"call","name":"Edit","input":{"file_path":"/perf/a.rs","content":"')
suffix = b'","metadata":{"n":3,"flag":true}}}]}}\n'
for size in (25_000_000, 100_000_000):
    for escaped in (False, True):
        unit = b'\\u0078' if escaped else b'x'
        path = root / f'{"escaped" if escaped else "plain"}-{size}.jsonl'
        with path.open('xb') as out:
            out.write(prefix)
            for start in range(0, size, 65536):
                out.write(unit * min(65536, size - start))
            out.write(suffix)
```

Build the named test once, then run it in its own process for each fixture:

```sh
CLUSTERVISION_HOME=/tmp/cv-sweep-catalog \
CARGO_TARGET_DIR=/tmp/cv-sweep-target CARGO_BUILD_JOBS=2 \
cargo test -p cv-search --no-default-features --features probe-heap \
  production_sweep_heap_probe --lib --no-run

CV_SWEEP_INPUT_PROBE=/tmp/cv-sweep-fixtures/plain-100000000.jsonl \
CV_SWEEP_INPUT_BYTES=100000000 \
CLUSTERVISION_HOME=/tmp/cv-sweep-catalog \
CARGO_TARGET_DIR=/tmp/cv-sweep-target CARGO_BUILD_JOBS=2 \
cargo test -p cv-search --no-default-features --features probe-heap \
  production_sweep_heap_probe --lib -- --ignored --nocapture --test-threads=1
```

The JSON result reports charged queue peak, largest document, waits, composed heap peak,
wall time, and unchanged-sweep time. `CV_SWEEP_HASH_BODY=1` additionally compares every giant
session's complete canonical body bytes with an independent FNV oracle. Hashing adds measured
work; the report distinguishes this mode. `CV_SWEEP_COPIES` and `CV_SWEEP_SMALL` select the mix.

`CV_SWEEP_COMPARE_UNBOUNDED=1` disables only the raw-document byte limit in the probe build for
an otherwise identical control. This knob is absent from ordinary production builds. The
64 MiB bound includes compact-document byte capacity, conservative field-vector capacity and
wrapper overhead; a single document larger than the limit runs alone. Parser working sets,
each worker's body buffer and document waiting for admission, completed event/checkpoint
payloads, Tantivy's 50 MB indexing arenas and merge memory remain separate costs.
