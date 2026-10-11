#!/usr/bin/env bash
# Install one exact Roo Code CLI release (GitHub RooCodeInc/Roo-Code, tag cli-v<version>; it bundles
# the Roo Code extension and runs it in a mock VS Code host) into
# ~/tools/harness-versions/roo/<version>/ (persistent, idempotent). Same steps as the upstream
# apps/cli/install.sh, minus the ~/.local/bin symlink.
#
#   tools/harness-corpus/roo/install.sh <version>      # e.g. 0.1.17
#
# Needs gh (release download), npm, node >= 20, shasum. Prints the `roo` binary path.
set -euo pipefail
V="${1:?usage: install.sh <version>}"
case "$V" in *[!0-9a-z.-]*) echo "bad version: $V" >&2; exit 2 ;; esac
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) PLAT=darwin-arm64 ;; Linux-x86_64) PLAT=linux-x64 ;; Linux-aarch64) PLAT=linux-arm64 ;;
  *) echo "no Roo CLI build for $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac
PREFIX="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/roo/$V"
DIR="$PREFIX/roo-cli-$PLAT"
BIN="$DIR/bin/roo"
if [ ! -x "$BIN" ] || ! "$BIN" --version 2>/dev/null | grep -qxF "$V"; then
  mkdir -p "$PREFIX/dl"
  gh release download "cli-v$V" -R RooCodeInc/Roo-Code -D "$PREFIX/dl" --clobber -p "roo-cli-$PLAT.tar.gz*" >&2
  (cd "$PREFIX/dl" && shasum -a 256 -c "roo-cli-$PLAT.tar.gz.sha256" >&2)
  rm -rf "$DIR"; tar -xzf "$PREFIX/dl/roo-cli-$PLAT.tar.gz" -C "$PREFIX"
  # npm install replaces node_modules, so keep the bundled ripgrep across it (as upstream does).
  RG="$DIR/node_modules/@vscode/ripgrep/bin/rg"; cp "$RG" "$PREFIX/rg"
  (cd "$DIR" && npm install --production --no-audit --no-fund --loglevel=error >&2)
  mkdir -p "$(dirname "$RG")" && cp "$PREFIX/rg" "$RG" && chmod +x "$RG"
fi
"$BIN" --version | grep -qxF "$V" || { echo "installed $BIN does not report $V" >&2; exit 1; }
echo "$BIN"
