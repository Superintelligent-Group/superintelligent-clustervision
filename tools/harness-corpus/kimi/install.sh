#!/usr/bin/env bash
# Install one exact legacy kimi-cli version (PyPI `kimi-cli`) into a venv at
# ~/tools/harness-versions/kimi/<version>/ (persistent, idempotent). Never touches the Homebrew
# `kimi-cli` formula or ~/.kimi.
#
#   tools/harness-corpus/kimi/install.sh <version>      # e.g. 1.51.0
#
# 1.52.0 (the final release) short-circuits every entry point to a Kimi Code installer and writes
# no sessions; 1.51.0 is the last version that runs. Prints the path of the installed `kimi`.
set -euo pipefail
V="${1:?usage: install.sh <version>}"
case "$V" in *[!0-9a-z.-]*) echo "bad version: $V" >&2; exit 2 ;; esac
PREFIX="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/kimi/$V"
BIN="$PREFIX/bin/kimi"
if [ ! -x "$BIN" ] || ! "$BIN" --version 2>/dev/null | grep -qF "$V"; then
  command -v uv >/dev/null || { echo "uv not on PATH" >&2; exit 1; }
  uv venv -q --python 3.13 "$PREFIX" >&2
  VIRTUAL_ENV="$PREFIX" uv pip install -q "kimi-cli==$V" >&2
fi
"$BIN" --version | grep -qF "$V" || { echo "installed $BIN does not report $V" >&2; exit 1; }
echo "$BIN"
