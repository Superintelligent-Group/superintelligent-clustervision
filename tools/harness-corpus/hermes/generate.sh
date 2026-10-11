#!/usr/bin/env bash
# Generate the Hermes corpus for one installed version: a real state.db Hermes wrote itself, in a
# throwaway HERMES_HOME, driven by a cheap real model (Z.ai GLM through chat_shim.py).
#
#   tools/harness-corpus/hermes/generate.sh <version> <out-dir> [--keep] [--only "chat tools ..."]
#
# <out-dir> is normally crates/cv-core/tests/corpus/hermes/<version>; it must not exist yet. Needs:
# install.sh <version> run first, python3, git, sqlite3, ~/.zai-key. Env: ZAI_KEY_FILE,
# CORPUS_MODEL (default glm-5.3-flash), SWITCH_MODEL (default glm-4.5-air), PORT (default 18811),
# MAX_CALLS (default 95).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$HERE/../lib.sh"

V="${1:?usage: generate.sh <version> <out-dir>}"; OUT="${2:?usage: generate.sh <version> <out-dir>}"; shift 2
KEEP=0
ONLY="chat tools affordances skills delegate image failure hooks import tty loop manage"
while [ $# -gt 0 ]; do
  case "$1" in --keep) KEEP=1 ;; --only) ONLY="$2"; shift ;; *) echo "unknown arg $1" >&2; exit 2 ;; esac; shift
done
mkdir -p "$(dirname "$OUT")"; OUT="$(cd "$(dirname "$OUT")" && pwd -P)/$(basename "$OUT")"
[ ! -e "$OUT" ] || { echo "refusing: $OUT exists (add a new version instead of regenerating)" >&2; exit 1; }
PREFIX="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/hermes/$V"
BIN="$PREFIX/venv/bin/hermes"
[ -x "$BIN" ] || { echo "hermes $V not installed: run $HERE/install.sh $V" >&2; exit 1; }
for t in python3 git sqlite3; do command -v "$t" >/dev/null || { echo "$t not on PATH" >&2; exit 1; }; done
MODEL="${CORPUS_MODEL:-glm-5.3-flash}"; SWITCH="${SWITCH_MODEL:-glm-4.5-air}"; PORT="${PORT:-18811}"
UPSTREAM="https://api.z.ai/api/coding/paas/v4"
(exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && { echo "port $PORT is taken; set PORT" >&2; exit 1; }

corpus_work
SHIM_PID=""
cleanup() { [ -z "$SHIM_PID" ] || kill "$SHIM_PID" 2>/dev/null || true; [ "$KEEP" = 1 ] && echo "kept $WORK" || rm -rf "$WORK"; }
trap cleanup EXIT
# HERMES_HOME is the critical one: unset, Hermes warns and falls back to ~/.hermes.
export HOME="$CORPUS_HOME" HERMES_HOME="$CORPUS_HOME/.hermes" TMPDIR="$WORK/tmp"
mkdir -p "$TMPDIR" "$HERMES_HOME" "$WORK/bin"
for v in XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME OPENAI_API_KEY OPENAI_BASE_URL \
  OPENROUTER_API_KEY ANTHROPIC_API_KEY GLM_API_KEY ZAI_API_KEY HERMES_PROFILE; do unset "$v"; done
corpus_guard HOME HERMES_HOME TMPDIR
PROJECT="$CORPUS_HOME/project"
# Helper scripts live inside $WORK so any path Hermes records scrubs to /home/corpus/.work.
cp "$HERE/hook.py" "$HERE/../codex/mcp_server.py" "$WORK/bin/"

cd "$PROJECT"
printf 'alpha\nbeta\n' >a.txt; printf 'gamma\n' >b.txt; printf 'TODO: fix\n' >notes.txt
python3 -I -c 'import base64,sys; sys.stdout.buffer.write(base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg=="))' >dot.png
git init -q -b main . && git -c user.name=corpus -c user.email=corpus@example.invalid add -A \
  && git -c user.name=corpus -c user.email=corpus@example.invalid commit -qm init

# The dummy key is the shim's; the real one never reaches Hermes or its config.
cat >"$HERMES_HOME/config.yaml" <<YAML
model:
  provider: custom
  base_url: http://127.0.0.1:$PORT/v1
  api_key: corpus-local-shim
  default: $MODEL
  context_length: 131072
  supports_vision: true
agent:
  max_turns: 12
delegation:
  max_iterations: 6
  subagent_auto_approve: true
compression:
  enabled: true
  # Summarize everything but the last two messages: with the default 20-message tail a short
  # session has nothing to compress and /compress refuses ("summary would grow").
  protect_first_n: 0
  protect_last_n: 2
updates:
  check_on_startup: false
telemetry:
  enabled: false
  shared_metrics: {enabled: false, send: false, offer_version: 2}
approvals:
  destructive_slash_confirm: false
hooks_auto_accept: true
hooks:
  pre_tool_call:
    - matcher: "terminal"
      command: "python3 -I $WORK/bin/hook.py"
      timeout: 10
  pre_llm_call:
    - command: "python3 -I $WORK/bin/hook.py"
      timeout: 10
mcp_servers:
  corpus:
    command: python3
    args: ["-I", "$WORK/bin/mcp_server.py"]
YAML
# The hooks are only enabled for the hooks scenario; the rest of the run uses a copy without them.
python3 -I - "$HERMES_HOME/config.yaml" <<'PY'
import re, sys
p = sys.argv[1]; s = open(p).read()
open(p + ".hooks", "w").write(s)
open(p, "w").write(re.sub(r"hooks:\n(?:  .*\n|    .*\n|      .*\n)+", "", s))
PY

corpus_load_key UPSTREAM_KEY "${ZAI_KEY_FILE:-$REAL_HOME/.zai-key}"
python3 -I "$HERE/chat_shim.py" --port "$PORT" --upstream "$UPSTREAM" --model "$MODEL" --also "$SWITCH" \
  --log "$WORK/calls.jsonl" --max-calls "${MAX_CALLS:-95}" &
SHIM_PID=$!
unset UPSTREAM_KEY
for _ in $(seq 50); do (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && break; sleep 0.1; done
touch "$WORK/calls.jsonl"

LAST=""
# Every step must leave the throwaway tree intact; stop at the first one that does not.
intact() {
  [ -f "$WORK/calls.jsonl" ] && [ -d "$PROJECT" ] && [ -f "$HERMES_HOME/config.yaml" ] \
    || { echo "refusing to continue: the throwaway tree was damaged by the last step ($*)" >&2; exit 1; }
}
# One headless turn (-Q: answer and exit). Sets LAST to the session id Hermes printed (on stderr).
hq() {
  echo "--- hermes chat $*" | cut -c1-150
  if [ "$1" = --resume ] && [ -z "$2" ]; then echo "    (skipped: no session to resume)"; return; fi
  local out
  out="$(cd "$PROJECT" && "$BIN" chat -Q --yolo "$@" </dev/null 2>&1)" || echo "    (exit $?)"
  printf '%s\n' "$out" >>"$WORK/exec.log"
  LAST="$(printf '%s\n' "$out" | sed -n 's/^session_id: *//p' | tail -1)"
  [ -n "$LAST" ] || echo "    (no session id printed)"
  intact "$1"
}
# An interactive session through a pty: each -l line is typed once the CLI goes quiet.
htty() {
  echo "--- hermes chat --cli (tty) $*" | cut -c1-150
  (cd "$PROJECT" && python3 -I "$HERE/drive_tty.py" --log "$WORK/tty.log" --paste --idle 8 --min 3 --timeout 600 \
    --watch "$WORK/calls.jsonl" --watch "$PROJECT/a.txt" "$@") || echo "    (exit $?)"
  intact tty
}
hermes_cmd() { (cd "$PROJECT" && "$BIN" "$@" </dev/null >>"$WORK/exec.log" 2>&1) || echo "    ($1 $2: exit $?)"; intact "$1"; }
want() { case " $ONLY " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
SCEN=(); SKIP=()

if want chat; then      # 1 multi-turn chat + 9 resume; reasoning on every turn (GLM thinking)
  hq -q "Say hello in five words."
  id="$LAST"
  hq --resume "$id" -q "Now say it in French."
  hq --resume "$id" -q "Which language did you use first? One word."
  CHAT_ID="$id"
  SCEN+=("chat" "resume" "reasoning")
fi
if want tools; then     # 2 read/patch/terminal, 3 parallel calls, 4 a tool error
  hq -q "Read a.txt and b.txt at the same time (two parallel read_file calls), then reply with their combined line count."
  id="$LAST"
  hq --resume "$id" -q "Use the patch tool to replace TODO with DONE in notes.txt, then run: cat notes.txt"
  hq --resume "$id" -q "Run: cat missing-file.txt   and tell me the error in one line."
  TOOLS_ID="$id"
  SCEN+=("tools" "parallel_tools" "tool_error")
fi
if want affordances; then # one session, one tool family per turn (no extra title calls)
  hq -q "Use the todo tool to record a two-item plan (1: read a.txt, 2: summarize it) and mark item 1 done. Then use the memory tool to remember: corpus prefers brevity. Reply ok."
  id="$LAST"
  hq --resume "$id" -q "Use the cronjob tool to create a job named corpus-ping that runs every day at 09:00 with the prompt 'say ok'. Reply with its id."
  hq --resume "$id" -q "Use the execute_code tool to compute 2**10 in Python and tell me the result."
  hq --resume "$id" -q "Start this with the terminal tool in the background: sleep 2; echo bg-done   Then use the process tool to wait for it and tell me its output."
  hq --resume "$id" -q "Use the session_search tool to find the session where we said hello in French. One line."
  SCEN+=("todo" "memory" "cronjob" "execute_code" "background_process" "session_search")
fi
if want skills; then    # skill_manage create, then a session that preloads the skill (-s)
  hq -q "Use the skill_manage tool (load it with tool_search first if it is not in your tool list) to create a skill named corpus-hello whose instructions are: greet the user with the word ahoy. Reply ok."
  hq -s corpus-hello -q "Greet me."
  SCEN+=("skill_manage" "skill_preload")
fi
if want delegate; then  # 7 sub-agents: a synchronous delegate_task (the background one is in the loop tty)
  hq -q "Use delegate_task to give one subagent the goal 'reply with the word ok'. Then tell me what it said."
  SCEN+=("subagent")
fi
if want image; then     # 6 image input (image_url parts; the shim gives the model a placeholder), then 10 MCP
  hq --image "$PROJECT/dot.png" -q "An image is attached. Do not use any tools. Reply with the single word: received."
  hq --resume "$LAST" -q "Use the add tool from the corpus MCP server (find it with tool_search if it is not in your tool list) to add 2 and 3, then say the result."
  SCEN+=("image" "mcp")
fi
if want failure; then   # a provider error: the shim answers 500 to this prompt without calling upstream
  hq -q "CORPUS-PROVIDER-ERROR: say ok."
  SCEN+=("provider_error")
fi
if want hooks; then     # shell hooks: pre_llm_call context injection and a pre_tool_call block
  cp "$HERMES_HOME/config.yaml" "$WORK/config.plain"; cp "$HERMES_HOME/config.yaml.hooks" "$HERMES_HOME/config.yaml"
  hq --accept-hooks -q "Run this with the terminal tool: echo forbidden-word   Then tell me what happened."
  cp "$WORK/config.plain" "$HERMES_HOME/config.yaml"
  SCEN+=("hooks")
fi
if want import; then    # a foreign import (no model call): cv's own Claude Code fixture
  hermes_cmd sessions import "$HERE/../../../crates/cv-core/tests/fixtures/claude/rich_blocks.jsonl"
  SCEN+=("import")
fi
if want tty; then       # slash commands only the interactive CLI dispatches
  htty -l "Review @file:a.txt and reply with its first word only." \
       -l "/model $SWITCH" -l "Say the word switched." \
       -l "/model $MODEL" \
       -l "/title corpus interactive" \
       -l "Say the word one." -l "/retry" -l "Say the word two." -l "/undo" \
       -l "/queue Say the word queued." -l "Say the word three." \
       -l "Without using tools, write about 3000 words on the history of the number zero." \
       -l "Without using tools, write about 3000 words on the history of the abacus." \
       -l "Say the word five." \
       -l "/compress here 1" -l "Say the word four." \
       -l "/branch" -l "Say the word branched." \
       -l "/exit" -- "$BIN" chat --cli --yolo
  SCEN+=("context_reference" "model_switch" "title" "retry" "undo" "queue" "compaction" "branch")
fi
if want loop; then      # timers and goals: /goal (judge-driven continuation), /loop, /heartbeat, /bg, /btw
  htty -l "/goal Reply with the word done." \
       -l "/loop 1m Say the word tick." -l "/loop stop" \
       -l "/heartbeat every 10m Say the word beat." -l "/heartbeat off" \
       -l "/btw What is 2+2? One word." \
       -l "/bg Say the word background." \
       -l "Use delegate_task with background=true to give one subagent the goal 'reply with the word later'. Do not wait for it; just say started." \
       -l "Did the background subagent finish? One line." \
       -l "/exit" -- "$BIN" chat --cli --yolo
  SCEN+=("goal" "loop" "heartbeat" "btw" "bg" "background_subagent")
fi
if want manage; then    # store-side affordances with no model call
  [ -z "${CHAT_ID:-}" ] || { hermes_cmd sessions pin "$CHAT_ID"; hermes_cmd sessions rename "$CHAT_ID" "corpus chat"; }
  # Archive matches ended sessions by filter: give the tools session a known title, then archive it.
  [ -z "${TOOLS_ID:-}" ] || { hermes_cmd sessions rename "$TOOLS_ID" "corpus tools"; hermes_cmd sessions archive --title "corpus tools" --yes; }
  SCEN+=("pin" "rename" "archive")
fi
CALLS="$(grep -c '"refused": false' "$WORK/calls.jsonl" || true)"
INJECTED="$(grep -c '"injected": true' "$WORK/calls.jsonl" || true)"
echo "model calls: $CALLS"
grep -q '"refused": true' "$WORK/calls.jsonl" && echo "warning: the shim refused calls past MAX_CALLS" >&2

# Copy the store as Hermes wrote it: state.db through the backup API (WAL holds the recent rows).
# Not config, .env, auth, logs, caches, cron/, memories/ or skills/ (cv reads none of them).
mkdir -p "$OUT/.hermes"
python3 -I - "$HERMES_HOME/state.db" "$OUT/.hermes/state.db" <<'PY'
import sqlite3, sys
src = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True); dst = sqlite3.connect(sys.argv[2])
src.backup(dst); dst.close(); src.close()
PY
for d in "$HERMES_HOME"/profiles/*/; do
  [ -f "$d/state.db" ] || continue
  n="$(basename "$d")"; mkdir -p "$OUT/.hermes/profiles/$n"
  python3 -I -c 'import sqlite3,sys; s=sqlite3.connect(sys.argv[1]); d=sqlite3.connect(sys.argv[2]); s.backup(d)' "$d/state.db" "$OUT/.hermes/profiles/$n/state.db"
done
corpus_scrub "$OUT"
# Empty the FTS5 indexes (a derived search index; `INSERT INTO t(t) VALUES('rebuild')` restores it from
# the messages). The trigram index alone is ~4x the text it indexes and would push the store past
# the 1 MB cap. Tables, schema and every message row stay exactly as Hermes wrote them.
find "$OUT" -name state.db | while read -r db; do
  python3 -I - "$db" <<'PY'
import re, sqlite3, sys
con = sqlite3.connect(sys.argv[1])
for name, sql in con.execute("SELECT name, sql FROM sqlite_master WHERE type='table'").fetchall():
    if re.search(r"USING\s+fts5", sql or "", re.I):
        con.execute(f'INSERT INTO "{name}"("{name}") VALUES (\'delete-all\')')
con.commit(); con.execute("VACUUM"); con.close()
PY
done
COMMIT="$(cat "$PREFIX/COMMIT" 2>/dev/null || echo unknown)"
corpus_manifest "$OUT" \
  "harness=\"hermes\"" "version=\"$V\"" "upstream_commit=\"$COMMIT\"" "generated=\"$(date -u +%Y-%m-%d)\"" \
  "model=\"$MODEL (switch: $SWITCH) via Z.ai coding endpoint, through chat_shim.py\"" \
  "model_calls=$CALLS" "injected_provider_errors=$INJECTED" \
  "scenarios=[$(printf '"%s",' "${SCEN[@]}" | sed 's/,$//')]" \
  "skipped=[$( [ ${#SKIP[@]} -eq 0 ] || printf '"%s",' "${SKIP[@]}" | sed 's/,$//')]" \
  "generator_commit=\"$(git -C "$HERE" rev-parse --short HEAD)\""
corpus_size_check "$OUT"
