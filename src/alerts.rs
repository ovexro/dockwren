//! Resource alerts: a full disk, sustained high memory or CPU, and containers that crash,
//! restart in a loop or report themselves unhealthy. Everything goes through the notification
//! store, so the on/off switches, the bell and the delivery channels (ntfy, webhook) apply.
//!
//! Thresholds are fixed on purpose: a warning at 85 % disk, critical at 95 %, memory and CPU
//! averaged over 5 and 15 minutes. Each condition has a lower "clear" level so a value hovering
//! around the line does not flap.

use std::collections::HashSet;
use std::time::Duration;

use crate::docker::{ContainerInfo, DockerClient};
use crate::metrics::{self, Metrics};
use crate::notify::{self, NewEvent, Severity, Shared};

const POLL: Duration = Duration::from_secs(60);

pub const DISK_KIND: &str = "host.disk_full";
pub const MEM_KIND: &str = "host.memory_high";
pub const CPU_KIND: &str = "host.cpu_high";
pub const CONTAINER_KIND: &str = "container.problem";

const DISK_CRIT_PCT: f64 = 95.0;
const DISK_CLEAR_PCT: f64 = 80.0;
const MEM_PCT: f64 = 90.0;
const MEM_CLEAR_PCT: f64 = 85.0;
/// Samples are 10 s apart.
const MEM_SAMPLES: usize = 30;
const CPU_PCT: f64 = 90.0;
const CPU_CLEAR_PCT: f64 = 80.0;
const CPU_SAMPLES: usize = 90;

/// Is the condition on? Turns on at `raise`, stays on until the value drops below `clear`.
fn level(value: f64, raise: f64, clear: f64, was_open: bool) -> bool {
    value >= raise || (was_open && value >= clear)
}

/// Mean of the newest `n` values, or `None` while fewer than `n` exist.
fn recent_mean(values: &[f64], n: usize) -> Option<f64> {
    if values.len() < n {
        return None;
    }
    Some(values[values.len() - n..].iter().sum::<f64>() / n as f64)
}

/// Why a container needs attention, if it does.
fn container_problem(c: &ContainerInfo) -> Option<String> {
    match c.state.as_str() {
        "restarting" => Some("it keeps restarting".into()),
        "dead" => Some("it is dead".into()),
        "running" if c.status.contains("(unhealthy)") => Some("its health check is failing".into()),
        "exited" if c.managed => {
            // `Exited (1) 3 minutes ago`. 0 is a clean stop; 137 and 143 are what a manual
            // stop or kill leaves behind, which is not worth a notification.
            let code: i32 = c
                .status
                .strip_prefix("Exited (")?
                .split(')')
                .next()?
                .parse()
                .ok()?;
            (![0, 137, 143].contains(&code)).then(|| format!("it crashed with exit code {code}"))
        }
        _ => None,
    }
}

fn resolve_stale(shared: &Shared, prefix: &str, keep: &HashSet<String>) {
    let stale: Vec<String> = shared
        .lock()
        .unwrap()
        .open_keys_with_prefix(prefix)
        .into_iter()
        .filter(|k| !keep.contains(k))
        .collect();
    for k in stale {
        notify::resolve(shared, &k);
    }
}

fn is_open(shared: &Shared, key: &str) -> bool {
    shared
        .lock()
        .unwrap()
        .open_keys_with_prefix(key)
        .iter()
        .any(|k| k == key)
}

/// Disk, memory and CPU against the latest metrics.
pub fn check_host(shared: &Shared, m: &Metrics) {
    let mut open = HashSet::new();
    for d in &m.disks {
        let Some(&(_, pct)) = d.pct.back() else {
            continue;
        };
        let key = format!("{DISK_KIND}:{}", d.label);
        if !level(
            pct,
            crate::metrics::DISK_WARN_PCT,
            DISK_CLEAR_PCT,
            is_open(shared, &key),
        ) {
            continue;
        }
        let sev = if pct >= DISK_CRIT_PCT {
            Severity::Critical
        } else {
            Severity::Warning
        };
        open.insert(key.clone());
        notify::raise(
            shared,
            NewEvent::new(
                "host",
                DISK_KIND,
                sev,
                &format!("Disk {} is {pct:.0}% full", d.label),
            )
            .body(format!(
                "{} of {} used. Free some space or old snapshots before apps start failing.",
                metrics::fmt_bytes(d.used as f64),
                metrics::fmt_bytes(d.total as f64)
            ))
            .link("/metrics")
            .key(key),
        );
    }
    resolve_stale(shared, &format!("{DISK_KIND}:"), &open);

    let mem: Vec<f64> = m.samples.iter().map(|s| s.mem).collect();
    let cpu: Vec<f64> = m.samples.iter().map(|s| s.cpu).collect();
    sustained(
        shared,
        MEM_KIND,
        &mem,
        MEM_SAMPLES,
        MEM_PCT,
        MEM_CLEAR_PCT,
        "Memory",
        "5 minutes",
    );
    sustained(
        shared,
        CPU_KIND,
        &cpu,
        CPU_SAMPLES,
        CPU_PCT,
        CPU_CLEAR_PCT,
        "CPU",
        "15 minutes",
    );
}

#[allow(clippy::too_many_arguments)]
fn sustained(
    shared: &Shared,
    kind: &str,
    values: &[f64],
    n: usize,
    raise: f64,
    clear: f64,
    what: &str,
    window: &str,
) {
    let Some(avg) = recent_mean(values, n) else {
        return;
    };
    if level(avg, raise, clear, is_open(shared, kind)) {
        notify::raise(
            shared,
            NewEvent::new(
                "host",
                kind,
                Severity::Warning,
                &format!("{what} use is high ({avg:.0}%)"),
            )
            .body(format!("Averaged over the last {window}."))
            .link("/metrics"),
        );
    } else {
        notify::resolve(shared, kind);
    }
}

/// Containers with a problem now, as (key, name, reason).
fn container_problems(list: &[ContainerInfo]) -> Vec<(String, String, String)> {
    list.iter()
        .filter_map(|c| {
            container_problem(c)
                .map(|why| (format!("{CONTAINER_KIND}:{}", c.name), c.name.clone(), why))
        })
        .collect()
}

/// Raise for problems seen on two polls in a row (an update recreates containers for a moment);
/// close the events of containers that are fine again. Returns the keys seen this round.
pub fn check_containers(
    shared: &Shared,
    list: &[ContainerInfo],
    previous: &HashSet<String>,
) -> HashSet<String> {
    let problems = container_problems(list);
    let now: HashSet<String> = problems.iter().map(|p| p.0.clone()).collect();
    let mut confirmed = HashSet::new();
    for (key, name, why) in problems {
        if !previous.contains(&key) {
            continue;
        }
        notify::raise(
            shared,
            NewEvent::new(
                "docker",
                CONTAINER_KIND,
                Severity::Warning,
                &format!("Container {name}: {why}"),
            )
            .body("Open the container's logs to see why.")
            .link("/containers")
            .key(key.clone()),
        );
        confirmed.insert(key);
    }
    // Keep open events whose problem is still there, even on the first sighting after a restart
    // of the panel; resolve the rest.
    let keep: HashSet<String> = confirmed.union(&now).cloned().collect();
    resolve_stale(shared, &format!("{CONTAINER_KIND}:"), &keep);
    now
}

pub fn spawn(shared: Shared, metrics: metrics::Shared, docker: DockerClient) {
    tokio::spawn(async move {
        let mut previous = HashSet::new();
        loop {
            tokio::time::sleep(POLL).await;
            {
                let m = metrics.lock().unwrap();
                check_host(&shared, &m);
            }
            if let Ok(list) = docker.containers().await {
                previous = check_containers(&shared, &list, &previous);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{DiskSeries, Sample};
    use std::collections::VecDeque;

    fn store() -> Shared {
        std::sync::Arc::new(std::sync::Mutex::new(notify::Store::memory()))
    }

    fn open(s: &Shared) -> Vec<(String, Severity)> {
        let g = s.lock().unwrap();
        g.open_events()
            .iter()
            .map(|e| (e.key.clone(), e.severity))
            .collect()
    }

    fn container(name: &str, state: &str, status: &str, managed: bool) -> ContainerInfo {
        ContainerInfo {
            id: "abc".into(),
            name: name.into(),
            image: "img".into(),
            state: state.into(),
            status: status.into(),
            managed,
        }
    }

    #[test]
    fn level_has_hysteresis() {
        assert!(level(86.0, 85.0, 80.0, false));
        assert!(!level(83.0, 85.0, 80.0, false));
        assert!(level(83.0, 85.0, 80.0, true));
        assert!(!level(79.0, 85.0, 80.0, true));
    }

    #[test]
    fn recent_mean_needs_enough_samples() {
        assert_eq!(recent_mean(&[1.0, 2.0], 3), None);
        assert_eq!(recent_mean(&[100.0, 2.0, 4.0], 2), Some(3.0));
    }

    #[test]
    fn container_problems_are_classified() {
        let p = |s: &str, st: &str, m| container_problem(&container("c", s, st, m));
        assert!(p("restarting", "Restarting (1) 2 seconds ago", false).is_some());
        assert!(p("running", "Up 2 hours (unhealthy)", false).is_some());
        assert!(p("running", "Up 2 hours (healthy)", false).is_none());
        assert!(
            p("exited", "Exited (1) 3 minutes ago", true)
                .unwrap()
                .contains("exit code 1")
        );
        assert!(p("exited", "Exited (0) 3 minutes ago", true).is_none());
        assert!(p("exited", "Exited (137) 3 minutes ago", true).is_none());
        // Containers the panel does not manage are only judged while they run.
        assert!(p("exited", "Exited (1) 3 minutes ago", false).is_none());
    }

    #[test]
    fn container_alert_needs_two_polls_and_clears() {
        let s = store();
        let bad = vec![container("web", "restarting", "Restarting (1) now", true)];
        let first = check_containers(&s, &bad, &HashSet::new());
        assert!(open(&s).is_empty(), "one sighting is not enough");
        let second = check_containers(&s, &bad, &first);
        assert_eq!(
            open(&s),
            vec![("container.problem:web".into(), Severity::Warning)]
        );
        // still broken: stays open, no duplicate
        check_containers(&s, &bad, &second);
        assert_eq!(open(&s).len(), 1);
        let good = vec![container("web", "running", "Up 5 seconds", true)];
        check_containers(&s, &good, &second);
        assert!(open(&s).is_empty());
    }

    fn metrics_with(disk_pct: f64, mem: f64, cpu: f64, n: usize) -> Metrics {
        let mut m = Metrics::default();
        m.disks.push(DiskSeries::for_test(
            "/data",
            100,
            disk_pct as u64,
            disk_pct,
        ));
        m.samples = (0..n)
            .map(|i| Sample {
                t: i as u64,
                cpu,
                mem,
                rx: 0.0,
                tx: 0.0,
            })
            .collect::<VecDeque<_>>();
        m
    }

    #[test]
    fn disk_warns_escalates_and_clears_with_hysteresis() {
        let s = store();
        check_host(&s, &metrics_with(86.0, 10.0, 10.0, 0));
        assert_eq!(
            open(&s),
            vec![("host.disk_full:/data".into(), Severity::Warning)]
        );
        check_host(&s, &metrics_with(96.0, 10.0, 10.0, 0));
        assert_eq!(open(&s)[0].1, Severity::Critical);
        check_host(&s, &metrics_with(82.0, 10.0, 10.0, 0));
        assert_eq!(open(&s).len(), 1, "between the levels it stays open");
        check_host(&s, &metrics_with(70.0, 10.0, 10.0, 0));
        assert!(open(&s).is_empty());
    }

    #[test]
    fn memory_and_cpu_need_a_full_window() {
        let s = store();
        check_host(&s, &metrics_with(10.0, 95.0, 95.0, 29));
        assert!(open(&s).is_empty(), "window not full yet");
        check_host(&s, &metrics_with(10.0, 95.0, 95.0, 30));
        assert_eq!(
            open(&s),
            vec![("host.memory_high".into(), Severity::Warning)]
        );
        check_host(&s, &metrics_with(10.0, 95.0, 95.0, 90));
        assert_eq!(open(&s).len(), 2);
        check_host(&s, &metrics_with(10.0, 20.0, 20.0, 90));
        assert!(open(&s).is_empty());
    }
}
