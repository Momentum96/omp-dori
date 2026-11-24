//! The standing lead (COORD-01, COORD-02, HOST-26, HOST-28, HOST-29): one session the hub starts when
//! the quota gate allows, with its resume rules, crash recovery, the session cap and memory admission,
//! and the API other parts use to reach it:
//!
//! - [`Lead::deliver`] stores a message in the lead's inbox and delivers it when the lead can read;
//!   it returns a [`Receipt`] (slice 2b hands owner messages over with it, 2c urgent events).
//! - [`Lead::receipt`] reads a receipt again by request ID.
//!
//! Resume rule (acceptance check 12): the lead resumes its last session (same ID and file) after a
//! clean close; after an unclean stop with a free lease it resumes with a note; with a held lease it
//! is reported and not started. A configured lead session ID must be resumed exactly.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, Notify, broadcast, watch};

use super::inbox::{self, HUB_SENDER, LEAD, Listed, Mailbox, Message, Note, QUOTA_PREFIX, Receipt};
use super::{Control, Role, Runtime, SessionEvent, Snapshot, StartError, StartSpec, Status};
use crate::config::Config;
use crate::jobs::event::{Emit, EventKind, Severity};
use crate::log;
use crate::omp::Omp;
use crate::omp::launch::Target;
use crate::omp::transcript;
use crate::store::Store;
use crate::store::host::{HostWrite, SessionRow, SessionUpdate};
use crate::sys::latency::Latency;
use crate::sys::now_ms;

/// How often the inbox is checked while a written batch waits for its entry.
const CONFIRM_TICK: Duration = Duration::from_secs(5);
/// The sweep that retries anything left (COORD req. 16).
const SWEEP: Duration = Duration::from_mins(5);
/// How often an automatic start that is waiting looks again.
const AUTOSTART_TICK: Duration = Duration::from_secs(30);
/// Bound for a graceful stop at hub shutdown: 15 s, TERM, 5 s, KILL.
pub(crate) const STOP_BUDGET: Duration = Duration::from_secs(22);

/// Builds the runtime for a start from the config in force and the resolved OMP executable.
pub(crate) type Factory<R> = Box<dyn Fn(&Config, &Path) -> R + Send + Sync>;
/// Records a hub event.
pub(crate) type Events = Arc<dyn Fn(Emit) + Send + Sync>;

/// What the lead needs from the hub.
pub(crate) struct Env<R: Runtime> {
    pub(crate) config: watch::Receiver<Arc<Config>>,
    pub(crate) omp: Arc<Omp>,
    /// Whether the quota gate holds new session starts now (OJ-08).
    pub(crate) gate_held: Arc<dyn Fn() -> bool + Send + Sync>,
    pub(crate) events: Events,
    pub(crate) store: Store,
    pub(crate) state_dir: PathBuf,
    pub(crate) make: Factory<R>,
}

/// Where the lead stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    NotStarted,
    Starting,
    Live,
    Stopped,
    /// Not started and not to be started automatically until the owner acts.
    Held,
}

/// `cataly lead status`.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct View {
    pub(crate) phase: Phase,
    pub(crate) reason: Option<String>,
    pub(crate) session: Option<Snapshot>,
    pub(crate) waiting: usize,
    pub(crate) sent: usize,
    pub(crate) in_doubt: usize,
}

/// How the next start goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    Fresh(String),
    Resume {
        id: String,
        file: PathBuf,
        note: Option<String>,
    },
    Refuse(String),
}

/// The lease answer for one session ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lease {
    Free,
    Held,
    Unknown,
}

/// Decides fresh or resume from the newest lead record (HOST req. 81-82, 86-88; check 12).
pub(crate) fn plan(
    last: Option<&SessionRow>,
    required: Option<&str>,
    fresh: bool,
    lease: Lease,
) -> Plan {
    let resumable = last.and_then(|row| {
        let id = row.session_id.clone()?;
        let file = PathBuf::from(row.session_file.clone()?);
        file.is_file().then_some((row, id, file))
    });
    if let Some(required) = required {
        return match &resumable {
            Some((row, id, file)) if id == required => match row.state.as_str() {
                "orphan_held" | "held" if lease != Lease::Free => Plan::Refuse(format!(
                    "the required lead session {required} is held by a running OMP or its lease is unknown"
                )),
                _ => Plan::Resume {
                    id: id.clone(),
                    file: file.clone(),
                    note: unclean(row),
                },
            },
            _ => Plan::Refuse(format!(
                "the required lead session {required} is not the last lead session or its transcript is gone; a fresh lead is never substituted"
            )),
        };
    }
    if fresh {
        return Plan::Fresh("the owner asked for a fresh lead".into());
    }
    let Some((row, id, file)) = resumable else {
        return Plan::Fresh(match last {
            None => "no earlier lead session".into(),
            Some(_) => "the earlier lead has no transcript to resume".into(),
        });
    };
    match row.state.as_str() {
        "stopped" | "lost" => Plan::Resume {
            id,
            file,
            note: unclean(row),
        },
        "orphan_held" | "held" => match lease {
            Lease::Free => Plan::Resume {
                id,
                file,
                note: Some("resumed after an unclean stop; its lease is free again".into()),
            },
            Lease::Held => Plan::Refuse(format!(
                "the earlier lead session {id} is still held by a running OMP; stop it or start fresh"
            )),
            Lease::Unknown => Plan::Refuse(format!(
                "the lease of the earlier lead session {id} is unknown; it is held for the owner"
            )),
        },
        "starting" | "live" => Plan::Refuse(format!(
            "the earlier lead session {id} was not recovered yet"
        )),
        other => Plan::Fresh(format!("the earlier lead ended as {other}")),
    }
}

/// The note for a resume after anything but a clean close.
fn unclean(row: &SessionRow) -> Option<String> {
    match (row.state.as_str(), row.stop_how.as_deref()) {
        ("stopped", Some("clean")) => None,
        (_, how) => Some(format!(
            "resumed after an unclean stop ({})",
            how.unwrap_or("hub stopped")
        )),
    }
}

/// The standing lead.
pub(crate) struct Lead<R: Runtime> {
    env: Env<R>,
    mailbox: Mailbox,
    current: Mutex<Option<Arc<R::Session>>>,
    status: Mutex<(Phase, Option<String>)>,
    starting: AsyncMutex<()>,
    flushing: AsyncMutex<()>,
    wake: Notify,
    recovered: AtomicBool,
    overlay: OnceLock<PathBuf>,
    /// Internal `deliver` call to the prompt line written (DESIGN 2.3 wake, hub part).
    pub(crate) wake_latency: Latency,
    /// Prompt line written to the entry found in the session file.
    pub(crate) confirm_latency: Latency,
}

impl<R: Runtime> Lead<R> {
    pub(crate) fn new(env: Env<R>) -> Arc<Self> {
        let mailbox = Mailbox {
            target: LEAD.into(),
            store: env.store.clone(),
        };
        Arc::new(Self {
            env,
            mailbox,
            current: Mutex::new(None),
            status: Mutex::new((Phase::NotStarted, None)),
            starting: AsyncMutex::new(()),
            flushing: AsyncMutex::new(()),
            wake: Notify::new(),
            recovered: AtomicBool::new(false),
            overlay: OnceLock::new(),
            wake_latency: Latency::default(),
            confirm_latency: Latency::default(),
        })
    }

    fn current(&self) -> Option<Arc<R::Session>> {
        self.current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn phase(&self) -> MutexGuard<'_, (Phase, Option<String>)> {
        self.status.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_phase(&self, phase: Phase, reason: Option<String>) {
        *self.phase() = (phase, reason);
    }

    fn emit(&self, emit: Emit) {
        (self.env.events)(emit);
    }

    /// The supervised task: recovery once, the automatic start when the gate allows, then the inbox.
    pub(crate) async fn run(
        self: Arc<Self>,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), String> {
        let mut last_refusal: Option<String> = None;
        let mut probed = self.env.omp.report_changes();
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            if self.env.omp.report().is_some() {
                if !self.recovered.swap(true, Ordering::AcqRel) {
                    self.recover().await;
                }
                if self.wants_autostart() {
                    match self.start(false).await {
                        Ok(_) => last_refusal = None,
                        Err(reason) if last_refusal.as_deref() != Some(&reason) => {
                            log::info("lead not started yet", &[("reason", &reason)]);
                            last_refusal = Some(reason);
                        }
                        Err(_) => {}
                    }
                }
                let _ = self.flush(None).await;
            }
            let tick = if self.mailbox.unconfirmed().await {
                CONFIRM_TICK
            } else if self.wants_autostart() || self.env.omp.report().is_none() {
                AUTOSTART_TICK.min(SWEEP)
            } else {
                SWEEP
            };
            tokio::select! {
                () = self.wake.notified() => {}
                _ = probed.changed() => {}
                () = tokio::time::sleep(tick) => {}
                _ = shutdown.changed() => {}
            }
        }
    }

    /// An automatic start is due: configured, wanted, nothing running, and not held for the owner.
    fn wants_autostart(&self) -> bool {
        let configured = self
            .env
            .config
            .borrow()
            .lead
            .as_ref()
            .is_some_and(|l| l.autostart);
        configured && self.current().is_none() && matches!(self.phase().0, Phase::NotStarted)
    }

    /// Start-up recovery (HOST req. 85-88): records left starting or live get their lease checked.
    async fn recover(&self) {
        let rows = match self
            .env
            .store
            .read(crate::store::Reads::open_sessions)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                log::warn("session recovery skipped", &[("error", &e)]);
                return;
            }
        };
        for row in rows {
            let update = match &row.session_id {
                None => SessionUpdate {
                    state: Some("failed".into()),
                    stop: Some((
                        "failed".into(),
                        Some("the hub stopped during the start".into()),
                    )),
                    ..SessionUpdate::default()
                },
                Some(id) => match self.lease(id).await {
                    Lease::Free => SessionUpdate {
                        state: Some("lost".into()),
                        stop: Some(("lost".into(), Some("the hub stopped while it ran".into()))),
                        ..SessionUpdate::default()
                    },
                    Lease::Held => {
                        self.emit(
                            Emit::new(EventKind::LeadNotStarted, Severity::Warning, true, "lead")
                                .with(
                                    "reason",
                                    format!("session {id} is still held by a running OMP"),
                                ),
                        );
                        SessionUpdate {
                            state: Some("orphan_held".into()),
                            ..SessionUpdate::default()
                        }
                    }
                    Lease::Unknown => SessionUpdate {
                        state: Some("held".into()),
                        fault: Some("lease unknown at recovery".into()),
                        ..SessionUpdate::default()
                    },
                },
            };
            let update = SessionUpdate {
                handle: row.handle.clone(),
                ..update
            };
            if let Err(e) = self.env.store.host(HostWrite::Session(update)).await {
                log::warn("session recovery not recorded", &[("error", &e)]);
            }
        }
    }

    async fn lease(&self, id: &str) -> Lease {
        let Some(path) = self.env.omp.path() else {
            return Lease::Unknown;
        };
        match crate::omp::usage::lease(&path, &[id]).await {
            Ok(lease) if lease.held.iter().any(|h| h == id) => Lease::Held,
            Ok(lease) if lease.unknown.iter().any(|u| u == id) => Lease::Unknown,
            Ok(_) => Lease::Free,
            Err(e) => {
                log::warn("lease check failed", &[("error", &e)]);
                Lease::Unknown
            }
        }
    }

    /// Why a start must wait now, if it must (HOST req. 89-90; OJ-08; DESIGN 3.4).
    async fn admission(&self, config: &Config) -> Result<PathBuf, String> {
        let report = self.env.omp.report().ok_or("OMP has not been probed yet")?;
        report.sessions()?;
        let exe = self
            .env
            .omp
            .path()
            .ok_or("the OMP executable was not found")?;
        if (self.env.gate_held)() {
            return Err(
                "the quota gate holds new session starts; `cataly quota lift` lifts it".into(),
            );
        }
        let live = usize::from(self.current().is_some_and(|s| s.alive()));
        if live >= usize::try_from(config.sessions.cap).unwrap_or(usize::MAX) {
            return Err(format!(
                "the session cap of {} is reached",
                config.sessions.cap
            ));
        }
        let pressure = config
            .jobs
            .values()
            .find_map(|j| match &j.work {
                crate::config::Work::Owner(crate::config::Owner::Pressure(p)) => Some(p.clone()),
                _ => None,
            })
            .unwrap_or_else(crate::config::owner::Pressure::admission_default);
        let sample = crate::jobs::owner::pressure::sample(&self.env.state_dir).await;
        // Only the memory part of the rule applies to a session start (HOST req. 90); the cap above
        // counts sessions.
        let sample = crate::jobs::owner::pressure::Sample {
            sessions: Some(0),
            disk_free_gb: Some(f64::INFINITY),
            ..sample
        };
        let held = crate::jobs::owner::pressure::admission(&sample, &pressure);
        if !held.is_empty() {
            return Err(format!(
                "memory admission holds the start: {}",
                held.join(", ")
            ));
        }
        Ok(exe)
    }

    /// Starts the lead (COORD req. 2): refused when a lead runs; resume or fresh by [`plan`].
    pub(crate) async fn start(self: &Arc<Self>, fresh: bool) -> Result<View, String> {
        let _one = self.starting.lock().await;
        if self.current().is_some_and(|s| s.alive()) {
            return Err("a lead is already running".into());
        }
        let config = Arc::clone(&self.env.config.borrow());
        let Some(settings) = config.lead.clone() else {
            return Err("no [lead] section in the config".into());
        };
        let exe = self.admission(&config).await?;
        let cwd = settings
            .cwd
            .clone()
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .ok_or("no working directory: set [lead] cwd")?;
        let cwd = std::fs::canonicalize(&cwd).map_err(|e| format!("lead cwd: {e}"))?;
        let (target, expected, note) = self.target(&settings, fresh, &cwd).await?;
        self.overlay(&config)?;
        let handle = super::random_hex(8);
        let now = now_ms();
        let row = SessionRow {
            handle: handle.clone(),
            role: "lead".into(),
            runtime: "headless".into(),
            cwd: cwd.display().to_string(),
            name: "lead".into(),
            model: settings.model.clone(),
            thinking: settings.thinking.clone(),
            mode: if expected.is_some() {
                "resume"
            } else {
                "fresh"
            }
            .into(),
            expected_id: expected.clone(),
            expected_file: match &target {
                Target::Resume(f) => Some(f.display().to_string()),
                Target::Fresh(_) => None,
            },
            state: "starting".into(),
            session_id: None,
            session_file: None,
            pid: None,
            started_ms: now,
            updated_ms: now,
            stop_how: None,
            stop_detail: None,
            stop_ms: None,
            fault: None,
        };
        // Record first, then spawn (HOST req. 6).
        self.env
            .store
            .host(HostWrite::Record(row))
            .await
            .map_err(|e| e.to_string())?;
        self.set_phase(Phase::Starting, None);
        let runtime = (self.env.make)(&config, &exe);
        let spec = StartSpec {
            handle: handle.clone(),
            role: Role::Lead,
            cwd,
            model: settings.model.clone(),
            thinking: settings.thinking.clone(),
            target,
            expected_id: expected.clone(),
            instructions: settings.instructions.clone(),
        };
        match runtime.start(spec).await {
            Ok((session, started)) => {
                self.bound(handle, session, &started, expected.is_some(), note)
                    .await;
                Ok(self.view().await)
            }
            Err(e) => Err(self.not_started(handle, &e).await),
        }
    }

    /// Fresh or resume target for the next start, from the newest lead record.
    async fn target(
        &self,
        settings: &crate::config::session::LeadSettings,
        fresh: bool,
        cwd: &Path,
    ) -> Result<(Target, Option<String>, Option<String>), String> {
        let last = self
            .env
            .store
            .read(|r| r.latest_session("lead"))
            .await
            .map_err(|e| e.to_string())?;
        let held = last
            .as_ref()
            .filter(|r| r.state == "orphan_held" || r.state == "held")
            .and_then(|r| r.session_id.clone());
        let lease = match held {
            Some(id) => self.lease(&id).await,
            None => Lease::Unknown,
        };
        // Both levels owner-only: the resume check tolerates OMP's group-writable files only below a
        // 0700 parent (`omp::transcript::exact`).
        let sessions = self.env.state_dir.join("sessions");
        crate::sys::fs::ensure_private_dir(&sessions).map_err(|e| e.to_string())?;
        let dir = sessions.join("lead");
        crate::sys::fs::ensure_private_dir(&dir).map_err(|e| e.to_string())?;
        match plan(last.as_ref(), settings.session_id.as_deref(), fresh, lease) {
            Plan::Refuse(reason) => {
                self.set_phase(Phase::Held, Some(reason.clone()));
                self.emit(
                    Emit::new(EventKind::LeadNotStarted, Severity::Warning, true, "lead")
                        .with("reason", &reason),
                );
                Err(reason)
            }
            Plan::Fresh(reason) => {
                log::info("lead starts fresh", &[("reason", &reason)]);
                Ok((Target::Fresh(dir), None, None))
            }
            Plan::Resume { id, file, note } => {
                let verified =
                    transcript::verify(file.clone(), dir.clone(), id.clone(), cwd.to_path_buf())
                        .await;
                if matches!(verified, Ok(Some(_))) {
                    return Ok((Target::Resume(file), Some(id), note));
                }
                let reason = format!(
                    "the transcript of lead session {id} did not verify for an exact resume"
                );
                if settings.session_id.is_some() {
                    self.set_phase(Phase::Held, Some(reason.clone()));
                    return Err(reason);
                }
                log::warn("lead starts fresh", &[("reason", &reason)]);
                Ok((Target::Fresh(dir), None, None))
            }
        }
    }

    /// A start bound its identity: record it, follow its events, leave the resume note.
    async fn bound(
        self: &Arc<Self>,
        handle: String,
        session: Arc<R::Session>,
        started: &super::Started,
        resumed: bool,
        note: Option<String>,
    ) {
        self.update(SessionUpdate {
            handle: handle.clone(),
            state: Some("live".into()),
            session_id: Some(started.session_id.clone()),
            session_file: Some(started.file.clone()),
            pid: started.pid.map(i64::from),
            fault: started.held.clone(),
            stop: None,
        })
        .await;
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&session));
        self.set_phase(Phase::Live, started.held.clone());
        tokio::spawn(Arc::clone(self).watch(handle, session.subscribe()));
        self.emit(
            Emit::new(EventKind::LeadStarted, Severity::Info, false, "lead")
                .with("session", &started.session_id)
                .with("model", started.model.as_deref().unwrap_or("unknown"))
                .with("how", if resumed { "resumed" } else { "fresh" }),
        );
        if let Some(reason) = &started.held {
            self.emit(
                Emit::new(EventKind::SessionFenced, Severity::Warning, true, "lead")
                    .with("role", "lead")
                    .with("reason", reason),
            );
        }
        if let Some(note) = note {
            let _ = self
                .mailbox
                .admit(&Message {
                    sender: HUB_SENDER.into(),
                    text: format!("[resume note] {note}. Check the last turns before going on."),
                    urgent: false,
                    owner: false,
                    identity: Some(format!("resume-note:{}", started.session_id)),
                    request_id: None,
                })
                .await;
        }
        self.wake.notify_one();
    }

    /// A start that did not bind: the record says why, and the owner hears it.
    async fn not_started(&self, handle: String, e: &StartError) -> String {
        let detail = e.to_string();
        self.update(SessionUpdate {
            handle,
            state: Some("failed".into()),
            stop: Some(("failed".into(), Some(detail.clone()))),
            ..SessionUpdate::default()
        })
        .await;
        let phase = match e {
            StartError::Refused(_) => Phase::NotStarted,
            StartError::Failed(_) => Phase::Held,
        };
        self.set_phase(phase, Some(detail.clone()));
        self.emit(
            Emit::new(EventKind::LeadNotStarted, Severity::Warning, true, "lead")
                .with("reason", &detail),
        );
        detail
    }

    /// Writes the overlay once per hub run (DESIGN 4.2).
    fn overlay(&self, config: &Config) -> Result<PathBuf, String> {
        if let Some(path) = self.overlay.get() {
            return Ok(path.clone());
        }
        let path = self.env.state_dir.join("omp-overlay.yml");
        crate::omp::launch::write_overlay(&path, &config.fallback).map_err(|e| e.to_string())?;
        Ok(self.overlay.get_or_init(|| path).clone())
    }

    async fn update(&self, update: SessionUpdate) {
        if let Err(e) = self.env.store.host(HostWrite::Session(update)).await {
            log::warn("session record not updated", &[("error", &e)]);
        }
    }

    /// Follows one session's events: hub events, inbox wakes and the stop record.
    async fn watch(
        self: Arc<Self>,
        handle: String,
        mut events: broadcast::Receiver<(u64, SessionEvent)>,
    ) {
        loop {
            let event = match events.recv().await {
                Ok((_, event)) => event,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    self.wake.notify_one();
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            };
            match event {
                SessionEvent::TurnEnded { .. }
                | SessionEvent::Quiet
                | SessionEvent::Settled { .. }
                | SessionEvent::Message { .. } => self.wake.notify_one(),
                SessionEvent::Fallback { from, to, reason } => self.emit(
                    Emit::new(EventKind::ModelFallback, Severity::Info, false, "lead")
                        .with("role", "lead")
                        .with("from", from)
                        .with("to", to)
                        .with("reason", reason),
                ),
                SessionEvent::FallbackRecovered { model } => self.emit(
                    Emit::new(EventKind::ModelRestored, Severity::Info, false, "lead")
                        .with("role", "lead")
                        .with("model", model),
                ),
                SessionEvent::SpendOffer { title, count } => self.emit(
                    Emit::new(EventKind::SpendOffer, Severity::Warning, true, "lead")
                        .with("role", "lead")
                        .with("dialog", title)
                        .with("count", count),
                ),
                SessionEvent::QuotaBlocked { message } => {
                    self.emit(
                        Emit::new(EventKind::QuotaBlocked, Severity::Warning, true, "lead")
                            .with("role", "lead")
                            .with("message", &message),
                    );
                    // The lead is told once (OPC-58, OPC-64).
                    let _ = self
                        .mailbox
                        .admit(&Message {
                            sender: HUB_SENDER.into(),
                            text: format!(
                                "[quota exhausted] {message}. OMP's configured fallback is exhausted or absent. \
                                 Stop, save progress and report; only owner requests reach you until it clears."
                            ),
                            urgent: false,
                            owner: false,
                            identity: Some(format!("{QUOTA_PREFIX}{handle}:{}", now_ms() / 3_600_000)),
                            request_id: None,
                        })
                        .await;
                }
                SessionEvent::Fault { reason } => {
                    self.update(SessionUpdate {
                        handle: handle.clone(),
                        fault: Some(reason.clone()),
                        ..SessionUpdate::default()
                    })
                    .await;
                    self.emit(
                        Emit::new(EventKind::SessionFenced, Severity::Warning, true, "lead")
                            .with("role", "lead")
                            .with("reason", reason),
                    );
                }
                SessionEvent::Stopped { record } => {
                    self.update(SessionUpdate {
                        handle: handle.clone(),
                        state: Some("stopped".into()),
                        stop: Some((record.how.clone(), record.detail.clone())),
                        ..SessionUpdate::default()
                    })
                    .await;
                    if record.how == "exited" {
                        self.emit(
                            Emit::new(EventKind::SessionStopped, Severity::Warning, true, "lead")
                                .with("role", "lead")
                                .with("how", &record.how)
                                .with("detail", record.detail.as_deref().unwrap_or("")),
                        );
                    }
                    self.set_phase(Phase::Stopped, Some(format!("stopped: {}", record.how)));
                    self.wake.notify_one();
                    return;
                }
                SessionEvent::TurnStarted
                | SessionEvent::DialogOpened { .. }
                | SessionEvent::DialogClosed { .. }
                | SessionEvent::ModelChanged { .. }
                | SessionEvent::InterruptUnconfirmed => {}
            }
        }
    }

    /// Resolves written batches and sends what may go (one flush at a time).
    async fn flush(&self, called: Option<Instant>) -> Result<(), String> {
        let _one = self.flushing.lock().await;
        let session = self.current();
        let blocked = session
            .as_ref()
            .is_some_and(|s| s.snapshot().quota_blocked.is_some());
        let notes = self
            .mailbox
            .flush(session.as_deref(), blocked)
            .await
            .map_err(|e| e.to_string())?;
        for note in notes {
            match note {
                Note::Sent { written } => {
                    if let (Some(called), Some(written)) = (called, written) {
                        self.wake_latency
                            .record(written.saturating_duration_since(called));
                    }
                }
                Note::Confirmed { after_ms } => {
                    self.confirm_latency
                        .record(Duration::from_millis(u64::try_from(after_ms).unwrap_or(0)));
                }
                Note::InDoubt {
                    delivery,
                    rows,
                    reason,
                    notify,
                } => {
                    log::warn(
                        "delivery in doubt",
                        &[("delivery", &delivery), ("reason", &reason)],
                    );
                    self.emit(
                        Emit::new(EventKind::DeliveryInDoubt, Severity::Warning, true, "lead")
                            .with("target", LEAD)
                            .with("count", rows.len())
                            .with("reason", &reason),
                    );
                    if notify {
                        let _ = self
                            .mailbox
                            .admit(&Message {
                                sender: HUB_SENDER.into(),
                                text: inbox::in_doubt_notice(LEAD, &rows, &reason),
                                urgent: true,
                                owner: false,
                                identity: Some(inbox::in_doubt_identity(&delivery)),
                                request_id: None,
                            })
                            .await;
                    }
                }
                Note::Failed {
                    delivery,
                    rows,
                    reason,
                } => {
                    log::warn(
                        "delivery failed",
                        &[("delivery", &delivery), ("reason", &reason)],
                    );
                    self.emit(
                        Emit::new(EventKind::DeliveryFailed, Severity::Warning, true, "lead")
                            .with("target", LEAD)
                            .with("count", rows)
                            .with("reason", &reason),
                    );
                }
            }
        }
        Ok(())
    }

    /// Hands a message to the lead (COORD-10, COORD-15): stored, then delivered at once when the lead
    /// can read. Accepted even with no lead session; it goes out once a lead exists (COORD req. 24).
    pub(crate) async fn deliver(&self, request: Message) -> Result<Receipt, String> {
        let called = Instant::now();
        let (receipt, stored) = self.mailbox.admit(&request).await?;
        let Some(id) = stored else {
            return Ok(receipt);
        };
        if let Err(e) = self.flush(Some(called)).await {
            log::warn("lead inbox flush failed", &[("error", &e)]);
        }
        self.wake.notify_one();
        Ok(self.mailbox.receipt(request.request_id, id).await)
    }

    /// The receipt of an earlier request ID; `None` when the ID is unknown.
    pub(crate) async fn receipt(&self, request_id: &str) -> Result<Option<Receipt>, String> {
        let rid = request_id.to_string();
        let found = self
            .env
            .store
            .read(move |r| r.receipt(LEAD, &rid))
            .await
            .map_err(|e| e.to_string())?;
        Ok(match found {
            None => None,
            Some(crate::store::host::StoredReceipt { inbox_id: None }) => Some(Receipt {
                request_id: Some(request_id.into()),
                inbox_id: None,
                state: inbox::ReceiptState::Duplicate,
                delivery_id: None,
                reason: None,
            }),
            Some(crate::store::host::StoredReceipt { inbox_id: Some(id) }) => {
                Some(self.mailbox.receipt(Some(request_id.into()), id).await)
            }
        })
    }

    pub(crate) async fn view(&self) -> View {
        let (phase, reason) = self.phase().clone();
        let entries = self.mailbox.list().await.unwrap_or_default();
        let count = |s: &str| entries.iter().filter(|(l, _)| l.state == s).count();
        View {
            phase,
            reason,
            session: self.current().map(|s| (*s.snapshot()).clone()),
            waiting: count("pending"),
            sent: count("sent"),
            in_doubt: count("in_doubt"),
        }
    }

    pub(crate) async fn inbox(&self) -> Result<Vec<Listed>, String> {
        Ok(self
            .mailbox
            .list()
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|(l, _)| l)
            .collect())
    }

    pub(crate) async fn settle(&self, index: usize, confirm: bool) -> Result<(), String> {
        self.mailbox.settle(index, confirm).await?;
        self.wake.notify_one();
        Ok(())
    }

    fn live(&self) -> Result<Arc<R::Session>, String> {
        self.current()
            .filter(|s| s.alive())
            .ok_or_else(|| "no lead session is running".to_string())
    }

    pub(crate) async fn interrupt(&self) -> Result<String, String> {
        self.live()?.interrupt().await
    }

    pub(crate) async fn set_model(&self, selector: &str) -> Result<String, String> {
        self.live()?.set_model(selector).await
    }

    pub(crate) fn accept_model(&self) -> Result<(), String> {
        self.live()?.accept_model()?;
        self.set_phase(Phase::Live, None);
        self.wake.notify_one();
        Ok(())
    }

    pub(crate) async fn answer(&self, dialog: &str, answer: &Value) -> Result<(), String> {
        let session = self.live()?;
        let id = session
            .snapshot()
            .session_id
            .clone()
            .ok_or("the lead is not bound")?;
        session.answer(&id, dialog, answer).await
    }

    pub(crate) fn events(&self, after: u64) -> Result<super::EventPage, String> {
        Ok(self
            .current()
            .ok_or("no lead session")?
            .events_after(after, super::headless::EVENT_PAGE))
    }

    /// Stops the lead; refused while it works or has a dialog unless forced (HOST req. 83).
    pub(crate) async fn stop(&self, force: bool) -> Result<View, String> {
        let session = self.live()?;
        let snapshot = session.snapshot();
        if !force && (snapshot.status == Status::Working || !snapshot.dialogs.is_empty()) {
            return Err("the lead is working or has a pending dialog; use --force".into());
        }
        let handle = snapshot.handle.clone();
        let record = session.close(force).await?;
        // Written before the answer, so an immediate start sees the stop record.
        self.update(SessionUpdate {
            handle,
            state: Some("stopped".into()),
            stop: Some((record.how.clone(), record.detail.clone())),
            ..SessionUpdate::default()
        })
        .await;
        self.set_phase(Phase::Stopped, Some(format!("stopped: {}", record.how)));
        Ok(self.view().await)
    }

    /// Hub shutdown: a clean close so the next start resumes the same session.
    pub(crate) async fn shutdown(&self) {
        let Some(session) = self.current().filter(|s| s.alive()) else {
            return;
        };
        let handle = session.snapshot().handle.clone();
        match tokio::time::timeout(STOP_BUDGET + Duration::from_secs(5), session.close(true)).await
        {
            Ok(Ok(record)) => {
                self.update(SessionUpdate {
                    handle,
                    state: Some("stopped".into()),
                    stop: Some((record.how.clone(), record.detail.clone())),
                    ..SessionUpdate::default()
                })
                .await;
            }
            Ok(Err(e)) => log::warn("lead not closed at shutdown", &[("error", &e)]),
            Err(_) => log::warn("lead close timed out at shutdown", &[]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(state: &str, how: Option<&str>, file: Option<&Path>) -> SessionRow {
        SessionRow {
            handle: "h".into(),
            role: "lead".into(),
            runtime: "headless".into(),
            cwd: "/".into(),
            name: "lead".into(),
            model: "p/m".into(),
            thinking: None,
            mode: "fresh".into(),
            expected_id: None,
            expected_file: None,
            state: state.into(),
            session_id: Some("s1".into()),
            session_file: file.map(|f| f.display().to_string()),
            pid: None,
            started_ms: 1,
            updated_ms: 1,
            stop_how: how.map(Into::into),
            stop_detail: None,
            stop_ms: None,
            fault: None,
        }
    }

    /// Acceptance check 12: clean close resumes; unclean with a free lease resumes with a note; a held
    /// lease is reported and not started; a required ID is never replaced by a fresh lead.
    #[test]
    fn resume_rules() {
        let dir = crate::sys::fs::tests::scratch("lead-plan");
        let file = dir.join("s.jsonl");
        std::fs::write(&file, "{}\n").unwrap();
        let resume = |note: Option<&str>| Plan::Resume {
            id: "s1".into(),
            file: file.clone(),
            note: note.map(Into::into),
        };
        assert!(matches!(
            plan(None, None, false, Lease::Free),
            Plan::Fresh(_)
        ));
        let clean = row("stopped", Some("clean"), Some(&file));
        assert_eq!(
            plan(Some(&clean), None, false, Lease::Unknown),
            resume(None)
        );
        assert!(matches!(
            plan(Some(&clean), None, true, Lease::Unknown),
            Plan::Fresh(_)
        ));
        let crashed = row("stopped", Some("exited"), Some(&file));
        assert_eq!(
            plan(Some(&crashed), None, false, Lease::Unknown),
            resume(Some("resumed after an unclean stop (exited)"))
        );
        let lost = row("lost", Some("lost"), Some(&file));
        assert!(matches!(
            plan(Some(&lost), None, false, Lease::Unknown),
            Plan::Resume { note: Some(_), .. }
        ));
        let held = row("orphan_held", None, Some(&file));
        assert!(matches!(
            plan(Some(&held), None, false, Lease::Held),
            Plan::Refuse(_)
        ));
        assert!(matches!(
            plan(Some(&held), None, false, Lease::Unknown),
            Plan::Refuse(_)
        ));
        assert!(matches!(
            plan(Some(&held), None, false, Lease::Free),
            Plan::Resume { note: Some(_), .. }
        ));
        let gone = row("stopped", Some("clean"), Some(&dir.join("missing.jsonl")));
        assert!(matches!(
            plan(Some(&gone), None, false, Lease::Free),
            Plan::Fresh(_)
        ));
        assert_eq!(
            plan(Some(&clean), Some("s1"), true, Lease::Free),
            resume(None)
        );
        assert!(matches!(
            plan(Some(&clean), Some("s2"), false, Lease::Free),
            Plan::Refuse(_)
        ));
        assert!(matches!(
            plan(Some(&gone), Some("s1"), false, Lease::Free),
            Plan::Refuse(_)
        ));
    }
}
