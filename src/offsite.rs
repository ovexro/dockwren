//! Off-host backups: after each snapshot, the instance's backup folder is pushed to an
//! encrypted restic repository (SFTP or S3-compatible) by a throwaway `restic/restic`
//! container, so backups survive the loss of this disk. Nothing is installed on the host.
//!
//! Settings (including the credentials) live in `<data>/offsite.toml`, mode 0600. Working
//! files for the container (SSH key, pinned host key, restic cache) live in `<data>/offsite/`.
//! Secrets reach the container through environment variables (`-e NAME`, never on a command
//! line), and are masked in any error text.

use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// Pinned restic image; it also ships the `ssh` client SFTP needs.
pub const IMAGE: &str = "restic/restic:0.18.0";
/// Restic host name used for every snapshot, so "same backup as last time" detection works
/// no matter what the throwaway container is called.
const HOST: &str = "dockwren";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Off,
    Sftp,
    S3,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Off => "off",
            Kind::Sftp => "sftp",
            Kind::S3 => "s3",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Kind::Off),
            "sftp" => Some(Kind::Sftp),
            "s3" => Some(Kind::S3),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub kind: Kind,
    // S3-compatible
    pub endpoint: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    // SFTP
    pub host: String,
    pub port: u16,
    pub user: String,
    pub path: String,
    pub private_key: String,
    /// Restic repository password (encryption key).
    pub password: String,
}

/// What the settings form submits. Empty secret fields mean "keep the saved one".
#[derive(Default, Deserialize)]
pub struct Form {
    pub kind: String,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub bucket: String,
    #[serde(default)]
    pub access_key: String,
    #[serde(default)]
    pub secret_key: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub private_key: String,
    #[serde(default)]
    pub password: String,
}

fn plain(s: &str, extra: &[char]) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
}

impl Config {
    pub fn enabled(&self) -> bool {
        self.kind != Kind::Off
    }

    /// Validate a submitted form against the saved config. Returns the new config and whether a
    /// repository password was generated (to be shown to the user once).
    pub fn from_form(f: &Form, old: &Config) -> Result<(Config, bool), &'static str> {
        let kind = Kind::parse(&f.kind).ok_or("Pick off, SFTP or S3.")?;
        if kind == Kind::Off {
            // Keep the saved settings (and the password) so switching back on is one click.
            return Ok((
                Config {
                    kind,
                    ..old.clone()
                },
                false,
            ));
        }
        let keep = |new: &str, old: &str| {
            if new.trim().is_empty() {
                old.to_string()
            } else {
                new.trim().to_string()
            }
        };
        let mut c = Config {
            kind,
            password: keep(&f.password, &old.password),
            ..Config::default()
        };
        match kind {
            Kind::S3 => {
                let endpoint = f.endpoint.trim().trim_end_matches('/');
                if !(endpoint.starts_with("https://") || endpoint.starts_with("http://"))
                    || endpoint
                        .chars()
                        .any(|c| c.is_whitespace() || c == '\'' || c == '"')
                    || endpoint.len() > 200
                {
                    return Err("The endpoint must be a URL starting with https:// (or http://).");
                }
                if !plain(f.bucket.trim(), &['.', '-', '_', '/']) {
                    return Err("The bucket may only contain letters, digits and . - _ /");
                }
                c.endpoint = endpoint.to_string();
                c.bucket = f.bucket.trim().trim_matches('/').to_string();
                c.access_key = keep(&f.access_key, &old.access_key);
                c.secret_key = keep(&f.secret_key, &old.secret_key);
                if c.access_key.is_empty() || c.secret_key.is_empty() {
                    return Err("Enter the access key and the secret key.");
                }
                if c.access_key.chars().any(char::is_whitespace)
                    || c.secret_key.chars().any(char::is_whitespace)
                {
                    return Err("Keys cannot contain spaces.");
                }
            }
            Kind::Sftp => {
                if !plain(f.host.trim(), &['.', '-']) {
                    return Err("The host may only contain letters, digits, . and -");
                }
                let port: u16 = if f.port.trim().is_empty() {
                    22
                } else {
                    f.port
                        .trim()
                        .parse()
                        .ok()
                        .filter(|p| *p > 0)
                        .ok_or("The port must be 1 to 65535.")?
                };
                if !plain(f.user.trim(), &['.', '-', '_']) {
                    return Err("The user may only contain letters, digits, . - _");
                }
                let path = f.path.trim();
                if !plain(path, &['.', '-', '_', '/']) || path.contains("..") {
                    return Err("The folder may only contain letters, digits, . - _ /");
                }
                c.host = f.host.trim().to_string();
                c.port = port;
                c.user = f.user.trim().to_string();
                c.path = path.to_string();
                let key = f.private_key.trim();
                c.private_key = if key.is_empty() {
                    old.private_key.clone()
                } else {
                    if !key.starts_with("-----BEGIN ") || !key.contains("PRIVATE KEY-----") {
                        return Err("Paste the whole private key, starting with -----BEGIN.");
                    }
                    format!("{key}\n")
                };
                if c.private_key.is_empty() {
                    return Err("Paste the SSH private key for this account.");
                }
            }
            Kind::Off => unreachable!(),
        }
        let generated = c.password.is_empty();
        if generated {
            c.password = crate::auth::generate_password();
        } else if c.password.len() < 8 {
            return Err("The repository password must be at least 8 characters.");
        }
        Ok((c, generated))
    }

    /// Restic repository string.
    pub fn repo(&self) -> String {
        match self.kind {
            Kind::S3 => format!("s3:{}/{}/dockwren", self.endpoint, self.bucket),
            Kind::Sftp => format!("sftp:{}@{}:{}", self.user, self.host, self.path),
            Kind::Off => String::new(),
        }
    }

    /// Short human description, free of secrets.
    pub fn describe(&self) -> String {
        match self.kind {
            Kind::S3 => format!("{}/{}", self.endpoint, self.bucket),
            Kind::Sftp => format!("{}@{}:{}", self.user, self.host, self.path),
            Kind::Off => "off".into(),
        }
    }

    /// Words of every secret, for masking error text.
    fn secrets(&self) -> Vec<&str> {
        [&self.password, &self.secret_key, &self.access_key]
            .into_iter()
            .filter(|s| !s.is_empty())
            .map(String::as_str)
            .collect()
    }

    /// `-o sftp.command=...` value: pinned host key (trusted on first use), key file from /state.
    fn sftp_command(&self) -> String {
        format!(
            "ssh -p {} -i /state/id_key -o IdentitiesOnly=yes -o BatchMode=yes \
             -o UserKnownHostsFile=/state/known_hosts -o StrictHostKeyChecking=accept-new \
             {}@{} -s sftp",
            self.port, self.user, self.host
        )
    }
}

pub fn mask(text: &str, secrets: &[&str]) -> String {
    let mut out = text.to_string();
    for s in secrets {
        out = out.replace(s, "***");
    }
    out
}

/// Last few useful lines of restic/docker output for an error message.
fn tail(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let start = lines.len().saturating_sub(4);
    lines[start..].join("; ")
}

fn write_private(path: &Path, content: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(content)?;
    std::fs::rename(&tmp, path)
}

pub struct Offsite {
    data_dir: Option<PathBuf>,
    cfg: Mutex<Config>,
}

impl Offsite {
    /// In-memory only (tests, or no data directory): never runs anything.
    pub fn memory() -> Self {
        Self {
            data_dir: None,
            cfg: Mutex::new(Config::default()),
        }
    }

    pub fn open(data_dir: &Path) -> Self {
        let cfg = std::fs::read_to_string(data_dir.join("offsite.toml"))
            .ok()
            .and_then(|t| toml::from_str::<Config>(&t).ok())
            .unwrap_or_default();
        Self {
            data_dir: Some(data_dir.to_path_buf()),
            cfg: Mutex::new(cfg),
        }
    }

    pub fn get(&self) -> Config {
        self.cfg.lock().unwrap().clone()
    }

    pub fn set(&self, c: Config) -> Result<(), String> {
        if let Some(d) = &self.data_dir {
            let text = toml::to_string(&c).map_err(|e| e.to_string())?;
            write_private(&d.join("offsite.toml"), text.as_bytes())
                .map_err(|e| format!("cannot save: {e}"))?;
        }
        *self.cfg.lock().unwrap() = c;
        Ok(())
    }

    /// Epoch seconds of the last successful upload (0 = never).
    pub fn last_ok(&self) -> u64 {
        self.data_dir
            .as_ref()
            .and_then(|d| std::fs::read_to_string(d.join("offsite.last")).ok())
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(0)
    }

    fn mark_ok(&self) {
        if let Some(d) = &self.data_dir {
            let _ = write_private(
                &d.join("offsite.last"),
                crate::schedule::now().to_string().as_bytes(),
            );
        }
    }

    fn data_dir(&self) -> Result<&Path, String> {
        self.data_dir
            .as_deref()
            .ok_or_else(|| "no data directory".to_string())
    }

    /// Run restic (or, with `shell`, a `sh -c` script that calls it) in a throwaway container.
    /// `binds` are extra `host:container[:ro]` mounts. Returns stdout; errors carry masked stderr.
    async fn run(
        &self,
        cfg: &Config,
        binds: &[String],
        shell: Option<&[String]>,
        args: &[String],
        limit: Duration,
    ) -> Result<String, String> {
        let data = self.data_dir()?;
        let state = data.join("offsite");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&state)
            .map_err(|e| format!("{}: {e}", state.display()))?;
        if cfg.kind == Kind::Sftp {
            write_private(&state.join("id_key"), cfg.private_key.as_bytes())
                .map_err(|e| format!("id_key: {e}"))?;
            let kh = state.join("known_hosts");
            if !kh.exists() {
                write_private(&kh, b"").map_err(|e| format!("known_hosts: {e}"))?;
            }
        }
        let mut cmd = Command::new("docker");
        if std::env::var_os("DOCKER_CONFIG").is_none() {
            cmd.env("DOCKER_CONFIG", data.join("docker-config"));
        }
        cmd.env("RESTIC_PASSWORD", &cfg.password);
        cmd.args([
            "run",
            "--rm",
            "--security-opt",
            "no-new-privileges",
            "--user",
            "0",
            "-e",
            "RESTIC_PASSWORD",
            "-e",
            "RESTIC_CACHE_DIR=/state/cache",
        ]);
        if cfg.kind == Kind::S3 {
            cmd.env("AWS_ACCESS_KEY_ID", &cfg.access_key)
                .env("AWS_SECRET_ACCESS_KEY", &cfg.secret_key)
                .args(["-e", "AWS_ACCESS_KEY_ID", "-e", "AWS_SECRET_ACCESS_KEY"]);
        }
        cmd.arg("-v").arg(format!("{}:/state", state.display()));
        for b in binds {
            cmd.arg("-v").arg(b);
        }
        let mut restic: Vec<String> = vec!["-r".into(), cfg.repo()];
        if cfg.kind == Kind::Sftp {
            restic.push("-o".into());
            restic.push(format!("sftp.command={}", cfg.sftp_command()));
        }
        match shell {
            // `sh -c 'script' sh <extra...>`; the script uses `restic "$@"`-style positional args
            // that the caller placed after the restic prefix, so quoting never matters.
            Some(extra) => {
                cmd.args(["--entrypoint", "sh", IMAGE, "-c"]);
                cmd.arg(&extra[0]).arg("sh");
                cmd.args(&extra[1..]);
                cmd.args(&restic).args(args);
            }
            None => {
                cmd.arg(IMAGE).args(&restic).args(args);
            }
        }
        cmd.kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let secrets = cfg.secrets();
        let out = match tokio::time::timeout(limit, cmd.output()).await {
            Err(_) => {
                return Err(
                    "timed out: check the address, the bucket or folder, and the credentials"
                        .into(),
                );
            }
            Ok(Err(e)) => return Err(format!("cannot run docker: {e}")),
            Ok(Ok(o)) => o,
        };
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if out.status.success() {
            Ok(mask(&stdout, &secrets))
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            Err(mask(&tail(&err), &secrets))
        }
    }

    /// Make sure the repository exists and the credentials work; creates it when it is missing.
    pub async fn ensure_repo(&self, cfg: &Config) -> Result<(), String> {
        let t = Duration::from_secs(120);
        match self
            .run(cfg, &[], None, &["cat".into(), "config".into()], t)
            .await
        {
            Ok(_) => Ok(()),
            Err(e)
                if e.contains("unable to open config file")
                    || e.contains("Is there a repository") =>
            {
                self.run(cfg, &[], None, &["init".into()], t)
                    .await
                    .map(|_| ())
            }
            Err(e) => Err(e),
        }
    }

    /// Push `<backups_root>/<name>` and trim the remote to the newest `keep` points in time.
    pub async fn push(
        &self,
        backups_root: &Path,
        name: &str,
        keep: usize,
        log: &mut (dyn FnMut(&str) + Send),
    ) -> Result<(), String> {
        let cfg = self.get();
        if !cfg.enabled() {
            return Ok(());
        }
        if !crate::runner::valid_name(name) || !backups_root.join(name).is_dir() {
            return Err(format!("no local backups for {name}"));
        }
        log(&format!("uploading to {}", cfg.describe()));
        self.ensure_repo(&cfg).await?;
        let bind = format!("{}:/backups:ro", backups_root.display());
        let out = self
            .run(
                &cfg,
                std::slice::from_ref(&bind),
                None,
                &[
                    "backup".into(),
                    format!("/backups/{name}"),
                    "--tag".into(),
                    name.into(),
                    "--host".into(),
                    HOST.into(),
                ],
                Duration::from_secs(6 * 3600),
            )
            .await?;
        if let Some(l) = out.lines().rev().find(|l| l.starts_with("snapshot ")) {
            log(l);
        }
        self.run(
            &cfg,
            &[bind],
            None,
            &[
                "forget".into(),
                "--tag".into(),
                name.into(),
                "--host".into(),
                HOST.into(),
                "--keep-last".into(),
                keep.to_string(),
                "--prune".into(),
                "--retry-lock".into(),
                "5m".into(),
            ],
            Duration::from_secs(3600),
        )
        .await?;
        self.mark_ok();
        log("off-host copy done");
        Ok(())
    }

    /// Bring the newest off-host copy of `name` back into `<backups_root>/<name>/` (existing
    /// snapshots are never overwritten). Returns the snapshot names that were added; then
    /// `dockwren restore` works as usual.
    pub async fn pull(
        &self,
        backups_root: &Path,
        name: &str,
        log: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Vec<String>, String> {
        let cfg = self.get();
        if !cfg.enabled() {
            return Err("no off-host target is configured".into());
        }
        if !crate::runner::valid_name(name) {
            return Err("invalid app name".into());
        }
        let data = self.data_dir()?;
        let out_dir = data.join("offsite").join(format!("pull-{name}"));
        let _ = std::fs::remove_dir_all(&out_dir);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&out_dir)
            .map_err(|e| format!("{}: {e}", out_dir.display()))?;
        let meta = std::fs::metadata(data).map_err(|e| e.to_string())?;
        log(&format!("downloading from {}", cfg.describe()));
        // Restic runs as root in the container; hand the result back to the data dir's owner.
        let script = "owner=\"$1\"; shift; restic \"$@\" && chown -R \"$owner\" /out";
        let extra = vec![script.to_string(), format!("{}:{}", meta.uid(), meta.gid())];
        let res = self
            .run(
                &cfg,
                &[format!("{}:/out", out_dir.display())],
                Some(&extra),
                &[
                    "restore".into(),
                    "latest".into(),
                    "--tag".into(),
                    name.into(),
                    "--target".into(),
                    "/out".into(),
                ],
                Duration::from_secs(6 * 3600),
            )
            .await;
        let added = res.and_then(|_| {
            let src = out_dir.join("backups").join(name);
            let dst = backups_root.join(name);
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dst)
                .map_err(|e| format!("{}: {e}", dst.display()))?;
            let mut added = Vec::new();
            let entries = std::fs::read_dir(&src)
                .map_err(|_| format!("the off-host copy has no backups for {name}"))?;
            for e in entries.flatten() {
                let Ok(n) = e.file_name().into_string() else {
                    continue;
                };
                if !crate::backup::is_snapshot_name(&n) || dst.join(&n).exists() {
                    continue;
                }
                std::fs::rename(e.path(), dst.join(&n)).map_err(|e| format!("{n}: {e}"))?;
                added.push(n);
            }
            added.sort();
            Ok(added)
        });
        let _ = std::fs::remove_dir_all(&out_dir);
        let added = added?;
        log(&format!("added {} snapshot(s)", added.len()));
        Ok(added)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(kind: &str) -> Form {
        Form {
            kind: kind.into(),
            ..Form::default()
        }
    }

    fn s3_form() -> Form {
        Form {
            endpoint: "https://s3.example.test/".into(),
            bucket: "my-bucket".into(),
            access_key: "AKIAEXAMPLE".into(),
            secret_key: "sekret-value".into(),
            ..form("s3")
        }
    }

    /// Not a real key; assembled so secret scanners do not flag the fixture.
    fn key() -> String {
        format!(
            "-----BEGIN {0} PRIVATE KEY-----\nabc\n-----END {0} PRIVATE KEY-----",
            "OPENSSH"
        )
    }

    fn sftp_form() -> Form {
        Form {
            host: "backup.example.test".into(),
            user: "dw".into(),
            path: "/srv/dockwren".into(),
            private_key: key(),
            ..form("sftp")
        }
    }

    #[test]
    fn s3_form_builds_a_repo_and_generates_a_password() {
        let (c, generated) = Config::from_form(&s3_form(), &Config::default()).unwrap();
        assert!(generated);
        assert_eq!(c.password.len(), 24);
        assert_eq!(c.repo(), "s3:https://s3.example.test/my-bucket/dockwren");
        assert!(!c.describe().contains("sekret"));
    }

    #[test]
    fn blank_secrets_keep_the_saved_ones() {
        let (old, _) = Config::from_form(&s3_form(), &Config::default()).unwrap();
        let f = Form {
            secret_key: String::new(),
            access_key: String::new(),
            password: String::new(),
            ..s3_form()
        };
        let (c, generated) = Config::from_form(&f, &old).unwrap();
        assert!(!generated);
        assert_eq!(c.secret_key, "sekret-value");
        assert_eq!(c.password, old.password);
    }

    #[test]
    fn typed_password_is_used_for_an_existing_repository() {
        let f = Form {
            password: "my-old-repo-password".into(),
            ..s3_form()
        };
        let (c, generated) = Config::from_form(&f, &Config::default()).unwrap();
        assert!(!generated);
        assert_eq!(c.password, "my-old-repo-password");
        let short = Form {
            password: "short".into(),
            ..s3_form()
        };
        assert!(Config::from_form(&short, &Config::default()).is_err());
    }

    #[test]
    fn sftp_form_is_validated() {
        let (c, _) = Config::from_form(&sftp_form(), &Config::default()).unwrap();
        assert_eq!(c.port, 22);
        assert_eq!(c.repo(), "sftp:dw@backup.example.test:/srv/dockwren");
        assert!(c.sftp_command().contains("-p 22"));
        assert!(c.private_key.ends_with("-----\n"));
        for bad in [
            Form {
                host: "a b".into(),
                ..sftp_form()
            },
            Form {
                user: "x;rm".into(),
                ..sftp_form()
            },
            Form {
                path: "../etc".into(),
                ..sftp_form()
            },
            Form {
                path: "/a b".into(),
                ..sftp_form()
            },
            Form {
                port: "0".into(),
                ..sftp_form()
            },
            Form {
                port: "x".into(),
                ..sftp_form()
            },
            Form {
                private_key: "not a key".into(),
                ..sftp_form()
            },
            Form {
                private_key: String::new(),
                ..sftp_form()
            },
        ] {
            assert!(Config::from_form(&bad, &Config::default()).is_err());
        }
    }

    #[test]
    fn s3_form_rejects_bad_input() {
        for bad in [
            Form {
                endpoint: "ftp://x".into(),
                ..s3_form()
            },
            Form {
                endpoint: "https://a b".into(),
                ..s3_form()
            },
            Form {
                bucket: "a b".into(),
                ..s3_form()
            },
            Form {
                bucket: String::new(),
                ..s3_form()
            },
            Form {
                access_key: String::new(),
                ..s3_form()
            },
            form("tape"),
        ] {
            assert!(Config::from_form(&bad, &Config::default()).is_err());
        }
    }

    #[test]
    fn turning_off_keeps_the_saved_settings() {
        let (on, _) = Config::from_form(&s3_form(), &Config::default()).unwrap();
        let (off, _) = Config::from_form(&form("off"), &on).unwrap();
        assert!(!off.enabled());
        assert_eq!(off.password, on.password);
        assert_eq!(off.endpoint, on.endpoint);
    }

    #[test]
    fn secrets_are_masked_in_errors() {
        let (c, _) = Config::from_form(&s3_form(), &Config::default()).unwrap();
        let msg = format!("denied for sekret-value / {} / AKIAEXAMPLE", c.password);
        let m = mask(&msg, &c.secrets());
        assert!(!m.contains("sekret") && !m.contains("AKIA") && !m.contains(&c.password));
    }

    #[test]
    fn config_persists_with_private_mode() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("dw-offsite-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let o = Offsite::open(&d);
        assert!(!o.get().enabled());
        let (c, _) = Config::from_form(&s3_form(), &Config::default()).unwrap();
        o.set(c.clone()).unwrap();
        assert_eq!(Offsite::open(&d).get(), c);
        let mode = std::fs::metadata(d.join("offsite.toml"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
