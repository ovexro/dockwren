//! Time-based one-time passwords (RFC 6238, SHA-1, 6 digits, 30 s) as authenticator apps
//! such as Google Authenticator expect them. Pure functions; the login flow is in `auth`.

use sha1::{Digest, Sha1};

pub const STEP_SECS: u64 = 30;
const DIGITS: u32 = 6;
const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub fn new_secret() -> Vec<u8> {
    let mut s = vec![0u8; 20];
    rand::fill(&mut s[..]);
    s
}

/// RFC 4648 base32 without padding (what authenticator apps show and accept).
pub fn base32(data: &[u8]) -> String {
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in data {
        buf = (buf << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(B32[((buf >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(B32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn from_base32(s: &str) -> Option<Vec<u8>> {
    let (mut buf, mut bits) = (0u32, 0u32);
    let mut out = Vec::new();
    for c in s.trim().bytes() {
        let v = B32.iter().position(|&x| x == c.to_ascii_uppercase())? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn hmac_sha1(key: &[u8], msg: &[u8]) -> [u8; 20] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..20].copy_from_slice(&Sha1::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha1::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let mut outer = Sha1::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

fn hotp(secret: &[u8], counter: u64) -> u32 {
    let h = hmac_sha1(secret, &counter.to_be_bytes());
    let o = (h[19] & 15) as usize;
    let n = u32::from_be_bytes([h[o] & 0x7f, h[o + 1], h[o + 2], h[o + 3]]);
    n % 10u32.pow(DIGITS)
}

#[cfg(test)]
pub fn code_at(secret: &[u8], unix: u64) -> String {
    format!("{:06}", hotp(secret, unix / STEP_SECS))
}

/// Accepts the previous, current and next step (clock drift). Returns the matching step so
/// the caller can refuse to accept the same one twice.
pub fn verify(secret: &[u8], input: &str, unix: u64, after_step: u64) -> Option<u64> {
    let digits: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    if digits.len() != DIGITS as usize || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let now = unix / STEP_SECS;
    let mut found = None;
    for step in now.saturating_sub(1)..=now + 1 {
        // Check every candidate so timing does not reveal which one matched.
        if same(&format!("{:06}", hotp(secret, step)), &digits) && step > after_step {
            found = Some(step);
        }
    }
    found
}

pub fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |d, (x, y)| d | (x ^ y)) == 0
}

/// `otpauth://` link the QR code carries. `label` and `issuer` are fixed, URL-safe words.
pub fn otpauth_uri(secret: &[u8], label: &str, issuer: &str) -> String {
    format!(
        "otpauth://totp/{issuer}:{label}?secret={}&issuer={issuer}&algorithm=SHA1&digits={DIGITS}&period={STEP_SECS}",
        base32(secret)
    )
}

pub fn qr_svg(data: &str) -> Option<String> {
    use qrcode::{QrCode, render::svg};
    let code = QrCode::new(data.as_bytes()).ok()?;
    Some(
        code.render::<svg::Color>()
            .min_dimensions(200, 200)
            .quiet_zone(true)
            .dark_color(svg::Color("#000000"))
            .light_color(svg::Color("#ffffff"))
            .build(),
    )
}

/// One-time recovery code like `k7m2-q9xd-4tpa` (about 60 bits).
pub fn recovery_code() -> String {
    const CH: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut b = [0u8; 12];
    rand::fill(&mut b[..]);
    let s: Vec<char> = b
        .iter()
        .map(|x| CH[*x as usize % CH.len()] as char)
        .collect();
    s.chunks(4)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    const RFC_KEY: &[u8] = b"12345678901234567890";

    #[test]
    fn matches_rfc_6238_vectors() {
        // RFC 6238 appendix B (SHA-1), last 6 of the 8-digit values.
        assert_eq!(code_at(RFC_KEY, 59), "287082");
        assert_eq!(code_at(RFC_KEY, 1111111109), "081804");
        assert_eq!(code_at(RFC_KEY, 1234567890), "005924");
        assert_eq!(code_at(RFC_KEY, 20000000000), "353130");
    }

    #[test]
    fn base32_round_trips_and_matches_rfc_4648() {
        assert_eq!(base32(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32(b"fo"), "MZXQ");
        assert_eq!(from_base32("mzxw6ytboi").unwrap(), b"foobar");
        assert_eq!(from_base32("MZXW6YTBOI1"), None);
        let s = new_secret();
        assert_eq!(from_base32(&base32(&s)).unwrap(), s);
    }

    #[test]
    fn window_and_replay() {
        let t = 1_700_000_000;
        let cur = code_at(RFC_KEY, t);
        let prev = code_at(RFC_KEY, t - 30);
        let old = code_at(RFC_KEY, t - 90);
        assert_eq!(verify(RFC_KEY, &cur, t, 0), Some(t / 30));
        assert_eq!(verify(RFC_KEY, &prev, t, 0), Some(t / 30 - 1));
        assert_eq!(verify(RFC_KEY, &old, t, 0), None);
        // The same step cannot be used again.
        assert_eq!(verify(RFC_KEY, &cur, t, t / 30), None);
        // Spaces are fine, junk is not.
        let spaced = format!("{} {}", &cur[..3], &cur[3..]);
        assert!(verify(RFC_KEY, &spaced, t, 0).is_some());
        assert_eq!(verify(RFC_KEY, "12345a", t, 0), None);
        assert_eq!(verify(RFC_KEY, "", t, 0), None);
    }

    #[test]
    fn uri_and_qr_and_recovery_code_shapes() {
        let u = otpauth_uri(RFC_KEY, "admin", "Dockwren");
        assert!(u.starts_with(&format!(
            "otpauth://totp/Dockwren:admin?secret={}",
            base32(RFC_KEY)
        )));
        assert!(qr_svg(&u).unwrap().contains("<svg"));
        let r = recovery_code();
        assert_eq!(r.len(), 14);
        assert_eq!(r.matches('-').count(), 2);
    }
}
