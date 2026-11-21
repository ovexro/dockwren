//! Per-instance settings, passwords, limits and log cap.

use super::*;

pub(super) struct SnapshotView {
    pub(super) name: String,
    pub(super) when: String,
    pub(super) verified: Option<String>,
}

#[derive(Template)]
#[template(path = "settings.html")]
pub(super) struct SettingsPage {
    pub(super) instance: String,
    pub(super) fields: Vec<FieldView>,
    pub(super) error: Option<String>,
}

/// The editable fields of an instance with their current values (from `.env`).
pub(super) fn settings_view(
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
pub(super) fn is_viewable(s: &crate::template::Secret) -> bool {
    s.reveal && matches!(s.kind, crate::template::SecretKind::Password)
}

#[derive(Template)]
#[template(path = "passwords.html")]
pub(super) struct PasswordsPage {
    pub(super) instance: String,
    /// (key, can be rotated)
    pub(super) secrets: Vec<(String, bool)>,
    pub(super) two_factor: bool,
    pub(super) error: Option<String>,
    pub(super) shown: Vec<(String, String)>,
}

pub(super) fn passwords_view(
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
pub(super) struct LimitsPage {
    pub(super) instance: String,
    pub(super) service: String,
    pub(super) memory: String,
    pub(super) cpus: String,
    pub(super) error: Option<String>,
}

/// Name of the main service of an instance, from its template.
pub(super) fn main_service(apps: &Apps, dir: &std::path::Path) -> Result<String, StatusCode> {
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
