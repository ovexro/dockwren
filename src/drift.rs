//! Drift detection: does what runs still match the Compose files Dockwren manages?
//!
//! Compose stamps every container with a hash of its service's configuration
//! (`com.docker.compose.config-hash`). `docker compose config --hash "*"` prints the hash the
//! files on disk would give today. A difference means the files (or `.env`) changed after the
//! container was created, or the container was swapped by hand; a service with no container
//! while its siblings run means one was removed. "Apply" (`up -d`) brings the host back in line.

use std::collections::BTreeMap;

use crate::runner::{self, Instance};

const HASH_LABEL: &str = "com.docker.compose.config-hash";

/// `config --hash "*"` output: one `service hash` pair per line.
fn parse_wanted(out: &str) -> BTreeMap<String, String> {
    out.lines()
        .filter_map(|l| {
            let mut p = l.split_whitespace();
            Some((p.next()?.to_string(), p.next()?.to_string()))
        })
        .collect()
}

/// `ps -a --format json` rows (NDJSON or array) as (service, state, hash label).
fn parse_running(ps: &str) -> Vec<(String, String, String)> {
    let t = ps.trim();
    let rows: Vec<serde_json::Value> = if t.starts_with('[') {
        serde_json::from_str(t).unwrap_or_default()
    } else {
        t.lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    };
    rows.iter()
        .map(|r| {
            let labels = r["Labels"].as_str().unwrap_or("");
            let hash = labels
                .split(',')
                .find_map(|kv| kv.strip_prefix(&format!("{HASH_LABEL}=")))
                .unwrap_or("")
                .to_string();
            (
                r["Service"].as_str().unwrap_or("").to_string(),
                r["State"].as_str().unwrap_or("").to_string(),
                hash,
            )
        })
        .collect()
}

/// The differences, as short readable lines (empty when nothing drifted). A fully stopped app
/// (no containers at all) is "stopped", not drift.
pub fn compare(wanted_out: &str, ps: &str) -> Vec<String> {
    let wanted = parse_wanted(wanted_out);
    let running = parse_running(ps);
    if running.is_empty() || wanted.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (svc, want) in &wanted {
        let mine: Vec<_> = running.iter().filter(|(s, _, _)| s == svc).collect();
        if mine.is_empty() {
            out.push(format!("{svc} has no container"));
        } else if mine.iter().any(|(_, _, h)| !h.is_empty() && h != want) {
            out.push(format!("{svc} runs an older configuration"));
        }
    }
    for (svc, _, _) in &running {
        if !wanted.contains_key(svc) && !out.iter().any(|o| o.starts_with(&format!("{svc} "))) {
            out.push(format!("{svc} is not in the Compose file any more"));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Check one app against its files. `None` on any docker error (not drift: unknown).
pub async fn check(inst: &Instance) -> Option<Vec<String>> {
    let wanted = runner::compose(inst, &["config", "--hash", "*"])
        .await
        .ok()?;
    let ps = runner::compose(inst, &["ps", "-a", "--format", "json"])
        .await
        .ok()?;
    Some(compare(&wanted, &ps))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(svc: &str, state: &str, hash: &str) -> String {
        format!(r#"{{"Service":"{svc}","State":"{state}","Labels":"a=b,{HASH_LABEL}={hash},c=d"}}"#)
    }

    #[test]
    fn same_hashes_mean_no_drift() {
        let ps = format!(
            "{}\n{}",
            row("web", "running", "h1"),
            row("db", "running", "h2")
        );
        assert!(compare("web h1\ndb h2\n", &ps).is_empty());
    }

    #[test]
    fn changed_missing_and_extra_services_are_reported() {
        let ps = format!(
            "{}\n{}",
            row("web", "running", "old"),
            row("gone", "running", "x")
        );
        let d = compare("web new\ndb h2\n", &ps);
        assert_eq!(
            d,
            vec![
                "db has no container",
                "gone is not in the Compose file any more",
                "web runs an older configuration"
            ]
        );
    }

    #[test]
    fn stopped_app_and_unlabelled_containers_are_not_drift() {
        assert!(compare("web h1\n", "").is_empty());
        let ps = r#"[{"Service":"web","State":"running","Labels":""}]"#;
        assert!(compare("web h1\n", ps).is_empty());
    }
}
