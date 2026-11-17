//! Adopting Compose projects that were started outside the panel (phase 1: read-only scan).
//!
//! A project is found through the labels Compose puts on its containers. Its compose file is
//! resolved by `docker compose config` and checked with the same allow-list as pasted stacks
//! ([`crate::stack::lint_model`]), plus one rule of its own: host bind mounts are refused,
//! because the panel's backups only cover `<instance>/data`. Nothing here changes anything.

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

/// Adoption-only rule on top of [`crate::stack::lint_model`].
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
                return Err(format!(
                    "service {name} keeps data in the host folder {src}; only named volumes can be adopted for now"
                ));
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
    crate::stack::lint_model(&model, &canon)?;
    lint_adoption(&model)?;
    Ok(model)
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
    fn host_bind_mounts_are_not_adoptable() {
        let m = json!({"services": {"db": {"volumes": [
            {"type": "volume", "source": "pg", "target": "/d"},
            {"type": "bind", "source": "/srv/pg", "target": "/e"}]}}});
        let e = lint_adoption(&m).unwrap_err();
        assert!(e.contains("db") && e.contains("/srv/pg"), "{e}");
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
        ] {
            let dir = base.join(tag);
            std::fs::create_dir_all(&dir).unwrap();
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
    }
}
