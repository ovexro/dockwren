//! The router and the security headers.

use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn app(
    auth: SharedAuth,
    docker: DockerClient,
    apps: apps::Apps,
    metrics: metrics::Shared,
    notify: notify::Shared,
    terminal: Arc<terminal::Switch>,
    os: Arc<osupdate::Control>,
    su: Arc<selfupdate::Control>,
) -> Router {
    let terminal_routes = terminal::router(
        auth.clone(),
        docker.clone(),
        notify.clone(),
        terminal.clone(),
    );
    let policy_routes = updatepolicy::router(auth.clone(), apps.clone(), os.clone(), su.clone());
    let os_routes = osupdate::router(auth.clone(), notify.clone(), os);
    let su_routes = selfupdate::router(auth.clone(), su);
    let protected = Router::new()
        .route("/", get(index))
        .route("/metrics", get(metrics::page))
        .route("/apps", get(apps_page))
        .route("/apps/tab/{tab}", get(apps_tab))
        .route("/apps/{id}/vulns", get(vulns_page))
        .route("/containers", get(containers_page))
        .route("/resources", get(resources_page))
        .route("/settings", get(settings_page))
        .route("/settings/tab/{tab}", get(settings_tab))
        .route("/settings/audit", get(audit::show))
        .route("/settings/templates", get(thirdparty::show))
        .route(
            "/settings/templates/review",
            post(thirdparty::review_submit),
        )
        .route("/settings/templates/add", post(thirdparty::add_submit))
        .route(
            "/settings/templates/remove",
            post(thirdparty::remove_submit),
        )
        .route("/settings/doctor", get(doctor::show))
        .route("/settings/ups", get(ups::show))
        .route("/settings/disk", get(apps::disk_show))
        .route("/settings/disk/prune-images", post(apps::prune_images))
        .route("/settings/disk/remove-orphans", post(apps::remove_orphans))
        .route(
            "/instances/{name}/trim-snapshots",
            post(apps::trim_snapshots),
        )
        .route("/cloudflare", get(cloudflare_page))
        .route("/notifications", get(notifications_page))
        .route("/notifications/list", get(notify::list))
        .route("/notifications/count", get(notify::count))
        .route(
            "/notifications/dismiss-all",
            post(notify::dismiss_everything),
        )
        .route("/notifications/{id}/dismiss", post(notify::dismiss_one))
        .route("/overview", get(overview))
        .route("/containers/list", get(containers))
        .route("/containers/{id}/{action}", post(container_action))
        .route("/containers/{id}/logs", get(container_logs))
        .route("/containers/{id}/logs/download", get(download_logs))
        .route("/containers/{name}/graphs", get(metrics::container_graphs))
        .route("/logs/close", get(close_logs))
        .route("/catalog", get(apps::list))
        .route("/apps/{id}/icon.svg", get(apps::icon))
        .route("/apps/check-host", get(apps::check_host))
        .route("/stacks/new", get(apps::stack_form))
        .route("/stacks/install", post(apps::stack_submit))
        .route(
            "/stacks/adopt",
            get(apps::adopt_scan).post(apps::adopt_submit),
        )
        .route("/apps/{id}/new", get(apps::new_form))
        .route("/apps/{id}/install", post(apps::install_submit))
        .route("/jobs/{id}", get(apps::job))
        .route("/instances/list", get(apps::instances))
        .route("/instances/{name}/remove", post(apps::instance_remove))
        .route("/instances/{name}/delete", post(apps::instance_delete))
        .route("/instances/{name}/start", post(apps::instance_start))
        .route("/instances/{name}/apply", post(apps::instance_apply))
        .route(
            "/instances/{name}/needs",
            get(apps::needs_form).post(apps::needs_submit),
        )
        .route(
            "/instances/{name}/update",
            get(apps::update_preview).post(apps::instance_update),
        )
        .route("/instances-bulk/{action}", post(apps::instances_bulk))
        .route("/instances/{name}/backup", post(apps::instance_backup))
        .route("/instances/{name}/skip-update", post(apps::instance_skip))
        .route(
            "/instances/{name}/unskip-update",
            post(apps::instance_unskip),
        )
        .route(
            "/settings/backups",
            get(apps::schedule_show).post(apps::schedule_save),
        )
        .route(
            "/settings/offsite",
            get(apps::offsite_show).post(apps::offsite_save),
        )
        .route(
            "/instances/{name}/power",
            get(apps::power_form).post(apps::power_submit),
        )
        .route("/settings/registries", get(apps::registries_show))
        .route("/settings/registries/login", post(apps::registries_login))
        .route("/settings/registries/logout", post(apps::registries_logout))
        .route(
            "/instances/{name}/public",
            get(apps::public_form).post(apps::public_submit),
        )
        .route(
            "/instances/{name}/public/publish",
            post(apps::public_publish),
        )
        .route("/instances/{name}/public/direct", post(apps::public_direct))
        .route("/settings/notifications", get(notify::kinds_show))
        .route("/settings/notifications/{kind}", post(notify::kind_set))
        .route("/settings/channels", get(deliver::show).post(deliver::save))
        .route("/settings/channels/test", post(deliver::test))
        .route("/settings/channels/silence", post(deliver::silence))
        .route("/settings/channels/resume", post(deliver::resume))
        .route("/settings/channels/{which}/remove", post(deliver::remove))
        .route("/settings/config-export", post(apps::config_download))
        .route("/settings/2fa", get(auth::twofa_show))
        .route("/settings/2fa/start", post(auth::twofa_start))
        .route("/settings/2fa/enable", post(auth::twofa_enable))
        .route("/settings/2fa/disable", post(auth::twofa_disable))
        .route("/settings/sessions", get(auth::sessions_show))
        .route(
            "/settings/sessions/end-others",
            post(auth::sessions_end_others),
        )
        .route("/settings/passkeys", get(auth::passkeys_show))
        .route("/settings/passkeys/options", post(auth::passkey_options))
        .route("/settings/passkeys/register", post(auth::passkey_register))
        .route("/settings/passkeys/remove", post(auth::passkey_remove))
        .route("/settings/cloudflare", get(apps::cloudflare_show))
        .route(
            "/settings/cloudflare/connect",
            post(apps::cloudflare_connect),
        )
        .route("/settings/cloudflare/tunnel", post(apps::cloudflare_tunnel))
        .route(
            "/settings/cloudflare/disconnect",
            post(apps::cloudflare_disconnect),
        )
        .route(
            "/instances/{name}/restore",
            get(apps::restore_form).post(apps::restore_submit),
        )
        .route(
            "/instances/{name}/settings",
            get(apps::settings_form).post(apps::settings_submit),
        )
        .route(
            "/instances/{name}/passwords",
            get(apps::passwords_form).post(apps::passwords_submit),
        )
        .route(
            "/instances/{name}/limits",
            get(apps::limits_form).post(apps::limits_submit),
        )
        .route(
            "/instances/{name}/stack",
            get(apps::stack_edit_form).post(apps::stack_edit_submit),
        )
        .route("/instances/{name}/logcap", post(apps::logcap_submit))
        .route("/instances/{name}/pull", post(apps::pull_submit))
        .route("/instances/{name}/verify", post(apps::verify_submit))
        .route(
            "/instances/{name}/restore-test",
            post(apps::restore_test_submit),
        )
        .route("/instances/{name}/clone", post(apps::clone_submit))
        .route("/instances/{name}/cleanup", post(apps::cleanup_submit))
        .route("/static/htmx.min.js", get(htmx_js))
        .route("/logout", post(auth::logout))
        .route_layer(from_fn_with_state(auth.clone(), auth::require_session));
    let router = Router::new()
        .route("/health", get(health))
        .route("/api/status", get(statusapi::status))
        .route("/api/metrics", get(statusapi::metrics))
        .route("/static/dockwren.css", get(stylesheet))
        .route("/favicon.svg", get(favicon))
        .route("/favicon.ico", get(favicon))
        .route("/manifest.webmanifest", get(manifest))
        .route("/static/icon-192.png", get(|| png_icon(ICON_192)))
        .route("/static/icon-512.png", get(|| png_icon(ICON_512)))
        .route("/apple-touch-icon.png", get(|| png_icon(APPLE_ICON)))
        .route("/login", get(auth::login_form).post(auth::login_submit))
        .route("/login/passkey", post(auth::login_passkey))
        .route("/login/code", post(auth::login_code))
        .route("/static/passkey.js", get(passkey_js))
        .route("/static/confirm.js", get(confirm_js))
        .merge(protected)
        .with_state(AppState {
            auth,
            docker,
            apps,
            metrics,
            notify,
            terminal,
        });
    router
        .merge(terminal_routes)
        .merge(os_routes)
        .merge(su_routes)
        .merge(policy_routes)
        .layer(middleware::from_fn(security_headers))
}

/// Content Security Policy for every panel page: scripts only from the panel's own origin (no inline
/// handlers or script bodies; a test scans the templates), everything else locked to it too.
/// Styles keep `'unsafe-inline'` because templates use `style=` attributes and htmx adds a style tag.
pub(super) const CSP: &str = "default-src 'self'; script-src 'self'; \
style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; connect-src 'self'; \
object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'";

/// Adds the browser hardening headers to every response. A header a handler already set (the app
/// icon route sends its own stricter CSP) is kept. HSTS is sent only when the request arrived over
/// HTTPS through a proxy or tunnel, so a plain-HTTP LAN panel is never pinned to HTTPS.
pub(super) async fn security_headers(req: Request<axum::body::Body>, next: Next) -> Response {
    let https = req
        .headers()
        .get("x-forwarded-proto")
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"https"));
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    let mut set = |name: header::HeaderName, value: &'static str| {
        if !h.contains_key(&name) {
            h.insert(name, HeaderValue::from_static(value));
        }
    };
    set(header::CONTENT_SECURITY_POLICY, CSP);
    set(header::X_FRAME_OPTIONS, "DENY");
    set(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set(header::REFERRER_POLICY, "same-origin");
    set(
        header::HeaderName::from_static("permissions-policy"),
        "camera=(), microphone=(), geolocation=(), payment=(), usb=(), publickey-credentials-get=(self)",
    );
    if https {
        set(header::STRICT_TRANSPORT_SECURITY, "max-age=15552000");
    }
    res
}
