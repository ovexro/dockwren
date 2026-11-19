//! Which app needs which other app (for example a web app that uses a database stack).
//!
//! Kept per app in `<instance>/needs`, one name per line. Dockwren uses it to warn before an
//! app that others need is stopped or deleted, to show a need that is not running, and to start
//! the needs first when a stopped app is started. Nothing else changes: Docker still runs
//! each stack on its own.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::runner;

const FILE: &str = "needs";

/// The needs of one app, as saved (not checked against what exists).
pub fn read(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join(FILE))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| runner::valid_name(l))
        .map(String::from)
        .collect()
}

pub fn write(dir: &Path, needs: &[String]) -> std::io::Result<()> {
    let path = dir.join(FILE);
    if needs.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let text: String = needs.iter().map(|n| format!("{n}\n")).collect();
    runner::replace_private(&path, &text)
}

/// Every app's needs, limited to apps that exist.
pub fn all(root: &Path) -> BTreeMap<String, Vec<String>> {
    let names: BTreeSet<String> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| runner::valid_name(n) && root.join(n).join("state.toml").is_file())
        .collect();
    names
        .iter()
        .map(|n| {
            let needs = read(&root.join(n))
                .into_iter()
                .filter(|d| names.contains(d) && d != n)
                .collect();
            (n.clone(), needs)
        })
        .collect()
}

/// Apps that need `name`, directly or through another app (sorted).
pub fn dependents(map: &BTreeMap<String, Vec<String>>, name: &str) -> Vec<String> {
    let mut found = BTreeSet::new();
    let mut todo = vec![name.to_string()];
    while let Some(cur) = todo.pop() {
        for (app, needs) in map {
            if needs.contains(&cur) && found.insert(app.clone()) {
                todo.push(app.clone());
            }
        }
    }
    found.remove(name);
    found.into_iter().collect()
}

/// `name` and everything it needs, the needs first (each app once).
pub fn start_order(map: &BTreeMap<String, Vec<String>>, name: &str) -> Vec<String> {
    fn visit(
        map: &BTreeMap<String, Vec<String>>,
        n: &str,
        seen: &mut BTreeSet<String>,
        out: &mut Vec<String>,
    ) {
        if !seen.insert(n.to_string()) {
            return;
        }
        for d in map.get(n).into_iter().flatten() {
            visit(map, d, seen, out);
        }
        out.push(n.to_string());
    }
    let mut out = Vec::new();
    visit(map, name, &mut BTreeSet::new(), &mut out);
    out
}

/// Check a new list of needs for `name`: each must exist, not be itself, and not lead back to it.
pub fn validate(
    map: &BTreeMap<String, Vec<String>>,
    name: &str,
    wanted: &[String],
) -> Result<Vec<String>, String> {
    let mut clean: Vec<String> = Vec::new();
    for w in wanted {
        if w == name {
            return Err("an app cannot need itself".into());
        }
        if !map.contains_key(w) {
            return Err(format!("{w} is not an installed app"));
        }
        if !clean.contains(w) {
            clean.push(w.clone());
        }
    }
    for w in &clean {
        if dependents(map, name).contains(w) {
            return Err(format!(
                "{w} already needs {name} (directly or through another app): that would be a loop"
            ));
        }
    }
    clean.sort();
    Ok(clean)
}

/// Drop `name` from every other app's needs (after it is deleted).
pub fn forget(root: &Path, name: &str) {
    for (app, needs) in all(root) {
        if needs.iter().any(|n| n == name) {
            let rest: Vec<String> = needs.into_iter().filter(|n| n != name).collect();
            let _ = write(&root.join(app), &rest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect()))
            .collect()
    }

    #[test]
    fn dependents_are_transitive() {
        let m = map(&[
            ("db", &[]),
            ("api", &["db"]),
            ("web", &["api"]),
            ("solo", &[]),
        ]);
        assert_eq!(dependents(&m, "db"), vec!["api", "web"]);
        assert!(dependents(&m, "web").is_empty());
    }

    #[test]
    fn start_order_puts_needs_first_once() {
        let m = map(&[("db", &[]), ("cache", &["db"]), ("web", &["db", "cache"])]);
        assert_eq!(start_order(&m, "web"), vec!["db", "cache", "web"]);
        assert_eq!(start_order(&m, "db"), vec!["db"]);
    }

    #[test]
    fn validate_refuses_self_unknown_and_loops() {
        let m = map(&[("db", &[]), ("web", &["db"])]);
        assert!(validate(&m, "web", &["web".into()]).is_err());
        assert!(validate(&m, "web", &["nope".into()]).is_err());
        assert!(
            validate(&m, "db", &["web".into()])
                .unwrap_err()
                .contains("loop")
        );
        assert_eq!(
            validate(&m, "web", &["db".into(), "db".into()]).unwrap(),
            vec!["db"]
        );
    }

    #[test]
    fn round_trip_and_forget() {
        let root = std::env::temp_dir().join(format!("dw-needs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for n in ["db", "web"] {
            std::fs::create_dir_all(root.join(n)).unwrap();
            std::fs::write(root.join(n).join("state.toml"), "").unwrap();
        }
        write(&root.join("web"), &["db".into(), "gone".into()]).unwrap();
        assert_eq!(all(&root)["web"], vec!["db"], "unknown names are ignored");
        forget(&root, "db");
        assert!(read(&root.join("web")).iter().all(|n| n != "db"));
        write(&root.join("web"), &[]).unwrap();
        assert!(!root.join("web").join(FILE).exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
