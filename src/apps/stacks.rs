//! Pasted and adopted Compose stacks.

use super::*;

#[derive(Template)]
#[template(path = "adopt.html")]
pub(super) struct AdoptPage {
    pub(super) items: Vec<crate::adopt::Candidate>,
    pub(super) error: Option<String>,
    pub(super) note: Option<String>,
}

/// `GET /stacks/adopt`: read-only scan of Compose projects the panel does not manage.
pub async fn adopt_scan(State(state): State<crate::AppState>) -> Html<String> {
    let data_dir = state
        .apps
        .root
        .parent()
        .map(|d| d.to_path_buf())
        .unwrap_or_default();
    render(adopt_page(&state, &data_dir).await)
}

/// `POST /stacks/adopt`: register one scanned project as an instance (re-checked from scratch).
pub async fn adopt_submit(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Form(values): Form<HashMap<String, String>>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let apps = &state.apps;
    let get = |k: &str| values.get(k).cloned().unwrap_or_default();
    let (project, name) = (get("project"), get("name").trim().to_string());
    let t = apps
        .template(CUSTOM_ID)
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let data_dir = apps
        .root
        .parent()
        .map(|d| d.to_path_buf())
        .unwrap_or_default();
    let result = {
        let _guard = apps.lock.lock().await;
        match state.docker.foreign_compose_containers().await {
            Ok(list) => crate::adopt::adopt(&data_dir, &apps.root, t, list, &project, &name).await,
            Err(e) => Err(e.to_string()),
        }
    };
    if result.is_ok() {
        crate::audit::record("stack.adopt", &name);
    }
    let error = result.err();
    let note = if error.is_none() {
        format!("Adopted {project} as {name}. Nothing was restarted.")
    } else {
        String::new()
    };
    let mut page = adopt_page(&state, &data_dir).await;
    page.error = error;
    page.note = (!note.is_empty()).then_some(note);
    Ok(render(page))
}

pub(super) async fn adopt_page(state: &crate::AppState, data_dir: &std::path::Path) -> AdoptPage {
    match state.docker.foreign_compose_containers().await {
        Ok(list) => AdoptPage {
            items: crate::adopt::scan(data_dir, list).await,
            error: None,
            note: None,
        },
        Err(e) => AdoptPage {
            items: Vec::new(),
            error: Some(e.to_string()),
            note: None,
        },
    }
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

#[derive(Template)]
#[template(path = "stack_edit.html")]
pub(super) struct StackEdit {
    pub(super) name: String,
    pub(super) compose: String,
    pub(super) env: String,
    pub(super) error: Option<String>,
    pub(super) adopted: bool,
    /// Earlier versions kept by earlier edits: (id, "2026-11-13 15:11 UTC"), newest first.
    pub(super) versions: Vec<(u64, String)>,
    /// When an earlier version is loaded: its date and the changes from the live file to it.
    pub(super) loaded: Option<String>,
    pub(super) changes: Vec<(char, String)>,
}

/// Variables Dockwren sets itself; they are not shown in the edit form and are rebuilt on save.
pub(super) const STACK_OWN_KEYS: &[&str] = &["INSTANCE_NAME", "WEB_PORT"];

/// Directory of a stack the user pasted or adopted (not a catalog app), and whether it is
/// adopted (its containers run under their own Compose project name).
pub(super) fn stack_dir(apps: &Apps, name: &str) -> Result<(std::path::PathBuf, bool), StatusCode> {
    known_instance(apps, name)?;
    let dir = apps.root.join(name);
    let state: InstanceState = std::fs::read_to_string(dir.join("state.toml"))
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    if state.template != CUSTOM_ID {
        return Err(StatusCode::NOT_FOUND);
    }
    let adopted = dir.join(runner::PROJECT_FILE).exists();
    Ok((dir, adopted))
}

pub(super) fn stack_edit_view(
    name: &str,
    dir: &std::path::Path,
    adopted: bool,
    entered: Option<(String, String)>,
    error: Option<String>,
    revision: Option<u64>,
) -> Result<StackEdit, StatusCode> {
    let live = || -> Result<(String, String), StatusCode> {
        let compose = std::fs::read_to_string(dir.join("compose.yaml"))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let env = if adopted {
            // An adopted project's own `.env`, exactly as it is (comments, quoting).
            std::fs::read_to_string(dir.join(".env")).unwrap_or_default()
        } else {
            runner::read_env(dir)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                .iter()
                .filter(|(k, _)| !STACK_OWN_KEYS.contains(&k.as_str()))
                .map(|(k, v)| format!("{k}={v}\n"))
                .collect()
        };
        Ok((compose, env))
    };
    let mut loaded = None;
    let mut changes = Vec::new();
    let (compose, env) = match (entered, revision) {
        (Some(e), _) => e,
        (None, Some(id)) => {
            let rev = crate::stackhistory::load(dir, id).ok_or(StatusCode::NOT_FOUND)?;
            let env = if adopted {
                rev.env.clone()
            } else {
                // Dockwren's own keys are rebuilt on save, as in the live form.
                crate::stack::parse_env(&rev.env)
                    .unwrap_or_default()
                    .iter()
                    .filter(|(k, _)| !STACK_OWN_KEYS.contains(&k.as_str()))
                    .map(|(k, v)| format!("{k}={v}\n"))
                    .collect()
            };
            let (cur_compose, cur_env) = live()?;
            changes = crate::stackhistory::diff(&cur_compose, &rev.compose);
            let env_changes = crate::stackhistory::diff(&cur_env, &env);
            if env_changes.iter().any(|(k, _)| *k != ' ') {
                changes.push(('@', "environment".into()));
                changes.extend(env_changes);
            }
            loaded = Some(utc_stamp(rev.id));
            (rev.compose, env)
        }
        (None, None) => live()?,
    };
    let versions = crate::stackhistory::list_ids(dir)
        .into_iter()
        .map(|id| (id, utc_stamp(id)))
        .collect();
    Ok(StackEdit {
        name: name.to_string(),
        compose,
        env,
        error,
        adopted,
        versions,
        loaded,
        changes,
    })
}

/// `GET /instances/{name}/stack`: the pasted Compose file and environment, ready to edit.
pub async fn stack_edit_form(
    State(apps): State<Apps>,
    Path(name): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Html<String>, StatusCode> {
    let (dir, adopted) = stack_dir(&apps, &name)?;
    let revision = match q.get("rev") {
        Some(r) => Some(r.parse::<u64>().map_err(|_| StatusCode::BAD_REQUEST)?),
        None => None,
    };
    Ok(render(stack_edit_view(
        &name, &dir, adopted, None, None, revision,
    )?))
}

/// `POST /instances/{name}/stack`: check the new file like a fresh paste, back the stack up,
/// then recreate it. When it does not come back healthy the old files are put back.
pub async fn stack_edit_submit(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(values): Form<HashMap<String, String>>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let (dir, adopted) = stack_dir(&apps, &name)?;
    let get = |k: &str| values.get(k).cloned().unwrap_or_default();
    let (compose, env_text) = (get("compose"), get("env"));
    let again = |error: String| {
        Ok(render(stack_edit_view(
            &name,
            &dir,
            adopted,
            Some((compose.clone(), env_text.clone())),
            Some(error),
            None,
        )?))
    };
    let t = apps
        .template(CUSTOM_ID)
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let data_dir = apps
        .root
        .parent()
        .map(|d| d.to_path_buf())
        .unwrap_or_default();
    let old_env_text =
        std::fs::read_to_string(dir.join(".env")).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let old_compose = std::fs::read_to_string(dir.join("compose.yaml"))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let (new_env_text, port, old_port, env_same);
    if adopted {
        // Kept verbatim like at adoption; the adoption rule (host folders must be plain) on top.
        let mut text = env_text.replace("\r\n", "\n");
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        let model = match crate::stack::check_raw_opts(&data_dir, &compose, &text, true).await {
            Ok(m) => m,
            Err(e) => return again(e),
        };
        if let Err(e) = crate::adopt::lint_adoption(&model) {
            return again(e);
        }
        port = crate::stack::first_published_port(&model);
        if let Some(p) = port
            && !text
                .lines()
                .any(|l| l.trim_start().starts_with("WEB_PORT="))
        {
            text.push_str(&format!("WEB_PORT={p}\n"));
        }
        old_port = match crate::stack::check_raw_opts(&data_dir, &old_compose, &old_env_text, true)
            .await
        {
            Ok(m) => crate::stack::first_published_port(&m),
            Err(_) => None,
        };
        env_same = text == old_env_text;
        new_env_text = text;
    } else {
        let mut env = match crate::stack::parse_env(&env_text) {
            Ok(e) => e,
            Err(e) => return again(e),
        };
        if let Some(k) = STACK_OWN_KEYS.iter().find(|k| env.contains_key(**k)) {
            return again(format!("environment: {k} is set by Dockwren"));
        }
        env.insert("INSTANCE_NAME".into(), name.clone());
        let model = match crate::stack::check(&data_dir, &compose, &env).await {
            Ok(m) => m,
            Err(e) => return again(e),
        };
        let old_env = runner::read_env(&dir).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        old_port = old_env.get("WEB_PORT").and_then(|p| p.parse::<u16>().ok());
        port = crate::stack::first_published_port(&model);
        if let Some(p) = port {
            env.insert("WEB_PORT".into(), p.to_string());
        }
        env_same = old_env == env;
        new_env_text = install::render_env(&env);
    }
    if old_compose == compose && env_same {
        return again("Nothing changed.".into());
    }
    // A port the stack already holds is fine; a new one must be free and unclaimed.
    if let Some(p) = port.filter(|p| Some(*p) != old_port) {
        let _guard = apps.lock.lock().await;
        if runner::claimed_ports(&apps.root).contains(&p) || !install::port_is_free(p) {
            return again(format!(
                "port {p} is already in use on this server; pick another"
            ));
        }
    }
    let job_id = new_job(&apps, &name, "Updating stack")?;
    crate::audit::record("stack.edit", &name);
    let keep = apps.schedule.get().keep;
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
            log("backing up first");
            crate::backup::snapshot_keep(&apps2.backups_root(), &inst, t, &mut |s| log(s), keep)
                .await
                .map_err(|e| format!("backup failed, stack left unchanged: {e}"))?;
            log("writing the new file");
            runner::replace_private(&inst.dir.join("compose.yaml"), &compose)
                .map_err(|e| format!("compose.yaml: {e}"))?;
            crate::install::replace_env_text(&inst.dir, &new_env_text)
                .map_err(|e| format!(".env: {e}"))?;
            let up = async {
                log("checking compose file");
                runner::compose(&inst, &["config", "-q"]).await?;
                log("pulling images");
                runner::compose(&inst, &["pull"]).await?;
                log("recreating containers");
                runner::compose(&inst, &["up", "-d", "--remove-orphans"]).await?;
                log("waiting for healthy");
                runner::wait_stable(&inst, std::time::Duration::from_secs(240)).await
            }
            .await;
            if let Err(e) = up {
                log("failed: putting the old file back");
                runner::replace_private(&inst.dir.join("compose.yaml"), &old_compose)
                    .map_err(|e| format!("compose.yaml: {e}"))?;
                crate::install::replace_env_text(&inst.dir, &old_env_text)
                    .map_err(|e| format!(".env: {e}"))?;
                let back = runner::compose(&inst, &["up", "-d", "--remove-orphans"]).await;
                return Err(match back {
                    Ok(_) => format!("{e} (old file restored)"),
                    Err(b) => format!("{e} (restoring the old file also failed: {b})"),
                });
            }
            // Keep what was replaced, now that the new file runs (best effort).
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            if let Err(e) = crate::stackhistory::save(&inst.dir, now, &old_compose, &old_env_text) {
                log(&format!("could not keep the old version: {e}"));
            }
            Ok::<(), String>(())
        }
        .await;
        apps2.note_job(
            "Updating stack",
            &name,
            result.as_ref().err().map(|e| e.as_str()),
        );
        apps2.with_job(&id, |j| match result {
            Ok(()) => {
                j.log.push("stack updated".into());
                j.state = JobState::Done;
            }
            Err(e) => j.state = JobState::Failed(e),
        });
    });
    job_fragment(&apps, &job_id).ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}
