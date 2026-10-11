#!/usr/bin/env bash
# Install one exact Grok CLI version into ~/tools/harness-versions/grok/<version>/ (persistent,
# idempotent). Never touches ~/.grok (where the official installer puts its binary and symlinks).
#
#   tools/harness-corpus/grok/install.sh <version>      # e.g. 1.0.50
#
# Fetches the same artifact the official installer (https://x.ai/cli/install.sh) does,
# `<base>/grok-<version>-<os>-<arch>[.zst]`, from x.ai or its GCS fallback. No checksum is
# published; the binary must report the version. Prints the path of the binary on stdout.
set -euo pipefail
V="${1:?usage: install.sh <version>}"
case "$V" in *[!0-9a-zA-Z.-]*) echo "bad version: $V" >&2; exit 2 ;; esac
PREFIX="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/grok/$V"
BIN="$PREFIX/grok"
if [ ! -x "$BIN" ] || ! "$BIN" --version </dev/null 2>/dev/null | grep -qF "grok $V "; then
  case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) PLAT=macos-aarch64 ;; Darwin-x86_64) PLAT=macos-x86_64 ;;
    Linux-aarch64) PLAT=linux-aarch64 ;; Linux-x86_64) PLAT=linux-x86_64 ;;
    *) echo "unsupported platform $(uname -s)-$(uname -m)" >&2; exit 1 ;;
  esac
  mkdir -p "$PREFIX"
  TMP="$PREFIX/.download.$$"
  trap 'rm -f "$TMP" "$TMP.zst"' EXIT
  ok=0
  for base in https://x.ai/cli https://storage.googleapis.com/grok-build-public-artifacts/cli; do
    if command -v zstd >/dev/null && curl -fsSL -o "$TMP.zst" "$base/grok-$V-$PLAT.zst" && zstd -q -d -f -o "$TMP" "$TMP.zst"; then ok=1; break; fi
    if curl -fsSL -o "$TMP" "$base/grok-$V-$PLAT"; then ok=1; break; fi
  done
  [ "$ok" = 1 ] || { echo "could not download grok $V for $PLAT" >&2; exit 1; }
  chmod +x "$TMP" && mv -f "$TMP" "$BIN"
fi
"$BIN" --version </dev/null | grep -qF "grok $V " || { echo "installed $BIN does not report $V" >&2; exit 1; }
echo "$BIN"
