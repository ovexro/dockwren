//! Real restore test: restore the newest snapshot of an app into a throwaway copy
//! (own name, own ports, own folder), wait until it is healthy, then delete the copy.
//! Unlike `verify`, which only reads the archive, this proves the snapshot comes back to life.
//! The live app is never touched.

use std::path::{Path, PathBuf};

use crate::runner::{self, Instance};
use crate::template::Template;

/// Free memory (MiB) below which the test is skipped: a second copy must not starve the live apps.
const MIN_FREE_MIB: u64 = 1024;

/// Days between passing tests of the same app when run from the schedule.
pub const EVERY_DAYS: u64 = 6;

pub fn root(data_dir: &Path) -> PathBuf {
    data_dir.join("restore-test")
}

fn stamp_file(data_dir: &Path, name: &str) -> PathBuf {
    root(data_dir).join(format!("last-{name}"))
}

/// Epoch of the last passing test of `name`.
pub fn last_pass(data_dir: &Path, name: &str) -> Option<u64> {
    std::fs::read_to_string(stamp_file(data_dir, name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Is a scheduled test due for `name`?
pub fn due(data_dir: &Path, name: &str, now: u64) -> bool {
    last_pass(data_dir, name).is_none_or(|t| now.saturating_sub(t) >= EVERY_DAYS * 86_400)
}

fn available_mib() -> Option<u64> {
    let m = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: u64 = m
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kb / 1024)
}

/// Name of the throwaway copy of `name`: valid instance name, never equal to a real one's prefix style.
fn test_name(name: &str) -> String {
    let mut n = format!("rt-{name}");
    n.truncate(31);
    while n.ends_with('-') {
        n.pop();
    }
    n
}

/// Delete a throwaway directory. Files written by containers belong to root, so they are
/// removed from inside a container first; the (now empty) folder is removed last.
async fn wipe(inst: &Instance, root: &Path, image: Option<&str>) {
    // Safety: only ever below `<data>/restore-test/` and only `rt-*` folders.
    let ok = inst.dir.parent() == Some(root)
        && inst
            .dir
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("rt-"));
    if !ok {
        return;
    }
    let _ = runner::compose(inst, &["down", "-v"]).await;
    if let Some(image) = image
        && let Ok(rd) = std::fs::read_dir(&inst.dir)
    {
        let names: Vec<String> = rd
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .map(|n| format!("/d/{n}"))
            .collect();
        if !names.is_empty() {
            let mut cmd = tokio::process::Command::new("docker");
            if let Some(data) = root.parent() {
                cmd.env("DOCKER_CONFIG", data.join("docker-config"));
            }
            let _ = cmd
                .args([
                    "run",
                    "--rm",
                    "--network",
                    "none",
                    "--user",
                    "0",
                    "--entrypoint",
                    "rm",
                ])
                .arg("-v")
                .arg(format!("{}:/d", inst.dir.display()))
                .arg(image)
                .arg("-rf")
                .args(&names)
                .output()
                .await;
        }
    }
    let _ = std::fs::remove_dir_all(&inst.dir);
}

/// Restore the newest snapshot of `live` into a throwaway copy and delete it again.
/// `Ok(None)` = skipped (reason logged), `Ok(Some(snapshot))` = passed.
pub async fn run(
    data_dir: &Path,
    live: &Instance,
    t: &Template,
    log: &mut (dyn FnMut(&str) + Send),
) -> Result<Option<String>, String> {
    if !t.manifest.lan_ports.is_empty() {
        log("skipped: this app uses fixed network ports that a second copy would clash with");
        return Ok(None);
    }
    if let Some(m) = available_mib()
        && m < MIN_FREE_MIB
    {
        log(&format!(
            "skipped: only {m} MiB of memory free (need {MIN_FREE_MIB})"
        ));
        return Ok(None);
    }
    let backups = data_dir.join("backups");
    let snaps = crate::backup::list(&backups, &live.name);
    let snap = snaps
        .iter()
        .rev()
        .find(|p| !p.join(crate::backup::SAFETY_MARK).exists())
        .or(snaps.last())
        .ok_or("no snapshot to test")?
        .clone();
    let snap_name = snap
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("bad snapshot name")?
        .to_string();

    let root = root(data_dir);
    let tname = test_name(&live.name);
    if !runner::valid_name(&tname) {
        return Err(format!("cannot name a test copy of {}", live.name));
    }
    let dir = root.join(&tname);
    let inst = Instance {
        name: tname.clone(),
        dir: dir.clone(),
    };
    // A crashed earlier test may have left its copy behind.
    if dir.exists() {
        log("removing a leftover test copy");
        wipe(&inst, &root, None).await;
        if dir.exists() {
            return Err(format!("cannot clear {}", dir.display()));
        }
    }
    let _ = std::fs::remove_dir_all(root.join("backups").join(&tname));

    // Throwaway copy: same files, own name and ports.
    let setup = || -> Result<(), String> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o750)
            .create(&dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        for f in [
            "compose.yaml",
            "compose.override.yaml",
            crate::limits::YAML,
            crate::logcap::YAML,
            "state.toml",
        ] {
            let from = live.dir.join(f);
            if from.is_file() {
                std::fs::copy(&from, dir.join(f)).map_err(|e| format!("{f}: {e}"))?;
            }
        }
        let mut env = runner::read_env(&live.dir)?;
        let mut taken = runner::claimed_ports(live.dir.parent().unwrap_or(&live.dir));
        for (k, v) in env.iter_mut() {
            if k.ends_with("_PORT") && v.parse::<u16>().is_ok() {
                let p = crate::install::allocate(&taken, &crate::install::port_is_free)?;
                taken.insert(p);
                *v = p.to_string();
            }
        }
        env.insert("INSTANCE_NAME".into(), tname.clone());
        crate::install::write_env(&dir, &env).map_err(|e| format!(".env: {e}"))?;
        let link_dir = root.join("backups").join(&tname);
        std::fs::create_dir_all(&link_dir).map_err(|e| format!("{}: {e}", link_dir.display()))?;
        std::os::unix::fs::symlink(&snap, link_dir.join(&snap_name))
            .map_err(|e| format!("snapshot link: {e}"))
    };
    if let Err(e) = setup() {
        wipe(&inst, &root, None).await;
        let _ = std::fs::remove_dir_all(root.join("backups").join(&tname));
        return Err(e);
    }

    log(&format!(
        "restoring snapshot {snap_name} into a throwaway copy ({tname})"
    ));
    let image = runner::compose(&inst, &["config", "--images"])
        .await
        .ok()
        .and_then(|o| {
            o.lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(String::from)
        });
    let result = crate::backup::restore(
        &root.join("backups"),
        &inst,
        t,
        Some(&snap_name),
        false,
        log,
    )
    .await;
    log("deleting the throwaway copy");
    wipe(&inst, &root, image.as_deref()).await;
    let _ = std::fs::remove_dir_all(root.join("backups").join(&tname));
    result.map_err(|e| format!("restore test of snapshot {snap_name} failed: {e}"))?;
    let now = crate::schedule::now();
    let _ = std::fs::write(stamp_file(data_dir, &live.name), now.to_string());
    Ok(Some(snap_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_names_are_valid_and_distinct() {
        for n in ["gitea", "ab", "a-very-long-instance-name-of-31ch", "x-"] {
            let t = test_name(n);
            assert!(runner::valid_name(&t), "{t}");
            assert_ne!(t, n);
        }
    }

    #[test]
    fn due_after_six_days() {
        let d =
            std::env::temp_dir().join(format!("dockwren-rt-{}", crate::auth::generate_password()));
        std::fs::create_dir_all(root(&d)).unwrap();
        assert!(due(&d, "app", 1_000_000));
        std::fs::write(stamp_file(&d, "app"), "1000000").unwrap();
        assert!(!due(&d, "app", 1_000_000 + 5 * 86_400));
        assert!(due(&d, "app", 1_000_000 + 6 * 86_400));
        let _ = std::fs::remove_dir_all(&d);
    }
}
