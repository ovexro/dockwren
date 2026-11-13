//! Public hostnames for apps. The hostname is stored per instance in `public.txt`
//! (not state.toml, which updates rewrite). `expose` wires it up in Cloudflare:
//! a tunnel ingress rule plus a proxied CNAME. It needs a scoped API token, either
//! in the environment or pasted in the web panel (kept in `cloudflare.json`, 0600).

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
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

pub fn read(dir: &Path) -> Option<String> {
    let h = std::fs::read_to_string(dir.join(FILE))
        .ok()?
        .trim()
        .to_string();
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
        return Err(
            "not a valid hostname (lower-case letters, digits, hyphens, at least one dot)".into(),
        );
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
    (1..labels.len().saturating_sub(1))
        .map(|i| labels[i..].join("."))
        .collect()
}

/// New ingress list with a rule for `host` inserted before the catch-all.
/// `Ok(None)` when an identical rule already exists; Err when the hostname
/// is routed somewhere else (we never overwrite).
fn plan_ingress(
    existing: &[Value],
    host: &str,
    service: &str,
) -> Result<Option<Vec<Value>>, String> {
    for r in existing {
        if r.get("hostname").and_then(Value::as_str) == Some(host) {
            return match r.get("service").and_then(Value::as_str) {
                Some(s) if s == service => Ok(None),
                other => Err(format!(
                    "{host} already routes to {} in this tunnel; not overwriting",
                    other.unwrap_or("?")
                )),
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

/// Ingress list without the rule for `host`. `None` when there is no such rule.
fn plan_removal(existing: &[Value], host: &str) -> Option<Vec<Value>> {
    let out: Vec<Value> = existing
        .iter()
        .filter(|r| r.get("hostname").and_then(Value::as_str) != Some(host))
        .cloned()
        .collect();
    (out.len() != existing.len()).then_some(out)
}

struct Api {
    token: String,
}

impl Api {
    /// One Cloudflare API call through curl. The token goes in on stdin as a curl
    /// config, so it never shows up in the process list.
    async fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let mut cmd = tokio::process::Command::new("curl");
        cmd.args([
            "-sS",
            "--max-time",
            "30",
            "-K",
            "-",
            "-X",
            method,
            "-H",
            "Content-Type: application/json",
        ]);
        if let Some(b) = body {
            cmd.args(["--data-binary", &b.to_string()]);
        }
        cmd.arg(format!("https://api.cloudflare.com/client/v4{path}"));
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("cannot run curl: {e}"))?;
        let cfg = format!("header = \"Authorization: Bearer {}\"\n", self.token);
        child
            .stdin
            .take()
            .unwrap()
            .write_all(cfg.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let out = child.wait_with_output().await.map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!(
                "curl: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let v: Value =
            serde_json::from_slice(&out.stdout).map_err(|e| format!("bad API response: {e}"))?;
        if v["success"].as_bool() != Some(true) {
            let msg = v["errors"][0]["message"]
                .as_str()
                .unwrap_or("unknown error");
            return Err(format!("Cloudflare {method} {path}: {msg}"));
        }
        Ok(v)
    }
}

/// Name of the Access application we create for `host`; used to recognise our own on removal.
fn access_name(host: &str) -> String {
    format!("Dockwren: {host}")
}

/// Plausible e-mail address (one `@`, no spaces or quotes); Cloudflare does the real check.
pub fn valid_email(e: &str) -> bool {
    let Some((local, domain)) = e.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.contains('@')
        && !e
            .chars()
            .any(|c| c.is_whitespace() || c == '"' || c == '\\')
}

#[derive(Debug, PartialEq, Eq)]
enum AccessPlan {
    Create,
    Present,
}

/// Decide what to do about the Access application for `host`. Never touches one we did not create.
fn plan_access(apps: &[Value], host: &str) -> Result<AccessPlan, String> {
    match apps.iter().find(|a| a["domain"] == host) {
        None => Ok(AccessPlan::Create),
        Some(a) if a["name"] == access_name(host).as_str() => Ok(AccessPlan::Present),
        Some(a) => Err(format!(
            "an Access application ({}) already covers {host}; not touching it",
            a["name"].as_str().unwrap_or("?")
        )),
    }
}

/// Body for a new self-hosted Access application that lets only `emails` in.
fn access_body(host: &str, emails: &[String]) -> Value {
    json!({
        "name": access_name(host),
        "domain": host,
        "type": "self_hosted",
        "session_duration": "24h",
        "policies": [{
            "name": "Owner only",
            "decision": "allow",
            "include": emails.iter().map(|e| json!({"email": {"email": e}})).collect::<Vec<_>>(),
        }],
    })
}

pub struct Settings {
    pub token: String,
    pub account: String,
    pub tunnel: String,
}

impl Settings {
    /// Cloudflare settings: the environment wins; otherwise what was pasted in the panel.
    /// `None` unless token, account and tunnel are all known (opt-in).
    pub fn from_env() -> Option<Self> {
        Self::from_environment().or_else(|| {
            let v = load_saved();
            let get = |x: Option<String>| x.filter(|v| !v.is_empty());
            Some(Self {
                token: get(v.token)?,
                account: get(v.account)?,
                tunnel: get(v.tunnel)?,
            })
        })
    }

    /// Only the process environment (set by the server admin).
    pub fn from_environment() -> Option<Self> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Some(Self {
            token: get("CLOUDFLARE_API_TOKEN")?,
            account: get("CLOUDFLARE_ACCOUNT_ID")?,
            tunnel: get("DOCKWREN_TUNNEL_ID")?,
        })
    }
}

/// What the web panel keeps after the user pastes a token.
#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
pub struct Saved {
    pub token: Option<String>,
    pub account: Option<String>,
    pub tunnel: Option<String>,
}

fn store_path() -> std::path::PathBuf {
    crate::config::Config::from_env()
        .data_dir
        .join("cloudflare.json")
}

pub fn load_saved() -> Saved {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_saved(v: &Saved) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = store_path();
    let tmp = path.with_extension("json.new");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| e.to_string())?;
    f.write_all(
        serde_json::to_string(v)
            .map_err(|e| e.to_string())?
            .as_bytes(),
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

pub fn forget_saved() -> Result<(), String> {
    match std::fs::remove_file(store_path()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
        _ => Ok(()),
    }
}

/// What a token can see.
pub struct Probe {
    pub account: String,
    pub zones: Vec<String>,
    /// (id, name) of remotely managed tunnels in that account.
    pub tunnels: Vec<(String, String)>,
}

/// Check a token against Cloudflare: list the domains it covers, find the account and
/// the tunnels. Read-only.
pub async fn probe(token: &str) -> Result<Probe, String> {
    let api = Api {
        token: token.to_string(),
    };
    let z = api
        .call("GET", "/zones?per_page=50", None)
        .await
        .map_err(|e| {
            if e.contains("Invalid") || e.contains("Authentication") {
                "Cloudflare does not accept this token. Copy the whole token again (no spaces) and check it is still active.".to_string()
            } else {
                e
            }
        })?;
    let zones: Vec<Value> = z["result"].as_array().cloned().unwrap_or_default();
    if zones.is_empty() {
        return Err("the token works but covers no domain: add Zone / Zone / Read and pick your domain under Zone Resources".into());
    }
    let account = zones[0]["account"]["id"]
        .as_str()
        .ok_or("could not read the account id from Cloudflare")?
        .to_string();
    let t = api
        .call(
            "GET",
            &format!("/accounts/{account}/cfd_tunnel?is_deleted=false&per_page=50"),
            None,
        )
        .await
        .map_err(|e| format!("{e} (the token needs Account / Cloudflare Tunnel / Edit)"))?;
    let tunnels = t["result"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter(|x| x["config_src"].as_str().is_none_or(|c| c == "cloudflare"))
        .filter_map(|x| {
            Some((
                x["id"].as_str()?.to_string(),
                x["name"].as_str()?.to_string(),
            ))
        })
        .collect();
    Ok(Probe {
        account,
        zones: zones
            .iter()
            .filter_map(|z| z["name"].as_str().map(String::from))
            .collect(),
        tunnels,
    })
}

/// After an install that was given a public hostname: record it and, when Cloudflare
/// settings are configured, publish it. A publishing failure is logged, never fatal
/// (the app is installed and running; `dockwren expose` can be retried).
pub async fn publish_after_install(
    dir: &Path,
    name: &str,
    host: &str,
    log: &mut (dyn FnMut(&str) + Send),
) {
    if let Err(e) = write(dir, host) {
        log(&format!("could not record hostname: {e}"));
        return;
    }
    let Some(settings) = Settings::from_env() else {
        log(&format!(
            "hostname recorded; Cloudflare is not configured here. Publish it with: dockwren expose {name} --yes"
        ));
        return;
    };
    let Some(port) = web_port(dir) else {
        log("this app has no WEB_PORT to publish");
        return;
    };
    log(&format!("publishing https://{host}"));
    let result = expose(&settings, host, &service_url(port), &[], true, &mut |s| {
        log(&format!("  {s}"))
    })
    .await;
    if let Err(e) = result {
        log(&format!(
            "publishing failed: {e}. The app is installed; retry with: dockwren expose {name} --yes"
        ));
    }
}

/// Wire `host` to the local `service` through the tunnel. With `apply` false only
/// reads from Cloudflare and reports what would change.
pub async fn expose(
    s: &Settings,
    host: &str,
    service: &str,
    access: &[String],
    apply: bool,
    log: &mut (dyn FnMut(&str) + Send),
) -> Result<(), String> {
    if !valid_hostname(host) {
        return Err(format!("{host} is not a valid hostname"));
    }
    let api = Api {
        token: s.token.clone(),
    };

    let mut zone = None;
    for cand in zone_candidates(host) {
        let r = api
            .call("GET", &format!("/zones?name={cand}"), None)
            .await?;
        if let Some(z) = r["result"][0]["id"].as_str() {
            zone = Some((z.to_string(), cand));
            break;
        }
    }
    let (zone_id, zone_name) = zone
        .ok_or_else(|| format!("no Cloudflare zone found for {host} (does the token cover it?)"))?;
    log(&format!("zone {zone_name}"));

    let cfg_path = format!(
        "/accounts/{}/cfd_tunnel/{}/configurations",
        s.account, s.tunnel
    );
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
                api.call("PUT", &cfg_path, Some(&json!({"config": config})))
                    .await?;
            }
        }
    }

    // Access goes in before the DNS record so the app is never reachable unprotected.
    if !access.is_empty() {
        let apps = api
            .call("GET", &format!("/accounts/{}/access/apps", s.account), None)
            .await?;
        let apps: Vec<Value> = apps["result"].as_array().cloned().unwrap_or_default();
        match plan_access(&apps, host)? {
            AccessPlan::Present => log("Access: application already in place (not changed)"),
            AccessPlan::Create => {
                log(&format!("Access: only {} may sign in", access.join(", ")));
                if apply {
                    api.call(
                        "POST",
                        &format!("/accounts/{}/access/apps", s.account),
                        Some(&access_body(host, access)),
                    )
                    .await?;
                }
            }
        }
    }

    let target = format!("{}.cfargotunnel.com", s.tunnel);
    let dns = api
        .call(
            "GET",
            &format!("/zones/{zone_id}/dns_records?name={host}"),
            None,
        )
        .await?;
    match dns["result"].as_array().and_then(|a| a.first()) {
        Some(rec) if rec["type"] == "CNAME" && rec["content"] == target.as_str() => {
            log("DNS: already points at the tunnel")
        }
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
                let body =
                    json!({"type": "CNAME", "name": host, "content": target, "proxied": true});
                api.call(
                    "POST",
                    &format!("/zones/{zone_id}/dns_records"),
                    Some(&body),
                )
                .await?;
            }
        }
    }
    log(if apply {
        "done"
    } else {
        "dry run only; re-run with --yes to apply"
    });
    Ok(())
}

/// Undo `expose`: drop the tunnel rule and the CNAME. Only a CNAME that points at
/// our tunnel is deleted; anything else under that name is left alone.
pub async fn unexpose(
    s: &Settings,
    host: &str,
    apply: bool,
    log: &mut dyn FnMut(&str),
) -> Result<(), String> {
    if !valid_hostname(host) {
        return Err(format!("{host} is not a valid hostname"));
    }
    let api = Api {
        token: s.token.clone(),
    };

    let mut zone = None;
    for cand in zone_candidates(host) {
        let r = api
            .call("GET", &format!("/zones?name={cand}"), None)
            .await?;
        if let Some(z) = r["result"][0]["id"].as_str() {
            zone = Some((z.to_string(), cand));
            break;
        }
    }
    let (zone_id, zone_name) = zone
        .ok_or_else(|| format!("no Cloudflare zone found for {host} (does the token cover it?)"))?;
    log(&format!("zone {zone_name}"));

    let cfg_path = format!(
        "/accounts/{}/cfd_tunnel/{}/configurations",
        s.account, s.tunnel
    );
    let cur = api.call("GET", &cfg_path, None).await?;
    let mut config = cur["result"]["config"].clone();
    let existing: Vec<Value> = config["ingress"].as_array().cloned().unwrap_or_default();
    match plan_removal(&existing, host) {
        None => log("tunnel route: not present"),
        Some(new) => {
            log(&format!("tunnel route: remove {host}"));
            if apply {
                config["ingress"] = Value::Array(new);
                api.call("PUT", &cfg_path, Some(&json!({"config": config})))
                    .await?;
            }
        }
    }

    let target = format!("{}.cfargotunnel.com", s.tunnel);
    let dns = api
        .call(
            "GET",
            &format!("/zones/{zone_id}/dns_records?name={host}"),
            None,
        )
        .await?;
    match dns["result"].as_array().and_then(|a| a.first()) {
        None => log("DNS: no record"),
        Some(rec) if rec["type"] == "CNAME" && rec["content"] == target.as_str() => {
            log(&format!("DNS: delete CNAME {host} -> {target}"));
            if apply {
                let id = rec["id"].as_str().ok_or("DNS record has no id")?;
                api.call(
                    "DELETE",
                    &format!("/zones/{zone_id}/dns_records/{id}"),
                    None,
                )
                .await?;
            }
        }
        Some(rec) => log(&format!(
            "DNS: leaving the {} record ({}) alone, it does not point at this tunnel",
            rec["type"].as_str().unwrap_or("?"),
            rec["content"].as_str().unwrap_or("?")
        )),
    }

    // Last, so the app is not exposed unprotected while this runs.
    let apps = api
        .call("GET", &format!("/accounts/{}/access/apps", s.account), None)
        .await?;
    let apps: Vec<Value> = apps["result"].as_array().cloned().unwrap_or_default();
    match apps.iter().find(|a| a["domain"] == host) {
        None => log("Access: no application"),
        Some(a) if a["name"] == access_name(host).as_str() => {
            log("Access: delete our application");
            if apply {
                let id = a["id"].as_str().ok_or("Access application has no id")?;
                api.call(
                    "DELETE",
                    &format!("/accounts/{}/access/apps/{id}", s.account),
                    None,
                )
                .await?;
            }
        }
        Some(a) => log(&format!(
            "Access: leaving {} alone, it is not ours",
            a["name"].as_str().unwrap_or("?")
        )),
    }
    log(if apply {
        "done"
    } else {
        "dry run only; re-run with --yes to apply"
    });
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
        for bad in [
            "",
            "localhost",
            "Vault.example.com",
            "-a.example.com",
            "a..b",
            "a.b.",
            "a b.c",
            "a/b.c",
            "a.b-",
            "é.com",
        ] {
            assert!(!valid_hostname(bad), "{bad}");
        }
        assert!(!valid_hostname(&format!("{}.com", "a".repeat(64))));
    }

    #[test]
    fn zone_guesses_longest_first_and_skip_tld() {
        assert_eq!(
            zone_candidates("v.app.example.com"),
            ["app.example.com", "example.com"]
        );
        assert_eq!(zone_candidates("vault.example.com"), ["example.com"]);
        assert!(zone_candidates("example.com").is_empty());
    }

    #[test]
    fn ingress_inserted_before_catch_all() {
        let ex = vec![
            json!({"hostname": "panel.x.dev", "service": "http://127.0.0.1:8080"}),
            json!({"service": "http_status:404"}),
        ];
        let new = plan_ingress(&ex, "v.x.dev", "http://127.0.0.1:9000")
            .unwrap()
            .unwrap();
        assert_eq!(new.len(), 3);
        assert_eq!(new[1]["hostname"], "v.x.dev");
        assert_eq!(new[2]["service"], "http_status:404");
        assert_eq!(new[0], ex[0]);
        // empty and catch-all-less lists still end with a catch-all
        let new = plan_ingress(&[], "v.x.dev", "http://127.0.0.1:9000")
            .unwrap()
            .unwrap();
        assert_eq!(new.len(), 2);
        assert!(new[1].get("hostname").is_none());
    }

    #[test]
    fn ingress_idempotent_and_never_overwrites() {
        let ex = vec![
            json!({"hostname": "v.x.dev", "service": "http://127.0.0.1:9000"}),
            json!({"service": "http_status:404"}),
        ];
        assert!(
            plan_ingress(&ex, "v.x.dev", "http://127.0.0.1:9000")
                .unwrap()
                .is_none()
        );
        assert!(
            plan_ingress(&ex, "v.x.dev", "http://127.0.0.1:9001")
                .unwrap_err()
                .contains("not overwriting")
        );
    }

    #[test]
    fn ingress_removal_keeps_the_rest() {
        let ex = vec![
            json!({"hostname": "a.x.dev", "service": "http://127.0.0.1:1"}),
            json!({"hostname": "v.x.dev", "service": "http://127.0.0.1:2"}),
            json!({"service": "http_status:404"}),
        ];
        let new = plan_removal(&ex, "v.x.dev").unwrap();
        assert_eq!(new, vec![ex[0].clone(), ex[2].clone()]);
        assert!(plan_removal(&ex, "gone.x.dev").is_none());
    }

    #[test]
    fn access_plan_never_touches_foreign_apps() {
        let ours = json!({"name": "Dockwren: a.example.com", "domain": "a.example.com"});
        let foreign = json!({"name": "Hand made", "domain": "b.example.com"});
        assert_eq!(plan_access(&[], "a.example.com"), Ok(AccessPlan::Create));
        assert_eq!(
            plan_access(&[ours.clone(), foreign.clone()], "a.example.com"),
            Ok(AccessPlan::Present)
        );
        assert!(plan_access(&[foreign], "b.example.com").is_err());
    }

    #[test]
    fn access_body_lists_each_email() {
        let b = access_body(
            "a.example.com",
            &["x@example.com".into(), "y@example.com".into()],
        );
        assert_eq!(b["domain"], "a.example.com");
        assert_eq!(b["policies"][0]["decision"], "allow");
        assert_eq!(
            b["policies"][0]["include"][1]["email"]["email"],
            "y@example.com"
        );
    }

    #[test]
    fn emails() {
        assert!(valid_email("a@b.co"));
        for bad in ["", "a", "a@b", "@b.co", "a@@b.co", "a b@c.de", "a\"@b.co"] {
            assert!(!valid_email(bad), "{bad}");
        }
    }

    #[test]
    fn store_roundtrip() {
        let d =
            std::env::temp_dir().join(format!("dockwren-pub-{}", crate::auth::generate_password()));
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
