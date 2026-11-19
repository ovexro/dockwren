//! Adopting Compose projects that were started outside the panel (phase 1: read-only scan).
//!
//! A project is found through the labels Compose puts on its containers. Its compose file is
//! resolved by `docker compose config` and checked with the same allow-list as pasted stacks
//! ([`crate::stack::lint_model`]), plus one rule of its own: host bind mounts must be plain
//! folders the panel can back up in place (see [`crate::backup::bind_source_ok`]).
//! Nothing here changes anything.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use crate::docker::ForeignContainer;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub project: String,
    pub working_dir: String,
    /// `service (state)` per container.
    pub containers: Vec<String>,
    /// `Ok(())` = could be adopted; `Err` = the reason it cannot.
    pub verdict: Result<(), String>,
}

impl Candidate {
    /// A panel instance name derived from the project name (letters, digits, hyphens).
    pub fn suggested_name(&self) -> String {
        let mut n: String = self
            .project
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .to_lowercase();
        if !n.starts_with(|c: char| c.is_ascii_lowercase()) {
            n.insert_str(0, "app-");
        }
        n.truncate(31);
        n
    }
}

/// Groups containers by project; the order is by project name.
fn group(list: Vec<ForeignContainer>) -> BTreeMap<String, Vec<ForeignContainer>> {
    let mut map: BTreeMap<String, Vec<ForeignContainer>> = BTreeMap::new();
    for c in list {
        map.entry(c.project.clone()).or_default().push(c);
    }
    map
}

/// The one compose file of a project, or why there is no single usable one.
fn single_file(c: &ForeignContainer) -> Result<&str, String> {
    let files: Vec<&str> = c
        .config_files
        .split(',')
        .filter(|f| !f.is_empty())
        .collect();
    match files.as_slice() {
        [] => Err("Compose did not record a compose file (started with `docker run`-style tools or an old version)".into()),
        [one] if one.starts_with('/') => Ok(one),
        [_] => Err("the recorded compose file path is not absolute".into()),
        _ => Err("it uses several compose files (-f twice or an override file)".into()),
    }
}

/// Adoption-only rule on top of [`crate::stack::lint_model`]. Host folders stay where they
/// are and are archived in place by the backups.
pub fn lint_adoption(model: &Value) -> Result<(), String> {
    for (name, svc) in model
        .get("services")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        for v in svc
            .get("volumes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if v.get("type").and_then(Value::as_str) == Some("bind") {
                let src = v.get("source").and_then(Value::as_str).unwrap_or("?");
                crate::backup::bind_source_ok(src).map_err(|e| format!("service {name}: {e}"))?;
            }
        }
    }
    Ok(())
}

async fn resolve(data_dir: &Path, project: &str, file: &str, wd: &str) -> Result<Value, String> {
    let text =
        std::fs::read_to_string(file).map_err(|e| format!("the panel cannot read {file}: {e}"))?;
    crate::stack::precheck(&text)?;
    let wd_path = Path::new(wd);
    if !wd_path.is_absolute() {
        return Err("the project directory was not recorded".into());
    }
    let out = tokio::process::Command::new("docker")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("DOCKER_CONFIG", data_dir.join("docker-config"))
        .envs(std::env::var_os("DOCKER_HOST").map(|h| ("DOCKER_HOST", h)))
        .args([
            "compose",
            "-p",
            project,
            "--project-directory",
            wd,
            "-f",
            file,
        ])
        .args(["config", "--format", "json"])
        .output()
        .await
        .map_err(|e| format!("cannot run docker: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let msg: Vec<&str> = err
            .lines()
            .filter(|l| !l.trim().is_empty())
            .take(4)
            .collect();
        return Err(format!(
            "Docker does not accept this file: {}",
            msg.join(" ")
        ));
    }
    let model: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("unreadable compose model: {e}"))?;
    let canon = wd_path.canonicalize().map_err(|e| format!("{wd}: {e}"))?;
    crate::stack::lint_model_opts(&model, &canon, true)?;
    lint_adoption(&model)?;
    Ok(model)
}

fn read_compose(file: &str) -> Result<String, String> {
    std::fs::read_to_string(file).map_err(|e| format!("the panel cannot read {file}: {e}"))
}

/// Per-service config hashes of a project, the value Compose compares to decide whether a
/// container must be recreated.
async fn config_hashes(
    data_dir: &Path,
    project: &str,
    wd: &Path,
    env_file: Option<&Path>,
    file: &Path,
) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("docker");
    cmd.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("DOCKER_CONFIG", data_dir.join("docker-config"))
        .envs(std::env::var_os("DOCKER_HOST").map(|h| ("DOCKER_HOST", h)))
        .args(["compose", "-p", project, "--project-directory"])
        .arg(wd);
    if let Some(e) = env_file {
        cmd.arg("--env-file").arg(e);
    }
    let out = cmd
        .arg("-f")
        .arg(file)
        .args(["config", "--hash", "*"])
        .output()
        .await
        .map_err(|e| format!("cannot run docker: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "docker compose config --hash: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let mut lines: Vec<&str> = std::str::from_utf8(&out.stdout)
        .map_err(|e| e.to_string())?
        .lines()
        .collect();
    lines.sort_unstable();
    Ok(lines.join("\n"))
}

/// Registers a running Compose project as a panel instance. Nothing is stopped, recreated or
/// moved: the compose file (and `.env`) are copied into a new instance folder, the project
/// name is recorded, and the copy must produce exactly the config hashes of the original,
/// otherwise Compose would recreate the containers on the next `up` and the adoption is undone.
pub async fn adopt(
    data_dir: &Path,
    root: &Path,
    template: &crate::template::Template,
    list: Vec<ForeignContainer>,
    project: &str,
    name: &str,
) -> Result<crate::runner::Instance, String> {
    use crate::runner;
    if !runner::valid_name(name) {
        return Err("Name: lower-case letters, digits and hyphens, starting with a letter".into());
    }
    if !runner::valid_project(project) {
        return Err("unusable project name".into());
    }
    let cs: Vec<ForeignContainer> = list.into_iter().filter(|c| c.project == project).collect();
    let first = cs.first().ok_or("that project is no longer running here")?;
    let file = single_file(first)?.to_string();
    let wd = first.working_dir.clone();
    let model = resolve(data_dir, project, &file, &wd).await?;
    let text = read_compose(&file)?;
    let env_path = Path::new(&wd).join(".env");
    let env_raw = match std::fs::read(&env_path) {
        Ok(b) if b.len() <= crate::stack::MAX_COMPOSE_BYTES => b,
        Ok(_) => return Err(".env is larger than 64 KiB".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("the panel cannot read {}: {e}", env_path.display())),
    };
    let original = config_hashes(
        data_dir,
        project,
        Path::new(&wd),
        env_path.is_file().then_some(env_path.as_path()),
        Path::new(&file),
    )
    .await?;

    let port = crate::stack::first_published_port(&model);
    let dir = root.join(name);
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::create_dir_all(root).map_err(|e| format!("{}: {e}", root.display()))?;
        std::fs::DirBuilder::new()
            .mode(0o750)
            .create(&dir)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::AlreadyExists => format!("instance {name} already exists"),
                _ => format!("{}: {e}", dir.display()),
            })?;
    }
    let inst = runner::Instance {
        name: name.to_string(),
        dir: dir.clone(),
    };
    let build = || -> Result<(), String> {
        let err = |what: &str, e: std::io::Error| format!("{}: {what}: {e}", dir.display());
        runner::write_private(&dir.join("compose.yaml"), &absolutize_binds(&text, &wd))
            .map_err(|e| err("compose.yaml", e))?;
        let mut env = String::from_utf8_lossy(&env_raw).into_owned();
        if let Some(p) = port
            && !env.lines().any(|l| l.trim_start().starts_with("WEB_PORT="))
        {
            if !env.is_empty() && !env.ends_with('\n') {
                env.push('\n');
            }
            env.push_str(&format!("WEB_PORT={p}\n"));
        }
        crate::install::write_env_text(&dir, &env).map_err(|e| err(".env", e))?;
        std::fs::create_dir_all(dir.join("data")).map_err(|e| err("data", e))?;
        runner::write_private(&dir.join(runner::PROJECT_FILE), &format!("{project}\n"))
            .map_err(|e| err("project", e))?;
        runner::write_private(&dir.join("state.toml"), &runner::state_text(template, 1))
            .map_err(|e| err("state.toml", e))
    };
    let copy_hashes = async {
        build()?;
        config_hashes(
            data_dir,
            project,
            &dir,
            Some(&dir.join(".env")),
            &dir.join("compose.yaml"),
        )
        .await
    }
    .await;
    match copy_hashes {
        Ok(h) if h == original => {
            crate::terminal::set_adopted(runner::adopted_projects(root));
            Ok(inst)
        }
        other => {
            // `dir` was created above by this call and holds only the files written here.
            let _ = std::fs::remove_dir_all(&dir);
            Err(match other {
                Err(e) => e,
                Ok(_) if relative_bind_hint(&model, &wd) => "this project keeps data in a folder written relative to its own directory (like ./data); the copy would point somewhere else. Write the full path in its compose file (like /srv/app/data), restart it, and adopt again. Nothing was adopted".into(),
                Ok(_) => "the copied file would not match the running containers (Compose would recreate them), so nothing was adopted".into(),
            })
        }
    }
}

/// Joins a relative path onto `base` lexically (`.` and `..` resolved, no filesystem access).
fn join_lexical(base: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    format!("/{}", parts.join("/"))
}

/// The copy of the compose file runs from the panel's own folder, so a host folder written
/// relative to the project (`./data`, `../shared`, `source: ./data`) would point somewhere else.
/// Rewrites exactly those sources to absolute paths; anything unusual is left alone and the
/// configuration check that follows refuses the adoption instead.
fn absolutize_binds(text: &str, wd: &str) -> String {
    let is_rel = |p: &str| p == "." || p == ".." || p.starts_with("./") || p.starts_with("../");
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let t = line.trim_start();
        let indent = &line[..line.len() - t.len()];
        let fixed = if let Some(rest) = t.strip_prefix("- ") {
            // short syntax: `- ./data:/var/lib/x[:ro]`, optionally quoted
            let (q, body) = match rest.chars().next() {
                Some(c @ ('"' | '\'')) => (Some(c), &rest[1..]),
                _ => (None, rest),
            };
            match body.find(':') {
                Some(i) if is_rel(&body[..i]) => Some(format!(
                    "{indent}- {}{}{}",
                    q.map(String::from).unwrap_or_default(),
                    join_lexical(wd, &body[..i]),
                    &body[i..]
                )),
                _ => None,
            }
        } else if let Some(rest) = t.strip_prefix("source:") {
            // long syntax: `source: ./data`, optionally quoted, optional trailing comment
            let v = rest.trim();
            let (v, tail) = match v.find(" #") {
                Some(i) => (v[..i].trim_end(), &v[i..]),
                None => (v, ""),
            };
            let unq = v.trim_matches(|c| c == '"' || c == '\'');
            is_rel(unq).then(|| format!("{indent}source: {}{tail}", join_lexical(wd, unq)))
        } else {
            None
        };
        out.push_str(fixed.as_deref().unwrap_or(line));
        out.push('\n');
    }
    if !text.ends_with('\n') {
        out.pop();
    }
    out
}

/// True when a host bind mount lies inside the project directory: written as `./data` it resolves
/// elsewhere once the file is copied into the panel, which is the usual reason the copy differs.
fn relative_bind_hint(model: &Value, wd: &str) -> bool {
    let base = Path::new(wd);
    model
        .get("services")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|m| m.values())
        .filter_map(|svc| svc.get("volumes").and_then(Value::as_array))
        .flatten()
        .filter(|v| v.get("type").and_then(Value::as_str) == Some("bind"))
        .filter_map(|v| v.get("source").and_then(Value::as_str))
        .any(|src| Path::new(src).starts_with(base))
}

/// Looks at every Compose project the panel does not manage and says whether it could be adopted.
pub async fn scan(data_dir: &Path, list: Vec<ForeignContainer>) -> Vec<Candidate> {
    let mut out = Vec::new();
    for (project, cs) in group(list) {
        let first = &cs[0];
        let verdict = match single_file(first) {
            Ok(file) => resolve(data_dir, &project, file, &first.working_dir)
                .await
                .map(|_| ()),
            Err(e) => Err(e),
        };
        out.push(Candidate {
            working_dir: first.working_dir.clone(),
            containers: cs
                .iter()
                .map(|c| format!("{} ({})", c.service, c.state))
                .collect(),
            project,
            verdict,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fc(project: &str, files: &str) -> ForeignContainer {
        ForeignContainer {
            project: project.into(),
            service: "web".into(),
            working_dir: "/srv/x".into(),
            config_files: files.into(),
            state: "running".into(),
        }
    }

    #[test]
    fn containers_are_grouped_by_project() {
        let g = group(vec![fc("b", "/a"), fc("a", "/a"), fc("b", "/a")]);
        assert_eq!(g.keys().collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(g["b"].len(), 2);
    }

    #[test]
    fn exactly_one_absolute_compose_file_is_needed() {
        assert_eq!(
            single_file(&fc("p", "/srv/x/compose.yaml")),
            Ok("/srv/x/compose.yaml")
        );
        assert!(single_file(&fc("p", "")).is_err());
        assert!(single_file(&fc("p", "compose.yaml")).is_err());
        assert!(
            single_file(&fc("p", "/a.yml,/b.yml"))
                .unwrap_err()
                .contains("several")
        );
    }

    #[test]
    fn relative_host_folders_become_absolute_in_the_copy() {
        let t = "services:\n  a:\n    volumes:\n      - ./data:/d\n      - \"../shared:/s:ro\"\n      - pg:/p\n      - /abs:/x\n      - type: bind\n        source: ./conf # note\n        target: /c\n    command: [\"./run:x\"]\n";
        let r = absolutize_binds(t, "/srv/app");
        assert!(r.contains("      - /srv/app/data:/d\n"), "{r}");
        assert!(r.contains("      - \"/srv/shared:/s:ro\"\n"), "{r}");
        assert!(r.contains("      - pg:/p\n") && r.contains("      - /abs:/x\n"), "{r}");
        assert!(r.contains("        source: /srv/app/conf # note\n"), "{r}");
        assert!(r.contains("command: [\"./run:x\"]"), "{r}");
        assert_eq!(absolutize_binds("a: 1", "/srv"), "a: 1");
        assert_eq!(join_lexical("/srv/app", "."), "/srv/app");
    }

    #[test]
    fn a_bind_inside_the_project_directory_is_flagged() {
        let inside = json!({"services": {"a": {"volumes": [
            {"type": "bind", "source": "/srv/app/data", "target": "/d"}]}}});
        assert!(relative_bind_hint(&inside, "/srv/app"));
        assert!(!relative_bind_hint(&inside, "/srv/other"));
        let named = json!({"services": {"a": {"volumes": [{"type": "volume", "source": "pg", "target": "/d"}]}}});
        assert!(!relative_bind_hint(&named, "/srv/app"));
    }

    #[test]
    fn only_plain_host_folders_are_adoptable() {
        let m = json!({"services": {"db": {"volumes": [
            {"type": "volume", "source": "pg", "target": "/d"},
            {"type": "bind", "source": "/srv/pg", "target": "/e"}]}}});
        assert_eq!(lint_adoption(&m), Ok(()));
        let m = json!({"services": {"db": {"volumes": [
            {"type": "bind", "source": "/etc/pg", "target": "/e"}]}}});
        let e = lint_adoption(&m).unwrap_err();
        assert!(e.contains("db") && e.contains("/etc/pg"), "{e}");
        let ok = json!({"services": {"db": {"volumes": [{"type": "volume", "source": "pg", "target": "/d"}]}}});
        assert_eq!(lint_adoption(&ok), Ok(()));
    }

    /// Real Docker: two throw-away projects (created, never started) with a named volume and
    /// with a host bind mount.
    #[tokio::test]
    async fn scan_finds_real_projects() {
        let base = std::env::temp_dir().join(format!("dw-adopt-{}", std::process::id()));
        let mut names = Vec::new();
        for (tag, vol) in [
            ("ok", "volumes: [\"pgdata:/var/lib/postgresql/data\"]"),
            ("bind", "volumes: [\"/tmp:/x\"]"),
            ("hostdir", "volumes: [\"HOSTDIR:/x\"]"),
        ] {
            let dir = base.join(tag);
            std::fs::create_dir_all(&dir).unwrap();
            let hostdir = base.join("hostdata");
            std::fs::create_dir_all(&hostdir).unwrap();
            let vol = vol.replace("HOSTDIR", &hostdir.display().to_string());
            let project = format!("dwadopt-{tag}-{}", std::process::id());
            let file = dir.join("compose.yaml");
            std::fs::write(
                &file,
                format!(
                    "services:\n  db:\n    image: postgres:16-alpine\n    {vol}\nvolumes:\n  pgdata:\n"
                ),
            )
            .unwrap();
            let up = std::process::Command::new("docker")
                .args(["compose", "-p", &project, "-f"])
                .arg(&file)
                .args(["create"])
                .output()
                .unwrap();
            assert!(
                up.status.success(),
                "{}",
                String::from_utf8_lossy(&up.stderr)
            );
            names.push((project, file));
        }
        let docker = crate::docker::DockerClient::connect().unwrap();
        let list = docker.foreign_compose_containers().await.unwrap();
        let found = scan(&base, list).await;
        for (project, file) in &names {
            let _ = std::process::Command::new("docker")
                .args(["compose", "-p", project, "-f"])
                .arg(file)
                .args(["down", "-v"])
                .output();
        }
        let _ = std::fs::remove_dir_all(&base);
        let get = |p: &str| found.iter().find(|c| c.project == p).unwrap();
        assert_eq!(get(&names[0].0).verdict, Ok(()), "{:?}", get(&names[0].0));
        let e = get(&names[1].0).verdict.clone().unwrap_err();
        assert!(e.contains("/tmp"), "{e}");
        assert_eq!(get(&names[2].0).verdict, Ok(()), "{:?}", get(&names[2].0));
    }

    /// Real Docker: adopt a started project, then `up -d` through the panel's runner must keep
    /// the very same container, and a copy of the instance must not touch it.
    #[tokio::test]
    async fn adopted_project_is_not_recreated() {
        use crate::runner;
        let base = std::env::temp_dir().join(format!("dw-adopt2-{}", std::process::id()));
        let src = base.join("src");
        let root = base.join("data/instances");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let project = format!("dwadopt2-{}", std::process::id());
        let file = src.join("compose.yaml");
        std::fs::write(
            &file,
            "services:\n  db:\n    image: postgres:16-alpine\n    environment:\n      POSTGRES_PASSWORD: ${PW}\n    ports: [\"127.0.0.1:${PORT}:5432\"]\n    volumes: [\"pgdata:/var/lib/postgresql/data\"]\nvolumes:\n  pgdata:\n",
        )
        .unwrap();
        // A free port, so a stray process or leftover container cannot break the test.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        std::fs::write(src.join(".env"), format!("PW=secret\nPORT={port}")).unwrap();
        // Cleans up even when an assertion below panics.
        struct Cleanup(String, std::path::PathBuf, std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::process::Command::new("docker")
                    .args(["compose", "-p", &self.0, "-f"])
                    .arg(&self.1)
                    .args(["down", "-v"])
                    .output();
                let _ = std::fs::remove_dir_all(&self.2);
            }
        }
        let _cleanup = Cleanup(project.clone(), file.clone(), base.clone());
        let sh = |args: &[&str]| {
            let o = std::process::Command::new("docker")
                .args(["compose", "-p", &project, "-f"])
                .arg(&file)
                .args(args)
                .output()
                .unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };
        sh(&["up", "-d"]);
        let before = sh(&["ps", "-q"]);

        let docker = crate::docker::DockerClient::connect().unwrap();
        let t = crate::template::catalog(None)
            .unwrap()
            .into_iter()
            .find(|t| t.manifest.app.id == "custom")
            .unwrap();
        let list = docker.foreign_compose_containers().await.unwrap();
        let res = adopt(&base, &root, &t, list, &project, "adopted-x").await;
        let mut after = String::new();
        let mut proj_of_copy = String::new();
        if let Ok(inst) = &res {
            runner::compose(inst, &["up", "-d"]).await.unwrap();
            after = runner::compose(inst, &["ps", "-q"])
                .await
                .unwrap()
                .trim()
                .to_string();
            assert_eq!(runner::project_name(inst), project);
            // A copy (as clones and restore tests make) has no project file.
            let copy = runner::Instance {
                name: "rt-adopted-x".into(),
                dir: root.join("rt-adopted-x"),
            };
            proj_of_copy = runner::project_name(&copy);
            // Other tests build `Apps`, which resets this process-wide set: refresh it right
            // before checking so the check does not depend on their timing.
            crate::terminal::set_adopted(runner::adopted_projects(&root));
            assert!(crate::terminal::managed(Some(&project)));
        }
        let inst = res.unwrap();
        assert_eq!(inst.name, "adopted-x");
        assert_eq!(before, after, "container was recreated");
        assert_eq!(proj_of_copy, "dockwren-rt-adopted-x");
    }
}
