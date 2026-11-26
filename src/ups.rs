//! UPS monitoring through NUT (Network UPS Tools). When `upsd` answers on `127.0.0.1:3493`
//! (override with `DOCKWREN_NUT=host:port`, `DOCKWREN_NUT=off` disables), the panel reads each
//! UPS every 10 seconds over NUT's plain-text protocol (read-only: `LIST UPS`, `LIST VAR`, no
//! login, nothing is changed) and raises notifications for a power cut, a low battery, a battery
//! that needs replacing and an overload. They clear by themselves when the condition ends, and
//! then reach ntfy, Telegram and webhooks like every other event. No `upsd`, nothing happens.
//! NUT itself (`upsmon`) stays in charge of shutting the server down.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use askama::Template;
use axum::response::Html;

use crate::notify::{self, NewEvent, Severity, Shared};

pub const KIND: &str = "host.ups";
const DEFAULT_ADDR: &str = "127.0.0.1:3493";
const POLL: Duration = Duration::from_secs(10);
/// While `upsd` does not answer (NUT not installed) there is no reason to knock every 10 s.
const POLL_IDLE: Duration = Duration::from_secs(60);
const TIMEOUT: Duration = Duration::from_secs(2);
/// A problem must show on this many polls in a row before it is raised: a one-second brownout
/// that flips the UPS to battery and back is not news.
const CONFIRM: u32 = 2;
const MAX_UPS: usize = 4;
const MAX_LINES: usize = 2000;

/// What the panel keeps of one UPS.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ups {
    pub name: String,
    pub model: String,
    /// NUT status flags: `OL`, `OB`, `LB`, `RB`, `CHRG`, `DISCHRG`, `OVER`, `FSD`...
    pub status: Vec<String>,
    pub charge: Option<f64>,
    /// Seconds of runtime NUT estimates at the current load.
    pub runtime: Option<u64>,
    pub load: Option<f64>,
}

impl Ups {
    fn has(&self, flag: &str) -> bool {
        self.status.iter().any(|s| s == flag)
    }

    /// One line for the Health page and the notification bodies.
    pub fn summary(&self) -> String {
        // The model NUT detected; the config identifier only when there is no model.
        let name = if self.model.is_empty() {
            self.name.clone()
        } else {
            self.model.clone()
        };
        let state = if self.has("OB") {
            "on battery"
        } else if self.has("OL") {
            "online"
        } else {
            "state unknown"
        };
        let mut s = format!("{name}: {state}");
        if let Some(c) = self.charge {
            s += &format!(", battery {c:.0}%");
        }
        if let Some(r) = self.runtime {
            s += &format!(", about {} left", crate::metrics::short_span(r));
        }
        if let Some(l) = self.load {
            s += &format!(", load {l:.0}%");
        }
        s
    }
}

/// `UPS <name> "<description>"` lines of a `LIST UPS` answer.
fn parse_list_ups(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.strip_prefix("UPS "))
        .filter_map(|l| l.split_whitespace().next())
        .filter(|n| {
            !n.is_empty()
                && n.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        })
        .map(str::to_string)
        .take(MAX_UPS)
        .collect()
}

/// `VAR <ups> <name> "<value>"` lines of a `LIST VAR` answer.
fn parse_vars(text: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for l in text.lines() {
        let mut p = l.splitn(4, ' ');
        if p.next() != Some("VAR") {
            continue;
        }
        let (Some(_), Some(k), Some(v)) = (p.next(), p.next(), p.next()) else {
            continue;
        };
        let v = v.trim();
        let v = v.strip_prefix('"').unwrap_or(v);
        let v = v.strip_suffix('"').unwrap_or(v);
        m.insert(k.to_string(), v.replace("\\\"", "\"").replace("\\\\", "\\"));
    }
    m
}

fn ups_from(name: &str, vars: &HashMap<String, String>) -> Ups {
    let get = |k: &str| vars.get(k).map(String::as_str);
    let num = |k: &str| get(k).and_then(|v| v.trim().parse::<f64>().ok());
    Ups {
        name: name.to_string(),
        model: get("device.model")
            .or_else(|| get("ups.model"))
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_control())
            .take(60)
            .collect(),
        status: get("ups.status")
            .unwrap_or("")
            .split_whitespace()
            .map(str::to_string)
            .collect(),
        charge: num("battery.charge"),
        runtime: num("battery.runtime").map(|r| r.max(0.0) as u64),
        load: num("ups.load"),
    }
}

/// Send one command and read the answer up to its `END LIST ...` line (or an `ERR`).
fn ask(
    w: &mut TcpStream,
    r: &mut BufReader<TcpStream>,
    cmd: &str,
    end: &str,
) -> Result<String, String> {
    w.write_all(format!("{cmd}\n").as_bytes())
        .map_err(|e| e.to_string())?;
    let mut out = String::new();
    let mut line = String::new();
    for _ in 0..MAX_LINES {
        line.clear();
        if r.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            return Err("connection closed".into());
        }
        if line.starts_with("ERR") {
            return Err(line.trim().to_string());
        }
        if line.starts_with(end) {
            return Ok(out);
        }
        out.push_str(&line);
    }
    Err("answer too long".into())
}

/// Read every UPS `upsd` at `addr` knows about. Blocking, with short timeouts.
pub fn query(addr: &str) -> Result<Vec<Ups>, String> {
    let sa = addr
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or("no address")?;
    let mut w = TcpStream::connect_timeout(&sa, TIMEOUT).map_err(|e| e.to_string())?;
    w.set_read_timeout(Some(TIMEOUT)).ok();
    w.set_write_timeout(Some(TIMEOUT)).ok();
    let mut r = BufReader::new(w.try_clone().map_err(|e| e.to_string())?);
    let names = parse_list_ups(&ask(&mut w, &mut r, "LIST UPS", "END LIST UPS")?);
    let mut out = Vec::new();
    for n in names {
        let vars = parse_vars(&ask(
            &mut w,
            &mut r,
            &format!("LIST VAR {n}"),
            "END LIST VAR",
        )?);
        out.push(ups_from(&n, &vars));
    }
    let _ = w.write_all(b"LOGOUT\n");
    Ok(out)
}

/// Where to look for `upsd`; `None` when switched off with `DOCKWREN_NUT=off`.
pub fn nut_addr() -> Option<String> {
    match std::env::var("DOCKWREN_NUT") {
        Ok(v) if matches!(v.trim(), "off" | "0" | "false") => None,
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => Some(DEFAULT_ADDR.to_string()),
    }
}

/// One problem of one UPS: `(severity, key suffix, title, body)`.
fn problems(u: &Ups) -> Vec<(Severity, &'static str, String, String)> {
    let mut v = Vec::new();
    let detail = u.summary();
    if u.has("FSD") || (u.has("OB") && u.has("LB")) {
        v.push((
            Severity::Critical,
            "power",
            format!(
                "Power cut: UPS {} battery is low, the server is shutting down",
                u.name
            ),
            format!("{detail}. NUT stops the server cleanly before the battery runs out."),
        ));
    } else if u.has("OB") {
        v.push((
            Severity::Warning,
            "power",
            format!("Power cut: UPS {} on battery", u.name),
            format!("{detail}. NUT shuts the server down by itself if the battery runs low."),
        ));
    }
    if u.has("RB") {
        v.push((
            Severity::Warning,
            "replace",
            format!("UPS {} battery needs replacing", u.name),
            format!("{detail}. The UPS reports that the battery is worn out and may not carry the load in a power cut."),
        ));
    }
    if u.has("OVER") {
        v.push((
            Severity::Warning,
            "overload",
            format!("UPS {} is overloaded", u.name),
            format!("{detail}. Unplug something from it, or it may cut the power."),
        ));
    }
    v
}

fn key_for(name: &str, what: &str) -> String {
    format!("{KIND}:{name}:{what}")
}

/// Raise what is wrong (after `CONFIRM` polls in a row) and close what is not any more.
/// `seen` counts consecutive polls per problem key and lives as long as the checker.
pub fn apply(shared: &Shared, list: &[Ups], seen: &mut HashMap<String, u32>) {
    let mut present = HashSet::new();
    for u in list {
        // Neither on mains nor on battery says nothing: keep what is open as it is, or an
        // unknown state would be announced as "power restored".
        if !u.has("OL") && !u.has("OB") {
            present.extend(["power", "replace", "overload"].map(|what| key_for(&u.name, what)));
        }
        for (sev, what, title, body) in problems(u) {
            let key = key_for(&u.name, what);
            present.insert(key.clone());
            let n = seen.entry(key.clone()).or_insert(0);
            *n += 1;
            if *n >= CONFIRM {
                notify::raise(
                    shared,
                    NewEvent::new("host", KIND, sev, &title)
                        .body(body)
                        .link("/settings/doctor")
                        .key(key),
                );
            }
        }
    }
    seen.retain(|k, _| present.contains(k));
    let stale: Vec<String> = shared
        .lock()
        .unwrap()
        .open_keys_with_prefix(&format!("{KIND}:"))
        .into_iter()
        .filter(|k| !present.contains(k))
        .collect();
    for k in stale {
        notify::resolve(shared, &k);
    }
}

/// USB vendor ids that only make UPS devices (as `idVendor` appears in sysfs).
const UPS_VENDORS: [(&str, &str); 6] = [
    ("0463", "Eaton"),
    ("051d", "APC"),
    ("0764", "CyberPower"),
    ("09ae", "Tripp Lite"),
    ("0d9f", "PowerCOM"),
    ("06da", "Phoenixtec"),
];

/// UPS devices plugged into this server, from `/sys/bus/usb/devices` (readable without root).
pub fn usb_ups(sys: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(sys) else {
        return out;
    };
    for e in rd.flatten() {
        let read = |f: &str| {
            std::fs::read_to_string(e.path().join(f))
                .map(|s| s.trim().to_lowercase())
                .unwrap_or_default()
        };
        let vendor = read("idVendor");
        let Some((_, brand)) = UPS_VENDORS.iter().find(|v| v.0 == vendor) else {
            continue;
        };
        let product: String = std::fs::read_to_string(e.path().join("product"))
            .unwrap_or_default()
            .trim()
            .chars()
            .filter(|c| !c.is_control())
            .take(60)
            .collect();
        let id = format!("{vendor}:{}", read("idProduct"));
        out.push(if product.is_empty() {
            format!("{brand} UPS ({id})")
        } else if product.to_lowercase().starts_with(&brand.to_lowercase()) {
            format!("{product} ({id})")
        } else {
            format!("{brand} {product} ({id})")
        });
    }
    out.sort();
    out
}

#[derive(Template)]
#[template(path = "ups.html")]
struct View {
    /// `off`, `connected`, `empty`, `setup` or `none`.
    mode: &'static str,
    badge_state: &'static str,
    badge_text: String,
    lines: Vec<String>,
    detected: Vec<String>,
}

/// Badge for a set of UPSes: the worst one decides.
fn badge(list: &[Ups]) -> (&'static str, &'static str) {
    if list
        .iter()
        .any(|u| u.has("FSD") || (u.has("OB") && u.has("LB")))
    {
        ("exited", "Battery low")
    } else if list.iter().any(|u| u.has("OB")) {
        ("paused", "On battery")
    } else if list.iter().any(|u| u.has("RB")) {
        ("paused", "Replace battery")
    } else {
        ("running", "Connected")
    }
}

fn view(addr: Option<&str>, found: Result<Vec<Ups>, String>, usb: Vec<String>) -> View {
    let mk = |mode, badge_state, badge_text: &str, lines, detected| View {
        mode,
        badge_state,
        badge_text: badge_text.to_string(),
        lines,
        detected,
    };
    if addr.is_none() {
        return mk("off", "", "Off", vec![], vec![]);
    }
    match found {
        Ok(list) if !list.is_empty() => {
            let (state, text) = badge(&list);
            mk(
                "connected",
                state,
                text,
                list.iter().map(Ups::summary).collect(),
                vec![],
            )
        }
        Ok(_) => mk("empty", "paused", "No UPS in NUT", vec![], usb),
        Err(_) if !usb.is_empty() => mk("setup", "paused", "Detected, not set up", vec![], usb),
        Err(_) => mk("none", "", "Not detected", vec![], vec![]),
    }
}

/// `GET /settings/ups`: the UPS section of Settings → System.
pub async fn show() -> Html<String> {
    let addr = nut_addr();
    let found = match addr.clone() {
        Some(a) => tokio::task::spawn_blocking(move || query(&a))
            .await
            .unwrap_or_else(|e| Err(e.to_string())),
        None => Err("off".into()),
    };
    let usb = usb_ups(std::path::Path::new("/sys/bus/usb/devices"));
    Html(
        view(addr.as_deref(), found, usb)
            .render()
            .expect("ups renders"),
    )
}

pub fn spawn_checker(shared: Shared) {
    let Some(addr) = nut_addr() else { return };
    tokio::spawn(async move {
        let mut seen: HashMap<String, u32> = HashMap::new();
        loop {
            let mut wait = POLL_IDLE;
            let on = shared.lock().unwrap().is_enabled(KIND);
            if on {
                let a = addr.clone();
                if let Ok(Ok(list)) = tokio::task::spawn_blocking(move || query(&a)).await {
                    apply(&shared, &list, &mut seen);
                    wait = POLL;
                }
            }
            tokio::time::sleep(wait).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    const LIST: &str = "UPS eaton \"Eaton 5E Gen2 700 USB\"\nUPS bad;name \"x\"\n";
    const VARS: &str = "VAR eaton battery.charge \"100\"\nVAR eaton battery.runtime \"10822\"\nVAR eaton device.model \"Eaton 5E 700 G2\"\nVAR eaton ups.load \"12\"\nVAR eaton ups.status \"OL CHRG\"\n";

    fn ups(status: &str) -> Ups {
        Ups {
            name: "eaton".into(),
            model: "Eaton 5E 700 G2".into(),
            status: status.split_whitespace().map(str::to_string).collect(),
            charge: Some(80.0),
            runtime: Some(900),
            load: Some(20.0),
        }
    }

    #[test]
    fn parses_nut_answers_and_ignores_odd_names() {
        assert_eq!(parse_list_ups(LIST), vec!["eaton".to_string()]);
        let u = ups_from("eaton", &parse_vars(VARS));
        assert_eq!(u.model, "Eaton 5E 700 G2");
        assert_eq!(u.status, vec!["OL", "CHRG"]);
        assert_eq!(
            (u.charge, u.runtime, u.load),
            (Some(100.0), Some(10822), Some(12.0))
        );
        assert!(u.summary().contains("online"));
        assert!(u.summary().contains("battery 100%"));
    }

    #[test]
    fn problem_matrix() {
        assert!(problems(&ups("OL CHRG")).is_empty());
        let p = problems(&ups("OB DISCHRG"));
        assert_eq!((p.len(), p[0].0, p[0].1), (1, Severity::Warning, "power"));
        let p = problems(&ups("OB DISCHRG LB"));
        assert_eq!((p[0].0, p[0].1), (Severity::Critical, "power"));
        let p = problems(&ups("FSD OB LB"));
        assert_eq!(p[0].0, Severity::Critical);
        assert_eq!(problems(&ups("OL RB"))[0].1, "replace");
        assert_eq!(problems(&ups("OL OVER"))[0].1, "overload");
    }

    #[test]
    fn needs_two_polls_escalates_and_clears() {
        let s: Shared = Arc::new(Mutex::new(notify::Store::memory()));
        let mut seen = HashMap::new();
        let open = |s: &Shared| s.lock().unwrap().open_events().len();
        apply(&s, &[ups("OB DISCHRG")], &mut seen);
        assert_eq!(open(&s), 0, "one poll is a blip");
        apply(&s, &[ups("OL")], &mut seen);
        apply(&s, &[ups("OB DISCHRG")], &mut seen);
        assert_eq!(open(&s), 0, "the blip must not count towards the next one");
        apply(&s, &[ups("OB DISCHRG")], &mut seen);
        assert_eq!(open(&s), 1);
        apply(&s, &[ups("OB DISCHRG")], &mut seen);
        assert_eq!(open(&s), 1, "same condition is one event");
        apply(&s, &[ups("OB DISCHRG LB")], &mut seen);
        let sev = s.lock().unwrap().open_events()[0].severity;
        assert_eq!(sev, Severity::Critical);
        assert_eq!(open(&s), 1, "escalation updates in place");
        for unknown in ["", "WAIT"] {
            apply(&s, &[ups(unknown)], &mut seen);
            assert_eq!(open(&s), 1, "'{unknown}' is not 'power back'");
        }
        apply(&s, &[ups("OL CHRG")], &mut seen);
        assert_eq!(open(&s), 0, "power back closes it");
    }

    /// A tiny fake `upsd` on a local port.
    #[test]
    fn query_talks_to_a_fake_upsd() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let t = std::thread::spawn(move || {
            let (c, _) = l.accept().unwrap();
            let mut w = c.try_clone().unwrap();
            let mut r = BufReader::new(c);
            let mut line = String::new();
            loop {
                line.clear();
                if r.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                match line.trim() {
                    "LIST UPS" => write!(w, "BEGIN LIST UPS\n{LIST}END LIST UPS\n").unwrap(),
                    "LIST VAR eaton" => {
                        write!(w, "BEGIN LIST VAR eaton\n{VARS}END LIST VAR eaton\n").unwrap()
                    }
                    "LOGOUT" => break,
                    _ => writeln!(w, "ERR UNKNOWN-COMMAND").unwrap(),
                }
            }
        });
        let got = query(&addr).unwrap();
        t.join().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "eaton");
        assert_eq!(got[0].status, vec!["OL", "CHRG"]);
    }

    #[test]
    fn finds_ups_on_usb_from_sysfs() {
        let d = std::env::temp_dir().join(format!("dw-sysusb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for (n, v, p, name) in [
            ("1-6", "0463", "ffff", "Eaton 5E 700 G2"),
            ("1-2", "c0f4", "05c0", "keyboard"),
            ("1-3", "051D", "0002", ""),
        ] {
            let dir = d.join(n);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("idVendor"), format!("{v}\n")).unwrap();
            std::fs::write(dir.join("idProduct"), format!("{p}\n")).unwrap();
            if !name.is_empty() {
                std::fs::write(dir.join("product"), format!("{name}\n")).unwrap();
            }
        }
        assert_eq!(
            usb_ups(&d),
            vec![
                "APC UPS (051d:0002)".to_string(),
                "Eaton 5E 700 G2 (0463:ffff)".to_string()
            ]
        );
        assert!(usb_ups(&d.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn section_modes_and_badges() {
        let usb = vec!["Eaton UPS (0463:ffff)".to_string()];
        let v = view(None, Err("off".into()), vec![]);
        assert_eq!(v.mode, "off");
        let v = view(Some("x"), Err("refused".into()), vec![]);
        assert_eq!((v.mode, v.badge_text.as_str()), ("none", "Not detected"));
        let v = view(Some("x"), Err("refused".into()), usb.clone());
        assert_eq!((v.mode, v.badge_state), ("setup", "paused"));
        let v = view(Some("x"), Ok(vec![]), usb);
        assert_eq!(v.mode, "empty");
        let v = view(Some("x"), Ok(vec![ups("OL CHRG")]), vec![]);
        assert_eq!(
            (v.mode, v.badge_state, v.badge_text.as_str()),
            ("connected", "running", "Connected")
        );
        assert_eq!(badge(&[ups("OB DISCHRG")]), ("paused", "On battery"));
        assert_eq!(badge(&[ups("OB DISCHRG LB")]), ("exited", "Battery low"));
        assert_eq!(badge(&[ups("OL RB")]), ("paused", "Replace battery"));
        let html = view(Some("x"), Ok(vec![ups("OL")]), vec![])
            .render()
            .unwrap();
        assert!(html.contains("Connected") && html.contains("Eaton 5E 700 G2"));
        let html = view(
            Some("x"),
            Err("x".into()),
            vec!["Eaton UPS (0463:ffff)".into()],
        )
        .render()
        .unwrap();
        assert!(html.contains("sudo dockwren setup-ups") && html.contains("Eaton UPS (0463:ffff)"));
    }

    #[test]
    fn unreachable_upsd_is_an_error_not_a_panic() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        drop(l);
        assert!(query(&addr).is_err());
    }
}
