#!/usr/bin/env bash
# Dockwren installer for Debian 13 / Ubuntu 24.04+ (x86_64 or aarch64).
#
#   curl -fsSL <url>/install.sh | sudo bash
#   sudo ./install.sh --binary ./dockwren        # use a local binary
#
# Env: DOCKWREN_URL   binary download URL (default: latest GitHub release for this arch)
#      DOCKWREN_SHA256  expected sha256 of the binary (verified when set)
#      DOCKWREN_BIND    listen address (default 127.0.0.1:8080)
set -euo pipefail

BINARY=""
while [ $# -gt 0 ]; do
  case "$1" in
    --binary) BINARY="${2:?--binary needs a path}"; shift 2 ;;
    -h|--help) sed -n 2,10p "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

say() { printf '==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "run as root (sudo)"
[ -r /etc/os-release ] || die "cannot detect the OS"
# shellcheck disable=SC1091
. /etc/os-release
case "${ID:-}" in
  debian|ubuntu) ;;
  *) die "unsupported OS '${ID:-?}': Debian 13 or Ubuntu 24.04+ required" ;;
esac
case "$(uname -m)" in
  x86_64) ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  *) die "unsupported CPU $(uname -m)" ;;
esac
command -v systemctl >/dev/null || die "systemd is required"

export DEBIAN_FRONTEND=noninteractive
need_pkgs=()
command -v curl >/dev/null || need_pkgs+=(curl)
command -v tar >/dev/null || need_pkgs+=(tar)
[ -f /etc/ssl/certs/ca-certificates.crt ] || need_pkgs+=(ca-certificates)
if [ ${#need_pkgs[@]} -gt 0 ]; then
  say "installing ${need_pkgs[*]}"
  apt-get update -qq
  apt-get install -y -qq "${need_pkgs[@]}"
fi

if ! command -v docker >/dev/null; then
  say "installing Docker (official convenience script)"
  curl -fsSL https://get.docker.com | sh
fi
if ! docker compose version >/dev/null 2>&1; then
  say "installing the Docker Compose plugin"
  apt-get update -qq
  apt-get install -y -qq docker-compose-plugin
fi
systemctl enable --now docker >/dev/null 2>&1 || true

if ! id dockwren >/dev/null 2>&1; then
  say "creating system user dockwren"
  useradd --system --home-dir /var/lib/dockwren --shell /usr/sbin/nologin dockwren
fi
usermod -aG docker dockwren

TMP="$(mktemp -d)"
trap 'rm -rf "${TMP:?}"' EXIT
if [ -n "$BINARY" ]; then
  [ -f "$BINARY" ] || die "binary not found: $BINARY"
  cp "$BINARY" "$TMP/dockwren"
else
  URL="${DOCKWREN_URL:-https://github.com/ovexro/dockwren/releases/latest/download/dockwren-${ARCH}-linux}"
  say "downloading $URL"
  curl -fsSL "$URL" -o "$TMP/dockwren" || die "download failed (set DOCKWREN_URL or use --binary)"
fi
if [ -n "${DOCKWREN_SHA256:-}" ]; then
  echo "${DOCKWREN_SHA256}  $TMP/dockwren" | sha256sum -c - >/dev/null || die "sha256 mismatch"
fi
chmod 755 "$TMP/dockwren"
"$TMP/dockwren" nosuchcommand >/dev/null 2>&1 && die "binary did not behave like dockwren"
install -m 755 "$TMP/dockwren" /usr/local/bin/dockwren

say "writing systemd unit"
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
Environment=DOCKWREN_BIND=${DOCKWREN_BIND:-127.0.0.1:8080}
StateDirectory=dockwren
StateDirectoryMode=0700
Restart=on-failure
RestartSec=3
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
systemctl enable dockwren.service >/dev/null
systemctl restart dockwren.service

BIND="${DOCKWREN_BIND:-127.0.0.1:8080}"
say "waiting for the panel"
for _ in $(seq 1 30); do
  if curl -fsS "http://${BIND}/health" >/dev/null 2>&1; then OK=1; break; fi
  sleep 1
done
[ "${OK:-0}" = 1 ] || die "panel did not come up; see: journalctl -u dockwren -n 50"

echo
say "Dockwren is running on ${BIND}"
PW="$(journalctl -u dockwren --no-pager -o cat 2>/dev/null | grep -A1 'generated admin password' | tail -1 | tr -d ' ' || true)"
if [ -n "$PW" ]; then
  echo "    admin password (shown once, not stored): $PW"
else
  echo "    admin password already set; to reset: sudo -u dockwren env DOCKWREN_DATA=/var/lib/dockwren dockwren set-password"
fi
case "$BIND" in
  127.0.0.1:*|localhost:*)
    echo "    it listens locally only; from your computer run:"
    echo "      ssh -L ${BIND##*:}:127.0.0.1:${BIND##*:} <user>@<this-server>"
    echo "    then open http://localhost:${BIND##*:}" ;;
esac
