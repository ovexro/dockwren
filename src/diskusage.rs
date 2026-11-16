//! Disk usage per app: its data folder, its snapshots and the images it runs, with
//! suggestions for freeing space. Sizes are read, never changed, except by the two
//! explicit buttons (keep fewer snapshots, remove unused images).

use std::path::Path;

use tokio::process::Command;

use crate::runner::Instance;

/// Snapshots kept by the "free space" button.
pub const KEEP_AFTER_TRIM: usize = 3;

/// Total size in bytes of the files below `path` (links are not followed); the flag is true
/// when some part could not be read (owned by another user), so the real size is larger.
pub fn dir_size(path: &Path) -> (u64, bool) {
    let mut total = 0u64;
    let mut partial = false;
    let mut stack = vec![path.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            partial = true;
            continue;
        };
        for e in rd.flatten() {
            let Ok(m) = e.path().symlink_metadata() else {
                partial = true;
                continue;
            };
            if m.is_dir() {
                stack.push(e.path());
            } else {
                total += m.len();
            }
        }
    }
    (total, partial)
}

#[derive(Debug, Clone, Default)]
pub struct AppUsage {
    pub name: String,
    /// The app's folder (data, config), without the old copies below.
    pub data: u64,
    /// Old copies kept aside by earlier restores (`*.pre-restore-*`, `*.failed-restore-*`).
    pub leftovers: u64,
    pub snapshots: usize,
    pub snapshot_bytes: u64,
    /// Bytes the snapshots beyond the newest `KEEP_AFTER_TRIM` take.
    pub trimmable: u64,
    pub images: u64,
    pub partial: bool,
}

impl AppUsage {
    pub fn total(&self) -> u64 {
        self.data + self.leftovers + self.snapshot_bytes + self.images
    }
}

fn is_leftover(name: &str) -> bool {
    name.contains(".pre-restore-") || name.contains(".failed-restore-")
}

/// Sizes of one app. Blocking (walks the disk): call from `spawn_blocking`.
pub fn measure(inst: &Instance, backups_root: &Path) -> AppUsage {
    let mut u = AppUsage {
        name: inst.name.clone(),
        ..AppUsage::default()
    };
    if let Ok(rd) = std::fs::read_dir(&inst.dir) {
        for e in rd.flatten() {
            let leftover = e.file_name().to_str().is_some_and(is_leftover);
            let Ok(m) = e.path().symlink_metadata() else {
                u.partial = true;
                continue;
            };
            let (size, partial) = if m.is_dir() {
                dir_size(&e.path())
            } else {
                (m.len(), false)
            };
            u.partial |= partial;
            if leftover {
                u.leftovers += size;
            } else {
                u.data += size;
            }
        }
    }
    let snaps = crate::backup::list(backups_root, &inst.name);
    u.snapshots = snaps.len();
    let sizes: Vec<u64> = snaps.iter().map(|s| dir_size(s).0).collect();
    u.snapshot_bytes = sizes.iter().sum();
    u.trimmable = sizes
        .iter()
        .take(sizes.len().saturating_sub(KEEP_AFTER_TRIM))
        .sum();
    u
}

fn docker(data_dir: &Path) -> Command {
    let mut c = Command::new("docker");
    if std::env::var_os("DOCKER_CONFIG").is_none() {
        c.env("DOCKER_CONFIG", data_dir.join("docker-config"));
    }
    c
}

/// Total size of the images an app uses (shared layers are counted per image, so this is an
/// upper bound).
pub async fn image_bytes(data_dir: &Path, inst: &Instance) -> u64 {
    let Ok(out) = crate::runner::compose(inst, &["config", "--images"]).await else {
        return 0;
    };
    let names: Vec<&str> = out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if names.is_empty() {
        return 0;
    }
    let Ok(o) = docker(data_dir)
        .args(["image", "inspect", "--format", "{{.Size}}"])
        .args(&names)
        .output()
        .await
    else {
        return 0;
    };
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<u64>().ok())
        .sum()
}

/// "Images: 18.02GB (16.24GB reclaimable)" style line from `docker system df`.
pub async fn docker_images_line(data_dir: &Path) -> Option<(String, String)> {
    let o = docker(data_dir)
        .args(["system", "df", "--format", "{{json .}}"])
        .output()
        .await
        .ok()?;
    for l in String::from_utf8_lossy(&o.stdout).lines() {
        let v: serde_json::Value = serde_json::from_str(l).ok()?;
        if v["Type"] == "Images" {
            return Some((
                v["Size"].as_str()?.to_string(),
                v["Reclaimable"].as_str()?.to_string(),
            ));
        }
    }
    None
}

/// `docker image prune -a -f`: remove every image no container uses. Returns docker's summary line.
pub async fn prune_images(data_dir: &Path) -> Result<String, String> {
    let o = docker(data_dir)
        .args(["image", "prune", "-a", "-f"])
        .output()
        .await
        .map_err(|e| format!("cannot run docker: {e}"))?;
    if !o.status.success() {
        return Err(String::from_utf8_lossy(&o.stderr)
            .lines()
            .next()
            .unwrap_or("docker failed")
            .to_string());
    }
    Ok(String::from_utf8_lossy(&o.stdout)
        .lines()
        .rev()
        .find(|l| l.contains("reclaimed"))
        .unwrap_or("done")
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let d =
            std::env::temp_dir().join(format!("dockwren-du-{}", crate::auth::generate_password()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn sizes_add_up_and_leftovers_are_separate() {
        let root = tmp();
        let inst = root.join("instances").join("app");
        std::fs::create_dir_all(inst.join("data")).unwrap();
        std::fs::write(inst.join("data/a"), vec![0u8; 1000]).unwrap();
        std::fs::write(inst.join(".env"), vec![0u8; 24]).unwrap();
        std::fs::create_dir_all(inst.join("data.pre-restore-5")).unwrap();
        std::fs::write(inst.join("data.pre-restore-5/b"), vec![0u8; 500]).unwrap();
        let b = root.join("backups").join("app");
        for (n, sz) in [
            ("100", 10),
            ("200", 20),
            ("300", 30),
            ("400", 40),
            ("500", 50),
        ] {
            std::fs::create_dir_all(b.join(n)).unwrap();
            std::fs::write(b.join(n).join("files.tar.gz"), vec![0u8; sz]).unwrap();
        }
        let u = measure(
            &Instance {
                name: "app".into(),
                dir: inst,
            },
            &root.join("backups"),
        );
        assert_eq!((u.data, u.leftovers), (1024, 500));
        assert_eq!((u.snapshots, u.snapshot_bytes), (5, 150));
        assert_eq!(u.trimmable, 10 + 20); // all but the newest 3
        let _ = std::fs::remove_dir_all(&root);
    }
}
