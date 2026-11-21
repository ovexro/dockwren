//! Instance list and per-instance actions (power, needs, start, apply, delete).

use super::*;

#[derive(Deserialize)]
pub(super) struct InstanceState {
    pub(super) template: String,
    pub(super) version: u32,
}

/// The template version the user chose to skip for this instance, if any.
pub(super) fn skipped_version(dir: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(dir.join("skip-update"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub(super) fn instance_ports(dir: &std::path::Path) -> String {
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

pub(super) async fn instance_views(apps: &Apps) -> Vec<InstanceView> {
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
        let status_is_stopped = status == "stopped";
        let drift = if status_is_stopped {
            Vec::new()
        } else {
            crate::drift::check(&inst).await.unwrap_or_default()
        };
        let newer = apps
            .template(&state.template)
            .map(|t| t.manifest.app.version)
            .filter(|v| *v > state.version);
        let update = newer.map(|v| format!("{} → {}", state.version, v));
        let update_skipped = newer.is_some() && skipped_version(&dir) == newer;
        let snaps = crate::backup::list(&apps.backups_root(), &name);
        let snapshots = snaps.len();
        let last_backup_at = snaps
            .last()
            .and_then(|p| p.file_name()?.to_str()?.parse::<u64>().ok());
        let last_backup = last_backup_at.map(|e| utc_stamp(e).trim_end_matches(" UTC").to_string());
        let name_for_upstream = name.clone();
        let editable = apps
            .template(&state.template)
            .is_some_and(|t| t.manifest.fields.iter().any(|f| f.editable));
        let stack_editable = state.template == CUSTOM_ID;
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
            last_backup_at,
            hostname: crate::public::read(&dir),
            public_up: crate::webcheck::status(&name_for_upstream)
                .map(|s| s == crate::webcheck::Status::Up),
            uptime: crate::webcheck::uptime_line(&dir),
            vulns: crate::vulnscan::summary(&name_for_upstream).and_then(|v| v.line()),
            editable,
            stack_editable,
            deletable: status_is_stopped && crate::public::read(&dir).is_none(),
            needs: Vec::new(),
            stopped: status_is_stopped,
            drift,
            power: crate::power::read(&dir).describe(),
            stop_confirm: String::new(),
            delete_confirm: String::new(),
            limitable,
            has_passwords,
            uncapped: !crate::logcap::enabled(&dir),
        });
    }
    // Needs between apps: which ones are up, and who is affected when this one stops.
    let map = crate::needs::all(root);
    let healthy: std::collections::HashSet<String> = out
        .iter()
        .filter(|v| v.status == "healthy")
        .map(|v| v.name.clone())
        .collect();
    for v in &mut out {
        v.needs = map
            .get(&v.name)
            .into_iter()
            .flatten()
            .map(|n| (n.clone(), healthy.contains(n)))
            .collect();
        let users = crate::needs::dependents(&map, &v.name);
        let also = if users.is_empty() {
            String::new()
        } else {
            format!(" {} needs it and will stop working.", users.join(", "))
        };
        v.stop_confirm = format!(
            "Stop {} and remove its containers? Data is kept.{also}",
            v.name
        );
        v.delete_confirm = format!(
            "Delete {} completely? Its volumes and data folder are erased and cannot be recovered. Backups are kept.{also}",
            v.name
        );
    }
    out
}

/// Read-only status of every app for the JSON API: health, last backup, scan counts.
/// No hostnames, ports or secrets.
pub async fn status_json(apps: &Apps) -> Vec<serde_json::Value> {
    instance_views(apps)
        .await
        .into_iter()
        .map(|v| {
            let scan = crate::vulnscan::summary(&v.name).map(|s| {
                serde_json::json!({
                    "scanned_at": s.scanned,
                    "critical": s.critical,
                    "high": s.high,
                    "fixable_critical": s.fixable_critical,
                })
            });
            serde_json::json!({
                "name": v.name,
                "app": v.app,
                "status": v.status,
                "healthy": v.status == "healthy",
                "public_up": v.public_up,
                "uptime": v.uptime,
                "update_available": v.update.is_some(),
                "backup": {"snapshots": v.snapshots, "last_at": v.last_backup_at},
                "scan": scan,
            })
        })
        .collect()
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

pub async fn instances(
    State(apps): State<Apps>,
    axum::extract::Query(q): axum::extract::Query<Vec<(String, String)>>,
) -> Html<String> {
    render(InstancesPage {
        instances: instance_views(&apps).await,
        picked: q
            .into_iter()
            .filter(|(k, _)| k == "name")
            .map(|(_, v)| v)
            .collect(),
        error: None,
        note: None,
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
        crate::power::forget_stopped(&apps.root.join(&removed));
        crate::terminal::set_adopted(runner::adopted_projects(&apps.root));
    }
    let note = error
        .is_none()
        .then(|| format!("{removed}: containers removed, data kept. It stays in the list as stopped; Delete app removes it completely."));
    Ok(render(InstancesPage {
        instances: instance_views(&apps).await,
        picked: Vec::new(),
        error,
        note,
    }))
}

#[derive(Template)]
#[template(path = "power.html")]
pub(super) struct PowerForm {
    pub(super) name: String,
    pub(super) stop: String,
    pub(super) start: String,
    pub(super) error: Option<String>,
}

pub(super) fn power_view(
    apps: &Apps,
    name: &str,
    error: Option<String>,
    typed: Option<(&str, &str)>,
) -> PowerForm {
    let plan = crate::power::read(&apps.root.join(name));
    let t = |m: Option<u16>| m.map(crate::power::fmt_time).unwrap_or_default();
    PowerForm {
        name: name.to_string(),
        stop: typed.map_or_else(|| t(plan.stop), |(s, _)| s.to_string()),
        start: typed.map_or_else(|| t(plan.start), |(_, s)| s.to_string()),
        error,
    }
}

/// `GET /instances/{name}/power`: the stop/start times of one app.
pub async fn power_form(
    State(apps): State<Apps>,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    known_instance(&apps, &name)?;
    Ok(render(power_view(&apps, &name, None, None)))
}

#[derive(serde::Deserialize)]
pub struct PowerValues {
    #[serde(default)]
    stop: String,
    #[serde(default)]
    start: String,
}

/// `POST /instances/{name}/power`: save (or clear) the stop/start times.
pub async fn power_submit(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(v): Form<PowerValues>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    known_instance(&apps, &name)?;
    let plan = match crate::power::Plan::from_form(&v.stop, &v.start) {
        Ok(p) => p,
        Err(e) => {
            return Ok(render(power_view(
                &apps,
                &name,
                Some(e.to_string()),
                Some((&v.stop, &v.start)),
            )));
        }
    };
    if let Err(e) = crate::power::write(&apps.root.join(&name), &plan, crate::schedule::now()) {
        return Ok(render(power_view(
            &apps,
            &name,
            Some(format!("could not save: {e}")),
            Some((&v.stop, &v.start)),
        )));
    }
    let note = plan.describe().map_or_else(
        || format!("{name} has no schedule."),
        |d| format!("{name} {d}."),
    );
    crate::audit::record("app.power", &format!("{name}: {note}"));
    Ok(Html(format!(
        "<div id=\"power\"><p>{note}</p><button type=\"button\" data-clear=\"install\">Close</button></div>"
    )))
}

#[derive(Template)]
#[template(path = "needs.html")]
pub(super) struct NeedsForm {
    pub(super) name: String,
    /// Every other app and whether it is ticked.
    pub(super) choices: Vec<(String, bool)>,
    pub(super) error: Option<String>,
}

pub(super) fn needs_view(
    apps: &Apps,
    name: &str,
    ticked: Option<&[String]>,
    error: Option<String>,
) -> NeedsForm {
    let map = crate::needs::all(&apps.root);
    let current = map.get(name).cloned().unwrap_or_default();
    let ticked = ticked.unwrap_or(&current);
    NeedsForm {
        name: name.to_string(),
        choices: map
            .keys()
            .filter(|k| k.as_str() != name)
            .map(|k| (k.clone(), ticked.contains(k)))
            .collect(),
        error,
    }
}

/// `GET /instances/{name}/needs`: tick the apps this one needs.
pub async fn needs_form(
    State(apps): State<Apps>,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    known_instance(&apps, &name)?;
    Ok(render(needs_view(&apps, &name, None, None)))
}

/// `POST /instances/{name}/needs`: save the ticked apps (no loops, only installed apps).
pub async fn needs_submit(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(values): Form<Vec<(String, String)>>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    known_instance(&apps, &name)?;
    let wanted: Vec<String> = values
        .into_iter()
        .filter(|(k, _)| k == "need")
        .map(|(_, v)| v)
        .collect();
    let map = crate::needs::all(&apps.root);
    let saved = crate::needs::validate(&map, &name, &wanted).and_then(|clean| {
        crate::needs::write(&apps.root.join(&name), &clean)
            .map(|()| clean)
            .map_err(|e| format!("could not save: {e}"))
    });
    match saved {
        Ok(clean) => {
            crate::audit::record("app.needs", &format!("{name}: {}", clean.join(",")));
            let note = if clean.is_empty() {
                format!("{name} needs no other app.")
            } else {
                format!("{name} needs {}.", clean.join(", "))
            };
            Ok(Html(format!(
                "<div id=\"needs\"><p>{note}</p><button type=\"button\" data-clear=\"install\">Close</button></div>"
            )))
        }
        Err(e) => Ok(render(needs_view(&apps, &name, Some(&wanted), Some(e)))),
    }
}

/// `POST /instances/{name}/start`: start a stopped app, the apps it needs first.
pub async fn instance_start(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    known_instance(&apps, &name)?;
    let job_id = new_job(&apps, &name, "Starting")?;
    crate::audit::record("app.start", &name);
    crate::power::forget_stopped(&apps.root.join(&name));
    let apps2 = apps.clone();
    let id = job_id.clone();
    tokio::spawn(async move {
        let result = apps2
            .start_with_needs(&name, |s| {
                apps2.with_job(&id, |j| j.log.push(s.to_string()));
            })
            .await;
        apps2.note_job("Starting", &name, result.as_ref().err().map(|e| e.as_str()));
        apps2.with_job(&id, |j| match result {
            Ok(()) => {
                j.log.push("started".into());
                j.state = JobState::Done;
            }
            Err(e) => j.state = JobState::Failed(e),
        });
    });
    job_fragment(&apps, &job_id).ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// `POST /instances/{name}/apply`: bring the containers in line with the Compose files
/// (`up -d` recreates only what drifted).
pub async fn instance_apply(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    known_instance(&apps, &name)?;
    let job_id = new_job(&apps, &name, "Applying")?;
    crate::audit::record("app.apply", &name);
    let apps2 = apps.clone();
    let id = job_id.clone();
    tokio::spawn(async move {
        let inst = runner::Instance {
            dir: apps2.root.join(&name),
            name: name.clone(),
        };
        apps2.with_job(&id, |j| j.log.push("recreating what differs".into()));
        let result = match runner::compose(&inst, &["up", "-d"]).await {
            Ok(_) => runner::wait_stable(&inst, std::time::Duration::from_secs(240)).await,
            Err(e) => Err(e),
        };
        apps2.note_job("Applying", &name, result.as_ref().err().map(|e| e.as_str()));
        apps2.with_job(&id, |j| match result {
            Ok(()) => {
                j.log.push("in sync".into());
                j.state = JobState::Done;
            }
            Err(e) => j.state = JobState::Failed(e),
        });
    });
    job_fragment(&apps, &job_id).ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// `POST /instances/{name}/delete`: remove a stopped, unpublished app completely: its
/// containers' leftovers (volumes, network) and its folder with all data. Backups are kept.
pub async fn instance_delete(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    known_instance(&apps, &name)?;
    let dir = apps.root.join(&name);
    let inst = runner::Instance {
        name: name.clone(),
        dir: dir.clone(),
    };
    let refuse = if crate::public::read(&dir).is_some() {
        Some("Remove its Public URL first.".to_string())
    } else {
        match runner::compose(&inst, &["ps", "-a", "--format", "json"]).await {
            Ok(ps) => match runner::judge(&ps) {
                runner::Status::Waiting(w) if w == "no containers yet" => None,
                _ => Some("Stop & remove its containers first.".to_string()),
            },
            Err(e) => Some(e),
        }
    };
    let mut error = refuse.map(|r| format!("{name} was not deleted: {r}"));
    let mut note = None;
    if error.is_none() {
        let done = runner::compose(&inst, &["down", "-v"])
            .await
            .map(|_| ())
            .and_then(|()| {
                // `dir` is `apps.root/<name>` with a validated name that has a state.toml.
                std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))
            });
        match done {
            Ok(()) => {
                crate::audit::record("app.delete", &name);
                crate::needs::forget(&apps.root, &name);
                crate::terminal::set_adopted(runner::adopted_projects(&apps.root));
                note = Some(format!(
                    "{name} deleted: containers, volumes and data are gone. Its backups are kept."
                ));
            }
            Err(e) => error = Some(format!("{name} was not fully deleted: {e}")),
        }
    }
    Ok(render(InstancesPage {
        instances: instance_views(&apps).await,
        picked: Vec::new(),
        error,
        note,
    }))
}
