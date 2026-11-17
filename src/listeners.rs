//! Host listener scan: TCP sockets that listen on every interface (`0.0.0.0` / `::`), read from
//! `/proc/net/tcp{,6}`. Complements the Docker "Open ports" check, which cannot see containers
//! with `network_mode: host` (such as the Caddy HTTPS container) or plain host services.

use std::collections::BTreeSet;

/// Ports that are expected to listen everywhere: ssh. 80/443 are added by the caller while the
/// Caddy container runs; extra ports come from `DOCKWREN_LISTEN_ALLOW` (comma separated).
pub const BASE_ALLOW: &[u16] = &[22];

/// Parse the text of `/proc/net/tcp` or `/proc/net/tcp6`: ports in LISTEN state bound to the
/// wildcard address.
pub fn wildcard_listeners(text: &str) -> BTreeSet<u16> {
    let mut out = BTreeSet::new();
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 4 || f[3] != "0A" {
            continue;
        }
        let Some((addr, port)) = f[1].split_once(':') else { continue };
        if !addr.chars().all(|c| c == '0') {
            continue;
        }
        if let Ok(p) = u16::from_str_radix(port, 16) {
            out.insert(p);
        }
    }
    out
}

/// Wildcard listeners on this host (IPv4 and IPv6 merged).
pub fn scan() -> BTreeSet<u16> {
    let mut all = BTreeSet::new();
    for f in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(t) = std::fs::read_to_string(f) {
            all.extend(wildcard_listeners(&t));
        }
    }
    all
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
   0: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 0 1 1\n\
   1: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 0 2 1\n\
   2: 00000000:01BB 00000000:0000 01 00000000:00000000 00:00000000 0 3 1\n";
    const T6: &str = "  sl  local_address rem_address   st\n\
   0: 00000000000000000000000000000000:1F40 00000000000000000000000000000000:0000 0A 0:0 0 4 1\n\
   1: 00000000000000000000000001000000:0050 00000000000000000000000000000000:0000 0A 0:0 0 5 1\n";

    #[test]
    fn parses_wildcard_listen_only() {
        assert_eq!(wildcard_listeners(T4), BTreeSet::from([22]));
        assert_eq!(wildcard_listeners(T6), BTreeSet::from([8000]));
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
}
