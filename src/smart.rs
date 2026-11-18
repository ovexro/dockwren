//! Disk health (SMART). The panel cannot open disk devices (it runs unprivileged), so a small
//! root-run systemd timer, installed once by `dockwren setup-smart`, runs `dockwren
//! smart-collect` every hour. That asks `smartctl` (smartmontools) about every disk and writes
//! a short summary to `<data dir>/smart.json`. The panel only reads that file: it shows it in
//! Health and raises a notification when a disk reports trouble. No file, no SMART section.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::notify::{self, NewEvent, Severity, Shared};

pub const KIND: &str = "host.smart";
const FILE: &str = "smart.json";
const POLL: Duration = Duration::from_secs(900);
/// The timer runs hourly; three missed runs mean the timer or smartctl broke.
const STALE_SECS: u64 = 3 * 3600;
const UNIT_DIR: &str = "/etc/systemd/system";
const SERVICE_UNIT: &str = "dockwren-smart.service";
const TIMER_UNIT: &str = "dockwren-smart.timer";
const SMARTCTL: [&str; 4] = [
    "/usr/sbin/smartctl",
    "/usr/bin/smartctl",
    "/usr/local/sbin/smartctl",
    "/usr/local/bin/smartctl",
];
/// Temperature (°C) at which a disk is reported as running hot.
const HOT_C: i64 = 65;
const WEAR_WARN_PCT: i64 = 90;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disk {
    pub name: String,
    pub model: String,
    /// `None` when the drive does not answer the overall health question.
    pub passed: Option<bool>,
    pub temp_c: Option<i64>,
    pub hours: Option<i64>,
    pub realloc: Option<i64>,
    pub pending: Option<i64>,
    pub uncorrectable: Option<i64>,
    /// NVMe: percentage of the rated endurance used.
    pub wear_pct: Option<i64>,
    pub spare_pct: Option<i64>,
    pub spare_threshold: Option<i64>,
    pub media_errors: Option<i64>,
    pub critical_warning: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Report {
    pub updated: u64,
    pub disks: Vec<Disk>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Grade {
    Ok,
    Warn,
    Bad,
}

impl Disk {
    /// What is wrong with this disk, worst first. Empty means healthy.
    pub fn problems(&self) -> Vec<(Grade, String)> {
        let mut v = Vec::new();
        if self.passed == Some(false) {
            v.push((
                Grade::Bad,
                "the drive's own health test reports FAILED".into(),
            ));
        }
        if self.critical_warning.unwrap_or(0) != 0 {
            v.push((
                Grade::Bad,
                format!(
                    "critical warning flags set ({:#x})",
                    self.critical_warning.unwrap_or(0)
                ),
            ));
        }
        if let (Some(s), Some(t)) = (self.spare_pct, self.spare_threshold)
            && s <= t
        {
            v.push((
                Grade::Bad,
                format!("spare capacity {s}% is at its threshold {t}%"),
            ));
        }
        if let Some(n) = self.pending.filter(|n| *n > 0) {
            v.push((
                Grade::Warn,
                format!("{n} sectors waiting to be reallocated"),
            ));
        }
        if let Some(n) = self.uncorrectable.filter(|n| *n > 0) {
            v.push((Grade::Warn, format!("{n} uncorrectable sectors")));
        }
        if let Some(n) = self.media_errors.filter(|n| *n > 0) {
            v.push((Grade::Warn, format!("{n} media errors")));
        }
        if let Some(w) = self.wear_pct.filter(|w| *w >= WEAR_WARN_PCT) {
            v.push((
                Grade::Warn,
                format!("{w}% of its rated write endurance used"),
            ));
        }
        if let Some(t) = self.temp_c.filter(|t| *t >= HOT_C) {
            v.push((Grade::Warn, format!("running hot at {t} °C")));
        }
        v.sort_by_key(|p| std::cmp::Reverse(p.0));
        v
    }

    pub fn grade(&self) -> Grade {
        self.problems().first().map_or(Grade::Ok, |p| p.0)
    }

    /// One line for the Health page.
    pub fn summary(&self) -> String {
        let mut parts = vec![if self.model.is_empty() {
            self.name.clone()
        } else {
            format!("{} ({})", self.model, self.name)
        }];
        parts.push(match self.passed {
            Some(true) => "PASSED".into(),
            Some(false) => "FAILED".into(),
            None => "no verdict".into(),
        });
        if let Some(t) = self.temp_c {
            parts.push(format!("{t} °C"));
        }
        if let Some(w) = self.wear_pct {
            parts.push(format!("{w}% worn"));
        }
        if let Some(h) = self.hours {
            parts.push(format!("{} days on", h / 24));
        }
        if let Some(n) = self.realloc.filter(|n| *n > 0) {
            parts.push(format!("{n} reallocated"));
        }
        let probs = self.problems();
        if !probs.is_empty() {
            let t: Vec<String> = probs.into_iter().map(|p| p.1).collect();
            parts.push(format!("⚠ {}", t.join("; ")));
        }
        parts.join(", ")
    }
}

// ---------- parsing smartctl's JSON ----------

fn int(v: &Value, path: &[&str]) -> Option<i64> {
    let mut cur = v;
    for p in path {
        cur = cur.get(*p)?;
    }
    cur.as_i64()
}

/// Reduce one `smartctl -j -a` document to what the panel shows.
pub fn parse_smartctl(name: &str, v: &Value) -> Option<Disk> {
    // A device that did not answer has no model and no health data.
    let model = v.get("model_name").and_then(Value::as_str).unwrap_or("");
    let has_data = v.get("smart_status").is_some()
        || v.get("nvme_smart_health_information_log").is_some()
        || v.get("ata_smart_attributes").is_some();
    if !has_data {
        return None;
    }
    let mut d = Disk {
        name: name.to_string(),
        model: model.chars().take(60).collect(),
        passed: v
            .get("smart_status")
            .and_then(|s| s.get("passed"))
            .and_then(Value::as_bool),
        temp_c: int(v, &["temperature", "current"]),
        hours: int(v, &["power_on_time", "hours"]),
        ..Disk::default()
    };
    if let Some(table) = v
        .get("ata_smart_attributes")
        .and_then(|a| a.get("table"))
        .and_then(Value::as_array)
    {
        for a in table {
            let raw = int(a, &["raw", "value"]);
            match a.get("id").and_then(Value::as_i64) {
                Some(5) => d.realloc = raw,
                Some(197) => d.pending = raw,
                Some(198) => d.uncorrectable = raw,
                _ => {}
            }
        }
    }
    if let Some(n) = v.get("nvme_smart_health_information_log") {
        d.wear_pct = int(n, &["percentage_used"]);
        d.spare_pct = int(n, &["available_spare"]);
        d.spare_threshold = int(n, &["available_spare_threshold"]);
        d.media_errors = int(n, &["media_errors"]);
        d.critical_warning = int(n, &["critical_warning"]);
        if d.temp_c.is_none() {
            d.temp_c = int(n, &["temperature"]);
        }
    }
    Some(d)
}

// ---------- the root-run collector ----------

fn smartctl_path() -> Option<&'static str> {
    SMARTCTL.iter().copied().find(|p| Path::new(p).exists())
}

fn run_json(bin: &str, args: &[&str]) -> Option<Value> {
    // smartctl's exit status is a bit mask (bit 3+ = "disk has problems"); the JSON is what counts.
    let out = std::process::Command::new(bin).args(args).output().ok()?;
    serde_json::from_slice(&out.stdout).ok()
}

/// `dockwren smart-collect`: ask smartctl about every disk and write `<data dir>/smart.json`.
pub fn collect(data_dir: &Path) -> Result<usize, String> {
    let bin = smartctl_path()
        .ok_or("smartctl not found; install smartmontools (apt install smartmontools)")?;
    let scan = run_json(bin, &["--scan", "-j"]).ok_or("smartctl --scan gave no JSON")?;
    let mut disks = Vec::new();
    for dev in scan
        .get("devices")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let Some(name) = dev.get("name").and_then(Value::as_str) else {
            continue;
        };
        // Autodetection first: the scan labels SATA disks "scsi", which hides the ATA data.
        // Fall back to the scan's own type for devices that need it (RAID controllers).
        let mut found = run_json(bin, &["-j", "-a", name]).and_then(|v| parse_smartctl(name, &v));
        if found.is_none()
            && let Some(t) = dev.get("type").and_then(Value::as_str)
        {
            found =
                run_json(bin, &["-j", "-a", "-d", t, name]).and_then(|v| parse_smartctl(name, &v));
        }
        disks.extend(found);
    }
    let report = Report {
        updated: now(),
        disks,
    };
    let n = report.disks.len();
    write_report(data_dir, &report)?;
    Ok(n)
}

fn write_report(data_dir: &Path, r: &Report) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let body = serde_json::to_vec_pretty(r).map_err(|e| e.to_string())?;
    let tmp = data_dir.join(format!(".{FILE}.tmp"));
    std::fs::write(&tmp, body).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    // Root writes it; the panel user (owner of the data folder) must be able to read it.
    if let Ok(m) = std::fs::metadata(data_dir) {
        let _ = std::os::unix::fs::chown(&tmp, Some(m.uid()), Some(m.gid()));
    }
    let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o640));
    std::fs::rename(&tmp, data_dir.join(FILE)).map_err(|e| format!("cannot replace {FILE}: {e}"))
}

// ---------- setup (root, one time) ----------

fn service_unit(data_dir: &Path, exe: &Path) -> String {
    format!(
        "[Unit]\nDescription=Dockwren disk health (SMART) collector\n\n[Service]\nType=oneshot\nExecStart={} smart-collect --data-dir {}\nTimeoutStartSec=5min\nNoNewPrivileges=true\nProtectSystem=strict\nProtectHome=true\nPrivateTmp=true\nReadWritePaths={}\n",
        exe.display(),
        data_dir.display(),
        data_dir.display()
    )
}

fn timer_unit() -> String {
    "[Unit]\nDescription=Read disk health (SMART) every hour for Dockwren\n\n[Timer]\nOnBootSec=2min\nOnUnitActiveSec=1h\nRandomizedDelaySec=60\n\n[Install]\nWantedBy=timers.target\n".to_string()
}

fn plain_path_ok(p: &Path) -> bool {
    p.is_absolute()
        && !p
            .to_string_lossy()
            .chars()
            .any(|c| c.is_whitespace() || c == '%' || c == '"')
}

/// `dockwren setup-smart`: write the two units, enable the timer and run the first collection.
pub fn setup(data_dir: &Path) -> Result<usize, String> {
    if !data_dir.is_dir() || !plain_path_ok(data_dir) {
        return Err(format!(
            "{} is not an existing absolute directory without spaces; pass --data-dir /path/to/dockwren-data",
            data_dir.display()
        ));
    }
    if smartctl_path().is_none() {
        return Err("smartctl not found; install it first: apt install smartmontools".into());
    }
    let exe: PathBuf = std::env::current_exe().map_err(|e| e.to_string())?;
    if !plain_path_ok(&exe) {
        return Err("the dockwren binary path must not contain spaces, % or quotes".into());
    }
    for (name, body) in [
        (SERVICE_UNIT, service_unit(data_dir, &exe)),
        (TIMER_UNIT, timer_unit()),
    ] {
        let p = Path::new(UNIT_DIR).join(name);
        std::fs::write(&p, body)
            .map_err(|e| format!("cannot write {}: {e} (run as root)", p.display()))?;
    }
    for args in [&["daemon-reload"][..], &["enable", "--now", TIMER_UNIT][..]] {
        let st = std::process::Command::new("systemctl")
            .args(args)
            .status()
            .map_err(|e| format!("cannot run systemctl: {e}"))?;
        if !st.success() {
            return Err(format!("systemctl {} failed", args.join(" ")));
        }
    }
    collect(data_dir)
}

// ---------- the panel side ----------

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The latest report, or `None` when SMART collection was never set up.
pub fn read(data_dir: &Path) -> Option<Report> {
    let text = std::fs::read_to_string(data_dir.join(FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn is_stale(r: &Report, now: u64) -> bool {
    now.saturating_sub(r.updated) > STALE_SECS
}

fn key_for(name: &str) -> String {
    format!("{KIND}:{name}")
}

/// Raise or clear the notification of every disk in a report.
pub fn apply(shared: &Shared, r: &Report) {
    let mut open = HashSet::new();
    for d in &r.disks {
        let probs = d.problems();
        let Some(worst) = probs.first().map(|p| p.0) else {
            continue;
        };
        let key = key_for(&d.name);
        open.insert(key.clone());
        let list: Vec<String> = probs.iter().map(|p| p.1.clone()).collect();
        let model = if d.model.is_empty() {
            &d.name
        } else {
            &d.model
        };
        notify::raise(
            shared,
            NewEvent::new(
                "host",
                KIND,
                if worst == Grade::Bad {
                    Severity::Critical
                } else {
                    Severity::Warning
                },
                &format!("Disk {} ({model}) reports a problem", d.name),
            )
            .body(format!(
                "{}. Back up what matters and plan to replace the disk if this keeps up.",
                list.join("; ")
            ))
            .link("/settings/doctor")
            .key(key),
        );
    }
    let stale: Vec<String> = shared
        .lock()
        .unwrap()
        .open_keys_with_prefix(&format!("{KIND}:"))
        .into_iter()
        .filter(|k| !open.contains(k))
        .collect();
    for k in stale {
        notify::resolve(shared, &k);
    }
}

pub fn spawn_checker(shared: Shared, data_dir: PathBuf) {
    tokio::spawn(async move {
        loop {
            if shared.lock().unwrap().is_enabled(KIND)
                && let Some(r) = read(&data_dir)
            {
                apply(&shared, &r);
            }
            tokio::time::sleep(POLL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn sata(pending: i64, passed: bool) -> Value {
        json!({
            "model_name": "WDC WD10JPCX",
            "smart_status": {"passed": passed},
            "temperature": {"current": 34},
            "power_on_time": {"hours": 4800},
            "ata_smart_attributes": {"table": [
                {"id": 5, "name": "Reallocated_Sector_Ct", "raw": {"value": 2}},
                {"id": 197, "name": "Current_Pending_Sector", "raw": {"value": pending}},
                {"id": 198, "name": "Offline_Uncorrectable", "raw": {"value": 0}},
                {"id": 9, "name": "Power_On_Hours", "raw": {"value": 4800}}
            ]}
        })
    }

    fn nvme(wear: i64, crit: i64) -> Value {
        json!({
            "model_name": "SAMSUNG MZVL4512",
            "smart_status": {"passed": true},
            "temperature": {"current": 41},
            "power_on_time": {"hours": 900},
            "nvme_smart_health_information_log": {
                "critical_warning": crit, "percentage_used": wear,
                "available_spare": 100, "available_spare_threshold": 10, "media_errors": 0
            }
        })
    }

    #[test]
    fn parses_sata_and_nvme() {
        let d = parse_smartctl("/dev/sda", &sata(0, true)).unwrap();
        assert_eq!(d.passed, Some(true));
        assert_eq!(
            (d.realloc, d.pending, d.uncorrectable),
            (Some(2), Some(0), Some(0))
        );
        assert_eq!((d.temp_c, d.hours), (Some(34), Some(4800)));
        // Reallocated sectors alone are shown, not alarmed on.
        assert_eq!(d.grade(), Grade::Ok);
        assert!(d.summary().contains("2 reallocated"));
        let n = parse_smartctl("/dev/nvme0", &nvme(3, 0)).unwrap();
        assert_eq!(
            (n.wear_pct, n.spare_pct, n.spare_threshold),
            (Some(3), Some(100), Some(10))
        );
        assert_eq!(n.grade(), Grade::Ok);
    }

    #[test]
    fn grades_problems() {
        assert_eq!(
            parse_smartctl("a", &sata(8, true)).unwrap().grade(),
            Grade::Warn
        );
        assert_eq!(
            parse_smartctl("a", &sata(0, false)).unwrap().grade(),
            Grade::Bad
        );
        assert_eq!(
            parse_smartctl("n", &nvme(92, 0)).unwrap().grade(),
            Grade::Warn
        );
        assert_eq!(
            parse_smartctl("n", &nvme(3, 4)).unwrap().grade(),
            Grade::Bad
        );
        let hot = Disk {
            temp_c: Some(70),
            ..Disk::default()
        };
        assert_eq!(hot.grade(), Grade::Warn);
    }

    #[test]
    fn device_without_data_is_skipped() {
        assert!(parse_smartctl("/dev/sdb", &json!({"smartctl": {"exit_status": 2}})).is_none());
    }

    #[test]
    fn raises_once_escalates_and_clears() {
        let s: Shared = Arc::new(Mutex::new(notify::Store::memory()));
        let bad = Report {
            updated: 1,
            disks: vec![parse_smartctl("/dev/sda", &sata(8, true)).unwrap()],
        };
        apply(&s, &bad);
        apply(&s, &bad);
        assert_eq!(s.lock().unwrap().open_events().len(), 1);
        let good = Report {
            updated: 2,
            disks: vec![parse_smartctl("/dev/sda", &sata(0, true)).unwrap()],
        };
        apply(&s, &good);
        assert!(s.lock().unwrap().open_events().is_empty());
    }

    #[test]
    fn stale_after_three_hours_and_missing_file_is_none() {
        let r = Report {
            updated: 1000,
            disks: vec![],
        };
        assert!(!is_stale(&r, 1000 + 3600));
        assert!(is_stale(&r, 1000 + 4 * 3600));
        assert!(read(Path::new("/nonexistent-dockwren-dir")).is_none());
    }

    #[test]
    fn units_are_hardened_and_hourly() {
        let u = service_unit(
            Path::new("/var/lib/dockwren"),
            Path::new("/usr/local/bin/dockwren"),
        );
        assert!(u.contains(
            "ExecStart=/usr/local/bin/dockwren smart-collect --data-dir /var/lib/dockwren"
        ));
        assert!(
            u.contains("ProtectSystem=strict") && u.contains("ReadWritePaths=/var/lib/dockwren")
        );
        assert!(timer_unit().contains("OnUnitActiveSec=1h"));
        assert!(!plain_path_ok(Path::new("/a b")) && plain_path_ok(Path::new("/var/lib/dockwren")));
    }
}
