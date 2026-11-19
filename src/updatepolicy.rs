//! Settings → Updates: "What updates by itself", showing the live state of each switch.
//! Read-only; every switch itself lives where it always did.

use std::sync::Arc;

use askama::Template;
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
};

use crate::apps::Apps;
use crate::auth::SharedAuth;

#[derive(Clone)]
struct St {
    auth: SharedAuth,
    apps: Apps,
    os: Arc<crate::osupdate::Control>,
    su: Arc<crate::selfupdate::Control>,
}

pub fn router(
    auth: SharedAuth,
    apps: Apps,
    os: Arc<crate::osupdate::Control>,
    su: Arc<crate::selfupdate::Control>,
) -> Router {
    Router::new()
        .route("/settings/updatepolicy", get(show))
        .with_state(St { auth, apps, os, su })
}

#[derive(Template)]
#[template(path = "updatepolicy.html")]
struct View {
    auto_apps: String,
    os_button: String,
    self_button: String,
}

/// Current state of each switch, as short text.
fn describe(auto: Option<(u8, u8)>, os: (bool, bool), self_installed: bool) -> View {
    let auto_apps = match auto {
        Some((from, to)) if from == to => "On now: any time of day".to_string(),
        Some((from, to)) => format!("On now: between {from:02}:00 and {to:02}:00 UTC"),
        None => "Off now (turn on under Scheduled backups)".to_string(),
    };
    let os_button = match os {
        (true, true) => "Install button is on",
        (true, false) => "Install button is off (turn it on above)",
        (false, _) => "Install button not set up on this server",
    }
    .to_string();
    let self_button = if self_installed {
        "Update button is set up"
    } else {
        "Button not set up yet: run sudo dockwren self-update once"
    }
    .to_string();
    View {
        auto_apps,
        os_button,
        self_button,
    }
}

async fn show(State(s): State<St>, headers: HeaderMap) -> Response {
    if s.auth.session_of(&headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let auto = s.apps.auto_update_window();
    let v = describe(auto, (s.os.installed(), s.os.enabled()), s.su.installed());
    Html(v.render().expect("update policy renders")).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_each_state() {
        let v = describe(Some((3, 6)), (true, true), true);
        assert_eq!(v.auto_apps, "On now: between 03:00 and 06:00 UTC");
        assert!(v.os_button.contains("on"));
        assert_eq!(
            describe(Some((0, 0)), (true, false), false).auto_apps,
            "On now: any time of day"
        );
        let off = describe(None, (false, false), false);
        assert!(off.auto_apps.starts_with("Off now"));
        assert!(off.os_button.contains("not set up"));
        assert!(off.self_button.contains("self-update"));
        assert!(
            describe(None, (true, false), false)
                .os_button
                .contains("off")
        );
    }
}
