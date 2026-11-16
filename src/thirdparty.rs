//! Third-party templates: a git repository pinned to one full commit hash.
//!
//! Trust model: nothing is tracked or updated by itself. The user names a https URL and a
//! 40-character commit; Dockwren fetches exactly that commit (no hooks, no submodules, no
//! checkout: only `manifest.toml`, `compose.yaml` and `icon.svg` are read from the commit),
//! runs the normal template checks, refuses anything that runs commands inside containers
//! (`post_start`, `migrations`) or reuses a bundled id, and shows the files for approval.
//! The approved copy is stored under `<data>/templates/` and re-checked at every start.

use std::path::{Path, PathBuf};
use std::time::Duration;

use askama::Template as _;
use axum::Form;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Html;
use serde::{Deserialize, Serialize};

use crate::apps::Apps;
use crate::template::{self, Template};

const MAX_FILE: u64 = 64 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub id: String,
    pub url: String,
    pub sha: String,
}

impl Source {
    /// `url@sha`, stored on the template and shown in the catalog.
    pub fn label(&self) -> String {
        format!("{}@{}", self.url, &self.sha[..self.sha.len().min(7)])
    }
}

#[derive(Default, Serialize, Deserialize)]
struct SourceFile {
    #[serde(default)]
    source: Vec<Source>,
}

/// The files read from the pinned commit.
#[derive(Debug)]
pub struct Fetched {
    pub manifest: String,
    pub compose: String,
    pub icon: Option<String>,
}

pub fn dir(data: &Path) -> PathBuf {
    data.join("templates")
}

pub fn check_url(url: &str) -> Result<(), String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or("The URL must start with https://")?;
    let host = rest.split('/').next().unwrap_or_default();
    let ok_chars = |s: &str| {
        s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._~/:%+".contains(c))
    };
    if url.len() > 300
        || host.is_empty()
        || host.contains('@')
        || !ok_chars(rest)
        || !host.contains('.')
    {
        return Err(
            "That does not look like a plain https repository URL (no credentials, no spaces)."
                .into(),
        );
    }
    Ok(())
}

pub fn check_sha(sha: &str) -> Result<String, String> {
    let sha = sha.trim().to_ascii_lowercase();
    if sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(sha)
    } else {
        Err(
            "The commit must be the full 40-character hash (a branch or tag is not accepted)."
                .into(),
        )
    }
}

async fn git(tmp: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let local = if cfg!(test) { "always" } else { "never" };
    let out = tokio::process::Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.https.allow=always",
            "-c",
            &format!("protocol.file.allow={local}"),
            "-c",
            "transfer.fsckObjects=true",
        ])
        .args(args)
        .current_dir(tmp)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", tmp)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| format!("cannot run git ({e}); install git on this server"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(format!(
            "git {}: {}",
            args.first().copied().unwrap_or(""),
            err.lines().last().unwrap_or("failed").trim()
        ))
    }
}

/// Fetch exactly commit `sha` of `url` and read the three template files from it.
pub async fn fetch(data: &Path, url: &str, sha: &str) -> Result<Fetched, String> {
    check_url(url)?;
    let sha = check_sha(sha)?;
    let base = dir(data);
    std::fs::create_dir_all(&base).map_err(|e| format!("cannot create {}: {e}", base.display()))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = base.join(format!(".fetch-{}-{nanos}", std::process::id()));
    std::fs::create_dir(&tmp).map_err(|e| e.to_string())?;
    let result = tokio::time::timeout(FETCH_TIMEOUT, fetch_into(&tmp, url, &sha))
        .await
        .unwrap_or_else(|_| Err("fetching took too long".into()));
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

async fn fetch_into(tmp: &Path, url: &str, sha: &str) -> Result<Fetched, String> {
    git(tmp, &["init", "-q"]).await?;
    git(
        tmp,
        &[
            "fetch",
            "-q",
            "--depth",
            "1",
            "--no-tags",
            "--no-recurse-submodules",
            url,
            sha,
        ],
    )
    .await
    .map_err(|e| format!("{e} (the host must allow fetching a commit by hash)"))?;
    let head = String::from_utf8_lossy(&git(tmp, &["rev-parse", "FETCH_HEAD"]).await?)
        .trim()
        .to_string();
    if head != sha {
        return Err("The server returned a different commit than the one requested.".into());
    }
    let read = |name: &'static str, required: bool| async move {
        let spec = format!("{sha}:{name}");
        let size = match git(tmp, &["cat-file", "-s", &spec]).await {
            Ok(s) => String::from_utf8_lossy(&s)
                .trim()
                .parse::<u64>()
                .unwrap_or(u64::MAX),
            Err(_) if !required => return Ok(None),
            Err(_) => return Err(format!("{name} is missing at the top of that commit")),
        };
        if size > MAX_FILE {
            return Err(format!("{name} is larger than {} KB", MAX_FILE / 1024));
        }
        let bytes = git(tmp, &["cat-file", "blob", &spec]).await?;
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| format!("{name} is not valid UTF-8 text"))
    };
    Ok(Fetched {
        manifest: read("manifest.toml", true).await?.unwrap_or_default(),
        compose: read("compose.yaml", true).await?.unwrap_or_default(),
        icon: read("icon.svg", false).await?,
    })
}

/// Parse with the normal checks, then apply the extra third-party rules.
pub fn review(f: &Fetched, source: Option<String>) -> Result<Template, String> {
    let mut t = template::parse(&f.manifest, &f.compose)?;
    let id = t.manifest.app.id.clone();
    let bundled = template::catalog(None)?;
    if bundled.iter().any(|b| b.manifest.app.id == id) {
        return Err(format!(
            "The id {id:?} belongs to a bundled template and cannot be replaced."
        ));
    }
    if !t.manifest.post_start.is_empty() || !t.manifest.migrations.is_empty() {
        return Err("Third-party templates may not run commands inside containers (post_start, migrations).".into());
    }
    strict_compose(&f.compose)?;
    t.icon = f.icon.clone();
    t.source = source;
    Ok(t)
}

/// The normal template check is a line scan written for trusted authors. For third-party
/// files, also refuse the Compose keys and shapes that could reach the host or hide a
/// port/volume from that scan (long volume syntax, inline lists, devices, extra privileges).
fn strict_compose(compose: &str) -> Result<(), String> {
    const KEYS: &[&str] = &[
        "devices:",
        "device_cgroup_rules:",
        "security_opt:",
        "ipc:",
        "uts:",
        "userns_mode:",
        "cgroup:",
        "cgroup_parent:",
        "sysctls:",
        "volumes_from:",
        "extends:",
        "include:",
        "runtime:",
        "source:",
        "type:",
        "driver_opts:",
        "external:",
        "<<:",
        "configs:",
        "secrets:",
    ];
    for (n, raw) in compose.lines().enumerate() {
        let line = raw.trim().trim_start_matches("- ").trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let at = |why: &str| {
            format!(
                "compose.yaml:{}: {why} is not allowed in third-party templates",
                n + 1
            )
        };
        if raw.contains('\t') {
            return Err(at("a tab character"));
        }
        if let Some(k) = KEYS.iter().find(|k| line.starts_with(**k)) {
            return Err(at(k));
        }
        for key in ["ports:", "volumes:"] {
            if line.starts_with(key) && !line[key.len()..].trim().is_empty() {
                return Err(at(&format!("an inline {key} list")));
            }
        }
        for path in [
            "/var/run", "/proc", "/sys/", "/dev/", "/etc/", "/root", "/home/",
        ] {
            // A mount source starts the item; paths inside the container (after the colon) are fine.
            if line
                .split(':')
                .next()
                .is_some_and(|src| src.starts_with(path))
            {
                return Err(at(&format!("a mount from {path}")));
            }
        }
    }
    Ok(())
}

fn read_sources(data: &Path) -> Vec<Source> {
    std::fs::read_to_string(dir(data).join("sources.toml"))
        .ok()
        .and_then(|t| toml::from_str::<SourceFile>(&t).ok())
        .map(|f| f.source)
        .unwrap_or_default()
}

fn write_sources(data: &Path, sources: &[Source]) -> Result<(), String> {
    let text = toml::to_string(&SourceFile {
        source: sources.to_vec(),
    })
    .map_err(|e| e.to_string())?;
    std::fs::write(dir(data).join("sources.toml"), text).map_err(|e| e.to_string())
}

pub fn list(data: &Path) -> Vec<Source> {
    read_sources(data)
}

/// Store an approved template (replacing an older pin of the same id).
pub fn add(data: &Path, url: &str, sha: &str, f: &Fetched) -> Result<Source, String> {
    let sha = check_sha(sha)?;
    let mut src = Source {
        id: String::new(),
        url: url.to_string(),
        sha,
    };
    let t = review(f, Some(src.label()))?;
    src.id = t.manifest.app.id.clone();
    let d = dir(data).join(&src.id);
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    std::fs::write(d.join("manifest.toml"), &f.manifest).map_err(|e| e.to_string())?;
    std::fs::write(d.join("compose.yaml"), &f.compose).map_err(|e| e.to_string())?;
    match &f.icon {
        Some(i) => std::fs::write(d.join("icon.svg"), i).map_err(|e| e.to_string())?,
        None => {
            let _ = std::fs::remove_file(d.join("icon.svg"));
        }
    }
    let mut all = read_sources(data);
    all.retain(|s| s.id != src.id);
    all.push(src.clone());
    all.sort_by(|a, b| a.id.cmp(&b.id));
    write_sources(data, &all)?;
    Ok(src)
}

/// Forget a source and delete its stored copy. Installed apps keep running.
pub fn remove(data: &Path, id: &str) -> Result<(), String> {
    let mut all = read_sources(data);
    let Some(pos) = all.iter().position(|s| s.id == id) else {
        return Err(format!("No third-party template {id:?}."));
    };
    all.remove(pos);
    write_sources(data, &all)?;
    // The id came from our own sources file and was validated as [a-z0-9-]+.
    if id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !id.is_empty()
    {
        let _ = std::fs::remove_dir_all(dir(data).join(id));
    }
    Ok(())
}

/// Stored third-party templates, re-checked now. A bad one is skipped with a warning so it
/// can never stop the panel from starting.
pub fn load_all(data: &Path) -> Vec<Template> {
    let mut out = Vec::new();
    for s in read_sources(data) {
        let d = dir(data).join(&s.id);
        let read = |n: &str| std::fs::read_to_string(d.join(n));
        let f = match (read("manifest.toml"), read("compose.yaml")) {
            (Ok(manifest), Ok(compose)) => Fetched {
                manifest,
                compose,
                icon: read("icon.svg").ok(),
            },
            _ => {
                eprintln!("third-party template {}: stored files are missing", s.id);
                continue;
            }
        };
        match review(&f, Some(s.label())) {
            Ok(t) if t.manifest.app.id == s.id => out.push(t),
            Ok(_) => eprintln!("third-party template {}: id changed, skipped", s.id),
            Err(e) => eprintln!("third-party template {}: {e}, skipped", s.id),
        }
    }
    out
}

// ---- Settings UI -------------------------------------------------------

fn data_dir(apps: &Apps) -> PathBuf {
    apps.root
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| apps.root.clone())
}

#[derive(askama::Template)]
#[template(path = "thirdparty.html")]
struct ListPage {
    sources: Vec<Source>,
    loaded: Vec<String>,
    note: Option<String>,
    error: Option<String>,
}

fn list_html(apps: &Apps, note: Option<String>, error: Option<String>) -> Html<String> {
    let sources = list(&data_dir(apps));
    let loaded = apps
        .templates
        .iter()
        .filter(|t| t.source.is_some())
        .map(|t| t.manifest.app.id.clone())
        .collect();
    Html(
        ListPage {
            sources,
            loaded,
            note,
            error,
        }
        .render()
        .expect("template renders"),
    )
}

pub async fn show(State(apps): State<Apps>) -> Html<String> {
    list_html(&apps, None, None)
}

#[derive(Deserialize)]
pub struct AddForm {
    url: String,
    sha: String,
}

#[derive(askama::Template)]
#[template(path = "thirdparty_review.html")]
struct ReviewPage {
    url: String,
    sha: String,
    id: String,
    name: String,
    version: u32,
    description: String,
    lan_ports: Vec<String>,
    manifest: String,
    compose: String,
    replaces: Option<String>,
}

/// `POST /settings/templates/review`: fetch the commit and show everything for approval.
pub async fn review_submit(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Form(f): Form<AddForm>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let data = data_dir(&apps);
    let (url, sha) = (f.url.trim().to_string(), f.sha.trim().to_ascii_lowercase());
    let result = async {
        let fetched = fetch(&data, &url, &sha).await?;
        let t = review(&fetched, None)?;
        Ok::<_, String>((fetched, t))
    }
    .await;
    match result {
        Err(e) => Ok(list_html(&apps, None, Some(e))),
        Ok((fetched, t)) => {
            let replaces = list(&data)
                .into_iter()
                .find(|s| s.id == t.manifest.app.id)
                .map(|s| s.label());
            Ok(Html(
                ReviewPage {
                    url,
                    sha,
                    id: t.manifest.app.id.clone(),
                    name: t.manifest.app.name.clone(),
                    version: t.manifest.app.version,
                    description: t.manifest.app.description.clone(),
                    lan_ports: t
                        .manifest
                        .lan_ports
                        .iter()
                        .map(|l| format!("{}/{} ({})", l.port, l.protocol, l.label))
                        .collect(),
                    manifest: fetched.manifest,
                    compose: fetched.compose,
                    replaces,
                }
                .render()
                .expect("template renders"),
            ))
        }
    }
}

/// `POST /settings/templates/add`: the approval click. Fetches the same commit again and
/// stores it (the hash pins the content, so it is the same files that were reviewed).
pub async fn add_submit(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Form(f): Form<AddForm>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let data = data_dir(&apps);
    let (url, sha) = (f.url.trim().to_string(), f.sha.trim().to_ascii_lowercase());
    let result = async {
        let fetched = fetch(&data, &url, &sha).await?;
        add(&data, &url, &sha, &fetched)
    }
    .await;
    Ok(match result {
        Ok(s) => {
            crate::audit::record("template.add", &s.label());
            list_html(
                &apps,
                Some(format!(
                    "Added {}. Restart Dockwren (sudo systemctl restart dockwren) to load it into the catalog.",
                    s.id
                )),
                None,
            )
        }
        Err(e) => list_html(&apps, None, Some(e)),
    })
}

#[derive(Deserialize)]
pub struct RemoveForm {
    id: String,
}

pub async fn remove_submit(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Form(f): Form<RemoveForm>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(match remove(&data_dir(&apps), &f.id) {
        Ok(()) => {
            crate::audit::record("template.remove", &f.id);
            list_html(
                &apps,
                Some("Removed. Installed apps keep running; restart Dockwren to drop it from the catalog.".into()),
                None,
            )
        }
        Err(e) => list_html(&apps, None, Some(e)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = include_str!("../apps/ghost/manifest.toml");
    const COMPOSE: &str = include_str!("../apps/ghost/compose.yaml");

    #[test]
    fn urls_and_hashes_are_strict() {
        assert!(check_url("https://github.com/someone/templates").is_ok());
        for bad in [
            "http://github.com/a/b",
            "git@github.com:a/b",
            "https://user:pw@github.com/a/b",
            "https://github.com/a b",
            "https://localhost/a",
            "file:///etc",
            "--upload-pack=x",
            "https://",
        ] {
            assert!(check_url(bad).is_err(), "{bad}");
        }
        assert!(check_sha(&"A".repeat(40)).is_ok());
        assert!(check_sha("main").is_err());
        assert!(check_sha(&"a".repeat(39)).is_err());
        assert!(check_sha(&"g".repeat(40)).is_err());
    }

    fn fetched(manifest: &str) -> Fetched {
        Fetched {
            manifest: manifest.to_string(),
            compose: COMPOSE.to_string(),
            icon: None,
        }
    }

    #[test]
    fn a_bundled_id_is_refused() {
        let e = review(&fetched(MANIFEST), None).unwrap_err();
        assert!(e.contains("bundled"), "{e}");
    }

    #[test]
    fn strict_compose_refuses_host_reaching_shapes() {
        let ok = "services:\n  web:\n    image: nginx:1.27\n    ports:\n      - \"127.0.0.1:8080:80\"\n    volumes:\n      - data:/var/lib/x\n";
        assert!(strict_compose(ok).is_ok());
        for bad in [
            "    devices:\n      - /dev/sda:/dev/sda\n",
            "    security_opt:\n      - seccomp=unconfined\n",
            "    ipc: host\n",
            "    volumes: [\"/:/host\"]\n",
            "    ports: [\"80:80\"]\n",
            "    volumes:\n      - type: bind\n        source: /\n",
            "    volumes:\n      - /dev/mem:/m\n",
            "    extends:\n      file: x.yml\n",
        ] {
            assert!(strict_compose(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn commands_in_containers_are_refused() {
        let m = MANIFEST.replace("id = \"ghost\"", "id = \"ghost-x\"");
        assert!(review(&fetched(&m), None).is_ok());
        let with_hook =
            format!("{m}\n[[post_start]]\nname=\"p\"\nservice=\"ghost\"\nrun=[\"true\"]\n");
        let e = review(&fetched(&with_hook), None).unwrap_err();
        assert!(e.contains("inside containers"), "{e}");
    }

    #[tokio::test]
    async fn fetch_reads_only_the_pinned_commit() {
        let base = std::env::temp_dir().join(format!("dw-tpg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| {
            let o = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(o.status.success(), "{args:?}");
            String::from_utf8(o.stdout).unwrap().trim().to_string()
        };
        run(&["init", "-q"]);
        std::fs::write(
            repo.join("manifest.toml"),
            MANIFEST.replace("id = \"ghost\"", "id = \"ghost-x\""),
        )
        .unwrap();
        std::fs::write(repo.join("compose.yaml"), COMPOSE).unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "one"]);
        let sha = run(&["rev-parse", "HEAD"]);
        std::fs::write(repo.join("compose.yaml"), "changed later").unwrap();
        run(&["commit", "-qam", "two"]);
        let tmp = base.join("work");
        std::fs::create_dir_all(&tmp).unwrap();
        let url = format!("file://{}", repo.display());
        let f = fetch_into(&tmp, &url, &sha).await.unwrap();
        assert_eq!(f.compose, COMPOSE);
        assert!(f.icon.is_none());
        assert!(review(&f, None).is_ok());
        // A hash the repository does not have is an error, not a fallback to HEAD.
        let tmp2 = base.join("work2");
        std::fs::create_dir_all(&tmp2).unwrap();
        assert!(fetch_into(&tmp2, &url, &"1".repeat(40)).await.is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn add_list_load_remove_round_trip() {
        let data = std::env::temp_dir().join(format!("dw-tp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let m = MANIFEST.replace("id = \"ghost\"", "id = \"ghost-x\"");
        let f = fetched(&m);
        let sha = "a".repeat(40);
        let s = add(&data, "https://example.com/r", &sha, &f).unwrap();
        assert_eq!(s.id, "ghost-x");
        assert_eq!(list(&data), vec![s.clone()]);
        let t = load_all(&data);
        assert_eq!(t.len(), 1);
        assert_eq!(
            t[0].source.as_deref(),
            Some("https://example.com/r@aaaaaaa")
        );
        // Same id again replaces, not duplicates.
        add(&data, "https://example.com/r", &"b".repeat(40), &f).unwrap();
        assert_eq!(list(&data).len(), 1);
        remove(&data, "ghost-x").unwrap();
        assert!(list(&data).is_empty() && load_all(&data).is_empty());
        assert!(remove(&data, "ghost-x").is_err());
        let _ = std::fs::remove_dir_all(&data);
    }
}
