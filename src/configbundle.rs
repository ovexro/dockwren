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
];
pub const MIN_PASSPHRASE: usize = 12;

#[derive(Serialize, Deserialize, Default)]
struct Bundle {
    /// instance name -> file name -> content
    instances: BTreeMap<String, BTreeMap<String, String>>,
    /// instance name -> public hostname (a note only: re-publish with `dockwren expose`)
    #[serde(default)]
    hosts: BTreeMap<String, String>,
}

fn key(pass: &str, salt: &[u8]) -> Result<ChaCha20Poly1305, String> {
    let mut k = [0u8; 32];
    Argon2::default()
        .hash_password_into(pass.as_bytes(), salt, &mut k)
        .map_err(|e| format!("key derivation: {e}"))?;
    ChaCha20Poly1305::new_from_slice(&k).map_err(|e| e.to_string())
}

fn seal(b: &Bundle, pass: &str) -> Result<Vec<u8>, String> {
    if pass.chars().count() < MIN_PASSPHRASE {
        return Err(format!("passphrase must be at least {MIN_PASSPHRASE} characters"));
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
    let b = collect(instances_root)?;
    if b.instances.is_empty() {
        return Err("no instances to export".into());
    }
    let data = seal(&b, pass)?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(out)
        .map_err(|e| format!("{}: {e}", out.display()))?;
    f.write_all(&data).map_err(|e| e.to_string())?;
    Ok(b.instances.len())
}

pub struct Imported {
    pub created: Vec<String>,
    pub skipped: Vec<String>,
    pub hosts: BTreeMap<String, String>,
}

/// Recreate the instance directories from an export. Existing instances are skipped.
/// Nothing is started: the caller brings each created instance up.
pub fn import(instances_root: &Path, file: &Path, pass: &str) -> Result<Imported, String> {
    let data = std::fs::read(file).map_err(|e| {
        let hint = if e.kind() == std::io::ErrorKind::PermissionDenied {
            " (the panel user must be able to read the file: copy it into the data directory, e.g. /var/lib/dockwren, or run import as that user)"
        } else {
            ""
        };
        format!("{}: {e}{hint}", file.display())
    })?;
    let b = open(&data, pass)?;
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
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("dw-cfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
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
        let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&f).unwrap().permissions());
        assert_eq!(mode & 0o777, 0o600);
        assert!(export(&a, &f, "correct horse battery").is_err(), "no overwrite");

        let b = tmp("b");
        assert!(import(&b, &f, "wrong passphrase here").is_err());
        let r = import(&b, &f, "correct horse battery").unwrap();
        assert_eq!(r.created, vec!["gt"]);
        assert_eq!(
            std::fs::read_to_string(b.join("gt/.env")).unwrap(),
            "DB_PASSWORD='s3cret-value'\n"
        );
        assert!(!b.join("gt/data").exists());
        let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(b.join("gt/.env")).unwrap().permissions());
        assert_eq!(mode & 0o777, 0o600);
        let again = import(&b, &f, "correct horse battery").unwrap();
        assert_eq!(again.skipped, vec!["gt"]);
        std::fs::remove_dir_all(&a).unwrap();
        std::fs::remove_dir_all(&b).unwrap();
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
        std::fs::remove_dir_all(&a).unwrap();
    }
}
