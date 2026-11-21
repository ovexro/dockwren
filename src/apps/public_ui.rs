//! Public access (Cloudflare and direct) for an instance.

use super::*;

#[derive(Template)]
#[template(path = "public.html")]
pub(super) struct PublicPage {
    pub(super) instance: String,
    pub(super) hostname: String,
    pub(super) snippet: Option<String>,
    pub(super) command: Option<String>,
    pub(super) https_note: bool,
    pub(super) error: Option<String>,
    pub(super) saved: bool,
    pub(super) cf_ready: bool,
    /// Outcome of a Publish press: (succeeded, step lines).
    pub(super) result: Option<(bool, Vec<String>)>,
    /// The last Publish failed only because another DNS record uses the name.
    pub(super) offer_replace: bool,
    /// The hostname is not under any domain the Cloudflare token covers.
    pub(super) zone_warning: Option<String>,
    /// Published directly through Caddy (no Cloudflare).
    pub(super) direct: bool,
    /// Preflight lines for direct HTTPS: (ok, text).
    pub(super) checks: Vec<(bool, String)>,
    /// Expiry of the certificate Caddy serves (direct mode only).
    pub(super) cert_expiry: Option<String>,
}

pub(super) fn public_page(
    apps: &Apps,
    name: &str,
    hostname: String,
    error: Option<String>,
    saved: bool,
) -> PublicPage {
    let dir = apps.root.join(name);
    let port = crate::public::web_port(&dir);
    let valid = crate::public::valid_hostname(&hostname);
    let snippet = port.filter(|_| valid).map(|p| {
        format!(
            "- hostname: {hostname}\n  service: {}",
            crate::public::service_url(p)
        )
    });
    let command = valid.then(|| format!("dockwren expose {name}"));
    let https_note = std::fs::read_to_string(dir.join("state.toml"))
        .ok()
        .and_then(|s| toml::from_str::<InstanceState>(&s).ok())
        .and_then(|st| {
            apps.template(&st.template)
                .map(|t| t.manifest.app.needs_public_https)
        })
        .unwrap_or(false);
    PublicPage {
        instance: name.to_string(),
        hostname,
        snippet,
        command,
        https_note,
        error,
        saved,
        cf_ready: crate::public::Settings::from_env().is_some(),
        result: None,
        offer_replace: false,
        zone_warning: None,
        direct: crate::direct::is_direct(&dir),
        checks: Vec::new(),
        cert_expiry: None,
    }
}

/// Warning text when Cloudflare is connected but `host` is under none of its domains.
pub(super) async fn zone_warning_for(host: &str) -> Option<String> {
    if !crate::public::valid_hostname(host) {
        return None;
    }
    let s = crate::public::Settings::from_env()?;
    let zones = crate::public::zone_names(&s).await.ok()?;
    crate::public::outside_zones(host, &zones).then(|| {
        format!(
            "Warning: {host} is not under any domain this Cloudflare token can manage ({}). Publishing will fail. Use a hostname under one of those domains, or give the token access to this domain in Cloudflare.",
            zones.join(", ")
        )
    })
}

pub(super) async fn with_zone_warning(mut page: PublicPage) -> PublicPage {
    if page.cf_ready {
        page.zone_warning = zone_warning_for(&page.hostname).await;
    }
    page
}

pub(super) fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `GET /apps/check-host?PUBLIC_HOST=..`: the install form asks this as the hostname is typed.
pub async fn check_host(Query(q): Query<HashMap<String, String>>) -> Html<String> {
    let host = q.get("PUBLIC_HOST").map(|h| h.trim()).unwrap_or_default();
    Html(match zone_warning_for(host).await {
        Some(w) => format!("<small role=\"alert\">{}</small>", html_escape(&w)),
        None => String::new(),
    })
}

pub async fn public_form(
    State(apps): State<Apps>,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    known_instance(&apps, &name)?;
    let host = crate::public::read(&apps.root.join(&name)).unwrap_or_default();
    let mut page = public_page(&apps, &name, host.clone(), None, false);
    if page.direct {
        page.cert_expiry = crate::direct::cert_expiry(&host).await;
    }
    Ok(render(with_zone_warning(page).await))
}

pub async fn public_submit(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(values): Form<HashMap<String, String>>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    known_instance(&apps, &name)?;
    let host = values
        .get("hostname")
        .map(|h| h.trim().to_lowercase())
        .unwrap_or_default();
    let error = crate::public::write(&apps.root.join(&name), &host).err();
    let saved = error.is_none();
    if saved {
        crate::audit::record("app.hostname", &format!("{name} -> {host}"));
    }
    Ok(render(
        with_zone_warning(public_page(&apps, &name, host, error, saved)).await,
    ))
}

// ---- Cloudflare connection (paste a token) -----------------------------

#[derive(Template)]
#[template(path = "cloudflare.html")]
pub(super) struct CloudflarePanel {
    pub(super) env_managed: bool,
    pub(super) has_token: bool,
    pub(super) problem: Option<String>,
    pub(super) zones: Vec<String>,
    pub(super) tunnel_name: Option<String>,
    pub(super) choose: Vec<(String, String)>,
    pub(super) message: Option<(bool, String)>,
}

pub(super) async fn cloudflare_panel(message: Option<(bool, String)>) -> CloudflarePanel {
    let env = crate::public::Settings::from_environment();
    let saved = crate::public::load_saved();
    let (token, tunnel) = match &env {
        Some(e) => (Some(e.token.clone()), Some(e.tunnel.clone())),
        None => (saved.token.clone(), saved.tunnel.clone()),
    };
    let mut p = CloudflarePanel {
        env_managed: env.is_some(),
        has_token: token.is_some(),
        problem: None,
        zones: Vec::new(),
        tunnel_name: None,
        choose: Vec::new(),
        message,
    };
    if let Some(t) = token {
        match crate::public::probe(&t).await {
            Ok(probe) => {
                p.zones = probe.zones;
                match tunnel.filter(|id| !id.is_empty()) {
                    Some(id) => {
                        p.tunnel_name = Some(
                            probe
                                .tunnels
                                .iter()
                                .find(|(i, _)| *i == id)
                                .map_or(id, |(_, n)| n.clone()),
                        )
                    }
                    None => p.choose = probe.tunnels,
                }
            }
            Err(e) => p.problem = Some(e),
        }
    }
    p
}

pub async fn cloudflare_show() -> Html<String> {
    render(cloudflare_panel(None).await)
}

pub async fn cloudflare_connect(
    headers: HeaderMap,
    Form(values): Form<HashMap<String, String>>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let token = values.get("token").map(|t| t.trim()).unwrap_or_default();
    let msg = if crate::public::Settings::from_environment().is_some() {
        (
            false,
            "Cloudflare is configured by the server environment; change it there.".to_string(),
        )
    } else if token.is_empty()
        || token.len() > 200
        || token.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        (
            false,
            "That does not look like a token: paste only the token text, no spaces.".to_string(),
        )
    } else {
        match crate::public::probe(token).await {
            Err(e) => (false, e),
            Ok(p) => {
                let tunnel = (p.tunnels.len() == 1).then(|| p.tunnels[0].0.clone());
                let saved = crate::public::Saved {
                    token: Some(token.to_string()),
                    account: Some(p.account),
                    tunnel,
                };
                match crate::public::save_saved(&saved) {
                    Ok(()) => {
                        crate::audit::record("cloudflare.connect", "token saved");
                        (true, "token accepted and saved on this server.".to_string())
                    }
                    Err(e) => (false, format!("could not save: {e}")),
                }
            }
        }
    };
    Ok(render(cloudflare_panel(Some(msg)).await))
}

pub async fn cloudflare_tunnel(
    headers: HeaderMap,
    Form(values): Form<HashMap<String, String>>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let id = values.get("tunnel").map(|t| t.trim()).unwrap_or_default();
    let mut saved = crate::public::load_saved();
    let msg = match saved.token.clone() {
        None => (false, "connect a token first".to_string()),
        Some(t) => match crate::public::probe(&t).await {
            Ok(p) if p.tunnels.iter().any(|(i, _)| i == id) => {
                saved.tunnel = Some(id.to_string());
                match crate::public::save_saved(&saved) {
                    Ok(()) => {
                        crate::audit::record("cloudflare.tunnel", "selected");
                        (true, "tunnel selected.".to_string())
                    }
                    Err(e) => (false, format!("could not save: {e}")),
                }
            }
            Ok(_) => (false, "that tunnel is not in your account".to_string()),
            Err(e) => (false, e),
        },
    };
    Ok(render(cloudflare_panel(Some(msg)).await))
}

pub async fn cloudflare_disconnect(headers: HeaderMap) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let msg = match crate::public::forget_saved() {
        Ok(()) => {
            crate::audit::record("cloudflare.disconnect", "token forgotten");
            (
                true,
                "token forgotten. Apps already published stay online.".to_string(),
            )
        }
        Err(e) => (false, e),
    };
    Ok(render(cloudflare_panel(Some(msg)).await))
}

/// `POST /instances/{name}/public/publish`: wire the saved hostname to Cloudflare and
/// show every step plus a clear success or failure line.
pub async fn public_publish(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(values): Form<HashMap<String, String>>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    known_instance(&apps, &name)?;
    let replace_dns = values.get("replace").is_some_and(|v| v == "yes");
    crate::audit::record("app.publish", &name);
    let dir = apps.root.join(&name);
    let host = crate::public::read(&dir).unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    let outcome: Result<(), String> = async {
        if host.is_empty() {
            return Err("save a hostname first".to_string());
        }
        let settings = crate::public::Settings::from_env().ok_or(
            "Cloudflare is not connected: the server needs CLOUDFLARE_API_TOKEN, CLOUDFLARE_ACCOUNT_ID and DOCKWREN_TUNNEL_ID (see the guide below)",
        )?;
        let port = crate::public::web_port(&dir).ok_or("this app has no web port to publish")?;
        crate::public::expose(
            &settings,
            &host,
            &crate::public::service_url(port),
            &[],
            true,
            replace_dns,
            &mut |s| lines.push(s.to_string()),
        )
        .await
    }
    .await;
    let mut page = public_page(&apps, &name, host.clone(), None, false);
    page.result = Some(match outcome {
        Ok(()) => {
            lines.push(format!("Published: https://{host}"));
            (true, lines)
        }
        Err(e) => {
            page.offer_replace = e.starts_with(crate::public::DNS_CONFLICT);
            lines.push(e);
            (false, lines)
        }
    });
    Ok(render(page))
}

/// `POST /instances/{name}/public/direct`: `action` is `check` (preflight only), `on`
/// (publish through Caddy with a Let's Encrypt certificate) or `off` (take it down).
pub async fn public_direct(
    State(apps): State<Apps>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(values): Form<HashMap<String, String>>,
) -> Result<Html<String>, StatusCode> {
    if !crate::same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    known_instance(&apps, &name)?;
    let action = values.get("action").map(String::as_str).unwrap_or("check");
    let dir = apps.root.join(&name);
    let host = crate::public::read(&dir).unwrap_or_default();
    let data_dir = apps.root.parent().unwrap_or(&apps.root).to_path_buf();
    let mut lines: Vec<String> = Vec::new();
    let mut checks: Vec<(bool, String)> = Vec::new();
    let mut ok = true;
    if !crate::public::valid_hostname(&host) {
        ok = false;
        lines.push("save a valid hostname first".to_string());
    } else if action == "off" {
        crate::audit::record("app.direct.off", &name);
        let r = match crate::direct::set_direct(&dir, false) {
            Ok(()) => crate::direct::reconcile(&data_dir, &apps.root).await,
            Err(e) => Err(e),
        };
        match r {
            Ok(()) => lines.push(format!("Direct HTTPS for {host} is off.")),
            Err(e) => {
                ok = false;
                lines.push(e);
            }
        }
    } else {
        let port = crate::public::web_port(&dir);
        let dns = action == "on-dns" || action == "check-dns";
        let pre = crate::direct::preflight(&data_dir, &host, port, dns).await;
        let blocked = pre.iter().any(|(good, blocks, _)| !good && *blocks);
        checks = pre.into_iter().map(|(g, _, t)| (g, t)).collect();
        if action == "on" || action == "on-dns" {
            if blocked {
                ok = false;
                lines.push("Not published: fix the red items first.".to_string());
            } else {
                crate::audit::record("app.direct.on", &format!("{name} -> {host}"));
                let r = match crate::direct::set_mode(&dir, true, dns) {
                    Ok(()) => crate::direct::reconcile(&data_dir, &apps.root).await,
                    Err(e) => Err(e),
                };
                match r {
                    Ok(()) => lines.push(format!(
                        "Caddy now serves https://{host}. The first certificate can take a minute; see the Caddy container log if it does not appear."
                    )),
                    Err(e) => {
                        ok = false;
                        let _ = crate::direct::set_direct(&dir, false);
                        lines.push(e);
                    }
                }
            }
        } else {
            ok = !blocked;
            lines.push(if blocked {
                "Preflight found a blocking problem.".to_string()
            } else {
                "Preflight passed.".to_string()
            });
        }
    }
    let mut page = public_page(&apps, &name, host.clone(), None, false);
    if page.direct {
        page.cert_expiry = crate::direct::cert_expiry(&host).await;
    }
    page.checks = checks;
    page.result = Some((ok, lines));
    Ok(render(page))
}
