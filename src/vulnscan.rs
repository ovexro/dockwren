//! Image vulnerability scan. Once a day each installed app's images are scanned with
//! [Trivy](https://trivy.dev), run in a throwaway container (pinned version, no access to the
//! Docker socket: it reads the images straight from their registry). The Apps page shows
//! the number of critical and high findings per app; a notification is raised when an app gets
//! a critical finding that already has a fixed version, and again when a new one appears.
//! Findings without a fix are counted but stay quiet (nothing to do about them yet).
//! Nothing is changed: fixes arrive as image updates through Dockwren releases.
//! A failed scan (offline, private image) keeps the previous answer.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::apps::Apps;
use crate::notify::{self, NewEvent, Severity, Shared};
use crate::runner::{self, Instance};

const KIND: &str = "app.vulnerable";
const TRIVY: &str = "aquasec/trivy:0.76.2";
const EVERY: Duration = Duration::from_secs(24 * 3600);
const POLL: Duration = Duration::from_secs(600);
const FIRST_DELAY: Duration = Duration::from_secs(600);
const PER_IMAGE: Duration = Duration::from_secs(900);
const FILE: &str = "vulnscan.json";
/// Store key of the bundled Caddy image (direct HTTPS). Not a valid instance name, so it cannot
/// collide with an app.
const CADDY: &str = "@caddy";

fn label(name: &str) -> &str {
    if name == CADDY {
        "Caddy (direct HTTPS)"
    } else {
        name
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    /// Epoch of the scan.
    pub scanned: u64,
    pub critical: usize,
    pub high: usize,
    /// Critical findings that have a fixed version.
    pub fixable_critical: usize,
    /// IDs of those, sorted.
    pub ids: Vec<String>,
    /// IDs a notification was already raised for.
    #[serde(default)]
    pub notified: Vec<String>,
    /// Every finding, worst first, for the report page (empty in scans made before it existed).
    #[serde(default)]
    pub details: Vec<Detail>,
}

impl Summary {
    /// "2 critical, 5 high" for the Apps page; None when clean.
    pub fn line(&self) -> Option<String> {
        match (self.critical, self.high) {
            (0, 0) => None,
            (c, 0) => Some(format!("{c} critical")),
            (0, h) => Some(format!("{h} high")),
            (c, h) => Some(format!("{c} critical, {h} high")),
        }
    }
}

type Store = Mutex<HashMap<String, Summary>>;

fn store() -> &'static Store {
    static S: OnceLock<Store> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// Test hook: put a scan result into the store.
#[cfg(test)]
pub fn set_for_test(name: &str, s: Summary) {
    store().lock().unwrap().insert(name.to_string(), s);
}

/// Where the report of one app is shown.
pub fn report_path(name: &str) -> String {
    format!("/apps/{name}/vulns")
}

/// Name shown for a scanned item (the bundled Caddy has a store key that is not an app name).
pub fn display_name(name: &str) -> &str {
    label(name)
}

/// True for names that can have a report: an installed app's name or the bundled Caddy.
pub fn is_reportable(name: &str) -> bool {
    name == CADDY || runner::valid_name(name)
}

/// Last scan result of an app.
pub fn summary(name: &str) -> Option<Summary> {
    store().lock().unwrap().get(name).cloned()
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Finding {
    id: String,
    critical: bool,
    fixable: bool,
    pkg: String,
    installed: String,
    fixed: String,
    title: String,
}

/// One finding as the report page shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detail {
    pub id: String,
    pub critical: bool,
    pub pkg: String,
    pub installed: String,
    /// Empty when no fixed version exists yet.
    pub fixed: String,
    pub title: String,
}

impl Detail {
    pub fn severity(&self) -> &'static str {
        if self.critical { "critical" } else { "high" }
    }

    /// Public description of a CVE (None for other id schemes).
    pub fn url(&self) -> Option<String> {
        let ok = self.id.starts_with("CVE-")
            && self
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-');
        ok.then(|| format!("https://avd.aquasec.com/nvd/{}", self.id.to_lowercase()))
    }
}

/// The report keeps at most this many rows per app (the worst first).
const MAX_DETAILS: usize = 300;

/// Findings of one Trivy JSON report (only HIGH and CRITICAL are asked for).
fn parse(json: &[u8]) -> Option<Vec<Finding>> {
    let v: serde_json::Value = serde_json::from_slice(json).ok()?;
    let mut out = Vec::new();
    for r in v["Results"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
        for f in r["Vulnerabilities"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            let id = f["VulnerabilityID"].as_str()?;
            let critical = match f["Severity"].as_str()? {
                "CRITICAL" => true,
                "HIGH" => false,
                _ => continue,
            };
            let text = |k: &str, max: usize| -> String {
                f[k].as_str().unwrap_or("").chars().take(max).collect()
            };
            let fixed = text("FixedVersion", 120);
            out.push(Finding {
                id: id.chars().take(40).collect(),
                critical,
                fixable: !fixed.is_empty(),
                pkg: text("PkgName", 80),
                installed: text("InstalledVersion", 80),
                fixed,
                title: text("Title", 140),
            });
        }
    }
    Some(out)
}

/// Combine the findings of all images of one app (the same CVE in two images counts once).
fn summarize(findings: &[Finding], now: u64, previous: Option<&Summary>) -> Summary {
    let mut by_id: BTreeMap<&str, (bool, bool)> = BTreeMap::new();
    for f in findings {
        let e = by_id.entry(&f.id).or_default();
        e.0 |= f.critical;
        e.1 |= f.fixable;
    }
    let critical = by_id.values().filter(|(c, _)| *c).count();
    let high = by_id.len() - critical;
    let ids: Vec<String> = by_id
        .iter()
        .filter(|(_, (c, fix))| *c && *fix)
        .map(|(id, _)| id.to_string())
        .collect();
    // The report: one row per CVE and package, critical before high, fixable before not.
    let mut details: Vec<Detail> = Vec::new();
    for f in findings {
        let d = Detail {
            id: f.id.clone(),
            critical: f.critical,
            pkg: f.pkg.clone(),
            installed: f.installed.clone(),
            fixed: f.fixed.clone(),
            title: f.title.clone(),
        };
        if !details
            .iter()
            .any(|x| x.id == d.id && x.pkg == d.pkg && x.installed == d.installed)
        {
            details.push(d);
        }
    }
    details.sort_by(|a, b| {
        (!a.critical, a.fixed.is_empty(), &a.id, &a.pkg).cmp(&(
            !b.critical,
            b.fixed.is_empty(),
            &b.id,
            &b.pkg,
        ))
    });
    details.truncate(MAX_DETAILS);
    Summary {
        scanned: now,
        critical,
        high,
        fixable_critical: ids.len(),
        ids,
        notified: previous.map(|p| p.notified.clone()).unwrap_or_default(),
        details,
    }
}

fn event_key(name: &str) -> String {
    format!("{KIND}:{name}")
}

/// Raise, refresh or clear the notification of one app; updates `notified`.
fn sync(shared: &Shared, name: &str, s: &mut Summary) {
    let key = event_key(name);
    if s.ids.is_empty() {
        notify::resolve(shared, &key);
        s.notified.clear();
        return;
    }
    let known: BTreeSet<&String> = s.notified.iter().collect();
    if s.ids.iter().all(|i| known.contains(i)) {
        return;
    }
    // A finding nobody was told about: close the old event so this one notifies again.
    notify::resolve(shared, &key);
    let shown: Vec<&str> = s.ids.iter().take(8).map(String::as_str).collect();
    let more = s.ids.len().saturating_sub(shown.len());
    notify::raise(
        shared,
        NewEvent::new(
            "apps",
            KIND,
            Severity::Warning,
            &format!(
                "{}: {} critical vulnerabilit{} with a fix available",
                label(name),
                s.ids.len(),
                if s.ids.len() == 1 { "y" } else { "ies" }
            ),
        )
        .body(format!(
            "{}{}. A newer image fixes them; Dockwren delivers image updates with its releases (Apps shows the update when there is one). Findings without a fix are counted on the Apps page but do not notify.",
            shown.join(", "),
            if more > 0 { format!(" and {more} more") } else { String::new() }
        ))
        .link(report_path(name))
        .key(key),
    );
    s.notified = s.ids.clone();
}

fn file(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE)
}

fn load(data_dir: &Path) -> HashMap<String, Summary> {
    std::fs::read_to_string(file(data_dir))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save(data_dir: &Path, m: &HashMap<String, Summary>) {
    if let Ok(t) = serde_json::to_string(m) {
        let _ = std::fs::write(file(data_dir), t);
    }
}

/// Scan one image with Trivy. Err on any failure (offline, private image, timeout).
async fn scan_image(data_dir: &Path, image: &str) -> Result<Vec<Finding>, String> {
    // Image names come from the app's own compose file; refuse anything odd before it
    // reaches a command line.
    if image.starts_with('-')
        || !image
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ':' | '@'))
    {
        return Err(format!("odd image name {image}"));
    }
    let cache = data_dir.join("trivy-cache");
    std::fs::create_dir_all(&cache).map_err(|e| format!("{}: {e}", cache.display()))?;
    let mut cmd = tokio::process::Command::new("docker");
    cmd.env("DOCKER_CONFIG", data_dir.join("docker-config"));
    cmd.args(["run", "--rm", "--pull", "missing", "-v"])
        .arg(format!("{}:/root/.cache", cache.display()))
        .args([
            TRIVY,
            "image",
            "--quiet",
            "--format",
            "json",
            "--scanners",
            "vuln",
            "--severity",
            "HIGH,CRITICAL",
            "--image-src",
            "remote",
            "--",
            image,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let out = tokio::time::timeout(PER_IMAGE, cmd.output())
        .await
        .map_err(|_| "scan timed out".to_string())?
        .map_err(|e| format!("cannot run docker: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.lines().last().unwrap_or("trivy failed").to_string());
    }
    parse(&out.stdout).ok_or_else(|| "unexpected Trivy output".to_string())
}

/// Scan every image of one app; None when no image could be scanned.
pub async fn scan_app(
    data_dir: &Path,
    inst: &Instance,
    log: &mut (dyn FnMut(&str) + Send),
) -> Option<Vec<Finding>> {
    let images = runner::compose(inst, &["config", "--images"]).await.ok()?;
    let images: BTreeSet<&str> = images
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let mut all = Vec::new();
    let mut ok = 0;
    for image in &images {
        match scan_image(data_dir, image).await {
            Ok(f) => {
                ok += 1;
                all.extend(f);
            }
            Err(e) => log(&format!("{image}: not scanned ({e})")),
        }
    }
    (ok > 0).then_some(all)
}

async fn run_once(apps: &Apps, shared: &Shared, data_dir: &Path) {
    if let Some(m) = crate::restoretest::available_mib()
        && m < crate::restoretest::MIN_FREE_MIB
    {
        return;
    }
    let names: Vec<String> = apps.instance_names();
    let caddy_used = !crate::direct::collect(&apps.root).is_empty();
    store()
        .lock()
        .unwrap()
        .retain(|n, _| names.contains(n) || (caddy_used && n == CADDY));
    if !caddy_used {
        notify::resolve(shared, &event_key(CADDY));
    } else if let Ok(f) = scan_image(data_dir, crate::direct::IMAGE).await {
        let mut s = summarize(&f, crate::schedule::now(), summary(CADDY).as_ref());
        sync(shared, CADDY, &mut s);
        store().lock().unwrap().insert(CADDY.to_string(), s);
        save(data_dir, &store().lock().unwrap());
    }
    for name in names {
        let inst = Instance {
            name: name.clone(),
            dir: apps.root.join(&name),
        };
        let Some(findings) = scan_app(data_dir, &inst, &mut |_| {}).await else {
            continue;
        };
        let prev = summary(&name);
        let mut s = summarize(&findings, crate::schedule::now(), prev.as_ref());
        sync(shared, &name, &mut s);
        store().lock().unwrap().insert(name, s);
        save(data_dir, &store().lock().unwrap());
    }
}

pub fn spawn_checker(apps: Apps, shared: Shared, data_dir: PathBuf) {
    *store().lock().unwrap() = load(&data_dir);
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_DELAY).await;
        loop {
            let newest = store()
                .lock()
                .unwrap()
                .values()
                .map(|s| s.scanned)
                .min()
                .unwrap_or(0);
            let due = crate::schedule::now().saturating_sub(newest) >= EVERY.as_secs()
                || apps.instance_names().iter().any(|n| summary(n).is_none());
            if !shared.lock().unwrap().is_enabled(KIND) {
                store().lock().unwrap().clear();
            } else if due {
                run_once(&apps, &shared, &data_dir).await;
            }
            tokio::time::sleep(POLL).await;
        }
    });
}

/// Print-friendly result for the CLI.
pub fn describe(findings: &[Finding]) -> String {
    let s = summarize(findings, 0, None);
    format!(
        "{} critical ({} with a fix), {} high",
        s.critical, s.fixable_critical, s.high
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const REPORT: &str = r#"{"Results":[
      {"Target":"a","Vulnerabilities":[
        {"VulnerabilityID":"CVE-1","Severity":"CRITICAL","FixedVersion":"1.2"},
        {"VulnerabilityID":"CVE-2","Severity":"HIGH"},
        {"VulnerabilityID":"CVE-3","Severity":"CRITICAL"}]},
      {"Target":"b"},
      {"Target":"c","Vulnerabilities":[
        {"VulnerabilityID":"CVE-1","Severity":"CRITICAL","FixedVersion":"1.2"},
        {"VulnerabilityID":"CVE-4","Severity":"MEDIUM"}]}]}"#;

    #[test]
    fn parses_and_counts_each_cve_once() {
        let f = parse(REPORT.as_bytes()).unwrap();
        assert_eq!(f.len(), 4);
        let s = summarize(&f, 5, None);
        assert_eq!((s.critical, s.high, s.fixable_critical), (2, 1, 1));
        assert_eq!(s.ids, vec!["CVE-1"]);
        assert_eq!(s.line().as_deref(), Some("2 critical, 1 high"));
        assert!(parse(b"not json").is_none());
        assert_eq!(parse(b"{}").unwrap().len(), 0);
        assert_eq!(summarize(&[], 1, None).line(), None);
    }

    const FULL: &str = r#"{"Results":[{"Target":"a","Vulnerabilities":[
        {"VulnerabilityID":"CVE-2","Severity":"HIGH","PkgName":"zlib","InstalledVersion":"1.0","FixedVersion":"1.1","Title":"zlib overflow"},
        {"VulnerabilityID":"CVE-9","Severity":"CRITICAL","PkgName":"openssl","InstalledVersion":"3.0","Title":"no fix yet"},
        {"VulnerabilityID":"CVE-1","Severity":"CRITICAL","PkgName":"libc","InstalledVersion":"2.1","FixedVersion":"2.2","Title":"libc bug"},
        {"VulnerabilityID":"CVE-1","Severity":"CRITICAL","PkgName":"libc","InstalledVersion":"2.1","FixedVersion":"2.2","Title":"libc bug"}]}]}"#;

    #[test]
    fn the_report_lists_every_finding_worst_first_without_duplicates() {
        let s = summarize(&parse(FULL.as_bytes()).unwrap(), 7, None);
        let order: Vec<&str> = s.details.iter().map(|d| d.id.as_str()).collect();
        // critical with a fix, critical without, then high.
        assert_eq!(order, vec!["CVE-1", "CVE-9", "CVE-2"]);
        assert_eq!(s.details[0].pkg, "libc");
        assert_eq!(s.details[0].fixed, "2.2");
        assert_eq!(s.details[1].fixed, "");
        assert_eq!(
            s.details[0].url().as_deref(),
            Some("https://avd.aquasec.com/nvd/cve-1")
        );
        assert_eq!(s.details[0].severity(), "critical");
    }

    #[test]
    fn long_reports_are_capped_and_odd_ids_get_no_link() {
        let many: Vec<Finding> = (0..MAX_DETAILS + 50)
            .map(|i| Finding {
                id: format!("CVE-{i}"),
                critical: false,
                fixable: false,
                pkg: "p".into(),
                installed: "1".into(),
                fixed: String::new(),
                title: String::new(),
            })
            .collect();
        assert_eq!(summarize(&many, 1, None).details.len(), MAX_DETAILS);
        let d = Detail {
            id: "GHSA-x\"><b>".into(),
            critical: false,
            pkg: String::new(),
            installed: String::new(),
            fixed: String::new(),
            title: String::new(),
        };
        assert_eq!(d.url(), None);
    }

    #[test]
    fn an_old_store_file_without_details_still_loads() {
        let old = r#"{"scanned":5,"critical":1,"high":0,"fixable_critical":1,"ids":["CVE-1"]}"#;
        let s: Summary = serde_json::from_str(old).unwrap();
        assert!(s.details.is_empty());
        let back: Summary = serde_json::from_str(
            &serde_json::to_string(&summarize(&parse(FULL.as_bytes()).unwrap(), 7, None)).unwrap(),
        )
        .unwrap();
        assert_eq!(back.details.len(), 3);
    }

    fn open(shared: &Shared) -> usize {
        shared.lock().unwrap().open_events().len()
    }

    #[test]
    fn notifies_only_for_new_fixable_criticals() {
        let shared: Shared = Arc::new(Mutex::new(notify::Store::memory()));
        let fx = |id: &str| Finding {
            id: id.into(),
            critical: true,
            fixable: true,
            ..Default::default()
        };
        let mut s = summarize(&[fx("CVE-1")], 1, None);
        sync(&shared, "app", &mut s);
        assert_eq!(open(&shared), 1);
        let first = shared.lock().unwrap().open_events()[0].id;
        // Same finding tomorrow: nothing new.
        let mut s2 = summarize(&[fx("CVE-1")], 2, Some(&s));
        sync(&shared, "app", &mut s2);
        assert_eq!(open(&shared), 1);
        assert_eq!(shared.lock().unwrap().open_events()[0].id, first);
        // A new one: the old event closes, a fresh one is raised.
        let mut s3 = summarize(&[fx("CVE-1"), fx("CVE-9")], 3, Some(&s2));
        sync(&shared, "app", &mut s3);
        assert_eq!(open(&shared), 1);
        assert_ne!(shared.lock().unwrap().open_events()[0].id, first);
        // Fixed: closed.
        let mut s4 = summarize(&[], 4, Some(&s3));
        sync(&shared, "app", &mut s4);
        assert_eq!(open(&shared), 0);
        // An unfixable critical never notifies.
        let mut s5 = summarize(
            &[Finding {
                id: "CVE-7".into(),
                critical: true,
                fixable: false,
                ..Default::default()
            }],
            5,
            None,
        );
        sync(&shared, "app", &mut s5);
        assert_eq!(open(&shared), 0);
        assert_eq!(s5.critical, 1);
    }

    #[test]
    fn the_caddy_pin_is_scannable_and_labelled() {
        let img = crate::direct::IMAGE;
        assert!(
            img.chars()
                .all(|c| c.is_ascii_alphanumeric() || ".-/:".contains(c))
        );
        assert_eq!(label(CADDY), "Caddy (direct HTTPS)");
        assert_eq!(label("memos"), "memos");
        // Cannot clash with an instance name.
        assert!(!CADDY.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
        let shared: Shared = Arc::new(Mutex::new(notify::Store::memory()));
        let mut s = summarize(
            &[Finding {
                id: "CVE-1".into(),
                critical: true,
                fixable: true,
                ..Default::default()
            }],
            1,
            None,
        );
        sync(&shared, CADDY, &mut s);
        assert_eq!(open(&shared), 1);
        assert!(
            shared.lock().unwrap().open_events()[0]
                .title
                .starts_with("Caddy (direct HTTPS):")
        );
    }

    #[tokio::test]
    async fn odd_image_names_are_refused() {
        let d = std::env::temp_dir();
        for bad in ["-v", "a b", "x;y", "a$(b)"] {
            let e = scan_image(&d, bad).await.unwrap_err();
            assert!(e.contains("odd image name"), "{bad}: {e}");
        }
    }
}
