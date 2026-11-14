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
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const LOG_FILE: &str = "notifications.log";
const PREFS_FILE: &str = "notify-settings.json";
/// Every kind a producer raises: (kind, label, what it does). Settings lists exactly these.
pub const KINDS: [(&str, &str, &str); 4] = [
    (
        "app.update_available",
        "App updates available",
        "A newer version of an installed app's template is bundled with this panel.",
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
        "host.os_updates",
        "OS updates and reboot needed",
        "Every 6 hours, checks whether this server has pending package updates or needs a reboot. It only looks; it never installs anything.",
    ),
];
/// Resolved or dismissed events kept after compaction.
const KEEP_CLOSED: usize = 200;
/// Closed events older than this are dropped on compaction (90 days).
const MAX_AGE_SECS: u64 = 90 * 24 * 3600;
const MAX_TITLE: usize = 200;
const MAX_BODY: usize = 2000;
const MAX_LINK: usize = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Raised and not yet looked at.
    New,
    /// Opened or acknowledged, condition may still hold.
    Seen,
    /// Closed: the condition cleared or the admin dismissed it.
    Resolved,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
        s.rewrite();
        s
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

    /// Replay a log: the last line per id wins, bad lines are skipped, then retention applies.
    fn from_log(text: &str, now: u64) -> Self {
        let mut events: Vec<Event> = Vec::new();
        for line in text.lines() {
            let Ok(e) = serde_json::from_str::<Event>(line) else {
                continue;
            };
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
            self.resolve(&k);
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
            e.ts = now;
            e.title = title;
            e.body = body;
            e.link = link;
            if escalated {
                e.severity = n.severity;
                e.state = State::New;
            }
            let (id, snapshot) = (e.id, e.clone());
            self.append(&snapshot);
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
        let ids: Vec<u64> = self
            .events
            .iter()
            .filter(|e| e.key == key && e.state != State::Resolved)
            .map(|e| e.id)
            .collect();
        for id in &ids {
            self.set_state(*id, State::Resolved);
        }
        ids.len()
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
        v.sort_by_key(|e| std::cmp::Reverse((e.ts, e.id)));
        v
    }

    /// Resolved events, newest first.
    pub fn closed_events(&self) -> Vec<&Event> {
        let mut v: Vec<&Event> = self
            .events
            .iter()
            .filter(|e| e.state == State::Resolved)
            .collect();
        v.sort_by_key(|e| std::cmp::Reverse((e.ts, e.id)));
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
    fn slug(self) -> &'static str {
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
        age: age(now, e.ts),
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
        })
        .unwrap();
        let s = Store::from_log(&format!("not json\n{good}\n{{\"id\":1}}\n\n"), 20);
        assert_eq!(s.open_events().len(), 1);
        assert_eq!(s.next_id, 8);
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
