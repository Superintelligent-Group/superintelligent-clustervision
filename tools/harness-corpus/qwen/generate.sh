#!/usr/bin/env bash
# Generate the Qwen Code corpus for one installed version: real session JSONL Qwen Code wrote itself,
# in a throwaway home, driven by a cheap real model (Z.ai GLM, OpenAI-compatible, through
# ../key_proxy.py --translate chat, so the key never reaches the harness).
#
#   tools/harness-corpus/qwen/generate.sh <version> <out-dir> [--keep] [--only "chat tools ..."]
#
# <out-dir> is normally crates/cv-core/tests/corpus/qwen/<version>; it must not exist yet. Needs:
# install.sh <version> run first, python3, git, ~/.zai-key. Env: ZAI_KEY_FILE, CORPUS_MODEL (default
# glm-5.3-flash), MAX_CALLS (default 70).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$HERE/../lib.sh"

V="${1:?usage: generate.sh <version> <out-dir>}"; OUT="${2:?usage: generate.sh <version> <out-dir>}"; shift 2
KEEP=0
ONLY="chat tools background image todos tasks plan mcp subagent skill command hook memory web model compress fork cron goal"
while [ $# -gt 0 ]; do
  case "$1" in --keep) KEEP=1 ;; --only) ONLY="$2"; shift ;; *) echo "unknown arg $1" >&2; exit 2 ;; esac; shift
done
mkdir -p "$(dirname "$OUT")"; OUT="$(cd "$(dirname "$OUT")" && pwd -P)/$(basename "$OUT")"
[ ! -e "$OUT" ] || { echo "refusing: $OUT exists (add a new version instead of regenerating)" >&2; exit 1; }
BIN="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/qwen/$V/node_modules/.bin/qwen"
[ -x "$BIN" ] || { echo "qwen $V not installed: run $HERE/install.sh $V" >&2; exit 1; }
MODEL="${CORPUS_MODEL:-glm-5.3-flash}"
ALT_MODEL="glm-4.7-flash" # the name Qwen Code is told for the model switch; the proxy still answers with $MODEL
MAX_CALLS="${MAX_CALLS:-70}"
UPSTREAM="https://api.z.ai/api/coding/paas/v4"

corpus_work
cleanup() { corpus_proxy_stop; [ "$KEEP" = 1 ] && echo "kept $WORK" || rm -rf "$WORK"; }
trap cleanup EXIT
export HOME="$CORPUS_HOME" QWEN_HOME="$CORPUS_HOME/.qwen" TMPDIR="$WORK/tmp"
mkdir -p "$TMPDIR"
unset XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME QWEN_RUNTIME_DIR OPENAI_API_KEY OPENAI_BASE_URL \
  OPENAI_MODEL QWEN_MODEL ANTHROPIC_API_KEY GEMINI_API_KEY GOOGLE_API_KEY DASHSCOPE_API_KEY SANDBOX
corpus_guard HOME QWEN_HOME TMPDIR
PROJECT="$CORPUS_HOME/project"
QDIR="$QWEN_HOME"
mkdir -p "$QDIR/skills/corpus-greeting" "$QDIR/commands"

cd "$PROJECT"
printf 'alpha\nbeta\n' >a.txt; printf 'gamma\n' >b.txt; printf 'TODO: fix\n' >notes.txt
python3 -I -c 'import base64,sys; sys.stdout.buffer.write(base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg=="))' >dot.png
git init -q -b main . && git -c user.name=corpus -c user.email=corpus@example.invalid add -A \
  && git -c user.name=corpus -c user.email=corpus@example.invalid commit -qm init

cat >"$QDIR/corpus_hook.sh" <<'SH'
#!/bin/sh
cat >/dev/null
[ "${CORPUS_HOOK:-}" = 1 ] || exit 0
echo '{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"corpus hook: the magic word is teal."}}'
SH
chmod +x "$QDIR/corpus_hook.sh"
cat >"$QDIR/skills/corpus-greeting/SKILL.md" <<'MD'
---
name: corpus-greeting
description: How to greet the corpus user. Use when asked to greet with the corpus greeting.
---
Greet with exactly: "Greetings from the corpus skill."
MD
cat >"$QDIR/commands/corpus-hello.toml" <<'TOML'
description = "Say hello the corpus way"
prompt = "Reply with exactly: hello from a custom command. {{args}}"
TOML
cat >"$QDIR/settings.json" <<JSON
{
  "security": {"auth": {"selectedType": "openai"}, "folderTrust": {"enabled": false}},
  "privacy": {"usageStatisticsEnabled": false},
  "telemetry": {"enabled": false},
  "general": {"enableAutoUpdate": false, "checkpointing": {"enabled": true}},
  "model": {"name": "$MODEL"},
  "memory": {"enableManagedAutoMemory": false, "enableManagedAutoDream": false},
  "ui": {"enableFollowupSuggestions": false},
  "tools": {"todoWrite": {"enabled": true}},
  "mcpServers": {"corpus": {"command": "python3", "args": ["-I", "$HERE/../codex/mcp_server.py"], "trust": true}},
  "hooks": {"UserPromptSubmit": [{"hooks": [{"type": "command", "command": "$QDIR/corpus_hook.sh"}]}]}
}
JSON

# Thinking is left as Qwen Code asks for it (--thinking harness).
corpus_proxy "${ZAI_KEY_FILE:-$REAL_HOME/.zai-key}" chat "$UPSTREAM" --model "$MODEL" --thinking harness
# Background extras that each cost model calls (auto memory extraction and dreaming, follow-up
# suggestions) are off in settings.json; auto titles are off except in the chat scenario.
export QWEN_DISABLE_AUTO_TITLE=1
export OPENAI_API_KEY="corpus-local-shim" OPENAI_BASE_URL="$PROXY_URL/v1" OPENAI_MODEL="$MODEL"

# ws JSON: a workspace settings file for the next scenario only; ws with no argument removes it.
ws() { if [ $# -gt 0 ]; then mkdir -p "$PROJECT/.qwen"; printf '%s\n' "$1" >"$PROJECT/.qwen/settings.json"; else rm -rf "$PROJECT/.qwen"; fi; }
run() { echo "--- qwen $*" | cut -c1-150; timeout "${RUN_TIMEOUT:-300}" "$BIN" "$@" </dev/null >>"$WORK/run.log" 2>&1 || echo "    (exit $?)"; }
newid() { python3 -I -c 'import uuid; print(uuid.uuid4())'; }
want() { case " $ONLY " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
SCEN=()
# What Qwen Code would not do headless here (AFFORDANCES.md): recorded in MANIFEST.toml.
SKIP=("exit_plan_mode (not offered headless)" "save_memory (not offered headless)" "custom_title (no record written)" "rewind/branch (interactive)" "a cron fire (the process exits first)")

if want chat; then   # 1 multi-turn chat + 9 resume (--resume <id> appends to the same session file)
  id="$(newid)"
  QWEN_DISABLE_AUTO_TITLE=0 run --session-id "$id" "Say hello in five words."
  run -r "$id" "Now say it in French."
  run -r "$id" "Which language did you use first? One word."
  SCEN+=("chat" "resume")
fi
if want tools; then  # 2 read/edit/shell, 3 parallel calls, 4 a tool error, 5 reasoning
  id="$(newid)"
  run --session-id "$id" -y "Read a.txt and b.txt at the same time (two parallel read_file calls), then reply with their combined line count."
  run -r "$id" -y "Use the edit tool to change TODO to DONE in notes.txt, then run the shell command: cat notes.txt"
  run -r "$id" -y "Run the shell command: cat missing-file.txt   and tell me the error in one line."
  SCEN+=("tools" "parallel_tools" "tool_error" "reasoning")
fi
if want background; then
  run -y "Run the shell command 'sleep 1; echo bg-done' in the background (run_shell_command with is_background true), then say started."
  SCEN+=("background_shell")
fi
if want image; then  # 6 image input via @file (an at_command record + inline image part)
  run "@dot.png An image is attached. Do not use any tools. Reply with the single word: received."
  SCEN+=("image" "at_command")
fi
if want todos; then
  run -y "Use the todo_write tool to record a plan with three steps (read a.txt, read b.txt, summarize), the first in_progress. Then stop."
  SCEN+=("todo_write")
fi
if want tasks; then  # task_* tools register only with the (experimental) agent team feature on
  ws '{"experimental": {"agentTeam": true}}'
  run -y "Use task_create to create a task titled 'corpus task', then task_list, and say how many tasks exist."
  ws
  SCEN+=("task_tracker")
fi
if want plan; then  # exit_plan_mode waits for an approval headless never gives: the run is cut at 90 s
  RUN_TIMEOUT=90 run --approval-mode plan "Plan (do not implement) how to add a file c.txt containing delta, then call exit_plan_mode with the plan."
  SCEN+=("plan_mode")
fi
if want mcp; then
  run -y "Use the corpus MCP server's add tool to add 2 and 3, then say the result."
  SCEN+=("mcp")
fi
if want subagent; then # 7 a sub-agent (agent tool): subagents/<session>/agent-<id>.jsonl + meta.json
  run -y "Use the agent tool to launch one general-purpose sub-agent with the task: reply with the word ok. Then report what it said."
  SCEN+=("subagent")
fi
if want skill; then
  run -y "Use the corpus-greeting skill (via the skill tool) and greet me with it."
  SCEN+=("skill")
fi
if want command; then
  run "/corpus-hello please"
  SCEN+=("custom_command")
fi
if want hook; then   # a UserPromptSubmit hook's additionalContext
  CORPUS_HOOK=1 run "What is the magic word? One word."
  SCEN+=("hook")
fi
if want memory; then
  run -y "Use save_memory to remember that the corpus project's favourite colour is teal. Then say done."
  : # save_memory was not offered headless on 0.25.1 (see SKIP)
fi
if want web; then
  run -y "Use web_fetch to fetch https://example.com and tell me its title in five words or fewer."
  SCEN+=("web_fetch")
fi
if want model; then  # a model switch inside one session
  id="$(newid)"
  run --session-id "$id" "Name one color."
  run -r "$id" -m "$ALT_MODEL" "Name another color."
  SCEN+=("model_switch")
fi
if want compress; then # 8 /compress works headless: a chat_compression record
  id="$(newid)"
  run --session-id "$id" "List three prime numbers."
  run -r "$id" "/compress"
  run -r "$id" "List three more."
  SCEN+=("compress")
fi
if want fork; then   # --fork-session: a new session copied from the parent (forkedFrom on every record)
  id="$(newid)"
  run --session-id "$id" "Say the word fork."
  run -r "$id" --fork-session "Say the word branch."
  SCEN+=("fork")
fi
if want cron; then   # cron_create / cron_list (a session-scoped schedule; the process exits before it fires)
  run -y "Use cron_create to schedule the prompt 'say tick' every day at 09:00, then cron_list, then say done."
  SCEN+=("cron")
fi
if want goal; then   # /goal sets a session goal (goal_state records)
  run -y "/goal reply with the word goal"
  SCEN+=("goal")
fi

CALLS="$(corpus_calls)"
echo "model calls: $CALLS"

# Copy the store as Qwen Code wrote it: projects/ (chats, subagents, per-project memory and meta) and
# the legacy tmp/ if any. Not settings, installation ids, usage logs, the extension store, or our
# hook/skill/command inputs. The project dir name is the sanitized cwd (every non-alphanumeric byte
# a dash), which carries the throwaway path: rename it to the sanitized placeholder path and rewrite
# that spelling inside the files, before the scrub rewrites the slash-separated spellings. The tmp/
# dir name is the project hash (sha256 of the cwd, `getProjectHash`): it becomes the placeholder
# path's hash, so a reader can still find `scheduled_tasks.json` from the scrubbed cwd.
mkdir -p "$OUT/.qwen"
[ -d "$QDIR/projects" ] && cp -R "$QDIR/projects" "$OUT/.qwen/"
[ -d "$QDIR/tmp" ] && cp -R "$QDIR/tmp" "$OUT/.qwen/"
python3 -I - "$OUT/.qwen" "$PROJECT" "$CORPUS_HOME" "$WORK" <<'PY'
import hashlib, os, re, sys
root, project, home, work = sys.argv[1:5]
san = lambda p: re.sub(r"[^a-zA-Z0-9]", "-", p)
sha = lambda p: hashlib.sha256(p.encode()).hexdigest()
pairs = []
for base in {work, os.path.realpath(work), work.replace("/private/", "/", 1)}:
    pairs += [(sha(project.replace(work, base, 1)), sha("/home/corpus/project"))]
    pairs += [(san(project.replace(work, base, 1)), san("/home/corpus/project")),
              (san(home.replace(work, base, 1)), san("/home/corpus")), (san(base), san("/home/corpus/.work"))]
pairs.sort(key=lambda p: -len(p[0]))
for d, dirs, files in os.walk(root, topdown=False):
    for f in files:
        p = os.path.join(d, f)
        b = open(p, "rb").read()
        nb = b
        for a, z in pairs:
            nb = nb.replace(a.encode(), z.encode())
        if nb != b:
            open(p, "wb").write(nb)
    for name in dirs + files:
        new = name
        for a, z in pairs:
            new = new.replace(a, z)
        if new != name:
            os.rename(os.path.join(d, name), os.path.join(d, new))
PY
corpus_scrub "$OUT"
corpus_manifest "$OUT" \
  "harness=\"qwen\"" "version=\"$V\"" "generated=\"$(date -u +%Y-%m-%d)\"" \
  "model=\"$MODEL via Z.ai coding endpoint, through key_proxy.py --translate chat (OpenAI-compatible auth)\"" \
  "model_switch_name=\"$ALT_MODEL (the name Qwen Code recorded for the switch; the proxy answered with $MODEL)\"" \
  "model_calls=$CALLS" \
  "scenarios=[$(printf '"%s",' "${SCEN[@]}" | sed 's/,$//')]" \
  "skipped=[${SKIP[*]:+$(printf '"%s",' "${SKIP[@]}" | sed 's/,$//')}]" \
  "generator_commit=\"$(git -C "$HERE" rev-parse --short HEAD)\""
corpus_size_check "$OUT"
