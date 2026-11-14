//! Tells the admin when the host's scheduled backup failed or stopped running.
//!
//! The scheduled backup runs outside the panel (a systemd timer), so the panel only reads
//! its status file: one line such as `2026-11-14 03:30:20 backup OK snapshots=6`. The check is
//! off unless `DOCKWREN_BACKUP_STATUS` names that file. A line containing the word `FAIL`
//! raises a warning; a file not touched for 36 hours raises one too (the timer died, the
//! disk is gone). The next good run clears it. A missing file stays silent.

use std::time::{Duration, SystemTime};

use crate::notify::{self, NewEvent, Severity, Shared};

const KIND: &str = "backup.failed";
const POLL: Duration = Duration::from_secs(900);
const STALE: Duration = Duration::from_secs(36 * 3600);

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Ok,
    Failed(String),
    Stale(u64),
}

/// Judge a status line and how old the file is.
fn judge(line: &str, age: Duration) -> Verdict {
    let line = line.trim();
    if line
        .split_whitespace()
        .any(|w| w == "FAIL" || w == "FAILED")
    {
        return Verdict::Failed(line.chars().take(300).collect());
    }
    if age > STALE {
        return Verdict::Stale(age.as_secs() / 3600);
    }
    Verdict::Ok
}

fn apply(shared: &Shared, v: &Verdict) {
    match v {
        Verdict::Ok => {
            notify::resolve(shared, KIND);
        }
        Verdict::Failed(line) => {
            notify::raise(
                shared,
                NewEvent::new("host", KIND, Severity::Warning, "Scheduled backup failed")
                    .body(format!("Last status: {line}")),
            );
        }
        Verdict::Stale(h) => {
            notify::raise(
            shared,
            NewEvent::new("host", KIND, Severity::Warning, "Scheduled backup is overdue")
                .body(format!(
                    "No backup status update for {h} hours. Check the backup timer and that the backup disk is mounted."
                )),
            );
        }
    }
}

fn check(path: &str) -> Option<Verdict> {
    let line = std::fs::read_to_string(path).ok()?;
    let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
    let age = SystemTime::now().duration_since(mtime).unwrap_or_default();
    Some(judge(&line, age))
}

pub fn spawn_checker(shared: Shared) {
    let Some(path) = std::env::var("DOCKWREN_BACKUP_STATUS")
        .ok()
        .filter(|p| !p.is_empty())
    else {
        return;
    };
    tokio::spawn(async move {
        loop {
            if shared.lock().unwrap().is_enabled(KIND)
                && let Some(v) = check(&path)
            {
                apply(&shared, &v);
            }
            tokio::time::sleep(POLL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const H: u64 = 3600;

    #[test]
    fn judges_status_lines() {
        let fresh = Duration::from_secs(H);
        assert_eq!(
            judge("2026-11-14 03:30:20 backup OK snapshots=6\n", fresh),
            Verdict::Ok
        );
        assert!(matches!(
            judge("2026-11-14 03:30:20 backup FAIL restic exit 1", fresh),
            Verdict::Failed(_)
        ));
        assert_eq!(
            judge("x backup OK", Duration::from_secs(40 * H)),
            Verdict::Stale(40)
        );
        // A failure wins over staleness; "FAILSAFE" is not a failure.
        assert!(matches!(
            judge("x backup FAIL", Duration::from_secs(40 * H)),
            Verdict::Failed(_)
        ));
        assert_eq!(judge("x FAILSAFE ok", fresh), Verdict::Ok);
    }

    #[test]
    fn raises_once_and_clears_on_success() {
        let s: Shared = Arc::new(Mutex::new(notify::Store::memory()));
        let bad = Verdict::Failed("x backup FAIL".into());
        apply(&s, &bad);
        apply(&s, &bad);
        assert_eq!(s.lock().unwrap().open_events().len(), 1);
        apply(&s, &Verdict::Stale(40));
        assert_eq!(s.lock().unwrap().open_events().len(), 1);
        assert!(s.lock().unwrap().open_events()[0].title.contains("overdue"));
        apply(&s, &Verdict::Ok);
        assert!(s.lock().unwrap().open_events().is_empty());
    }

    #[test]
    fn missing_file_is_silent() {
        assert_eq!(check("/nonexistent/dockwren-status"), None);
    }
}
