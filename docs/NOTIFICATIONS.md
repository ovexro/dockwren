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

## Not here yet

Delivery channels (ntfy, webhook, e-mail) are designed but not built; the dispatch point will be
the "new or escalated" result that `raise` already returns.
