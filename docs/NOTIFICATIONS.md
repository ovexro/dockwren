# Notifications for contributors

Any feature can tell the admin that something needs attention. The panel keeps one small store
(`src/notify.rs`); the bell in the sidebar, the Notifications page and the Overview strip all read it.

## Raise and resolve

```rust
use crate::notify::{self, NewEvent, Severity};

// Something is wrong. Same `key` while it is open = one event, updated in place.
notify::raise(
    &shared,
    NewEvent::new("apps", "job.failed", Severity::Warning, "update gitea failed")
        .body("first line of the error")
        .link("/apps")                       // in-panel absolute path only
        .key("job.failed:update:gitea"),     // `kind:subject`; defaults to the kind
);

// The condition is gone. Closes every open event with that key.
notify::resolve(&shared, "job.failed:update:gitea");
```

Rules:

- **Raise from a condition, not an occurrence.** A periodic check may call `raise` every hour; the
  key makes that harmless. Always pair it with a `resolve` when the condition clears, so the
  entry closes by itself.
- **Severity:** `Info` (good to know), `Warning` (needs action soon), `Critical` (broken now).
  Only warnings and critical events show on Overview. A repeat with a higher severity becomes
  "new" again; a repeat with the same or lower severity does not.
- **No secrets** in title, body or link. Text is only length-capped and control characters are
  dropped; links that are not in-panel paths are discarded.
- **Never block on it.** `raise` and `resolve` only take a short mutex; do not call them while
  holding other locks.

## Adding a new kind

1. Add `(kind, label, help)` to `KINDS` in `src/notify.rs`. Settings lists exactly this table and
   gives each kind an on/off switch; a switched-off kind is dropped by `raise` and its open events
   are closed. Nothing else is needed for the switch.
2. Raise from your feature, using that kind string.
3. Test it with `Store::memory()`: raise, check `open_events()`, resolve, check again.
   See `src/release.rs` and the tests at the bottom of `src/apps.rs` for examples.

## Storage

`notifications.log` in the data dir: JSON lines, one full event per line, the last line per id
wins, mode 0600. It is compacted at start (every open event plus up to 200 closed ones from the
last 90 days). Switch state lives in `notify-settings.json` next to it.

## Delivery channels

`src/deliver.rs` pushes events out of the panel: ntfy (JSON publish, priority by severity), Telegram (bot `sendMessage`, plain text with a severity icon) and a generic
webhook (JSON POST with `title`, `body`, `severity`, `kind`, `link`, `ts`, plus `text` and `content` for
Slack and Discord). Producers do nothing: a loop checks every 20 s for open events that were not sent yet at
their current severity (`Store::pending_delivery`), so a new event and an escalation are sent, a repeat is not.
Each event remembers the highest severity sent (`notified`). A failed send is retried 3 times, then dropped.

- Settings: ntfy server/topic/token, Telegram bot token + chat id or `@channel` (an empty chat id with a new token is looked up with `getUpdates`: the chat that last wrote to the bot), webhook address, and a minimum severity (default warnings and critical).
  Addresses and tokens live in `notify-channels.json` (0600), go to curl on stdin and are never shown again.
- Turning the first channel on marks the events already open as sent, so it is not flooded.
- When a condition clears by itself after it was sent, one low-priority "Resolved: ..." message follows (`clear_pending`, `Store::pending_resolved`). Dismissing an event by hand sends nothing; e-mail is not built.
- Maintenance mode: "Silence pushes" (30 min or 2 h, at most 4 h) holds all channel delivery. Events are still raised and shown in the panel; when it ends (or on "Resume now") whatever is still open is sent, and events that cleared meanwhile are never sent. The end time lives in `notify-silence.txt`, so an update restart does not cancel it.
- Event text goes to a third party: keep secrets out of titles and bodies (already the rule above).

## OS updates

`src/hostupdates.rs` checks every 6 hours (`apt-get -s dist-upgrade`, unprivileged) and raises
`host.os_updates` with the package count, how many are security updates, and whether
`/var/run/reboot-required` exists. Installing is a separate opt-in (`src/osupdate.rs`, see
SECURITY.md): a start raises `host.os_update_run` (audit), a failure or an interrupted run raises
`job.failed` with the first `E:` line apt printed, and when a run ends the pending check runs again
so `host.os_updates` closes itself. The watcher handles each finished run once, identified by the
timestamp on the log's `== exit` line, so a run too short to be seen while active is still reported.

## Host backup status

Scheduled backups run outside the panel (for example a systemd timer), so they cannot raise
events themselves. Set `DOCKWREN_BACKUP_STATUS` to a file your backup job rewrites after every
run, one line, containing the word `FAIL` when it failed (`2026-11-14 03:30:20 backup OK`).
`src/backupwatch.rs` reads it every 15 minutes and raises `backup.failed` when the line says
`FAIL` or the file has not been touched for 36 hours. The next good run resolves it. Without the
variable, or while the file does not exist, nothing happens.

## UPS (NUT)

`src/ups.rs`. Every 10 seconds the panel asks NUT's `upsd` (default `127.0.0.1:3493`, `DOCKWREN_NUT`
to change or `off`) for `LIST UPS` and `LIST VAR`: plain text over TCP, read-only, no login. A
status flag must show on 2 polls in a row before anything is raised (a one-second brownout is not
news). `OB` raises a warning on `host.ups:<ups>:power`, escalated to critical when `LB` or `FSD`
follows (NUT is shutting the server down); `RB` raises `...:replace`, `OVER` raises `...:overload`.
Each closes when the flag goes away, which also sends the "resolved" message. If `upsd` does not
answer, nothing is raised and nothing is closed. Health shows each UPS.

## Disk health (SMART)

`src/smart.rs`. A root systemd timer (`dockwren-smart.timer`, installed by `sudo dockwren
setup-smart`) runs `dockwren smart-collect` hourly, which asks `smartctl` about each disk and writes
`smart.json` into the data folder. The panel reads that file every 15 minutes and raises `host.smart`
(one event per disk, warning or critical) for: failed health test or NVMe critical warning
(critical), spare capacity at its threshold (critical), pending or uncorrectable sectors, NVMe
media errors, 90 % of rated endurance used, or 65 °C and up (warning). Reallocated sectors alone are
shown in Health but not alarmed on. Health also warns when the file is older than 3 hours. Closes
when the disk reports healthy. Without `smart.json`, nothing happens.

## Sign-in notifications

`src/signin.rs`, called next to the audit-log lines in `auth.rs` and `auth/passkey.rs`. `login.new_source`
(warning, one event per address) fires when a sign-in succeeds from an address not seen before; the
address is the proxy header's value when there is one, else the peer. Known addresses are kept in
`signin-known.txt` (newest 100). IPv6 addresses count by their /64 network, because devices rotate the lower half (privacy addresses) and that is not a new place. When that file does not exist (first start after upgrading) it is
seeded from past `login` lines in the audit log and nothing is raised. `login.lockout` (warning) fires
when the limiter locks sign-in after five failures and resolves on the next successful sign-in.

## Resource alerts

`src/alerts.rs` checks every minute, from the metrics the sampler already keeps and the Docker
container list. Thresholds are fixed (no settings); each kind has its on/off switch in Settings.

- `host.disk_full`: one event per tracked disk, warning at 85 %, critical at 95 %, closes below 80 %.
- `host.memory_high`: memory averaged 90 % or more over 5 minutes, closes below 85 %.
- `host.cpu_high`: CPU averaged 90 % or more over 15 minutes, closes below 80 %.
- `host.inodes_high`: one event per tracked filesystem (`df -i`, every 5 minutes), warning at 85 % of
  inodes, critical at 95 %, closes below 80 %. Filesystems without a fixed inode count are skipped.
- `host.load_high`: the 15-minute load average (`/proc/loadavg`) at 2 times the CPU core count or
  more, closes below 1.5 times. The Overview has a Load tile (no alert state of its own).
- `container.problem`: a container that is restarting, dead, reports `(unhealthy)`, or (panel-managed
  ones only) exited with an error other than 0, 137 or 143, or was killed for running out of memory
  (Docker's OOM flag, so a manual kill with 137 stays quiet). It must be seen on two polls in a row, so
  an update that recreates containers does not raise it. Key `container.problem:<name>`.
- `host.listener`: a host socket listens on every interface (`0.0.0.0`/`::`) and is not explained by Docker
  (published ports), ssh (22), Caddy (80/443 while its container runs) or `DOCKWREN_LISTEN_ALLOW`. Read from
  `/proc/net/tcp{,6}`; the body names the likely owner (a process of the panel's own user, else the running
  host-network containers). Key `host.listener:<port>`, closes when the port closes.

## Public address checks

- `app.unreachable`: every 5 minutes the panel requests `https://<hostname>/` of each app that has a
  public hostname, from the server itself (DNS and the Cloudflare tunnel, like a visitor). A connection
  error or an HTTP status of 500 or more is a failed check; 200, redirects (a login page), 401, 403 and
  404 count as reachable. Three failed checks in a row (about 10 minutes) raise a warning, the first good
  one closes it. An app is watched only after its first successful check, so a hostname that was saved but
  never published stays quiet. The Apps page shows "reachable" or "unreachable" next to the address and, below it, the share of reachable
  checks over the last 30 days (kept in `uptime.log` in the app folder; also `uptime` in `/api/status`).
  Key `app.unreachable:<app name>`.

## Image vulnerabilities

- `app.vulnerable`: once a day (first run about 10 minutes after the panel starts) every app's images
  are scanned with Trivy (`src/vulnscan.rs`, pinned image, run in a throwaway container without Docker
  socket access, skipped when less than 1 GiB of memory is free). One warning per app when it has a
  CRITICAL finding that already has a fixed version, and a fresh one when a new such finding appears;
  it closes when none is left. Findings without a fix are only counted on the Apps page. A scan that
  fails (offline, private image) keeps the previous result. Key `app.vulnerable:<app name>`.

The lower "clear" levels stop a value that hovers around a line from opening and closing the event.
