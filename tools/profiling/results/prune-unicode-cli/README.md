# Whole-CLI Unicode prune instruction control

`evidence.json` retains the deciding counts and exact byte oracles. The contextual
negative controls are preserved: per-character +749.76%, batched replay +13.72%,
batched single-pass −1.71% relative to the same baseline. ASCII −24.98% and emoji
−77.72% use alpha,beta; Greek uses alpha,ος with ΟΣ after the entire 8 MiB x run.

`make-fixtures.py` recreates exact synthetic source bytes from the small template.
Export baseline5e366e96 into `/scratch/prune-unicode-cpu-control/before`; copy
that source to after/fixed/singlepass and replace only prune.rs from the recorded
variant SHAs. Touch exported Rust inputs before each build, using one exclusive
target. Build each with pinned Rust1.99, `CARGO_BUILD_JOBS=1`,
`CARGO_PROFILE_RELEASE_DEBUG=1`, `CV_BUILD_SHA=cpu-control`,
`cargo build --release -p clustervision`; preserve resulting cv-before/cv-after/
cv-fixed/cv-singlepass binaries outside the target. No probe-heap feature.

Copy the fixture directory to the synthetic scratch root. `run.py` establishes
frozen catalog seeds, checks both native gateways, then runs two instruction-only
Cachegrind children per initial variant/case. It retains completed case evidence
on restart. `run-fixed.py` and `run-singlepass.py` operate on the still-identical
Greek source inode/catalog seed and run only their native oracle and two children.
Do not rerun initial setup before these focused controls: each requires the saved
source metadata fingerprint. Every child restores catalog and watched directory
metadata and removes only its own generated siblings. Full transcript/sidecar,
stdout and stderr bytes must match; nothing is normalized.

Raw profiles, logs, seeds, source manifests and rejected setup/build evidence are
retained privately in the task evidence archive; repository evidence contains only
synthetic paths and fixtures. This is CPU work evidence, not a wall-clock claim.
