//! Duplicate an app: restore one of its snapshots into a new instance with its own name and
//! ports, and keep it. Same mechanics as the restore test (`restoretest`), but the copy lives
//! in `instances/` next to the original and is not deleted. The original is never touched.
//!
//! The copy has the original's data, accounts and passwords. It gets no public hostname: the
//! app's own settings (a site URL, a domain) may still name the original's address.

use std::path::Path;

use crate::runner::{self, Instance};
use crate::template::Template;

/// Duplicate `live` from `snap` (newest snapshot when None) as `new_name`; returns the snapshot used.
pub async fn run(
    data_dir: &Path,
    live: &Instance,
    t: &Template,
    new_name: &str,
    snap: Option<&str>,
    log: &mut (dyn FnMut(&str) + Send),
) -> Result<String, String> {
    if !runner::valid_name(new_name) {
        return Err("the new name must be 2 to 31 characters: lower-case letters, digits and hyphens, starting with a letter".into());
    }
    let instances = data_dir.join("instances");
    let dir = instances.join(new_name);
    if live.dir.parent() != Some(instances.as_path()) {
        return Err("unexpected instance location".into());
    }
    if dir.exists() || data_dir.join("backups").join(new_name).exists() {
        return Err(format!(
            "{new_name} already exists (or has old snapshots); pick another name"
        ));
    }
    if !t.manifest.lan_ports.is_empty() {
        return Err("this app uses fixed network ports that a second copy would clash with".into());
    }
    if let Some(m) = crate::restoretest::available_mib()
        && m < crate::restoretest::MIN_FREE_MIB
    {
        return Err(format!(
            "only {m} MiB of memory free (need {}): a second copy could starve the running apps",
            crate::restoretest::MIN_FREE_MIB
        ));
    }
    let backups = data_dir.join("backups");
    let snaps = crate::backup::list(&backups, &live.name);
    let snap_dir = match snap {
        Some(n) => snaps
            .iter()
            .find(|p| p.file_name().is_some_and(|f| f == n))
            .ok_or_else(|| format!("no snapshot {n} of {}", live.name))?,
        None => snaps
            .iter()
            .rev()
            .find(|p| !p.join(crate::backup::SAFETY_MARK).exists())
            .or(snaps.last())
            .ok_or("no snapshot to copy from: take a backup first")?,
    }
    .clone();
    let snap_name = snap_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("bad snapshot name")?
        .to_string();

    let inst = Instance {
        name: new_name.to_string(),
        dir: dir.clone(),
    };
    let tmp_root = data_dir.join("clone-tmp");
    let link_dir = tmp_root.join("backups").join(new_name);
    let setup = || -> Result<(), String> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
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
        let mut taken = runner::claimed_ports(&instances);
        for (k, v) in env.iter_mut() {
            if k.ends_with("_PORT") && v.parse::<u16>().is_ok() {
                let p = crate::install::allocate(&taken, &crate::install::port_is_free)?;
                taken.insert(p);
                *v = p.to_string();
            }
        }
        env.insert("INSTANCE_NAME".into(), new_name.to_string());
        crate::install::write_env(&dir, &env).map_err(|e| format!(".env: {e}"))?;
        std::fs::create_dir_all(&link_dir).map_err(|e| format!("{}: {e}", link_dir.display()))?;
        std::os::unix::fs::symlink(&snap_dir, link_dir.join(&snap_name))
            .map_err(|e| format!("snapshot link: {e}"))
    };
    if let Err(e) = setup() {
        crate::restoretest::wipe(&inst, &instances, new_name, None).await;
        let _ = std::fs::remove_dir_all(&link_dir);
        return Err(e);
    }
    log(&format!(
        "copying snapshot {snap_name} of {} into {new_name}",
        live.name
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
        &tmp_root.join("backups"),
        &inst,
        t,
        Some(&snap_name),
        false,
        log,
    )
    .await;
    let _ = std::fs::remove_dir_all(&link_dir);
    if let Err(e) = result {
        log("removing the failed copy");
        crate::restoretest::wipe(&inst, &instances, new_name, image.as_deref()).await;
        return Err(format!(
            "could not duplicate from snapshot {snap_name}: {e}"
        ));
    }
    log(
        "done. The copy has its own ports and no public hostname. Settings inside the app (site URL, domain) may still name the original address.",
    );
    Ok(snap_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dockwren-clone-{tag}-{}",
            crate::auth::generate_password()
        ));
        std::fs::create_dir_all(d.join("instances/orig")).unwrap();
        d
    }

    #[tokio::test]
    async fn refuses_bad_or_taken_names() {
        let d = tmp("names");
        std::fs::create_dir_all(d.join("instances/taken")).unwrap();
        let templates = crate::template::catalog(None).unwrap();
        let t = templates
            .iter()
            .find(|t| t.manifest.lan_ports.is_empty())
            .unwrap();
        let live = Instance {
            name: "orig".into(),
            dir: d.join("instances/orig"),
        };
        let mut log = |_: &str| {};
        for bad in ["A", "x", "-a", "a b", "a/b"] {
            assert!(
                run(&d, &live, t, bad, None, &mut log).await.is_err(),
                "{bad}"
            );
        }
        let e = run(&d, &live, t, "taken", None, &mut log)
            .await
            .unwrap_err();
        assert!(e.contains("already exists"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
