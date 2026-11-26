//! Notifications: one small store that any feature raises events into.
//!
//! A feature calls [`raise`] when something needs attention and [`resolve`] when the
//! condition clears. Events with the same `key` are merged, so a periodic check cannot
//! flood the list. State is kept in memory and mirrored to `notifications.log` (JSON
//! lines, one full event snapshot per line, the last line per id wins) in the data dir.
//!
//! Callers must never put secrets in `title`, `body` or `link`; text is only length-capped.

use askama::Template;
use axum::{
    extract::{Path as UrlPath, State as AxState},
    response::Html,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::wording::What;

const LOG_FILE: &str = "notifications.log";
const PREFS_FILE: &str = "notify-settings.json";
const SILENCE_FILE: &str = "notify-silence.txt";
/// Longest "silence" the admin can ask for (a forgotten one must not hide alerts for days).
pub const MAX_SILENCE_MIN: u64 = 240;
/// Every kind a producer raises: (kind, label, what it does). Settings lists exactly these.
pub const KINDS: [(&str, &str, &str); 23] = [
    (
        "terminal.opened",
        "Web terminal opened",
        "Someone opened a shell inside a container from the panel (only when the terminal is enabled).",
    ),
    (
        "app.update_available",
        "App updates available",
        "A newer version of an installed app's template is bundled with this panel.",
    ),
    (
        "app.upstream_newer",
        "Newer upstream version of an app",
        "Once a day, asks Docker Hub or GitHub's registry whether an installed app's main image has a newer tag (one anonymous request per image, only the image name is sent). Heads-up only: nothing is changed; apps update through Dockwren releases.",
    ),
    (
        "app.auto_updated",
        "App updated automatically",
        "Scheduled auto-update (Settings → Backups, off by default) moved an app to a newer template and it came back healthy. Failed updates are reported under failed installs, updates and restores.",
    ),
    (
        "job.failed",
        "Failed installs, updates and restores",
        "An install, update or restore finished with an error.",
    ),
    (
        "dockwren.update_available",
        "New Dockwren release",
        "Checks GitHub for a newer release every 6 hours (one anonymous request, nothing about you is sent).",
    ),
    (
        "login.new_source",
        "Sign-in from a new address",
        "Someone signed in to the panel from an address that has not signed in before (the first sign-in after upgrading only learns the addresses already in the audit log). Dismiss it if it was you.",
    ),
    (
        "login.lockout",
        "Sign-in locked after wrong attempts",
        "Five wrong passwords or codes in a row paused sign-in for a minute. Clears on the next successful sign-in.",
    ),
    (
        "host.os_updates",
        "OS updates and reboot needed",
        "Every 6 hours, checks whether this server has pending package updates or needs a reboot. It only looks; installing is a separate opt-in in Settings.",
    ),
    (
        "host.os_update_run",
        "OS update started from the panel",
        "Someone started an OS package update from Settings (only when installing OS updates from the panel is turned on).",
    ),
    (
        "backup.failed",
        "Scheduled backup failed or overdue",
        "Reads the status file of your host backup job (set DOCKWREN_BACKUP_STATUS). Raised when the last run failed or no run was recorded for 36 hours.",
    ),
    (
        "host.smart",
        "Disk health (SMART) problem",
        "A disk reports trouble: failed health test, sectors waiting for reallocation, uncorrectable sectors, NVMe critical warning, spare capacity at its threshold, 90% of rated write endurance used, or over 65 °C. Needs the optional SMART helper (sudo dockwren setup-smart), which reads smartctl every hour. Clears when the disk reports healthy again.",
    ),
    (
        "host.ups",
        "UPS: power cut, low or worn battery, overload",
        "Reads a UPS from NUT (Network UPS Tools, upsd on 127.0.0.1:3493, read-only) every 10 seconds. Raised when the UPS runs on battery (critical once the battery is low and NUT shuts the server down), when its battery needs replacing, or when it is overloaded. Clears when the condition ends. Nothing happens without NUT.",
    ),
    (
        "host.disk_full",
        "Disk almost full",
        "A tracked disk is at 85% or more (critical from 95%). Clears when it drops below 80%.",
    ),
    (
        "host.inodes_high",
        "Disk running out of inodes",
        "A tracked disk has used 85% or more of its inodes (critical from 95%), which stops new files even with free space. Checked every 5 minutes; clears below 80%. Filesystems without a fixed inode count are skipped.",
    ),
    (
        "host.load_high",
        "Load average high",
        "The 15-minute load average is at least twice the number of CPU cores. Clears below 1.5 times.",
    ),
    (
        "host.memory_high",
        "Memory use high",
        "Memory use averaged 90% or more over the last 5 minutes. Clears below 85%.",
    ),
    (
        "host.cpu_high",
        "CPU use high",
        "CPU use averaged 90% or more over the last 15 minutes. Clears below 80%.",
    ),
    (
        "app.unreachable",
        "App not reachable at its public address",
        "Every 5 minutes, requests the public https address of each app that has one (the app is only watched after its first successful check). Raised after 3 failed checks in a row (connection error or HTTP 500 and up); clears on the first good one. A failed check is ignored while this server itself has no internet (it then asks cloudflare.com and github.com).",
    ),
    (
        "app.vulnerable",
        "Critical vulnerabilities in an app's images",
        "Once a day, scans each app's images with Trivy (run in a throwaway container; image names go to their registries and Trivy's public vulnerability database is downloaded). Raised when an app has a critical finding that already has a fixed version, and again when a new one appears. Findings without a fix are only counted on the Apps page. Nothing is changed.",
    ),
    (
        "container.port_open",
        "Container port open on every interface",
        "A running container publishes a port on 0.0.0.0 or ::. Docker bypasses the ufw firewall for these, so they may be reachable from the internet. Checked every minute; clears when the port is closed or bound to 127.0.0.1 or a specific address.",
    ),
    (
        "host.listener",
        "Unexpected host listener",
        "A socket on this server listens on every network interface (0.0.0.0 or ::) and is not explained by Docker, ssh or the Caddy container (add ports with DOCKWREN_LISTEN_ALLOW). Catches host-network containers and stray services. Checked every minute; clears when the port closes.",
    ),
    (
        "container.problem",
        "Container crashed, restarting or unhealthy",
        "A container keeps restarting, failed its health check, was killed for running out of memory, or an app container exited with an error. Seen on two checks in a row (one minute apart) before it is raised.",
    ),
];
/// Resolved or dismissed events kept after compaction.
const KEEP_CLOSED: usize = 200;
/// Closed events older than this are dropped on compaction (90 days).
const MAX_AGE_SECS: u64 = 90 * 24 * 3600;
/// A message that could not be sent for this long is given up on (and the log says so).
pub const GIVE_UP_SECS: u64 = 24 * 3600;
const MAX_TITLE: usize = 200;
const MAX_BODY: usize = 2000;
const MAX_LINK: usize = 300;

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    #[default]
    Info,
    Warning,
    Critical,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Raised and not yet looked at.
    #[default]
    New,
    /// Opened or acknowledged, condition may still hold.
    Seen,
    /// Closed: the condition cleared or the admin dismissed it.
    Resolved,
}

/// What one channel has (not) received of one event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    /// Highest severity the channel actually accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent: Option<Severity>,
    /// No alert is owed up to this severity: the channel was set up later, or we gave up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip: Option<Severity>,
    /// An alert was due while the channel could not be reached.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub failed: bool,
    /// Nothing is owed any more about the event having cleared.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub done: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub id: u64,
    pub ts: u64,
    pub source: String,
    pub kind: String,
    pub severity: Severity,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    pub key: String,
    pub state: State,
    /// When the condition was first raised (`ts` moves on every later check).
    #[serde(default)]
    pub since: u64,
    /// When the event reached its current severity.
    #[serde(default)]
    pub raised: u64,
    /// Set when the condition ended by itself (not when the admin dismissed the event).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleared_at: Option<u64>,
    /// Per channel (`ntfy`, `webhook`, `telegram`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub delivery: BTreeMap<String, Delivery>,
    /// Logs written up to v0.1.122 (one state for all channels); read only.
    #[serde(default, skip_serializing)]
    pub notified: Option<Severity>,
    #[serde(default, skip_serializing)]
    pub clear_pending: bool,
}

impl Event {
    fn delivery_of(&self, channel: &str) -> Delivery {
        self.delivery.get(channel).copied().unwrap_or_default()
    }

    /// Is an alert still owed to this channel (whether or not the event is open)?
    fn alert_owed(&self, channel: &str, min: Severity) -> bool {
        let d = self.delivery_of(channel);
        self.severity >= min && Some(self.severity) > d.sent.max(d.skip)
    }

    /// The one message this channel is owed now, and since when it has been due.
    fn owed(&self, channel: &str, min: Severity) -> Option<(What, u64)> {
        if self.state != State::Resolved {
            return self
                .alert_owed(channel, min)
                .then_some((What::Alert, self.raised));
        }
        let at = self.cleared_at?;
        let d = self.delivery_of(channel);
        if d.done {
            None
        } else if d.sent.is_some() {
            Some((What::Cleared, at))
        } else {
            d.failed.then_some((What::Missed, at))
        }
    }
}

/// A message a channel is owed.
#[derive(Clone, Debug)]
pub struct Owed {
    pub event: Event,
    pub what: What,
    pub due: u64,
    /// An earlier attempt to send it failed.
    pub late: bool,
}

/// What a producer builds; the store assigns id, time and state.
pub struct NewEvent {
    source: String,
    kind: String,
    severity: Severity,
    title: String,
    body: Option<String>,
    link: Option<String>,
    key: Option<String>,
}

impl NewEvent {
    pub fn new(source: &str, kind: &str, severity: Severity, title: &str) -> Self {
        NewEvent {
            source: source.into(),
            kind: kind.into(),
            severity,
            title: title.into(),
            body: None,
            link: None,
            key: None,
        }
    }
    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }
    /// In-panel path such as `/instances/gitea`.
    pub fn link(mut self, link: impl Into<String>) -> Self {
        self.link = Some(link.into());
        self
    }
    /// Dedupe key, normally `kind:subject`. Defaults to `kind`.
    pub fn key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }
}

#[derive(Default)]
pub struct Store {
    events: Vec<Event>,
    next_id: u64,
    path: Option<PathBuf>,
    /// Kinds the admin switched off in Settings: `raise` drops them.
    muted: Vec<String>,
    prefs_path: Option<PathBuf>,
    /// Where events are sent besides the panel (ntfy, webhook).
    pub channels: crate::deliver::Config,
    channels_path: Option<PathBuf>,
    /// Channels whose last send failed, with the reason (kept in memory).
    down: BTreeMap<String, String>,
    /// Maintenance mode: until this unix time nothing is pushed to the channels. Events are still
    /// raised and shown in the panel, and are delivered afterwards if still open.
    silenced_until: u64,
    silence_path: Option<PathBuf>,
}

pub type Shared = Arc<Mutex<Store>>;

/// Text safe to store and render: control characters (except newline in bodies) dropped,
/// length capped on a char boundary.
fn clean(s: &str, max: usize, keep_newlines: bool) -> String {
    s.chars()
        .filter(|c| !c.is_control() || (keep_newlines && *c == '\n'))
        .take(max)
        .collect()
}

/// Only in-panel absolute paths are accepted as links.
fn clean_link(s: &str) -> Option<String> {
    let l = clean(s, MAX_LINK, false);
    (l.starts_with('/') && !l.starts_with("//")).then_some(l)
}

fn now_unix() -> u64 {
    crate::metrics::now_unix()
}

impl Store {
    /// In-memory only (tests, or when the data dir is unusable).
    pub fn memory() -> Self {
        Store::default()
    }

    /// Load `notifications.log` from `data_dir`, compact it, and keep appending to it.
    pub fn open(data_dir: &Path) -> Self {
        Self::open_at(data_dir, now_unix())
    }

    fn open_at(data_dir: &Path, now: u64) -> Self {
        let path = data_dir.join(LOG_FILE);
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let mut s = Store::from_log(&text, now);
        s.path = Some(path);
        let prefs = data_dir.join(PREFS_FILE);
        s.muted = std::fs::read_to_string(&prefs)
            .ok()
            .and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok())
            .unwrap_or_default();
        s.prefs_path = Some(prefs);
        let chan = data_dir.join(crate::deliver::FILE);
        s.channels = std::fs::read_to_string(&chan)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        s.channels_path = Some(chan);
        s.migrate_legacy();
        let sil = data_dir.join(SILENCE_FILE);
        s.silenced_until = std::fs::read_to_string(&sil)
            .ok()
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(0);
        s.silence_path = Some(sil);
        s.rewrite();
        s
    }

    /// Seconds of maintenance mode left (0 = not silenced).
    pub fn silence_left(&self) -> u64 {
        self.silenced_until.saturating_sub(now_unix())
    }

    /// Silence the channels for `minutes` (capped); 0 ends it. Remembered across restarts,
    /// because an update restarts the panel.
    pub fn silence_for(&mut self, minutes: u64) {
        let m = minutes.min(MAX_SILENCE_MIN);
        self.silenced_until = if m == 0 { 0 } else { now_unix() + m * 60 };
        if let Some(p) = &self.silence_path
            && let Ok(mut f) = Self::open_file(p, true)
        {
            let _ = f.write_all(self.silenced_until.to_string().as_bytes());
        }
    }

    pub fn is_enabled(&self, kind: &str) -> bool {
        !self.muted.iter().any(|k| k == kind)
    }

    /// Switch one known kind on or off and remember it across restarts.
    pub fn set_enabled(&mut self, kind: &str, on: bool) {
        if !KINDS.iter().any(|k| k.0 == kind) {
            return;
        }
        let mut muted: Vec<String> = self.muted.iter().filter(|k| *k != kind).cloned().collect();
        if !on {
            muted.push(kind.to_string());
        }
        self.set_muted(muted);
        if let Some(p) = &self.prefs_path
            && let Ok(mut f) = Self::open_file(p, true)
            && let Ok(t) = serde_json::to_string(&self.muted)
        {
            let _ = f.write_all(t.as_bytes());
        }
    }

    /// Replace the delivery settings and remember them (file mode 0600: URLs and tokens are secrets).
    /// A channel that was off starts from now: what happened before is not sent to it.
    pub fn set_channels(&mut self, c: crate::deliver::Config) {
        let before = self.channels.enabled();
        self.channels = c;
        let after = self.channels.enabled();
        for ch in after.iter().filter(|c| !before.contains(c)) {
            self.start_channel(ch);
        }
        self.down.retain(|ch, _| after.contains(&ch.as_str()));
        if let Some(p) = &self.channels_path
            && let Ok(mut f) = Self::open_file(p, true)
            && let Ok(t) = serde_json::to_string(&self.channels)
        {
            let _ = f.write_all(t.as_bytes());
        }
    }

    fn start_channel(&mut self, channel: &str) {
        let ids: Vec<u64> = self.events.iter().map(|e| e.id).collect();
        for id in ids {
            self.update_delivery(id, channel, |e, d| {
                *d = Delivery {
                    skip: Some(e.severity),
                    done: e.state == State::Resolved,
                    ..Delivery::default()
                };
            });
        }
    }

    /// Events from an older log say once for all channels what was sent.
    fn migrate_legacy(&mut self) {
        let channels = self.channels.enabled();
        for e in &mut self.events {
            let Some(sent) = e.notified else { continue };
            if !e.delivery.is_empty() {
                continue;
            }
            if e.clear_pending && e.cleared_at.is_none() {
                e.cleared_at = Some(e.ts);
            }
            for ch in &channels {
                let d = Delivery {
                    sent: Some(sent),
                    done: e.state == State::Resolved && !e.clear_pending,
                    ..Delivery::default()
                };
                e.delivery.insert(ch.to_string(), d);
            }
        }
    }

    /// Change one channel's state of one event; written to the log only when it changed.
    fn update_delivery(&mut self, id: u64, channel: &str, f: impl FnOnce(&Event, &mut Delivery)) {
        let Some(e) = self.events.iter_mut().find(|e| e.id == id) else {
            return;
        };
        let old = e.delivery.get(channel).copied();
        let mut d = old.unwrap_or_default();
        f(e, &mut d);
        if old == Some(d) || (old.is_none() && d == Delivery::default()) {
            return;
        }
        e.delivery.insert(channel.to_string(), d);
        let snapshot = e.clone();
        self.append(&snapshot);
    }

    /// Every message `channel` is owed, oldest first.
    pub fn outbox(&self, channel: &str, min: Severity) -> Vec<Owed> {
        let mut v: Vec<Owed> = self
            .events
            .iter()
            .filter_map(|e| {
                let (what, due) = e.owed(channel, min)?;
                Some(Owed {
                    late: e.delivery_of(channel).failed,
                    event: e.clone(),
                    what,
                    due,
                })
            })
            .collect();
        v.sort_by_key(|o| (o.due, o.event.id));
        v
    }

    /// The channel accepted this message (`o` is the event as it was when it was sent).
    pub fn delivered(&mut self, channel: &str, o: &Owed) {
        let sev = o.event.severity;
        let what = o.what;
        self.update_delivery(o.event.id, channel, |_, d| match what {
            What::Alert => d.sent = d.sent.max(Some(sev)),
            What::Cleared | What::Missed => d.done = true,
        });
        self.down.remove(channel);
    }

    /// Sending `o` failed. Every alert waiting for this channel is now late: if its problem
    /// ends before the channel works again, a summary is owed instead of silence.
    pub fn send_failed(&mut self, channel: &str, o: &Owed, reason: &str, min: Severity) {
        self.down.insert(channel.to_string(), reason.to_string());
        let mut ids: Vec<u64> = self
            .outbox(channel, min)
            .iter()
            .filter(|w| w.what == What::Alert)
            .map(|w| w.event.id)
            .collect();
        if o.what == What::Alert {
            ids.push(o.event.id); // it may have cleared while curl was trying
        }
        for id in ids {
            self.update_delivery(id, channel, |_, d| d.failed = true);
        }
    }

    /// Stop trying messages that have been due for `GIVE_UP_SECS`; returns them for the log.
    /// An alert given up on still gets its summary if the problem ends later.
    pub fn give_up(&mut self, channel: &str, min: Severity, now: u64) -> Vec<Owed> {
        let old: Vec<Owed> = self
            .outbox(channel, min)
            .into_iter()
            .filter(|o| now.saturating_sub(o.due) > GIVE_UP_SECS)
            .collect();
        for o in &old {
            let what = o.what;
            self.update_delivery(o.event.id, channel, |e, d| match what {
                What::Alert => d.skip = Some(e.severity),
                What::Cleared | What::Missed => d.done = true,
            });
        }
        old
    }

    /// Why the channel's last send failed; `None` while it works.
    pub fn channel_error(&self, channel: &str) -> Option<&str> {
        self.down.get(channel).map(String::as_str)
    }

    /// A channel that was changed or removed starts with a clean record.
    pub fn forget_channel_error(&mut self, channel: &str) {
        self.down.remove(channel);
    }

    /// Replay a log: the last line per id wins, bad lines are skipped, then retention applies.
    fn from_log(text: &str, now: u64) -> Self {
        let mut events: Vec<Event> = Vec::new();
        for line in text.lines() {
            let Ok(mut e) = serde_json::from_str::<Event>(line) else {
                continue;
            };
            // The file is ours, but links end up in href: never trust it blindly.
            e.link = e.link.as_deref().and_then(clean_link);
            if e.since == 0 {
                e.since = e.ts;
            }
            if e.raised == 0 {
                e.raised = e.since;
            }
            match events.iter_mut().find(|x| x.id == e.id) {
                Some(slot) => *slot = e,
                None => events.push(e),
            }
        }
        let next_id = events.iter().map(|e| e.id).max().map_or(1, |m| m + 1);
        let mut s = Store {
            events,
            next_id,
            path: None,
            muted: Vec::new(),
            prefs_path: None,
            channels: Default::default(),
            channels_path: None,
            down: BTreeMap::new(),
            silenced_until: 0,
            silence_path: None,
        };
        s.retain(now);
        s
    }

    /// Keep every open event, plus the newest `KEEP_CLOSED` closed ones that are not too old.
    fn retain(&mut self, now: u64) {
        let mut closed: Vec<u64> = self
            .events
            .iter()
            .filter(|e| e.state == State::Resolved)
            .map(|e| e.id)
            .collect();
        closed.sort_by_key(|id| {
            std::cmp::Reverse(self.events.iter().find(|e| e.id == *id).map(|e| e.ts))
        });
        let keep: Vec<u64> = closed
            .into_iter()
            .take(KEEP_CLOSED)
            .filter(|id| {
                self.events
                    .iter()
                    .find(|e| e.id == *id)
                    .is_some_and(|e| now.saturating_sub(e.ts) <= MAX_AGE_SECS)
            })
            .collect();
        self.events
            .retain(|e| e.state != State::Resolved || keep.contains(&e.id));
    }

    fn open_file(path: &Path, truncate: bool) -> std::io::Result<std::fs::File> {
        let mut o = std::fs::OpenOptions::new();
        o.create(true).mode(0o600);
        if truncate {
            o.write(true).truncate(true);
        } else {
            o.append(true);
        }
        o.open(path)
    }

    /// Rewrite the whole file from memory (compaction). Failures are ignored: the
    /// in-memory store keeps working and the next start just replays the old log.
    fn rewrite(&self) {
        let Some(path) = &self.path else { return };
        let mut out = String::new();
        for e in &self.events {
            if let Ok(l) = serde_json::to_string(e) {
                out.push_str(&l);
                out.push('\n');
            }
        }
        if let Ok(mut f) = Self::open_file(path, true) {
            let _ = f.write_all(out.as_bytes());
        }
    }

    fn append(&self, e: &Event) {
        let Some(path) = &self.path else { return };
        if let Ok(mut f) = Self::open_file(path, false)
            && let Ok(l) = serde_json::to_string(e)
        {
            let _ = writeln!(f, "{l}");
        }
    }

    /// Raise an event. An open event with the same key is updated in place (and put back to
    /// `New` only when its severity rises); otherwise a new event is added.
    /// Returns the event id and whether this is new or escalated (what channels should send).
    pub fn raise(&mut self, n: NewEvent) -> (u64, bool) {
        self.raise_at(n, now_unix())
    }

    /// Switch kinds off: open events of those kinds are closed and new ones are ignored.
    pub fn set_muted(&mut self, kinds: Vec<String>) {
        let open: Vec<String> = self
            .events
            .iter()
            .filter(|e| e.state != State::Resolved && kinds.contains(&e.kind))
            .map(|e| e.key.clone())
            .collect();
        self.muted = kinds;
        for k in open {
            self.supersede(&k);
        }
    }

    fn raise_at(&mut self, n: NewEvent, now: u64) -> (u64, bool) {
        if self.muted.contains(&n.kind) {
            return (0, false);
        }
        let key = clean(n.key.as_deref().unwrap_or(&n.kind), MAX_LINK, false);
        let title = clean(&n.title, MAX_TITLE, false);
        let body = n.body.as_deref().map(|b| clean(b, MAX_BODY, true));
        let link = n.link.as_deref().and_then(clean_link);
        if let Some(e) = self
            .events
            .iter_mut()
            .find(|e| e.key == key && e.state != State::Resolved)
        {
            let escalated = n.severity > e.severity;
            // Checks repeat every few seconds: only a visible change is worth a log line.
            let changed = escalated || e.title != title || e.link != link;
            e.ts = now;
            e.title = title;
            e.body = body;
            e.link = link;
            if escalated {
                e.severity = n.severity;
                e.state = State::New;
                e.raised = now;
            }
            let (id, snapshot) = (e.id, e.clone());
            if changed {
                self.append(&snapshot);
            }
            return (id, escalated);
        }
        let e = Event {
            id: self.next_id,
            ts: now,
            source: clean(&n.source, 40, false),
            kind: clean(&n.kind, MAX_LINK, false),
            severity: n.severity,
            title,
            body,
            link,
            key,
            state: State::New,
            since: now,
            raised: now,
            ..Event::default()
        };
        self.next_id += 1;
        let id = e.id;
        self.append(&e);
        self.events.push(e);
        self.compact_if_large();
        (id, true)
    }

    /// Close every open event with this key (the condition cleared). Returns how many.
    pub fn resolve(&mut self, key: &str) -> usize {
        self.resolve_at(key, now_unix())
    }

    fn resolve_at(&mut self, key: &str, now: u64) -> usize {
        let ids = self.open_ids(key);
        // While a channel is unreachable, an alert still waiting for it must not vanish.
        let min = self.channels.min;
        let unreachable: Vec<String> = if self.silence_left() > 0 {
            Vec::new()
        } else {
            self.down.keys().cloned().collect()
        };
        for id in &ids {
            for ch in &unreachable {
                self.update_delivery(*id, ch, |e, d| {
                    d.failed |= e.alert_owed(ch, min);
                });
            }
            if let Some(e) = self.events.iter_mut().find(|e| e.id == *id) {
                e.cleared_at = Some(now);
            }
            self.set_state(*id, State::Resolved);
        }
        ids.len()
    }

    /// Close the open events with this key without saying the condition cleared: a newer
    /// event replaces them, or the admin switched the kind off.
    pub fn supersede(&mut self, key: &str) {
        for id in self.open_ids(key) {
            self.set_state(id, State::Resolved);
        }
    }

    fn open_ids(&self, key: &str) -> Vec<u64> {
        self.events
            .iter()
            .filter(|e| e.key == key && e.state != State::Resolved)
            .map(|e| e.id)
            .collect()
    }

    /// Admin dismissed one event.
    pub fn dismiss(&mut self, id: u64) -> bool {
        self.set_state(id, State::Resolved)
    }

    pub fn dismiss_all(&mut self) -> usize {
        let ids: Vec<u64> = self
            .events
            .iter()
            .filter(|e| e.state != State::Resolved)
            .map(|e| e.id)
            .collect();
        for id in &ids {
            self.set_state(*id, State::Resolved);
        }
        ids.len()
    }

    /// Mark every `New` event as `Seen` (the admin opened the list).
    pub fn mark_seen(&mut self) {
        let ids: Vec<u64> = self
            .events
            .iter()
            .filter(|e| e.state == State::New)
            .map(|e| e.id)
            .collect();
        for id in ids {
            self.set_state(id, State::Seen);
        }
    }

    fn set_state(&mut self, id: u64, state: State) -> bool {
        let Some(e) = self.events.iter_mut().find(|e| e.id == id) else {
            return false;
        };
        if e.state == state {
            return false;
        }
        e.state = state;
        let snapshot = e.clone();
        self.append(&snapshot);
        true
    }

    /// The append-only log grows with every change; compact it when it has gotten long.
    fn compact_if_large(&mut self) {
        if self.next_id.is_multiple_of(500) {
            self.retain(now_unix());
            self.rewrite();
        }
    }

    /// Number of events not yet looked at (the bell badge).
    pub fn unread(&self) -> usize {
        self.events.iter().filter(|e| e.state == State::New).count()
    }

    /// Keys of open events that start with `prefix`.
    pub fn open_keys_with_prefix(&self, prefix: &str) -> Vec<String> {
        self.events
            .iter()
            .filter(|e| e.state != State::Resolved && e.key.starts_with(prefix))
            .map(|e| e.key.clone())
            .collect()
    }

    /// Open (not resolved) events, newest first.
    pub fn open_events(&self) -> Vec<&Event> {
        let mut v: Vec<&Event> = self
            .events
            .iter()
            .filter(|e| e.state != State::Resolved)
            .collect();
        v.sort_by_key(|e| std::cmp::Reverse((e.since, e.id)));
        v
    }

    /// Resolved events, newest first.
    pub fn closed_events(&self) -> Vec<&Event> {
        let mut v: Vec<&Event> = self
            .events
            .iter()
            .filter(|e| e.state == State::Resolved)
            .collect();
        v.sort_by_key(|e| std::cmp::Reverse((e.cleared_at.unwrap_or(e.ts), e.id)));
        v
    }
}

pub fn shared(data_dir: &Path) -> Shared {
    Arc::new(Mutex::new(Store::open(data_dir)))
}

/// Raise through the shared handle; what producers call.
pub fn raise(shared: &Shared, n: NewEvent) -> (u64, bool) {
    shared.lock().unwrap().raise(n)
}

/// Resolve through the shared handle; what producers call when a condition clears.
pub fn resolve(shared: &Shared, key: &str) -> usize {
    shared.lock().unwrap().resolve(key)
}

// ---- web UI ----

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Info => "Info",
            Severity::Warning => "Warning",
            Severity::Critical => "Critical",
        }
    }
    pub fn slug(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }
}

/// "just now", "5 min ago", "3 h ago", "2 d ago".
fn age(now: u64, ts: u64) -> String {
    let d = now.saturating_sub(ts);
    match d {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", d / 60),
        3600..=86399 => format!("{} h ago", d / 3600),
        _ => format!("{} d ago", d / 86400),
    }
}

pub struct Row {
    pub id: u64,
    pub severity: &'static str,
    pub label: &'static str,
    pub title: String,
    pub body: Option<String>,
    pub link: Option<String>,
    pub source: String,
    pub age: String,
}

fn row(e: &Event, now: u64) -> Row {
    Row {
        id: e.id,
        severity: e.severity.slug(),
        label: e.severity.label(),
        title: e.title.clone(),
        body: e.body.clone(),
        link: e.link.clone(),
        source: e.source.clone(),
        age: when(now, e),
    }
}

/// When it started; for a condition that ended by itself, also how long it lasted.
fn when(now: u64, e: &Event) -> String {
    let started = age(now, e.since);
    match e.cleared_at {
        Some(end) => format!(
            "started {started}, lasted {}",
            crate::wording::duration(end.saturating_sub(e.since))
        ),
        None => started,
    }
}

#[derive(Template)]
#[template(path = "notifications.html")]
struct ListTemplate {
    open: Vec<Row>,
    closed: Vec<Row>,
}

fn render_list(s: &Store) -> Html<String> {
    let now = now_unix();
    let page = ListTemplate {
        open: s.open_events().iter().map(|e| row(e, now)).collect(),
        closed: s
            .closed_events()
            .iter()
            .take(20)
            .map(|e| row(e, now))
            .collect(),
    };
    Html(page.render().expect("notifications template renders"))
}

/// The list the Notifications page shows; opening it marks the events as seen.
pub async fn list(AxState(shared): AxState<Shared>) -> Html<String> {
    let mut s = shared.lock().unwrap();
    s.mark_seen();
    render_list(&s)
}

pub async fn dismiss_one(
    AxState(shared): AxState<Shared>,
    UrlPath(id): UrlPath<u64>,
) -> Html<String> {
    let mut s = shared.lock().unwrap();
    s.dismiss(id);
    render_list(&s)
}

pub async fn dismiss_everything(AxState(shared): AxState<Shared>) -> Html<String> {
    let mut s = shared.lock().unwrap();
    s.dismiss_all();
    render_list(&s)
}

/// The bell badge in the sidebar: empty when nothing is unread.
pub async fn count(AxState(shared): AxState<Shared>) -> Html<String> {
    let n = shared.lock().unwrap().unread();
    Html(count_html(n))
}

fn count_html(n: usize) -> String {
    let inner = if n == 0 {
        String::new()
    } else {
        format!("<b class=\"count\" title=\"{n} unread\">{n}<span class=\"sr\"> unread</span></b>")
    };
    format!(
        "<span id=\"bell\" hx-get=\"/notifications/count\" hx-trigger=\"every 30s\" hx-swap=\"outerHTML\">{inner}</span>"
    )
}

pub struct KindRow {
    pub kind: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub on: bool,
}

#[derive(Template)]
#[template(path = "notify_settings.html")]
struct KindsTemplate {
    kinds: Vec<KindRow>,
}

fn render_kinds(s: &Store) -> Html<String> {
    let kinds = KINDS
        .iter()
        .map(|k| KindRow {
            kind: k.0,
            label: k.1,
            help: k.2,
            on: s.is_enabled(k.0),
        })
        .collect();
    Html(
        KindsTemplate { kinds }
            .render()
            .expect("notify settings render"),
    )
}

/// Settings: one switch per kind of notification.
pub async fn kinds_show(AxState(shared): AxState<Shared>) -> Html<String> {
    render_kinds(&shared.lock().unwrap())
}

#[derive(Deserialize)]
pub struct KindForm {
    on: Option<String>,
}

pub async fn kind_set(
    AxState(shared): AxState<Shared>,
    UrlPath(kind): UrlPath<String>,
    axum::Form(f): axum::Form<KindForm>,
) -> Html<String> {
    let mut s = shared.lock().unwrap();
    s.set_enabled(&kind, f.on.is_some());
    render_kinds(&s)
}

/// An open warning or critical event, for the strip on Overview.
pub struct Alert {
    pub severity: &'static str,
    pub label: &'static str,
    pub title: String,
    pub link: Option<String>,
}

pub fn alerts(s: &Store) -> Vec<Alert> {
    s.open_events()
        .into_iter()
        .filter(|e| e.severity >= Severity::Warning)
        .take(5)
        .map(|e| Alert {
            severity: e.severity.slug(),
            label: e.severity.label(),
            title: e.title.clone(),
            link: e.link.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(key: &str, sev: Severity) -> NewEvent {
        NewEvent::new("apps", "app.update_available", sev, "Update").key(key)
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dw-notify-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn muted_kind_is_dropped_and_open_ones_close() {
        let mut s = Store::memory();
        s.raise_at(ev("k:a", Severity::Info), 100);
        assert_eq!(s.open_events().len(), 1);
        s.set_muted(vec!["app.update_available".into()]);
        assert!(s.open_events().is_empty());
        assert_eq!(s.raise_at(ev("k:b", Severity::Info), 200), (0, false));
        assert!(s.open_events().is_empty());
        s.set_muted(Vec::new());
        s.raise_at(ev("k:b", Severity::Info), 300);
        assert_eq!(s.open_events().len(), 1);
    }

    #[test]
    fn silence_persists_is_capped_and_ends() {
        let d = tmpdir("silence");
        let mut s = Store::open(&d);
        assert_eq!(s.silence_left(), 0);
        s.silence_for(30);
        assert!(s.silence_left() > 29 * 60 && s.silence_left() <= 30 * 60);
        let s2 = Store::open(&d);
        assert!(s2.silence_left() > 29 * 60);
        let mut s3 = s2;
        s3.silence_for(100_000);
        assert!(s3.silence_left() <= MAX_SILENCE_MIN * 60);
        s3.silence_for(0);
        assert_eq!(Store::open(&d).silence_left(), 0);
    }

    #[test]
    fn switches_persist_and_unknown_kinds_are_ignored() {
        let d = tmpdir("prefs");
        let mut s = Store::open(&d);
        assert!(s.is_enabled("job.failed"));
        s.set_enabled("job.failed", false);
        s.set_enabled("nonsense", false);
        assert!(!s.is_enabled("job.failed"));
        assert!(s.is_enabled("nonsense"));
        let s2 = Store::open(&d);
        assert!(!s2.is_enabled("job.failed"));
        assert!(s2.is_enabled("app.update_available"));
        let mut s3 = s2;
        s3.set_enabled("job.failed", true);
        assert!(Store::open(&d).is_enabled("job.failed"));
    }

    #[test]
    fn same_key_is_merged_not_duplicated() {
        let mut s = Store::memory();
        let (a, new_a) = s.raise_at(ev("k:gitea", Severity::Info), 100);
        let (b, new_b) = s.raise_at(ev("k:gitea", Severity::Info), 200);
        assert_eq!(a, b);
        assert!(new_a && !new_b);
        assert_eq!(s.open_events().len(), 1);
        assert_eq!(s.open_events()[0].ts, 200);
    }

    #[test]
    fn repeated_checks_keep_the_start_time_and_do_not_grow_the_log() {
        let d = tmpdir("since");
        let mut s = Store::open_at(&d, 100);
        let lines = || {
            std::fs::read_to_string(d.join(LOG_FILE))
                .unwrap()
                .lines()
                .count()
        };
        s.raise_at(ev("k", Severity::Warning).body("battery 100%"), 100);
        for t in 1..90 {
            s.raise_at(
                ev("k", Severity::Warning).body(format!("battery {}%", 100 - t / 10)),
                100 + t * 10,
            );
        }
        assert_eq!(lines(), 1, "the same condition is written once");
        let e = s.open_events()[0].clone();
        assert_eq!((e.since, e.raised, e.ts), (100, 100, 990));
        s.raise_at(ev("k", Severity::Critical), 1_000);
        let e = s.open_events()[0].clone();
        assert_eq!((e.since, e.raised), (100, 1_000));
        s.resolve_at("k", 1_300);
        let e = s.closed_events()[0].clone();
        assert_eq!((e.since, e.cleared_at), (100, Some(1_300)));
        assert_eq!(when(1_360, &e), "started 21 min ago, lasted 20 min");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn different_keys_are_separate() {
        let mut s = Store::memory();
        s.raise(ev("k:gitea", Severity::Info));
        s.raise(ev("k:memos", Severity::Info));
        assert_eq!(s.open_events().len(), 2);
        assert_eq!(s.unread(), 2);
    }

    #[test]
    fn escalation_reopens_as_new_and_reports_true() {
        let mut s = Store::memory();
        s.raise(ev("d", Severity::Warning));
        s.mark_seen();
        assert_eq!(s.unread(), 0);
        let (_, esc) = s.raise(ev("d", Severity::Warning));
        assert!(!esc);
        assert_eq!(s.unread(), 0, "same severity keeps it seen");
        let (_, esc) = s.raise(ev("d", Severity::Critical));
        assert!(esc);
        assert_eq!(s.unread(), 1);
        assert_eq!(s.open_events()[0].severity, Severity::Critical);
    }

    #[test]
    fn resolve_closes_and_a_later_raise_starts_a_new_event() {
        let mut s = Store::memory();
        let (a, _) = s.raise(ev("k", Severity::Info));
        assert_eq!(s.resolve("k"), 1);
        assert_eq!(s.resolve("k"), 0);
        assert!(s.open_events().is_empty());
        assert_eq!(s.closed_events().len(), 1);
        let (b, new) = s.raise(ev("k", Severity::Info));
        assert!(new);
        assert_ne!(a, b);
    }

    #[test]
    fn dismiss_and_dismiss_all() {
        let mut s = Store::memory();
        let (a, _) = s.raise(ev("a", Severity::Info));
        s.raise(ev("b", Severity::Info));
        s.raise(ev("c", Severity::Info));
        assert!(s.dismiss(a));
        assert!(!s.dismiss(a), "already closed");
        assert!(!s.dismiss(999));
        assert_eq!(s.dismiss_all(), 2);
        assert_eq!(s.unread(), 0);
    }

    #[test]
    fn key_defaults_to_kind() {
        let mut s = Store::memory();
        s.raise(NewEvent::new("host", "disk.low", Severity::Warning, "Disk"));
        s.raise(NewEvent::new("host", "disk.low", Severity::Warning, "Disk"));
        assert_eq!(s.open_events().len(), 1);
        assert_eq!(s.open_events()[0].key, "disk.low");
    }

    #[test]
    fn text_is_capped_and_control_chars_dropped() {
        let mut s = Store::memory();
        let long = "x".repeat(5000);
        s.raise(
            NewEvent::new("a", "k", Severity::Info, &format!("t\u{1b}[31m{long}"))
                .body(format!("a\nb\u{7}{long}")),
        );
        let e = &s.open_events()[0];
        assert_eq!(e.title.chars().count(), MAX_TITLE);
        assert!(!e.title.contains('\u{1b}'));
        let b = e.body.as_ref().unwrap();
        assert_eq!(b.chars().count(), MAX_BODY);
        assert!(b.starts_with("a\nb") && !b.contains('\u{7}'));
    }

    /// Every literal `.link("...")` in the code must open a full page of the panel (not a
    /// fragment like /metrics, and not a route that does not exist).
    #[test]
    fn notification_links_open_real_pages() {
        const PAGES: &[&str] = &[
            "/",
            "/apps",
            "/containers",
            "/resources",
            "/notifications",
            "/cloudflare",
            "/settings",
            "/settings/doctor",
            "/settings/audit",
        ];
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let mut files = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        let re = regex::Regex::new(r#"\.link\(\s*"([^"]+)"\s*\)"#).unwrap();
        let mut seen = 0;
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap();
            // Skip the pattern as quoted in comments (the doc line above has `"..."`).
            for c in re.captures_iter(&text).filter(|c| &c[1] != "...") {
                seen += 1;
                let path = c[1].split(['?', '#']).next().unwrap();
                assert!(
                    PAGES.contains(&path),
                    "{}: link {path} is not a full page",
                    f.display()
                );
            }
        }
        assert!(seen >= 10, "found only {seen} links: the pattern broke");
    }

    #[test]
    fn only_in_panel_links_are_kept() {
        let mut s = Store::memory();
        for (i, l) in [
            "/instances/gt",
            "https://evil.example/",
            "//evil.example",
            "javascript:x",
        ]
        .iter()
        .enumerate()
        {
            s.raise(
                NewEvent::new("a", "k", Severity::Info, "t")
                    .key(format!("k{i}"))
                    .link(*l),
            );
        }
        let mut links: Vec<_> = s.open_events().iter().map(|e| e.link.clone()).collect();
        links.sort();
        assert_eq!(
            links,
            vec![None, None, None, Some("/instances/gt".to_string())]
        );
    }

    #[test]
    fn persists_and_reloads_with_state_changes() {
        let d = tmpdir("persist");
        {
            let mut s = Store::open_at(&d, 1000);
            s.raise_at(ev("a", Severity::Warning), 1000);
            s.raise_at(ev("b", Severity::Info), 1001);
            s.resolve("b");
            s.mark_seen();
        }
        let s = Store::open_at(&d, 1002);
        assert_eq!(s.open_events().len(), 1);
        assert_eq!(s.open_events()[0].key, "a");
        assert_eq!(s.open_events()[0].state, State::Seen);
        assert_eq!(s.closed_events().len(), 1);
        // ids keep increasing after a reload
        let mut s = s;
        let (id, _) = s.raise_at(ev("c", Severity::Info), 1003);
        assert_eq!(id, 3);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn log_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmpdir("mode");
        let mut s = Store::open_at(&d, 1);
        s.raise_at(ev("a", Severity::Info), 1);
        let m = std::fs::metadata(d.join(LOG_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(m & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bad_log_lines_are_skipped() {
        let good = serde_json::to_string(&Event {
            id: 7,
            ts: 10,
            source: "a".into(),
            kind: "k".into(),
            severity: Severity::Info,
            title: "t".into(),
            body: None,
            link: None,
            key: "k".into(),
            state: State::New,
            ..Event::default()
        })
        .unwrap();
        let s = Store::from_log(&format!("not json\n{good}\n{{\"id\":1}}\n\n"), 20);
        assert_eq!(s.open_events().len(), 1);
        assert_eq!(s.next_id, 8);
    }

    #[test]
    fn loaded_log_drops_unsafe_links() {
        let mk = |id: u64, link: &str| {
            serde_json::to_string(&Event {
                id,
                ts: 10,
                source: "a".into(),
                kind: "k".into(),
                severity: Severity::Info,
                title: "t".into(),
                body: None,
                link: Some(link.into()),
                key: format!("k{id}"),
                state: State::New,
                ..Event::default()
            })
            .unwrap()
                + "\n"
        };
        let log = mk(1, "/apps") + &mk(2, "javascript:alert(1)") + &mk(3, "//evil.example/x");
        let s = Store::from_log(&log, 20);
        let links: Vec<_> = s.open_events().iter().map(|e| e.link.clone()).collect();
        assert_eq!(links.len(), 3);
        assert_eq!(links.iter().flatten().collect::<Vec<_>>(), vec!["/apps"]);
    }

    #[test]
    fn retention_keeps_open_drops_old_and_excess_closed() {
        let now = 10 * MAX_AGE_SECS;
        let mut log = String::new();
        let mk = |id: u64, ts: u64, state: &str| {
            format!(
                "{{\"id\":{id},\"ts\":{ts},\"source\":\"a\",\"kind\":\"k\",\"severity\":\"info\",\"title\":\"t\",\"key\":\"k{id}\",\"state\":\"{state}\"}}\n"
            )
        };
        log.push_str(&mk(1, 1, "new")); // ancient but open: kept
        log.push_str(&mk(2, 1, "resolved")); // ancient closed: dropped
        for i in 0..(KEEP_CLOSED as u64 + 10) {
            log.push_str(&mk(100 + i, now - 1000 + i, "resolved"));
        }
        let s = Store::from_log(&log, now);
        assert_eq!(s.open_events().len(), 1);
        assert_eq!(s.closed_events().len(), KEEP_CLOSED);
        assert!(s.closed_events().iter().all(|e| e.id != 2));
        // the newest closed ones survive
        assert_eq!(s.closed_events()[0].id, 100 + KEEP_CLOSED as u64 + 9);
    }

    #[test]
    fn last_line_per_id_wins() {
        let line = |state: &str| {
            format!(
                "{{\"id\":1,\"ts\":5,\"source\":\"a\",\"kind\":\"k\",\"severity\":\"info\",\"title\":\"t\",\"key\":\"k\",\"state\":\"{state}\"}}\n"
            )
        };
        let s = Store::from_log(&(line("new") + &line("resolved")), 6);
        assert!(s.open_events().is_empty());
        assert_eq!(s.closed_events().len(), 1);
    }

    #[test]
    fn age_text() {
        assert_eq!(age(100, 100), "just now");
        assert_eq!(age(100, 200), "just now");
        assert_eq!(age(1000, 700), "5 min ago");
        assert_eq!(age(20000, 0), "5 h ago");
        assert_eq!(age(200000, 0), "2 d ago");
    }

    #[test]
    fn alerts_only_warning_and_up_and_html_is_escaped() {
        let mut s = Store::memory();
        s.raise(ev("i", Severity::Info));
        s.raise(NewEvent::new("a", "k", Severity::Critical, "<script>x</script>").key("c"));
        let a = alerts(&s);
        assert_eq!(a.len(), 1);
        let sh: Shared = Arc::new(Mutex::new(s));
        let html = render_list(&sh.lock().unwrap()).0;
        assert!(!html.contains("<script>x"), "{html}");
        assert!(
            html.contains("&lt;script&gt;") || html.contains("&#60;script&#62;"),
            "{html}"
        );
        assert!(!count_html(0).contains("class=\"count\""));
        assert!(count_html(3).contains(">3<"));
    }

    #[test]
    fn shared_helpers_raise_and_resolve() {
        let sh: Shared = Arc::new(Mutex::new(Store::memory()));
        raise(&sh, ev("k", Severity::Info));
        assert_eq!(sh.lock().unwrap().unread(), 1);
        assert_eq!(resolve(&sh, "k"), 1);
        assert_eq!(sh.lock().unwrap().unread(), 0);
    }
}
