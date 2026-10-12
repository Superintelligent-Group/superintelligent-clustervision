#!/usr/bin/env bash
# Generate the Grok CLI corpus for one installed version: real sessions Grok wrote itself, in a
# throwaway GROK_HOME, driven by a cheap real model (Z.ai GLM as a BYOK `[model.*]` entry, through
# ../key_proxy.py, which alone holds the key). No xAI login is used or copied.
#
#   tools/harness-corpus/grok/generate.sh <version> <out-dir> [--keep] [--only "chat tools ..."]
#
# <out-dir> is normally crates/cv-core/tests/corpus/grok/<version>; it must not exist yet. Needs:
# install.sh <version> run first, python3, git, uuidgen, ~/.zai-key. Env: ZAI_KEY_FILE,
# CORPUS_MODEL (default glm-5.3-flash), CORPUS_MODEL_ALT (default glm-5.3), MAX_CALLS (default 85). Set TMPDIR to your own scratch dir: the throwaway home goes there.
#
# Every scenario is listed in AFFORDANCES.md with the record it is meant to leave.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$HERE/../lib.sh"

V="${1:?usage: generate.sh <version> <out-dir>}"; OUT="${2:?usage: generate.sh <version> <out-dir>}"; shift 2
KEEP=0
ONLY="chat tools image todo plan agents background schedule mcp skill workflow interrupt"
while [ $# -gt 0 ]; do
  case "$1" in --keep) KEEP=1 ;; --only) ONLY="$2"; shift ;; *) echo "unknown arg $1" >&2; exit 2 ;; esac; shift
done
mkdir -p "$(dirname "$OUT")"; OUT="$(cd "$(dirname "$OUT")" && pwd -P)/$(basename "$OUT")"
[ ! -e "$OUT" ] || { echo "refusing: $OUT exists (add a new version instead of regenerating)" >&2; exit 1; }
BIN="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/grok/$V/grok"
[ -x "$BIN" ] || { echo "grok $V not installed: run $HERE/install.sh $V" >&2; exit 1; }
MODEL="${CORPUS_MODEL:-glm-5.3-flash}"; ALT="${CORPUS_MODEL_ALT:-glm-5.3}"; MAX_CALLS="${MAX_CALLS:-85}"
UPSTREAM="https://api.z.ai/api/coding/paas/v4"

corpus_work
cleanup() { corpus_proxy_stop; [ "$KEEP" = 1 ] && echo "kept $WORK" || rm -rf "$WORK"; }
trap cleanup EXIT
export HOME="$CORPUS_HOME" GROK_HOME="$CORPUS_HOME/.grok" TMPDIR="$WORK/tmp"
mkdir -p "$TMPDIR" "$GROK_HOME"
corpus_guard HOME GROK_HOME TMPDIR
PROJECT="$CORPUS_HOME/project"
# Only the proxy sees the real key; Grok gets a dummy one for its BYOK models at $PORT.
corpus_proxy "${ZAI_KEY_FILE:-$REAL_HOME/.zai-key}" passthrough "$UPSTREAM"

# The scratch project: a git repo with a few small files, an AGENTS.md, a saved workflow.
cd "$PROJECT"
printf 'alpha\nbeta\n' >a.txt; printf 'gamma\n' >b.txt; printf 'TODO: fix\n' >notes.txt
printf '# Project rules\n\nThis is a corpus test repository. Keep answers short.\n' >AGENTS.md
python3 -I -c 'import base64,sys; sys.stdout.buffer.write(base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg=="))' >dot.png
mkdir -p .grok/workflows
cat >.grok/workflows/corpus-hello.rhai <<'RHAI'
let meta = #{
    name: "corpus-hello",
    description: "Two agents each reply with one word",
    phases: [ #{ title: "Say", detail: "Each agent replies with one word" } ],
};
phase("Say");
let a = agent("Reply with the single word ok. Do not use any tools.", #{ label: "say-ok", capability_mode: "read-only" });
let b = agent("Reply with the single word yes. Do not use any tools.", #{ label: "say-yes", capability_mode: "read-only" });
#{ ok: a.success, yes: b.success }
RHAI
git init -q -b main . && git -c user.name=corpus -c user.email=corpus@example.invalid add -A \
  && git -c user.name=corpus -c user.email=corpus@example.invalid commit -qm init

# User config in the throwaway GROK_HOME: the BYOK models, hooks, an MCP server, a skill.
cat >"$GROK_HOME/config.toml" <<TOML
[cli]
auto_update = false

[features]
telemetry = false
feedback = false
codebase_indexing = false

[models]
default = "corpus-glm"
session_summary = "corpus-glm"

[model.corpus-glm]
model = "$MODEL"
base_url = "http://127.0.0.1:$PORT"
name = "GLM (corpus)"
env_key = "CORPUS_PROXY_KEY"
context_window = 128000

[model.corpus-glm-alt]
model = "$ALT"
base_url = "http://127.0.0.1:$PORT"
name = "GLM alt (corpus)"
env_key = "CORPUS_PROXY_KEY"
context_window = 128000

[toolset.bash]
output_byte_limit = 2000

[mcp_servers.corpus]
command = "python3"
args = ["-I", "$HERE/../codex/mcp_server.py"]

[[hooks.PreToolUse]]
matcher = "run_terminal_cmd"
  [[hooks.PreToolUse.hooks]]
  type = "command"
  command = "true"

[[hooks.PostToolUse]]
matcher = "search_replace"
  [[hooks.PostToolUse.hooks]]
  type = "command"
  command = "echo corpus post-edit hook"

[[hooks.SessionStart]]
  [[hooks.SessionStart.hooks]]
  type = "command"
  command = "echo corpus session start hook"
TOML
mkdir -p "$GROK_HOME/skills/corpus-greet"
cat >"$GROK_HOME/skills/corpus-greet/SKILL.md" <<'MD'
---
name: corpus-greet
description: Greet the user in exactly three words. Use when asked for a corpus greeting.
---
Reply with exactly three words of greeting and nothing else. Do not use any tools.
MD

GENV=(PATH=/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin HOME="$HOME" GROK_HOME="$GROK_HOME" TMPDIR="$TMPDIR"
  TERM=dumb LANG=en_US.UTF-8 TZ=UTC CORPUS_PROXY_KEY=corpus-local-proxy GROK_TELEMETRY_ENABLED=0 RUST_LOG=off
  GROK_TURN_SUMMARY=0 GROK_SESSION_RECAP=0 GROK_TITLE_REFRESH=0)
# Per-turn summaries, the session recap and title refreshes are each a model call per turn; they run in
# the chat session only (SUMS), so summary.json shows their fields once without spending the budget.
SUMS=(GROK_TURN_SUMMARY=1 GROK_SESSION_RECAP=1 GROK_TITLE_REFRESH=1)

# gk [VAR=val ...] -- <grok args>: one headless `grok -p` run in the project.
gk() {
  local extra=()
  while [ "$1" != "--" ]; do extra+=("$1"); shift; done; shift
  echo "--- grok $*" | cut -c1-150
  (cd "$PROJECT" && env -i "${GENV[@]}" ${extra[@]+"${extra[@]}"} "$BIN" --output-format json --always-approve "$@" \
    </dev/null >>"$WORK/run.log" 2>&1) || echo "    (exit $?)"
}
newid() { uuidgen | tr 'A-Z' 'a-z'; }
want() { case " $ONLY " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
SCEN=(); SKIP=()
# --tools takes internal ids (run_terminal_cmd, read_file, search_replace, list_dir, grep, todo_write,
# task, ...). Any allowlist also drops the subagent, background and scheduler tools, so the scenarios
# that need those run with the default set.

if want chat; then   # multi-turn, resume, a model switch, /compact, a forked resume
  id="$(newid)"; CHAT_ID="$id"
  gk "${SUMS[@]}" -- -s "$id" --tools "list_dir" -p "Say hello in five words."
  gk "${SUMS[@]}" -- -r "$id" --tools "list_dir" -p "Now say it in French."
  gk -- -r "$id" --tools "list_dir" -m corpus-glm-alt -p "Which language did you use first? One word."
  gk -- -r "$id" --tools "list_dir" -p "/compact"
  gk -- -r "$id" --tools "list_dir" -p "What did we talk about? One short sentence."
  gk -- -r "$id" --fork-session --tools "list_dir" -p "Say goodbye in three words."
  SCEN+=("chat" "resume" "model_switch" "compaction" "fork_session" "reasoning" "hooks" "agents_md")
fi
if want tools; then  # read/edit/write/shell, parallel calls, an error, an output cap, rewind points
  id="$(newid)"
  gk -- -s "$id" -p "Read a.txt and b.txt at the same time (two parallel read_file calls), then reply with their combined line count."
  gk -- -r "$id" -p "Use search_replace to replace TODO with DONE in notes.txt, then run: cat notes.txt"
  gk -- -r "$id" -p "Run: cat missing-file.txt   and tell me the error in one line."
  gk -- -r "$id" -p "Run exactly this command, unmodified and without pipes: seq 1 1000   then tell me only the last number."
  gk -- -r "$id" -p "Use the write tool to create c.txt containing the word delta."
  SCEN+=("tools" "parallel_tools" "tool_error" "output_cap" "write" "rewind_points")
fi
if want image; then  # an image read by a tool, and an @-referenced image in the prompt
  gk -- --tools "read_file" -p "Use read_file on dot.png once, then, without any other tool call, say in five words what you got."
  gk -- --tools "read_file" -p "@dot.png An image is attached. Do not use tools. Reply with the single word: received."
  SCEN+=("image_tool_result" "image_mention")
fi
if want todo; then   # todo_write: plan.json
  gk -- --tools "todo_write" -p "Use todo_write to create two todos (alpha, beta), then mark alpha completed with todo_write. Then say done."
  SCEN+=("todo")
fi
if want plan; then   # enter_plan_mode / exit_plan_mode (auto-approved by --always-approve)
  gk -- --tools "enter_plan_mode,exit_plan_mode,read_file" -p "Call enter_plan_mode. Then, without reading or changing any file, write a two-step plan for renaming b.txt to c.txt and call exit_plan_mode with it. Then say done."
  SCEN+=("plan_mode")
fi
if want agents; then # spawn_subagent (a child session), then its output
  gk -- -p "Use spawn_subagent to start a subagent with the task 'Read a.txt and reply with its first line'. Wait for it with get_command_or_subagent_output, then report its answer."
  SCEN+=("subagent" "subagent_output")
fi
if want background; then # a background command and a monitor
  gk -- -p "Run this command in the background (run_terminal_command with background set): sleep 3; echo corpus-done   Then wait for it with get_command_or_subagent_output and report its output. Then use monitor to run: echo monitor-line   and report the line it printed."
  SCEN+=("background_command" "monitor")
fi
if want schedule; then # scheduler_create / scheduler_list / scheduler_delete
  gk -- -p "Use scheduler_create to run the prompt 'say tick' every 30 minutes. Then scheduler_list. Then scheduler_delete that task. Then say done."
  SCEN+=("scheduler")
fi
if want mcp; then    # MCP through search_tool / use_tool
  gk -- --tools "search_tool,use_tool" -p "Use search_tool to find the corpus add tool, then use_tool to add 2 and 3, then say the result."
  SCEN+=("mcp")
fi
if want skill; then  # a user skill invoked as a slash command
  gk -- --tools "list_dir" -p "/corpus-greet"
  SCEN+=("skill")
fi
if want workflow; then # the saved Rhai workflow, by name
  gk -- -p "Run the registered workflow named corpus-hello with the workflow tool (source type name). Wait for its completion notification, then report what it returned."
  SCEN+=("workflow")
fi
if want interrupt; then # SIGINT while a command is running
  id="$(newid)"
  echo "--- grok (interrupted)"
  (cd "$PROJECT" && exec env -i "${GENV[@]}" "$BIN" --output-format json --always-approve -s "$id" --tools "run_terminal_cmd" \
    -p "Run: sleep 40   and then say done." </dev/null >>"$WORK/run.log" 2>&1) &
  ipid=$!
  for _ in $(seq 120); do grep -qs '"run_terminal_command"\|run_terminal_command' "$GROK_HOME"/sessions/*/"$id"/chat_history.jsonl && break; sleep 0.5; done
  sleep 2; kill -INT "$ipid" 2>/dev/null || true; wait "$ipid" 2>/dev/null || true
  SCEN+=("interrupt")
fi

CALLS="$(corpus_calls)"
echo "model calls: $CALLS"

# Copy the session tree Grok wrote: per-session dirs (chat_history, updates, events, summary,
# system prompt, tool definitions, signals, usage, rewind points, plan, subagents/, compaction
# checkpoints) and the per-cwd prompt_history.jsonl. Not the FTS index session_search.sqlite (cv
# does not read it), lock files, logs, config, or the docs Grok unpacks into GROK_HOME.
mkdir -p "$OUT/.grok"
cp -R "$GROK_HOME/sessions" "$OUT/.grok/"
find "$OUT" -name '*.lock' -delete
rm -f "$OUT/.grok/sessions/session_search.sqlite"*
[ -d "$GROK_HOME/memory" ] && cp -R "$GROK_HOME/memory" "$OUT/.grok/"
# events.jsonl (a phase/telemetry stream, up to 200 KB a session) and tool_definitions.json (every
# tool schema, 40 KB) are kept for the chat session only: cv reads neither, and one copy shows the shape.
for d in "$OUT"/.grok/sessions/*/*/; do
  case "$d" in *"/${CHAT_ID:-none}/") ;; *) rm -f "$d/events.jsonl" "$d/tool_definitions.json" ;; esac
done
# Not personal, but /Users/ paths, which the corpus never holds: the build machine in Grok's bundled
# workflow paths, and the examples in its image tools' descriptions.
grep -rlE '/Users/(admin/buildkite|me)/' "$OUT" | while IFS= read -r f; do
  python3 -I - "$f" <<'PY'
import sys
p = sys.argv[1]
b = open(p, "rb").read().replace(b"/Users/admin/buildkite/", b"/build/buildkite/").replace(b"/Users/me/", b"/home/me/")
open(p, "wb").write(b)
PY
done
find "$OUT" -type d -empty -delete
corpus_scrub "$OUT"
corpus_manifest "$OUT" \
  "harness=\"grok\"" "version=\"$V\"" "generated=\"$(date -u +%Y-%m-%d)\"" \
  "model=\"$MODEL (switch target $ALT) as BYOK [model.*] entries via Z.ai's chat endpoint, through key_proxy.py\"" \
  "model_calls=$CALLS" \
  "scenarios=[$(printf '"%s",' "${SCEN[@]}" | sed 's/,$//')]" \
  "skipped=[$( [ ${#SKIP[@]} -gt 0 ] && printf '"%s",' "${SKIP[@]}" | sed 's/,$//')]" \
  "generator_commit=\"$(git -C "$HERE" rev-parse --short HEAD)\""
corpus_size_check "$OUT"
