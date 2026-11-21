//! Taking snapshots.

use super::*;

/// Take one snapshot of `inst` described by `t`. Returns the snapshot directory.
pub async fn snapshot(
    backups_root: &Path,
    inst: &Instance,
    t: &Template,
    log: &mut (dyn FnMut(&str) + Send),
) -> Result<PathBuf, String> {
    snapshot_keep(backups_root, inst, t, log, KEEP).await
}

/// `snapshot` with an explicit number of snapshots to keep afterwards.
pub async fn snapshot_keep(
    backups_root: &Path,
    inst: &Instance,
    t: &Template,
    log: &mut (dyn FnMut(&str) + Send),
    keep: usize,
) -> Result<PathBuf, String> {
    let b = &t.manifest.backup;
    if b.paths.is_empty() && b.dumps.is_empty() {
        return Err("this app declares nothing to back up".into());
    }
    for p in &b.paths {
        if !inst.dir.join(p).exists() {
            return Err(format!(
                "backup path {p} does not exist in {}",
                inst.dir.display()
            ));
        }
    }
    let parent = backups_root.join(&inst.name);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&parent)
        .map_err(|e| format!("{}: {e}", parent.display()))?;
    let mut secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    // Ids are epoch seconds; if one already exists (backup right after backup/restore), take the next free second.
    while parent.join(secs.to_string()).exists() || parent.join(format!("{secs}.partial")).exists()
    {
        secs += 1;
    }
    let tmp = parent.join(format!("{secs}.partial"));
    let fin = parent.join(secs.to_string());
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&tmp)
        .map_err(|e| format!("{}: {e}", tmp.display()))?;

    // On failure the partial dir is left for inspection; it is never listed or pruned.
    for d in &b.dumps {
        log(&format!("dumping {} ({})", d.output, d.service));
        let args: Vec<&str> = ["exec", "-T", d.service.as_str()]
            .into_iter()
            .chain(d.run.iter().map(String::as_str))
            .collect();
        let out = runner::compose(inst, &args)
            .await
            .map_err(|e| format!("dump {}: {e}", d.output))?;
        if out.trim().is_empty() {
            return Err(format!(
                "dump {} produced no output (partial snapshot in {})",
                d.output,
                tmp.display()
            ));
        }
        write_private(&tmp.join(&d.output), out.as_bytes())
            .map_err(|e| format!("{}: {e}", d.output))?;
    }
    if !b.paths.is_empty() {
        log("archiving files");
        let image = first_image(inst).await?;
        let dir =
            std::fs::canonicalize(&inst.dir).map_err(|e| format!("{}: {e}", inst.dir.display()))?;
        let archive = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(tmp.join("files.tar.gz"))
            .map_err(|e| format!("files.tar.gz: {e}"))?;
        // Root inside a throwaway container so root-owned app data is readable;
        // the archive streams out through stdout, so the file stays ours.
        let mut cmd = Command::new("docker");
        if std::env::var_os("DOCKER_CONFIG").is_none()
            && let Some(data) = inst.dir.parent().and_then(Path::parent)
        {
            cmd.env("DOCKER_CONFIG", data.join("docker-config"));
        }
        let out = cmd
            .args([
                "run",
                "--rm",
                "--network",
                "none",
                "--user",
                "0",
                "--entrypoint",
                "tar",
            ])
            .arg("-v")
            .arg(format!("{}:/src:ro", dir.display()))
            .args(["-w", "/src", &image, "--numeric-owner", "-czf", "-", "--"])
            .args(&b.paths)
            .stdout(std::process::Stdio::from(archive))
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run docker: {e}"))?
            .wait_with_output()
            .await
            .map_err(|e| format!("docker: {e}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let tail: Vec<&str> = err.lines().take(5).collect();
            return Err(format!(
                "archiving failed (partial snapshot in {}): {}",
                tmp.display(),
                tail.join("; ")
            ));
        }
        if std::fs::metadata(tmp.join("files.tar.gz"))
            .map(|m| m.len())
            .unwrap_or(0)
            == 0
        {
            return Err(format!(
                "archive is empty (partial snapshot in {})",
                tmp.display()
            ));
        }
    }
    if is_adopted(inst) {
        snapshot_volumes_into(inst, &tmp, log).await?;
    }
    std::fs::rename(&tmp, &fin).map_err(|e| format!("finalise snapshot: {e}"))?;
    let pruned = prune(backups_root, &inst.name, keep)?;
    if pruned > 0 {
        log(&format!("pruned {pruned} old snapshot(s), keeping {keep}"));
    }
    Ok(fin)
}

/// CLI entry: look the instance up, find its template and snapshot it.
pub async fn snapshot_instance(
    cfg: &Config,
    templates: &[Template],
    name: &str,
    log: &mut (dyn FnMut(&str) + Send),
) -> Result<PathBuf, String> {
    let dir = cfg.instances_dir().join(name);
    let state = std::fs::read_to_string(dir.join("state.toml"))
        .map_err(|_| format!("no instance named {name}"))?;
    let state: State = toml::from_str(&state).map_err(|e| format!("state.toml: {e}"))?;
    let t = templates
        .iter()
        .find(|t| t.manifest.app.id == state.template)
        .ok_or_else(|| format!("template {} is not available", state.template))?;
    snapshot(
        &cfg.data_dir.join("backups"),
        &Instance {
            name: name.to_string(),
            dir,
        },
        t,
        log,
    )
    .await
}
