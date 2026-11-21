//! Disk usage and Docker cleanup.

use super::*;

pub(super) struct DiskRow {
    pub(super) name: String,
    pub(super) data: String,
    pub(super) partial: bool,
    pub(super) leftovers: String,
    pub(super) snapshots: usize,
    pub(super) snapshot_bytes: String,
    pub(super) images: String,
    pub(super) total: String,
    pub(super) can_trim: bool,
    pub(super) trimmable: String,
}

#[derive(Template)]
#[template(path = "disk.html")]
pub(super) struct DiskPage {
    pub(super) rows: Vec<DiskRow>,
    pub(super) keep: usize,
    pub(super) images: Option<(String, String)>,
    pub(super) orphans: crate::orphans::Orphans,
    pub(super) orphan_size: String,
    pub(super) note: Option<String>,
    pub(super) error: Option<String>,
}

/// Everything the installed apps own (adopted ones keep their own project names).
pub(super) async fn protected_projects(apps: &Apps) -> crate::orphans::Protected {
    let mut p = crate::orphans::Protected {
        complete: true,
        ..Default::default()
    };
    for name in apps.instance_names() {
        let inst = runner::Instance {
            dir: apps.root.join(&name),
            name,
        };
        p.projects.insert(runner::project_name(&inst));
        let ok = match runner::compose(&inst, &["config", "--format", "json"]).await {
            Ok(j) => p.add_config(&j),
            Err(_) => false,
        };
        p.complete &= ok;
    }
    p
}

pub(super) async fn disk_html(
    apps: &Apps,
    note: Option<String>,
    error: Option<String>,
) -> Html<String> {
    use crate::metrics::fmt_bytes as fb;
    let data = apps
        .backups_root()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let mut rows = Vec::new();
    for name in apps.instance_names() {
        let inst = runner::Instance {
            dir: apps.root.join(&name),
            name,
        };
        let (i2, b) = (inst.clone(), apps.backups_root());
        let Ok(mut u) =
            tokio::task::spawn_blocking(move || crate::diskusage::measure(&i2, &b)).await
        else {
            continue;
        };
        u.images = crate::diskusage::image_bytes(&data, &inst).await;
        rows.push(DiskRow {
            total: fb(u.total() as f64),
            data: fb(u.data as f64),
            partial: u.partial,
            leftovers: fb(u.leftovers as f64),
            snapshots: u.snapshots,
            snapshot_bytes: fb(u.snapshot_bytes as f64),
            images: fb(u.images as f64),
            can_trim: u.trimmable > 0,
            trimmable: fb(u.trimmable as f64),
            name: u.name,
        });
    }
    let orphans = crate::orphans::scan(&data, &protected_projects(apps).await).await;
    render(DiskPage {
        rows,
        keep: crate::diskusage::KEEP_AFTER_TRIM,
        images: crate::diskusage::docker_images_line(&data).await,
        orphan_size: fb(orphans.volume_bytes() as f64),
        orphans,
        note,
        error,
    })
}

/// `GET /settings/disk`: sizes per app.
pub async fn disk_show(State(apps): State<Apps>) -> Html<String> {
    disk_html(&apps, None, None).await
}

/// `POST /instances/{name}/trim-snapshots`: delete the oldest local snapshots, keep the newest few.
pub async fn trim_snapshots(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    known_instance(&apps, &name)?;
    let busy = apps
        .jobs
        .lock()
        .unwrap()
        .iter()
        .any(|(_, j)| j.instance == name && j.state == JobState::Running);
    if busy {
        return Err(StatusCode::CONFLICT);
    }
    crate::audit::record("app.trim-snapshots", &name);
    let (note, error) = match crate::backup::prune(
        &apps.backups_root(),
        &name,
        crate::diskusage::KEEP_AFTER_TRIM,
    ) {
        Ok(n) => (Some(format!("{name}: deleted {n} old snapshot(s).")), None),
        Err(e) => (None, Some(e)),
    };
    Ok(disk_html(&apps, note, error).await)
}

/// `POST /settings/disk/prune-images`: remove Docker images no container uses.
pub async fn prune_images(
    State(apps): State<Apps>,
    headers: HeaderMap,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let data = apps
        .backups_root()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    crate::audit::record("docker.prune-images", "unused images");
    let (note, error) = match crate::diskusage::prune_images(&data).await {
        Ok(s) => (Some(s), None),
        Err(e) => (None, Some(e)),
    };
    Ok(disk_html(&apps, note, error).await)
}

#[derive(Deserialize)]
pub struct OrphanForm {
    /// `volumes` or `networks`.
    what: String,
}

/// `POST /settings/disk/remove-orphans`: remove unused Docker volumes or networks. Apps' own
/// volumes and networks are never touched (see `orphans.rs`).
pub async fn remove_orphans(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Form(f): Form<OrphanForm>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let volumes = match f.what.as_str() {
        "volumes" => true,
        "networks" => false,
        _ => return Err(StatusCode::BAD_REQUEST),
    };
    let data = apps
        .backups_root()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    crate::audit::record("docker.remove-orphans", &f.what);
    let (note, error) =
        match crate::orphans::remove(&data, &protected_projects(&apps).await, volumes).await {
            Ok(s) => (Some(s), None),
            Err(e) => (None, Some(e)),
        };
    Ok(disk_html(&apps, note, error).await)
}
