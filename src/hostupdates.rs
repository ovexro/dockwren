//! Tells the admin when the host has pending OS package updates or needs a reboot.
//! Detection only: `apt-get -s dist-upgrade` runs unprivileged against the package lists
//! the system's own apt timer keeps fresh. Installing is a separate, opt-in step (`osupdate`).

use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::notify::{self, NewEvent, Severity, Shared};

const KIND: &str = "host.os_updates";
const EVERY: Duration = Duration::from_secs(6 * 3600);
const POLL: Duration = Duration::from_secs(300);
const REBOOT_FLAG: &str = "/var/run/reboot-required";
const SHOW: usize = 10;

#[derive(Debug, PartialEq, Eq, Default)]
struct Pending {
    packages: Vec<String>,
    security: usize,
}

/// Packages `apt-get -s` would upgrade: lines like
/// `Inst libc6 [2.41-1] (2.41-2 Debian-Security:13/stable-security [amd64])`.
fn parse(out: &str) -> Pending {
    let mut p = Pending::default();
    for l in out.lines() {
        let Some(rest) = l.strip_prefix("Inst ") else {
            continue;
        };
        let Some(name) = rest.split_whitespace().next() else {
            continue;
        };
        p.packages.push(name.to_string());
        if rest.contains("-security") {
            p.security += 1;
        }
    }
    p
}

async fn pending() -> Result<Pending, String> {
    let out = tokio::process::Command::new("apt-get")
        .args(["-s", "dist-upgrade"])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("cannot run apt-get: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(parse(&String::from_utf8_lossy(&out.stdout)))
}

/// Raise or clear the notification for what the host reports.
fn apply(shared: &Shared, p: &Pending, reboot: bool) {
    if p.packages.is_empty() && !reboot {
        notify::resolve(shared, KIND);
        return;
    }
    let n = p.packages.len();
    let title = match (n, reboot) {
        (0, _) => "Reboot needed to finish OS updates".to_string(),
        (_, true) if p.security > 0 => format!(
            "{n} OS updates available ({} security), reboot needed",
            p.security
        ),
        (_, true) => format!("{n} OS updates available, reboot needed"),
        _ if p.security > 0 => format!("{n} OS updates available ({} security)", p.security),
        _ => format!("{n} OS updates available"),
    };
    let mut body = String::new();
    if n > 0 {
        let mut names: Vec<&str> = p.packages.iter().take(SHOW).map(String::as_str).collect();
        if n > SHOW {
            names.push("…");
        }
        body = format!(
            "{}. Install them in a terminal (sudo apt update && sudo apt upgrade) or, if turned on, from Settings.",
            names.join(", ")
        );
    }
    let sev = if p.security > 0 || reboot {
        Severity::Warning
    } else {
        Severity::Info
    };
    let mut e = NewEvent::new("host", KIND, sev, &title);
    if !body.is_empty() {
        e = e.body(body);
    }
    notify::raise(shared, e);
}

/// How many packages `apt-get -s` would upgrade right now (None when apt is unavailable).
pub async fn pending_count() -> Option<usize> {
    pending().await.ok().map(|p| p.packages.len())
}

/// Look again now and raise or clear the notification (used when an update run ends).
pub async fn recheck(shared: &Shared) {
    if !shared.lock().unwrap().is_enabled(KIND) {
        return;
    }
    if let Ok(p) = pending().await {
        apply(shared, &p, std::path::Path::new(REBOOT_FLAG).exists());
    }
}

pub fn spawn_checker(shared: Shared) {
    tokio::spawn(async move {
        let mut last: Option<Instant> = None;
        loop {
            let on = shared.lock().unwrap().is_enabled(KIND);
            if !on {
                last = None;
            } else if last.is_none_or(|t| t.elapsed() >= EVERY) {
                last = Some(Instant::now());
                // No apt-get (not Debian/Ubuntu) or a failed run: silent, retried in 6 h.
                if let Ok(p) = pending().await {
                    apply(&shared, &p, std::path::Path::new(REBOOT_FLAG).exists());
                }
            }
            tokio::time::sleep(POLL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const SAMPLE: &str = "NOTE: This is only a simulation!\nReading package lists...\n\
Inst libc6 [2.41-1] (2.41-2 Debian-Security:13/stable-security [amd64])\n\
Inst curl [8.14.1-1] (8.14.1-2 Debian:13.1/stable [amd64])\n\
Conf libc6 (2.41-2 Debian-Security:13/stable-security [amd64])\n\
Conf curl (8.14.1-2 Debian:13.1/stable [amd64])\n";

    fn shared() -> Shared {
        Arc::new(Mutex::new(notify::Store::memory()))
    }

    #[test]
    fn parses_install_lines_and_counts_security() {
        let p = parse(SAMPLE);
        assert_eq!(p.packages, vec!["libc6", "curl"]);
        assert_eq!(p.security, 1);
        assert_eq!(parse("Reading package lists...\n"), Pending::default());
    }

    #[test]
    fn severity_follows_security_and_reboot_and_it_clears() {
        let s = shared();
        apply(
            &s,
            &parse("Inst a [1] (2 Debian:13/stable [amd64])\n"),
            false,
        );
        assert_eq!(s.lock().unwrap().open_events()[0].severity, Severity::Info);
        apply(&s, &parse(SAMPLE), false);
        {
            let st = s.lock().unwrap();
            assert_eq!(st.open_events().len(), 1);
            assert_eq!(st.open_events()[0].severity, Severity::Warning);
            assert!(st.open_events()[0].title.contains("1 security"));
        }
        apply(&s, &Pending::default(), true);
        assert!(s.lock().unwrap().open_events()[0].title.contains("Reboot"));
        apply(&s, &Pending::default(), false);
        assert!(s.lock().unwrap().open_events().is_empty());
    }

    #[test]
    fn long_lists_are_cut() {
        let many: String = (0..25)
            .map(|i| format!("Inst pkg{i} [1] (2 Debian:13/stable [amd64])\n"))
            .collect();
        let s = shared();
        apply(&s, &parse(&many), false);
        let st = s.lock().unwrap();
        let e = st.open_events()[0];
        assert!(e.title.starts_with("25 OS updates"));
        assert!(e.body.as_deref().unwrap().contains("…"));
        assert!(!e.body.as_deref().unwrap().contains("pkg24"));
    }
}
