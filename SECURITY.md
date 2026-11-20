# Security policy

Dockwren holds access to the Docker socket, which is equivalent to root on the host, so security
reports are taken seriously.

## Reporting a vulnerability

Please do not open a public issue. Use GitHub's private vulnerability reporting on this repository
("Security" tab, "Report a vulnerability"). Include the version, what you did and what happened.
You will get an answer within a few days; fixes ship as a new release with a note in the changelog.

## Supported versions

Only the latest release receives fixes while the project is pre-1.0.

## Hardening notes

See "Security notes" in the README: keep the default loopback bind, put TLS and an identity layer
in front, and never expose the panel on a public port without HTTPS.

## Updating the panel from Settings

Off until you run `sudo dockwren self-update` once (or install with the current `install.sh`), which
adds a systemd drop-in. The button needs a session, a same-origin request and your password (plus the
2FA code). The panel itself only writes the file `selfupdate.requested` in its data directory and
exits; systemd restarts it and a root-run `ExecStartPre` hook (`dockwren self-update --on-start`)
fetches the newest release **itself** from GitHub over HTTPS and installs it only if the binary matches
the release's `SHA256SUMS`. The panel never supplies a binary or a version to root, so a compromised
panel can at most trigger an update to the newest genuine release. Limitation: the checksum file comes
from the same GitHub release as the binary, so this protects against corruption and tampering in
transit, not against a compromised release account. The previous binary is kept as `dockwren.prev`.

## Installing OS updates from the panel

Off by default and needs two separate steps by you: running `sudo dockwren setup-osupdate` on the
server (as root), and turning it on in Settings (password and 2FA code required; flag file
`osupdate.enabled` in the data directory).

The panel stays sandboxed and unprivileged: it cannot run `apt-get` and gets no sudo or polkit rule.
It can only create the file `osupdate.trigger` in its own data directory. A root-owned systemd path
unit (`dockwren-osupdate.path`) watches for that file and starts `dockwren-osupdate.service`, which
runs a fixed script (`/usr/local/lib/dockwren/osupdate.sh`): `apt-get update`, then
`apt-get full-upgrade` non-interactively, keeping your existing config files. The script ignores
the content of the trigger, so the only thing a compromised panel process could do with this feature
is start a package update. It never reboots. The log is written by root to
`/var/log/dockwren-osupdate.log` and only read by the panel.

- Each start needs the session cookie, a same-origin request and the password (plus the 2FA code when on).
- One run at a time; a second start while one runs is refused.
- Every start is written to the panel log and raises a `host.os_update_run` notification; a failed or
  interrupted run raises `job.failed`.
- `full-upgrade` can install new packages (for example a new kernel) and remove obsolete ones, which
  is what Debian recommends for routine updates; a plain `upgrade` would leave kernel updates behind.
- To remove it: `systemctl disable --now dockwren-osupdate.path`, then delete the two unit files in
  `/etc/systemd/system/` and `/usr/local/lib/dockwren/`.

## UPS (NUT)

If NUT's `upsd` answers on `127.0.0.1:3493` (or the address in `DOCKWREN_NUT`), the unprivileged
panel connects every 10 seconds and sends only `LIST UPS`, `LIST VAR <ups>` and `LOGOUT`: read-only,
no credentials, nothing is changed and the server is never shut down by Dockwren (that stays with
NUT's `upsmon`). Keep `upsd` on loopback (its default); `DOCKWREN_NUT=off` switches the reader off.

## Disk health (SMART)

Optional: running `sudo dockwren setup-smart` once installs `dockwren-smart.timer` and
`dockwren-smart.service`. The service runs as root, hardened (`ProtectSystem=strict`, `ProtectHome`,
`NoNewPrivileges`, `PrivateTmp`, write access only to the data folder), and runs the fixed command
`dockwren smart-collect --data-dir <data dir>` once an hour. That executes only `smartctl` (read-only
queries; never a self-test or a setting change) and writes `smart.json` with model, health verdict,
temperature, power-on hours and error counters (no serial numbers), owned by the panel user. The panel
gets no device access and no new privilege; it only reads the file, and a compromised panel can
at worst write a false `smart.json` that nothing privileged reads.
- To remove it: `systemctl disable --now dockwren-smart.timer`, delete the two unit files in
  `/etc/systemd/system/`, and delete `smart.json` from the data folder.

## Web terminal

Off by default. It is turned on either in Settings (password and 2FA code required; stored as a flag file `terminal.enabled` in the data directory, and the change is logged and raises a notification) or forced on with `DOCKWREN_TERMINAL=1`. While off, every terminal route answers 404. Turning it off does not close sessions already open. When on, a logged-in admin
can open a shell (bash, else sh, as the container's default user) inside a container of an app
Dockwren installed (Compose project `dockwren-<name>`). It is never a shell on the host, and other
containers are refused.

Threat model: anyone who controls the panel session could already control Docker, but the terminal
makes that one click, so it is guarded:

- Needs the session cookie, and the WebSocket upgrade must carry an `Origin` equal to the panel's host
  (a missing or foreign `Origin` is refused).
- Each open needs the password again (plus the 2FA code when 2FA is on); the grant lasts 10 minutes and
  is tied to that session. It shares the login rate limit.
- Requests that arrive through a Cloudflare Tunnel (`Cf-Connecting-Ip` present) are refused unless 2FA is on.
  This header is set by Cloudflare; if you run your own proxy that is not Cloudflare, it is not detected,
  so keep 2FA on there too.
- At most 3 terminals at once, closed after 30 minutes without typing or 4 hours in total; messages over 64 KiB are dropped.
- Every open and close is written to the panel log (container, short session hash, duration, reason) and
  raises a `terminal.opened` notification (bell, ntfy, webhook). Keystrokes and output are not recorded.
- On close the panel sends Ctrl-C and `exit` and closes stdin. Docker cannot kill an exec directly, so a
  process that ignores this can outlive the session.
- xterm.js is third-party JavaScript served from the panel's own origin; versions and checksums are in
  `static/README.md`.

## Config export files

`dockwren export FILE` writes every app's configuration, including the passwords in each app's `.env`,
into one file. It is encrypted with a passphrase you choose (at least 12 characters; ChaCha20-Poly1305,
key derived with Argon2id) and created with mode 0600. It is CLI only on purpose: there is no download
button in the web UI. Treat the file like a password manager export, and do not leave the passphrase
in shell history (use `DOCKWREN_PASSPHRASE` from a protected environment, or type it on stdin). The file
does not contain app data, the admin password or the off-host backup credentials.

With off-host backups on, the same kind of export (same format, no app data) is also uploaded after every
backup as a restic snapshot tagged `config`. It is sealed with the repository password and then encrypted
again by restic, so whoever holds the repository password can read your apps' passwords: protect that
password like the export passphrase. `dockwren recover` uses it to rebuild a lost server.

The restore test (`dockwren restore-test`, or weekly when switched on) starts a short-lived copy of an app from its newest snapshot. The copy has the same configuration and secrets as the app, its ports are bound to this server only, and it is deleted when the test ends (folder `restore-test/` in the data directory).

## Third-party templates

Templates from a git URL are pinned to one full commit hash, fetched without hooks, submodules or a checkout (only `manifest.toml`, `compose.yaml` and `icon.svg` are read), re-checked at every start, and shown for approval before they are stored. They may not run commands in containers, reuse a built-in id, or use devices, extra privileges or long-form mounts. The checks are text checks, not a sandbox: an approved app still runs whatever its container images do, so only add sources you have read.
