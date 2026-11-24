//! The job engine (DESIGN 6): one scheduler task with a min-heap of due times, the typed event stream
//! with durable append, routing to destinations, transition memory, and the built-in kinds for timed
//! jobs (`cron`, `once`) and condition jobs (`probe`).

pub(crate) mod cron;
pub(crate) mod event;
pub(crate) mod memory;
mod route;
mod run;
mod schedule;
mod stream;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{Mutex as AsyncMutex, Notify, Semaphore, mpsc, watch};

use crate::config::{Config, Trigger};
use crate::hub::supervisor::Supervisor;
use crate::log;
use crate::proto::{JobView, RunView};
use crate::store::{Apply, EventRecord, JobPatch, Store, StoreError};
use crate::sys::{ErrorClass, now_ms};
use event::{Emit, EventKind, Severity};
use memory::Memory;
pub(crate) use stream::Stream;

/// Job commands running at once across the hub (OJ req. 9).
const COMMAND_SLOTS: usize = 4;
/// Destination commands running at once.
const DESTINATION_SLOTS: usize = 4;

/// A job operation failed.
#[derive(Debug)]
pub(crate) enum JobsError {
    Store(StoreError),
    Fs(crate::sys::fs::FsError),
    NotFound(String),
    Busy(String),
    Refused(String),
    Stopped,
}

impl JobsError {
    pub(crate) fn class(&self) -> ErrorClass {
        match self {
            Self::Store(e) => e.class(),
            Self::Fs(e) => e.class(),
            Self::NotFound(_) | Self::Busy(_) | Self::Refused(_) => ErrorClass::Rejected,
            Self::Stopped => ErrorClass::Transient,
        }
    }
}

impl fmt::Display for JobsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(e) => e.fmt(f),
            Self::Fs(e) => e.fmt(f),
            Self::NotFound(name) => write!(f, "no job named '{name}' in the config"),
            Self::Busy(name) => write!(f, "job '{name}' is running; try again when it finished"),
            Self::Refused(why) => f.write_str(why),
            Self::Stopped => f.write_str("the hub is stopping"),
        }
    }
}

impl std::error::Error for JobsError {}

impl From<StoreError> for JobsError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

/// Messages to the scheduler task.
#[derive(Debug)]
pub(crate) enum Command {
    /// A job row changed outside the scheduler (hold, resume); re-read it.
    Refresh(String),
    /// A live condition run finished; schedule its next probe.
    Finished {
        name: String,
        scheduled_ms: i64,
        finished_ms: i64,
    },
}

/// Everything the engine's tasks share.
pub(crate) struct Ctx {
    pub(crate) store: Store,
    pub(crate) stream: Stream,
    pub(crate) config: watch::Receiver<Arc<Config>>,
    pub(crate) files_dir: PathBuf,
    pub(crate) event_log: PathBuf,
    pub(crate) shutdown: watch::Receiver<bool>,
    command_slots: Semaphore,
    destination_slots: Arc<Semaphore>,
    running: Mutex<HashSet<String>>,
    commands_tx: mpsc::Sender<Command>,
    commands: AsyncMutex<mpsc::Receiver<Command>>,
    route_wake: Arc<Notify>,
}

impl Ctx {
    pub(crate) fn new(
        store: Store,
        config: watch::Receiver<Arc<Config>>,
        files_dir: PathBuf,
        event_log: PathBuf,
        shutdown: watch::Receiver<bool>,
    ) -> Arc<Self> {
        let route_wake = Arc::new(Notify::new());
        let (commands_tx, commands) = mpsc::channel(64);
        Arc::new(Self {
            stream: Stream::new(store.clone(), Arc::clone(&route_wake)),
            store,
            config,
            files_dir,
            event_log,
            shutdown,
            command_slots: Semaphore::new(COMMAND_SLOTS),
            destination_slots: Arc::new(Semaphore::new(DESTINATION_SLOTS)),
            running: Mutex::new(HashSet::new()),
            commands_tx,
            commands: AsyncMutex::new(commands),
            route_wake,
        })
    }

    fn config(&self) -> Arc<Config> {
        Arc::clone(&self.config.borrow())
    }

    /// Marks a job as running; `false` when a run of it is already in progress (runs never overlap).
    fn claim(&self, name: &str) -> bool {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name.to_string())
    }

    fn release(&self, name: &str) {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(name);
    }

    fn is_running(&self, name: &str) -> bool {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(name)
    }

    fn running_count(&self) -> usize {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// The engine as the hub sees it.
#[derive(Clone)]
pub(crate) struct Jobs {
    ctx: Arc<Ctx>,
}

impl Jobs {
    /// Recovers runs and deliveries left by the previous hub, then starts the scheduler and the router
    /// under the supervisor.
    pub(crate) async fn start(ctx: Arc<Ctx>, supervisor: &Supervisor) -> Result<Self, JobsError> {
        run::recover(&ctx).await?;
        let sched = Arc::clone(&ctx);
        supervisor.spawn("scheduler", move || schedule::scheduler(Arc::clone(&sched)));
        let router = Arc::clone(&ctx);
        supervisor.spawn("router", move || route::router(Arc::clone(&router)));
        Ok(Self { ctx })
    }

    pub(crate) fn stream(&self) -> &Stream {
        &self.ctx.stream
    }

    pub(crate) fn running_count(&self) -> usize {
        self.ctx.running_count()
    }

    /// Waits until no run is in progress, up to `limit` (runs see the shutdown signal themselves).
    pub(crate) async fn drain(&self, limit: Duration) {
        let deadline = tokio::time::Instant::now() + limit;
        while self.ctx.running_count() > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The job view (WS req. 69).
    pub(crate) async fn list(&self) -> Result<Vec<JobView>, JobsError> {
        let config = self.ctx.config();
        let names: Vec<String> = config.jobs.keys().cloned().collect();
        let (rows, runs, streaks) = self
            .ctx
            .store
            .read(move |r| {
                let mut streaks = HashMap::new();
                for name in &names {
                    let errors = r
                        .memory(name, false)?
                        .into_iter()
                        .find(|(_, c, _)| c == "errors")
                        .and_then(|(_, _, v)| v.parse::<i64>().ok())
                        .unwrap_or(0);
                    streaks.insert(name.clone(), errors);
                }
                Ok((r.jobs()?, r.last_runs()?, streaks))
            })
            .await?;
        let mut out = Vec::with_capacity(config.jobs.len());
        for (name, job) in &config.jobs {
            let row = rows.iter().find(|r| &r.name == name);
            let running = self.ctx.is_running(name);
            let state = match row {
                _ if running => "running",
                _ if !job.enabled => "disabled",
                Some(r) if r.consumed => "consumed",
                Some(r) if r.held => "on hold",
                _ => "waiting for trigger",
            };
            out.push(JobView {
                name: name.clone(),
                kind: job.trigger.kind().to_string(),
                trigger: job.trigger.canonical(),
                enabled: job.enabled,
                dry_run: job.dry_run,
                state: state.to_string(),
                held: row.is_some_and(|r| r.held),
                consumed: row.is_some_and(|r| r.consumed),
                needs_ack: row.is_some_and(|r| r.needs_ack),
                next_due_ms: row.and_then(|r| r.next_due_ms).filter(|_| job.enabled),
                skipped: row.map_or(0, |r| r.skipped),
                error_streak: streaks.get(name).copied().unwrap_or(0),
                destinations: job.destinations.clone(),
                last_run: runs.iter().find(|r| &r.job == name).map(|r| RunView {
                    id: r.id,
                    dry: r.dry,
                    started_ms: r.started_ms,
                    ended_ms: r.ended_ms,
                    outcome: r.outcome.clone(),
                    exit_code: r.exit_code,
                    stdout_path: r.stdout_path.clone().filter(|_| r.kept),
                    stderr_path: r.stderr_path.clone().filter(|_| r.kept),
                    note: "the time limit ends only the direct child; anything it detached keeps running".into(),
                }),
            });
        }
        Ok(out)
    }

    /// Runs one dry tick of a job now and returns its events (OJ req. 11).
    pub(crate) async fn run_once(&self, name: &str) -> Result<Vec<EventRecord>, JobsError> {
        let job = self
            .ctx
            .config()
            .jobs
            .get(name)
            .cloned()
            .ok_or_else(|| JobsError::NotFound(name.into()))?;
        if *self.ctx.shutdown.borrow() {
            return Err(JobsError::Stopped);
        }
        if !self.ctx.claim(name) {
            return Err(JobsError::Busy(name.into()));
        }
        let result = run::execute(
            &self.ctx,
            run::Request {
                job,
                scheduled_ms: now_ms(),
                dry: true,
                consume: false,
            },
        )
        .await;
        self.ctx.release(name);
        result
    }

    /// Puts a job on hold (`job disable`).
    pub(crate) async fn hold(&self, name: &str) -> Result<(), JobsError> {
        if !self.ctx.config().jobs.contains_key(name) {
            return Err(JobsError::NotFound(name.into()));
        }
        let patch = JobPatch {
            held: Some(true),
            ..JobPatch::default()
        };
        self.ctx
            .store
            .patch_job(name.into(), patch)
            .await?
            .ok_or_else(|| JobsError::NotFound(name.into()))?;
        self.refresh(name).await
    }

    /// Resumes a held job (`job enable`), WS req. 66: refused while a run is in progress, needs `ack`
    /// after a broken-off run, never resumes a consumed one-shot; cron restarts from now, a probe runs
    /// one interval from now.
    pub(crate) async fn resume(&self, name: &str, ack: bool) -> Result<(), JobsError> {
        let job = self
            .ctx
            .config()
            .jobs
            .get(name)
            .cloned()
            .ok_or_else(|| JobsError::NotFound(name.into()))?;
        if self.ctx.is_running(name) {
            return Err(JobsError::Busy(name.into()));
        }
        let key = name.to_string();
        let row = self
            .ctx
            .store
            .read(move |r| r.job(&key))
            .await?
            .ok_or_else(|| JobsError::NotFound(name.into()))?;
        if row.consumed {
            return Err(JobsError::Refused(format!(
                "job '{name}' is a one-shot that was already consumed; add a new job instead"
            )));
        }
        if row.needs_ack && !ack {
            return Err(JobsError::Refused(format!(
                "job '{name}' had a run broken off; resume it with --ack (that occurrence is not repeated)"
            )));
        }
        let now = now_ms();
        let next = match &job.trigger {
            Trigger::Cron { cron, zone, .. } => jiff::Timestamp::from_millisecond(now)
                .ok()
                .and_then(|t| cron.next_after(zone, t))
                .map(jiff::Timestamp::as_millisecond),
            Trigger::Once { at } => Some(at.as_millisecond()),
            Trigger::Probe { every, .. } => Some(now.saturating_add(duration_ms(*every))),
        };
        let patch = JobPatch {
            held: Some(false),
            needs_ack: Some(false),
            next_due_ms: Some(next),
            ..JobPatch::default()
        };
        self.ctx.store.patch_job(name.into(), patch).await?;
        self.refresh(name).await
    }

    async fn refresh(&self, name: &str) -> Result<(), JobsError> {
        self.ctx
            .commands_tx
            .send(Command::Refresh(name.into()))
            .await
            .map_err(|_| JobsError::Stopped)
    }

    /// Records whether the config file on disk is valid: one event per distinct error, one when it is
    /// valid again (transition memory of job `hub`).
    pub(crate) async fn config_outcome(&self, error: Option<&str>) -> Result<(), JobsError> {
        let rows = self.ctx.store.read(|r| r.memory("hub", false)).await?;
        let mut memory = Memory::new("hub", false, rows);
        let emit = match error {
            Some(error) => memory.transition("config", "error", error).then(|| {
                Emit::new(EventKind::ConfigInvalid, Severity::Warning, true, "config")
                    .with("error", error)
            }),
            None => memory
                .clear("config", "error")
                .map(|_| Emit::new(EventKind::ConfigValid, Severity::Info, false, "config")),
        };
        if let Some(emit) = emit {
            self.hub_event(emit).await?;
        }
        let writes = memory.into_writes();
        if !writes.is_empty() {
            self.ctx
                .store
                .apply(Apply {
                    memory: writes,
                    ..Apply::default()
                })
                .await?;
        }
        Ok(())
    }

    /// Appends one hub event (routed nowhere but the store and the event log).
    pub(crate) async fn hub_event(&self, emit: Emit) -> Result<(), JobsError> {
        let event = event::build("hub", emit, &[], false, &[], now_ms());
        self.ctx.stream.append(vec![event]).await?;
        Ok(())
    }
}

pub(crate) fn duration_ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// Formats an epoch-milliseconds instant as RFC 3339.
pub(crate) fn instant_text(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms).map_or_else(|_| ms.to_string(), |t| t.to_string())
}

fn log_failure(what: &str, job: &str, error: &JobsError) {
    log::warn(
        what,
        &[("job", &job), ("error", error), ("class", &error.class())],
    );
}

/// Test-only crash points between durable steps.
#[cfg(test)]
pub(crate) mod fault {
    /// Aborts the process when `CATALY_FAULT` names this point.
    pub(crate) fn point(name: &str) {
        if std::env::var("CATALY_FAULT").is_ok_and(|v| v == name) {
            std::process::abort();
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config;
    use crate::store::tests::open_temp;

    pub(crate) fn ctx_with(
        config_text: &str,
        tag: &str,
    ) -> (Arc<Ctx>, watch::Sender<bool>, PathBuf) {
        let (store, _writer, dir) = open_temp(tag);
        ctx_in(store, dir, config_text)
    }

    pub(crate) fn ctx_in(
        store: Store,
        dir: PathBuf,
        config_text: &str,
    ) -> (Arc<Ctx>, watch::Sender<bool>, PathBuf) {
        let config = Arc::new(config::parse(config_text).unwrap());
        let (_config_tx, config_rx) = watch::channel(config);
        let (stop_tx, stop_rx) = watch::channel(false);
        let files = dir.join("files");
        crate::sys::fs::ensure_private_dir(&files).unwrap();
        let ctx = Ctx::new(store, config_rx, files, dir.join("events.jsonl"), stop_rx);
        (ctx, stop_tx, dir)
    }

    pub(crate) fn probe_config(argv: &str, extra: &str) -> String {
        format!("[jobs.p]\nkind = \"probe\"\nevery = \"30s\"\nargv = {argv}\ncwd = \"/\"\n{extra}")
    }

    #[tokio::test]
    async fn run_once_is_dry_and_refuses_overlap() {
        let (ctx, _stop, _dir) = ctx_with(
            &probe_config(r#"["sh", "-c", "sleep 0.3; echo up"]"#, ""),
            "jobs-once",
        );
        let jobs = Jobs {
            ctx: Arc::clone(&ctx),
        };
        let first = jobs.clone();
        let running = tokio::spawn(async move { first.run_once("p").await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(matches!(jobs.run_once("p").await, Err(JobsError::Busy(_))));
        let events = running.await.unwrap().unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].dry && events[0].kind == "probe.fired");
        assert!(matches!(
            jobs.run_once("nope").await,
            Err(JobsError::NotFound(_))
        ));
        // Dry state is kept apart from live state.
        let live = ctx.store.read(|r| r.memory("p", false)).await.unwrap();
        assert_eq!(live, []);
    }

    #[tokio::test]
    async fn config_outcome_reports_once_per_error_and_recovery() {
        let (ctx, _stop, _dir) = ctx_with("", "jobs-config");
        let jobs = Jobs {
            ctx: Arc::clone(&ctx),
        };
        jobs.config_outcome(Some("a: bad")).await.unwrap();
        jobs.config_outcome(Some("a: bad")).await.unwrap();
        jobs.config_outcome(Some("b: bad")).await.unwrap();
        jobs.config_outcome(None).await.unwrap();
        jobs.config_outcome(None).await.unwrap();
        let kinds: Vec<String> = ctx
            .store
            .read(|r| r.events_after(0, 10))
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(kinds, ["config.invalid", "config.invalid", "config.valid"]);
    }

    #[tokio::test]
    async fn hold_and_resume_rules() {
        let (ctx, _stop, _dir) = ctx_with(&probe_config(r#"["true"]"#, ""), "jobs-hold");
        let jobs = Jobs {
            ctx: Arc::clone(&ctx),
        };
        let reg = crate::store::JobRegistration {
            name: "p".into(),
            kind: "probe",
            trigger: "probe every 30s".into(),
            next_due_ms: Some(0),
            now_ms: 0,
        };
        ctx.store.register_job(reg).await.unwrap();
        // No scheduler runs in this test; drain its queue so refresh sends do not block.
        let drain = Arc::clone(&ctx);
        tokio::spawn(async move { while drain.commands.lock().await.recv().await.is_some() {} });
        jobs.hold("p").await.unwrap();
        ctx.store
            .patch_job(
                "p".into(),
                JobPatch {
                    needs_ack: Some(true),
                    ..JobPatch::default()
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            jobs.resume("p", false).await,
            Err(JobsError::Refused(_))
        ));
        jobs.resume("p", true).await.unwrap();
        let row = ctx.store.read(|r| r.job("p")).await.unwrap().unwrap();
        assert!(!row.held && !row.needs_ack && row.next_due_ms.unwrap() > now_ms());
        ctx.store
            .patch_job(
                "p".into(),
                JobPatch {
                    consumed: Some(true),
                    ..JobPatch::default()
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            jobs.resume("p", true).await,
            Err(JobsError::Refused(_))
        ));
        assert!(matches!(jobs.hold("x").await, Err(JobsError::NotFound(_))));
    }
}
