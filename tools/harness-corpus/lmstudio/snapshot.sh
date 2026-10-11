#!/usr/bin/env bash
# Record (or --check) the shape of LM Studio's conversation store on this machine, without its
# content: the merged JSON shape of ~/.lmstudio/conversations/*.conversation.json. Read-only. See
# README.md for why LM Studio has no generated corpus.
#
#   tools/harness-corpus/lmstudio/snapshot.sh           # writes crates/cv-core/tests/corpus/lmstudio/<app version>/
#   tools/harness-corpus/lmstudio/snapshot.sh --check   # exits 1 when the shape differs from the
#                                                       # committed snapshot of the installed version
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
APP="${LMSTUDIO_APP:-/Applications/LM Studio.app}"
DATA="${LMSTUDIO_HOME:-$HOME/.lmstudio}"
V="$(defaults read "$APP/Contents/Info.plist" CFBundleShortVersionString)"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
python3 -I "$HERE/../schema_snapshot.py" json "$DATA/conversations/*.conversation.json" >"$TMP/conversations.shape.txt"
OUT="$ROOT/crates/cv-core/tests/corpus/lmstudio/$V"
if [ "${1:-}" = "--check" ]; then
  diff -ru "$OUT" "$TMP" -x SNAPSHOT.toml && echo "lmstudio $V: store shape unchanged"
  exit
fi
mkdir -p "$OUT"
cp "$TMP"/*.txt "$OUT/"
NEWEST="$(ls -t "$DATA"/conversations/*.conversation.json | head -1)"
cat >"$OUT/SNAPSHOT.toml" <<TOML
# Written by tools/harness-corpus/lmstudio/snapshot.sh: a store SHAPE, not a corpus (no sessions).
harness = "lmstudio"
app_version = "$V"
recorded = "$(date -u +%Y-%m-%d)"
store = "~/.lmstudio/conversations/*.conversation.json"
files = $(ls "$DATA"/conversations/*.conversation.json | wc -l | tr -d ' ')
store_last_written = "$(date -u -r "$NEWEST" +%Y-%m-%dT%H:%M:%SZ)"
note = "Merged over every conversation file on disk, which older LM Studio versions may have written (store_last_written)."
generator_commit = "$(git -C "$HERE" rev-parse --short HEAD)"
TOML
echo "wrote $OUT"
