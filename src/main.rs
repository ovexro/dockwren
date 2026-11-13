mod auth;

use std::{path::PathBuf, sync::Arc};

use askama::Template;
use auth::SharedAuth;
use axum::{
    Router,
    middleware::from_fn_with_state,
    response::Html,
    routing::{get, post},
};

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

fn app(auth: SharedAuth) -> Router {
    let protected = Router::new()
        .route("/", get(index))
        .route("/logout", post(auth::logout))
        .route_layer(from_fn_with_state(auth.clone(), auth::require_session));
    Router::new()
        .route("/health", get(health))
        .route("/login", get(auth::login_form).post(auth::login_submit))
        .merge(protected)
        .with_state(auth)
}

#[tokio::main]
async fn main() {
    let data_dir =
        PathBuf::from(std::env::var("DOCKWREN_DATA").unwrap_or_else(|_| "./data".into()));
    let (hash, first_run_password) = auth::load_or_create(&data_dir)
        .unwrap_or_else(|e| panic!("cannot prepare data dir {}: {e}", data_dir.display()));
    let secure_cookie = std::env::var("DOCKWREN_SECURE_COOKIE").is_ok_and(|v| v == "1");
    let auth = Arc::new(auth::Auth::new(&hash, secure_cookie).unwrap_or_else(|e| panic!("{e}")));

    if let Some(password) = first_run_password {
        println!("First run: generated admin password (shown once, not stored):");
        println!("    {password}");
    }

    // Local-only by default; reach it over Tailscale or a tunnel.
    let addr = std::env::var("DOCKWREN_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));
    println!("dockwren listening on http://{addr}");
    axum::serve(listener, app(auth))
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
        app(Arc::new(auth::Auth::new(&hash, false).unwrap()))
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
