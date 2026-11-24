//! Liveness (DESIGN 2.2, OJ req. 47): the hub's main loop advances a counter every second; a detector
//! thread pings the systemd watchdog only while the counter moves, and without a watchdog (launchd, or a
//! hub started by hand) it ends the process after the stall limit.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::notify::Notifier;

/// The counter the main loop advances.
#[derive(Debug, Clone, Default)]
pub(crate) struct Heartbeat(Arc<AtomicU64>);

impl Heartbeat {
    pub(crate) fn beat(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn value(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// What one observation of the counter means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Liveness {
    Progress,
    Quiet,
    Stalled,
}

/// Tracks when the counter last moved.
#[derive(Debug)]
pub(crate) struct StallWatch {
    last: u64,
    since: Instant,
    limit: Duration,
}

impl StallWatch {
    pub(crate) fn new(value: u64, now: Instant, limit: Duration) -> Self {
        Self {
            last: value,
            since: now,
            limit,
        }
    }

    pub(crate) fn observe(&mut self, value: u64, now: Instant) -> Liveness {
        if value != self.last {
            self.last = value;
            self.since = now;
            Liveness::Progress
        } else if now.saturating_duration_since(self.since) >= self.limit {
            Liveness::Stalled
        } else {
            Liveness::Quiet
        }
    }
}

/// Starts the detector thread. `on_stall` runs once, only when no systemd watchdog is armed.
pub(crate) fn spawn_detector(
    heartbeat: Heartbeat,
    notifier: Arc<Notifier>,
    limit: Duration,
    tick: Duration,
    on_stall: impl FnOnce() + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("cataly-stall".into())
        .stack_size(64 * 1024)
        .spawn(move || {
            let mut watch = StallWatch::new(heartbeat.value(), Instant::now(), limit);
            let mut last_ping: Option<Instant> = None;
            let mut on_stall = Some(on_stall);
            loop {
                std::thread::sleep(tick);
                let now = Instant::now();
                match watch.observe(heartbeat.value(), now) {
                    Liveness::Progress => {
                        if let Some(every) = notifier.watchdog_interval()
                            && last_ping.is_none_or(|t| now.saturating_duration_since(t) >= every)
                        {
                            notifier.watchdog();
                            last_ping = Some(now);
                        }
                    }
                    Liveness::Quiet => {}
                    Liveness::Stalled => {
                        if notifier.watchdog_interval().is_none()
                            && let Some(stall) = on_stall.take()
                        {
                            stall();
                            return;
                        }
                    }
                }
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_reports_progress_quiet_and_stall() {
        let t0 = Instant::now();
        let mut w = StallWatch::new(0, t0, Duration::from_secs(60));
        assert_eq!(w.observe(0, t0 + Duration::from_secs(30)), Liveness::Quiet);
        assert_eq!(
            w.observe(1, t0 + Duration::from_secs(59)),
            Liveness::Progress
        );
        assert_eq!(w.observe(1, t0 + Duration::from_secs(118)), Liveness::Quiet);
        assert_eq!(
            w.observe(1, t0 + Duration::from_secs(119)),
            Liveness::Stalled
        );
    }

    #[test]
    fn detector_fires_when_the_counter_stops() {
        let beat = Heartbeat::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = spawn_detector(
            beat.clone(),
            Arc::new(Notifier::new(None, None)),
            Duration::from_millis(150),
            Duration::from_millis(20),
            move || tx.send(()).unwrap(),
        )
        .unwrap();
        beat.beat();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        handle.join().unwrap();
    }

    /// Under systemd (watchdog armed) a stall only stops the pings, so systemd restarts the hub;
    /// the detector never ends the process itself. Without a watchdog (launchd) it does, above.
    #[test]
    fn under_a_watchdog_a_stall_stops_the_pings_and_leaves_the_restart_to_systemd() {
        use std::os::unix::net::UnixDatagram;
        let dir = crate::sys::fs::tests::scratch("stall-systemd");
        let path = dir.join("n.sock");
        let server = UnixDatagram::bind(&path).unwrap();
        server
            .set_read_timeout(Some(Duration::from_millis(400)))
            .unwrap();
        let beat = Heartbeat::default();
        let (tx, rx) = std::sync::mpsc::channel();
        // WATCHDOG_USEC 100 ms: a ping every 50 ms while the counter moves.
        spawn_detector(
            beat.clone(),
            Arc::new(Notifier::new(path.to_str(), Some(100_000))),
            Duration::from_millis(150),
            Duration::from_millis(10),
            move || tx.send(()).unwrap(),
        )
        .unwrap();
        // Let the detector read the starting value first.
        std::thread::sleep(Duration::from_millis(50));
        let mut buf = [0u8; 32];
        for _ in 0..5 {
            beat.beat();
            let len = server.recv(&mut buf).unwrap();
            assert_eq!(&buf[..len], b"WATCHDOG=1");
            std::thread::sleep(Duration::from_millis(60));
        }
        // The counter stops: once the stall limit passes, no ping arrives any more.
        std::thread::sleep(Duration::from_millis(300));
        while server.recv(&mut buf).is_ok() {}
        assert!(server.recv(&mut buf).is_err(), "pings stopped");
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "the detector leaves the restart to systemd"
        );
    }
}
