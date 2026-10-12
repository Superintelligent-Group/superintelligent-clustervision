# tools/harness-corpus/drive.sh — sourced after lib.sh by generators that share the scratch project.
#
#   corpus_project                      -> the scratch project at $CORPUS_HOME/project: a git repo with
#                                          a.txt, b.txt, notes.txt and a 1x1 dot.png
#   toml_list ITEM...                   -> a TOML string array, [] when empty

corpus_project() {
  (
    cd "$CORPUS_HOME/project"
    printf 'alpha\nbeta\n' >a.txt; printf 'gamma\n' >b.txt; printf 'TODO: fix\n' >notes.txt
    python3 -I -c 'import base64,sys; sys.stdout.buffer.write(base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg=="))' >dot.png
    git init -q -b main . && git -c user.name=corpus -c user.email=corpus@example.invalid add -A \
      && git -c user.name=corpus -c user.email=corpus@example.invalid commit -qm init
  )
}

# toml_list ITEM... -> ["a","b"] (and [] for no items), for corpus_manifest
toml_list() {
  local out="" x
  for x in "$@"; do out="$out${out:+,}\"$x\""; done
  printf '[%s]' "$out"
}
