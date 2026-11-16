//! Docker log size cap for every service of an app.
//!
//! Docker's default `json-file` driver never rotates, so a chatty container can fill the
//! disk. `compose.logging.yaml` sets `max-size` and `max-file` on every service; it is
//! regenerated from `docker compose config --services` (the authoritative service list) on
//! install and update, and `runner::compose_files` adds it to every compose call.

use std::path::Path;

pub const YAML: &str = "compose.logging.yaml";
pub const MAX_SIZE: &str = "10m";
pub const MAX_FILE: &str = "3";

/// The override file for `services`; None when there are none.
pub fn yaml(services: &[String]) -> Option<String> {
    if services.is_empty() {
        return None;
    }
    let mut out = String::from("services:\n");
    for s in services {
        out.push_str(&format!(
            "  {s}:\n    logging:\n      driver: json-file\n      options:\n        max-size: \"{MAX_SIZE}\"\n        max-file: \"{MAX_FILE}\"\n"
        ));
    }
    Some(out)
}

/// Service names from `docker compose config --services` output. Names that are not plain
/// compose identifiers are dropped, so the generated YAML cannot be bent by odd input.
pub fn parse_services(out: &str) -> Vec<String> {
    let mut v: Vec<String> = out
        .lines()
        .map(str::trim)
        .filter(|l| {
            !l.is_empty()
                && l.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        })
        .map(String::from)
        .collect();
    v.sort();
    v.dedup();
    v
}

pub fn enabled(dir: &Path) -> bool {
    dir.join(YAML).is_file()
}

/// Write (or refresh) the cap file for an instance. The old file is removed first so the
/// service query sees the plain compose files; a failed query leaves no cap file behind.
pub async fn ensure(inst: &crate::runner::Instance) -> Result<(), String> {
    let path = inst.dir.join(YAML);
    let _ = std::fs::remove_file(&path);
    let out = crate::runner::compose(inst, &["config", "--services"]).await?;
    let Some(y) = yaml(&parse_services(&out)) else {
        return Ok(());
    };
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
    f.write_all(y.as_bytes()).map_err(err)?;
    drop(f);
    std::fs::rename(&tmp, &path).map_err(err)
}

pub fn remove(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(YAML));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_covers_every_service() {
        let y = yaml(&["db".into(), "web".into()]).unwrap();
        assert!(y.contains("  db:\n    logging:"));
        assert!(y.contains("  web:\n    logging:"));
        assert!(y.contains("max-size: \"10m\""));
        assert!(yaml(&[]).is_none());
    }

    #[test]
    fn parse_services_filters_and_sorts() {
        let v = parse_services("web\ndb\n\nweb\nbad name\nx: y\n");
        assert_eq!(v, vec!["db".to_string(), "web".to_string()]);
    }
}
