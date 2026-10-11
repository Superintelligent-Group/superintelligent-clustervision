#!/usr/bin/env bash
# Install one exact Kimi Code version (npm @moonshot-ai/kimi-code) into
# ~/tools/harness-versions/kimi-code/<version>/ (persistent, idempotent). Never touches the Homebrew
# `kimi-code` formula or ~/.kimi-code.
#
#   tools/harness-corpus/kimi-code/install.sh <version>      # e.g. 2.1.1
#
# Prints the path of the installed `kimi` binary on stdout.
set -euo pipefail
V="${1:?usage: install.sh <version>}"
case "$V" in *[!0-9a-z.-]*) echo "bad version: $V" >&2; exit 2 ;; esac
PREFIX="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/kimi-code/$V"
BIN="$PREFIX/node_modules/.bin/kimi"
if [ ! -x "$BIN" ] || ! "$BIN" --version 2>/dev/null | grep -qxF "$V"; then
  command -v npm >/dev/null || { echo "npm not on PATH" >&2; exit 1; }
  mkdir -p "$PREFIX"
  npm install --prefix "$PREFIX" --no-audit --no-fund --loglevel=error "@moonshot-ai/kimi-code@$V" >&2
fi
"$BIN" --version | grep -qxF "$V" || { echo "installed $BIN does not report $V" >&2; exit 1; }
echo "$BIN"
