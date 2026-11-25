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
//! is reported and not started. A configured lead session ID must be resumed exactly. When its OMP
//! exits by itself the lead is restarted on the same session with backoff, and held for the owner
//! after more than 5 restarts in 10 minutes (DESIGN 2.2).

use std::collections::VecDeque;
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
    /// Whether the bridge link in OMP's extension folder is current (DESIGN 4.2, 12.2 C19).
    pub(crate) bridge_link: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
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
    /// Pending entries that start a turn (urgent or the owner's).
    pub(crate) waiting: usize,
    /// Pending quiet notes; by design they wait for the next wake.
    pub(crate) notes: usize,
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

/// Automatic restarts after the lead's child exited by itself, by the supervision rule of DESIGN 2.2:
/// 1 s doubling to 60 s; more than 5 within 10 minutes hold the lead until the owner starts it.
const RESTART_FIRST: Duration = Duration::from_secs(1);
const RESTART_MAX: Duration = Duration::from_secs(60);
const RESTART_WINDOW: Duration = Duration::from_mins(10);
const RESTART_LIMIT: usize = 5;

#[derive(Default)]
struct Restarts {
    /// When each restart within the window was planned.
    recent: VecDeque<Instant>,
    /// The next restart: when, and the session (ID, transcript) it resumes.
    due: Option<(Instant, (String, PathBuf))>,
}

impl Restarts {
    /// Plans the next restart: its delay and its number within the window, or `None` at the limit.
    fn plan(&mut self, now: Instant, session: (String, PathBuf)) -> Option<(Duration, usize)> {
        while self
            .recent
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= RESTART_WINDOW)
        {
            self.recent.pop_front();
        }
        if self.recent.len() >= RESTART_LIMIT {
            self.due = None;
            return None;
        }
        let doublings = u32::try_from(self.recent.len()).unwrap_or(u32::MAX);
        let delay = RESTART_FIRST
            .saturating_mul(2u32.saturating_pow(doublings))
            .min(RESTART_MAX);
        self.recent.push_back(now);
        self.due = Some((now + delay, session));
        Some((delay, self.recent.len()))
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
    restarts: Mutex<Restarts>,
    /// The hub asked the current child to close, so its end is no exit by itself.
    closing: AtomicBool,
    /// Internal `deliver` call to the prompt line written (DESIGN 2.3 wake, hub part).
    pub(crate) wake_latency: Latency,
    /// Prompt line written to the entry found in the session file.
    pub(crate) confirm_latency: Latency,
    /// Every event of the current session, for the relays (answers, typing, questions).
    followers: broadcast::Sender<SessionEvent>,
    /// When the lead last read a turn start, before its followers see it (the relays' wake end).
    turn_started: Mutex<Option<Instant>>,
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
            restarts: Mutex::new(Restarts::default()),
            closing: AtomicBool::new(false),
            wake_latency: Latency::default(),
            confirm_latency: Latency::default(),
            followers: broadcast::channel(256).0,
            turn_started: Mutex::new(None),
        })
    }

    fn current(&self) -> Option<Arc<R::Session>> {
        self.current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Why the lead may not be stopped now, if it may not.
    pub(crate) fn busy(&self) -> Option<&'static str> {
        self.current().and_then(|s| s.snapshot().busy())
    }

    fn phase(&self) -> MutexGuard<'_, (Phase, Option<String>)> {
        self.status.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_phase(&self, phase: Phase, reason: Option<String>) {
        *self.phase() = (phase, reason);
    }

    fn restarts(&self) -> MutexGuard<'_, Restarts> {
        self.restarts.lock().unwrap_or_else(PoisonError::into_inner)
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
                if self.restart_wait() == Some(Duration::ZERO) {
                    self.restart().await;
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
            // A batch that found the lead between states (a turn just ending) goes when the lead
            // turns idle (see `watch`); the confirm tick is only the backstop.
            let live = self.current().is_some_and(|s| s.alive());
            let mut tick = if self.mailbox.unconfirmed().await
                || (live && self.mailbox.waking_pending().await)
            {
                CONFIRM_TICK
            } else if self.wants_autostart() || self.env.omp.report().is_none() {
                AUTOSTART_TICK.min(SWEEP)
            } else {
                SWEEP
            };
            if let Some(wait) = self.restart_wait() {
                tick = tick.min(wait);
            }
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
        (self.env.bridge_link)()?;
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

    /// Starts the lead (COORD req. 2): refused when a lead runs; resume or fresh by [`plan`]. Earlier
    /// automatic restarts no longer count.
    pub(crate) async fn start(self: &Arc<Self>, fresh: bool) -> Result<View, String> {
        *self.restarts() = Restarts::default();
        self.launch(fresh, None).await
    }

    /// A start; `again` is the session an automatic restart resumes.
    async fn launch(
        self: &Arc<Self>,
        fresh: bool,
        again: Option<(String, PathBuf)>,
    ) -> Result<View, String> {
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
        let automatic = again.is_some();
        let (target, expected, note) = match again {
            Some((id, file)) => self.again(id, file, &cwd).await?,
            None => self.target(&settings, fresh, &cwd).await?,
        };
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
            max_cost: None,
            tools: None,
        };
        match runtime.start(spec).await {
            Ok((session, started)) => {
                self.bound(handle, session, &started, expected.is_some(), note)
                    .await;
                Ok(self.view().await)
            }
            Err(e) => Err(self.not_started(handle, &e, automatic).await),
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
        self.closing.store(false, Ordering::Release);
        self.set_phase(Phase::Live, started.held.clone());
        let identity = (started.session_id.clone(), PathBuf::from(&started.file));
        tokio::spawn(Arc::clone(self).watch(
            handle,
            identity,
            session.subscribe(),
            session.states(),
        ));
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

    /// A start that did not bind: the record says why, and the owner hears it unless the start was
    /// an automatic restart (its retry or hold reports instead).
    async fn not_started(&self, handle: String, e: &StartError, automatic: bool) -> String {
        let detail = e.to_string();
        self.update(SessionUpdate {
            handle,
            state: Some("failed".into()),
            stop: Some(("failed".into(), Some(detail.clone()))),
            ..SessionUpdate::default()
        })
        .await;
        if automatic {
            return detail;
        }
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

    /// The target of an automatic restart: exactly the session that exited (HOST req. 81).
    async fn again(
        &self,
        id: String,
        file: PathBuf,
        cwd: &Path,
    ) -> Result<(Target, Option<String>, Option<String>), String> {
        let dir = self.env.state_dir.join("sessions").join("lead");
        match transcript::verify(file.clone(), dir, id.clone(), cwd.to_path_buf()).await {
            Ok(Some(_)) => Ok((
                Target::Resume(file),
                Some(id),
                Some("resumed after its OMP process exited by itself".into()),
            )),
            _ => Err(format!(
                "the transcript of lead session {id} did not verify for an exact resume"
            )),
        }
    }

    /// How long until the planned automatic restart; zero when it is due.
    fn restart_wait(&self) -> Option<Duration> {
        self.restarts()
            .due
            .as_ref()
            .map(|(at, _)| at.saturating_duration_since(Instant::now()))
    }

    /// The automatic restart (DESIGN 2.2): the same session when its lease is free (HOST req. 86-88).
    async fn restart(self: &Arc<Self>) {
        let Some((_, (id, file))) = self.restarts().due.take() else {
            return;
        };
        if self.current().is_some_and(|s| s.alive()) {
            // The owner started the lead meanwhile.
            return;
        }
        let lease = self.lease(&id).await;
        if lease != Lease::Free {
            let reason = format!(
                "the lead's OMP exited and its session {id} {}; not restarted, held for the owner",
                if lease == Lease::Held {
                    "is held by another OMP"
                } else {
                    "has an unknown lease"
                }
            );
            self.set_phase(Phase::Held, Some(reason.clone()));
            self.emit(
                Emit::new(EventKind::LeadNotStarted, Severity::Warning, true, "lead")
                    .with("reason", &reason),
            );
            return;
        }
        match self.launch(false, Some((id.clone(), file.clone()))).await {
            Ok(_) => log::info("lead restarted", &[("session", &id)]),
            Err(reason) => {
                log::warn("lead restart failed", &[("reason", &reason)]);
                self.exited((id, file), &format!("its restart failed: {reason}"), None);
            }
        }
    }

    /// After the child exited by itself (`exit` is its status), or a restart failed: plans the next
    /// restart, or past the limit holds the lead for the owner (DESIGN 2.2). Every exit is an event
    /// (HOST req. 70); the owner is alerted at the first exit of a series and at the hold.
    fn exited(&self, session: (String, PathBuf), why: &str, exit: Option<&str>) {
        let planned = self.restarts().plan(Instant::now(), session);
        let next = match planned {
            Some((delay, _)) => format!("restarting in {} s", delay.as_secs()),
            None => format!(
                "more than {RESTART_LIMIT} restarts within 10 minutes, held until `cataly lead start`"
            ),
        };
        let reason = format!("{why}; {next}");
        if let Some(exit) = exit {
            let urgent = matches!(planned, Some((_, 1)));
            self.emit(
                Emit::new(EventKind::SessionStopped, Severity::Warning, urgent, "lead")
                    .with("role", "lead")
                    .with("how", "exited")
                    .with("detail", format!("{exit}; {next}")),
            );
        }
        if planned.is_some() {
            self.set_phase(Phase::Stopped, Some(reason));
            self.wake.notify_one();
        } else {
            self.set_phase(Phase::Held, Some(reason.clone()));
            self.emit(
                Emit::new(EventKind::SessionFenced, Severity::Warning, true, "lead")
                    .with("role", "lead")
                    .with("reason", &reason),
            );
        }
    }

    /// Writes the lead's overlay once per hub run (DESIGN 4.2): goal mode off (OPC-36).
    fn overlay(&self, config: &Config) -> Result<PathBuf, String> {
        if let Some(path) = self.overlay.get() {
            return Ok(path.clone());
        }
        let path = crate::omp::launch::write_overlay(&self.env.state_dir, &config.fallback, false)
            .map_err(|e| e.to_string())?;
        Ok(self.overlay.get_or_init(|| path).clone())
    }

    async fn update(&self, update: SessionUpdate) {
        if let Err(e) = self.env.store.host(HostWrite::Session(update)).await {
            log::warn("session record not updated", &[("error", &e)]);
        }
    }

    /// A turn ended with the quota exhausted: the owner is alerted and the lead told once (OPC-58,
    /// OPC-64).
    async fn quota_blocked(&self, handle: &str, message: &str) {
        self.emit(
            Emit::new(EventKind::QuotaBlocked, Severity::Warning, true, "lead")
                .with("role", "lead")
                .with("message", message),
        );
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

    /// Follows one session's events: hub events, inbox wakes, the stop record and the restart.
    async fn watch(
        self: Arc<Self>,
        handle: String,
        identity: (String, PathBuf),
        mut events: broadcast::Receiver<(u64, SessionEvent)>,
        mut states: watch::Receiver<Arc<Snapshot>>,
    ) {
        let mut states_open = true;
        loop {
            let received = tokio::select! {
                received = events.recv() => received,
                changed = states.changed(), if states_open => {
                    // A readback showing the lead idle: a batch that met it between states (a turn
                    // just ending) goes now, not at the next tick; that readback brings no event of
                    // its own. Snapshots change only on events and readbacks, so this never polls.
                    if changed.is_err() {
                        states_open = false;
                        continue;
                    }
                    if states.borrow_and_update().status == Status::Idle {
                        self.wake.notify_one();
                    }
                    continue;
                }
            };
            let event = match received {
                Ok((_, event)) => event,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    self.wake.notify_one();
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            };
            self.forward(&event);
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
                SessionEvent::ModelUnexpected { model, requested } => self.emit(
                    Emit::new(EventKind::ModelUnexpected, Severity::Warning, true, "lead")
                        .with("role", "lead")
                        .with("model", model)
                        .with("requested", requested),
                ),
                SessionEvent::SpendOffer { title, count } => self.emit(
                    Emit::new(EventKind::SpendOffer, Severity::Warning, true, "lead")
                        .with("role", "lead")
                        .with("dialog", title)
                        .with("count", count),
                ),
                SessionEvent::QuotaBlocked { message } => {
                    self.quota_blocked(&handle, &message).await;
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
                    self.set_phase(Phase::Stopped, Some(format!("stopped: {}", record.how)));
                    if record.how == "exited" && !self.closing.load(Ordering::Acquire) {
                        let exit = record.detail.as_deref().unwrap_or("no exit status");
                        self.exited(
                            identity,
                            &format!("its OMP exited by itself ({exit})"),
                            Some(exit),
                        );
                    }
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
        self.report(notes, called).await;
        Ok(())
    }

    /// Records what a flush or a delivery found.
    async fn report(&self, notes: Vec<Note>, called: Option<Instant>) {
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
    }

    /// Hands a message to the lead (COORD-10, COORD-15): stored, then delivered at once when the lead
    /// can read. Accepted even with no lead session; it goes out once a lead exists (COORD req. 24).
    /// The message commits before it is written: a sender's retry after a crash finds its receipt.
    pub(crate) async fn deliver(&self, request: Message) -> Result<Receipt, String> {
        self.deliver_as(request, false).await
    }

    /// [`Lead::deliver`] for an owner request the caller knows was not written to the lead before:
    /// read live in this hub run, or absent from the lead's transcripts. It may be written while
    /// its records commit (decision 15, [`inbox::Mailbox::deliver_as`]).
    pub(crate) async fn deliver_live(&self, request: Message) -> Result<Receipt, String> {
        self.deliver_as(request, true).await
    }

    async fn deliver_as(&self, request: Message, live: bool) -> Result<Receipt, String> {
        let called = Instant::now();
        let (receipt, stored) = {
            let _one = self.flushing.lock().await;
            let session = self.current();
            let blocked = session
                .as_ref()
                .is_some_and(|s| s.snapshot().quota_blocked.is_some());
            let (receipt, stored, notes) = self
                .mailbox
                .deliver_as(&request, session.as_deref(), blocked, live)
                .await?;
            self.report(notes, Some(called)).await;
            (receipt, stored)
        };
        let Some(id) = stored else {
            return Ok(receipt);
        };
        self.wake.notify_one();
        Ok(self.mailbox.receipt(request.request_id, id).await)
    }

    /// When the lead last read a turn start of its session.
    pub(crate) fn turn_started(&self) -> Option<Instant> {
        *self
            .turn_started
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Passes an event to the relays; a turn start is stamped first. Nobody following is fine; a
    /// slow follower sees `Lagged` and reads the state again.
    fn forward(&self, event: &SessionEvent) {
        if *event == SessionEvent::TurnStarted {
            *self
                .turn_started
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
        }
        let _ = self.followers.send(event.clone());
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

    /// Decision 15 recovery: whether an owner request that an earlier hub run may have written
    /// to the lead before its record landed ([`inbox::wake_id`]) reached the lead's transcript.
    /// `Some(true)` when a user entry of the current or the previous lead transcript carries its
    /// marker; `Some(false)` when neither does and a lead started by this hub runs (the process an
    /// earlier run wrote to has let go of its session, so its file is final); `None` while no lead
    /// runs yet. A transcript that cannot be searched is an error: the caller fails closed.
    pub(crate) async fn written_earlier(&self, request_id: &str) -> Result<Option<bool>, String> {
        let Some(current) = self
            .current()
            .filter(|s| s.alive())
            .and_then(|s| s.snapshot().session_file.clone())
        else {
            return Ok(None);
        };
        let earlier = self
            .env
            .store
            .read(|r| r.session_files("lead", 2))
            .await
            .map_err(|e| e.to_string())?;
        let mut files = vec![current];
        for file in earlier {
            if !files.contains(&file) {
                files.push(file);
            }
        }
        let marker = inbox::marker(&inbox::wake_id(LEAD, request_id));
        for file in files {
            match transcript::search(PathBuf::from(&file), marker.clone()).await {
                Ok(transcript::Found::Yes) => return Ok(Some(true)),
                Ok(transcript::Found::No | transcript::Found::Missing) => {}
                Ok(transcript::Found::OutOfTime) => {
                    return Err(format!("the search of {file} ran out of time"));
                }
                Err(e) => return Err(format!("{file}: {e}")),
            }
        }
        Ok(Some(false))
    }

    /// Follows the events of whichever lead session runs, across restarts.
    pub(crate) fn follow(&self) -> broadcast::Receiver<SessionEvent> {
        self.followers.subscribe()
    }

    /// The current session's snapshot without waiting.
    pub(crate) fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.current().map(|s| s.snapshot())
    }

    pub(crate) async fn view(&self) -> View {
        let (phase, reason) = self.phase().clone();
        let entries = self.mailbox.list().await.unwrap_or_default();
        let count = |s: &str| entries.iter().filter(|(l, _)| l.state == s).count();
        let pending = |wakes: bool| {
            entries
                .iter()
                .filter(|(l, _)| l.state == "pending" && l.wakes == wakes)
                .count()
        };
        View {
            phase,
            reason,
            session: self.current().map(|s| (*s.snapshot()).clone()),
            waiting: pending(true),
            notes: pending(false),
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

    /// Stops the lead; refused while it works or has a dialog unless forced (HOST req. 83). With no
    /// lead running it cancels a planned automatic restart.
    pub(crate) async fn stop(&self, force: bool) -> Result<View, String> {
        let session = match self.live() {
            Ok(session) => session,
            Err(e) => {
                if self.restarts().due.take().is_none() {
                    return Err(e);
                }
                self.set_phase(
                    Phase::Stopped,
                    Some("the automatic restart was cancelled".into()),
                );
                return Ok(self.view().await);
            }
        };
        let snapshot = session.snapshot();
        if !force && (snapshot.status == Status::Working || !snapshot.dialogs.is_empty()) {
            return Err("the lead is working or has a pending dialog; use --force".into());
        }
        let handle = snapshot.handle.clone();
        self.closing.store(true, Ordering::Release);
        let record = match session.close(force).await {
            Ok(record) => record,
            Err(e) => {
                self.closing.store(false, Ordering::Release);
                return Err(e);
            }
        };
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
        self.closing.store(true, Ordering::Release);
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

    /// DESIGN 2.2: restarts wait 1, 2, 4, 8 and 16 s; a sixth within 10 minutes is refused (the lead
    /// is held); once the oldest restart is 10 minutes old one more is allowed.
    #[test]
    fn restarts_back_off_and_stop_at_the_limit() {
        let mut restarts = Restarts::default();
        let start = Instant::now();
        let session = || ("s1".to_string(), PathBuf::from("/s.jsonl"));
        let mut delays = Vec::new();
        for n in 0..5u64 {
            let (delay, count) = restarts
                .plan(start + Duration::from_secs(n), session())
                .unwrap();
            assert_eq!(count, usize::try_from(n + 1).unwrap());
            delays.push(delay.as_secs());
        }
        assert_eq!(delays, [1, 2, 4, 8, 16]);
        assert!(restarts.due.is_some());
        assert_eq!(
            restarts.plan(start + Duration::from_secs(5), session()),
            None
        );
        assert!(restarts.due.is_none(), "nothing stays planned once held");
        let later = start + RESTART_WINDOW;
        assert_eq!(
            restarts.plan(later, session()),
            Some((Duration::from_secs(16), 5))
        );
    }
}
