mod apps;
mod auth;
mod backup;
mod config;
mod docker;
mod install;
mod metrics;
mod public;
mod runner;
mod template;
mod update;

use std::sync::Arc;

use askama::Template;
use auth::SharedAuth;
use axum::{
    Router,
    extract::{FromRef, Path, State},
    http::{HeaderMap, StatusCode, header},
    middleware::from_fn_with_state,
    response::{Html, IntoResponse},
    routing::{get, post},
};
use docker::{Action, ContainerInfo, DockerClient};

#[derive(Clone)]
struct AppState {
    auth: SharedAuth,
    docker: DockerClient,
    apps: apps::Apps,
    metrics: metrics::Shared,
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
}

fn render_containers(result: Result<Vec<ContainerInfo>, String>) -> String {
    let page = match result {
        Ok(containers) => ContainersTemplate {
            containers,
            error: None,
        },
        Err(e) => ContainersTemplate {
            containers: Vec::new(),
            error: Some(format!("Docker is not reachable: {e}")),
        },
    };
    page.render().expect("containers template renders")
}

async fn containers(State(state): State<AppState>) -> Html<String> {
    let result = state.docker.containers().await.map_err(|e| e.to_string());
    Html(render_containers(result))
}

#[derive(Template)]
#[template(path = "logs.html")]
struct LogsTemplate {
    id: String,
    text: String,
    error: Option<String>,
}

async fn container_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Html<String>, StatusCode> {
    if !docker::valid_id(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let (text, error) = match state.docker.logs(&id, LOG_LINES).await {
        Ok(text) => (text, None),
        Err(e) => (String::new(), Some(format!("Cannot read logs: {e}"))),
    };
    let page = LogsTemplate { id, text, error };
    Ok(Html(page.render().expect("logs template renders")))
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
        return Ok(Html(render_containers(Err(e.to_string()))));
    }
    Ok(containers(State(state)).await)
}

const HTMX: &str = include_str!("../static/htmx.min.js");
const CSS: &str = include_str!("../static/dockwren.css");
const FAVICON: &str = include_str!("../static/favicon.svg");

/// Short content hash of the bundled assets, appended to their URLs so a new build is never served from a stale cache.
pub fn asset_version() -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in CSS.bytes().chain(HTMX.bytes()) {
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
    nav: &'static [(&'static str, &'static str, &'static str)],
    version: &'static str,
    assets: String,
}

/// (href, key, label) of the sidebar entries.
const NAV: [(&str, &str, &str); 6] = [
    ("/", "overview", "Overview"),
    ("/apps", "apps", "Apps"),
    ("/containers", "containers", "Containers"),
    ("/resources", "resources", "Resources"),
    ("/cloudflare", "cloudflare", "Cloudflare"),
    ("/settings", "settings", "Settings"),
];

fn page(active: &'static str) -> Html<String> {
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
async fn settings_page() -> Html<String> {
    page("settings")
}

fn app(
    auth: SharedAuth,
    docker: DockerClient,
    apps: apps::Apps,
    metrics: metrics::Shared,
) -> Router {
    let protected = Router::new()
        .route("/", get(index))
        .route("/metrics", get(metrics::page))
        .route("/apps", get(apps_page))
        .route("/containers", get(containers_page))
        .route("/resources", get(resources_page))
        .route("/settings", get(settings_page))
        .route("/cloudflare", get(cloudflare_page))
        .route("/overview", get(overview))
        .route("/containers/list", get(containers))
        .route("/containers/{id}/{action}", post(container_action))
        .route("/containers/{id}/logs", get(container_logs))
        .route("/containers/{name}/graphs", get(metrics::container_graphs))
        .route("/logs/close", get(close_logs))
        .route("/catalog", get(apps::list))
        .route("/apps/{id}/icon.svg", get(apps::icon))
        .route("/apps/check-host", get(apps::check_host))
        .route("/apps/{id}/new", get(apps::new_form))
        .route("/apps/{id}/install", post(apps::install_submit))
        .route("/jobs/{id}", get(apps::job))
        .route("/instances/list", get(apps::instances))
        .route("/instances/{name}/remove", post(apps::instance_remove))
        .route("/instances/{name}/update", post(apps::instance_update))
        .route(
            "/instances/{name}/public",
            get(apps::public_form).post(apps::public_submit),
        )
        .route(
            "/instances/{name}/public/publish",
            post(apps::public_publish),
        )
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
        .route("/instances/{name}/cleanup", post(apps::cleanup_submit))
        .route("/static/htmx.min.js", get(htmx_js))
        .route("/logout", post(auth::logout))
        .route_layer(from_fn_with_state(auth.clone(), auth::require_session));
    Router::new()
        .route("/health", get(health))
        .route("/static/dockwren.css", get(stylesheet))
        .route("/favicon.svg", get(favicon))
        .route("/favicon.ico", get(favicon))
        .route("/login", get(auth::login_form).post(auth::login_submit))
        .merge(protected)
        .with_state(AppState {
            auth,
            docker,
            apps,
            metrics,
        })
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

const USAGE: &str = "usage: dockwren [serve | set-password | install <app> [KEY=VALUE...] | backup <instance> | update <instance> | restore <instance> [snapshot | --list] | cleanup <instance> | expose <instance> [hostname] [--access EMAIL] [--yes] | unexpose <instance> [--yes] | remove <instance> | --version]";

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
        None | Some("serve") | Some("set-password") => {}
        Some(
            "install" | "backup" | "update" | "restore" | "cleanup" | "expose" | "unexpose"
            | "remove",
        ) => check_data_dir(&cfg.data_dir),
        Some(_) => {}
    }
    match cmd.as_deref() {
        None | Some("serve") => {}
        Some("set-password") => {
            let password = auth::set_password(&cfg.data_dir).unwrap_or_else(|e| {
                panic!("cannot set password in {}: {e}", cfg.data_dir.display())
            });
            println!("New admin password (shown once, not stored):");
            println!("    {password}");
            println!("A running server applies it immediately and signs all sessions out.");
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
            let name = std::env::args().nth(2).unwrap_or_default();
            if !runner::valid_name(&name) {
                eprintln!("usage: dockwren backup <instance>");
                std::process::exit(2);
            }
            let result = async {
                let templates = apps::catalog(&cfg)?;
                backup::snapshot_instance(&cfg, &templates, &name, &mut |s| println!("  {s}")).await
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
    let apps = apps::Apps::new(templates, cfg.instances_dir());
    let metrics = metrics::Shared::default();
    let mut disk_paths = vec![cfg.data_dir.display().to_string()];
    disk_paths.extend(cfg.disk_paths.iter().cloned());
    metrics::spawn(
        metrics.clone(),
        docker.clone(),
        disk_paths,
        cfg.data_dir.clone(),
    );
    axum::serve(listener, app(auth, docker, apps, metrics))
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
        )
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
        let html = render_containers(Ok(vec![ContainerInfo {
            id: "abc".into(),
            name: "<web>".into(),
            image: "nginx:1".into(),
            state: "running".into(),
            status: "Up 3 hours".into(),
        }]));
        assert!(html.contains("&#60;web&#62;") && !html.contains("<web>"));
        assert!(html.contains("nginx:1") && html.contains("Up 3 hours"));
    }

    #[test]
    fn logs_panel_escapes_output() {
        let page = LogsTemplate {
            id: "0123456789ab".into(),
            text: "<script>alert(1)</script>".into(),
            error: None,
        };
        let html = page.render().unwrap();
        assert!(!html.contains("<script>") && html.contains("&#60;script&#62;"));
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
        assert!(render_containers(Ok(vec![])).contains("No containers yet"));
        let err = render_containers(Err("boom".into()));
        assert!(err.contains("Docker is not reachable: boom"));
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
            assert_eq!(html.matches("aria-current=\"page\"").count(), 1, "{path}");
            assert!(
                html.contains(&format!("href=\"{path}\" aria-current")),
                "{path}"
            );
        }
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
    async fn app_routes_need_a_session() {
        for (method, uri) in [
            ("GET", "/apps"),
            ("GET", "/catalog"),
            ("GET", "/apps/gitea/new"),
            ("GET", "/apps/check-host?PUBLIC_HOST=x.example.com"),
            ("POST", "/apps/gitea/install"),
            ("GET", "/jobs/x"),
            ("GET", "/instances/list"),
            ("POST", "/instances/gt/remove"),
            ("POST", "/instances/gt/update"),
            ("GET", "/instances/gt/public"),
            ("POST", "/instances/gt/public"),
            ("POST", "/instances/gt/public/publish"),
            ("GET", "/cloudflare"),
            ("GET", "/settings/cloudflare"),
            ("POST", "/settings/cloudflare/connect"),
            ("POST", "/settings/cloudflare/tunnel"),
            ("POST", "/settings/cloudflare/disconnect"),
            ("GET", "/instances/gt/restore"),
            ("POST", "/instances/gt/restore"),
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
