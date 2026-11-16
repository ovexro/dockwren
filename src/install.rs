//! Install core: turn a template plus form input into the values for an
//! instance's `.env` file. Pure functions except `write_env`; no Docker here.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::template::{Field, FieldType, SecretKind, Template};

/// Host ports handed out by `auto`.
const AUTO_PORTS: std::ops::Range<u16> = 8100..9000;

pub struct Resolved {
    /// Every variable for `.env` (fields + secrets), sorted by key.
    pub env: BTreeMap<String, String>,
    /// Plaintext of secrets with `reveal = true`, shown once and not stored.
    pub revealed: Vec<(String, String)>,
    /// Public hostname given in the form (`PUBLIC_HOST`), already validated.
    pub public_host: Option<String>,
}

/// Replace `${KEY}` in `s` using `env`; an unknown key is an error.
pub fn subst(s: &str, env: &BTreeMap<String, String>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let j = after.find('}').ok_or("unterminated ${")?;
        let key = &after[..j];
        out.push_str(
            env.get(key)
                .ok_or_else(|| format!("unknown variable {key}"))?,
        );
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// First free port in the auto range, skipping `claimed` (other instances and
/// ports already picked in this install).
fn allocate(claimed: &HashSet<u16>, free: &dyn Fn(u16) -> bool) -> Result<u16, String> {
    AUTO_PORTS
        .clone()
        .find(|p| !claimed.contains(p) && free(*p))
        .ok_or_else(|| "no free port left in the auto range".to_string())
}

/// Validate one value for its field type; returns the normalised value.
fn validate_field(f: &Field, value: &str) -> Result<String, String> {
    let bad = |m: &str| format!("{}: {m}", f.label);
    if value.contains(['\n', '\r', '\'']) {
        return Err(bad("must not contain quotes or line breaks"));
    }
    match f.kind {
        FieldType::String | FieldType::Password => {
            if let Some(p) = &f.pattern {
                let re = regex::Regex::new(p).map_err(|e| bad(&format!("bad pattern: {e}")))?;
                if !re.is_match(value) {
                    return Err(bad(&format!("does not match {p}")));
                }
            }
            let len = value.chars().count() as i64;
            if f.min.is_some_and(|m| len < m) || f.max.is_some_and(|m| len > m) {
                return Err(bad("wrong length"));
            }
            Ok(value.to_string())
        }
        FieldType::Int => {
            let n: i64 = value.parse().map_err(|_| bad("must be a number"))?;
            if f.min.is_some_and(|m| n < m) || f.max.is_some_and(|m| n > m) {
                return Err(bad("out of range"));
            }
            Ok(n.to_string())
        }
        FieldType::Bool => match value {
            "true" | "on" | "1" => Ok("true".into()),
            "false" | "off" | "0" => Ok("false".into()),
            _ => Err(bad("must be true or false")),
        },
        FieldType::Port => match value.parse::<u16>() {
            Ok(p) if p >= 1024 => Ok(p.to_string()),
            _ => Err(bad("must be a port between 1024 and 65535")),
        },
        FieldType::Url => {
            let ok = (value.starts_with("http://") || value.starts_with("https://"))
                && !value.contains(char::is_whitespace)
                && value.split("//").nth(1).is_some_and(|h| !h.is_empty());
            if !ok {
                return Err(bad("must be an http(s) URL"));
            }
            Ok(if value.ends_with('/') {
                value.into()
            } else {
                format!("{value}/")
            })
        }
        FieldType::Email => {
            let ok = value.split_once('@').is_some_and(|(u, d)| {
                !u.is_empty() && d.contains('.') && !d.starts_with('.') && !d.ends_with('.')
            }) && !value.contains(char::is_whitespace);
            if !ok {
                return Err(bad("must be an e-mail address"));
            }
            Ok(value.into())
        }
        FieldType::Select => {
            if !f.options.iter().any(|o| o == value) {
                return Err(bad("not one of the allowed options"));
            }
            Ok(value.into())
        }
    }
}

/// Resolve the form `input` against the template: apply defaults, allocate
/// `auto` ports, validate every value, then generate secrets.
/// `claimed` = ports used by existing instances; `free` = host-level check.
pub fn resolve(
    t: &Template,
    input: &HashMap<String, String>,
    claimed: &HashSet<u16>,
    free: &dyn Fn(u16) -> bool,
) -> Result<Resolved, String> {
    let public_host = input
        .get("PUBLIC_HOST")
        .map(|v| v.trim().to_lowercase())
        .filter(|v| !v.is_empty());
    if let Some(h) = &public_host {
        if t.manifest.app.public_url_field.is_none() && !t.manifest.app.accepts_public_host {
            return Err("this app cannot be given a public hostname at install".into());
        }
        if !crate::public::valid_hostname(h) {
            return Err(
                "Public hostname: lower-case letters, digits, hyphens, at least one dot".into(),
            );
        }
    }
    if !t.manifest.lan_ports.is_empty() && input.get("LAN_OK").map(String::as_str) != Some("on") {
        return Err(format!(
            "{}: tick the box to confirm you want these ports open to your whole network",
            t.manifest.app.name
        ));
    }
    let derived_url = public_host.as_ref().map(|h| format!("https://{h}"));
    let mut env = BTreeMap::new();
    let mut taken = claimed.clone();
    for f in &t.manifest.fields {
        let given = if t.manifest.app.public_url_field.as_deref() == Some(f.key.as_str()) {
            derived_url.as_deref()
        } else {
            None
        }
        .or_else(|| {
            input
                .get(&f.key)
                .map(|v| v.trim())
                .filter(|v| !v.is_empty())
        });
        let raw = match (given, &f.default) {
            (Some(v), _) => v.to_string(),
            (None, Some(d)) => subst(d, &env)?,
            (None, None) if !f.required => String::new(),
            (None, None) => return Err(format!("{}: required", f.label)),
        };
        if raw.is_empty() && !f.required {
            env.insert(f.key.clone(), raw);
            continue;
        }
        let value = if f.kind == FieldType::Port {
            if raw == "auto" {
                let p = allocate(&taken, free)?;
                taken.insert(p);
                p.to_string()
            } else {
                let v = validate_field(f, &raw)?;
                let p: u16 = v.parse().expect("validated port");
                if taken.contains(&p) || !free(p) {
                    return Err(format!("{}: port {p} is already in use", f.label));
                }
                taken.insert(p);
                v
            }
        } else {
            validate_field(f, &raw)?
        };
        env.insert(f.key.clone(), value);
    }

    let mut revealed = Vec::new();
    for s in &t.manifest.secrets {
        let plain = match s.kind {
            SecretKind::Hex => {
                let mut bytes = vec![0u8; s.length.div_ceil(2)];
                rand::fill(&mut bytes[..]);
                let mut h: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                h.truncate(s.length);
                h
            }
            _ => (0..s.length)
                .map(|_| {
                    const C: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789";
                    C[rand::random_range(0..C.len())] as char
                })
                .collect(),
        };
        let stored = match s.kind {
            SecretKind::Argon2 => crate::auth::hash_password(&plain),
            _ => plain.clone(),
        };
        if s.reveal {
            revealed.push((s.key.clone(), plain));
        }
        env.insert(s.key.clone(), stored);
    }
    Ok(Resolved {
        env,
        revealed,
        public_host,
    })
}

/// Apply Settings-page `input` to an existing instance's `current` env. Only fields marked
/// `editable` can change; everything else (secrets, ports, name) is carried over untouched.
/// A key missing from `input` keeps its value; an empty value falls back to the default.
pub fn apply_edits(
    t: &Template,
    current: &BTreeMap<String, String>,
    input: &HashMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    let mut env = current.clone();
    for f in t.manifest.fields.iter().filter(|f| f.editable) {
        let Some(given) = input.get(&f.key).map(|v| v.trim()) else {
            continue;
        };
        let raw = match (given.is_empty(), &f.default) {
            (false, _) => given.to_string(),
            (true, Some(d)) => subst(d, &env)?,
            (true, None) if !f.required => String::new(),
            (true, None) => return Err(format!("{}: required", f.label)),
        };
        let value = if raw.is_empty() {
            raw
        } else {
            validate_field(f, &raw)?
        };
        env.insert(f.key.clone(), value);
    }
    Ok(env)
}

/// Atomically replace `<dir>/.env` (temp file + rename, mode 0600).
pub fn replace_env(dir: &Path, env: &BTreeMap<String, String>) -> std::io::Result<()> {
    let tmp = dir.join(".env.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(render_env(env).as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, dir.join(".env"))
}

// Manual Debug so secrets can never end up in logs or panic messages.
impl std::fmt::Debug for Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolved")
            .field("keys", &self.env.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// Host-level port check: can we bind it on loopback right now?
pub fn port_is_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Is something already listening on this port (any address)? Read from /proc/net because a
/// bind attempt cannot tell "in use" from "not allowed" for ports below 1024 without root.
pub fn lan_port_free(port: u16, protocol: &str) -> bool {
    let files: &[&str] = if protocol == "udp" {
        &["/proc/net/udp", "/proc/net/udp6"]
    } else {
        &["/proc/net/tcp", "/proc/net/tcp6"]
    };
    let mut readable = false;
    for f in files {
        let Ok(text) = std::fs::read_to_string(f) else {
            continue;
        };
        readable = true;
        if listening_on(&text, port, protocol == "udp") {
            return false;
        }
    }
    if readable {
        return true;
    }
    // No /proc/net: fall back to a bind attempt.
    let err = if protocol == "udp" {
        std::net::UdpSocket::bind(("0.0.0.0", port)).err()
    } else {
        std::net::TcpListener::bind(("0.0.0.0", port)).err()
    };
    err.is_none_or(|e| e.kind() != std::io::ErrorKind::AddrInUse)
}

/// Does a /proc/net/{tcp,udp}[6] table hold a listening socket on `port`?
/// TCP: state 0A (LISTEN). UDP: state 07 (unconnected, i.e. bound).
fn listening_on(table: &str, port: u16, udp: bool) -> bool {
    let want = if udp { "07" } else { "0A" };
    table.lines().skip(1).any(|l| {
        let mut c = l.split_whitespace();
        let local = c.nth(1).unwrap_or("");
        let state = c.nth(1).unwrap_or("");
        state == want
            && local
                .rsplit_once(':')
                .is_some_and(|(_, p)| u16::from_str_radix(p, 16) == Ok(port))
    })
}

/// Refuse the install when a declared LAN port is already taken on this server.
pub fn check_lan_ports(t: &Template) -> Result<(), String> {
    for l in &t.manifest.lan_ports {
        if !lan_port_free(l.port, &l.protocol) {
            return Err(format!(
                "port {}/{} ({}) is already in use on this server",
                l.port, l.protocol, l.label
            ));
        }
    }
    Ok(())
}

/// `.env` content for Compose: `KEY='value'` lines (single quotes = no interpolation).
pub fn render_env(env: &BTreeMap<String, String>) -> String {
    env.iter().map(|(k, v)| format!("{k}='{v}'\n")).collect()
}

/// Write `<dir>/.env` with mode 0600; refuses to overwrite an existing file.
pub fn write_env(dir: &Path, env: &BTreeMap<String, String>) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join(".env"))?;
    f.write_all(render_env(env).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpl(id: &str) -> Template {
        crate::template::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("apps").join(id))
            .expect("bundled template")
    }

    fn input(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn all_free(_: u16) -> bool {
        true
    }

    #[test]
    fn defaults_and_auto_ports_for_vaultwarden() {
        let r = resolve(
            &tmpl("vaultwarden"),
            &input(&[]),
            &HashSet::new(),
            &all_free,
        )
        .unwrap();
        assert_eq!(r.env["INSTANCE_NAME"], "vaultwarden");
        assert_eq!(r.env["WEB_PORT"], "8100");
        assert_eq!(r.env["PUBLIC_URL"], "http://127.0.0.1:8100/");
        assert_eq!(r.env["SIGNUPS_ALLOWED"], "true");
    }

    #[test]
    fn public_host_sets_the_address_field() {
        let r = resolve(
            &tmpl("vaultwarden"),
            &input(&[
                ("PUBLIC_HOST", " Vault.Example.com "),
                ("PUBLIC_URL", "http://ignored/"),
            ]),
            &HashSet::new(),
            &all_free,
        )
        .unwrap();
        assert_eq!(r.env["PUBLIC_URL"], "https://vault.example.com/");
        assert_eq!(r.public_host.as_deref(), Some("vault.example.com"));
        let r = resolve(
            &tmpl("vaultwarden"),
            &input(&[]),
            &HashSet::new(),
            &all_free,
        )
        .unwrap();
        assert_eq!(r.public_host, None);
    }

    #[test]
    fn bad_public_host_and_apps_without_an_address_field_are_refused() {
        for bad in [
            "nodot",
            "a b.example.com",
            "-x.example.com",
            "x.example.com/path",
        ] {
            let e = resolve(
                &tmpl("vaultwarden"),
                &input(&[("PUBLIC_HOST", bad)]),
                &HashSet::new(),
                &all_free,
            )
            .unwrap_err();
            assert!(e.contains("Public hostname"), "{bad}: {e}");
        }
        let mut no_host = tmpl("memos");
        no_host.manifest.app.accepts_public_host = false;
        let e = resolve(
            &no_host,
            &input(&[("PUBLIC_HOST", "memo.example.com")]),
            &HashSet::new(),
            &all_free,
        )
        .unwrap_err();
        assert!(e.contains("cannot be given"), "{e}");
    }

    #[test]
    fn apps_without_an_address_field_can_still_take_a_public_host() {
        let r = resolve(
            &tmpl("memos"),
            &input(&[("PUBLIC_HOST", "memo.example.com")]),
            &HashSet::new(),
            &all_free,
        )
        .unwrap();
        assert_eq!(r.public_host.as_deref(), Some("memo.example.com"));
        assert!(!r.env.contains_key("PUBLIC_HOST"));
    }

    #[test]
    fn auto_ports_skip_claimed_busy_and_each_other() {
        let claimed: HashSet<u16> = [8100].into();
        let free = |p: u16| p != 8101;
        let r = resolve(
            &tmpl("gitea"),
            &input(&[("ADMIN_EMAIL", "a@b.io")]),
            &claimed,
            &free,
        )
        .unwrap();
        assert_eq!(r.env["WEB_PORT"], "8102");
        assert_eq!(r.env["SSH_PORT"], "8103");
    }

    #[test]
    fn explicit_port_must_be_free_and_unclaimed() {
        let t = tmpl("vaultwarden");
        let claimed: HashSet<u16> = [9500].into();
        let e = resolve(&t, &input(&[("WEB_PORT", "9500")]), &claimed, &all_free).unwrap_err();
        assert!(e.contains("in use"), "{e}");
        let e = resolve(
            &t,
            &input(&[("WEB_PORT", "80")]),
            &HashSet::new(),
            &all_free,
        )
        .unwrap_err();
        assert!(e.contains("1024"), "{e}");
        let e = resolve(
            &t,
            &input(&[("WEB_PORT", "9501")]),
            &HashSet::new(),
            &|_| false,
        )
        .unwrap_err();
        assert!(e.contains("in use"), "{e}");
    }

    #[test]
    fn required_and_validation_errors() {
        let t = tmpl("gitea");
        let none = HashSet::new();
        assert!(
            resolve(&t, &input(&[]), &none, &all_free)
                .unwrap_err()
                .contains("e-mail")
        );
        for (k, v) in [
            ("ADMIN_EMAIL", "nope"),
            ("INSTANCE_NAME", "Bad Name"),
            ("PUBLIC_URL", "ftp://x"),
            ("ADMIN_USER", "it's"),
        ] {
            let mut i = input(&[("ADMIN_EMAIL", "a@b.io")]);
            i.insert(k.into(), v.into());
            assert!(resolve(&t, &i, &none, &all_free).is_err(), "{k}={v}");
        }
    }

    #[test]
    fn url_is_normalised_and_bool_accepts_form_values() {
        let r = resolve(
            &tmpl("vaultwarden"),
            &input(&[
                ("PUBLIC_URL", "https://vault.example.com"),
                ("SIGNUPS_ALLOWED", "off"),
            ]),
            &HashSet::new(),
            &all_free,
        )
        .unwrap();
        assert_eq!(r.env["PUBLIC_URL"], "https://vault.example.com/");
        assert_eq!(r.env["SIGNUPS_ALLOWED"], "false");
    }

    #[test]
    fn secrets_are_generated_and_unique() {
        let t = tmpl("gitea");
        let i = input(&[("ADMIN_EMAIL", "a@b.io")]);
        let a = resolve(&t, &i, &HashSet::new(), &all_free).unwrap();
        let b = resolve(&t, &i, &HashSet::new(), &all_free).unwrap();
        assert_eq!(a.env["POSTGRES_PASSWORD"].len(), 32);
        assert_ne!(a.env["POSTGRES_PASSWORD"], b.env["POSTGRES_PASSWORD"]);
        assert!(a.revealed.is_empty());
    }

    #[test]
    fn edits_change_only_editable_fields() {
        let t = tmpl("vaultwarden");
        let mut cur = BTreeMap::new();
        cur.insert("SIGNUPS_ALLOWED".to_string(), "true".to_string());
        cur.insert("WEB_PORT".to_string(), "8100".to_string());
        cur.insert("ADMIN_TOKEN".to_string(), "secret".to_string());
        let input: HashMap<String, String> = [
            ("SIGNUPS_ALLOWED", "false"),
            ("WEB_PORT", "9999"),
            ("ADMIN_TOKEN", "x"),
            ("PUBLIC_URL", "https://evil/"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let new = apply_edits(&t, &cur, &input).unwrap();
        assert_eq!(new["SIGNUPS_ALLOWED"], "false");
        assert_eq!(new["WEB_PORT"], "8100");
        assert_eq!(new["ADMIN_TOKEN"], "secret");
        assert!(!new.contains_key("PUBLIC_URL"));
    }

    #[test]
    fn edits_validate_and_keep_missing_keys() {
        let t = tmpl("vaultwarden");
        let mut cur = BTreeMap::new();
        cur.insert("SIGNUPS_ALLOWED".to_string(), "true".to_string());
        let bad: HashMap<String, String> =
            [("SIGNUPS_ALLOWED".to_string(), "maybe".to_string())].into();
        assert!(apply_edits(&t, &cur, &bad).is_err());
        let quote: HashMap<String, String> =
            [("SIGNUPS_ALLOWED".to_string(), "a'b".to_string())].into();
        assert!(apply_edits(&t, &cur, &quote).is_err());
        let none = HashMap::new();
        assert_eq!(apply_edits(&t, &cur, &none).unwrap(), cur);
    }

    #[test]
    fn replace_env_is_private_and_round_trips() {
        let d = std::env::temp_dir().join(format!("dw-repl-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let mut a = BTreeMap::new();
        a.insert("A".to_string(), "1".to_string());
        write_env(&d, &a).unwrap();
        a.insert("A".to_string(), "two words".to_string());
        replace_env(&d, &a).unwrap();
        let mode = std::fs::metadata(d.join(".env")).unwrap().permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o600
        );
        assert_eq!(crate::runner::read_env(&d).unwrap(), a);
        assert!(!d.join(".env.tmp").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn env_file_is_quoted_private_and_not_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let env: BTreeMap<_, _> = [
            ("B".to_string(), "x$y".to_string()),
            ("A".to_string(), "1".to_string()),
        ]
        .into();
        assert_eq!(render_env(&env), "A='1'\nB='x$y'\n");
        let dir =
            std::env::temp_dir().join(format!("dockwren-env-{}", crate::auth::generate_password()));
        std::fs::create_dir_all(&dir).unwrap();
        write_env(&dir, &env).unwrap();
        let mode = std::fs::metadata(dir.join(".env"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(write_env(&dir, &env).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn port_check_sees_a_bound_port() {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(!port_is_free(port));
        drop(l);
        assert!(port_is_free(port));
    }

    #[test]
    fn lan_ports_need_the_confirmation_box() {
        let t = tmpl("adguard-home");
        let none = HashSet::new();
        let free = |_: u16| true;
        let e = resolve(&t, &input(&[]), &none, &free).unwrap_err();
        assert!(e.contains("open to your whole network"), "{e}");
        assert!(resolve(&t, &input(&[("LAN_OK", "yes")]), &none, &free).is_err());
        assert!(resolve(&t, &input(&[("LAN_OK", "on")]), &none, &free).is_ok());
        // Templates without LAN ports never ask.
        assert!(resolve(&tmpl("uptime-kuma"), &input(&[]), &none, &free).is_ok());
    }

    #[test]
    fn proc_net_tables_are_parsed() {
        let tcp = "  sl  local_address rem_address   st tx_queue\n   0: 00000000:0035 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0\n   1: 0100007F:1F90 0100007F:C350 01 00000000:00000000 00:00000000 00000000     0\n";
        assert!(listening_on(tcp, 53, false));
        assert!(!listening_on(tcp, 8080, false)); // established, not listening
        assert!(!listening_on(tcp, 54, false));
        let udp = "  sl  local_address rem_address   st\n   0: 00000000:0035 00000000:0000 07 0:0 00:0 0 0\n";
        assert!(listening_on(udp, 53, true));
    }

    #[test]
    fn lan_port_in_use_is_detected() {
        let l = std::net::UdpSocket::bind(("0.0.0.0", 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        assert!(!lan_port_free(p, "udp"));
        drop(l);
        assert!(lan_port_free(p, "udp"));
    }
}
