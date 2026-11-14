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

`src/deliver.rs` pushes events out of the panel: ntfy (JSON publish, priority by severity) and a generic
webhook (JSON POST with `title`, `body`, `severity`, `kind`, `link`, `ts`, plus `text` and `content` for
Slack and Discord). Producers do nothing: a loop checks every 20 s for open events that were not sent yet at
their current severity (`Store::pending_delivery`), so a new event and an escalation are sent, a repeat is not.
Each event remembers the highest severity sent (`notified`). A failed send is retried 3 times, then dropped.

- Settings: ntfy server/topic/token, webhook address, and a minimum severity (default warnings and critical).
  Addresses and tokens live in `notify-channels.json` (0600), go to curl on stdin and are never shown again.
- Turning the first channel on marks the events already open as sent, so it is not flooded.
- When a condition clears by itself after it was sent, one low-priority "Resolved: ..." message follows (`clear_pending`, `Store::pending_resolved`). Dismissing an event by hand sends nothing; e-mail is not built.
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
