#!/usr/bin/env bash
# Generate the OpenClaw corpus for one installed version: real sessions OpenClaw wrote itself, in a
# throwaway home, driven by a cheap real model (Z.ai GLM through chat_shim.py), with a foreground
# gateway in that home so sub-agents, cron and compaction run as they do for a user.
#
#   tools/harness-corpus/openclaw/generate.sh <version> <out-dir> [--keep] [--only "chat tools ..."]
#
# <out-dir> is normally crates/cv-core/tests/corpus/openclaw/<version>; it must not exist yet.
# Needs: install.sh <version> run first, python3, git, sqlite3, ~/.zai-key. Env: ZAI_KEY_FILE,
# CORPUS_MODEL (default glm-5.3-flash), PORT (shim, default 18821), GW_PORT (default 18947),
# MAX_CALLS (default 60).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$HERE/../lib.sh"

V="${1:?usage: generate.sh <version> <out-dir>}"; OUT="${2:?usage: generate.sh <version> <out-dir>}"; shift 2
KEEP=0; ONLY="chat tools image subagent cron memory compact mcp"
while [ $# -gt 0 ]; do
  case "$1" in --keep) KEEP=1 ;; --only) ONLY="$2"; shift ;; *) echo "unknown arg $1" >&2; exit 2 ;; esac; shift
done
mkdir -p "$(dirname "$OUT")"; OUT="$(cd "$(dirname "$OUT")" && pwd -P)/$(basename "$OUT")"
[ ! -e "$OUT" ] || { echo "refusing: $OUT exists (add a new version instead of regenerating)" >&2; exit 1; }
VERSIONS="${HARNESS_VERSIONS:-$REAL_HOME/tools/harness-versions}"
BIN="$VERSIONS/openclaw/$V/node_modules/.bin/openclaw"
[ -x "$BIN" ] || { echo "openclaw $V not installed: run $HERE/install.sh $V" >&2; exit 1; }
command -v sqlite3 >/dev/null || { echo "sqlite3 not on PATH" >&2; exit 1; }
MODEL="${CORPUS_MODEL:-glm-5.3-flash}"; PORT="${PORT:-18821}"; GW_PORT="${GW_PORT:-18947}"
UPSTREAM="https://api.z.ai/api/coding/paas/v4"
# Ports must be free: a stale listener would answer for the shim or gateway and the store would
# come out looking fine.
for p in "$PORT" "$GW_PORT"; do
  if (exec 3<>"/dev/tcp/127.0.0.1/$p") 2>/dev/null; then echo "refusing: something listens on $p" >&2; exit 1; fi
done

corpus_work
SHIM_PID=""; GW_PID=""
cleanup() {
  [ -z "$GW_PID" ] || kill "$GW_PID" 2>/dev/null || true
  [ -z "$SHIM_PID" ] || kill "$SHIM_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  [ "$KEEP" = 1 ] && echo "kept $WORK" || rm -rf "$WORK"
}
trap cleanup EXIT
export HOME="$CORPUS_HOME" OPENCLAW_STATE_DIR="$CORPUS_HOME/.openclaw" TMPDIR="$WORK/tmp"
export OPENCLAW_CONFIG_PATH="$OPENCLAW_STATE_DIR/openclaw.json"
mkdir -p "$TMPDIR" "$OPENCLAW_STATE_DIR"
# Legacy names OpenClaw still honours (clawdbot/moltbot) would point it somewhere else.
unset CLAWDBOT_STATE_DIR CLAWDBOT_CONFIG_PATH MOLTBOT_STATE_DIR MOLTBOT_CONFIG_PATH OPENCLAW_PROFILE \
  XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME OPENAI_API_KEY ANTHROPIC_API_KEY ZAI_API_KEY Z_AI_API_KEY
corpus_guard HOME OPENCLAW_STATE_DIR TMPDIR
PROJECT="$CORPUS_HOME/project"
# A local, non-secret shared token for the loopback gateway in this throwaway home.
export OPENCLAW_GATEWAY_TOKEN="corpus-local-gateway"
export OPENCLAW_SKIP_CHANNELS=1 OPENCLAW_DISABLE_UPDATE_CHECK=1 OPENCLAW_NO_RESPAWN=1

cd "$PROJECT"
printf 'alpha\nbeta\n' >a.txt; printf 'gamma\n' >b.txt; printf 'TODO: fix\n' >notes.txt
python3 -I -c 'import base64,sys; sys.stdout.buffer.write(base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg=="))' >dot.png
# The bootstrap files OpenClaw would seed (AGENTS.md, SOUL.md, ...) are ~13 KB of prompt per call;
# a one-line AGENTS.md and skipBootstrap keep every call small.
printf 'You are a coding agent in a small test repository. Be brief. Use tools when asked.\n' >AGENTS.md
mkdir -p memory; printf '# Memory\n\nThe project codename is BLUEFIN.\n' >MEMORY.md
git init -q -b main . && git -c user.name=corpus -c user.email=corpus@example.invalid add -A \
  && git -c user.name=corpus -c user.email=corpus@example.invalid commit -qm init

MCP_JSON=""; MCP_SKIP=""
# `mcp.servers` arrived after 2026.1.x; an unknown config key makes the older gateway refuse to start.
if ! "$BIN" --help 2>/dev/null | grep -qE "^ +mcp( |$)"; then ONLY="${ONLY/mcp/}"; MCP_SKIP="mcp: this version has no mcp.servers config (MCP only through the mcporter skill)"; fi
case " $ONLY " in *" mcp "*) MCP_JSON=",\"mcp\":{\"servers\":{\"corpus\":{\"command\":\"python3\",\"args\":[\"-I\",\"$HERE/../codex/mcp_server.py\"]}}}" ;; esac
python3 -I - "$OPENCLAW_CONFIG_PATH" "$PORT" "$GW_PORT" "$PROJECT" "$MODEL" "$MCP_JSON" <<'PY'
import json, sys
path, port, gw, project, model, mcp = sys.argv[1:]
m = lambda mid, name, ctx: {"id": mid, "name": name, "reasoning": True, "input": ["text", "image"],
                            "contextWindow": ctx, "maxTokens": 4096}
cfg = {
  "models": {"mode": "merge", "providers": {"corpus": {
    "baseUrl": f"http://127.0.0.1:{port}/v1", "apiKey": "corpus-local-shim", "api": "openai-completions",
    # The second id is the same model behind the shim with OpenClaw's minimum context window, so a
    # session switched to it compacts on its own.
    "models": [m(model, "GLM (corpus)", 128000), m(model + "-tiny", "GLM tiny window (corpus)", 16000)]}}},
  "agents": {"defaults": {"model": {"primary": f"corpus/{model}"}, "workspace": project, "skipBootstrap": True,
                          "subagents": {"model": f"corpus/{model}"}}},
  "skills": {"allowBundled": []},
  "gateway": {"mode": "local", "bind": "loopback", "port": int(gw)},
}
if mcp:
    cfg.update(json.loads("{" + mcp.lstrip(",") + "}"))
json.dump(cfg, open(path, "w"), indent=1)
PY

corpus_load_key UPSTREAM_KEY "${ZAI_KEY_FILE:-$REAL_HOME/.zai-key}"
python3 -I "$HERE/chat_shim.py" --port "$PORT" --upstream "$UPSTREAM" --model "$MODEL" \
  --log "$WORK/calls.jsonl" --max-calls "${MAX_CALLS:-60}" &
SHIM_PID=$!
unset UPSTREAM_KEY
for _ in $(seq 50); do (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && break; sleep 0.1; done
: >"$WORK/calls.jsonl"

# The gateway in the foreground (never `gateway install`: no launchd service).
"$BIN" gateway run --port "$GW_PORT" --bind loopback --auth token --allow-unconfigured >"$WORK/gateway.log" 2>&1 &
GW_PID=$!
for _ in $(seq 300); do (exec 3<>"/dev/tcp/127.0.0.1/$GW_PORT") 2>/dev/null && break; sleep 0.2; done
(exec 3<>"/dev/tcp/127.0.0.1/$GW_PORT") 2>/dev/null || { echo "gateway did not start; see gateway.log" >&2; tail -20 "$WORK/gateway.log" >&2; exit 1; }

oc() { echo "--- openclaw $*" | cut -c1-150; "$BIN" "$@" </dev/null >>"$WORK/run.log" 2>&1 || echo "    (exit $?)"; }
# 2026.1.x routes every CLI turn by session key, and a direct-chat key always collapses to
# agent:main:main, so --session-id cannot open a second session there: a new session is a
# `sessions.reset` of main (what /new does), and an earlier one cannot be re-entered. Later versions
# bind an explicit --session-id to its own key (agent:main:explicit:<id>).
if "$BIN" agent --help 2>&1 | grep -q -- '--session-key'; then PER_ID=1; else PER_ID=0; fi
ask() {
  local sid="$1"; shift
  if [ "$PER_ID" = 1 ]; then oc agent --agent main --session-id "$sid" --thinking low --timeout 300 --json --message "$@"
  else oc agent --agent main --thinking low --timeout 300 --json --message "$@"; fi
}
uuid() { python3 -I -c 'import uuid; print(uuid.uuid4())'; }
want() { case " $ONLY " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
SCEN=(); SKIP=()

# Copy the store as OpenClaw wrote it, not config, auth, logs or caches:
# - agents/<id>/sessions/ (the JSONL store: transcripts, sessions.json, reset/bak archives; no locks)
# - agents/<id>/agent/openclaw-agent.sqlite (the sqlite store, via the backup API), keeping only the
#   tables named in KEEP_AGENT_TABLES: the full schema is ~190 tables, ~780 KB empty, and cv reads three.
# - the sub-agent and cron registries (subagents/runs.json, cron/ in the JSONL era; their tables in
#   state/openclaw.sqlite in the sqlite era), so the corpus shows what those affordances persist.
KEEP_AGENT_TABLES="schema_meta transcript_events session_windows session_nodes"
KEEP_STATE_TABLES="schema_meta subagent_runs task_runs cron_jobs cron_run_receipts"
copy_store() {
  mkdir -p "$OUT"
  ( cd "$OPENCLAW_STATE_DIR"
    find agents -path '*/sessions/*' -type f ! -name '*.lock' ! -name '*.tmp' -print
    for f in subagents/runs.json; do [ -f "$f" ] && echo "$f"; done
    [ -d cron ] && find cron -type f ! -name '*.lock' ! -name '*.bak' -print
    true
  ) | while IFS= read -r f; do mkdir -p "$OUT/$(dirname "$f")"; cp "$OPENCLAW_STATE_DIR/$f" "$OUT/$f"; done
  python3 -I - "$OPENCLAW_STATE_DIR" "$OUT" "$KEEP_AGENT_TABLES" "$KEEP_STATE_TABLES" <<'PY'
import glob, os, re, sqlite3, sys
src, out, keep_agent, keep_state = sys.argv[1], sys.argv[2], set(sys.argv[3].split()), set(sys.argv[4].split())
dbs = [(p, keep_agent) for p in glob.glob(os.path.join(src, "agents/*/agent/openclaw-agent*.sqlite"))
       if re.fullmatch(r"openclaw-agent(\.[^.]+)?\.sqlite", os.path.basename(p))]
if os.path.exists(os.path.join(src, "state/openclaw.sqlite")):
    dbs.append((os.path.join(src, "state/openclaw.sqlite"), keep_state))
for path, keep in dbs:
    dst = os.path.join(out, os.path.relpath(path, src))
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    s, d = sqlite3.connect(path), sqlite3.connect(dst)
    s.backup(d)
    s.close()
    names = [r[0] for r in d.execute("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")]
    # Virtual (FTS) tables first: dropping one drops its shadow tables.
    virt = {r[0] for r in d.execute("SELECT name FROM sqlite_master WHERE type='table' AND sql LIKE 'CREATE VIRTUAL%'")}
    d.execute("PRAGMA foreign_keys=OFF")
    for t in sorted(virt) + [n for n in names if n not in virt]:
        if t not in keep and d.execute("SELECT 1 FROM sqlite_master WHERE name=?", (t,)).fetchone():
            d.execute(f'DROP TABLE "{t}"')
    for (t,) in d.execute("SELECT name FROM sqlite_master WHERE type='trigger'").fetchall():
        d.execute(f'DROP TRIGGER "{t}"')
    d.commit()
    d.execute("PRAGMA journal_mode=DELETE")
    d.execute("VACUUM")
    d.close()
    print(f"copied {os.path.relpath(path, src)} (tables kept: {' '.join(sorted(keep))})")
PY
}
gw() { oc gateway call "$1" --params "$2" --json --timeout 300000; }
# A session's post-turn maintenance (memory flush, cache bookkeeping) holds its writer for a few
# seconds after the CLI returns; a gateway call that needs the session idle retries until it is.
gw_idle() {
  local i
  for i in 1 2 3 4 5 6 7 8 9 10; do
    if "$BIN" gateway call "$1" --params "$2" --json --timeout 300000 </dev/null >>"$WORK/run.log" 2>&1; then
      echo "--- gateway call $1 (ok after $i)"; return 0
    fi
    sleep 4
  done
  echo "    (gateway call $1 failed 10 times)"
}
key_of() {  # the session key OpenClaw bound a session id to
  "$BIN" gateway call sessions.list --params '{"limit":200}' --json 2>/dev/null | python3 -I -c '
import json, sys
d = json.loads(sys.stdin.read() or "{}")
for s in d.get("sessions", []):
    if s.get("sessionId") == sys.argv[1]:
        print(s.get("key")); break' "$1"
}
new_session() {  # prints a fresh session id; on 2026.1.x it also resets main so the next turn opens it
  [ "$PER_ID" = 1 ] || gw_idle sessions.reset '{"key":"agent:main:main"}' >&2
  uuid
}
NEW="$(uuid)"  # resume target, reused at the end

if want chat; then      # 1 plain multi-turn chat (9 resume: the last scenario continues this session)
  S="$NEW"
  ask "$S" "Say hello in five words."
  ask "$S" "Now say it in French."
  ask "$S" "Which language did you use first? One word."
  SCEN+=("chat")
fi
if want tools; then     # 2 read/edit/shell, 3 parallel calls, 4 a tool error, 5 reasoning (every turn)
  S="$(new_session)"
  ask "$S" "Read a.txt and b.txt at the same time (two parallel tool calls), then reply with their combined line count."
  ask "$S" "Edit notes.txt to replace TODO with DONE, then run the shell command: cat notes.txt"
  ask "$S" "Run the shell command: cat missing-file.txt   and tell me the error in one line."
  ask "$S" "Run the shell command 'sleep 3; echo finished' in the background, then use the process tool to wait for it and report its output."
  SCEN+=("tools" "parallel_tools" "tool_error" "reasoning" "background_process")
fi
if want image; then     # 6 an image: the read/view tool returns the PNG as an image block
  S="$(new_session)"
  ask "$S" "Open the image file dot.png with your file-reading or image-viewing tool, then reply with the single word: received."
  SCEN+=("image_tool_result")
fi
if want subagent; then  # 7 sub-agents: sessions_spawn runs a child session; its result is announced back
  S="$(new_session)"
  ask "$S" "Use the sessions_spawn tool to spawn one sub-agent with the task 'Reply with the single word ok.'. Then wait for it and tell me what it said."
  sleep 20   # the child run and its announce turn in the parent finish after the CLI returns
  SCEN+=("subagent")
fi
if want cron; then      # scheduled work: the cron tool adds a job; `cron run` fires it now
  S="$(new_session)"
  ask "$S" "Use the cron tool to add a job named corpus-ping that runs once, 10 minutes from now, as an isolated agent turn with the message 'Reply with the word pong.'. Then confirm in one line."
  JOB="$("$BIN" cron list --all --json 2>/dev/null || "$BIN" cron list --json 2>/dev/null)"
  JOB="$(printf '%s' "$JOB" | python3 -I -c '
import json, sys
t = sys.stdin.read(); d = json.loads(t[t.find("{"):] or "{}") if "{" in t else {}
jobs = d.get("jobs") or d.get("automations") or []
print(next((j.get("id") or j.get("jobId") for j in jobs if j.get("name") == "corpus-ping"), ""))')"
  if [ -n "$JOB" ]; then
    # 2026.1.x runs only due jobs unless --force; later versions run now by default (--due opts out).
    if "$BIN" cron run --help 2>&1 | grep -q -- '--force'; then oc cron run "$JOB" --force --expect-final --timeout 300000
    else oc cron run "$JOB" --expect-final --timeout 300000; fi
    sleep 15; SCEN+=("cron")
  else SKIP+=("cron: the model did not create the job"); fi
fi
if want memory; then    # memory tools over MEMORY.md, and web_fetch
  S="$(new_session)"
  ask "$S" "Search your memory for the project codename (memory_search, then memory_get if needed) and tell me what it is."
  # web fetch, kept local: the shim answers GET /v1/models (OpenClaw may refuse a loopback URL; the
  # refusal is a persisted tool error, which is fine)
  ask "$S" "Use the web_fetch tool to fetch http://127.0.0.1:$PORT/v1/models and tell me the model id it lists."
  SCEN+=("memory" "web_fetch")
fi
if want mcp; then       # 10 MCP tool call through the stdio server shared with the Codex generator
  S="$(new_session)"
  ask "$S" "Use the corpus MCP server's add tool to add 2 and 3, then say the result."
  SCEN+=("mcp")
fi
if want compact; then   # 8 compaction, model switch, reset
  S="$(new_session)"
  ask "$S" "List three prime numbers."
  if [ "$PER_ID" = 1 ]; then K="$(key_of "$S")"; else K="agent:main:main"; fi
  if [ -n "$K" ]; then
    # model switch: the tiny-window id (16k tokens, OpenClaw's minimum) makes the next turn compact
    gw_idle sessions.patch "{\"key\":\"$K\",\"model\":\"corpus/$MODEL-tiny\",\"thinkingLevel\":\"medium\"}"
    ask "$S" "Read a.txt, then list three more primes."
    # The explicit compaction (the /compact path). 2026.1.x can only truncate: keep the last
    # maxLines lines and archive the full file as <id>.jsonl.bak.<ts>. Later versions run a model
    # compaction (a `compaction` entry) when maxLines is absent, and still truncate when it is given.
    if [ "$PER_ID" = 1 ]; then gw_idle sessions.compact "{\"key\":\"$K\"}"
    else gw_idle sessions.compact "{\"key\":\"$K\",\"maxLines\":4}"; fi
    ask "$S" "Which primes have you listed so far?"
    gw_idle sessions.reset "{\"key\":\"$K\"}"                   # the /reset path: a new window under the key
    ask "$S" "Say ok."
    SCEN+=("compaction" "model_switch" "thinking_level_change" "reset")
  else
    SKIP+=("compaction: no session key for the session id")
  fi
fi
if want chat; then
  if [ "$PER_ID" = 1 ]; then ask "$NEW" "Repeat your French greeting."; SCEN+=("resume")
  else SKIP+=("resume: every 2026.1.x CLI turn continues the session its key points at (that is the chat scenario); an earlier session cannot be re-entered after a reset"); fi
fi
[ -z "$MCP_SKIP" ] || SKIP+=("$MCP_SKIP")

CALLS="$(wc -l <"$WORK/calls.jsonl" | tr -d ' ')"
echo "model calls: $CALLS"
kill "$GW_PID" 2>/dev/null || true; wait "$GW_PID" 2>/dev/null || true; GW_PID=""

copy_store
corpus_scrub "$OUT" --also "$VERSIONS/openclaw/$V=/opt/openclaw/$V"
corpus_manifest "$OUT" \
  "harness=\"openclaw\"" "version=\"$V\"" "generated=\"$(date -u +%Y-%m-%d)\"" \
  "model=\"$MODEL via Z.ai coding endpoint, through chat_shim.py (OpenClaw provider corpus, api openai-completions)\"" \
  "model_calls=$CALLS" \
  "scenarios=[$(printf '"%s",' "${SCEN[@]}" | sed 's/,$//')]" \
  "skipped=[$( [ ${#SKIP[@]} -gt 0 ] && printf '"%s",' "${SKIP[@]}" | sed 's/,$//')]" \
  "generator_commit=\"$(git -C "$HERE" rev-parse --short HEAD)\""
corpus_size_check "$OUT"
