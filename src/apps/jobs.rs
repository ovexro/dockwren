//! Background jobs: spawn, schedulers, power schedule and automatic updates.

use super::*;

impl Apps {
    /// Raise (on failure) or clear (on success) the "job failed" notification for a job.
    pub(super) fn note_job(&self, verb: &str, instance: &str, error: Option<&str>) {
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
    pub(super) fn pending_updates(&self) -> Vec<(String, u32, u32)> {
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
    pub(super) fn spawn_update(&self, name: &str) -> Result<String, StatusCode> {
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
                task_apps.templates(),
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

    /// One job that backs up or updates several apps one after the other (never in parallel: a
    /// small host would choke). Each app runs as its own ordinary job, so failures raise the usual
    /// events and rollbacks; this job only reports who was done, failed or skipped.
    pub(super) fn spawn_bulk(
        &self,
        update: bool,
        names: Vec<String>,
    ) -> Result<String, StatusCode> {
        let mut seen = std::collections::HashSet::new();
        let names: Vec<String> = names
            .into_iter()
            .filter(|n| {
                runner::valid_name(n)
                    && self.root.join(n).join("state.toml").is_file()
                    && seen.insert(n.clone())
            })
            .collect();
        if names.is_empty() {
            return Err(StatusCode::NOT_FOUND);
        }
        let verb = if update { "Updating" } else { "Backing up" };
        let job_id = new_job(self, "selected apps", verb)?;
        let apps = self.clone();
        let id = job_id.clone();
        tokio::spawn(async move {
            let pending: std::collections::HashSet<String> = apps
                .pending_updates()
                .into_iter()
                .map(|(n, _, _)| n)
                .collect();
            let keep = apps.schedule.get().keep;
            let (mut ok, mut bad) = (0usize, 0usize);
            for name in &names {
                let line = |apps: &Apps, t: String| apps.with_job(&id, |j| j.log.push(t));
                if update && !pending.contains(name) {
                    line(
                        &apps,
                        format!("{name}: no update to apply (or skipped), left alone"),
                    );
                    continue;
                }
                if update && !apps.is_healthy(name).await {
                    line(&apps, format!("{name}: not healthy, left alone"));
                    continue;
                }
                line(&apps, format!("{name}: started"));
                let sub = if update {
                    apps.spawn_update(name)
                } else {
                    apps.spawn_backup(name, keep)
                };
                let Ok(sub) = sub else {
                    bad += 1;
                    line(
                        &apps,
                        format!("{name}: could not start (another job is running for it?)"),
                    );
                    continue;
                };
                while apps.job_running(&sub) {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                match apps.with_job(&sub, |j| j.state.clone()) {
                    Some(JobState::Done) => {
                        ok += 1;
                        line(&apps, format!("{name}: done"));
                    }
                    Some(JobState::Failed(e)) => {
                        bad += 1;
                        let first = e
                            .lines()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(200)
                            .collect::<String>();
                        line(&apps, format!("{name}: FAILED: {first}"));
                    }
                    _ => {
                        bad += 1;
                        line(&apps, format!("{name}: result unknown"));
                    }
                }
            }
            apps.with_job(&id, |j| {
                j.log.push(format!("{ok} done, {bad} failed"));
                j.state = if bad == 0 {
                    JobState::Done
                } else {
                    JobState::Failed(format!("{bad} of {} did not finish", names.len()))
                };
            });
        });
        Ok(job_id)
    }

    /// Start a backup job for `name`, keeping `keep` snapshots. CONFLICT when a job for it runs.
    pub(super) fn spawn_backup(&self, name: &str, keep: usize) -> Result<String, StatusCode> {
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
    pub(super) fn spawn_verify(
        &self,
        name: &str,
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
    pub(super) fn spawn_restore_test(&self, name: &str) -> Result<String, StatusCode> {
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
    pub(super) fn spawn_clone(
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

    pub(super) fn job_done(&self, id: &str) -> bool {
        self.with_job(id, |j| j.state == JobState::Done)
            .unwrap_or(false)
    }

    pub(super) fn job_running(&self, id: &str) -> bool {
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

    /// True when the app has no containers at all (stopped with Remove or by a schedule).
    pub(super) async fn app_stopped(&self, name: &str) -> Result<bool, String> {
        let inst = runner::Instance {
            dir: self.root.join(name),
            name: name.to_string(),
        };
        let ps = runner::compose(&inst, &["ps", "-a", "--format", "json"]).await?;
        Ok(matches!(runner::judge(&ps),
            runner::Status::Waiting(w) if w == "no containers yet"))
    }

    /// Start `name` after everything it needs, each only if it is stopped.
    pub(super) async fn start_with_needs<L: Fn(&str) + Send + Sync>(
        &self,
        name: &str,
        log: L,
    ) -> Result<(), String> {
        let map = crate::needs::all(&self.root);
        for n in crate::needs::start_order(&map, name) {
            let inst = runner::Instance {
                dir: self.root.join(&n),
                name: n.clone(),
            };
            if !self
                .app_stopped(&n)
                .await
                .map_err(|e| format!("{n}: {e}"))?
            {
                log(&format!("{n} is already running"));
                continue;
            }
            log(&format!("starting {n}"));
            runner::bring_up(&inst, &mut |s| log(&format!("{n}: {s}")))
                .await
                .map_err(|e| format!("{n}: {e}"))?;
        }
        Ok(())
    }

    /// Run `work` as a job on `name` (so it cannot overlap an update or a backup) and wait for it.
    /// `None` when the app is busy: the caller tries again next minute.
    pub(super) async fn scheduled_job<F>(
        &self,
        name: &str,
        verb: &'static str,
        work: F,
    ) -> Option<Result<(), String>>
    where
        F: std::future::Future<Output = Result<(), String>>,
    {
        let id = new_job(self, name, verb).ok()?;
        let result = work.await;
        self.note_job(verb, name, result.as_ref().err().map(|e| e.as_str()));
        self.with_job(&id, |j| match &result {
            Ok(()) => j.state = JobState::Done,
            Err(e) => j.state = JobState::Failed(e.clone()),
        });
        Some(result)
    }

    /// One pass of the per-app stop/start schedule (see `power.rs`).
    pub async fn run_power_schedule(&self, now: u64) {
        use crate::power::Event;
        let mut stops: Vec<(String, u64)> = Vec::new();
        let mut starts: Vec<(String, u64)> = Vec::new();
        for name in self.instance_names() {
            let dir = self.root.join(&name);
            let plan = crate::power::read(&dir);
            if let Some((ev, at)) = plan.due(now, crate::power::state(&dir).0) {
                match ev {
                    Event::Stop => stops.push((name, at)),
                    Event::Start => starts.push((name, at)),
                }
            }
        }
        let map = crate::needs::all(&self.root);
        // Apps that need others stop first, so nothing is stopped under a running dependent.
        while !stops.is_empty() {
            let pending: Vec<String> = stops.iter().map(|(n, _)| n.clone()).collect();
            let pos = stops
                .iter()
                .position(|(n, _)| {
                    !crate::needs::dependents(&map, n)
                        .iter()
                        .any(|d| pending.contains(d))
                })
                .unwrap_or(0);
            let (name, at) = stops.remove(pos);
            let dir = self.root.join(&name);
            let stopped = self.app_stopped(&name).await.unwrap_or(true);
            let mut blocked = None;
            if !stopped {
                for d in crate::needs::dependents(&map, &name) {
                    if !pending.contains(&d) && !self.app_stopped(&d).await.unwrap_or(true) {
                        blocked = Some(d);
                        break;
                    }
                }
            }
            let (_, was) = crate::power::state(&dir);
            if stopped {
                let _ = crate::power::set_state(&dir, at, was);
            } else if let Some(d) = blocked {
                crate::audit::record("power.skip", &format!("{name}: {d} still needs it"));
                let _ = crate::power::set_state(&dir, at, was);
            } else {
                let inst = runner::Instance {
                    dir: dir.clone(),
                    name: name.clone(),
                };
                match self
                    .scheduled_job(&name, "Stopping", async { runner::take_down(&inst).await })
                    .await
                {
                    None => {}
                    Some(r) => {
                        crate::audit::record("power.stop", &name);
                        let _ = crate::power::set_state(&dir, at, r.is_ok() || was);
                    }
                }
            }
        }
        for (name, at) in starts {
            let dir = self.root.join(&name);
            let (_, was) = crate::power::state(&dir);
            if !was {
                let _ = crate::power::set_state(&dir, at, false);
                continue;
            }
            match self
                .scheduled_job(&name, "Starting", async {
                    self.start_with_needs(&name, |_| {}).await
                })
                .await
            {
                None => {}
                Some(r) => {
                    crate::audit::record("power.start", &name);
                    let _ = crate::power::set_state(&dir, at, r.is_err());
                }
            }
        }
    }

    /// Every minute: stop and start the apps that have a schedule.
    pub fn spawn_power_scheduler(&self) {
        let apps = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                apps.run_power_schedule(crate::schedule::now()).await;
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
    pub(super) async fn run_restore_tests(&self) {
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
    pub(super) async fn run_auto_updates(
        &self,
        tried: &mut std::collections::HashSet<(String, u32)>,
    ) {
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
            // Failures (and rollbacks) already raise `job.failed`; tell the admin about successes.
            let done = self
                .with_job(&id, |j| matches!(j.state, JobState::Done))
                .unwrap_or(false);
            if done && let Some(n) = &self.notify {
                crate::notify::raise(
                    n,
                    crate::notify::NewEvent::new(
                        "apps",
                        "app.auto_updated",
                        crate::notify::Severity::Info,
                        &format!("{name} was updated automatically"),
                    )
                    .body(
                        "A snapshot was taken first and the app came back healthy. Nothing to do.",
                    )
                    .link("/apps")
                    .key(format!("app.auto_updated:{name}:{to}")),
                );
            }
        }
    }

    pub(super) async fn is_healthy(&self, name: &str) -> bool {
        let inst = runner::Instance {
            name: name.to_string(),
            dir: self.root.join(name),
        };
        match runner::compose(&inst, &["ps", "-a", "--format", "json"]).await {
            Ok(ps) => runner::judge(&ps) == runner::Status::Healthy,
            Err(_) => false,
        }
    }

    /// `(from, to)` UTC hours of the auto-update window when automatic updates are on.
    pub fn auto_update_window(&self) -> Option<(u8, u8)> {
        let s = self.schedule.get();
        s.auto_update.then_some((s.update_from, s.update_to))
    }
}
