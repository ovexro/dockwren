//! Catalog loading, updates, backups, verify, restore tests and clones.

use super::*;

/// Load the template catalog for the server (bundled + optional override dir).
pub fn catalog(cfg: &crate::config::Config) -> Result<Vec<AppTemplate>, String> {
    catalog_in(&cfg.data_dir, cfg.apps_dir_override().as_deref())
}

pub(super) fn catalog_in(
    data_dir: &std::path::Path,
    over: Option<&std::path::Path>,
) -> Result<Vec<AppTemplate>, String> {
    let mut all = template::catalog(over)?;
    for t in crate::thirdparty::load_all(data_dir) {
        if all.iter().all(|a| a.manifest.app.id != t.manifest.app.id) {
            all.push(t);
        }
    }
    all.sort_by(|a, b| a.manifest.app.id.cmp(&b.manifest.app.id));
    Ok(all)
}

#[derive(Template)]
#[template(path = "update_preview.html")]
pub(super) struct UpdatePreviewPage {
    pub(super) instance: String,
    pub(super) skipped: bool,
    pub(super) p: crate::update::Preview,
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

pub(super) fn skip_update(
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

/// `POST /instances/bulk/{action}` (`backup` or `update`): the ticked apps (form field `name`,
/// repeated), or every app when none is ticked.
pub async fn instances_bulk(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(action): Path<String>,
    Form(values): Form<Vec<(String, String)>>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let update = match action.as_str() {
        "backup" => false,
        "update" => true,
        _ => return Err(StatusCode::NOT_FOUND),
    };
    let names: Vec<String> = values
        .into_iter()
        .filter(|(k, _)| k == "name")
        .map(|(_, v)| v)
        .collect();
    if names.is_empty() {
        // Never fall back to "everything": a lost tick must not start a run on all apps.
        return Ok(Html(
            "<p role=\"alert\">Tick at least one app first.</p>".to_string(),
        ));
    }
    let id = apps.spawn_bulk(update, names.clone())?;
    crate::audit::record(
        if update {
            "app.bulk_update"
        } else {
            "app.bulk_backup"
        },
        &names.join(","),
    );
    job_fragment(&apps, &id).ok_or(StatusCode::INTERNAL_SERVER_ERROR)
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
