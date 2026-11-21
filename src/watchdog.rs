//! systemd watchdog: if the panel's own runtime stops serving (a deadlock, all workers blocked),
//! the pings stop and systemd restarts the service (`Restart=on-failure` covers a watchdog kill).
//! A ping is only sent after a real `GET /health` over loopback succeeded, so this checks the
//! accept loop and the async runtime, not just that the process exists. It does nothing unless
//! systemd asked for it (`WatchdogSec=` sets `WATCHDOG_USEC`, `NotifyAccess=main` allows the socket).

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    os::{linux::net::SocketAddrExt, unix::net::UnixDatagram},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

/// Probe limit; well under the ping interval (a quarter of `WatchdogSec`).
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Starts the ping task when systemd enabled the watchdog for this service.
pub fn spawn(bind: &str) {
    let (Some(usec), Some(sock)) = (
        std::env::var("WATCHDOG_USEC")
            .ok()
            .and_then(|v| v.parse::<u64>().ok()),
        std::env::var("NOTIFY_SOCKET").ok(),
    ) else {
        return;
    };
    let Some(addr) = probe_addr(bind) else {
        eprintln!("watchdog: cannot parse bind address {bind}; watchdog off");
        return;
    };
    let every = Duration::from_micros(usec / 4).max(Duration::from_secs(1));
    println!("watchdog: pinging systemd every {every:?} after a /health check");
    tokio::spawn(async move {
        loop {
            if probe(addr, PROBE_TIMEOUT).await {
                if let Err(e) = notify(&sock, "WATCHDOG=1") {
                    eprintln!("watchdog: cannot notify systemd: {e}");
                }
            } else {
                eprintln!("watchdog: /health did not answer; not pinging systemd");
            }
            tokio::time::sleep(every).await;
        }
    });
}

/// Loopback address of the panel's own listener (a wildcard bind is reached via 127.0.0.1).
fn probe_addr(bind: &str) -> Option<SocketAddr> {
    let a: SocketAddr = bind.parse().ok()?;
    Some(if a.ip().is_unspecified() {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), a.port())
    } else {
        a
    })
}

/// True when `GET /health` answers 200 within `limit`.
async fn probe(addr: SocketAddr, limit: Duration) -> bool {
    let attempt = async {
        let mut s = TcpStream::connect(addr).await.ok()?;
        s.write_all(b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .await
            .ok()?;
        let mut head = [0u8; 12];
        s.read_exact(&mut head).await.ok()?;
        Some(&head == b"HTTP/1.1 200" || &head == b"HTTP/1.0 200")
    };
    matches!(tokio::time::timeout(limit, attempt).await, Ok(Some(true)))
}

/// One sd_notify datagram. `NOTIFY_SOCKET` is a path, or `@name` for an abstract socket.
fn notify(sock: &str, msg: &str) -> std::io::Result<()> {
    let s = UnixDatagram::unbound()?;
    if let Some(name) = sock.strip_prefix('@') {
        let a = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())?;
        s.send_to_addr(msg.as_bytes(), &a)?;
    } else {
        s.send_to(msg.as_bytes(), sock)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    async fn server(reply: &'static [u8], hang: bool) -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut c, _) = l.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut b = [0u8; 64];
                    let _ = c.read(&mut b).await;
                    if hang {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                    } else {
                        let _ = c.write_all(reply).await;
                    }
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn probe_accepts_only_a_200() {
        let ok = server(b"HTTP/1.1 200 OK\r\n\r\nok", false).await;
        let bad = server(b"HTTP/1.1 500 Internal Server Error\r\n\r\n", false).await;
        assert!(probe(ok, Duration::from_secs(2)).await);
        assert!(!probe(bad, Duration::from_secs(2)).await);
    }

    #[tokio::test]
    async fn probe_fails_when_the_server_hangs_or_is_gone() {
        let hung = server(b"", true).await;
        let t = std::time::Instant::now();
        assert!(!probe(hung, Duration::from_millis(300)).await);
        assert!(t.elapsed() < Duration::from_secs(3));
        let gone = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = gone.local_addr().unwrap();
        drop(gone);
        assert!(!probe(addr, Duration::from_secs(1)).await);
    }

    #[test]
    fn wildcard_bind_probes_loopback() {
        assert_eq!(
            probe_addr("0.0.0.0:8080").unwrap().to_string(),
            "127.0.0.1:8080"
        );
        assert_eq!(
            probe_addr("127.0.0.1:8080").unwrap().to_string(),
            "127.0.0.1:8080"
        );
        assert!(probe_addr("nonsense").is_none());
    }

    #[test]
    fn notify_sends_to_path_and_abstract_sockets() {
        let p = std::env::temp_dir().join(format!("dw-wd-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let rx = UnixDatagram::bind(&p).unwrap();
        notify(p.to_str().unwrap(), "WATCHDOG=1").unwrap();
        let mut buf = [0u8; 32];
        let n = rx.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"WATCHDOG=1");
        let _ = std::fs::remove_file(&p);
        let name = format!("dw-wd-abs-{}", std::process::id());
        let a = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let rx = UnixDatagram::bind_addr(&a).unwrap();
        notify(&format!("@{name}"), "WATCHDOG=1").unwrap();
        let n = rx.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"WATCHDOG=1");
    }
}
