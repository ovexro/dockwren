//! Scheduled stop and start of one app, every day at fixed times (UTC).
//!
//! The plan is kept per app in `<instance>/power` (`stop=HH:MM` / `start=HH:MM`), and what the
//! scheduler did last in `<instance>/power.state` (`last=<epoch>`, `stopped=1`). A stop is the
//! same as the Remove button: containers go, data stays. The scheduler acts on the most recent
//! time that has passed and that it has not acted on yet, so a panel restart or a busy app does
//! not lose a slot, and a manual start in between is not undone until the next stop time. A
//! scheduled start only happens for an app the schedule itself stopped.

use std::path::Path;

use crate::runner;

const PLAN: &str = "power";
const STATE: &str = "power.state";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Plan {
    /// Minutes after midnight UTC.
    pub stop: Option<u16>,
    pub start: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Stop,
    Start,
}

pub fn parse_time(s: &str) -> Result<Option<u16>, &'static str> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let bad = "Times are written HH:MM, for example 23:30.";
    let (h, m) = s.split_once(':').ok_or(bad)?;
    let (h, m): (u16, u16) = (h.parse().map_err(|_| bad)?, m.parse().map_err(|_| bad)?);
    if h > 23 || m > 59 || s.len() > 5 {
        return Err(bad);
    }
    Ok(Some(h * 60 + m))
}

pub fn fmt_time(m: u16) -> String {
    format!("{:02}:{:02}", m / 60, m % 60)
}

impl Plan {
    pub fn from_form(stop: &str, start: &str) -> Result<Plan, &'static str> {
        let p = Plan {
            stop: parse_time(stop)?,
            start: parse_time(start)?,
        };
        if p.stop.is_some() && p.stop == p.start {
            return Err("Stop and start cannot be the same time.");
        }
        Ok(p)
    }

    pub fn is_empty(&self) -> bool {
        self.stop.is_none() && self.start.is_none()
    }

    /// One line for the app list, e.g. `stops 23:00, starts 07:00 UTC`.
    pub fn describe(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(m) = self.stop {
            parts.push(format!("stops {}", fmt_time(m)));
        }
        if let Some(m) = self.start {
            parts.push(format!("starts {}", fmt_time(m)));
        }
        (!parts.is_empty()).then(|| format!("{} UTC", parts.join(", ")))
    }

    /// The most recent plan time at or before `now` (epoch seconds), with its event.
    pub fn latest(&self, now: u64) -> Option<(Event, u64)> {
        let slot = |m: u16| {
            let today = now - now % 86_400 + u64::from(m) * 60;
            if today > now { today - 86_400 } else { today }
        };
        let stop = self.stop.map(|m| (Event::Stop, slot(m)));
        let start = self.start.map(|m| (Event::Start, slot(m)));
        match (stop, start) {
            (Some(a), Some(b)) => Some(if a.1 >= b.1 { a } else { b }),
            (a, b) => a.or(b),
        }
    }

    /// What to do now: the latest event, if it is newer than what was last acted on.
    pub fn due(&self, now: u64, last: u64) -> Option<(Event, u64)> {
        self.latest(now).filter(|(_, at)| *at > last)
    }
}

pub fn read(dir: &Path) -> Plan {
    let text = std::fs::read_to_string(dir.join(PLAN)).unwrap_or_default();
    let mut p = Plan::default();
    for line in text.lines() {
        match line.trim().split_once('=') {
            Some(("stop", v)) => p.stop = parse_time(v).ok().flatten(),
            Some(("start", v)) => p.start = parse_time(v).ok().flatten(),
            _ => {}
        }
    }
    p
}

/// Saves the plan (an empty plan removes it and its state). `now` is recorded as the last time
/// acted on, so saving at 10:00 a plan that stops at 03:00 does not stop the app right away.
pub fn write(dir: &Path, p: &Plan, now: u64) -> std::io::Result<()> {
    if p.is_empty() {
        for f in [PLAN, STATE] {
            match std::fs::remove_file(dir.join(f)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        return Ok(());
    }
    let mut text = String::new();
    if let Some(m) = p.stop {
        text += &format!("stop={}\n", fmt_time(m));
    }
    if let Some(m) = p.start {
        text += &format!("start={}\n", fmt_time(m));
    }
    runner::replace_private(&dir.join(PLAN), &text)?;
    let stopped = state(dir).1;
    set_state(dir, now, stopped)
}

/// `(last acted on, stopped by the schedule)`.
pub fn state(dir: &Path) -> (u64, bool) {
    let text = std::fs::read_to_string(dir.join(STATE)).unwrap_or_default();
    let (mut last, mut stopped) = (0, false);
    for line in text.lines() {
        match line.trim().split_once('=') {
            Some(("last", v)) => last = v.parse().unwrap_or(0),
            Some(("stopped", v)) => stopped = v == "1",
            _ => {}
        }
    }
    (last, stopped)
}

pub fn set_state(dir: &Path, last: u64, stopped: bool) -> std::io::Result<()> {
    runner::replace_private(
        &dir.join(STATE),
        &format!("last={last}\nstopped={}\n", u8::from(stopped)),
    )
}

/// A manual start or stop: the schedule no longer owns the app's stopped state.
pub fn forget_stopped(dir: &Path) {
    let (last, stopped) = state(dir);
    if stopped {
        let _ = set_state(dir, last, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400;
    // 2026-11-19 00:00 UTC is a multiple of a day.
    const D0: u64 = 20_776 * DAY;

    fn at(h: u64, m: u64) -> u64 {
        D0 + h * 3600 + m * 60
    }

    #[test]
    fn times_are_parsed_strictly() {
        assert_eq!(parse_time("").unwrap(), None);
        assert_eq!(parse_time("07:05").unwrap(), Some(425));
        assert_eq!(parse_time("7:5").unwrap(), Some(425));
        for bad in ["24:00", "12:60", "12", "ab:cd", "1:2:3", "-1:00", "012:00"] {
            assert!(parse_time(bad).is_err(), "{bad}");
        }
        assert_eq!(fmt_time(425), "07:05");
        assert!(Plan::from_form("03:00", "03:00").is_err());
        assert!(Plan::from_form("", "").unwrap().is_empty());
    }

    #[test]
    fn latest_event_is_the_one_that_passed_last() {
        let p = Plan::from_form("23:00", "07:00").unwrap();
        assert_eq!(p.latest(at(12, 0)), Some((Event::Start, at(7, 0))));
        assert_eq!(p.latest(at(23, 30)), Some((Event::Stop, at(23, 0))));
        assert_eq!(p.latest(at(3, 0)), Some((Event::Stop, at(23, 0) - DAY)));
        let only_stop = Plan::from_form("03:00", "").unwrap();
        assert_eq!(only_stop.latest(at(12, 0)), Some((Event::Stop, at(3, 0))));
        assert_eq!(Plan::default().latest(at(1, 0)), None);
    }

    #[test]
    fn due_only_when_newer_than_last_acted() {
        let p = Plan::from_form("23:00", "07:00").unwrap();
        let now = at(23, 5);
        assert_eq!(p.due(now, at(7, 1)), Some((Event::Stop, at(23, 0))));
        assert_eq!(p.due(now, at(23, 0)), None);
        // A missed slot is caught up, but only the latest one.
        assert_eq!(
            p.due(at(12, 0), at(23, 0) - DAY),
            Some((Event::Start, at(7, 0)))
        );
    }

    #[test]
    fn plan_and_state_round_trip_and_saving_does_not_act_on_the_past() {
        let dir = std::env::temp_dir().join(format!("dockwren-power-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = Plan::from_form("23:00", "07:30").unwrap();
        let now = at(10, 0);
        write(&dir, &p, now).unwrap();
        assert_eq!(read(&dir), p);
        assert_eq!(state(&dir), (now, false));
        assert_eq!(read(&dir).due(now, state(&dir).0), None);
        set_state(&dir, at(23, 0), true).unwrap();
        forget_stopped(&dir);
        assert_eq!(state(&dir), (at(23, 0), false));
        assert_eq!(p.describe().unwrap(), "stops 23:00, starts 07:30 UTC");
        write(&dir, &Plan::default(), now).unwrap();
        assert!(read(&dir).is_empty());
        assert_eq!(state(&dir), (0, false));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
