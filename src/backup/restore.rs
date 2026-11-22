//! Restoring from a snapshot.

use super::*;

/// File inside a snapshot that marks it as a safety snapshot taken by `restore`.
pub const SAFETY_MARK: &str = ".safety";

/// Pick the snapshot to restore: the named one, or the newest regular one
/// (safety snapshots are only chosen by name, or when nothing else exists).
pub(super) fn pick(
    backups_root: &Path,
    instance: &str,
    name: Option<&str>,
) -> Result<PathBuf, String> {
    let all = list(backups_root, instance);
    match name {
        None => all
            .iter()
            .rev()
            .find(|p| !p.join(SAFETY_MARK).exists())
            .or(all.last())
            .cloned()
            .ok_or_else(|| format!("no snapshots of {instance}")),
        Some(n) => all
            .iter()
            .find(|p| p.file_name().is_some_and(|f| f == n) && is_snapshot_name(n))
            .cloned()
            .ok_or_else(|| {
                let have: Vec<String> = all
                    .iter()
                    .filter_map(|p| p.file_name()?.to_str().map(String::from))
                    .collect();
                format!("no snapshot {n} of {instance} (have: {})", have.join(", "))
            }),
    }
}

/// Restore `inst` from a snapshot (newest when `name` is None). Nothing is deleted:
/// current data is renamed to `<path>.pre-restore-<epoch>`, and a safety snapshot of
/// the current state is taken first unless `safety` is false.
pub async fn restore(
    backups_root: &Path,
    inst: &Instance,
    t: &Template,
    name: Option<&str>,
    safety: bool,
    log: &mut (dyn FnMut(&str) + Send),
) -> Result<PathBuf, String> {
    let b = &t.manifest.backup;
    let snap = pick(backups_root, &inst.name, name)?;
    for d in &b.dumps {
        if !snap.join(&d.output).is_file() {
            return Err(format!("snapshot is missing {}", d.output));
        }
        if d.restore.is_empty() {
            return Err(format!("this app has no restore command for {}", d.output));
        }
    }
    let paths = archived_paths(&snap, &b.paths);
    if !paths.is_empty() && !snap.join("files.tar.gz").is_file() {
        return Err("snapshot is missing files.tar.gz".into());
    }
    let vols = snapshot_volumes(&snap);
    let vol_names: Vec<(String, String)> = if vols.is_empty() {
        Vec::new()
    } else {
        let all = compose_volumes(inst).await?;
        let mut v = Vec::new();
        for k in &vols {
            let (_, n) = all.iter().find(|(key, _)| key == k).ok_or_else(|| {
                format!("the snapshot holds volume {k}, which the compose file no longer declares")
            })?;
            v.push((k.clone(), n.clone()));
        }
        v
    };
    let binds = snapshot_binds(&snap)?;
    if !binds.is_empty() {
        if !is_adopted(inst) {
            return Err(
                "the snapshot holds host folders, which only the adopted stack itself can restore"
                    .into(),
            );
        }
        let current = compose_binds(inst).await?;
        if let Some((_, src)) = binds.iter().find(|(_, src)| !current.contains(src)) {
            return Err(format!(
                "the snapshot holds the folder {src}, which the compose file no longer mounts"
            ));
        }
    }
    if safety {
        log("safety snapshot of the current state");
        // keep = existing + 1, so this cannot prune the snapshot being restored
        let keep = list(backups_root, &inst.name).len() + 1;
        let s = snapshot_keep(backups_root, inst, t, log, keep)
            .await
            .map_err(|e| format!("safety snapshot failed, nothing was changed: {e} (--no-safety-snapshot skips it)"))?;
        let _ = std::fs::write(s.join(SAFETY_MARK), b"");
        log(&format!("safety snapshot in {}", s.display()));
    }
    let image = first_image(inst).await?;
    let dir =
        std::fs::canonicalize(&inst.dir).map_err(|e| format!("{}: {e}", inst.dir.display()))?;
    log("stopping the instance");
    runner::take_down(inst).await?;

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    let undo = |moved: &[(PathBuf, PathBuf)]| {
        for (orig, aside) in moved.iter().rev() {
            if orig.exists() {
                let _ = std::fs::rename(orig, format!("{}.failed-restore-{stamp}", orig.display()));
            }
            let _ = std::fs::rename(aside, orig);
        }
    };
    for p in &paths {
        let orig = inst.dir.join(p);
        if orig.exists() {
            let aside = PathBuf::from(format!("{}.pre-restore-{stamp}", orig.display()));
            if let Err(e) = std::fs::rename(&orig, &aside) {
                undo(&moved);
                return Err(format!("cannot move {p} aside: {e}"));
            }
            log(&format!("moved {p} to {}", aside.display()));
            moved.push((orig, aside));
        }
    }
    if !paths.is_empty() {
        log("extracting files");
        let archive = std::fs::File::open(snap.join("files.tar.gz"))
            .map_err(|e| format!("files.tar.gz: {e}"))?;
        let out = docker_cmd(inst)
            .args([
                "run",
                "--rm",
                "-i",
                "--network",
                "none",
                "--user",
                "0",
                "--entrypoint",
                "tar",
            ])
            .arg("-v")
            .arg(format!("{}:/dst", dir.display()))
            .args([image.as_str(), "--numeric-owner", "-xzf", "-", "-C", "/dst"])
            .stdin(std::process::Stdio::from(archive))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run docker: {e}"))?
            .wait_with_output()
            .await
            .map_err(|e| format!("docker: {e}"))?;
        if !out.status.success() {
            undo(&moved);
            let err = String::from_utf8_lossy(&out.stderr);
            let tail: Vec<&str> = err.lines().take(5).collect();
            return Err(format!(
                "extracting failed, previous files put back: {}",
                tail.join("; ")
            ));
        }
    }
    if !vol_names.is_empty() {
        for (key, name) in &vol_names {
            log(&format!("restoring volume {key}"));
            restore_volume(
                inst,
                &image,
                name,
                &snap.join(format!("{VOL_PREFIX}{key}{VOL_SUFFIX}")),
            )
            .await
            .inspect_err(|_| undo(&moved))?;
        }
    }
    for (key, src) in &binds {
        log(&format!("restoring host folder {src}"));
        restore_bind(
            inst,
            &image,
            src,
            &snap.join(format!("{BIND_PREFIX}{key}{VOL_SUFFIX}")),
            stamp,
        )
        .await
        .inspect_err(|_| undo(&moved))?;
        log(&format!(
            "the previous content is in {src}.pre-restore-{stamp}"
        ));
    }
    // Database dumps go in BEFORE the app starts: an app that reads its database at start-up
    // (users, settings) must not meet an empty one. Only the services that hold a dump come up
    // first; a database that was never initialised is created by its own image on this first start.
    if !b.dumps.is_empty() {
        let mut svcs: Vec<&str> = Vec::new();
        for d in &b.dumps {
            if !svcs.contains(&d.service.as_str()) {
                svcs.push(d.service.as_str());
            }
        }
        log(&format!("starting {}", svcs.join(", ")));
        let up: Vec<&str> = ["up", "-d"]
            .into_iter()
            .chain(svcs.iter().copied())
            .collect();
        runner::compose(inst, &up).await?;
        runner::wait_healthy_services(inst, &svcs, std::time::Duration::from_secs(240)).await?;
        for d in &b.dumps {
            log(&format!("restoring {} into {}", d.output, d.service));
            let f = std::fs::File::open(snap.join(&d.output))
                .map_err(|e| format!("{}: {e}", d.output))?;
            let args: Vec<&str> = ["exec", "-T", d.service.as_str()]
                .into_iter()
                .chain(d.restore.iter().map(String::as_str))
                .collect();
            runner::compose_with_stdin(inst, &args, Some(f))
                .await
                .map_err(|e| format!("restore {}: {e}", d.output))?;
        }
    }
    log("starting");
    runner::compose(inst, &["up", "-d"]).await?;
    runner::wait_healthy(inst, std::time::Duration::from_secs(240)).await?;
    Ok(snap)
}
