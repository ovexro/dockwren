# Dockwren

A minimal, Docker-only control panel for your own server. Watch and control containers, install
apps from Compose templates, update them with automatic rollback, back up and restore their data,
and optionally publish them on a Cloudflare Tunnel hostname.

- Single static Rust binary (axum + bollard), server-rendered UI with a small amount of htmx
  (vendored, no CDN, no JS build step).
- Talks to the local Docker socket. No database, no agents.
- Bundled apps: AdGuard Home, Ghost, Gitea, Grafana, Homepage, Immich, Jellyfin, Memos, Miniflux, n8n, Navidrome, Nextcloud, Paperless-ngx, Pi-hole, Roundcube, SnappyMail, Stirling-PDF, Syncthing, Uptime Kuma, Vaultwarden, Wiki.js, WordPress.
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
- Overview: all containers (running or not) with image, state and status, refreshed every 5 s. Below the tiles it shows the hostname, OS, uptime, Dockwren version and (once apps are installed) the time since the last backup.
- Resource graphs: CPU, memory, network and disk space as plain one-colour lines (last 1 h, 24 h, 7 days or 30 days), plus CPU/memory per running container. Sampled every 10 s; the 1 h and 24 h views live in memory, while 5-minute averages are kept 30 days in `metrics.log` in the data directory and survive restarts. A disk above 85% is flagged.
- Start, stop (with confirmation) and restart a container; log viewer (last 200 lines, refreshed every 5 s).
- App catalog with icons and install forms (generated passwords, per-field hints).
- Add your own stack: paste a Compose file and Dockwren runs it as a managed app, after checking it against a strict allow-list (see [docs/STACKS.md](docs/STACKS.md)).
- Upstream version heads-up: once a day the panel asks Docker Hub or GitHub's registry (anonymous, only the image name is sent) whether each installed app's main image has a newer tag with the same shape (same `-apache`/`-alpine` suffix and number of parts). The Apps page shows "Upstream has 28.4.0" (a new major is listed separately) and one notification is raised. It never changes anything: apps move to new versions through the tested template updates that ship with Dockwren releases. Turn it off in Settings → Notifications.
- Updates with automatic rollback when the new version is not healthy, and optional migration
  steps per template.
- Backups (files plus database dumps) and restore, from the UI or the CLI; a safety snapshot is
  taken before a restore.
- Resource alerts: notifications (bell, ntfy, webhook) when a disk passes 85 % (critical at 95 %), memory stays at 90 % or more for 5 minutes, CPU for 15 minutes, or a container crashes, restarts in a loop or fails its health check. Fixed thresholds, one switch per kind in Settings → Notifications.
- App settings: apps with changeable options (time zone, sign-ups, mail servers, media folders...) have a **Settings** button on the Apps page. Saving validates the values, rewrites the app's `.env` and recreates its containers; if the app does not come back healthy the old values are put back. Passwords, ports and the public address are not editable there, and each change is written to the audit log (key names only, never values).
- Resource limits: every app has a **Resources** button to cap the memory (e.g. `512m`, `2g`) and CPUs (e.g. `1.5`) of its main service (not helpers like the database). Limits are kept per app, survive updates, and apply on save by recreating the containers; if the app does not come back healthy the old limits are put back. Empty fields remove the limit. Changes are written to the audit log.
- Duplicate an app: on the Restore page, type a new name and press **Duplicate from selected snapshot**. The snapshot is restored into a second app with its own name and ports (data, accounts and passwords included; the original keeps running untouched). The copy gets no public hostname, and settings inside the app (a site URL, a domain) may still name the original address. Skipped for apps with fixed network ports or when memory is low. Take a backup first if the app has none.
- Image vulnerability scan: once a day every app's images are scanned with [Trivy](https://trivy.dev) (a pinned image run in a throwaway container with no access to the Docker socket; it reads the images from their registry and downloads Trivy's public vulnerability database, cached in `trivy-cache/` of the data directory). The Apps page shows the HIGH and CRITICAL counts per app ("2 critical, 5 high"; each CVE counted once). A notification is raised only for critical findings that already have a fixed version, and again when a new one appears; findings without a fix stay quiet. Nothing is changed: fixes arrive as image updates through Dockwren releases. Run one by hand with `dockwren scan <instance>`. Switch in Settings → Notifications.
- Public address check: every 5 minutes the panel requests each app's public hostname from outside the app (DNS and tunnel included). Three failed checks in a row raise a notification, and the Apps page shows reachable or unreachable next to the address. An app is watched after its first successful check. Switch in Settings → Notifications.
- Log size cap: every container's Docker log is capped at 10 MB x 3 files (`compose.logging.yaml`, regenerated on install and update), so a chatty app cannot fill the disk. Apps installed with an older Dockwren show a **Cap logs** button on the Apps page; it recreates the containers and removes the cap again if the app does not come back healthy.
- Passwords: apps with a generated admin password get a **Passwords** button on the Apps page. After you enter your panel password (and 2FA code) again, it shows the password stored at install. Most apps apply that password only at first start, so if you changed it inside the app, the real one is the one you set there. Apps that read it on every start (Pi-hole) also get **Rotate and restart**: a new value, containers recreated, the old value put back if the app does not come back healthy.
- Update preview: **Update available** first shows what the update would do (image version changes, new or unused images, migrations, whether a snapshot is taken, and a refusal such as a database major jump) with an **Update now** button. Nothing changes until you press it.
- Config export/import (server move or disaster recovery): `dockwren export FILE` writes every app's configuration (compose files, `.env` with its passwords, limits, state; not the data) into one file encrypted with a passphrase (ChaCha20-Poly1305, key from Argon2id; at least 12 characters; given in `DOCKWREN_PASSPHRASE` or on the first line of stdin). On the new server `dockwren import FILE` recreates the apps with their original passwords and starts them empty (apps that already exist are skipped); then `dockwren backup pull <app>` and `dockwren restore <app>` bring the data back. Public addresses are not recreated: the command prints the `dockwren expose` line to run. Keep the file as safely as a password manager export. The off-host backup target (its credentials and repository password) is not in the file: connect it again in Settings, or copy `offsite.toml` into the data directory, before `backup pull`. The file must be readable by the panel user (put it in the data directory).
- Third-party templates: Settings → System → Third-party templates (or `dockwren template add <https-url> <full-commit-hash>`) adds an app template from a git repository (`manifest.toml` + `compose.yaml`, optional `icon.svg`, at the top level; needs `git` on the server). Only a full commit hash is accepted, so nothing changes under you; Dockwren fetches that commit (no hooks, no submodules, no checkout), runs the usual template checks plus stricter Compose rules (no devices, extra privileges, long-form mounts or inline port/volume lists), refuses templates that run commands inside containers (`post_start`, `migrations`) or reuse a built-in id, and shows you the files before you click Add. These apps are labelled third-party in the catalog. The checks are simple text checks, not a sandbox: read the files and trust the images. Restart Dockwren after adding or removing one.
- Health check: Settings → System → Health (or `dockwren doctor` in a terminal, exit code 1 on a failure) runs read-only checks of what Dockwren depends on: Docker and Compose versions, a private and writable data folder, free disk and memory, clock sync, DNS and a connection to Docker Hub, off-host backup age and whether Cloudflare is connected. It changes nothing.
- Scheduled backups: Settings → System → Scheduled backups takes a snapshot of every app daily or weekly (UTC hour of your choice) and keeps the newest N per app (default 7). **Back up now** on the Apps page takes one on demand and the table shows each app's last backup. Apps that are stopped or not healthy are skipped. A failed run raises a notification. Tick **Update apps automatically** to move healthy apps to a newer bundled template, only inside the UTC window you set (default 03 to 06; equal hours mean any time; each update snapshots first and rolls back on failure; a failed version is not retried until Dockwren restarts; off by default). On any pending update, **Skip this version** removes its notification and keeps auto-update away from it until a newer release appears; **Stop skipping this version** undoes it. Each scheduled snapshot is then checked automatically: dumps must be present and non-empty, and `files.tar.gz` is read end to end (gzip checksum, tar structure, every backed-up path present); a failure raises a notification. You can check any snapshot by hand with **Verify selected snapshot** on the Restore page or `dockwren verify <instance> [snapshot]`; it changes nothing and the app keeps running (it does not start a copy of the app, so it proves the snapshot is intact, not that the app boots from it). For that, tick **Test restores** in the same form: about once a week, after the scheduled backups, each healthy app's newest snapshot is restored into a throwaway copy (own name and ports, in `restore-test/` of the data directory), must come up healthy, and the copy is deleted again; a failure raises a notification. Skipped when less than 1 GiB of memory is free or the app uses fixed LAN ports. Run one by hand with **Test restore** on the Restore page or `dockwren restore-test <instance>`; the running app is never touched. These snapshots live in the data directory, so on their own they only protect against mistakes, not disk loss; add an off-host target (next item).
- Disk usage: Settings → System → Disk usage lists each app's data folder, old restore copies, local snapshots and images (an upper bound; a "+" marks files owned by another user that could not be counted), with two explicit buttons to free space: keep only the newest 3 local snapshots of an app (off-host copies are untouched) and remove Docker images no container uses.
- Off-host backups: Settings → System → Off-host backups uploads a copy after every backup to another server (SFTP) or an S3-compatible bucket (AWS, Backblaze B2, Wasabi, MinIO...), encrypted and deduplicated with [restic](https://restic.net) run in a throwaway container (nothing to install). The settings are tested before they are saved. Dockwren generates the repository password and shows it **once**: store it in your password manager, without it the copies cannot be read. Credentials stay in a private file in the data directory and are never shown again. Disk lost? The Restore page has a **Fetch snapshots from off-host** button, and on a new server you connect the same target with the same password, then `dockwren backup pull <app>` brings the snapshots back. Recreate the app with `dockwren import` (a snapshot holds its data, not its settings or passwords; see config export/import below) and run `dockwren restore <app>`; it replaces the fresh data with the backed-up data, so the app comes back with its original accounts and content. After every backup a config export (compose files, `.env` passwords, settings; no data) is uploaded to the same target, sealed with the repository password. **Whole server lost?** On the new server run `dockwren recover`: it asks for the off-host target (SFTP or S3 details and the repository password; or set `DOCKWREN_OFFSITE_KIND`, `_ENDPOINT`, `_BUCKET`, `_ACCESS_KEY`, `_SECRET_KEY`, `_HOST`, `_PORT`, `_USER`, `_PATH`, `_PRIVATE_KEY_FILE`, `_PASSWORD`), downloads the export, recreates every app and restores its newest snapshot. Apps that fail are listed and the rest still finish. A failed upload raises a notification; the local snapshot is kept.
- Cloudflare Tunnel publishing with DNS conflict handling and optional Cloudflare Access (see below).
- Direct HTTPS without Cloudflare: a bundled Caddy container gets a Let's Encrypt certificate and serves the app on ports 80/443 (see below).
- `dockwren set-password` to rotate the admin password.
- Web terminal (optional, off by default): a shell inside the containers of apps Dockwren installed, from the Containers page. Turn it on in Settings (or force it with `DOCKWREN_TERMINAL=1`); it asks for your password (and 2FA code) each time, is logged and shown in Notifications. Not a host shell.
- Audit log: Settings → Security lists the newest events (sign-ins, failed and locked-out attempts with the source address, app install/update/backup/restore/remove/publish, 2FA and terminal changes, Cloudflare and notification settings, OS and panel updates). It is stored as `audit.log` in the data directory (about the last 2000 lines) and never contains passwords, tokens or codes.
- Panel self-update: Settings → General shows when a newer release exists. Run `sudo dockwren self-update` once (it updates right away and enables the button; installs made with the current `install.sh` already have it). After that the **Update Dockwren now** button asks for your password (and 2FA code), restarts the panel, and a root-run systemd hook downloads the release from GitHub itself, checks the binary against the published `SHA256SUMS`, and swaps it in, keeping the old one as `dockwren.prev` (roll back with `sudo mv /usr/local/bin/dockwren.prev /usr/local/bin/dockwren && sudo systemctl restart dockwren`). The panel never hands a binary to root. Apps keep running.
- OS updates (optional, off by default): the panel always tells you when the server has pending package updates or needs a reboot. To install them from Settings too, run `sudo dockwren setup-osupdate --data-dir <your data dir>` once, then turn it on in Settings. Each start asks for your password (and 2FA code); it runs `apt-get update` and `apt-get full-upgrade` as root through a small systemd helper, shows the log live, and never reboots. Debian and Ubuntu only. See SECURITY.md.
- Two-factor sign-in (optional): turn it on in Settings with any authenticator app (Google Authenticator works). Locked out? `dockwren disable-2fa` turns it off.

CLI: `dockwren [serve | set-password | disable-2fa | setup-osupdate | self-update | install | backup [pull] | update | restore | verify | restore-test | scan | doctor | cleanup | expose | unexpose | remove]`.

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
| `DOCKWREN_BACKUP_STATUS` | unset | Status file of your host backup job (one line, containing `FAIL` when the last run failed). Raises a notification on failure or when the file is older than 36 hours. See docs/NOTIFICATIONS.md. |
| `DOCKWREN_TERMINAL` | unset | Set to `1` to force the web terminal on (a shell inside containers Dockwren installed). Unset, it is off until you turn it on in Settings (asks for your password). See SECURITY.md. |
| `DOCKWREN_SECURE_COOKIE` | unset | Set to `1` when served over HTTPS so the session cookie is marked `Secure`. |

## Changing the password

```sh
DOCKWREN_DATA=/path/to/data dockwren set-password
```

Prints a new generated password once. A running server applies it immediately and signs every
session out. If you lose the password, run the same command.

## Direct HTTPS (Caddy, no Cloudflare)

In an app's "Public URL" dialog, save a hostname, then press Check and Publish directly. Dockwren starts
one Caddy container (`dockwren-caddy`, host networking) that listens on ports 80 and 443, requests a
free Let's Encrypt certificate (HTTP-01) and proxies the hostname to the app. Requirements: an A record
for the hostname pointing at this server's public IP (DNS only, not proxied), and ports 80/443 free on the
server and forwarded to it. The Check button tests these first. A hostname uses either this or the
Cloudflare Tunnel, never both. Certificates are kept in `caddy/data` in the data directory. If `ufw` is active, Check
shows the exact `ufw allow` commands (the panel never changes your firewall). The dialog shows when the
certificate expires; Caddy renews it by itself. The config export keeps which apps are direct, and the
panel restarts Caddy on start, so a restored server re-issues certificates without extra steps.

### DNS challenge variant

"Publish with DNS challenge" proves the domain with a TXT record that Caddy creates in your Cloudflare
account (token with Zone / DNS / Edit, pasted in the Cloudflare tab), instead of answering on port 80.
Use it when port 80 is blocked or behind CGNAT, or when the app should be reachable only on your LAN or
over Tailscale but still have a real certificate (point the hostname's A record at the private address).
The first time, Dockwren builds a Caddy image that includes the Cloudflare DNS plugin; this takes a few
minutes. The token is stored in `caddy/cloudflare.env` (mode 0600) in the data directory.

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

## Bundled app notes

The install form shows the full note for each app. The ones that surprise people most:

- **Syncthing** has no web password until you set one (Actions > Settings > GUI). Devices connect through Syncthing relays; LAN discovery is not available in this template. Your synced folder is not backed up by Dockwren, only Syncthing's settings and device keys.
- **Homepage** has no login. It only answers to the host names in `ALLOWED_HOSTS`, so add your public hostname there (edit `.env`, then restart) or keep it private / behind Cloudflare Access.
- **Ghost** has no email sending configured, so the emailed sign-in code for new devices is off. Create the owner account at `/ghost` right after installing, and use a strong password.
- **SnappyMail** and **Roundcube** are webmail clients, not mail servers: they need an existing mailbox provider (IMAP/SMTP).
- **Immich** backs up its database only, not the photo library folder.
- **AdGuard Home** (and any app that needs LAN ports) only opens them if you tick the LAN ports checkbox; its admin port must stay 3000 during setup.
- **Pi-hole** works like AdGuard Home (LAN ports checkbox, port 53) but without its built-in DHCP server, which needs extra Docker permissions Dockwren does not grant; keep DHCP on your router. The admin password is shown once after install.

## Adding apps

Apps are small templates (a manifest plus a Compose file). See [docs/TEMPLATES.md](docs/TEMPLATES.md).
Panel features can raise notifications for the admin, and Settings can forward them to ntfy or a webhook: see [docs/NOTIFICATIONS.md](docs/NOTIFICATIONS.md).

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
