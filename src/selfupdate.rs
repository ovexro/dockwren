//! Update the panel itself to the latest GitHub release, checked against the release's SHA256SUMS.
//!
//! Two ways in, one code path (`root_update`):
//! * `sudo dockwren self-update` downloads, verifies, installs and restarts the service.
//! * The Settings button (after the CLI ran once and added a systemd drop-in) only writes a request
//!   file and exits; systemd restarts the service and the root-run `ExecStartPre=` hook
//!   (`dockwren self-update --on-start`) does the update before the new binary starts.
//!
//! The panel stays unprivileged and never hands a binary to root: root always downloads the release
//! itself and verifies the hash, so a compromised panel can at most ask for the newest real release.

use std::{
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use askama::Template;
use axum::{
    Form, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{auth::SharedAuth, release};

const DROPIN_DIR: &str = "/etc/systemd/system/dockwren.service.d";
const DROPIN_FILE: &str = "self-update.conf";
const DROPIN: &str = "[Service]\nTimeoutStartSec=300\nExecStartPre=-+/usr/local/bin/dockwren self-update --on-start\n";
const REQUEST: &str = "selfupdate.requested";
const FAILED: &str = "selfupdate.failed";
const BASE: &str = "https://github.com/ovexro/dockwren/releases/download";
const MAX_BYTES: u64 = 100 * 1024 * 1024;

fn arch() -> Option<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Some("x86_64"),
        "aarch64" => Some("aarch64"),
        _ => None,
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .fold(String::new(), |s, b| s + &format!("{b:02x}"))
}

/// The hash listed for `name` in `sha256sum` output (`<64 hex>  name` or `<hash> *name`).
fn hash_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (h, f) = l.split_once(char::is_whitespace)?;
        let f = f.trim().trim_start_matches('*');
        (f == name && h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| h.to_ascii_lowercase())
    })
}

async fn fetch(url: &str, max: u64) -> Result<Vec<u8>, String> {
    let out = tokio::process::Command::new("curl")
        .args([
            "-sS",
            "-f",
            "-L",
            "--proto",
            "=https",
            "--max-time",
            "120",
            "--max-filesize",
            &max.to_string(),
            "-A",
            concat!("dockwren/", env!("CARGO_PKG_VERSION")),
            url,
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("cannot run curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "download failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

/// Download `tag`'s binary for this CPU and return it only if the hash matches SHA256SUMS.
async fn download_verified(tag: &str) -> Result<Vec<u8>, String> {
    let a = arch().ok_or("unsupported CPU")?;
    let name = format!("dockwren-{a}-linux");
    let sums = fetch(&format!("{BASE}/{tag}/SHA256SUMS"), 1 << 20).await?;
    let want = hash_for(&String::from_utf8_lossy(&sums), &name)
        .ok_or_else(|| format!("{name} is not listed in SHA256SUMS"))?;
    let bin = fetch(&format!("{BASE}/{tag}/{name}"), MAX_BYTES).await?;
    if sha256_hex(&bin) != want {
        return Err("SHA256 mismatch: the download was not installed".into());
    }
    if bin.len() < 1_000_000 || !bin.starts_with(b"\x7fELF") {
        return Err("the download is not a Dockwren binary".into());
    }
    Ok(bin)
}

/// Put `bin` at `target` atomically, keeping the old one as `<target>.prev`.
fn install_binary(target: &Path, bin: &[u8]) -> Result<(), String> {
    let new = target.with_extension("new");
    let prev = target.with_extension("prev");
    let io = |e: std::io::Error| e.to_string();
    let _ = std::fs::remove_file(&new);
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(&new)
            .map_err(io)?;
        f.write_all(bin).map_err(io)?;
        f.sync_all().map_err(io)?;
    }
    let _ = std::fs::remove_file(&prev);
    if target.exists() {
        std::fs::hard_link(target, &prev).map_err(io)?;
    }
    std::fs::rename(&new, target).map_err(io)
}

/// Writes the drop-in that lets the Settings button work.
async fn install_dropin() -> Result<(), String> {
    let dir = Path::new(DROPIN_DIR);
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(DROPIN_FILE), DROPIN).map_err(|e| e.to_string())?;
    // Without a reload systemd would not run the hook until the next reboot.
    let st = tokio::process::Command::new("systemctl")
        .arg("daemon-reload")
        .status()
        .await
        .map_err(|e| e.to_string())?;
    st.success()
        .then_some(())
        .ok_or("systemctl daemon-reload failed".into())
}

/// What the root side did.
pub enum Outcome {
    UpToDate,
    Updated(String),
}

/// Download, verify and install the latest release (needs root for /usr/local/bin).
pub async fn root_update(target: &Path) -> Result<Outcome, String> {
    let tag = release::latest_tag().await?;
    if !release::is_newer(&tag, env!("CARGO_PKG_VERSION")) {
        return Ok(Outcome::UpToDate);
    }
    let bin = download_verified(&tag).await?;
    install_binary(target, &bin)?;
    Ok(Outcome::Updated(tag))
}

async fn restart_service() -> Result<(), String> {
    let st = tokio::process::Command::new("systemctl")
        .args(["restart", "dockwren.service"])
        .status()
        .await
        .map_err(|e| e.to_string())?;
    st.success()
        .then_some(())
        .ok_or("systemctl restart failed".into())
}

fn target_path() -> Result<PathBuf, String> {
    let p = std::env::current_exe().map_err(|e| e.to_string())?;
    if p.file_name().is_some_and(|n| n == "dockwren") && p.is_absolute() {
        Ok(p)
    } else {
        Err(format!("unexpected binary path {}", p.display()))
    }
}

/// `dockwren self-update` (interactive, root) and `--on-start` (systemd hook).
pub async fn cli(data_dir: &Path, on_start: bool) -> Result<(), String> {
    if on_start {
        // Only act when the panel asked; a plain start must stay instant and harmless.
        let req = data_dir.join(REQUEST);
        if std::fs::remove_file(&req).is_err() {
            return Ok(());
        }
        let res = match target_path() {
            Ok(t) => root_update(&t).await,
            Err(e) => Err(e),
        };
        let note = match &res {
            Ok(Outcome::Updated(t)) => {
                println!("self-update: installed {t}");
                None
            }
            Ok(Outcome::UpToDate) => Some("Already on the latest release.".to_string()),
            Err(e) => {
                println!("self-update failed: {e}");
                Some(e.clone())
            }
        };
        if let Some(n) = note {
            // create_new refuses a planted symlink; the file only carries text for the Settings page.
            let f = data_dir.join(FAILED);
            let _ = std::fs::remove_file(&f);
            if let Ok(mut h) = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&f)
            {
                let _ = h.write_all(n.as_bytes());
            }
        }
        return res.map(|_| ());
    }
    if !nix_is_root() {
        return Err("run as root: sudo dockwren self-update".into());
    }
    let target = target_path()?;
    match root_update(&target).await? {
        Outcome::UpToDate => println!(
            "Dockwren {} is the latest release.",
            env!("CARGO_PKG_VERSION")
        ),
        Outcome::Updated(t) => {
            println!(
                "Installed {t} (previous binary kept as {}.prev).",
                target.display()
            );
            restart_service()
                .await
                .map_err(|e| format!("{e}; restart it yourself"))?;
            println!("Service restarted.");
        }
    }
    install_dropin().await?;
    println!("The Update button in Settings > General is enabled.");
    Ok(())
}

fn nix_is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(2).map(|u| u == "0"))
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------- panel side

pub struct Control {
    dir: PathBuf,
    dropin: PathBuf,
    /// Tests turn the process exit off.
    exit_after: bool,
    requested: AtomicBool,
}

impl Control {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.to_path_buf(),
            dropin: Path::new(DROPIN_DIR).join(DROPIN_FILE),
            exit_after: true,
            requested: AtomicBool::new(false),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(dir: &Path, dropin: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            dropin: dropin.to_path_buf(),
            exit_after: false,
            requested: AtomicBool::new(false),
        }
    }

    fn installed(&self) -> bool {
        self.dropin.exists()
    }

    fn last_note(&self) -> Option<String> {
        let t = std::fs::read_to_string(self.dir.join(FAILED)).ok()?;
        Some(t.chars().take(300).collect())
    }

    fn request(&self) -> Result<(), String> {
        let _ = std::fs::remove_file(self.dir.join(FAILED));
        std::fs::write(self.dir.join(REQUEST), b"1").map_err(|e| e.to_string())
    }
}

#[derive(Clone)]
struct St {
    auth: SharedAuth,
    ctl: Arc<Control>,
}

pub fn router(auth: SharedAuth, ctl: Arc<Control>) -> Router {
    Router::new()
        .route("/settings/selfupdate", get(show))
        .route("/settings/selfupdate/run", post(run))
        .with_state(St { auth, ctl })
}

#[derive(Template)]
#[template(path = "selfupdate.html")]
struct View {
    current: &'static str,
    installed: bool,
    latest: Option<String>,
    newer: bool,
    note: Option<String>,
    two_factor: bool,
    error: Option<&'static str>,
    restarting: bool,
    checked: bool,
}

async fn view(s: &St, error: Option<&'static str>, restarting: bool, look: bool) -> Response {
    let current = env!("CARGO_PKG_VERSION");
    let latest = if restarting {
        None
    } else {
        release::latest_tag().await.ok()
    };
    let v = View {
        current,
        installed: s.ctl.installed(),
        newer: latest
            .as_deref()
            .is_some_and(|l| release::is_newer(l, current)),
        latest,
        note: s.ctl.last_note(),
        two_factor: s.auth.two_factor_enabled(),
        error,
        restarting,
        checked: look,
    };
    Html(v.render().expect("selfupdate renders")).into_response()
}

async fn show(State(s): State<St>, headers: HeaderMap) -> Response {
    if s.auth.session_of(&headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    view(&s, None, false, true).await
}

#[derive(Deserialize)]
struct RunForm {
    password: String,
    #[serde(default)]
    code: String,
}

async fn run(State(s): State<St>, headers: HeaderMap, Form(f): Form<RunForm>) -> Response {
    if s.auth.session_of(&headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !crate::same_origin(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !s.ctl.installed() {
        return view(
            &s,
            Some("Not set up yet: run sudo dockwren self-update once."),
            false,
            false,
        )
        .await;
    }
    if !s.auth.reauth(f.password, &f.code).await {
        return view(&s, Some("Wrong password or code."), false, false).await;
    }
    if s.ctl.requested.swap(true, Ordering::SeqCst) {
        return view(&s, None, true, false).await;
    }
    if let Err(e) = s.ctl.request() {
        s.ctl.requested.store(false, Ordering::SeqCst);
        eprintln!("selfupdate: cannot write the request: {e}");
        return view(
            &s,
            Some("Could not start the update (see the panel log)."),
            false,
            false,
        )
        .await;
    }
    eprintln!("selfupdate: update requested from Settings; restarting");
    if s.ctl.exit_after {
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            // Non-zero so systemd's Restart=on-failure brings the service back through the hook.
            std::process::exit(75);
        });
    }
    view(&s, None, true, false).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_hash_for_the_right_file() {
        let h1 = "a".repeat(64);
        let h2 = "B".repeat(64);
        let sums = format!("{h1}  dockwren-x86_64-linux\n{h2} *dockwren-aarch64-linux\n");
        assert_eq!(hash_for(&sums, "dockwren-x86_64-linux"), Some(h1.clone()));
        assert_eq!(
            hash_for(&sums, "dockwren-aarch64-linux"),
            Some("b".repeat(64))
        );
        assert_eq!(hash_for(&sums, "dockwren-riscv-linux"), None);
        assert_eq!(
            hash_for("short  dockwren-x86_64-linux", "dockwren-x86_64-linux"),
            None
        );
        assert_eq!(
            hash_for(
                &format!("{h1}  x-dockwren-x86_64-linux"),
                "dockwren-x86_64-linux"
            ),
            None
        );
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn install_swaps_atomically_and_keeps_the_previous() {
        let d = std::env::temp_dir().join(format!("dw-su-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let t = d.join("dockwren");
        std::fs::write(&t, b"old").unwrap();
        install_binary(&t, b"new!").unwrap();
        assert_eq!(std::fs::read(&t).unwrap(), b"new!");
        assert_eq!(std::fs::read(t.with_extension("prev")).unwrap(), b"old");
        assert!(!t.with_extension("new").exists());
        install_binary(&t, b"newer").unwrap();
        assert_eq!(std::fs::read(t.with_extension("prev")).unwrap(), b"new!");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[tokio::test]
    async fn on_start_without_a_request_does_nothing() {
        let d = std::env::temp_dir().join(format!("dw-su2-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert!(cli(&d, true).await.is_ok());
        assert!(!d.join(FAILED).exists());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
