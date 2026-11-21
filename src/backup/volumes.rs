//! Snapshot sources: named volumes and host folders of adopted stacks, archived and restored
//! through a throwaway root container.

use super::*;

/// An image of the instance that is already pulled; it only supplies `tar`.
pub(super) async fn first_image(inst: &Instance) -> Result<String, String> {
    let out = runner::compose(inst, &["config", "--images"]).await?;
    out.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(String::from)
        .ok_or_else(|| "instance has no images".into())
}

/// Snapshot file holding one named volume of an adopted stack: `vol-<compose key>.tar.gz`.
pub(super) const VOL_PREFIX: &str = "vol-";
pub(super) const VOL_SUFFIX: &str = ".tar.gz";
/// Host folders of an adopted stack: `bind-<key>.tar.gz`, with `binds.json` mapping key -> path.
pub(super) const BIND_PREFIX: &str = "bind-";
pub(super) const BINDS_FILE: &str = "binds.json";

/// System locations that are never backed up or restored as "data".
pub(super) const BIND_DENY: [&str; 12] = [
    "/proc",
    "/sys",
    "/dev",
    "/run",
    "/boot",
    "/etc",
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/var/run",
    "/var/lib/docker",
];

/// Whether a host folder may be archived and restored by the panel. The path ends up in
/// `docker run --mount`, in a shell argument and in a snapshot, so it must be plain.
pub fn bind_source_ok(src: &str) -> Result<(), String> {
    let p = Path::new(src);
    let bad = |why: &str| Err(format!("the host folder {src} {why}"));
    if !p.is_absolute() || src.chars().any(|c| c.is_control() || c == ',' || c == ':') {
        return bad(
            "has a path the panel cannot back up (relative, or it contains , : or control characters)",
        );
    }
    if src.split('/').any(|s| s == "." || s == "..")
        || p.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return bad("contains . or .. in its path");
    }
    // The parent folder is what gets mounted for a restore, so it must not be the root.
    if p.parent().is_none_or(|q| q.parent().is_none()) {
        return bad("is too close to the root of the disk to be backed up safely");
    }
    if BIND_DENY.iter().any(|d| p.starts_with(d)) {
        return bad("is a system folder, not app data");
    }
    Ok(())
}

/// Unique host folders bind-mounted by the services in `docker compose config` JSON (sorted).
pub(super) fn parse_binds(config_json: &str) -> Result<Vec<String>, String> {
    let v: serde_json::Value =
        serde_json::from_str(config_json).map_err(|e| format!("compose config: {e}"))?;
    let mut out = std::collections::BTreeSet::new();
    for svc in v
        .get("services")
        .and_then(|m| m.as_object())
        .into_iter()
        .flat_map(|m| m.values())
    {
        for vol in svc
            .get("volumes")
            .and_then(|m| m.as_array())
            .into_iter()
            .flatten()
        {
            if vol.get("type").and_then(|t| t.as_str()) == Some("bind")
                && let Some(src) = vol.get("source").and_then(|t| t.as_str())
            {
                bind_source_ok(src)?;
                out.insert(src.to_string());
            }
        }
    }
    Ok(out.into_iter().collect())
}

pub(super) async fn compose_binds(inst: &Instance) -> Result<Vec<String>, String> {
    parse_binds(&runner::compose(inst, &["config", "--format", "json"]).await?)
}

/// True when the instance is an adopted stack whose data lives in host folders: a second copy
/// of it (clone, restore test) would share and overwrite them.
pub async fn shares_host_folders(inst: &Instance) -> Result<bool, String> {
    if !is_adopted(inst) {
        return Ok(false);
    }
    Ok(!compose_binds(inst).await?.is_empty())
}

/// `(key, host folder)` pairs recorded in a snapshot (sorted by key); empty when it has none.
pub(super) fn snapshot_binds(snap: &Path) -> Result<Vec<(String, String)>, String> {
    let Ok(text) = std::fs::read_to_string(snap.join(BINDS_FILE)) else {
        return Ok(Vec::new());
    };
    let map: std::collections::BTreeMap<String, String> =
        serde_json::from_str(&text).map_err(|e| format!("{BINDS_FILE}: {e}"))?;
    let mut out = Vec::new();
    for (k, src) in map {
        if !valid_volume_key(&k) {
            return Err(format!("{BINDS_FILE}: bad key {k}"));
        }
        bind_source_ok(&src)?;
        if !snap.join(format!("{BIND_PREFIX}{k}{VOL_SUFFIX}")).is_file() {
            return Err(format!("snapshot is missing {BIND_PREFIX}{k}{VOL_SUFFIX}"));
        }
        out.push((k, src));
    }
    Ok(out)
}

/// Adopted stacks keep their data in named volumes, which `backup.paths` cannot reach.
pub(super) fn is_adopted(inst: &Instance) -> bool {
    inst.dir.join(runner::PROJECT_FILE).is_file()
}

/// Compose volume keys are letters, digits, `.`, `_`, `-`; anything else never reaches a file name.
pub(super) fn valid_volume_key(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 64
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Volume keys stored in a snapshot (sorted).
pub(super) fn snapshot_volumes(snap: &Path) -> Vec<String> {
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
pub(super) fn parse_volumes(config_json: &str) -> Result<Vec<(String, String)>, String> {
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

pub(super) async fn compose_volumes(inst: &Instance) -> Result<Vec<(String, String)>, String> {
    parse_volumes(&runner::compose(inst, &["config", "--format", "json"]).await?)
}

pub(super) async fn volume_exists(inst: &Instance, name: &str) -> bool {
    docker_cmd(inst)
        .args(["volume", "inspect", name])
        .output()
        .await
        .is_ok_and(|o| o.status.success())
}

/// `-v` for a named volume; `--mount` for a host folder, which (unlike `-v`) fails instead of
/// creating a missing folder as root.
pub(super) fn source_mount(source: &str, bind: bool, target: &str, ro: bool) -> Vec<String> {
    if bind {
        let ro = if ro { ",readonly" } else { "" };
        vec![
            "--mount".into(),
            format!("type=bind,source={source},target={target}{ro}"),
        ]
    } else {
        vec![
            "-v".into(),
            format!("{source}:{target}{}", if ro { ":ro" } else { "" }),
        ]
    }
}

/// Archives one volume or host folder into `dest` through a throwaway root container (read-only mount).
pub(super) async fn archive_volume(
    inst: &Instance,
    image: &str,
    volume: &str,
    bind: bool,
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
        .args(source_mount(volume, bind, "/src", true))
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
            "archiving {volume} failed (a single file cannot be backed up, only folders): {}",
            tail.join("; ")
        ));
    }
    if std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0) == 0 {
        return Err(format!("archive of {volume} is empty"));
    }
    Ok(())
}

/// Archives every named volume of an adopted stack. The stack is stopped for the copy (a
/// running database would give an inconsistent file) and started again whatever happens.
pub(super) async fn snapshot_volumes_into(
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
    let binds = compose_binds(inst).await?;
    if vols.is_empty() && binds.is_empty() {
        return Ok(());
    }
    let image = first_image(inst).await?;
    log("stopping the stack for a consistent copy of its data");
    runner::compose(inst, &["stop"]).await?;
    let mut result = Ok(());
    for (key, name) in &vols {
        log(&format!("archiving volume {key}"));
        let dest = tmp.join(format!("{VOL_PREFIX}{key}{VOL_SUFFIX}"));
        if let Err(e) = archive_volume(inst, &image, name, false, &dest).await {
            result = Err(e);
            break;
        }
    }
    let mut map = std::collections::BTreeMap::new();
    if result.is_ok() {
        for (i, src) in binds.iter().enumerate() {
            let key = format!("b{}", i + 1);
            log(&format!("archiving host folder {src}"));
            let dest = tmp.join(format!("{BIND_PREFIX}{key}{VOL_SUFFIX}"));
            if let Err(e) = archive_volume(inst, &image, src, true, &dest).await {
                result = Err(e);
                break;
            }
            map.insert(key, src.clone());
        }
    }
    if result.is_ok() && !map.is_empty() {
        result = serde_json::to_vec(&map)
            .map_err(|e| e.to_string())
            .and_then(|b| write_private(&tmp.join(BINDS_FILE), &b).map_err(|e| e.to_string()));
    }
    log("starting the stack again");
    let started = runner::compose(inst, &["start"]).await;
    result?;
    started.map(|_| ())
}

/// Empties volume `name` (it is removed and created again by Compose) and unpacks `archive`.
pub(super) async fn restore_volume(
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

/// Puts a host folder back from `archive`. The current folder is moved to
/// `<folder>.pre-restore-<stamp>` (nothing is deleted); if unpacking fails it is moved back.
/// Runs as root in a throwaway container that mounts only the folder's parent.
pub(super) async fn restore_bind(
    inst: &Instance,
    image: &str,
    src: &str,
    archive: &Path,
    stamp: u64,
) -> Result<(), String> {
    bind_source_ok(src)?;
    let p = Path::new(src);
    let (parent, leaf) = match (p.parent(), p.file_name().and_then(|n| n.to_str())) {
        (Some(a), Some(b)) => (a.to_string_lossy().into_owned(), b.to_string()),
        _ => return Err(format!("cannot restore {src}")),
    };
    let file = std::fs::File::open(archive).map_err(|e| format!("{}: {e}", archive.display()))?;
    const SCRIPT: &str = r#"n=$1; s=$2
if [ -e "/p/$n" ]; then mv "/p/$n" "/p/$n.pre-restore-$s" || exit 1; fi
mkdir "/p/$n" || exit 1
if tar --numeric-owner -xzf - -C "/p/$n"; then exit 0; fi
mv "/p/$n" "/p/$n.failed-restore-$s"
if [ -e "/p/$n.pre-restore-$s" ]; then mv "/p/$n.pre-restore-$s" "/p/$n"; fi
exit 1"#;
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
            "sh",
        ])
        .args(source_mount(&parent, true, "/p", false))
        .args([image, "-c", SCRIPT, "sh", &leaf, &stamp.to_string()])
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
            "restoring {src} failed, its previous content was put back: {}",
            tail.join("; ")
        ));
    }
    Ok(())
}
