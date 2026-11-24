//! Supervision (DESIGN 2.2): every long-lived task runs under a supervisor that logs why it ended and
//! restarts it with backoff (1 s doubling to 60 s). More than 5 restarts of one task within 10 minutes
//! asks the hub to exit with status 70 so the service manager restarts it cleanly.

use std::collections::VecDeque;
use std::fmt::Display;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

use crate::log;

/// Exit status asked for when a task keeps failing.
pub(crate) const EXIT_RESTARTS: u8 = 70;

/// Restart timing.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Policy {
    pub(crate) first_backoff: Duration,
    pub(crate) max_backoff: Duration,
    pub(crate) window: Duration,
    pub(crate) max_restarts: usize,
}

impl Policy {
    pub(crate) const HUB: Self = Self {
        first_backoff: Duration::from_secs(1),
        max_backoff: Duration::from_secs(60),
        window: Duration::from_secs(600),
        max_restarts: 5,
    };
}

struct Inner {
    policy: Policy,
    shutdown: watch::Receiver<bool>,
    exit: watch::Sender<Option<u8>>,
    panics: AtomicU64,
    restarts: AtomicU64,
}

/// Starts and restarts the hub's long-lived tasks.
#[derive(Clone)]
pub(crate) struct Supervisor(Arc<Inner>);

impl Supervisor {
    /// Returns the supervisor and the receiver on which it asks the hub to exit.
    pub(crate) fn new(
        policy: Policy,
        shutdown: watch::Receiver<bool>,
    ) -> (Self, watch::Receiver<Option<u8>>) {
        let (exit, exit_rx) = watch::channel(None);
        let inner = Inner {
            policy,
            shutdown,
            exit,
            panics: AtomicU64::new(0),
            restarts: AtomicU64::new(0),
        };
        (Self(Arc::new(inner)), exit_rx)
    }

    /// Panics caught in supervised tasks since start.
    pub(crate) fn panics(&self) -> u64 {
        self.0.panics.load(Ordering::Relaxed)
    }

    /// Restarts of supervised tasks since start.
    pub(crate) fn restarts(&self) -> u64 {
        self.0.restarts.load(Ordering::Relaxed)
    }

    /// Runs `factory()` as task `name` until the hub shuts down, restarting it whenever it ends.
    pub(crate) fn spawn<F, Fut, E>(&self, name: &'static str, factory: F)
    where
        F: Fn() -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
        E: Display + Send + 'static,
    {
        let inner = Arc::clone(&self.0);
        tokio::spawn(async move { supervise(&inner, name, factory).await });
    }
}

async fn supervise<F, Fut, E>(inner: &Inner, name: &'static str, factory: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), E>> + Send + 'static,
    E: Display + Send + 'static,
{
    let policy = inner.policy;
    let mut shutdown = inner.shutdown.clone();
    let mut recent: VecDeque<Instant> = VecDeque::new();
    let mut backoff = policy.first_backoff;
    loop {
        let started = Instant::now();
        let ended = tokio::spawn(factory()).await;
        if *shutdown.borrow() {
            return;
        }
        match ended {
            Ok(Ok(())) => log::warn("task ended", &[("task", &name)]),
            Ok(Err(e)) => log::warn("task failed", &[("task", &name), ("error", &e)]),
            Err(join) if join.is_panic() => {
                inner.panics.fetch_add(1, Ordering::Relaxed);
                log::error("task panicked", &[("task", &name)]);
            }
            Err(_) => log::warn("task cancelled", &[("task", &name)]),
        }
        let now = Instant::now();
        while recent
            .front()
            .is_some_and(|t| now.duration_since(*t) >= policy.window)
        {
            recent.pop_front();
        }
        recent.push_back(now);
        if recent.len() > policy.max_restarts {
            log::error(
                "task keeps failing; the hub exits so it is restarted",
                &[("task", &name)],
            );
            let _ = inner.exit.send(Some(EXIT_RESTARTS));
            return;
        }
        if started.elapsed() >= policy.max_backoff {
            backoff = policy.first_backoff;
        }
        inner.restarts.fetch_add(1, Ordering::Relaxed);
        tokio::select! {
            () = tokio::time::sleep(backoff) => {}
            _ = shutdown.changed() => return,
        }
        backoff = (backoff * 2).min(policy.max_backoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn quick() -> Policy {
        Policy {
            first_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(40),
            window: Duration::from_secs(60),
            max_restarts: 5,
        }
    }

    #[tokio::test]
    async fn restarts_with_backoff_then_asks_to_exit() {
        let (_stop, stop_rx) = watch::channel(false);
        let (sup, mut exit) = Supervisor::new(quick(), stop_rx);
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&runs);
        let started = Instant::now();
        sup.spawn("flaky", move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>("boom")
            }
        });
        tokio::time::timeout(Duration::from_secs(5), exit.wait_for(Option::is_some))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*exit.borrow(), Some(EXIT_RESTARTS));
        assert_eq!(
            runs.load(Ordering::SeqCst),
            6,
            "five restarts, the sixth failure ends the hub"
        );
        assert_eq!(sup.restarts(), 5);
        // 5 + 10 + 20 + 40 + 40 ms of backoff.
        assert!(started.elapsed() >= Duration::from_millis(115));
    }

    #[tokio::test]
    async fn panics_are_caught_and_counted() {
        let (_stop, stop_rx) = watch::channel(false);
        let (sup, _exit) = Supervisor::new(quick(), stop_rx);
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&runs);
        sup.spawn("panicky", move || {
            let counter = Arc::clone(&counter);
            async move {
                assert!(
                    counter.fetch_add(1, Ordering::SeqCst) > 0,
                    "first run fails"
                );
                std::future::pending::<Result<(), String>>().await
            }
        });
        for _ in 0..100 {
            if runs.load(Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        assert_eq!(sup.panics(), 1);
    }

    #[tokio::test]
    async fn shutdown_stops_restarting() {
        let (stop, stop_rx) = watch::channel(false);
        let (sup, exit) = Supervisor::new(quick(), stop_rx);
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&runs);
        stop.send(true).unwrap();
        sup.spawn("done", move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok::<(), String>(())
            }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(*exit.borrow(), None);
    }
}
