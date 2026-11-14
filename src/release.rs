//! Looks for a newer Dockwren release on GitHub and raises `dockwren.update_available`.
//! One anonymous request every 6 hours through curl; off when the admin switches the kind off.

use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::notify::{self, NewEvent, Severity, Shared};

const KIND: &str = "dockwren.update_available";
const URL: &str = "https://api.github.com/repos/ovexro/dockwren/releases/latest";
const EVERY: Duration = Duration::from_secs(6 * 3600);
/// How often the loop looks at the switch (so turning the check on acts within minutes).
const POLL: Duration = Duration::from_secs(300);

/// `v0.1.3` or `0.1.3` -> (0, 1, 3). Anything else is not a release we compare.
fn parse(v: &str) -> Option<(u32, u32, u32)> {
    let mut it = v.trim().trim_start_matches('v').split('.');
    let t = (
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    );
    it.next().is_none().then_some(t)
}

fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse(latest), parse(current)), (Some(l), Some(c)) if l > c)
}

/// The release tag from GitHub's JSON, nothing else.
fn tag_of(json: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(json).ok()?;
    let t = v["tag_name"].as_str()?;
    parse(t)?;
    Some(t.to_string())
}

async fn latest_tag() -> Result<String, String> {
    let out = tokio::process::Command::new("curl")
        .args([
            "-sS",
            "-f",
            "--max-time",
            "20",
            "-H",
            "Accept: application/vnd.github+json",
            "-A",
            concat!("dockwren/", env!("CARGO_PKG_VERSION")),
            URL,
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("cannot run curl: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    tag_of(&out.stdout).ok_or_else(|| "no release tag in the answer".to_string())
}

/// Raise or clear the notification for what GitHub reported.
fn apply(shared: &Shared, latest: &str, current: &str) {
    if is_newer(latest, current) {
        notify::raise(
            shared,
            NewEvent::new(
                "dockwren",
                KIND,
                Severity::Info,
                &format!("Dockwren {} is available", latest.trim_start_matches('v')),
            )
            .body(format!(
                "You run {current}. Release notes: https://github.com/ovexro/dockwren/releases/tag/{latest}"
            ))
            .link("/settings"),
        );
    } else {
        notify::resolve(shared, KIND);
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
                // A failed check (offline, rate limit) is silent: it retries in 6 h.
                if let Ok(tag) = latest_tag().await {
                    apply(&shared, &tag, env!("CARGO_PKG_VERSION"));
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

    #[test]
    fn compares_versions_numerically() {
        assert!(is_newer("v0.1.4", "0.1.3"));
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("v0.10.0", "0.9.0"));
        assert!(!is_newer("v0.1.3", "0.1.3"));
        assert!(!is_newer("v0.1.2", "0.1.3"));
        assert!(!is_newer("garbage", "0.1.3"));
        assert!(!is_newer("v0.1.4-rc1", "0.1.3"));
    }

    #[test]
    fn only_a_clean_tag_is_taken() {
        assert_eq!(
            tag_of(br#"{"tag_name":"v0.2.0","x":1}"#).as_deref(),
            Some("v0.2.0")
        );
        assert_eq!(tag_of(br#"{"tag_name":"v0.2.0\nevil"}"#), None);
        assert_eq!(tag_of(br#"{"message":"Not Found"}"#), None);
        assert_eq!(tag_of(b"not json"), None);
    }

    #[test]
    fn raises_then_clears_and_respects_the_switch() {
        let s: Shared = Arc::new(Mutex::new(notify::Store::memory()));
        apply(&s, "v0.2.0", "0.1.3");
        assert_eq!(s.lock().unwrap().open_events().len(), 1);
        apply(&s, "v0.2.0", "0.1.3");
        assert_eq!(s.lock().unwrap().open_events().len(), 1);
        apply(&s, "v0.2.0", "0.2.0");
        assert!(s.lock().unwrap().open_events().is_empty());
        s.lock().unwrap().set_enabled(KIND, false);
        apply(&s, "v0.3.0", "0.1.3");
        assert!(s.lock().unwrap().open_events().is_empty());
    }
}
