#!/usr/bin/env bash
# Wait for the release workflow of a tag, then check the DRAFT is complete. Replaces ad-hoc
# Monitor conditions (2026-11-23: a hand-written "draft with >=4 assets" condition could never
# fire, the draft has 3 files until the user signs it, and nobody noticed for minutes).
#
#   scripts/wait-release.sh vX.Y.Z [TIMEOUT_SECONDS]      default 900
#
# Exit 0  every workflow run of the tag succeeded and the release has the expected files
# Exit 1  a workflow run failed (its failed log tail is printed) or files are missing
# Exit 3  timeout (state is printed, nothing is silent)
# Expected files before signing: both binaries + SHA256SUMS. SHA256SUMS.sig appears only after
# the user runs scripts/sign-release.sh, so it is reported but never required.
set -euo pipefail

TAG="${1:?usage: scripts/wait-release.sh vX.Y.Z [timeout-seconds]}"
LIMIT="${2:-900}"
case "$TAG" in v[0-9]*.[0-9]*.[0-9]*) ;; *) echo "wait-release: '$TAG' is not a vX.Y.Z tag" >&2; exit 1 ;; esac
START=$(date +%s)
elapsed() { echo $(( $(date +%s) - START )); }
say() { printf '[%4ss] %s\n' "$(elapsed)" "$*"; }

last=""
while :; do
  if [ "$(elapsed)" -ge "$LIMIT" ]; then
    say "TIMEOUT after ${LIMIT}s. Last state: ${last:-no workflow run seen for $TAG}"
    exit 3
  fi
  runs="$(gh run list --branch "$TAG" --limit 20 --json databaseId,workflowName,status,conclusion \
    --jq '.[] | [.databaseId, .workflowName, .status, .conclusion] | @tsv' 2>/dev/null || true)"
  if [ -z "$runs" ]; then
    state="no workflow run for $TAG yet (was the tag pushed?)"
  else
    state="$(printf '%s\n' "$runs" | awk -F'\t' '{printf "%s=%s%s ", $2, $3, ($4!=""?"/"$4:"")}')"
    pending="$(printf '%s\n' "$runs" | awk -F'\t' '$3!="completed"' | wc -l)"
    failed="$(printf '%s\n' "$runs" | awk -F'\t' '$3=="completed" && $4!="success"')"
    if [ -n "$failed" ]; then
      say "FAILED: $state"
      printf '%s\n' "$failed" | while IFS=$'\t' read -r id name _ concl; do
        echo "--- $name ($concl), run $id, tail of the failed log:"
        gh run view "$id" --log-failed 2>/dev/null | tail -25 || true
      done
      exit 1
    fi
    if [ "$pending" -eq 0 ]; then
      say "all workflow runs succeeded: $state"
      break
    fi
  fi
  if [ "$state" != "$last" ]; then say "$state"; last="$state"; fi
  left=$(( LIMIT - $(elapsed) )); [ "$left" -gt 10 ] && left=10; [ "$left" -gt 0 ] && sleep "$left"
done

info="$(gh release view "$TAG" --json isDraft,assets --jq '[.isDraft, (.assets | map(.name) | join(" "))] | @tsv' 2>/dev/null || true)"
if [ -z "$info" ]; then say "no release object for $TAG"; exit 1; fi
draft="$(printf '%s' "$info" | cut -f1)"
files="$(printf '%s' "$info" | cut -f2)"
missing=""
for f in dockwren-x86_64-linux dockwren-aarch64-linux SHA256SUMS; do
  case " $files " in *" $f "*) ;; *) missing="$missing $f" ;; esac
done
if [ -n "$missing" ]; then say "release $TAG is missing:$missing (has: $files)"; exit 1; fi
case " $files " in
  *" SHA256SUMS.sig "*) sig="signed" ;;
  *) sig="NOT signed yet (user runs scripts/sign-release.sh $TAG)" ;;
esac
say "release $TAG ready: draft=$draft, files: $files, $sig"
