//! Looks at the registries for a newer upstream tag of each installed app's main image.
//!
//! Information only: the panel never changes a tag by itself. Apps are updated through the
//! template versions that ship with Dockwren releases (tested); this check just tells you
//! that upstream has moved on. Anonymous requests through curl to Docker Hub and GHCR, once
//! per day, only the image name is sent. Off when the admin switches the kind off.

use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::apps::Apps;
use crate::notify::{self, NewEvent, Severity, Shared};

pub const KIND: &str = "app.upstream_newer";
const PREFIX: &str = "app.upstream_newer:";
const EVERY: Duration = Duration::from_secs(24 * 3600);
const POLL: Duration = Duration::from_secs(300);
const HUB_PAGES: usize = 3;
const GHCR_PAGES: usize = 40;
const GHCR_PAGE: usize = 1000;

/// What upstream has beyond the tag an instance runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub image: String,
    pub current: String,
    /// Highest newer tag with the same first number (patch/minor).
    pub minor: Option<String>,
    /// Highest newer tag with a higher first number (may need a migration).
    pub major: Option<String>,
    /// Epoch of the last successful registry lookup (a failed one keeps the old answer).
    pub checked: u64,
}

impl Found {
    /// One line for the instance list.
    pub fn summary(&self) -> String {
        match (&self.minor, &self.major) {
            (Some(a), Some(b)) => format!("Upstream has {a}, and {b} (new major)"),
            (Some(a), None) => format!("Upstream has {a}"),
            (None, Some(b)) => format!("Upstream has {b} (new major)"),
            (None, None) => String::new(),
        }
    }
}

type Newer = (Option<String>, Option<String>);

pub type Cache = Arc<Mutex<BTreeMap<String, Found>>>;

// ---- tags -------------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct Shape {
    v: bool,
    nums: Vec<u64>,
    suffix: String,
}

/// `v3.4.1` -> v + [3,4,1]; `7.1.4-php8.3-apache` -> [7,1,4] + `-php8.3-apache`.
/// Anything not of the form `[v]N(.N)*[-suffix]` (latest, rc builds, dates with letters...) is None.
fn shape(tag: &str) -> Option<Shape> {
    let (v, rest) = match tag.strip_prefix('v') {
        Some(r) => (true, r),
        None => (false, tag),
    };
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    let (num, suffix) = rest.split_at(end);
    if !suffix.is_empty()
        && !(suffix.starts_with('-')
            && suffix.len() > 1
            && suffix
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
    {
        return None;
    }
    let nums = num
        .split('.')
        .map(|p| p.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;
    Some(Shape {
        v,
        nums,
        suffix: suffix.to_string(),
    })
}

/// Highest newer tags with the same shape as `current`: (same first number, higher first number).
/// A pin without at least two numbers (`valkey:9`, `latest`) has nothing to compare.
pub fn newer(current: &str, candidates: &[String]) -> Newer {
    let Some(cur) = shape(current) else {
        return (None, None);
    };
    if cur.nums.len() < 2 {
        return (None, None);
    }
    let mut minor: Option<(Vec<u64>, &String)> = None;
    let mut major: Option<(Vec<u64>, &String)> = None;
    for c in candidates {
        let Some(s) = shape(c) else { continue };
        if s.v != cur.v || s.suffix != cur.suffix || s.nums.len() != cur.nums.len() {
            continue;
        }
        if s.nums <= cur.nums {
            continue;
        }
        let slot = if s.nums[0] == cur.nums[0] {
            &mut minor
        } else {
            &mut major
        };
        if slot.as_ref().is_none_or(|(n, _)| s.nums > *n) {
            *slot = Some((s.nums, c));
        }
    }
    (minor.map(|m| m.1.clone()), major.map(|m| m.1.clone()))
}

// ---- registries -------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum Registry {
    Hub(String),
    Ghcr(String),
}

fn safe_repo(r: &str) -> bool {
    !r.is_empty()
        && r.split('/').all(|p| {
            !p.is_empty()
                && p != ".."
                && p != "."
                && p.chars().all(|c| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
                })
        })
}

fn locate(image: &str) -> Option<Registry> {
    let (host, path) = match image.split_once('/') {
        Some((h, p)) if h.contains('.') || h.contains(':') => (h, p),
        Some(_) => ("docker.io", image),
        None => ("docker.io", image),
    };
    match host {
        "docker.io" | "index.docker.io" | "registry-1.docker.io" => {
            let repo = if path.contains('/') {
                path.to_string()
            } else {
                format!("library/{path}")
            };
            safe_repo(&repo).then_some(Registry::Hub(repo))
        }
        "ghcr.io" => safe_repo(path).then(|| Registry::Ghcr(path.to_string())),
        _ => None,
    }
}

async fn curl(url: &str, bearer: Option<&str>) -> Result<Vec<u8>, String> {
    let mut cmd = tokio::process::Command::new("curl");
    cmd.args([
        "-sS",
        "-f",
        "--max-time",
        "20",
        "-A",
        concat!("dockwren/", env!("CARGO_PKG_VERSION")),
    ]);
    if let Some(t) = bearer {
        cmd.args(["-H", &format!("Authorization: Bearer {t}")]);
    }
    let out = cmd
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("cannot run curl: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(out.stdout)
}

fn hub_page(json: &[u8]) -> Option<(Vec<String>, Option<String>)> {
    let v: serde_json::Value = serde_json::from_slice(json).ok()?;
    let tags = v["results"]
        .as_array()?
        .iter()
        .filter_map(|r| r["name"].as_str().map(str::to_string))
        .collect();
    let next = v["next"]
        .as_str()
        .filter(|u| u.starts_with("https://hub.docker.com/"))
        .map(str::to_string);
    Some((tags, next))
}

fn ghcr_page(json: &[u8]) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_slice(json).ok()?;
    Some(
        v["tags"]
            .as_array()?
            .iter()
            .filter_map(|t| t.as_str().map(str::to_string))
            .collect(),
    )
}

/// Tags that could be newer than `current` (same suffix keeps Docker Hub's answer small).
async fn fetch_tags(image: &str, current: &str) -> Result<Vec<String>, String> {
    let reg = locate(image).ok_or("registry not supported")?;
    let mut all = Vec::new();
    match reg {
        Registry::Hub(repo) => {
            let mut url = format!(
                "https://hub.docker.com/v2/repositories/{repo}/tags?page_size=100&ordering=last_updated"
            );
            if let Some(s) = shape(current).filter(|s| !s.suffix.is_empty()) {
                url.push_str(&format!("&name={}", s.suffix));
            }
            for _ in 0..HUB_PAGES {
                let body = curl(&url, None).await?;
                let (tags, next) = hub_page(&body).ok_or("unexpected answer from Docker Hub")?;
                all.extend(tags);
                match next {
                    Some(n) => url = n,
                    None => break,
                }
            }
        }
        Registry::Ghcr(repo) => {
            let tok = curl(
                &format!("https://ghcr.io/token?scope=repository:{repo}:pull"),
                None,
            )
            .await?;
            let tok: serde_json::Value =
                serde_json::from_slice(&tok).map_err(|_| "unexpected token answer")?;
            let tok = tok["token"].as_str().ok_or("no token from ghcr.io")?;
            // The listing order is internal and `last` only takes a tag from the previous page,
            // so walk it page by page (immich has tens of thousands of build tags).
            let mut last: Option<String> = None;
            for _ in 0..GHCR_PAGES {
                let mut url = format!("https://ghcr.io/v2/{repo}/tags/list?n={GHCR_PAGE}");
                if let Some(l) = &last {
                    // Only tags made of URL-safe characters are used as the cursor.
                    url.push_str(&format!("&last={l}"));
                }
                let body = curl(&url, Some(tok)).await?;
                let tags = ghcr_page(&body).ok_or("unexpected answer from ghcr.io")?;
                let full = tags.len() >= GHCR_PAGE;
                last = tags
                    .last()
                    .filter(|t| {
                        t.chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
                    })
                    .cloned();
                all.extend(tags);
                if !full || last.is_none() {
                    break;
                }
            }
        }
    }
    Ok(all)
}

// ---- running ----------------------------------------------------------------------------

/// Notifications that match `found`: one per instance with a newer upstream tag.
fn sync(shared: &Shared, found: &BTreeMap<String, Found>) {
    let mut keep = Vec::new();
    for (name, f) in found {
        if f.minor.is_none() && f.major.is_none() {
            continue;
        }
        let key = format!(
            "{PREFIX}{name}:{}:{}",
            f.minor.as_deref().unwrap_or("-"),
            f.major.as_deref().unwrap_or("-")
        );
        keep.push(key.clone());
        notify::raise(
            shared,
            NewEvent::new(
                "apps",
                KIND,
                Severity::Info,
                &format!("Newer upstream version for {name}"),
            )
            .body(format!(
                "{name} runs {}:{}. {}. Heads-up only: Dockwren updates apps through its tested releases.",
                f.image,
                f.current,
                f.summary()
            ))
            .link("/apps")
            .key(key),
        );
    }
    let stale: Vec<String> = shared
        .lock()
        .unwrap()
        .open_keys_with_prefix(PREFIX)
        .into_iter()
        .filter(|k| !keep.contains(k))
        .collect();
    for k in stale {
        notify::resolve(shared, &k);
    }
}

async fn run_once(apps: &Apps, shared: &Shared) {
    let targets = apps.upstream_targets();
    let mut tags: HashMap<(String, String), Option<Newer>> = HashMap::new();
    for (_, image, current) in &targets {
        let k = (image.clone(), current.clone());
        if tags.contains_key(&k) {
            continue;
        }
        // A failed lookup (offline, rate limit) keeps the previous answer until tomorrow.
        let r = fetch_tags(image, current)
            .await
            .ok()
            .map(|t| newer(current, &t));
        tags.insert(k, r);
    }
    let cache = apps.upstream_cache();
    let mut map = cache.lock().unwrap().clone();
    map.retain(|n, _| targets.iter().any(|(t, _, _)| t == n));
    for (name, image, current) in targets {
        match &tags[&(image.clone(), current.clone())] {
            Some((minor, major)) if minor.is_some() || major.is_some() => {
                map.insert(
                    name,
                    Found {
                        image,
                        current,
                        minor: minor.clone(),
                        major: major.clone(),
                        checked: crate::schedule::now(),
                    },
                );
            }
            Some(_) => {
                map.remove(&name);
            }
            None => {}
        }
    }
    sync(shared, &map);
    *cache.lock().unwrap() = map;
}

pub fn spawn_checker(apps: Apps, shared: Shared) {
    tokio::spawn(async move {
        let mut last: Option<Instant> = None;
        loop {
            let on = shared.lock().unwrap().is_enabled(KIND);
            if !on {
                if last.take().is_some() || !apps.upstream_cache().lock().unwrap().is_empty() {
                    apps.upstream_cache().lock().unwrap().clear();
                    sync(&shared, &BTreeMap::new());
                }
            } else if last.is_none_or(|t| t.elapsed() >= EVERY) {
                last = Some(Instant::now());
                run_once(&apps, &shared).await;
            }
            tokio::time::sleep(POLL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn shapes_are_strict() {
        assert!(shape("28.3.0").is_some());
        assert!(shape("v3.4.1").is_some());
        assert!(shape("7.1.4-php8.3-apache").is_some());
        assert_eq!(shape("12.4").unwrap().nums, vec![12, 4]);
        assert!(shape("latest").is_none());
        assert!(shape("28.3.0rc1").is_none());
        assert!(shape("28.3.0-").is_none());
        assert!(shape("28..3").is_none());
        assert!(shape("28.3.0-a b").is_none());
        assert!(shape("28.3.0-x/../y").is_none());
        assert!(shape("").is_none());
    }

    #[test]
    fn finds_the_highest_patch_and_major_with_the_same_shape() {
        let c = v(&[
            "28.3.0",
            "28.3.1",
            "28.4.0",
            "28.10.0",
            "29.0.1",
            "29.1.0",
            "28.3.1-rc1",
            "latest",
            "28.3.2-alpine",
            "27.9.9",
            "v30.0.0",
            "1.2",
        ]);
        let (minor, major) = newer("28.3.0", &c);
        assert_eq!(minor.as_deref(), Some("28.10.0"));
        assert_eq!(major.as_deref(), Some("29.1.0"));
    }

    #[test]
    fn suffix_prefix_and_length_must_match() {
        let c = v(&[
            "7.1.5-php8.3-apache",
            "7.1.6-php8.4-apache",
            "7.1.6-apache",
            "7.2-php8.3-apache",
            "8.0.0-php8.3-apache",
        ]);
        let (minor, major) = newer("7.1.4-php8.3-apache", &c);
        assert_eq!(minor.as_deref(), Some("7.1.5-php8.3-apache"));
        assert_eq!(major.as_deref(), Some("8.0.0-php8.3-apache"));
        assert_eq!(
            newer("v3.4.1", &v(&["3.4.2", "v3.4.2"])).0.as_deref(),
            Some("v3.4.2")
        );
        assert_eq!(
            newer("2.1", &v(&["2.2", "2.1.1", "3.0"])),
            (Some("2.2".into()), Some("3.0".into()))
        );
    }

    #[test]
    fn nothing_to_compare_for_major_only_or_floating_pins() {
        assert_eq!(newer("9", &v(&["10", "9.1"])), (None, None));
        assert_eq!(newer("latest", &v(&["1.2.3"])), (None, None));
        assert_eq!(newer("28.3.0", &v(&["28.3.0", "27.0.0"])), (None, None));
    }

    #[test]
    fn images_map_to_registries() {
        assert_eq!(
            locate("nextcloud"),
            Some(Registry::Hub("library/nextcloud".into()))
        );
        assert_eq!(
            locate("gitea/gitea"),
            Some(Registry::Hub("gitea/gitea".into()))
        );
        assert_eq!(
            locate("docker.io/valkey/valkey"),
            Some(Registry::Hub("valkey/valkey".into()))
        );
        assert_eq!(
            locate("ghcr.io/immich-app/immich-server"),
            Some(Registry::Ghcr("immich-app/immich-server".into()))
        );
        assert_eq!(locate("quay.io/x/y"), None);
        assert_eq!(locate("localhost:5000/x"), None);
        assert_eq!(locate("ghcr.io/../etc"), None);
        assert_eq!(locate("Bad/Upper"), None);
    }

    #[test]
    fn registry_answers_are_parsed_defensively() {
        let (t, n) = hub_page(
            br#"{"next":"https://hub.docker.com/v2/x?page=2","results":[{"name":"1.0"},{"x":1}]}"#,
        )
        .unwrap();
        assert_eq!(t, v(&["1.0"]));
        assert!(n.is_some());
        let (_, n) = hub_page(br#"{"next":"https://evil.example/","results":[]}"#).unwrap();
        assert!(n.is_none());
        assert!(hub_page(b"nope").is_none());
        assert_eq!(ghcr_page(br#"{"tags":["a","b"]}"#).unwrap(), v(&["a", "b"]));
        assert!(ghcr_page(br#"{"errors":[]}"#).is_none());
    }

    #[test]
    fn notifications_follow_the_findings() {
        use crate::notify::Store;
        let s: Shared = Arc::new(Mutex::new(Store::memory()));
        let mut m = BTreeMap::new();
        m.insert(
            "g".to_string(),
            Found {
                image: "gitea/gitea".into(),
                current: "28.3.0".into(),
                minor: Some("28.4.0".into()),
                major: None,
                checked: 0,
            },
        );
        sync(&s, &m);
        sync(&s, &m);
        assert_eq!(s.lock().unwrap().open_events().len(), 1);
        m.get_mut("g").unwrap().minor = Some("28.5.0".into());
        sync(&s, &m);
        assert_eq!(s.lock().unwrap().open_events().len(), 1);
        sync(&s, &BTreeMap::new());
        assert!(s.lock().unwrap().open_events().is_empty());
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Real registries; run by hand: cargo test live -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn real_registries() {
        for (img, cur) in [
            ("gitea/gitea", "1.20.0"),
            ("nextcloud", "29.0.1-apache"),
            ("wordpress", "6.5.0-php8.3-apache"),
            ("ghcr.io/immich-app/immich-server", "v1.100.0"),
            ("ghcr.io/gethomepage/homepage", "v0.9.0"),
        ] {
            let t = fetch_tags(img, cur).await;
            match t {
                Ok(t) => println!("{img}:{cur} {} tags -> {:?}", t.len(), newer(cur, &t)),
                Err(e) => println!("{img}: ERR {e}"),
            }
        }
    }

    /// Whole pipeline against the real registry with a fake old instance.
    #[tokio::test]
    #[ignore]
    async fn pipeline_end_to_end() {
        use crate::notify::Store;
        let root = std::env::temp_dir().join(format!("dw-up-{}", std::process::id()));
        let d = root.join("old");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("state.toml"), "template = \"gitea\"\nversion = 1\n").unwrap();
        std::fs::write(
            d.join("compose.yaml"),
            "services:\n  g:\n    image: gitea/gitea:1.20.0\n",
        )
        .unwrap();
        let s: Shared = Arc::new(Mutex::new(Store::memory()));
        let apps = Apps::new(crate::template::catalog(None).unwrap(), root.clone());
        run_once(&apps, &s).await;
        let found = apps.upstream_cache().lock().unwrap().clone();
        println!("{found:?}");
        println!("{:?}", s.lock().unwrap().open_events().len());
        assert_eq!(found["old"].image, "gitea/gitea");
        assert_eq!(s.lock().unwrap().open_events().len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }
}
