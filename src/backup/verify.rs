//! Checking a snapshot without touching the running app.

use super::*;

/// File inside a snapshot that records when `verify` last passed (epoch seconds).
pub const VERIFIED_MARK: &str = ".verified";

/// Epoch of the last successful `verify` of a snapshot directory, if any.
pub fn verified_at(snap: &Path) -> Option<u64> {
    std::fs::read_to_string(snap.join(VERIFIED_MARK))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Does a tar member name equal `want` or sit below it? Tolerates `./` prefixes and trailing slashes.
pub(super) fn tar_entry_matches(entry: &str, want: &str) -> bool {
    let e = entry.trim_start_matches("./").trim_end_matches('/');
    let w = want.trim_start_matches("./").trim_end_matches('/');
    e == w || e.strip_prefix(w).is_some_and(|r| r.starts_with('/'))
}

/// Check a snapshot without touching the running app: every dump is present and
/// non-empty and has a restore command, and `files.tar.gz` reads end to end
/// (gzip checksum and tar structure) and holds every path the template backs up.
/// Records the time in the snapshot on success. Returns the snapshot directory.
pub async fn verify(
    backups_root: &Path,
    inst: &Instance,
    t: &Template,
    name: Option<&str>,
    log: &mut (dyn FnMut(&str) + Send),
) -> Result<PathBuf, String> {
    use std::collections::HashSet;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};

    let b = &t.manifest.backup;
    let snap = pick(backups_root, &inst.name, name)?;
    log(&format!(
        "checking snapshot {}",
        snap.file_name().and_then(|n| n.to_str()).unwrap_or("?")
    ));
    for d in &b.dumps {
        let len = std::fs::metadata(snap.join(&d.output))
            .map_err(|_| format!("snapshot is missing {}", d.output))?
            .len();
        if len == 0 {
            return Err(format!("{} is empty", d.output));
        }
        if d.restore.is_empty() {
            return Err(format!("this app has no restore command for {}", d.output));
        }
        log(&format!("{} ok ({len} bytes)", d.output));
    }
    if !b.paths.is_empty() {
        let tar = snap.join("files.tar.gz");
        let archive = std::fs::File::open(&tar).map_err(|_| "snapshot is missing files.tar.gz")?;
        log("reading files.tar.gz end to end");
        let image = first_image(inst).await?;
        let mut child = docker_cmd(inst)
            .args([
                "run",
                "--rm",
                "-i",
                "--network",
                "none",
                "--entrypoint",
                "tar",
                image.as_str(),
                "-tzf",
                "-",
            ])
            .stdin(std::process::Stdio::from(archive))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run docker: {e}"))?;
        let mut lines = tokio::io::BufReader::new(child.stdout.take().ok_or("no stdout")?).lines();
        let mut found: HashSet<&str> = HashSet::new();
        let mut count = 0u64;
        while let Some(line) = lines.next_line().await.map_err(|e| format!("tar: {e}"))? {
            count += 1;
            for p in &b.paths {
                if !found.contains(p.as_str()) && tar_entry_matches(&line, p) {
                    found.insert(p.as_str());
                }
            }
        }
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut err).await;
        }
        let status = child.wait().await.map_err(|e| format!("docker: {e}"))?;
        if !status.success() {
            let tail: Vec<&str> = err.lines().take(3).collect();
            return Err(format!("files.tar.gz is damaged: {}", tail.join("; ")));
        }
        if let Some(p) = b.paths.iter().find(|p| !found.contains(p.as_str())) {
            return Err(format!("files.tar.gz does not contain {p}"));
        }
        log(&format!("files.tar.gz ok ({count} entries)"));
    }
    let mut archives: Vec<String> = snapshot_volumes(&snap)
        .into_iter()
        .map(|k| format!("{VOL_PREFIX}{k}{VOL_SUFFIX}"))
        .collect();
    archives.extend(
        snapshot_binds(&snap)?
            .into_iter()
            .map(|(k, _)| format!("{BIND_PREFIX}{k}{VOL_SUFFIX}")),
    );
    for file in archives {
        log(&format!("reading {file} end to end"));
        let archive = std::fs::File::open(snap.join(&file)).map_err(|e| format!("{file}: {e}"))?;
        let image = first_image(inst).await?;
        let out = docker_cmd(inst)
            .args([
                "run",
                "--rm",
                "-i",
                "--network",
                "none",
                "--entrypoint",
                "tar",
                image.as_str(),
                "-tzf",
                "-",
            ])
            .stdin(std::process::Stdio::from(archive))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(|e| format!("cannot run docker: {e}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let tail: Vec<&str> = err.lines().take(3).collect();
            return Err(format!("{file} is damaged: {}", tail.join("; ")));
        }
        let count = out.stdout.iter().filter(|&&b| b == b'\n').count();
        log(&format!("{file} ok ({count} entries)"));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    let _ = std::fs::write(snap.join(VERIFIED_MARK), now.to_string());
    Ok(snap)
}

/// CLI entry for `verify`.
pub async fn verify_instance(
    cfg: &Config,
    templates: &[Template],
    name: &str,
    snap: Option<&str>,
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
    verify(
        &cfg.data_dir.join("backups"),
        &Instance {
            name: name.to_string(),
            dir,
        },
        t,
        snap,
        log,
    )
    .await
}

/// CLI entry for `restore`.
pub async fn restore_instance(
    cfg: &Config,
    templates: &[Template],
    name: &str,
    snap: Option<&str>,
    safety: bool,
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
    restore(
        &cfg.data_dir.join("backups"),
        &Instance {
            name: name.to_string(),
            dir,
        },
        t,
        snap,
        safety,
        log,
    )
    .await
}
