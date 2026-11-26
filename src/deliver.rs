//! Delivery channels: push notifications out of the panel (ntfy, webhook, Telegram).
//!
//! One background loop per channel asks the store what that channel is owed
//! (`Store::outbox`) and sends it through curl. A message counts as sent only when the
//! channel accepted it. While a channel cannot be reached (internet down, for example) the
//! loop retries with growing pauses, up to `MAX_BACKOFF`, for up to `notify::GIVE_UP_SECS`;
//! what is owed is kept in the notification log, so it survives restarts. An alert whose
//! problem ends before it could be sent becomes one summary message. Failures are logged.
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
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

use crate::notify::{Event, Owed, Severity, Shared};
use crate::wording::{self, Message, What};

pub const FILE: &str = "notify-channels.json";
const DEFAULT_NTFY: &str = "https://ntfy.sh";
const TELEGRAM_API: &str = "https://api.telegram.org";
/// How often a channel looks for something to send: short, so a power cut is told at once.
const TICK: Duration = Duration::from_secs(5);
const FIRST_BACKOFF: u64 = 10;
/// Longest pause between attempts, so messages go out soon after the internet is back.
const MAX_BACKOFF: u64 = 60;
/// Most messages one channel sends in one go.
const BATCH: usize = 20;
const CHANNELS: [&str; 3] = ["ntfy", "webhook", "telegram"];

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
    /// Telegram bot token (from @BotFather): a secret, never shown again.
    #[serde(default)]
    pub telegram_token: String,
    /// Chat the bot writes to: a number (private chat, group, channel) or `@channelname`.
    #[serde(default)]
    pub telegram_chat: String,
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
            telegram_token: String::new(),
            telegram_chat: String::new(),
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
    pub fn has_telegram(&self) -> bool {
        !self.telegram_token.is_empty() && !self.telegram_chat.is_empty()
    }
    pub fn any(&self) -> bool {
        !self.enabled().is_empty()
    }
    /// The channels that are set up, by name.
    pub fn enabled(&self) -> Vec<&'static str> {
        CHANNELS.into_iter().filter(|c| self.has(c)).collect()
    }
    fn has(&self, channel: &str) -> bool {
        match channel {
            "ntfy" => self.has_ntfy(),
            "webhook" => self.has_webhook(),
            "telegram" => self.has_telegram(),
            _ => false,
        }
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

/// `123456:ABC-def_...` as BotFather hands it out.
fn valid_bot_token(t: &str) -> bool {
    let Some((id, secret)) = t.split_once(':') else {
        return false;
    };
    t.len() <= 200
        && !id.is_empty()
        && id.chars().all(|c| c.is_ascii_digit())
        && !secret.is_empty()
        && secret
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// A number (`123`, `-1001234567890`) or a public channel name (`@mychannel`).
fn valid_chat(c: &str) -> bool {
    if let Some(name) = c.strip_prefix('@') {
        // A name ending in "bot" is a bot (often the user's own): bots cannot be written to.
        return (5..=32).contains(&name.len())
            && !name.to_ascii_lowercase().ends_with("bot")
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    }
    let digits = c.strip_prefix('-').unwrap_or(c);
    !digits.is_empty() && digits.len() <= 20 && digits.chars().all(|c| c.is_ascii_digit())
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
fn ntfy_body(m: &Message, topic: &str) -> String {
    serde_json::json!({
        "topic": topic,
        "title": m.title,
        "message": m.body.clone().unwrap_or_else(|| m.title.clone()),
        "priority": priority(m.severity),
        "tags": [m.tag],
    })
    .to_string()
}

fn webhook_body(m: &Message) -> String {
    let text = match &m.body {
        Some(b) => format!("[{}] {}\n{}", m.severity.label(), m.title, b),
        None => format!("[{}] {}", m.severity.label(), m.title),
    };
    serde_json::json!({
        "id": m.id,
        "ts": m.ts,
        "source": m.source,
        "kind": m.kind,
        "severity": m.severity.slug(),
        "state": m.state,
        "since": m.since,
        "cleared_at": m.cleared_at,
        "title": m.title,
        "body": m.body,
        "link": m.link,
        "text": text,
        "content": text,
    })
    .to_string()
}

/// The character ntfy shows for a tag, for channels that have no tags.
fn emoji(tag: &str) -> &'static str {
    match tag {
        "white_check_mark" => "\u{2705}",
        "electric_plug" => "\u{1F50C}",
        "warning" => "\u{26A0}\u{FE0F}",
        "rotating_light" => "\u{1F6A8}",
        _ => "\u{2139}\u{FE0F}",
    }
}

/// Telegram `sendMessage` body: plain text (no markup, so nothing in a title can break it).
fn telegram_body(m: &Message, chat: &str) -> String {
    let mut text = format!("{} {}", emoji(m.tag), m.title);
    if let Some(b) = &m.body {
        text.push_str("\n\n");
        text.push_str(b);
    }
    let text: String = text.chars().take(3500).collect();
    serde_json::json!({
        "chat_id": chat,
        "text": text,
        "disable_web_page_preview": true,
    })
    .to_string()
}

fn telegram_url(base: &str, token: &str, method: &str) -> String {
    format!("{}/bot{token}/{method}", base.trim_end_matches('/'))
}

/// The chat that last wrote to the bot, so the admin does not have to look up a chat id.
fn chat_from_updates(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v["result"].as_array()?.iter().rev().find_map(|u| {
        ["message", "channel_post", "my_chat_member"]
            .iter()
            .find_map(|k| u[*k]["chat"]["id"].as_i64())
            .map(|id| id.to_string())
    })
}

/// One POST through curl. URL and auth header go in on stdin as a curl config.
async fn post(url: &str, bearer: Option<&str>, body: &str) -> Result<(), String> {
    post_out(url, bearer, body).await.map(|_| ())
}

/// Same, returning the response body.
async fn post_out(url: &str, bearer: Option<&str>, body: &str) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("curl");
    cmd.args([
        "-sS",
        "-f",
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
        .stdout(Stdio::piped())
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
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
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
    let mut text = reason.replace(url, "<address>");
    // curl also names the server on its own ("Could not resolve host: x", "connect to x port 1").
    let authority = url
        .split("://")
        .nth(1)
        .and_then(|r| r.split(['/', '?', '#']).next())
        .unwrap_or("");
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => (h, Some(p)),
        _ => (authority, None),
    };
    let host = host.trim_matches(['[', ']']);
    if !host.is_empty() {
        text = text.replace(host, "<server>");
    }
    if let Some(p) = port {
        text = text.replace(&format!("port {p}"), "port <port>");
    }
    text.chars().filter(|c| !c.is_control()).take(160).collect()
}

fn sent_to(cfg: &Config) -> String {
    let mut v = Vec::new();
    if cfg.has_ntfy() {
        v.push("ntfy");
    }
    if cfg.has_webhook() {
        v.push("the webhook");
    }
    if cfg.has_telegram() {
        v.push("Telegram");
    }
    v.join(", ")
}

/// Send one message to one channel. Errors never contain the address.
async fn send_to(cfg: &Config, channel: &str, m: &Message) -> Result<(), String> {
    match channel {
        "ntfy" => {
            let server = if cfg.ntfy_server.is_empty() {
                DEFAULT_NTFY
            } else {
                cfg.ntfy_server.as_str()
            };
            let token = (!cfg.ntfy_token.is_empty()).then_some(cfg.ntfy_token.as_str());
            let body = ntfy_body(m, &cfg.ntfy_topic);
            post(server.trim_end_matches('/'), token, &body).await
        }
        "webhook" => post(&cfg.webhook_url, None, &webhook_body(m)).await,
        "telegram" => {
            let url = telegram_url(TELEGRAM_API, &cfg.telegram_token, "sendMessage");
            post(&url, None, &telegram_body(m, &cfg.telegram_chat)).await
        }
        _ => Err("unknown channel".into()),
    }
}

/// Send one message to every configured channel (the test button). Errors are labelled.
async fn send_all(cfg: &Config, m: &Message) -> Vec<String> {
    let mut errors = Vec::new();
    for channel in cfg.enabled() {
        if let Err(e) = send_to(cfg, channel, m).await {
            errors.push(format!("{channel}: {e}"));
        }
    }
    errors
}

/// What one channel's loop remembers between rounds.
#[derive(Default)]
struct Retry {
    /// Failed attempts in a row.
    fails: u32,
    wait_until: Option<Instant>,
    /// When each waiting message was last tried, so one bad message cannot block the rest.
    tried: Tried,
    /// The settings the failures happened with.
    config: Option<Config>,
}

fn backoff(fails: u32) -> Duration {
    let secs = FIRST_BACKOFF << fails.saturating_sub(1).min(8);
    Duration::from_secs(secs.min(MAX_BACKOFF))
}

/// Message -> (last attempt, the server refused this message itself).
type Tried = HashMap<(u64, What), (Instant, bool)>;

/// The server answered and said no to this request (not "no connection" or "slow down").
fn refused(reason: &str) -> bool {
    reason
        .split_once("returned error: ")
        .is_some_and(|(_, code)| code.starts_with('4') && !code.starts_with("429"))
}

fn pick(mut owed: Vec<Owed>, tried: &Tried) -> Option<Owed> {
    // Never tried first (`None` sorts first), then the longest ago; ties keep the oldest first.
    owed.sort_by_key(|o| tried.get(&(o.event.id, o.what)).map(|t| t.0));
    owed.into_iter().next()
}

/// One round of one channel: send what it is owed, stop at the first failure.
async fn round(shared: &Shared, channel: &str, retry: &mut Retry) {
    for _ in 0..BATCH {
        let (cfg, next) = {
            let mut s = shared.lock().unwrap();
            let cfg = s.channels.clone();
            if !cfg.has(channel) {
                *retry = Retry::default();
                return;
            }
            if retry.config.as_ref().is_some_and(|old| *old != cfg) {
                // New settings: try again at once.
                *retry = Retry::default();
                s.forget_channel_error(channel);
            }
            retry.config = Some(cfg.clone());
            if s.silence_left() > 0 {
                return; // maintenance mode: hold everything, send it afterwards
            }
            for o in s.give_up(channel, cfg.min, crate::metrics::now_unix()) {
                eprintln!(
                    "notify: {channel}: gave up on \"{}\" after {} hours without a successful send",
                    o.event.title,
                    crate::notify::GIVE_UP_SECS / 3600
                );
            }
            if retry.wait_until.is_some_and(|t| Instant::now() < t) {
                return;
            }
            let next = pick(s.outbox(channel, cfg.min), &retry.tried);
            if next.is_none() {
                retry.tried.clear();
            }
            (cfg, next)
        };
        let Some(owed) = next else { return };
        let (event, what, late) = (owed.event.clone(), owed.what, owed.late);
        // Asking `date` for local time blocks briefly: not on the async threads.
        let Ok(message) = tokio::task::spawn_blocking(move || {
            wording::compose(&event, what, late, &wording::local_stamp)
        })
        .await
        else {
            return;
        };
        match send_to(&cfg, channel, &message).await {
            Ok(()) => {
                if retry.fails > 0 {
                    eprintln!(
                        "notify: {channel}: delivery works again after {} failed attempts",
                        retry.fails
                    );
                }
                retry.fails = 0;
                retry.wait_until = None;
                // The channel works: back to oldest first, except for refused messages,
                // which must not get in front of the others again.
                retry.tried.remove(&(owed.event.id, owed.what));
                retry.tried.retain(|_, t| t.1);
                shared.lock().unwrap().delivered(channel, &owed);
            }
            Err(reason) => {
                retry.fails = retry.fails.saturating_add(1);
                retry.wait_until = Some(Instant::now() + backoff(retry.fails));
                retry.tried.insert(
                    (owed.event.id, owed.what),
                    (Instant::now(), refused(&reason)),
                );
                if retry.fails == 1 {
                    eprintln!(
                        "notify: {channel}: could not send \"{}\": {reason} (retrying)",
                        message.title
                    );
                }
                shared
                    .lock()
                    .unwrap()
                    .send_failed(channel, &owed, &reason, cfg.min);
                return;
            }
        }
    }
}

/// Background loops: deliver what is pending, each channel on its own.
pub fn spawn(shared: Shared) {
    for channel in CHANNELS {
        let shared = shared.clone();
        tokio::spawn(async move {
            let mut retry = Retry::default();
            loop {
                tokio::time::sleep(TICK).await;
                round(&shared, channel, &mut retry).await;
            }
        });
    }
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
    has_telegram: bool,
    telegram_chat: String,
    configured: bool,
    /// (channel, reason) for channels whose last send failed.
    failing: Vec<(&'static str, String)>,
    min: &'static str,
    silence_min: u64,
}

fn render(s: &crate::notify::Store, message: Option<(bool, String)>) -> Html<String> {
    let c = &s.channels;
    Html(
        ChannelsTemplate {
            message,
            ntfy_server: c.ntfy_server.clone(),
            ntfy_topic: c.ntfy_topic.clone(),
            has_token: !c.ntfy_token.is_empty(),
            has_ntfy: c.has_ntfy(),
            has_webhook: c.has_webhook(),
            webhook_host: host_of(&c.webhook_url),
            has_telegram: c.has_telegram(),
            telegram_chat: c.telegram_chat.clone(),
            configured: c.any(),
            failing: c
                .enabled()
                .into_iter()
                .filter_map(|ch| s.channel_error(ch).map(|e| (ch, e.to_string())))
                .collect(),
            min: c.min.slug(),
            silence_min: s.silence_left().div_ceil(60),
        }
        .render()
        .expect("channels template renders"),
    )
}

pub async fn show(AxState(shared): AxState<Shared>) -> Html<String> {
    render(&shared.lock().unwrap(), None)
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
    telegram_token: String,
    #[serde(default)]
    telegram_chat: String,
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
    let chat = f.telegram_chat.trim();
    let tg_token = f.telegram_token.trim();
    if !tg_token.is_empty() {
        if !valid_bot_token(tg_token) {
            return Err(
                "That does not look like a Telegram bot token (it looks like 123456:ABC-def...)."
                    .into(),
            );
        }
        c.telegram_token = tg_token.to_string();
    }
    if !chat.is_empty() {
        if !valid_chat(chat) {
            return Err(
                "The Telegram chat must be a number (for example 123456789 or -1001234567890) or a channel like @channelname. A name ending in \"bot\" is a bot, not a chat: leave the field empty and send your bot a message to find your chat id."
                    .into(),
            );
        }
        c.telegram_chat = chat.to_string();
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
    axum::Form(mut f): axum::Form<ChannelsForm>,
) -> Html<String> {
    let mut found = false;
    if f.telegram_chat.trim().is_empty() {
        // A new token without a chat: ask Telegram which chat wrote to the bot last.
        let (token, has_chat) = {
            let s = shared.lock().unwrap();
            let t = f.telegram_token.trim();
            let t = if t.is_empty() {
                &s.channels.telegram_token
            } else {
                t
            };
            (t.to_string(), !s.channels.telegram_chat.is_empty())
        };
        if !has_chat && valid_bot_token(&token) {
            let url = telegram_url(TELEGRAM_API, &token, "getUpdates");
            if let Ok(out) = post_out(&url, None, "{}").await
                && let Some(id) = chat_from_updates(&out)
            {
                f.telegram_chat = id;
                found = true;
            }
        }
    }
    let mut s = shared.lock().unwrap();
    match merged(&s.channels, &f) {
        Ok(c) => {
            s.set_channels(c);
            crate::audit::record("settings.channels", "saved");
            let note = if found {
                "Saved. The Telegram chat was found automatically."
            } else if !s.channels.telegram_token.is_empty() && s.channels.telegram_chat.is_empty() {
                "Saved, but Telegram is not active yet: send any message to your bot (or add it to the channel as admin and post once), then press Save again, or type the chat id."
            } else {
                "Saved."
            };
            render(&s, Some((true, note.into())))
        }
        Err(m) => render(&s, Some((false, m))),
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
        "telegram" => {
            c.telegram_token.clear();
            c.telegram_chat.clear();
        }
        _ => {}
    }
    s.set_channels(c);
    crate::audit::record("settings.channels", &format!("{which} removed"));
    render(&s, Some((true, "Removed.".into())))
}

#[derive(Deserialize)]
pub struct SilenceForm {
    #[serde(default)]
    minutes: u64,
}

/// Maintenance mode on: hold pushes for a while (events stay in the panel and go out afterwards).
pub async fn silence(
    AxState(shared): AxState<Shared>,
    axum::Form(f): axum::Form<SilenceForm>,
) -> Html<String> {
    let mut s = shared.lock().unwrap();
    let m = f.minutes.clamp(1, crate::notify::MAX_SILENCE_MIN);
    s.silence_for(m);
    crate::audit::record("settings.silence", &format!("{m} min"));
    render(
        &s,
        Some((true, format!("Notifications on hold for {m} min."))),
    )
}

/// Maintenance mode off: whatever is still open is sent within a minute.
pub async fn resume(AxState(shared): AxState<Shared>) -> Html<String> {
    let mut s = shared.lock().unwrap();
    s.silence_for(0);
    crate::audit::record("settings.silence", "ended");
    render(&s, Some((true, "Notifications resumed.".into())))
}

pub async fn test(AxState(shared): AxState<Shared>) -> Html<String> {
    let cfg = shared.lock().unwrap().channels.clone();
    let e = Event {
        ts: crate::metrics::now_unix(),
        source: "dockwren".into(),
        kind: "test".into(),
        title: "Test notification from Dockwren".into(),
        body: Some("If you can read this, delivery works.".into()),
        key: "test".into(),
        ..Event::default()
    };
    let m = wording::compose(&e, What::Alert, false, &wording::local_stamp);
    let errors = send_all(&cfg, &m).await;
    let msg = if errors.is_empty() {
        (
            true,
            format!(
                "Test sent to {}. It should arrive in a few seconds.",
                sent_to(&cfg)
            ),
        )
    } else {
        (false, errors.join("; "))
    };
    render(&shared.lock().unwrap(), Some(msg))
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
            telegram_token: String::new(),
            telegram_chat: String::new(),
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
            ..Event::default()
        };
        let m = wording::compose(&e, What::Alert, false, &|_| None);
        let w: serde_json::Value = serde_json::from_str(&webhook_body(&m)).unwrap();
        assert_eq!(w["severity"], "warning");
        assert_eq!(w["title"], "update \"gitea\" failed");
        assert!(w["text"].as_str().unwrap().starts_with("[Warning] update"));
        assert_eq!(w["state"], "alert");
        let n: serde_json::Value = serde_json::from_str(&ntfy_body(&m, "t")).unwrap();
        assert_eq!(n["priority"], 4);
        assert_eq!(n["message"], "line1\nline2");
    }

    #[test]
    fn telegram_fields_are_checked_and_merged() {
        assert!(valid_bot_token("123456789:AAE-abc_DEF123"));
        assert!(!valid_bot_token("abc:def"));
        assert!(!valid_bot_token("123456789"));
        assert!(!valid_bot_token("123:a/b"));
        assert!(valid_chat("-1001234567890") && valid_chat("42") && valid_chat("@dockwren_news"));
        assert!(!valid_chat("@dockwren_alerts_bot") && !valid_chat("@MyBot"));
        assert!(!valid_chat("@ab") && !valid_chat("-") && !valid_chat("12a") && !valid_chat(""));
        let mut f = form("", "", "warning");
        f.telegram_token = "123456789:AAE-abc".into();
        let c = merged(&Config::default(), &f).unwrap();
        assert!(!c.has_telegram(), "token without a chat is not active");
        f.telegram_token.clear();
        f.telegram_chat = "@dockwren_news".into();
        let c = merged(&c, &f).unwrap();
        assert!(c.has_telegram() && c.any());
        assert_eq!(c.telegram_token, "123456789:AAE-abc");
        f.telegram_chat = "no way".into();
        assert!(merged(&c, &f).is_err());
    }

    #[test]
    fn telegram_payload_and_chat_detection() {
        let e = Event {
            id: 1,
            ts: 1,
            source: "backup".into(),
            kind: "backup.failed".into(),
            severity: Severity::Critical,
            title: "Backup <failed> & more".into(),
            body: Some("disk full".into()),
            link: None,
            key: "k".into(),
            state: crate::notify::State::New,
            ..Event::default()
        };
        let m = wording::compose(&e, What::Alert, false, &|_| None);
        let v: serde_json::Value = serde_json::from_str(&telegram_body(&m, "-100123")).unwrap();
        assert_eq!(v["chat_id"], "-100123");
        assert!(
            v["text"]
                .as_str()
                .unwrap()
                .contains("Backup <failed> & more\n\ndisk full")
        );
        assert!(v.get("parse_mode").is_none());
        assert_eq!(
            telegram_url("https://api.telegram.org/", "1:AB", "sendMessage"),
            "https://api.telegram.org/bot1:AB/sendMessage"
        );
        let upd = r#"{"ok":true,"result":[{"update_id":1,"message":{"chat":{"id":55}}},{"update_id":2,"message":{"chat":{"id":-1009}}}]}"#;
        assert_eq!(chat_from_updates(upd).as_deref(), Some("-1009"));
        assert_eq!(chat_from_updates(r#"{"ok":true,"result":[]}"#), None);
        assert_eq!(chat_from_updates("nonsense"), None);
    }

    #[tokio::test]
    async fn telegram_send_goes_to_the_bot_url_and_errors_hide_the_token() {
        let (base, h) = serve_once("200 OK");
        let base = base.trim_end_matches("/hook-secret").to_string();
        let url = telegram_url(&base, "1:TOKENSECRET", "sendMessage");
        post(&url, None, "{\"chat_id\":\"1\",\"text\":\"hi\"}")
            .await
            .unwrap();
        let req = h.join().unwrap();
        assert!(
            req.starts_with("POST /bot1:TOKENSECRET/sendMessage"),
            "{req}"
        );
        let (base, h) = serve_once("401 Unauthorized");
        let base = base.trim_end_matches("/hook-secret").to_string();
        let url = telegram_url(&base, "1:TOKENSECRET", "sendMessage");
        let err = post(&url, None, "{}").await.unwrap_err();
        h.join().unwrap();
        assert!(err.contains("401") && !err.contains("TOKENSECRET"), "{err}");
    }

    #[test]
    fn curl_errors_do_not_leak_the_address() {
        let url = "https://hooks.example/T000/secret";
        let m = clean_reason(
            &format!("curl: (22) The requested URL returned error: 404 {url}"),
            url,
        );
        assert!(!m.contains("secret"));
        for (reason, url) in [
            ("Could not resolve host: hooks.example", url),
            (
                "Failed to connect to ntfy.home.example port 8443 after 0 ms",
                "https://ntfy.home.example:8443",
            ),
            ("Failed to connect to fd00::7 port 80", "http://[fd00::7]/x"),
        ] {
            let m = clean_reason(reason, url);
            for part in ["hooks.example", "ntfy.home.example", "8443", "fd00::7"] {
                assert!(!m.contains(part), "{m}");
            }
        }
    }

    use crate::notify::GIVE_UP_SECS;

    const CH: &str = "ntfy";

    fn on() -> Config {
        Config {
            ntfy_topic: "t".into(),
            ..Config::default()
        }
    }

    fn store() -> Store {
        let mut s = Store::memory();
        s.set_channels(on());
        s
    }

    fn warn(s: &mut Store, key: &str) -> u64 {
        s.raise(NewEvent::new("a", "k", Severity::Warning, key).key(key))
            .0
    }

    fn owed(s: &Store) -> Vec<(u64, What)> {
        s.outbox(CH, Severity::Warning)
            .iter()
            .map(|o| (o.event.id, o.what))
            .collect()
    }

    fn first(s: &Store) -> Owed {
        s.outbox(CH, Severity::Warning).remove(0)
    }

    #[test]
    fn owed_follows_severity_and_sent_state() {
        let mut s = store();
        s.raise(NewEvent::new("a", "k", Severity::Info, "i").key("i"));
        let w = warn(&mut s, "w");
        assert_eq!(owed(&s), vec![(w, What::Alert)]);
        assert_eq!(s.outbox(CH, Severity::Info).len(), 2);
        let o = first(&s);
        s.delivered(CH, &o);
        assert!(owed(&s).is_empty());
        // escalation sends again
        s.raise(NewEvent::new("a", "k", Severity::Critical, "w").key("w"));
        assert_eq!(owed(&s), vec![(w, What::Alert)]);
        // once it cleared, what the channel got is followed up; the alert is not sent late
        s.resolve("w");
        assert_eq!(owed(&s), vec![(w, What::Cleared)]);
        let o = first(&s);
        s.delivered(CH, &o);
        assert!(owed(&s).is_empty());
    }

    #[test]
    fn a_failed_send_stays_owed_and_never_counts_as_sent() {
        let mut s = store();
        let w = warn(&mut s, "cut");
        for _ in 0..50 {
            let o = first(&s);
            s.send_failed(CH, &o, "no route", Severity::Warning);
        }
        assert_eq!(owed(&s), vec![(w, What::Alert)]);
        assert!(first(&s).late);
        assert_eq!(s.channel_error(CH), Some("no route"));
        // it ends while the channel is still unreachable: one summary, not "cleared"
        s.resolve("cut");
        assert_eq!(owed(&s), vec![(w, What::Missed)]);
        let o = first(&s);
        s.delivered(CH, &o);
        assert!(owed(&s).is_empty() && s.channel_error(CH).is_none());
    }

    #[test]
    fn what_came_and_went_during_an_outage_is_summed_up() {
        let mut s = store();
        warn(&mut s, "first");
        let o = first(&s);
        s.send_failed(CH, &o, "down", Severity::Warning);
        // raised and cleared while the channel was known to be down, never even tried
        let b = warn(&mut s, "second");
        s.resolve("second");
        assert!(owed(&s).contains(&(b, What::Missed)));
    }

    #[test]
    fn a_blip_on_a_working_channel_and_a_dismissed_event_owe_nothing() {
        let mut s = store();
        warn(&mut s, "blip");
        s.resolve("blip");
        assert!(owed(&s).is_empty());
        let d = warn(&mut s, "dismissed");
        let o = first(&s);
        s.delivered(CH, &o);
        s.dismiss(d);
        s.resolve("dismissed");
        assert!(owed(&s).is_empty());
        // superseded (replaced by a newer event, or switched off) is not "cleared"
        warn(&mut s, "old");
        let o = first(&s);
        s.delivered(CH, &o);
        s.supersede("old");
        assert!(owed(&s).is_empty());
    }

    #[test]
    fn it_cleared_while_curl_was_failing() {
        let mut s = store();
        let w = warn(&mut s, "cut");
        let o = first(&s); // in flight
        s.resolve("cut");
        s.send_failed(CH, &o, "timeout", Severity::Warning);
        assert_eq!(owed(&s), vec![(w, What::Missed)]);
    }

    #[test]
    fn it_cleared_while_curl_was_succeeding() {
        let mut s = store();
        let w = warn(&mut s, "cut");
        let o = first(&s);
        s.resolve("cut");
        s.delivered(CH, &o);
        assert_eq!(owed(&s), vec![(w, What::Cleared)]);
    }

    #[test]
    fn gives_up_after_a_day_but_still_sums_up_when_it_ends() {
        let mut s = store();
        let w = warn(&mut s, "long");
        let o = first(&s);
        s.send_failed(CH, &o, "down", Severity::Warning);
        let now = crate::metrics::now_unix();
        assert!(s.give_up(CH, Severity::Warning, now + 60).is_empty());
        assert_eq!(
            s.give_up(CH, Severity::Warning, now + GIVE_UP_SECS + 60)
                .len(),
            1
        );
        assert!(owed(&s).is_empty());
        s.resolve("long");
        assert_eq!(owed(&s), vec![(w, What::Missed)]);
        assert_eq!(
            s.give_up(CH, Severity::Warning, now + 3 * GIVE_UP_SECS)
                .len(),
            1
        );
        assert!(owed(&s).is_empty());
    }

    #[test]
    fn channels_are_tracked_one_by_one_and_a_new_one_starts_from_now() {
        let mut s = store();
        warn(&mut s, "before");
        let o = first(&s);
        s.delivered(CH, &o);
        let mut both = on();
        both.webhook_url = "https://hooks.example/x".into();
        s.set_channels(both);
        assert!(s.outbox("webhook", Severity::Info).is_empty());
        s.resolve("before");
        assert_eq!(owed(&s).len(), 1);
        assert!(
            s.outbox("webhook", Severity::Info).is_empty(),
            "no 'cleared' for something this channel never heard of"
        );
        let n = warn(&mut s, "after");
        assert!(owed(&s).contains(&(n, What::Alert)));
        let hook = s.outbox("webhook", Severity::Warning).remove(0);
        s.delivered("webhook", &hook);
        assert!(owed(&s).contains(&(n, What::Alert)), "ntfy is still owed");
    }

    #[test]
    fn what_is_owed_survives_a_restart() {
        let d = std::env::temp_dir().join(format!("dw-owed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let w = {
            let mut s = Store::open(&d);
            s.set_channels(on());
            let w = warn(&mut s, "cut");
            let o = first(&s);
            s.send_failed(CH, &o, "down", Severity::Warning);
            w
        };
        let mut s = Store::open(&d);
        assert_eq!(owed(&s), vec![(w, What::Alert)]);
        s.resolve("cut");
        assert_eq!(owed(&Store::open(&d)), vec![(w, What::Missed)]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_log_from_the_previous_version_is_not_sent_again() {
        let d = std::env::temp_dir().join(format!("dw-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(FILE), serde_json::to_string(&on()).unwrap()).unwrap();
        let now = crate::metrics::now_unix();
        let line = |id: u64, state: &str, extra: &str| {
            format!(
                "{{\"id\":{id},\"ts\":{now},\"source\":\"a\",\"kind\":\"k\",\"severity\":\"warning\",\"title\":\"t\",\"key\":\"k{id}\",\"state\":\"{state}\"{extra}}}\n"
            )
        };
        let log = line(1, "seen", ",\"notified\":\"warning\"")
            + &line(2, "resolved", ",\"notified\":\"warning\"")
            + &line(
                3,
                "resolved",
                ",\"notified\":\"warning\",\"clear_pending\":true",
            )
            + &line(4, "resolved", "");
        std::fs::write(d.join("notifications.log"), log).unwrap();
        let mut s = Store::open(&d);
        assert_eq!(owed(&s), vec![(3, What::Cleared)]);
        s.resolve("k1");
        assert!(owed(&s).contains(&(1, What::Cleared)));
        // and the state is still right after the log was rewritten in the new format
        assert_eq!(owed(&Store::open(&d)).len(), 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn pauses_grow_and_stay_short() {
        let secs: Vec<u64> = (1..6).map(|f| backoff(f).as_secs()).collect();
        assert_eq!(secs, vec![10, 20, 40, 60, 60]);
        assert_eq!(backoff(u32::MAX).as_secs(), MAX_BACKOFF);
    }

    #[test]
    fn one_rejected_message_does_not_block_the_others() {
        let mut s = store();
        let a = warn(&mut s, "a");
        let b = warn(&mut s, "b");
        let mut tried = HashMap::new();
        let o = pick(s.outbox(CH, Severity::Warning), &tried).unwrap();
        assert_eq!(o.event.id, a);
        tried.insert((a, What::Alert), (Instant::now(), true));
        let o = pick(s.outbox(CH, Severity::Warning), &tried).unwrap();
        assert_eq!(o.event.id, b);
        assert!(refused("The requested URL returned error: 400"));
        assert!(!refused("The requested URL returned error: 429"));
        assert!(!refused("The requested URL returned error: 502"));
        assert!(!refused("Could not resolve host: ntfy.sh"));
    }

    /// A fake ntfy server on a fixed port, answering 200 to `n` requests; returns the bodies.
    fn fake_ntfy(addr: &str, n: usize) -> std::thread::JoinHandle<Vec<serde_json::Value>> {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind(addr).unwrap();
        std::thread::spawn(move || {
            (0..n)
                .map(|_| {
                    let (mut c, _) = l.accept().unwrap();
                    let (mut got, mut buf) = (Vec::new(), [0u8; 4096]);
                    let body = loop {
                        let r = c.read(&mut buf).unwrap();
                        assert!(r > 0, "request cut short");
                        got.extend_from_slice(&buf[..r]);
                        let t = String::from_utf8_lossy(&got).into_owned();
                        if let Some((_, body)) = t.split_once("\r\n\r\n")
                            && let Ok(v) = serde_json::from_str::<serde_json::Value>(body)
                        {
                            break v;
                        }
                    };
                    write!(
                        c,
                        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                    body
                })
                .collect()
        })
    }

    /// The outage of 2026-11-26 in one test: two power cuts while nothing can be reached.
    #[tokio::test]
    async fn power_cuts_without_internet_arrive_as_summaries_when_it_is_back() {
        // A port with nothing listening is "no internet".
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .to_string();
        let mut cfg = on();
        cfg.ntfy_server = format!("http://{addr}");
        let mut st = Store::memory();
        st.set_channels(cfg);
        let shared: Shared = std::sync::Arc::new(std::sync::Mutex::new(st));
        let cut = |s: &Shared| {
            s.lock().unwrap().raise(
                NewEvent::new(
                    "host",
                    "host.ups",
                    Severity::Warning,
                    "Power cut: UPS eaton on battery",
                )
                .key("host.ups:eaton:power"),
            );
        };
        let mut retry = Retry::default();
        let pause_over = |r: &mut Retry| r.wait_until = None;
        cut(&shared);
        for _ in 0..10 {
            round(&shared, CH, &mut retry).await;
            pause_over(&mut retry);
        }
        assert_eq!(
            retry.fails, 10,
            "still trying after far more than 3 attempts"
        );
        shared.lock().unwrap().resolve("host.ups:eaton:power");
        cut(&shared);
        round(&shared, CH, &mut retry).await;
        shared.lock().unwrap().resolve("host.ups:eaton:power");

        let server = fake_ntfy(&addr, 2);
        pause_over(&mut retry);
        round(&shared, CH, &mut retry).await;
        let got = server.join().unwrap();
        for m in &got {
            assert!(
                m["title"]
                    .as_str()
                    .unwrap()
                    .starts_with("Power restored after a cut of"),
                "{m}"
            );
            assert_eq!(m["tags"][0], "white_check_mark");
        }
        let s = shared.lock().unwrap();
        assert!(s.outbox(CH, Severity::Warning).is_empty());
        assert!(s.channel_error(CH).is_none());
    }

    /// With internet during the cut: the alert at once, then a different "restored" message.
    #[tokio::test]
    async fn power_cut_with_internet_gets_an_alert_and_then_a_restored_message() {
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .to_string();
        let server = fake_ntfy(&addr, 2);
        let mut cfg = on();
        cfg.ntfy_server = format!("http://{addr}");
        let mut st = Store::memory();
        st.set_channels(cfg);
        let shared: Shared = std::sync::Arc::new(std::sync::Mutex::new(st));
        shared.lock().unwrap().raise(
            NewEvent::new(
                "host",
                "host.ups",
                Severity::Warning,
                "Power cut: UPS eaton on battery",
            )
            .key("host.ups:eaton:power"),
        );
        let mut retry = Retry::default();
        round(&shared, CH, &mut retry).await;
        shared.lock().unwrap().resolve("host.ups:eaton:power");
        round(&shared, CH, &mut retry).await;
        let got = server.join().unwrap();
        assert_eq!(got[0]["tags"][0], "electric_plug");
        assert!(got[0]["message"].as_str().unwrap().starts_with("Since "));
        assert_eq!(got[1]["tags"][0], "white_check_mark");
        assert_eq!(
            got[1]["title"],
            "Power restored: UPS eaton is back on mains"
        );
        assert!(
            got[1]["message"]
                .as_str()
                .unwrap()
                .starts_with("Power was out: ")
        );
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
