//! Install OS package updates from the panel (opt-in, off by default).
//!
//! The panel itself stays sandboxed and unprivileged: it cannot run `apt-get`. Instead it drops
//! a trigger file in its own data directory. A root-owned systemd path unit
//! (`dockwren-osupdate.path`, installed once by `dockwren setup-osupdate`) sees the file and starts
//! `dockwren-osupdate.service`, which runs `apt-get update && apt-get upgrade` and writes a log the
//! panel only reads. No sudo, no polkit, and no user name appears in any rule: the panel just
//! needs write access to its own data directory, which it always has.
//!
//! Every start needs a session, a same-origin request, and the password (plus a 2FA code when on).

use std::{
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
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

use crate::{
    auth::SharedAuth,
    hostupdates,
    notify::{self, NewEvent, Severity},
};

const ENABLED_FILE: &str = "osupdate.enabled";
const TRIGGER_FILE: &str = "osupdate.trigger";
const LOG_PATH: &str = "/var/log/dockwren-osupdate.log";
const UNIT_DIR: &str = "/etc/systemd/system";
const SCRIPT_DIR: &str = "/usr/local/lib/dockwren";
const SCRIPT_NAME: &str = "osupdate.sh";
const PATH_UNIT: &str = "dockwren-osupdate.path";
const SERVICE_UNIT: &str = "dockwren-osupdate.service";
/// A log that has not grown for this long without an exit line means the run died.
const STALE: Duration = Duration::from_secs(30 * 60);
const TAIL_BYTES: u64 = 64 * 1024;
const SHOW_LINES: usize = 150;
const WATCH_EVERY: Duration = Duration::from_secs(5);

/// What the root helper is doing, judged from the trigger file and the log.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Run {
    Idle,
    /// Trigger written, helper not started yet.
    Queued,
    Running,
    /// Ended with this exit code of apt-get.
    Finished(i32),
    /// The log stopped growing without an exit line (reboot, crash).
    Interrupted,
}

impl Run {
    pub fn active(self) -> bool {
        matches!(self, Run::Queued | Run::Running)
    }
}

fn exit_code(log: &str) -> Option<i32> {
    log.lines()
        .rev()
        .find(|l| !l.trim().is_empty())?
        .strip_prefix("== exit ")?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Pure judgement so it can be tested without a filesystem.
pub fn judge(trigger: bool, log: Option<(&str, Duration)>) -> Run {
    if trigger {
        return Run::Queued;
    }
    let Some((text, age)) = log else {
        return Run::Idle;
    };
    if let Some(code) = exit_code(text) {
        return Run::Finished(code);
    }
    if text.trim().is_empty() {
        return Run::Idle;
    }
    if age < STALE {
        Run::Running
    } else {
        Run::Interrupted
    }
}

/// dpkg prints its progress percentages without newlines; show them as one short line.
fn tidy(line: &str) -> String {
    if line.starts_with("(Reading database") {
        "(Reading database …)".to_string()
    } else {
        line.to_string()
    }
}

/// Identifies one finished run (its exit line carries a timestamp) so the watcher handles each
/// result exactly once, even when the run was too short to be seen while active.
fn run_id(run: Run, log: &str) -> Option<String> {
    match run {
        Run::Finished(_) => log
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(str::to_string),
        Run::Interrupted => Some(format!("interrupted:{}", log.len())),
        _ => None,
    }
}

/// First `E:` line apt printed, else a generic hint.
fn summary(log: &str) -> String {
    log.lines()
        .find(|l| l.starts_with("E: "))
        .map(|l| l.chars().take(300).collect())
        .unwrap_or_else(|| "See the log under Settings, OS updates.".to_string())
}

#[derive(Debug)]
pub enum StartError {
    Busy,
    Io(std::io::Error),
}

/// Paths the panel works with; injectable for tests.
pub struct Control {
    dir: Option<PathBuf>,
    log: PathBuf,
    unit: PathBuf,
}

impl Control {
    pub fn new(data_dir: Option<&Path>) -> Self {
        Self {
            dir: data_dir.map(Path::to_path_buf),
            log: PathBuf::from(LOG_PATH),
            unit: Path::new(UNIT_DIR).join(PATH_UNIT),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_paths(dir: &Path, log: &Path, unit: &Path) -> Self {
        Self {
            dir: Some(dir.to_path_buf()),
            log: log.to_path_buf(),
            unit: unit.to_path_buf(),
        }
    }

    /// The root helper is set up (the path unit exists).
    pub fn installed(&self) -> bool {
        self.unit.exists()
    }

    pub fn enabled(&self) -> bool {
        self.dir
            .as_ref()
            .is_some_and(|d| d.join(ENABLED_FILE).exists())
    }

    fn set_enabled(&self, on: bool) -> std::io::Result<()> {
        let Some(d) = &self.dir else {
            return Err(std::io::Error::other("no data directory"));
        };
        let f = d.join(ENABLED_FILE);
        if on {
            std::fs::write(f, "1\n")
        } else {
            match std::fs::remove_file(f) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        }
    }

    /// Last 64 KB of the log and how long ago it changed.
    fn read_log(&self) -> Option<(String, Duration)> {
        let mut f = std::fs::File::open(&self.log).ok()?;
        let meta = f.metadata().ok()?;
        let age = meta
            .modified()
            .ok()
            .and_then(|m| SystemTime::now().duration_since(m).ok())
            .unwrap_or_default();
        let len = meta.len();
        if len > TAIL_BYTES {
            f.seek(SeekFrom::Start(len - TAIL_BYTES)).ok()?;
        }
        let mut buf = Vec::new();
        f.take(TAIL_BYTES).read_to_end(&mut buf).ok()?;
        Some((String::from_utf8_lossy(&buf).into_owned(), age))
    }

    pub fn state(&self) -> (Run, String) {
        let trigger = self
            .dir
            .as_ref()
            .is_some_and(|d| d.join(TRIGGER_FILE).exists());
        let log = self.read_log();
        let run = judge(trigger, log.as_ref().map(|(t, a)| (t.as_str(), *a)));
        (run, log.map(|(t, _)| t).unwrap_or_default())
    }

    fn start(&self) -> Result<(), StartError> {
        if self.state().0.active() {
            return Err(StartError::Busy);
        }
        let Some(d) = &self.dir else {
            return Err(StartError::Io(std::io::Error::other("no data directory")));
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(d.join(TRIGGER_FILE))
        {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(StartError::Busy),
            Err(e) => Err(StartError::Io(e)),
        }
    }
}

// ---------- setup (root, one time) ----------

/// The helper script the root unit runs. It ignores the trigger's content: the only thing the
/// unprivileged panel can do is make it start.
fn script() -> String {
    r#"#!/bin/sh
# Installed by `dockwren setup-osupdate`. Runs as root, started by dockwren-osupdate.path.
TRIGGER="$1"
LOG=/var/log/dockwren-osupdate.log
exec >"$LOG" 2>&1
echo "== started $(date -u +%Y-%m-%dT%H:%M:%SZ)"
rm -f -- "$TRIGGER"
export DEBIAN_FRONTEND=noninteractive LC_ALL=C NEEDRESTART_SUSPEND=1
apt-get -q -o DPkg::Lock::Timeout=300 update &&
apt-get -q -y -o DPkg::Lock::Timeout=300 \
  -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold full-upgrade
rc=$?
echo "== exit $rc $(date -u +%Y-%m-%dT%H:%M:%SZ)"
exit 0
"#
    .to_string()
}

fn path_unit(data_dir: &Path) -> String {
    format!(
        "[Unit]\nDescription=Start Dockwren OS updates when the panel asks\n\n[Path]\nPathExists={}\nUnit={SERVICE_UNIT}\n\n[Install]\nWantedBy=multi-user.target\n",
        data_dir.join(TRIGGER_FILE).display()
    )
}

fn service_unit(data_dir: &Path) -> String {
    format!(
        "[Unit]\nDescription=Dockwren OS package update (started from the panel)\n\n[Service]\nType=oneshot\nExecStart={SCRIPT_DIR}/{SCRIPT_NAME} {}\nTimeoutStartSec=2h\n",
        data_dir.join(TRIGGER_FILE).display()
    )
}

/// `dockwren setup-osupdate`: write the helper and the two units, then enable the path unit.
pub fn setup(data_dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let s = data_dir.display().to_string();
    if !data_dir.is_absolute() || !data_dir.is_dir() {
        return Err(format!(
            "{s} is not an existing absolute directory; pass --data-dir /path/to/dockwren-data"
        ));
    }
    if s.chars().any(|c| c.is_whitespace() || c == '%' || c == '"') {
        return Err("the data directory path must not contain spaces, % or quotes".into());
    }
    let write = |path: PathBuf, body: String, mode: u32| -> Result<(), String> {
        std::fs::write(&path, body)
            .map_err(|e| format!("cannot write {}: {e} (run as root)", path.display()))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("cannot chmod {}: {e}", path.display()))
    };
    std::fs::create_dir_all(SCRIPT_DIR)
        .map_err(|e| format!("cannot create {SCRIPT_DIR}: {e} (run as root)"))?;
    write(Path::new(SCRIPT_DIR).join(SCRIPT_NAME), script(), 0o755)?;
    write(
        Path::new(UNIT_DIR).join(SERVICE_UNIT),
        service_unit(data_dir),
        0o644,
    )?;
    write(
        Path::new(UNIT_DIR).join(PATH_UNIT),
        path_unit(data_dir),
        0o644,
    )?;
    let ctl = |args: &[&str]| -> Result<(), String> {
        let st = std::process::Command::new("systemctl")
            .args(args)
            .status()
            .map_err(|e| format!("cannot run systemctl: {e}"))?;
        if st.success() {
            Ok(())
        } else {
            Err(format!("systemctl {} failed", args.join(" ")))
        }
    };
    ctl(&["daemon-reload"])?;
    ctl(&["enable", "--now", PATH_UNIT])?;
    Ok(())
}

// ---------- HTTP ----------

#[derive(Clone)]
struct OsState {
    auth: SharedAuth,
    notify: notify::Shared,
    ctl: Arc<Control>,
}

pub fn router(auth: SharedAuth, notify: notify::Shared, ctl: Arc<Control>) -> Router {
    Router::new()
        .route("/settings/osupdates", get(show).post(toggle))
        .route("/host/os-updates/run", post(run))
        .route("/host/os-updates/log", get(log_fragment))
        .with_state(OsState { auth, notify, ctl })
}

#[derive(Template)]
#[template(path = "osupdate.html")]
struct SettingTemplate {
    installed: bool,
    on: bool,
    pending: Option<usize>,
    two_factor: bool,
    error: Option<&'static str>,
}

#[derive(Template)]
#[template(path = "osupdate_log.html")]
struct LogTemplate {
    active: bool,
    queued: bool,
    code: Option<i32>,
    interrupted: bool,
    lines: Vec<String>,
}

async fn setting_html(s: &OsState, error: Option<&'static str>) -> Response {
    let installed = s.ctl.installed();
    let on = s.ctl.enabled();
    let pending = if installed && on {
        hostupdates::pending_count().await
    } else {
        None
    };
    let t = SettingTemplate {
        installed,
        on,
        pending,
        two_factor: s.auth.two_factor_enabled(),
        error,
    };
    Html(t.render().expect("osupdate setting renders")).into_response()
}

async fn show(State(s): State<OsState>, headers: HeaderMap) -> Response {
    if s.auth.session_of(&headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    setting_html(&s, None).await
}

async fn log_fragment(State(s): State<OsState>, headers: HeaderMap) -> Response {
    if s.auth.session_of(&headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let (run, text) = s.ctl.state();
    let lines: Vec<String> = {
        let all: Vec<&str> = text.lines().collect();
        all[all.len().saturating_sub(SHOW_LINES)..]
            .iter()
            .map(|l| tidy(l))
            .collect()
    };
    let t = LogTemplate {
        active: run.active(),
        queued: run == Run::Queued,
        code: match run {
            Run::Finished(c) => Some(c),
            _ => None,
        },
        interrupted: run == Run::Interrupted,
        lines,
    };
    Html(t.render().expect("osupdate log renders")).into_response()
}

#[derive(Deserialize)]
struct ToggleForm {
    on: String,
    password: String,
    #[serde(default)]
    code: String,
}

async fn toggle(
    State(s): State<OsState>,
    headers: HeaderMap,
    Form(form): Form<ToggleForm>,
) -> Response {
    if s.auth.session_of(&headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !crate::same_origin(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !s.ctl.installed() {
        return setting_html(&s, Some("The helper is not set up yet.")).await;
    }
    if !s.auth.reauth(form.password, &form.code).await {
        return setting_html(&s, Some("Wrong password or code.")).await;
    }
    let on = form.on == "1";
    if let Err(e) = s.ctl.set_enabled(on) {
        eprintln!("osupdate: cannot change setting: {e}");
        return setting_html(&s, Some("Could not save the setting (see the panel log).")).await;
    }
    eprintln!(
        "osupdate: turned {} from Settings",
        if on { "on" } else { "off" }
    );
    setting_html(&s, None).await
}

#[derive(Deserialize)]
struct RunForm {
    password: String,
    #[serde(default)]
    code: String,
}

async fn run(State(s): State<OsState>, headers: HeaderMap, Form(form): Form<RunForm>) -> Response {
    if s.auth.session_of(&headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !crate::same_origin(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !s.ctl.installed() || !s.ctl.enabled() {
        return setting_html(
            &s,
            Some("Installing OS updates from the panel is turned off."),
        )
        .await;
    }
    if !s.auth.reauth(form.password, &form.code).await {
        return setting_html(&s, Some("Wrong password or code.")).await;
    }
    match s.ctl.start() {
        Ok(()) => {
            eprintln!("osupdate: update started from the panel");
            notify::raise(
                &s.notify,
                NewEvent::new(
                    "host",
                    "host.os_update_run",
                    Severity::Info,
                    "OS update started from the panel",
                )
                .key(format!("host.os_update_run:{}", tag())),
            );
            setting_html(&s, None).await
        }
        Err(StartError::Busy) => setting_html(&s, Some("An update is already running.")).await,
        Err(StartError::Io(e)) => {
            eprintln!("osupdate: cannot start: {e}");
            setting_html(&s, Some("Could not start the update (see the panel log).")).await
        }
    }
}

fn tag() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// Notices when a run ends: raises `job.failed` on failure, clears it on success, and looks for
/// pending updates again so the "updates available" notice closes itself.
pub fn spawn_watcher(ctl: Arc<Control>, shared: notify::Shared) {
    tokio::spawn(async move {
        // Results already there at startup were handled by an earlier panel process.
        let mut handled = {
            let (run, text) = ctl.state();
            run_id(run, &text)
        };
        loop {
            tokio::time::sleep(WATCH_EVERY).await;
            if !ctl.installed() {
                continue;
            }
            let (run, text) = ctl.state();
            let Some(id) = run_id(run, &text) else {
                continue;
            };
            if handled.as_deref() == Some(id.as_str()) {
                continue;
            }
            handled = Some(id);
            let key = "job.failed:os-updates";
            match run {
                Run::Finished(0) => {
                    notify::resolve(&shared, key);
                }
                _ => {
                    let title = match run {
                        Run::Finished(c) => format!("OS update failed (apt-get exit {c})"),
                        _ => "OS update was interrupted".to_string(),
                    };
                    notify::raise(
                        &shared,
                        NewEvent::new("host", "job.failed", Severity::Warning, &title)
                            .body(summary(&text))
                            .link("/settings")
                            .key(key),
                    );
                }
            }
            hostupdates::recheck(&shared).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: Duration = Duration::from_secs(10);

    #[test]
    fn judges_the_run_from_trigger_and_log() {
        assert_eq!(judge(false, None), Run::Idle);
        assert_eq!(judge(true, None), Run::Queued);
        // A stale finished log plus a fresh trigger is a new run waiting to start.
        assert_eq!(judge(true, Some(("== exit 0\n", D))), Run::Queued);
        assert_eq!(
            judge(false, Some(("== started x\nGet:1\n", D))),
            Run::Running
        );
        assert_eq!(
            judge(false, Some(("== started x\nGet:1\n", STALE + D))),
            Run::Interrupted
        );
        assert_eq!(
            judge(false, Some(("a\n== exit 0\n\n", D))),
            Run::Finished(0)
        );
        assert_eq!(
            judge(false, Some(("a\n== exit 100\n", D))),
            Run::Finished(100)
        );
        // The helper's exit line carries a timestamp.
        assert_eq!(
            judge(false, Some(("a\n== exit 100 2026-11-14T13:20:43Z\n", D))),
            Run::Finished(100)
        );
        assert_eq!(judge(false, Some(("", D))), Run::Idle);
        // An exit line that is not the last line does not count (a new run overwrote nothing yet).
        assert_eq!(judge(false, Some(("== exit 0\nmore\n", D))), Run::Running);
    }

    #[test]
    fn each_finished_run_has_its_own_id() {
        let a = "== started x\n== exit 0 2026-11-14T13:00:00Z\n";
        let b = "== started y\n== exit 0 2026-11-14T13:05:00Z\n";
        let ia = run_id(Run::Finished(0), a);
        assert!(ia.is_some());
        assert_ne!(ia, run_id(Run::Finished(0), b));
        assert_eq!(run_id(Run::Running, a), None);
        assert_eq!(run_id(Run::Idle, a), None);
        assert!(run_id(Run::Interrupted, a).is_some());
    }

    #[test]
    fn dpkg_progress_noise_is_shortened() {
        assert_eq!(
            tidy("(Reading database ... 5%(Reading database ... 10%"),
            "(Reading database …)"
        );
        assert_eq!(tidy("Setting up curl"), "Setting up curl");
    }

    #[test]
    fn summary_prefers_the_apt_error_line() {
        assert_eq!(
            summary("x\nE: Unable to lock\n== exit 100\n"),
            "E: Unable to lock"
        );
        assert!(summary("nothing\n").contains("log"));
    }

    #[test]
    fn units_carry_the_data_dir_and_no_user_name() {
        let d = Path::new("/var/lib/dockwren");
        let p = path_unit(d);
        assert!(p.contains("PathExists=/var/lib/dockwren/osupdate.trigger"));
        assert!(p.contains("Unit=dockwren-osupdate.service"));
        let s = service_unit(d);
        assert!(s.contains(
            "ExecStart=/usr/local/lib/dockwren/osupdate.sh /var/lib/dockwren/osupdate.trigger"
        ));
        assert!(s.contains("Type=oneshot"));
        assert!(!p.contains("User=") && !s.contains("User="));
        let sh = script();
        assert!(sh.contains("full-upgrade"));
        assert!(sh.contains("DEBIAN_FRONTEND=noninteractive"));
    }

    #[test]
    fn setup_rejects_relative_and_odd_paths() {
        assert!(setup(Path::new("./data")).unwrap_err().contains("absolute"));
        assert!(setup(Path::new("/does/not/exist")).is_err());
    }

    fn scratch(tag_: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dockwren-os-{tag_}-{}", tag()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn start_writes_one_trigger_and_refuses_while_active() {
        let d = scratch("start");
        let ctl = Control::with_paths(&d, &d.join("log"), &d.join("unit"));
        assert!(!ctl.installed() && !ctl.enabled());
        std::fs::write(d.join("unit"), "").unwrap();
        assert!(ctl.installed());
        ctl.set_enabled(true).unwrap();
        assert!(ctl.enabled());
        assert!(ctl.start().is_ok());
        assert!(d.join(TRIGGER_FILE).exists());
        assert!(matches!(ctl.start(), Err(StartError::Busy)));
        assert_eq!(ctl.state().0, Run::Queued);
        // The helper removes the trigger and writes a log.
        std::fs::remove_file(d.join(TRIGGER_FILE)).unwrap();
        std::fs::write(d.join("log"), "== started x\n").unwrap();
        assert_eq!(ctl.state().0, Run::Running);
        assert!(matches!(ctl.start(), Err(StartError::Busy)));
        std::fs::write(d.join("log"), "== started x\n== exit 0\n").unwrap();
        assert_eq!(ctl.state().0, Run::Finished(0));
        assert!(ctl.start().is_ok());
        ctl.set_enabled(false).unwrap();
        assert!(!ctl.enabled());
        ctl.set_enabled(false).unwrap();
    }

    #[test]
    fn log_tail_is_capped() {
        let d = scratch("tail");
        let ctl = Control::with_paths(&d, &d.join("log"), &d.join("unit"));
        let big = "x".repeat(200 * 1024) + "\n== exit 0\n";
        std::fs::write(d.join("log"), &big).unwrap();
        let (run, text) = ctl.state();
        assert_eq!(run, Run::Finished(0));
        assert!(text.len() <= TAIL_BYTES as usize);
    }
}
