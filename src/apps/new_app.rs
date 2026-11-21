//! App list, install form and install jobs.

use super::*;

pub async fn list(State(apps): State<Apps>) -> Html<String> {
    render(AppsPage {
        apps: apps
            .templates()
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

pub(super) const FALLBACK_ICON: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" role="img"><rect width="64" height="64" rx="14" fill="#8A8F98"/><path d="M32 14 49 23v18L32 50 15 41V23z" fill="none" stroke="#fff" stroke-width="4" stroke-linejoin="round"/></svg>"##;

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

pub async fn install_submit(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(values): Form<HashMap<String, String>>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let t = apps.template(&id).ok_or(StatusCode::NOT_FOUND)?;

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

pub(super) fn job_fragment(apps: &Apps, id: &str) -> Option<Html<String>> {
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
