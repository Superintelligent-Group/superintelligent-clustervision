# Whole-operation Cline port CPU control

The final checked path uses **5.68% more instructions** than baseline for this one
mixed escaped 8 MiB workload. The original checked writer used 12.46% more. A
shared string-publication repair removes an unused decoded-length counting pass,
reducing the operation by about 67.16 million instructions. This is an accepted
CPU tradeoff alongside the separate bounded-memory gates, **not a speed claim**.

| Revision | Matched instruction counts | Change versus baseline |
| --- | --- | --- |
| Baseline 56e94da5 | 990,571,017 / 990,536,761 | — |
| Rejected intermediate 87f6322d | 1,113,952,373 / 1,113,924,021 | +12.46% |
| 87f6322d plus only 7a54dc49 wire patch | 1,046,771,904 / 1,046,787,523 | +5.68% |

The final source is isolated from unrelated commits merged into the owner's
branch. Only `harness/wire/write.rs` from 7a54dc49 replaces that file in 87f6322d.
Remaining work includes strict captured-range UTF8/JSON acceptance, both existing
source-feature validation boundaries and exact pretty-format publication. The
unused `StringCount` pass is gone. No validation policy or ownership state changed.

`evidence.json` records source/binary hashes, controls, attribution and exact
output hashes. Every normal native/instrumented run produced identical CLI output,
warnings and all three Cline files, with unchanged source bytes/metadata and frozen
catalog seed. Nothing is normalized. Baseline and intermediate `--strict` both
exit 1 with identical diagnostics/output: the verifier classifies timestamps
4→1 and message IDs 4→0 as **unexpected** losses. Those losses are pre-existing
and unchanged here; this control does not reclassify them. Final normal-port
warnings are identical too.

The operation includes actual CLI discovery/selection, checked preparation,
publication, target reparsing and fidelity reporting. One giant Text field has
4 MiB ordinary bytes and 4 MiB quote/backslash/newline/tab/λ bytes. Fixed timestamps
determine the Cline task ID. There are four messages and 9,507,799 bytes of useful
API history output, plus metadata/UI files. Setup, artifact removal and hashing
are outside the measured children. Guest user instructions are not timing, RSS
or syscall measurements. The memory gate is a separate workload and instrument.

## Reproduce

Use the pinned ARM64 Linux environment described in the profiling README, capped
at 3 CPUs/8 GiB, with one exclusive target and build job. `make-fixture.py` creates
the exact synthetic source without reading harness stores. Export 56e94da5 and
87f6322d to `before` and `after` under `/scratch/cline-port-cpu-control`. Copy after
to fixed and replace only wire/write.rs from 7a54dc49. Force exported Rust input
mtimes to avoid stale Cargo reuse. Build each with pinned Rust1.99.0:

```sh
CARGO_BUILD_JOBS=1 CARGO_PROFILE_RELEASE_DEBUG=1 CV_BUILD_SHA=cpu-control \
  cargo build --release -p clustervision
```

Preserve resulting `cv-before`, `cv-after`, `cv-fixed` binaries outside the target.
Copy generated source.jsonl to the control root. `run.py` prepares a one-session
frozen synthetic catalog, checks both native outputs, then takes two Cachegrind
counts per initial variant. Its normal command uses default port behavior, since
strict mode rejects the pre-existing losses. The runner may reuse its owned frozen
seed after the rejected strict preparation. `run-fixed.py` checks that original
source inode/metadata and seed, runs the repaired native oracle, then only two
repair children. Do not replace/reprepare the source before that focused rerun.

Both drivers reset only owned output/catalog state before each child. Cache and
branch simulation are disabled. No real store is written. Raw profiles, logs,
source manifests, strict diagnostics, frozen seed and final files are retained in
the private task archive; repository artifacts contain only synthetic paths.

Read-only writer review confirmed existing role/block insertion order, first-user
title/cwd injection, Tool-result pairing IDs, thinking-mode codec and metadata/UI
construction. Checked history/flush completes before destination creation. Owner
native/portable byte oracles cover all roles/modes for Cline and Roo, plus guarded
source failure; shared repair tests cover canonical escapes, skipped fallback,
invalid Unicode, writer failure and source-change precedence.
