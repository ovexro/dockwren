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
}

/// Replace `${KEY}` in `s` using `env`; an unknown key is an error.
fn subst(s: &str, env: &BTreeMap<String, String>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let j = after.find('}').ok_or("unterminated ${")?;
        let key = &after[..j];
        out.push_str(env.get(key).ok_or_else(|| format!("unknown variable {key}"))?);
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
            Ok(if value.ends_with('/') { value.into() } else { format!("{value}/") })
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
    let mut env = BTreeMap::new();
    let mut taken = claimed.clone();
    for f in &t.manifest.fields {
        let given = input.get(&f.key).map(|v| v.trim()).filter(|v| !v.is_empty());
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
    Ok(Resolved { env, revealed })
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
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn all_free(_: u16) -> bool {
        true
    }

    #[test]
    fn defaults_and_auto_ports_for_vaultwarden() {
        let r = resolve(&tmpl("vaultwarden"), &input(&[]), &HashSet::new(), &all_free).unwrap();
        assert_eq!(r.env["INSTANCE_NAME"], "vaultwarden");
        assert_eq!(r.env["WEB_PORT"], "8100");
        assert_eq!(r.env["PUBLIC_URL"], "http://127.0.0.1:8100/");
        assert_eq!(r.env["SIGNUPS_ALLOWED"], "true");
    }

    #[test]
    fn auto_ports_skip_claimed_busy_and_each_other() {
        let claimed: HashSet<u16> = [8100].into();
        let free = |p: u16| p != 8101;
        let r = resolve(&tmpl("gitea"), &input(&[("ADMIN_EMAIL", "a@b.io")]), &claimed, &free).unwrap();
        assert_eq!(r.env["WEB_PORT"], "8102");
        assert_eq!(r.env["SSH_PORT"], "8103");
    }

    #[test]
    fn explicit_port_must_be_free_and_unclaimed() {
        let t = tmpl("vaultwarden");
        let claimed: HashSet<u16> = [9500].into();
        let e = resolve(&t, &input(&[("WEB_PORT", "9500")]), &claimed, &all_free).unwrap_err();
        assert!(e.contains("in use"), "{e}");
        let e = resolve(&t, &input(&[("WEB_PORT", "80")]), &HashSet::new(), &all_free).unwrap_err();
        assert!(e.contains("1024"), "{e}");
        let e = resolve(&t, &input(&[("WEB_PORT", "9501")]), &HashSet::new(), &|_| false).unwrap_err();
        assert!(e.contains("in use"), "{e}");
    }

    #[test]
    fn required_and_validation_errors() {
        let t = tmpl("gitea");
        let none = HashSet::new();
        assert!(resolve(&t, &input(&[]), &none, &all_free).unwrap_err().contains("e-mail"));
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
            &input(&[("PUBLIC_URL", "https://vault.example.com"), ("SIGNUPS_ALLOWED", "off")]),
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
    fn env_file_is_quoted_private_and_not_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let env: BTreeMap<_, _> = [("B".to_string(), "x$y".to_string()), ("A".to_string(), "1".to_string())].into();
        assert_eq!(render_env(&env), "A='1'\nB='x$y'\n");
        let dir = std::env::temp_dir().join(format!("dockwren-env-{}", crate::auth::generate_password()));
        std::fs::create_dir_all(&dir).unwrap();
        write_env(&dir, &env).unwrap();
        let mode = std::fs::metadata(dir.join(".env")).unwrap().permissions().mode();
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
}
