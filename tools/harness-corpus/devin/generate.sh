#!/usr/bin/env bash
# Generate the Devin CLI corpus for one installed version: the sessions.db (and transcripts/,
# summaries/) Devin wrote itself under ~/.local/share/devin/cli, in a throwaway home.
#
#   tools/harness-corpus/devin/generate.sh <version> <out-dir> [--keep] [--only "chat tools ..."]
#
# Devin routes every model call through Cognition's backend under the user's Devin account; it
# has no custom-endpoint or bring-your-own-key path, so Z.ai cannot drive it. It needs a signed-in
# account: the generator copies ONLY ~/.local/share/devin/credentials.toml into the throwaway home
# and deletes the copy on exit, and it uses a model the account lists as Free (default swe-2-medium,
# `devin models list`) so the run costs nothing. Env: DEVIN_CREDENTIALS (default the real file),
# CORPUS_MODEL.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$HERE/../lib.sh"
. "$HERE/../drive.sh"

V="${1:?usage: generate.sh <version> <out-dir>}"; OUT="${2:?usage: generate.sh <version> <out-dir>}"; shift 2
KEEP=0; ONLY="chat tools subagent plan"
while [ $# -gt 0 ]; do
  case "$1" in --keep) KEEP=1 ;; --only) ONLY="$2"; shift ;; *) echo "unknown arg $1" >&2; exit 2 ;; esac; shift
done
mkdir -p "$(dirname "$OUT")"; OUT="$(cd "$(dirname "$OUT")" && pwd -P)/$(basename "$OUT")"
[ ! -e "$OUT" ] || { echo "refusing: $OUT exists (add a new version instead of regenerating)" >&2; exit 1; }
BIN="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/devin/$V/node_modules/.bin/devin"
[ -x "$BIN" ] || { echo "devin $V not installed: run $HERE/install.sh $V" >&2; exit 1; }
MODEL="${CORPUS_MODEL:-swe-2-medium}"
CREDS="${DEVIN_CREDENTIALS:-$REAL_HOME/.local/share/devin/credentials.toml}"
[ -r "$CREDS" ] || { echo "no Devin credentials at $CREDS (run devin auth login first)" >&2; exit 1; }

corpus_work
DSTORE="$CORPUS_HOME/.local/share/devin"
cleanup() { rm -f "$DSTORE/credentials.toml"; [ "$KEEP" = 1 ] && echo "kept $WORK" || rm -rf "$WORK"; }
trap cleanup EXIT
export HOME="$CORPUS_HOME" TMPDIR="$WORK/tmp"
mkdir -p "$TMPDIR"
unset XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME DEVIN_MODEL DEVIN_PERMISSION_MODE \
  CHISEL_SESSION_DB WINDSURF_API_KEY DEVIN_API_URL
corpus_guard HOME TMPDIR
mkdir -p "$DSTORE"; install -m 600 "$CREDS" "$DSTORE/credentials.toml"
STORE="$DSTORE/cli"
PROJECT="$CORPUS_HOME/project"
corpus_project
mkdir -p "$CORPUS_HOME/.config/devin"
cat >"$CORPUS_HOME/.config/devin/config.json" <<JSON
{"mcpServers": {"corpus": {"command": "python3", "args": ["-I", "$HERE/../codex/mcp_server.py"]}}}
JSON

cd "$PROJECT"
run() {
  echo "--- devin $*" | cut -c1-140
  "$BIN" --model "$MODEL" --permission-mode "${PMODE:-dangerous}" --respect-workspace-trust false \
    "$@" </dev/null >>"$WORK/exec.log" 2>&1 || echo "    (exit $?)"
}
last_id() { sqlite3 "$STORE/sessions.db" "select id from sessions order by created_at desc, rowid desc limit 1"; }
want() { case " $ONLY " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
SCEN=(); SKIP=()

if want chat; then   # 1 multi-turn chat + 9 resume (-r continues a session)
  run -p "Say hello in five words."
  id="$(last_id)"
  run -r "$id" -p "Now say it in French."
  run -r "$id" -p "Which language did you use first? One word."
  SCEN+=("chat" "resume")
fi
if want tools; then  # 2 read/edit/shell, 3 parallel calls, 4 a tool error, an image file read,
  # the todo tool, 10 MCP, web fetch: one resumed session, because every Devin session row and
  # every turn's re-copied system prefix cost ~20-30 KB (the 1 MB cap allows three or four sessions)
  run -p "Read a.txt and b.txt at the same time (two parallel tool calls), then reply with their combined line count."
  id="$(last_id)"
  run -r "$id" -p "Edit notes.txt to replace TODO with DONE, then run the command: cat notes.txt"
  run -r "$id" -p "Run the command: cat missing-file.txt   and tell me the error in one line."
  run -r "$id" -p "Look at the image file dot.png with your file-reading tool and tell me its size in pixels."
  run -r "$id" -p "Use your todo list tool to record two todos for renaming b.txt, mark the first done, and do not edit files."
  run -r "$id" -p "Use the corpus MCP server's add tool to add 2 and 3, then say the result."
  run -r "$id" -p "Fetch https://example.com with your web tool and tell me the page title."
  SCEN+=("tools" "parallel_tools" "tool_error" "image_read" "todo_list" "mcp" "web_fetch")
fi
if want subagent; then # 7 sub-agents: run_subagent. 3000.11.3 writes the child's chain into the
  # parent's message_nodes forest as a second root, with no subagent_heads row.
  run -p "Delegate to a sub-agent (your subagent tool) the task 'list the files in this directory', wait for it, then report what it found."
  SCEN+=("subagent")
fi
if want plan; then   # plan mode (sessions.agent_mode = plan) through the /plan command
  run -p "/plan how to rename b.txt to c.txt, in two steps, without editing anything"
  SCEN+=("plan_mode")
fi
SKIP+=("sidekick (subagent_heads): Devin's sidekick runs only under a Fusion lead+sidekick model pairing, whose lead models are paid"
  "compaction: -p /compact writes summaries/history_<hex>.md but no summary nodes before print mode exits, and DEVIN_COMPACTION_THRESHOLDS takes an undocumented format (JSON and k=v were ignored); the 262K window of the free model is out of budget"
  "image: print mode has no attachment flag (the corpus reads dot.png through the read tool instead)")
rm -f "$DSTORE/credentials.toml"

# Copy the store as Devin wrote it: sessions.db (backup API: WAL), transcripts/, summaries/.
# Not credentials, config, logs, installation_id, app_state.json, locks or plugins.
mkdir -p "$OUT/.local/share/devin/cli"
sqlite3 "$STORE/sessions.db" ".backup '$OUT/.local/share/devin/cli/sessions.db'"
sqlite3 "$OUT/.local/share/devin/cli/sessions.db" "PRAGMA journal_mode=DELETE" >/dev/null
rm -f "$OUT/.local/share/devin/cli/sessions.db-shm" "$OUT/.local/share/devin/cli/sessions.db-wal"
for d in transcripts summaries; do [ -d "$STORE/$d" ] && cp -R "$STORE/$d" "$OUT/.local/share/devin/cli/"; done
NSESS="$(sqlite3 "$OUT/.local/share/devin/cli/sessions.db" "select count(*) from sessions")"
corpus_scrub "$OUT"
corpus_manifest "$OUT" \
  "harness=\"devin\"" "version=\"$V\"" "generated=\"$(date -u +%Y-%m-%d)\"" \
  "model=\"$MODEL (Devin backend, a Free model on the signed-in account; Z.ai cannot drive Devin)\"" \
  "model_calls=\"not observable (Devin's backend); $NSESS sessions, one devin -p per turn\"" \
  "scenarios=$(toml_list ${SCEN[@]+"${SCEN[@]}"})" \
  "skipped=$(toml_list ${SKIP[@]+"${SKIP[@]}"})" \
  "generator_commit=\"$(git -C "$HERE" rev-parse --short HEAD)\""
corpus_size_check "$OUT"
