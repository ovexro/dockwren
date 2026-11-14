//! Web terminal: an interactive shell inside a panel-managed container, over a WebSocket.
//! Opt-in (`DOCKWREN_TERMINAL=1`); without it none of these routes exist.
//!
//! Every open needs: a valid session, a same-origin upgrade, a recent re-authentication
//! (password, plus a code when 2FA is on), a container of a panel-managed instance, and a
//! free slot. Reaching the panel through a Cloudflare Tunnel additionally requires 2FA.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    Form, Router,
    extract::{
        Path, State,
        ws::{Message, WebSocket, WebSocketUpgrade, rejection::WebSocketUpgradeRejection},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use crate::{
    auth::{SharedAuth, sha_hex},
    docker::{self, DockerClient, ShellSession},
    notify::{self, NewEvent, Severity},
};

const MAX_SESSIONS: usize = 3;
const GRANT_TTL: Duration = Duration::from_secs(10 * 60);
const IDLE_LIMIT: Duration = Duration::from_secs(30 * 60);
const TIME_LIMIT: Duration = Duration::from_secs(4 * 60 * 60);
const MAX_MESSAGE: usize = 64 * 1024;

/// Panel-managed Compose projects are named `dockwren-<instance>` (see `runner::compose`).
const PROJECT_PREFIX: &str = "dockwren-";

/// Re-auth grants and the live-session counter.
#[derive(Default)]
pub struct Gate {
    grants: Mutex<HashMap<String, Instant>>, // key: sha256(session token)
    active: AtomicUsize,
}

/// One open terminal slot; released on drop.
struct Permit(Arc<Gate>);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Gate {
    fn grant(&self, token: &str) {
        let mut g = self.grants.lock().unwrap();
        let now = Instant::now();
        g.retain(|_, until| *until > now);
        g.insert(sha_hex(token), now + GRANT_TTL);
    }

    fn granted(&self, token: &str) -> bool {
        self.grants
            .lock()
            .unwrap()
            .get(&sha_hex(token))
            .is_some_and(|until| *until > Instant::now())
    }

    fn try_open(self: &Arc<Self>) -> Option<Permit> {
        let n = self.active.fetch_add(1, Ordering::SeqCst);
        if n >= MAX_SESSIONS {
            self.active.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Permit(Arc::clone(self)))
    }
}

#[derive(Clone)]
struct TermState {
    auth: SharedAuth,
    docker: DockerClient,
    notify: notify::Shared,
    gate: Arc<Gate>,
}

pub fn router(auth: SharedAuth, docker: DockerClient, notify: notify::Shared) -> Router {
    Router::new()
        .route("/containers/{id}/terminal/grant", post(grant))
        .route("/containers/{id}/terminal/ws", get(ws))
        .with_state(TermState {
            auth,
            docker,
            notify,
            gate: Arc::new(Gate::default()),
        })
}

/// Strict: browsers always send `Origin` on a WebSocket upgrade, so a missing one is refused.
fn origin_ok(headers: &HeaderMap) -> bool {
    headers.contains_key(header::ORIGIN) && crate::same_origin(headers)
}

/// Requests that came through a Cloudflare Tunnel carry `Cf-Connecting-Ip`.
fn via_tunnel(headers: &HeaderMap) -> bool {
    headers.contains_key("cf-connecting-ip")
}

fn managed(project: Option<&str>) -> bool {
    project.is_some_and(|p| p.len() > PROJECT_PREFIX.len() && p.starts_with(PROJECT_PREFIX))
}

#[derive(Deserialize)]
struct GrantForm {
    password: String,
    #[serde(default)]
    code: String,
}

/// Checks the common preconditions; returns the session token on success.
fn precheck<'a>(s: &TermState, headers: &'a HeaderMap, id: &str) -> Result<&'a str, StatusCode> {
    let token = s.auth.session_of(headers).ok_or(StatusCode::UNAUTHORIZED)?;
    if !origin_ok(headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    if !docker::valid_id(id) {
        return Err(StatusCode::NOT_FOUND);
    }
    if via_tunnel(headers) && !s.auth.two_factor_enabled() {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(token)
}

/// `POST /containers/{id}/terminal/grant`: password (+ code) in, a 10 minute grant out.
async fn grant(
    State(s): State<TermState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<GrantForm>,
) -> Response {
    let token = match precheck(&s, &headers, &id) {
        Ok(t) => t,
        Err(code) => return code.into_response(),
    };
    if !s.auth.reauth(form.password, &form.code).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    s.gate.grant(token);
    StatusCode::NO_CONTENT.into_response()
}

/// `GET /containers/{id}/terminal/ws`: the WebSocket upgrade.
async fn ws(
    State(s): State<TermState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let token = match precheck(&s, &headers, &id) {
        Ok(t) => t.to_string(),
        Err(code) => return code.into_response(),
    };
    if !s.gate.granted(&token) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !managed(s.docker.compose_project(&id).await.as_deref()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    // Checked last so that auth failures answer 401/403 even for a plain GET.
    let Ok(upgrade) = upgrade else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(permit) = s.gate.try_open() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    let shell = match s.docker.open_shell(&id).await {
        Ok(sh) => sh,
        Err(e) => {
            eprintln!("terminal: cannot open shell in {id}: {e}");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let who: String = sha_hex(&token).chars().take(8).collect();
    upgrade
        .max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |socket| session(socket, shell, s, id, who, permit))
}

#[derive(Deserialize)]
struct Resize {
    resize: (u16, u16),
}

fn parse_resize(text: &str) -> Option<(u16, u16)> {
    serde_json::from_str::<Resize>(text).ok().map(|r| r.resize)
}

async fn session(
    socket: WebSocket,
    mut shell: ShellSession,
    s: TermState,
    id: String,
    who: String,
    _permit: Permit,
) {
    let started = Instant::now();
    eprintln!("terminal: opened container={id} session={who}");
    notify::raise(
        &s.notify,
        NewEvent::new(
            "terminal",
            "terminal.opened",
            Severity::Info,
            &format!("Terminal opened in container {id}"),
        )
        .key(format!(
            "terminal.opened:{id}:{}",
            started.elapsed().as_nanos() ^ rand_tag()
        )),
    );

    let (mut tx, mut rx) = socket.split();
    let end = tokio::time::Instant::now() + TIME_LIMIT;
    let mut idle = tokio::time::Instant::now() + IDLE_LIMIT;
    let reason = loop {
        tokio::select! {
            out = shell.output.next() => match out {
                Some(Ok(bytes)) => {
                    if tx.send(Message::Binary(bytes.into())).await.is_err() {
                        break "browser disconnected";
                    }
                }
                _ => break "shell exited",
            },
            msg = rx.next() => match msg {
                Some(Ok(Message::Binary(data))) => {
                    idle = tokio::time::Instant::now() + IDLE_LIMIT;
                    if shell.input.write_all(&data).await.is_err() {
                        break "shell exited";
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    if let Some((cols, rows)) = parse_resize(text.as_str()) {
                        let _ = s.docker.resize_shell(&shell.exec_id, cols, rows).await;
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                _ => break "browser disconnected",
            },
            _ = tokio::time::sleep_until(idle) => break "idle timeout",
            _ = tokio::time::sleep_until(end) => break "time limit",
        }
    };
    // Docker cannot kill an exec directly: interrupt, ask the shell to exit, close stdin.
    let _ = shell.input.write_all(b"\x03exit\n").await;
    let _ = shell.input.shutdown().await;
    let _ = tx.send(Message::Close(None)).await;
    eprintln!(
        "terminal: closed container={id} session={who} after={}s reason={reason}",
        started.elapsed().as_secs()
    );
}

/// Cheap uniqueness for event keys (each open is its own event, never merged).
fn rand_tag() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_must_be_present_and_match() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "panel.example".parse().unwrap());
        assert!(!origin_ok(&h), "missing Origin is refused");
        h.insert(header::ORIGIN, "https://panel.example".parse().unwrap());
        assert!(origin_ok(&h));
        h.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert!(!origin_ok(&h));
    }

    #[test]
    fn tunnel_detected_by_cloudflare_header() {
        let mut h = HeaderMap::new();
        assert!(!via_tunnel(&h));
        h.insert("cf-connecting-ip", "203.0.113.9".parse().unwrap());
        assert!(via_tunnel(&h));
    }

    #[test]
    fn only_panel_projects_are_managed() {
        assert!(managed(Some("dockwren-gitea")));
        assert!(!managed(Some("dockwren-")));
        assert!(!managed(Some("other")));
        assert!(!managed(None));
    }

    #[test]
    fn slots_are_limited_and_released() {
        let gate = Arc::new(Gate::default());
        let held: Vec<_> = (0..MAX_SESSIONS)
            .map(|_| gate.try_open().unwrap())
            .collect();
        assert!(gate.try_open().is_none(), "4th session refused");
        drop(held);
        assert!(gate.try_open().is_some());
    }

    #[test]
    fn grants_are_per_session() {
        let gate = Gate::default();
        assert!(!gate.granted("tok"));
        gate.grant("tok");
        assert!(gate.granted("tok"));
        assert!(!gate.granted("other"));
    }

    #[test]
    fn resize_messages_parse() {
        assert_eq!(parse_resize(r#"{"resize":[120,40]}"#), Some((120, 40)));
        assert_eq!(parse_resize("ls"), None);
        assert_eq!(parse_resize(r#"{"resize":[-1,2]}"#), None);
    }
}
