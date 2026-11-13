//! Runtime configuration, read from environment variables.

use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq)]
pub struct Config {
    /// Directory for Dockwren's own state (admin hash, later settings).
    pub data_dir: PathBuf,
    /// Listen address. Local-only by default; reach it over Tailscale or a tunnel.
    pub bind: String,
    /// Mark the session cookie `Secure` (set when served over HTTPS).
    pub secure_cookie: bool,
}

impl Config {
    /// Where templates live (`DOCKWREN_APPS`, default `./apps`).
    pub fn apps_dir(&self) -> PathBuf {
        std::env::var("DOCKWREN_APPS").map(PathBuf::from).unwrap_or_else(|_| "./apps".into())
    }

    /// Installed instances live under the data dir.
    pub fn instances_dir(&self) -> PathBuf {
        self.data_dir.join("instances")
    }

    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        Self {
            data_dir: get("DOCKWREN_DATA")
                .unwrap_or_else(|| "./data".into())
                .into(),
            bind: get("DOCKWREN_BIND").unwrap_or_else(|| "127.0.0.1:8080".into()),
            secure_cookie: get("DOCKWREN_SECURE_COOKIE").is_some_and(|v| v == "1"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_local_only() {
        let c = Config::from_lookup(|_| None);
        assert_eq!(c.data_dir, PathBuf::from("./data"));
        assert_eq!(c.bind, "127.0.0.1:8080");
        assert!(!c.secure_cookie);
    }

    #[test]
    fn env_overrides_defaults() {
        let c = Config::from_lookup(|k| match k {
            "DOCKWREN_DATA" => Some("/srv/dw".into()),
            "DOCKWREN_BIND" => Some("0.0.0.0:9000".into()),
            "DOCKWREN_SECURE_COOKIE" => Some("1".into()),
            _ => None,
        });
        assert_eq!(c.data_dir, PathBuf::from("/srv/dw"));
        assert_eq!(c.bind, "0.0.0.0:9000");
        assert!(c.secure_cookie);
    }
}
