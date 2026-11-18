//! Instance snapshots: run the template's `backup.dumps`, tar its `backup.paths`
//! and keep the newest few under `<data>/backups/<instance>/<epoch-seconds>/`.
//! The nightly restic job picks the whole data dir up, so snapshots end up
//! on the backup disk too.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tokio::process::Command;

use crate::config::Config;
use crate::runner::{self, Instance};
use crate::template::Template;

/// Snapshots kept per instance; older ones are pruned after a successful run.
pub const KEEP: usize = 7;

#[derive(Deserialize)]
struct State {
    template: String,
}

fn write_private(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(content)
}

/// Snapshot directory names are plain epoch seconds: sortable, no time crate needed.
pub fn is_snapshot_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 12 && n.bytes().all(|b| b.is_ascii_digit())
}

/// Existing snapshots of an instance, oldest first.
pub fn list(backups_root: &Path, instance: &str) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(backups_root.join(instance)) else {
        return Vec::new();
    };
    let mut v: Vec<(u64, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().into_string().ok()?;
            if !is_snapshot_name(&n) {
                return None;
            }
            Some((n.parse().ok()?, e.path()))
        })
        .collect();
    v.sort();
    v.into_iter().map(|(_, p)| p).collect()
}

/// Remove all but the newest `keep` snapshots. Only directories that `list`
/// returned (digits-only names inside this instance's backup dir) are touched.
pub fn prune(backups_root: &Path, instance: &str, keep: usize) -> Result<usize, String> {
    let all = list(backups_root, instance);
    let old = all.len().saturating_sub(keep);
    for p in &all[..old] {
        std::fs::remove_dir_all(p).map_err(|e| format!("prune {}: {e}", p.display()))?;
    }
    Ok(old)
}

/// An image of the instance that is already pulled; it only supplies `tar`.
async fn first_image(inst: &Instance) -> Result<String, String> {
    let out = runner::compose(inst, &["config", "--images"]).await?;
    out.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(String::from)
        .ok_or_else(|| "instance has no images".into())
}

/// Snapshot file holding one named volume of an adopted stack: `vol-<compose key>.tar.gz`.
const VOL_PREFIX: &str = "vol-";
const VOL_SUFFIX: &str = ".tar.gz";

/// Adopted stacks keep their data in named volumes, which `backup.paths` cannot reach.
fn is_adopted(inst: &Instance) -> bool {
    inst.dir.join(runner::PROJECT_FILE).is_file()
}

/// Compose volume keys are letters, digits, `.`, `_`, `-`; anything else never reaches a file name.
fn valid_volume_key(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 64
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Volume keys stored in a snapshot (sorted).
fn snapshot_volumes(snap: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(snap)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|n| {
            n.strip_prefix(VOL_PREFIX)
                .and_then(|r| r.strip_suffix(VOL_SUFFIX))
                .filter(|k| valid_volume_key(k))
                .map(String::from)
        })
        .collect();
    v.sort();
    v
}

/// Pulls `(compose key, Docker volume name)` of every non-external named volume out of the
/// JSON of `docker compose config`.
fn parse_volumes(config_json: &str) -> Result<Vec<(String, String)>, String> {
    let v: serde_json::Value =
        serde_json::from_str(config_json).map_err(|e| format!("compose config: {e}"))?;
    let mut out = Vec::new();
    if let Some(map) = v.get("volumes").and_then(|m| m.as_object()) {
        for (key, def) in map {
            if def
                .get("external")
                .is_some_and(|e| e.as_bool() != Some(false) && !e.is_null())
            {
                continue;
            }
            let Some(name) = def.get("name").and_then(|n| n.as_str()) else {
                continue;
            };
            if !valid_volume_key(key) {
                return Err(format!("volume name {key} is not supported for backups"));
            }
            out.push((key.clone(), name.to_string()));
        }
    }
    out.sort();
    Ok(out)
}

async fn compose_volumes(inst: &Instance) -> Result<Vec<(String, String)>, String> {
    parse_volumes(&runner::compose(inst, &["config", "--format", "json"]).await?)
}

async fn volume_exists(inst: &Instance, name: &str) -> bool {
    docker_cmd(inst)
        .args(["volume", "inspect", name])
        .output()
        .await
        .is_ok_and(|o| o.status.success())
}

/// Archives one volume into `dest` through a throwaway root container (read-only mount).
async fn archive_volume(
    inst: &Instance,
    image: &str,
    volume: &str,
    dest: &Path,
) -> Result<(), String> {
    let archive = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dest)
        .map_err(|e| format!("{}: {e}", dest.display()))?;
    let out = docker_cmd(inst)
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
        .arg(format!("{volume}:/src:ro"))
        .args(["-w", "/src", image, "--numeric-owner", "-czf", "-", "."])
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
            "archiving volume {volume} failed: {}",
            tail.join("; ")
        ));
    }
    if std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0) == 0 {
        return Err(format!("archive of volume {volume} is empty"));
    }
    Ok(())
}

/// Archives every named volume of an adopted stack. The stack is stopped for the copy (a
/// running database would give an inconsistent file) and started again whatever happens.
async fn snapshot_volumes_into(
    inst: &Instance,
    tmp: &Path,
    log: &mut (dyn FnMut(&str) + Send),
) -> Result<(), String> {
    let mut vols = Vec::new();
    for (key, name) in compose_volumes(inst).await? {
        if volume_exists(inst, &name).await {
            vols.push((key, name));
        }
    }
    if vols.is_empty() {
        return Ok(());
    }
    let image = first_image(inst).await?;
    log("stopping the stack for a consistent copy of its volumes");
    runner::compose(inst, &["stop"]).await?;
    let mut result = Ok(());
    for (key, name) in &vols {
        log(&format!("archiving volume {key}"));
        let dest = tmp.join(format!("{VOL_PREFIX}{key}{VOL_SUFFIX}"));
        if let Err(e) = archive_volume(inst, &image, name, &dest).await {
            result = Err(e);
            break;
        }
    }
    log("starting the stack again");
    let started = runner::compose(inst, &["start"]).await;
    result?;
    started.map(|_| ())
}

/// Empties volume `name` (it is removed and created again by Compose) and unpacks `archive`.
async fn restore_volume(
    inst: &Instance,
    image: &str,
    name: &str,
    archive: &Path,
) -> Result<(), String> {
    let file = std::fs::File::open(archive).map_err(|e| format!("{}: {e}", archive.display()))?;
    // Not present yet in a throwaway copy; that is fine.
    let _ = docker_cmd(inst).args(["volume", "rm", name]).output().await;
    // Lets Compose create the volume with its own labels.
    runner::compose(inst, &["up", "--no-start"]).await?;
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
        .arg(format!("{name}:/dst"))
        .args([image, "--numeric-owner", "-xzf", "-", "-C", "/dst"])
        .stdin(std::process::Stdio::from(file))
        .stdout(std::process::Stdio::null())
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
            "extracting volume {name} failed (restore from the safety snapshot): {}",
            tail.join("; ")
        ));
    }
    Ok(())
}

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

fn docker_cmd(inst: &Instance) -> Command {
    let mut cmd = Command::new("docker");
    if std::env::var_os("DOCKER_CONFIG").is_none()
        && let Some(data) = inst.dir.parent().and_then(Path::parent)
    {
        cmd.env("DOCKER_CONFIG", data.join("docker-config"));
    }
    cmd
}

/// File inside a snapshot that marks it as a safety snapshot taken by `restore`.
pub const SAFETY_MARK: &str = ".safety";

/// Pick the snapshot to restore: the named one, or the newest regular one
/// (safety snapshots are only chosen by name, or when nothing else exists).
fn pick(backups_root: &Path, instance: &str, name: Option<&str>) -> Result<PathBuf, String> {
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
    if !b.paths.is_empty() && !snap.join("files.tar.gz").is_file() {
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
    for p in &b.paths {
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
    if !b.paths.is_empty() {
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
    log("starting");
    runner::compose(inst, &["up", "-d"]).await?;
    runner::wait_healthy(inst, std::time::Duration::from_secs(240)).await?;
    for d in &b.dumps {
        log(&format!("restoring {} into {}", d.output, d.service));
        let f =
            std::fs::File::open(snap.join(&d.output)).map_err(|e| format!("{}: {e}", d.output))?;
        let args: Vec<&str> = ["exec", "-T", d.service.as_str()]
            .into_iter()
            .chain(d.restore.iter().map(String::as_str))
            .collect();
        runner::compose_with_stdin(inst, &args, Some(f))
            .await
            .map_err(|e| format!("restore {}: {e}", d.output))?;
    }
    if !b.dumps.is_empty() {
        log("restarting so the app sees the restored database");
        runner::compose(inst, &["restart"]).await?;
        runner::wait_healthy(inst, std::time::Duration::from_secs(240)).await?;
    }
    Ok(snap)
}

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
fn tar_entry_matches(entry: &str, want: &str) -> bool {
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
    for key in snapshot_volumes(&snap) {
        let file = format!("{VOL_PREFIX}{key}{VOL_SUFFIX}");
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

/// Is `n` a leftover directory name made by `restore` (`<path>.pre-restore-<epoch>` or `.failed-restore-<epoch>`)?
fn is_restore_leftover(n: &str) -> bool {
    ["pre-restore", "failed-restore"].iter().any(|k| {
        n.split_once(&format!(".{k}-")).is_some_and(|(base, num)| {
            !base.is_empty() && !base.contains('/') && is_snapshot_name(num)
        })
    }) && !n.contains('/')
}

/// Delete the `*.pre-restore-*` / `*.failed-restore-*` directories `restore` leaves in the
/// instance dir (they are root-owned, so a throwaway container removes them). Returns the names removed.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        std::env::temp_dir().join(format!("dockwren-bak-{}", crate::auth::generate_password()))
    }

    #[test]
    fn leftover_names() {
        assert!(is_restore_leftover("data.pre-restore-1794581095"));
        assert!(is_restore_leftover("data.failed-restore-17"));
        for bad in [
            "data",
            "data.pre-restore-",
            "data.pre-restore-x",
            ".pre-restore-1",
            "data.pre-restore-1/../x",
            "compose.yaml",
        ] {
            assert!(!is_restore_leftover(bad), "{bad}");
        }
    }

    #[test]
    fn snapshot_names() {
        assert!(is_snapshot_name("1731500000"));
        for bad in ["", "17.partial", "../1", "a1", "1234567890123"] {
            assert!(!is_snapshot_name(bad), "{bad}");
        }
    }

    #[test]
    fn prune_keeps_newest_and_ignores_foreign_entries() {
        let root = tmp();
        let inst = root.join("app");
        for n in ["100", "300", "200", "400"] {
            std::fs::create_dir_all(inst.join(n)).unwrap();
        }
        std::fs::create_dir_all(inst.join("500.partial")).unwrap();
        std::fs::write(inst.join("notes"), "x").unwrap();
        assert_eq!(prune(&root, "app", 2).unwrap(), 2);
        let left: Vec<String> = list(&root, "app")
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["300", "400"]);
        assert!(inst.join("500.partial").exists() && inst.join("notes").exists());
        assert_eq!(prune(&root, "app", 7).unwrap(), 0);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn pick_defaults_to_newest_and_rejects_unknown_or_tricky_names() {
        let root = tmp();
        for n in ["100", "200"] {
            std::fs::create_dir_all(root.join("app").join(n)).unwrap();
        }
        assert!(pick(&root, "app", None).unwrap().ends_with("200"));
        assert!(pick(&root, "app", Some("100")).unwrap().ends_with("100"));
        for bad in ["300", "../app/100", ""] {
            assert!(
                pick(&root, "app", Some(bad))
                    .unwrap_err()
                    .contains("have: 100, 200"),
                "{bad}"
            );
        }
        std::fs::write(root.join("app/200").join(SAFETY_MARK), b"").unwrap();
        assert!(pick(&root, "app", None).unwrap().ends_with("100"));
        assert!(pick(&root, "app", Some("200")).unwrap().ends_with("200"));
        std::fs::write(root.join("app/100").join(SAFETY_MARK), b"").unwrap();
        assert!(pick(&root, "app", None).unwrap().ends_with("200"));
        assert!(
            pick(&root, "none", None)
                .unwrap_err()
                .contains("no snapshots")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn dot_and_empty_backup_paths_are_rejected_by_templates() {
        for bad in [".", "", "a//b", "./x"] {
            let m =
                format!("[app]\nid=\"x\"\nname=\"X\"\nversion=1\n[backup]\npaths=[\"{bad}\"]\n");
            assert!(
                crate::template::parse(&m, "services: {}\n").is_err(),
                "{bad:?}"
            );
        }
    }

    #[tokio::test]
    async fn snapshot_refuses_missing_paths_without_leaving_files() {
        let t =
            crate::template::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("apps/vaultwarden"))
                .unwrap();
        let root = tmp();
        let inst = Instance {
            name: "vw".into(),
            dir: root.join("instances/vw"),
        };
        std::fs::create_dir_all(&inst.dir).unwrap();
        let backups = root.join("backups");
        let mut l = |_: &str| {};
        let err = snapshot(&backups, &inst, &t, &mut l).await.unwrap_err();
        assert!(err.contains("does not exist"), "{err}");
        assert!(!backups.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn volumes_come_from_compose_config() {
        let j = r#"{"volumes":{"pg":{"name":"proj_pg"},"ext":{"name":"shared","external":true},"b-1.x":{"name":"proj_b"}}}"#;
        assert_eq!(
            parse_volumes(j).unwrap(),
            [
                ("b-1.x".into(), "proj_b".into()),
                ("pg".into(), "proj_pg".into())
            ]
        );
        assert!(parse_volumes(r#"{"services":{}}"#).unwrap().is_empty());
        assert!(parse_volumes(r#"{"volumes":{"a b":{"name":"x"}}}"#).is_err());
        assert!(parse_volumes("nope").is_err());
    }

    #[test]
    fn snapshot_volume_files_are_found_by_name() {
        let d = tmp();
        std::fs::create_dir_all(&d).unwrap();
        for n in [
            "vol-pg.tar.gz",
            "vol-a.b.tar.gz",
            "vol-.tar.gz",
            "vol-x y.tar.gz",
            "files.tar.gz",
            "vol-pg.tar",
        ] {
            std::fs::write(d.join(n), b"x").unwrap();
        }
        assert_eq!(snapshot_volumes(&d), ["a.b", "pg"]);
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// Real Docker: an adopted-style project with a named volume is snapshotted (stopped and
    /// started again), the volume is changed, then restored and verified.
    #[tokio::test]
    async fn adopted_volume_round_trip() {
        let root = tmp();
        let name = format!("vt{}", std::process::id());
        let inst = Instance {
            name: name.clone(),
            dir: root.join("instances").join(&name),
        };
        std::fs::create_dir_all(inst.dir.join("data")).unwrap();
        std::fs::write(
            inst.dir.join("compose.yaml"),
            "services:\n  app:\n    image: alpine:3\n    command: [\"sleep\", \"600\"]\n    volumes: [\"vol:/v\"]\nvolumes:\n  vol:\n",
        )
        .unwrap();
        std::fs::write(inst.dir.join(".env"), "").unwrap();
        std::fs::write(
            inst.dir.join(runner::PROJECT_FILE),
            format!("dwvt-{name}\n"),
        )
        .unwrap();
        let t = crate::template::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("apps/custom"))
            .unwrap();
        let backups = root.join("backups");
        let mut l = |_: &str| {};
        let run = async {
            runner::compose(&inst, &["up", "-d"]).await?;
            runner::compose(
                &inst,
                &["exec", "-T", "app", "sh", "-c", "echo before > /v/f"],
            )
            .await?;
            let snap = snapshot(&backups, &inst, &t, &mut l).await?;
            assert!(snap.join("vol-vol.tar.gz").is_file());
            let state = runner::compose(&inst, &["ps", "--status", "running", "-q"]).await?;
            assert!(!state.trim().is_empty(), "stack must be running again");
            runner::compose(
                &inst,
                &[
                    "exec",
                    "-T",
                    "app",
                    "sh",
                    "-c",
                    "echo after > /v/f; echo x > /v/g",
                ],
            )
            .await?;
            verify(&backups, &inst, &t, None, &mut l).await?;
            restore(&backups, &inst, &t, None, false, &mut l).await?;
            let f = runner::compose(&inst, &["exec", "-T", "app", "sh", "-c", "cat /v/f; ls /v"])
                .await?;
            assert!(f.contains("before") && !f.contains('g'), "{f}");
            Ok::<(), String>(())
        }
        .await;
        let _ = runner::compose(&inst, &["down", "-v"]).await;
        let _ = std::process::Command::new("docker")
            .args(["run", "--rm", "-v"])
            .arg(format!("{}:/d", root.display()))
            .args(["alpine:3", "rm", "-rf", "/d/instances", "/d/backups"])
            .output();
        let _ = std::fs::remove_dir_all(&root);
        run.unwrap();
    }

    #[test]
    fn tar_entry_matching() {
        assert!(tar_entry_matches("data/", "data"));
        assert!(tar_entry_matches("./data/x/y", "data"));
        assert!(tar_entry_matches("data", "./data"));
        assert!(!tar_entry_matches("database/x", "data"));
        assert!(!tar_entry_matches("other/data", "data"));
    }

    #[tokio::test]
    async fn verify_flags_missing_and_empty_dumps() {
        let t = crate::template::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("apps/miniflux"))
            .unwrap();
        let d = &t.manifest.backup.dumps;
        if d.is_empty() {
            return;
        }
        let root = tmp();
        let inst = Instance {
            name: "mf".into(),
            dir: root.join("instances/mf"),
        };
        let backups = root.join("backups");
        let snap = backups.join("mf/100");
        std::fs::create_dir_all(&snap).unwrap();
        let mut l = |_: &str| {};
        let err = verify(&backups, &inst, &t, None, &mut l).await.unwrap_err();
        assert!(err.contains("missing"), "{err}");
        std::fs::write(snap.join(&d[0].output), b"").unwrap();
        let err = verify(&backups, &inst, &t, None, &mut l).await.unwrap_err();
        assert!(err.contains("empty"), "{err}");
        assert!(verified_at(&snap).is_none());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
