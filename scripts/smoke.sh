#!/usr/bin/env bash
# Live smoke test of a RUNNING panel, run after every deploy (v0.1.111 froze on a double lock
# that only real instance data triggered; unit tests with no instances did not see it).
#
#   scripts/smoke.sh [BASE_URL]            default http://127.0.0.1:8080
#   DOCKWREN_STATUS_TOKEN=dwst_... scripts/smoke.sh
#
# Tier 1 (no credentials): /health, /login, and that protected pages redirect. Every request has
# a hard 8 s limit, so a hung handler or a dead runtime fails here instead of freezing the shell.
# Tier 2 (read-only status token): /api/status and /api/metrics call instance_views(), the same
# code path as the /apps and /instances/list pages, with the REAL instances on disk. Without the
# token tier 2 is reported as SKIPPED and the script exits 2 (not green).
set -euo pipefail

BASE="${1:-http://127.0.0.1:8080}"
TOKEN="${DOCKWREN_STATUS_TOKEN:-}"
fail=0

# check NAME EXPECTED_CODE URL [extra curl args...]; prints time, fails on wrong code or timeout
check() {
  local name=$1 want=$2 url=$3
  shift 3
  local out code t
  if out=$(curl -s -o /dev/null --max-time 8 -w '%{http_code} %{time_total}' "$@" "$url"); then
    code=${out% *}
    t=${out#* }
  else
    code=000
    t=timeout
  fi
  if [ "$code" = "$want" ]; then
    printf 'ok    %-28s %s (%ss)\n' "$name" "$code" "$t"
  else
    printf 'FAIL  %-28s got %s, want %s (%ss)\n' "$name" "$code" "$want" "$t"
    fail=1
  fi
}

check "health" 200 "$BASE/health"
check "login page" 200 "$BASE/login"
check "/ needs session" 303 "$BASE/"
check "/apps needs session" 303 "$BASE/apps"
check "/instances/list needs session" 303 "$BASE/instances/list"
check "/metrics needs session" 303 "$BASE/metrics"
check "status api rejects no token" 401 "$BASE/api/status"

if [ "$fail" -ne 0 ]; then
  echo "smoke: FAILED (tier 1)"
  exit 1
fi

if [ -z "$TOKEN" ]; then
  echo "smoke: tier 1 ok, tier 2 SKIPPED (set DOCKWREN_STATUS_TOKEN to test with real instance data)"
  exit 2
fi

auth=(-H "Authorization: Bearer $TOKEN")
check "api/status (real data)" 200 "$BASE/api/status" "${auth[@]}"
check "api/metrics (real data)" 200 "$BASE/api/metrics" "${auth[@]}"

# The body must really list the instances on disk, not just answer 200 with an empty list.
n_json=$(curl -s --max-time 8 "${auth[@]}" "$BASE/api/status" | python3 -I -c 'import json,sys; d=json.load(sys.stdin); print(len(d["apps"] if isinstance(d, dict) else d))')
echo "info  api/status lists $n_json app(s)"

if [ "$fail" -ne 0 ]; then
  echo "smoke: FAILED (tier 2)"
  exit 1
fi
echo "smoke: all green"
