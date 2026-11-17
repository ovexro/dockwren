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

fn authorized(state: &AppState, headers: &HeaderMap) -> bool {
    match (state.auth.data_dir(), bearer(headers)) {
        (Some(dir), Some(t)) => token_matches(dir, t),
        _ => false,
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        "unauthorized\n",
    )
        .into_response()
}

/// Prometheus label value: backslash, quote and newline escaped.
fn label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Prometheus text format (0.0.4) from the same data as `/api/status`.
fn prometheus(version: &str, apps: &[serde_json::Value], last_backup: Option<u64>) -> String {
    use std::fmt::Write;
    let mut o = String::new();
    let head = |o: &mut String, name: &str, help: &str| {
        let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} gauge");
    };
    head(&mut o, "dockwren_info", "Dockwren version (always 1).");
    let _ = writeln!(o, "dockwren_info{{version=\"{}\"}} 1", label(version));
    head(&mut o, "dockwren_apps", "Number of installed apps.");
    let _ = writeln!(o, "dockwren_apps {}", apps.len());
    if let Some(t) = last_backup {
        head(
            &mut o,
            "dockwren_last_backup_timestamp_seconds",
            "Unix time of the newest backup of any app.",
        );
        let _ = writeln!(o, "dockwren_last_backup_timestamp_seconds {t}");
    }
    type Pick = fn(&serde_json::Value) -> Option<f64>;
    let series: [(&str, &str, Pick); 9] = [
        ("dockwren_app_healthy", "1 when the app is healthy.", |a| {
            a["healthy"].as_bool().map(f64::from)
        }),
        (
            "dockwren_app_public_up",
            "1 when the public address answers (only apps with one).",
            |a| a["public_up"].as_bool().map(f64::from),
        ),
        (
            "dockwren_app_update_available",
            "1 when a newer version of the app is available.",
            |a| a["update_available"].as_bool().map(f64::from),
        ),
        (
            "dockwren_app_backup_snapshots",
            "Snapshots kept for the app.",
            |a| a["backup"]["snapshots"].as_f64(),
        ),
        (
            "dockwren_app_last_backup_timestamp_seconds",
            "Unix time of the app's newest backup.",
            |a| a["backup"]["last_at"].as_f64(),
        ),
        (
            "dockwren_app_scan_timestamp_seconds",
            "Unix time of the app's last image scan.",
            |a| a["scan"]["scanned_at"].as_f64(),
        ),
        (
            "dockwren_app_scan_critical",
            "Critical vulnerabilities in the last scan.",
            |a| a["scan"]["critical"].as_f64(),
        ),
        (
            "dockwren_app_scan_high",
            "High vulnerabilities in the last scan.",
            |a| a["scan"]["high"].as_f64(),
        ),
        (
            "dockwren_app_scan_fixable_critical",
            "Critical vulnerabilities with a fix available in the last scan.",
            |a| a["scan"]["fixable_critical"].as_f64(),
        ),
    ];
    for (name, help, pick) in series {
        let rows: Vec<String> = apps
            .iter()
            .filter_map(|a| {
                let v = pick(a)?;
                Some(format!(
                    "{name}{{instance=\"{}\",app=\"{}\"}} {v}",
                    label(a["name"].as_str()?),
                    label(a["app"].as_str().unwrap_or(""))
                ))
            })
            .collect();
        if rows.is_empty() {
            continue;
        }
        head(&mut o, name, help);
        for r in rows {
            let _ = writeln!(o, "{r}");
        }
    }
    o
}

/// `GET /api/metrics`: the same read-only token, Prometheus text format.
pub async fn metrics(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&state, &headers) {
        return unauthorized();
    }
    let apps = crate::apps::status_json(&state.apps).await;
    let body = prometheus(
        env!("CARGO_PKG_VERSION"),
        &apps,
        crate::apps::latest_backup(&state.apps),
    );
    (
        [
            (
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

pub async fn status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&state, &headers) {
        return unauthorized();
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
    fn prometheus_text_has_labelled_series_and_skips_missing_ones() {
        let apps = vec![
            serde_json::json!({"name":"gt","app":"gitea","healthy":true,"public_up":null,
                "update_available":false,"backup":{"snapshots":3,"last_at":1700000000},
                "scan":{"scanned_at":1700000100,"critical":1,"high":4,"fixable_critical":0}}),
            serde_json::json!({"name":"we\"ird","app":"x","healthy":false,"public_up":true,
                "update_available":true,"backup":{"snapshots":0,"last_at":null},"scan":null}),
        ];
        let t = prometheus("1.2.3", &apps, Some(1700000000));
        assert!(t.contains("dockwren_info{version=\"1.2.3\"} 1"));
        assert!(t.contains("dockwren_apps 2"));
        assert!(t.contains("dockwren_last_backup_timestamp_seconds 1700000000"));
        assert!(t.contains("dockwren_app_healthy{instance=\"gt\",app=\"gitea\"} 1"));
        assert!(t.contains("dockwren_app_healthy{instance=\"we\\\"ird\",app=\"x\"} 0"));
        assert!(t.contains("dockwren_app_public_up{instance=\"we\\\"ird\",app=\"x\"} 1"));
        assert!(
            !t.contains("public_up{instance=\"gt\""),
            "no value, no series"
        );
        assert!(t.contains("dockwren_app_scan_high{instance=\"gt\",app=\"gitea\"} 4"));
        assert!(!t.contains("scan_high{instance=\"we"));
        assert!(!prometheus("1", &[], None).contains("last_backup_timestamp"));
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
