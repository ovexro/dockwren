//! Direct HTTPS: publish an app without Cloudflare. One Caddy container (project
//! `dockwren-caddy`, host networking) owns ports 80/443, gets Let's Encrypt certificates over
//! HTTP-01 and reverse-proxies each hostname to the app's `127.0.0.1:WEB_PORT`.
//!
//! The mode is per instance (`public-mode.txt` holds `direct`; no file means the Cloudflare
//! tunnel path). The Caddyfile is always regenerated from the instances in direct mode, so the
//! instance folders stay the single source of truth. Certificates live in `data/caddy/data`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

const MODE_FILE: &str = "public-mode.txt";
const IMAGE: &str = "caddy:2.11.9-alpine";
const CONTAINER: &str = "dockwren-caddy";
const ADMIN_SOCK: &str = "unix//config/admin.sock";

/// True when the instance is published directly (not through the tunnel).
pub fn is_direct(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(MODE_FILE)).is_ok_and(|s| s.trim() == "direct")
}

pub fn set_direct(dir: &Path, on: bool) -> Result<(), String> {
    let p = dir.join(MODE_FILE);
    if on {
        std::fs::write(&p, "direct\n").map_err(|e| format!("cannot write {}: {e}", p.display()))
    } else {
        match std::fs::remove_file(&p) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("cannot remove {}: {e}", p.display())),
        }
    }
}

pub fn caddy_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("caddy")
}

/// Caddyfile for the given (hostname, local port) pairs. Hostnames are validated by
/// `public::valid_hostname` before they get here, so they cannot inject Caddyfile syntax.
pub fn caddyfile(entries: &[(String, u16)]) -> String {
    let mut out = format!("{{\n\tadmin {ADMIN_SOCK}\n}}\n");
    for (host, port) in entries {
        out.push_str(&format!(
            "\n{host} {{\n\treverse_proxy 127.0.0.1:{port}\n}}\n"
        ));
    }
    out
}

fn compose_yaml(dir: &Path) -> String {
    format!(
        "name: dockwren-caddy\nservices:\n  caddy:\n    image: {IMAGE}\n    container_name: {CONTAINER}\n    network_mode: host\n    restart: unless-stopped\n    volumes:\n      - {d}/Caddyfile:/etc/caddy/Caddyfile:ro\n      - {d}/data:/data\n      - {d}/config:/config\n    logging:\n      driver: json-file\n      options:\n        max-size: \"10m\"\n        max-file: \"3\"\n",
        d = dir.display()
    )
}

async fn docker(data_dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("docker");
    if std::env::var_os("DOCKER_CONFIG").is_none() {
        cmd.env("DOCKER_CONFIG", data_dir.join("docker-config"));
    }
    let out = cmd
        .args(args)
        .output()
        .await
        .map_err(|e| format!("cannot run docker: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = err.lines().rev().take(6).collect();
        Err(tail.into_iter().rev().collect::<Vec<_>>().join("\n"))
    }
}

/// Hostnames and ports of every instance in direct mode, sorted by hostname.
pub fn collect(instances_root: &Path) -> Vec<(String, u16)> {
    let mut v: Vec<(String, u16)> = std::fs::read_dir(instances_root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let dir = e.path();
            if !is_direct(&dir) {
                return None;
            }
            let host = crate::public::read(&dir)?;
            let port = crate::public::web_port(&dir)?;
            Some((host, port))
        })
        .collect();
    v.sort();
    v.dedup_by(|a, b| a.0 == b.0);
    v
}

/// Make Caddy match the instances: write the files, start (or reload, or remove) the container.
pub async fn reconcile(data_dir: &Path, instances_root: &Path) -> Result<(), String> {
    let entries = collect(instances_root);
    let dir = caddy_dir(data_dir);
    let compose = dir.join("compose.yaml");
    if entries.is_empty() {
        if compose.is_file() {
            docker(
                data_dir,
                &["compose", "-f", &compose.display().to_string(), "down"],
            )
            .await?;
        }
        return Ok(());
    }
    for sub in ["data", "config"] {
        std::fs::create_dir_all(dir.join(sub)).map_err(|e| format!("cannot create {sub}: {e}"))?;
    }
    std::fs::write(dir.join("Caddyfile"), caddyfile(&entries))
        .map_err(|e| format!("cannot write Caddyfile: {e}"))?;
    std::fs::write(&compose, compose_yaml(&dir))
        .map_err(|e| format!("cannot write compose.yaml: {e}"))?;
    let was_running = running(data_dir).await;
    docker(
        data_dir,
        &["compose", "-f", &compose.display().to_string(), "up", "-d"],
    )
    .await?;
    if was_running {
        docker(
            data_dir,
            &[
                "exec",
                CONTAINER,
                "caddy",
                "reload",
                "--config",
                "/etc/caddy/Caddyfile",
                "--address",
                ADMIN_SOCK,
            ],
        )
        .await
        .map_err(|e| format!("Caddy reload failed: {e}"))?;
    }
    Ok(())
}

pub async fn running(data_dir: &Path) -> bool {
    docker(
        data_dir,
        &["inspect", "-f", "{{.State.Running}}", CONTAINER],
    )
    .await
    .is_ok_and(|s| s.trim() == "true")
}

/// One preflight line: (ok, blocks the publish when not ok, text).
pub type Check = (bool, bool, String);

/// Listener lines of `ss -ltnH` that use `port`.
fn port_busy(ss: &str, port: u16) -> bool {
    let suffix = format!(":{port}");
    ss.lines().any(|l| {
        l.split_whitespace()
            .nth(3)
            .is_some_and(|a| a.ends_with(&suffix))
    })
}

async fn public_ip() -> Option<String> {
    let out = Command::new("curl")
        .args(["-sS", "--max-time", "5", "-4", "https://api.ipify.org"])
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    let ip = String::from_utf8_lossy(&out.stdout).trim().to_string();
    ip.parse::<std::net::Ipv4Addr>().is_ok().then_some(ip)
}

/// Things that must hold for Let's Encrypt to succeed. Read-only.
pub async fn preflight(data_dir: &Path, host: &str, port: Option<u16>) -> Vec<Check> {
    let mut v: Vec<Check> = Vec::new();
    v.push(match port {
        Some(p) => (true, false, format!("App listens on 127.0.0.1:{p}.")),
        None => (
            false,
            true,
            "This app has no web port to publish.".to_string(),
        ),
    });
    let ours = running(data_dir).await;
    let ss = Command::new("ss")
        .arg("-ltnH")
        .output()
        .await
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    for p in [80u16, 443] {
        v.push(if port_busy(&ss, p) && !ours {
            (
                false,
                true,
                format!(
                    "Port {p} is already used by another program on this server. Free it first."
                ),
            )
        } else {
            (true, false, format!("Port {p} is free for Caddy."))
        });
    }
    let resolved: Vec<String> =
        match tokio::time::timeout(Duration::from_secs(5), tokio::net::lookup_host((host, 443)))
            .await
        {
            Ok(Ok(it)) => it
                .filter(|a| a.is_ipv4())
                .map(|a| a.ip().to_string())
                .collect(),
            _ => Vec::new(),
        };
    let mine = public_ip().await;
    v.push(match (resolved.is_empty(), &mine) {
        (true, _) => (
            false,
            false,
            format!("{host} has no DNS A record yet. Point it at this server's public IP before publishing, or the certificate will fail."),
        ),
        (false, Some(ip)) if resolved.iter().any(|r| r == ip) => {
            (true, false, format!("{host} points at this server ({ip})."))
        }
        (false, Some(ip)) => (
            false,
            false,
            format!(
                "{host} points at {}, but this server's public IP is {ip}. If it is a Cloudflare address, switch the record to DNS only (grey cloud) and remove the tunnel route. Otherwise fix the A record.",
                resolved.join(", ")
            ),
        ),
        (false, None) => (
            true,
            false,
            format!("{host} resolves to {}; could not look up this server's public IP to compare.", resolved.join(", ")),
        ),
    });
    v.push((
        true,
        false,
        "Ports 80 and 443 must also be forwarded to this server on your router or firewall."
            .to_string(),
    ));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caddyfile_has_one_block_per_host() {
        let f = caddyfile(&[
            ("a.example.com".into(), 8081),
            ("b.example.com".into(), 8082),
        ]);
        assert!(f.contains("a.example.com {\n\treverse_proxy 127.0.0.1:8081\n}"));
        assert!(f.contains("b.example.com {\n\treverse_proxy 127.0.0.1:8082\n}"));
        assert!(f.starts_with("{\n\tadmin unix//config/admin.sock\n}"));
    }

    #[test]
    fn port_detection_matches_only_listeners() {
        let ss = "LISTEN 0 4096 0.0.0.0:80 0.0.0.0:*\nLISTEN 0 4096 127.0.0.1:8080 0.0.0.0:*\nLISTEN 0 4096 [::]:443 [::]:*\n";
        assert!(port_busy(ss, 80));
        assert!(port_busy(ss, 443));
        assert!(!port_busy(ss, 8));
        assert!(!port_busy("LISTEN 0 4096 127.0.0.1:8080 0.0.0.0:*", 80));
    }

    #[test]
    fn mode_file_roundtrip() {
        let d = std::env::temp_dir().join(format!("dw-direct-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert!(!is_direct(&d));
        set_direct(&d, true).unwrap();
        assert!(is_direct(&d));
        set_direct(&d, false).unwrap();
        assert!(!is_direct(&d));
        set_direct(&d, false).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }
}
