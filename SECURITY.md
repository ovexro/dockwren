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
