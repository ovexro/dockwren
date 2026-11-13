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
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    f.write_all(content)
}

/// Snapshot directory names are plain epoch seconds: sortable, no time crate needed.
fn is_snapshot_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 12 && n.bytes().all(|b| b.is_ascii_digit())
}

/// Existing snapshots of an instance, oldest first.
pub fn list(backups_root: &Path, instance: &str) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(backups_root.join(instance)) else { return Vec::new() };
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
    out.lines().map(str::trim).find(|l| !l.is_empty()).map(String::from).ok_or_else(|| "instance has no images".into())
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
async fn snapshot_keep(
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
            return Err(format!("backup path {p} does not exist in {}", inst.dir.display()));
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
    while parent.join(secs.to_string()).exists() || parent.join(format!("{secs}.partial")).exists() {
        secs += 1;
    }
    let tmp = parent.join(format!("{secs}.partial"));
    let fin = parent.join(secs.to_string());
    std::fs::DirBuilder::new().mode(0o700).create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;

    // On failure the partial dir is left for inspection; it is never listed or pruned.
    for d in &b.dumps {
        log(&format!("dumping {} ({})", d.output, d.service));
        let args: Vec<&str> = ["exec", "-T", d.service.as_str()]
            .into_iter()
            .chain(d.run.iter().map(String::as_str))
            .collect();
        let out = runner::compose(inst, &args).await.map_err(|e| format!("dump {}: {e}", d.output))?;
        if out.trim().is_empty() {
            return Err(format!("dump {} produced no output (partial snapshot in {})", d.output, tmp.display()));
        }
        write_private(&tmp.join(&d.output), out.as_bytes()).map_err(|e| format!("{}: {e}", d.output))?;
    }
    if !b.paths.is_empty() {
        log("archiving files");
        let image = first_image(inst).await?;
        let dir = std::fs::canonicalize(&inst.dir).map_err(|e| format!("{}: {e}", inst.dir.display()))?;
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
            .args(["run", "--rm", "--network", "none", "--user", "0", "--entrypoint", "tar"])
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
            return Err(format!("archiving failed (partial snapshot in {}): {}", tmp.display(), tail.join("; ")));
        }
        if std::fs::metadata(tmp.join("files.tar.gz")).map(|m| m.len()).unwrap_or(0) == 0 {
            return Err(format!("archive is empty (partial snapshot in {})", tmp.display()));
        }
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
    snapshot(&cfg.data_dir.join("backups"), &Instance { name: name.to_string(), dir }, t, log).await
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

/// Pick the snapshot to restore: the named one, or the newest.
fn pick(backups_root: &Path, instance: &str, name: Option<&str>) -> Result<PathBuf, String> {
    let all = list(backups_root, instance);
    match name {
        None => all.last().cloned().ok_or_else(|| format!("no snapshots of {instance}")),
        Some(n) => all
            .iter()
            .find(|p| p.file_name().is_some_and(|f| f == n) && is_snapshot_name(n))
            .cloned()
            .ok_or_else(|| {
                let have: Vec<String> =
                    all.iter().filter_map(|p| p.file_name()?.to_str().map(String::from)).collect();
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
    if safety {
        log("safety snapshot of the current state");
        // keep = existing + 1, so this cannot prune the snapshot being restored
        let keep = list(backups_root, &inst.name).len() + 1;
        let s = snapshot_keep(backups_root, inst, t, log, keep)
            .await
            .map_err(|e| format!("safety snapshot failed, nothing was changed: {e} (--no-safety-snapshot skips it)"))?;
        log(&format!("safety snapshot in {}", s.display()));
    }
    let image = first_image(inst).await?;
    let dir = std::fs::canonicalize(&inst.dir).map_err(|e| format!("{}: {e}", inst.dir.display()))?;
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
        let archive = std::fs::File::open(snap.join("files.tar.gz")).map_err(|e| format!("files.tar.gz: {e}"))?;
        let out = docker_cmd(inst)
            .args(["run", "--rm", "-i", "--network", "none", "--user", "0", "--entrypoint", "tar"])
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
            return Err(format!("extracting failed, previous files put back: {}", tail.join("; ")));
        }
    }
    log("starting");
    runner::compose(inst, &["up", "-d"]).await?;
    runner::wait_healthy(inst, std::time::Duration::from_secs(240)).await?;
    for d in &b.dumps {
        log(&format!("restoring {} into {}", d.output, d.service));
        let f = std::fs::File::open(snap.join(&d.output)).map_err(|e| format!("{}: {e}", d.output))?;
        let args: Vec<&str> =
            ["exec", "-T", d.service.as_str()].into_iter().chain(d.restore.iter().map(String::as_str)).collect();
        runner::compose_with_stdin(inst, &args, Some(f)).await.map_err(|e| format!("restore {}: {e}", d.output))?;
    }
    if !b.dumps.is_empty() {
        log("restarting so the app sees the restored database");
        runner::compose(inst, &["restart"]).await?;
        runner::wait_healthy(inst, std::time::Duration::from_secs(240)).await?;
    }
    Ok(snap)
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
    let state = std::fs::read_to_string(dir.join("state.toml")).map_err(|_| format!("no instance named {name}"))?;
    let state: State = toml::from_str(&state).map_err(|e| format!("state.toml: {e}"))?;
    let t = templates
        .iter()
        .find(|t| t.manifest.app.id == state.template)
        .ok_or_else(|| format!("template {} is not available", state.template))?;
    restore(&cfg.data_dir.join("backups"), &Instance { name: name.to_string(), dir }, t, snap, safety, log).await
}

/// Is `n` a leftover directory name made by `restore` (`<path>.pre-restore-<epoch>` or `.failed-restore-<epoch>`)?
fn is_restore_leftover(n: &str) -> bool {
    ["pre-restore", "failed-restore"].iter().any(|k| {
        n.split_once(&format!(".{k}-"))
            .is_some_and(|(base, num)| !base.is_empty() && !base.contains('/') && is_snapshot_name(num))
    }) && !n.contains('/')
}

/// Delete the `*.pre-restore-*` / `*.failed-restore-*` directories `restore` leaves in the
/// instance dir (they are root-owned, so a throwaway container removes them). Returns the names removed.
pub async fn cleanup_restore_leftovers(inst: &Instance, log: &mut (dyn FnMut(&str) + Send)) -> Result<Vec<String>, String> {
    let dir = std::fs::canonicalize(&inst.dir).map_err(|e| format!("{}: {e}", inst.dir.display()))?;
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| is_restore_leftover(n))
        .collect();
    names.sort();
    if names.is_empty() {
        return Ok(names);
    }
    let image = first_image(inst).await?;
    for n in &names {
        log(&format!("removing {n}"));
        let out = docker_cmd(inst)
            .args(["run", "--rm", "--network", "none", "--user", "0", "--entrypoint", "rm"])
            .arg("-v")
            .arg(format!("{}:/w", dir.display()))
            .args([image.as_str(), "-rf", "--"])
            .arg(format!("/w/{n}"))
            .output()
            .await
            .map_err(|e| format!("cannot run docker: {e}"))?;
        if !out.status.success() {
            return Err(format!("removing {n}: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
    }
    Ok(names)
}

pub async fn cleanup_instance(cfg: &Config, name: &str, log: &mut (dyn FnMut(&str) + Send)) -> Result<Vec<String>, String> {
    let dir = cfg.instances_dir().join(name);
    if !dir.join("state.toml").is_file() {
        return Err(format!("no instance named {name}"));
    }
    cleanup_restore_leftovers(&Instance { name: name.to_string(), dir }, log).await
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
        for bad in ["data", "data.pre-restore-", "data.pre-restore-x", ".pre-restore-1", "data.pre-restore-1/../x", "compose.yaml"] {
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
        let left: Vec<String> =
            list(&root, "app").iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
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
            assert!(pick(&root, "app", Some(bad)).unwrap_err().contains("have: 100, 200"), "{bad}");
        }
        assert!(pick(&root, "none", None).unwrap_err().contains("no snapshots"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn dot_and_empty_backup_paths_are_rejected_by_templates() {
        for bad in [".", "", "a//b", "./x"] {
            let m = format!("[app]\nid=\"x\"\nname=\"X\"\nversion=1\n[backup]\npaths=[\"{bad}\"]\n");
            assert!(crate::template::parse(&m, "services: {}\n").is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn snapshot_refuses_missing_paths_without_leaving_files() {
        let t = crate::template::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("apps/vaultwarden")).unwrap();
        let root = tmp();
        let inst = Instance { name: "vw".into(), dir: root.join("instances/vw") };
        std::fs::create_dir_all(&inst.dir).unwrap();
        let backups = root.join("backups");
        let mut l = |_: &str| {};
        let err = snapshot(&backups, &inst, &t, &mut l).await.unwrap_err();
        assert!(err.contains("does not exist"), "{err}");
        assert!(!backups.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
