//! What the intake of every relay shares (DESIGN 5.1): health for `cataly <relay> status` (MSG
//! req. 15), the supervised start-serve-stop loop, and the handling round that drains the durable
//! arrival queue through the pipeline and posts answers, driven by the lead's events. A relay brings
//! its live source (Gateway, long poll, Socket Mode) and its start-up checks; one stalled relay never
//! delays another, since each runs in its own task with its own queue (MSG req. 2).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::{Notify, broadcast, watch};

use super::Relay;
use super::pipeline::{Arrival, Pipeline, Step};
use crate::config::Config;
use crate::host::lead::{Events, Lead};
use crate::host::{Runtime, SessionEvent};
use crate::jobs::event::{Emit, EventKind, Severity};
use crate::log;
use crate::store::Store;
use crate::store::relay::{Arrived, QueueRow, RelayDone, RelayWrite};
use crate::sys::latency::Latency;
use crate::sys::now_ms;

/// A failed start that may pass is tried again after this long.
pub(crate) const RETRY_START: Duration = Duration::from_secs(30);
/// The handling round runs at least this often while work is open.
const TICK: Duration = Duration::from_secs(3);
const IDLE_TICK: Duration = Duration::from_secs(30);

/// Intake health (DISC-IN req. 60, MSG req. 15).
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct Health {
    /// `off`, `starting`, `connecting`, `connected`, `unhealthy`, `stopped`.
    pub(crate) state: String,
    pub(crate) reason: Option<String>,
    /// The relay's own account: the bot, or workspace and member.
    pub(crate) bot: Option<String>,
    pub(crate) owner: Option<String>,
    pub(crate) connected_ms: Option<i64>,
    /// The catch-up read owed for this connection finished.
    pub(crate) caught_up: bool,
    pub(crate) buttons: bool,
    /// Followed Discord threads; remembered Slack threads.
    pub(crate) followed_threads: usize,
    pub(crate) last_event_ms: Option<i64>,
    pub(crate) last_error: Option<String>,
}

/// What the hub keeps of one relay's intake: health, the restart signal and the measurements.
#[derive(Default)]
pub(crate) struct Shared {
    health: Mutex<Health>,
    /// Restarts intake (after a reset or a release).
    pub(crate) kick: Notify,
    /// Frame or poll response read to OMP `agent_start` (DESIGN 2.3).
    pub(crate) wake_latency: Latency,
    /// Connection established to its catch-up read finished.
    pub(crate) catch_up_latency: Latency,
    /// Receive time minus the platform's message time (TELE-07).
    pub(crate) receive_lag: Latency,
}

impl Shared {
    pub(crate) fn health(&self) -> Health {
        self.lock().clone()
    }

    fn lock(&self) -> MutexGuard<'_, Health> {
        self.health.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn set(&self, f: impl FnOnce(&mut Health)) {
        f(&mut self.lock());
    }
}

/// Why a start or a run ended.
pub(crate) enum Stop {
    /// Waits for a config change or the owner (bad token, binding, permission).
    Fatal(String),
    /// May pass by itself (network); tried again after 30 s.
    Retry(String),
    /// The config changed or the owner asked: start again now.
    Again,
    Shutdown,
}

/// The supervised intake task of one relay: runs while `off` finds nothing missing in the config;
/// fatal conditions wait for the owner instead of ending the task (DISC-IN req. 1-3, 20).
pub(crate) async fn supervise<F, Fut>(
    name: &'static str,
    shared: Arc<Shared>,
    mut config_rx: watch::Receiver<Arc<Config>>,
    mut shutdown: watch::Receiver<bool>,
    events: Events,
    off: impl Fn(&Config) -> Option<String>,
    mut attempt: F,
) -> Result<(), String>
where
    F: FnMut(Arc<Config>) -> Fut,
    Fut: Future<Output = Stop>,
{
    let mut reported: Option<String> = None;
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        let config = Arc::clone(&config_rx.borrow_and_update());
        let wait = if let Some(reason) = off(&config) {
            shared.set(|h| {
                *h = Health {
                    state: "off".into(),
                    reason: Some(reason),
                    ..Health::default()
                };
            });
            None
        } else {
            shared.set(|h| h.state = "starting".into());
            match attempt(config).await {
                Stop::Shutdown => return Ok(()),
                Stop::Again => continue,
                Stop::Fatal(reason) | Stop::Retry(reason)
                    if reported.as_deref() == Some(reason.as_str()) =>
                {
                    shared.set(|h| h.state = "stopped".into());
                    Some(RETRY_START)
                }
                Stop::Fatal(reason) => {
                    stopped(name, &shared, &events, &reason);
                    reported = Some(reason);
                    None
                }
                Stop::Retry(reason) => {
                    stopped(name, &shared, &events, &reason);
                    reported = Some(reason);
                    Some(RETRY_START)
                }
            }
        };
        tokio::select! {
            _ = config_rx.changed() => {}
            () = shared.kick.notified() => {}
            () = async {
                match wait {
                    Some(w) => tokio::time::sleep(w).await,
                    None => std::future::pending().await,
                }
            } => {}
            _ = shutdown.changed() => {}
        }
    }
}

fn stopped(name: &'static str, shared: &Shared, events: &Events, reason: &str) {
    log::warn(
        "relay intake stopped",
        &[("relay", &name), ("reason", &reason)],
    );
    shared.set(|h| {
        h.state = "stopped".into();
        h.reason = Some(reason.to_string());
        h.caught_up = false;
    });
    events(
        Emit::new(EventKind::RelayStopped, Severity::Warning, true, name)
            .with("relay", name)
            .with("reason", reason),
    );
}

/// Binds the relay's state to (account, owner), refusing state that belongs to another pair
/// (DISC-IN req. 3; TELE req. 17; SLACK-03).
pub(crate) async fn bind(
    store: &Store,
    relay: &str,
    account: &str,
    owner: &str,
) -> Result<(), Stop> {
    let bound = store
        .relay(RelayWrite::Bind {
            relay: relay.into(),
            account: account.into(),
            owner: owner.into(),
        })
        .await
        .map_err(|e| Stop::Retry(e.to_string()))?;
    if let RelayDone::Bound(Some((account, was))) = bound {
        return Err(Stop::Fatal(format!(
            "intake state belongs to account {account} and owner {was}; `cataly {relay} reset` starts afresh"
        )));
    }
    Ok(())
}

/// The handling round of a relay without questions (Telegram, Slack): queue, answers, lead events.
pub(crate) struct Round<R: Relay, T: Runtime> {
    pub(crate) pipeline: Arc<Pipeline<R, T>>,
    store: Store,
    lead: Arc<Lead<T>>,
    pub(crate) wake: Notify,
    turn_running: AtomicBool,
    /// A turn ended (no continuation) since the last turn start.
    turn_ended: AtomicBool,
}

impl<R: Relay, T: Runtime> Round<R, T> {
    pub(crate) fn new(pipeline: Arc<Pipeline<R, T>>, store: Store, lead: Arc<Lead<T>>) -> Self {
        Self {
            pipeline,
            store,
            lead,
            wake: Notify::new(),
            turn_running: AtomicBool::new(false),
            turn_ended: AtomicBool::new(false),
        }
    }

    /// Queues an owner message durably (MSG req. 4): its position moves only after hand-off.
    /// `read_at` is when its frame or poll response was read, for the wake latency.
    pub(crate) async fn arrive(
        &self,
        room: &str,
        message: &str,
        seq: i64,
        parent: Option<&str>,
        raw: String,
        read_at: Instant,
    ) -> Result<Arrived, String> {
        let arrival = Arrival {
            room: room.into(),
            message: message.into(),
            seq,
            parent: parent.map(str::to_string),
            raw,
        };
        self.pipeline
            .arrive(arrival, Some(read_at), &self.wake)
            .await
    }

    /// Runs rounds until `source` ends, the config's `settings` change, or the owner or a shutdown
    /// stops them.
    pub(crate) async fn serve<S: PartialEq>(
        self: &Arc<Self>,
        mut source: tokio::sync::oneshot::Receiver<Stop>,
        mut config: watch::Receiver<Arc<Config>>,
        settings: impl Fn(&Config) -> S,
        kick: &Notify,
        mut shutdown: watch::Receiver<bool>,
    ) -> Stop {
        let mut events = self.lead.follow();
        let current = settings(&config.borrow());
        loop {
            self.round().await;
            let tick = if self.busy().await { TICK } else { IDLE_TICK };
            tokio::select! {
                () = self.wake.notified() => {}
                event = events.recv() => match event {
                    Ok(event) => self.on_event(&event),
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => events = self.lead.follow(),
                },
                () = tokio::time::sleep(tick) => {}
                end = &mut source => return end.unwrap_or(Stop::Shutdown),
                _ = config.changed() => {
                    if settings(&config.borrow_and_update()) != current {
                        return Stop::Again;
                    }
                }
                () = kick.notified() => return Stop::Again,
                _ = shutdown.changed() => return Stop::Shutdown,
            }
        }
    }

    async fn busy(&self) -> bool {
        self.turn_running.load(Ordering::Acquire)
            || self
                .store
                .read(|r| Ok(!r.queue(R::NAME)?.is_empty() || !r.answers_open(R::NAME)?.is_empty()))
                .await
                .unwrap_or(true)
    }

    fn on_event(&self, event: &SessionEvent) {
        self.pipeline.on_event(event);
        match event {
            SessionEvent::TurnStarted => {
                self.turn_running.store(true, Ordering::Release);
                self.turn_ended.store(false, Ordering::Release);
            }
            SessionEvent::TurnEnded {
                will_continue: false,
            }
            | SessionEvent::Settled { .. }
            | SessionEvent::Quiet => {
                self.turn_running.store(false, Ordering::Release);
                self.turn_ended.store(true, Ordering::Release);
                self.wake.notify_one();
            }
            SessionEvent::Stopped { .. } => self.wake.notify_one(),
            _ => {}
        }
    }

    async fn round(self: &Arc<Self>) {
        let me = Arc::clone(self);
        let drained = self
            .pipeline
            .drain(move |row| {
                let me = Arc::clone(&me);
                async move { me.handle(&row).await }
            })
            .await;
        if let Err(e) = drained {
            log::warn(
                "relay queue not drained",
                &[("relay", &R::NAME), ("error", &e)],
            );
        }
        let turn_over =
            !self.turn_running.load(Ordering::Acquire) && self.turn_ended.load(Ordering::Acquire);
        if let Err(e) = self.pipeline.answers(turn_over).await {
            log::warn(
                "relay answers not posted",
                &[("relay", &R::NAME), ("error", &e)],
            );
        }
    }

    async fn handle(&self, row: &QueueRow) -> Result<Step, String> {
        let read_at = self.pipeline.read_at(&row.room, &row.message);
        let inbound = match self.pipeline.relay.normalise(row) {
            Ok(i) => i,
            Err(fault) => {
                log::warn(
                    "owner message unreadable; skipped",
                    &[
                        ("relay", &R::NAME),
                        ("message", &row.message),
                        ("fault", &fault),
                    ],
                );
                self.pipeline.finish(&row.room, &row.message, None).await?;
                return Ok(Step::Done);
            }
        };
        if let Some(step) = self.pipeline.resume(row, &inbound, read_at).await? {
            return Ok(step);
        }
        self.pipeline.hand_off(row, &inbound, read_at, None).await
    }
}

/// Marks an event seen now in a relay's health.
pub(crate) fn touched(shared: &Shared) {
    shared.set(|h| h.last_event_ms = Some(now_ms()));
}

/// What a relay's intake needs from the hub (Telegram, Slack).
pub(crate) struct Env<T: Runtime> {
    pub(crate) config: watch::Receiver<Arc<Config>>,
    pub(crate) store: Store,
    pub(crate) lead: Arc<Lead<T>>,
    pub(crate) lanes: Arc<crate::lanes::Lanes<T>>,
    pub(crate) events: Events,
    pub(crate) spill_dir: std::path::PathBuf,
    /// Token files default to `secrets/` beside it.
    pub(crate) config_file: std::path::PathBuf,
}

impl<T: Runtime> Env<T> {
    pub(crate) fn parts(&self) -> super::pipeline::Parts<T> {
        super::pipeline::Parts {
            store: self.store.clone(),
            lead: Arc::clone(&self.lead),
            lanes: Arc::clone(&self.lanes),
            spill_dir: self.spill_dir.clone(),
            events: Arc::clone(&self.events),
            config: self.config.clone(),
        }
    }
}

/// A relay's token file: the configured path, else `<config dir>/secrets/<name>`.
pub(crate) fn secret_path(
    configured: Option<&std::path::PathBuf>,
    config_file: &std::path::Path,
    name: &str,
) -> std::path::PathBuf {
    configured.cloned().unwrap_or_else(|| {
        config_file
            .parent()
            .unwrap_or(std::path::Path::new("/"))
            .join("secrets")
            .join(name)
    })
}

/// Reads an owner-only token file (SLACK req. 35): missing, empty, foreign-owned, too open or not a
/// regular file is refused, and the error names only the path.
pub(crate) fn read_secret(path: &std::path::Path) -> Result<String, String> {
    match crate::sys::fs::read_private(path, 64 * 1024) {
        Ok(Some(bytes)) => {
            let text =
                String::from_utf8(bytes).map_err(|_| format!("{} is not text", path.display()))?;
            let text = text.trim().to_string();
            if text.is_empty() {
                Err(format!("{} is empty", path.display()))
            } else {
                Ok(text)
            }
        }
        Ok(None) => Err(format!("no token file at {}", path.display())),
        Err(e) => Err(format!("token file {}: {e}", path.display())),
    }
}
