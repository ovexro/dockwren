# Operations

Configuration, password, systemd service, bundled app notes and security notes.

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
| `DOCKWREN_LISTEN_ALLOW` | none | Extra ports allowed to listen on every interface without a warning (comma-separated). |
| `DOCKWREN_DISKS` | unset | Extra directories (comma separated) whose disks get a graph; the data directory's disk is always shown. |
| `DOCKWREN_BACKUP_STATUS` | unset | Status file of your host backup job (one line, containing `FAIL` when the last run failed). Raises a notification on failure or when the file is older than 36 hours. See [NOTIFICATIONS.md](NOTIFICATIONS.md). |
| `DOCKWREN_NUT` | `127.0.0.1:3493` | Where the panel looks for NUT's `upsd` to read a UPS (read-only). Set `off` to disable. Nothing happens when nothing answers there. See [NOTIFICATIONS.md](NOTIFICATIONS.md). |
| `DOCKWREN_TERMINAL` | unset | Set to `1` to force the web terminal on (a shell inside containers Dockwren installed). Unset, it is off until you turn it on in Settings (asks for your password). See [SECURITY.md](../SECURITY.md). |
| `DOCKWREN_SECURE_COOKIE` | unset | Set to `1` when served over HTTPS so the session cookie is marked `Secure`. |

## Changing the password

```sh
DOCKWREN_DATA=/path/to/data dockwren set-password
```

Prints a new generated password once. A running server applies it immediately and signs every
session out. If you lose the password, run the same command.

## Bundled app notes

The install form shows the full note for each app. The ones that surprise people most:

- **Syncthing** has no web password until you set one (Actions > Settings > GUI). Devices connect through Syncthing relays; LAN discovery is not available in this template. Your synced folder is not backed up by Dockwren, only Syncthing's settings and device keys.
- **Homepage** has no login. It only answers to the host names in `ALLOWED_HOSTS`, so add your public hostname there (edit `.env`, then restart) or keep it private / behind Cloudflare Access.
- **Ghost** has no email sending configured, so the emailed sign-in code for new devices is off. Create the owner account at `/ghost` right after installing, and use a strong password.
- **SnappyMail** and **Roundcube** are webmail clients, not mail servers: they need an existing mailbox provider (IMAP/SMTP).
- **Immich** backs up its database only, not the photo library folder.
- **AdGuard Home** (and any app that needs LAN ports) only opens them if you tick the LAN ports checkbox; its admin port must stay 3000 during setup.
- **Pi-hole** works like AdGuard Home (LAN ports checkbox, port 53) but without its built-in DHCP server, which needs extra Docker permissions Dockwren does not grant; keep DHCP on your router. The admin password is shown once after install.

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
```

`systemd-analyze security dockwren` rates this unit 1.3 (OK); the default for a plain service is 8+. An install made before the sandbox was added keeps its old unit until you run `install.sh` again (it only rewrites the unit and restarts the panel).

**Watchdog.** `install.sh` (and `sudo dockwren self-update`) also write a drop-in, `/etc/systemd/system/dockwren.service.d/watchdog.conf`, with `WatchdogSec=90` and `NotifyAccess=main`. The panel then pings systemd every 22 seconds, but only after a real `GET /health` to itself succeeded, so a deadlocked or frozen panel stops pinging and systemd restarts it (`journalctl -u dockwren` shows `Watchdog timeout`). Without the drop-in the panel behaves as before.

Read the first-run password with `journalctl -u dockwren -o cat | grep -A1 'First run'`.


## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Tests that need Docker talk to the local daemon and skip when it is not reachable.
