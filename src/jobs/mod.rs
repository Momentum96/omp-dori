//! The job engine (DESIGN 6): one scheduler task with a min-heap of due times, the typed event stream
//! with durable append, routing to destinations, transition memory, the built-in kinds for timed
//! jobs (`cron`, `once`) and condition jobs (`probe`), and the owner job kinds (`owner`).

pub(crate) mod cron;
pub(crate) mod event;
pub(crate) mod memory;
pub(crate) mod notes;
pub(crate) mod owner;
mod route;
mod run;
mod schedule;
mod stream;
pub(crate) mod update;
pub(crate) mod usage;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{Mutex as AsyncMutex, Notify, Semaphore, mpsc, watch};

use crate::config::{Config, Job, Owner, Trigger, Work};
use crate::hub::supervisor::Supervisor;
use crate::log;
use crate::omp::Omp;
use crate::proto::{JobView, RunView};
use crate::relay::discord::Rest;
use crate::store::{Apply, EventRecord, JobPatch, MemoryWrite, Store, StoreError};
use crate::sys::{ErrorClass, now_ms};
use event::{Emit, EventKind, Severity};
use memory::Memory;
use owner::quota::{Gate, GateView};
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
    /// The config file; its directory holds `secrets/`.
    pub(crate) config_file: PathBuf,
    pub(crate) omp: Arc<Omp>,
    /// The quota gate (OJ-08).
    pub(crate) gate: Gate,
    pub(crate) shutdown: watch::Receiver<bool>,
    config_error: Mutex<Option<String>>,
    /// The Discord client, rebuilt when the token or settings change.
    discord: Mutex<Option<(String, Arc<Rest>)>>,
    command_slots: Semaphore,
    destination_slots: Arc<Semaphore>,
    running: Mutex<HashSet<String>>,
    commands_tx: mpsc::Sender<Command>,
    commands: AsyncMutex<mpsc::Receiver<Command>>,
    route_wake: Arc<Notify>,
}

/// Where the engine keeps and finds its files.
pub(crate) struct Files {
    pub(crate) dir: PathBuf,
    pub(crate) event_log: PathBuf,
    pub(crate) config_file: PathBuf,
}

impl Ctx {
    pub(crate) fn new(
        store: Store,
        config: watch::Receiver<Arc<Config>>,
        files: Files,
        omp: Arc<Omp>,
        shutdown: watch::Receiver<bool>,
    ) -> Arc<Self> {
        let route_wake = Arc::new(Notify::new());
        let (commands_tx, commands) = mpsc::channel(64);
        Arc::new(Self {
            stream: Stream::new(store.clone(), Arc::clone(&route_wake)),
            store,
            config,
            files_dir: files.dir,
            event_log: files.event_log,
            config_file: files.config_file,
            omp,
            gate: Gate::default(),
            shutdown,
            config_error: Mutex::new(None),
            discord: Mutex::new(None),
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

    /// Why the config file on disk is not applied, if it is not.
    pub(crate) fn config_error(&self) -> Option<String> {
        self.config_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn set_config_error(&self, error: Option<String>) {
        *self
            .config_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = error;
    }

    /// The Discord client for the current settings and token; rebuilt when either changes.
    pub(crate) fn discord(&self, config: &Config) -> Result<Arc<Rest>, String> {
        let path = owner::health::token_path(config, &self.config_file);
        let token = match crate::sys::fs::read_private(&path, 4096) {
            Ok(Some(bytes)) => String::from_utf8(bytes)
                .map_err(|_| "the Discord token file is not text".to_string())?
                .trim()
                .to_string(),
            Ok(None) => return Err(format!("no Discord token file at {}", path.display())),
            Err(e) => return Err(format!("Discord token file: {e}")),
        };
        if token.is_empty() {
            return Err("the Discord token file is empty".into());
        }
        let base = config.discord.api_base.clone().map_or_else(
            || crate::relay::http::Base::parse(crate::relay::discord::API),
            Ok,
        )?;
        let digest = ring::digest::digest(&ring::digest::SHA256, token.as_bytes());
        let key = format!("{base:?}{:?}{:?}", config.discord.guild, digest.as_ref());
        let mut cached = self.discord.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((k, rest)) = cached.as_ref()
            && *k == key
        {
            return Ok(Arc::clone(rest));
        }
        let rest = Arc::new(Rest::new(base, &token, config.discord.guild)?);
        *cached = Some((key, Arc::clone(&rest)));
        Ok(rest)
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

/// Runs one occurrence of a job: its command, or one tick of its owner kind.
pub(crate) async fn perform(
    ctx: &Ctx,
    job: &Arc<Job>,
    scheduled_ms: i64,
    dry: bool,
    consume: bool,
) -> Result<Vec<EventRecord>, JobsError> {
    match &job.work {
        Work::Command { .. } => {
            let request = run::Request {
                job: Arc::clone(job),
                scheduled_ms,
                dry,
                consume,
            };
            run::execute(ctx, request).await
        }
        Work::Owner(kind) => owner::tick(ctx, job, kind, dry).await,
    }
}

/// Memory job and subject of the owner's quota-gate lifts.
const HUB: &str = "hub";
const LIFT: &str = "quota.lift";

/// The engine as the hub sees it.
#[derive(Clone)]
pub(crate) struct Jobs {
    ctx: Arc<Ctx>,
}

impl Jobs {
    /// Recovers runs and deliveries left by the previous hub, restores the quota gate, then starts
    /// the scheduler and the router under the supervisor.
    pub(crate) async fn start(ctx: Arc<Ctx>, supervisor: &Supervisor) -> Result<Self, JobsError> {
        run::recover(&ctx).await?;
        restore_gate(&ctx).await?;
        let sched = Arc::clone(&ctx);
        supervisor.spawn("scheduler", move || schedule::scheduler(Arc::clone(&sched)));
        let router = Arc::clone(&ctx);
        supervisor.spawn("router", move || route::router(Arc::clone(&router)));
        Ok(Self { ctx })
    }

    pub(crate) fn ctx(&self) -> &Arc<Ctx> {
        &self.ctx
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
        let (rows, runs, ticks) = self
            .ctx
            .store
            .read(move |r| {
                let mut ticks = HashMap::new();
                for name in &names {
                    let memory = r.memory(name, false)?;
                    let get = |subject: &str, condition: &str| {
                        memory
                            .iter()
                            .find(|(s, c, _)| s == subject && c == condition)
                            .map(|(_, _, v)| v.clone())
                    };
                    let errors = get(name, "errors")
                        .or_else(|| get(owner::TICK, "failures"))
                        .and_then(|v| v.parse::<i64>().ok())
                        .unwrap_or(0);
                    let last_good =
                        get(owner::TICK, "last_good").and_then(|v| v.parse::<i64>().ok());
                    ticks.insert(name.clone(), (errors, last_good, get(owner::TICK, "error")));
                }
                Ok((r.jobs()?, r.last_runs()?, ticks))
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
                kind: job.kind().to_string(),
                trigger: job.trigger.canonical(),
                enabled: job.enabled,
                dry_run: job.dry_run,
                state: state.to_string(),
                held: row.is_some_and(|r| r.held),
                consumed: row.is_some_and(|r| r.consumed),
                needs_ack: row.is_some_and(|r| r.needs_ack),
                next_due_ms: row.and_then(|r| r.next_due_ms).filter(|_| job.enabled),
                skipped: row.map_or(0, |r| r.skipped),
                error_streak: ticks.get(name).map_or(0, |t| t.0),
                last_good_ms: ticks.get(name).and_then(|t| t.1),
                last_error: ticks.get(name).and_then(|t| t.2.clone()),
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
        let result = perform(&self.ctx, &job, now_ms(), true, false).await;
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
            Trigger::Probe { every, .. } | Trigger::Every { every } => {
                Some(now.saturating_add(duration_ms(*every)))
            }
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

    /// The quota gate now.
    pub(crate) fn gate(&self) -> GateView {
        self.ctx.gate.view(now_ms())
    }

    /// The owner lifts the quota gate until the current windows reset (OJ req. 25); the lift
    /// survives a restart.
    pub(crate) async fn lift(&self) -> Result<GateView, JobsError> {
        let view = self.ctx.gate.lift(now_ms());
        let value = self.ctx.gate.lifted_until().map(|until| until.to_string());
        self.write_memory(HUB, LIFT, "until", value).await?;
        Ok(view)
    }

    async fn write_memory(
        &self,
        job: &str,
        subject: &str,
        condition: &str,
        value: Option<String>,
    ) -> Result<(), JobsError> {
        let write = MemoryWrite {
            job: job.into(),
            subject: subject.into(),
            condition: condition.into(),
            dry: false,
            value,
        };
        self.ctx
            .store
            .apply(Apply {
                memory: vec![write],
                ..Apply::default()
            })
            .await?;
        Ok(())
    }

    /// The `pr` job that follows PRs named by the owner: the one given, else the only one.
    fn pr_job(&self, name: Option<&str>) -> Result<String, JobsError> {
        let config = self.ctx.config();
        let pr_jobs: Vec<&String> = config
            .jobs
            .iter()
            .filter(|(_, j)| matches!(j.work, Work::Owner(Owner::Pr(_))))
            .map(|(n, _)| n)
            .collect();
        match (name, pr_jobs.as_slice()) {
            (Some(n), _) if pr_jobs.iter().any(|p| *p == n) => Ok(n.to_string()),
            (Some(n), _) => Err(JobsError::Refused(format!("'{n}' is not a job of kind pr"))),
            (None, [one]) => Ok((*one).clone()),
            (None, []) => Err(JobsError::Refused(
                "no job of kind pr is configured; add one to follow pull requests".into(),
            )),
            (None, _) => Err(JobsError::Refused(
                "several pr jobs exist; name one with --job".into(),
            )),
        }
    }

    /// Follows a PR (WS req. 40): following again keeps the original start.
    pub(crate) async fn pr_follow(
        &self,
        reference: &str,
        job: Option<&str>,
    ) -> Result<String, JobsError> {
        let pr = owner::pr::parse_ref(reference).ok_or_else(|| {
            JobsError::Refused(format!(
                "'{reference}' is neither owner/repo#N nor a PR URL"
            ))
        })?;
        let job = self.pr_job(job)?;
        let (j, p) = (job.clone(), pr.clone());
        let known = self
            .ctx
            .store
            .read(move |r| r.memory(&j, false))
            .await?
            .into_iter()
            .any(|(s, c, _)| s == p && c == owner::pr::FOLLOW);
        if !known {
            self.write_memory(&job, &pr, owner::pr::FOLLOW, Some(now_ms().to_string()))
                .await?;
        }
        Ok(pr)
    }

    /// Stops following a PR; a later follow starts afresh.
    pub(crate) async fn pr_unfollow(
        &self,
        reference: &str,
        job: Option<&str>,
    ) -> Result<String, JobsError> {
        let pr = owner::pr::parse_ref(reference).ok_or_else(|| {
            JobsError::Refused(format!(
                "'{reference}' is neither owner/repo#N nor a PR URL"
            ))
        })?;
        let job = self.pr_job(job)?;
        self.write_memory(&job, &pr, owner::pr::FOLLOW, None)
            .await?;
        self.write_memory(&job, &pr, "cursor", None).await?;
        Ok(pr)
    }

    /// Every followed PR of every `pr` and `github` job.
    pub(crate) async fn pr_list(&self) -> Result<Vec<crate::proto::PrView>, JobsError> {
        let config = self.ctx.config();
        let mut out = Vec::new();
        for (name, job) in &config.jobs {
            if !matches!(job.work, Work::Owner(Owner::Pr(_) | Owner::Github(_))) {
                continue;
            }
            let key = name.clone();
            let rows = self.ctx.store.read(move |r| r.memory(&key, false)).await?;
            let memory = Memory::new(name, false, rows);
            for (pr, since) in memory.with_condition(owner::pr::FOLLOW) {
                let state = memory
                    .get(&pr, "cursor")
                    .and_then(|c| serde_json::from_str::<owner::pr::Cursor>(c).ok())
                    .map_or_else(|| "not read yet".to_string(), |c| c.state);
                out.push(crate::proto::PrView {
                    pr: pr.clone(),
                    job: name.clone(),
                    since_ms: since.parse().unwrap_or(0),
                    state,
                    error: memory.get(&pr, "error").map(str::to_string),
                });
            }
        }
        Ok(out)
    }

    /// The health report (WS-34).
    pub(crate) async fn health(&self) -> Result<Vec<owner::health::Check>, JobsError> {
        owner::health::checks(&self.ctx).await
    }
}

/// Rebuilds the quota gate from each quota job's stored causes and the owner's stored lift.
async fn restore_gate(ctx: &Ctx) -> Result<(), JobsError> {
    let config = ctx.config();
    for (name, job) in &config.jobs {
        if !matches!(job.work, Work::Owner(Owner::Quota(_))) {
            continue;
        }
        let key = name.clone();
        let rows = ctx.store.read(move |r| r.memory(&key, false)).await?;
        let memory = Memory::new(name, false, rows);
        ctx.gate.set(
            name,
            owner::quota::stored_causes(memory.get("gate", "hold")),
        );
    }
    let rows = ctx.store.read(|r| r.memory(HUB, false)).await?;
    let memory = Memory::new(HUB, false, rows);
    if let Some(until) = memory
        .get(LIFT, "until")
        .and_then(|v| v.parse::<i64>().ok())
    {
        ctx.gate.restore_lift(Some(until));
    }
    Ok(())
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config;
    use crate::store::tests::open_temp;
    use crate::sys::fs::tests::Scratch;

    pub(crate) fn ctx_with(
        config_text: &str,
        tag: &str,
    ) -> (Arc<Ctx>, watch::Sender<bool>, Scratch) {
        let (store, _writer, dir) = open_temp(tag);
        let (ctx, stop) = ctx_in(store, &dir, config_text);
        (ctx, stop, dir)
    }

    pub(crate) fn ctx_in(
        store: Store,
        dir: &std::path::Path,
        config_text: &str,
    ) -> (Arc<Ctx>, watch::Sender<bool>) {
        let config = Arc::new(config::parse(config_text).unwrap());
        let (_config_tx, config_rx) = watch::channel(config);
        let (stop_tx, stop_rx) = watch::channel(false);
        let files = dir.join("files");
        crate::sys::fs::ensure_private_dir(&files).unwrap();
        let paths = Files {
            dir: files,
            event_log: dir.join("events.jsonl"),
            config_file: dir.join("config.toml"),
        };
        let ctx = Ctx::new(store, config_rx, paths, Omp::new(), stop_rx);
        (ctx, stop_tx)
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
