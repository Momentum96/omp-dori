//! Service-manager notification over `NOTIFY_SOCKET` (systemd `Type=notify`, `WatchdogSec`), using a
//! plain Unix datagram.

use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::time::Duration;

/// Longest interval between watchdog pings this hub uses (DESIGN 2.2).
const WATCHDOG_PING: Duration = Duration::from_secs(10);

/// Sends state lines to the service manager when one asked for them; otherwise does nothing.
#[derive(Debug)]
pub(crate) struct Notifier {
    target: Option<(UnixDatagram, SocketAddr)>,
    watchdog: Option<Duration>,
}

impl Notifier {
    /// Reads `NOTIFY_SOCKET` and `WATCHDOG_USEC` (honoured only when `WATCHDOG_PID` is absent or ours).
    pub(crate) fn from_env() -> Self {
        let socket = std::env::var_os("NOTIFY_SOCKET").and_then(|s| s.into_string().ok());
        let usec = std::env::var("WATCHDOG_USEC")
            .ok()
            .and_then(|v| v.parse::<u64>().ok());
        let pid_ok = std::env::var("WATCHDOG_PID")
            .ok()
            .is_none_or(|p| p.parse::<u32>().ok() == Some(std::process::id()));
        Self::new(socket.as_deref(), usec.filter(|_| pid_ok))
    }

    pub(crate) fn new(socket: Option<&str>, watchdog_usec: Option<u64>) -> Self {
        let target = socket.and_then(|path| {
            let addr = address(path)?;
            let sock = UnixDatagram::unbound().ok()?;
            Some((sock, addr))
        });
        let watchdog = watchdog_usec
            .filter(|u| *u > 0 && target.is_some())
            .map(|u| Duration::from_micros(u / 2).min(WATCHDOG_PING));
        Self { target, watchdog }
    }

    /// How often to ping while the main loop moves; `None` when no watchdog is armed.
    pub(crate) fn watchdog_interval(&self) -> Option<Duration> {
        self.watchdog
    }

    pub(crate) fn ready(&self) {
        self.send("READY=1");
    }

    pub(crate) fn stopping(&self) {
        self.send("STOPPING=1");
    }

    pub(crate) fn watchdog(&self) {
        self.send("WATCHDOG=1");
    }

    fn send(&self, line: &str) {
        if let Some((sock, addr)) = &self.target {
            let _ = sock.send_to_addr(line.as_bytes(), addr);
        }
    }
}

fn address(path: &str) -> Option<SocketAddr> {
    if let Some(name) = path.strip_prefix('@') {
        abstract_address(name)
    } else {
        SocketAddr::from_pathname(path).ok()
    }
}

#[cfg(target_os = "linux")]
fn abstract_address(name: &str) -> Option<SocketAddr> {
    use std::os::linux::net::SocketAddrExt;
    SocketAddr::from_abstract_name(name.as_bytes()).ok()
}

#[cfg(not(target_os = "linux"))]
fn abstract_address(_name: &str) -> Option<SocketAddr> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::fs::tests::scratch;

    #[test]
    fn sends_state_lines_and_derives_watchdog() {
        let dir = scratch("notify");
        let path = dir.join("n.sock");
        let server = UnixDatagram::bind(&path).unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let n = Notifier::new(path.to_str(), Some(30_000_000));
        assert_eq!(n.watchdog_interval(), Some(Duration::from_secs(10)));
        n.ready();
        n.watchdog();
        let mut buf = [0u8; 64];
        let len = server.recv(&mut buf).unwrap();
        assert_eq!(&buf[..len], b"READY=1");
        let len = server.recv(&mut buf).unwrap();
        assert_eq!(&buf[..len], b"WATCHDOG=1");
        let short = Notifier::new(path.to_str(), Some(4_000_000));
        assert_eq!(short.watchdog_interval(), Some(Duration::from_secs(2)));
    }

    #[test]
    fn without_socket_nothing_is_armed() {
        let n = Notifier::new(None, Some(30_000_000));
        assert_eq!(n.watchdog_interval(), None);
        n.ready();
    }
}
