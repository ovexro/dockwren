# Dockwren

A minimal, Docker-only control panel for your own server. Watch and control containers, install
apps from Compose templates, update them with automatic rollback, back up and restore their data,
and optionally publish them on a Cloudflare Tunnel hostname.

- Single static Rust binary (axum + bollard), server-rendered UI with a small amount of htmx
  (vendored, no CDN, no JS build step).
- Talks to the local Docker socket. No database, no agents.
- Bundled apps: Gitea, Memos, Miniflux, Paperless-ngx, Uptime Kuma, Vaultwarden, WordPress.
  Adding your own takes a manifest and a Compose file.
- Status: early (v0.1.x). It works and is tested on Debian 13 and Ubuntu 24.04 (x86_64) and on a
  Raspberry Pi 5 (aarch64), but expect rough edges.

License: Apache-2.0.

## Quick start

On a fresh Debian or Ubuntu server, as root (installs Docker if it is missing):

```sh
curl -fsSL https://raw.githubusercontent.com/ovexro/dockwren/main/install.sh | sudo bash
```

It downloads the latest release binary for your architecture (x86_64 or aarch64), creates a
`dockwren` system user and a systemd service, and prints the admin password **once**. The panel
listens on `127.0.0.1:8080` only; reach it with an SSH tunnel:

```sh
ssh -L 8080:127.0.0.1:8080 you@your-server
```

then open `http://localhost:8080`. Read the script before piping it to a shell; options are
described at its top (for example `--binary` to use a binary you copied yourself).

Release binaries and `SHA256SUMS` are on the
[releases page](https://github.com/ovexro/dockwren/releases).

## What works today

- Single-admin login: strong generated password, Argon2id hash, `HttpOnly` + `SameSite=Strict`
  session cookie, login rate limit, sessions that survive restarts.
- Overview: all containers (running or not) with image, state and status, refreshed every 5 s.
- Resource graphs: CPU, memory, network and disk space as plain one-colour lines (last 1 h, 24 h, 7 days or 30 days), plus CPU/memory per running container. Sampled every 10 s; the 1 h and 24 h views live in memory, while 5-minute averages are kept 30 days in `metrics.log` in the data directory and survive restarts. A disk above 85% is flagged.
- Start, stop (with confirmation) and restart a container; log viewer (last 200 lines, refreshed every 5 s).
- App catalog with icons and install forms (generated passwords, per-field hints).
- Updates with automatic rollback when the new version is not healthy, and optional migration
  steps per template.
- Backups (files plus database dumps) and restore, from the UI or the CLI; a safety snapshot is
  taken before a restore.
- Cloudflare Tunnel publishing with DNS conflict handling and optional Cloudflare Access (see below).
- `dockwren set-password` to rotate the admin password.

CLI: `dockwren [serve | set-password | install | backup | update | restore | cleanup | expose | unexpose | remove]`.

## Requirements

- Linux with Docker running and access to its socket (your user in the `docker` group, or run
  as a user that can read `/var/run/docker.sock`). `DOCKER_HOST` is honoured.
- Rust (stable, edition 2024) to build from source.

## Build from source

```sh
cargo build --release
DOCKWREN_DATA=./data ./target/release/dockwren
```

On the first run Dockwren generates an admin password, prints it **once** and stores only its
hash. Open `http://127.0.0.1:8080` and sign in with it.

## Configuration

All settings are environment variables.

| Variable | Default | Meaning |
| --- | --- | --- |
| `DOCKWREN_DATA` | `./data` | Directory for Dockwren's own state (`admin.hash`, `sessions`), created with mode 0700. |
| `DOCKWREN_BIND` | `127.0.0.1:8080` | Listen address. Local-only by default. |
| `DOCKWREN_DISKS` | unset | Extra directories (comma separated) whose disks get a graph; the data directory's disk is always shown. |
| `DOCKWREN_SECURE_COOKIE` | unset | Set to `1` when served over HTTPS so the session cookie is marked `Secure`. |

## Changing the password

```sh
DOCKWREN_DATA=/path/to/data dockwren set-password
```

Prints a new generated password once. A running server applies it immediately and signs every
session out. If you lose the password, run the same command.

## Public hostnames (Cloudflare Tunnel)

Each app has a "Public URL" button that records a hostname and shows the tunnel ingress rule to
add. To let Dockwren create the tunnel route and the proxied DNS record, run on the server:

```
CLOUDFLARE_API_TOKEN=... CLOUDFLARE_ACCOUNT_ID=... DOCKWREN_TUNNEL_ID=... \
  dockwren expose <instance> [hostname]          # preview only
  dockwren expose <instance> [hostname] --yes    # apply
  dockwren expose <instance> [hostname] --access you@example.com --yes
  dockwren expose <instance> [hostname] --replace-dns --yes   # delete other DNS records under that name first
                                                 # also put a Cloudflare Access login in front
```

Create the token in the Cloudflare dashboard (My Profile, API Tokens, Create Token, Custom token) with
Account / Cloudflare Tunnel / Edit, Zone / DNS / Edit and Zone / Zone / Read, limited to your account and
the one zone (add Account / Access: Apps and Policies / Edit only for `--access`). The Account ID is on
any zone's Overview page; the Tunnel ID is under Zero Trust, Networks, Tunnels (a remotely managed tunnel).
The same steps are on the panel's Public URL page. It is read from the environment
only. Existing routes and DNS records are never overwritten. Do not put Cloudflare Access in front of
apps used by mobile or desktop clients.

**One-step install and publish (opt-in).** Apps with an address setting (WordPress, Paperless-ngx,
Gitea, Vaultwarden, Miniflux) have an optional "Public hostname" field on the install form (CLI:
`PUBLIC_HOST=blog.example.com`). It sets the app's address to `https://<hostname>` and records the
hostname. If the three variables above are also in the panel's own environment (for example via a
systemd `EnvironmentFile` with mode 0600), the panel publishes it right after the install. By
default they are not set, the panel holds no Cloudflare credentials, and you publish with
`dockwren expose`. Enabling the variables gives anyone who can log in to the panel the power to edit
that tunnel and those DNS records, so use a token scoped to the one zone.

## Adding apps

Apps are small templates (a manifest plus a Compose file). See [docs/TEMPLATES.md](docs/TEMPLATES.md).

## Support

Dockwren is free and open source. If it saves you time, you can support its development via
[PayPal](https://www.paypal.com/paypalme/ovexro).

## Security notes

Access to the Docker socket is equivalent to root on the host. Treat the panel accordingly:

- Keep the default bind address and put a TLS-terminating proxy in front, or reach the panel over
  a private network (for example Tailscale). Do not expose it on a public port without HTTPS.
- Behind HTTPS, set `DOCKWREN_SECURE_COOKIE=1`.
- An extra identity layer in front (for example Cloudflare Access or your proxy's auth) is
  recommended for anything reachable from the internet.
- The login limiter is global (5 failures in 5 minutes lock sign-in for 60 seconds), not per IP.
- State files in `DOCKWREN_DATA` are private (0600); sessions are stored as SHA-256 hashes.

## Run as a service (systemd)

```ini
[Unit]
Description=Dockwren panel
After=network-online.target docker.service
Wants=network-online.target

[Service]
User=dockwren
SupplementaryGroups=docker
ExecStart=/usr/local/bin/dockwren
Environment=DOCKWREN_DATA=/var/lib/dockwren
Environment=DOCKWREN_BIND=127.0.0.1:8080
StateDirectory=dockwren
StateDirectoryMode=0700
Restart=on-failure
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

Read the first-run password with `journalctl -u dockwren -o cat | grep -A1 'First run'`.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Tests that need Docker talk to the local daemon and skip when it is not reachable.

## License

Apache-2.0, see [LICENSE](LICENSE). Security reports: see [SECURITY.md](SECURITY.md).
