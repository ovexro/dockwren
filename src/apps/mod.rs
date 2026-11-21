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

mod disk;
mod instance_settings;
mod instances;
mod jobs;
mod new_app;
mod operations;
mod panel_settings;
mod public_ui;
mod restore_ui;
mod stacks;
mod views;

pub use disk::*;
pub use instance_settings::*;
pub use instances::*;
pub use new_app::*;
pub use operations::*;
pub use panel_settings::*;
pub use public_ui::*;
pub use restore_ui::*;
pub use stacks::*;
use views::*;

/// How many finished jobs are kept in memory.
const KEEP_JOBS: usize = 20;

#[derive(Clone)]
pub struct Apps {
    /// Swapped as a whole when third-party templates change. Each generation is leaked on
    /// purpose (a few KB per add/remove) so callers can keep `&'static` references.
    templates: Arc<std::sync::RwLock<&'static [AppTemplate]>>,
    pub root: PathBuf,
    jobs: Arc<Mutex<Vec<(String, Job)>>>,
    /// Serialises "pick ports + create instance dir" so parallel installs cannot clash.
    lock: Arc<tokio::sync::Mutex<()>>,
    notify: Option<crate::notify::Shared>,
    auth: Option<crate::auth::SharedAuth>,
    schedule: Arc<crate::schedule::Schedule>,
    offsite: Arc<crate::offsite::Offsite>,
    upstream: crate::upstream::Cache,
}

impl Apps {
    pub fn new(templates: Vec<AppTemplate>, root: PathBuf) -> Self {
        crate::terminal::set_adopted(runner::adopted_projects(&root));
        Self {
            templates: Arc::new(std::sync::RwLock::new(Box::leak(
                templates.into_boxed_slice(),
            ))),
            root,
            jobs: Arc::default(),
            lock: Arc::default(),
            notify: None,
            auth: None,
            schedule: Arc::new(crate::schedule::Schedule::memory()),
            offsite: Arc::new(crate::offsite::Offsite::memory()),
            upstream: Arc::default(),
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

    /// Needed for actions that ask for the password again (show/rotate app passwords).
    pub fn with_auth(mut self, auth: crate::auth::SharedAuth) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Let jobs and the update check raise notifications.
    pub fn with_notify(mut self, notify: crate::notify::Shared) -> Self {
        self.notify = Some(notify);
        self
    }

    pub fn instance_names(&self) -> Vec<String> {
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

    /// The current catalog.
    pub fn templates(&self) -> &'static [AppTemplate] {
        *self.templates.read().unwrap()
    }

    /// Rebuild the catalog from the bundled templates plus the stored third-party ones, so an
    /// added or removed template shows up without restarting the panel.
    pub fn reload_templates(&self) {
        let data = self
            .root
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.root.clone());
        let over = std::env::var("DOCKWREN_APPS").ok().map(PathBuf::from);
        if let Ok(all) = catalog_in(&data, over.as_deref()) {
            *self.templates.write().unwrap() = Box::leak(all.into_boxed_slice());
        }
    }

    fn template(&self, id: &str) -> Option<&'static AppTemplate> {
        self.templates().iter().find(|t| t.manifest.app.id == id)
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
    /// Template note, shown after a successful install.
    note: String,
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
            note: String::new(),
        },
    ));
    Ok(job_id)
}

/// "2026-11-13 15:11 UTC" from epoch seconds (no time crate needed).
pub(crate) fn utc_stamp(epoch: u64) -> String {
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

#[cfg(test)]
mod tests {

    #[test]
    fn needs_form_and_list_render() {
        let html = render(NeedsForm {
            name: "web".into(),
            choices: vec![("db".into(), true), ("cache".into(), false)],
            error: Some("loop".into()),
        })
        .0;
        assert!(html.contains("What does web need?"));
        assert!(html.contains("value=\"db\" checked"));
        assert!(html.contains("value=\"cache\">"));
        assert!(html.contains("loop"));
        let view = |name: &str, needs: Vec<(String, bool)>, stopped: bool| InstanceView {
            name: name.into(),
            app: "custom".into(),
            ports: String::new(),
            status: if stopped { "stopped" } else { "healthy" }.into(),
            update: None,
            update_skipped: false,
            upstream: None,
            snapshots: 0,
            last_backup: None,
            last_backup_at: None,
            hostname: None,
            public_up: None,
            uptime: None,
            vulns: None,
            upstream_age: None,
            public_age: None,
            vulns_age: None,
            editable: false,
            stack_editable: false,
            deletable: stopped,
            limitable: false,
            has_passwords: false,
            needs,
            stopped,
            drift: Vec::new(),
            power: None,
            stop_confirm: format!("Stop {name}? api needs it and will stop working."),
            delete_confirm: String::new(),
            uncapped: false,
        };
        let page = render(InstancesPage {
            instances: vec![
                view("db", vec![], true),
                view(
                    "api",
                    vec![("db".into(), false), ("cache".into(), true)],
                    false,
                ),
            ],
            picked: Vec::new(),
            error: None,
            note: None,
        })
        .0;
        assert!(page.contains("needs "));
        assert!(page.contains("db (not running)"));
        assert!(!page.contains("cache (not running)"));
        assert!(page.contains("/instances/db/start"));
        assert!(!page.contains("/instances/api/start"));
        assert!(page.contains("api needs it and will stop working."));
    }

    #[test]
    fn cached_results_show_their_age() {
        let mk = |fresh: bool| {
            let mut v = InstanceView {
                name: "web".into(),
                app: "custom".into(),
                ports: String::new(),
                status: "healthy".into(),
                update: None,
                update_skipped: false,
                upstream: Some("Upstream has 2.0".into()),
                snapshots: 0,
                last_backup: None,
                last_backup_at: None,
                hostname: Some("x.example".into()),
                public_up: Some(true),
                uptime: None,
                vulns: Some("1 critical".into()),
                upstream_age: None,
                public_age: None,
                vulns_age: None,
                editable: false,
                stack_editable: false,
                deletable: false,
                limitable: false,
                has_passwords: false,
                needs: vec![],
                stopped: false,
                drift: Vec::new(),
                power: None,
                stop_confirm: String::new(),
                delete_confirm: String::new(),
                uncapped: false,
            };
            let (a, stale) =
                crate::metrics::age_note(0, if fresh { 60 } else { 5 * 86400 }, 3 * 86400);
            v.upstream_age = Some((a.clone(), stale));
            v.public_age = Some((a.clone(), stale));
            v.vulns_age = Some((a, stale));
            render(InstancesPage {
                instances: vec![v],
                picked: Vec::new(),
                error: None,
                note: None,
            })
            .0
        };
        let fresh = mk(true);
        assert!(fresh.contains("Checked 1m ago."), "{fresh}");
        assert!(!fresh.contains("<em>(checked"));
        let old = mk(false);
        assert!(old.contains("<em>(checked 5d 0h ago)</em>"));
        assert!(old.contains("<em>(scanned 5d 0h ago)</em>"));
    }

    #[test]
    fn stack_edit_form_lists_and_loads_earlier_versions() {
        let dir = std::env::temp_dir().join(format!("dw-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("compose.yaml"),
            "services:\n  a:\n    image: new\n",
        )
        .unwrap();
        std::fs::write(dir.join(".env"), "INSTANCE_NAME=x\nWEB_PORT=8080\nFOO=2\n").unwrap();
        crate::stackhistory::save(
            &dir,
            1_700_000_000,
            "services:\n  a:\n    image: old\n",
            "INSTANCE_NAME=x\nWEB_PORT=8080\nFOO=1\n",
        )
        .unwrap();
        let plain = render(stack_edit_view("x", &dir, false, None, None, None).unwrap()).0;
        assert!(plain.contains("Earlier versions (1)"));
        assert!(plain.contains("?rev=1700000000"));
        assert!(!plain.contains("loaded."));
        let loaded =
            render(stack_edit_view("x", &dir, false, None, None, Some(1_700_000_000)).unwrap()).0;
        assert!(loaded.contains("loaded."));
        assert!(loaded.contains("- ") && loaded.contains("image: old"));
        assert!(loaded.contains("FOO=1") && !loaded.contains("WEB_PORT=8080\nFOO"));
        assert!(stack_edit_view("x", &dir, false, None, None, Some(5)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
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
    fn a_skipped_version_is_not_pending_until_a_newer_one_exists() {
        let (apps, n, root) = notifying_apps("skip");
        let bundled = apps.template("gitea").unwrap().manifest.app.version;
        instance(&root, "old", "gitea", bundled - 1);
        assert_eq!(apps.pending_updates().len(), 1);
        std::fs::write(root.join("old").join("skip-update"), bundled.to_string()).unwrap();
        assert!(apps.pending_updates().is_empty());
        apps.sync_update_notifications();
        assert!(n.lock().unwrap().open_events().is_empty());
        // A skip for an older version does not hide the current one.
        std::fs::write(
            root.join("old").join("skip-update"),
            (bundled - 1).to_string(),
        )
        .unwrap();
        assert_eq!(apps.pending_updates().len(), 1);
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

#[cfg(test)]
mod power_docker_tests {
    use super::*;

    const D0: u64 = 20_776 * 86_400;

    fn stack(root: &std::path::Path, name: &str, needs: &[&str]) -> runner::Instance {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("state.toml"), "template = \"x\"\nversion = 1\n").unwrap();
        std::fs::write(dir.join(".env"), "").unwrap();
        std::fs::write(
            dir.join("compose.yaml"),
            "services:\n  s:\n    image: alpine:latest\n    command: sleep 600\n",
        )
        .unwrap();
        let needs: Vec<String> = needs.iter().map(|s| s.to_string()).collect();
        crate::needs::write(&dir, &needs).unwrap();
        runner::Instance {
            dir,
            name: name.to_string(),
        }
    }

    /// Needs a running Docker and the alpine image: two throwaway stacks, `web` needs `db`.
    #[tokio::test]
    #[ignore = "needs docker"]
    async fn schedule_stops_dependents_first_and_starts_needs_first() {
        let root = std::env::temp_dir().join(format!("dockwren-power-dk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let db = stack(&root, "dwp-db", &[]);
        let web = stack(&root, "dwp-web", &["dwp-db"]);
        let apps = Apps::new(Vec::new(), root.clone());
        let result = async {
            runner::bring_up(&db, &mut |_| {}).await?;
            runner::bring_up(&web, &mut |_| {}).await?;
            let plan = crate::power::Plan::from_form("03:00", "04:00").unwrap();
            for i in [&db, &web] {
                crate::power::write(&i.dir, &plan, D0).unwrap();
            }
            // Only the db is due to stop while web (no schedule) runs: it is left alone.
            let only_db = crate::power::Plan::from_form("02:00", "").unwrap();
            crate::power::write(&web.dir, &crate::power::Plan::default(), D0).unwrap();
            crate::power::write(&db.dir, &only_db, D0 - 86_400).unwrap();
            apps.run_power_schedule(D0 + 2 * 3600 + 600).await;
            assert!(!apps.app_stopped("dwp-db").await?, "db stopped under web");
            // Both due: web goes first, then db.
            crate::power::write(&web.dir, &plan, D0).unwrap();
            crate::power::write(&db.dir, &plan, D0).unwrap();
            apps.run_power_schedule(D0 + 3 * 3600 + 1800).await;
            assert!(apps.app_stopped("dwp-web").await?);
            assert!(apps.app_stopped("dwp-db").await?);
            assert_eq!(crate::power::state(&web.dir), (D0 + 3 * 3600, true));
            // Start time: both come back (the db first, as web needs it).
            apps.run_power_schedule(D0 + 4 * 3600 + 1800).await;
            assert!(!apps.app_stopped("dwp-db").await?);
            assert!(!apps.app_stopped("dwp-web").await?);
            assert_eq!(crate::power::state(&web.dir), (D0 + 4 * 3600, false));
            // A manual stop is not undone: no new time has passed.
            runner::take_down(&web).await?;
            apps.run_power_schedule(D0 + 4 * 3600 + 2400).await;
            assert!(apps.app_stopped("dwp-web").await?);
            Ok::<(), String>(())
        }
        .await;
        let _ = runner::take_down(&web).await;
        let _ = runner::take_down(&db).await;
        let _ = std::fs::remove_dir_all(&root);
        result.unwrap();
    }
}
