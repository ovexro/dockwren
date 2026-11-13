# Dockwren

A minimal, Docker-only control panel for your own server. Early development: usable for
watching and controlling containers, but not yet for installing apps.

- Single Rust binary (axum + bollard), server-rendered UI with a small amount of htmx
  (vendored, no CDN, no JS build step).
- Talks to the local Docker socket. No database, no agents.
- Planned: one-click apps from Compose templates, Cloudflare Tunnel / DNS integration.

## What works today

- Single-admin login: strong generated password, Argon2id hash, `HttpOnly` + `SameSite=Strict`
  session cookie, login rate limit, sessions that survive restarts.
- Overview: all containers (running or not) with image, state and status, refreshed every 5 s.
- Start, stop (with confirmation) and restart a container.
- Log viewer: last 200 lines (stdout + stderr), refreshed every 5 s.
- `dockwren set-password` to rotate the admin password.

## Requirements

- Linux with Docker running and access to its socket (your user in the `docker` group, or run
  as a user that can read `/var/run/docker.sock`). `DOCKER_HOST` is honoured.
- Rust (stable, edition 2024) to build from source.

## Build and run

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
| `DOCKWREN_SECURE_COOKIE` | unset | Set to `1` when served over HTTPS so the session cookie is marked `Secure`. |

## Changing the password

```sh
DOCKWREN_DATA=/path/to/data dockwren set-password
```

Prints a new generated password once. A running server applies it immediately and signs every
session out. If you lose the password, run the same command.

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

Not chosen yet.
