//! What a pushed message says. One place for the words, so an alert, its "cleared" follow-up
//! and the one-message summary of a problem that was already over read clearly and differently.

use crate::notify::{Event, Severity};

/// Which of the three messages an event owes a channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum What {
    /// The problem is there now (new or escalated).
    Alert,
    /// The channel got the alert; the problem is gone now.
    Cleared,
    /// The alert could not be sent while the problem lasted and it is over: one summary.
    Missed,
}

/// A message ready for any channel.
#[derive(Clone, Debug)]
pub struct Message {
    pub id: u64,
    pub ts: u64,
    pub source: String,
    pub kind: String,
    pub severity: Severity,
    pub title: String,
    pub body: Option<String>,
    pub link: Option<String>,
    /// ntfy tag (shown as an emoji); Telegram shows the matching character.
    pub tag: &'static str,
    /// `alert`, `cleared` or `missed`.
    pub state: &'static str,
    pub since: u64,
    pub cleared_at: Option<u64>,
}

/// A wall-clock moment in the server's time zone: ("26 Nov", "13:41", "EET").
pub type Stamp = (String, String, String);

/// The server's local time, asked from `date` (the panel has no time-zone code of its own).
pub fn local_stamp(ts: u64) -> Option<Stamp> {
    let out = std::process::Command::new("date")
        .env("LC_ALL", "C")
        .args(["-d", &format!("@{ts}"), "+%-d %b|%H:%M|%Z"])
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let mut p = text.trim().split('|');
    Some((p.next()?.into(), p.next()?.into(), p.next()?.into()))
}

/// "under a minute", "15 min", "2h 5min", "3d 4h".
pub fn duration(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    match (d, h, m) {
        (0, 0, 0) => "under a minute".into(),
        (0, 0, _) => format!("{m} min"),
        (0, _, 0) => format!("{h}h"),
        (0, _, _) => format!("{h}h {m}min"),
        (_, 0, _) => format!("{d}d"),
        _ => format!("{d}d {h}h"),
    }
}

/// "26 Nov 13:41–13:56 EET", or both dates when it ran past midnight.
fn span(from: &Stamp, to: &Stamp) -> String {
    if from.0 == to.0 && from.2 == to.2 {
        format!("{} {}–{} {}", from.0, from.1, to.1, to.2)
    } else {
        format!(
            "{} {} {} – {} {} {}",
            from.0, from.1, from.2, to.0, to.1, to.2
        )
    }
}

/// The key without its `kind:` prefix: the app, disk, container or UPS it is about.
fn subject(e: &Event) -> &str {
    e.key
        .strip_prefix(e.kind.as_str())
        .and_then(|r| r.strip_prefix(':'))
        .unwrap_or("")
}

/// For `host.ups` keys (`host.ups:<name>:<what>`): the UPS name and the problem.
fn ups_parts(e: &Event) -> Option<(&str, &str)> {
    (e.kind == crate::ups::KIND)
        .then(|| subject(e).rsplit_once(':'))
        .flatten()
}

fn is_power(e: &Event) -> bool {
    matches!(ups_parts(e), Some((_, "power")))
}

/// What is true again once the event cleared, said as good news (never the problem's title).
pub fn cleared_title(e: &Event) -> String {
    let s = subject(e);
    match e.kind.as_str() {
        "host.ups" => match ups_parts(e) {
            Some((n, "power")) => format!("Power restored: UPS {n} is back on mains"),
            Some((n, "replace")) => format!("UPS {n} no longer asks for a new battery"),
            Some((n, "overload")) => format!("UPS {n} is no longer overloaded"),
            _ => "UPS is fine again".into(),
        },
        "app.unreachable" => format!("{s} is reachable again"),
        "app.update_available" => format!("{s}: no update waiting any more"),
        "app.upstream_newer" => format!(
            "{}: no newer upstream version to report",
            s.split(':').next().unwrap_or(s)
        ),
        "app.vulnerable" => format!("{s}: no critical vulnerability with a fix left"),
        "job.failed" => match s.split_once(':') {
            Some((verb, name)) => format!("Fixed: {verb} {name} has now succeeded"),
            None => "Fixed: the OS update has now succeeded".into(),
        },
        "dockwren.update_available" => "Dockwren is up to date".into(),
        "login.lockout" => "Sign-in works again after the lockout".into(),
        "host.os_updates" => "No OS updates or reboot waiting any more".into(),
        "backup.failed" => "Scheduled backup is OK again".into(),
        "host.smart" => format!("Disk {s}: no health problem reported any more"),
        "host.disk_full" => format!("Disk {s} has enough free space again"),
        "host.disk_forecast" => format!("Disk {s} is no longer about to fill up"),
        "host.inodes_high" => format!("Disk {s} has enough free inodes again"),
        "host.load_high" => "Load is back to normal".into(),
        "host.memory_high" => "Memory use is back to normal".into(),
        "host.cpu_high" => "CPU use is back to normal".into(),
        "container.problem" => format!("Container {s}: the problem is gone"),
        "container.port_open" => match s.rsplit_once(':') {
            Some((name, port)) => {
                format!("Container {name}: port {port} is no longer open on every interface")
            }
            None => "The container port is no longer open on every interface".into(),
        },
        "host.listener" => format!("Port {s}: the unexpected listener is gone"),
        _ => format!("Cleared: {}", e.title),
    }
}

/// Build the message of one kind for one event. `now` decides whether an alert is late;
/// `late` says an earlier attempt to send it failed.
pub fn compose(e: &Event, what: What, late: bool, clock: &dyn Fn(u64) -> Option<Stamp>) -> Message {
    let power = is_power(e);
    let mut m = Message {
        id: e.id,
        ts: e.ts,
        source: e.source.clone(),
        kind: e.kind.clone(),
        severity: e.severity,
        title: e.title.clone(),
        body: e.body.clone(),
        link: e.link.clone(),
        tag: match e.severity {
            Severity::Info => "information_source",
            Severity::Warning if power => "electric_plug",
            Severity::Warning => "warning",
            Severity::Critical => "rotating_light",
        },
        state: "alert",
        since: e.since,
        cleared_at: e.cleared_at,
    };
    if what == What::Alert {
        if (power || late)
            && let Some((day, time, zone)) = clock(e.since)
        {
            let mut head = format!("Since {day} {time} {zone}.");
            if late {
                head.push_str(" It could not be sent earlier.");
            }
            m.body = Some(match &e.body {
                Some(b) => format!("{head}\n{b}"),
                None => head,
            });
        }
        return m;
    }
    let end = e.cleared_at.unwrap_or(e.ts);
    let lasted = duration(end.saturating_sub(e.since));
    let when = match (clock(e.since), clock(end)) {
        (Some(a), Some(b)) => Some(span(&a, &b)),
        _ => None,
    };
    let times = match &when {
        Some(w) => format!("{w} ({lasted})"),
        None => lasted.clone(),
    };
    m.severity = Severity::Info;
    m.link = None;
    m.tag = "white_check_mark";
    m.title = cleared_title(e);
    m.state = "cleared";
    let mut body = if power {
        format!("Power was out: {times}.")
    } else {
        format!("It lasted: {times}.\nThe problem was: {}", e.title)
    };
    if what == What::Missed {
        m.state = "missed";
        if power && let Some((n, _)) = ups_parts(e) {
            m.title = format!("Power restored after a cut of {lasted} (UPS {n})");
        }
        body.push_str("\nAlready over: the alert could not be sent while it lasted.");
    }
    m.body = Some(body);
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: &str, key: &str, title: &str) -> Event {
        Event {
            id: 1,
            ts: 1_000,
            source: "host".into(),
            kind: kind.into(),
            severity: Severity::Warning,
            title: title.into(),
            body: Some("Battery 100%.".into()),
            link: Some("/settings".into()),
            key: key.into(),
            since: 1_000,
            cleared_at: Some(1_900),
            ..Event::default()
        }
    }

    fn clock(ts: u64) -> Option<Stamp> {
        let t = if ts < 1_500 { "13:41" } else { "13:56" };
        Some(("26 Nov".into(), t.into(), "EET".into()))
    }

    #[test]
    fn power_cut_and_power_back_read_differently() {
        let e = ev(
            "host.ups",
            "host.ups:eaton:power",
            "Power cut: UPS eaton on battery",
        );
        let a = compose(&e, What::Alert, false, &clock);
        assert_eq!(a.tag, "electric_plug");
        assert_eq!(a.title, e.title);
        assert_eq!(
            a.body.as_deref(),
            Some("Since 26 Nov 13:41 EET.\nBattery 100%.")
        );
        let c = compose(&e, What::Cleared, false, &clock);
        assert_eq!(
            (c.tag, c.severity, c.state),
            ("white_check_mark", Severity::Info, "cleared")
        );
        assert_eq!(c.title, "Power restored: UPS eaton is back on mains");
        assert_eq!(
            c.body.as_deref(),
            Some("Power was out: 26 Nov 13:41–13:56 EET (15 min).")
        );
        assert!(!c.title.contains("on battery") && c.link.is_none());
        let m = compose(&e, What::Missed, true, &clock);
        assert_eq!(m.title, "Power restored after a cut of 15 min (UPS eaton)");
        assert_eq!(m.state, "missed");
        assert!(
            m.body
                .unwrap()
                .contains("could not be sent while it lasted")
        );
    }

    #[test]
    fn a_late_alert_says_since_when() {
        let e = ev("backup.failed", "backup.failed", "Scheduled backup failed");
        let a = compose(&e, What::Alert, false, &clock);
        assert_eq!(
            (a.tag, a.body.as_deref()),
            ("warning", Some("Battery 100%."))
        );
        let late = compose(&e, What::Alert, true, &clock);
        assert!(
            late.body
                .unwrap()
                .starts_with("Since 26 Nov 13:41 EET. It could not be sent earlier.\n")
        );
    }

    #[test]
    fn every_kind_that_can_clear_has_its_own_words() {
        // Kinds that only announce something that happened: nothing ever clears them.
        const ONE_SHOT: [&str; 4] = [
            "terminal.opened",
            "app.auto_updated",
            "login.new_source",
            "host.os_update_run",
        ];
        for (kind, _, _) in crate::notify::KINDS {
            if ONE_SHOT.contains(&kind) {
                continue;
            }
            let e = ev(kind, &format!("{kind}:x:power"), "PROBLEM");
            let t = cleared_title(&e);
            assert!(!t.contains("PROBLEM"), "{kind} has no wording: {t}");
        }
    }

    #[test]
    fn subjects_come_from_the_key() {
        let t = |kind: &str, key: &str| cleared_title(&ev(kind, key, "x"));
        assert_eq!(
            t("app.unreachable", "app.unreachable:memos"),
            "memos is reachable again"
        );
        assert_eq!(
            t("job.failed", "job.failed:update:gitea"),
            "Fixed: update gitea has now succeeded"
        );
        assert_eq!(
            t("job.failed", "job.failed:os-updates"),
            "Fixed: the OS update has now succeeded"
        );
        assert_eq!(
            t("container.port_open", "container.port_open:db:5432/tcp"),
            "Container db: port 5432/tcp is no longer open on every interface"
        );
    }

    #[test]
    fn durations_and_spans() {
        assert_eq!(duration(59), "under a minute");
        assert_eq!(duration(185), "3 min");
        assert_eq!(duration(2 * 3600 + 300), "2h 5min");
        assert_eq!(duration(3600), "1h");
        assert_eq!(duration(3 * 86400 + 4 * 3600), "3d 4h");
        let a = ("26 Nov".to_string(), "23:50".to_string(), "EET".to_string());
        let b = ("27 Nov".to_string(), "00:10".to_string(), "EET".to_string());
        assert_eq!(span(&a, &b), "26 Nov 23:50 EET – 27 Nov 00:10 EET");
    }

    #[test]
    fn the_server_clock_answers() {
        let (day, time, zone) = local_stamp(1_700_000_000).expect("date runs");
        assert!(day.ends_with("Nov"), "{day}");
        assert!(time.len() == 5 && time.contains(':'), "{time}");
        assert!(!zone.is_empty());
    }
}
