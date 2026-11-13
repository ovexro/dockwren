//! Public hostnames for apps. The hostname is stored per instance in `public.txt`
//! (not state.toml, which updates rewrite). `expose` wires it up in Cloudflare:
//! a tunnel ingress rule plus a proxied CNAME. It needs a scoped API token in
//! the environment and never runs from the web panel, so the panel holds no
//! Cloudflare credentials.

use std::path::Path;
use std::process::Stdio;

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

const FILE: &str = "public.txt";

/// Lower-case DNS name with at least two labels, 253 chars max.
pub fn valid_hostname(h: &str) -> bool {
    h.len() <= 253
        && h.split('.').count() >= 2
        && h.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

pub fn read(dir: &Path) -> Option<String> {
    let h = std::fs::read_to_string(dir.join(FILE)).ok()?.trim().to_string();
    valid_hostname(&h).then_some(h)
}

/// Set (or with an empty string, clear) the hostname.
pub fn write(dir: &Path, host: &str) -> Result<(), String> {
    let path = dir.join(FILE);
    if host.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        };
    }
    if !valid_hostname(host) {
        return Err("not a valid hostname (lower-case letters, digits, hyphens, at least one dot)".into());
    }
    std::fs::write(&path, format!("{host}\n")).map_err(|e| e.to_string())
}

/// Port the app listens on for its web UI, from the instance `.env`.
pub fn web_port(dir: &Path) -> Option<u16> {
    let env = std::fs::read_to_string(dir.join(".env")).ok()?;
    env.lines()
        .find_map(|l| l.strip_prefix("WEB_PORT="))
        .and_then(|v| v.trim_matches('\'').parse().ok())
}

pub fn service_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// Candidate zone names for a hostname, longest first: a.b.c.d -> b.c.d, c.d (never a bare TLD).
fn zone_candidates(host: &str) -> Vec<String> {
    let labels: Vec<&str> = host.split('.').collect();
    (1..labels.len().saturating_sub(1)).map(|i| labels[i..].join(".")).collect()
}

/// New ingress list with a rule for `host` inserted before the catch-all.
/// `Ok(None)` when an identical rule already exists; Err when the hostname
/// is routed somewhere else (we never overwrite).
fn plan_ingress(existing: &[Value], host: &str, service: &str) -> Result<Option<Vec<Value>>, String> {
    for r in existing {
        if r.get("hostname").and_then(Value::as_str) == Some(host) {
            return match r.get("service").and_then(Value::as_str) {
                Some(s) if s == service => Ok(None),
                other => Err(format!("{host} already routes to {} in this tunnel; not overwriting", other.unwrap_or("?"))),
            };
        }
    }
    let mut out = existing.to_vec();
    let rule = json!({"hostname": host, "service": service});
    match out.last() {
        Some(last) if last.get("hostname").is_none() => out.insert(out.len() - 1, rule),
        _ => {
            out.push(rule);
            out.push(json!({"service": "http_status:404"}));
        }
    }
    Ok(Some(out))
}

struct Api {
    token: String,
}

impl Api {
    /// One Cloudflare API call through curl. The token goes in on stdin as a curl
    /// config, so it never shows up in the process list.
    async fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let mut cmd = tokio::process::Command::new("curl");
        cmd.args(["-sS", "--max-time", "30", "-K", "-", "-X", method, "-H", "Content-Type: application/json"]);
        if let Some(b) = body {
            cmd.args(["--data-binary", &b.to_string()]);
        }
        cmd.arg(format!("https://api.cloudflare.com/client/v4{path}"));
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("cannot run curl: {e}"))?;
        let cfg = format!("header = \"Authorization: Bearer {}\"\n", self.token);
        child.stdin.take().unwrap().write_all(cfg.as_bytes()).await.map_err(|e| e.to_string())?;
        let out = child.wait_with_output().await.map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!("curl: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        let v: Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("bad API response: {e}"))?;
        if v["success"].as_bool() != Some(true) {
            let msg = v["errors"][0]["message"].as_str().unwrap_or("unknown error");
            return Err(format!("Cloudflare {method} {path}: {msg}"));
        }
        Ok(v)
    }
}

pub struct Settings {
    pub token: String,
    pub account: String,
    pub tunnel: String,
}

/// Wire `host` to the local `service` through the tunnel. With `apply` false only
/// reads from Cloudflare and reports what would change.
pub async fn expose(s: &Settings, host: &str, service: &str, apply: bool, log: &mut dyn FnMut(&str)) -> Result<(), String> {
    if !valid_hostname(host) {
        return Err(format!("{host} is not a valid hostname"));
    }
    let api = Api { token: s.token.clone() };

    let mut zone = None;
    for cand in zone_candidates(host) {
        let r = api.call("GET", &format!("/zones?name={cand}"), None).await?;
        if let Some(z) = r["result"][0]["id"].as_str() {
            zone = Some((z.to_string(), cand));
            break;
        }
    }
    let (zone_id, zone_name) = zone.ok_or_else(|| format!("no Cloudflare zone found for {host} (does the token cover it?)"))?;
    log(&format!("zone {zone_name}"));

    let cfg_path = format!("/accounts/{}/cfd_tunnel/{}/configurations", s.account, s.tunnel);
    let cur = api.call("GET", &cfg_path, None).await?;
    let mut config = cur["result"]["config"].clone();
    if !config.is_object() {
        config = json!({});
    }
    let existing: Vec<Value> = config["ingress"].as_array().cloned().unwrap_or_default();
    match plan_ingress(&existing, host, service)? {
        None => log("tunnel route: already in place"),
        Some(new) => {
            log(&format!("tunnel route: add {host} -> {service}"));
            if apply {
                config["ingress"] = Value::Array(new);
                api.call("PUT", &cfg_path, Some(&json!({"config": config}))).await?;
            }
        }
    }

    let target = format!("{}.cfargotunnel.com", s.tunnel);
    let dns = api.call("GET", &format!("/zones/{zone_id}/dns_records?name={host}"), None).await?;
    match dns["result"].as_array().and_then(|a| a.first()) {
        Some(rec) if rec["type"] == "CNAME" && rec["content"] == target.as_str() => log("DNS: already points at the tunnel"),
        Some(rec) => {
            return Err(format!(
                "a DNS {} record for {host} already exists (content {}); not overwriting",
                rec["type"].as_str().unwrap_or("?"),
                rec["content"].as_str().unwrap_or("?")
            ));
        }
        None => {
            log(&format!("DNS: add proxied CNAME {host} -> {target}"));
            if apply {
                let body = json!({"type": "CNAME", "name": host, "content": target, "proxied": true});
                api.call("POST", &format!("/zones/{zone_id}/dns_records"), Some(&body)).await?;
            }
        }
    }
    log(if apply { "done" } else { "dry run only; re-run with --yes to apply" });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostnames() {
        for ok in ["vault.example.com", "a-b.c1.dev", "x.y"] {
            assert!(valid_hostname(ok), "{ok}");
        }
        for bad in ["", "localhost", "Vault.example.com", "-a.example.com", "a..b", "a.b.", "a b.c", "a/b.c", "a.b-", "é.com"] {
            assert!(!valid_hostname(bad), "{bad}");
        }
        assert!(!valid_hostname(&format!("{}.com", "a".repeat(64))));
    }

    #[test]
    fn zone_guesses_longest_first_and_skip_tld() {
        assert_eq!(zone_candidates("v.app.example.com"), ["app.example.com", "example.com"]);
        assert_eq!(zone_candidates("vault.example.com"), ["example.com"]);
        assert!(zone_candidates("example.com").is_empty());
    }

    #[test]
    fn ingress_inserted_before_catch_all() {
        let ex = vec![json!({"hostname": "panel.x.dev", "service": "http://127.0.0.1:8080"}), json!({"service": "http_status:404"})];
        let new = plan_ingress(&ex, "v.x.dev", "http://127.0.0.1:9000").unwrap().unwrap();
        assert_eq!(new.len(), 3);
        assert_eq!(new[1]["hostname"], "v.x.dev");
        assert_eq!(new[2]["service"], "http_status:404");
        assert_eq!(new[0], ex[0]);
        // empty and catch-all-less lists still end with a catch-all
        let new = plan_ingress(&[], "v.x.dev", "http://127.0.0.1:9000").unwrap().unwrap();
        assert_eq!(new.len(), 2);
        assert!(new[1].get("hostname").is_none());
    }

    #[test]
    fn ingress_idempotent_and_never_overwrites() {
        let ex = vec![json!({"hostname": "v.x.dev", "service": "http://127.0.0.1:9000"}), json!({"service": "http_status:404"})];
        assert!(plan_ingress(&ex, "v.x.dev", "http://127.0.0.1:9000").unwrap().is_none());
        assert!(plan_ingress(&ex, "v.x.dev", "http://127.0.0.1:9001").unwrap_err().contains("not overwriting"));
    }

    #[test]
    fn store_roundtrip() {
        let d = std::env::temp_dir().join(format!("dockwren-pub-{}", crate::auth::generate_password()));
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(read(&d), None);
        write(&d, "v.example.com").unwrap();
        assert_eq!(read(&d).as_deref(), Some("v.example.com"));
        assert!(write(&d, "Bad Host").is_err());
        write(&d, "").unwrap();
        assert_eq!(read(&d), None);
        write(&d, "").unwrap();
        std::fs::write(d.join(".env"), "A_PORT=1\nWEB_PORT='9000'\n").unwrap();
        assert_eq!(web_port(&d), Some(9000));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
