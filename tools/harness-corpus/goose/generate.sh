#!/usr/bin/env bash
# Generate the Goose corpus for one installed version: a real sessions.db that Goose wrote itself,
# in a throwaway GOOSE_PATH_ROOT, driven by a cheap real model (Z.ai GLM through ../key_proxy.py --translate chat).
#
#   tools/harness-corpus/goose/generate.sh <version> <out-dir> [--keep] [--only "chat tools ..."]
#
# <out-dir> is normally crates/cv-core/tests/corpus/goose/<version>; it must not exist yet (a
# committed version is never regenerated in place). Needs: install.sh <version> run first, python3,
# sqlite3, git, ~/.zai-key. Env: ZAI_KEY_FILE (default ~/.zai-key), CORPUS_MODEL (default
# glm-5.3-flash), SWITCH_MODEL (default glm-4.7-flash), MAX_CALLS (default 60).
# The scenario names are listed in AFFORDANCES.md, which says what each one leaves in the store.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$HERE/../lib.sh"

V="${1:?usage: generate.sh <version> <out-dir>}"; OUT="${2:?usage: generate.sh <version> <out-dir>}"; shift 2
KEEP=0
ONLY="chat tools image compact mcp subagent todo skill hook maxturns switch recipe schedule recall approve fork import rename"
while [ $# -gt 0 ]; do
  case "$1" in --keep) KEEP=1 ;; --only) ONLY="$2"; shift ;; *) echo "unknown arg $1" >&2; exit 2 ;; esac; shift
done
mkdir -p "$(dirname "$OUT")"; OUT="$(cd "$(dirname "$OUT")" && pwd -P)/$(basename "$OUT")"
[ ! -e "$OUT" ] || { echo "refusing: $OUT exists (add a new version instead of regenerating)" >&2; exit 1; }
BIN="${HARNESS_VERSIONS:-$REAL_HOME/tools/harness-versions}/goose/$V/bin/goose"
[ -x "$BIN" ] || { echo "goose $V not installed: run $HERE/install.sh $V" >&2; exit 1; }
for t in python3 sqlite3 git; do command -v "$t" >/dev/null || { echo "missing $t" >&2; exit 1; }; done
MODEL="${CORPUS_MODEL:-glm-5.3-flash}"; SWITCH_MODEL="${SWITCH_MODEL:-glm-4.7-flash}"; MAX_CALLS="${MAX_CALLS:-60}"
UPSTREAM="https://api.z.ai/api/coding/paas/v4"

corpus_work
cleanup() { corpus_proxy_stop; [ "$KEEP" = 1 ] && echo "kept $WORK" || rm -rf "$WORK"; }
trap cleanup EXIT
# GOOSE_PATH_ROOT relocates config, state and data together; HOME covers ~/.agents (skills, plugins).
export HOME="$CORPUS_HOME" GOOSE_PATH_ROOT="$CORPUS_HOME/.goose" TMPDIR="$WORK/tmp"
mkdir -p "$TMPDIR"
unset XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME OPENAI_API_KEY OPENAI_BASE_URL GOOSE_PROVIDER GOOSE_MODEL
corpus_guard HOME GOOSE_PATH_ROOT TMPDIR
PROJECT="$CORPUS_HOME/project"
DB="$GOOSE_PATH_ROOT/data/sessions/sessions.db"

# The scratch project: a git repo with a few small files, a project skill, and a policy hook.
cd "$PROJECT"
printf 'alpha\nbeta\n' >a.txt; printf 'gamma\n' >b.txt; printf 'TODO: fix\n' >notes.txt
python3 -I -c 'import base64,sys; sys.stdout.buffer.write(base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg=="))' >dot.png
mkdir -p .agents/skills/greet
cat >.agents/skills/greet/SKILL.md <<'MD'
---
name: greet
description: How to greet the corpus user. Load this when asked to greet.
---
Greet with exactly: "Ahoy from the greet skill."
MD
# A PreToolUse hook (Open Plugins hooks.json) that blocks any tool call mentioning FORBIDDEN.
mkdir -p .agents/plugins/corpus-policy/hooks
cat >.agents/plugins/corpus-policy/hooks/hooks.json <<'JSON'
{"hooks": {"PreToolUse": [{"hooks": [{"type": "command", "command": "sh ${PLUGIN_ROOT}/policy.sh"}]}]}}
JSON
cat >.agents/plugins/corpus-policy/policy.sh <<'SH'
#!/bin/sh
if grep -q FORBIDDEN; then echo '{"decision":"block","reason":"the corpus policy forbids that word"}'; fi
exit 0
SH
chmod +x .agents/plugins/corpus-policy/policy.sh
cat >"$WORK/recipe.yaml" <<'YAML'
version: 1.0.0
title: corpus recipe
description: A one-parameter recipe for the corpus.
parameters:
  - key: word
    input_type: string
    requirement: required
    description: the word to echo
prompt: "Reply with exactly the word {{ word }} and nothing else. Do not use tools."
YAML
git init -q -b main . && git -c user.name=corpus -c user.email=corpus@example.invalid add -A \
  && git -c user.name=corpus -c user.email=corpus@example.invalid commit -qm init

# Only the proxy sees the real key; Goose gets a dummy one for an openai provider at the proxy. The
# switch model passes unpinned (--also), so a model switch in a session is a real one.
corpus_proxy "${ZAI_KEY_FILE:-$REAL_HOME/.zai-key}" chat "$UPSTREAM" --model "$MODEL" --also "$SWITCH_MODEL"
export GOOSE_DISABLE_KEYRING=1 GOOSE_PROVIDER=openai GOOSE_MODEL="$MODEL" OPENAI_HOST="$PROXY_URL" \
  OPENAI_API_KEY=corpus-local-shim GOOSE_MODE=auto GOOSE_TELEMETRY_ENABLED=false GOOSE_CLI_MIN_PRIORITY=0.0

# g returns goose's exit status (a scenario whose command this version lacks goes to SKIPS); run
# carries on past a failed turn, which the store records as whatever Goose kept of it.
g() { local rc=0; echo "--- goose $*" | cut -c1-150; "$BIN" "$@" </dev/null >>"$WORK/run.log" 2>&1 || rc=$?; [ "$rc" = 0 ] || echo "    (exit $rc)"; return "$rc"; }
run() { g run --max-turns 6 "$@" || true; }
want() { case " $ONLY " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
sid() { sqlite3 "$DB" "SELECT id FROM sessions WHERE name = '$1' ORDER BY created_at DESC LIMIT 1;"; }
SCEN=()
# Affordances Goose has but this generator cannot drive headless (AFFORDANCES.md says why).
SKIPS=("interrupt (Ctrl-C needs a terminal)" "steer (interactive queue only)" "bang_shell (interactive only; in goose run the model gets the text)" "web_fetch (no default web tool)")

if want chat; then    # plain multi-turn chat + resume (by name), three turns in one session
  run -t "Say hello in five words." --name chat
  run --resume --name chat -t "Now say it in French."
  run --resume --name chat -t "Which language did you use first? One word."
  SCEN+=("chat" "resume" "reasoning")
fi
if want tools; then   # shell / write / edit / tree / analyze, parallel calls, a tool error
  run --name tools -t "Run two shell tool calls in parallel, in one response: 'cat a.txt' and 'cat b.txt'. Then reply with the combined line count."
  run --resume --name tools -t "Use the edit tool to replace TODO with DONE in notes.txt, then use the write tool to create c.txt containing 'delta'. Reply done."
  run --resume --name tools -t "Run the shell command 'cat missing-file.txt' and tell me the error in one line."
  run --resume --name tools -t "Call the tree tool on this directory and the analyze tool on a.txt, then reply with one word."
  SCEN+=("tools" "parallel_tools" "tool_error")
fi
if want image; then   # developer read_image: the image comes back inside a toolResponse
  run --name image -t "Use the read_image tool on dot.png. Then reply with the single word: received."
  SCEN+=("image")
fi
if want compact; then # auto-compaction: a small context limit makes the resumed turn compact first
  # (the check compares the last turn's total_tokens with the limit; the system prompt alone is ~4k)
  run --name compact -t "List three prime numbers."
  GOOSE_CONTEXT_LIMIT=8000 GOOSE_AUTO_COMPACT_THRESHOLD=0.3 run --resume --name compact -t "List three more."
  SCEN+=("compaction")
fi
if want mcp; then     # a stdio MCP extension (the codex corpus's one-tool server)
  run --name mcp --with-extension "corpus:python3 -I $HERE/../codex/mcp_server.py" \
    -t "Use the corpus add tool to add 2 and 3, then say the result."
  SCEN+=("mcp")
fi
if want subagent; then # summon's delegate tool: a child session (session_type sub_agent)
  run --name subagent -t "Use the delegate tool once, with instructions 'Reply with the word ok. Do not use tools.' Then report what it said."
  SCEN+=("subagent")
fi
if want todo; then    # the todo platform extension (default-off in newer versions)
  run --name todo --with-builtin todo -t "Use todo_write to record a two-item todo list (read a.txt; read b.txt). Then reply done without doing them."
  SCEN+=("todo")
fi
if want skill; then   # a project skill under .agents/skills, loaded with load_skill / load
  run --name skill -t "Load the greet skill, then greet me exactly as it says."
  SCEN+=("skill")
fi
if want hook; then    # a PreToolUse plugin hook that denies the call
  run --name hook -t "Use the shell tool to run: echo FORBIDDEN. If it is blocked, say why in one line."
  SCEN+=("hook_deny")
fi
if want maxturns; then # --max-turns 1 stops the agent after its first tool round
  g run --max-turns 1 --name maxturns -t "Run 'cat a.txt' with the shell tool, then 'cat b.txt' in a second step, then summarize." || true
  SCEN+=("max_turns")
fi
if want switch; then  # a model switch inside one session
  run --resume --name chat --model "$SWITCH_MODEL" -t "Say goodbye in three words."
  SCEN+=("model_switch")
fi
if want recipe; then  # a parameterized recipe (sessions.recipe_json, user_recipe_values_json)
  run --name recipe --recipe "$WORK/recipe.yaml" --params word=pineapple
  SCEN+=("recipe")
fi
if want schedule; then # a scheduled job run now: session_type scheduled + schedule_id
  if g schedule add --schedule-id corpus-job --cron "0 0 1 1 *" --recipe-source "$WORK/recipe.yaml" --params word=kiwi \
    && g schedule run-now --schedule-id corpus-job; then SCEN+=("schedule"); else SKIPS+=("schedule (failed in $V)"); fi
fi
if want recall; then  # chatrecall: search past sessions
  run --name recall --with-builtin chatrecall -t "Use the chat recall tool to search past sessions for the word French, then answer in one line."
  SCEN+=("chat_recall")
fi
if want approve; then # approve mode with no terminal to approve from
  # Goose refuses (exit 1) and keeps only the prompt rows: that refusal is what is captured.
  GOOSE_MODE=approve run --name approve -t "Use the shell tool to run: ls"
  SCEN+=("approval")
fi
if want fork; then    # fork a session (interactive entry; stdin is closed, so it exits at once)
  if g session --resume --name chat --fork; then SCEN+=("fork"); else SKIPS+=("fork (failed in $V)"); fi
fi
if want import; then  # Goose's own importer reading a Claude Code transcript (no model)
  if g session import "$HERE/../../../crates/cv-core/tests/fixtures/claude/rich_blocks.jsonl"; then SCEN+=("import")
  else SKIPS+=("import (failed in $V)"); fi
fi
if want rename; then  # sessions.name rewritten, user_set_name
  id="$(sid tools)"
  if [ -n "$id" ] && g session rename --session-id "$id" --new-name tools-renamed; then SCEN+=("rename")
  else SKIPS+=("rename (no goose session rename in $V)"); fi
fi

# The model decides whether to batch calls; say what it actually did.
if want tools && [ -f "$DB" ]; then
  par="$(sqlite3 "$DB" "SELECT COUNT(*) FROM messages WHERE role = 'assistant' AND (SELECT COUNT(*) FROM json_each(content_json) j WHERE json_extract(j.value, '$.type') = 'toolRequest') > 1;")"
  if [ "$par" = 0 ]; then
    SCEN=($(printf '%s\n' "${SCEN[@]}" | grep -vx parallel_tools))
    SKIPS+=("parallel_tools (asked for; the model made its calls one per turn)")
  fi
fi
CALLS="$(corpus_calls)"
echo "model calls: $CALLS"
[ -f "$DB" ] || { echo "goose wrote no store at $DB (see $WORK/run.log with --keep)" >&2; exit 1; }

# Copy the store Goose wrote: sessions.db only (VACUUM INTO checkpoints the WAL into one file). Not
# config, logs (llm_request.*.jsonl holds full requests), the model catalog, apps or schedule.json.
mkdir -p "$OUT/sessions"
sqlite3 "$DB" "VACUUM INTO '$OUT/sessions/sessions.db'"
corpus_scrub "$OUT"
corpus_manifest "$OUT" \
  "harness=\"goose\"" "version=\"$V\"" "generated=\"$(date -u +%Y-%m-%d)\"" \
  "model=\"$MODEL via Z.ai coding endpoint, through key_proxy.py --translate chat (switch scenario: $SWITCH_MODEL)\"" \
  "schema_version=$(sqlite3 "$OUT/sessions/sessions.db" 'SELECT MAX(version) FROM schema_version;')" \
  "model_calls=$CALLS" \
  "scenarios=[$(printf '"%s",' "${SCEN[@]}" | sed 's/,$//')]" \
  "skipped=[$(printf '"%s",' "${SKIPS[@]}" | sed 's/,$//')]" \
  "generator_commit=\"$(git -C "$HERE" rev-parse --short HEAD)\""
corpus_size_check "$OUT"
sqlite3 "$OUT/sessions/sessions.db" "SELECT '  ' || id || '  ' || COALESCE(name,'') || '  ' || session_type || '  ' || (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id) FROM sessions s ORDER BY id;"
