//! Config export/import: every instance's small config files (`.env`, compose files, limits,
//! state) in ONE file encrypted with a passphrase, for rebuilding this server elsewhere.
//!
//! Snapshots hold app data only; the passwords in `.env` are needed to restore a database.
//! Format: `DWCFG1\n` + 16-byte salt + 12-byte nonce + ChaCha20-Poly1305 ciphertext of a JSON
//! document. Key = Argon2id(passphrase, salt). Import never overwrites an existing instance.

use argon2::Argon2;
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit,
    aead::{Aead, Payload},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

const MAGIC: &[u8] = b"DWCFG1\n";
/// Files copied per instance. Anything else in the directory (data, backups) is left out.
const FILES: &[&str] = &[
    "state.toml",
    "compose.yaml",
    "compose.override.yaml",
    ".env",
    "limits.toml",
    "compose.limits.yaml",
    "public-mode.txt",
];
/// Panel settings in the data directory (the parent of the instances directory): the login
/// (admin hash, 2FA), notification channels, backup schedule, OS-update switch and the
/// Cloudflare connection. Whoever can open the export can take over the panel.
const PANEL_FILES: &[&str] = &[
    "admin.hash",
    "totp.secret",
    "totp.recovery",
    "notify-channels.json",
    "backup-schedule.toml",
    "osupdate.enabled",
    "cloudflare.json",
];
pub const MIN_PASSPHRASE: usize = 12;

#[derive(Serialize, Deserialize, Default)]
struct Bundle {
    /// instance name -> file name -> content
    instances: BTreeMap<String, BTreeMap<String, String>>,
    /// instance name -> public hostname (a note only: re-publish with `dockwren expose`)
    #[serde(default)]
    hosts: BTreeMap<String, String>,
    /// panel settings file name -> content (absent in older exports)
    #[serde(default)]
    panel: BTreeMap<String, String>,
}

fn key(pass: &str, salt: &[u8]) -> Result<ChaCha20Poly1305, String> {
    let mut k = [0u8; 32];
    Argon2::default()
        .hash_password_into(pass.as_bytes(), salt, &mut k)
        .map_err(|e| format!("key derivation: {e}"))?;
    ChaCha20Poly1305::new_from_slice(&k).map_err(|e| e.to_string())
}

fn seal(b: &Bundle, pass: &str, min: usize) -> Result<Vec<u8>, String> {
    if pass.chars().count() < min {
        return Err(format!("passphrase must be at least {min} characters"));
    }
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 12];
    rand::fill(&mut salt[..]);
    rand::fill(&mut nonce[..]);
    let json = serde_json::to_vec(b).map_err(|e| e.to_string())?;
    let ct = key(pass, &salt)?
        .encrypt(
            &nonce.into(),
            Payload {
                msg: &json,
                aad: MAGIC,
            },
        )
        .map_err(|_| "encryption failed".to_string())?;
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

fn open(data: &[u8], pass: &str) -> Result<Bundle, String> {
    let bad = || "not a Dockwren config export".to_string();
    let rest = data.strip_prefix(MAGIC).ok_or_else(bad)?;
    if rest.len() < 16 + 12 + 16 {
        return Err(bad());
    }
    let (salt, rest) = rest.split_at(16);
    let (nonce, ct) = rest.split_at(12);
    let nonce: [u8; 12] = nonce.try_into().map_err(|_| bad())?;
    let json = key(pass, salt)?
        .decrypt(
            &nonce.into(),
            Payload {
                msg: ct,
                aad: MAGIC,
            },
        )
        .map_err(|_| "wrong passphrase, or the file is damaged".to_string())?;
    serde_json::from_slice(&json).map_err(|_| bad())
}

fn collect(instances_root: &Path) -> Result<Bundle, String> {
    let mut b = Bundle::default();
    if let Some(data) = instances_root.parent() {
        for f in PANEL_FILES {
            match std::fs::read_to_string(data.join(f)) {
                Ok(c) => {
                    b.panel.insert(f.to_string(), c);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{f}: {e}")),
            }
        }
    }
    let entries = std::fs::read_dir(instances_root).map_err(|e| format!("instances: {e}"))?;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !crate::runner::valid_name(&name) || !e.path().join("state.toml").is_file() {
            continue;
        }
        let mut files = BTreeMap::new();
        for f in FILES {
            match std::fs::read_to_string(e.path().join(f)) {
                Ok(s) => {
                    files.insert(f.to_string(), s);
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(format!("{name}/{f}: {err}")),
            }
        }
        if let Some(h) = crate::public::read(&e.path()) {
            b.hosts.insert(name.clone(), h);
        }
        b.instances.insert(name, files);
    }
    Ok(b)
}

/// Write the encrypted export to `out` (mode 0600, never overwrites). Returns the instance count.
pub fn export(instances_root: &Path, out: &Path, pass: &str) -> Result<usize, String> {
    let (data, n) = export_bytes(instances_root, pass, MIN_PASSPHRASE)?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(out)
        .map_err(|e| format!("{}: {e}", out.display()))?;
    f.write_all(&data).map_err(|e| e.to_string())?;
    Ok(n)
}

/// The sealed export as bytes plus the instance count. `min` is the shortest accepted
/// passphrase (the off-host copy is sealed with the repository password, which may be 8+).
pub fn export_bytes(
    instances_root: &Path,
    pass: &str,
    min: usize,
) -> Result<(Vec<u8>, usize), String> {
    let b = collect(instances_root)?;
    if b.instances.is_empty() {
        return Err("no instances to export".into());
    }
    Ok((seal(&b, pass, min)?, b.instances.len()))
}

pub struct Imported {
    pub created: Vec<String>,
    pub skipped: Vec<String>,
    pub hosts: BTreeMap<String, String>,
    /// Panel settings files written.
    pub panel_restored: Vec<String>,
    /// Panel settings files left alone because they already exist here.
    pub panel_kept: Vec<String>,
}

/// Recreate the instance directories from an export. Existing instances are skipped.
/// Nothing is started: the caller brings each created instance up.
pub fn import(
    instances_root: &Path,
    file: &Path,
    pass: &str,
    replace_panel: bool,
) -> Result<Imported, String> {
    let data = std::fs::read(file).map_err(|e| {
        let hint = if e.kind() == std::io::ErrorKind::PermissionDenied {
            " (the panel user must be able to read the file: copy it into the data directory, e.g. /var/lib/dockwren, or run import as that user)"
        } else {
            ""
        };
        format!("{}: {e}{hint}", file.display())
    })?;
    import_bytes(instances_root, &data, pass, replace_panel)
}

/// Same as [`import`], from the bytes of an export. Panel settings that already exist are kept
/// unless `replace_panel` (a full server rebuild) is set.
pub fn import_bytes(
    instances_root: &Path,
    data: &[u8],
    pass: &str,
    replace_panel: bool,
) -> Result<Imported, String> {
    let b = open(data, pass)?;
    if let Some(f) = b.panel.keys().find(|f| !PANEL_FILES.contains(&f.as_str())) {
        return Err(format!("unexpected panel file {f:?} in the export"));
    }
    // Validate everything before writing anything.
    for (name, files) in &b.instances {
        if !crate::runner::valid_name(name) {
            return Err(format!("invalid instance name {name:?} in the file"));
        }
        if let Some(f) = files.keys().find(|f| !FILES.contains(&f.as_str())) {
            return Err(format!("{name}: unexpected file {f:?} in the export"));
        }
        for need in ["state.toml", "compose.yaml", ".env"] {
            if !files.contains_key(need) {
                return Err(format!("{name}: {need} missing in the export"));
            }
        }
    }
    std::fs::create_dir_all(instances_root).map_err(|e| e.to_string())?;
    let mut out = Imported {
        created: Vec::new(),
        skipped: Vec::new(),
        hosts: b.hosts.clone(),
        panel_restored: Vec::new(),
        panel_kept: Vec::new(),
    };
    for (name, files) in &b.instances {
        let dir = instances_root.join(name);
        if dir.exists() {
            out.skipped.push(name.clone());
            continue;
        }
        std::fs::create_dir(&dir).map_err(|e| format!("{name}: {e}"))?;
        for (f, content) in files {
            let mode = if f == ".env" { 0o600 } else { 0o640 };
            let mut h = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(dir.join(f))
                .map_err(|e| format!("{name}/{f}: {e}"))?;
            h.write_all(content.as_bytes())
                .map_err(|e| format!("{name}/{f}: {e}"))?;
        }
        out.created.push(name.clone());
    }
    if let Some(data) = instances_root.parent() {
        for (f, content) in &b.panel {
            let path = data.join(f);
            if path.exists() && !replace_panel {
                out.panel_kept.push(f.clone());
                continue;
            }
            let tmp = data.join(format!(".{f}.import"));
            let _ = std::fs::remove_file(&tmp);
            let mut h = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|e| format!("{f}: {e}"))?;
            h.write_all(content.as_bytes())
                .map_err(|e| format!("{f}: {e}"))?;
            std::fs::rename(&tmp, &path).map_err(|e| format!("{f}: {e}"))?;
            out.panel_restored.push(f.clone());
        }
        if replace_panel && out.panel_restored.iter().any(|f| f == "admin.hash") {
            // Sessions belong to the old login.
            let _ = std::fs::remove_file(data.join("sessions"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("dw-cfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let root = d.join("instances");
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn make_instance(root: &Path, name: &str) {
        let d = root.join(name);
        std::fs::create_dir_all(d.join("data")).unwrap();
        std::fs::write(d.join("state.toml"), "template = \"x\"\nversion = 1\n").unwrap();
        std::fs::write(d.join("compose.yaml"), "services: {}\n").unwrap();
        std::fs::write(d.join(".env"), "DB_PASSWORD='s3cret-value'\n").unwrap();
        std::fs::write(d.join("data/big.bin"), "not exported").unwrap();
    }

    #[test]
    fn export_import_round_trip_and_secrets_are_not_plain() {
        let a = tmp("a");
        make_instance(&a, "gt");
        let f = a.join("out.dwcfg");
        assert_eq!(export(&a, &f, "correct horse battery").unwrap(), 1);
        let raw = std::fs::read(&f).unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("s3cret-value"));
        let mode =
            std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&f).unwrap().permissions());
        assert_eq!(mode & 0o777, 0o600);
        assert!(
            export(&a, &f, "correct horse battery").is_err(),
            "no overwrite"
        );

        let b = tmp("b");
        assert!(import(&b, &f, "wrong passphrase here", false).is_err());
        let r = import(&b, &f, "correct horse battery", false).unwrap();
        assert_eq!(r.created, vec!["gt"]);
        assert_eq!(
            std::fs::read_to_string(b.join("gt/.env")).unwrap(),
            "DB_PASSWORD='s3cret-value'\n"
        );
        assert!(!b.join("gt/data").exists());
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(b.join("gt/.env")).unwrap().permissions(),
        );
        assert_eq!(mode & 0o777, 0o600);
        let again = import(&b, &f, "correct horse battery", false).unwrap();
        assert_eq!(again.skipped, vec!["gt"]);
        std::fs::remove_dir_all(a.parent().unwrap()).unwrap();
        std::fs::remove_dir_all(b.parent().unwrap()).unwrap();
    }

    #[test]
    fn bytes_round_trip_with_the_shorter_repository_password() {
        let a = tmp("d");
        make_instance(&a, "gt");
        assert!(export_bytes(&a, "8chars!!", MIN_PASSPHRASE).is_err());
        let (data, n) = export_bytes(&a, "8chars!!", 8).unwrap();
        assert_eq!(n, 1);
        let b = tmp("e");
        assert!(import_bytes(&b, &data, "wrong-pass", false).is_err());
        assert_eq!(
            import_bytes(&b, &data, "8chars!!", false).unwrap().created,
            vec!["gt"]
        );
        std::fs::remove_dir_all(a.parent().unwrap()).unwrap();
        std::fs::remove_dir_all(b.parent().unwrap()).unwrap();
    }

    #[test]
    fn short_passphrase_garbage_and_tampering_are_rejected() {
        let a = tmp("c");
        make_instance(&a, "gt");
        let f = a.join("o.dwcfg");
        assert!(export(&a, &f, "short").is_err());
        assert!(!f.exists());
        export(&a, &f, "long enough passphrase").unwrap();
        let mut raw = std::fs::read(&f).unwrap();
        let n = raw.len();
        raw[n - 1] ^= 1;
        assert!(open(&raw, "long enough passphrase").is_err());
        assert!(open(b"hello", "long enough passphrase").is_err());
        std::fs::remove_dir_all(a.parent().unwrap()).unwrap();
    }

    #[test]
    fn panel_settings_travel_and_existing_ones_are_kept_unless_replacing() {
        let a = tmp("p");
        make_instance(&a, "gt");
        let da = a.parent().unwrap();
        std::fs::write(da.join("admin.hash"), "HASH-A").unwrap();
        std::fs::write(da.join("totp.secret"), "TOTP-A").unwrap();
        std::fs::write(da.join("sessions"), "old").unwrap();
        std::fs::write(da.join("unrelated.txt"), "no").unwrap();
        let (data, _) = export_bytes(&a, "long enough passphrase", MIN_PASSPHRASE).unwrap();

        // Fresh server: everything absent is written, mode 0600, nothing else.
        let b = tmp("q");
        let db = b.parent().unwrap();
        let r = import_bytes(&b, &data, "long enough passphrase", false).unwrap();
        assert_eq!(r.panel_restored, vec!["admin.hash", "totp.secret"]);
        assert_eq!(
            std::fs::read_to_string(db.join("admin.hash")).unwrap(),
            "HASH-A"
        );
        assert!(!db.join("unrelated.txt").exists());
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(db.join("admin.hash"))
                .unwrap()
                .permissions(),
        );
        assert_eq!(mode & 0o777, 0o600);

        // Server with its own login: kept by default, replaced on a rebuild (sessions dropped).
        let c = tmp("r");
        let dc = c.parent().unwrap();
        std::fs::write(dc.join("admin.hash"), "HASH-NEW").unwrap();
        std::fs::write(dc.join("sessions"), "stale").unwrap();
        let r = import_bytes(&c, &data, "long enough passphrase", false).unwrap();
        assert_eq!(r.panel_kept, vec!["admin.hash"]);
        assert_eq!(
            std::fs::read_to_string(dc.join("admin.hash")).unwrap(),
            "HASH-NEW"
        );
        std::fs::remove_dir_all(c.join("gt")).unwrap();
        let r = import_bytes(&c, &data, "long enough passphrase", true).unwrap();
        assert!(r.panel_kept.is_empty());
        assert_eq!(
            std::fs::read_to_string(dc.join("admin.hash")).unwrap(),
            "HASH-A"
        );
        assert!(!dc.join("sessions").exists());

        for d in [da, db, dc] {
            std::fs::remove_dir_all(d).unwrap();
        }
    }
}
