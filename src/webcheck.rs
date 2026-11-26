//! Watches the public URLs of installed apps from the outside.
//!
//! Every few minutes each app with a public hostname is requested over HTTPS (through DNS and
//! the Cloudflare tunnel, like a visitor would). A connection error or an HTTP status of 500
//! or more counts as a failed probe; anything else (200, a redirect to a login page, 401, 404)
//! means the address is reachable. Three failed probes in a row raise a warning, one good probe
//! clears it. A hostname that was saved but never published would always fail, so an app is
//! only watched after its first successful probe (remembered in `public.ok` in its folder).
//!
//! A failed probe only counts when this server itself has internet: it then asks two
//! well-known addresses (`DOCKWREN_ONLINE_CHECK`, `off` skips this). When neither answers,
//! the outage is the server's, so nothing is raised or counted against the app's uptime.

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
const HISTORY: &str = "uptime.log";
const HISTORY_DAYS: u64 = 30;
/// Rewrite the log without old lines once it grows past this many bytes (about 30 days is 100 KB).
const HISTORY_COMPACT_AT: u64 = 150_000;
const ONLINE_CHECK: &str = "https://www.cloudflare.com/cdn-cgi/trace,https://github.com/";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Up,
    Down,
}

#[derive(Default)]
struct Track {
    fails: u32,
}

fn statuses() -> &'static Mutex<HashMap<String, (Status, u64)>> {
    static S: OnceLock<Mutex<HashMap<String, (Status, u64)>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// Epoch of the last probe that settled an app's public status.
pub fn checked_at(name: &str) -> Option<u64> {
    statuses().lock().unwrap().get(name).map(|s| s.1)
}

/// Last known state of an app's public URL; None until it has been probed.
pub fn status(name: &str) -> Option<Status> {
    statuses().lock().unwrap().get(name).map(|s| s.0)
}

/// Whether an HTTP status code from curl (000 when there was no answer) means reachable.
fn reachable(code: u16) -> bool {
    (100..500).contains(&code)
}

async fn probe(host: &str) -> bool {
    reachable(http_code(&format!("https://{host}/")).await)
}

/// The HTTP status of one request; 0 when there was no answer.
async fn http_code(url: &str) -> u16 {
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
            url,
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await;
    out.ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(0)
}

/// The addresses to ask whether this server has internet; empty when switched off.
fn online_targets(setting: Option<&str>) -> Vec<String> {
    let list = match setting.map(str::trim) {
        Some("off" | "0" | "false") => return Vec::new(),
        Some(v) if !v.is_empty() => v,
        _ => ONLINE_CHECK,
    };
    list.split(',')
        .map(str::trim)
        .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
        .map(str::to_string)
        .collect()
}

/// Whether anything on the internet answers (any HTTP status). True when the check is off.
async fn online(targets: &[String]) -> bool {
    for url in targets {
        if http_code(url).await >= 100 {
            return true;
        }
    }
    targets.is_empty()
}

/// The result of one probe, or `None` when it says nothing about the app because the server
/// is offline. The failure count starts over: after an outage the tunnel needs a moment.
async fn judged(t: &mut Track, ok: bool, targets: &[String]) -> Option<bool> {
    if ok || online(targets).await {
        return Some(ok);
    }
    t.fails = 0;
    None
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

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Append one probe result ("epoch 1" reachable, "epoch 0" failed) to the app's history file.
fn record(dir: &std::path::Path, ok: bool) {
    use std::io::Write;
    let path = dir.join(HISTORY);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{} {}", now(), u8::from(ok));
    }
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > HISTORY_COMPACT_AT) {
        let cutoff = now().saturating_sub(HISTORY_DAYS * 86400);
        let kept: String = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter(|l| parse_line(l).is_some_and(|(t, _)| t >= cutoff))
            .map(|l| format!("{l}\n"))
            .collect();
        let tmp = dir.join(format!("{HISTORY}.tmp"));
        if std::fs::write(&tmp, kept).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

fn parse_line(l: &str) -> Option<(u64, bool)> {
    let (t, v) = l.split_once(' ')?;
    Some((t.parse().ok()?, v == "1"))
}

/// Share of reachable probes in the last 30 days, in percent, and how many probes that is.
fn uptime_from(text: &str, now: u64) -> Option<(f64, usize)> {
    let cutoff = now.saturating_sub(HISTORY_DAYS * 86400);
    let (mut up, mut all) = (0usize, 0usize);
    for (t, ok) in text.lines().filter_map(parse_line) {
        if t >= cutoff {
            all += 1;
            up += usize::from(ok);
        }
    }
    (all > 0).then(|| (up as f64 * 100.0 / all as f64, all))
}

/// "99.9% reachable over 30 days" for the app folder; None before any probe was recorded.
pub fn uptime_line(dir: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(HISTORY)).ok()?;
    let (pct, n) = uptime_from(&text, now())?;
    // Truncate (never round up): 99.96% must not read as 100%.
    let pct = (pct * 10.0).floor() / 10.0;
    let days = (n * (POLL.as_secs() as usize)) as f64 / 86400.0;
    Some(if days >= HISTORY_DAYS as f64 - 0.5 {
        format!("{pct:.1}% reachable over 30 days")
    } else {
        format!("{pct:.1}% reachable over {days:.1} days")
    })
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
    let targets = online_targets(std::env::var("DOCKWREN_ONLINE_CHECK").ok().as_deref());
    for (name, dir, host) in &seen {
        let up = probe(host).await;
        let t = tracks.entry(name.clone()).or_default();
        let Some(ok) = judged(t, up, &targets).await else {
            continue;
        };
        let armed = dir.join(MARKER).exists();
        if ok && !armed {
            let _ = std::fs::write(dir.join(MARKER), "");
        }
        if armed || ok {
            record(dir, ok);
        }
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
        statuses().lock().unwrap().insert(name.clone(), (st, now()));
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
    fn online_check_setting() {
        assert_eq!(online_targets(None).len(), 2);
        assert_eq!(online_targets(Some("")).len(), 2);
        assert!(online_targets(Some("off")).is_empty());
        assert_eq!(
            online_targets(Some(" http://10.0.0.1/ , ftp://x, https://a.example/")),
            vec!["http://10.0.0.1/", "https://a.example/"]
        );
    }

    #[tokio::test]
    async fn a_failure_while_the_server_is_offline_does_not_count() {
        // Nothing listens here: for the check, the server has no internet.
        let dead = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let dead = vec![format!("http://{dead}/")];
        let mut t = Track { fails: 2 };
        assert_eq!(judged(&mut t, false, &dead).await, None);
        assert_eq!(t.fails, 0, "checks start over after the outage");
        // a probe that succeeds needs no second opinion
        assert_eq!(judged(&mut t, true, &dead).await, Some(true));
        // one address answering (any status) is enough: the failure is the app's
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live = format!("http://{}/", l.local_addr().unwrap());
        let h = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut c, _) = l.accept().unwrap();
            let _ = c.read(&mut [0u8; 2048]);
            let _ = write!(c, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        });
        let both = vec![dead[0].clone(), live];
        assert_eq!(judged(&mut t, false, &both).await, Some(false));
        h.join().unwrap();
        assert_eq!(judged(&mut t, false, &[]).await, Some(false));
    }

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
    fn uptime_counts_only_last_30_days() {
        let now = 100 * 86400;
        let text = format!(
            "{} 0\n{} 1\n{} 1\n{} 0\nbad\n",
            1,
            now - 86400,
            now - 60,
            now - 30
        );
        let (pct, n) = uptime_from(&text, now).unwrap();
        assert_eq!(n, 3);
        assert!((pct - 66.666).abs() < 0.1);
        assert!(uptime_from("", now).is_none());
    }

    #[test]
    fn never_raises_before_first_success() {
        let mut t = Track::default();
        for _ in 0..5 {
            assert_eq!(step(&mut t, false, false), None);
        }
    }
}
