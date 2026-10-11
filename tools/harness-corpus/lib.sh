# tools/harness-corpus/lib.sh — sourced by every <harness>/generate.sh. Bash only.
#
#   corpus_work                  -> sets WORK (a fresh temp dir) and CORPUS_HOME=$WORK/home (the
#                                   throwaway $HOME; the project lives at $CORPUS_HOME/project)
#   corpus_guard VAR...          -> refuse unless each named env var is set, non-empty, inside $WORK,
#                                   and not ember's real home or anything under it
#   corpus_load_key VAR FILE     -> export VAR from FILE without ever printing it
#   corpus_scrub DIR [--also FROM=TO ...]
#                                -> rewrite $WORK paths to /home/corpus in every file (sqlite too),
#                                   then fail on anything that looks like a secret or a real path
#   corpus_size_check DIR        -> warn over 300 KB, fail over 1 MB
#   corpus_manifest DIR k=v...   -> write DIR/MANIFEST.toml

CORPUS_LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REAL_HOME="$(cd ~ && pwd -P)"

corpus_work() {
  WORK="$(mktemp -d "${TMPDIR:-/tmp}/cv-corpus.XXXXXX")"
  WORK="$(cd "$WORK" && pwd -P)"
  CORPUS_HOME="$WORK/home"
  mkdir -p "$CORPUS_HOME/project"
  export WORK CORPUS_HOME
}

corpus_guard() {
  local var val real
  for var in "$@"; do
    val="${!var:-}"
    [ -n "$val" ] || { echo "refusing: \$$var is unset" >&2; exit 1; }
    mkdir -p "$val"
    real="$(cd "$val" && pwd -P)"
    case "$real" in
      "$WORK"/*|"$WORK") ;;
      *) echo "refusing: \$$var=$real is outside the throwaway dir $WORK" >&2; exit 1 ;;
    esac
    case "$real/" in
      "$REAL_HOME"/*) case "$WORK" in "$REAL_HOME"/*) ;; *) echo "refusing: \$$var under the real home" >&2; exit 1 ;; esac ;;
    esac
  done
}

corpus_load_key() {
  local var="$1" file="$2" k
  [ -r "$file" ] || { echo "no key file $file" >&2; exit 1; }
  IFS= read -r k <"$file" || true
  [ -n "$k" ] || { echo "key file $file is empty" >&2; exit 1; }
  printf -v "$var" '%s' "$k"
  export "${var?}"
  CORPUS_KEY_FILES="${CORPUS_KEY_FILES:+$CORPUS_KEY_FILES:}$file"
  export CORPUS_KEY_FILES
}

corpus_scrub() {
  CORPUS_REAL_HOME="$REAL_HOME" python3 -I "$CORPUS_LIB_DIR/scrub.py" --work "$WORK" --home "$CORPUS_HOME" --dir "$@"
}

corpus_size_check() {
  local kb
  # Bytes as git stores them, not `du` blocks: a store of 150 small files is mostly block padding.
  kb="$(python3 -I -c 'import os,sys; print(sum(os.path.getsize(os.path.join(d,f)) for d,_,fs in os.walk(sys.argv[1]) for f in fs) // 1024)' "$1")"
  echo "corpus size: ${kb} KB ($1)"
  [ "$kb" -le 1024 ] || { echo "over the 1 MB hard cap — trim the scenario prompts" >&2; exit 1; }
  [ "$kb" -le 300 ] || echo "warning: over the 300 KB target" >&2
}

corpus_manifest() {
  local dir="$1"; shift
  {
    echo "# Written by tools/harness-corpus. Never regenerate a committed version in place."
    local kv
    for kv in "$@"; do
      printf '%s = %s\n' "${kv%%=*}" "${kv#*=}"
    done
  } >"$dir/MANIFEST.toml"
}
