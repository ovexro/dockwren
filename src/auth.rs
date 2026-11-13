//! Single-admin authentication: Argon2 password hash on disk, in-memory sessions,
//! and a simple login rate limit.

use std::{
    collections::HashMap,
    fs, io,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use askama::Template;
use axum::{
    Form,
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
};
use serde::Deserialize;

pub const COOKIE_NAME: &str = "dockwren_session";
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const MAX_FAILURES: u32 = 5;
const FAILURE_WINDOW: Duration = Duration::from_secs(5 * 60);
const LOCKOUT: Duration = Duration::from_secs(60);
const HASH_FILE: &str = "admin.hash";
const PASSWORD_LEN: usize = 24;
const PASSWORD_CHARS: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";

#[derive(Default)]
struct Limiter {
    failures: u32,
    window_start: Option<Instant>,
    locked_until: Option<Instant>,
}

pub struct Auth {
    hash: PasswordHash,
    secure_cookie: bool,
    sessions: Mutex<HashMap<String, Instant>>,
    limiter: Mutex<Limiter>,
}

pub type SharedAuth = Arc<Auth>;

/// Generates a strong random password (no look-alike characters).
pub fn generate_password() -> String {
    (0..PASSWORD_LEN)
        .map(|_| PASSWORD_CHARS[rand::random_range(0..PASSWORD_CHARS.len())] as char)
        .collect()
}

pub fn hash_password(password: &str) -> String {
    Argon2::default()
        .hash_password(password.as_bytes())
        .expect("hashing a password")
        .to_string()
}

/// Loads the admin hash from `<data_dir>/admin.hash`. On first run, generates a
/// password, stores only its hash (mode 0600) and returns the password once.
pub fn load_or_create(data_dir: &Path) -> io::Result<(String, Option<String>)> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(data_dir)?;
    let path = data_dir.join(HASH_FILE);
    match fs::read_to_string(&path) {
        Ok(hash) => Ok((hash.trim().to_string(), None)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let password = generate_password();
            let hash = hash_password(&password);
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?;
            io::Write::write_all(&mut file, hash.as_bytes())?;
            Ok((hash, Some(password)))
        }
        Err(e) => Err(e),
    }
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes[..]);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Auth {
    pub fn new(hash: &str, secure_cookie: bool) -> Result<Self, String> {
        let hash = hash
            .parse::<PasswordHash>()
            .map_err(|e| format!("invalid admin hash: {e}"))?;
        Ok(Self {
            hash,
            secure_cookie,
            sessions: Mutex::default(),
            limiter: Mutex::default(),
        })
    }

    fn verify(&self, password: &str) -> bool {
        Argon2::default()
            .verify_password(password.as_bytes(), &self.hash)
            .is_ok()
    }

    /// Returns true if logins are currently locked out.
    fn locked(&self) -> bool {
        let limiter = self.limiter.lock().unwrap();
        limiter.locked_until.is_some_and(|t| Instant::now() < t)
    }

    fn record_failure(&self) {
        let now = Instant::now();
        let mut l = self.limiter.lock().unwrap();
        if l.window_start
            .is_none_or(|s| now.duration_since(s) > FAILURE_WINDOW)
        {
            l.window_start = Some(now);
            l.failures = 0;
        }
        l.failures += 1;
        if l.failures >= MAX_FAILURES {
            l.locked_until = Some(now + LOCKOUT);
            l.failures = 0;
            l.window_start = None;
        }
    }

    fn record_success(&self) {
        *self.limiter.lock().unwrap() = Limiter::default();
    }

    fn create_session(&self) -> String {
        let token = random_token();
        let now = Instant::now();
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|_, exp| *exp > now);
        sessions.insert(token.clone(), now + SESSION_TTL);
        token
    }

    fn session_valid(&self, token: &str) -> bool {
        let sessions = self.sessions.lock().unwrap();
        sessions.get(token).is_some_and(|exp| Instant::now() < *exp)
    }

    fn end_session(&self, token: &str) {
        self.sessions.lock().unwrap().remove(token);
    }

    fn cookie(&self, value: &str, max_age: u64) -> HeaderValue {
        let secure = if self.secure_cookie { "; Secure" } else { "" };
        HeaderValue::from_str(&format!(
            "{COOKIE_NAME}={value}; HttpOnly; SameSite=Strict; Path=/; Max-Age={max_age}{secure}"
        ))
        .expect("valid cookie header")
    }
}

fn session_token(req_headers: &header::HeaderMap) -> Option<&str> {
    req_headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE_NAME)
        .map(|(_, value)| value)
}

/// Middleware: everything behind it needs a valid session.
pub async fn require_session(State(auth): State<SharedAuth>, req: Request, next: Next) -> Response {
    match session_token(req.headers()) {
        Some(t) if auth.session_valid(t) => next.run(req).await,
        _ => Redirect::to("/login").into_response(),
    }
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    error: Option<&'static str>,
}

fn login_page(status: StatusCode, error: Option<&'static str>) -> Response {
    let body = LoginTemplate { error }
        .render()
        .expect("login template renders");
    (status, Html(body)).into_response()
}

pub async fn login_form() -> Response {
    login_page(StatusCode::OK, None)
}

#[derive(Deserialize)]
pub struct LoginForm {
    password: String,
}

pub async fn login_submit(State(auth): State<SharedAuth>, Form(form): Form<LoginForm>) -> Response {
    if auth.locked() {
        return login_page(
            StatusCode::TOO_MANY_REQUESTS,
            Some("Too many attempts. Try again in a minute."),
        );
    }
    // Argon2 is CPU-heavy: keep it off the async workers.
    let checker = Arc::clone(&auth);
    let ok = tokio::task::spawn_blocking(move || checker.verify(&form.password))
        .await
        .unwrap_or(false);
    if !ok {
        auth.record_failure();
        return login_page(StatusCode::UNAUTHORIZED, Some("Wrong password."));
    }
    auth.record_success();
    let token = auth.create_session();
    let mut res = Redirect::to("/").into_response();
    res.headers_mut().insert(
        header::SET_COOKIE,
        auth.cookie(&token, SESSION_TTL.as_secs()),
    );
    res
}

pub async fn logout(State(auth): State<SharedAuth>, headers: header::HeaderMap) -> Response {
    if let Some(t) = session_token(&headers) {
        auth.end_session(t);
    }
    let mut res = Redirect::to("/login").into_response();
    res.headers_mut()
        .insert(header::SET_COOKIE, auth.cookie("", 0));
    res
}
