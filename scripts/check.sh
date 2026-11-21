#!/usr/bin/env bash
# Everything that must be green before a commit is pushed or a tag is made. Stops at the first failure.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --quiet
gitleaks detect --no-banner >/dev/null
scripts/docs-check.sh >/dev/null
echo "check: all green"
