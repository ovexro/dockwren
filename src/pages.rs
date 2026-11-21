//! Page shell, navigation and the simple pages.

use super::*;

#[derive(Template)]
#[template(path = "page.html")]
pub(super) struct PageTemplate {
    title: &'static str,
    active: &'static str,
    nav: &'static [(&'static str, &'static str, &'static str, &'static str)],
    version: &'static str,
    assets: String,
    tab: &'static str,
    /// Ready-made HTML for pages that are not a plain list (the vulnerability report).
    extra: String,
}

#[derive(Template)]
#[template(path = "vulns.html")]
struct VulnsTemplate {
    label: String,
    scanned: Option<String>,
    has_counts: bool,
    critical: usize,
    high: usize,
    details: Vec<crate::vulnscan::Detail>,
}

/// Tabs of the Settings page: (key, label). The first is the default.
pub(super) const SETTINGS_TABS: [(&str, &str); 6] = [
    ("general", "General"),
    ("notifications", "Notifications"),
    ("security", "Security"),
    ("backups", "Backups"),
    ("updates", "Updates"),
    ("system", "System"),
];

/// Tabs of the Apps page: (key, label). The first is the default.
pub(super) const APPS_TABS: [(&str, &str); 2] = [("installed", "Installed"), ("add", "Add an app")];

/// (href, key, label, short label for the narrow bottom bar) of the sidebar entries.
pub(super) const NAV: [(&str, &str, &str, &str); 7] = [
    ("/", "overview", "Overview", "Overview"),
    ("/apps", "apps", "Apps", "Apps"),
    ("/containers", "containers", "Containers", "Docker"),
    ("/resources", "resources", "Resources", "Stats"),
    ("/notifications", "notifications", "Notifications", "Alerts"),
    ("/cloudflare", "cloudflare", "Cloudflare", "Cloudflare"),
    ("/settings", "settings", "Settings", "Settings"),
];

pub(super) fn page(active: &'static str) -> Html<String> {
    page_tab(active, "")
}

pub(super) fn page_tab(active: &'static str, tab: &'static str) -> Html<String> {
    let title = NAV
        .iter()
        .find(|n| n.1 == active)
        .map_or("Dockwren", |n| n.2);
    let page = PageTemplate {
        title,
        active,
        nav: &NAV,
        version: env!("CARGO_PKG_VERSION"),
        assets: asset_version(),
        tab,
        extra: String::new(),
    };
    Html(page.render().expect("page template renders"))
}

pub(super) async fn health() -> &'static str {
    "ok"
}

pub(super) async fn overview(State(state): State<AppState>) -> Html<String> {
    let list = state.docker.containers().await.map_err(|e| e.to_string());
    let rows: Result<Vec<(String, String, String)>, String> =
        list.map(|l| l.into_iter().map(|c| (c.name, c.state, c.status)).collect());
    let mut page = metrics::overview(
        &state.metrics.lock().unwrap(),
        rows.as_ref().map(|v| v.as_slice()).map_err(|e| e.clone()),
    );
    page.apps = apps::instance_count(&state.apps);
    page.facts = metrics::host_facts();
    if page.apps > 0 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        page.facts.push(metrics::Fact {
            icon: metrics::ICON_BACKUP,
            label: "Last backup",
            value: match apps::latest_backup(&state.apps) {
                Some(t) => format!("{} ago", metrics::short_span(now.saturating_sub(t))),
                None => "none yet".to_string(),
            },
            link: Some("/apps"),
        });
    }
    page.alerts = notify::alerts(&state.notify.lock().unwrap());
    Html(page.render().expect("overview template renders"))
}

pub(super) async fn index() -> Html<String> {
    page("overview")
}
/// `GET /apps`: the installed apps, or the catalog when nothing is installed yet.
pub(super) async fn apps_page(State(state): State<AppState>) -> Html<String> {
    let tab = if state.apps.instance_names().is_empty() {
        APPS_TABS[1].0
    } else {
        APPS_TABS[0].0
    };
    page_tab("apps", tab)
}
/// `GET /apps/tab/{tab}`: one tab of Apps; every tab has its own URL.
pub(super) async fn apps_tab(Path(tab): Path<String>) -> Response {
    match APPS_TABS.iter().find(|t| t.0 == tab) {
        Some(t) => page_tab("apps", t.0).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
/// `GET /apps/{name}/vulns`: the last image scan of one app, every finding.
pub(super) async fn vulns_page(Path(name): Path<String>) -> Response {
    if !crate::vulnscan::is_reportable(&name) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let sum = crate::vulnscan::summary(&name);
    let body = VulnsTemplate {
        label: format!("Vulnerabilities: {}", crate::vulnscan::display_name(&name)),
        scanned: sum.as_ref().map(|s| crate::apps::utc_stamp(s.scanned)),
        has_counts: sum.as_ref().is_some_and(|s| s.critical + s.high > 0),
        critical: sum.as_ref().map_or(0, |s| s.critical),
        high: sum.as_ref().map_or(0, |s| s.high),
        details: sum.map(|s| s.details).unwrap_or_default(),
    }
    .render()
    .expect("vulns template renders");
    let page = PageTemplate {
        title: "Vulnerabilities",
        active: "apps",
        nav: &NAV,
        version: env!("CARGO_PKG_VERSION"),
        assets: asset_version(),
        tab: "vulns",
        extra: body,
    };
    Html(page.render().expect("page template renders")).into_response()
}
pub(super) async fn containers_page() -> Html<String> {
    page("containers")
}
pub(super) async fn resources_page() -> Html<String> {
    page("resources")
}
pub(super) async fn cloudflare_page() -> Html<String> {
    page("cloudflare")
}
pub(super) async fn notifications_page() -> Html<String> {
    page("notifications")
}
pub(super) async fn settings_page() -> Html<String> {
    page_tab("settings", SETTINGS_TABS[0].0)
}
/// `GET /settings/tab/{tab}`: one tab of Settings; every tab has its own URL.
pub(super) async fn settings_tab(Path(tab): Path<String>) -> Response {
    match SETTINGS_TABS.iter().find(|t| t.0 == tab) {
        Some(t) => page_tab("settings", t.0).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
