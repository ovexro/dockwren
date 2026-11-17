//! Host listener scan: TCP sockets that listen on every interface (`0.0.0.0` / `::`), read from
//! `/proc/net/tcp{,6}`. Complements the Docker "Open ports" check, which cannot see containers
//! with `network_mode: host` (such as the Caddy HTTPS container) or plain host services.

use std::collections::{BTreeMap, BTreeSet};

/// Ports that are expected to listen everywhere: ssh. 80/443 are added by the caller while the
/// Caddy container runs; extra ports come from `DOCKWREN_LISTEN_ALLOW` (comma separated).
pub const BASE_ALLOW: &[u16] = &[22];

/// Parse the text of `/proc/net/tcp` or `/proc/net/tcp6`: wildcard-bound LISTEN sockets as
/// port -> socket inode.
pub fn wildcard_inodes(text: &str) -> BTreeMap<u16, u64> {
    let mut out = BTreeMap::new();
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 10 || f[3] != "0A" {
            continue;
        }
        let Some((addr, port)) = f[1].split_once(':') else { continue };
        if !addr.chars().all(|c| c == '0') {
            continue;
        }
        if let (Ok(p), Ok(i)) = (u16::from_str_radix(port, 16), f[9].parse::<u64>()) {
            out.insert(p, i);
        }
    }
    out
}

/// Wildcard listeners on this host (IPv4 and IPv6 merged): port -> socket inode.
pub fn scan() -> BTreeMap<u16, u64> {
    let mut all = BTreeMap::new();
    for f in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(t) = std::fs::read_to_string(f) {
            all.extend(wildcard_inodes(&t));
        }
    }
    all
}

/// Name of the process that holds the socket, when this user may look (its own processes; other
/// users' `/proc/<pid>/fd` is unreadable without root, so root services stay unnamed).
pub fn owner_of(inode: u64) -> Option<String> {
    let want = format!("socket:[{inode}]");
    for e in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = e.file_name();
        let pid = name.to_str()?;
        if !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(e.path().join("fd")) else { continue };
        for fd in fds.flatten() {
            if std::fs::read_link(fd.path()).is_ok_and(|l| l.to_str() == Some(want.as_str())) {
                let comm = std::fs::read_to_string(e.path().join("comm")).unwrap_or_default();
                return Some(format!("{} (pid {pid})", comm.trim()));
            }
        }
    }
    None
}

/// Who probably owns a listener nothing explains: the process when visible, else the running
/// host-network containers (the usual source), else a hint about permissions.
pub fn describe_owner(process: Option<String>, host_containers: &[String]) -> String {
    match (process, host_containers) {
        (Some(p), _) => format!("process {p}"),
        (None, []) => "owner not visible to the panel (not one of its own processes)".to_string(),
        (None, c) => format!("possibly host-network container {}", c.join(", ")),
    }
}

/// Ports from `DOCKWREN_LISTEN_ALLOW`, ignoring anything that is not a port number.
pub fn parse_allow(s: &str) -> Vec<u16> {
    s.split(',').filter_map(|x| x.trim().parse().ok()).collect()
}

/// Listeners that nothing explains: not allowlisted and not a Docker-published port (those are
/// reported by the Open ports check).
pub fn unexpected(found: &BTreeSet<u16>, allow: &[u16], docker: &BTreeSet<u16>) -> Vec<u16> {
    found
        .iter()
        .copied()
        .filter(|p| !allow.contains(p) && !docker.contains(p))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const T4: &str = "  sl  local_address rem_address   st tx_queue rx_queue\n\
   0: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000 0 0 111 1 0\n\
   1: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000 0 0 222 1 0\n\
   2: 00000000:01BB 00000000:0000 01 00000000:00000000 00:00000000 00000000 0 0 333 1 0\n";
    const T6: &str = "  sl  local_address rem_address   st\n\
   0: 00000000000000000000000000000000:1F40 00000000000000000000000000000000:0000 0A 0:0 0:0 0 0 0 444 1 0\n\
   1: 00000000000000000000000001000000:0050 00000000000000000000000000000000:0000 0A 0:0 0:0 0 0 0 555 1 0\n";

    #[test]
    fn parses_wildcard_listen_only() {
        assert_eq!(wildcard_inodes(T4), BTreeMap::from([(22, 111)]));
        assert_eq!(wildcard_inodes(T6), BTreeMap::from([(8000, 444)]));
    }

    #[test]
    fn filters_allowed_and_docker() {
        let found = BTreeSet::from([22, 80, 8000, 9999]);
        let docker = BTreeSet::from([8000]);
        assert_eq!(unexpected(&found, &[22, 80], &docker), vec![9999]);
    }

    #[test]
    fn allow_list_parsing() {
        assert_eq!(parse_allow("22, 8443,x,,70000"), vec![22, 8443]);
    }

    #[test]
    fn names_the_most_likely_owner() {
        assert_eq!(describe_owner(Some("nginx (pid 7)".into()), &[]), "process nginx (pid 7)");
        assert_eq!(
            describe_owner(None, &["dockwren-caddy".into(), "x".into()]),
            "possibly host-network container dockwren-caddy, x"
        );
        assert!(describe_owner(None, &[]).starts_with("owner not visible"));
    }

    #[test]
    fn finds_own_socket_owner() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let ino = std::fs::read_link(format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&l)))
            .unwrap();
        let ino: u64 = ino.to_str().unwrap().trim_start_matches("socket:[").trim_end_matches(']').parse().unwrap();
        assert!(owner_of(ino).is_some());
        assert!(owner_of(u64::MAX).is_none());
    }
}
