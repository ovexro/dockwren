#!/usr/bin/env bash
# Build the release binary for this machine's CPU into dist/ with a checksum.
# Cross-builds (aarch64) are done on an aarch64 host for now.
set -euo pipefail
cd "$(dirname "$0")/.."
case "$(uname -m)" in
  x86_64) A=x86_64 ;;
  aarch64) A=aarch64 ;;
  *) echo "unsupported CPU" >&2; exit 1 ;;
esac
cargo build --release
mkdir -p dist
cp target/release/dockwren "dist/dockwren-${A}-linux"
(cd dist && sha256sum "dockwren-${A}-linux" > "SHA256SUMS-${A}")
echo "built dist/dockwren-${A}-linux (needs glibc >= 2.39: Debian 13, Ubuntu 24.04+)"
