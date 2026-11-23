//! Static files embedded in the binary.

use super::*;

pub(super) const HTMX: &str = include_str!("../static/htmx.min.js");
pub(super) const CSS: &str = include_str!("../static/dockwren.css");
pub(super) const PASSKEY_JS: &str = include_str!("../static/passkey.js");
pub(super) const CONFIRM_JS: &str = include_str!("../static/confirm.js");
pub(super) const FOLLOW_JS: &str = include_str!("../static/follow.js");
pub(super) const FAVICON: &str = include_str!("../static/favicon.svg");
pub(super) const MANIFEST: &str = include_str!("../static/manifest.webmanifest");
pub(super) const ICON_192: &[u8] = include_bytes!("../static/icon-192.png");
pub(super) const ICON_512: &[u8] = include_bytes!("../static/icon-512.png");
pub(super) const APPLE_ICON: &[u8] = include_bytes!("../static/apple-touch-icon.png");

/// Short content hash of the bundled assets, appended to their URLs so a new build is never served from a stale cache.
pub fn asset_version() -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in CSS
        .bytes()
        .chain(HTMX.bytes())
        .chain(PASSKEY_JS.bytes())
        .chain(CONFIRM_JS.bytes())
        .chain(FOLLOW_JS.bytes())
        .chain(terminal::TERMINAL_JS.bytes())
    {
        h = (h ^ u64::from(b)).wrapping_mul(0x100000001b3);
    }
    format!("{:08x}", h >> 32)
}

pub(super) async fn stylesheet() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        CSS,
    )
}

pub(super) async fn favicon() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        FAVICON,
    )
}

pub(super) async fn manifest() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/manifest+json"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        MANIFEST,
    )
}

pub(super) async fn png_icon(bytes: &'static [u8]) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        bytes,
    )
}

pub(super) async fn passkey_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        PASSKEY_JS,
    )
}

pub(super) async fn confirm_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        CONFIRM_JS,
    )
}

pub(super) async fn follow_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        FOLLOW_JS,
    )
}

pub(super) async fn htmx_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        HTMX,
    )
}
