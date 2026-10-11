#!/usr/bin/env bash
# Capture OpenCode 2.x's in-place migration of a 1.x store: copy a committed 1.x corpus database
# into a throwaway data dir, start <new-version>'s server over it (the server runs the v1 → v2
# migration at startup, `packages/core/src/database/v1-migration.ts`), wait until it reports the
# migration completed, and commit the database it left. No model calls.
#
#   tools/harness-corpus/opencode/migrate.sh <from-corpus-dir> <new-version> <out-dir>
#   e.g. migrate.sh crates/cv-core/tests/corpus/opencode/1.18.20 2.0.25 \
#          crates/cv-core/tests/corpus/opencode/1.18.20-migrated-2.0.25
#
# The input is the already-scrubbed corpus copy, so its paths are already /home/corpus/...; the
# migration only rewrites rows, it does not need the project on disk.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$HERE/../lib.sh"

FROM="${1:?usage: migrate.sh <from-corpus-dir> <new-version> <out-dir>}"; V="${2:?}"; OUT="${3:?}"
SRC="$FROM/.local/share/opencode/opencode.db"
[ -f "$SRC" ] || { echo "no $SRC" >&2; exit 1; }
FROM_V="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$FROM/MANIFEST.toml")"
mkdir -p "$(dirname "$OUT")"; OUT="$(cd "$(dirname "$OUT")" && pwd -P)/$(basename "$OUT")"
[ ! -e "$OUT" ] || { echo "refusing: $OUT exists (add a new version instead of regenerating)" >&2; exit 1; }
BIN="${HARNESS_VERSIONS:-$REAL_HOME/tools/harness-versions}/opencode/$V/node_modules/.bin/opencode"
[ -x "$BIN" ] || { echo "opencode $V not installed: run $HERE/install.sh $V" >&2; exit 1; }
SPORT="${PORT:-18776}"

corpus_work
SERVE_PID=""
cleanup() { [ -z "$SERVE_PID" ] || kill "$SERVE_PID" 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT
export HOME="$CORPUS_HOME" TMPDIR="$WORK/tmp"
export XDG_DATA_HOME="$CORPUS_HOME/.local/share" XDG_CONFIG_HOME="$CORPUS_HOME/.config"
export XDG_STATE_HOME="$CORPUS_HOME/.local/state" XDG_CACHE_HOME="$CORPUS_HOME/.cache"
mkdir -p "$TMPDIR" "$XDG_DATA_HOME/opencode" "$XDG_CONFIG_HOME/opencode"
unset OPENCODE_DB OPENCODE_CONFIG OPENCODE_CONFIG_DIR OPENCODE_CONFIG_CONTENT
export OPENCODE_DISABLE_AUTOUPDATE=1 OPENCODE_DISABLE_SHARE=1
corpus_guard HOME TMPDIR XDG_DATA_HOME XDG_CONFIG_HOME XDG_STATE_HOME XDG_CACHE_HOME
DB="$XDG_DATA_HOME/opencode/opencode.db"
cp "$SRC" "$DB"
echo '{"$schema":"https://opencode.ai/config.json","autoupdate":false,"share":"disabled"}' >"$XDG_CONFIG_HOME/opencode/opencode.json"

# A loopback-only password for this throwaway server (2.x requires one); not a credential.
export OPENCODE_PASSWORD="corpus-local-server"
cd "$CORPUS_HOME/project"
"$BIN" serve --port "$SPORT" >>"$WORK/serve.log" 2>&1 &
SERVE_PID=$!
status=""
for _ in $(seq 300); do
  status="$(curl -s -u "opencode:$OPENCODE_PASSWORD" "http://127.0.0.1:$SPORT/api/experimental/migration/v1" || true)"
  case "$status" in *'"completed"'*) break ;; *'"error"'*) echo "migration failed: $status" >&2; exit 1 ;; esac
  sleep 0.2
done
case "$status" in *'"completed"'*) echo "migration: $status" ;; *) echo "migration did not finish: $status" >&2; exit 1 ;; esac
kill "$SERVE_PID"; wait "$SERVE_PID" 2>/dev/null || true; SERVE_PID=""

mkdir -p "$OUT/.local/share/opencode"
python3 -I "$HERE/thin.py" "$DB" "$OUT/.local/share/opencode/opencode.db"
corpus_scrub "$OUT"
corpus_manifest "$OUT" \
  "harness=\"opencode\"" "version=\"$V\"" "generated=\"$(date -u +%Y-%m-%d)\"" \
  "migrated_from=\"$FROM_V (tests/corpus/opencode/$(basename "$FROM"))\"" \
  "how=\"opencode $V serve over a copy of that store; its v1 to v2 migration ran at startup\"" \
  "model_calls=0" \
  "thinned=[\"kv rows models-dev:*\",\"event rows\"]" \
  "generator_commit=\"$(git -C "$HERE" rev-parse --short HEAD)\""
corpus_size_check "$OUT"
