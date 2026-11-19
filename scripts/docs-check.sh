#!/usr/bin/env bash
# Check that the public docs match the code. Run before every release tag (see BACKLOG.md ## Standing).
# Read-only. Exit 1 when something is out of date. Needs: grep, and `gh` for the GitHub checks (skipped if absent).
set -euo pipefail
cd "$(dirname "$0")/.."
bad=0
fail() { printf 'STALE: %s\n' "$*"; bad=1; }

ver=$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)

# 1. Other version numbers written in the docs (v0.1.N) must be the current one.
while IFS= read -r l; do fail "version in docs is not $ver: $l"; done < <(
  grep -noE 'v0\.1\.[0-9]+' README.md docs/*.md SECURITY.md site/index.html 2>/dev/null | grep -v "v$ver\$" || true)

# 2. Every bundled app (apps/<id>/manifest.toml) is named in the README app list and on the site.
for d in apps/*/; do
  id=$(basename "$d"); [ "$id" = custom ] && continue
  [ -f "$d/manifest.toml" ] || continue
  n=$(grep -m1 '^name = ' "$d/manifest.toml" | cut -d'"' -f2)
  grep -q -- "$n" README.md || fail "app '$n' missing from the README app list"
  grep -q -- "$n" site/index.html || fail "app '$n' missing from site/index.html"
done

# 3. Every CLI subcommand in the usage string appears in the README CLI line.
usage=$(grep -m1 '^const USAGE' src/main.rs)
cli=$(grep -m1 '^CLI:' README.md || true)
for w in $(printf '%s' "$usage" | sed -E 's/^[^[]*\[//; s/\]";?$//' | tr '|' '\n' | awk '{print $1}'); do
  case "$w" in -*) continue ;; esac
  printf '%s' "$cli" | grep -q -- "$w" || fail "CLI command '$w' missing from the README CLI line"
done

# 4. install.sh: options and env vars it documents are in the README, and the README names no option it lacks.
for o in $(grep -oE '^\s+--[a-z-]+\)' install.sh | tr -d ' )'); do
  grep -q -- "$o" README.md || fail "install.sh option $o not described in the README"
done
quick=$(sed -n '/^## Quick start/,/^## /p' README.md)
for o in $(printf '%s' "$quick" | grep -oE -- '`--[a-z-]+`' | tr -d '`' | sort -u); do
  grep -q -- "$o)" install.sh || grep -q -- "$o" install.sh || fail "README mentions $o, install.sh has no such option"
done

# 5. Every env var in the README / docs/OPERATIONS.md table is read by the code.
for v in $(grep -hoE '^\| `DOCKWREN_[A-Z0-9_]+`' README.md docs/OPERATIONS.md | tr -d '|` '); do
  grep -rq -- "$v" src/ || fail "README documents $v but src/ never reads it"
done

# 6. Images and relative links in the README exist.
for f in $(grep -oE '(src|href)="[^"#:]+"|\]\([^)#:]+\)' README.md | sed -E 's/^(src|href)="//; s/^\]\(//; s/["\)]$//' | sort -u); do
  [ -e "$f" ] || fail "README links to $f which does not exist"
done

# 7. Nothing tracked names a real host or private IP (memory no-hostnames-in-git).
if git grep -nE '(^|[^0-9.])(192\.168\.|10\.[0-9]+\.[0-9]+\.|100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.|172\.(1[6-9]|2[0-9]|3[01])\.)[0-9]+\.[0-9]+|neo50q' -- . ':!Cargo.lock' ':!*.svg' ':!scripts/docs-check.sh' | grep -vE '[0-9]/[0-9]+'; then
  fail "private IP or real hostname found in tracked files (above)"; fi

# 8a. Every feature keyword in scripts/features.txt is mentioned in the README and on the site
# (a new feature is added to features.txt in the same release, so the public pages cannot lag behind).
while IFS= read -r k; do
  case "$k" in ''|'#'*) continue ;; esac
  cat README.md docs/FEATURES.md | grep -qiF -- "$k" || fail "feature '$k' (scripts/features.txt) is not in the README or docs/FEATURES.md"
  grep -qiF -- "$k" site/index.html || fail "feature '$k' (scripts/features.txt) is not on the site"
done < scripts/features.txt

# 8. GitHub description, topics and license.
if command -v gh >/dev/null && repo=$(gh repo view --json nameWithOwner -q .nameWithOwner 2>/dev/null); then
  info=$(gh repo view "$repo" --json description,repositoryTopics,licenseInfo -q '[.description, (.repositoryTopics|length), .licenseInfo.key] | @tsv')
  d=$(printf '%s' "$info" | cut -f1); t=$(printf '%s' "$info" | cut -f2); l=$(printf '%s' "$info" | cut -f3)
  [ -n "$d" ] || fail "GitHub description is empty"
  [ "${t:-0}" -ge 5 ] || fail "GitHub has fewer than 5 topics"
  [ "$l" = apache-2.0 ] || fail "GitHub license is '$l', expected apache-2.0"
  printf '%s' "$d" | grep -qi https || fail "GitHub description does not mention HTTPS"
fi

[ "$bad" = 0 ] && echo "docs-check: all consistent for v$ver" || { echo "docs-check: fix the items above, then run again"; exit 1; }
