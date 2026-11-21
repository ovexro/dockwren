//! Restore, pull and cleanup of snapshots.

use super::*;

#[derive(Template)]
#[template(path = "restore.html")]
pub(super) struct RestorePage {
    pub(super) instance: String,
    pub(super) snapshots: Vec<SnapshotView>,
    pub(super) leftovers: usize,
    pub(super) offsite: bool,
}

pub(super) fn known_instance(apps: &Apps, name: &str) -> Result<(), StatusCode> {
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
    let tpl = apps
        .template(&state.template)
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
            tpl,
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
