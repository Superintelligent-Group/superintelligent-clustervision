# tools/harness-corpus/drive.sh — sourced after lib.sh by generators whose harness already speaks
# the provider's wire (chat completions, Anthropic messages) and only needs the key kept out of it.
#
#   corpus_project                      -> the scratch project at $CORPUS_HOME/project: a git repo with
#                                          a.txt, b.txt, notes.txt and a 1x1 dot.png
#   corpus_proxy UPSTREAM AUTH [MODEL]  -> starts key_proxy.py on a free port with the key from
#                                          ${ZAI_KEY_FILE:-~/.zai-key}; sets PROXY_URL
#                                          (http://127.0.0.1:<port>), PROXY_PID, and CALLS_LOG
#   corpus_proxy_stop                   -> stops it (safe to call twice; put it in the EXIT trap)
#   corpus_calls                        -> the number of model calls made so far

corpus_proxy() {
  local upstream="$1" auth="$2" model="${3:-}" port
  port="$(python3 -I -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')"
  CALLS_LOG="$WORK/calls.jsonl"
  corpus_load_key UPSTREAM_KEY "${ZAI_KEY_FILE:-$REAL_HOME/.zai-key}"
  python3 -I "$CORPUS_LIB_DIR/key_proxy.py" --port "$port" --upstream "$upstream" --auth "$auth" \
    --log "$CALLS_LOG" --max-calls "${MAX_CALLS:-40}" ${model:+--model "$model"} &
  PROXY_PID=$!
  unset UPSTREAM_KEY
  local _
  for _ in $(seq 50); do (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null && break; sleep 0.1; done
  (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null || { echo "key_proxy did not start on $port" >&2; exit 1; }
  PROXY_URL="http://127.0.0.1:$port"
  export PROXY_URL CALLS_LOG
}

corpus_proxy_stop() {
  [ -z "${PROXY_PID:-}" ] || kill "$PROXY_PID" 2>/dev/null || true
  PROXY_PID=""
}

corpus_calls() {
  if [ -f "${CALLS_LOG:-}" ]; then wc -l <"$CALLS_LOG" | tr -d ' '; else echo 0; fi
}

corpus_project() {
  (
    cd "$CORPUS_HOME/project"
    printf 'alpha\nbeta\n' >a.txt; printf 'gamma\n' >b.txt; printf 'TODO: fix\n' >notes.txt
    python3 -I -c 'import base64,sys; sys.stdout.buffer.write(base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg=="))' >dot.png
    git init -q -b main . && git -c user.name=corpus -c user.email=corpus@example.invalid add -A \
      && git -c user.name=corpus -c user.email=corpus@example.invalid commit -qm init
  )
}
