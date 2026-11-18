//! Unused Docker volumes and networks: listed with sizes on Settings → Disk usage and removed
//! only when the admin clicks the button.
//!
//! What counts as unused: a volume no container (running or stopped) mounts, a network with no
//! container attached. Anything that carries the compose project label of an installed app is
//! kept and only counted: a stopped app's data must never be offered for deletion. The removal
//! re-reads the list at click time and passes names to `docker volume rm` / `docker network rm`,
//! which refuse anything still in use.

use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;

use crate::diskusage::docker;

/// What belongs to installed apps and must never be offered for deletion.
#[derive(Debug, Default, Clone)]
pub struct Protected {
    /// Compose project names.
    pub projects: HashSet<String>,
    /// Volume and network names the apps' compose files declare (covers `external` ones, which
    /// carry no project label).
    pub names: HashSet<String>,
    /// False when an app's compose file could not be read: then nothing is offered, because
    /// the declared names are not known.
    pub complete: bool,
}

impl Protected {
    /// Add the volume and network names of one app's `docker compose config --format json`.
    /// Returns false when the text is not usable.
    pub fn add_config(&mut self, json: &str) -> bool {
        let Ok(v) = serde_json::from_str::<Value>(json) else {
            return false;
        };
        for section in ["volumes", "networks"] {
            for def in v[section].as_object().into_iter().flat_map(|m| m.values()) {
                if let Some(n) = def["name"].as_str() {
                    self.names.insert(n.to_string());
                }
            }
        }
        true
    }

    fn keeps(&self, name: &str, labels_project: Option<&str>) -> bool {
        self.names.contains(name) || labels_project.is_some_and(|p| self.projects.contains(p))
    }
}

const PROJECT_LABEL: &str = "com.docker.compose.project";
/// Networks Docker itself owns.
const BUILTIN_NETWORKS: [&str; 3] = ["bridge", "host", "none"];

#[derive(Debug, PartialEq, Eq)]
pub struct OrphanVolume {
    pub name: String,
    pub bytes: u64,
    pub size: String,
    /// Created implicitly by an image's `VOLUME` line (long hex name), not by a person or compose.
    pub anonymous: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Orphans {
    pub volumes: Vec<OrphanVolume>,
    /// Unused volumes kept because they belong to an installed app.
    pub kept_volumes: usize,
    pub networks: Vec<String>,
    pub kept_networks: usize,
}

impl Orphans {
    pub fn volume_bytes(&self) -> u64 {
        self.volumes.iter().map(|v| v.bytes).sum()
    }
}

/// Docker's human sizes ("15.33kB", "1.2GB", "0B") back to bytes (decimal units).
pub fn parse_size(s: &str) -> u64 {
    let s = s.trim();
    let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let Ok(n) = s[..split].trim().parse::<f64>() else {
        return 0;
    };
    let mult = match s[split..].to_ascii_lowercase().as_str() {
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        _ => 1.0,
    };
    (n * mult) as u64
}

/// The compose project in a label string such as `a=b,com.docker.compose.project=x,c=d`.
fn project_of(labels: &str) -> Option<&str> {
    labels
        .split(',')
        .find_map(|kv| kv.strip_prefix(PROJECT_LABEL)?.strip_prefix('='))
}

/// From `docker system df -v --format '{{json .}}'`: volumes nobody mounts.
pub fn parse_volumes(json: &str, protected: &Protected) -> (Vec<OrphanVolume>, usize) {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return (Vec::new(), 0);
    };
    let (mut out, mut kept) = (Vec::new(), 0);
    for vol in v["Volumes"].as_array().into_iter().flatten() {
        let (Some(name), Some(links)) = (vol["Name"].as_str(), vol["Links"].as_str()) else {
            continue;
        };
        if links != "0" {
            continue;
        }
        let labels = vol["Labels"].as_str().unwrap_or("");
        if protected.keeps(name, project_of(labels)) {
            kept += 1;
            continue;
        }
        let size = vol["Size"].as_str().unwrap_or("0B").to_string();
        out.push(OrphanVolume {
            name: name.to_string(),
            bytes: parse_size(&size),
            anonymous: labels.contains("com.docker.volume.anonymous"),
            size,
        });
    }
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.name.cmp(&b.name)));
    (out, kept)
}

/// From `docker network inspect` of every network: empty ones that are not Docker's own.
pub fn parse_networks(json: &str, protected: &Protected) -> (Vec<String>, usize) {
    let Ok(Value::Array(list)) = serde_json::from_str::<Value>(json) else {
        return (Vec::new(), 0);
    };
    let (mut out, mut kept) = (Vec::new(), 0);
    for n in &list {
        let Some(name) = n["Name"].as_str() else {
            continue;
        };
        let empty = n["Containers"].as_object().is_none_or(|c| c.is_empty());
        if !empty || BUILTIN_NETWORKS.contains(&name) {
            continue;
        }
        if protected.keeps(name, n["Labels"][PROJECT_LABEL].as_str()) {
            kept += 1;
            continue;
        }
        out.push(name.to_string());
    }
    out.sort();
    (out, kept)
}

async fn run(data_dir: &Path, args: &[&str]) -> Option<String> {
    let o = docker(data_dir).args(args).output().await.ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Look at Docker now. `protected` = compose project names of the installed apps.
pub async fn scan(data_dir: &Path, protected: &Protected) -> Orphans {
    let mut o = Orphans::default();
    if !protected.complete {
        return o;
    }
    if let Some(t) = run(data_dir, &["system", "df", "-v", "--format", "{{json .}}"]).await {
        (o.volumes, o.kept_volumes) = parse_volumes(&t, protected);
    }
    if let Some(ids) = run(data_dir, &["network", "ls", "-q"]).await {
        let ids: Vec<&str> = ids.split_whitespace().collect();
        if !ids.is_empty() {
            let mut args = vec!["network", "inspect"];
            args.extend(ids);
            if let Some(t) = run(data_dir, &args).await {
                (o.networks, o.kept_networks) = parse_networks(&t, protected);
            }
        }
    }
    o
}

/// Remove what `scan` lists right now. Returns a one-line summary. Names that became used in
/// the meantime are refused by docker and simply not counted.
pub async fn remove(
    data_dir: &Path,
    protected: &Protected,
    volumes: bool,
) -> Result<String, String> {
    let o = scan(data_dir, protected).await;
    let names: Vec<&str> = if volumes {
        o.volumes.iter().map(|v| v.name.as_str()).collect()
    } else {
        o.networks.iter().map(String::as_str).collect()
    };
    let what = if volumes { "volume" } else { "network" };
    if names.is_empty() {
        return Ok(format!("No unused {what}s."));
    }
    let (done, failed) = rm_names(data_dir, what, &names).await?;
    Ok(if failed == 0 {
        format!("Removed {done} unused {what}(s).")
    } else {
        format!("Removed {done} unused {what}(s); {failed} could not be removed (in use or gone).")
    })
}

/// `docker volume rm` / `docker network rm` for exactly these names, in small batches (one
/// refusal must not hide the others). Returns (removed, not removed).
async fn rm_names(data_dir: &Path, what: &str, names: &[&str]) -> Result<(usize, usize), String> {
    let (mut done, mut failed) = (0usize, 0usize);
    for chunk in names.chunks(20) {
        let mut args = vec![what, "rm"];
        args.extend(chunk);
        let out = docker(data_dir)
            .args(&args)
            .output()
            .await
            .map_err(|e| format!("cannot run docker: {e}"))?;
        let ok = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count();
        done += ok;
        failed += chunk.len().saturating_sub(ok);
    }
    Ok((done, failed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn protected() -> Protected {
        let mut p = Protected {
            projects: HashSet::from(["dockwren-wp".to_string()]),
            complete: true,
            ..Protected::default()
        };
        assert!(p.add_config(r#"{"volumes":{"d":{"name":"shared_ext","external":true}},"networks":{"n":{"name":"edge"}}}"#));
        assert!(!p.add_config("nope"));
        p
    }

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("0B"), 0);
        assert_eq!(parse_size("15.33kB"), 15330);
        assert_eq!(parse_size("1.2GB"), 1_200_000_000);
        assert_eq!(parse_size("nonsense"), 0);
    }

    #[test]
    fn only_unmounted_unprotected_volumes_are_listed() {
        let json = r#"{"Volumes":[
          {"Name":"aaa","Links":"0","Size":"2MB","Labels":"com.docker.volume.anonymous="},
          {"Name":"bbb","Links":"1","Size":"9GB","Labels":""},
          {"Name":"dockwren-wp_db","Links":"0","Size":"500MB","Labels":"com.docker.compose.project=dockwren-wp,x=y"},
          {"Name":"old_data","Links":"0","Size":"5MB","Labels":"com.docker.compose.project=gone"},
          {"Name":"shared_ext","Links":"0","Size":"7MB","Labels":""}
        ]}"#;
        let (v, kept) = parse_volumes(json, &protected());
        assert_eq!(kept, 2);
        let names: Vec<_> = v.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["old_data", "aaa"]); // biggest first
        assert!(v[1].anonymous && !v[0].anonymous);
        assert_eq!(parse_volumes("not json", &protected()), (Vec::new(), 0));
    }

    #[test]
    fn only_empty_foreign_networks_are_listed() {
        let json = r#"[
          {"Name":"bridge","Containers":{},"Labels":{}},
          {"Name":"busy","Containers":{"abc":{}},"Labels":{}},
          {"Name":"dockwren-wp_default","Containers":{},"Labels":{"com.docker.compose.project":"dockwren-wp"}},
          {"Name":"edge","Containers":{},"Labels":{}},
          {"Name":"stray","Containers":{},"Labels":null}
        ]"#;
        assert_eq!(
            parse_networks(json, &protected()),
            (vec!["stray".to_string()], 2)
        );
    }

    /// Needs a running Docker: creates a throwaway volume and network, finds them in the scan,
    /// and removes exactly those (and a name that does not exist, counted as not removed).
    #[tokio::test]
    #[ignore = "needs docker"]
    async fn scan_finds_and_rm_removes_a_throwaway_volume_and_network() {
        let dir = std::env::temp_dir();
        let d = |a: &[&str]| {
            std::process::Command::new("docker")
                .args(a)
                .output()
                .unwrap()
                .status
                .success()
        };
        assert!(d(&["volume", "create", "dwtest-orphan-vol"]));
        assert!(d(&["network", "create", "dwtest-orphan-net"]));
        let p = Protected {
            complete: true,
            ..Protected::default()
        };
        let o = scan(&dir, &p).await;
        assert!(o.volumes.iter().any(|v| v.name == "dwtest-orphan-vol"));
        assert!(o.networks.iter().any(|n| n == "dwtest-orphan-net"));
        assert_eq!(
            rm_names(&dir, "volume", &["dwtest-orphan-vol", "dwtest-nope"]).await,
            Ok((1, 1))
        );
        assert_eq!(
            rm_names(&dir, "network", &["dwtest-orphan-net"]).await,
            Ok((1, 0))
        );
        let mut p2 = p.clone();
        p2.projects.insert("x".into());
        p2.complete = false;
        assert_eq!(scan(&dir, &p2).await, Orphans::default());
    }
}
