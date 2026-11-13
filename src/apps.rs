//! Web UI for templates and instances: app list, install form, install jobs,
//! instance list. Server-rendered fragments driven by htmx.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use askama::Template;
use axum::{
    extract::{Form, Path, State},
    http::{HeaderMap, StatusCode},
    response::Html,
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
}

impl Apps {
    pub fn new(templates: Vec<AppTemplate>, root: PathBuf) -> Self {
        Self {
            templates: Arc::new(templates),
            root,
            jobs: Arc::default(),
            lock: Arc::default(),
        }
    }

    fn template(&self, id: &str) -> Option<&AppTemplate> {
        self.templates.iter().find(|t| t.manifest.app.id == id)
    }

    fn with_job<R>(&self, id: &str, f: impl FnOnce(&mut Job) -> R) -> Option<R> {
        let mut jobs = self.jobs.lock().unwrap();
        jobs.iter_mut().find(|(j, _)| j == id).map(|(_, job)| f(job))
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
#[template(path = "apps.html")]
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
}

#[derive(Template)]
#[template(path = "instances.html")]
struct InstancesPage {
    instances: Vec<InstanceView>,
    error: Option<String>,
}

fn form_view(t: &AppTemplate, values: &HashMap<String, String>, error: Option<String>) -> FormPage {
    let fields = t
        .manifest
        .fields
        .iter()
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
                value: if bool_or_select && entered.is_empty() { default.clone() } else { entered },
                placeholder: default,
                help: f.help.clone().unwrap_or_default(),
                options,
                required: f.required && f.default.is_none(),
            }
        })
        .collect();
    FormPage {
        id: t.manifest.app.id.clone(),
        name: t.manifest.app.name.clone(),
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

pub async fn new_form(State(apps): State<Apps>, Path(id): Path<String>) -> Result<Html<String>, StatusCode> {
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
        .filter_map(|k| resolved.env.get(*k).map(|v| (k.to_string(), format!("127.0.0.1:{v}"))))
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
            let mut shown = runner::run_post_start(&inst, t, &resolved.env, &mut |s| push(s)).await?;
            shown.extend(resolved.revealed.iter().cloned());
            Ok::<_, String>(shown)
        }
        .await;
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
        let secrets = if j.state == JobState::Done { std::mem::take(&mut j.secrets) } else { Vec::new() };
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

pub async fn job(State(apps): State<Apps>, Path(id): Path<String>) -> Result<Html<String>, StatusCode> {
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
    ports.values().map(|p| format!("127.0.0.1:{p}")).collect::<Vec<_>>().join(", ")
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
        let Ok(state) = std::fs::read_to_string(dir.join("state.toml")) else { continue };
        let Ok(state) = toml::from_str::<InstanceState>(&state) else { continue };
        let inst = runner::Instance { name: name.clone(), dir: dir.clone() };
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
        out.push(InstanceView { name, app: state.template, ports: instance_ports(&dir), status, update });
    }
    out
}

pub async fn instances(State(apps): State<Apps>) -> Html<String> {
    render(InstancesPage { instances: instance_views(&apps).await, error: None })
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
    let error = runner::take_down(&runner::Instance { name, dir }).await.err();
    Ok(render(InstancesPage { instances: instance_views(&apps).await, error }))
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
    let job_id = crate::auth::generate_password();
    {
        let mut jobs = apps.jobs.lock().unwrap();
        if jobs.iter().any(|(_, j)| j.instance == name && j.state == JobState::Running) {
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
                instance: name.clone(),
                verb: "Updating",
                log: Vec::new(),
                state: JobState::Running,
                secrets: Vec::new(),
                ports: Vec::new(),
            },
        ));
    }
    let task_apps = apps.clone();
    let task_id = job_id.clone();
    tokio::spawn(async move {
        let backups = task_apps.root.parent().map(|d| d.join("backups")).unwrap_or_else(|| task_apps.root.join("backups"));
        let result = crate::update::update_instance(&task_apps.root, &backups, &task_apps.templates, &name, &mut |s| {
            task_apps.with_job(&task_id, |j| j.log.push(s.to_string()));
        })
        .await;
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
