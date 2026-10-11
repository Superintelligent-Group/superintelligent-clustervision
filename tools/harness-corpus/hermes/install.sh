#!/usr/bin/env bash
# Install one exact Hermes Agent release into ~/tools/harness-versions/hermes/<version>/ (persistent,
# idempotent). Never touches the Homebrew install or ~/.hermes.
#
#   tools/harness-corpus/hermes/install.sh <version>      # e.g. 2026.8.31, 0.21.6
#
# Hermes refuses to build a wheel outside Nix (setup.py), and its old PyPI releases stop at 0.19, so
# this is its supported source-checkout layout: `git archive v<version>` from the ~/pug/hermes-agent
# checkout (HERMES_SRC overrides; the checkout itself is only read) into <prefix>/src, then
# `uv sync --frozen --no-dev --extra mcp` (an editable install from the release's own uv.lock, plus
# the MCP client the corpus MCP scenario needs; without it Hermes connects zero servers and says
# nothing) into <prefix>/venv. Prints the path of the `hermes` binary on stdout.
set -euo pipefail
V="${1:?usage: install.sh <version>}"
case "$V" in *[!0-9a-z.-]*) echo "bad version: $V" >&2; exit 2 ;; esac
SRC_REPO="${HERMES_SRC:-$HOME/pug/hermes-agent}"
PREFIX="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/hermes/$V"
BIN="$PREFIX/venv/bin/hermes"
command -v uv >/dev/null || { echo "uv not on PATH" >&2; exit 1; }
if [ ! -x "$BIN" ]; then
  rm -rf "$PREFIX/src" && mkdir -p "$PREFIX/src"
  git -C "$SRC_REPO" rev-parse -q --verify "refs/tags/v$V" >/dev/null \
    || { echo "no tag v$V in $SRC_REPO (git -C $SRC_REPO fetch --tags)" >&2; exit 1; }
  git -C "$SRC_REPO" archive "v$V" | tar -x -C "$PREFIX/src"
  git -C "$SRC_REPO" rev-parse --short=10 "v$V^{commit}" >"$PREFIX/COMMIT"
fi
if [ ! -x "$BIN" ] || ! "$PREFIX/venv/bin/python" -c 'import mcp' 2>/dev/null; then
  # The release's requires-python cap picks the interpreter (2026.8.31: <3.14; 0.21.x: 3.14).
  PY="$(grep -E '^requires-python' "$PREFIX/src/pyproject.toml" | grep -q '<3.14' && echo 3.13 || echo 3.14)"
  (cd "$PREFIX/src" && UV_PROJECT_ENVIRONMENT="$PREFIX/venv" uv sync --frozen --no-dev --extra mcp --python "$PY" >&2)
fi
[ -x "$BIN" ] || { echo "install failed: no $BIN" >&2; exit 1; }
echo "$BIN"
