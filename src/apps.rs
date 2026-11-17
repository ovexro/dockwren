//! Web UI for templates and instances: app list, install form, install jobs,
//! instance list. Server-rendered fragments driven by htmx.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use askama::Template;
use axum::{
    extract::{Form, Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use serde::Deserialize;

use crate::template::{self, FieldType, Template as AppTemplate};
use crate::{install, runner};

/// How many finished jobs are kept in memory.
const KEEP_JOBS: usize = 20;

#[derive(Clone)]
pub struct Apps {
    pub templates: Arc<Vec<AppTemplate>>,
    pub root: PathBuf,
    jobs: Arc<Mutex<Vec<(String, Job)>>>,
    /// Serialises "pick ports + create instance dir" so parallel installs cannot clash.
    lock: Arc<tokio::sync::Mutex<()>>,
    notify: Option<crate::notify::Shared>,
    auth: Option<crate::auth::SharedAuth>,
    schedule: Arc<crate::schedule::Schedule>,
    offsite: Arc<crate::offsite::Offsite>,
    upstream: crate::upstream::Cache,
}

impl Apps {
    pub fn new(templates: Vec<AppTemplate>, root: PathBuf) -> Self {
        Self {
            templates: Arc::new(templates),
            root,
            jobs: Arc::default(),
            lock: Arc::default(),
            notify: None,
            auth: None,
            schedule: Arc::new(crate::schedule::Schedule::memory()),
            offsite: Arc::new(crate::offsite::Offsite::memory()),
            upstream: Arc::default(),
        }
    }

    pub fn with_offsite(mut self, o: Arc<crate::offsite::Offsite>) -> Self {
        self.offsite = o;
        self
    }

    pub fn with_schedule(mut self, s: Arc<crate::schedule::Schedule>) -> Self {
        self.schedule = s;
        self
    }

    /// Needed for actions that ask for the password again (show/rotate app passwords).
    pub fn with_auth(mut self, auth: crate::auth::SharedAuth) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Let jobs and the update check raise notifications.
    pub fn with_notify(mut self, notify: crate::notify::Shared) -> Self {
        self.notify = Some(notify);
        self
    }

    /// Raise (on failure) or clear (on success) the "job failed" notification for a job.
    fn note_job(&self, verb: &str, instance: &str, error: Option<&str>) {
        let Some(n) = &self.notify else { return };
        let key = format!("job.failed:{verb}:{instance}");
        match error {
            Some(e) => {
                let first = e
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(300)
                    .collect::<String>();
                crate::notify::raise(
                    n,
                    crate::notify::NewEvent::new(
                        "apps",
                        "job.failed",
                        crate::notify::Severity::Warning,
                        &format!("{verb} {instance} failed"),
                    )
                    .body(first)
                    .link("/apps")
                    .key(key),
                );
            }
            None => {
                crate::notify::resolve(n, &key);
            }
        }
    }

    pub fn upstream_cache(&self) -> crate::upstream::Cache {
        self.upstream.clone()
    }

    /// (instance, image, installed tag) of each instance's main image: the first entry of the
    /// template's `image_pins`, as written in the instance's own compose file.
    pub fn upstream_targets(&self) -> Vec<(String, String, String)> {
        let Ok(rd) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for e in rd.flatten() {
            let Ok(name) = e.file_name().into_string() else {
                continue;
            };
            if !runner::valid_name(&name) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(e.path().join("state.toml")) else {
                continue;
            };
            let Ok(st) = toml::from_str::<InstanceState>(&text) else {
                continue;
            };
            let Some(main) = self
                .template(&st.template)
                .and_then(|t| t.manifest.update.image_pins.first().cloned())
            else {
                continue;
            };
            let Ok(compose) = std::fs::read_to_string(e.path().join("compose.yaml")) else {
                continue;
            };
            if let Some(tag) = crate::update::images(&compose).remove(&main) {
                out.push((name, main, tag));
            }
        }
        out.sort();
        out
    }

    /// Instances whose bundled template is newer: (name, installed version, bundled version).
    fn pending_updates(&self) -> Vec<(String, u32, u32)> {
        let Ok(rd) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for e in rd.flatten() {
            let Ok(name) = e.file_name().into_string() else {
                continue;
            };
            if !runner::valid_name(&name) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(e.path().join("state.toml")) else {
                continue;
            };
            let Ok(st) = toml::from_str::<InstanceState>(&text) else {
                continue;
            };
            if let Some(t) = self.template(&st.template)
                && t.manifest.app.version > st.version
                && skipped_version(&e.path()) != Some(t.manifest.app.version)
            {
                out.push((name, st.version, t.manifest.app.version));
            }
        }
        out.sort();
        out
    }

    /// Make the "update available" notifications match reality: raise one per instance with
    /// a newer bundled template, close the ones whose condition is gone.
    pub fn sync_update_notifications(&self) {
        const PREFIX: &str = "app.update_available:";
        let Some(n) = &self.notify else { return };
        let pending = self.pending_updates();
        for (name, from, to) in &pending {
            crate::notify::raise(
                n,
                crate::notify::NewEvent::new(
                    "apps",
                    "app.update_available",
                    crate::notify::Severity::Info,
                    &format!("Update available for {name}"),
                )
                .body(format!("Version {from} → {to}. A snapshot is taken first and the old version is restored if the update fails."))
                .link("/apps")
                .key(format!("{PREFIX}{name}")),
            );
        }
        let stale: Vec<String> = n
            .lock()
            .unwrap()
            .open_keys_with_prefix(PREFIX)
            .into_iter()
            .filter(|k| {
                !pending
                    .iter()
                    .any(|(name, _, _)| format!("{PREFIX}{name}") == *k)
            })
            .collect();
        for k in stale {
            crate::notify::resolve(n, &k);
        }
    }

    /// Check at start and then every hour.
    pub fn spawn_update_checker(&self) {
        let apps = self.clone();
        tokio::spawn(async move {
            loop {
                apps.sync_update_notifications();
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        });
    }

    /// Start an update job for `name` (snapshot first, rollback on failure). CONFLICT when a job for it runs.
    fn spawn_update(&self, name: &str) -> Result<String, StatusCode> {
        if !runner::valid_name(name) || !self.root.join(name).join("state.toml").is_file() {
            return Err(StatusCode::NOT_FOUND);
        }
        let job_id = new_job(self, name, "Updating")?;
        let task_apps = self.clone();
        let task_id = job_id.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            let backups = task_apps.backups_root();
            let result = crate::update::update_instance(
                &task_apps.root,
                &backups,
                &task_apps.templates,
                &name,
                &mut |s| {
                    task_apps.with_job(&task_id, |j| j.log.push(s.to_string()));
                },
            )
            .await;
            task_apps.note_job("Updating", &name, result.as_ref().err().map(|e| e.as_str()));
            task_apps.sync_update_notifications();
            task_apps.with_job(&task_id, |j| match result {
                Ok(crate::update::Outcome::UpToDate) => {
                    j.log.push("already up to date".into());
                    j.state = JobState::Done;
                }
                Ok(crate::update::Outcome::Updated { from, to }) => {
                    j.log.push(format!("updated from version {from} to {to}"));
                    j.state = JobState::Done;
                }
                Err(e) => j.state = JobState::Failed(e),
            });
        });
        Ok(job_id)
    }

    /// Start a backup job for `name`, keeping `keep` snapshots. CONFLICT when a job for it runs.
    fn spawn_backup(&self, name: &str, keep: usize) -> Result<String, StatusCode> {
        let dir = self.root.join(name);
        let text =
            std::fs::read_to_string(dir.join("state.toml")).map_err(|_| StatusCode::NOT_FOUND)?;
        let st: InstanceState =
            toml::from_str(&text).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        self.template(&st.template)
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
        let tpl_id = st.template;
        let job_id = new_job(self, name, "Backing up")?;
        let apps = self.clone();
        let id = job_id.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            let inst = runner::Instance {
                name: name.clone(),
                dir,
            };
            let t = apps.template(&tpl_id).expect("checked above");
            let result = crate::backup::snapshot_keep(
                &apps.backups_root(),
                &inst,
                t,
                &mut |s| {
                    apps.with_job(&id, |j| j.log.push(s.to_string()));
                },
                keep,
            )
            .await;
            // Local snapshot first; the off-host copy is a second step that can fail on its own.
            let result = match result {
                Ok(dir) if apps.offsite.get().enabled() => apps
                    .offsite
                    .push(&apps.backups_root(), &name, keep, &mut |s| {
                        apps.with_job(&id, |j| j.log.push(s.to_string()));
                    })
                    .await
                    .map(|_| dir)
                    .map_err(|e| format!("local snapshot saved, off-host upload failed: {e}")),
                other => other,
            };
            apps.note_job(
                "Backing up",
                &name,
                result.as_ref().err().map(|e| e.as_str()),
            );
            apps.with_job(&id, |j| match result {
                Ok(_) => {
                    j.log.push("backup done".into());
                    j.state = JobState::Done;
                }
                Err(e) => j.state = JobState::Failed(e),
            });
        });
        Ok(job_id)
    }

    /// Start a job that checks a snapshot (the newest when `snap` is None). CONFLICT when a job for it runs.
    fn spawn_verify(&self, name: &str, snap: Option<String>) -> Result<String, StatusCode> {
        let dir = self.root.join(name);
        let text =
            std::fs::read_to_string(dir.join("state.toml")).map_err(|_| StatusCode::NOT_FOUND)?;
        let st: InstanceState =
            toml::from_str(&text).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        self.template(&st.template)
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
        let tpl_id = st.template;
        let job_id = new_job(self, name, "Verifying")?;
        let apps = self.clone();
        let id = job_id.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            let inst = runner::Instance {
                name: name.clone(),
                dir,
            };
            let t = apps.template(&tpl_id).expect("checked above");
            let result =
                crate::backup::verify(&apps.backups_root(), &inst, t, snap.as_deref(), &mut |s| {
                    apps.with_job(&id, |j| j.log.push(s.to_string()));
                })
                .await;
            apps.note_job(
                "Verifying",
                &name,
                result.as_ref().err().map(|e| e.as_str()),
            );
            apps.with_job(&id, |j| match result {
                Ok(_) => {
                    j.log.push("backup verified: it can be restored".into());
                    j.state = JobState::Done;
                }
                Err(e) => j.state = JobState::Failed(e),
            });
        });
        Ok(job_id)
    }

    /// Start a job that restores the newest snapshot into a throwaway copy and deletes it again.
    fn spawn_restore_test(&self, name: &str) -> Result<String, StatusCode> {
        let dir = self.root.join(name);
        let text =
            std::fs::read_to_string(dir.join("state.toml")).map_err(|_| StatusCode::NOT_FOUND)?;
        let st: InstanceState =
            toml::from_str(&text).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        self.template(&st.template)
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
        let tpl_id = st.template;
        let job_id = new_job(self, name, "Restore test of")?;
        let apps = self.clone();
        let id = job_id.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            let inst = runner::Instance {
                name: name.clone(),
                dir,
            };
            let t = apps.template(&tpl_id).expect("checked above");
            let data = apps
                .backups_root()
                .parent()
                .map(std::path::Path::to_path_buf);
            let result = match data {
                Some(data) => {
                    crate::restoretest::run(&data, &inst, t, &mut |s| {
                        apps.with_job(&id, |j| j.log.push(s.to_string()));
                    })
                    .await
                }
                None => Err("no data directory".to_string()),
            };
            apps.note_job(
                "Restore test of",
                &name,
                result.as_ref().err().map(|e| e.as_str()),
            );
            apps.with_job(&id, |j| match result {
                Ok(Some(_)) => {
                    j.log
                        .push("restore test passed: the snapshot came back healthy".into());
                    j.state = JobState::Done;
                }
                Ok(None) => {
                    j.log.push("restore test skipped".into());
                    j.state = JobState::Done;
                }
                Err(e) => j.state = JobState::Failed(e),
            });
        });
        Ok(job_id)
    }

    /// Start a job that duplicates an app from one of its snapshots under a new name.
    fn spawn_clone(
        &self,
        name: &str,
        new_name: &str,
        snap: Option<String>,
    ) -> Result<String, StatusCode> {
        let dir = self.root.join(name);
        let text =
            std::fs::read_to_string(dir.join("state.toml")).map_err(|_| StatusCode::NOT_FOUND)?;
        let st: InstanceState =
            toml::from_str(&text).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        self.template(&st.template)
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
        let tpl_id = st.template;
        let job_id = new_job(self, new_name, "Duplicate into")?;
        let apps = self.clone();
        let id = job_id.clone();
        let (name, new_name) = (name.to_string(), new_name.to_string());
        tokio::spawn(async move {
            let inst = runner::Instance { name, dir };
            let t = apps.template(&tpl_id).expect("checked above");
            let data = apps
                .backups_root()
                .parent()
                .map(std::path::Path::to_path_buf);
            let result = match data {
                Some(data) => {
                    crate::clone::run(&data, &inst, t, &new_name, snap.as_deref(), &mut |s| {
                        apps.with_job(&id, |j| j.log.push(s.to_string()));
                    })
                    .await
                }
                None => Err("no data directory".to_string()),
            };
            apps.note_job(
                "Duplicate into",
                &new_name,
                result.as_ref().err().map(|e| e.as_str()),
            );
            apps.with_job(&id, |j| match result {
                Ok(_) => j.state = JobState::Done,
                Err(e) => j.state = JobState::Failed(e),
            });
        });
        Ok(job_id)
    }

    fn job_done(&self, id: &str) -> bool {
        self.with_job(id, |j| j.state == JobState::Done)
            .unwrap_or(false)
    }

    fn job_running(&self, id: &str) -> bool {
        self.with_job(id, |j| j.state == JobState::Running)
            .unwrap_or(false)
    }

    /// Every minute: when a scheduled slot has passed, back up each app one after the other.
    pub fn spawn_backup_scheduler(&self) {
        let apps = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                if !apps.schedule.is_due() {
                    continue;
                }
                // Record the run first: a crash mid-way must not repeat it every minute.
                apps.schedule.mark_run(crate::schedule::now());
                let keep = apps.schedule.get().keep;
                for name in apps.instance_names() {
                    // Stopped or unhealthy apps are skipped: dumps need a running container,
                    // and a nightly failure for an app the user stopped on purpose is noise.
                    if !apps.is_healthy(&name).await {
                        continue;
                    }
                    // Busy (an update or restore is running) or not backed up here: next slot.
                    let Ok(id) = apps.spawn_backup(&name, keep) else {
                        continue;
                    };
                    while apps.job_running(&id) {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                    // Check the snapshot just taken; a failure raises its own notification.
                    if apps.job_done(&id)
                        && let Ok(vid) = apps.spawn_verify(&name, None)
                    {
                        while apps.job_running(&vid) {
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        }
                    }
                }
                if apps.schedule.get().restore_test {
                    apps.run_restore_tests().await;
                }
            }
        });
    }

    /// Every minute: inside the chosen UTC window, update each healthy app that has a newer
    /// bundled version (and was not skipped). Each (app, version) is tried once per process,
    /// so a failed update that rolled back is not retried every minute.
    pub fn spawn_update_scheduler(&self) {
        let apps = self.clone();
        tokio::spawn(async move {
            let mut tried: std::collections::HashSet<(String, u32)> = Default::default();
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                let s = apps.schedule.get();
                let hour = ((crate::schedule::now() / 3600) % 24) as u8;
                if s.auto_update && crate::schedule::in_window(hour, s.update_from, s.update_to) {
                    apps.run_auto_updates(&mut tried).await;
                }
            }
        });
    }

    /// After the scheduled backups: restore-test each healthy app whose last pass is older than
    /// a week, one after the other. A failure raises a notification through the job.
    async fn run_restore_tests(&self) {
        let Some(data) = self
            .backups_root()
            .parent()
            .map(std::path::Path::to_path_buf)
        else {
            return;
        };
        for name in self.instance_names() {
            if !crate::restoretest::due(&data, &name, crate::schedule::now())
                || !self.is_healthy(&name).await
            {
                continue;
            }
            let Ok(id) = self.spawn_restore_test(&name) else {
                continue;
            };
            while self.job_running(&id) {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }

    /// Update every healthy app whose bundled template is newer, one after the other. A failed
    /// update rolls itself back and raises the usual failure event.
    async fn run_auto_updates(&self, tried: &mut std::collections::HashSet<(String, u32)>) {
        for (name, _, to) in self.pending_updates() {
            if tried.contains(&(name.clone(), to)) {
                continue;
            }
            // Never touch an app the user stopped, or one that was already broken.
            if !self.is_healthy(&name).await {
                continue;
            }
            tried.insert((name.clone(), to));
            let Ok(id) = self.spawn_update(&name) else {
                continue;
            };
            while self.job_running(&id) {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }

    async fn is_healthy(&self, name: &str) -> bool {
        let inst = runner::Instance {
            name: name.to_string(),
            dir: self.root.join(name),
        };
        match runner::compose(&inst, &["ps", "-a", "--format", "json"]).await {
            Ok(ps) => runner::judge(&ps) == runner::Status::Healthy,
            Err(_) => false,
        }
    }

    pub fn instance_names(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&self.root)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| {
                        runner::valid_name(n) && self.root.join(n).join("state.toml").is_file()
                    })
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    fn template(&self, id: &str) -> Option<&AppTemplate> {
        self.templates.iter().find(|t| t.manifest.app.id == id)
    }

    fn backups_root(&self) -> PathBuf {
        self.root
            .parent()
            .map(|d| d.join("backups"))
            .unwrap_or_else(|| self.root.join("backups"))
    }

    fn with_job<R>(&self, id: &str, f: impl FnOnce(&mut Job) -> R) -> Option<R> {
        let mut jobs = self.jobs.lock().unwrap();
        jobs.iter_mut()
            .find(|(j, _)| j == id)
            .map(|(_, job)| f(job))
    }
}

#[derive(Clone, PartialEq, Eq)]
enum JobState {
    Running,
    Done,
    Failed(String),
}

#[derive(Clone)]
struct Job {
    instance: String,
    verb: &'static str,
    log: Vec<String>,
    state: JobState,
    /// Secrets to show once; emptied when delivered to the browser.
    secrets: Vec<(String, String)>,
    ports: Vec<(String, String)>,
    /// Template note, shown after a successful install.
    note: String,
}

// ---- views -------------------------------------------------------------

struct AppView {
    id: String,
    name: String,
    description: String,
    https_note: bool,
    /// `url@sha` for a pinned third-party template.
    third_party: Option<String>,
}

/// Hidden template that pasted Compose stacks are filed under.
const CUSTOM_ID: &str = "custom";

#[derive(Template)]
#[template(path = "stack_form.html")]
struct StackForm {
    name: String,
    compose: String,
    env: String,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "catalog.html")]
struct AppsPage {
    apps: Vec<AppView>,
}

struct FieldView {
    key: String,
    label: String,
    /// text | password | select
    input: &'static str,
    value: String,
    placeholder: String,
    help: String,
    options: Vec<String>,
    required: bool,
}

#[derive(Template)]
#[template(path = "app_form.html")]
struct FormPage {
    id: String,
    name: String,
    note: String,
    https_note: bool,
    /// "53/udp (DNS)" lines; non-empty means the form asks for confirmation.
    lan_ports: Vec<String>,
    fields: Vec<FieldView>,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "job.html")]
struct JobPage {
    id: String,
    verb: &'static str,
    instance: String,
    log: Vec<String>,
    running: bool,
    error: Option<String>,
    secrets: Vec<(String, String)>,
    ports: Vec<(String, String)>,
    note: String,
}

struct InstanceView {
    name: String,
    app: String,
    ports: String,
    status: String,
    /// "1 → 2" when the bundled template is newer than the instance.
    update: Option<String>,
    /// The user chose to skip that version.
    update_skipped: bool,
    /// "Upstream has 28.4.0" when a registry has a newer tag (information only).
    upstream: Option<String>,
    snapshots: usize,
    last_backup: Option<String>,
    hostname: Option<String>,
    /// Last external check of the public URL: Some(true) reachable, Some(false) down.
    public_up: Option<bool>,
    /// Last image scan: "2 critical, 5 high" (None when clean or not scanned yet).
    vulns: Option<String>,
    /// The template has settings that can be changed after install.
    editable: bool,
    /// The template is known, so the main service can be limited.
    limitable: bool,
    /// The template has generated passwords that can be shown again.
    has_passwords: bool,
    /// Docker logs of this app have no size cap yet.
    uncapped: bool,
}

#[derive(Template)]
#[template(path = "instances.html")]
struct InstancesPage {
    instances: Vec<InstanceView>,
    error: Option<String>,
}

fn form_view(t: &AppTemplate, values: &HashMap<String, String>, error: Option<String>) -> FormPage {
    let url_field = t.manifest.app.public_url_field.as_deref();
    let mut fields: Vec<FieldView> = t
        .manifest
        .fields
        .iter()
        .filter(|f| Some(f.key.as_str()) != url_field)
        .map(|f| {
            let bool_or_select = matches!(f.kind, FieldType::Bool | FieldType::Select);
            let options = match f.kind {
                FieldType::Bool => vec!["true".to_string(), "false".to_string()],
                FieldType::Select => f.options.clone(),
                _ => Vec::new(),
            };
            let default = f.default.clone().unwrap_or_default();
            let entered = values.get(&f.key).cloned().unwrap_or_default();
            FieldView {
                key: f.key.clone(),
                label: f.label.clone(),
                input: if bool_or_select {
                    "select"
                } else if f.kind == FieldType::Password {
                    "password"
                } else {
                    "text"
                },
                // Selects need a concrete value; text inputs show the default as a placeholder.
                value: if bool_or_select && entered.is_empty() {
                    default.clone()
                } else {
                    entered
                },
                placeholder: default,
                help: f.help.clone().unwrap_or_default(),
                options,
                required: f.required && f.default.is_none(),
            }
        })
        .collect();
    if url_field.is_some() || t.manifest.app.accepts_public_host {
        let base = "The web address people type in their browser to open this app, for example blog.example.com (the domain must already be in your Cloudflare account). Leave it empty to use the app only on this server.";
        let help = if crate::public::Settings::from_env().is_some() {
            format!("{base} Dockwren will also make it reachable from the internet for you.")
        } else {
            format!(
                "{base} Cloudflare is not connected yet: paste a token in the Cloudflare tab first, then press Publish on the app's Public URL page."
            )
        };
        let at = usize::from(fields.first().is_some_and(|f| f.key == "INSTANCE_NAME"));
        fields.insert(
            at,
            FieldView {
                key: "PUBLIC_HOST".into(),
                label: "Public hostname".into(),
                input: "text",
                value: values.get("PUBLIC_HOST").cloned().unwrap_or_default(),
                placeholder: "blog.example.com".into(),
                help,
                options: Vec::new(),
                required: false,
            },
        );
    }
    FormPage {
        id: t.manifest.app.id.clone(),
        name: t.manifest.app.name.clone(),
        note: t.manifest.app.note.clone().unwrap_or_default(),
        https_note: t.manifest.app.needs_public_https,
        lan_ports: t
            .manifest
            .lan_ports
            .iter()
            .map(|l| format!("{}/{} ({})", l.port, l.protocol, l.label))
            .collect(),
        fields,
        error,
    }
}

fn render<T: askama::Template>(page: T) -> Html<String> {
    Html(page.render().expect("template renders"))
}

// ---- handlers ----------------------------------------------------------

pub async fn list(State(apps): State<Apps>) -> Html<String> {
    render(AppsPage {
        apps: apps
            .templates
            .iter()
            .filter(|t| t.manifest.app.id != CUSTOM_ID)
            .map(|t| AppView {
                id: t.manifest.app.id.clone(),
                name: t.manifest.app.name.clone(),
                description: t.manifest.app.description.clone(),
                https_note: t.manifest.app.needs_public_https,
                third_party: t.source.clone(),
            })
            .collect(),
    })
}

const FALLBACK_ICON: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" role="img"><rect width="64" height="64" rx="14" fill="#8A8F98"/><path d="M32 14 49 23v18L32 50 15 41V23z" fill="none" stroke="#fff" stroke-width="4" stroke-linejoin="round"/></svg>"##;

/// `GET /apps/{id}/icon.svg`: the template's icon, locked down so it can only ever be drawn.
pub async fn icon(
    State(apps): State<Apps>,
    Path(id): Path<String>,
) -> Result<Response, StatusCode> {
    // Templates without an icon (third-party ones may omit it) get a plain placeholder.
    let svg = apps
        .template(&id)
        .map(|t| t.icon.clone().unwrap_or_else(|| FALLBACK_ICON.to_string()))
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; sandbox",
            ),
        ],
        svg,
    )
        .into_response())
}

pub async fn new_form(
    State(apps): State<Apps>,
    Path(id): Path<String>,
) -> Result<Html<String>, StatusCode> {
    let t = apps.template(&id).ok_or(StatusCode::NOT_FOUND)?;
    Ok(render(form_view(t, &HashMap::new(), None)))
}

pub async fn stack_form() -> Html<String> {
    render(StackForm {
        name: String::new(),
        compose: String::new(),
        env: String::new(),
        error: None,
    })
}

/// `POST /stacks/install`: run a pasted Compose file as an app, after it passes [`crate::stack`].
pub async fn stack_submit(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Form(values): Form<HashMap<String, String>>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let get = |k: &str| values.get(k).cloned().unwrap_or_default();
    let (name, compose, env_text) = (get("name").trim().to_string(), get("compose"), get("env"));
    let again = |error: String| {
        Ok(render(StackForm {
            name: name.clone(),
            compose: compose.clone(),
            env: env_text.clone(),
            error: Some(error),
        }))
    };
    let t = apps
        .template(CUSTOM_ID)
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    if !runner::valid_name(&name) {
        return again(
            "Name: lower-case letters, digits and hyphens, starting with a letter".into(),
        );
    }
    let mut env = match crate::stack::parse_env(&env_text) {
        Ok(e) => e,
        Err(e) => return again(e),
    };
    if env.contains_key("INSTANCE_NAME") {
        return again("environment: INSTANCE_NAME is set by Dockwren".into());
    }
    env.insert("INSTANCE_NAME".into(), name.clone());
    let data_dir = apps
        .root
        .parent()
        .map(|d| d.to_path_buf())
        .unwrap_or_default();
    let model = match crate::stack::check(&data_dir, &compose, &env).await {
        Ok(m) => m,
        Err(e) => return again(e),
    };
    // Publishing a port also lets Dockwren list it and put a public hostname in front of it.
    let port = crate::stack::first_published_port(&model);
    if let Some(p) = port {
        env.entry("WEB_PORT".into())
            .or_insert_with(|| p.to_string());
    }
    let prepared = {
        let _guard = apps.lock.lock().await;
        let claimed = runner::claimed_ports(&apps.root);
        match port {
            Some(p) if claimed.contains(&p) || !install::port_is_free(p) => Err(format!(
                "port {p} is already in use on this server; pick another"
            )),
            _ => runner::prepare_with(&apps.root, t, &compose, &env),
        }
    };
    let inst = match prepared {
        Ok(i) => i,
        Err(e) => return again(e),
    };
    // The folder the backups cover and the only place bind mounts may point to.
    if let Err(e) = std::fs::create_dir_all(inst.dir.join("data")) {
        return again(format!("{}: {e}", inst.dir.display()));
    }
    crate::audit::record("stack.install", &inst.name);
    let job_id = new_job(&apps, &inst.name, "Installing")?;
    apps.with_job(&job_id, |j| {
        j.log.push(format!("stack {} prepared", inst.name));
        if let Some(p) = port {
            j.ports.push(("WEB_PORT".into(), format!("127.0.0.1:{p}")));
        }
    });
    let task_apps = apps.clone();
    let task_id = job_id.clone();
    tokio::spawn(async move {
        let result = runner::bring_up(&inst, &mut |s| {
            task_apps.with_job(&task_id, |j| j.log.push(s.to_string()));
        })
        .await;
        task_apps.note_job(
            "Installing",
            &inst.name,
            result.as_ref().err().map(|e| e.as_str()),
        );
        task_apps.with_job(&task_id, |j| match result {
            Ok(()) => j.state = JobState::Done,
            Err(e) => j.state = JobState::Failed(e),
        });
    });
    job_fragment(&apps, &job_id).ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}

pub async fn install_submit(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(values): Form<HashMap<String, String>>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let t_idx = apps
        .templates
        .iter()
        .position(|t| t.manifest.app.id == id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let t = &apps.templates[t_idx];

    let prepared = {
        let _guard = apps.lock.lock().await;
        let claimed = runner::claimed_ports(&apps.root);
        install::resolve(t, &values, &claimed, &install::port_is_free)
            .and_then(|r| install::check_lan_ports(t).map(|()| r))
            .and_then(|r| runner::prepare(&apps.root, t, &r.env).map(|inst| (r, inst)))
    };
    let (resolved, inst) = match prepared {
        Ok(ok) => ok,
        Err(e) => return Ok(render(form_view(t, &values, Some(e)))),
    };

    crate::audit::record("app.install", &format!("{} ({})", inst.name, id));
    let job_id = crate::auth::generate_password();
    let ports = ["WEB_PORT", "SSH_PORT"]
        .iter()
        .filter_map(|k| {
            resolved
                .env
                .get(*k)
                .map(|v| (k.to_string(), format!("127.0.0.1:{v}")))
        })
        .collect();
    {
        let mut jobs = apps.jobs.lock().unwrap();
        if jobs.len() >= KEEP_JOBS
            && let Some(i) = jobs.iter().position(|(_, j)| j.state != JobState::Running)
        {
            jobs.remove(i);
        }
        jobs.push((
            job_id.clone(),
            Job {
                instance: inst.name.clone(),
                verb: "Installing",
                log: vec![format!("instance {} prepared", inst.name)],
                state: JobState::Running,
                secrets: Vec::new(),
                ports,
                note: t.manifest.app.note.clone().unwrap_or_default(),
            },
        ));
    }

    let task_apps = apps.clone();
    let task_id = job_id.clone();
    tokio::spawn(async move {
        let push = |line: &str| {
            task_apps.with_job(&task_id, |j| j.log.push(line.to_string()));
        };
        let result = async {
            runner::bring_up(&inst, &mut |s| push(s)).await?;
            let t = &task_apps.templates[t_idx];
            let mut shown =
                runner::run_post_start(&inst, t, &resolved.env, &mut |s| push(s)).await?;
            shown.extend(resolved.revealed.iter().cloned());
            if let Some(host) = &resolved.public_host {
                crate::public::publish_after_install(&inst.dir, &inst.name, host, &mut |s| push(s))
                    .await;
            }
            Ok::<_, String>(shown)
        }
        .await;
        task_apps.note_job(
            "Installing",
            &inst.name,
            result.as_ref().err().map(|e| e.as_str()),
        );
        task_apps.with_job(&task_id, |j| match result {
            Ok(shown) => {
                j.secrets = shown;
                j.state = JobState::Done;
            }
            Err(e) => j.state = JobState::Failed(e),
        });
    });

    job_fragment(&apps, &job_id).ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}

fn job_fragment(apps: &Apps, id: &str) -> Option<Html<String>> {
    apps.with_job(id, |j| {
        // Secrets are handed out exactly once, on the response that reports completion.
        let secrets = if j.state == JobState::Done {
            std::mem::take(&mut j.secrets)
        } else {
            Vec::new()
        };
        render(JobPage {
            id: id.to_string(),
            verb: j.verb,
            instance: j.instance.clone(),
            log: j.log.clone(),
            running: j.state == JobState::Running,
            error: match &j.state {
                JobState::Failed(e) => Some(e.clone()),
                _ => None,
            },
            secrets,
            ports: j.ports.clone(),
            note: j.note.clone(),
        })
    })
}

pub async fn job(
    State(apps): State<Apps>,
    Path(id): Path<String>,
) -> Result<Html<String>, StatusCode> {
    job_fragment(&apps, &id).ok_or(StatusCode::NOT_FOUND)
}

#[derive(Deserialize)]
struct InstanceState {
    template: String,
    version: u32,
}

/// The template version the user chose to skip for this instance, if any.
fn skipped_version(dir: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(dir.join("skip-update"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn instance_ports(dir: &std::path::Path) -> String {
    let env = std::fs::read_to_string(dir.join(".env")).unwrap_or_default();
    let mut ports = BTreeMap::new();
    for line in env.lines() {
        if let Some((k, v)) = line.split_once('=')
            && k.ends_with("_PORT")
        {
            ports.insert(k.to_string(), v.trim_matches('\'').to_string());
        }
    }
    ports
        .values()
        .map(|p| format!("127.0.0.1:{p}"))
        .collect::<Vec<_>>()
        .join(", ")
}

async fn instance_views(apps: &Apps) -> Vec<InstanceView> {
    let root = &apps.root;
    let mut names: Vec<String> = std::fs::read_dir(root)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| runner::valid_name(n))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    let mut out = Vec::new();
    for name in names {
        let dir = root.join(&name);
        let Ok(state) = std::fs::read_to_string(dir.join("state.toml")) else {
            continue;
        };
        let Ok(state) = toml::from_str::<InstanceState>(&state) else {
            continue;
        };
        let inst = runner::Instance {
            name: name.clone(),
            dir: dir.clone(),
        };
        let status = match runner::compose(&inst, &["ps", "-a", "--format", "json"]).await {
            Ok(ps) => match runner::judge(&ps) {
                runner::Status::Healthy => "healthy".to_string(),
                runner::Status::Waiting(w) if w == "no containers yet" => "stopped".to_string(),
                runner::Status::Waiting(w) => format!("starting: {w}"),
                runner::Status::Failed(w) => format!("problem: {w}"),
            },
            Err(_) => "unknown".to_string(),
        };
        let newer = apps
            .template(&state.template)
            .map(|t| t.manifest.app.version)
            .filter(|v| *v > state.version);
        let update = newer.map(|v| format!("{} → {}", state.version, v));
        let update_skipped = newer.is_some() && skipped_version(&dir) == newer;
        let snaps = crate::backup::list(&apps.backups_root(), &name);
        let snapshots = snaps.len();
        let last_backup = snaps
            .last()
            .and_then(|p| p.file_name()?.to_str()?.parse::<u64>().ok())
            .map(|e| utc_stamp(e).trim_end_matches(" UTC").to_string());
        let name_for_upstream = name.clone();
        let editable = apps
            .template(&state.template)
            .is_some_and(|t| t.manifest.fields.iter().any(|f| f.editable));
        let limitable = apps.template(&state.template).is_some();
        let has_passwords = apps
            .template(&state.template)
            .is_some_and(|t| t.manifest.secrets.iter().any(is_viewable));
        out.push(InstanceView {
            name,
            app: state.template,
            ports: instance_ports(&dir),
            status,
            update,
            update_skipped,
            upstream: apps
                .upstream
                .lock()
                .unwrap()
                .get(&name_for_upstream)
                .map(|f| f.summary()),
            snapshots,
            last_backup,
            hostname: crate::public::read(&dir),
            public_up: crate::webcheck::status(&name_for_upstream)
                .map(|s| s == crate::webcheck::Status::Up),
            vulns: crate::vulnscan::summary(&name_for_upstream).and_then(|v| v.line()),
            editable,
            limitable,
            has_passwords,
            uncapped: !crate::logcap::enabled(&dir),
        });
    }
    out
}

/// Epoch of the newest snapshot of any installed app; None when there is none.
pub fn latest_backup(apps: &Apps) -> Option<u64> {
    let rd = std::fs::read_dir(&apps.root).ok()?;
    rd.flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| runner::valid_name(n))
        .filter_map(|n| crate::backup::list(&apps.backups_root(), &n).pop())
        .filter_map(|p| p.file_name()?.to_str()?.parse::<u64>().ok())
        .max()
}

/// Number of installed apps (instance folders with a state file); cheap, no Docker calls.
pub fn instance_count(apps: &Apps) -> usize {
    std::fs::read_dir(&apps.root)
        .map(|rd| {
            rd.flatten()
                .filter(|e| {
                    e.file_name().to_str().is_some_and(runner::valid_name)
                        && e.path().join("state.toml").is_file()
                })
                .count()
        })
        .unwrap_or(0)
}

pub async fn instances(State(apps): State<Apps>) -> Html<String> {
    render(InstancesPage {
        instances: instance_views(&apps).await,
        error: None,
    })
}

pub async fn instance_remove(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    if !runner::valid_name(&name) {
        return Err(StatusCode::NOT_FOUND);
    }
    let dir = apps.root.join(&name);
    if !dir.join("state.toml").is_file() {
        return Err(StatusCode::NOT_FOUND);
    }
    let removed = name.clone();
    let error = runner::take_down(&runner::Instance { name, dir })
        .await
        .err();
    if error.is_none() {
        crate::audit::record("app.remove", &removed);
    }
    Ok(render(InstancesPage {
        instances: instance_views(&apps).await,
        error,
    }))
}

/// Load the template catalog for the server (bundled + optional override dir).
pub fn catalog(cfg: &crate::config::Config) -> Result<Vec<AppTemplate>, String> {
    let mut all = template::catalog(cfg.apps_dir_override().as_deref())?;
    for t in crate::thirdparty::load_all(&cfg.data_dir) {
        if all.iter().all(|a| a.manifest.app.id != t.manifest.app.id) {
            all.push(t);
        }
    }
    all.sort_by(|a, b| a.manifest.app.id.cmp(&b.manifest.app.id));
    Ok(all)
}

#[derive(Template)]
#[template(path = "update_preview.html")]
struct UpdatePreviewPage {
    instance: String,
    skipped: bool,
    p: crate::update::Preview,
}

/// `GET /instances/{name}/update`: what the update would change, with a button to run it.
pub async fn update_preview(
    State(apps): State<Apps>,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    known_instance(&apps, &name)?;
    let dir = apps.root.join(&name);
    let state: InstanceState = std::fs::read_to_string(dir.join("state.toml"))
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let t = apps
        .template(&state.template)
        .ok_or(StatusCode::NOT_FOUND)?;
    if t.manifest.app.version <= state.version {
        return Err(StatusCode::NOT_FOUND);
    }
    let old = std::fs::read_to_string(dir.join("compose.yaml"))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(render(UpdatePreviewPage {
        skipped: skipped_version(&dir) == Some(t.manifest.app.version),
        instance: name,
        p: crate::update::preview(&old, t, state.version),
    }))
}

/// `POST /instances/{name}/skip-update` and `/unskip-update`: ignore (or stop ignoring) the
/// bundled version that is newer than this instance. A still newer release is offered again.
pub async fn instance_skip(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    skip_update(&apps, &headers, &name, false)
}

pub async fn instance_unskip(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    skip_update(&apps, &headers, &name, true)
}

fn skip_update(
    apps: &Apps,
    headers: &HeaderMap,
    name: &str,
    undo: bool,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    known_instance(apps, name)?;
    let dir = apps.root.join(name);
    let state: InstanceState = std::fs::read_to_string(dir.join("state.toml"))
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let to = apps
        .template(&state.template)
        .map(|t| t.manifest.app.version)
        .filter(|v| *v > state.version)
        .ok_or(StatusCode::NOT_FOUND)?;
    let file = dir.join("skip-update");
    if undo {
        let _ = std::fs::remove_file(&file);
    } else {
        std::fs::write(&file, to.to_string()).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    crate::audit::record(
        if undo {
            "app.unskip-update"
        } else {
            "app.skip-update"
        },
        name,
    );
    apps.sync_update_notifications();
    Ok(Html(format!(
        "<p role=\"status\">{}</p>",
        if undo {
            "Version no longer skipped. Reload the page to see the update button."
        } else {
            "This version is skipped: no notification, no auto-update. Reload to see it."
        }
    )))
}

/// Start an update job for an instance whose bundled template is newer.
pub async fn instance_update(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let job_id = apps.spawn_update(&name)?;
    crate::audit::record("app.update", &name);
    job_fragment(&apps, &job_id).ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// `POST /instances/{name}/backup`: take a snapshot now.
pub async fn instance_backup(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    if !runner::valid_name(&name) {
        return Err(StatusCode::NOT_FOUND);
    }
    let keep = apps.schedule.get().keep;
    let id = apps.spawn_backup(&name, keep)?;
    crate::audit::record("app.backup", &name);
    job_fragment(&apps, &id).ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// `POST /instances/{name}/verify`: check a snapshot (form field `snapshot`, newest when absent).
pub async fn verify_submit(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(values): Form<HashMap<String, String>>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    known_instance(&apps, &name)?;
    let snap = values.get("snapshot").filter(|s| !s.is_empty()).cloned();
    let id = apps.spawn_verify(&name, snap)?;
    crate::audit::record("app.verify", &name);
    job_fragment(&apps, &id).ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// `POST /instances/{name}/restore-test`: restore the newest snapshot into a throwaway copy.
pub async fn restore_test_submit(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) || !runner::valid_name(&name) {
        return Err(StatusCode::FORBIDDEN);
    }
    let id = apps.spawn_restore_test(&name)?;
    crate::audit::record("app.restore-test", &name);
    job_fragment(&apps, &id).ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// `POST /instances/{name}/clone`: duplicate an app from a snapshot (fields `new_name`, `snapshot`).
pub async fn clone_submit(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(values): Form<HashMap<String, String>>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) || !runner::valid_name(&name) {
        return Err(StatusCode::FORBIDDEN);
    }
    known_instance(&apps, &name)?;
    let new_name = values.get("new_name").map(|s| s.trim()).unwrap_or("");
    if !runner::valid_name(new_name) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let snap = values.get("snapshot").filter(|s| !s.is_empty()).cloned();
    let id = apps.spawn_clone(&name, new_name, snap)?;
    crate::audit::record("app.clone", &format!("{name} -> {new_name}"));
    job_fragment(&apps, &id).ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}

struct DiskRow {
    name: String,
    data: String,
    partial: bool,
    leftovers: String,
    snapshots: usize,
    snapshot_bytes: String,
    images: String,
    total: String,
    can_trim: bool,
    trimmable: String,
}

#[derive(Template)]
#[template(path = "disk.html")]
struct DiskPage {
    rows: Vec<DiskRow>,
    keep: usize,
    images: Option<(String, String)>,
    note: Option<String>,
    error: Option<String>,
}

async fn disk_html(apps: &Apps, note: Option<String>, error: Option<String>) -> Html<String> {
    use crate::metrics::fmt_bytes as fb;
    let data = apps
        .backups_root()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let mut rows = Vec::new();
    for name in apps.instance_names() {
        let inst = runner::Instance {
            dir: apps.root.join(&name),
            name,
        };
        let (i2, b) = (inst.clone(), apps.backups_root());
        let Ok(mut u) =
            tokio::task::spawn_blocking(move || crate::diskusage::measure(&i2, &b)).await
        else {
            continue;
        };
        u.images = crate::diskusage::image_bytes(&data, &inst).await;
        rows.push(DiskRow {
            total: fb(u.total() as f64),
            data: fb(u.data as f64),
            partial: u.partial,
            leftovers: fb(u.leftovers as f64),
            snapshots: u.snapshots,
            snapshot_bytes: fb(u.snapshot_bytes as f64),
            images: fb(u.images as f64),
            can_trim: u.trimmable > 0,
            trimmable: fb(u.trimmable as f64),
            name: u.name,
        });
    }
    render(DiskPage {
        rows,
        keep: crate::diskusage::KEEP_AFTER_TRIM,
        images: crate::diskusage::docker_images_line(&data).await,
        note,
        error,
    })
}

/// `GET /settings/disk`: sizes per app.
pub async fn disk_show(State(apps): State<Apps>) -> Html<String> {
    disk_html(&apps, None, None).await
}

/// `POST /instances/{name}/trim-snapshots`: delete the oldest local snapshots, keep the newest few.
pub async fn trim_snapshots(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    known_instance(&apps, &name)?;
    let busy = apps
        .jobs
        .lock()
        .unwrap()
        .iter()
        .any(|(_, j)| j.instance == name && j.state == JobState::Running);
    if busy {
        return Err(StatusCode::CONFLICT);
    }
    crate::audit::record("app.trim-snapshots", &name);
    let (note, error) = match crate::backup::prune(
        &apps.backups_root(),
        &name,
        crate::diskusage::KEEP_AFTER_TRIM,
    ) {
        Ok(n) => (Some(format!("{name}: deleted {n} old snapshot(s).")), None),
        Err(e) => (None, Some(e)),
    };
    Ok(disk_html(&apps, note, error).await)
}

/// `POST /settings/disk/prune-images`: remove Docker images no container uses.
pub async fn prune_images(
    State(apps): State<Apps>,
    headers: HeaderMap,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let data = apps
        .backups_root()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    crate::audit::record("docker.prune-images", "unused images");
    let (note, error) = match crate::diskusage::prune_images(&data).await {
        Ok(s) => (Some(s), None),
        Err(e) => (None, Some(e)),
    };
    Ok(disk_html(&apps, note, error).await)
}

#[derive(Template)]
#[template(path = "backup_schedule.html")]
struct SchedulePage {
    mode: &'static str,
    hour: u8,
    keep: usize,
    auto_update: bool,
    update_from: u8,
    update_to: u8,
    restore_test: bool,
    last: Option<String>,
    note: Option<&'static str>,
    error: Option<&'static str>,
}

fn schedule_html(
    apps: &Apps,
    note: Option<&'static str>,
    error: Option<&'static str>,
) -> Html<String> {
    let s = apps.schedule.get();
    let last = apps.schedule.last_run();
    render(SchedulePage {
        mode: s.mode.as_str(),
        hour: s.hour,
        keep: s.keep,
        auto_update: s.auto_update,
        update_from: s.update_from,
        update_to: s.update_to,
        restore_test: s.restore_test,
        last: (s.mode != crate::schedule::Mode::Off && last > 0).then(|| utc_stamp(last)),
        note,
        error,
    })
}

pub async fn schedule_show(State(apps): State<Apps>) -> Html<String> {
    schedule_html(&apps, None, None)
}

#[derive(Deserialize)]
pub struct ScheduleForm {
    mode: String,
    hour: String,
    keep: String,
    /// A checkbox: present only when ticked.
    auto_update: Option<String>,
    update_from: Option<String>,
    update_to: Option<String>,
    restore_test: Option<String>,
}

pub async fn schedule_save(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Form(f): Form<ScheduleForm>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let s = match crate::schedule::Settings::from_form(
        &f.mode,
        &f.hour,
        &f.keep,
        f.auto_update.is_some(),
        (
            &f.update_from
                .unwrap_or_else(|| apps.schedule.get().update_from.to_string()),
            &f.update_to
                .unwrap_or_else(|| apps.schedule.get().update_to.to_string()),
        ),
        f.restore_test.is_some(),
    ) {
        Ok(s) => s,
        Err(e) => return Ok(schedule_html(&apps, None, Some(e))),
    };
    if apps.schedule.set(s).is_err() {
        return Ok(schedule_html(
            &apps,
            None,
            Some("Could not save the schedule."),
        ));
    }
    crate::audit::record("settings.backup-schedule", "changed");
    Ok(schedule_html(&apps, Some("Saved."), None))
}

#[derive(Template)]
#[template(path = "offsite.html")]
struct OffsitePage {
    kind: &'static str,
    endpoint: String,
    bucket: String,
    host: String,
    port: u16,
    user: String,
    path: String,
    has_secrets: bool,
    describe: String,
    last: Option<String>,
    new_password: Option<String>,
    note: Option<&'static str>,
    error: Option<String>,
}

fn offsite_html(
    apps: &Apps,
    note: Option<&'static str>,
    error: Option<String>,
    new_password: Option<String>,
) -> Html<String> {
    let c = apps.offsite.get();
    let last = apps.offsite.last_ok();
    render(OffsitePage {
        kind: c.kind.as_str(),
        endpoint: c.endpoint.clone(),
        bucket: c.bucket.clone(),
        host: c.host.clone(),
        port: if c.port == 0 { 22 } else { c.port },
        user: c.user.clone(),
        path: c.path.clone(),
        has_secrets: !c.password.is_empty(),
        describe: c.describe(),
        last: (c.enabled() && last > 0).then(|| utc_stamp(last)),
        new_password,
        note,
        error,
    })
}

pub async fn offsite_show(State(apps): State<Apps>) -> Html<String> {
    offsite_html(&apps, None, None, None)
}

pub async fn offsite_save(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Form(f): Form<crate::offsite::Form>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let old = apps.offsite.get();
    let (cfg, generated) = match crate::offsite::Config::from_form(&f, &old) {
        Ok(v) => v,
        Err(e) => return Ok(offsite_html(&apps, None, Some(e.to_string()), None)),
    };
    // Nothing is saved unless the target answers and the password fits the repository.
    if cfg.enabled()
        && let Err(e) = apps.offsite.ensure_repo(&cfg).await
    {
        return Ok(offsite_html(
            &apps,
            None,
            Some(if e.contains("wrong password") {
                format!(
                    "Nothing was saved: {e}. If a repository already exists there, type its password; otherwise use an empty folder or bucket."
                )
            } else {
                format!("Could not reach the target, nothing was saved: {e}")
            }),
            None,
        ));
    }
    if let Err(e) = apps.offsite.set(cfg.clone()) {
        return Ok(offsite_html(&apps, None, Some(e), None));
    }
    Ok(offsite_html(
        &apps,
        Some(if cfg.enabled() {
            "Saved. The target works."
        } else {
            "Saved. Off-host copies are off."
        }),
        None,
        generated.then_some(cfg.password),
    ))
}

/// Register a running job for `name`; CONFLICT if one is already running for it.
fn new_job(apps: &Apps, name: &str, verb: &'static str) -> Result<String, StatusCode> {
    let job_id = crate::auth::generate_password();
    let mut jobs = apps.jobs.lock().unwrap();
    if jobs
        .iter()
        .any(|(_, j)| j.instance == name && j.state == JobState::Running)
    {
        return Err(StatusCode::CONFLICT);
    }
    if jobs.len() >= KEEP_JOBS
        && let Some(i) = jobs.iter().position(|(_, j)| j.state != JobState::Running)
    {
        jobs.remove(i);
    }
    jobs.push((
        job_id.clone(),
        Job {
            instance: name.to_string(),
            verb,
            log: Vec::new(),
            state: JobState::Running,
            secrets: Vec::new(),
            ports: Vec::new(),
            note: String::new(),
        },
    ));
    Ok(job_id)
}

/// "2026-11-13 15:11 UTC" from epoch seconds (no time crate needed).
pub(crate) fn utc_stamp(epoch: u64) -> String {
    let days = (epoch / 86400) as i64;
    let rem = epoch % 86400;
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
}

struct SnapshotView {
    name: String,
    when: String,
    verified: Option<String>,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPage {
    instance: String,
    fields: Vec<FieldView>,
    error: Option<String>,
}

/// The editable fields of an instance with their current values (from `.env`).
fn settings_view(
    apps: &Apps,
    name: &str,
    entered: Option<&HashMap<String, String>>,
    error: Option<String>,
) -> Result<SettingsPage, StatusCode> {
    let dir = apps.root.join(name);
    let state: InstanceState = std::fs::read_to_string(dir.join("state.toml"))
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let t = apps
        .template(&state.template)
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let env = runner::read_env(&dir).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let fields: Vec<FieldView> = t
        .manifest
        .fields
        .iter()
        .filter(|f| f.editable)
        .map(|f| {
            let select = matches!(f.kind, FieldType::Bool | FieldType::Select);
            let options = match f.kind {
                FieldType::Bool => vec!["true".to_string(), "false".to_string()],
                FieldType::Select => f.options.clone(),
                _ => Vec::new(),
            };
            let value = entered
                .and_then(|e| e.get(&f.key))
                .or_else(|| env.get(&f.key))
                .cloned()
                .unwrap_or_default();
            FieldView {
                key: f.key.clone(),
                label: f.label.clone(),
                input: if select { "select" } else { "text" },
                value,
                placeholder: f.default.clone().unwrap_or_default(),
                help: f.help.clone().unwrap_or_default(),
                options,
                required: f.required && f.default.is_none(),
            }
        })
        .collect();
    if fields.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(SettingsPage {
        instance: name.to_string(),
        fields,
        error,
    })
}

/// `GET /instances/{name}/settings`: form with the settings that can be changed after install.
pub async fn settings_form(
    State(apps): State<Apps>,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    known_instance(&apps, &name)?;
    Ok(render(settings_view(&apps, &name, None, None)?))
}

/// `POST /instances/{name}/settings`: validate, rewrite `.env`, recreate the containers.
/// When the app does not come back healthy the old `.env` is put back and started again.
pub async fn settings_submit(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(values): Form<HashMap<String, String>>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    known_instance(&apps, &name)?;
    let dir = apps.root.join(&name);
    let state: InstanceState = std::fs::read_to_string(dir.join("state.toml"))
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let t = apps
        .template(&state.template)
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let old = runner::read_env(&dir).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let new = match crate::install::apply_edits(t, &old, &values) {
        Ok(n) => n,
        Err(e) => return Ok(render(settings_view(&apps, &name, Some(&values), Some(e))?)),
    };
    let changed: Vec<&str> = t
        .manifest
        .fields
        .iter()
        .filter(|f| f.editable && old.get(&f.key) != new.get(&f.key))
        .map(|f| f.key.as_str())
        .collect();
    if changed.is_empty() {
        return Ok(render(settings_view(
            &apps,
            &name,
            Some(&values),
            Some("Nothing changed.".into()),
        )?));
    }
    let job_id = new_job(&apps, &name, "Applying settings to")?;
    crate::audit::record("app.settings", &format!("{name}: {}", changed.join(", ")));
    let apps2 = apps.clone();
    let id = job_id.clone();
    let changed: Vec<String> = changed.iter().map(|s| s.to_string()).collect();
    tokio::spawn(async move {
        let inst = runner::Instance {
            name: name.clone(),
            dir,
        };
        let log = |s: &str| {
            apps2.with_job(&id, |j| j.log.push(s.to_string()));
        };
        let result = async {
            log(&format!("changing {}", changed.join(", ")));
            crate::install::replace_env(&inst.dir, &new).map_err(|e| format!(".env: {e}"))?;
            let up = async {
                log("checking compose file");
                runner::compose(&inst, &["config", "-q"]).await?;
                log("recreating containers");
                runner::compose(&inst, &["up", "-d"]).await?;
                log("waiting for healthy");
                runner::wait_healthy(&inst, std::time::Duration::from_secs(240)).await
            }
            .await;
            if let Err(e) = up {
                log("failed: putting the old settings back");
                crate::install::replace_env(&inst.dir, &old).map_err(|e| format!(".env: {e}"))?;
                let back = runner::compose(&inst, &["up", "-d"]).await;
                return Err(match back {
                    Ok(_) => format!("{e} (old settings restored)"),
                    Err(b) => format!("{e} (restoring the old settings also failed: {b})"),
                });
            }
            Ok::<(), String>(())
        }
        .await;
        apps2.note_job(
            "Applying settings to",
            &name,
            result.as_ref().err().map(|e| e.as_str()),
        );
        apps2.with_job(&id, |j| match result {
            Ok(()) => {
                j.log.push("settings applied".into());
                j.state = JobState::Done;
            }
            Err(e) => j.state = JobState::Failed(e),
        });
    });
    job_fragment(&apps, &job_id).ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// A generated password the user may ask to see again (stored in `.env` as plain text).
fn is_viewable(s: &crate::template::Secret) -> bool {
    s.reveal && matches!(s.kind, crate::template::SecretKind::Password)
}

#[derive(Template)]
#[template(path = "passwords.html")]
struct PasswordsPage {
    instance: String,
    /// (key, can be rotated)
    secrets: Vec<(String, bool)>,
    two_factor: bool,
    error: Option<String>,
    shown: Vec<(String, String)>,
}

fn passwords_view(
    apps: &Apps,
    name: &str,
    error: Option<String>,
    shown: Vec<(String, String)>,
) -> Result<PasswordsPage, StatusCode> {
    let dir = apps.root.join(name);
    let state: InstanceState = std::fs::read_to_string(dir.join("state.toml"))
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let t = apps
        .template(&state.template)
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let secrets: Vec<(String, bool)> = t
        .manifest
        .secrets
        .iter()
        .filter(|s| is_viewable(s))
        .map(|s| (s.key.clone(), s.rotate))
        .collect();
    if secrets.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(PasswordsPage {
        instance: name.to_string(),
        secrets,
        two_factor: apps.auth.as_ref().is_some_and(|a| a.two_factor_enabled()),
        error,
        shown,
    })
}

/// `GET /instances/{name}/passwords`: re-auth form for the generated passwords of an app.
pub async fn passwords_form(
    State(apps): State<Apps>,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    known_instance(&apps, &name)?;
    Ok(render(passwords_view(&apps, &name, None, Vec::new())?))
}

#[derive(Deserialize)]
pub struct PasswordsForm {
    password: String,
    #[serde(default)]
    code: String,
    /// "show" or "rotate"
    action: String,
    #[serde(default)]
    key: String,
}

/// `POST /instances/{name}/passwords`: after re-auth, show the stored passwords, or rotate one
/// (new value in `.env` + recreate; the old `.env` is put back when the app does not come up).
pub async fn passwords_submit(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(form): Form<PasswordsForm>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    known_instance(&apps, &name)?;
    let auth = apps.auth.clone().ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let page = |e: &str| passwords_view(&apps, &name, Some(e.to_string()), Vec::new());
    if !auth.reauth(form.password.clone(), &form.code).await {
        return Ok(render(page("Wrong password or code.")?));
    }
    let dir = apps.root.join(&name);
    let state: InstanceState = std::fs::read_to_string(dir.join("state.toml"))
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let t = apps
        .template(&state.template)
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let old = runner::read_env(&dir).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if form.action == "show" {
        let shown: Vec<(String, String)> = t
            .manifest
            .secrets
            .iter()
            .filter(|s| is_viewable(s))
            .filter_map(|s| old.get(&s.key).map(|v| (s.key.clone(), v.clone())))
            .collect();
        crate::audit::record("app.passwords_shown", &name);
        return Ok(render(passwords_view(&apps, &name, None, shown)?));
    }
    let Some(secret) = t
        .manifest
        .secrets
        .iter()
        .find(|s| s.key == form.key && is_viewable(s) && s.rotate)
    else {
        return Ok(render(page("This password cannot be rotated here.")?));
    };
    let key = secret.key.clone();
    let plain = crate::install::generate_secret(secret);
    let mut new = old.clone();
    new.insert(key.clone(), plain.clone());
    let job_id = new_job(&apps, &name, "Rotating the password of")?;
    crate::audit::record("app.password_rotated", &format!("{name}: {key}"));
    let apps2 = apps.clone();
    let id = job_id.clone();
    tokio::spawn(async move {
        let inst = runner::Instance {
            name: name.clone(),
            dir,
        };
        let log = |s: &str| {
            apps2.with_job(&id, |j| j.log.push(s.to_string()));
        };
        let result = async {
            log(&format!("new value for {key}"));
            crate::install::replace_env(&inst.dir, &new).map_err(|e| format!(".env: {e}"))?;
            let up = async {
                log("recreating containers");
                runner::compose(&inst, &["up", "-d"]).await?;
                log("waiting for healthy");
                runner::wait_healthy(&inst, std::time::Duration::from_secs(240)).await
            }
            .await;
            if let Err(e) = up {
                log("failed: putting the old password back");
                crate::install::replace_env(&inst.dir, &old).map_err(|e| format!(".env: {e}"))?;
                let back = runner::compose(&inst, &["up", "-d"]).await;
                return Err(match back {
                    Ok(_) => format!("{e} (old password restored)"),
                    Err(b) => format!("{e} (restoring the old password also failed: {b})"),
                });
            }
            Ok::<(), String>(())
        }
        .await;
        apps2.note_job(
            "Rotating the password of",
            &name,
            result.as_ref().err().map(|e| e.as_str()),
        );
        apps2.with_job(&id, |j| match result {
            Ok(()) => {
                j.log.push("password rotated".into());
                j.secrets.push((key.clone(), plain));
                j.state = JobState::Done;
            }
            Err(e) => j.state = JobState::Failed(e),
        });
    });
    job_fragment(&apps, &job_id).ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(Template)]
#[template(path = "limits.html")]
struct LimitsPage {
    instance: String,
    service: String,
    memory: String,
    cpus: String,
    error: Option<String>,
}

/// Name of the main service of an instance, from its template.
fn main_service(apps: &Apps, dir: &std::path::Path) -> Result<String, StatusCode> {
    let state: InstanceState = std::fs::read_to_string(dir.join("state.toml"))
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let t = apps
        .template(&state.template)
        .ok_or(StatusCode::NOT_FOUND)?;
    t.manifest
        .services
        .iter()
        .find(|s| s.main)
        .map(|s| s.name.clone())
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// `GET /instances/{name}/limits`: form with the memory and CPU limits of the main service.
pub async fn limits_form(
    State(apps): State<Apps>,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    known_instance(&apps, &name)?;
    let dir = apps.root.join(&name);
    let service = main_service(&apps, &dir)?;
    let l = crate::limits::load(&dir);
    Ok(render(LimitsPage {
        instance: name,
        service,
        memory: l.memory,
        cpus: l.cpus,
        error: None,
    }))
}

/// `POST /instances/{name}/limits`: store the limits, recreate the containers; on failure the
/// old limits are put back. Empty fields remove the limit.
pub async fn limits_submit(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(values): Form<HashMap<String, String>>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    known_instance(&apps, &name)?;
    let dir = apps.root.join(&name);
    let service = main_service(&apps, &dir)?;
    let get = |k: &str| values.get(k).map(String::as_str).unwrap_or("");
    let old = crate::limits::load(&dir);
    let page = |error: &str| {
        render(LimitsPage {
            instance: name.clone(),
            service: service.clone(),
            memory: get("memory").to_string(),
            cpus: get("cpus").to_string(),
            error: Some(error.to_string()),
        })
    };
    let new = match crate::limits::Limits::parse(get("memory"), get("cpus")) {
        Ok(l) => l,
        Err(e) => return Ok(page(&e)),
    };
    if new == old {
        return Ok(page("Nothing changed."));
    }
    let job_id = new_job(&apps, &name, "Applying limits to")?;
    let summary = if new.is_empty() {
        "removed".to_string()
    } else {
        format!("memory={} cpus={}", new.memory, new.cpus)
    };
    crate::audit::record("app.limits", &format!("{name}: {summary}"));
    let apps2 = apps.clone();
    let id = job_id.clone();
    tokio::spawn(async move {
        let inst = runner::Instance {
            name: name.clone(),
            dir,
        };
        let log = |s: &str| {
            apps2.with_job(&id, |j| j.log.push(s.to_string()));
        };
        let result = async {
            crate::limits::save(&inst.dir, &new, &service)?;
            let up = async {
                log("checking compose file");
                runner::compose(&inst, &["config", "-q"]).await?;
                log("recreating containers");
                runner::compose(&inst, &["up", "-d"]).await?;
                log("waiting for healthy");
                runner::wait_healthy(&inst, std::time::Duration::from_secs(240)).await
            }
            .await;
            if let Err(e) = up {
                log("failed: putting the old limits back");
                crate::limits::save(&inst.dir, &old, &service)?;
                let back = runner::compose(&inst, &["up", "-d"]).await;
                return Err(match back {
                    Ok(_) => format!("{e} (old limits restored)"),
                    Err(b) => format!("{e} (restoring the old limits also failed: {b})"),
                });
            }
            Ok::<(), String>(())
        }
        .await;
        apps2.note_job(
            "Applying limits to",
            &name,
            result.as_ref().err().map(|e| e.as_str()),
        );
        apps2.with_job(&id, |j| match result {
            Ok(()) => {
                j.log.push("limits applied".into());
                j.state = JobState::Done;
            }
            Err(e) => j.state = JobState::Failed(e),
        });
    });
    job_fragment(&apps, &job_id).ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// `POST /instances/{name}/logcap`: cap the Docker logs of an installed app (10 MB x 3 files
/// per container) and recreate the containers; on failure the cap file is removed again.
pub async fn logcap_submit(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    known_instance(&apps, &name)?;
    let dir = apps.root.join(&name);
    if crate::logcap::enabled(&dir) {
        return Err(StatusCode::CONFLICT);
    }
    let job_id = new_job(&apps, &name, "Capping logs of")?;
    crate::audit::record("app.logcap", &name);
    let apps2 = apps.clone();
    let id = job_id.clone();
    tokio::spawn(async move {
        let inst = runner::Instance {
            name: name.clone(),
            dir,
        };
        let log = |s: &str| {
            apps2.with_job(&id, |j| j.log.push(s.to_string()));
        };
        let result = async {
            log("writing the log cap");
            crate::logcap::ensure(&inst).await?;
            let up = async {
                log("checking compose file");
                runner::compose(&inst, &["config", "-q"]).await?;
                log("recreating containers");
                runner::compose(&inst, &["up", "-d"]).await?;
                log("waiting for healthy");
                runner::wait_healthy(&inst, std::time::Duration::from_secs(240)).await
            }
            .await;
            if let Err(e) = up {
                log("failed: removing the log cap");
                crate::logcap::remove(&inst.dir);
                let back = runner::compose(&inst, &["up", "-d"]).await;
                return Err(match back {
                    Ok(_) => format!("{e} (log cap removed)"),
                    Err(b) => format!("{e} (removing the log cap also failed: {b})"),
                });
            }
            Ok::<(), String>(())
        }
        .await;
        apps2.note_job(
            "Capping logs of",
            &name,
            result.as_ref().err().map(|e| e.as_str()),
        );
        apps2.with_job(&id, |j| match result {
            Ok(()) => {
                j.log.push("logs are capped".into());
                j.state = JobState::Done;
            }
            Err(e) => j.state = JobState::Failed(e),
        });
    });
    job_fragment(&apps, &job_id).ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(Template)]
#[template(path = "restore.html")]
struct RestorePage {
    instance: String,
    snapshots: Vec<SnapshotView>,
    leftovers: usize,
    offsite: bool,
}

fn known_instance(apps: &Apps, name: &str) -> Result<(), StatusCode> {
    if runner::valid_name(name) && apps.root.join(name).join("state.toml").is_file() {
        Ok(())
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

/// Form listing the snapshots of an instance (newest first, newest preselected).
pub async fn restore_form(
    State(apps): State<Apps>,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    known_instance(&apps, &name)?;
    let snapshots = crate::backup::list(&apps.backups_root(), &name)
        .iter()
        .rev()
        .filter_map(|p| {
            let n = p.file_name()?.to_str()?.to_string();
            let when = utc_stamp(n.parse().ok()?);
            let verified = crate::backup::verified_at(p).map(utc_stamp);
            Some(SnapshotView {
                name: n,
                when,
                verified,
            })
        })
        .collect();
    let leftovers = crate::backup::restore_leftover_names(&apps.root.join(&name))
        .map(|v| v.len())
        .unwrap_or(0);
    Ok(render(RestorePage {
        instance: name,
        snapshots,
        leftovers,
        offsite: apps.offsite.get().enabled(),
    }))
}

/// `POST /instances/{name}/pull`: download the newest off-host copy of the app's snapshots
/// (existing local snapshots are never overwritten), so they show up in the Restore list.
pub async fn pull_submit(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    known_instance(&apps, &name)?;
    if !apps.offsite.get().enabled() {
        return Err(StatusCode::CONFLICT);
    }
    let job_id = new_job(&apps, &name, "Downloading backups of")?;
    crate::audit::record("app.backup-pull", &name);
    let apps2 = apps.clone();
    let id = job_id.clone();
    tokio::spawn(async move {
        let result = apps2
            .offsite
            .pull(&apps2.backups_root(), &name, &mut |s| {
                apps2.with_job(&id, |j| j.log.push(s.to_string()));
            })
            .await;
        apps2.note_job(
            "Downloading backups of",
            &name,
            result.as_ref().err().map(|e| e.as_str()),
        );
        apps2.with_job(&id, |j| match result {
            Ok(added) => {
                j.log.push(if added.is_empty() {
                    "nothing new: every off-host snapshot is already here".into()
                } else {
                    "done: open Restore again to pick one".into()
                });
                j.state = JobState::Done;
            }
            Err(e) => j.state = JobState::Failed(e),
        });
    });
    job_fragment(&apps, &job_id).ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// Start a restore job. A safety snapshot is always taken first and current
/// data is moved aside, never deleted.
pub async fn restore_submit(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(values): Form<HashMap<String, String>>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    known_instance(&apps, &name)?;
    let snap = values
        .get("snapshot")
        .cloned()
        .ok_or(StatusCode::BAD_REQUEST)?;
    let state: InstanceState = std::fs::read_to_string(apps.root.join(&name).join("state.toml"))
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let t_idx = apps
        .templates
        .iter()
        .position(|t| t.manifest.app.id == state.template)
        .ok_or(StatusCode::NOT_FOUND)?;
    let job_id = new_job(&apps, &name, "Restoring")?;
    crate::audit::record("app.restore", &name);
    let task_apps = apps.clone();
    let task_id = job_id.clone();
    tokio::spawn(async move {
        let inst = runner::Instance {
            name: name.clone(),
            dir: task_apps.root.join(&name),
        };
        let result = crate::backup::restore(
            &task_apps.backups_root(),
            &inst,
            &task_apps.templates[t_idx],
            Some(&snap),
            true,
            &mut |s| {
                task_apps.with_job(&task_id, |j| j.log.push(s.to_string()));
            },
        )
        .await;
        task_apps.note_job(
            "Restoring",
            &name,
            result.as_ref().err().map(|e| e.as_str()),
        );
        task_apps.with_job(&task_id, |j| match result {
            Ok(p) => {
                j.log.push(format!("restored from {}", p.display()));
                j.state = JobState::Done;
            }
            Err(e) => j.state = JobState::Failed(e),
        });
    });
    job_fragment(&apps, &job_id).ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// Start a job that deletes the `.pre-restore-*` / `.failed-restore-*` copies a restore leaves behind.
pub async fn cleanup_submit(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    known_instance(&apps, &name)?;
    let job_id = new_job(&apps, &name, "Cleaning up")?;
    crate::audit::record("app.cleanup", &name);
    let task_apps = apps.clone();
    let task_id = job_id.clone();
    tokio::spawn(async move {
        let inst = runner::Instance {
            name: name.clone(),
            dir: task_apps.root.join(&name),
        };
        let result = crate::backup::cleanup_restore_leftovers(&inst, &mut |s| {
            task_apps.with_job(&task_id, |j| j.log.push(s.to_string()));
        })
        .await;
        task_apps.with_job(&task_id, |j| match result {
            Ok(v) => {
                j.log
                    .push(format!("removed {} leftover folder(s)", v.len()));
                j.state = JobState::Done;
            }
            Err(e) => j.state = JobState::Failed(e),
        });
    });
    job_fragment(&apps, &job_id).ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(Template)]
#[template(path = "public.html")]
struct PublicPage {
    instance: String,
    hostname: String,
    snippet: Option<String>,
    command: Option<String>,
    https_note: bool,
    error: Option<String>,
    saved: bool,
    cf_ready: bool,
    /// Outcome of a Publish press: (succeeded, step lines).
    result: Option<(bool, Vec<String>)>,
    /// The last Publish failed only because another DNS record uses the name.
    offer_replace: bool,
    /// The hostname is not under any domain the Cloudflare token covers.
    zone_warning: Option<String>,
    /// Published directly through Caddy (no Cloudflare).
    direct: bool,
    /// Preflight lines for direct HTTPS: (ok, text).
    checks: Vec<(bool, String)>,
    /// Expiry of the certificate Caddy serves (direct mode only).
    cert_expiry: Option<String>,
}

fn public_page(
    apps: &Apps,
    name: &str,
    hostname: String,
    error: Option<String>,
    saved: bool,
) -> PublicPage {
    let dir = apps.root.join(name);
    let port = crate::public::web_port(&dir);
    let valid = crate::public::valid_hostname(&hostname);
    let snippet = port.filter(|_| valid).map(|p| {
        format!(
            "- hostname: {hostname}\n  service: {}",
            crate::public::service_url(p)
        )
    });
    let command = valid.then(|| format!("dockwren expose {name}"));
    let https_note = std::fs::read_to_string(dir.join("state.toml"))
        .ok()
        .and_then(|s| toml::from_str::<InstanceState>(&s).ok())
        .and_then(|st| {
            apps.template(&st.template)
                .map(|t| t.manifest.app.needs_public_https)
        })
        .unwrap_or(false);
    PublicPage {
        instance: name.to_string(),
        hostname,
        snippet,
        command,
        https_note,
        error,
        saved,
        cf_ready: crate::public::Settings::from_env().is_some(),
        result: None,
        offer_replace: false,
        zone_warning: None,
        direct: crate::direct::is_direct(&dir),
        checks: Vec::new(),
        cert_expiry: None,
    }
}

/// Warning text when Cloudflare is connected but `host` is under none of its domains.
async fn zone_warning_for(host: &str) -> Option<String> {
    if !crate::public::valid_hostname(host) {
        return None;
    }
    let s = crate::public::Settings::from_env()?;
    let zones = crate::public::zone_names(&s).await.ok()?;
    crate::public::outside_zones(host, &zones).then(|| {
        format!(
            "Warning: {host} is not under any domain this Cloudflare token can manage ({}). Publishing will fail. Use a hostname under one of those domains, or give the token access to this domain in Cloudflare.",
            zones.join(", ")
        )
    })
}

async fn with_zone_warning(mut page: PublicPage) -> PublicPage {
    if page.cf_ready {
        page.zone_warning = zone_warning_for(&page.hostname).await;
    }
    page
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `GET /apps/check-host?PUBLIC_HOST=..`: the install form asks this as the hostname is typed.
pub async fn check_host(Query(q): Query<HashMap<String, String>>) -> Html<String> {
    let host = q.get("PUBLIC_HOST").map(|h| h.trim()).unwrap_or_default();
    Html(match zone_warning_for(host).await {
        Some(w) => format!("<small role=\"alert\">{}</small>", html_escape(&w)),
        None => String::new(),
    })
}

pub async fn public_form(
    State(apps): State<Apps>,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    known_instance(&apps, &name)?;
    let host = crate::public::read(&apps.root.join(&name)).unwrap_or_default();
    let mut page = public_page(&apps, &name, host.clone(), None, false);
    if page.direct {
        page.cert_expiry = crate::direct::cert_expiry(&host).await;
    }
    Ok(render(with_zone_warning(page).await))
}

pub async fn public_submit(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(values): Form<HashMap<String, String>>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    known_instance(&apps, &name)?;
    let host = values
        .get("hostname")
        .map(|h| h.trim().to_lowercase())
        .unwrap_or_default();
    let error = crate::public::write(&apps.root.join(&name), &host).err();
    let saved = error.is_none();
    if saved {
        crate::audit::record("app.hostname", &format!("{name} -> {host}"));
    }
    Ok(render(
        with_zone_warning(public_page(&apps, &name, host, error, saved)).await,
    ))
}

// ---- Cloudflare connection (paste a token) -----------------------------

#[derive(Template)]
#[template(path = "cloudflare.html")]
struct CloudflarePanel {
    env_managed: bool,
    has_token: bool,
    problem: Option<String>,
    zones: Vec<String>,
    tunnel_name: Option<String>,
    choose: Vec<(String, String)>,
    message: Option<(bool, String)>,
}

async fn cloudflare_panel(message: Option<(bool, String)>) -> CloudflarePanel {
    let env = crate::public::Settings::from_environment();
    let saved = crate::public::load_saved();
    let (token, tunnel) = match &env {
        Some(e) => (Some(e.token.clone()), Some(e.tunnel.clone())),
        None => (saved.token.clone(), saved.tunnel.clone()),
    };
    let mut p = CloudflarePanel {
        env_managed: env.is_some(),
        has_token: token.is_some(),
        problem: None,
        zones: Vec::new(),
        tunnel_name: None,
        choose: Vec::new(),
        message,
    };
    if let Some(t) = token {
        match crate::public::probe(&t).await {
            Ok(probe) => {
                p.zones = probe.zones;
                match tunnel.filter(|id| !id.is_empty()) {
                    Some(id) => {
                        p.tunnel_name = Some(
                            probe
                                .tunnels
                                .iter()
                                .find(|(i, _)| *i == id)
                                .map_or(id, |(_, n)| n.clone()),
                        )
                    }
                    None => p.choose = probe.tunnels,
                }
            }
            Err(e) => p.problem = Some(e),
        }
    }
    p
}

pub async fn cloudflare_show() -> Html<String> {
    render(cloudflare_panel(None).await)
}

pub async fn cloudflare_connect(
    headers: HeaderMap,
    Form(values): Form<HashMap<String, String>>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let token = values.get("token").map(|t| t.trim()).unwrap_or_default();
    let msg = if crate::public::Settings::from_environment().is_some() {
        (
            false,
            "Cloudflare is configured by the server environment; change it there.".to_string(),
        )
    } else if token.is_empty()
        || token.len() > 200
        || token.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        (
            false,
            "That does not look like a token: paste only the token text, no spaces.".to_string(),
        )
    } else {
        match crate::public::probe(token).await {
            Err(e) => (false, e),
            Ok(p) => {
                let tunnel = (p.tunnels.len() == 1).then(|| p.tunnels[0].0.clone());
                let saved = crate::public::Saved {
                    token: Some(token.to_string()),
                    account: Some(p.account),
                    tunnel,
                };
                match crate::public::save_saved(&saved) {
                    Ok(()) => {
                        crate::audit::record("cloudflare.connect", "token saved");
                        (true, "token accepted and saved on this server.".to_string())
                    }
                    Err(e) => (false, format!("could not save: {e}")),
                }
            }
        }
    };
    Ok(render(cloudflare_panel(Some(msg)).await))
}

pub async fn cloudflare_tunnel(
    headers: HeaderMap,
    Form(values): Form<HashMap<String, String>>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let id = values.get("tunnel").map(|t| t.trim()).unwrap_or_default();
    let mut saved = crate::public::load_saved();
    let msg = match saved.token.clone() {
        None => (false, "connect a token first".to_string()),
        Some(t) => match crate::public::probe(&t).await {
            Ok(p) if p.tunnels.iter().any(|(i, _)| i == id) => {
                saved.tunnel = Some(id.to_string());
                match crate::public::save_saved(&saved) {
                    Ok(()) => {
                        crate::audit::record("cloudflare.tunnel", "selected");
                        (true, "tunnel selected.".to_string())
                    }
                    Err(e) => (false, format!("could not save: {e}")),
                }
            }
            Ok(_) => (false, "that tunnel is not in your account".to_string()),
            Err(e) => (false, e),
        },
    };
    Ok(render(cloudflare_panel(Some(msg)).await))
}

pub async fn cloudflare_disconnect(headers: HeaderMap) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let msg = match crate::public::forget_saved() {
        Ok(()) => {
            crate::audit::record("cloudflare.disconnect", "token forgotten");
            (
                true,
                "token forgotten. Apps already published stay online.".to_string(),
            )
        }
        Err(e) => (false, e),
    };
    Ok(render(cloudflare_panel(Some(msg)).await))
}

/// `POST /instances/{name}/public/publish`: wire the saved hostname to Cloudflare and
/// show every step plus a clear success or failure line.
pub async fn public_publish(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(values): Form<HashMap<String, String>>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    known_instance(&apps, &name)?;
    let replace_dns = values.get("replace").is_some_and(|v| v == "yes");
    crate::audit::record("app.publish", &name);
    let dir = apps.root.join(&name);
    let host = crate::public::read(&dir).unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    let outcome: Result<(), String> = async {
        if host.is_empty() {
            return Err("save a hostname first".to_string());
        }
        let settings = crate::public::Settings::from_env().ok_or(
            "Cloudflare is not connected: the server needs CLOUDFLARE_API_TOKEN, CLOUDFLARE_ACCOUNT_ID and DOCKWREN_TUNNEL_ID (see the guide below)",
        )?;
        let port = crate::public::web_port(&dir).ok_or("this app has no web port to publish")?;
        crate::public::expose(
            &settings,
            &host,
            &crate::public::service_url(port),
            &[],
            true,
            replace_dns,
            &mut |s| lines.push(s.to_string()),
        )
        .await
    }
    .await;
    let mut page = public_page(&apps, &name, host.clone(), None, false);
    page.result = Some(match outcome {
        Ok(()) => {
            lines.push(format!("Published: https://{host}"));
            (true, lines)
        }
        Err(e) => {
            page.offer_replace = e.starts_with(crate::public::DNS_CONFLICT);
            lines.push(e);
            (false, lines)
        }
    });
    Ok(render(page))
}

/// `POST /instances/{name}/public/direct`: `action` is `check` (preflight only), `on`
/// (publish through Caddy with a Let's Encrypt certificate) or `off` (take it down).
pub async fn public_direct(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(values): Form<HashMap<String, String>>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    known_instance(&apps, &name)?;
    let action = values.get("action").map(String::as_str).unwrap_or("check");
    let dir = apps.root.join(&name);
    let host = crate::public::read(&dir).unwrap_or_default();
    let data_dir = apps.root.parent().unwrap_or(&apps.root).to_path_buf();
    let mut lines: Vec<String> = Vec::new();
    let mut checks: Vec<(bool, String)> = Vec::new();
    let mut ok = true;
    if !crate::public::valid_hostname(&host) {
        ok = false;
        lines.push("save a valid hostname first".to_string());
    } else if action == "off" {
        crate::audit::record("app.direct.off", &name);
        let r = match crate::direct::set_direct(&dir, false) {
            Ok(()) => crate::direct::reconcile(&data_dir, &apps.root).await,
            Err(e) => Err(e),
        };
        match r {
            Ok(()) => lines.push(format!("Direct HTTPS for {host} is off.")),
            Err(e) => {
                ok = false;
                lines.push(e);
            }
        }
    } else {
        let port = crate::public::web_port(&dir);
        let pre = crate::direct::preflight(&data_dir, &host, port).await;
        let blocked = pre.iter().any(|(good, blocks, _)| !good && *blocks);
        checks = pre.into_iter().map(|(g, _, t)| (g, t)).collect();
        if action == "on" {
            if blocked {
                ok = false;
                lines.push("Not published: fix the red items first.".to_string());
            } else {
                crate::audit::record("app.direct.on", &format!("{name} -> {host}"));
                let r = match crate::direct::set_direct(&dir, true) {
                    Ok(()) => crate::direct::reconcile(&data_dir, &apps.root).await,
                    Err(e) => Err(e),
                };
                match r {
                    Ok(()) => lines.push(format!(
                        "Caddy now serves https://{host}. The first certificate can take a minute; see the Caddy container log if it does not appear."
                    )),
                    Err(e) => {
                        ok = false;
                        let _ = crate::direct::set_direct(&dir, false);
                        lines.push(e);
                    }
                }
            }
        } else {
            ok = !blocked;
            lines.push(if blocked {
                "Preflight found a blocking problem.".to_string()
            } else {
                "Preflight passed.".to_string()
            });
        }
    }
    let mut page = public_page(&apps, &name, host.clone(), None, false);
    if page.direct {
        page.cert_expiry = crate::direct::cert_expiry(&host).await;
    }
    page.checks = checks;
    page.result = Some((ok, lines));
    Ok(render(page))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    fn notifying_apps(tag: &str) -> (Apps, crate::notify::Shared, PathBuf) {
        let root = std::env::temp_dir().join(format!("dw-apps-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let n: crate::notify::Shared = Arc::new(StdMutex::new(crate::notify::Store::memory()));
        let apps =
            Apps::new(crate::template::catalog(None).unwrap(), root.clone()).with_notify(n.clone());
        (apps, n, root)
    }

    fn instance(root: &std::path::Path, name: &str, template: &str, version: u32) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("state.toml"),
            format!("template = \"{template}\"\nversion = {version}\n"),
        )
        .unwrap();
    }

    #[test]
    fn update_notifications_follow_the_template_versions() {
        let (apps, n, root) = notifying_apps("upd");
        let bundled = apps.template("gitea").unwrap().manifest.app.version;
        instance(&root, "old", "gitea", bundled - 1);
        instance(&root, "cur", "gitea", bundled);
        apps.sync_update_notifications();
        apps.sync_update_notifications(); // idempotent
        {
            let s = n.lock().unwrap();
            let open = s.open_events();
            assert_eq!(open.len(), 1);
            assert_eq!(open[0].key, "app.update_available:old");
            assert_eq!(open[0].severity, crate::notify::Severity::Info);
        }
        instance(&root, "old", "gitea", bundled); // updated
        apps.sync_update_notifications();
        assert!(n.lock().unwrap().open_events().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_skipped_version_is_not_pending_until_a_newer_one_exists() {
        let (apps, n, root) = notifying_apps("skip");
        let bundled = apps.template("gitea").unwrap().manifest.app.version;
        instance(&root, "old", "gitea", bundled - 1);
        assert_eq!(apps.pending_updates().len(), 1);
        std::fs::write(root.join("old").join("skip-update"), bundled.to_string()).unwrap();
        assert!(apps.pending_updates().is_empty());
        apps.sync_update_notifications();
        assert!(n.lock().unwrap().open_events().is_empty());
        // A skip for an older version does not hide the current one.
        std::fs::write(
            root.join("old").join("skip-update"),
            (bundled - 1).to_string(),
        )
        .unwrap();
        assert_eq!(apps.pending_updates().len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn removed_instance_clears_its_update_notification() {
        let (apps, n, root) = notifying_apps("rm");
        let bundled = apps.template("gitea").unwrap().manifest.app.version;
        instance(&root, "gone", "gitea", bundled - 1);
        apps.sync_update_notifications();
        assert_eq!(n.lock().unwrap().open_events().len(), 1);
        std::fs::remove_dir_all(root.join("gone")).unwrap();
        apps.sync_update_notifications();
        assert!(n.lock().unwrap().open_events().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn failed_jobs_raise_and_a_later_success_clears() {
        let (apps, n, root) = notifying_apps("job");
        apps.note_job(
            "Updating",
            "gt",
            Some("pull failed\nsecond line with detail"),
        );
        apps.note_job("Updating", "gt", Some("pull failed again"));
        {
            let s = n.lock().unwrap();
            let open = s.open_events();
            assert_eq!(open.len(), 1);
            assert_eq!(open[0].title, "Updating gt failed");
            assert_eq!(open[0].body.as_deref(), Some("pull failed again"));
        }
        apps.note_job("Updating", "other", None); // other key: no effect
        assert_eq!(n.lock().unwrap().open_events().len(), 1);
        apps.note_job("Updating", "gt", None);
        assert!(n.lock().unwrap().open_events().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn first_error_line_only_goes_into_the_notification() {
        let (apps, n, root) = notifying_apps("line");
        apps.note_job("Installing", "x", Some("short\nTOKEN=abc123"));
        let s = n.lock().unwrap();
        assert!(
            !s.open_events()[0]
                .body
                .as_deref()
                .unwrap()
                .contains("abc123")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    use super::utc_stamp;

    #[test]
    fn utc_stamps() {
        assert_eq!(utc_stamp(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc_stamp(951_782_400), "2000-02-29 00:00 UTC");
        assert_eq!(utc_stamp(1_794_582_685), "2026-11-13 15:11 UTC");
    }
}

#[cfg(test)]
mod offsite_page_tests {
    use super::*;

    #[test]
    fn generated_password_is_shown_once_and_secrets_never_are() {
        let apps = Apps::new(Vec::new(), PathBuf::from("/nonexistent"));
        let html = offsite_html(
            &apps,
            Some("Saved."),
            None,
            Some("pw-shown-once-xyz".into()),
        )
        .0;
        assert!(html.contains("pw-shown-once-xyz"), "{html}");
        let again = offsite_html(&apps, None, None, None).0;
        assert!(!again.contains("pw-shown-once-xyz"));
    }
}
