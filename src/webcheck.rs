//! Watches the public URLs of installed apps from the outside.
//!
//! Every few minutes each app with a public hostname is requested over HTTPS (through DNS and
//! the Cloudflare tunnel, like a visitor would). A connection error or an HTTP status of 500
//! or more counts as a failed probe; anything else (200, a redirect to a login page, 401, 404)
//! means the address is reachable. Three failed probes in a row raise a warning, one good probe
//! clears it. A hostname that was saved but never published would always fail, so an app is
//! only watched after its first successful probe (remembered in `public.ok` in its folder).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::apps::Apps;
use crate::notify::{self, NewEvent, Severity, Shared};

const KIND: &str = "app.unreachable";
const POLL: Duration = Duration::from_secs(300);
const FAILS_TO_RAISE: u32 = 3;
const MARKER: &str = "public.ok";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Up,
    Down,
}

#[derive(Default)]
struct Track {
    fails: u32,
}

fn statuses() -> &'static Mutex<HashMap<String, Status>> {
    static S: OnceLock<Mutex<HashMap<String, Status>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// Last known state of an app's public URL; None until it has been probed.
pub fn status(name: &str) -> Option<Status> {
    statuses().lock().unwrap().get(name).copied()
}

/// Whether an HTTP status code from curl (000 when there was no answer) means reachable.
fn reachable(code: u16) -> bool {
    (100..500).contains(&code)
}

async fn probe(host: &str) -> bool {
    let out = tokio::process::Command::new("curl")
        .args([
            "-sS",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--max-time",
            "15",
            "-A",
            concat!("dockwren/", env!("CARGO_PKG_VERSION")),
            &format!("https://{host}/"),
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await;
    match out {
        Ok(o) => reachable(
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse()
                .unwrap_or(0),
        ),
        Err(_) => false,
    }
}

/// Feed one probe result in. Returns the verdict to act on: Some(true) raise, Some(false) clear.
fn step(t: &mut Track, ok: bool, armed: bool) -> Option<bool> {
    if ok {
        t.fails = 0;
        return Some(false);
    }
    t.fails = t.fails.saturating_add(1);
    (armed && t.fails == FAILS_TO_RAISE).then_some(true)
}

fn event_key(name: &str) -> String {
    format!("{KIND}:{name}")
}

async fn run_once(apps: &Apps, shared: &Shared, tracks: &mut HashMap<String, Track>) {
    let mut seen = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&apps.root) {
        for e in rd.flatten() {
            let Ok(name) = e.file_name().into_string() else {
                continue;
            };
            let dir = e.path();
            if let Some(host) = crate::public::read(&dir) {
                seen.push((name, dir, host));
            }
        }
    }
    let names: Vec<&String> = seen.iter().map(|(n, _, _)| n).collect();
    tracks.retain(|n, _| names.contains(&n));
    statuses().lock().unwrap().retain(|n, _| names.contains(&n));
    for (name, dir, host) in &seen {
        let ok = probe(host).await;
        let armed = dir.join(MARKER).exists();
        if ok && !armed {
            let _ = std::fs::write(dir.join(MARKER), "");
        }
        let t = tracks.entry(name.clone()).or_default();
        let verdict = step(t, ok, armed || ok);
        let st = if ok {
            Status::Up
        } else if armed && t.fails >= FAILS_TO_RAISE {
            Status::Down
        } else if let Some(prev) = status(name).filter(|_| armed) {
            // Not confirmed yet: keep showing the last known state.
            prev
        } else {
            continue;
        };
        statuses().lock().unwrap().insert(name.clone(), st);
        match verdict {
            Some(true) => {
                notify::raise(
                    shared,
                    NewEvent::new(
                        "apps",
                        KIND,
                        Severity::Warning,
                        &format!("{name} is not reachable at its public address"),
                    )
                    .key(event_key(name))
                    .body(format!(
                        "https://{host} failed {FAILS_TO_RAISE} checks in a row (about 10 minutes). Check the Cloudflare tunnel, the DNS record and that the app is running."
                    )),
                );
            }
            Some(false) => {
                notify::resolve(shared, &event_key(name));
            }
            None => {}
        }
    }
}

pub fn spawn_checker(apps: Apps, shared: Shared) {
    tokio::spawn(async move {
        let mut tracks: HashMap<String, Track> = HashMap::new();
        loop {
            if shared.lock().unwrap().is_enabled(KIND) {
                run_once(&apps, &shared, &mut tracks).await;
            } else {
                statuses().lock().unwrap().clear();
                tracks.clear();
            }
            tokio::time::sleep(POLL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes() {
        for ok in [200, 301, 302, 401, 403, 404] {
            assert!(reachable(ok), "{ok}");
        }
        for bad in [0, 500, 502, 530] {
            assert!(!reachable(bad), "{bad}");
        }
    }

    #[test]
    fn raises_on_third_failure_once_and_clears_on_success() {
        let mut t = Track::default();
        assert_eq!(step(&mut t, false, true), None);
        assert_eq!(step(&mut t, false, true), None);
        assert_eq!(step(&mut t, false, true), Some(true));
        assert_eq!(step(&mut t, false, true), None);
        assert_eq!(step(&mut t, true, true), Some(false));
        assert_eq!(step(&mut t, false, true), None);
    }

    #[test]
    fn never_raises_before_first_success() {
        let mut t = Track::default();
        for _ in 0..5 {
            assert_eq!(step(&mut t, false, false), None);
        }
    }
}
