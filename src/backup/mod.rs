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

mod cleanup;
mod restore;
mod snapshot;
mod verify;
mod volumes;

pub use cleanup::*;
pub use restore::*;
pub use snapshot::*;
pub use verify::*;
pub use volumes::*;

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

fn docker_cmd(inst: &Instance) -> Command {
    let mut cmd = Command::new("docker");
    if std::env::var_os("DOCKER_CONFIG").is_none()
        && let Some(data) = inst.dir.parent().and_then(Path::parent)
    {
        cmd.env("DOCKER_CONFIG", data.join("docker-config"));
    }
    cmd
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
    fn bind_sources_must_be_plain_app_folders() {
        for ok in ["/srv/pg", "/home/u/app/data", "/var/lib/myapp"] {
            assert_eq!(bind_source_ok(ok), Ok(()), "{ok}");
        }
        for bad in [
            "/",
            "/tmp",
            "relative/x",
            "/srv/a:b",
            "/srv/a,b",
            "/srv/../etc/x",
            "/etc/app",
            "/proc/1",
            "/var/run/docker.sock",
            "/var/lib/docker/volumes",
            "/srv/a\nb",
        ] {
            assert!(bind_source_ok(bad).is_err(), "{bad}");
        }
        let j = r#"{"services":{"a":{"volumes":[{"type":"bind","source":"/srv/pg","target":"/x"},{"type":"volume","source":"v","target":"/y"}]},"b":{"volumes":[{"type":"bind","source":"/srv/pg","target":"/z"},{"type":"bind","source":"/srv/app","target":"/w"}]}}}"#;
        assert_eq!(parse_binds(j).unwrap(), ["/srv/app", "/srv/pg"]);
        let evil =
            r#"{"services":{"a":{"volumes":[{"type":"bind","source":"/etc/x","target":"/x"}]}}}"#;
        assert!(parse_binds(evil).is_err());
    }

    /// Real Docker: a host folder of an adopted-style project is archived in place (stack
    /// stopped and started again), changed, then restored; the changed folder is kept aside.
    #[tokio::test]
    async fn adopted_bind_round_trip() {
        let root = tmp();
        let name = format!("bt{}", std::process::id());
        let inst = Instance {
            name: name.clone(),
            dir: root.join("instances").join(&name),
        };
        let host = root.join("host/appdata");
        std::fs::create_dir_all(inst.dir.join("data")).unwrap();
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(host.join("f"), "before").unwrap();
        std::fs::write(
            inst.dir.join("compose.yaml"),
            format!(
                "services:\n  app:\n    image: alpine:3\n    command: [\"sleep\", \"600\"]\n    volumes: [\"{}:/v\"]\n",
                host.display()
            ),
        )
        .unwrap();
        std::fs::write(inst.dir.join(".env"), "").unwrap();
        std::fs::write(
            inst.dir.join(runner::PROJECT_FILE),
            format!("dwbt-{name}\n"),
        )
        .unwrap();
        let t = crate::template::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("apps/custom"))
            .unwrap();
        let backups = root.join("backups");
        let mut l = |_: &str| {};
        let run = async {
            assert!(shares_host_folders(&inst).await?);
            runner::compose(&inst, &["up", "-d"]).await?;
            let snap = snapshot(&backups, &inst, &t, &mut l).await?;
            assert!(snap.join("bind-b1.tar.gz").is_file() && snap.join(BINDS_FILE).is_file());
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
            let aside: Vec<_> = std::fs::read_dir(root.join("host"))
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().contains(".pre-restore-"))
                .collect();
            assert_eq!(aside.len(), 1, "previous folder must be kept aside");
            // A copy without the project file must refuse to write into the live folder.
            let copy = Instance {
                name: format!("rt-{name}"),
                dir: root.join("instances").join(format!("rt-{name}")),
            };
            std::fs::create_dir_all(&copy.dir).unwrap();
            std::fs::copy(inst.dir.join("compose.yaml"), copy.dir.join("compose.yaml")).unwrap();
            std::fs::create_dir_all(backups.join(&copy.name)).unwrap();
            std::os::unix::fs::symlink(
                &snap,
                backups.join(&copy.name).join(snap.file_name().unwrap()),
            )
            .unwrap();
            let e = restore(&backups, &copy, &t, None, false, &mut l)
                .await
                .unwrap_err();
            assert!(e.contains("only the adopted stack"), "{e}");
            Ok::<(), String>(())
        }
        .await;
        let _ = runner::compose(&inst, &["down", "-v"]).await;
        let _ = std::process::Command::new("docker")
            .args(["run", "--rm", "-v"])
            .arg(format!("{}:/d", root.display()))
            .args([
                "alpine:3",
                "rm",
                "-rf",
                "/d/instances",
                "/d/backups",
                "/d/host",
            ])
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
