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
async fn pwa_files_are_public() {
    for (path, ct) in [
        ("/manifest.webmanifest", "application/manifest+json"),
        ("/static/icon-192.png", "image/png"),
        ("/static/icon-512.png", "image/png"),
        ("/apple-touch-icon.png", "image/png"),
    ] {
        let res = test_app()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{path}");
        assert_eq!(res.headers()[header::CONTENT_TYPE], ct, "{path}");
    }
}

#[tokio::test]
async fn security_headers_on_every_response() {
    let get = |uri: &'static str, proto: Option<&'static str>| {
        let mut r = Request::get(uri);
        if let Some(p) = proto {
            r = r.header("x-forwarded-proto", p);
        }
        test_app().oneshot(r.body(Body::empty()).unwrap())
    };
    // Public page, a redirect and a 404 all carry the headers.
    for uri in ["/login", "/", "/no-such-page"] {
        let res = get(uri, None).await.unwrap();
        let h = res.headers();
        assert_eq!(h[header::X_FRAME_OPTIONS], "DENY", "{uri}");
        assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff", "{uri}");
        assert_eq!(h[header::REFERRER_POLICY], "same-origin", "{uri}");
        assert!(
            h["permissions-policy"]
                .to_str()
                .unwrap()
                .contains("camera=()"),
            "{uri}"
        );
        let csp = h[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
        assert!(
            csp.contains("frame-ancestors 'none'") && csp.contains("object-src 'none'"),
            "{uri}"
        );
        assert!(
            csp.contains("script-src 'self';"),
            "{uri}: scripts must not allow inline"
        );
        assert!(
            !h.contains_key(header::STRICT_TRANSPORT_SECURITY),
            "{uri}: no HSTS over plain HTTP"
        );
    }
    // HSTS only behind an https proxy.
    let res = get("/login", Some("https")).await.unwrap();
    assert!(
        res.headers()
            .contains_key(header::STRICT_TRANSPORT_SECURITY)
    );
    let res = get("/login", Some("http")).await.unwrap();
    assert!(
        !res.headers()
            .contains_key(header::STRICT_TRANSPORT_SECURITY)
    );
}

/// The CSP forbids inline scripts, so no template (or HTML built in Rust) may use them.
#[test]
fn no_inline_scripts_or_handlers() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir.join("templates"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.extend(
        std::fs::read_dir(dir.join("src"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "rs")),
    );
    let handler = regex_lite_on_attr;
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        // Lines of test code legitimately mention these strings as hostile input.
        let body = text.split("#[cfg(test)]").next().unwrap();
        assert!(
            !handler(body),
            "{}: inline event handler (use a data- attribute and static/confirm.js)",
            f.display()
        );
        for part in body.split("<script").skip(1) {
            assert!(
                part.trim_start().starts_with("src=")
                    || part.contains("src=") && part.find("src=") < part.find('>'),
                "{}: inline <script> body is blocked by the CSP",
                f.display()
            );
        }
    }
}

/// True when the text has an ` onxxx=` attribute (plain or backslash-escaped quote).
fn regex_lite_on_attr(text: &str) -> bool {
    text.match_indices(" on").any(|(i, _)| {
        let rest = &text[i + 3..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_lowercase())
            .collect();
        !name.is_empty() && rest[name.len()..].starts_with('=')
    })
}

#[tokio::test]
async fn handler_csp_is_not_overridden() {
    let app = Router::new()
        .route(
            "/x",
            get(|| async {
                (
                    [(header::CONTENT_SECURITY_POLICY, "default-src 'none'")],
                    "ok",
                )
            }),
        )
        .layer(middleware::from_fn(security_headers));
    let res = app
        .oneshot(Request::get("/x").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(
        res.headers()[header::CONTENT_SECURITY_POLICY],
        "default-src 'none'"
    );
    assert_eq!(res.headers()[header::X_FRAME_OPTIONS], "DENY");
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
        // Settings and Apps also mark their active tab.
        let expected = if path == "/settings" || path == "/apps" {
            2
        } else {
            1
        };
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
async fn apps_tabs_have_their_own_urls() {
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
    let installed = body_of(get("/apps/tab/installed").await).await;
    assert!(installed.contains("/instances/list") && !installed.contains("hx-get=\"/catalog\""));
    assert!(installed.contains("href=\"/apps/tab/installed\" aria-current"));
    let add = body_of(get("/apps/tab/add").await).await;
    assert!(add.contains("hx-get=\"/catalog\"") && !add.contains("/instances/list"));
    assert!(add.contains("href=\"/apps/tab/add\" aria-current"));
    assert!(add.contains("<section id=\"install\">"));
    assert_eq!(get("/apps/tab/nope").await.status(), StatusCode::NOT_FOUND);
    // Nothing installed in the test panel: /apps opens the catalog.
    assert!(
        body_of(get("/apps").await)
            .await
            .contains("hx-get=\"/catalog\"")
    );
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
        ("backups", "/settings/offsite", "/settings/osupdates"),
        ("updates", "/settings/osupdates", "/settings/offsite"),
        ("system", "/settings/ups", "/settings/osupdates"),
    ] {
        let path: &'static str = Box::leak(format!("/settings/tab/{tab}").into_boxed_str());
        let res = get(path).await;
        assert_eq!(res.status(), StatusCode::OK, "{tab}");
        let html = body_of(res).await;
        assert!(html.contains(has), "{tab} should contain {has}");
        if tab == "updates" {
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
async fn registry_logins_page_needs_a_session_and_refuses_bad_input() {
    let app = test_app();
    let anon = app
        .clone()
        .oneshot(
            Request::get("/settings/registries")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(anon.status(), StatusCode::OK);
    let cookie = cookie_of(&app.clone().oneshot(login_req(PASSWORD)).await.unwrap());
    let page = app
        .clone()
        .oneshot(
            Request::get("/settings/registries")
                .header(header::COOKIE, cookie.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert!(body_of(page).await.contains("No registry logins yet"));
    let post = |path: &'static str, body: &'static str| {
        let app = app.clone();
        let cookie = cookie.clone();
        async move {
            let req = Request::post(path)
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap();
            body_of(app.oneshot(req).await.unwrap()).await
        }
    };
    let bad = post(
        "/settings/registries/login",
        "server=ghcr.io%2Fowner&user=u&token=secret-tok",
    )
    .await;
    assert!(bad.contains("must be a host name"), "{bad}");
    assert!(!bad.contains("secret-tok"), "{bad}");
    let none = post("/settings/registries/logout", "server=ghcr.io").await;
    assert!(none.contains("No saved login"), "{none}");
}

#[tokio::test]
async fn power_schedule_form_validates_and_saves() {
    let root = std::env::temp_dir().join(format!("dockwren-power-ui-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let inst = root.join("gt");
    std::fs::create_dir_all(&inst).unwrap();
    std::fs::write(inst.join("state.toml"), "template = \"x\"\nversion = 1\n").unwrap();
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
            let req = Request::post("/instances/gt/power")
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap();
            body_of(app.oneshot(req).await.unwrap()).await
        }
    };
    let bad = post("stop=25%3A00&start=07%3A00").await;
    assert!(bad.contains("HH:MM"), "{bad}");
    assert!(bad.contains("25:00"), "{bad}");
    let same = post("stop=03%3A00&start=03%3A00").await;
    assert!(same.contains("cannot be the same"), "{same}");
    let ok = post("stop=23%3A00&start=07%3A00").await;
    assert!(ok.contains("stops 23:00, starts 07:00 UTC"), "{ok}");
    assert_eq!(
        std::fs::read_to_string(inst.join("power")).unwrap(),
        "stop=23:00\nstart=07:00\n"
    );
    let cleared = post("stop=&start=").await;
    assert!(cleared.contains("no schedule"), "{cleared}");
    assert!(!inst.join("power").exists());
    let missing = Request::post("/instances/nope/power")
        .header(header::COOKIE, cookie.clone())
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("stop=&start="))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(missing).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    let _ = std::fs::remove_dir_all(&root);
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
    assert!(bad.contains("must be a plain URL"), "{bad}");
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
        ("GET", "/apps/tab/add"),
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
        ("POST", "/instances/gt/delete"),
        ("POST", "/instances/gt/update"),
        ("POST", "/instances-bulk/backup"),
        ("POST", "/instances-bulk/update"),
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
        ("POST", "/settings/channels/telegram/remove"),
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
        html.contains("name=\"ADMIN_EMAIL\"") && html.contains("hx-post=\"/apps/gitea/install\"")
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
        .oneshot(post("/instances-bulk/backup", None, ""))
        .await
        .unwrap();
    assert!(body_of(res).await.contains("Tick at least one app"));
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

#[tokio::test]
async fn the_vulnerability_report_page_shows_findings_and_refuses_bad_names() {
    use crate::vulnscan::{Detail, Summary};
    let app = test_app();
    let cookie = cookie_of(&app.clone().oneshot(login_req(PASSWORD)).await.unwrap());
    crate::vulnscan::set_for_test(
        "report-demo",
        Summary {
            scanned: 1_700_000_000,
            critical: 1,
            high: 0,
            fixable_critical: 1,
            ids: vec!["CVE-2024-1".into()],
            notified: vec![],
            details: vec![Detail {
                id: "CVE-2024-1".into(),
                critical: true,
                pkg: "libssl".into(),
                installed: "3.0.1".into(),
                fixed: "3.0.2".into(),
                title: "<script>alert(1)</script> bad".into(),
            }],
        },
    );
    let get = |path: &'static str, cookie: Option<String>| {
        let app = app.clone();
        async move {
            let mut req = Request::get(path);
            if let Some(c) = cookie {
                req = req.header(header::COOKIE, c);
            }
            app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap()
        }
    };
    let anon = get("/apps/report-demo/vulns", None).await;
    assert_ne!(anon.status(), StatusCode::OK, "the report needs a session");
    let res = get("/apps/report-demo/vulns", Some(cookie.clone())).await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_of(res).await;
    if let Ok(path) = std::env::var("DW_DUMP_VULNS") {
        std::fs::write(path, &html).unwrap();
    }
    assert!(html.contains("Vulnerabilities: report-demo"));
    assert!(html.contains("libssl") && html.contains("3.0.2"));
    assert!(html.contains("https://avd.aquasec.com/nvd/cve-2024-1"));
    assert!(!html.contains("<script>alert(1)"), "titles are escaped");
    assert_eq!(
        get("/apps/Bad_Name/vulns", Some(cookie.clone()))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let none = body_of(get("/apps/never-scanned/vulns", Some(cookie)).await).await;
    assert!(none.contains("has not been scanned yet"));
}
