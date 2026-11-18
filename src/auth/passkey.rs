//! Passkeys (WebAuthn, ES256 only) as a second sign-in step, next to TOTP.
//! Sign-in is still password first; a registered passkey then replaces the 6-digit code.
//! Credentials live in `passkeys.json` in the data dir. Each one is bound to the hostname
//! it was registered on (the WebAuthn rp id), so it works on that address only; TOTP and the
//! recovery codes remain the way in on any other address. Attestation is not verified (we
//! only need the public key); the authenticator proves possession at every sign-in.

use super::*;
use axum::Json;
use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier};
use serde::Serialize;
use serde_json::{Value, json};

const PASSKEY_FILE: &str = "passkeys.json";
const PENDING_TTL: u64 = 300;
const MAX_PASSKEYS: usize = 10;
const MAX_PENDING: usize = 32;

#[derive(Serialize, Deserialize, Clone)]
pub(super) struct Passkey {
    /// Credential id, base64url.
    id: String,
    /// Public key, SEC1 uncompressed point, base64url.
    key: String,
    /// WebAuthn rp id (hostname) it is bound to.
    rp: String,
    count: u32,
    name: String,
}

#[derive(PartialEq, Clone, Copy)]
pub(super) enum Kind {
    Register,
    Login,
}

pub(super) struct Pending {
    kind: Kind,
    challenge: String,
    rp: String,
    expires: u64,
}

// ---------- base64url ----------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn b64u(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().fold(0u32, |a, b| (a << 8) | u32::from(*b)) << (8 * (3 - chunk.len()));
        for i in 0..=chunk.len() {
            out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

fn unb64u(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.trim_end_matches('=').bytes() {
        let v = B64.iter().position(|b| *b == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

// ---------- minimal CBOR (just what attestation objects and COSE keys use) ----------

#[derive(Debug)]
enum Cbor {
    Int(i128),
    Bytes(Vec<u8>),
    Text(String),
    Map(Vec<(Cbor, Cbor)>),
    Other,
}

fn cbor(d: &[u8], pos: &mut usize, depth: u8) -> Option<Cbor> {
    if depth > 6 {
        return None;
    }
    let b = *d.get(*pos)?;
    *pos += 1;
    let (major, info) = (b >> 5, b & 31);
    let mut take = |n: usize| -> Option<u64> {
        let s = d.get(*pos..pos.checked_add(n)?)?;
        *pos += n;
        Some(s.iter().fold(0u64, |a, x| (a << 8) | u64::from(*x)))
    };
    let arg = match info {
        0..=23 => u64::from(info),
        24 => take(1)?,
        25 => take(2)?,
        26 => take(4)?,
        27 => take(8)?,
        _ => return None, // indefinite lengths are not used here
    };
    let len = || usize::try_from(arg).ok();
    Some(match major {
        0 => Cbor::Int(i128::from(arg)),
        1 => Cbor::Int(-1 - i128::from(arg)),
        2 | 3 => {
            let n = len()?;
            let s = d.get(*pos..pos.checked_add(n)?)?;
            *pos += n;
            if major == 2 {
                Cbor::Bytes(s.to_vec())
            } else {
                Cbor::Text(String::from_utf8(s.to_vec()).ok()?)
            }
        }
        4 => {
            let n = len().filter(|n| *n <= 64)?;
            for _ in 0..n {
                cbor(d, pos, depth + 1)?;
            }
            Cbor::Other
        }
        5 => {
            let n = len().filter(|n| *n <= 64)?;
            let mut m = Vec::new();
            for _ in 0..n {
                m.push((cbor(d, pos, depth + 1)?, cbor(d, pos, depth + 1)?));
            }
            Cbor::Map(m)
        }
        6 => cbor(d, pos, depth + 1)?,
        _ => Cbor::Other,
    })
}

fn map_get<'a>(m: &'a [(Cbor, Cbor)], key: &Cbor) -> Option<&'a Cbor> {
    m.iter().find(|(k, _)| same(k, key)).map(|(_, v)| v)
}

fn same(a: &Cbor, b: &Cbor) -> bool {
    match (a, b) {
        (Cbor::Int(x), Cbor::Int(y)) => x == y,
        (Cbor::Text(x), Cbor::Text(y)) => x == y,
        _ => false,
    }
}

// ---------- WebAuthn checks ----------

struct AuthData<'a> {
    rp_hash: &'a [u8],
    flags: u8,
    count: u32,
    rest: &'a [u8],
}

fn auth_data(d: &[u8]) -> Option<AuthData<'_>> {
    if d.len() < 37 {
        return None;
    }
    Some(AuthData {
        rp_hash: &d[..32],
        flags: d[32],
        count: u32::from_be_bytes(d[33..37].try_into().ok()?),
        rest: &d[37..],
    })
}

const FLAG_UP: u8 = 0x01;
const FLAG_AT: u8 = 0x40;

/// Checks clientDataJSON: operation type, our challenge, and that the origin is this very host.
fn client_data_ok(json: &[u8], want_type: &str, challenge: &str, host_port: &str) -> bool {
    let Ok(v) = serde_json::from_slice::<Value>(json) else {
        return false;
    };
    let origin = v["origin"].as_str().unwrap_or("");
    let origin_host = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .unwrap_or("");
    v["type"] == want_type
        && v["challenge"].as_str() == Some(challenge)
        && !host_port.is_empty()
        && origin_host.eq_ignore_ascii_case(host_port)
}

/// Verifies a registration; returns (credential id, SEC1 public key, signature counter).
fn check_registration(
    attestation: &[u8],
    client: &[u8],
    p: &Pending,
    host_port: &str,
) -> Result<(Vec<u8>, Vec<u8>, u32), &'static str> {
    const BAD: &str = "The passkey could not be verified.";
    if !client_data_ok(client, "webauthn.create", &p.challenge, host_port) {
        return Err(BAD);
    }
    let mut pos = 0;
    let Some(Cbor::Map(att)) = cbor(attestation, &mut pos, 0) else {
        return Err(BAD);
    };
    let Some(Cbor::Bytes(ad)) = map_get(&att, &Cbor::Text("authData".into())) else {
        return Err(BAD);
    };
    let a = auth_data(ad).ok_or(BAD)?;
    if a.rp_hash != Sha256::digest(p.rp.as_bytes()).as_slice()
        || a.flags & FLAG_UP == 0
        || a.flags & FLAG_AT == 0
        || a.rest.len() < 18
    {
        return Err(BAD);
    }
    let id_len = usize::from(u16::from_be_bytes([a.rest[16], a.rest[17]]));
    let id = a.rest.get(18..18 + id_len).ok_or(BAD)?;
    if id.is_empty() || id_len > 1023 {
        return Err(BAD);
    }
    let mut pos = 0;
    let Some(Cbor::Map(key)) = cbor(&a.rest[18 + id_len..], &mut pos, 0) else {
        return Err(BAD);
    };
    let int = |n: i128| map_get(&key, &Cbor::Int(n));
    let (Some(Cbor::Int(2)), Some(Cbor::Int(-7)), Some(Cbor::Int(1))) = (int(1), int(3), int(-1))
    else {
        return Err("This authenticator does not offer an ES256 (P-256) key.");
    };
    let (Some(Cbor::Bytes(x)), Some(Cbor::Bytes(y))) = (int(-2), int(-3)) else {
        return Err(BAD);
    };
    if x.len() != 32 || y.len() != 32 {
        return Err(BAD);
    }
    let sec1 = [&[4u8][..], x, y].concat();
    VerifyingKey::from_sec1_bytes(&sec1).map_err(|_| BAD)?;
    Ok((id.to_vec(), sec1, a.count))
}

/// Verifies a sign-in assertion; returns the new signature counter.
fn check_assertion(
    pk: &Passkey,
    client: &[u8],
    authenticator_data: &[u8],
    signature: &[u8],
    p: &Pending,
    host_port: &str,
) -> Result<u32, &'static str> {
    const BAD: &str = "The passkey could not be verified.";
    if !client_data_ok(client, "webauthn.get", &p.challenge, host_port) || pk.rp != p.rp {
        return Err(BAD);
    }
    let a = auth_data(authenticator_data).ok_or(BAD)?;
    if a.rp_hash != Sha256::digest(p.rp.as_bytes()).as_slice() || a.flags & FLAG_UP == 0 {
        return Err(BAD);
    }
    // A counter that did not grow means a cloned authenticator (0 forever is allowed: many don't count).
    if (pk.count > 0 || a.count > 0) && a.count <= pk.count {
        return Err(BAD);
    }
    let key = unb64u(&pk.key)
        .and_then(|k| VerifyingKey::from_sec1_bytes(&k).ok())
        .ok_or(BAD)?;
    let sig = Signature::from_der(signature).map_err(|_| BAD)?;
    let msg = [authenticator_data, Sha256::digest(client).as_slice()].concat();
    key.verify(&msg, &sig).map_err(|_| BAD)?;
    Ok(a.count)
}

// ---------- storage and state ----------

/// The hostname and `host[:port]` of this request, or `None` for an IP address
/// (WebAuthn needs a domain name) or a missing Host header.
pub(super) fn host_of(headers: &header::HeaderMap) -> Option<(String, String)> {
    let hp = headers
        .get(header::HOST)?
        .to_str()
        .ok()?
        .to_ascii_lowercase();
    if hp.starts_with('[') {
        return None;
    }
    let host = hp.split(':').next()?.to_string();
    if host.is_empty() || host.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    Some((host, hp))
}

impl Auth {
    fn all_passkeys(&self) -> Vec<Passkey> {
        self.file(PASSKEY_FILE)
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str::<Vec<Passkey>>(&t).ok())
            .unwrap_or_default()
    }

    fn save_passkeys(&self, list: &[Passkey]) -> io::Result<()> {
        let path = self
            .file(PASSKEY_FILE)
            .ok_or_else(|| io::Error::other("no data directory"))?;
        if list.is_empty() {
            return match fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        write_private(&path, &serde_json::to_string(list).unwrap_or_default())
    }

    pub(super) fn has_passkeys(&self) -> bool {
        !self.all_passkeys().is_empty()
    }

    fn new_pending(&self, kind: Kind, rp: &str) -> (String, String) {
        let id = random_token();
        let challenge = b64u(&rand::random::<[u8; 32]>());
        let now = now_unix();
        let mut map = self.pk_pending.lock().unwrap();
        map.retain(|_, p| p.expires > now);
        if map.len() >= MAX_PENDING {
            map.clear();
        }
        map.insert(
            id.clone(),
            Pending {
                kind,
                challenge: challenge.clone(),
                rp: rp.to_string(),
                expires: now + PENDING_TTL,
            },
        );
        (id, challenge)
    }

    fn pending(&self, id: &str, kind: Kind) -> Option<Pending> {
        let map = self.pk_pending.lock().unwrap();
        let p = map.get(id)?;
        (p.kind == kind && p.expires > now_unix()).then(|| Pending {
            kind: p.kind,
            challenge: p.challenge.clone(),
            rp: p.rp.clone(),
            expires: p.expires,
        })
    }

    fn drop_pending(&self, id: &str) {
        self.pk_pending.lock().unwrap().remove(id);
    }

    /// Passkeys usable on `rp`, for the sign-in options.
    pub(super) fn passkeys_for(&self, rp: &str) -> Vec<Passkey> {
        self.all_passkeys()
            .into_iter()
            .filter(|k| k.rp == rp)
            .collect()
    }

    /// WebAuthn `get()` options for a password-verified sign-in, plus its pending id.
    pub(super) fn login_challenge(&self, rp: &str) -> (String, String) {
        let (id, challenge) = self.new_pending(Kind::Login, rp);
        let options = self.login_options(&id, &challenge, rp);
        (id, options)
    }

    fn login_options(&self, id: &str, challenge: &str, rp: &str) -> String {
        let allow: Vec<Value> = self
            .passkeys_for(rp)
            .iter()
            .map(|k| json!({"type": "public-key", "id": k.id}))
            .collect();
        json!({"pending": id, "publicKey": {
            "challenge": challenge, "rpId": rp, "allowCredentials": allow,
            "userVerification": "preferred", "timeout": 120_000}})
        .to_string()
    }

    /// Options for the sign-in page of a pending login that is still valid.
    pub(super) fn login_options_for(&self, id: &str) -> Option<String> {
        let p = self.pending(id, Kind::Login)?;
        Some(self.login_options(id, &p.challenge, &p.rp))
    }

    pub(super) fn finish_login_pending(&self, id: &str) {
        self.drop_pending(id);
    }

    pub(super) fn login_pending_ok(&self, id: &str) -> bool {
        self.pending(id, Kind::Login).is_some()
    }

    /// Sign-in with an assertion. Consumes the pending login whatever the outcome.
    pub(super) fn verify_login_assertion(
        &self,
        a: &Assertion,
        host_port: &str,
    ) -> Result<(), &'static str> {
        const BAD: &str = "The passkey could not be verified.";
        let p = self.pending(&a.pending, Kind::Login).ok_or(BAD)?;
        self.drop_pending(&a.pending);
        let mut list = self.all_passkeys();
        let pk = list.iter_mut().find(|k| k.id == a.id).ok_or(BAD)?;
        let (client, ad, sig) = (
            unb64u(&a.client_data_json).ok_or(BAD)?,
            unb64u(&a.authenticator_data).ok_or(BAD)?,
            unb64u(&a.signature).ok_or(BAD)?,
        );
        let count = check_assertion(pk, &client, &ad, &sig, &p, host_port)?;
        pk.count = count;
        self.save_passkeys(&list).map_err(|_| BAD)
    }
}

// ---------- sign-in endpoints ----------

#[derive(Deserialize)]
pub struct Assertion {
    pending: String,
    id: String,
    #[serde(rename = "clientDataJSON")]
    client_data_json: String,
    #[serde(rename = "authenticatorData")]
    authenticator_data: String,
    signature: String,
}

pub async fn login_passkey(
    State(auth): State<SharedAuth>,
    info: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: header::HeaderMap,
    Json(a): Json<Assertion>,
) -> Response {
    let src = source(info, &headers);
    let fail = |status: StatusCode, msg: &str| {
        (status, Json(json!({"ok": false, "error": msg}))).into_response()
    };
    if auth.locked() {
        crate::audit::record("login.locked", &src);
        return fail(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many attempts. Try again in a minute.",
        );
    }
    let Some((_, host_port)) = host_of(&headers) else {
        return fail(
            StatusCode::BAD_REQUEST,
            "Open the panel through its hostname.",
        );
    };
    if let Err(msg) = auth.verify_login_assertion(&a, &host_port) {
        auth.record_failure();
        crate::audit::record("login.failed", &format!("{src} (passkey)"));
        return fail(StatusCode::UNAUTHORIZED, msg);
    }
    auth.record_success();
    crate::audit::record("login", &format!("{src} (passkey)"));
    crate::signin::success(&src);
    let token = auth.create_session();
    let mut res = Json(json!({"ok": true})).into_response();
    res.headers_mut()
        .insert(header::SET_COOKIE, auth.cookie(&token, SESSION_TTL_SECS));
    res
}

#[derive(Deserialize)]
pub struct CodeLogin {
    pending: String,
    code: String,
}

/// The sign-in page's fallback: an authenticator or recovery code instead of the passkey.
pub async fn login_code(
    State(auth): State<SharedAuth>,
    info: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: header::HeaderMap,
    Form(f): Form<CodeLogin>,
) -> Response {
    let src = source(info, &headers);
    if auth.locked() {
        crate::audit::record("login.locked", &src);
        return login_page(
            StatusCode::TOO_MANY_REQUESTS,
            Some("Too many attempts. Try again in a minute."),
        );
    }
    if !auth.login_pending_ok(&f.pending) {
        return Redirect::to("/login").into_response();
    }
    if f.code.trim().is_empty() || !auth.second_factor_ok(&f.code) {
        auth.record_failure();
        crate::audit::record("login.failed", &src);
        return passkey_login_page(&auth, &f.pending, Some("That code is not right."));
    }
    auth.finish_login_pending(&f.pending);
    auth.record_success();
    crate::audit::record("login", &src);
    crate::signin::success(&src);
    let token = auth.create_session();
    let mut res = Redirect::to("/").into_response();
    res.headers_mut()
        .insert(header::SET_COOKIE, auth.cookie(&token, SESSION_TTL_SECS));
    res
}

#[derive(Template)]
#[template(path = "login_passkey.html")]
struct LoginPasskeyTemplate {
    error: Option<&'static str>,
    assets: String,
    pending: String,
    options: String,
}

pub(super) fn passkey_login_page(
    auth: &Auth,
    pending: &str,
    error: Option<&'static str>,
) -> Response {
    let Some(options) = auth.login_options_for(pending) else {
        return Redirect::to("/login").into_response();
    };
    let body = LoginPasskeyTemplate {
        error,
        assets: crate::asset_version(),
        pending: pending.to_string(),
        options,
    }
    .render()
    .expect("login passkey template renders");
    Html(body).into_response()
}

// ---------- settings: list, add, remove ----------

pub struct Row {
    id: String,
    name: String,
    rp: String,
}

#[derive(Template)]
#[template(path = "passkeys.html")]
struct PasskeysTemplate {
    available: bool,
    can_add: bool,
    host: String,
    list: Vec<Row>,
    recovery: Vec<String>,
    error: Option<&'static str>,
}

fn render(
    auth: &Auth,
    headers: &header::HeaderMap,
    recovery: Vec<String>,
    error: Option<&'static str>,
) -> String {
    let host = host_of(headers).map(|h| h.0);
    PasskeysTemplate {
        available: auth.dir.is_some(),
        can_add: host.is_some(),
        host: host.unwrap_or_default(),
        list: auth
            .all_passkeys()
            .into_iter()
            .map(|k| Row {
                id: k.id,
                name: k.name,
                rp: k.rp,
            })
            .collect(),
        recovery,
        error,
    }
    .render()
    .expect("passkeys template renders")
}

pub async fn show(State(auth): State<SharedAuth>, headers: header::HeaderMap) -> Html<String> {
    Html(render(&auth, &headers, Vec::new(), None))
}

pub async fn register_options(
    State(auth): State<SharedAuth>,
    headers: header::HeaderMap,
) -> Response {
    let fail = |msg: &str| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": msg})),
        )
            .into_response()
    };
    let Some((rp, _)) = host_of(&headers) else {
        return fail("Open the panel through its hostname (not an IP address) to add a passkey.");
    };
    if auth.dir.is_none() {
        return fail("Passkeys need the panel's data directory.");
    }
    let all = auth.all_passkeys();
    if all.len() >= MAX_PASSKEYS {
        return fail("That is the most passkeys the panel keeps.");
    }
    let exclude: Vec<Value> = all
        .iter()
        .filter(|k| k.rp == rp)
        .map(|k| json!({"type": "public-key", "id": k.id}))
        .collect();
    let (id, challenge) = auth.new_pending(Kind::Register, &rp);
    Json(json!({"pending": id, "publicKey": {
        "challenge": challenge,
        "rp": {"id": rp, "name": "Dockwren"},
        "user": {"id": b64u(b"admin"), "name": "admin", "displayName": "Dockwren admin"},
        "pubKeyCredParams": [{"type": "public-key", "alg": -7}],
        "excludeCredentials": exclude,
        "attestation": "none",
        "authenticatorSelection": {"residentKey": "discouraged", "userVerification": "preferred"},
        "timeout": 120_000}}))
    .into_response()
}

#[derive(Deserialize)]
pub struct Registration {
    pending: String,
    id: String,
    #[serde(rename = "clientDataJSON")]
    client_data_json: String,
    #[serde(rename = "attestationObject")]
    attestation_object: String,
    #[serde(default)]
    name: String,
}

pub async fn register(
    State(auth): State<SharedAuth>,
    headers: header::HeaderMap,
    Json(r): Json<Registration>,
) -> Response {
    let fail = |msg: &str| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": msg})),
        )
            .into_response()
    };
    let Some((_, host_port)) = host_of(&headers) else {
        return fail("Open the panel through its hostname.");
    };
    let Some(p) = auth.pending(&r.pending, Kind::Register) else {
        return fail("The request expired. Try again.");
    };
    auth.drop_pending(&r.pending);
    let (Some(client), Some(att), Some(sent_id)) = (
        unb64u(&r.client_data_json),
        unb64u(&r.attestation_object),
        unb64u(&r.id),
    ) else {
        return fail("The passkey could not be verified.");
    };
    let (cred_id, sec1, count) = match check_registration(&att, &client, &p, &host_port) {
        Ok(v) if v.0 == sent_id => v,
        Ok(_) => return fail("The passkey could not be verified."),
        Err(e) => return fail(e),
    };
    let mut list = auth.all_passkeys();
    let id = b64u(&cred_id);
    if list.iter().any(|k| k.id == id) || list.len() >= MAX_PASSKEYS {
        return fail("That passkey is already registered.");
    }
    let name: String = r
        .name
        .chars()
        .filter(|c| !c.is_control())
        .take(40)
        .collect();
    list.push(Passkey {
        id,
        key: b64u(&sec1),
        rp: p.rp.clone(),
        count,
        name: if name.trim().is_empty() {
            format!("Passkey {}", list.len() + 1)
        } else {
            name.trim().to_string()
        },
    });
    // Without TOTP the recovery codes are the only way in if the passkey is lost: make them now.
    let recovery = match auth.ensure_recovery() {
        Ok(c) => c,
        Err(_) => return fail("Could not save the settings."),
    };
    if auth.save_passkeys(&list).is_err() {
        return fail("Could not save the settings.");
    }
    crate::audit::record("passkey.add", &p.rp);
    Json(json!({"ok": true, "html": render(&auth, &headers, recovery, None)})).into_response()
}

#[derive(Deserialize)]
pub struct Remove {
    id: String,
    code: String,
}

pub async fn remove(
    State(auth): State<SharedAuth>,
    headers: header::HeaderMap,
    Form(f): Form<Remove>,
) -> Html<String> {
    // Needs a current code or a recovery code, so a stolen session alone cannot remove it.
    if !auth.second_factor_ok(&f.code) {
        auth.record_failure();
        return Html(render(
            &auth,
            &headers,
            Vec::new(),
            Some("That code is not right."),
        ));
    }
    let mut list = auth.all_passkeys();
    let before = list.len();
    list.retain(|k| k.id != f.id);
    if list.len() != before {
        let _ = auth.save_passkeys(&list);
        auth.drop_recovery_if_unused();
        crate::audit::record("passkey.remove", "");
    }
    Html(render(&auth, &headers, Vec::new(), None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::{SigningKey, signature::Signer};

    fn host() -> String {
        "panel.example.test".to_string()
    }

    fn enc_len(major: u8, n: usize) -> Vec<u8> {
        let m = major << 5;
        if n < 24 {
            vec![m | n as u8]
        } else {
            vec![m | 25, (n >> 8) as u8, n as u8]
        }
    }
    fn enc_bytes(b: &[u8]) -> Vec<u8> {
        [enc_len(2, b.len()), b.to_vec()].concat()
    }
    fn enc_text(t: &str) -> Vec<u8> {
        [enc_len(3, t.len()), t.as_bytes().to_vec()].concat()
    }
    fn enc_int(n: i64) -> Vec<u8> {
        if n >= 0 {
            enc_len(0, n as usize)
        } else {
            enc_len(1, (-1 - n) as usize)
        }
    }

    fn key() -> SigningKey {
        SigningKey::from_slice(&[7u8; 32]).unwrap()
    }

    fn sec1(k: &SigningKey) -> Vec<u8> {
        k.verifying_key().to_sec1_point(false).as_bytes().to_vec()
    }

    fn client_json(kind: &str, challenge: &str, origin: &str) -> Vec<u8> {
        json!({"type": kind, "challenge": challenge, "origin": origin})
            .to_string()
            .into_bytes()
    }

    fn pending(kind: Kind) -> Pending {
        Pending {
            kind,
            challenge: "abc".into(),
            rp: "panel.example.test".into(),
            expires: now_unix() + 60,
        }
    }

    fn attestation(rp: &str, cred_id: &[u8], k: &SigningKey, flags: u8) -> Vec<u8> {
        let sec = sec1(k);
        let mut cose = vec![0xa5];
        cose.extend(enc_int(1));
        cose.extend(enc_int(2));
        cose.extend(enc_int(3));
        cose.extend(enc_int(-7));
        cose.extend(enc_int(-1));
        cose.extend(enc_int(1));
        cose.extend(enc_int(-2));
        cose.extend(enc_bytes(&sec[1..33]));
        cose.extend(enc_int(-3));
        cose.extend(enc_bytes(&sec[33..]));
        let mut ad = Sha256::digest(rp.as_bytes()).to_vec();
        ad.push(flags);
        ad.extend([0, 0, 0, 0]);
        ad.extend([0u8; 16]);
        ad.extend((cred_id.len() as u16).to_be_bytes());
        ad.extend(cred_id);
        ad.extend(cose);
        let mut out = vec![0xa3];
        out.extend(enc_text("fmt"));
        out.extend(enc_text("none"));
        out.extend(enc_text("attStmt"));
        out.push(0xa0);
        out.extend(enc_text("authData"));
        out.extend(enc_bytes(&ad));
        out
    }

    #[test]
    fn base64url_round_trips() {
        for n in 0..40 {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(unb64u(&b64u(&data)).unwrap(), data);
        }
        assert_eq!(b64u(&[0xfb, 0xff]), "-_8");
        assert!(unb64u("a$b").is_none());
    }

    #[test]
    fn registration_is_accepted_and_bad_ones_refused() {
        let k = key();
        let p = pending(Kind::Register);
        let good = client_json("webauthn.create", "abc", "https://panel.example.test");
        let att = attestation(&host(), b"cred-1", &k, FLAG_UP | FLAG_AT);
        let (id, pk, _) = check_registration(&att, &good, &p, &host()).unwrap();
        assert_eq!(id, b"cred-1");
        assert_eq!(pk, sec1(&k));
        // Wrong challenge, wrong origin, wrong operation.
        for bad in [
            client_json("webauthn.create", "zzz", "https://panel.example.test"),
            client_json("webauthn.create", "abc", "https://evil.example.test"),
            client_json("webauthn.get", "abc", "https://panel.example.test"),
        ] {
            assert!(check_registration(&att, &bad, &p, &host()).is_err());
        }
        // Authenticator bound to another rp id, or without user presence.
        let other = attestation("evil.example.test", b"cred-1", &k, FLAG_UP | FLAG_AT);
        assert!(check_registration(&other, &good, &p, &host()).is_err());
        let nouser = attestation(&host(), b"cred-1", &k, FLAG_AT);
        assert!(check_registration(&nouser, &good, &p, &host()).is_err());
        assert!(check_registration(b"\xff\xff", &good, &p, &host()).is_err());
    }

    fn assertion(k: &SigningKey, rp: &str, count: u32, client: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut ad = Sha256::digest(rp.as_bytes()).to_vec();
        ad.push(FLAG_UP);
        ad.extend(count.to_be_bytes());
        let msg = [ad.as_slice(), Sha256::digest(client).as_slice()].concat();
        let sig: Signature = k.sign(&msg);
        (ad, sig.to_der().as_bytes().to_vec())
    }

    fn stored(count: u32) -> Passkey {
        Passkey {
            id: "x".into(),
            key: b64u(&sec1(&key())),
            rp: "panel.example.test".into(),
            count,
            name: "t".into(),
        }
    }

    #[test]
    fn assertion_checks_signature_counter_and_binding() {
        let k = key();
        let p = pending(Kind::Login);
        let client = client_json("webauthn.get", "abc", "https://panel.example.test");
        let (ad, sig) = assertion(&k, &host(), 5, &client);
        assert_eq!(
            check_assertion(&stored(4), &client, &ad, &sig, &p, &host()),
            Ok(5)
        );
        // Replayed or cloned: counter did not grow.
        assert!(check_assertion(&stored(5), &client, &ad, &sig, &p, &host()).is_err());
        // Counter that stays 0 is allowed.
        let (ad0, sig0) = assertion(&k, &host(), 0, &client);
        assert_eq!(
            check_assertion(&stored(0), &client, &ad0, &sig0, &p, &host()),
            Ok(0)
        );
        // Tampered client data, another key, another host.
        let other = client_json("webauthn.get", "abc", "https://evil.example.test");
        assert!(check_assertion(&stored(4), &other, &ad, &sig, &p, &host()).is_err());
        let k2 = SigningKey::from_slice(&[9u8; 32]).unwrap();
        let (ad2, sig2) = assertion(&k2, &host(), 5, &client);
        assert!(check_assertion(&stored(4), &client, &ad2, &sig2, &p, &host()).is_err());
        let (ad3, sig3) = assertion(&k, "evil.example.test", 5, &client);
        assert!(check_assertion(&stored(4), &client, &ad3, &sig3, &p, &host()).is_err());
        let mut bound = stored(4);
        bound.rp = "elsewhere.example.test".into();
        assert!(check_assertion(&bound, &client, &ad, &sig, &p, &host()).is_err());
    }

    #[test]
    fn host_header_is_parsed() {
        let mut h = header::HeaderMap::new();
        h.insert(header::HOST, "Panel.Example.test:8443".parse().unwrap());
        assert_eq!(
            host_of(&h),
            Some((
                "panel.example.test".into(),
                "panel.example.test:8443".into()
            ))
        );
        for bad in ["203.0.113.5:8080", "[::1]:80", "localhost"] {
            h.insert(header::HOST, bad.parse().unwrap());
            if bad == "localhost" {
                assert!(host_of(&h).is_some());
            } else {
                assert!(host_of(&h).is_none(), "{bad}");
            }
        }
    }

    #[test]
    fn cbor_rejects_truncated_and_deep_input() {
        let mut pos = 0;
        assert!(cbor(&[0x58, 0x20, 1, 2], &mut pos, 0).is_none());
        let deep = vec![0x81; 20];
        pos = 0;
        assert!(cbor(&deep, &mut pos, 0).is_none());
    }
}
