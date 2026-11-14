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

use askama::Template;
use axum::{
    Form, Router,
    extract::{
        Path, State,
        ws::{Message, WebSocket, WebSocketUpgrade, rejection::WebSocketUpgradeRejection},
    },
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
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

pub const XTERM_JS: &str = include_str!("../static/xterm.js");
pub const XTERM_CSS: &str = include_str!("../static/xterm.css");
pub const FIT_JS: &str = include_str!("../static/addon-fit.js");
pub const TERMINAL_JS: &str = include_str!("../static/terminal.js");

const MAX_SESSIONS: usize = 3;
const GRANT_TTL: Duration = Duration::from_secs(10 * 60);
const IDLE_LIMIT: Duration = Duration::from_secs(30 * 60);
const TIME_LIMIT: Duration = Duration::from_secs(4 * 60 * 60);
const MAX_MESSAGE: usize = 64 * 1024;

/// Panel-managed Compose projects are named `dockwren-<instance>` (see `runner::compose`).
const PROJECT_PREFIX: &str = "dockwren-";

/// Whether the terminal is on: forced by `DOCKWREN_TERMINAL=1`, or turned on in Settings
/// (a flag file in the data directory).
pub struct Switch {
    forced: bool,
    file: Option<std::path::PathBuf>,
}

impl Switch {
    pub fn new(forced: bool, data_dir: Option<&std::path::Path>) -> Self {
        Self {
            forced,
            file: data_dir.map(|d| d.join("terminal.enabled")),
        }
    }

    pub fn is_on(&self) -> bool {
        self.forced || self.file.as_ref().is_some_and(|f| f.exists())
    }

    pub fn forced(&self) -> bool {
        self.forced
    }

    fn set(&self, on: bool) -> std::io::Result<()> {
        let Some(f) = &self.file else {
            return Err(std::io::Error::other("no data directory"));
        };
        if on {
            std::fs::write(f, "1\n")
        } else {
            match std::fs::remove_file(f) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        }
    }
}

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
    switch: Arc<Switch>,
}

pub fn router(
    auth: SharedAuth,
    docker: DockerClient,
    notify: notify::Shared,
    switch: Arc<Switch>,
) -> Router {
    Router::new()
        .route("/containers/{id}/terminal", get(page))
        .route(
            "/static/xterm.js",
            get(|| asset("text/javascript", XTERM_JS)),
        )
        .route("/static/xterm.css", get(|| asset("text/css", XTERM_CSS)))
        .route(
            "/static/addon-fit.js",
            get(|| asset("text/javascript", FIT_JS)),
        )
        .route(
            "/static/terminal.js",
            get(|| asset("text/javascript", TERMINAL_JS)),
        )
        .route("/settings/terminal", get(setting_show).post(setting_set))
        .route("/containers/{id}/terminal/grant", post(grant))
        .route("/containers/{id}/terminal/ws", get(ws))
        .with_state(TermState {
            auth,
            docker,
            notify,
            gate: Arc::new(Gate::default()),
            switch,
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

pub fn managed(project: Option<&str>) -> bool {
    project.is_some_and(|p| p.len() > PROJECT_PREFIX.len() && p.starts_with(PROJECT_PREFIX))
}

async fn asset(kind: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, kind),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

#[derive(Template)]
#[template(path = "terminal_setting.html")]
struct SettingTemplate {
    on: bool,
    forced: bool,
    two_factor: bool,
    error: Option<&'static str>,
}

fn setting_html(s: &TermState, error: Option<&'static str>) -> Response {
    let t = SettingTemplate {
        on: s.switch.is_on(),
        forced: s.switch.forced(),
        two_factor: s.auth.two_factor_enabled(),
        error,
    };
    Html(t.render().expect("terminal setting renders")).into_response()
}

/// `GET /settings/terminal`: the on/off control shown in Settings.
async fn setting_show(State(s): State<TermState>, headers: HeaderMap) -> Response {
    if s.auth.session_of(&headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    setting_html(&s, None)
}

#[derive(Deserialize)]
struct SettingForm {
    on: String,
    password: String,
    #[serde(default)]
    code: String,
}

/// `POST /settings/terminal`: turn the terminal on or off; needs the password (and 2FA code) again.
async fn setting_set(
    State(s): State<TermState>,
    headers: HeaderMap,
    Form(form): Form<SettingForm>,
) -> Response {
    if s.auth.session_of(&headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !crate::same_origin(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if s.switch.forced() {
        return setting_html(
            &s,
            Some("Set by DOCKWREN_TERMINAL=1; remove that variable to control it here."),
        );
    }
    if !s.auth.reauth(form.password, &form.code).await {
        return setting_html(&s, Some("Wrong password or code."));
    }
    let on = form.on == "1";
    if let Err(e) = s.switch.set(on) {
        eprintln!("terminal: cannot change setting: {e}");
        return setting_html(&s, Some("Could not save the setting (see the panel log)."));
    }
    let word = if on { "on" } else { "off" };
    eprintln!("terminal: turned {word} from Settings");
    notify::raise(
        &s.notify,
        NewEvent::new(
            "terminal",
            "terminal.opened",
            Severity::Info,
            &format!("Web terminal turned {word}"),
        )
        .key(format!("terminal.setting:{}", rand_tag())),
    );
    setting_html(&s, None)
}

#[derive(Template)]
#[template(path = "terminal.html")]
struct PageTemplate {
    id: String,
    name: String,
    two_factor: bool,
    assets: String,
}

/// `GET /containers/{id}/terminal`: the page with the re-auth form and the terminal.
async fn page(State(s): State<TermState>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    if !s.switch.is_on() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if s.auth.session_of(&headers).is_none() {
        return Redirect::to("/login").into_response();
    }
    if !docker::valid_id(&id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let list = s.docker.containers().await.unwrap_or_default();
    let Some(c) = list
        .into_iter()
        .find(|c| id.starts_with(&c.id) && c.managed && c.state == "running")
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let page = PageTemplate {
        id,
        name: c.name,
        two_factor: s.auth.two_factor_enabled(),
        assets: crate::asset_version(),
    };
    Html(page.render().expect("terminal template renders")).into_response()
}

#[derive(Deserialize)]
struct GrantForm {
    password: String,
    #[serde(default)]
    code: String,
}

/// Checks the common preconditions; returns the session token on success.
fn precheck<'a>(s: &TermState, headers: &'a HeaderMap, id: &str) -> Result<&'a str, StatusCode> {
    if !s.switch.is_on() {
        return Err(StatusCode::NOT_FOUND);
    }
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
    fn switch_follows_the_flag_file_unless_forced() {
        let dir = std::env::temp_dir().join(format!("dockwren-sw-{}", rand_tag()));
        std::fs::create_dir_all(&dir).unwrap();
        let sw = Switch::new(false, Some(&dir));
        assert!(!sw.is_on());
        sw.set(true).unwrap();
        assert!(sw.is_on());
        sw.set(false).unwrap();
        sw.set(false).unwrap(); // already off is fine
        assert!(!sw.is_on());
        assert!(Switch::new(true, None).is_on());
        assert!(Switch::new(false, None).set(true).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

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
