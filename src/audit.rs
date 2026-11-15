//! Audit log: who did what in the panel (sign-ins, app changes, settings, updates).
//!
//! One line per event in `audit.log` in the data directory (`unix-time TAB action TAB detail`),
//! newest at the end, trimmed to the last `KEEP` lines once the file passes `MAX_BYTES`.
//! Never holds passwords, tokens or codes: callers pass only names and plain facts.

use askama::Template;
use axum::response::Html;
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

const FILE: &str = "audit.log";
const MAX_BYTES: u64 = 512 * 1024;
const KEEP: usize = 2000;
const DETAIL_MAX: usize = 200;

#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    pub at: u64,
    pub action: String,
    pub detail: String,
}

pub struct Log {
    path: PathBuf,
}

fn clean(s: &str, max: usize) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max)
        .collect()
}

impl Log {
    pub fn at(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join(FILE),
        }
    }

    pub fn append(&self, now: u64, action: &str, detail: &str) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let line = format!(
            "{now}\t{}\t{}\n",
            clean(action, 40),
            clean(detail, DETAIL_MAX)
        );
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)?;
        f.write_all(line.as_bytes())?;
        if f.metadata()?.len() > MAX_BYTES {
            self.trim();
        }
        Ok(())
    }

    fn trim(&self) {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return;
        };
        let lines: Vec<&str> = text.lines().collect();
        let keep = &lines[lines.len().saturating_sub(KEEP)..];
        let tmp = self.path.with_extension("tmp");
        if std::fs::write(&tmp, keep.join("\n") + "\n").is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    /// The newest `n` entries, newest first.
    pub fn recent(&self, n: usize) -> Vec<Entry> {
        let text = std::fs::read_to_string(&self.path).unwrap_or_default();
        text.lines()
            .rev()
            .filter_map(|l| {
                let mut p = l.splitn(3, '\t');
                Some(Entry {
                    at: p.next()?.parse().ok()?,
                    action: p.next()?.to_string(),
                    detail: p.next().unwrap_or("").to_string(),
                })
            })
            .take(n)
            .collect()
    }
}

static GLOBAL: OnceLock<Mutex<Log>> = OnceLock::new();

/// Turn the log on for this process (tests never call it, so they write nothing).
pub fn init(data_dir: &Path) {
    let _ = GLOBAL.set(Mutex::new(Log::at(data_dir)));
}

/// Record one event; failures are logged to stderr and never break the action itself.
pub fn record(action: &str, detail: &str) {
    let Some(g) = GLOBAL.get() else { return };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    if let Err(e) = g.lock().unwrap().append(now, action, detail) {
        eprintln!("audit: cannot write: {e}");
    }
}

pub fn recent(n: usize) -> Vec<Entry> {
    GLOBAL
        .get()
        .map(|g| g.lock().unwrap().recent(n))
        .unwrap_or_default()
}

#[derive(Template)]
#[template(path = "audit.html")]
struct View {
    rows: Vec<(String, String, String)>,
}

/// `GET /settings/audit`: the newest events (needs a session; it lives in the protected router).
pub async fn show() -> Html<String> {
    let rows = recent(100)
        .into_iter()
        .map(|e| (crate::apps::utc_stamp(e.at), e.action, e.detail))
        .collect();
    Html(View { rows }.render().expect("audit renders"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dw-audit-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn appends_and_lists_newest_first_without_control_characters() {
        let d = tmp("a");
        let log = Log::at(&d);
        log.append(10, "login", "from 1.2.3.4").unwrap();
        log.append(20, "app.install", "gitea\nfake\t2 line")
            .unwrap();
        let r = log.recent(10);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].at, 20);
        assert_eq!(r[0].detail, "gitea fake 2 line");
        assert_eq!(r[1].action, "login");
        assert_eq!(log.recent(1).len(), 1);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn trims_to_the_newest_lines_when_too_big() {
        let d = tmp("b");
        let log = Log::at(&d);
        let big = "x".repeat(150);
        for i in 0..4000u64 {
            log.append(i, "a", &big).unwrap();
        }
        let r = log.recent(usize::MAX);
        assert!(r.len() <= 4000 && r.len() >= KEEP, "{}", r.len());
        assert_eq!(r[0].at, 3999);
        assert!(std::fs::metadata(d.join(FILE)).unwrap().len() < MAX_BYTES + 1000);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[tokio::test]
    async fn the_page_lists_events_escaped() {
        let d = tmp("c");
        Log::at(&d)
            .append(1_794_582_685, "app.install", "<b>x</b>")
            .unwrap();
        let rows: Vec<_> = Log::at(&d)
            .recent(5)
            .into_iter()
            .map(|e| (crate::apps::utc_stamp(e.at), e.action, e.detail))
            .collect();
        let html = View { rows }.render().unwrap();
        assert!(html.contains("2026-11-13 15:11 UTC"));
        assert!(!html.contains("<b>x"));
        assert!(
            View { rows: vec![] }
                .render()
                .unwrap()
                .contains("Nothing recorded")
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn recording_before_init_is_a_no_op() {
        record("x", "y");
        assert!(recent(5).is_empty());
    }
}
