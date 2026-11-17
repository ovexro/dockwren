//! `dockwren doctor` and the Health section of Settings: read-only checks of the host that
//! Dockwren depends on (Docker, Compose, disk, memory, clock, DNS, registry, backups).
//! Nothing here changes anything or sends data anywhere except a DNS lookup and a TCP connect
//! to the Docker registry.

use std::path::Path;
use std::time::Duration;

use askama::Template;
use axum::{extract::State, response::Html};
use tokio::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

impl Level {
    pub fn tag(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub name: &'static str,
    pub level: Level,
    pub detail: String,
}

fn item(name: &'static str, level: Level, detail: impl Into<String>) -> Item {
    Item {
        name,
        level,
        detail: detail.into(),
    }
}

/// Run `prog args` and return trimmed stdout when it exits 0 within 10 seconds.
async fn out(prog: &str, args: &[&str]) -> Option<String> {
    let run = Command::new(prog).args(args).output();
    let o = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .ok()?
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// `(major, minor)` of a version like `v2.29.1` or `27.3.1`.
pub fn version_pair(v: &str) -> Option<(u32, u32)> {
    let mut p = v.trim().trim_start_matches('v').split(['.', '-', '+']);
    Some((p.next()?.parse().ok()?, p.next()?.parse().ok()?))
}

/// Percentage free of a filesystem and the level that goes with it.
pub fn disk_level(free_pct: f64) -> Level {
    if free_pct < 5.0 {
        Level::Fail
    } else if free_pct < 15.0 {
        Level::Warn
    } else {
        Level::Ok
    }
}

/// `MemAvailable` and `MemTotal` (kB) from /proc/meminfo text.
pub fn parse_meminfo(text: &str) -> Option<(u64, u64)> {
    let get = |key: &str| {
        text.lines().find_map(|l| {
            l.strip_prefix(key)?
                .trim()
                .strip_suffix("kB")?
                .trim()
                .parse()
                .ok()
        })
    };
    Some((get("MemAvailable:")?, get("MemTotal:")?))
}

pub fn memory_level(avail: u64, total: u64) -> Level {
    if total == 0 {
        return Level::Warn;
    }
    let pct = avail as f64 / total as f64 * 100.0;
    if pct < 5.0 {
        Level::Fail
    } else if pct < 10.0 {
        Level::Warn
    } else {
        Level::Ok
    }
}

fn gib(kb: u64) -> String {
    format!("{:.1} GiB", kb as f64 / 1024.0 / 1024.0)
}

async fn docker_checks(v: &mut Vec<Item>) {
    match out("docker", &["version", "--format", "{{.Server.Version}}"]).await {
        Some(s) if !s.is_empty() => v.push(match version_pair(&s) {
            Some((m, _)) if m >= 20 => item("Docker", Level::Ok, format!("engine {s}")),
            _ => item("Docker", Level::Warn, format!("engine {s} is old; 20.10 or newer is expected")),
        }),
        _ => v.push(item(
            "Docker",
            Level::Fail,
            "cannot talk to the Docker daemon: is it running, and is this user in the docker group?",
        )),
    }
    match out("docker", &["compose", "version", "--short"]).await {
        Some(s) if !s.is_empty() => v.push(match version_pair(&s) {
            Some((m, n)) if (m, n) >= (2, 20) => item(
                "Docker Compose",
                Level::Ok,
                format!("v{}", s.trim_start_matches('v')),
            ),
            _ => item(
                "Docker Compose",
                Level::Warn,
                format!("{s} is old; v2.20 or newer is expected"),
            ),
        }),
        _ => v.push(item(
            "Docker Compose",
            Level::Fail,
            "`docker compose` (the v2 plugin) is missing; apps cannot start without it",
        )),
    }
}

fn data_dir_check(data_dir: &Path) -> Item {
    use std::os::unix::fs::PermissionsExt;
    let probe = data_dir.join(".doctor-probe");
    let writable = std::fs::write(&probe, b"x").is_ok();
    let _ = std::fs::remove_file(&probe);
    if !writable {
        return item(
            "Data folder",
            Level::Fail,
            format!("{} is not writable", data_dir.display()),
        );
    }
    match std::fs::metadata(data_dir).map(|m| m.permissions().mode() & 0o077) {
        Ok(0) => item(
            "Data folder",
            Level::Ok,
            format!("{} is writable and private", data_dir.display()),
        ),
        Ok(_) => item(
            "Data folder",
            Level::Warn,
            format!(
                "{} is readable by other users; it holds password hashes and tokens (chmod 700)",
                data_dir.display()
            ),
        ),
        Err(e) => item(
            "Data folder",
            Level::Warn,
            format!("cannot inspect {}: {e}", data_dir.display()),
        ),
    }
}

async fn disk_check(data_dir: &Path) -> Item {
    let Some(p) = data_dir.to_str() else {
        return item("Disk space", Level::Warn, "data folder path is not text");
    };
    let Some(text) = out("df", &["-P", "-k", p]).await else {
        return item("Disk space", Level::Warn, "df failed");
    };
    let line = text.lines().nth(1).unwrap_or_default();
    let cols: Vec<&str> = line.split_whitespace().collect();
    let (Some(total), Some(avail)) = (
        cols.get(1).and_then(|c| c.parse::<u64>().ok()),
        cols.get(3).and_then(|c| c.parse::<u64>().ok()),
    ) else {
        return item("Disk space", Level::Warn, "cannot read df output");
    };
    let pct = if total == 0 {
        0.0
    } else {
        avail as f64 / total as f64 * 100.0
    };
    item(
        "Disk space",
        disk_level(pct),
        format!(
            "{} free of {} ({pct:.0}%) where app data lives",
            gib(avail),
            gib(total)
        ),
    )
}

fn memory_check() -> Item {
    let text = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    match parse_meminfo(&text) {
        Some((a, t)) => item(
            "Memory",
            memory_level(a, t),
            format!("{} available of {}", gib(a), gib(t)),
        ),
        None => item("Memory", Level::Warn, "cannot read /proc/meminfo"),
    }
}

async fn clock_check() -> Item {
    match out("timedatectl", &["show", "-p", "NTPSynchronized", "--value"])
        .await
        .as_deref()
    {
        Some("yes") => item("Clock", Level::Ok, "synchronised with a time server"),
        Some(_) => item(
            "Clock",
            Level::Warn,
            "not synchronised; wrong time breaks certificates and two-factor codes",
        ),
        None => item(
            "Clock",
            Level::Warn,
            "cannot tell (timedatectl not available)",
        ),
    }
}

async fn network_checks(v: &mut Vec<Item>) {
    let host = "registry-1.docker.io:443";
    let r = tokio::time::timeout(Duration::from_secs(6), async {
        let mut addrs = tokio::net::lookup_host(host)
            .await
            .map_err(|_| "DNS lookup failed")?;
        let a = addrs.next().ok_or("DNS returned no address")?;
        tokio::net::TcpStream::connect(a)
            .await
            .map_err(|_| "DNS works but the connection was refused or blocked")?;
        Ok::<(), &str>(())
    })
    .await;
    v.push(match r {
        Ok(Ok(())) => item(
            "Internet and DNS",
            Level::Ok,
            "Docker Hub resolves and accepts connections",
        ),
        Ok(Err(e)) => item(
            "Internet and DNS",
            Level::Fail,
            format!("{e} (registry-1.docker.io); installs and updates will fail"),
        ),
        Err(_) => item(
            "Internet and DNS",
            Level::Fail,
            "timed out reaching registry-1.docker.io; installs and updates will fail",
        ),
    });
}

fn backup_checks(data_dir: &Path, now: u64, v: &mut Vec<Item>) {
    let off = crate::offsite::Offsite::open(data_dir);
    if !off.get().enabled() {
        v.push(item(
            "Off-host backups",
            Level::Warn,
            "not set up: backups live on this server only and die with its disk",
        ));
        return;
    }
    let last = off.last_ok();
    v.push(if last == 0 {
        item(
            "Off-host backups",
            Level::Warn,
            "configured but nothing has been uploaded yet",
        )
    } else {
        let age = now.saturating_sub(last);
        let span = crate::metrics::short_span(age);
        if age > 3 * 86400 {
            item(
                "Off-host backups",
                Level::Warn,
                format!("last upload {span} ago"),
            )
        } else {
            item(
                "Off-host backups",
                Level::Ok,
                format!("last upload {span} ago"),
            )
        }
    });
}

fn tunnel_check(v: &mut Vec<Item>) {
    v.push(match crate::public::Settings::from_env() {
        Some(_) => item(
            "Cloudflare",
            Level::Ok,
            "token, account and tunnel are configured",
        ),
        None => item("Cloudflare", Level::Ok, "not connected (optional)"),
    });
}

/// Every check, in display order. Takes a few seconds at most (each probe has a timeout).
/// Docker-published ports that listen on every interface (ufw does not guard those).
async fn ports_check(v: &mut Vec<Item>) {
    let list = match crate::docker::DockerClient::connect() {
        Ok(d) => d.containers().await.ok(),
        Err(_) => None,
    };
    let Some(list) = list else { return };
    let open = crate::alerts::open_ports(&list);
    v.push(if open.is_empty() {
        item("Open ports", Level::Ok, "No container publishes a port on every network interface.")
    } else {
        let names: Vec<String> = open.iter().map(|o| format!("{} ({})", o.1, o.2)).collect();
        item(
            "Open ports",
            Level::Warn,
            format!("Reachable on every interface, and Docker bypasses ufw: {}. Publish on 127.0.0.1 unless that is intended.", names.join(", ")),
        )
    });
}

/// Host sockets that listen on every interface and that nothing explains (host-network
/// containers and plain host services, which the Docker check cannot see).
async fn listeners_check(v: &mut Vec<Item>) {
    let list = match crate::docker::DockerClient::connect() {
        Ok(d) => d.containers().await.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let odd = crate::alerts::host_listeners(&list);
    v.push(if odd.is_empty() {
        item("Host listeners", Level::Ok, "Only expected ports (ssh, Caddy) listen on every interface.")
    } else {
        let ports: Vec<String> = odd.iter().map(|(p, who)| format!("{p} ({who})")).collect();
        item(
            "Host listeners",
            Level::Warn,
            format!("Listening on every interface and not explained by Docker or Caddy: {}. Bind to 127.0.0.1 or allow with DOCKWREN_LISTEN_ALLOW if intended.", ports.join(", ")),
        )
    });
}

pub async fn run(data_dir: &Path) -> Vec<Item> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut v = Vec::new();
    docker_checks(&mut v).await;
    v.push(data_dir_check(data_dir));
    v.push(disk_check(data_dir).await);
    v.push(memory_check());
    v.push(clock_check().await);
    network_checks(&mut v).await;
    ports_check(&mut v).await;
    listeners_check(&mut v).await;
    backup_checks(data_dir, now, &mut v);
    tunnel_check(&mut v);
    v
}

/// Worst level of a result set.
pub fn worst(items: &[Item]) -> Level {
    items
        .iter()
        .map(|i| i.level)
        .fold(Level::Ok, |a, l| match (a, l) {
            (Level::Fail, _) | (_, Level::Fail) => Level::Fail,
            (Level::Warn, _) | (_, Level::Warn) => Level::Warn,
            _ => Level::Ok,
        })
}

/// Text for the terminal.
pub fn render_text(items: &[Item]) -> String {
    let mut s = String::new();
    for i in items {
        s.push_str(&format!(
            "[{:>4}] {}: {}\n",
            i.level.tag(),
            i.name,
            i.detail
        ));
    }
    s
}

#[derive(Template)]
#[template(path = "doctor.html")]
struct View {
    rows: Vec<(String, &'static str, String)>,
    summary: String,
}

/// `GET /settings/doctor`: run the checks and show them.
pub async fn show(State(apps): State<crate::apps::Apps>) -> Html<String> {
    let data_dir = apps
        .root
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| apps.root.clone());
    let items = run(&data_dir).await;
    let summary = match worst(&items) {
        Level::Ok => "Everything checks out.".to_string(),
        Level::Warn => "Works, but some things deserve attention.".to_string(),
        Level::Fail => {
            "Something is broken that will stop apps from installing or running.".to_string()
        }
    };
    let rows = items
        .into_iter()
        .map(|i| (i.name.to_string(), i.level.tag(), i.detail))
        .collect();
    Html(View { rows, summary }.render().expect("doctor renders"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_with_prefixes_and_suffixes() {
        assert_eq!(version_pair("v2.29.1"), Some((2, 29)));
        assert_eq!(version_pair("27.3.1"), Some((27, 3)));
        assert_eq!(version_pair("2.20.0-desktop.1"), Some((2, 20)));
        assert_eq!(version_pair("nonsense"), None);
    }

    #[test]
    fn disk_and_memory_levels_have_thresholds() {
        assert_eq!(disk_level(50.0), Level::Ok);
        assert_eq!(disk_level(10.0), Level::Warn);
        assert_eq!(disk_level(2.0), Level::Fail);
        assert_eq!(memory_level(500, 1000), Level::Ok);
        assert_eq!(memory_level(80, 1000), Level::Warn);
        assert_eq!(memory_level(10, 1000), Level::Fail);
        assert_eq!(memory_level(0, 0), Level::Warn);
    }

    #[test]
    fn meminfo_parses_available_and_total() {
        let t = "MemTotal:        8000000 kB\nMemFree: 1 kB\nMemAvailable:    4000000 kB\n";
        assert_eq!(parse_meminfo(t), Some((4_000_000, 8_000_000)));
        assert_eq!(parse_meminfo("nothing"), None);
    }

    #[test]
    fn worst_picks_the_most_severe_and_text_lists_all() {
        let a = vec![item("a", Level::Ok, "x"), item("b", Level::Warn, "y")];
        assert_eq!(worst(&a), Level::Warn);
        let b = vec![item("a", Level::Warn, "x"), item("b", Level::Fail, "y")];
        assert_eq!(worst(&b), Level::Fail);
        assert_eq!(worst(&[]), Level::Ok);
        let t = render_text(&b);
        assert!(
            t.contains("[warn] a: x") && t.contains("[FAIL] b: y"),
            "{t}"
        );
    }

    #[test]
    fn data_folder_check_flags_open_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("dw-doctor-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(data_dir_check(&d).level, Level::Ok);
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(data_dir_check(&d).level, Level::Warn);
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(data_dir_check(&d).level, Level::Fail);
    }
}
