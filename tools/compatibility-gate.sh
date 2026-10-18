#!/usr/bin/env bash
# Build release/candidate with the same private serde probe. Retain owned warm artifacts.
# No cache is shared across candidate worktrees. No cleanup, downloads, or push.
set -euo pipefail
fail() { echo "compatibility-gate: $*" >&2; exit 1; }
root=$(git rev-parse --show-toplevel)
cd "$root"
release=$(git tag --merged HEAD --sort=-version:refname | python3 -c 'import re,sys; tags=[s.strip() for s in sys.stdin if re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+\n?",s)]; print(tags[0] if tags else "")')
[[ -n "$release" ]] || fail 'No reachable stable release tag; fetch full history/tags.'
sha=$(git rev-parse "$release^{commit}")
probe=crates/cv-core/examples/compatibility_probe.rs
# Owner + release SHA + probe bytes + toolchain determine cache identity.
key=$(python3 -c 'import hashlib,pathlib,sys; print(hashlib.sha256((sys.argv[1]+sys.argv[2]+"1.99.0").encode()+pathlib.Path(sys.argv[3]).read_bytes()).hexdigest())' "$root" "$sha" "$probe")
scratch=${CV_COMPAT_CACHE_ROOT:-${TMPDIR:-/tmp}/cv-compat-cache}/$key
mkdir -p "$scratch"
scratch=$(cd "$scratch" && pwd -P)
# Portable, fail-closed lock: never steal another invocation's lock, even after a crash.
mkdir "$scratch/lock" 2>/dev/null || fail "cache busy; inspect retained lock: $scratch/lock"
trap 'rmdir "$scratch/lock"' EXIT
baseline=$scratch/baseline
receipt=$scratch/owner
identity=$(printf '%s\n%s\n%s' "$root" "$sha" "$key")
if [[ -e $baseline ]]; then
    [[ -f $receipt && $(cat "$receipt") == "$identity" ]] || fail 'unowned or mismatched baseline cache'
    [[ $(git -C "$baseline" rev-parse HEAD) == "$sha" ]] || fail 'baseline SHA changed'
    [[ $(git -C "$baseline" rev-parse --show-toplevel) == "$baseline" ]] || fail 'baseline worktree replaced'
    [[ -z $(git -C "$baseline" symbolic-ref -q HEAD || :) ]] || fail 'baseline has an attached branch'
    cmp "$probe" "$baseline/$probe" || fail 'baseline probe changed'
    # Only the deliberately installed private probe may differ from the release.
    while IFS= read -r path; do
        [[ $path == "$probe" ]] || fail "baseline has existing changes: $path"
    done < <(git -C "$baseline" status --porcelain --untracked-files=all | cut -c4-)
else
    git worktree add --detach "$baseline" "$sha"
    cp "$probe" "$baseline/$probe"
    printf '%s\n' "$identity" > "$receipt"
fi
# CI need not provision sccache. Honor an explicit wrapper; use one only when installed.
if [[ -z ${RUSTC_WRAPPER+x} ]] && command -v sccache >/dev/null; then
    export RUSTC_WRAPPER=sccache
fi
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-8}
[[ "$CARGO_BUILD_JOBS" =~ ^[1-8]$ ]] || fail 'CARGO_BUILD_JOBS must be an integer from 1 to 8'
export CLUSTERVISION_HOME="$scratch/build-home"
candidate_target=${CARGO_TARGET_DIR:-"$root/target"}
mkdir -p "$candidate_target"
candidate_target=$(cd "$candidate_target" && pwd)
[[ $candidate_target != "$scratch/baseline-target" ]] || fail 'candidate and baseline targets must differ'
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
    --baseline-tag "$release" --candidate-version "$version" --exercise-mutations
printf 'Compatibility artifacts retained in %s\n' "$scratch"
