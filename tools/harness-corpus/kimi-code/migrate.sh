#!/usr/bin/env bash
# Capture what a newer Kimi Code does to a store an older one wrote: open the old run's sessions
# with the new version, continue each top-level session by one turn, and copy the result.
#
#   tools/harness-corpus/kimi-code/migrate.sh <old-version> <old-work-dir> <new-version> <out-dir>
#
# <old-work-dir> is the dir a `generate.sh <old> ... --keep` run left (its `home/` is the old
# store). The store records absolute paths (state.json cwd, session_index sessionDir), so the new
# version must run at the SAME path: the old dir is first copied to <old-work-dir>.orig, then
# modified in place. <out-dir> is normally crates/cv-core/tests/corpus/kimi-code/<old>-then-<new>.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$HERE/../lib.sh"
OLD_V="${1:?usage: migrate.sh <old-version> <old-work-dir> <new-version> <out-dir>}"; OLD="${2:?}"; V="${3:?}"; OUT="${4:?}"
OLD="$(cd "$OLD" && pwd -P)"
mkdir -p "$(dirname "$OUT")"; OUT="$(cd "$(dirname "$OUT")" && pwd -P)/$(basename "$OUT")"
[ ! -e "$OUT" ] || { echo "refusing: $OUT exists" >&2; exit 1; }
[ ! -e "$OLD.orig" ] || { echo "refusing: $OLD.orig exists (this old run was already migrated)" >&2; exit 1; }
BIN="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/kimi-code/$V/node_modules/.bin/kimi"
[ -x "$BIN" ] || { echo "kimi-code $V not installed: run $HERE/install.sh $V" >&2; exit 1; }
cp -Rp "$OLD" "$OLD.orig"

WORK="$OLD"; CORPUS_HOME="$WORK/home"; export WORK CORPUS_HOME
PROXY_PID=""
cleanup() { [ -z "$PROXY_PID" ] || kill "$PROXY_PID" 2>/dev/null || true; }
trap cleanup EXIT
export HOME="$CORPUS_HOME" KIMI_CODE_HOME="$CORPUS_HOME/.kimi-code" TMPDIR="$WORK/tmp"
unset XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME KIMI_API_KEY KIMI_BASE_URL KIMI_SHARE_DIR \
  OPENAI_API_KEY ANTHROPIC_API_KEY
corpus_guard HOME KIMI_CODE_HOME TMPDIR

corpus_load_key UPSTREAM_KEY "${KIMI_KEY_FILE:-$REAL_HOME/.kimikey}"
rm -f "$WORK/port" "$WORK/calls.migrate.jsonl"
python3 -I "$HERE/../passthrough_proxy.py" --port-file "$WORK/port" --upstream "https://api.kimi.com/coding/v1" \
  --strip-prefix /v1 --log "$WORK/calls.migrate.jsonl" --max-calls "${MAX_CALLS:-10}" &
PROXY_PID=$!
unset UPSTREAM_KEY
for _ in $(seq 50); do [ -s "$WORK/port" ] && break; sleep 0.1; done
export KIMI_MODEL_NAME="${CORPUS_MODEL:-kimi-for-coding}" KIMI_MODEL_API_KEY="corpus-local-proxy" \
  KIMI_MODEL_BASE_URL="http://127.0.0.1:$(cat "$WORK/port")/v1" KIMI_CODE_NO_AUTO_UPDATE=1

cd "$CORPUS_HOME/project"
"$BIN" session list </dev/null >>"$WORK/migrate.log" 2>&1 || echo "    (session list exit $?)"
# Every session that has a prompt in it (the /clear stub has none) gets one more turn.
for s in "$KIMI_CODE_HOME"/sessions/wd_*/session_*; do
  id="$(basename "$s")"
  grep -q '"type":"turn.prompt"' "$s/agents/main/wire.jsonl" 2>/dev/null || continue
  echo "--- kimi $V -S $id -p 'Say ok.'"
  "$BIN" -S "$id" -p "Say ok." </dev/null >>"$WORK/migrate.log" 2>&1 || echo "    (exit $?)"
done
CALLS="$(wc -l <"$WORK/calls.migrate.jsonl" | tr -d ' ')"
echo "model calls: $CALLS"

mkdir -p "$OUT/.kimi-code"
(cd "$KIMI_CODE_HOME" && find sessions -path '*/logs' -prune -o -path 'sessions/.index-*' -prune -o -type f -print \
  | while IFS= read -r f; do mkdir -p "$OUT/.kimi-code/$(dirname "$f")"; cp "$f" "$OUT/.kimi-code/$f"; done)
for f in session_index.jsonl workspaces.json; do
  [ -e "$KIMI_CODE_HOME/$f" ] && cp "$KIMI_CODE_HOME/$f" "$OUT/.kimi-code/"
done
corpus_scrub "$OUT"
corpus_manifest "$OUT" \
  "harness=\"kimi-code\"" "version=\"$V\"" "generated=\"$(date -u +%Y-%m-%d)\"" \
  "migrated_from=\"$OLD_V (the same generate.sh run as corpus/kimi-code/$OLD_V, opened by $V)\"" \
  "model=\"${CORPUS_MODEL:-kimi-for-coding} via api.kimi.com/coding/v1, through passthrough_proxy.py\"" \
  "model_calls=$CALLS" \
  "scenarios=[\"session_list\",\"resume_each_session_once\"]" \
  "skipped=[]" \
  "generator_commit=\"$(git -C "$HERE" rev-parse --short HEAD)\""
corpus_size_check "$OUT"
