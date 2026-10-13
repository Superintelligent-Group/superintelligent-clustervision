# Prune probe

`prune_probe` mutates its input's directory: **use synthetic/scratch fixtures only**, never real
harness stores. Supply a scratch `CLUSTERVISION_HOME` when building/running. The fixed new id
makes old/new transcript, sidecar and report comparisons deterministic.

```sh
CARGO_TARGET_DIR=/tmp/prune-target CARGO_BUILD_JOBS=1 cargo build \
  -p clustervision-core --release --example prune_probe
cp /tmp/prune-target/release/examples/prune_probe /tmp/prune-clean
CARGO_TARGET_DIR=/tmp/prune-target CARGO_BUILD_JOBS=1 cargo build \
  -p clustervision-core --release --example prune_probe --features probe-heap
cp /tmp/prune-target/release/examples/prune_probe /tmp/prune-heap
CLUSTERVISION_HOME=/tmp/prune-home /tmp/prune-clean /tmp/fixture/source.jsonl revive
CLUSTERVISION_HOME=/tmp/prune-home /tmp/prune-heap /tmp/fixture/source.jsonl revive
```

Accepted modes combine freely: `revive`, `window` (4000 tokens), `range` (turns 3–12), `drop`,
`thinking`, `declassify` (two synthetic terms), `dry`, `resources`. Default keep-last is 2.
The report includes all result fields and revive fields plus elapsed wall time and an optional
requested Rust heap snapshot. Measure timing only with the clean binary: counting allocations
changes runtime. Allocator counters exclude stacks, mmap, filesystem page cache and C heaps.

The 2026-10-12 scale fixture has 128/512 assistant/user pairs. Each assistant carries a small
Read input and cumulative usage; each user carries a 64 KiB result plus a 64 KiB mirror. This
produces approximately 16/64 MiB transcripts, with small individual records. The separate semantic
control covers compaction boundaries, synthetic and sidechain records, singleton titles, thinking,
declassification, usage iterations, CRLF, malformed rows, trailing whitespace and resource copies.

A single giant changed record still uses the owned mutation fallback. Scale measurements show
aggregate retention removed; they do not establish constant scratch for an arbitrarily giant row.
