mod auth;
mod config;
mod docker;
mod install;
mod runner;
mod template;

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
fn same_origin(headers: &HeaderMap) -> bool {
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

async fn htmx_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/javascript")], HTMX)
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    version: &'static str,
}

async fn health() -> &'static str {
    "ok"
}

async fn index() -> Html<String> {
    let page = IndexTemplate {
        version: env!("CARGO_PKG_VERSION"),
    };
    Html(page.render().expect("index template renders"))
}

fn app(auth: SharedAuth, docker: DockerClient) -> Router {
    let protected = Router::new()
        .route("/", get(index))
        .route("/containers", get(containers))
        .route("/containers/{id}/{action}", post(container_action))
        .route("/containers/{id}/logs", get(container_logs))
        .route("/logs/close", get(close_logs))
        .route("/static/htmx.min.js", get(htmx_js))
        .route("/logout", post(auth::logout))
        .route_layer(from_fn_with_state(auth.clone(), auth::require_session));
    Router::new()
        .route("/health", get(health))
        .route("/login", get(auth::login_form).post(auth::login_submit))
        .merge(protected)
        .with_state(AppState { auth, docker })
}

/// `dockwren install <app> [KEY=VALUE...]`: resolve, prepare, bring up.
async fn cli_install(cfg: &config::Config, args: &[String]) -> Result<(), String> {
    let app = args.first().ok_or("usage: dockwren install <app> [KEY=VALUE...]")?;
    let templates = template::load_all(&cfg.apps_dir())?;
    let t = templates
        .iter()
        .find(|t| &t.manifest.app.id == app)
        .ok_or_else(|| format!("unknown app {app}"))?;
    let input = args[1..]
        .iter()
        .map(|a| a.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())).ok_or(format!("expected KEY=VALUE, got {a}")))
        .collect::<Result<std::collections::HashMap<_, _>, _>>()?;
    let root = cfg.instances_dir();
    let claimed = runner::claimed_ports(&root);
    let resolved = install::resolve(t, &input, &claimed, &install::port_is_free)?;
    let inst = runner::prepare(&root, t, &resolved.env)?;
    println!("instance {} prepared in {}", inst.name, inst.dir.display());
    runner::bring_up(&inst, &mut |step| println!("  {step}")).await.map_err(|e| {
        format!("{e}\n(instance kept in {}; `dockwren remove {}` stops it)", inst.dir.display(), inst.name)
    })?;
    let captured = runner::run_post_start(&inst, t, &resolved.env, &mut |step| println!("  {step}")).await?;
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

#[tokio::main]
async fn main() {
    let cfg = config::Config::from_env();
    match std::env::args().nth(1).as_deref() {
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
        Some("remove") => {
            let name = std::env::args().nth(2).unwrap_or_default();
            if !runner::valid_name(&name) {
                eprintln!("usage: dockwren remove <instance>");
                std::process::exit(2);
            }
            let inst = runner::Instance { dir: cfg.instances_dir().join(&name), name };
            match runner::take_down(&inst).await {
                Ok(()) => println!("{} stopped and containers removed (data kept in {})", inst.name, inst.dir.display()),
                Err(e) => {
                    eprintln!("remove failed: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some(other) => {
            eprintln!("unknown command '{other}'\nusage: dockwren [serve | set-password | install <app> [KEY=VALUE...] | remove <instance>]");
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
    axum::serve(listener, app(auth, docker))
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
        app(Arc::new(auth::Auth::new(&hash, false).unwrap()), docker)
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
        for path in ["/containers", "/static/htmx.min.js"] {
            let res = test_app()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::SEE_OTHER, "{path}");
        }
    }

    #[tokio::test]
    async fn containers_fragment_renders_for_signed_in_user() {
        let app = test_app();
        let res = app.clone().oneshot(login_req(PASSWORD)).await.unwrap();
        let cookie = cookie_of(&res);
        let res = app
            .oneshot(
                Request::get("/containers")
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
}
