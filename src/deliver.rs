//! Delivery channels: push notifications out of the panel (ntfy, webhook).
//!
//! A background loop looks every 20 seconds for open events the channels have not received at
//! their current severity (`Store::pending_delivery`), sends them through curl and marks them
//! as sent. Sending is best effort: a failed send is retried a few times, then dropped.
//! Addresses and tokens are secrets: they live in `notify-channels.json` (mode 0600), reach curl
//! through stdin (never the process list) and are never shown again in the UI.

use askama::Template;
use axum::{
    extract::{Path as UrlPath, State as AxState},
    response::Html,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

use crate::notify::{Event, Severity, Shared};

pub const FILE: &str = "notify-channels.json";
const DEFAULT_NTFY: &str = "https://ntfy.sh";
const TICK: Duration = Duration::from_secs(20);
const MAX_ATTEMPTS: u8 = 3;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub ntfy_server: String,
    #[serde(default)]
    pub ntfy_topic: String,
    #[serde(default)]
    pub ntfy_token: String,
    #[serde(default)]
    pub webhook_url: String,
    #[serde(default = "warning")]
    pub min: Severity,
}

fn warning() -> Severity {
    Severity::Warning
}

impl Default for Config {
    fn default() -> Self {
        Config {
            ntfy_server: String::new(),
            ntfy_topic: String::new(),
            ntfy_token: String::new(),
            webhook_url: String::new(),
            min: Severity::Warning,
        }
    }
}

impl Config {
    pub fn has_ntfy(&self) -> bool {
        !self.ntfy_topic.is_empty()
    }
    pub fn has_webhook(&self) -> bool {
        !self.webhook_url.is_empty()
    }
    pub fn any(&self) -> bool {
        self.has_ntfy() || self.has_webhook()
    }
}

/// Text that is safe inside a curl config string and a URL: printable ASCII, no quotes, backslash,
/// spaces or control characters.
fn plain(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_ascii_graphic() && c != '"' && c != '\\')
}

/// `http(s)://host[:port][/path]`, nothing else.
pub fn valid_url(u: &str) -> bool {
    let Some(rest) = u
        .strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"))
    else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    !host.is_empty() && !host.contains('@') && plain(u) && u.len() <= 500
}

fn valid_topic(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 64
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The host part of a URL, for showing which webhook is saved without revealing the path.
fn host_of(u: &str) -> String {
    u.split("://")
        .nth(1)
        .and_then(|r| r.split(['/', '?', '#']).next())
        .unwrap_or("")
        .to_string()
}

fn priority(s: Severity) -> u8 {
    match s {
        Severity::Info => 3,
        Severity::Warning => 4,
        Severity::Critical => 5,
    }
}

/// ntfy JSON publish body (the topic goes in the body, so titles may use any character).
fn ntfy_body(e: &Event, topic: &str) -> String {
    let tag = match e.severity {
        Severity::Info => "information_source",
        Severity::Warning => "warning",
        Severity::Critical => "rotating_light",
    };
    serde_json::json!({
        "topic": topic,
        "title": e.title,
        "message": e.body.clone().unwrap_or_else(|| e.title.clone()),
        "priority": priority(e.severity),
        "tags": [tag],
    })
    .to_string()
}

fn webhook_body(e: &Event) -> String {
    let text = match &e.body {
        Some(b) => format!("[{}] {}\n{}", e.severity.label(), e.title, b),
        None => format!("[{}] {}", e.severity.label(), e.title),
    };
    serde_json::json!({
        "id": e.id,
        "ts": e.ts,
        "source": e.source,
        "kind": e.kind,
        "severity": e.severity.slug(),
        "title": e.title,
        "body": e.body,
        "link": e.link,
        "text": text,
        "content": text,
    })
    .to_string()
}

/// One POST through curl. URL and auth header go in on stdin as a curl config.
async fn post(url: &str, bearer: Option<&str>, body: &str) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new("curl");
    cmd.args([
        "-sS",
        "-f",
        "-o",
        "/dev/null",
        "--max-time",
        "15",
        "--proto",
        "=http,https",
        "-K",
        "-",
        "-X",
        "POST",
        "-H",
        "Content-Type: application/json",
        "-A",
        concat!("dockwren/", env!("CARGO_PKG_VERSION")),
        "--data-binary",
        body,
    ]);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("cannot run curl: {e}"))?;
    let mut cfg = format!("url = \"{url}\"\n");
    if let Some(t) = bearer {
        cfg.push_str(&format!("header = \"Authorization: Bearer {t}\"\n"));
    }
    child
        .stdin
        .take()
        .unwrap()
        .write_all(cfg.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let out = child.wait_with_output().await.map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    // curl's own message can quote the URL (a secret for webhooks): keep only the reason.
    let err = String::from_utf8_lossy(&out.stderr);
    let reason = err
        .trim()
        .rsplit("): ")
        .next()
        .unwrap_or("request failed")
        .trim();
    Err(clean_reason(reason, url))
}

fn clean_reason(reason: &str, url: &str) -> String {
    reason
        .replace(url, "<address>")
        .chars()
        .filter(|c| !c.is_control())
        .take(160)
        .collect()
}

/// Send one event to every configured channel. Errors are per channel, labelled.
async fn send(cfg: &Config, e: &Event) -> Vec<String> {
    let mut errors = Vec::new();
    if cfg.has_ntfy() {
        let server = if cfg.ntfy_server.is_empty() {
            DEFAULT_NTFY
        } else {
            cfg.ntfy_server.as_str()
        };
        let body = ntfy_body(e, &cfg.ntfy_topic);
        let token = (!cfg.ntfy_token.is_empty()).then_some(cfg.ntfy_token.as_str());
        if let Err(m) = post(server.trim_end_matches('/'), token, &body).await {
            errors.push(format!("ntfy: {m}"));
        }
    }
    if cfg.has_webhook()
        && let Err(m) = post(&cfg.webhook_url, None, &webhook_body(e)).await
    {
        errors.push(format!("webhook: {m}"));
    }
    errors
}

/// Background loop: deliver what is pending.
pub fn spawn(shared: Shared) {
    tokio::spawn(async move {
        let mut attempts: HashMap<(u64, Severity), u8> = HashMap::new();
        loop {
            tokio::time::sleep(TICK).await;
            let (cfg, pending) = {
                let s = shared.lock().unwrap();
                if !s.channels.any() {
                    attempts.clear();
                    continue;
                }
                (s.channels.clone(), s.pending_delivery(s.channels.min))
            };
            for e in pending {
                let errors = send(&cfg, &e).await;
                let n = attempts.entry((e.id, e.severity)).or_insert(0);
                *n += 1;
                if errors.is_empty() || *n >= MAX_ATTEMPTS {
                    attempts.remove(&(e.id, e.severity));
                    shared.lock().unwrap().mark_notified(e.id, e.severity);
                }
            }
        }
    });
}

// ---- web UI ----

#[derive(Template)]
#[template(path = "notify_channels.html")]
struct ChannelsTemplate {
    message: Option<(bool, String)>,
    ntfy_server: String,
    ntfy_topic: String,
    has_token: bool,
    has_ntfy: bool,
    has_webhook: bool,
    webhook_host: String,
    configured: bool,
    min: &'static str,
}

fn render(c: &Config, message: Option<(bool, String)>) -> Html<String> {
    Html(
        ChannelsTemplate {
            message,
            ntfy_server: c.ntfy_server.clone(),
            ntfy_topic: c.ntfy_topic.clone(),
            has_token: !c.ntfy_token.is_empty(),
            has_ntfy: c.has_ntfy(),
            has_webhook: c.has_webhook(),
            webhook_host: host_of(&c.webhook_url),
            configured: c.any(),
            min: c.min.slug(),
        }
        .render()
        .expect("channels template renders"),
    )
}

pub async fn show(AxState(shared): AxState<Shared>) -> Html<String> {
    let c = shared.lock().unwrap().channels.clone();
    render(&c, None)
}

#[derive(Deserialize)]
pub struct ChannelsForm {
    #[serde(default)]
    ntfy_server: String,
    #[serde(default)]
    ntfy_topic: String,
    #[serde(default)]
    ntfy_token: String,
    #[serde(default)]
    webhook_url: String,
    #[serde(default)]
    min: String,
}

/// Apply a submitted form on top of the saved settings. Empty secret fields keep what is saved.
fn merged(old: &Config, f: &ChannelsForm) -> Result<Config, String> {
    let mut c = old.clone();
    let topic = f.ntfy_topic.trim();
    let server = f.ntfy_server.trim().trim_end_matches('/');
    if topic.is_empty() {
        c.ntfy_topic.clear();
        c.ntfy_server.clear();
        c.ntfy_token.clear();
    } else {
        if !valid_topic(topic) {
            return Err("The ntfy topic may only use letters, digits, - and _ (up to 64).".into());
        }
        if !server.is_empty() && !valid_url(server) {
            return Err("The ntfy server must be a full address such as https://ntfy.sh.".into());
        }
        let token = f.ntfy_token.trim();
        if !token.is_empty() {
            if !plain(token) || token.len() > 200 {
                return Err("That ntfy token has characters a token cannot contain.".into());
            }
            c.ntfy_token = token.to_string();
        }
        c.ntfy_topic = topic.to_string();
        c.ntfy_server = server.to_string();
    }
    let hook = f.webhook_url.trim();
    if !hook.is_empty() {
        if !valid_url(hook) {
            return Err("The webhook address must start with https:// (or http://).".into());
        }
        c.webhook_url = hook.to_string();
    }
    c.min = match f.min.as_str() {
        "info" => Severity::Info,
        "critical" => Severity::Critical,
        _ => Severity::Warning,
    };
    Ok(c)
}

pub async fn save(
    AxState(shared): AxState<Shared>,
    axum::Form(f): axum::Form<ChannelsForm>,
) -> Html<String> {
    let mut s = shared.lock().unwrap();
    match merged(&s.channels, &f) {
        Ok(c) => {
            s.set_channels(c);
            let c = s.channels.clone();
            render(&c, Some((true, "Saved.".into())))
        }
        Err(m) => {
            let c = s.channels.clone();
            render(&c, Some((false, m)))
        }
    }
}

pub async fn remove(
    AxState(shared): AxState<Shared>,
    UrlPath(which): UrlPath<String>,
) -> Html<String> {
    let mut s = shared.lock().unwrap();
    let mut c = s.channels.clone();
    match which.as_str() {
        "ntfy" => {
            c.ntfy_topic.clear();
            c.ntfy_server.clear();
            c.ntfy_token.clear();
        }
        "webhook" => c.webhook_url.clear(),
        _ => {}
    }
    s.set_channels(c);
    let c = s.channels.clone();
    render(&c, Some((true, "Removed.".into())))
}

pub async fn test(AxState(shared): AxState<Shared>) -> Html<String> {
    let cfg = shared.lock().unwrap().channels.clone();
    let e = Event {
        id: 0,
        ts: crate::metrics::now_unix(),
        source: "dockwren".into(),
        kind: "test".into(),
        severity: Severity::Info,
        title: "Test notification from Dockwren".into(),
        body: Some("If you can read this, delivery works.".into()),
        link: None,
        key: "test".into(),
        state: crate::notify::State::New,
        notified: None,
    };
    let errors = send(&cfg, &e).await;
    let msg = if errors.is_empty() {
        (
            true,
            "Test sent. It should arrive in a few seconds.".to_string(),
        )
    } else {
        (false, errors.join("; "))
    };
    render(&cfg, Some(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::{NewEvent, Store};

    fn form(topic: &str, hook: &str, min: &str) -> ChannelsForm {
        ChannelsForm {
            ntfy_server: String::new(),
            ntfy_topic: topic.into(),
            ntfy_token: String::new(),
            webhook_url: hook.into(),
            min: min.into(),
        }
    }

    #[test]
    fn urls_and_topics_are_checked() {
        assert!(valid_url("https://ntfy.sh"));
        assert!(valid_url("http://10.0.0.5:8080/hook?a=b"));
        assert!(!valid_url("ftp://x.example/"));
        assert!(!valid_url("https://"));
        assert!(!valid_url("https://user@x.example/"));
        assert!(!valid_url("https://x.example/\"quote"));
        assert!(!valid_url("https://x.example/a b"));
        assert!(!valid_url("https://x.example/\nheader = x"));
        assert!(valid_topic("my-Topic_1"));
        assert!(!valid_topic("a/b"));
        assert!(!valid_topic(""));
    }

    #[test]
    fn form_merges_and_keeps_saved_secrets() {
        let old = Config {
            ntfy_token: "tk_abc".into(),
            webhook_url: "https://hooks.example/secret".into(),
            ..Config::default()
        };
        let c = merged(&old, &form("topic1", "", "critical")).unwrap();
        assert_eq!(c.ntfy_topic, "topic1");
        assert_eq!(c.ntfy_token, "tk_abc");
        assert_eq!(c.webhook_url, "https://hooks.example/secret");
        assert_eq!(c.min, Severity::Critical);
        let c = merged(&c, &form("", "", "info")).unwrap();
        assert!(!c.has_ntfy() && c.ntfy_token.is_empty());
        assert!(c.has_webhook());
        assert!(merged(&c, &form("bad topic", "", "info")).is_err());
        assert!(merged(&c, &form("", "javascript:alert(1)", "info")).is_err());
    }

    #[test]
    fn payloads_carry_the_text() {
        let e = Event {
            id: 3,
            ts: 5,
            source: "apps".into(),
            kind: "job.failed".into(),
            severity: Severity::Warning,
            title: "update \"gitea\" failed".into(),
            body: Some("line1\nline2".into()),
            link: Some("/apps".into()),
            key: "k".into(),
            state: crate::notify::State::New,
            notified: None,
        };
        let w: serde_json::Value = serde_json::from_str(&webhook_body(&e)).unwrap();
        assert_eq!(w["severity"], "warning");
        assert_eq!(w["title"], "update \"gitea\" failed");
        assert!(w["text"].as_str().unwrap().starts_with("[Warning] update"));
        let n: serde_json::Value = serde_json::from_str(&ntfy_body(&e, "t")).unwrap();
        assert_eq!(n["priority"], 4);
        assert_eq!(n["message"], "line1\nline2");
    }

    #[test]
    fn curl_errors_do_not_leak_the_address() {
        let url = "https://hooks.example/T000/secret";
        let m = clean_reason(
            &format!("curl: (22) The requested URL returned error: 404 {url}"),
            url,
        );
        assert!(!m.contains("secret"));
    }

    #[test]
    fn pending_follows_severity_and_sent_state() {
        let mut s = Store::memory();
        s.raise(NewEvent::new("a", "k", Severity::Info, "i").key("i"));
        let (w, _) = s.raise(NewEvent::new("a", "k", Severity::Warning, "w").key("w"));
        assert_eq!(s.pending_delivery(Severity::Warning).len(), 1);
        assert_eq!(s.pending_delivery(Severity::Info).len(), 2);
        s.mark_notified(w, Severity::Warning);
        assert_eq!(s.pending_delivery(Severity::Warning).len(), 0);
        // escalation sends again
        s.raise(NewEvent::new("a", "k", Severity::Critical, "w").key("w"));
        assert_eq!(s.pending_delivery(Severity::Warning).len(), 1);
        // a resolved event is never sent
        s.resolve("w");
        assert_eq!(s.pending_delivery(Severity::Info).len(), 1);
    }

    #[test]
    fn turning_a_channel_on_does_not_flood_it() {
        let mut s = Store::memory();
        s.raise(NewEvent::new("a", "k", Severity::Critical, "old").key("old"));
        s.set_channels(Config {
            ntfy_topic: "t".into(),
            ..Config::default()
        });
        assert!(s.pending_delivery(Severity::Info).is_empty());
        s.raise(NewEvent::new("a", "k", Severity::Critical, "new").key("new"));
        assert_eq!(s.pending_delivery(Severity::Info).len(), 1);
    }

    /// One-shot local HTTP server: answers `status`, returns what it received.
    fn serve_once(status: &'static str) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/hook-secret", l.local_addr().unwrap());
        let h = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = c.read(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
                let t = String::from_utf8_lossy(&got).to_string();
                if let Some((head, body)) = t.split_once("\r\n\r\n") {
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase()
                                .strip_prefix("content-length: ")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if body.len() >= len {
                        break;
                    }
                }
            }
            let _ = write!(
                c,
                "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            String::from_utf8_lossy(&got).to_string()
        });
        (url, h)
    }

    #[tokio::test]
    async fn curl_sends_body_and_bearer_and_reports_failures_without_the_address() {
        let (url, h) = serve_once("200 OK");
        post(&url, Some("tk_secret"), "{\"a\":1}").await.unwrap();
        let req = h.join().unwrap();
        assert!(req.starts_with("POST /hook-secret"), "{req}");
        assert!(req.contains("Authorization: Bearer tk_secret"));
        assert!(req.contains("Content-Type: application/json"));
        assert!(req.ends_with("{\"a\":1}"));
        let (url, h) = serve_once("404 Not Found");
        let err = post(&url, None, "{}").await.unwrap_err();
        h.join().unwrap();
        assert!(err.contains("404"), "{err}");
        assert!(
            !err.contains("hook-secret") && !err.contains("127.0.0.1"),
            "{err}"
        );
    }
}
