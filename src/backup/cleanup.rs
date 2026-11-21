//! Removing the leftovers `restore` leaves behind.

use super::*;

/// Is `n` a leftover directory name made by `restore` (`<path>.pre-restore-<epoch>` or `.failed-restore-<epoch>`)?
pub(super) fn is_restore_leftover(n: &str) -> bool {
    ["pre-restore", "failed-restore"].iter().any(|k| {
        n.split_once(&format!(".{k}-")).is_some_and(|(base, num)| {
            !base.is_empty() && !base.contains('/') && is_snapshot_name(num)
        })
    }) && !n.contains('/')
}

/// Names of the restore leftovers in an instance dir, sorted.
pub fn restore_leftover_names(dir: &Path) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| is_restore_leftover(n))
        .collect();
    names.sort();
    Ok(names)
}

/// Delete the `*.pre-restore-*` / `*.failed-restore-*` directories `restore` leaves in the
/// instance dir (they are root-owned, so a throwaway container removes them). Returns the names removed.
pub async fn cleanup_restore_leftovers(
    inst: &Instance,
    log: &mut (dyn FnMut(&str) + Send),
) -> Result<Vec<String>, String> {
    let dir =
        std::fs::canonicalize(&inst.dir).map_err(|e| format!("{}: {e}", inst.dir.display()))?;
    let names = restore_leftover_names(&dir)?;
    if names.is_empty() {
        return Ok(names);
    }
    let image = first_image(inst).await?;
    for n in &names {
        log(&format!("removing {n}"));
        let out = docker_cmd(inst)
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
            .arg(format!("{}:/w", dir.display()))
            .args([image.as_str(), "-rf", "--"])
            .arg(format!("/w/{n}"))
            .output()
            .await
            .map_err(|e| format!("cannot run docker: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "removing {n}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }
    Ok(names)
}

pub async fn cleanup_instance(
    cfg: &Config,
    name: &str,
    log: &mut (dyn FnMut(&str) + Send),
) -> Result<Vec<String>, String> {
    let dir = cfg.instances_dir().join(name);
    if !dir.join("state.toml").is_file() {
        return Err(format!("no instance named {name}"));
    }
    cleanup_restore_leftovers(
        &Instance {
            name: name.to_string(),
            dir,
        },
        log,
    )
    .await
}
