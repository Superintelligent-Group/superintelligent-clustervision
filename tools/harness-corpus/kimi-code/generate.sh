#!/usr/bin/env bash
# Generate the Kimi Code corpus for one installed version: real sessions Kimi Code wrote itself, in a
# throwaway home, driven by Kimi's coding model through ../key_proxy.py (which alone holds the key).
#
#   tools/harness-corpus/kimi-code/generate.sh <version> <out-dir> [--keep] [--only "chat tools ..."]
#
# <out-dir> is normally crates/cv-core/tests/corpus/kimi-code/<version>; it must not exist yet.
# Needs: install.sh <version> run first, python3, git, ~/.kimikey (a Kimi Code membership key for
# https://api.kimi.com/coding/v1). Env: KIMI_KEY_FILE, CORPUS_MODEL (default kimi-for-coding),
# MAX_CALLS (default 60: the affordance scenarios need more than the base 40).
#
# --home DIR runs against an existing throwaway home instead of a fresh one (the migration capture
# uses it: a copy of an older version's home, opened by a newer version).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$HERE/../lib.sh"

V="${1:?usage: generate.sh <version> <out-dir>}"; OUT="${2:?usage: generate.sh <version> <out-dir>}"; shift 2
KEEP=0
ALL="chat tools image todo plan background cron goal skill mcp fetch subagent swarm compact tui fork"
ONLY="$ALL"
while [ $# -gt 0 ]; do
  case "$1" in --keep) KEEP=1 ;; --only) ONLY="$2"; shift ;; *) echo "unknown arg $1" >&2; exit 2 ;; esac; shift
done
mkdir -p "$(dirname "$OUT")"; OUT="$(cd "$(dirname "$OUT")" && pwd -P)/$(basename "$OUT")"
[ ! -e "$OUT" ] || { echo "refusing: $OUT exists (add a new version instead of regenerating)" >&2; exit 1; }
BIN="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/kimi-code/$V/node_modules/.bin/kimi"
[ -x "$BIN" ] || { echo "kimi-code $V not installed: run $HERE/install.sh $V" >&2; exit 1; }
MODEL="${CORPUS_MODEL:-kimi-for-coding}"
UPSTREAM="https://api.kimi.com/coding/v1"

corpus_work
cleanup() { corpus_proxy_stop; [ "$KEEP" = 1 ] && echo "kept $WORK" || rm -rf "$WORK"; }
trap cleanup EXIT
export HOME="$CORPUS_HOME" KIMI_CODE_HOME="$CORPUS_HOME/.kimi-code" TMPDIR="$WORK/tmp"
mkdir -p "$TMPDIR" "$KIMI_CODE_HOME"
unset XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME KIMI_API_KEY KIMI_BASE_URL KIMI_SHARE_DIR \
  OPENAI_API_KEY ANTHROPIC_API_KEY
corpus_guard HOME KIMI_CODE_HOME TMPDIR
PROJECT="$CORPUS_HOME/project"

cd "$PROJECT"
printf 'alpha\nbeta\n' >a.txt; printf 'gamma\n' >b.txt; printf 'TODO: fix\n' >notes.txt
python3 -I -c 'import base64,sys; sys.stdout.buffer.write(base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg=="))' >dot.png
printf '# Project notes\nThis is a tiny test repository. Keep answers short.\n' >AGENTS.md
git init -q -b main . && git -c user.name=corpus -c user.email=corpus@example.invalid add -A \
  && git -c user.name=corpus -c user.email=corpus@example.invalid commit -qm init

# Hooks (recorded as hook results), a user skill, and the stdio MCP server (copied into the
# throwaway dir so no repo path reaches the store).
cp "$HERE/../codex/mcp_server.py" "$WORK/mcp_server.py"
cat >"$KIMI_CODE_HOME/config.toml" <<TOML
[[hooks]]
event = "PostToolUse"
matcher = "Bash"
command = "echo corpus-hook-saw-bash"

[[hooks]]
event = "UserPromptSubmit"
command = "echo corpus-hook-saw-prompt"
TOML
cat >"$KIMI_CODE_HOME/mcp.json" <<JSON
{"mcpServers": {"corpus": {"command": "python3", "args": ["-I", "$WORK/mcp_server.py"]}}}
JSON
mkdir -p "$KIMI_CODE_HOME/skills/greet"
cat >"$KIMI_CODE_HOME/skills/greet/SKILL.md" <<'MD'
---
name: greet
description: Greet the user in the corpus house style. Use when asked to greet.
---
Reply with exactly: "Greetings from the corpus skill."
MD

# Only the proxy sees the real key; Kimi Code gets a dummy key for a provider on 127.0.0.1.
MAX_CALLS="${MAX_CALLS:-60}"
corpus_proxy "${KIMI_KEY_FILE:-$REAL_HOME/.kimikey}" passthrough "$UPSTREAM"
export KIMI_MODEL_NAME="$MODEL" KIMI_MODEL_API_KEY="corpus-local-proxy" KIMI_MODEL_BASE_URL="http://127.0.0.1:$PORT/v1" \
  KIMI_CODE_NO_AUTO_UPDATE=1

LAST=""
run() {
  echo "--- kimi $*" | cut -c1-140
  "$BIN" "$@" </dev/null >"$WORK/run.out" 2>&1 || echo "    (exit $?)"
  cat "$WORK/run.out" >>"$WORK/exec.log"
  LAST="$(grep -oE 'session_[0-9a-f-]{36}' "$WORK/run.out" | tail -1 || true)"
  [ -n "$LAST" ] || echo "    (no session id in output)"
}
want() { case " $ONLY " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
SCEN=(); SKIP=()
# Every new session costs a ~70 KB `llm.tools_snapshot` record (55 KB per sub-agent), so the
# single-agent scenarios are chained on ONE session by resume (-S); that is also what a long real
# session looks like. Prompt mode runs in the "auto" permission mode: no approvals, no questions.
A=""
on_a() { if [ -z "$A" ]; then run -p "$1"; A="$LAST"; else run -S "$A" -p "$1"; fi; }

if want chat; then      # 1 multi-turn chat + 9 resume
  on_a "Say hello in five words."
  on_a "Now say it in French."
  on_a "Which language did you use first? One word."
  SCEN+=("chat" "resume")
fi
if want tools; then     # 2 read/edit/shell, 3 parallel, 4 tool error, 5 thinking (every turn)
  on_a "Read a.txt and b.txt at the same time (two parallel Read calls), then reply with their combined line count."
  on_a "Use Edit to replace TODO with DONE in notes.txt, then run with Bash: cat notes.txt"
  on_a "Run with Bash: cat missing-file.txt   and tell me the error in one line."
  SCEN+=("tools" "parallel_tools" "tool_error" "thinking" "hooks")
fi
if want image; then     # 6 image input: ReadMediaFile returns the image as a part
  on_a "Use ReadMediaFile on dot.png and tell me its main colour in one word."
  SCEN+=("image")
fi
if want todo; then      # planning: the TodoList tool
  on_a "Use the TodoList tool to record three todos: read a.txt, read b.txt, summarize. Then mark the first one done with TodoList. Do nothing else."
  SCEN+=("todo")
fi
if want plan; then      # plan mode through its tools (--plan cannot be combined with -p)
  on_a "Call EnterPlanMode. Then plan (do not execute) how to rename b.txt to c.txt in two bullet points, and call ExitPlanMode with that plan. Do not rename anything."
  SCEN+=("plan_mode")
fi
if want background; then # a background shell and its completion
  on_a "Start this with Bash in the background (run_in_background): sleep 2; echo bg-done   Then use TaskOutput to wait for it and report its output in one line."
  SCEN+=("background_task")
fi
if want cron; then      # schedules: CronCreate / CronList / CronDelete
  on_a "Use CronCreate to schedule the prompt 'say tick' every day at 09:00. Then use CronList, then CronDelete that job. Reply with the job id."
  SCEN+=("cron")
fi
if want goal; then      # goals: CreateGoal / UpdateGoal
  on_a "Use CreateGoal with the objective 'count the txt files'. Then count them with Glob, then use UpdateGoal to mark the goal complete. Reply with the count."
  SCEN+=("goal")
fi
if want skill; then     # a user skill through the Skill tool
  on_a "Use the greet skill."
  SCEN+=("skill")
fi
if want mcp; then       # 10 MCP tool call through the stdio server
  on_a "Use the corpus MCP server's add tool to add 2 and 3, then say the result."
  SCEN+=("mcp")
fi
if want fetch; then     # web fetch
  on_a "Use FetchURL on https://example.com and give me the page title in one line."
  SCEN+=("fetch_url")
fi
if want subagent; then  # 7 sub-agents: the Agent tool, then AgentSwarm (several at once)
  on_a "Use the Agent tool once to spawn a sub-agent with the task 'reply with the word ok'. Then report what it said."
  SCEN+=("subagent")
  if want swarm; then
    on_a "Use AgentSwarm with two tiny tasks: 'reply with the word one' and 'reply with the word two'. Report both answers."
    SCEN+=("swarm")
  fi
fi
if want compact; then   # 8 compaction: prompt mode ignores /compact, so shrink the window for one turn.
  # Auto-compaction fires when used + reservedContextSize (50k) >= the window; 60k leaves ~10k.
  KIMI_MODEL_MAX_CONTEXT_SIZE=60000 on_a "List three prime numbers."
  on_a "List three more."
  SCEN+=("compaction")
fi
C=""
if want tui; then       # interactive-only affordances, through a pty: /undo, an interrupt (Esc), /clear.
  # The model URL points at a closed port for this run, so the interrupted turn records the
  # harness's API-error retries and no model call is made.
  run -p "Say ok."; C="$LAST"
  echo "--- kimi -S $C (tui: /undo, Say hi. + Esc, /clear)"
  (cd "$PROJECT" && TERM=xterm-256color TUI_SCRIPT="$HERE/tui.json" KIMI_MODEL_BASE_URL="http://127.0.0.1:9/v1" \
    python3 -I "$HERE/../tui_driver.py" "$WORK/tui.log" "$BIN" -S "$C") || echo "    (tui exit $?)"
  SCEN+=("undo" "interrupt" "api_retry" "clear")
fi
if want fork; then      # fork (2.x, no model call) of the small session
  if "$BIN" --help 2>/dev/null | grep -qE '^  fork '; then
    [ -n "$C" ] || { run -p "Say ok."; C="$LAST"; }
    echo "--- kimi fork $C"
    "$BIN" fork "$C" </dev/null >>"$WORK/exec.log" 2>&1 || echo "    (exit $?)"
    SCEN+=("fork")
  else
    SKIP+=("fork: no kimi fork command in $V")
  fi
fi
SKIP+=("ask_user: prompt mode runs in auto permission mode, which withholds AskUserQuestion"
  "approvals: auto permission mode never prompts"
  "model_switch: one env-defined model (KIMI_MODEL_NAME); a second needs a config.toml provider with the key in it")

CALLS="$(corpus_calls)"
echo "model calls: $CALLS"

# Copy what Kimi Code wrote: the session dirs (transcripts, state, tool-result/task/media sidecars),
# the session index and the workspace registry. Not config, mcp.json, skills, device_id, logs,
# caches, or the sessions/.index-dirty and .index-cache bookkeeping.
mkdir -p "$OUT/.kimi-code"
(cd "$KIMI_CODE_HOME" && find sessions -path '*/logs' -prune -o -path 'sessions/.index-*' -prune -o -type f -print \
  | while IFS= read -r f; do mkdir -p "$OUT/.kimi-code/$(dirname "$f")"; cp "$f" "$OUT/.kimi-code/$f"; done)
for f in session_index.jsonl workspaces.json; do
  [ -e "$KIMI_CODE_HOME/$f" ] && cp "$KIMI_CODE_HOME/$f" "$OUT/.kimi-code/"
done
corpus_scrub "$OUT"
corpus_manifest "$OUT" \
  "harness=\"kimi-code\"" "version=\"$V\"" "generated=\"$(date -u +%Y-%m-%d)\"" \
  "model=\"$MODEL via api.kimi.com/coding/v1, through key_proxy.py\"" \
  "model_calls=$CALLS" \
  "scenarios=[$(printf '"%s",' "${SCEN[@]}" | sed 's/,$//')]" \
  "skipped=[$(printf '"%s",' "${SKIP[@]}" | sed 's/,$//')]" \
  "generator_commit=\"$(git -C "$HERE" rev-parse --short HEAD)\""
corpus_size_check "$OUT"
