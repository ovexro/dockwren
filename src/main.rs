use askama::Template;
use axum::{Router, response::Html, routing::get};

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

fn app() -> Router {
    Router::new()
        .route("/", get(index))
        .route("/health", get(health))
}

#[tokio::main]
async fn main() {
    // Local-only by default; reach it over Tailscale or a tunnel.
    let addr = std::env::var("DOCKWREN_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));
    println!("dockwren listening on http://{addr}");
    axum::serve(listener, app()).await.expect("server error");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn get_body(path: &str) -> (StatusCode, String) {
        let res = app()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn health_is_ok() {
        assert_eq!(get_body("/health").await, (StatusCode::OK, "ok".into()));
    }

    #[tokio::test]
    async fn index_shows_name() {
        let (status, body) = get_body("/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Dockwren"));
    }
}
