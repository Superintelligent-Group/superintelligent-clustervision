#!/usr/bin/env bash
# Build last stable release plus the same private serde probe in an isolated checkout/target.
# The caller owns candidate's checkout/target. No downloads, cleanup of other artifacts, or push.
set -euo pipefail
root=$(git rev-parse --show-toplevel)
cd "$root"
release=$(git tag --merged HEAD --sort=-version:refname | python3 -c 'import re,sys; tags=[s.strip() for s in sys.stdin if re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+\n?",s)]; print(tags[0] if tags else "")')
[[ -n "$release" ]] || { echo 'No reachable stable release tag; fetch release tags before this gate.' >&2; exit 1; }
sha=$(git rev-parse "$release^{commit}")
scratch=$(mktemp -d "${TMPDIR:-/tmp}/cv-compat-build.XXXXXXXX")
baseline="$scratch/baseline"
git worktree add --detach "$baseline" "$sha"
# An intentional temporary addition: exercise the released serde implementation, not a pristine build claim.
cp crates/cv-core/examples/compatibility_probe.rs "$baseline/crates/cv-core/examples/compatibility_probe.rs"
export RUSTC_WRAPPER=${RUSTC_WRAPPER:-sccache}
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-8}
[[ "$CARGO_BUILD_JOBS" =~ ^[1-8]$ ]] || { echo 'CARGO_BUILD_JOBS must be an integer from 1 to 8' >&2; exit 1; }
export CLUSTERVISION_HOME="$scratch/build-home"
candidate_target=${CARGO_TARGET_DIR:-"$root/target"}
# Absolute target paths survive the baseline's different working directory.
mkdir -p "$candidate_target"
candidate_target=$(cd "$candidate_target" && pwd)
(
    cd "$baseline"
    CARGO_TARGET_DIR="$scratch/baseline-target" cargo +1.99.0 build --locked -p clustervision --bin cv
    CARGO_TARGET_DIR="$scratch/baseline-target" cargo +1.99.0 build --locked -p clustervision-core --example compatibility_probe
)
CARGO_TARGET_DIR="$candidate_target" cargo +1.99.0 build --locked -p clustervision --bin cv
CARGO_TARGET_DIR="$candidate_target" cargo +1.99.0 build --locked -p clustervision-core --example compatibility_probe
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
printf 'Compatibility baseline %s (%s); candidate %s; temporary probe added to %s\n' "$release" "$sha" "$(git rev-parse HEAD)" "$baseline"
python3 -I tools/check-compatibility.py \
    --baseline "$scratch/baseline-target/debug/cv" --candidate "$candidate_target/debug/cv" \
    --baseline-probe "$scratch/baseline-target/debug/examples/compatibility_probe" \
    --candidate-probe "$candidate_target/debug/examples/compatibility_probe" \
    --baseline-source "$baseline" --candidate-source "$root" \
    --baseline-tag "$release" --candidate-version "$version"
# Keep this run's own checkout/target for diagnosis; the caller may remove it explicitly later.
printf 'Compatibility artifacts retained in %s\n' "$scratch"
