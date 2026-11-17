//! Read-only status API: `GET /api/status` with `Authorization: Bearer <token>`.
//! The token is made on the command line (`dockwren status-token`), shown once and kept only
//! as a SHA-256 hash in the data dir. It cannot sign in or change anything.

use std::path::Path;

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};

use crate::AppState;

const FILE: &str = "status-token.sha256";

fn sha_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Makes a new token (replacing any old one) and returns it; only its hash is stored.
pub fn create(data_dir: &Path) -> std::io::Result<String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes[..]);
    let token = format!(
        "dwst_{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    std::fs::create_dir_all(data_dir)?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(data_dir.join(FILE))?;
    f.write_all(sha_hex(&token).as_bytes())?;
    Ok(token)
}

/// Deletes the token; true when there was one.
pub fn revoke(data_dir: &Path) -> std::io::Result<bool> {
    match std::fs::remove_file(data_dir.join(FILE)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

fn token_matches(data_dir: &Path, presented: &str) -> bool {
    let Ok(stored) = std::fs::read_to_string(data_dir.join(FILE)) else {
        return false;
    };
    let (a, b) = (stored.trim().as_bytes(), sha_hex(presented).into_bytes());
    a.len() == b.len() && a.iter().zip(&b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

pub async fn status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let ok = match (state.auth.data_dir(), bearer(&headers)) {
        (Some(dir), Some(t)) => token_matches(dir, t),
        _ => false,
    };
    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "unauthorized\n",
        )
            .into_response();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let apps = crate::apps::status_json(&state.apps).await;
    let last_backup = crate::apps::latest_backup(&state.apps);
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "time": now,
            "apps": apps,
            "last_backup_at": last_backup,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("dockwren-st-{}", crate::auth::generate_password()))
    }

    #[test]
    fn token_roundtrip_is_stored_hashed_and_revocable() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp();
        let t = create(&d).unwrap();
        assert!(t.starts_with("dwst_") && t.len() == 69);
        let stored = std::fs::read_to_string(d.join(FILE)).unwrap();
        assert!(!stored.contains(&t));
        assert_eq!(
            std::fs::metadata(d.join(FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(token_matches(&d, &t));
        assert!(!token_matches(&d, "dwst_nope"));
        assert!(!token_matches(&d, ""));
        let t2 = create(&d).unwrap();
        assert!(!token_matches(&d, &t), "a new token replaces the old one");
        assert!(token_matches(&d, &t2));
        assert!(revoke(&d).unwrap());
        assert!(!token_matches(&d, &t2));
        assert!(!revoke(&d).unwrap());
    }

    #[test]
    fn bearer_header_parsing() {
        let mut h = HeaderMap::new();
        assert_eq!(bearer(&h), None);
        h.insert(header::AUTHORIZATION, "Bearer abc".parse().unwrap());
        assert_eq!(bearer(&h), Some("abc"));
        h.insert(header::AUTHORIZATION, "bearer  abc ".parse().unwrap());
        assert_eq!(bearer(&h), Some("abc"));
        h.insert(header::AUTHORIZATION, "Basic abc".parse().unwrap());
        assert_eq!(bearer(&h), None);
    }
}
