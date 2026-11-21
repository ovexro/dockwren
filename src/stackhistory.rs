//! Earlier versions of a pasted or adopted stack's Compose file and environment.
//!
//! Every successful "Edit stack" keeps the file it replaced under `<instance>/stack-history/<epoch>/`
//! (`compose.yaml` + `env`, mode 0640 like the live files). The newest `KEEP` are kept. A version
//! is only loaded into the editor; applying it goes through the normal checks, backup and rollback.

use std::path::{Path, PathBuf};

const DIR: &str = "stack-history";
const KEEP: usize = 20;

pub struct Revision {
    pub id: u64,
    pub compose: String,
    pub env: String,
}

fn root(instance_dir: &Path) -> PathBuf {
    instance_dir.join(DIR)
}

/// Keep `compose` and `env` as a new version stamped `epoch`, then drop the oldest beyond `KEEP`.
pub fn save(instance_dir: &Path, epoch: u64, compose: &str, env: &str) -> std::io::Result<()> {
    let base = root(instance_dir);
    // A second edit within the same second must not collide.
    let mut id = epoch;
    while base.join(id.to_string()).exists() {
        id += 1;
    }
    let dir = base.join(id.to_string());
    std::fs::create_dir_all(&dir)?;
    crate::runner::write_private(&dir.join("compose.yaml"), compose)?;
    // The saved `.env` holds passwords: owner only, like the live file.
    crate::runner::write_secret(&dir.join("env"), env)?;
    let ids = list_ids(instance_dir);
    for old in ids.iter().skip(KEEP) {
        let _ = std::fs::remove_dir_all(base.join(old.to_string()));
    }
    Ok(())
}

/// Version ids, newest first.
pub fn list_ids(instance_dir: &Path) -> Vec<u64> {
    let mut v: Vec<u64> = std::fs::read_dir(root(instance_dir))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect();
    v.sort_unstable_by(|a, b| b.cmp(a));
    v
}

/// One version, by its numeric id (never a path taken from the request).
pub fn load(instance_dir: &Path, id: u64) -> Option<Revision> {
    if !list_ids(instance_dir).contains(&id) {
        return None;
    }
    let dir = root(instance_dir).join(id.to_string());
    Some(Revision {
        id,
        compose: std::fs::read_to_string(dir.join("compose.yaml")).ok()?,
        env: std::fs::read_to_string(dir.join("env")).unwrap_or_default(),
    })
}

/// Line diff from `old` to `new`: ('-', line), ('+', line) and (' ', line) for unchanged lines.
pub fn diff(old: &str, new: &str) -> Vec<(char, String)> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let (n, m) = (a.len(), b.len());
    // Longest common subsequence table; stacks are small, but cap it anyway.
    if n.saturating_mul(m) > 4_000_000 {
        return vec![('-', "(file too large to compare)".into())];
    }
    let mut t = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            t[i][j] = if a[i] == b[j] {
                t[i + 1][j + 1] + 1
            } else {
                t[i + 1][j].max(t[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < n && j < m {
        if a[i] == b[j] {
            out.push((' ', a[i].to_string()));
            i += 1;
            j += 1;
        } else if t[i + 1][j] >= t[i][j + 1] {
            out.push(('-', a[i].to_string()));
            i += 1;
        } else {
            out.push(('+', b[j].to_string()));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|l| ('-', l.to_string())));
    out.extend(b[j..].iter().map(|l| ('+', l.to_string())));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dw-hist-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_saved_env_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("mode");
        save(&d, 5, "services: {}", "DB_PASSWORD='x'").unwrap();
        let mode = |n: &str| {
            std::fs::metadata(root(&d).join("5").join(n))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("env"), 0o600);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn saves_lists_newest_first_and_trims() {
        let d = tmp("trim");
        for i in 0..25u64 {
            save(&d, 1000 + i, &format!("c{i}"), &format!("e{i}")).unwrap();
        }
        let ids = list_ids(&d);
        assert_eq!(ids.len(), KEEP);
        assert_eq!(ids[0], 1024);
        let r = load(&d, 1024).unwrap();
        assert_eq!((r.compose.as_str(), r.env.as_str()), ("c24", "e24"));
        assert!(load(&d, 1000).is_none(), "trimmed");
        assert!(load(&d, 7).is_none(), "unknown id");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn same_second_does_not_collide() {
        let d = tmp("same");
        save(&d, 50, "a", "").unwrap();
        save(&d, 50, "b", "").unwrap();
        assert_eq!(list_ids(&d), vec![51, 50]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn diff_marks_changes() {
        let d = diff("a\nb\nc", "a\nx\nc\nd");
        assert_eq!(
            d,
            vec![
                (' ', "a".into()),
                ('-', "b".into()),
                ('+', "x".into()),
                (' ', "c".into()),
                ('+', "d".into())
            ]
        );
        assert!(diff("same", "same").iter().all(|(k, _)| *k == ' '));
    }
}
