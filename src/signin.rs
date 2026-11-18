//! Sign-in notifications: a sign-in from an address never seen before, and lockouts after
//! repeated wrong passwords or codes. The audit log keeps the full record; this raises the
//! two events worth interrupting someone for.
//!
//! Known addresses live in `signin-known.txt` in the data directory (one per line, newest
//! last, at most `MAX_KNOWN`). On the first run after an upgrade the file does not exist yet,
//! so it is seeded from past `login` entries in the audit log and nothing is raised.

use crate::notify::{self, NewEvent, Severity};
use std::{
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

pub const NEW_SOURCE: &str = "login.new_source";
pub const LOCKOUT: &str = "login.lockout";
const FILE: &str = "signin-known.txt";
const MAX_KNOWN: usize = 100;

struct Watch {
    notify: notify::Shared,
    path: PathBuf,
    known: Vec<String>,
}

static GLOBAL: OnceLock<Mutex<Watch>> = OnceLock::new();

/// Turn the watch on for this process (tests never call it).
pub fn init(notify: notify::Shared, data_dir: &Path) {
    let path = data_dir.join(FILE);
    let known = match std::fs::read_to_string(&path) {
        Ok(t) => t.lines().map(str::to_string).collect(),
        Err(_) => {
            let seeded = seed(crate::audit::Log::at(data_dir).recent(2000));
            save(&path, &seeded);
            seeded
        }
    };
    let _ = GLOBAL.set(Mutex::new(Watch {
        notify,
        path,
        known,
    }));
}

/// The address a sign-in counts under: the proxy's claim when there is one (the peer is then
/// just Caddy or a tunnel), else the peer. `detail` is an audit line such as
/// `from 10.0.0.2 (proxy says 203.0.113.7) (passkey)`.
pub fn address_of(detail: &str) -> Option<String> {
    let d = detail.strip_prefix("from ")?;
    let addr = match d.split_once("(proxy says ") {
        Some((_, rest)) => rest.split(')').next()?,
        None => d.split_whitespace().next()?,
    };
    let addr = addr.trim();
    let ok = !addr.is_empty()
        && addr.len() <= 60
        && addr
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | ',' | ' ' | '-'));
    ok.then(|| addr.to_string())
}

fn seed(entries: Vec<crate::audit::Entry>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in entries.into_iter().rev().filter(|e| e.action == "login") {
        if let Some(a) = address_of(&e.detail)
            && !out.contains(&a)
        {
            out.push(a);
        }
    }
    out.truncate(MAX_KNOWN);
    out
}

fn save(path: &Path, known: &[String]) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
    {
        let _ = f.write_all((known.join("\n") + "\n").as_bytes());
    }
}

/// Is this address new? Learns it either way. Pure bookkeeping, so it can be tested.
fn learn(known: &mut Vec<String>, addr: &str) -> bool {
    if known.iter().any(|k| k == addr) {
        return false;
    }
    known.push(addr.to_string());
    if known.len() > MAX_KNOWN {
        known.remove(0);
    }
    true
}

/// A sign-in succeeded (`detail` is the audit detail). Clears any lockout alert.
pub fn success(detail: &str) {
    let Some(g) = GLOBAL.get() else { return };
    let mut w = g.lock().unwrap();
    notify::resolve(&w.notify, LOCKOUT);
    let Some(addr) = address_of(detail) else {
        return;
    };
    let mut known = std::mem::take(&mut w.known);
    let new = learn(&mut known, &addr);
    w.known = known;
    if !new {
        return;
    }
    save(&w.path, &w.known);
    notify::raise(
        &w.notify,
        NewEvent::new(
            "auth",
            NEW_SOURCE,
            Severity::Warning,
            &format!("Sign-in from a new address: {addr}"),
        )
        .body("Someone signed in to the panel from an address that has not signed in before. If that was you (new network, phone, VPN), dismiss this; if not, change the password and look at the Audit log.")
        .link("/settings/audit")
        .key(format!("{NEW_SOURCE}:{addr}")),
    );
}

/// Too many wrong attempts: the panel locked sign-ins for a minute.
pub fn lockout() {
    let Some(g) = GLOBAL.get() else { return };
    let w = g.lock().unwrap();
    notify::raise(
        &w.notify,
        NewEvent::new(
            "auth",
            LOCKOUT,
            Severity::Warning,
            "Sign-in locked after repeated wrong passwords or codes",
        )
        .body("Five failed attempts in a row. Sign-in is paused for a minute. If this was not you, someone is guessing; the Audit log shows the addresses.")
        .link("/settings/audit")
        .key(LOCKOUT),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_prefers_the_proxy_claim() {
        assert_eq!(address_of("from 10.0.0.2"), Some("10.0.0.2".into()));
        assert_eq!(
            address_of("from 10.0.0.2 (proxy says 203.0.113.7) (passkey)"),
            Some("203.0.113.7".into())
        );
        assert_eq!(address_of("from unknown"), Some("unknown".into()));
        assert_eq!(address_of("logout"), None);
        assert_eq!(address_of("from 1.2.3.4 (proxy says <script>)"), None);
    }

    #[test]
    fn learn_flags_only_the_first_sighting_and_caps() {
        let mut k = Vec::new();
        assert!(learn(&mut k, "1.1.1.1"));
        assert!(!learn(&mut k, "1.1.1.1"));
        for i in 0..MAX_KNOWN + 5 {
            learn(&mut k, &format!("9.9.9.{i}"));
        }
        assert_eq!(k.len(), MAX_KNOWN);
    }

    #[test]
    fn seed_reads_past_logins_only() {
        let e = |a: &str, d: &str| crate::audit::Entry {
            at: 1,
            action: a.into(),
            detail: d.into(),
        };
        let s = seed(vec![
            e("login.failed", "from 6.6.6.6"),
            e("login", "from 1.1.1.1"),
            e("login", "from 1.1.1.1"),
        ]);
        assert_eq!(s, vec!["1.1.1.1".to_string()]);
    }
}
