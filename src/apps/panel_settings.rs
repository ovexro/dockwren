//! Panel-wide settings: backup schedule, config export, off-host copy, registries.

use super::*;

#[derive(Template)]
#[template(path = "backup_schedule.html")]
pub(super) struct SchedulePage {
    pub(super) mode: &'static str,
    pub(super) hour: u8,
    pub(super) keep: usize,
    pub(super) auto_update: bool,
    pub(super) update_from: u8,
    pub(super) update_to: u8,
    pub(super) restore_test: bool,
    pub(super) last: Option<String>,
    pub(super) note: Option<&'static str>,
    pub(super) error: Option<&'static str>,
}

pub(super) fn schedule_html(
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

#[derive(Deserialize)]
pub struct ConfigDownloadForm {
    password: String,
    #[serde(default)]
    code: String,
    passphrase: String,
}

pub(super) fn config_download_error(msg: &str) -> Response {
    let msg = msg.replace('&', "&amp;").replace('<', "&lt;");
    (
        StatusCode::BAD_REQUEST,
        Html(format!(
            "<!doctype html><meta charset=\"utf-8\"><title>Config export</title><p role=\"alert\">{msg}</p><p><a href=\"/settings/tab/backups\">Back to Settings</a></p>"
        )),
    )
        .into_response()
}

/// `POST /settings/config-export`: after re-auth, download the sealed config export
/// (compose files, `.env` passwords, panel settings; no data) encrypted with a passphrase
/// typed here. Restore it with `dockwren import` / `dockwren recover`.
pub async fn config_download(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Form(f): Form<ConfigDownloadForm>,
) -> Response {
    if !crate::same_origin(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(auth) = apps.auth.clone() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    if !auth.reauth(f.password, &f.code).await {
        return config_download_error("Wrong password or code.");
    }
    let root = apps.root.clone();
    let pass = f.passphrase;
    let res = tokio::task::spawn_blocking(move || {
        crate::configbundle::export_bytes(&root, &pass, crate::configbundle::MIN_PASSPHRASE)
    })
    .await;
    match res {
        Ok(Ok((bytes, n))) => {
            crate::audit::record("config.export_download", &format!("{n} instances"));
            (
                [
                    (header::CONTENT_TYPE, "application/octet-stream"),
                    (
                        header::CONTENT_DISPOSITION,
                        "attachment; filename=\"dockwren-config.dwx\"",
                    ),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                bytes,
            )
                .into_response()
        }
        Ok(Err(e)) => config_download_error(&e),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Template)]
#[template(path = "offsite.html")]
pub(super) struct OffsitePage {
    pub(super) kind: &'static str,
    pub(super) endpoint: String,
    pub(super) bucket: String,
    pub(super) host: String,
    pub(super) port: u16,
    pub(super) user: String,
    pub(super) path: String,
    pub(super) has_secrets: bool,
    pub(super) describe: String,
    pub(super) last: Option<String>,
    pub(super) new_password: Option<String>,
    pub(super) note: Option<&'static str>,
    pub(super) error: Option<String>,
}

pub(super) fn offsite_html(
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

#[derive(Template)]
#[template(path = "registries.html")]
pub(super) struct RegistriesPage {
    pub(super) logins: Vec<(String, String)>,
    pub(super) note: Option<String>,
    pub(super) error: Option<String>,
}

pub(super) fn registries_html(
    apps: &Apps,
    note: Option<String>,
    error: Option<String>,
) -> Html<String> {
    let logins = apps
        .root
        .parent()
        .map(crate::registry::list)
        .unwrap_or_default()
        .into_iter()
        .map(|k| (crate::registry::display_name(&k), k))
        .collect();
    render(RegistriesPage {
        logins,
        note,
        error,
    })
}

pub async fn registries_show(State(apps): State<Apps>) -> Html<String> {
    registries_html(&apps, None, None)
}

#[derive(serde::Deserialize)]
pub struct RegistryForm {
    #[serde(default)]
    server: String,
    #[serde(default)]
    user: String,
    #[serde(default)]
    token: String,
}

pub async fn registries_login(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Form(f): Form<RegistryForm>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let Some(data) = apps.root.parent() else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };
    Ok(
        match crate::registry::login(data, &f.server, &f.user, &f.token).await {
            Ok(name) => {
                crate::audit::record("registry.login", &name);
                registries_html(&apps, Some(format!("Logged in to {name}.")), None)
            }
            Err(e) => registries_html(
                &apps,
                None,
                Some(format!(
                    "Not saved: {}",
                    if e.is_empty() { "login refused" } else { &e }
                )),
            ),
        },
    )
}

pub async fn registries_logout(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Form(f): Form<RegistryForm>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let Some(data) = apps.root.parent() else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };
    Ok(match crate::registry::logout(data, &f.server).await {
        Ok(()) => {
            let name = crate::registry::display_name(&f.server);
            crate::audit::record("registry.logout", &name);
            registries_html(&apps, Some(format!("Logged out of {name}.")), None)
        }
        Err(e) => registries_html(&apps, None, Some(e)),
    })
}
