//! Host and container metrics for the Overview graphs.
//!
//! A background task samples /proc every 10 s and keeps about 24 h in memory (no database, lost on
//! restart). The page draws plain inline SVG lines; the parsers below are pure and unit-tested.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::docker::{ContainerUsage, DockerClient};

const INTERVAL_SECS: u64 = 10;
const KEEP_SAMPLES: usize = 24 * 3600 / INTERVAL_SECS as usize;
/// Long-term history: one averaged point per 5 minutes, kept 30 days, persisted to disk.
const LONG_EVERY: u64 = 30; // ticks (= 5 minutes)
const LONG_SECS: u64 = LONG_EVERY * INTERVAL_SECS;
const KEEP_LONG: usize = 30 * 24 * 3600 / LONG_SECS as usize;
const DISK_EVERY: u64 = LONG_EVERY; // disk space moves slowly: checked every 5 minutes
const KEEP_DISK: usize = KEEP_LONG;
const LOG_FILE: &str = "metrics.log";
/// A disk this full is flagged in the UI.
pub const DISK_WARN_PCT: f64 = 85.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub t: u64,
    pub cpu: f64, // percent of the whole machine
    pub mem: f64, // percent used
    pub rx: f64,  // bytes per second
    pub tx: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiskSeries {
    pub label: String,
    fs: String,
    pub total: u64,
    pub used: u64,
    pub pct: VecDeque<(u64, f64)>,
}

/// One container's history: (time, cpu %, memory bytes). `short` is 10 s resolution for 24 h,
/// `long` is 5-minute averages for 30 days (persisted).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ContainerSeries {
    pub short: VecDeque<(u64, f64, f64)>,
    pub long: VecDeque<(u64, f64, f64)>,
}

#[derive(Debug, Default)]
pub struct Metrics {
    pub per: BTreeMap<String, ContainerSeries>,
    pub samples: VecDeque<Sample>,
    /// 5-minute averages, up to 30 days (also persisted).
    pub long: VecDeque<Sample>,
    pub mem_total: u64,
    pub mem_used: u64,
    pub disks: Vec<DiskSeries>,
    pub containers: Vec<ContainerUsage>,
}

pub type Shared = Arc<Mutex<Metrics>>;

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

// ---- parsers (pure) ----

/// First line of /proc/stat: (busy, total) jiffies.
pub fn parse_cpu(text: &str) -> Option<(u64, u64)> {
    let line = text.lines().next()?;
    let mut f = line.split_whitespace();
    if f.next()? != "cpu" {
        return None;
    }
    let v: Vec<u64> = f.take(8).map(|x| x.parse().ok()).collect::<Option<_>>()?;
    if v.len() < 8 {
        return None;
    }
    let total: u64 = v.iter().sum();
    let idle = v[3] + v[4];
    Some((total - idle, total))
}

/// /proc/meminfo: (MemTotal, MemAvailable) in bytes.
pub fn parse_meminfo(text: &str) -> Option<(u64, u64)> {
    let get = |key: &str| -> Option<u64> {
        let line = text.lines().find(|l| l.starts_with(key))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb * 1024)
    };
    Some((get("MemTotal:")?, get("MemAvailable:")?))
}

fn skip_iface(name: &str) -> bool {
    name == "lo"
        || name.starts_with("docker")
        || name.starts_with("br-")
        || name.starts_with("veth")
}

/// /proc/net/dev: total (rx, tx) bytes over physical-ish interfaces.
pub fn parse_net_dev(text: &str) -> (u64, u64) {
    let (mut rx, mut tx) = (0u64, 0u64);
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        if skip_iface(name.trim()) {
            continue;
        }
        let f: Vec<&str> = rest.split_whitespace().collect();
        if f.len() < 9 {
            continue;
        }
        rx += f[0].parse().unwrap_or(0);
        tx += f[8].parse().unwrap_or(0);
    }
    (rx, tx)
}

/// `df -P -k PATH` output: (filesystem, total bytes, used bytes). Total is used+available, like df's Use%.
pub fn parse_df(text: &str) -> Option<(String, u64, u64)> {
    let line = text.lines().nth(1)?;
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() < 6 {
        return None;
    }
    let used: u64 = f[2].parse().ok()?;
    let avail: u64 = f[3].parse().ok()?;
    Some((f[0].to_string(), (used + avail) * 1024, used * 1024))
}

// ---- persistence ----

fn sample_line(s: &Sample) -> String {
    format!(
        "S\t{}\t{:.2}\t{:.2}\t{:.1}\t{:.1}\n",
        s.t, s.cpu, s.mem, s.rx, s.tx
    )
}

fn disk_line(d: &DiskSeries, t: u64, pct: f64) -> String {
    format!(
        "D\t{t}\t{}\t{}\t{}\t{}\t{pct:.2}\n",
        d.label, d.fs, d.total, d.used
    )
}

fn container_line(name: &str, p: (u64, f64, f64)) -> String {
    format!("C\t{}\t{name}\t{:.2}\t{:.0}\n", p.0, p.1, p.2)
}

/// Rebuilds the long-term history from log text, dropping anything older than 30 days.
pub fn load_log(text: &str, now: u64) -> Metrics {
    let cutoff = now.saturating_sub(30 * 24 * 3600);
    let mut m = Metrics::default();
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let Some(t) = f
            .get(1)
            .and_then(|x| x.parse::<u64>().ok())
            .filter(|t| *t >= cutoff)
        else {
            continue;
        };
        match (f[0], f.len()) {
            ("S", 6) => {
                let n: Vec<Option<f64>> = f[2..].iter().map(|x| x.parse().ok()).collect();
                if let [Some(cpu), Some(mem), Some(rx), Some(tx)] = n[..] {
                    m.long.push_back(Sample {
                        t,
                        cpu,
                        mem,
                        rx,
                        tx,
                    });
                }
            }
            ("D", 7) => {
                let (Ok(total), Ok(used), Ok(pct)) = (f[4].parse(), f[5].parse(), f[6].parse())
                else {
                    continue;
                };
                let i = match m.disks.iter().position(|d| d.fs == f[3]) {
                    Some(i) => i,
                    None => {
                        m.disks.push(DiskSeries {
                            label: f[2].into(),
                            fs: f[3].into(),
                            total,
                            used,
                            pct: VecDeque::new(),
                        });
                        m.disks.len() - 1
                    }
                };
                let d = &mut m.disks[i];
                (d.total, d.used) = (total, used);
                d.pct.push_back((t, pct));
            }
            ("C", 5) => {
                if let (Ok(cpu), Ok(mem)) = (f[3].parse::<f64>(), f[4].parse::<f64>()) {
                    m.per
                        .entry(f[2].to_string())
                        .or_default()
                        .long
                        .push_back((t, cpu, mem));
                }
            }
            _ => {}
        }
    }
    for c in m.per.values_mut() {
        while c.long.len() > KEEP_LONG {
            c.long.pop_front();
        }
    }
    while m.long.len() > KEEP_LONG {
        m.long.pop_front();
    }
    for d in &mut m.disks {
        while d.pct.len() > KEEP_DISK {
            d.pct.pop_front();
        }
    }
    m
}

/// Writes the compacted history back (startup) so the file never grows beyond ~30 days.
fn compact_log(path: &std::path::Path, m: &Metrics) {
    let mut out = String::new();
    for s in &m.long {
        out.push_str(&sample_line(s));
    }
    for d in &m.disks {
        for (t, pct) in &d.pct {
            out.push_str(&disk_line(d, *t, *pct));
        }
    }
    for (name, c) in &m.per {
        for p in &c.long {
            out.push_str(&container_line(name, *p));
        }
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, out).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn append_log(path: &std::path::Path, text: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(text.as_bytes());
    }
}

// ---- sampling ----

fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

async fn df(path: &str) -> Option<(String, u64, u64)> {
    let out = tokio::process::Command::new("df")
        .args(["-P", "-k", path])
        .output()
        .await
        .ok()?;
    parse_df(&String::from_utf8_lossy(&out.stdout))
}

async fn sample_disks(shared: &Shared, paths: &[String], t: u64, log: &std::path::Path) {
    for path in paths {
        let Some((fs, total, used)) = df(path).await else {
            continue;
        };
        let pct = if total == 0 {
            0.0
        } else {
            used as f64 / total as f64 * 100.0
        };
        let mut m = shared.lock().unwrap();
        // Several paths on one filesystem are shown once.
        let slot = match m.disks.iter().position(|d| d.fs == fs) {
            Some(i) => i,
            None => {
                m.disks.push(DiskSeries {
                    label: path.clone(),
                    fs,
                    total,
                    used,
                    pct: VecDeque::new(),
                });
                m.disks.len() - 1
            }
        };
        let d = &mut m.disks[slot];
        d.total = total;
        d.used = used;
        d.pct.push_back((t, pct));
        if d.pct.len() > KEEP_DISK {
            d.pct.pop_front();
        }
        let line = disk_line(d, t, pct);
        drop(m);
        append_log(log, &line);
    }
}

/// Mean of samples, stamped with the newest time.
pub fn average<'a>(it: impl Iterator<Item = &'a Sample>) -> Sample {
    let v: Vec<&Sample> = it.collect();
    let n = v.len().max(1) as f64;
    let sum = |f: fn(&Sample) -> f64| v.iter().map(|s| f(s)).sum::<f64>() / n;
    Sample {
        t: v.iter().map(|s| s.t).max().unwrap_or(0),
        cpu: sum(|s| s.cpu),
        mem: sum(|s| s.mem),
        rx: sum(|s| s.rx),
        tx: sum(|s| s.tx),
    }
}

/// Adds one Docker stats reading to the per-container histories. Every `LONG_EVERY`th reading
/// (`long`) also stores a 5-minute average; returns the log lines to persist. Series with no data
/// for 30 days are dropped.
pub fn record_containers(
    m: &mut Metrics,
    list: &[ContainerUsage],
    t: u64,
    long: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    for u in list {
        let c = m.per.entry(u.name.clone()).or_default();
        c.short.push_back((t, u.cpu_pct, u.mem_bytes as f64));
        if c.short.len() > KEEP_SAMPLES {
            c.short.pop_front();
        }
        if long {
            let n = c.short.len().min(LONG_EVERY as usize);
            let recent = c.short.iter().rev().take(n);
            let (cpu, mem) = recent.fold((0.0, 0.0), |a, p| (a.0 + p.1, a.1 + p.2));
            let avg = (t, cpu / n as f64, mem / n as f64);
            c.long.push_back(avg);
            if c.long.len() > KEEP_LONG {
                c.long.pop_front();
            }
            lines.push(container_line(&u.name, avg));
        }
    }
    let cutoff = t.saturating_sub(30 * 24 * 3600);
    m.per.retain(|_, c| {
        c.short
            .back()
            .into_iter()
            .chain(c.long.back())
            .any(|p| p.0 >= cutoff)
    });
    lines
}

/// Previous reading: cpu (busy, total), net (rx, tx), time.
type Prev = ((u64, u64), (u64, u64), u64);

/// Starts the sampler; `disk_paths` are the directories whose filesystems are tracked.
pub fn spawn(
    shared: Shared,
    docker: DockerClient,
    disk_paths: Vec<String>,
    data_dir: std::path::PathBuf,
) {
    let log = data_dir.join(LOG_FILE);
    {
        let mut m = shared.lock().unwrap();
        let loaded = load_log(&read(&log.to_string_lossy()), now_unix());
        compact_log(&log, &loaded);
        m.long = loaded.long;
        m.disks = loaded.disks;
        m.per = loaded.per;
    }
    let usage = shared.clone();
    let usage_log = log.clone();
    tokio::spawn(async move {
        let log = usage_log;
        let mut n = 0u64;
        loop {
            if let Ok(list) = docker.usage().await {
                n += 1;
                let lines = {
                    let mut m = usage.lock().unwrap();
                    let lines =
                        record_containers(&mut m, &list, now_unix(), n.is_multiple_of(LONG_EVERY));
                    m.containers = list;
                    lines
                };
                if !lines.is_empty() {
                    append_log(&log, &lines.concat());
                }
            }
            tokio::time::sleep(Duration::from_secs(INTERVAL_SECS)).await;
        }
    });
    tokio::spawn(async move {
        let mut prev: Option<Prev> = None;
        let mut tick = 0u64;
        let mut interval = tokio::time::interval(Duration::from_secs(INTERVAL_SECS));
        loop {
            interval.tick().await;
            let t = now_unix();
            if tick.is_multiple_of(DISK_EVERY) {
                sample_disks(&shared, &disk_paths, t, &log).await;
            }
            tick += 1;
            let (Some(cpu), Some((mem_total, mem_avail))) = (
                parse_cpu(&read("/proc/stat")),
                parse_meminfo(&read("/proc/meminfo")),
            ) else {
                continue;
            };
            let net = parse_net_dev(&read("/proc/net/dev"));
            if let Some((pc, pn, pt)) = prev {
                let dt = t.saturating_sub(pt).max(1) as f64;
                let dtotal = cpu.1.saturating_sub(pc.1);
                let cpu_pct = if dtotal == 0 {
                    0.0
                } else {
                    cpu.0.saturating_sub(pc.0) as f64 / dtotal as f64 * 100.0
                };
                let used = mem_total.saturating_sub(mem_avail);
                let s = Sample {
                    t,
                    cpu: cpu_pct,
                    mem: used as f64 / mem_total.max(1) as f64 * 100.0,
                    rx: net.0.saturating_sub(pn.0) as f64 / dt,
                    tx: net.1.saturating_sub(pn.1) as f64 / dt,
                };
                let mut m = shared.lock().unwrap();
                m.mem_total = mem_total;
                m.mem_used = used;
                m.samples.push_back(s);
                if m.samples.len() > KEEP_SAMPLES {
                    m.samples.pop_front();
                }
                if tick.is_multiple_of(LONG_EVERY) && m.samples.len() >= LONG_EVERY as usize {
                    let avg = average(m.samples.iter().rev().take(LONG_EVERY as usize));
                    m.long.push_back(avg);
                    if m.long.len() > KEEP_LONG {
                        m.long.pop_front();
                    }
                    let line = sample_line(&avg);
                    drop(m);
                    append_log(&log, &line);
                }
            }
            prev = Some((cpu, net, t));
        }
    });
}

// ---- drawing ----

pub const W: f64 = 240.0;
pub const H: f64 = 60.0;
const MAX_POINTS: usize = 240;

/// Averages `(t, v)` points into at most `MAX_POINTS` buckets.
pub fn downsample(points: &[(u64, f64)]) -> Vec<(u64, f64)> {
    if points.len() <= MAX_POINTS {
        return points.to_vec();
    }
    let size = points.len().div_ceil(MAX_POINTS);
    points
        .chunks(size)
        .map(|c| {
            (
                c[c.len() / 2].0,
                c.iter().map(|p| p.1).sum::<f64>() / c.len() as f64,
            )
        })
        .collect()
}

/// SVG `points` attribute: x follows time from the first point inside the window to `end` (so a
/// short history fills the chart), y runs 0..`max` bottom to top. A single point becomes a flat line.
pub fn polyline(points: &[(u64, f64)], start: u64, end: u64, max: f64) -> String {
    let pts: Vec<(u64, f64)> = downsample(points)
        .into_iter()
        .filter(|(t, _)| *t >= start)
        .collect();
    let Some(first) = pts.first().map(|p| p.0) else {
        return String::new();
    };
    let span = end.saturating_sub(first).max(1) as f64;
    let y = |v: f64| H - (v / max).clamp(0.0, 1.0) * H;
    if let [(_, v)] = pts[..] {
        return format!("0.0,{0:.1} {W:.1},{0:.1}", y(v));
    }
    pts.iter()
        .map(|(t, v)| format!("{:.1},{:.1}", (t - first) as f64 / span * W, y(*v)))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn fmt_bytes(b: f64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut i) = (b, 0);
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} B")
    } else {
        format!("{v:.1} {}", units[i])
    }
}

/// Smallest "nice" ceiling (1, 2, 5 x 10^n) at or above `v`, never below 1 KB/s.
pub fn nice_max(v: f64) -> f64 {
    let v = v.max(1024.0);
    let p = 10f64.powf(v.log10().floor());
    [1.0, 2.0, 5.0, 10.0]
        .iter()
        .map(|m| m * p)
        .find(|c| *c >= v)
        .unwrap_or(10.0 * p)
}

/// Smallest 1, 2, 5 x 10^n percent ceiling at or above `v`, never below 5.
pub fn nice_pct(v: f64) -> f64 {
    let v = v.max(5.0);
    let p = 10f64.powf(v.log10().floor());
    [1.0, 2.0, 5.0, 10.0]
        .iter()
        .map(|m| m * p)
        .find(|c| *c >= v)
        .unwrap_or(10.0 * p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_stat() {
        let text = "cpu  100 0 50 800 40 5 5 0 0 0\ncpu0 1 2 3 4 5 6 7 8 9 10\n";
        // total = 100+0+50+800+40+5+5+0 = 1000, idle = 840
        assert_eq!(parse_cpu(text), Some((160, 1000)));
        assert_eq!(parse_cpu("intr 1 2 3"), None);
        assert_eq!(parse_cpu("cpu  1 2 3"), None);
    }

    #[test]
    fn parses_meminfo() {
        let text =
            "MemTotal:       16000000 kB\nMemFree:         100 kB\nMemAvailable:    4000000 kB\n";
        assert_eq!(
            parse_meminfo(text),
            Some((16_000_000 * 1024, 4_000_000 * 1024))
        );
        assert_eq!(parse_meminfo("MemTotal: 1 kB\n"), None);
    }

    #[test]
    fn net_dev_skips_loopback_and_docker() {
        let text = "Inter-|   Receive                                                |  Transmit\n \
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    \
lo: 999 1 0 0 0 0 0 0 999 1 0 0 0 0 0 0\n  \
eno1: 1000 5 0 0 0 0 0 0 2000 6 0 0 0 0 0 0\n\
docker0: 777 1 0 0 0 0 0 0 777 1 0 0 0 0 0 0\n\
veth12ab: 555 1 0 0 0 0 0 0 555 1 0 0 0 0 0 0\n\
tailscale0: 10 1 0 0 0 0 0 0 20 1 0 0 0 0 0 0\n";
        assert_eq!(parse_net_dev(text), (1010, 2020));
    }

    #[test]
    fn parses_df() {
        let text = "Filesystem     1024-blocks     Used Available Capacity Mounted on\n/dev/sda2         1000000   400000    500000      45% /\n";
        assert_eq!(
            parse_df(text),
            Some(("/dev/sda2".into(), 900_000 * 1024, 400_000 * 1024))
        );
        assert_eq!(parse_df("Filesystem\n"), None);
    }

    #[test]
    fn polyline_maps_time_and_value() {
        let pts = [(100, 0.0), (150, 50.0), (200, 100.0)];
        assert_eq!(
            polyline(&pts, 100, 200, 100.0),
            "0.0,60.0 120.0,30.0 240.0,0.0"
        );
        // points before the window are dropped, values above max are clamped
        assert_eq!(
            polyline(&[(50, 5.0), (200, 500.0)], 100, 200, 100.0),
            "0.0,0.0 240.0,0.0"
        );
        // short history stretches over the chart
        assert_eq!(
            polyline(&[(190, 0.0), (200, 100.0)], 0, 200, 100.0),
            "0.0,60.0 240.0,0.0"
        );
        assert_eq!(polyline(&[], 0, 10, 1.0), "");
    }

    #[test]
    fn downsample_caps_points_and_averages() {
        let pts: Vec<(u64, f64)> = (0..960)
            .map(|i| (i, if i % 2 == 0 { 0.0 } else { 10.0 }))
            .collect();
        let d = downsample(&pts);
        assert!(d.len() <= MAX_POINTS);
        assert!(d.iter().all(|p| (p.1 - 5.0).abs() < 0.01));
        assert_eq!(downsample(&pts[..10]).len(), 10);
    }

    #[test]
    fn formats_bytes_and_scales() {
        assert_eq!(fmt_bytes(512.0), "512 B");
        assert_eq!(fmt_bytes(1536.0), "1.5 KB");
        assert_eq!(fmt_bytes(3.0 * 1024.0 * 1024.0 * 1024.0), "3.0 GB");
        assert_eq!(nice_max(0.0), 2000.0);
        assert_eq!(nice_max(3000.0), 5000.0);
        assert_eq!(nice_max(120_000.0), 200_000.0);
    }
}

// ---- page ----

use askama::Template;
use axum::{
    extract::{Query, State},
    response::Html,
};

pub struct Line {
    pub points: String,
    pub dashed: bool,
}

pub struct Chart {
    pub title: String,
    pub now: String,
    pub scale: String,
    pub lines: Vec<Line>,
    pub warn: bool,
}

#[derive(Template)]
#[template(path = "metrics.html")]
struct MetricsTemplate {
    range: &'static str,
    ranges: Vec<(&'static str, &'static str)>,
    ready: bool,
    fresh: bool,
    charts: Vec<Chart>,
    w: f64,
    h: f64,
}

#[derive(serde::Deserialize)]
pub struct RangeQuery {
    range: Option<String>,
    /// Set by the page's own 10 s refresh; absent on first load and when a range is picked.
    live: Option<String>,
}

/// (query value, label, window in seconds)
const RANGES: [(&str, &str, u64); 4] = [
    ("1h", "1 h", 3600),
    ("24h", "24 h", 86400),
    ("7d", "7 days", 7 * 86400),
    ("30d", "30 days", 30 * 86400),
];

fn build(m: &Metrics, now: u64, range: &str, fresh: bool) -> MetricsTemplate {
    let (key, _, window) = *RANGES.iter().find(|r| r.0 == range).unwrap_or(&RANGES[0]);
    let source = if window > 86400 { &m.long } else { &m.samples };
    let start = now.saturating_sub(window);
    let recent: Vec<&Sample> = source.iter().filter(|s| s.t >= start).collect();
    let series =
        |f: fn(&Sample) -> f64| -> Vec<(u64, f64)> { recent.iter().map(|s| (s.t, f(s))).collect() };
    let last = recent.last();
    let mut charts = Vec::new();
    charts.push(Chart {
        title: "CPU".into(),
        now: last.map_or("–".into(), |s| format!("{:.0}%", s.cpu)),
        scale: "0–100%".into(),
        lines: vec![Line {
            points: polyline(&series(|s| s.cpu), start, now, 100.0),
            dashed: false,
        }],
        warn: false,
    });
    charts.push(Chart {
        title: "Memory".into(),
        now: last.map_or("–".into(), |s| {
            format!(
                "{:.0}% ({} of {})",
                s.mem,
                fmt_bytes(m.mem_used as f64),
                fmt_bytes(m.mem_total as f64)
            )
        }),
        scale: "0–100%".into(),
        lines: vec![Line {
            points: polyline(&series(|s| s.mem), start, now, 100.0),
            dashed: false,
        }],
        warn: false,
    });
    let peak = recent.iter().map(|s| s.rx.max(s.tx)).fold(0.0, f64::max);
    let top = nice_max(peak);
    charts.push(Chart {
        title: "Network (solid: in, dashed: out)".into(),
        now: last.map_or("–".into(), |s| {
            format!("in {}/s, out {}/s", fmt_bytes(s.rx), fmt_bytes(s.tx))
        }),
        scale: format!("0–{}/s", fmt_bytes(top)),
        lines: vec![
            Line {
                points: polyline(&series(|s| s.rx), start, now, top),
                dashed: false,
            },
            Line {
                points: polyline(&series(|s| s.tx), start, now, top),
                dashed: true,
            },
        ],
        warn: false,
    });
    for d in &m.disks {
        let pct = if d.total == 0 {
            0.0
        } else {
            d.used as f64 / d.total as f64 * 100.0
        };
        let pts: Vec<(u64, f64)> = d.pct.iter().copied().collect();
        charts.push(Chart {
            title: format!("Disk {}", d.label),
            now: format!(
                "{pct:.0}% ({} of {})",
                fmt_bytes(d.used as f64),
                fmt_bytes(d.total as f64)
            ),
            scale: "0–100%".into(),
            lines: vec![Line {
                points: polyline(&pts, start, now, 100.0),
                dashed: false,
            }],
            warn: pct >= DISK_WARN_PCT,
        });
    }
    MetricsTemplate {
        range: key,
        ranges: RANGES.iter().map(|r| (r.0, r.1)).collect(),
        ready: last.is_some(),
        fresh,
        charts,
        w: W,
        h: H,
    }
}

/// One tile of the Overview page: a headline number linking to its detail page.
pub struct Tile {
    pub title: &'static str,
    pub value: String,
    pub detail: String,
    pub warn: bool,
}

/// CPU, memory, network and the fullest disk, from the latest sample.
pub fn tiles(m: &Metrics) -> Vec<Tile> {
    let Some(s) = m.samples.back() else {
        return Vec::new();
    };
    let mut out = vec![
        Tile {
            title: "CPU",
            value: format!("{:.0}%", s.cpu),
            detail: "now".into(),
            warn: false,
        },
        Tile {
            title: "Memory",
            value: format!("{:.0}%", s.mem),
            detail: format!(
                "{} of {}",
                fmt_bytes(m.mem_used as f64),
                fmt_bytes(m.mem_total as f64)
            ),
            warn: false,
        },
        Tile {
            title: "Network",
            value: format!("{}/s", fmt_bytes(s.rx)),
            detail: format!("in, out {}/s", fmt_bytes(s.tx)),
            warn: false,
        },
    ];
    let worst = m
        .disks
        .iter()
        .filter(|d| d.total > 0)
        .map(|d| (d, d.used as f64 / d.total as f64 * 100.0))
        .max_by(|a, b| a.1.total_cmp(&b.1));
    if let Some((d, pct)) = worst {
        out.push(Tile {
            title: "Disk",
            value: format!("{pct:.0}%"),
            detail: format!(
                "{} · {} free",
                d.label,
                fmt_bytes((d.total - d.used) as f64)
            ),
            warn: pct >= DISK_WARN_PCT,
        });
    }
    out
}

#[derive(Template)]
#[template(path = "overview.html")]
pub struct OverviewTemplate {
    pub tiles: Vec<Tile>,
    pub running: usize,
    pub total: usize,
    pub warnings: Vec<String>,
    pub docker_error: Option<String>,
}

pub fn overview(
    m: &Metrics,
    containers: Result<&[(String, String, String)], String>,
) -> OverviewTemplate {
    let tiles = tiles(m);
    let mut warnings: Vec<String> = tiles
        .iter()
        .filter(|t| t.warn)
        .map(|t| format!("{} is almost full ({}, {})", t.title, t.value, t.detail))
        .collect();
    let (mut running, mut total, mut docker_error) = (0, 0, None);
    match containers {
        Ok(list) => {
            total = list.len();
            for (name, state, status) in list {
                if state == "running" {
                    running += 1;
                }
                if status.contains("unhealthy") {
                    warnings.push(format!("{name} is unhealthy"));
                }
            }
        }
        Err(e) => docker_error = Some(format!("Docker is not reachable: {e}")),
    }
    OverviewTemplate {
        tiles,
        running,
        total,
        warnings,
        docker_error,
    }
}

#[derive(Template)]
#[template(path = "container_graphs.html")]
struct ContainerGraphsTemplate {
    name: String,
    range: &'static str,
    ranges: Vec<(&'static str, &'static str)>,
    charts: Vec<Chart>,
    w: f64,
    h: f64,
}

fn build_container(m: &Metrics, name: &str, now: u64, range: &str) -> ContainerGraphsTemplate {
    let (key, _, window) = *RANGES.iter().find(|r| r.0 == range).unwrap_or(&RANGES[0]);
    let start = now.saturating_sub(window);
    let hist = m
        .per
        .get(name)
        .map(|p| if window > 86400 { &p.long } else { &p.short });
    let pts: Vec<&(u64, f64, f64)> = hist
        .into_iter()
        .flatten()
        .filter(|p| p.0 >= start)
        .collect();
    let cpu: Vec<(u64, f64)> = pts.iter().map(|p| (p.0, p.1)).collect();
    let mem: Vec<(u64, f64)> = pts.iter().map(|p| (p.0, p.2)).collect();
    let cpu_top = nice_pct(cpu.iter().map(|p| p.1).fold(0.0, f64::max));
    let mem_top = nice_max(mem.iter().map(|p| p.1).fold(0.0, f64::max));
    let usage = m.containers.iter().find(|c| c.name == name);
    ContainerGraphsTemplate {
        name: name.to_string(),
        range: key,
        ranges: RANGES.iter().map(|r| (r.0, r.1)).collect(),
        charts: vec![
            Chart {
                title: "CPU".into(),
                now: usage.map_or("–".into(), |c| format!("{:.1}%", c.cpu_pct)),
                scale: format!("0–{cpu_top:.0}%"),
                lines: vec![Line {
                    points: polyline(&cpu, start, now, cpu_top),
                    dashed: false,
                }],
                warn: false,
            },
            Chart {
                title: "Memory".into(),
                now: usage.map_or("–".into(), |c| fmt_bytes(c.mem_bytes as f64)),
                scale: format!("0–{}", fmt_bytes(mem_top)),
                lines: vec![Line {
                    points: polyline(&mem, start, now, mem_top),
                    dashed: false,
                }],
                warn: false,
            },
        ],
        w: W,
        h: H,
    }
}

pub async fn container_graphs(
    State(shared): State<Shared>,
    axum::extract::Path(name): axum::extract::Path<String>,
    Query(q): Query<RangeQuery>,
) -> Html<String> {
    let page = build_container(
        &shared.lock().unwrap(),
        &name,
        now_unix(),
        q.range.as_deref().unwrap_or("1h"),
    );
    Html(page.render().expect("container graphs template renders"))
}

pub async fn page(State(shared): State<Shared>, Query(q): Query<RangeQuery>) -> Html<String> {
    let page = build(
        &shared.lock().unwrap(),
        now_unix(),
        q.range.as_deref().unwrap_or("1h"),
        q.live.is_none(),
    );
    Html(page.render().expect("metrics template renders"))
}

#[cfg(test)]
mod page_tests {
    use super::*;

    #[test]
    fn overview_shows_tiles_and_warnings() {
        let mut m = Metrics::default();
        assert!(overview(&m, Ok(&[])).tiles.is_empty());
        m.samples.push_back(Sample {
            t: 1,
            cpu: 12.0,
            mem: 40.0,
            rx: 10.0,
            tx: 5.0,
        });
        m.disks.push(DiskSeries {
            label: "/".into(),
            fs: "x".into(),
            total: 100 << 30,
            used: 90 << 30,
            pct: VecDeque::new(),
        });
        let list = [
            (
                "a".to_string(),
                "running".to_string(),
                "Up 1 hour (unhealthy)".to_string(),
            ),
            ("b".to_string(), "exited".to_string(), "Exited".to_string()),
        ];
        let o = overview(&m, Ok(&list));
        assert_eq!((o.tiles.len(), o.running, o.total), (4, 1, 2));
        assert_eq!(o.warnings.len(), 2);
        let html = o.render().unwrap();
        assert!(html.contains("12%") && html.contains("a is unhealthy"));
        assert!(
            overview(&m, Err("boom".into()))
                .render()
                .unwrap()
                .contains("boom")
        );
    }

    fn sample(t: u64, cpu: f64) -> Sample {
        Sample {
            t,
            cpu,
            mem: 50.0,
            rx: 2000.0,
            tx: 500.0,
        }
    }

    #[test]
    fn empty_history_says_collecting() {
        let html = build(&Metrics::default(), 1000, "1h", false)
            .render()
            .unwrap();
        assert!(html.contains("Collecting the first samples"));
        assert!(!html.contains("<svg"));
    }

    #[test]
    fn renders_lines_and_warns_on_full_disk() {
        let mut m = Metrics {
            mem_total: 8 << 30,
            mem_used: 4 << 30,
            ..Default::default()
        };
        m.samples.extend([sample(900, 10.0), sample(1000, 30.0)]);
        m.disks.push(DiskSeries {
            label: "/data".into(),
            fs: "/dev/x".into(),
            total: 100 << 30,
            used: 90 << 30,
            pct: VecDeque::from([(900, 90.0), (1000, 90.0)]),
        });
        m.containers.push(ContainerUsage {
            name: "web<1>".into(),
            cpu_pct: 1.5,
            mem_bytes: 50 << 20,
        });
        let html = build(&m, 1000, "1h", false).render().unwrap();
        assert!(html.contains("<polyline points=\"") && html.contains("30%"));
        assert!(html.contains("chart warn") && html.contains("almost full"));
        assert!(html.contains("range=1h") && !html.contains("web&#60;1&#62;"));
        assert!(
            build(&m, 1000, "24h", false)
                .render()
                .unwrap()
                .contains("range=24h")
        );
    }

    #[test]
    fn long_ranges_use_the_persisted_history() {
        let mut m = Metrics::default();
        m.long.extend([sample(1000, 20.0), sample(300_000, 40.0)]);
        let html = build(&m, 300_000, "7d", false).render().unwrap();
        assert!(html.contains("range=7d") && html.contains("40%"));
        assert!(
            build(&m, 300_000, "1h", false)
                .render()
                .unwrap()
                .contains("Collecting")
        );
    }

    #[test]
    fn log_round_trips_and_drops_old_or_bad_lines() {
        let now = 100 * 86400;
        let mut m = Metrics::default();
        m.long.push_back(sample(now - 60, 12.5));
        let mut d = DiskSeries {
            label: "/srv data".into(),
            fs: "/dev/x".into(),
            total: 1000,
            used: 900,
            pct: VecDeque::new(),
        };
        d.pct.push_back((now - 30, 90.0));
        let text = format!(
            "{}{}S\t{}\t1\t1\t1\t1\ngarbage\nS\t{}\tx\t1\t1\t1\n",
            sample_line(&m.long[0]),
            disk_line(&d, now - 30, 90.0),
            now - 40 * 86400, // older than 30 days
            now - 10,         // unparsable value
        );
        let back = load_log(&text, now);
        assert_eq!(back.long.len(), 1);
        assert_eq!(back.long[0].cpu, 12.5);
        assert_eq!(back.disks[0].label, "/srv data");
        assert_eq!((back.disks[0].total, back.disks[0].used), (1000, 900));
        assert_eq!(back.disks[0].pct.len(), 1);
    }

    #[test]
    fn container_history_averages_persists_and_prunes() {
        let u = |name: &str, cpu: f64, mem: u64| ContainerUsage {
            name: name.into(),
            cpu_pct: cpu,
            mem_bytes: mem,
        };
        let mut m = Metrics::default();
        let t0 = 100 * 86400;
        assert!(record_containers(&mut m, &[u("a", 10.0, 100)], t0, false).is_empty());
        let lines = record_containers(&mut m, &[u("a", 30.0, 300)], t0 + 10, true);
        assert_eq!(m.per["a"].short.len(), 2);
        assert_eq!(m.per["a"].long[0], (t0 + 10, 20.0, 200.0));
        let back = load_log(&lines.concat(), t0 + 10);
        assert_eq!(back.per["a"].long[0], (t0 + 10, 20.0, 200.0));
        record_containers(&mut m, &[u("b", 1.0, 1)], t0 + 31 * 86400, false);
        assert!(!m.per.contains_key("a") && m.per.contains_key("b"));
    }

    #[test]
    fn nice_pct_rounds_up() {
        assert_eq!(
            (nice_pct(0.0), nice_pct(7.0), nice_pct(130.0)),
            (5.0, 10.0, 200.0)
        );
    }

    #[test]
    fn container_graphs_draw_history() {
        let mut m = Metrics::default();
        m.samples.push_back(sample(1000, 10.0));
        m.containers.push(ContainerUsage {
            name: "web".into(),
            cpu_pct: 2.0,
            mem_bytes: 10 << 20,
        });
        m.per
            .entry("web".into())
            .or_default()
            .short
            .extend([(900, 1.0, 5e6), (1000, 2.0, 1e7)]);
        let host = build(&m, 1000, "1h", false).render().unwrap();
        assert!(!host.contains("web"));
        let html = build_container(&m, "web", 1000, "1h").render().unwrap();
        assert!(html.contains("/containers/web/graphs?range=1h") && html.contains("10.0 MB"));
        assert_eq!(html.matches("<polyline").count(), 2);
    }

    #[test]
    fn average_is_the_mean_stamped_with_the_newest_time() {
        let a = [sample(10, 10.0), sample(30, 30.0), sample(20, 20.0)];
        let avg = average(a.iter());
        assert_eq!((avg.t, avg.cpu, avg.rx), (30, 20.0, 2000.0));
    }
}
