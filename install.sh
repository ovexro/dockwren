#!/usr/bin/env bash
# Dockwren installer for Debian 13 / Ubuntu 24.04+ (x86_64 or aarch64).
#
#   curl -fsSL <url>/install.sh | sudo bash
#   sudo ./install.sh --binary ./dockwren        # use a local binary (not checked: you supplied it)
#
# The downloaded binary is always checked against the SHA256SUMS of the same GitHub release.
# Env: DOCKWREN_URL        binary download URL (default: latest GitHub release for this arch);
#                          with a custom URL you must also set DOCKWREN_SHA256
#      DOCKWREN_SHA256     expected sha256 of the binary (overrides the release's SHA256SUMS)
#      DOCKWREN_NO_VERIFY  set to 1 to install a custom DOCKWREN_URL without a checksum (not advised)
#      DOCKWREN_BIND       listen address (default 127.0.0.1:8080)
#      NO_COLOR            set to get plain output (also used automatically when not a terminal)
set -euo pipefail

BINARY=""
while [ $# -gt 0 ]; do
  case "$1" in
    --binary) BINARY="${2:?--binary needs a path}"; shift 2 ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

# ---------------------------------------------------------------- output helpers
TOTAL=8
STEP=0
SP=""
FANCY=0
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-dumb}" != dumb ]; then FANCY=1; fi
case "${LC_ALL:-${LC_CTYPE:-${LANG:-}}}" in *[Uu][Tt][Ff]-8*|*[Uu][Tt][Ff]8*) UTF=1 ;; *) UTF=0 ;; esac
if [ "$FANCY" = 1 ]; then
  G=$'\033[32m'; R=$'\033[31m'; D=$'\033[2m'; B=$'\033[1m'; N=$'\033[0m'
else
  G=""; R=""; D=""; B=""; N=""
fi
if [ "$UTF" = 1 ]; then
  OKM="✔"; BADM="✘"; FILL="█"; EMPTY="░"
  FR=(⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏)
else
  OKM="ok"; BADM="!!"; FILL="#"; EMPTY="-"
  FR=("|" "/" "-" "\\")
fi

bar() { # percent -> fixed-width bar
  local f=$(( $1 * 20 / 100 )) i s=""
  for ((i = 0; i < 20; i++)); do
    if [ "$i" -lt "$f" ]; then s+="$FILL"; else s+="$EMPTY"; fi
  done
  printf '%s' "$s"
}

spinner() { # label percent (runs in the background until killed)
  local i=0 t0=$SECONDS
  while :; do
    printf '\r\033[K  %s%s%s %s  %s%s %3d%%%s  %ds' \
      "$B" "${FR[i % ${#FR[@]}]}" "$N" "$1" "$D" "$(bar "$2")" "$2" "$N" $((SECONDS - t0))
    i=$((i + 1))
    sleep 0.1
  done
}

stop_spinner() {
  if [ -n "$SP" ]; then
    kill "$SP" 2>/dev/null || true
    wait "$SP" 2>/dev/null || true
    SP=""
    printf '\r\033[K'
  fi
}

die() { stop_spinner; printf '%serror:%s %s\n' "$R" "$N" "$*" >&2; exit 1; }

ok_line() { # label seconds [note]
  if [ "$FANCY" = 1 ]; then
    printf '  %s%s%s %s%s%s\n' "$G" "$OKM" "$N" "$1" "${3:+  $D$3$N}" ""
  else
    printf ' %s%s\n' "${3:-ok}" " ($2s)"
  fi
}

begin() { STEP=$((STEP + 1)); LABEL="[$STEP/$TOTAL] $1"; T0=$SECONDS; }

fail_step() {
  stop_spinner
  if [ "$FANCY" = 1 ]; then printf '  %s%s%s %s\n' "$R" "$BADM" "$N" "$LABEL" >&2; else printf ' FAILED\n' >&2; fi
  printf '\n%s--- last lines of the log (%s) ---%s\n' "$D" "$LOG" "$N" >&2
  tail -n 25 "$LOG" >&2
  exit 1
}

# run "title" cmd args... : runs cmd quietly (output to the log) with a spinner and progress bar.
run() {
  begin "$1"; shift
  local pct=$(( (STEP - 1) * 100 / TOTAL )) rc
  if [ "$FANCY" = 1 ]; then
    spinner "$LABEL" "$pct" &
    SP=$!
  else
    printf '%s ...' "$LABEL"
  fi
  printf '### %s\n' "$LABEL" >>"$LOG"
  set +e
  ( set -e; "$@" ) >>"$LOG" 2>&1 </dev/null
  rc=$?
  set -e
  [ "$rc" -eq 0 ] || fail_step
  stop_spinner
  if [ "$FANCY" = 1 ]; then ok_line "$LABEL" "$((SECONDS - T0))" "$((SECONDS - T0))s"; else ok_line "$LABEL" "$((SECONDS - T0))"; fi
}

skip() { # title : step not needed
  begin "$1"
  if [ "$FANCY" = 1 ]; then ok_line "$LABEL" 0 "already there"; else printf '%s ... already there\n' "$LABEL"; fi
}

# ---------------------------------------------------------------- pre-flight
[ "$(id -u)" -eq 0 ] || die "run as root (sudo)"
case "$(uname -m)" in
  x86_64) ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  *) die "unsupported CPU $(uname -m)" ;;
esac
BIND="${DOCKWREN_BIND:-127.0.0.1:8080}"
LOG="$(mktemp /tmp/dockwren-install.XXXXXX.log)"
TMP="$(mktemp -d)"
cleanup() { stop_spinner; rm -rf "${TMP:?}"; }
trap cleanup EXIT
export DEBIAN_FRONTEND=noninteractive

if [ "$FANCY" = 1 ]; then
  printf '\n  %sDockwren installer%s  %s(%s)%s\n\n' "$B" "$N" "$D" "$ARCH" "$N"
else
  printf 'Dockwren installer (%s)\n' "$ARCH"
fi

# ---------------------------------------------------------------- steps
check_system() {
  [ -r /etc/os-release ] || die "cannot detect the OS"
  # shellcheck disable=SC1091
  . /etc/os-release
  case "${ID:-}" in
    debian|ubuntu) ;;
    *) die "unsupported OS '${ID:-?}': Debian 13 or Ubuntu 24.04+ required" ;;
  esac
  command -v systemctl >/dev/null || die "systemd is required"
  if [ -z "$BINARY" ] && [ -n "${DOCKWREN_URL:-}" ] && [ -z "${DOCKWREN_SHA256:-}" ] && [ "${DOCKWREN_NO_VERIFY:-0}" != 1 ]; then
    die "DOCKWREN_URL needs DOCKWREN_SHA256 too (or DOCKWREN_NO_VERIFY=1 to skip the check)"
  fi
}

install_prereqs() {
  apt-get update -qq
  apt-get install -y -qq "${need_pkgs[@]}"
}

install_docker() {
  if ! command -v docker >/dev/null; then
    curl -fsSL https://get.docker.com | sh
  fi
  if ! docker compose version >/dev/null 2>&1; then
    apt-get update -qq
    apt-get install -y -qq docker-compose-plugin
  fi
}

make_user() {
  if ! id dockwren >/dev/null 2>&1; then
    useradd --system --home-dir /var/lib/dockwren --shell /usr/sbin/nologin dockwren
  fi
  usermod -aG docker dockwren
}

copy_local() {
  [ -f "$BINARY" ] || die "binary not found: $BINARY"
  cp "$BINARY" "$TMP/dockwren"
}

verify_binary() {
  local want="" name="dockwren-${ARCH}-linux"
  if [ -n "$BINARY" ]; then
    echo "local binary supplied with --binary: not checked against a release"
  elif [ -n "${DOCKWREN_SHA256:-}" ]; then
    want="$DOCKWREN_SHA256"
  elif [ -z "${DOCKWREN_URL:-}" ]; then
    curl -fsSL "https://github.com/ovexro/dockwren/releases/latest/download/SHA256SUMS" -o "$TMP/SHA256SUMS" \
      || die "could not download SHA256SUMS from the release"
    want="$(awk -v n="$name" '$2 == n || $2 == "*" n { print $1 }' "$TMP/SHA256SUMS")"
    [ -n "$want" ] || die "$name is not listed in the release SHA256SUMS"
  else
    echo "WARNING: DOCKWREN_NO_VERIFY=1, binary is NOT verified"
  fi
  if [ -n "$want" ]; then
    echo "${want}  $TMP/dockwren" | sha256sum -c - || die "sha256 mismatch: the download is not the published binary"
  fi
  chmod 755 "$TMP/dockwren"
  if "$TMP/dockwren" nosuchcommand >/dev/null 2>&1; then die "binary did not behave like dockwren"; fi
}

setup_service() {
  install -m 755 "$TMP/dockwren" /usr/local/bin/dockwren
  cat > /etc/systemd/system/dockwren.service <<UNIT
[Unit]
Description=Dockwren panel
After=network-online.target docker.service
Wants=network-online.target

[Service]
User=dockwren
SupplementaryGroups=docker
ExecStart=/usr/local/bin/dockwren
Environment=DOCKWREN_DATA=/var/lib/dockwren
Environment=DOCKWREN_BIND=${BIND}
StateDirectory=dockwren
StateDirectoryMode=0700
Restart=on-failure
RestartSec=3
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectKernelLogs=true
ProtectControlGroups=true
ProtectClock=true
ProtectHostname=true
ProtectProc=invisible
RestrictNamespaces=true
RestrictRealtime=true
RestrictSUIDSGID=true
LockPersonality=true
MemoryDenyWriteExecute=true
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK
SystemCallArchitectures=native
SystemCallFilter=@system-service
SystemCallFilter=~@privileged @resources
SystemCallErrorNumber=EPERM
CapabilityBoundingSet=

[Install]
WantedBy=multi-user.target
UNIT
  # Lets Settings > General update the panel: the button restarts the service and this root-run
  # hook then downloads the newest release itself and checks its SHA256 (see src/selfupdate.rs).
  mkdir -p /etc/systemd/system/dockwren.service.d
  cat > /etc/systemd/system/dockwren.service.d/self-update.conf <<'DROPIN'
[Service]
TimeoutStartSec=300
ExecStartPre=-+/usr/local/bin/dockwren self-update --on-start
DROPIN
  systemctl daemon-reload
  systemctl enable dockwren.service
  systemctl restart dockwren.service
}

wait_panel() {
  local _
  for _ in $(seq 1 30); do
    if curl -fsS "http://${BIND}/health" >/dev/null 2>&1; then return 0; fi
    sleep 1
  done
  echo "panel did not come up; see: journalctl -u dockwren -n 50"
  return 1
}

# ---------------------------------------------------------------- run
run "Checking the system" check_system

need_pkgs=()
command -v curl >/dev/null || need_pkgs+=(curl)
command -v tar >/dev/null || need_pkgs+=(tar)
[ -f /etc/ssl/certs/ca-certificates.crt ] || need_pkgs+=(ca-certificates)
if [ ${#need_pkgs[@]} -gt 0 ]; then
  run "Installing ${need_pkgs[*]}" install_prereqs
else
  skip "Prerequisites"
fi

if command -v docker >/dev/null && docker compose version >/dev/null 2>&1; then
  skip "Docker and Compose"
else
  run "Installing Docker (can take a few minutes)" install_docker
fi
systemctl enable --now docker >/dev/null 2>&1 || true

run "Creating the dockwren system user" make_user

if [ -n "$BINARY" ]; then
  run "Copying the local binary" copy_local
else
  URL="${DOCKWREN_URL:-https://github.com/ovexro/dockwren/releases/latest/download/dockwren-${ARCH}-linux}"
  begin "Downloading Dockwren"
  printf '### %s\n%s\n' "$LABEL" "$URL" >>"$LOG"
  if [ "$FANCY" = 1 ]; then
    printf '  %s%s%s\n' "$B" "$LABEL" "$N"
    if curl -fL# "$URL" -o "$TMP/dockwren"; then rc=0; else rc=$?; fi
    printf '\r\033[K\033[A\033[K'
  else
    printf '%s ...' "$LABEL"
    if curl -fsSL "$URL" -o "$TMP/dockwren"; then rc=0; else rc=$?; fi
  fi
  [ "$rc" -eq 0 ] || die "download failed (set DOCKWREN_URL or use --binary)"
  size="$(( $(stat -c %s "$TMP/dockwren") / 1048576 )) MB"
  if [ "$FANCY" = 1 ]; then ok_line "$LABEL" "$((SECONDS - T0))" "$size, $((SECONDS - T0))s"; else ok_line "$LABEL" "$((SECONDS - T0))" "ok, $size"; fi
fi

run "Verifying the binary (SHA256)" verify_binary
run "Installing the service" setup_service
run "Starting the panel" wait_panel

# ---------------------------------------------------------------- summary
PW="$(journalctl -u dockwren --no-pager -o cat 2>/dev/null | grep -A1 'generated admin password' | tail -1 | tr -d ' ' || true)"
VER="$(/usr/local/bin/dockwren --version 2>/dev/null || true)"
rm -f "$LOG"
printf '\n  %s%s %s is running on %s%s\n\n' "$B" "$OKM" "${VER:-Dockwren}" "$BIND" "$N"
if [ -n "$PW" ]; then
  printf '    admin password  %s%s%s  (shown once, not stored)\n' "$B" "$PW" "$N"
else
  echo "    admin password already set; to reset:"
  echo "      sudo -u dockwren env DOCKWREN_DATA=/var/lib/dockwren dockwren set-password"
fi
case "$BIND" in
  127.0.0.1:*|localhost:*)
    echo
    echo "    it listens locally only; from your computer run:"
    echo "      ssh -L ${BIND##*:}:127.0.0.1:${BIND##*:} <user>@<this-server>"
    echo "    then open http://localhost:${BIND##*:}" ;;
esac
echo
