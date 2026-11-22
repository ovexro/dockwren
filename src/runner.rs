//! Runner: create an instance directory from a template and drive it with the
//! `docker compose` CLI (Compose keeps owning interpolation and `depends_on`).

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::process::Command;

use crate::template::{Health, Template};

#[derive(Debug, Clone)]
pub struct Instance {
    pub name: String,
    pub dir: PathBuf,
}

/// Instance names become directory and Compose project names: keep them tame.
pub fn valid_name(n: &str) -> bool {
    let mut c = n.chars();
    c.next().is_some_and(|f| f.is_ascii_lowercase())
        && n.len() >= 2
        && n.len() <= 31
        && n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// File in an adopted instance's directory holding the Compose project name the containers
/// already run under. It is not part of `state.toml` on purpose: clones and restore tests
/// copy that file, and a copy must never drive the original's containers.
pub const PROJECT_FILE: &str = "project";

/// Compose project names Docker accepts and that cannot smuggle anything into a command line.
pub fn valid_project(p: &str) -> bool {
    (1..=63).contains(&p.len())
        && p.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && p.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// The Compose project of an instance: the adopted one, else `dockwren-<name>`.
pub fn project_name(inst: &Instance) -> String {
    std::fs::read_to_string(inst.dir.join(PROJECT_FILE))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|p| valid_project(p))
        .unwrap_or_else(|| format!("dockwren-{}", inst.name))
}

/// Projects of all adopted instances under `root`.
pub fn adopted_projects(root: &Path) -> std::collections::HashSet<String> {
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path().join(PROJECT_FILE)).ok())
        .map(|t| t.trim().to_string())
        .filter(|p| valid_project(p))
        .collect()
}

/// JSON string literal (valid YAML scalar) for generated override files.
fn json_str(s: &str) -> String {
    serde_json::to_string(s).expect("string serialises")
}

/// `compose.override.yaml` carrying the panel-defined healthchecks, so the
/// template's own compose.yaml stays untouched. None when no service needs one.
pub fn override_yaml(t: &Template) -> Option<String> {
    let mut out = String::new();
    for s in &t.manifest.services {
        if let Health::Panel {
            test,
            interval,
            start_period,
        } = &s.health
        {
            if out.is_empty() {
                out.push_str("services:\n");
            }
            out.push_str(&format!("  {}:\n    healthcheck:\n", s.name));
            let items: Vec<String> = test.iter().map(|x| json_str(x)).collect();
            out.push_str(&format!("      test: [{}]\n", items.join(", ")));
            out.push_str(&format!(
                "      interval: {}\n",
                json_str(interval.as_deref().unwrap_or("15s"))
            ));
            out.push_str(&format!(
                "      start_period: {}\n",
                json_str(start_period.as_deref().unwrap_or("30s"))
            ));
            out.push_str("      timeout: \"5s\"\n      retries: 5\n");
        }
    }
    (!out.is_empty()).then_some(out)
}

pub(crate) fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o640)
        .open(path)?;
    f.write_all(content.as_bytes())
}

/// A new file readable by this user only (0600): for copies of `.env` and other secrets.
pub(crate) fn write_secret(path: &Path, content: &str) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(content.as_bytes())
}

/// Replace an existing file atomically (temp file + rename, mode 0640).
pub(crate) fn replace_private(path: &Path, content: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    write_private(&tmp, content)?;
    std::fs::rename(&tmp, path)
}

/// Create `<root>/<name>/` with compose.yaml, optional override, `.env` (0600)
/// and state.toml. Fails if the instance already exists. Nothing is deleted on
/// failure: the error names the directory so the user can inspect it.
pub fn prepare(
    root: &Path,
    t: &Template,
    env: &BTreeMap<String, String>,
) -> Result<Instance, String> {
    prepare_with(root, t, &t.compose, env)
}

/// [`prepare`] with a compose file other than the template's (a pasted stack).
pub fn prepare_with(
    root: &Path,
    t: &Template,
    compose_text: &str,
    env: &BTreeMap<String, String>,
) -> Result<Instance, String> {
    let name = env
        .get("INSTANCE_NAME")
        .ok_or("INSTANCE_NAME missing")?
        .clone();
    if !valid_name(&name) {
        return Err(format!("invalid instance name {name:?}"));
    }
    let dir = root.join(&name);
    std::fs::create_dir_all(root).map_err(|e| format!("{}: {e}", root.display()))?;
    std::fs::DirBuilder::new()
        .mode(0o750)
        .create(&dir)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => format!("instance {name} already exists"),
            _ => format!("{}: {e}", dir.display()),
        })?;
    let err = |what: &str, e: std::io::Error| format!("{}: {what}: {e}", dir.display());
    write_private(&dir.join("compose.yaml"), compose_text).map_err(|e| err("compose.yaml", e))?;
    if let Some(o) = override_yaml(t) {
        write_private(&dir.join("compose.override.yaml"), &o).map_err(|e| err("override", e))?;
    }
    crate::install::write_env(&dir, env).map_err(|e| err(".env", e))?;
    let state = state_text(t, t.manifest.app.version);
    write_private(&dir.join("state.toml"), &state).map_err(|e| err("state.toml", e))?;
    Ok(Instance { name, dir })
}

/// Contents of an instance's state.toml; third-party templates also record `url@sha`.
pub fn state_text(t: &Template, version: u32) -> String {
    let mut s = format!(
        "template = {}\nversion = {version}\n",
        json_str(&t.manifest.app.id)
    );
    if let Some(src) = &t.source {
        s.push_str(&format!("source = {}\n", json_str(src)));
    }
    s
}

/// Ports already claimed by installed instances (`*_PORT='n'` lines in their `.env`).
pub fn claimed_ports(root: &Path) -> std::collections::HashSet<u16> {
    let mut out = std::collections::HashSet::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for e in entries.flatten() {
        let Ok(env) = std::fs::read_to_string(e.path().join(".env")) else {
            continue;
        };
        for line in env.lines() {
            if let Some((k, v)) = line.split_once('=')
                && k.ends_with("_PORT")
                && let Ok(p) = v.trim_matches('\'').parse::<u16>()
            {
                out.insert(p);
            }
        }
    }
    out
}

/// Explicit `-f` list when the instance has resource limits or a log cap. Naming any file
/// turns off compose's automatic loading of the override, so name that one too.
fn compose_files(dir: &Path) -> Vec<String> {
    let extra: Vec<&str> = [crate::limits::YAML, crate::logcap::YAML]
        .into_iter()
        .filter(|f| dir.join(f).is_file())
        .collect();
    if extra.is_empty() {
        return Vec::new();
    }
    let mut files = vec!["compose.yaml"];
    if dir.join("compose.override.yaml").is_file() {
        files.push("compose.override.yaml");
    }
    files.extend(extra);
    files
        .into_iter()
        .flat_map(|f| ["-f".to_string(), dir.join(f).display().to_string()])
        .collect()
}

/// Run `docker compose` for an instance; returns stdout, or stderr on failure.
pub async fn compose(inst: &Instance, args: &[&str]) -> Result<String, String> {
    compose_with_stdin(inst, args, None).await
}

/// Like `compose`, with a file piped to stdin (for `exec -T` restores).
pub async fn compose_with_stdin(
    inst: &Instance,
    args: &[&str],
    stdin: Option<std::fs::File>,
) -> Result<String, String> {
    let mut cmd = Command::new("docker");
    if let Some(f) = stdin {
        cmd.stdin(std::process::Stdio::from(f));
    }
    // The Docker CLI refuses to load plugins when it cannot read ~/.docker (e.g.
    // under systemd ProtectHome), so give it a config dir inside our data dir.
    if std::env::var_os("DOCKER_CONFIG").is_none()
        && let Some(data) = inst.dir.parent().and_then(Path::parent)
    {
        cmd.env("DOCKER_CONFIG", data.join("docker-config"));
    }
    let out = cmd
        .arg("compose")
        .args(["-p", &project_name(inst)])
        .arg("--project-directory")
        .arg(&inst.dir)
        .arg("--env-file")
        .arg(inst.dir.join(".env"))
        .args(compose_files(&inst.dir))
        .args(args)
        .output()
        .await
        .map_err(|e| format!("cannot run docker: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = err.lines().rev().take(8).collect();
        Err(format!(
            "docker compose {}: {}",
            args.join(" "),
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        ))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    Healthy,
    Waiting(String),
    Failed(String),
}

/// Judge `docker compose ps -a --format json` output (NDJSON or a JSON array).
pub fn judge(ps: &str) -> Status {
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let trimmed = ps.trim();
    if trimmed.starts_with('[') {
        rows = serde_json::from_str(trimmed).unwrap_or_default();
    } else {
        rows.extend(trimmed.lines().filter_map(|l| serde_json::from_str(l).ok()));
    }
    if rows.is_empty() {
        return Status::Waiting("no containers yet".into());
    }
    let mut waiting = Vec::new();
    for r in &rows {
        let svc = r["Service"].as_str().unwrap_or("?");
        let state = r["State"].as_str().unwrap_or("");
        let health = r["Health"].as_str().unwrap_or("");
        match (state, health) {
            ("exited" | "dead", _) => return Status::Failed(format!("{svc} is {state}")),
            (_, "unhealthy") => return Status::Failed(format!("{svc} is unhealthy")),
            ("running", "" | "healthy") => {}
            _ => waiting.push(format!("{svc} ({state} {health})").trim().to_string()),
        }
    }
    if waiting.is_empty() {
        Status::Healthy
    } else {
        Status::Waiting(waiting.join(", "))
    }
}

/// Poll until every service is up and healthy, one fails, or `timeout` passes.
pub async fn wait_healthy(inst: &Instance, timeout: Duration) -> Result<(), String> {
    wait_healthy_services(inst, &[], timeout).await
}

/// [`wait_healthy`] limited to the named services (all of them when the list is empty).
pub async fn wait_healthy_services(
    inst: &Instance,
    services: &[&str],
    timeout: Duration,
) -> Result<(), String> {
    let start = Instant::now();
    let mut args = vec!["ps", "-a", "--format", "json"];
    args.extend_from_slice(services);
    loop {
        let ps = compose(inst, &args).await?;
        match judge(&ps) {
            Status::Healthy => return Ok(()),
            Status::Failed(why) => return Err(why),
            Status::Waiting(what) if start.elapsed() > timeout => {
                return Err(format!("timed out waiting for: {what}"));
            }
            Status::Waiting(_) => tokio::time::sleep(Duration::from_secs(3)).await,
        }
    }
}

/// [`wait_healthy`], then once more after a pause: a container that starts and immediately
/// exits still reads "running" in the first poll.
pub async fn wait_stable(inst: &Instance, timeout: Duration) -> Result<(), String> {
    wait_healthy(inst, timeout).await?;
    tokio::time::sleep(Duration::from_secs(8)).await;
    wait_healthy(inst, Duration::from_secs(60)).await
}

/// Pull the first capture group out of a hook's stdout.
pub fn extract(regex: &str, stdout: &str) -> Option<String> {
    regex::Regex::new(regex)
        .ok()?
        .captures(stdout)?
        .get(1)
        .map(|m| m.as_str().to_string())
}

/// Run one hook command in a running service via `compose exec`; returns stdout.
async fn exec_hook(
    inst: &Instance,
    service: &str,
    user: Option<&str>,
    run: &[String],
    env: &BTreeMap<String, String>,
) -> Result<String, String> {
    let cmd: Vec<String> = run
        .iter()
        .map(|a| crate::install::subst(a, env))
        .collect::<Result<_, _>>()?;
    let mut args = vec!["exec", "-T"];
    if let Some(u) = user {
        args.extend(["-u", u]);
    }
    args.push(service);
    args.extend(cmd.iter().map(String::as_str));
    compose(inst, &args).await
}

/// Run the migrations a version jump `from` -> `to` crosses, oldest first.
pub async fn run_migrations(
    inst: &Instance,
    t: &Template,
    from: u32,
    to: u32,
    env: &BTreeMap<String, String>,
    log: &mut (dyn FnMut(&str) + Send),
) -> Result<(), String> {
    let mut todo: Vec<_> = t
        .manifest
        .migrations
        .iter()
        .filter(|g| g.to_version > from && g.to_version <= to)
        .collect();
    todo.sort_by_key(|g| g.to_version);
    for g in todo {
        log(&format!(
            "migration {} (to version {})",
            g.name, g.to_version
        ));
        exec_hook(inst, &g.service, g.user.as_deref(), &g.run, env)
            .await
            .map_err(|e| format!("migration {}: {e}", g.name))?;
    }
    Ok(())
}

/// Parse an instance `.env` (`KEY=value` or `KEY='value'` lines).
pub fn read_env(dir: &Path) -> Result<BTreeMap<String, String>, String> {
    let raw = std::fs::read_to_string(dir.join(".env")).map_err(|e| format!(".env: {e}"))?;
    Ok(raw
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| {
            (
                k.trim().to_string(),
                v.trim().trim_matches('\'').to_string(),
            )
        })
        .collect())
}

/// Run the template's `post_start` hooks in order via `compose exec`.
/// Returns captured `(label, value)` pairs, to be shown once and not stored.
pub async fn run_post_start(
    inst: &Instance,
    t: &Template,
    env: &BTreeMap<String, String>,
    log: &mut (dyn FnMut(&str) + Send),
) -> Result<Vec<(String, String)>, String> {
    let mut shown = Vec::new();
    for h in &t.manifest.post_start {
        log(&format!("running {}", h.name));
        let out = exec_hook(inst, &h.service, h.user.as_deref(), &h.run, env)
            .await
            .map_err(|e| format!("post_start {}: {e}", h.name))?;
        if let Some(c) = &h.capture {
            let v = extract(&c.regex, &out).ok_or_else(|| {
                format!(
                    "post_start {}: output did not match the capture pattern",
                    h.name
                )
            })?;
            shown.push((c.label.clone(), v));
        }
    }
    Ok(shown)
}

/// Install steps 3-5 of the spec: validate, pull, start, wait healthy (and still healthy after a pause,
/// so a container that exits right after `up -d` is not reported as installed).
/// `log` receives one line per step.
pub async fn bring_up(inst: &Instance, log: &mut (dyn FnMut(&str) + Send)) -> Result<(), String> {
    log("capping container logs");
    crate::logcap::ensure(inst).await?;
    log("checking compose file");
    compose(inst, &["config", "-q"]).await?;
    log("pulling images");
    compose(inst, &["pull"]).await?;
    log("starting");
    compose(inst, &["up", "-d"]).await?;
    log("waiting for healthy");
    wait_stable(inst, Duration::from_secs(240)).await
}

/// Stop and remove containers; data directories are kept.
pub async fn take_down(inst: &Instance) -> Result<(), String> {
    compose(inst, &["down"]).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::collections::HashSet;

    fn tmpl(id: &str) -> Template {
        crate::template::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("apps").join(id)).unwrap()
    }

    fn tmp() -> PathBuf {
        std::env::temp_dir().join(format!("dockwren-run-{}", crate::auth::generate_password()))
    }

    #[test]
    fn names_are_restricted() {
        for ok in ["gitea", "vw-2", "ab"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [
            "",
            "a",
            "../x",
            "Gitea",
            "1abc",
            "a/b",
            "a b",
            "-a",
            &"a".repeat(32),
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn limits_file_adds_explicit_compose_files() {
        let d = std::env::temp_dir().join(format!("dw-cf-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert!(compose_files(&d).is_empty());
        std::fs::write(d.join(crate::limits::YAML), "x").unwrap();
        let n = |v: &[String]| v.iter().filter(|a| *a == "-f").count();
        assert_eq!(n(&compose_files(&d)), 2);
        std::fs::write(d.join("compose.override.yaml"), "x").unwrap();
        let f = compose_files(&d);
        assert_eq!(n(&f), 3);
        assert!(f.last().unwrap().ends_with("compose.limits.yaml"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn override_only_for_panel_health() {
        assert!(override_yaml(&tmpl("vaultwarden")).is_none());
        let y = override_yaml(&tmpl("gitea")).unwrap();
        assert!(y.starts_with("services:\n  gitea:\n    healthcheck:\n"));
        assert!(y.contains("\"CMD-SHELL\""));
        assert!(!y.contains("  db:"));
    }

    #[test]
    fn prepare_writes_files_with_right_modes_and_refuses_duplicates() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp();
        let t = tmpl("gitea");
        let input: HashMap<String, String> =
            [("ADMIN_EMAIL".to_string(), "a@b.io".to_string())].into();
        let r = crate::install::resolve(&t, &input, &HashSet::new(), &|_| true).unwrap();
        let inst = prepare(&root, &t, &r.env).unwrap();
        let mode = |f: &str| {
            std::fs::metadata(inst.dir.join(f))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(".env"), 0o600);
        assert_eq!(mode("compose.yaml"), 0o640);
        assert!(inst.dir.join("compose.override.yaml").exists());
        assert!(
            std::fs::read_to_string(inst.dir.join("state.toml"))
                .unwrap()
                .contains("gitea")
        );
        let mut tp = tmpl("gitea");
        tp.source = Some("https://example.com/r@abc".into());
        let st = state_text(&tp, 3);
        assert!(st.contains("source = \"https://example.com/r@abc\""));
        assert!(toml::from_str::<toml::Table>(&st).is_ok());
        assert!(!state_text(&t, 3).contains("source"));
        assert!(
            prepare(&root, &t, &r.env)
                .unwrap_err()
                .contains("already exists")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn prepare_rejects_path_tricks() {
        let t = tmpl("vaultwarden");
        let env: BTreeMap<_, _> = [("INSTANCE_NAME".to_string(), "../evil".to_string())].into();
        assert!(prepare(&tmp(), &t, &env).unwrap_err().contains("invalid"));
    }

    #[test]
    fn env_file_is_parsed() {
        let d = tmp();
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(".env"), "A=1\nB='two words'\nC=\n").unwrap();
        let e = read_env(&d).unwrap();
        assert_eq!(
            (e["A"].as_str(), e["B"].as_str(), e["C"].as_str()),
            ("1", "two words", "")
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn extract_takes_first_group() {
        let out = "generated random password is 'Ab3xyz'\nNew user 'admin' has been successfully created!";
        assert_eq!(
            extract("generated random password is '([^']+)'", out).as_deref(),
            Some("Ab3xyz")
        );
        assert_eq!(extract("nope (.)", out), None);
    }

    #[test]
    fn judge_reads_ndjson_and_arrays() {
        let up = r#"{"Service":"db","State":"running","Health":"healthy"}
{"Service":"web","State":"running","Health":""}"#;
        assert_eq!(judge(up), Status::Healthy);
        let starting = r#"{"Service":"web","State":"running","Health":"starting"}"#;
        assert!(matches!(judge(starting), Status::Waiting(_)));
        let dead = r#"[{"Service":"web","State":"exited","Health":""}]"#;
        assert_eq!(judge(dead), Status::Failed("web is exited".into()));
        let sick = r#"{"Service":"web","State":"running","Health":"unhealthy"}"#;
        assert_eq!(judge(sick), Status::Failed("web is unhealthy".into()));
        assert!(matches!(judge(""), Status::Waiting(_)));
    }
}
