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
    schedule: Arc<crate::schedule::Schedule>,
    offsite: Arc<crate::offsite::Offsite>,
}

impl Apps {
    pub fn new(templates: Vec<AppTemplate>, root: PathBuf) -> Self {
        Self {
            templates: Arc::new(templates),
            root,
            jobs: Arc::default(),
            lock: Arc::default(),
            notify: None,
            schedule: Arc::new(crate::schedule::Schedule::memory()),
            offsite: Arc::new(crate::offsite::Offsite::memory()),
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
                }
            }
        });
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

    fn instance_names(&self) -> Vec<String> {
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
}

// ---- views -------------------------------------------------------------

struct AppView {
    id: String,
    name: String,
    description: String,
    https_note: bool,
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
}

struct InstanceView {
    name: String,
    app: String,
    ports: String,
    status: String,
    /// "1 → 2" when the bundled template is newer than the instance.
    update: Option<String>,
    snapshots: usize,
    last_backup: Option<String>,
    hostname: Option<String>,
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
            .map(|t| AppView {
                id: t.manifest.app.id.clone(),
                name: t.manifest.app.name.clone(),
                description: t.manifest.app.description.clone(),
                https_note: t.manifest.app.needs_public_https,
            })
            .collect(),
    })
}

/// `GET /apps/{id}/icon.svg`: the template's icon, locked down so it can only ever be drawn.
pub async fn icon(
    State(apps): State<Apps>,
    Path(id): Path<String>,
) -> Result<Response, StatusCode> {
    let svg = apps
        .template(&id)
        .and_then(|t| t.icon.clone())
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
            .and_then(|r| runner::prepare(&apps.root, t, &r.env).map(|inst| (r, inst)))
    };
    let (resolved, inst) = match prepared {
        Ok(ok) => ok,
        Err(e) => return Ok(render(form_view(t, &values, Some(e)))),
    };

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
        let update = apps
            .template(&state.template)
            .filter(|t| t.manifest.app.version > state.version)
            .map(|t| format!("{} → {}", state.version, t.manifest.app.version));
        let snaps = crate::backup::list(&apps.backups_root(), &name);
        let snapshots = snaps.len();
        let last_backup = snaps
            .last()
            .and_then(|p| p.file_name()?.to_str()?.parse::<u64>().ok())
            .map(|e| utc_stamp(e).trim_end_matches(" UTC").to_string());
        out.push(InstanceView {
            name,
            app: state.template,
            ports: instance_ports(&dir),
            status,
            update,
            snapshots,
            last_backup,
            hostname: crate::public::read(&dir),
        });
    }
    out
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
    let error = runner::take_down(&runner::Instance { name, dir })
        .await
        .err();
    Ok(render(InstancesPage {
        instances: instance_views(&apps).await,
        error,
    }))
}

/// Load the template catalog for the server (bundled + optional override dir).
pub fn catalog(cfg: &crate::config::Config) -> Result<Vec<AppTemplate>, String> {
    template::catalog(cfg.apps_dir_override().as_deref())
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
    if !runner::valid_name(&name) || !apps.root.join(&name).join("state.toml").is_file() {
        return Err(StatusCode::NOT_FOUND);
    }
    let job_id = new_job(&apps, &name, "Updating")?;
    let task_apps = apps.clone();
    let task_id = job_id.clone();
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
    job_fragment(&apps, &id).ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(Template)]
#[template(path = "backup_schedule.html")]
struct SchedulePage {
    mode: &'static str,
    hour: u8,
    keep: usize,
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
}

pub async fn schedule_save(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Form(f): Form<ScheduleForm>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let s = match crate::schedule::Settings::from_form(&f.mode, &f.hour, &f.keep) {
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
        },
    ));
    Ok(job_id)
}

/// "2026-11-13 15:11 UTC" from epoch seconds (no time crate needed).
fn utc_stamp(epoch: u64) -> String {
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
}

#[derive(Template)]
#[template(path = "restore.html")]
struct RestorePage {
    instance: String,
    snapshots: Vec<SnapshotView>,
    leftovers: usize,
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
            Some(SnapshotView { name: n, when })
        })
        .collect();
    let leftovers = crate::backup::restore_leftover_names(&apps.root.join(&name))
        .map(|v| v.len())
        .unwrap_or(0);
    Ok(render(RestorePage {
        instance: name,
        snapshots,
        leftovers,
    }))
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
    Ok(render(
        with_zone_warning(public_page(&apps, &name, host, None, false)).await,
    ))
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
                    Ok(()) => (true, "token accepted and saved on this server.".to_string()),
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
                    Ok(()) => (true, "tunnel selected.".to_string()),
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
        Ok(()) => (
            true,
            "token forgotten. Apps already published stay online.".to_string(),
        ),
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
