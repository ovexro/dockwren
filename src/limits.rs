//! Per-app resource limits (memory, CPUs) for the main service.
//!
//! The values live in `limits.toml` in the instance directory, which updates never touch.
//! `compose.limits.yaml` is generated from it; `runner::compose_with_stdin` adds that file
//! to every `docker compose` call. Empty values mean "no limit" and remove both files.

use serde::{Deserialize, Serialize};
use std::path::Path;

pub const TOML: &str = "limits.toml";
pub const YAML: &str = "compose.limits.yaml";

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Docker size such as `512m` or `2g`.
    #[serde(default)]
    pub memory: String,
    /// Decimal number of CPUs such as `1.5`.
    #[serde(default)]
    pub cpus: String,
}

impl Limits {
    pub fn is_empty(&self) -> bool {
        self.memory.is_empty() && self.cpus.is_empty()
    }

    /// Check and normalise the user input (trimmed, memory lower-cased).
    pub fn parse(memory: &str, cpus: &str) -> Result<Limits, String> {
        let memory = memory.trim().to_ascii_lowercase();
        let cpus = cpus.trim().to_string();
        if !memory.is_empty() {
            let (num, unit) = memory.split_at(memory.len() - 1);
            let n: u64 = num
                .parse()
                .map_err(|_| "Memory must look like 512m or 2g.".to_string())?;
            let mb = match unit {
                "m" => n,
                "g" => n.saturating_mul(1024),
                _ => return Err("Memory must look like 512m or 2g.".into()),
            };
            if !(64..=1_048_576).contains(&mb) {
                return Err("Memory must be at least 64m (and at most 1024g).".into());
            }
        }
        if !cpus.is_empty() {
            let ok_chars = cpus.chars().all(|c| c.is_ascii_digit() || c == '.')
                && cpus.matches('.').count() <= 1;
            let v: f64 = if ok_chars {
                cpus.parse().unwrap_or(0.0)
            } else {
                0.0
            };
            if !(0.1..=64.0).contains(&v) {
                return Err("CPUs must be a number from 0.1 to 64.".into());
            }
        }
        Ok(Limits { memory, cpus })
    }

    /// The override file for `service`; None when there is nothing to limit.
    pub fn yaml(&self, service: &str) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut out = format!("services:\n  {service}:\n");
        if !self.memory.is_empty() {
            out.push_str(&format!("    mem_limit: \"{}\"\n", self.memory));
        }
        if !self.cpus.is_empty() {
            out.push_str(&format!("    cpus: \"{}\"\n", self.cpus));
        }
        Some(out)
    }
}

/// Current limits of an instance (empty when none are set or the file is unreadable).
pub fn load(dir: &Path) -> Limits {
    std::fs::read_to_string(dir.join(TOML))
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

fn replace(path: &Path, content: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("dockwren-new");
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o640)
        .open(&tmp)
        .map_err(err)?;
    f.write_all(content.as_bytes()).map_err(err)?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(err)
}

fn remove(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Store the limits and regenerate (or remove) the compose override.
pub fn save(dir: &Path, l: &Limits, service: &str) -> Result<(), String> {
    match l.yaml(service) {
        Some(y) => {
            let t = toml::to_string(l).map_err(|e| e.to_string())?;
            replace(&dir.join(TOML), &t)?;
            replace(&dir.join(YAML), &y)
        }
        None => {
            remove(&dir.join(TOML))?;
            remove(&dir.join(YAML))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_and_rejects() {
        assert_eq!(Limits::parse(" 512M ", "1.5").unwrap().memory, "512m");
        assert!(Limits::parse("", "").unwrap().is_empty());
        assert!(Limits::parse("2g", "").is_ok());
        for bad in ["63m", "12", "m", "1.5g", "-1g", "2gb", "9999999g"] {
            assert!(Limits::parse(bad, "").is_err(), "{bad}");
        }
        for bad in ["0", "0.05", "65", "1e1", "1..2", "-1", "abc"] {
            assert!(Limits::parse("", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn yaml_and_files_round_trip() {
        let l = Limits::parse("1g", "2").unwrap();
        assert_eq!(
            l.yaml("web").unwrap(),
            "services:\n  web:\n    mem_limit: \"1g\"\n    cpus: \"2\"\n"
        );
        assert!(Limits::default().yaml("web").is_none());
        let d = std::env::temp_dir().join(format!("dw-lim-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        save(&d, &l, "web").unwrap();
        assert_eq!(load(&d), l);
        assert!(d.join(YAML).exists());
        save(&d, &Limits::default(), "web").unwrap();
        assert!(!d.join(YAML).exists() && !d.join(TOML).exists());
        assert!(load(&d).is_empty());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
