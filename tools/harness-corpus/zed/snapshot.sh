#!/usr/bin/env bash
# Record (or --check) the shape of the Zed agent store on this machine, without its content:
# threads/threads.db's DDL plus the merged JSON shape of its zstd `data` blobs, and the DDL of the
# agent tables in db/<n>-stable/db.sqlite. Read-only source copies include WAL; nothing is written
# next to the store. See README.md for why Zed has no generated corpus.
#
#   tools/harness-corpus/zed/snapshot.sh            # writes crates/cv-core/tests/corpus/zed/<app version>/
#   tools/harness-corpus/zed/snapshot.sh --check    # exits 1 when the store's shape differs from the
#                                                   # committed snapshot of the installed version
set -euo pipefail
case "${1:-}" in
  ""|--check) ;;
  *) echo "usage: snapshot.sh [--check]" >&2; exit 2 ;;
esac
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
APP="${ZED_APP:-/Applications/Zed.app}"
DATA="${ZED_DATA:-$HOME/Library/Application Support/Zed}"
V="$(defaults read "$APP/Contents/Info.plist" CFBundleShortVersionString)"
OUT="$ROOT/crates/cv-core/tests/corpus/zed/$V"
if [ "${1:-}" != "--check" ] && [ -e "$OUT" ]; then
  echo "refusing: $OUT exists (add a new version instead of regenerating)" >&2
  exit 1
fi
THREADS="$DATA/threads/threads.db"
WORKSPACE="$(ls -d "$DATA"/db/*-stable/db.sqlite 2>/dev/null | head -1)"
[ -f "$THREADS" ] || { echo "no $THREADS" >&2; exit 1; }
SNAP=(python3 -I "$HERE/../schema_snapshot.py")
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
"${SNAP[@]}" sqlite "$THREADS" --json-column threads.data:zstd >"$TMP/threads.schema.txt"
[ -z "$WORKSPACE" ] || "${SNAP[@]}" sqlite "$WORKSPACE" \
  --tables sidebar_threads,sidebar_terminal_threads,thread_archived_worktrees,archived_git_worktrees \
  >"$TMP/workspace-agent-tables.schema.txt"
if [ "${1:-}" = "--check" ]; then
  diff -ru "$OUT" "$TMP" -x SNAPSHOT.toml && echo "zed $V: store shape unchanged"
  exit
fi
mkdir -p "$OUT"
cp "$TMP"/*.txt "$OUT/"
LAST="$(python3 -I - "$HERE/../schema_snapshot.py" "$THREADS" <<'PY'
import importlib.util
import sys
spec = importlib.util.spec_from_file_location("schema_snapshot", sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
with module.sqlite_snapshot(sys.argv[2]) as database:
    print(database.execute("SELECT max(updated_at) FROM threads").fetchone()[0] or "")
PY
)"
cat >"$OUT/SNAPSHOT.toml" <<TOML
# Written by tools/harness-corpus/zed/snapshot.sh: a store SHAPE, not a corpus (no sessions).
harness = "zed"
app_version = "$V"
recorded = "$(date -u +%Y-%m-%d)"
store = "threads/threads.db (+ the agent tables of db/*-stable/db.sqlite)"
store_last_written = "$LAST"
note = "The DDL is what the installed app migrated to; the JSON shape is merged over every thread on disk, which older Zed versions may have written (store_last_written)."
generator_commit = "$(git -C "$HERE" rev-parse --short HEAD)"
TOML
echo "wrote $OUT"
