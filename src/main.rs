mod adopt;
mod alerts;
mod apps;
mod audit;
mod auth;
mod backup;
mod backupwatch;
mod clone;
mod config;
mod configbundle;
mod deliver;
mod direct;
mod diskusage;
mod docker;
mod doctor;
mod hostupdates;
mod install;
mod limits;
mod listeners;
mod logcap;
mod metrics;
mod notify;
mod offsite;
mod osupdate;
mod public;
mod release;
mod restoretest;
mod runner;
mod schedule;
mod selfupdate;
mod signin;
mod smart;
mod stack;
mod statusapi;
mod template;
mod terminal;
mod thirdparty;
mod totp;
mod update;
mod updatepolicy;
mod upstream;
mod vulnscan;
mod webcheck;

use std::sync::Arc;

use askama::Template;
use auth::SharedAuth;
use axum::{
    Router,
    extract::{FromRef, Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    middleware::from_fn_with_state,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use docker::{Action, ContainerInfo, DockerClient};

#[derive(Clone)]
struct AppState {
    auth: SharedAuth,
    docker: DockerClient,
    apps: apps::Apps,
    metrics: metrics::Shared,
    notify: notify::Shared,
    terminal: Arc<terminal::Switch>,
}

impl FromRef<AppState> for notify::Shared {
    fn from_ref(state: &AppState) -> Self {
        state.notify.clone()
    }
}

impl FromRef<AppState> for metrics::Shared {
    fn from_ref(state: &AppState) -> Self {
        state.metrics.clone()
    }
}

impl FromRef<AppState> for apps::Apps {
    fn from_ref(state: &AppState) -> Self {
        state.apps.clone()
    }
}

impl FromRef<AppState> for SharedAuth {
    fn from_ref(state: &AppState) -> Self {
        state.auth.clone()
    }
}

#[derive(Template)]
#[template(path = "containers.html")]
struct ContainersTemplate {
    containers: Vec<ContainerInfo>,
    error: Option<String>,
    terminal: bool,
}

fn render_containers(result: Result<Vec<ContainerInfo>, String>, terminal: bool) -> String {
    let page = match result {
        Ok(containers) => ContainersTemplate {
            containers,
            error: None,
            terminal,
        },
        Err(e) => ContainersTemplate {
            containers: Vec::new(),
            error: Some(format!("Docker is not reachable: {e}")),
            terminal,
        },
    };
    page.render().expect("containers template renders")
}

async fn containers(State(state): State<AppState>) -> Html<String> {
    let result = state.docker.containers().await.map_err(|e| e.to_string());
    Html(render_containers(result, state.terminal.is_on()))
}

#[derive(Template)]
#[template(path = "logs.html")]
struct LogsTemplate {
    id: String,
    text: String,
    error: Option<String>,
    q: String,
    level: String,
    since: String,
    /// Lines that matched (only meaningful while a filter is on).
    matched: usize,
    filtered: bool,
    /// Render only the refreshing part (the filter form stays untouched while typing).
    part: bool,
}

#[derive(serde::Deserialize, Default)]
struct LogQuery {
    q: Option<String>,
    level: Option<String>,
    since: Option<String>,
    part: Option<String>,
}

/// Parsed, bounded log filter.
struct LogFilter {
    q: String,
    level: String,
    since: String,
}

impl LogFilter {
    fn from(q: &LogQuery) -> Self {
        let pick = |v: &Option<String>, ok: &[&str], d: &str| {
            v.as_deref()
                .filter(|x| ok.contains(x))
                .unwrap_or(d)
                .to_string()
        };
        Self {
            q: q.q.as_deref().unwrap_or("").chars().take(100).collect(),
            level: pick(&q.level, &["all", "warn", "error"], "all"),
            since: pick(&q.since, &["all", "15m", "1h", "24h"], "all"),
        }
    }
    fn active(&self) -> bool {
        !self.q.is_empty() || self.level != "all" || self.since != "all"
    }
    /// Unix time to read logs from (0 = no limit).
    fn since_unix(&self) -> i32 {
        let secs = match self.since.as_str() {
            "15m" => 900,
            "1h" => 3600,
            "24h" => 86400,
            _ => return 0,
        };
        (metrics::now_unix().saturating_sub(secs)).min(i32::MAX as u64) as i32
    }
}

/// Lines read from Docker when a filter is on (the plain view shows the last `LOG_LINES`).
const LOG_SEARCH_LINES: u32 = 20_000;
/// Most matching lines shown in the page; the download has everything that was read.
const LOG_SHOWN_LINES: usize = 500;

async fn read_logs(
    docker: &docker::DockerClient,
    id: &str,
    f: &LogFilter,
) -> Result<(String, usize), bollard::errors::Error> {
    if !f.active() {
        let t = docker.logs(id, LOG_LINES).await?;
        let n = t.lines().count();
        return Ok((t, n));
    }
    let raw = docker
        .logs_since(id, LOG_SEARCH_LINES, f.since_unix())
        .await?;
    Ok(docker::filter_log_lines(&raw, &f.q, &f.level))
}

async fn container_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LogQuery>,
) -> Result<Html<String>, StatusCode> {
    if !docker::valid_id(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let f = LogFilter::from(&query);
    let (text, matched, error) = match read_logs(&state.docker, &id, &f).await {
        Ok((t, n)) => {
            // Show the newest lines only.
            let skip = n.saturating_sub(LOG_SHOWN_LINES);
            let shown: Vec<&str> = t.lines().skip(skip).collect();
            (shown.join("\n"), n, None)
        }
        Err(e) => (String::new(), 0, Some(format!("Cannot read logs: {e}"))),
    };
    let page = LogsTemplate {
        filtered: f.active(),
        id,
        text,
        error,
        q: f.q,
        level: f.level,
        since: f.since,
        matched,
        part: query.part.is_some(),
    };
    Ok(Html(page.render().expect("logs template renders")))
}

/// The same filter as a plain-text download (everything that was read, not just the page's tail).
async fn download_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LogQuery>,
) -> Result<impl IntoResponse, StatusCode> {
    if !docker::valid_id(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let f = LogFilter::from(&query);
    let raw = state
        .docker
        .logs_since(&id, LOG_SEARCH_LINES, f.since_unix())
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    let (text, _) = docker::filter_log_lines(&raw, &f.q, &f.level);
    Ok((
        [
            (
                header::CONTENT_TYPE,
                "text/plain; charset=utf-8".to_string(),
            ),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{id}.log\""),
            ),
        ],
        text,
    ))
}

async fn close_logs() -> Html<&'static str> {
    Html("")
}

const LOG_LINES: u32 = 200;

/// Rejects cross-origin POSTs (defence in depth on top of the SameSite=Strict session cookie).
pub fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true; // non-browser client; it still needs the session cookie
    };
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    let origin_host = origin
        .to_str()
        .ok()
        .and_then(|o| o.split_once("://"))
        .map(|(_, h)| h);
    host.is_some() && host == origin_host
}

async fn container_action(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, action)): Path<(String, String)>,
) -> Result<Html<String>, StatusCode> {
    if !same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let action = Action::parse(&action).ok_or(StatusCode::NOT_FOUND)?;
    if !docker::valid_id(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    if let Err(e) = state.docker.act(&id, action).await {
        return Ok(Html(render_containers(
            Err(e.to_string()),
            state.terminal.is_on(),
        )));
    }
    Ok(containers(State(state)).await)
}

const HTMX: &str = include_str!("../static/htmx.min.js");
const CSS: &str = include_str!("../static/dockwren.css");
const PASSKEY_JS: &str = include_str!("../static/passkey.js");
const FAVICON: &str = include_str!("../static/favicon.svg");

/// Short content hash of the bundled assets, appended to their URLs so a new build is never served from a stale cache.
pub fn asset_version() -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in CSS
        .bytes()
        .chain(HTMX.bytes())
        .chain(PASSKEY_JS.bytes())
        .chain(terminal::TERMINAL_JS.bytes())
    {
        h = (h ^ u64::from(b)).wrapping_mul(0x100000001b3);
    }
    format!("{:08x}", h >> 32)
}

async fn stylesheet() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        CSS,
    )
}

async fn favicon() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        FAVICON,
    )
}

async fn passkey_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        PASSKEY_JS,
    )
}

async fn htmx_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        HTMX,
    )
}

#[derive(Template)]
#[template(path = "page.html")]
struct PageTemplate {
    title: &'static str,
    active: &'static str,
    nav: &'static [(&'static str, &'static str, &'static str, &'static str)],
    version: &'static str,
    assets: String,
    tab: &'static str,
}

/// Tabs of the Settings page: (key, label). The first is the default.
const SETTINGS_TABS: [(&str, &str); 4] = [
    ("general", "General"),
    ("notifications", "Notifications"),
    ("security", "Security"),
    ("system", "System"),
];

/// (href, key, label, short label for the narrow bottom bar) of the sidebar entries.
const NAV: [(&str, &str, &str, &str); 7] = [
    ("/", "overview", "Overview", "Overview"),
    ("/apps", "apps", "Apps", "Apps"),
    ("/containers", "containers", "Containers", "Docker"),
    ("/resources", "resources", "Resources", "Stats"),
    ("/notifications", "notifications", "Notifications", "Alerts"),
    ("/cloudflare", "cloudflare", "Cloudflare", "Cloudflare"),
    ("/settings", "settings", "Settings", "Settings"),
];

fn page(active: &'static str) -> Html<String> {
    page_tab(active, "")
}

fn page_tab(active: &'static str, tab: &'static str) -> Html<String> {
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
    };
    Html(page.render().expect("page template renders"))
}

async fn health() -> &'static str {
    "ok"
}

async fn overview(State(state): State<AppState>) -> Html<String> {
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

async fn index() -> Html<String> {
    page("overview")
}
async fn apps_page() -> Html<String> {
    page("apps")
}
async fn containers_page() -> Html<String> {
    page("containers")
}
async fn resources_page() -> Html<String> {
    page("resources")
}
async fn cloudflare_page() -> Html<String> {
    page("cloudflare")
}
async fn notifications_page() -> Html<String> {
    page("notifications")
}
async fn settings_page() -> Html<String> {
    page_tab("settings", SETTINGS_TABS[0].0)
}
/// `GET /settings/tab/{tab}`: one tab of Settings; every tab has its own URL.
async fn settings_tab(Path(tab): Path<String>) -> Response {
    match SETTINGS_TABS.iter().find(|t| t.0 == tab) {
        Some(t) => page_tab("settings", t.0).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[allow(clippy::too_many_arguments)]
fn app(
    auth: SharedAuth,
    docker: DockerClient,
    apps: apps::Apps,
    metrics: metrics::Shared,
    notify: notify::Shared,
    terminal: Arc<terminal::Switch>,
    os: Arc<osupdate::Control>,
    su: Arc<selfupdate::Control>,
) -> Router {
    let terminal_routes = terminal::router(
        auth.clone(),
        docker.clone(),
        notify.clone(),
        terminal.clone(),
    );
    let policy_routes = updatepolicy::router(auth.clone(), apps.clone(), os.clone(), su.clone());
    let os_routes = osupdate::router(auth.clone(), notify.clone(), os);
    let su_routes = selfupdate::router(auth.clone(), su);
    let protected = Router::new()
        .route("/", get(index))
        .route("/metrics", get(metrics::page))
        .route("/apps", get(apps_page))
        .route("/containers", get(containers_page))
        .route("/resources", get(resources_page))
        .route("/settings", get(settings_page))
        .route("/settings/tab/{tab}", get(settings_tab))
        .route("/settings/audit", get(audit::show))
        .route("/settings/templates", get(thirdparty::show))
        .route(
            "/settings/templates/review",
            post(thirdparty::review_submit),
        )
        .route("/settings/templates/add", post(thirdparty::add_submit))
        .route(
            "/settings/templates/remove",
            post(thirdparty::remove_submit),
        )
        .route("/settings/doctor", get(doctor::show))
        .route("/settings/disk", get(apps::disk_show))
        .route("/settings/disk/prune-images", post(apps::prune_images))
        .route(
            "/instances/{name}/trim-snapshots",
            post(apps::trim_snapshots),
        )
        .route("/cloudflare", get(cloudflare_page))
        .route("/notifications", get(notifications_page))
        .route("/notifications/list", get(notify::list))
        .route("/notifications/count", get(notify::count))
        .route(
            "/notifications/dismiss-all",
            post(notify::dismiss_everything),
        )
        .route("/notifications/{id}/dismiss", post(notify::dismiss_one))
        .route("/overview", get(overview))
        .route("/containers/list", get(containers))
        .route("/containers/{id}/{action}", post(container_action))
        .route("/containers/{id}/logs", get(container_logs))
        .route("/containers/{id}/logs/download", get(download_logs))
        .route("/containers/{name}/graphs", get(metrics::container_graphs))
        .route("/logs/close", get(close_logs))
        .route("/catalog", get(apps::list))
        .route("/apps/{id}/icon.svg", get(apps::icon))
        .route("/apps/check-host", get(apps::check_host))
        .route("/stacks/new", get(apps::stack_form))
        .route("/stacks/install", post(apps::stack_submit))
        .route(
            "/stacks/adopt",
            get(apps::adopt_scan).post(apps::adopt_submit),
        )
        .route("/apps/{id}/new", get(apps::new_form))
        .route("/apps/{id}/install", post(apps::install_submit))
        .route("/jobs/{id}", get(apps::job))
        .route("/instances/list", get(apps::instances))
        .route("/instances/{name}/remove", post(apps::instance_remove))
        .route(
            "/instances/{name}/update",
            get(apps::update_preview).post(apps::instance_update),
        )
        .route("/instances/{name}/backup", post(apps::instance_backup))
        .route("/instances/{name}/skip-update", post(apps::instance_skip))
        .route(
            "/instances/{name}/unskip-update",
            post(apps::instance_unskip),
        )
        .route(
            "/settings/backups",
            get(apps::schedule_show).post(apps::schedule_save),
        )
        .route(
            "/settings/offsite",
            get(apps::offsite_show).post(apps::offsite_save),
        )
        .route(
            "/instances/{name}/public",
            get(apps::public_form).post(apps::public_submit),
        )
        .route(
            "/instances/{name}/public/publish",
            post(apps::public_publish),
        )
        .route("/instances/{name}/public/direct", post(apps::public_direct))
        .route("/settings/notifications", get(notify::kinds_show))
        .route("/settings/notifications/{kind}", post(notify::kind_set))
        .route("/settings/channels", get(deliver::show).post(deliver::save))
        .route("/settings/channels/test", post(deliver::test))
        .route("/settings/channels/{which}/remove", post(deliver::remove))
        .route("/settings/config-export", post(apps::config_download))
        .route("/settings/2fa", get(auth::twofa_show))
        .route("/settings/2fa/start", post(auth::twofa_start))
        .route("/settings/2fa/enable", post(auth::twofa_enable))
        .route("/settings/2fa/disable", post(auth::twofa_disable))
        .route("/settings/passkeys", get(auth::passkeys_show))
        .route("/settings/passkeys/options", post(auth::passkey_options))
        .route("/settings/passkeys/register", post(auth::passkey_register))
        .route("/settings/passkeys/remove", post(auth::passkey_remove))
        .route("/settings/cloudflare", get(apps::cloudflare_show))
        .route(
            "/settings/cloudflare/connect",
            post(apps::cloudflare_connect),
        )
        .route("/settings/cloudflare/tunnel", post(apps::cloudflare_tunnel))
        .route(
            "/settings/cloudflare/disconnect",
            post(apps::cloudflare_disconnect),
        )
        .route(
            "/instances/{name}/restore",
            get(apps::restore_form).post(apps::restore_submit),
        )
        .route(
            "/instances/{name}/settings",
            get(apps::settings_form).post(apps::settings_submit),
        )
        .route(
            "/instances/{name}/passwords",
            get(apps::passwords_form).post(apps::passwords_submit),
        )
        .route(
            "/instances/{name}/limits",
            get(apps::limits_form).post(apps::limits_submit),
        )
        .route("/instances/{name}/logcap", post(apps::logcap_submit))
        .route("/instances/{name}/pull", post(apps::pull_submit))
        .route("/instances/{name}/verify", post(apps::verify_submit))
        .route(
            "/instances/{name}/restore-test",
            post(apps::restore_test_submit),
        )
        .route("/instances/{name}/clone", post(apps::clone_submit))
        .route("/instances/{name}/cleanup", post(apps::cleanup_submit))
        .route("/static/htmx.min.js", get(htmx_js))
        .route("/logout", post(auth::logout))
        .route_layer(from_fn_with_state(auth.clone(), auth::require_session));
    let router = Router::new()
        .route("/health", get(health))
        .route("/api/status", get(statusapi::status))
        .route("/api/metrics", get(statusapi::metrics))
        .route("/static/dockwren.css", get(stylesheet))
        .route("/favicon.svg", get(favicon))
        .route("/favicon.ico", get(favicon))
        .route("/login", get(auth::login_form).post(auth::login_submit))
        .route("/login/passkey", post(auth::login_passkey))
        .route("/login/code", post(auth::login_code))
        .route("/static/passkey.js", get(passkey_js))
        .merge(protected)
        .with_state(AppState {
            auth,
            docker,
            apps,
            metrics,
            notify,
            terminal,
        });
    router
        .merge(terminal_routes)
        .merge(os_routes)
        .merge(su_routes)
        .merge(policy_routes)
}

/// `dockwren install <app> [KEY=VALUE...]`: resolve, prepare, bring up.
async fn cli_expose(cfg: &config::Config, args: &[String]) -> Result<(), String> {
    let apply = args.iter().any(|a| a == "--yes");
    let replace_dns = args.iter().any(|a| a == "--replace-dns");
    let usage = "usage: dockwren expose <instance> [hostname] [--access EMAIL[,EMAIL...]] [--replace-dns] [--yes]";
    let mut access: Vec<String> = Vec::new();
    let mut pos: Vec<&String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--access" {
            let v = it.next().ok_or(usage)?;
            access = v.split(',').map(str::to_string).collect();
            if let Some(bad) = access.iter().find(|e| !public::valid_email(e)) {
                return Err(format!("not an e-mail address: {bad}"));
            }
        } else if !a.starts_with("--") {
            pos.push(a);
        }
    }
    let name = pos.first().ok_or(usage)?;
    if !runner::valid_name(name) {
        return Err("invalid instance name".into());
    }
    let dir = cfg.instances_dir().join(name.as_str());
    if !dir.join("state.toml").is_file() {
        return Err(format!("no instance named {name}"));
    }
    let host = match pos.get(1) {
        Some(h) => {
            public::write(&dir, h)?;
            h.to_string()
        }
        None => public::read(&dir).ok_or("no hostname set: pass one, or set it in the web UI")?,
    };
    let port = public::web_port(&dir).ok_or("this app has no WEB_PORT to publish")?;
    let settings = public::Settings::from_env().ok_or(
        "Cloudflare is not connected: set CLOUDFLARE_API_TOKEN, CLOUDFLARE_ACCOUNT_ID and DOCKWREN_TUNNEL_ID, or paste a token in the panel's Cloudflare tab (and run this with the same DOCKWREN_DATA)",
    )?;
    public::expose(
        &settings,
        &host,
        &public::service_url(port),
        &access,
        apply,
        replace_dns,
        &mut |s| println!("  {s}"),
    )
    .await
}

/// `dockwren unexpose <instance> [--yes]`: remove the tunnel route and DNS record, then forget the hostname.
async fn cli_unexpose(cfg: &config::Config, args: &[String]) -> Result<(), String> {
    let apply = args.iter().any(|a| a == "--yes");
    let name = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or("usage: dockwren unexpose <instance> [--yes]")?;
    if !runner::valid_name(name) {
        return Err("invalid instance name".into());
    }
    let dir = cfg.instances_dir().join(name.as_str());
    if !dir.join("state.toml").is_file() {
        return Err(format!("no instance named {name}"));
    }
    let host = public::read(&dir).ok_or("this instance has no public hostname")?;
    let settings = public::Settings::from_env().ok_or(
        "Cloudflare is not connected: set CLOUDFLARE_API_TOKEN, CLOUDFLARE_ACCOUNT_ID and DOCKWREN_TUNNEL_ID, or paste a token in the panel's Cloudflare tab (and run this with the same DOCKWREN_DATA)",
    )?;
    public::unexpose(&settings, &host, apply, &mut |s| println!("  {s}")).await?;
    if apply {
        public::write(&dir, "")?;
    }
    Ok(())
}

/// Passphrase from `DOCKWREN_PASSPHRASE`, else the first line of stdin.
fn read_passphrase() -> Result<String, String> {
    if let Ok(p) = std::env::var("DOCKWREN_PASSPHRASE") {
        return Ok(p);
    }
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("passphrase: {e}"))?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

/// `dockwren export FILE` / `dockwren import FILE`: encrypted copy of every app's config.
async fn cli_config(cfg: &config::Config, export: bool, args: &[String]) -> Result<(), String> {
    let [file] = args else {
        return Err(format!(
            "usage: dockwren {} FILE   (passphrase from DOCKWREN_PASSPHRASE or the first line of stdin)",
            if export { "export" } else { "import" }
        ));
    };
    let file = std::path::Path::new(file);
    let pass = read_passphrase()?;
    let root = cfg.instances_dir();
    if export {
        let n = configbundle::export(&root, file, &pass)?;
        println!(
            "{n} app(s) exported to {} (encrypted; it holds your apps' passwords, keep it safe).",
            file.display()
        );
        return Ok(());
    }
    let r = configbundle::import(&root, file, &pass, false)?;
    for n in &r.skipped {
        println!("{n}: already exists here, skipped");
    }
    print_panel(&r);
    for n in &r.created {
        println!("{n}: recreated, starting (empty, data comes from a snapshot)");
        let inst = runner::Instance {
            name: n.clone(),
            dir: root.join(n),
        };
        if let Err(e) = runner::bring_up(&inst, &mut |s| println!("  {s}")).await {
            println!("  {n}: could not start: {e}");
        }
        if let Some(h) = r.hosts.get(n) {
            println!("  was public at {h}: publish again with `dockwren expose {n} {h}`");
        }
        println!(
            "  then restore data: dockwren backup pull {n} (if off-host backups are set up), dockwren restore {n}"
        );
    }
    Ok(())
}

fn print_panel(r: &configbundle::Imported) {
    if !r.panel_restored.is_empty() {
        println!(
            "panel settings restored: {} (sign in with your old password and 2FA; restart the panel to apply)",
            r.panel_restored.join(", ")
        );
    }
    if !r.panel_kept.is_empty() {
        println!(
            "panel settings kept as they are on this server: {}",
            r.panel_kept.join(", ")
        );
    }
}

/// One answer for `recover`: env `DOCKWREN_OFFSITE_<KEY>`, else a prompt (hidden for secrets).
fn ask(key: &str, label: &str, secret: bool) -> Result<String, String> {
    if let Ok(v) = std::env::var(format!("DOCKWREN_OFFSITE_{key}")) {
        return Ok(v);
    }
    use std::io::{IsTerminal, Write};
    let tty = std::io::stdin().is_terminal();
    eprint!("{label}: ");
    let _ = std::io::stderr().flush();
    let hide = secret
        && tty
        && std::process::Command::new("stty")
            .arg("-echo")
            .status()
            .is_ok_and(|s| s.success());
    let mut line = String::new();
    let r = std::io::stdin().read_line(&mut line);
    if hide {
        let _ = std::process::Command::new("stty").arg("echo").status();
        eprintln!();
    }
    r.map_err(|e| format!("{label}: {e}"))?;
    Ok(line.trim().to_string())
}

/// Off-host target for `recover`: the saved one, or asked for (kind and fields).
fn recover_target(off: &offsite::Offsite) -> Result<(), String> {
    if off.get().enabled() {
        println!("using the off-host target already saved on this server");
        return Ok(());
    }
    let mut f = offsite::Form {
        kind: ask("KIND", "Off-host target type (sftp or s3)", false)?.to_lowercase(),
        ..Default::default()
    };
    match f.kind.as_str() {
        "s3" => {
            f.endpoint = ask("ENDPOINT", "S3 endpoint URL (https://...)", false)?;
            f.bucket = ask("BUCKET", "Bucket", false)?;
            f.access_key = ask("ACCESS_KEY", "Access key", true)?;
            f.secret_key = ask("SECRET_KEY", "Secret key", true)?;
        }
        "sftp" => {
            f.host = ask("HOST", "SFTP host", false)?;
            f.port = ask("PORT", "SFTP port (empty = 22)", false)?;
            f.user = ask("USER", "SFTP user", false)?;
            f.path = ask("PATH", "Folder on the server", false)?;
            let file = ask(
                "PRIVATE_KEY_FILE",
                "Path of the SSH private key file",
                false,
            )?;
            f.private_key = std::fs::read_to_string(&file).map_err(|e| format!("{file}: {e}"))?;
        }
        _ => return Err("the target type must be sftp or s3".into()),
    }
    f.password = ask(
        "PASSWORD",
        "Repository password (from your password manager)",
        true,
    )?;
    if f.password.is_empty() {
        return Err("the repository password is needed".into());
    }
    let (c, _) = offsite::Config::from_form(&f, &offsite::Config::default())?;
    off.set(c)
}

/// `dockwren recover`: rebuild a lost server from the off-host target alone.
async fn cli_recover(cfg: &config::Config) -> Result<(), String> {
    let off = offsite::Offsite::open(&cfg.data_dir);
    recover_target(&off)?;
    println!("downloading the config export...");
    let bytes = off.pull_config().await?;
    let root = cfg.instances_dir();
    let r = configbundle::import_bytes(&root, &bytes, &off.get().password, true)?;
    for n in &r.skipped {
        println!("{n}: already exists here, left alone");
    }
    print_panel(&r);
    let backups = cfg.data_dir.join("backups");
    let templates = apps::catalog(cfg)?;
    let mut failed = Vec::new();
    let mut empty: Vec<String> = Vec::new();
    for n in &r.created {
        println!("== {n}");
        let inst = runner::Instance {
            name: n.clone(),
            dir: root.join(n),
        };
        let step = async {
            runner::bring_up(&inst, &mut |s| println!("  {s}"))
                .await
                .map_err(|e| format!("could not start: {e}"))?;
            match off.pull(&backups, n, &mut |s| println!("  {s}")).await {
                Ok(_) => {}
                Err(e) if !backup::list(&backups, n).is_empty() => {
                    println!("  pull: {e} (using the snapshots already here)")
                }
                Err(e) if e.contains("no snapshot found") => {
                    println!("  no backup of {n} exists off-host: it stays empty (a fresh app)");
                    empty.push(n.clone());
                    return Ok(());
                }
                Err(e) => return Err(format!("no data to restore: {e}")),
            }
            backup::restore_instance(cfg, &templates, n, None, false, &mut |s| println!("  {s}"))
                .await
                .map(|snap| println!("  restored from {}", snap.display()))
        };
        if let Err(e) = step.await {
            println!("  {n}: {e}");
            failed.push(n.clone());
        } else if let Some(h) = r.hosts.get(n) {
            if empty.contains(n) {
                // A fresh app without its data must not be opened to the internet by itself.
                println!(
                    "  was public at {h}: publish again with `dockwren expose {n} {h} --yes` once it is set up"
                );
            } else {
                public::publish_after_install(&root.join(n), n, h, &mut |s| println!("  {s}"))
                    .await;
            }
        }
    }
    println!(
        "recovered {} of {} app(s); {} started empty (no backup existed)",
        r.created.len() - failed.len() - empty.len(),
        r.created.len(),
        empty.len()
    );
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "not recovered: {} (the others are done; retry those with `dockwren backup pull <app>` and `dockwren restore <app>`)",
            failed.join(", ")
        ))
    }
}

async fn cli_install(cfg: &config::Config, args: &[String]) -> Result<(), String> {
    let app = args
        .first()
        .ok_or("usage: dockwren install <app> [KEY=VALUE...]")?;
    let templates = apps::catalog(cfg)?;
    let t = templates
        .iter()
        .find(|t| &t.manifest.app.id == app)
        .ok_or_else(|| format!("unknown app {app}"))?;
    let input = args[1..]
        .iter()
        .map(|a| {
            a.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or(format!("expected KEY=VALUE, got {a}"))
        })
        .collect::<Result<std::collections::HashMap<_, _>, _>>()?;
    let root = cfg.instances_dir();
    let claimed = runner::claimed_ports(&root);
    let resolved = install::resolve(t, &input, &claimed, &install::port_is_free)?;
    install::check_lan_ports(t)?;
    let inst = runner::prepare(&root, t, &resolved.env)?;
    println!("instance {} prepared in {}", inst.name, inst.dir.display());
    runner::bring_up(&inst, &mut |step| println!("  {step}"))
        .await
        .map_err(|e| {
            format!(
                "{e}\n(instance kept in {}; `dockwren remove {}` stops it)",
                inst.dir.display(),
                inst.name
            )
        })?;
    let captured =
        runner::run_post_start(&inst, t, &resolved.env, &mut |step| println!("  {step}")).await?;
    if let Some(host) = &resolved.public_host {
        public::publish_after_install(&inst.dir, &inst.name, host, &mut |s| println!("  {s}"))
            .await;
    }
    for key in ["WEB_PORT", "SSH_PORT"] {
        if let Some(p) = resolved.env.get(key) {
            println!("  {key}: 127.0.0.1:{p}");
        }
    }
    for (k, v) in captured.iter().chain(&resolved.revealed) {
        println!("  {k} (shown once): {v}");
    }
    println!("up and healthy");
    Ok(())
}

const USAGE: &str = "usage: dockwren [serve | set-password | disable-2fa | status-token [--revoke] | install <app> [KEY=VALUE...] | export FILE | import FILE | recover | backup <instance> | backup pull <instance> | update <instance> | restore <instance> [snapshot | --list] | verify <instance> [snapshot] | restore-test <instance> | scan <instance> | doctor | template add <https-url> <commit> [--yes] | template list | template remove <id> | cleanup <instance> | expose <instance> [hostname] [--access EMAIL] [--yes] | unexpose <instance> [--yes] | remove <instance> | setup-osupdate [--data-dir DIR] | setup-smart [--data-dir DIR] | self-update | --version]";

/// CLI commands work on the panel's data directory; explain the usual permission mistake
/// instead of failing later with a bare "Permission denied".
fn check_data_dir(dir: &std::path::Path) {
    let probe = dir.join(".dockwren-probe");
    let ok = std::fs::create_dir_all(dir)
        .and_then(|_| std::fs::write(&probe, b""))
        .and_then(|_| std::fs::remove_file(&probe));
    if let Err(e) = ok {
        eprintln!(
            "cannot use the data directory {} ({e}).\nSet DOCKWREN_DATA to the panel's data directory and run as its owner, for example:\n  sudo -E -u dockwren env DOCKWREN_DATA=/var/lib/dockwren dockwren <command>",
            dir.display()
        );
        std::process::exit(1);
    }
}

#[tokio::main]
async fn main() {
    let cfg = config::Config::from_env();
    let cmd = std::env::args().nth(1);
    match cmd.as_deref() {
        Some("--version" | "-V" | "version") => {
            println!("dockwren {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("--help" | "-h" | "help") => {
            println!("{USAGE}");
            return;
        }
        None
        | Some("serve")
        | Some("set-password")
        | Some("disable-2fa")
        | Some("status-token")
        | Some("setup-osupdate")
        | Some("setup-smart")
        | Some("smart-collect")
        | Some("self-update") => {}
        Some(
            "install" | "export" | "import" | "recover" | "backup" | "update" | "restore"
            | "verify" | "restore-test" | "scan" | "template" | "cleanup" | "expose" | "unexpose"
            | "remove",
        ) => check_data_dir(&cfg.data_dir),
        Some(_) => {}
    }
    match cmd.as_deref() {
        None | Some("serve") => {}
        Some("self-update") => {
            let on_start = std::env::args().nth(2).as_deref() == Some("--on-start");
            if let Err(e) = selfupdate::cli(&cfg.data_dir, on_start).await {
                eprintln!("self-update failed: {e}");
                std::process::exit(1);
            }
            return;
        }
        Some(c @ ("setup-smart" | "smart-collect")) => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            let dir = match args.as_slice() {
                [] => cfg.data_dir.clone(),
                [flag, d] if flag == "--data-dir" => std::path::PathBuf::from(d),
                _ => {
                    eprintln!("usage: dockwren {c} [--data-dir DIR]");
                    std::process::exit(2);
                }
            };
            let res = if c == "setup-smart" {
                smart::setup(&dir)
            } else {
                smart::collect(&dir)
            };
            match res {
                Ok(n) => println!("SMART: {n} disk(s) read into {}/smart.json.", dir.display()),
                Err(e) => {
                    eprintln!("{c} failed: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("setup-osupdate") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            let dir = match args.as_slice() {
                [] => cfg.data_dir.clone(),
                [flag, d] if flag == "--data-dir" => std::path::PathBuf::from(d),
                _ => {
                    eprintln!("usage: dockwren setup-osupdate [--data-dir DIR]");
                    std::process::exit(2);
                }
            };
            match osupdate::setup(&dir) {
                Ok(()) => println!(
                    "OS-update helper installed for {}. Turn it on in the panel: Settings, OS updates.",
                    dir.display()
                ),
                Err(e) => {
                    eprintln!("setup-osupdate failed: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("disable-2fa") => {
            match auth::disable_two_factor(&cfg.data_dir) {
                Ok(true) => println!("Two-factor sign-in is off. Set it up again in Settings."),
                Ok(false) => println!("Two-factor sign-in was not on."),
                Err(e) => {
                    eprintln!("cannot turn it off in {}: {e}", cfg.data_dir.display());
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("status-token") => {
            let revoke = std::env::args().nth(2).as_deref() == Some("--revoke");
            if revoke {
                match statusapi::revoke(&cfg.data_dir) {
                    Ok(true) => println!("Status token revoked."),
                    Ok(false) => println!("There was no status token."),
                    Err(e) => {
                        eprintln!("cannot revoke: {e}");
                        std::process::exit(1);
                    }
                }
                return;
            }
            let token = statusapi::create(&cfg.data_dir).unwrap_or_else(|e| {
                panic!("cannot write the token in {}: {e}", cfg.data_dir.display())
            });
            println!("New read-only status token (shown once, only its hash is stored):");
            println!("    {token}");
            println!(
                "Use it as: curl -H 'Authorization: Bearer <token>' https://<panel>/api/status"
            );
            println!("It replaces any earlier token. Revoke with: dockwren status-token --revoke");
            return;
        }
        Some("set-password") => {
            let password = auth::set_password(&cfg.data_dir).unwrap_or_else(|e| {
                panic!("cannot set password in {}: {e}", cfg.data_dir.display())
            });
            println!("New admin password (shown once, not stored):");
            println!("    {password}");
            println!("A running server applies it immediately and signs all sessions out.");
            return;
        }
        Some(c @ ("export" | "import")) => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            if let Err(e) = cli_config(&cfg, c == "export", &args).await {
                eprintln!("{c} failed: {e}");
                std::process::exit(1);
            }
            return;
        }
        Some("recover") => {
            if let Err(e) = cli_recover(&cfg).await {
                eprintln!("recover failed: {e}");
                std::process::exit(1);
            }
            return;
        }
        Some("install") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            if let Err(e) = cli_install(&cfg, &args).await {
                eprintln!("install failed: {e}");
                std::process::exit(1);
            }
            return;
        }
        Some("backup") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            let (pull, name) = match args.as_slice() {
                [n] => (false, n.clone()),
                [p, n] if p == "pull" => (true, n.clone()),
                _ => (false, String::new()),
            };
            if !runner::valid_name(&name) {
                eprintln!("usage: dockwren backup <instance> | dockwren backup pull <instance>");
                std::process::exit(2);
            }
            let off = offsite::Offsite::open(&cfg.data_dir);
            let backups = cfg.data_dir.join("backups");
            if pull {
                match off.pull(&backups, &name, &mut |s| println!("  {s}")).await {
                    Ok(added) if added.is_empty() => {
                        println!("nothing new: every off-host snapshot is already here")
                    }
                    Ok(added) => println!(
                        "{} snapshot(s) added; install the app if this server does not have it yet, then: dockwren restore {name}",
                        added.len()
                    ),
                    Err(e) => {
                        eprintln!("pull failed: {e}");
                        std::process::exit(1);
                    }
                }
                return;
            }
            let result = async {
                let templates = apps::catalog(&cfg)?;
                let dir =
                    backup::snapshot_instance(&cfg, &templates, &name, &mut |s| println!("  {s}"))
                        .await?;
                off.push(&backups, &name, backup::KEEP, &mut |s| println!("  {s}"))
                    .await
                    .map_err(|e| {
                        format!(
                            "local snapshot saved at {}, off-host upload failed: {e}",
                            dir.display()
                        )
                    })?;
                Ok::<_, String>(dir)
            }
            .await;
            match result {
                Ok(dir) => println!("backup written to {}", dir.display()),
                Err(e) => {
                    eprintln!("backup failed: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("update") => {
            let name = std::env::args().nth(2).unwrap_or_default();
            if !runner::valid_name(&name) {
                eprintln!("usage: dockwren update <instance>");
                std::process::exit(2);
            }
            let result = async {
                let templates = apps::catalog(&cfg)?;
                update::update_instance(
                    &cfg.instances_dir(),
                    &cfg.data_dir.join("backups"),
                    &templates,
                    &name,
                    &mut |s| println!("  {s}"),
                )
                .await
            }
            .await;
            match result {
                Ok(update::Outcome::UpToDate) => println!("{name} is already up to date"),
                Ok(update::Outcome::Updated { from, to }) => {
                    println!("{name} updated from template version {from} to {to}, up and healthy")
                }
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("expose") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            if let Err(e) = cli_expose(&cfg, &args).await {
                eprintln!("expose failed: {e}");
                std::process::exit(1);
            }
            return;
        }
        Some("unexpose") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            if let Err(e) = cli_unexpose(&cfg, &args).await {
                eprintln!("unexpose failed: {e}");
                std::process::exit(1);
            }
            return;
        }
        Some("restore") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            let safety = !args.iter().any(|a| a == "--no-safety-snapshot");
            let list = args.iter().any(|a| a == "--list");
            let pos: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
            let name = pos.first().map(|s| s.as_str()).unwrap_or_default();
            if list {
                if !runner::valid_name(name) || pos.len() != 1 || args.len() != 2 {
                    eprintln!("usage: dockwren restore <instance> --list");
                    std::process::exit(2);
                }
                let snaps = backup::list(&cfg.data_dir.join("backups"), name);
                if snaps.is_empty() {
                    println!("no snapshots of {name}");
                }
                for p in snaps {
                    let id = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                    let when = std::process::Command::new("date")
                        .args(["-d", &format!("@{id}"), "+%F %T"])
                        .output()
                        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                        .unwrap_or_default();
                    println!("{id}  {when}");
                }
                return;
            }
            if !runner::valid_name(name)
                || pos.len() > 2
                || args.len() > pos.len() + usize::from(!safety)
            {
                eprintln!("usage: dockwren restore <instance> [snapshot] [--no-safety-snapshot]");
                std::process::exit(2);
            }
            let result = async {
                let templates = apps::catalog(&cfg)?;
                backup::restore_instance(
                    &cfg,
                    &templates,
                    name,
                    pos.get(1).map(|s| s.as_str()),
                    safety,
                    &mut |s| println!("  {s}"),
                )
                .await
            }
            .await;
            match result {
                Ok(snap) => println!("restored from {}, up and healthy", snap.display()),
                Err(e) => {
                    eprintln!("restore failed: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("verify") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            let name = args.first().map(|s| s.as_str()).unwrap_or_default();
            if !runner::valid_name(name) || args.len() > 2 {
                eprintln!("usage: dockwren verify <instance> [snapshot]");
                std::process::exit(2);
            }
            let result = async {
                let templates = apps::catalog(&cfg)?;
                backup::verify_instance(
                    &cfg,
                    &templates,
                    name,
                    args.get(1).map(|s| s.as_str()),
                    &mut |s| println!("  {s}"),
                )
                .await
            }
            .await;
            match result {
                Ok(snap) => println!("{} verified: it can be restored", snap.display()),
                Err(e) => {
                    eprintln!("verify failed: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("doctor") => {
            let items = doctor::run(&cfg.data_dir).await;
            print!("{}", doctor::render_text(&items));
            if doctor::worst(&items) == doctor::Level::Fail {
                std::process::exit(1);
            }
            return;
        }
        Some("scan") => {
            let name = std::env::args().nth(2).unwrap_or_default();
            if !runner::valid_name(&name) || std::env::args().count() != 3 {
                eprintln!("usage: dockwren scan <instance>");
                std::process::exit(2);
            }
            let inst = runner::Instance {
                dir: cfg.instances_dir().join(&name),
                name: name.clone(),
            };
            if !inst.dir.join("state.toml").is_file() {
                eprintln!("no instance named {name}");
                std::process::exit(1);
            }
            match vulnscan::scan_app(&cfg.data_dir, &inst, &mut |s| println!("  {s}")).await {
                Some(f) => println!("{name}: {}", vulnscan::describe(&f)),
                None => {
                    eprintln!("{name}: no image could be scanned");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("restore-test") => {
            let name = std::env::args().nth(2).unwrap_or_default();
            if !runner::valid_name(&name) || std::env::args().count() != 3 {
                eprintln!("usage: dockwren restore-test <instance>");
                std::process::exit(2);
            }
            let result = async {
                let templates = apps::catalog(&cfg)?;
                let dir = cfg.instances_dir().join(&name);
                let state = std::fs::read_to_string(dir.join("state.toml"))
                    .map_err(|_| format!("no instance named {name}"))?;
                let tpl = toml::from_str::<toml::Table>(&state)
                    .ok()
                    .and_then(|t| t.get("template")?.as_str().map(String::from))
                    .ok_or("state.toml: no template")?;
                let t = templates
                    .iter()
                    .find(|t| t.manifest.app.id == tpl)
                    .ok_or_else(|| format!("template {tpl} is not available"))?;
                restoretest::run(
                    &cfg.data_dir,
                    &runner::Instance {
                        name: name.clone(),
                        dir,
                    },
                    t,
                    &mut |s| println!("  {s}"),
                )
                .await
            }
            .await;
            match result {
                Ok(Some(s)) => println!("restore test passed (snapshot {s}): it came back healthy"),
                Ok(None) => println!("restore test skipped"),
                Err(e) => {
                    eprintln!("restore test failed: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("template") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            let usage = "usage: dockwren template add <https-url> <full-commit-hash> [--yes] | template list | template remove <id>";
            match args
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .as_slice()
            {
                ["list"] => {
                    for s in thirdparty::list(&cfg.data_dir) {
                        println!("{}  {} @ {}", s.id, s.url, s.sha);
                    }
                }
                ["remove", id] => match thirdparty::remove(&cfg.data_dir, id) {
                    Ok(()) => {
                        println!("removed {id}; restart dockwren to drop it from the catalog")
                    }
                    Err(e) => {
                        eprintln!("{e}");
                        std::process::exit(1);
                    }
                },
                ["add", url, sha, rest @ ..] if rest.is_empty() || rest == ["--yes"] => {
                    let fetched = match thirdparty::fetch(&cfg.data_dir, url, sha).await {
                        Ok(f) => f,
                        Err(e) => {
                            eprintln!("cannot fetch: {e}");
                            std::process::exit(1);
                        }
                    };
                    let t = match thirdparty::review(&fetched, None) {
                        Ok(t) => t,
                        Err(e) => {
                            eprintln!("refused: {e}");
                            std::process::exit(1);
                        }
                    };
                    println!(
                        "== {} ({}) version {}\n{}\n\n== compose.yaml\n{}",
                        t.manifest.app.name,
                        t.manifest.app.id,
                        t.manifest.app.version,
                        t.manifest.app.description,
                        fetched.compose
                    );
                    if rest != ["--yes"] {
                        print!("Add this template? [y/N] ");
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                        let mut line = String::new();
                        let _ = std::io::stdin().read_line(&mut line);
                        if !line.trim().eq_ignore_ascii_case("y") {
                            println!("not added");
                            return;
                        }
                    }
                    match thirdparty::add(&cfg.data_dir, url, sha, &fetched) {
                        Ok(s) => println!("added {}; restart dockwren to load it", s.id),
                        Err(e) => {
                            eprintln!("cannot add: {e}");
                            std::process::exit(1);
                        }
                    }
                }
                _ => {
                    eprintln!("{usage}");
                    std::process::exit(2);
                }
            }
            return;
        }
        Some("cleanup") => {
            let name = std::env::args().nth(2).unwrap_or_default();
            if !runner::valid_name(&name) || std::env::args().count() != 3 {
                eprintln!(
                    "usage: dockwren cleanup <instance>   (removes *.pre-restore-* / *.failed-restore-* leftovers)"
                );
                std::process::exit(2);
            }
            match backup::cleanup_instance(&cfg, &name, &mut |s| println!("  {s}")).await {
                Ok(n) if n.is_empty() => println!("nothing to clean up"),
                Ok(n) => println!(
                    "removed {} leftover director{}",
                    n.len(),
                    if n.len() == 1 { "y" } else { "ies" }
                ),
                Err(e) => {
                    eprintln!("cleanup failed: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("remove") => {
            let name = std::env::args().nth(2).unwrap_or_default();
            if !runner::valid_name(&name) {
                eprintln!("usage: dockwren remove <instance>");
                std::process::exit(2);
            }
            let inst = runner::Instance {
                dir: cfg.instances_dir().join(&name),
                name,
            };
            match runner::take_down(&inst).await {
                Ok(()) => println!(
                    "{} stopped and containers removed (data kept in {})",
                    inst.name,
                    inst.dir.display()
                ),
                Err(e) => {
                    eprintln!("remove failed: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some(other) => {
            eprintln!("unknown command '{other}'\n{USAGE}");
            std::process::exit(2);
        }
    }
    let (_, first_run_password) = auth::load_or_create(&cfg.data_dir)
        .unwrap_or_else(|e| panic!("cannot prepare data dir {}: {e}", cfg.data_dir.display()));
    let auth = Arc::new(
        auth::Auth::open(&cfg.data_dir, cfg.secure_cookie).unwrap_or_else(|e| panic!("{e}")),
    );

    if let Some(password) = first_run_password {
        println!("First run: generated admin password (shown once, not stored):");
        println!("    {password}");
    }

    let docker =
        DockerClient::connect().unwrap_or_else(|e| panic!("cannot set up Docker client: {e}"));

    let listener = tokio::net::TcpListener::bind(&cfg.bind)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {}: {e}", cfg.bind));
    println!("dockwren listening on http://{}", cfg.bind);
    let templates = apps::catalog(&cfg).unwrap_or_else(|e| panic!("bad template: {e}"));
    audit::init(&cfg.data_dir);
    let notify = notify::shared(&cfg.data_dir);
    let apps = apps::Apps::new(templates, cfg.instances_dir())
        .with_notify(notify.clone())
        .with_auth(auth.clone())
        .with_schedule(Arc::new(schedule::Schedule::open(&cfg.data_dir)))
        .with_offsite(Arc::new(offsite::Offsite::open(&cfg.data_dir)));
    apps.spawn_update_checker();
    apps.spawn_backup_scheduler();
    apps.spawn_update_scheduler();
    release::spawn_checker(notify.clone());
    upstream::spawn_checker(apps.clone(), notify.clone());
    hostupdates::spawn_checker(notify.clone());
    webcheck::spawn_checker(apps.clone(), notify.clone());
    {
        // Direct HTTPS: make sure the Caddy container matches the instances (also after a restore).
        let (root, data) = (
            apps.root.clone(),
            apps.root.parent().map(|p| p.to_path_buf()),
        );
        tokio::spawn(async move {
            if let Some(data) = data
                && let Err(e) = direct::reconcile(&data, &root).await
            {
                eprintln!("direct HTTPS: {e}");
            }
        });
    }
    vulnscan::spawn_checker(apps.clone(), notify.clone(), cfg.data_dir.clone());
    backupwatch::spawn_checker(notify.clone());
    smart::spawn_checker(notify.clone(), cfg.data_dir.clone());
    signin::init(notify.clone(), &cfg.data_dir);
    let os_control = Arc::new(osupdate::Control::new(Some(&cfg.data_dir)));
    osupdate::spawn_watcher(os_control.clone(), notify.clone());
    deliver::spawn(notify.clone());
    let metrics = metrics::Shared::default();
    let mut disk_paths = vec![cfg.data_dir.display().to_string()];
    disk_paths.extend(cfg.disk_paths.iter().cloned());
    let inode_paths = disk_paths.clone();
    metrics::spawn(
        metrics.clone(),
        docker.clone(),
        disk_paths,
        cfg.data_dir.clone(),
    );
    alerts::spawn(notify.clone(), metrics.clone(), docker.clone(), inode_paths);
    axum::serve(
        listener,
        app(
            auth,
            docker,
            apps,
            metrics,
            notify,
            Arc::new(terminal::Switch::new(cfg.terminal, Some(&cfg.data_dir))),
            os_control,
            Arc::new(selfupdate::Control::new(&cfg.data_dir)),
        )
        .into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .expect("server error");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, Response, StatusCode, header},
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const PASSWORD: &str = "correct horse battery";

    fn test_app() -> Router {
        let hash = auth::hash_password(PASSWORD);
        let docker = DockerClient::connect().unwrap();
        let apps = apps::Apps::new(
            template::catalog(None).unwrap(),
            std::env::temp_dir().join("dockwren-test-none"),
        );
        app(
            Arc::new(auth::Auth::new(&hash, false).unwrap()),
            docker,
            apps,
            metrics::Shared::default(),
            Arc::new(std::sync::Mutex::new(notify::Store::memory())),
            Arc::new(terminal::Switch::new(false, None)),
            Arc::new(osupdate::Control::new(None)),
            Arc::new(selfupdate::Control::new(&std::env::temp_dir())),
        )
    }

    fn terminal_app() -> Router {
        let hash = auth::hash_password(PASSWORD);
        let docker = DockerClient::connect().unwrap();
        let apps = apps::Apps::new(
            template::catalog(None).unwrap(),
            std::env::temp_dir().join("dockwren-test-none"),
        );
        app(
            Arc::new(auth::Auth::new(&hash, false).unwrap()),
            docker,
            apps,
            metrics::Shared::default(),
            Arc::new(std::sync::Mutex::new(notify::Store::memory())),
            Arc::new(terminal::Switch::new(true, None)),
            Arc::new(osupdate::Control::new(None)),
            Arc::new(selfupdate::Control::new(&std::env::temp_dir())),
        )
    }

    #[tokio::test]
    async fn terminal_routes_exist_only_when_enabled() {
        let ws = |app: Router, cookie: Option<&str>, origin: Option<&str>, uri: &str| {
            let mut r = Request::get(uri).header(header::HOST, "panel.test");
            if let Some(c) = cookie {
                r = r.header(header::COOKIE, c);
            }
            if let Some(o) = origin {
                r = r.header(header::ORIGIN, o);
            }
            app.oneshot(r.body(Body::empty()).unwrap())
        };
        let id = "0123456789ab";
        let uri = format!("/containers/{id}/terminal/ws");
        // Disabled: no such route.
        let res = ws(test_app(), None, None, &uri).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        let app = terminal_app();
        // Enabled, no session: 401.
        let res = ws(app.clone(), None, None, &uri).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let cookie = cookie_of(&app.clone().oneshot(login_req(PASSWORD)).await.unwrap());
        // Bad or missing Origin: 403.
        let res = ws(app.clone(), Some(&cookie), Some("https://evil.test"), &uri)
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let res = ws(app.clone(), Some(&cookie), None, &uri).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        // Malformed id: 404.
        let res = ws(
            app.clone(),
            Some(&cookie),
            Some("http://panel.test"),
            "/containers/zz/terminal/ws",
        )
        .await
        .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        // Good origin but no re-auth grant yet: 403.
        let res = ws(app.clone(), Some(&cookie), Some("http://panel.test"), &uri)
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn os_update_run_needs_session_origin_helper_switch_and_password() {
        let dir = std::env::temp_dir().join(format!("dockwren-os-{}", auth::generate_password()));
        std::fs::create_dir_all(&dir).unwrap();
        let unit = dir.join("unit");
        let log = dir.join("log");
        let hash = auth::hash_password(PASSWORD);
        let app = app(
            Arc::new(auth::Auth::new(&hash, false).unwrap()),
            DockerClient::connect().unwrap(),
            apps::Apps::new(template::catalog(None).unwrap(), dir.join("none")),
            metrics::Shared::default(),
            Arc::new(std::sync::Mutex::new(notify::Store::memory())),
            Arc::new(terminal::Switch::new(false, None)),
            Arc::new(osupdate::Control::with_paths(&dir, &log, &unit)),
            Arc::new(selfupdate::Control::new(&std::env::temp_dir())),
        );
        let post = |cookie: Option<&str>, origin: &str, path: &str, body: &str| {
            let mut r = Request::post(path)
                .header(header::HOST, "panel.test")
                .header(header::ORIGIN, origin)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
            if let Some(c) = cookie {
                r = r.header(header::COOKIE, c);
            }
            app.clone()
                .oneshot(r.body(Body::from(body.to_string())).unwrap())
        };
        let good = format!("password={PASSWORD}");
        let run = "/host/os-updates/run";
        assert_eq!(
            post(None, "http://panel.test", run, &good)
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let cookie = cookie_of(&app.clone().oneshot(login_req(PASSWORD)).await.unwrap());
        let res = post(Some(&cookie), "http://evil.test", run, &good)
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        // Helper not set up: the page says how, nothing is written.
        let res = app
            .clone()
            .oneshot(
                Request::get("/settings/osupdates")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(body_of(res).await.contains("setup-osupdate"));
        let res = post(Some(&cookie), "http://panel.test", run, &good)
            .await
            .unwrap();
        assert!(body_of(res).await.contains("turned off"));
        assert!(!dir.join("osupdate.trigger").exists());
        // Helper present but switch off: still refused.
        std::fs::write(&unit, "").unwrap();
        let res = post(Some(&cookie), "http://panel.test", run, &good)
            .await
            .unwrap();
        assert!(body_of(res).await.contains("turned off"));
        // Turning it on needs the password.
        let tog = "/settings/osupdates";
        let res = post(
            Some(&cookie),
            "http://panel.test",
            tog,
            "on=1&password=wrong",
        )
        .await
        .unwrap();
        assert!(body_of(res).await.contains("Wrong password"));
        assert!(!dir.join("osupdate.enabled").exists());
        post(
            Some(&cookie),
            "http://panel.test",
            tog,
            &format!("on=1&{good}"),
        )
        .await
        .unwrap();
        assert!(dir.join("osupdate.enabled").exists());
        // Start: wrong password writes nothing, right password writes the trigger once.
        let res = post(Some(&cookie), "http://panel.test", run, "password=wrong")
            .await
            .unwrap();
        assert!(body_of(res).await.contains("Wrong password"));
        assert!(!dir.join("osupdate.trigger").exists());
        post(Some(&cookie), "http://panel.test", run, &good)
            .await
            .unwrap();
        assert!(dir.join("osupdate.trigger").exists());
        let res = post(Some(&cookie), "http://panel.test", run, &good)
            .await
            .unwrap();
        assert!(body_of(res).await.contains("already running"));
        // The log fragment follows the helper's log and stops polling when it ends.
        std::fs::remove_file(dir.join("osupdate.trigger")).unwrap();
        std::fs::write(&log, "== started x\nGet:1 <b>x</b>\n").unwrap();
        let get_log = || {
            app.clone().oneshot(
                Request::get("/host/os-updates/log")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
        };
        let html = body_of(get_log().await.unwrap()).await;
        assert!(html.contains("every 2s") && html.contains("Working"));
        assert!(html.contains("&#60;b&#62;") || html.contains("&lt;b&gt;"));
        std::fs::write(&log, "== started x\nE: boom\n== exit 100\n").unwrap();
        let html = body_of(get_log().await.unwrap()).await;
        assert!(!html.contains("every 2s") && html.contains("exit 100"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn terminal_setting_needs_session_origin_and_password() {
        let dir = std::env::temp_dir().join(format!("dockwren-tset-{}", auth::generate_password()));
        std::fs::create_dir_all(&dir).unwrap();
        let hash = auth::hash_password(PASSWORD);
        let sw = Arc::new(terminal::Switch::new(false, Some(&dir)));
        let app = app(
            Arc::new(auth::Auth::new(&hash, false).unwrap()),
            DockerClient::connect().unwrap(),
            apps::Apps::new(template::catalog(None).unwrap(), dir.join("none")),
            metrics::Shared::default(),
            Arc::new(std::sync::Mutex::new(notify::Store::memory())),
            sw.clone(),
            Arc::new(osupdate::Control::new(None)),
            Arc::new(selfupdate::Control::new(&std::env::temp_dir())),
        );
        let post = |cookie: Option<&str>, origin: &str, body: &str| {
            let mut r = Request::post("/settings/terminal")
                .header(header::HOST, "panel.test")
                .header(header::ORIGIN, origin)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
            if let Some(c) = cookie {
                r = r.header(header::COOKIE, c);
            }
            app.clone()
                .oneshot(r.body(Body::from(body.to_string())).unwrap())
        };
        let on = format!("on=1&password={PASSWORD}");
        assert_eq!(
            post(None, "http://panel.test", &on).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        let cookie = cookie_of(&app.clone().oneshot(login_req(PASSWORD)).await.unwrap());
        let res = post(Some(&cookie), "http://evil.test", &on).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let res = post(Some(&cookie), "http://panel.test", "on=1&password=wrong")
            .await
            .unwrap();
        assert!(body_of(res).await.contains("Wrong password"));
        assert!(!sw.is_on());
        let res = post(Some(&cookie), "http://panel.test", &on).await.unwrap();
        assert!(body_of(res).await.contains("Turn off"));
        assert!(sw.is_on());
        let off = format!("on=0&password={PASSWORD}");
        post(Some(&cookie), "http://panel.test", &off)
            .await
            .unwrap();
        assert!(!sw.is_on());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn terminal_grant_needs_password_and_refuses_tunnel_without_2fa() {
        let app = terminal_app();
        let cookie = cookie_of(&app.clone().oneshot(login_req(PASSWORD)).await.unwrap());
        let grant = |body: &str, tunnel: bool| {
            let mut r = Request::post("/containers/0123456789ab/terminal/grant")
                .header(header::HOST, "panel.test")
                .header(header::ORIGIN, "http://panel.test")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
            if tunnel {
                r = r.header("cf-connecting-ip", "203.0.113.9");
            }
            app.clone()
                .oneshot(r.body(Body::from(body.to_string())).unwrap())
        };
        let res = grant("password=wrong", false).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let res = grant(&format!("password={PASSWORD}"), true).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "tunnel without 2FA");
        let res = grant(&format!("password={PASSWORD}"), false).await.unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
    }

    async fn body_of(res: Response<Body>) -> String {
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn login_req(password: &str) -> Request<Body> {
        Request::post("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("password={password}")))
            .unwrap()
    }

    fn cookie_of(res: &Response<Body>) -> String {
        let set = res.headers()[header::SET_COOKIE].to_str().unwrap();
        set.split(';').next().unwrap().to_string()
    }

    #[tokio::test]
    async fn metrics_needs_a_session_then_renders() {
        let app = test_app();
        let res = app
            .clone()
            .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_ne!(res.status(), StatusCode::OK);
        let login = app.clone().oneshot(login_req(PASSWORD)).await.unwrap();
        let cookie = cookie_of(&login);
        let res = app
            .oneshot(
                Request::get("/metrics?range=24h")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(body_of(res).await.contains("id=\"metrics\""));
    }

    #[tokio::test]
    async fn assets_are_versioned_and_revalidated() {
        let v = asset_version();
        assert_eq!(v.len(), 8);
        let app = test_app();
        let login = body_of(
            app.clone()
                .oneshot(Request::get("/login").body(Body::empty()).unwrap())
                .await
                .unwrap(),
        )
        .await;
        assert!(login.contains(&format!("/static/dockwren.css?v={v}")));
        let css = app
            .oneshot(
                Request::get("/static/dockwren.css?v=x")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(css.headers()[header::CACHE_CONTROL], "no-cache");
    }

    #[tokio::test]
    async fn favicon_is_public_svg() {
        for path in ["/favicon.svg", "/favicon.ico"] {
            let res = test_app()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "{path}");
            assert_eq!(res.headers()[header::CONTENT_TYPE], "image/svg+xml");
        }
    }

    #[tokio::test]
    async fn health_is_public() {
        let res = test_app()
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_of(res).await, "ok");
    }

    #[tokio::test]
    async fn index_requires_session() {
        let res = test_app()
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(res.headers()[header::LOCATION], "/login");
    }

    #[tokio::test]
    async fn login_page_is_public() {
        let res = test_app()
            .oneshot(Request::get("/login").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(body_of(res).await.contains("Sign in"));
    }

    #[tokio::test]
    async fn wrong_password_is_rejected() {
        let res = test_app().oneshot(login_req("nope")).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(res.headers().get(header::SET_COOKIE).is_none());
    }

    #[tokio::test]
    async fn login_grants_access_and_logout_revokes_it() {
        let app = test_app();
        let res = app.clone().oneshot(login_req(PASSWORD)).await.unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let set = res.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_string();
        assert!(set.contains("HttpOnly") && set.contains("SameSite=Strict"));
        let cookie = cookie_of(&res);

        let res = app
            .clone()
            .oneshot(
                Request::get("/")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(body_of(res).await.contains("Dockwren"));

        let res = app
            .clone()
            .oneshot(
                Request::post("/logout")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);

        let res = app
            .oneshot(
                Request::get("/")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn forged_session_is_rejected() {
        let res = test_app()
            .oneshot(
                Request::get("/")
                    .header(header::COOKIE, format!("{}=deadbeef", auth::COOKIE_NAME))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn repeated_failures_lock_out_even_the_right_password() {
        let app = test_app();
        for _ in 0..5 {
            let res = app.clone().oneshot(login_req("bad")).await.unwrap();
            assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        }
        let res = app.oneshot(login_req(PASSWORD)).await.unwrap();
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn containers_table_lists_rows_and_escapes() {
        let html = render_containers(
            Ok(vec![ContainerInfo {
                id: "abc".into(),
                name: "<web>".into(),
                image: "nginx:1".into(),
                state: "running".into(),
                status: "Up 3 hours".into(),
                managed: true,
                oom_killed: false,
                ports: vec![],
                host_network: false,
            }]),
            true,
        );
        assert!(html.contains("&#60;web&#62;") && !html.contains("<web>"));
        assert!(html.contains("nginx:1") && html.contains("Up 3 hours"));
        assert!(html.contains("/containers/abc/terminal"));
    }

    #[test]
    fn logs_panel_escapes_output() {
        let page = LogsTemplate {
            id: "0123456789ab".into(),
            text: "<script>alert(1)</script>".into(),
            error: None,
            q: "\"><b>".into(),
            level: "all".into(),
            since: "all".into(),
            matched: 0,
            filtered: false,
            part: false,
        };
        let html = page.render().unwrap();
        assert!(!html.contains("<script>") && html.contains("&#60;script&#62;"));
        assert!(!html.contains("<b>"), "the search text is escaped too");
    }

    #[tokio::test]
    async fn logs_need_session_and_valid_id() {
        let app = test_app();
        let res = app
            .clone()
            .oneshot(
                Request::get("/containers/0123456789ab/logs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let cookie = cookie_of(&app.clone().oneshot(login_req(PASSWORD)).await.unwrap());
        let res = app
            .oneshot(
                Request::get("/containers/web/logs")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn same_origin_check() {
        let mut h = HeaderMap::new();
        assert!(same_origin(&h)); // no Origin header
        h.insert(header::HOST, "panel.example".parse().unwrap());
        h.insert(header::ORIGIN, "https://panel.example".parse().unwrap());
        assert!(same_origin(&h));
        h.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert!(!same_origin(&h));
    }

    #[tokio::test]
    async fn actions_need_session_and_valid_input() {
        let app = test_app();
        let anon = app
            .clone()
            .oneshot(
                Request::post("/containers/0123456789ab/stop")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(anon.status(), StatusCode::SEE_OTHER);

        let cookie = cookie_of(&app.clone().oneshot(login_req(PASSWORD)).await.unwrap());
        let post = |uri: &str, origin: Option<&str>| {
            let mut r = Request::post(uri)
                .header(header::COOKIE, &cookie)
                .header(header::HOST, "p.example");
            if let Some(o) = origin {
                r = r.header(header::ORIGIN, o);
            }
            r.body(Body::empty()).unwrap()
        };
        let res = app
            .clone()
            .oneshot(post(
                "/containers/0123456789ab/stop",
                Some("https://evil.example"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let res = app
            .clone()
            .oneshot(post("/containers/0123456789ab/delete", None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let res = app
            .oneshot(post("/containers/web/stop", None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn containers_empty_and_error_states() {
        assert!(render_containers(Ok(vec![]), false).contains("No containers yet"));
        let err = render_containers(Err("boom".into()), false);
        assert!(err.contains("Docker is not reachable: boom"));
    }

    #[tokio::test]
    async fn status_api_needs_a_valid_token_and_ignores_sessions() {
        let dir = std::env::temp_dir().join(format!("dockwren-api-{}", auth::generate_password()));
        let (_, _) = auth::load_or_create(&dir).unwrap();
        let token = statusapi::create(&dir).unwrap();
        let build = || {
            app(
                Arc::new(auth::Auth::open(&dir, false).unwrap()),
                DockerClient::connect().unwrap(),
                apps::Apps::new(template::catalog(None).unwrap(), dir.join("instances")),
                metrics::Shared::default(),
                Arc::new(std::sync::Mutex::new(notify::Store::memory())),
                Arc::new(terminal::Switch::new(false, None)),
                Arc::new(osupdate::Control::new(None)),
                Arc::new(selfupdate::Control::new(&std::env::temp_dir())),
            )
        };
        for auth_header in [None, Some("Bearer dwst_wrong"), Some("Basic abc")] {
            let mut r = Request::get("/api/status");
            if let Some(h) = auth_header {
                r = r.header(header::AUTHORIZATION, h);
            }
            let res = build()
                .oneshot(r.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{auth_header:?}");
        }
        let res = build()
            .oneshot(
                Request::get("/api/status")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert!(v["apps"].as_array().unwrap().is_empty());
        assert!(v["last_backup_at"].is_null());
        let res = build()
            .oneshot(Request::get("/api/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let res = build()
            .oneshot(
                Request::get("/api/metrics")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            res.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/plain")
        );
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("dockwren_apps 0"));
        // the token does not open the panel
        let res = build()
            .oneshot(
                Request::get("/containers/list")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn containers_and_assets_require_session() {
        for path in [
            "/containers",
            "/containers/list",
            "/overview",
            "/containers/web/graphs",
            "/resources",
            "/settings",
            "/static/htmx.min.js",
        ] {
            let res = test_app()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::SEE_OTHER, "{path}");
        }
    }

    #[tokio::test]
    async fn every_page_renders_with_its_nav_entry_active() {
        let app = test_app();
        let cookie = cookie_of(&app.clone().oneshot(login_req(PASSWORD)).await.unwrap());
        for (path, title) in [
            ("/", "Overview"),
            ("/apps", "Apps"),
            ("/containers", "Containers"),
            ("/resources", "Resources"),
            ("/settings", "Settings"),
        ] {
            let req = Request::get(path)
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap();
            let res = app.clone().oneshot(req).await.unwrap();
            assert_eq!(res.status(), StatusCode::OK, "{path}");
            let html = body_of(res).await;
            assert!(html.contains(&format!("<h1>{title}</h1>")), "{path}");
            // Settings also marks its active tab.
            let expected = if path == "/settings" { 2 } else { 1 };
            assert_eq!(
                html.matches("aria-current=\"page\"").count(),
                expected,
                "{path}"
            );
            assert!(
                html.contains(&format!("href=\"{path}\" aria-current")),
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn settings_tabs_have_their_own_urls_and_only_their_own_sections() {
        let app = test_app();
        let cookie = cookie_of(&app.clone().oneshot(login_req(PASSWORD)).await.unwrap());
        let get = |path: &'static str| {
            let app = app.clone();
            let cookie = cookie.clone();
            async move {
                let req = Request::get(path)
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap();
                app.oneshot(req).await.unwrap()
            }
        };
        for (tab, has, hasnt) in [
            ("general", "<h2>About</h2>", "/settings/2fa"),
            ("notifications", "/settings/channels", "<h2>About</h2>"),
            ("security", "/settings/terminal", "/settings/osupdates"),
            ("system", "/settings/osupdates", "/settings/2fa"),
        ] {
            let path: &'static str = Box::leak(format!("/settings/tab/{tab}").into_boxed_str());
            let res = get(path).await;
            assert_eq!(res.status(), StatusCode::OK, "{tab}");
            let html = body_of(res).await;
            assert!(html.contains(has), "{tab} should contain {has}");
            if tab == "system" {
                assert!(html.contains("What updates by itself"));
            }
            assert!(!html.contains(hasnt), "{tab} should not load {hasnt}");
            assert!(
                html.contains(&format!("href=\"{path}\" aria-current")),
                "{tab}"
            );
        }
        assert_eq!(
            get("/settings/tab/nope").await.status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn backup_schedule_form_validates_and_saves() {
        let app = test_app();
        let cookie = cookie_of(&app.clone().oneshot(login_req(PASSWORD)).await.unwrap());
        let post = |body: &'static str| {
            let app = app.clone();
            let cookie = cookie.clone();
            async move {
                let req = Request::post("/settings/backups")
                    .header(header::COOKIE, cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap();
                body_of(app.oneshot(req).await.unwrap()).await
            }
        };
        let bad = post("mode=hourly&hour=3&keep=7").await;
        assert!(bad.contains("Pick off, daily or weekly"), "{bad}");
        let ok = post("mode=daily&hour=4&keep=5").await;
        assert!(ok.contains("Saved."), "{ok}");
        assert!(
            ok.contains("value=\"4\"") && ok.contains("value=\"5\""),
            "{ok}"
        );
        assert!(ok.contains("<option value=\"daily\" selected>"), "{ok}");
        assert!(
            !ok.contains("checked"),
            "auto-update is off by default: {ok}"
        );
        let on = post("mode=daily&hour=4&keep=5&auto_update=1").await;
        assert!(on.contains("type=\"checkbox\" value=\"1\" checked"), "{on}");
    }

    #[tokio::test]
    async fn offsite_form_validates_and_turns_off_without_docker() {
        let app = test_app();
        let cookie = cookie_of(&app.clone().oneshot(login_req(PASSWORD)).await.unwrap());
        let post = |body: &'static str| {
            let app = app.clone();
            let cookie = cookie.clone();
            async move {
                let req = Request::post("/settings/offsite")
                    .header(header::COOKIE, cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap();
                body_of(app.oneshot(req).await.unwrap()).await
            }
        };
        let bad = post("kind=s3&endpoint=ftp%3A%2F%2Fx&bucket=b").await;
        assert!(bad.contains("must be a URL"), "{bad}");
        let bad = post("kind=sftp&host=h&user=u&path=..%2Fx&private_key=k").await;
        assert!(bad.contains("folder may only"), "{bad}");
        let off = post("kind=off").await;
        assert!(off.contains("Off-host copies are off"), "{off}");
        assert!(!off.contains("Repository password (shown once)"), "{off}");
    }

    #[tokio::test]
    async fn config_export_download_needs_the_password_and_a_long_passphrase() {
        let root = std::env::temp_dir().join(format!("dockwren-cfgdl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let inst = root.join("gt");
        std::fs::create_dir_all(&inst).unwrap();
        std::fs::write(inst.join("state.toml"), "template = \"x\"\nversion = 1\n").unwrap();
        std::fs::write(inst.join("compose.yaml"), "services: {}\n").unwrap();
        std::fs::write(inst.join(".env"), "DB_PASSWORD='s3cret-value'\n").unwrap();
        let auth: SharedAuth =
            Arc::new(auth::Auth::new(&auth::hash_password(PASSWORD), false).unwrap());
        let apps =
            apps::Apps::new(template::catalog(None).unwrap(), root.clone()).with_auth(auth.clone());
        let app = app(
            auth,
            DockerClient::connect().unwrap(),
            apps,
            metrics::Shared::default(),
            Arc::new(std::sync::Mutex::new(notify::Store::memory())),
            Arc::new(terminal::Switch::new(false, None)),
            Arc::new(osupdate::Control::new(None)),
            Arc::new(selfupdate::Control::new(&std::env::temp_dir())),
        );
        let cookie = cookie_of(&app.clone().oneshot(login_req(PASSWORD)).await.unwrap());
        let post = |body: &'static str| {
            let app = app.clone();
            let cookie = cookie.clone();
            async move {
                let req = Request::post("/settings/config-export")
                    .header(header::COOKIE, cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap();
                app.oneshot(req).await.unwrap()
            }
        };
        let res = post("password=nope&passphrase=a+long+enough+passphrase").await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(body_of(res).await.contains("Wrong password"));
        let res = post("password=correct+horse+battery&passphrase=short").await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let res = post("password=correct+horse+battery&passphrase=a+long+enough+passphrase").await;
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            res.headers()[header::CONTENT_DISPOSITION]
                .to_str()
                .unwrap()
                .contains("attachment")
        );
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("s3cret-value"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn containers_fragment_renders_for_signed_in_user() {
        let app = test_app();
        let res = app.clone().oneshot(login_req(PASSWORD)).await.unwrap();
        let cookie = cookie_of(&res);
        let res = app
            .oneshot(
                Request::get("/containers/list")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[test]
    fn first_run_creates_private_hash_file_and_reuses_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("dockwren-test-{}", auth::generate_password()));
        let (hash, pw) = auth::load_or_create(&dir).unwrap();
        let pw = pw.expect("password shown on first run");
        assert_eq!(pw.len(), 24);
        assert!(hash.starts_with("$argon2id$"));
        let mode = std::fs::metadata(dir.join("admin.hash"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let (hash2, pw2) = auth::load_or_create(&dir).unwrap();
        assert_eq!(hash, hash2);
        assert!(pw2.is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[tokio::test]
    async fn self_update_needs_session_origin_setup_and_password() {
        let dir = std::env::temp_dir().join(format!("dockwren-su-{}", auth::generate_password()));
        std::fs::create_dir_all(&dir).unwrap();
        let dropin = dir.join("dropin");
        let hash = auth::hash_password(PASSWORD);
        let app = app(
            Arc::new(auth::Auth::new(&hash, false).unwrap()),
            DockerClient::connect().unwrap(),
            apps::Apps::new(template::catalog(None).unwrap(), dir.join("none")),
            metrics::Shared::default(),
            Arc::new(std::sync::Mutex::new(notify::Store::memory())),
            Arc::new(terminal::Switch::new(false, None)),
            Arc::new(osupdate::Control::new(None)),
            Arc::new(selfupdate::Control::for_test(&dir, &dropin)),
        );
        let post = |cookie: Option<&str>, origin: &str, body: &str| {
            let mut r = Request::post("/settings/selfupdate/run")
                .header(header::HOST, "panel.test")
                .header(header::ORIGIN, origin)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
            if let Some(c) = cookie {
                r = r.header(header::COOKIE, c);
            }
            app.clone()
                .oneshot(r.body(Body::from(body.to_string())).unwrap())
        };
        let good = format!("password={PASSWORD}");
        let res = post(None, "http://panel.test", &good).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let cookie = cookie_of(&app.clone().oneshot(login_req(PASSWORD)).await.unwrap());
        let res = post(Some(&cookie), "http://evil.test", &good)
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        // Not set up (no drop-in): refused, nothing written.
        let res = post(Some(&cookie), "http://panel.test", &good)
            .await
            .unwrap();
        assert!(body_of(res).await.contains("self-update"));
        assert!(!dir.join("selfupdate.requested").exists());
        // Set up, wrong password: refused.
        std::fs::write(&dropin, "").unwrap();
        let res = post(Some(&cookie), "http://panel.test", "password=wrong")
            .await
            .unwrap();
        assert!(body_of(res).await.contains("Wrong password"));
        assert!(!dir.join("selfupdate.requested").exists());
        // Right password: the request file appears.
        let res = post(Some(&cookie), "http://panel.test", &good)
            .await
            .unwrap();
        assert!(body_of(res).await.contains("Updating"));
        assert!(dir.join("selfupdate.requested").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn app_routes_need_a_session() {
        for (method, uri) in [
            ("GET", "/apps"),
            ("GET", "/catalog"),
            ("GET", "/apps/gitea/new"),
            ("GET", "/apps/check-host?PUBLIC_HOST=x.example.com"),
            ("POST", "/apps/gitea/install"),
            ("GET", "/stacks/new"),
            ("POST", "/stacks/install"),
            ("GET", "/stacks/adopt"),
            ("POST", "/stacks/adopt"),
            ("GET", "/jobs/x"),
            ("GET", "/instances/list"),
            ("POST", "/instances/gt/remove"),
            ("POST", "/instances/gt/update"),
            ("GET", "/instances/gt/public"),
            ("POST", "/instances/gt/public"),
            ("POST", "/instances/gt/public/publish"),
            ("POST", "/instances/gt/public/direct"),
            ("GET", "/notifications"),
            ("GET", "/notifications/list"),
            ("GET", "/notifications/count"),
            ("POST", "/notifications/dismiss-all"),
            ("POST", "/notifications/1/dismiss"),
            ("GET", "/cloudflare"),
            ("GET", "/settings/channels"),
            ("POST", "/settings/channels"),
            ("POST", "/settings/channels/test"),
            ("POST", "/settings/channels/ntfy/remove"),
            ("GET", "/settings/2fa"),
            ("POST", "/settings/2fa/start"),
            ("POST", "/settings/2fa/enable"),
            ("POST", "/settings/2fa/disable"),
            ("GET", "/settings/cloudflare"),
            ("POST", "/settings/cloudflare/connect"),
            ("POST", "/settings/cloudflare/tunnel"),
            ("POST", "/settings/cloudflare/disconnect"),
            ("GET", "/instances/gt/settings"),
            ("POST", "/instances/gt/settings"),
            ("GET", "/instances/gt/limits"),
            ("POST", "/instances/gt/limits"),
            ("POST", "/instances/gt/logcap"),
            ("GET", "/instances/gt/restore"),
            ("POST", "/instances/gt/restore"),
            ("POST", "/instances/gt/verify"),
            ("POST", "/instances/gt/trim-snapshots"),
            ("GET", "/settings/disk"),
            ("POST", "/settings/disk/prune-images"),
            ("POST", "/instances/gt/pull"),
            ("POST", "/instances/gt/restore-test"),
            ("POST", "/instances/gt/cleanup"),
        ] {
            let res = test_app()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::SEE_OTHER, "{method} {uri}");
        }
    }

    #[tokio::test]
    async fn apps_ui_renders_and_rejects_bad_input() {
        let app = test_app();
        let cookie = cookie_of(&app.clone().oneshot(login_req(PASSWORD)).await.unwrap());
        let get = |uri: &str| {
            Request::get(uri)
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap()
        };
        let post = |uri: &str, origin: Option<&str>, body: &str| {
            let mut r = Request::post(uri)
                .header(header::COOKIE, &cookie)
                .header(header::HOST, "p.example")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
            if let Some(o) = origin {
                r = r.header(header::ORIGIN, o);
            }
            r.body(Body::from(body.to_string())).unwrap()
        };

        let res = app.clone().oneshot(get("/catalog")).await.unwrap();
        let html = body_of(res).await;
        assert!(html.contains("Gitea") && html.contains("Vaultwarden"));

        let res = app.clone().oneshot(get("/apps/gitea/new")).await.unwrap();
        let html = body_of(res).await;
        assert!(
            html.contains("name=\"ADMIN_EMAIL\"")
                && html.contains("hx-post=\"/apps/gitea/install\"")
        );
        // Secrets are generated, never asked for in the form.
        assert!(!html.contains("POSTGRES_PASSWORD"));

        let res = app.clone().oneshot(get("/apps/nope/new")).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let res = app.clone().oneshot(get("/jobs/unknown")).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // Cross-origin install is refused before anything is created.
        let res = app
            .clone()
            .oneshot(post(
                "/apps/gitea/install",
                Some("https://evil.example"),
                "ADMIN_EMAIL=a@b.io",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // Invalid input comes back as the form with an error; nothing is created.
        let res = app
            .clone()
            .oneshot(post(
                "/apps/gitea/install",
                None,
                "ADMIN_EMAIL=not-an-email",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_of(res).await;
        assert!(html.contains("role=\"alert\"") && html.contains("e-mail"));
        assert!(html.contains("not-an-email"), "entered values are kept");
        assert!(!std::env::temp_dir().join("dockwren-test-none").exists());

        let res = app.clone().oneshot(get("/instances/list")).await.unwrap();
        assert!(body_of(res).await.contains("No apps installed"));
        let res = app
            .clone()
            .oneshot(post("/instances/..%2Fx/remove", None, ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let res = app
            .clone()
            .oneshot(post("/instances/ghost/remove", None, ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let res = app
            .clone()
            .oneshot(get("/instances/ghost/restore"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let res = app
            .oneshot(post("/instances/ghost/restore", None, "snapshot=1"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
}
