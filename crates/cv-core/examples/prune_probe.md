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

Large unchanged records now stream through source-backed restamping and unchanged revival.
The giant ignored-field 16/64 MiB control uses 1.192 MiB requested peak in both cases. Changed
records, usage corrections and malformed giant records retain an owned fallback. See
`docs/design/PRUNE-SOURCE-EVIDENCE.json` for separate allocator-free CPU costs.

Large changed string tool-result controls now use source-backed canonical replacement and sidecar
writing (269,475 bytes default / 463,553 bytes revival at both 16/64 MiB). Rich payloads and other
mutation kinds retain owned fallback; see `docs/design/PRUNE-PAYLOAD-EVIDENCE.json`.

The source user path also covers rich arrays and mirrors; mixed/image and string/mirror 16/64 MiB
controls now have constant requested peak below 465 KB for default/revive/drop/window controls
without a nonnull parent reroot. See `docs/design/PRUNE-RICH-EVIDENCE.json`.

Large Thinking and ASCII declassification now share source turn projections, including their
sidecars and composed revival estimates. Native 16/64 MiB actual operations have constant
requested peak: Thinking 269,950 bytes, ASCII declassification 332,541 bytes (revival below464KB).
Unicode declassification preserves a whole demanded-string lowercase fallback; usage correction
still owns selected rows at this milestone. See `PRUNE-TURN-EVIDENCE.json`; no timing claim.
