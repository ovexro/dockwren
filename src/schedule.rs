//! Scheduled backups: how often, at which hour (UTC) and how many snapshots to keep.
//!
//! Settings live in `<data>/backup-schedule.toml`, the time of the last scheduled run in
//! `<data>/backup-schedule.last` (epoch seconds). The scheduler itself is in `apps.rs`, because
//! it runs backups as normal jobs; this file only holds the pure parts and the storage.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

pub const DEFAULT_KEEP: usize = 7;
pub const MAX_KEEP: usize = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Off,
    Daily,
    /// Sundays.
    Weekly,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Daily => "daily",
            Mode::Weekly => "weekly",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Mode::Off),
            "daily" => Some(Mode::Daily),
            "weekly" => Some(Mode::Weekly),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub mode: Mode,
    /// Hour of the day, UTC, 0..=23.
    pub hour: u8,
    /// Snapshots kept per app, 1..=MAX_KEEP.
    pub keep: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: Mode::Off,
            hour: 3,
            keep: DEFAULT_KEEP,
        }
    }
}

impl Settings {
    /// Build settings from submitted form text; `Err` is a message for the user.
    pub fn from_form(mode: &str, hour: &str, keep: &str) -> Result<Self, &'static str> {
        let mode = Mode::parse(mode).ok_or("Pick off, daily or weekly.")?;
        let hour: u8 = hour
            .trim()
            .parse()
            .ok()
            .filter(|h| *h < 24)
            .ok_or("The hour must be 0 to 23.")?;
        let keep: usize = keep
            .trim()
            .parse()
            .ok()
            .filter(|k| (1..=MAX_KEEP).contains(k))
            .ok_or("Keep between 1 and 60 snapshots.")?;
        Ok(Self { mode, hour, keep })
    }
}

/// Epoch seconds of the most recent scheduled slot at or before `now` (None when off).
/// Daily: every day at `hour` UTC. Weekly: Sundays at `hour` UTC.
pub fn last_slot(now: u64, mode: Mode, hour: u8) -> Option<u64> {
    const DAY: u64 = 86_400;
    let offset = u64::from(hour) * 3600;
    // Shift so that a "day" starts at the slot hour; day 0 (1970-01-01) was a Thursday.
    let shifted = now.checked_sub(offset)?;
    let days = shifted / DAY;
    match mode {
        Mode::Off => None,
        Mode::Daily => Some(days * DAY + offset),
        Mode::Weekly => {
            let weekday = (days + 3) % 7; // 0 = Monday .. 6 = Sunday
            let back = (weekday + 1) % 7; // days since the last Sunday
            Some((days - back) * DAY + offset)
        }
    }
}

/// Is a scheduled run due? True when the last slot is newer than the last run.
pub fn due(now: u64, s: &Settings, last_run: u64) -> bool {
    last_slot(now, s.mode, s.hour).is_some_and(|slot| last_run < slot)
}

pub struct Schedule {
    dir: Option<PathBuf>,
    settings: Mutex<Settings>,
}

impl Schedule {
    /// In-memory only (tests, or no data directory).
    pub fn memory() -> Self {
        Self {
            dir: None,
            settings: Mutex::new(Settings::default()),
        }
    }

    pub fn open(data_dir: &Path) -> Self {
        let settings = std::fs::read_to_string(data_dir.join("backup-schedule.toml"))
            .ok()
            .and_then(|t| toml::from_str::<Settings>(&t).ok())
            .filter(|s| s.hour < 24 && (1..=MAX_KEEP).contains(&s.keep))
            .unwrap_or_default();
        Self {
            dir: Some(data_dir.to_path_buf()),
            settings: Mutex::new(settings),
        }
    }

    pub fn get(&self) -> Settings {
        *self.settings.lock().unwrap()
    }

    /// Save new settings. Turning the schedule on (or changing it) counts the current moment
    /// as the last run, so enabling it at 15:00 does not fire the 03:00 slot straight away.
    pub fn set(&self, s: Settings) -> Result<(), String> {
        if let Some(dir) = &self.dir {
            let text = toml::to_string(&s).map_err(|e| e.to_string())?;
            write_private(&dir.join("backup-schedule.toml"), text.as_bytes())
                .map_err(|e| format!("cannot save: {e}"))?;
        }
        let old = std::mem::replace(&mut *self.settings.lock().unwrap(), s);
        if old != s {
            self.mark_run(now());
        }
        Ok(())
    }

    pub fn last_run(&self) -> u64 {
        self.dir
            .as_ref()
            .and_then(|d| std::fs::read_to_string(d.join("backup-schedule.last")).ok())
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(0)
    }

    pub fn mark_run(&self, epoch: u64) {
        if let Some(d) = &self.dir {
            let _ = write_private(
                &d.join("backup-schedule.last"),
                epoch.to_string().as_bytes(),
            );
        }
    }

    pub fn is_due(&self) -> bool {
        due(now(), &self.get(), self.last_run())
    }
}

/// Write a 0600 file by renaming a temp file over the target.
fn write_private(path: &Path, content: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(content)?;
    std::fs::rename(&tmp, path)
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-11-14 19:00:00 UTC is a Saturday.
    const SAT_19: u64 = 1_794_682_800;

    #[test]
    fn daily_slot_is_today_after_the_hour_and_yesterday_before() {
        let today_03 = SAT_19 - 16 * 3600;
        assert_eq!(last_slot(SAT_19, Mode::Daily, 3), Some(today_03));
        assert_eq!(
            last_slot(today_03 - 1, Mode::Daily, 3),
            Some(today_03 - 86_400)
        );
        assert_eq!(last_slot(today_03, Mode::Daily, 3), Some(today_03));
    }

    #[test]
    fn weekly_slot_is_the_last_sunday() {
        // Saturday 2026-11-14 -> Sunday 2026-11-08 03:00.
        let sun = SAT_19 - 16 * 3600 - 6 * 86_400;
        assert_eq!(last_slot(SAT_19, Mode::Weekly, 3), Some(sun));
        // On Sunday itself, before the hour, the previous Sunday still counts.
        assert_eq!(last_slot(sun - 1, Mode::Weekly, 3), Some(sun - 7 * 86_400));
        assert_eq!(last_slot(sun, Mode::Weekly, 3), Some(sun));
    }

    #[test]
    fn off_is_never_due() {
        let s = Settings::default();
        assert!(!due(SAT_19, &s, 0));
    }

    #[test]
    fn due_only_when_the_last_run_is_older_than_the_slot() {
        let s = Settings {
            mode: Mode::Daily,
            hour: 3,
            keep: 7,
        };
        let slot = SAT_19 - 16 * 3600;
        assert!(due(SAT_19, &s, 0));
        assert!(due(SAT_19, &s, slot - 1));
        assert!(!due(SAT_19, &s, slot));
        assert!(!due(SAT_19, &s, SAT_19));
    }

    #[test]
    fn form_is_validated() {
        assert!(Settings::from_form("daily", "3", "7").is_ok());
        assert!(Settings::from_form("hourly", "3", "7").is_err());
        assert!(Settings::from_form("daily", "24", "7").is_err());
        assert!(Settings::from_form("daily", "3", "0").is_err());
        assert!(Settings::from_form("daily", "3", "61").is_err());
        assert!(Settings::from_form("daily", "x", "7").is_err());
    }

    #[test]
    fn settings_persist_and_a_change_resets_the_last_run() {
        let d = std::env::temp_dir().join(format!("dw-sched-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let sc = Schedule::open(&d);
        assert_eq!(sc.get(), Settings::default());
        let s = Settings {
            mode: Mode::Weekly,
            hour: 5,
            keep: 3,
        };
        sc.set(s).unwrap();
        assert_eq!(Schedule::open(&d).get(), s);
        assert!(sc.last_run() > 0, "enabling counts as a run");
        assert!(!sc.is_due());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
