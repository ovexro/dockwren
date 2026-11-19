//! Registry logins: so private images (GHCR, Docker Hub, a self-hosted registry) can be pulled.
//!
//! Every Docker CLI call Dockwren makes uses `DOCKER_CONFIG=<data>/docker-config`, so a
//! `docker login` into that directory is picked up by installs, updates and adopted stacks
//! without any further plumbing. This module is only the safe front door to it: it validates
//! the input, hands the token to `docker login` on stdin (never on the command line), lists the
//! saved server names (never the credentials) and logs out.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Docker's own key for Docker Hub; `docker login` with no server argument uses it.
const HUB_KEY: &str = "https://index.docker.io/v1/";

/// The Docker config directory used by every call (an explicit `DOCKER_CONFIG` wins, as elsewhere).
pub fn config_dir(data: &Path) -> PathBuf {
    match std::env::var_os("DOCKER_CONFIG") {
        Some(d) => PathBuf::from(d),
        None => data.join("docker-config"),
    }
}

/// A registry address as `host` or `host:port`; `docker.io` / empty mean Docker Hub.
/// Returns `None` for Docker Hub (no server argument), `Some(addr)` otherwise.
pub fn parse_server(raw: &str) -> Result<Option<String>, &'static str> {
    let s = raw.trim().to_ascii_lowercase();
    let s = s
        .strip_prefix("https://")
        .unwrap_or(&s)
        .trim_end_matches('/')
        .to_string();
    if s.is_empty() || s == "docker.io" || s == "index.docker.io" || s == "registry-1.docker.io" {
        return Ok(None);
    }
    if s.len() > 253
        || !s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
        || s.starts_with(['.', '-', ':'])
        || s.matches(':').count() > 1
    {
        return Err(
            "The registry must be a host name such as ghcr.io or registry.example.com:5000.",
        );
    }
    if let Some((_, port)) = s.split_once(':')
        && !matches!(port.parse::<u32>(), Ok(1..=65535))
    {
        return Err("The registry port must be a number from 1 to 65535.");
    }
    Ok(Some(s))
}

fn check_user(u: &str) -> Result<&str, &'static str> {
    let u = u.trim();
    if u.is_empty()
        || u.len() > 200
        || u.chars().any(|c| c.is_control() || c.is_whitespace())
        || u.starts_with('-')
    {
        return Err("Enter the user name (no spaces).");
    }
    Ok(u)
}

fn check_token(t: &str) -> Result<&str, &'static str> {
    if t.is_empty() {
        return Err("Enter the access token or password.");
    }
    if t.len() > 4096 || t.chars().any(char::is_control) {
        return Err("The token has to be one line of at most 4096 characters.");
    }
    Ok(t)
}

/// What the user sees for a saved login: the server name only.
pub fn display_name(key: &str) -> String {
    if key == HUB_KEY || key.contains("docker.io") {
        "Docker Hub".into()
    } else {
        key.trim_start_matches("https://")
            .trim_end_matches('/')
            .to_string()
    }
}

/// Servers with a saved login (sorted). Reads only the key names of `auths`.
pub fn list(data: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(config_dir(data).join("config.json")) else {
        return Vec::new();
    };
    names_from_config(&text)
}

fn names_from_config(text: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let mut keys: Vec<String> = v
        .get("auths")
        .and_then(|a| a.as_object())
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    keys.sort();
    keys
}

fn docker(data: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("docker");
    cmd.env("DOCKER_CONFIG", config_dir(data));
    cmd.kill_on_drop(true);
    cmd
}

fn ensure_dir(data: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    let d = config_dir(data);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&d)
        .map_err(|e| format!("{}: {e}", d.display()))
}

fn short(text: &str, secret: &str) -> String {
    let t = if secret.is_empty() {
        text.to_string()
    } else {
        text.replace(secret, "***")
    };
    let lines: Vec<&str> = t
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("WARNING"))
        .collect();
    let start = lines.len().saturating_sub(3);
    lines[start..].join("; ")
}

/// `docker login`, token on stdin. Docker checks it with the registry, so a wrong token is refused
/// and nothing is saved.
pub async fn login(data: &Path, server: &str, user: &str, token: &str) -> Result<String, String> {
    use tokio::io::AsyncWriteExt;
    let host = parse_server(server)?;
    let user = check_user(user)?;
    let token = check_token(token)?;
    ensure_dir(data)?;
    let mut cmd = docker(data);
    cmd.args(["login", "-u", user, "--password-stdin"]);
    if let Some(h) = &host {
        cmd.arg(h);
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("cannot run docker: {e}"))?;
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(token.as_bytes()).await;
        let _ = si.write_all(b"\n").await;
    }
    let out = match tokio::time::timeout(Duration::from_secs(45), child.wait_with_output()).await {
        Err(_) => return Err("The registry did not answer in time.".into()),
        Ok(Err(e)) => return Err(format!("cannot run docker: {e}")),
        Ok(Ok(o)) => o,
    };
    if out.status.success() {
        Ok(display_name(host.as_deref().unwrap_or(HUB_KEY)))
    } else {
        Err(short(&String::from_utf8_lossy(&out.stderr), token))
    }
}

/// `docker logout` for a server that is currently in the saved list.
pub async fn logout(data: &Path, key: &str) -> Result<(), String> {
    if !list(data).iter().any(|k| k == key) {
        return Err("No saved login for that registry.".into());
    }
    let mut cmd = docker(data);
    cmd.args(["logout", key])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let out = tokio::time::timeout(Duration::from_secs(20), cmd.output())
        .await
        .map_err(|_| "timed out".to_string())?
        .map_err(|e| format!("cannot run docker: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(short(&String::from_utf8_lossy(&out.stderr), ""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_names_are_validated() {
        assert_eq!(parse_server("").unwrap(), None);
        assert_eq!(parse_server("Docker.io").unwrap(), None);
        assert_eq!(
            parse_server("https://GHCR.io/").unwrap().unwrap(),
            "ghcr.io"
        );
        assert_eq!(
            parse_server("reg.example.com:5000").unwrap().unwrap(),
            "reg.example.com:5000"
        );
        for bad in [
            "ghcr.io/owner",
            "a b",
            "x;rm",
            "-evil",
            "h:99999",
            "h:x",
            "a:1:2",
            "$(id)",
            "http://insecure.example",
        ] {
            assert!(parse_server(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn user_and_token_are_checked() {
        assert!(check_user("octocat").is_ok());
        assert!(check_user("").is_err());
        assert!(check_user("a b").is_err());
        assert!(check_user("-p").is_err());
        assert!(check_token("ghp_abc").is_ok());
        assert!(check_token("").is_err());
        assert!(check_token("a\nb").is_err());
    }

    #[test]
    fn listing_shows_server_names_only() {
        let cfg = r#"{"auths":{"ghcr.io":{"auth":"c2VjcmV0"},"https://index.docker.io/v1/":{"auth":"eA=="}},"x":1}"#;
        let names = names_from_config(cfg);
        assert_eq!(names, vec!["ghcr.io", "https://index.docker.io/v1/"]);
        assert_eq!(display_name(&names[1]), "Docker Hub");
        assert_eq!(display_name("ghcr.io"), "ghcr.io");
        assert!(names_from_config("not json").is_empty());
        assert!(names_from_config("{}").is_empty());
    }

    #[test]
    fn errors_never_carry_the_token() {
        let e = short(
            "Error: bad token tok-123 for user\nWARNING: unencrypted",
            "tok-123",
        );
        assert!(!e.contains("tok-123"), "{e}");
        assert!(!e.contains("WARNING"), "{e}");
    }

    #[tokio::test]
    async fn bad_input_is_refused_before_docker_runs() {
        let dir = std::env::temp_dir().join(format!("dockwren-reg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(logout(&dir, "ghcr.io").await.is_err());
        assert!(login(&dir, "a/b", "u", "t").await.is_err());
        assert!(login(&dir, "ghcr.io", "", "t").await.is_err());
        assert!(login(&dir, "ghcr.io", "u", "").await.is_err());
        assert!(!dir.join("docker-config").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
