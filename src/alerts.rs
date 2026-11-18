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
pub const DISK_FORECAST_KIND: &str = "host.disk_forecast";
pub const MEM_KIND: &str = "host.memory_high";
pub const CPU_KIND: &str = "host.cpu_high";
pub const CONTAINER_KIND: &str = "container.problem";
pub const PORT_KIND: &str = "container.port_open";
pub const INODE_KIND: &str = "host.inodes_high";
pub const LOAD_KIND: &str = "host.load_high";
pub const LISTEN_KIND: &str = "host.listener";

const DISK_CRIT_PCT: f64 = 95.0;
const DISK_CLEAR_PCT: f64 = 80.0;
/// Warn when the weekly trend fills a disk within this many days; clear above the second value.
const FORECAST_WARN_DAYS: f64 = 14.0;
const FORECAST_CLEAR_DAYS: f64 = 21.0;
const INODE_PCT: f64 = 85.0;
const INODE_CRIT_PCT: f64 = 95.0;
const INODE_CLEAR_PCT: f64 = 80.0;
/// 15-minute load average per CPU core: raised at this, cleared below the lower figure.
const LOAD_PER_CORE: f64 = 2.0;
const LOAD_CLEAR_PER_CORE: f64 = 1.5;
const INODE_EVERY: u32 = 5; // polls (minutes) between inode checks
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
        "exited" if c.managed && c.oom_killed => {
            Some("it was killed for running out of memory".into())
        }
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

    // Early warning: the disk is still under the fixed warning level but is filling fast.
    let mut open_f = HashSet::new();
    for d in &m.disks {
        let Some(&(_, pct)) = d.pct.back() else {
            continue;
        };
        let key = format!("{DISK_FORECAST_KIND}:{}", d.label);
        let Some(days) = d.days_until_full(metrics::FORECAST_ALERT_SECS) else {
            continue;
        };
        let on = days < FORECAST_WARN_DAYS || (is_open(shared, &key) && days < FORECAST_CLEAR_DAYS);
        if !on || pct >= metrics::DISK_WARN_PCT {
            continue; // at 85 % the regular alert above takes over
        }
        open_f.insert(key.clone());
        notify::raise(
            shared,
            NewEvent::new(
                "host",
                DISK_FORECAST_KIND,
                Severity::Warning,
                &format!("Disk {} will be full in about {} days", d.label, days.round().max(1.0)),
            )
            .body(format!(
                "At the pace of the last week, {} ({pct:.0}% used now) fills up soon. Free space or old snapshots, or add disk.",
                d.label
            ))
            .link("/metrics")
            .key(key),
        );
    }
    resolve_stale(shared, &format!("{DISK_FORECAST_KIND}:"), &open_f);

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

/// `df -P -i` output: (filesystem, inodes total, inodes used). `None` for filesystems that
/// have no fixed inode count (btrfs, some network mounts report 0 or `-`).
pub fn parse_df_inodes(text: &str) -> Option<(String, u64, u64)> {
    let f: Vec<&str> = text.lines().nth(1)?.split_whitespace().collect();
    if f.len() < 6 {
        return None;
    }
    let total: u64 = f[1].parse().ok()?;
    let used: u64 = f[2].parse().ok()?;
    (total > 0).then(|| (f[0].to_string(), total, used))
}

async fn df_inodes(path: &str) -> Option<(String, u64, u64)> {
    let out = tokio::process::Command::new("df")
        .args(["-P", "-i", path])
        .output()
        .await
        .ok()?;
    parse_df_inodes(&String::from_utf8_lossy(&out.stdout))
}

/// One alert per filesystem that is running out of inodes (many small files fill these long
/// before the space is gone). `rows` are (label, total, used).
pub fn check_inodes(shared: &Shared, rows: &[(String, u64, u64)]) {
    let mut open = HashSet::new();
    for (label, total, used) in rows {
        let pct = *used as f64 / *total as f64 * 100.0;
        let key = format!("{INODE_KIND}:{label}");
        if !level(pct, INODE_PCT, INODE_CLEAR_PCT, is_open(shared, &key)) {
            continue;
        }
        let sev = if pct >= INODE_CRIT_PCT {
            Severity::Critical
        } else {
            Severity::Warning
        };
        open.insert(key.clone());
        notify::raise(
            shared,
            NewEvent::new(
                "host",
                INODE_KIND,
                sev,
                &format!("Disk {label} has used {pct:.0}% of its inodes"),
            )
            .body(format!(
                "{used} of {total} file slots (inodes) are taken. When they run out nothing new can be written even with free space; look for folders with millions of small files (caches, mail, sessions)."
            ))
            .link("/metrics")
            .key(key),
        );
    }
    resolve_stale(shared, &format!("{INODE_KIND}:"), &open);
}

/// 15-minute load average from `/proc/loadavg` text.
pub fn parse_load15(text: &str) -> Option<f64> {
    text.split_whitespace().nth(2)?.parse().ok()
}

/// Sustained load: the 15-minute average per core, with hysteresis.
pub fn check_load(shared: &Shared, load15: f64, cores: usize) {
    let per_core = load15 / cores.max(1) as f64;
    if level(
        per_core,
        LOAD_PER_CORE,
        LOAD_CLEAR_PER_CORE,
        is_open(shared, LOAD_KIND),
    ) {
        notify::raise(
            shared,
            NewEvent::new(
                "host",
                LOAD_KIND,
                Severity::Warning,
                &format!("Load is high ({load15:.1} on {cores} cores)"),
            )
            .body("The 15-minute load average is at least twice the number of cores: more work is waiting for the CPU or the disk than the server can handle. Check Containers for the busiest app.")
            .link("/metrics"),
        );
    } else {
        notify::resolve(shared, LOAD_KIND);
    }
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

/// Running containers with a port published on every network interface: `(key, container,
/// "8080/tcp")`. Docker opens these in its own firewall chain, so ufw does not block them.
pub fn open_ports(list: &[ContainerInfo]) -> Vec<(String, String, String)> {
    let mut v = Vec::new();
    for c in list.iter().filter(|c| c.state == "running") {
        for p in &c.ports {
            if p.exposure() == crate::docker::Exposure::Everywhere {
                let what = format!("{}/{}", p.port, p.proto);
                v.push((
                    format!("{PORT_KIND}:{}:{what}", c.name),
                    c.name.clone(),
                    what,
                ));
            }
        }
    }
    v
}

/// Raise for each newly open port, close the events of ports that are closed or rebound.
pub fn check_ports(shared: &Shared, list: &[ContainerInfo]) {
    let mut keep = HashSet::new();
    for (key, name, what) in open_ports(list) {
        notify::raise(
            shared,
            NewEvent::new(
                "docker",
                PORT_KIND,
                Severity::Warning,
                &format!("Container {name} listens on port {what} on every network interface"),
            )
            .body("Docker bypasses the ufw firewall for published ports, so this may be reachable from the internet. Publish it on 127.0.0.1 (or a Tailscale/LAN address) unless that is intended.")
            .link("/containers")
            .key(key.clone()),
        );
        keep.insert(key);
    }
    resolve_stale(shared, &format!("{PORT_KIND}:"), &keep);
}

/// Host sockets listening on every interface that neither Docker nor the allowlist (ssh, Caddy
/// while it runs, `DOCKWREN_LISTEN_ALLOW`) explains.
pub fn host_listeners(list: &[ContainerInfo]) -> Vec<(u16, String)> {
    let mut allow: Vec<u16> = crate::listeners::BASE_ALLOW.to_vec();
    if list
        .iter()
        .any(|c| c.name == "dockwren-caddy" && c.state == "running")
    {
        allow.extend([80, 443]);
    }
    if let Ok(extra) = std::env::var("DOCKWREN_LISTEN_ALLOW") {
        allow.extend(crate::listeners::parse_allow(&extra));
    }
    let docker: std::collections::BTreeSet<u16> = list
        .iter()
        .flat_map(|c| c.ports.iter().map(|p| p.port))
        .collect();
    let found = crate::listeners::scan();
    let ports: std::collections::BTreeSet<u16> = found.keys().copied().collect();
    let hosts: Vec<String> = list
        .iter()
        .filter(|c| c.state == "running" && c.host_network)
        .map(|c| c.name.clone())
        .collect();
    crate::listeners::unexpected(&ports, &allow, &docker)
        .into_iter()
        .map(|p| {
            let who = crate::listeners::describe_owner(
                found.get(&p).and_then(|i| crate::listeners::owner_of(*i)),
                &hosts,
            );
            (p, who)
        })
        .collect()
}

/// Raise for each unexplained wildcard listener, close the events of ports that went away.
pub fn check_listeners(shared: &Shared, list: &[ContainerInfo]) {
    let mut keep = HashSet::new();
    for (port, who) in host_listeners(list) {
        let key = format!("{LISTEN_KIND}:{port}");
        notify::raise(
            shared,
            NewEvent::new(
                "host",
                LISTEN_KIND,
                Severity::Warning,
                &format!("Something on this host listens on port {port} on every network interface"),
            )
            .body(format!("Likely owner: {who}. Not explained by Docker, ssh or Caddy, so it may be reachable from the internet. Bind it to 127.0.0.1, or set DOCKWREN_LISTEN_ALLOW if it is intended."))
            .link("/settings")
            .key(key.clone()),
        );
        keep.insert(key);
    }
    resolve_stale(shared, &format!("{LISTEN_KIND}:"), &keep);
}

pub fn spawn(
    shared: Shared,
    metrics: metrics::Shared,
    docker: DockerClient,
    disk_paths: Vec<String>,
) {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    tokio::spawn(async move {
        let mut previous = HashSet::new();
        let mut tick = 0u32;
        loop {
            tokio::time::sleep(POLL).await;
            if let Some(l) =
                parse_load15(&std::fs::read_to_string("/proc/loadavg").unwrap_or_default())
            {
                check_load(&shared, l, cores);
            }
            if tick.is_multiple_of(INODE_EVERY) {
                let mut rows: Vec<(String, u64, u64)> = Vec::new();
                let mut seen = HashSet::new();
                for p in &disk_paths {
                    if let Some((fs, total, used)) = df_inodes(p).await
                        && seen.insert(fs)
                    {
                        rows.push((p.clone(), total, used));
                    }
                }
                check_inodes(&shared, &rows);
            }
            tick = tick.wrapping_add(1);
            {
                let m = metrics.lock().unwrap();
                check_host(&shared, &m);
            }
            if let Ok(list) = docker.containers().await {
                check_ports(&shared, &list);
                check_listeners(&shared, &list);
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
            oom_killed: false,
            ports: vec![],
            host_network: false,
        }
    }

    #[test]
    fn only_wildcard_bindings_of_running_containers_count() {
        use crate::docker::PublishedPort;
        let p = |ip: &str, port| PublishedPort {
            ip: ip.into(),
            port,
            proto: "tcp".into(),
        };
        let mut web = container("web", "running", "Up", true);
        web.ports = vec![
            p("127.0.0.1", 8100),
            p("0.0.0.0", 8080),
            p("203.0.113.5", 9000),
        ];
        let mut off = container("off", "exited", "Exited (0)", true);
        off.ports = vec![p("::", 81)];
        let got = open_ports(&[web, off]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "container.port_open:web:8080/tcp");
    }

    #[test]
    fn inodes_warn_escalate_and_clear() {
        let s = store();
        check_inodes(&s, &[("/data".into(), 1000, 500)]);
        assert!(open(&s).is_empty());
        check_inodes(&s, &[("/data".into(), 1000, 860)]);
        assert_eq!(
            open(&s),
            vec![("host.inodes_high:/data".into(), Severity::Warning)]
        );
        check_inodes(&s, &[("/data".into(), 1000, 970)]);
        assert_eq!(open(&s)[0].1, Severity::Critical);
        check_inodes(&s, &[("/data".into(), 1000, 820)]); // above the clear level: stays
        assert_eq!(open(&s).len(), 1);
        check_inodes(&s, &[("/data".into(), 1000, 700)]);
        assert!(open(&s).is_empty());
    }

    #[test]
    fn df_inodes_parses_and_skips_filesystems_without_a_count() {
        let ok = "Filesystem Inodes IUsed IFree IUse% Mounted on\n/dev/sda1 1000 250 750 25% /\n";
        assert_eq!(parse_df_inodes(ok), Some(("/dev/sda1".into(), 1000, 250)));
        let btrfs = "Filesystem Inodes IUsed IFree IUse% Mounted on\n/dev/sdb 0 0 0 - /mnt\n";
        assert_eq!(parse_df_inodes(btrfs), None);
    }

    #[test]
    fn load_is_judged_per_core_with_hysteresis() {
        let s = store();
        assert_eq!(parse_load15("0.10 0.20 3.50 1/200 999\n"), Some(3.5));
        check_load(&s, 7.0, 4);
        assert!(open(&s).is_empty());
        check_load(&s, 8.0, 4);
        assert_eq!(open(&s).len(), 1);
        check_load(&s, 6.5, 4); // 1.6 per core: above the clear level
        assert_eq!(open(&s).len(), 1);
        check_load(&s, 5.0, 4);
        assert!(open(&s).is_empty());
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
        let mut oom = container("c", "exited", "Exited (137) 3 minutes ago", true);
        oom.oom_killed = true;
        assert!(container_problem(&oom).unwrap().contains("memory"));
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
    fn disk_forecast_warns_early_and_clears() {
        let s = store();
        let day = 86400;
        let mk = |step: f64| {
            let mut m = metrics_with(40.0, 10.0, 10.0, 0);
            let d = &mut m.disks[0];
            d.pct = (0..=6).map(|i| (i * day, 40.0 + i as f64 * step)).collect();
            m
        };
        check_host(&s, &mk(4.0)); // last 64 %, 36/4 = 9 days
        assert_eq!(
            open(&s),
            vec![("host.disk_forecast:/data".into(), Severity::Warning)]
        );
        check_host(&s, &mk(3.0)); // last 58 %, 42/3 = 14 days: still open (hysteresis)
        assert_eq!(open(&s).len(), 1);
        check_host(&s, &mk(0.5)); // last 43 %, 114 days: clears
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
