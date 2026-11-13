#!/usr/bin/env bash
# Build a static (musl) release binary for this machine's CPU into dist/ with a checksum.
# Runs on any Linux distro regardless of glibc. Needs: rustup target add <arch>-unknown-linux-musl
# Cross-builds (aarch64) are done on an aarch64 host for now.
set -euo pipefail
cd "$(dirname "$0")/.."
case "$(uname -m)" in
  x86_64) A=x86_64 ;;
  aarch64) A=aarch64 ;;
  *) echo "unsupported CPU" >&2; exit 1 ;;
esac
T="${A}-unknown-linux-musl"
cargo build --release --target "$T"
mkdir -p dist
cp "target/$T/release/dockwren" "dist/dockwren-${A}-linux"
(cd dist && sha256sum "dockwren-${A}-linux" > "SHA256SUMS-${A}")
echo "built dist/dockwren-${A}-linux (static, no glibc requirement)"
