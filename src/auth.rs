//! Single-admin authentication: Argon2 password hash on disk, persistent sessions
//! (stored hashed), and a simple login rate limit.

use std::{
    collections::HashMap,
    fs, io,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
use sha2::{Digest, Sha256};

pub const COOKIE_NAME: &str = "dockwren_session";
const SESSION_TTL_SECS: u64 = 12 * 60 * 60;
const MAX_FAILURES: u32 = 5;
const FAILURE_WINDOW: Duration = Duration::from_secs(5 * 60);
const LOCKOUT: Duration = Duration::from_secs(60);
mod twofa;
pub use twofa::{
    disable as disable_two_factor, enable as twofa_enable, show as twofa_show,
    start as twofa_start, turn_off as twofa_disable,
};

const HASH_FILE: &str = "admin.hash";
const SESSION_FILE: &str = "sessions";
const PASSWORD_LEN: usize = 24;
const PASSWORD_CHARS: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";

#[derive(Default)]
struct Limiter {
    failures: u32,
    window_start: Option<Instant>,
    locked_until: Option<Instant>,
}

/// The admin credential currently in force. `fp` ties sessions to it: changing the
/// password changes the fingerprint and so signs everyone out.
struct Cred {
    hash: PasswordHash,
    fp: String,
    mtime: Option<SystemTime>,
}

struct Session {
    expires: u64, // unix seconds
    fp: String,
}

pub struct Auth {
    cred: Mutex<Cred>,
    hash_path: Option<PathBuf>,
    sessions_path: Option<PathBuf>,
    secure_cookie: bool,
    sessions: Mutex<HashMap<String, Session>>, // key: sha256(token) in hex
    limiter: Mutex<Limiter>,
    /// Data dir: where the optional 2FA files live (`None` in tests = 2FA unavailable).
    dir: Option<PathBuf>,
    /// Last accepted TOTP step, so a code cannot be used twice.
    last_step: Mutex<u64>,
    /// Secret shown during setup, not active until confirmed with a code.
    pending: Mutex<Option<Vec<u8>>>,
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

fn sha_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn ensure_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// Writes `content` to `path` (mode 0600) via a temp file and rename, so readers never see a partial file.
fn write_private(path: &Path, content: &str) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    io::Write::write_all(&mut file, content.as_bytes())?;
    file.sync_all()?;
    fs::rename(&tmp, path)
}

/// Loads the admin hash from `<data_dir>/admin.hash`. On first run, generates a
/// password, stores only its hash (mode 0600) and returns the password once.
pub fn load_or_create(data_dir: &Path) -> io::Result<(String, Option<String>)> {
    ensure_dir(data_dir)?;
    let path = data_dir.join(HASH_FILE);
    match fs::read_to_string(&path) {
        Ok(hash) => Ok((hash.trim().to_string(), None)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let password = generate_password();
            let hash = hash_password(&password);
            write_private(&path, &hash)?;
            Ok((hash, Some(password)))
        }
        Err(e) => Err(e),
    }
}

/// Replaces the admin password with a freshly generated one and returns it. A running
/// server picks the change up on its next request and signs all sessions out.
pub fn set_password(data_dir: &Path) -> io::Result<String> {
    ensure_dir(data_dir)?;
    let password = generate_password();
    write_private(&data_dir.join(HASH_FILE), &hash_password(&password))?;
    let _ = fs::remove_file(data_dir.join(SESSION_FILE));
    Ok(password)
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes[..]);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_cred(hash: &str) -> Result<(PasswordHash, String), String> {
    let hash = hash.trim();
    let parsed = hash
        .parse::<PasswordHash>()
        .map_err(|e| format!("invalid admin hash: {e}"))?;
    Ok((parsed, sha_hex(hash)))
}

impl Auth {
    /// In-memory only (no files): used by tests.
    #[cfg(test)]
    pub fn new(hash: &str, secure_cookie: bool) -> Result<Self, String> {
        let (hash, fp) = parse_cred(hash)?;
        Ok(Self {
            cred: Mutex::new(Cred {
                hash,
                fp,
                mtime: None,
            }),
            hash_path: None,
            sessions_path: None,
            secure_cookie,
            sessions: Mutex::default(),
            limiter: Mutex::default(),
            dir: None,
            last_step: Mutex::default(),
            pending: Mutex::default(),
        })
    }

    /// Production: credential and sessions live in `data_dir`.
    pub fn open(data_dir: &Path, secure_cookie: bool) -> Result<Self, String> {
        let hash_path = data_dir.join(HASH_FILE);
        let sessions_path = data_dir.join(SESSION_FILE);
        let text =
            fs::read_to_string(&hash_path).map_err(|e| format!("cannot read admin hash: {e}"))?;
        let (hash, fp) = parse_cred(&text)?;
        let mtime = fs::metadata(&hash_path).and_then(|m| m.modified()).ok();
        let auth = Self {
            cred: Mutex::new(Cred { hash, fp, mtime }),
            hash_path: Some(hash_path),
            sessions_path: Some(sessions_path.clone()),
            secure_cookie,
            sessions: Mutex::new(load_sessions(&sessions_path)),
            limiter: Mutex::default(),
            dir: Some(data_dir.to_path_buf()),
            last_step: Mutex::default(),
            pending: Mutex::default(),
        };
        Ok(auth)
    }

    /// Current credential, re-read from disk when the file changed (e.g. `set-password`).
    fn credential(&self) -> (PasswordHash, String) {
        let mut cred = self.cred.lock().unwrap();
        if let Some(path) = &self.hash_path {
            let mtime = fs::metadata(path).and_then(|m| m.modified()).ok();
            if mtime != cred.mtime {
                if let Ok((hash, fp)) = fs::read_to_string(path)
                    .map_err(|e| e.to_string())
                    .and_then(|t| parse_cred(&t))
                {
                    cred.hash = hash;
                    cred.fp = fp;
                }
                cred.mtime = mtime;
            }
        }
        (cred.hash.clone(), cred.fp.clone())
    }

    fn verify(&self, password: &str) -> bool {
        let (hash, _) = self.credential();
        Argon2::default()
            .verify_password(password.as_bytes(), &hash)
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

    fn persist(&self, sessions: &HashMap<String, Session>) {
        if let Some(path) = &self.sessions_path {
            let text: String = sessions
                .iter()
                .map(|(k, s)| format!("{k} {} {}\n", s.expires, s.fp))
                .collect();
            if let Err(e) = write_private(path, &text) {
                eprintln!("warning: cannot save sessions: {e}");
            }
        }
    }

    fn create_session(&self) -> String {
        let (_, fp) = self.credential();
        let token = random_token();
        let now = now_unix();
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|_, s| s.expires > now);
        sessions.insert(
            sha_hex(&token),
            Session {
                expires: now + SESSION_TTL_SECS,
                fp,
            },
        );
        self.persist(&sessions);
        token
    }

    fn session_valid(&self, token: &str) -> bool {
        let (_, fp) = self.credential();
        let sessions = self.sessions.lock().unwrap();
        sessions
            .get(&sha_hex(token))
            .is_some_and(|s| now_unix() < s.expires && s.fp == fp)
    }

    fn end_session(&self, token: &str) {
        let mut sessions = self.sessions.lock().unwrap();
        if sessions.remove(&sha_hex(token)).is_some() {
            self.persist(&sessions);
        }
    }

    fn cookie(&self, value: &str, max_age: u64) -> HeaderValue {
        let secure = if self.secure_cookie { "; Secure" } else { "" };
        HeaderValue::from_str(&format!(
            "{COOKIE_NAME}={value}; HttpOnly; SameSite=Strict; Path=/; Max-Age={max_age}{secure}"
        ))
        .expect("valid cookie header")
    }
}

fn load_sessions(path: &Path) -> HashMap<String, Session> {
    let now = now_unix();
    let text = fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split(' ');
            let key = parts.next()?.to_string();
            let expires: u64 = parts.next()?.parse().ok()?;
            let fp = parts.next()?.to_string();
            (expires > now && key.len() == 64).then_some((key, Session { expires, fp }))
        })
        .collect()
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
    assets: String,
}

fn login_page(status: StatusCode, error: Option<&'static str>) -> Response {
    let body = LoginTemplate {
        error,
        assets: crate::asset_version(),
    }
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
    /// Authenticator or recovery code; only needed when 2FA is on.
    #[serde(default)]
    code: String,
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
    let code = form.code.clone();
    let ok = tokio::task::spawn_blocking(move || checker.verify(&form.password))
        .await
        .unwrap_or(false);
    // One message for every failure, so it does not say which factor was wrong.
    if !ok || !auth.second_factor_ok(&code) {
        auth.record_failure();
        return login_page(StatusCode::UNAUTHORIZED, Some("Wrong password or code."));
    }
    auth.record_success();
    let token = auth.create_session();
    let mut res = Redirect::to("/").into_response();
    res.headers_mut()
        .insert(header::SET_COOKIE, auth.cookie(&token, SESSION_TTL_SECS));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("dockwren-auth-{}", generate_password()))
    }

    #[test]
    fn sessions_survive_a_restart_and_are_stored_hashed() {
        let dir = temp_dir();
        let (_, _) = load_or_create(&dir).unwrap();
        let first = Auth::open(&dir, false).unwrap();
        let token = first.create_session();
        assert!(first.session_valid(&token));

        let stored = fs::read_to_string(dir.join(SESSION_FILE)).unwrap();
        assert!(!stored.contains(&token), "raw token must not be on disk");

        let second = Auth::open(&dir, false).unwrap(); // "restart"
        assert!(second.session_valid(&token));
        second.end_session(&token);
        assert!(!Auth::open(&dir, false).unwrap().session_valid(&token));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn set_password_applies_live_and_signs_everyone_out() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir();
        let (_, old_pw) = load_or_create(&dir).unwrap();
        let old_pw = old_pw.unwrap();
        let auth = Auth::open(&dir, false).unwrap();
        let token = auth.create_session();
        assert!(auth.verify(&old_pw));

        // make sure the mtime differs even on coarse clocks
        std::thread::sleep(Duration::from_millis(20));
        let new_pw = set_password(&dir).unwrap();
        assert_ne!(new_pw, old_pw);
        let mode = fs::metadata(dir.join(HASH_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        assert!(auth.verify(&new_pw), "new password works without a restart");
        assert!(!auth.verify(&old_pw), "old password is rejected");
        assert!(!auth.session_valid(&token), "old sessions are signed out");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupt_session_lines_are_ignored() {
        let dir = temp_dir();
        ensure_dir(&dir).unwrap();
        let far = now_unix() + 1000;
        let good = format!("{} {far} fp", "a".repeat(64));
        fs::write(
            dir.join(SESSION_FILE),
            format!("garbage\nshort 1 x\n{good}\n{} 1 old\n", "b".repeat(64)),
        )
        .unwrap();
        let loaded = load_sessions(&dir.join(SESSION_FILE));
        assert_eq!(loaded.len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn auth_with_2fa() -> (SharedAuth, Vec<u8>, Vec<String>, PathBuf) {
        let dir = temp_dir();
        load_or_create(&dir).unwrap();
        let auth = Auth::open(&dir, false).unwrap();
        assert!(!auth.two_factor_enabled());
        let secret = crate::totp::new_secret();
        let codes = auth.turn_on(&secret).unwrap();
        (Arc::new(auth), secret, codes, dir)
    }

    #[test]
    fn second_factor_accepts_a_fresh_code_once_and_refuses_junk() {
        let (auth, secret, _, dir) = auth_with_2fa();
        assert!(auth.two_factor_enabled());
        let code = crate::totp::code_at(&secret, now_unix());
        assert!(!auth.second_factor_ok(""));
        assert!(!auth.second_factor_ok("000000") || code == "000000");
        assert!(auth.second_factor_ok(&code));
        assert!(!auth.second_factor_ok(&code), "same code twice");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn recovery_codes_work_once_and_are_stored_hashed() {
        let (auth, _, codes, dir) = auth_with_2fa();
        assert_eq!(codes.len(), 8);
        let file = fs::read_to_string(dir.join("totp.recovery")).unwrap();
        assert!(!file.contains(&codes[0]));
        assert!(auth.second_factor_ok(&codes[0].to_uppercase()));
        assert!(!auth.second_factor_ok(&codes[0]));
        assert!(auth.second_factor_ok(&codes[1]));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn login_needs_the_code_when_on_and_cli_turns_it_off() {
        let (auth, secret, _, dir) = auth_with_2fa();
        // The test password is the one in admin.hash, which we do not know: replace it.
        let pw = set_password(&dir).unwrap();
        let login = |code: String| {
            let a = Arc::clone(&auth);
            let pw = pw.clone();
            async move {
                login_submit(State(a), Form(LoginForm { password: pw, code }))
                    .await
                    .status()
            }
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            assert_eq!(login(String::new()).await, StatusCode::UNAUTHORIZED);
            let code = crate::totp::code_at(&secret, now_unix());
            assert_eq!(login(code).await, StatusCode::SEE_OTHER);
        });
        assert!(twofa::disable(&dir).unwrap());
        assert!(!auth.two_factor_enabled());
        rt.block_on(async {
            assert_eq!(login(String::new()).await, StatusCode::SEE_OTHER);
        });
        let _ = fs::remove_dir_all(dir);
    }
}
