# Instruction-count control

`cachegrind.py` runs an existing `store_probe` binary natively, then twice under
Cachegrind, with a new scratch catalog before every invocation. It compares the
complete JSON outputs except the probe's `wall_seconds` instrumentation field,
records `Ir` (executed instructions), and emits function attribution reports.
No timings are compared. `RAYON_NUM_THREADS` is pinned to one by default;
`--threads N` changes and records it. The output directory must not exist.

Use Linux ARM64 on Apple Silicon, explicitly selecting the Docker platform.
Task-owned container and volume names below are examples; choose unique names.
The checkout is read-only and all compilation, inputs and catalogs live in the
new volume. No harness credentials or real stores are mounted.

```sh
docker run -d --platform linux/arm64 --name cv-cachegrind-control \
  --cpus=3 --memory=8g -v "$PWD:/work:ro" \
  -v cv-cachegrind-control:/scratch debian:trixie-slim sleep infinity
docker exec cv-cachegrind-control sh -c '
  apt-get update -qq &&
  apt-get install -y -qq curl ca-certificates build-essential python3 valgrind &&
  curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs |
    RUSTUP_HOME=/scratch/rustup CARGO_HOME=/scratch/cargo sh -s -- \
    -y --profile minimal --default-toolchain 1.99.0'
docker exec -e RUSTUP_HOME=/scratch/rustup -e CARGO_HOME=/scratch/cargo \
  cv-cachegrind-control sh -c 'cd /work &&
    CARGO_TARGET_DIR=/scratch/target CARGO_PROFILE_RELEASE_DEBUG=1 \
    CARGO_BUILD_JOBS=2 CLUSTERVISION_HOME=/scratch/build-home \
    /scratch/cargo/bin/cargo build -p clustervision-core --release --example store_probe'
docker exec cv-cachegrind-control python3 /work/tools/perf-fixture.py \
  claude /scratch/fixture --payload-bytes 65536 --records 16
docker exec cv-cachegrind-control python3 /work/tools/profiling/cachegrind.py \
  --input /scratch/fixture/projects/-perf/00000000-0000-4000-8000-000000000001.jsonl \
  /scratch/profile -- /scratch/target/release/examples/store_probe \
  count claude 00000000-0000-4000-8000-000000000001 \
  /scratch/fixture/projects/-perf/00000000-0000-4000-8000-000000000001.jsonl \
  --root /scratch/fixture --mode lazy
```

Default core features retain mmap, SQLite and parallel support. The recipe sets
no `RUSTFLAGS`; record any flags used at build time separately from the runner
environment (the binary cannot recover its build flags). `count` avoids
hashing the transcript as part of the profiled workload. For content-fidelity
checks, run a separate native `hash` operation with `--hash-detail`; count parity
alone proves only this operation's output. Archive the compiler version, image
identity, features, binary and input SHA256 alongside `summary.json`.

The [Cachegrind manual](https://valgrind.org/docs/manual/cg-manual.html) documents
instruction counts and attribution. Both cache and branch simulations are
explicitly disabled. These are ARM64 Linux guest instruction counts, not native
macOS instructions, Apple Silicon cache behavior or elapsed-time predictions.
The total includes process startup, dynamic loader, parsing, catalog work and
output serialization. Attribution helps identify which work a change removes.

Instructions can still vary because allocator addresses, ASLR, hash seeds,
filesystem/catalog state, syscall outcomes and threads change control flow.
Keep input, binary, command, path lengths and initial catalog state fixed; report
the observed difference rather than declaring the metric perfectly deterministic.

## Initial control (2026-10-12)

Source `26867544`, Debian trixie ARM64 image manifest
`sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f`;
container verified `aarch64`, 3 CPUs and 8 GiB memory. Rust 1.99.0
`b940084d7eb6a299eb4bfeb8e34901bc051e7ac4`, LLVM 23.1.1, Valgrind 3.24.0-3,
glibc 2.41-12+deb13u4 and GCC 14.2.0. Build: opt-level 3, ThinLTO,
debuginfo 1, default core features, no RUSTFLAGS. Runtime: one Rayon worker.

The deterministic synthetic history fixture has 16 records, 65,536 decoded
payload bytes, 76,259 physical bytes and 48 emitted messages. Its SHA256 and
the binary SHA256 are in [the result](results/claude-count-arm64.json).
Instruction-only runs counted **1,871,091** and **1,870,995** instructions:
96 fewer in the second run, **0.0051%** variation. Both complete count outputs
matched the native output after removing only `wall_seconds`. This profiles one
parser/count operation, not the entire `cv` CLI command boundary or fleet query.
A separate unprofiled native content hash oracle produced message SHA256
`11300364c11606b79e254d84c0b8d230047620977a6f67d20bcce9145378cfdb`;
it is not included in instruction counts.

Top function attribution in run one (inclusive of inlined source locations):

| Function | Ir | Share |
|---|---:|---:|
| serde_json SliceRead::skip_to_escape | 407,540 | 21.8% |
| dynamic loader relocation | 168,905 | 9.0% |
| core::str::from_utf8 | 119,081 | 6.4% |
| store::json::valid_unicode_escapes | 102,363 | 5.5% |
| store::json::value_compatible | 91,768 | 4.9% |

The nonzero run diff is in hashbrown/indexmap table entry/control and comparison
functions, consistent with randomized hash seeds. Parser attribution above is
unchanged between the runs. This is a useful low-variance work metric, not an
assertion that all process-level instruction totals are deterministic.

Raw Cachegrind files and both attribution reports are retained in the task-owned
Docker volume at `/scratch/profile-pinned`; compiler/package evidence is at
`/scratch/compiler.txt` and `/scratch/packages.txt`. The container is stopped
after capture; restarting it preserves the exclusive Linux target and results.

## Whole CLI controls

`--output bytes` compares stdout and stderr byte for byte against the native
child. `--harness-home` supplies an isolated synthetic HOME/XDG environment;
`--seed-catalog` restores a frozen catalog/index before each child. Fixtures and
seeds are hashed before and after the three invocations to reject input mutation.
A directory hash covers sorted relative filenames, lengths and bytes, without
following symlinks. Setup/generation/copying/hashing are outside the profiled child.

Index provenance includes its canonical path (`fts:<index path>`). Relocating a
seed causes legitimate reindexing. Use `--catalog-home` to restore the original
canonical path; it must initially be absent and becomes runner-owned. Prepare the
seed at that path, checkpoint SQLite, then move it aside before profiling. Do not
rewrite the seed's provenance strings to pretend a relocated index is unchanged.
`--expect-stderr TEXT` can require operation diagnostics in each child.

Example after building the actual CLI in the same exclusive Linux target:

```sh
# In the container; setup is outside Cachegrind.
python3 /scratch/cli-work/tools/profiling/cli-fixture.py /scratch/cli-fixture
export HOME=/scratch/cli-fixture/home RAYON_NUM_THREADS=1
export XDG_CONFIG_HOME=/scratch/seed-config XDG_DATA_HOME=/scratch/seed-data
export XDG_CACHE_HOME=/scratch/seed-cache
CV=/scratch/target/release/cv
RUNNER=/scratch/cli-work/tools/profiling/cachegrind.py
python3 "$RUNNER" --input "$HOME" --harness-home "$HOME" --output bytes \
  /scratch/cli-ls-cold -- "$CV" ls --json --limit 20
CLUSTERVISION_HOME=/scratch/cli-seed "$CV" ls --json --limit 20
CLUSTERVISION_HOME=/scratch/cli-seed "$CV" index
# Checkpoint all seed *.db files with sqlite3's PRAGMA wal_checkpoint(TRUNCATE).
python3 "$RUNNER" --input "$HOME" --harness-home "$HOME" --output bytes \
  --seed-catalog /scratch/cli-seed /scratch/cli-ls-warm -- "$CV" ls --json --limit 20
CLUSTERVISION_HOME=/scratch/profiled-index-catalog "$CV" index
CLUSTERVISION_HOME=/scratch/profiled-index-catalog "$CV" index
# Require the second setup pass to say 0 changed/new, 8 unchanged, 0 errors;
# checkpoint SQLite, then move this task-owned directory to /scratch/index-seed-fixed.
python3 "$RUNNER" --input "$HOME" --harness-home "$HOME" --output bytes \
  --seed-catalog /scratch/index-seed-fixed --catalog-home /scratch/profiled-index-catalog \
  --expect-stderr '0 changed/new' --expect-stderr '8 unchanged, 0 errors' \
  /scratch/cli-index-fixed -- "$CV" index
```

The warm seed's full-sync stamp must remain within the configured freshness
backstop (default 900 seconds); confirm that the intended warm route was measured.
For before/after controls retain the exact input inodes, seed bytes, canonical
catalog/index paths, auxiliary paths, commands and environment. Preserve old
reports before reusing their paths. Count only child instructions, not setup.

### Recorded CLI results (2026-10-13)

Eight synthetic sessions (four Claude, four Codex), 586,452 physical input bytes.
Source `78a3f86d` plus publication change `2a10cfda` (local combined `d14714a6`),
same Rust/Valgrind/image/build settings as above, actual default-feature CLI,
one Rayon worker, three CPUs/8 GiB. No timing comparison, cache/branch simulation,
semantic model or real harness data. Each row is two repeats of one fixed state:

| Operation | Ir run 1 | Ir run 2 | Difference |
|---|---:|---:|---:|
| cold `cv ls --json --limit 20` | 23,632,021 | 23,625,936 | 0.0257% |
| verified warm list, same command | 5,579,741 | 5,578,344 | 0.0250% |
| unchanged `cv index` | 20,039,841 | 20,033,524 | 0.0315% |

All stdout and stderr bytes match each operation's native child. Cold and warm
list outputs also match each other (3,091 bytes). Index diagnostics confirm zero
changed/new, zero removed, eight unchanged and zero errors. Hashes and provenance
are in the three `cli-*-arm64.json` results. Raw profiles and complete attribution
remain in the exclusive volume under their `/scratch/cli-*` operation directories.

File attribution accounts for all instructions; these categories are source-file
locations, **not inclusive call-tree costs** (inlining assigns work to Rust core
files separately):

| Source files | Cold list | Warm list | Unchanged index |
|---|---:|---:|---:|
| SQLite C | 7,383,981 (31.2%) | 2,154,697 (38.6%) | 7,971,380 (39.8%) |
| glibc allocator | 4,401,882 (18.6%) | 1,225,674 (22.0%) | 5,061,983 (25.3%) |
| dynamic loader | 565,025 (2.4%) | 563,637 (10.1%) | 565,469 (2.8%) |
| clap sources | 89,139 (0.4%) | 89,139 (1.6%) | 85,483 (0.4%) |

Warm list's SQLite parser alone executes 334,382 instructions; unchanged index's
executes 1,213,829, plus 779,418 in parser reduction and 639,980 in tokenization.
Cold list additionally spends 1,208,012 in serde JSON escape scanning. True unchanged
index attributes only 59,946 directly to Tantivy files and performs no transcript
indexing. A deliberately rejected relocated-seed control did tokenize/reindex;
its 133M instructions are not an unchanged-index result.

The next supported candidate is operation-scoped catalog connection/schema/SQL
preparation reuse: catalog initialization repeats across last-sync, freshness,
row reads and index planning. This can remove repeated setup without weakening
fresh inventory/source checks. It needs its own before/after proof; this profile
does not establish how much of SQLite or allocation is removable. Loader/clap
are fixed startup work for this small fixture. These user-space guest instruction
counts omit kernel/I/O wait costs and do not predict Apple Silicon elapsed time
or the scaling of a 6,000-session fleet. Known-ID windows and rich queries were
not profiled in this control.

### Matched catalog-owner comparison

[The comparison artifact](results/catalog-reader-comparison-arm64.json) records
baseline `d14714a6` and production `9dd193bd` on the **same eight-session fixture**.
Both binaries used the same recorded compiler, default features, optimized debug
profile, image and resource limits. Both ran at `/scratch/profile-bin/cv`, with
identical input inodes/metadata/content, frozen seed bytes/metadata, canonical
catalog/index and auxiliary paths, one Rayon worker and configured catalog
backstop of 3,600 seconds. The larger backstop prevents a frozen warm seed aging
into a cold scan during build/test work; it does not bypass metadata freshness.
The default-900-second exploratory results above are not the comparison baseline.

| Operation | Before Ir (two runs) | After Ir (two runs) | Mean reduction |
|---|---:|---:|---:|
| cold list | 23,623,945 / 23,643,712 | 20,420,190 / 20,422,311 | 13.59% |
| warm list | 5,581,467 / 5,582,102 | 3,683,962 / 3,681,027 | 34.03% |
| unchanged index | 20,051,981 / 20,050,922 | 17,046,383 / 17,051,089 | 14.98% |

Both phases independently match native stdout/stderr bytes, and before/after
outputs also match exactly. Index diagnostics still report eight unchanged,
zero changed/new, removed or errored sessions. After-repeat spreads are 0.010%,
0.080% and 0.028%; the small remaining variability is not presented as zero.
The before/after SQLite-C source attribution falls from 7,384,046 to 5,359,893
instructions (cold), 2,154,531 to 843,601 (warm), and 7,970,781 to 5,956,846
(index). Clap attribution is identical; loader attribution increases by 1,925
instructions in each operation. File attribution is not inclusive call cost.
The catalog setup reduction accounts for a substantial part of saved work.

`compare.py BEFORE.json AFTER.json` rejects mismatched runtime controls, source
state and output. When both summaries carry `build` metadata, it also compares
compiler/LLVM/profile/features/flags/image/resources while allowing source commit
and binary hash to differ. Raw runner summaries cannot recover their build flags:
when build metadata is absent the helper explicitly reports that it was not
verified. This experiment's build metadata was recorded from the build recipe and
container evidence, then checked separately and included in the durable artifact.

This confirms avoided fixed setup work on a small workload. It does not measure
kernel/I/O time, native macOS elapsed time, large-fleet costs or a current ranking
of all system operations. No additional workload matrix was added. Baseline and
after raw profiles remain under `/scratch/cli-*-reader-before` and `/scratch/cli-*`
in the retained task-owned volume; the container is stopped after capture.

### First-use scoped discovery cache allocations

`discovery-cache.py --before PROBE --after PROBE --output NEW_DIRECTORY` compares production
`cv_core::sessions()` using two `store_probe` binaries with `--features probe-heap`. It creates
one synthetic Claude inventory, warms its persisted catalog during setup, then appends a user
record. Every measured child starts with an empty process-local cache and the same restored
catalog path and original transcript identity. Adding 4,096 unrelated serialized cache rows
changes no affected inventory. Setup and result reporting are outside the measured child stage;
elapsed-time fields are discarded. The listing must remain one exact session reference with message count two.

The [recorded native allocation control](results/discovery-cache-allocation-native.json) uses
rustc 1.99, the same dev build recipe/default mmap+SQLite+parallel features, and one Rayon worker.
Build parity was verified separately; the runner checks the features reported by the probes.
At 4,096 unrelated rows, lazy payload decoding reduces allocation requests **50,421 → 13,549**,
requested bytes **21,478,200 → 8,967,285**, and peak above the starting Rust heap
**13,748,681 → 7,948,688 bytes**. All four listing digests and empty stderr match; with zero
unrelated rows, peak stays **20,706 bytes**. These are requested Rust allocations, excluding
SQLite/C, stacks, mmap and allocator overhead. No native elapsed-time or fleet-wide ranking is
claimed. Bulk SQL string copying and serialized-row retention still grow with cache size;
this removes unrelated JSON decoding and nested allocations, not the complete first-use floor.
The fixture is synthetic metadata, not a genuine harness corpus capture.

`index-discovery-work.py` is an evidence-only native `cv index` control for one synthetic Claude
primary. It requires the diagnostic [patch](results/index-discovery-work.patch), applied to
`c8d16a92` in a separate worktree, and the snapshot example contained in that patch. Build the
CLI with `--features probe-heap,cv-core/probe-heap,cv-search/probe-heap` and the example with
`cargo build -p cv-search --example index_work_snapshot --no-default-features`, using an exclusive
target and scratch catalog. Run with `--scratch NEW_DIRECTORY --cv FILE --snapshot FILE`.
The script creates only scratch stores and keeps discovery, parser/framer, mmap admission,
lazy-decoder consumption and actual prefix-guard `File::Read` return counts separate. The patch
is not runtime telemetry shipped in production. No elapsed-time comparison is collected.

The [native baseline](results/index-discovery-work-native.json) admits **8,404,110 bytes** in
both cold discovery and indexing. An **848-byte** append makes discovery submit all **8,404,958
bytes** to its metadata framer while resumed indexing supplies only the 848-byte suffix to its
framer; the mapped extent is still the whole file. Unchanged input submits no source bytes at
these instrumented boundaries. Required UTF-8/grammar/strict-compatibility inputs are separately
reported, not added together as physical I/O. The unescaped decoder consumes borrowed slices;
its consumption counters are not `Read` return counters.

The baseline intentionally records two failed parity controls: title-changing resume retained
obsolete body title postings, and a timestamp-restored same-size edit was skipped by tuple-only
primary freshness. Metadata, counts and events agreed with the fresh oracle; complete stored
fields and controlled public queries exposed the defects. Commit `022e9475` fixes the former
with metadata guards and an indexing-only migration. The latter motivates separate primary
identity authority. Query scores are evidence, not a universal fresh/incremental parity promise:
Tantivy statistics can include deleted postings until merge. Indexed body text is not stored,
so the snapshot oracle compares all live stored fields plus the explicitly listed term queries,
not every possible query or complete IR fidelity. This fixed single-primary control establishes
an overlap target; it does not rank a real fleet or measure physical storage I/O.

### Scoped keyed discovery versus full demand (2026-10-13)

[Matched instruction evidence](results/discovery-keyed-cpu-arm64.json) compares
`1377fdc9` with `b199c972`, identical default-feature CLI builds using the pinned
Linux control above. Actual `cv ls --json --harness claude --limit 100000` counts
36,874,230/36,857,176 baseline instructions versus4,023,954/4,023,954 keyed for one
changed Claude source plus4096unrelated persisted cache rows:89.085% fewer.
Forced full discovery (`--fresh`) of4096demanded warm files counts
501,433,563/501,373,628 versus546,727,785/545,854,238:8.952% more. Repeat spread is
at most0.160%; stdout/stderr match native and cross-revision exactly (one session,
325bytes;4096sessions,5,599,219bytes). This is guest instruction work, not timing.

First full-run SQLite source attribution increases41,838,251→64,660,153,
accounting for22.822M of45.294M added instructions; serialization attribution is
unchanged. File attribution is not inclusive call-tree attribution. Scoped keyed
reads are valuable; explicit full demand merits a requested-path batch candidate,
without resurrecting fleet-wide unrelated cache payload retention.

[Reproduction](discovery-cpu-control.py) creates synthetic sources/frozen seeds,
restores the same canonical catalog/binary paths, then invokes the existing
Cachegrind/compare tools. Build the two archived revisions into distinct binaries
at `/scratch/keyed-discovery-cpu/cv-before` and `cv-after` with the controls above;
place common build metadata at `build.json` and sources at `before`/`after`.
The source/archive/binary hashes and exact controls are in the artifact. Exported
builds both stamp `unknown`; forced affected-crate compilation establishes their
source. An initial identical-binary Cargo mtime reuse was detected and rejected;
touch exported source/build inputs or force an own-target scoped clean. Original
raw profiles/fixtures/targets remain in `cv-cachegrind-control` under
`/scratch/keyed-discovery-cpu`; host evidence/report is at
`/Users/ember/dev/cv-worktrees/keyed-discovery-cpu-evidence`. Existing artifacts
were retained; installed CLI and real stores were untouched.
