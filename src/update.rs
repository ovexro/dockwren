//! Update flow: move an instance to the bundled template's newer version.
//! Snapshot first (per `update.pre_update`), swap compose files, pull, start,
//! wait healthy, run any `migrations` the version jump crosses; on failure put the old compose files back and restart them.
//! Data is never rolled back automatically: the snapshot path is reported.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::backup;
use crate::runner::{self, Instance};
use crate::template::Template;

#[derive(Deserialize)]
struct State {
    template: String,
    version: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Change {
    pub image: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    UpToDate,
    Updated { from: u32, to: u32 },
}

/// `image: name:tag` lines of a compose file as name -> tag (untagged = "latest").
fn images(compose: &str) -> BTreeMap<String, String> {
    compose
        .lines()
        .filter_map(|l| l.trim().strip_prefix("image:"))
        .map(|v| v.trim().trim_matches(['"', '\'']))
        .map(|v| match v.rsplit_once(':') {
            Some((n, t)) if !t.contains('/') => (n.to_string(), t.to_string()),
            _ => (v.to_string(), "latest".to_string()),
        })
        .collect()
}

/// Leading number of a tag: `16-alpine` -> `16`, `28.3.0` -> `28`.
fn major(tag: &str) -> &str {
    tag.split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap_or("")
}

/// Image tag changes between two compose files. Refuses a major-version jump
/// for images the template lists in `update.pinned_major` (databases).
pub fn plan(old: &str, new: &str, pinned_major: &[String]) -> Result<Vec<Change>, String> {
    let (o, n) = (images(old), images(new));
    let mut out = Vec::new();
    for (image, to) in &n {
        let Some(from) = o.get(image) else { continue };
        if from == to {
            continue;
        }
        if pinned_major.contains(image) && major(from) != major(to) {
            return Err(format!(
                "{image} would jump from {from} to {to}; a major version change needs a manual migration"
            ));
        }
        out.push(Change {
            image: image.clone(),
            from: from.clone(),
            to: to.clone(),
        });
    }
    Ok(out)
}

/// Replace `path` atomically (write a sibling file, rename over it).
fn replace_file(path: &Path, content: &str, mode: u32) -> Result<(), String> {
    let tmp = path.with_extension("dockwren-new");
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)
        .map_err(err)?;
    f.write_all(content.as_bytes()).map_err(err)?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(err)
}

/// Write compose.yaml and the optional override (removed when `None`).
fn write_compose(dir: &Path, compose: &str, over: Option<&str>) -> Result<(), String> {
    replace_file(&dir.join("compose.yaml"), compose, 0o640)?;
    match over {
        Some(o) => replace_file(&dir.join("compose.override.yaml"), o, 0o640),
        None => match std::fs::remove_file(dir.join("compose.override.yaml")) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("compose.override.yaml: {e}"))
            }
            _ => Ok(()),
        },
    }
}

fn read_opt(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Update one instance to template `t`.
pub async fn update(
    backups_root: &Path,
    t: &Template,
    inst: &Instance,
    from: u32,
    log: &mut (dyn FnMut(&str) + Send),
) -> Result<Outcome, String> {
    let old_compose = std::fs::read_to_string(inst.dir.join("compose.yaml"))
        .map_err(|e| format!("compose.yaml: {e}"))?;
    let old_override = read_opt(&inst.dir.join("compose.override.yaml"))?;
    let new_override = runner::override_yaml(t);

    let changes = plan(&old_compose, &t.compose, &t.manifest.update.pinned_major)?;
    for c in &changes {
        log(&format!("{}: {} -> {}", c.image, c.from, c.to));
    }

    let mut snapshot = None;
    if t.manifest.update.pre_update.iter().any(|s| s == "backup") {
        log("taking a snapshot first");
        let dir = backup::snapshot(backups_root, inst, t, log)
            .await
            .map_err(|e| format!("snapshot failed, nothing was changed: {e}"))?;
        log(&format!("snapshot in {}", dir.display()));
        snapshot = Some(dir);
    }
    let snap_note = snapshot
        .map(|d| format!(" (snapshot: {})", d.display()))
        .unwrap_or_default();

    // Keep a copy on disk in case this process dies mid-update.
    replace_file(&inst.dir.join("compose.yaml.prev"), &old_compose, 0o640)?;
    write_compose(&inst.dir, &t.compose, new_override.as_deref())?;

    let to = t.manifest.app.version;
    let up = async {
        runner::bring_up(inst, log).await?;
        let env = runner::read_env(&inst.dir)?;
        runner::run_migrations(inst, t, from, to, &env, log).await
    }
    .await;
    match up {
        Ok(()) => {
            let state = format!("template = {:?}\nversion = {to}\n", t.manifest.app.id);
            replace_file(&inst.dir.join("state.toml"), &state, 0o640)?;
            Ok(Outcome::Updated { from, to })
        }
        Err(e) => {
            log("update failed, restoring the previous compose files");
            write_compose(&inst.dir, &old_compose, old_override.as_deref())?;
            let back = async {
                runner::compose(inst, &["up", "-d"]).await?;
                runner::wait_healthy(inst, Duration::from_secs(120)).await
            }
            .await;
            Err(match back {
                Ok(()) => format!(
                    "update failed: {e}\nrolled back; the old version is running again{snap_note}"
                ),
                Err(r) => format!(
                    "update failed: {e}\nrollback also failed: {r}\nrestore data from the snapshot if needed{snap_note}"
                ),
            })
        }
    }
}

/// CLI entry: look the instance up, find its template, update it.
pub async fn update_instance(
    instances_root: &Path,
    backups_root: &Path,
    templates: &[Template],
    name: &str,
    log: &mut (dyn FnMut(&str) + Send),
) -> Result<Outcome, String> {
    let dir = instances_root.join(name);
    let raw = std::fs::read_to_string(dir.join("state.toml"))
        .map_err(|_| format!("no instance named {name}"))?;
    let state: State = toml::from_str(&raw).map_err(|e| format!("state.toml: {e}"))?;
    let t = templates
        .iter()
        .find(|t| t.manifest.app.id == state.template)
        .ok_or_else(|| format!("template {} is not available", state.template))?;
    let to = t.manifest.app.version;
    if state.version == to {
        return Ok(Outcome::UpToDate);
    }
    if state.version > to {
        return Err(format!(
            "instance is at version {} but this Dockwren only knows version {to}",
            state.version
        ));
    }
    update(
        backups_root,
        t,
        &Instance {
            name: name.to_string(),
            dir,
        },
        state.version,
        log,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "services:\n  db:\n    image: postgres:16-alpine\n  app:\n    image: \"gitea/gitea:28.3.0\"\n  x:\n    image: nginx\n";

    #[test]
    fn images_are_parsed_with_tags() {
        let i = images(OLD);
        assert_eq!(i["postgres"], "16-alpine");
        assert_eq!(i["gitea/gitea"], "28.3.0");
        assert_eq!(i["nginx"], "latest");
        assert_eq!(
            images("image: localhost:5000/app")["localhost:5000/app"],
            "latest"
        );
    }

    #[test]
    fn major_is_the_leading_number() {
        assert_eq!(major("16-alpine"), "16");
        assert_eq!(major("28.3.0"), "28");
        assert_eq!(major("latest"), "");
    }

    #[test]
    fn plan_lists_changes_and_blocks_pinned_major_jumps() {
        let pins = vec!["postgres".to_string()];
        let new = OLD
            .replace("28.3.0", "28.4.1")
            .replace("16-alpine", "16.4-alpine");
        let c = plan(OLD, &new, &pins).unwrap();
        assert_eq!(c.len(), 2);
        assert!(c.contains(&Change {
            image: "gitea/gitea".into(),
            from: "28.3.0".into(),
            to: "28.4.1".into()
        }));
        assert!(
            plan(OLD, &OLD.replace("16-alpine", "17-alpine"), &pins)
                .unwrap_err()
                .contains("major")
        );
        // the same jump is fine for an image that is not pinned
        assert!(plan(OLD, &OLD.replace("28.3.0", "29.0.0"), &pins).is_ok());
        assert!(plan(OLD, OLD, &pins).unwrap().is_empty());
    }

    #[test]
    fn compose_files_are_swapped_and_override_removed() {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("dockwren-upd-{}", crate::auth::generate_password()));
        std::fs::create_dir_all(&dir).unwrap();
        write_compose(&dir, "a", Some("o")).unwrap();
        write_compose(&dir, "b", Some("p")).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("compose.yaml")).unwrap(),
            "b"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("compose.override.yaml")).unwrap(),
            "p"
        );
        assert_eq!(
            std::fs::metadata(dir.join("compose.yaml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        write_compose(&dir, "c", None).unwrap();
        assert!(!dir.join("compose.override.yaml").exists());
        write_compose(&dir, "d", None).unwrap(); // already absent: fine
        assert!(!dir.join("compose.yaml.dockwren-new").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
