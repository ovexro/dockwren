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
const APT_LISTS: &str = "/var/lib/apt/lists";
const SHOW: usize = 10;

#[derive(Debug, PartialEq, Eq, Default)]
struct Pending {
    packages: Vec<String>,
    security: usize,
    details: Vec<Pkg>,
}

/// One package `apt-get -s` would upgrade.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Pkg {
    pub name: String,
    pub from: String,
    pub to: String,
    pub security: bool,
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
        let security = rest.contains("-security");
        if security {
            p.security += 1;
        }
        let between = |open: char, close: char| {
            rest.split_once(open)
                .and_then(|(_, r)| r.split_once(close))
                .map(|(v, _)| v.to_string())
        };
        p.details.push(Pkg {
            name: name.to_string(),
            from: between('[', ']').unwrap_or_default(),
            to: between('(', ')')
                .and_then(|v| v.split_whitespace().next().map(str::to_string))
                .unwrap_or_default(),
            security,
        });
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

/// What the Settings card shows: the waiting packages and how old the package lists are.
pub struct Overview {
    pub packages: Vec<Pkg>,
    pub security: usize,
    pub lists_age: Option<Duration>,
}

/// Look now (None when apt is unavailable).
pub async fn overview() -> Option<Overview> {
    let p = pending().await.ok()?;
    Some(Overview {
        security: p.security,
        packages: p.details,
        lists_age: lists_age(std::path::Path::new(APT_LISTS)),
    })
}

/// Age of the newest `*Release` file in apt's lists directory: apt touches them on every
/// successful `apt-get update`, even when nothing changed (None when unreadable).
fn lists_age(dir: &std::path::Path) -> Option<Duration> {
    let newest = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with("Release"))
        .filter_map(|e| e.metadata().ok()?.modified().ok())
        .max()?;
    std::time::SystemTime::now().duration_since(newest).ok()
}

/// "just now", "12 min ago", "5 h ago", "3 days ago".
pub fn age_text(d: Duration) -> String {
    let m = d.as_secs() / 60;
    match m {
        0 => "just now".to_string(),
        1..=59 => format!("{m} min ago"),
        60..=2879 => format!("{} h ago", m / 60),
        _ => format!("{} days ago", m / 1440),
    }
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
    fn keeps_versions_and_security_per_package() {
        let p = parse(SAMPLE);
        assert_eq!(
            p.details[0],
            Pkg {
                name: "libc6".into(),
                from: "2.41-1".into(),
                to: "2.41-2".into(),
                security: true
            }
        );
        assert!(!p.details[1].security);
        // A package that is newly installed has no old version.
        let n = parse("Inst newpkg (1.0 Debian:13/stable [amd64])\n");
        assert_eq!(
            (n.details[0].from.as_str(), n.details[0].to.as_str()),
            ("", "1.0")
        );
    }

    #[test]
    fn ages_read_naturally() {
        let m = |n: u64| age_text(Duration::from_secs(n * 60));
        assert_eq!(m(0), "just now");
        assert_eq!(m(12), "12 min ago");
        assert_eq!(m(5 * 60), "5 h ago");
        assert_eq!(m(3 * 1440), "3 days ago");
    }

    #[test]
    fn lists_age_uses_the_newest_release_file() {
        let d = std::env::temp_dir().join(format!("dw-apt-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(lists_age(&d), None);
        std::fs::write(d.join("deb.debian.org_dists_trixie_InRelease"), "x").unwrap();
        std::fs::write(d.join("other_Packages"), "x").unwrap();
        assert!(lists_age(&d).unwrap() < Duration::from_secs(60));
        std::fs::remove_dir_all(&d).unwrap();
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
