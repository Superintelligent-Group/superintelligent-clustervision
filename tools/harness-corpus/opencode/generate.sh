#!/usr/bin/env bash
# Generate the OpenCode corpus for one installed version: a real opencode.db that OpenCode wrote
# itself, in a throwaway home, driven by a cheap real model (Z.ai GLM through ../key_proxy.py --translate chat).
#
#   tools/harness-corpus/opencode/generate.sh <version> <out-dir> [--keep] [--only "chat tools ..."]
#
# <out-dir> is normally crates/cv-core/tests/corpus/opencode/<version>; it must not exist yet (a
# committed version is never regenerated in place). Needs: install.sh <version> run first, python3,
# sqlite3, curl, git, ~/.zai-key. Env: ZAI_KEY_FILE (default ~/.zai-key), CORPUS_MODEL (default
# glm-5.3-flash), MAX_CALLS (default 60).
#
# The store lands at <out-dir>/.local/share/opencode/opencode.db (OpenCode's own data-dir layout).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$HERE/../lib.sh"

V="${1:?usage: generate.sh <version> <out-dir>}"; OUT="${2:?usage: generate.sh <version> <out-dir>}"; shift 2
KEEP=0
ONLY="chat tools todo image subagent background compact model mcp skill command plan shell fork webfetch error abort revert"
while [ $# -gt 0 ]; do
  case "$1" in --keep) KEEP=1 ;; --only) ONLY="$2"; shift ;; *) echo "unknown arg $1" >&2; exit 2 ;; esac; shift
done
mkdir -p "$(dirname "$OUT")"; OUT="$(cd "$(dirname "$OUT")" && pwd -P)/$(basename "$OUT")"
[ ! -e "$OUT" ] || { echo "refusing: $OUT exists (add a new version instead of regenerating)" >&2; exit 1; }
BIN="${HARNESS_VERSIONS:-$REAL_HOME/tools/harness-versions}/opencode/$V/node_modules/.bin/opencode"
[ -x "$BIN" ] || { echo "opencode $V not installed: run $HERE/install.sh $V" >&2; exit 1; }
for t in sqlite3 curl; do command -v $t >/dev/null || { echo "$t not on PATH" >&2; exit 1; }; done
MODEL="${CORPUS_MODEL:-glm-5.3-flash}"; MAX_CALLS="${MAX_CALLS:-60}"
UPSTREAM="https://api.z.ai/api/coding/paas/v4"
case "$V" in
  0.*|1.*) MAJOR=1; SESSION_TABLE=session ;;
  *) MAJOR=2; SESSION_TABLE=session_v2 ;;
esac

corpus_work
SERVE_PID=""
cleanup() {
  [ -z "$SERVE_PID" ] || kill "$SERVE_PID" 2>/dev/null || true
  [ -z "${apid:-}" ] || kill "$apid" 2>/dev/null || true
  corpus_proxy_stop
  [ "$KEEP" = 1 ] && echo "kept $WORK" || rm -rf "$WORK"
}
trap cleanup EXIT
export HOME="$CORPUS_HOME" TMPDIR="$WORK/tmp"
export XDG_DATA_HOME="$CORPUS_HOME/.local/share" XDG_CONFIG_HOME="$CORPUS_HOME/.config"
export XDG_STATE_HOME="$CORPUS_HOME/.local/state" XDG_CACHE_HOME="$CORPUS_HOME/.cache"
mkdir -p "$TMPDIR" "$XDG_CONFIG_HOME/opencode"
unset OPENCODE_DB OPENCODE_CONFIG OPENCODE_CONFIG_DIR OPENCODE_CONFIG_CONTENT OPENAI_API_KEY ANTHROPIC_API_KEY
export OPENCODE_DISABLE_AUTOUPDATE=1 OPENCODE_DISABLE_SHARE=1
corpus_guard HOME TMPDIR XDG_DATA_HOME XDG_CONFIG_HOME XDG_STATE_HOME XDG_CACHE_HOME
PROJECT="$CORPUS_HOME/project"
DB="$XDG_DATA_HOME/opencode/opencode.db"
# Only the proxy sees the real key; OpenCode gets a dummy one for a provider at 127.0.0.1:$PORT.
corpus_proxy "${ZAI_KEY_FILE:-$REAL_HOME/.zai-key}" chat "$UPSTREAM" --model "$MODEL"

# The scratch project: a git repo with a few small files, a skill and a custom command.
cd "$PROJECT"
printf 'alpha\nbeta\n' >a.txt; printf 'gamma\n' >b.txt; printf 'TODO: fix\n' >notes.txt
python3 -I -c 'import base64,sys; sys.stdout.buffer.write(base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg=="))' >dot.png
mkdir -p .opencode/skills/corpus-greeting .opencode/command
cat >.opencode/skills/corpus-greeting/SKILL.md <<'MD'
---
name: corpus-greeting
description: How to greet in the corpus project. Load it when asked to greet.
---
Greet with exactly the words: corpus says hi.
MD
cat >.opencode/command/shout.md <<'MD'
---
description: Repeat the argument in capitals
---
Reply with only this, in capital letters: $ARGUMENTS
MD
git init -q -b main . && git -c user.name=corpus -c user.email=corpus@example.invalid add -A \
  && git -c user.name=corpus -c user.email=corpus@example.invalid commit -qm init

# A v1 config (2.x normalizes it): one provider at the local proxy, three models — the real one, a
# second id for the model-switch scenario (the proxy answers both with $MODEL), and one the proxy
# always fails. (A tiny context limit does force auto-compaction, but 1.18 then loops: compact,
# synthetic "continue", overflow, compact... — so compaction goes through the API instead.)
cat >"$XDG_CONFIG_HOME/opencode/opencode.json" <<JSON
{
  "\$schema": "https://opencode.ai/config.json",
  "autoupdate": false,
  "share": "disabled",
  "model": "corpus/$MODEL",
  "small_model": "corpus/$MODEL",
  "permission": {"bash": {"*": "allow", "rm *": "deny"}, "edit": "allow", "webfetch": "allow", "external_directory": "deny"},
  "mcp": {"corpus": {"type": "local", "command": ["python3", "-I", "$HERE/../codex/mcp_server.py"], "enabled": true}},
  "provider": {
    "corpus": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "corpus proxy (Z.ai GLM)",
      "options": {"baseURL": "http://127.0.0.1:$PORT/v1", "apiKey": "corpus-local-proxy"},
      "models": {
        "$MODEL": {"name": "GLM", "tool_call": true, "reasoning": true, "attachment": true, "limit": {"context": 128000, "output": 8000}},
        "glm-alt": {"name": "GLM (second id)", "tool_call": true, "reasoning": true, "limit": {"context": 128000, "output": 8000}},
        "corpus-error": {"name": "always fails", "tool_call": true, "limit": {"context": 128000, "output": 8000}}
      }
    }
  }
}
JSON

# One OpenCode server for the whole run; every `run` attaches to it, and the affordances `run` has no
# flag for (compaction, a user shell command, revert, interrupt) go through its HTTP API. A server
# that outlives each `run` is also what lets 2.x background work finish and report back.
SPORT="$(python3 -I -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')"
URL="http://127.0.0.1:$SPORT"
AUTH=()
if [ "$MAJOR" = 2 ]; then
  # A loopback-only password for this throwaway server (2.x requires one); not a credential.
  export OPENCODE_PASSWORD="corpus-local-server"; AUTH=(-u "opencode:$OPENCODE_PASSWORD")
  ATTACH=(--server "$URL"); API=/api
else
  ATTACH=(--attach "$URL"); API=""
fi
"$BIN" serve --port "$SPORT" >>"$WORK/serve.log" 2>&1 &
SERVE_PID=$!
for _ in $(seq 150); do
  [ "$(curl -s -o /dev/null -w '%{http_code}' "${AUTH[@]}" "$URL$API/session")" = 200 ] && break; sleep 0.2
done
api() { # api METHOD PATH [JSON] -> body on stdout; non-2xx is reported, not fatal
  local m="$1" path="$2" body="${3:-}"
  if [ -n "$body" ]; then
    curl -s -X "$m" "${AUTH[@]}" -H 'content-type: application/json' -d "$body" "$URL$API$path" || echo "    (api $m $path failed)" >&2
  else
    curl -s -X "$m" "${AUTH[@]}" "$URL$API$path" || echo "    (api $m $path failed)" >&2
  fi
}
settle() { # wait until the session's loop is idle (2.x has an endpoint; 1.x `run` already blocks)
  [ "$MAJOR" = 2 ] && api POST "/experimental/session/$1/wait" >/dev/null || true
}

# --title everywhere: otherwise every new session spends one model call on a generated title.
run() {
  echo "--- opencode run $*" | cut -c1-140
  "$BIN" run "${ATTACH[@]}" "$@" </dev/null >>"$WORK/run.log" 2>&1 || echo "    (exit $?)"
}
last_id() { sqlite3 "$DB" "SELECT id FROM $SESSION_TABLE WHERE parent_id IS NULL ORDER BY time_created DESC LIMIT 1"; }
titled() { sqlite3 "$DB" "SELECT id FROM $SESSION_TABLE WHERE title = '$1' ORDER BY time_created LIMIT 1"; }
want() { case " $ONLY " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
SCEN=(); SKIP=()
skip() { SKIP+=("$1: $2"); }

if want chat; then   # 1 multi-turn chat + 9 resume (-s continues the same session) + 5 reasoning
  run --title chat "Say hello in five words."
  id="$(last_id)"
  run -s "$id" "Now say it in French."
  run -s "$id" "Which language did you use first? One word."
  SCEN+=("chat" "resume" "reasoning")
fi
if want tools; then  # 2 read/edit/shell, 3 parallel calls, 4 a tool error, approvals (a denied rule)
  run --title tools "Read a.txt and b.txt at the same time (two parallel read calls), then reply with their combined line count."
  id="$(last_id)"
  run -s "$id" "Use the edit tool to replace TODO with DONE in notes.txt, then run the shell command: cat notes.txt"
  run -s "$id" "Run the shell command: cat missing-file.txt   and tell me the error in one line."
  run -s "$id" "Run the shell command: rm b.txt   (a permission rule may deny it; report what happened in one line)."
  SCEN+=("tools" "parallel_tools" "tool_error" "permission_denied")
fi
if want todo; then
  if [ "$MAJOR" = 1 ]; then
    run --title todo "Use the todowrite tool to record two todos: 'read a.txt' and 'count lines'. Then do them, marking each completed with todowrite, and give the count."
    SCEN+=("todo")
  else skip todo "2.x has no todo tool (todowrite was removed; the todo table is gone)"; fi
fi
if want image; then  # 6 an image attachment (persisted with the prompt; the text model gets a placeholder)
  run --title image -f "$PROJECT/dot.png" -- "An image is attached. Do not use any tools. Reply with the single word: received."
  SCEN+=("image")
fi
if want subagent; then # 7 a sub-agent: a child session with parent_id
  if [ "$MAJOR" = 1 ]; then tool=task; else tool=subagent; fi
  run --title subagent "Use the $tool tool once with the general agent and the prompt 'reply with the word ok'. Then report what it said."
  SCEN+=("subagent")
fi
if want background; then
  if [ "$MAJOR" = 2 ]; then
    run --title background "Run the shell command 'sleep 1; echo bg-done' with background set to true. Do not poll; when you are notified that it finished, reply with its output."
    id="$(last_id)"; sleep 3; settle "$id"
    run -s "$id" "Use the subagent tool with background set to true, the general agent, and the prompt 'reply with the word ok'. When notified that it finished, report what it said."
    sleep 3; settle "$id"
    SCEN+=("background_shell" "background_subagent")
  else skip background "1.x has no background shell or background sub-agent"; fi
fi
if want compact; then # 8 a manual compaction through the API (what the TUI's /compact calls)
  run --title compact "List three prime numbers."
  id="$(last_id)"
  if [ "$MAJOR" = 2 ]; then api POST "/session/$id/compact" '{}' >/dev/null; sleep 1; settle "$id"
  else api POST "/session/$id/summarize" "{\"providerID\":\"corpus\",\"modelID\":\"$MODEL\",\"auto\":false}" >/dev/null; fi
  run -s "$id" "List three more."
  SCEN+=("compaction")
fi
if want model; then  # a model switch mid-session (a second model id at the same proxy)
  id="$(titled chat)"
  if [ -n "$id" ]; then run -s "$id" -m corpus/glm-alt "Say it once more in German."; SCEN+=("model_switch")
  else skip model "needs the chat scenario"; fi
fi
if want mcp; then    # 10 an MCP tool call through the stdio server shared with the codex generator
  run --title mcp "Use the corpus MCP server's add tool to add 2 and 3, then say the result."
  SCEN+=("mcp")
fi
if want skill; then
  run --title skill "Load the corpus-greeting skill with the skill tool, then greet me as it says."
  SCEN+=("skill")
fi
if want command; then # a custom command from .opencode/command/shout.md
  if [ "$MAJOR" = 1 ]; then run --title command --command shout "quiet words"; SCEN+=("custom_command")
  else
    # 2.0.25's `run` stores its message JSON-quoted ("\"/shout ...\""), so a /command is sent as plain
    # text; this run is kept as that record. The command API (POST /api/session/:id/command) is untried.
    run --title command "/shout quiet words"
    skip custom_command "2.x run sends /shout as quoted text, not a command; POST /api/session/:id/command not exercised"
  fi
fi
if want plan; then   # the plan agent, then back to build on the same session (an agent switch)
  run --title plan --agent plan "Plan, without editing anything, how you would rename b.txt to c.txt. Two lines."
  id="$(last_id)"
  run -s "$id" --agent build "Thanks. No changes needed; reply with the single word: ok."
  SCEN+=("plan_agent" "agent_switch")
fi
user_shell() { # a user shell command (the TUI's `!cmd`) on the tools session, no model call
  local id; id="$(titled tools)"
  if [ -n "$id" ]; then
    if [ "$MAJOR" = 2 ]; then api POST "/session/$id/shell" '{"command":"echo user-shell"}' >/dev/null; sleep 1; settle "$id"
    else api POST "/session/$id/shell" '{"agent":"build","command":"echo user-shell"}' >/dev/null; fi
    SCEN+=("user_shell")
  else skip shell "needs the tools scenario"; fi
}
# 1.x: here. 2.x: inside the revert block below, before the tools session's revert is staged.
if want shell && [ "$MAJOR" = 1 ]; then user_shell; fi
if want fork; then   # a fork of the chat session (a new session with its history)
  id="$(titled chat)"
  if [ -n "$id" ]; then run -s "$id" --fork "Say goodbye in five words."; SCEN+=("fork")
  else skip fork "needs the chat scenario"; fi
fi
if want webfetch; then
  run --title webfetch "Use the webfetch tool on https://example.com and tell me the page title only."
  SCEN+=("webfetch")
fi
if want error; then  # a provider error: the proxy answers model corpus-error with a 400
  run --title error -m corpus/corpus-error "This request will fail."
  SCEN+=("provider_error")
fi
if want abort; then  # an interrupt mid-turn, through the API (what Esc in the TUI calls)
  echo "--- opencode run (interrupted)"
  "$BIN" run "${ATTACH[@]}" --title abort "Run the shell command 'sleep 20' and then say done." </dev/null >>"$WORK/run.log" 2>&1 &
  apid=$!
  for _ in $(seq 60); do id="$(titled abort)"; [ -n "$id" ] && break; sleep 0.5; done
  sleep 8
  if [ "$MAJOR" = 2 ]; then api POST "/session/$id/interrupt" '{}' >/dev/null; else api POST "/session/$id/abort" >/dev/null; fi
  wait "$apid" 2>/dev/null || true
  SCEN+=("interrupt")
fi
last_user() { # the session's newest user prompt id
  if [ "$MAJOR" = 2 ]; then
    sqlite3 "$DB" "SELECT id FROM session_message WHERE session_id = '$1' AND type = 'user' ORDER BY seq DESC LIMIT 1"
  else
    sqlite3 "$DB" "SELECT id FROM message WHERE session_id = '$1' AND json_extract(data,'\$.role') = 'user' ORDER BY time_created DESC LIMIT 1"
  fi
}
stage() { api POST "/session/$1/revert/stage" "{\"messageID\":\"$(last_user "$1")\"}" >/dev/null; }
if want revert; then # undo to before a session's last prompt
  id="$(titled tools)"
  if [ -n "$id" ] && [ -n "$(last_user "$id")" ]; then
    if [ "$MAJOR" = 2 ]; then
      # Both 2.x modes: a staged revert only sets session_v2.revert (tools, after the user shell
      # command); a committed one deletes the rows after the boundary (plan loses its last turn).
      want shell && user_shell
      stage "$id"
      pid="$(titled plan)"; [ -z "$pid" ] || { stage "$pid"; api POST "/session/$pid/revert/commit" >/dev/null; }
    else api POST "/session/$id/revert" "{\"messageID\":\"$(last_user "$id")\"}" >/dev/null; fi
    SCEN+=("revert")
  else skip revert "needs the tools scenario"; fi
elif want shell && [ "$MAJOR" = 2 ]; then user_shell
fi

kill "$SERVE_PID" 2>/dev/null || true; wait "$SERVE_PID" 2>/dev/null || true; SERVE_PID=""
CALLS="$(corpus_calls)"
echo "model calls: $CALLS"

# Copy the store as OpenCode wrote it: opencode.db only (backup API, so the WAL is folded in). Not
# config, auth, logs, snapshots, the shell output dir, or caches. Two tables keep their schema but
# lose their rows (thin.py): the models.dev catalog 2.x caches in `kv` (~6 MB), and the `event`
# sync log (~400 KB here: a replay of the same message/part updates, which 2.x itself deletes when
# it migrates a 1.x store). cv reads neither.
mkdir -p "$OUT/.local/share/opencode"
python3 -I "$HERE/thin.py" "$DB" "$OUT/.local/share/opencode/opencode.db"
corpus_scrub "$OUT"
corpus_manifest "$OUT" \
  "harness=\"opencode\"" "version=\"$V\"" "generated=\"$(date -u +%Y-%m-%d)\"" \
  "model=\"$MODEL via Z.ai coding endpoint, through key_proxy.py --translate chat\"" \
  "model_calls=$CALLS" \
  "scenarios=[$(printf '"%s",' "${SCEN[@]}" | sed 's/,$//')]" \
  "skipped=[$( [ ${#SKIP[@]} -eq 0 ] || printf '"%s",' "${SKIP[@]}" | sed 's/,$//')]" \
  "thinned=[\"kv rows models-dev:*\",\"event rows\"]" \
  "generator_commit=\"$(git -C "$HERE" rev-parse --short HEAD)\""
corpus_size_check "$OUT"
