//! The hub (DESIGN 2): start-up in a fixed order, the single-instance lock, the control socket,
//! supervision, liveness and graceful shutdown.
//!
//! Start-up order: directories, hub log, lock, config, store (with migrations), socket, job engine
//! (recovery first), supervised tasks, stall detector, then `READY=1`.

pub(crate) mod dispatch;
pub(crate) mod socket;
pub(crate) mod supervisor;

use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

use crate::cli::exit;
use crate::config::{self, Config};
use crate::host::headless::Headless;
use crate::host::lead::{Env as LeadEnv, Lead};
use crate::jobs::event::{Emit, EventKind, Severity};
use crate::jobs::{Ctx, Files, Jobs};
use crate::log;
use crate::omp::Omp;
use crate::proto::{Stats, Status};
use crate::store::Store;
use crate::sys::command::KILL_GRACE;
use crate::sys::fs::{ensure_private_dir, write_atomic};
use crate::sys::latency::Latency;
use crate::sys::lock::{self, LockOutcome};
use crate::sys::notify::Notifier;
use crate::sys::paths::Paths;
use crate::sys::stall::{self, Heartbeat};
use crate::sys::{host, now_ms};
use dispatch::Dedupe;
use supervisor::{Policy, Supervisor};

/// The main loop must advance within this time (OJ req. 47).
const STALL_LIMIT: Duration = Duration::from_secs(60);
/// First store maintenance after start, then once a day.
const MAINTENANCE_FIRST: Duration = Duration::from_mins(10);
const MAINTENANCE_EVERY: Duration = Duration::from_hours(24);

/// State shared by the control socket and the dispatcher.
pub(crate) struct Hub {
    paths: Paths,
    started_ms: i64,
    store: Store,
    jobs: Jobs,
    config_tx: watch::Sender<Arc<Config>>,
    /// SHA-256 of the config file as last applied, so `setup` can tell whether a saved file is live.
    config_digest: Mutex<Option<String>>,
    control_latency: Latency,
    dedupe: Dedupe,
    shutdown: watch::Receiver<bool>,
    supervisor: Supervisor,
    lead: Arc<Lead<Headless>>,
    agent_start: Latency,
}

impl Hub {
    async fn status(&self) -> Result<Status, crate::store::StoreError> {
        let last_event_id = self.store.read(crate::store::Reads::last_event_id).await?;
        let now = now_ms();
        let omp = self.jobs.ctx().omp.report().map(|r| (*r).clone());
        let sessions = match &omp {
            Some(r) => r.sessions().err(),
            None => Some("OMP has not been probed yet".into()),
        };
        Ok(Status {
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: crate::proto::VERSION,
            pid: std::process::id(),
            started_ms: self.started_ms,
            uptime_s: (now - self.started_ms) / 1000,
            socket: self.paths.socket.display().to_string(),
            state_dir: self.paths.state_dir.display().to_string(),
            config_file: self.paths.config_file.display().to_string(),
            config_error: self.jobs.ctx().config_error(),
            config_digest: self
                .config_digest
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
            jobs: self.config_tx.borrow().jobs.len(),
            jobs_running: self.jobs.running_count(),
            last_event_id,
            task_restarts: self.supervisor.restarts(),
            panics: self.supervisor.panics(),
            omp,
            sessions_refused: sessions,
            quota_gate: self.jobs.gate(),
        })
    }

    async fn stats(&self) -> Stats {
        Stats {
            rss_bytes: host::self_rss().await.ok(),
            host_memory: host::memory().await.ok().map(Into::into),
            host_load: host::load().await.ok().map(Into::into),
            state_disk: host::disk(&self.paths.state_dir).ok().map(Into::into),
            control_latency: self.control_latency.summary().map(Into::into),
            store_commit_latency: self.store.commit_latency().summary().map(Into::into),
            wake_latency: self.lead.wake_latency.summary().map(Into::into),
            confirm_latency: self.lead.confirm_latency.summary().map(Into::into),
            agent_start_latency: self.agent_start.summary().map(Into::into),
        }
    }

    /// Reloads the config file. An invalid file is refused and the running config stays in force.
    async fn reload(&self) -> Result<usize, String> {
        let digest = config::file_digest(&self.paths.config_file);
        match config::load(&self.paths.config_file) {
            Ok(config) => {
                let jobs = config.jobs.len();
                log::set_level(config.hub.log_level);
                self.config_tx.send_replace(Arc::new(config));
                self.jobs.ctx().set_config_error(None);
                *self
                    .config_digest
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = digest;
                if let Err(e) = self.jobs.config_outcome(None).await {
                    log::warn("config event not recorded", &[("error", &e)]);
                }
                log::info("config reloaded", &[("jobs", &jobs)]);
                Ok(jobs)
            }
            Err(errors) => {
                let text = config::describe(&errors);
                self.jobs.ctx().set_config_error(Some(text.clone()));
                if let Err(e) = self.jobs.config_outcome(Some(&text)).await {
                    log::warn("config event not recorded", &[("error", &e)]);
                }
                log::warn(
                    "config refused; the previous one stays in force",
                    &[("error", &text)],
                );
                Err(format!(
                    "config not applied, the previous one stays in force: {text}"
                ))
            }
        }
    }
}

/// Prints a start-up refusal to stderr (the hub log may not exist yet).
fn refuse(message: &str) {
    crate::cli::out::error_line(message);
    log::error(message, &[]);
}

/// `cataly hub`: runs until a signal or a fatal condition; returns the process exit status.
pub(crate) fn run(paths: &Paths) -> ExitCode {
    for dir in [
        &paths.state_dir,
        &paths.logs_dir,
        &paths.files_dir,
        &paths.runtime_dir,
    ] {
        if let Err(e) = ensure_private_dir(dir) {
            refuse(&format!("cataly hub: {e}"));
            return ExitCode::from(exit::FAILED);
        }
    }
    if let Err(e) = log::to_file(paths.hub_log()) {
        refuse(&format!("cataly hub: hub log: {e}"));
        return ExitCode::from(exit::FAILED);
    }
    // A panic is logged with its location; the task's supervisor counts it and restarts the task.
    std::panic::set_hook(Box::new(|info| {
        let at = info
            .location()
            .map_or_else(String::new, |l| format!("{}:{}", l.file(), l.line()));
        log::error(
            "panic",
            &[
                ("at", &at),
                (
                    "message",
                    &info.payload_as_str().unwrap_or("non-text payload"),
                ),
            ],
        );
    }));
    let _lock = match lock::acquire(&paths.lock_file()) {
        Ok(LockOutcome::Acquired(lock)) => lock,
        Ok(LockOutcome::Held { pid }) => {
            let who = pid.map_or_else(|| "another process".to_string(), |p| format!("pid {p}"));
            refuse(&format!(
                "cataly hub is already running ({who}, socket {}); stop it first",
                paths.socket.display()
            ));
            return ExitCode::from(exit::ALREADY_RUNNING);
        }
        Err(e) => {
            refuse(&format!("cataly hub: lock: {e}"));
            return ExitCode::from(exit::FAILED);
        }
    };
    let config = match config::load(&paths.config_file) {
        Ok(config) => config,
        Err(errors) => {
            for e in &errors {
                refuse(&format!("cataly hub: config {e}"));
            }
            return ExitCode::from(exit::FAILED);
        }
    };
    log::set_level(config.hub.log_level);
    let (store, writer) = match Store::open(&paths.db_file(), &paths.backup_dir) {
        Ok(opened) => opened,
        Err(e) => {
            refuse(&format!("cataly hub: {e}"));
            return ExitCode::from(exit::FAILED);
        }
    };
    if let Err(e) = watch_writer(writer) {
        refuse(&format!("cataly hub: {e}"));
        return ExitCode::from(exit::FAILED);
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(8)
        .thread_name("cataly-hub")
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            refuse(&format!("cataly hub: runtime: {e}"));
            return ExitCode::from(exit::FAILED);
        }
    };
    let code = runtime.block_on(serve(paths, config, store));
    runtime.shutdown_timeout(Duration::from_secs(1));
    ExitCode::from(code)
}

async fn serve(paths: &Paths, config: Config, store: Store) -> u8 {
    let signals = match Signals::install() {
        Ok(s) => s,
        Err(e) => {
            refuse(&format!("cataly hub: signal handlers: {e}"));
            return exit::FAILED;
        }
    };
    let listener = match socket::bind(&paths.socket) {
        Ok(l) => Arc::new(l),
        Err(socket::BindError::Live) => {
            refuse(&format!(
                "cataly hub is already running (socket {} answers)",
                paths.socket.display()
            ));
            return exit::ALREADY_RUNNING;
        }
        Err(e) => {
            refuse(&format!("cataly hub: {e}"));
            return exit::FAILED;
        }
    };
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (supervisor, mut exit_rx) = Supervisor::new(Policy::HUB, shutdown_rx.clone());
    let job_count = config.jobs.len();
    let config_digest = config::file_digest(&paths.config_file);
    let (config_tx, config_rx) = watch::channel(Arc::new(config));
    let omp = Omp::new();
    let files = Files {
        dir: paths.files_dir.clone(),
        event_log: paths.event_log(),
        config_file: paths.config_file.clone(),
    };
    let ctx = Ctx::new(
        store.clone(),
        config_rx.clone(),
        files,
        Arc::clone(&omp),
        shutdown_rx.clone(),
    );
    let jobs = match Jobs::start(ctx, &supervisor).await {
        Ok(jobs) => jobs,
        Err(e) => {
            refuse(&format!("cataly hub: job recovery: {e}"));
            let _ = std::fs::remove_file(&paths.socket);
            return exit::FAILED;
        }
    };
    let agent_start = Latency::default();
    let lead = lead(paths, &jobs, &omp, &config_rx, &store, &agent_start);
    {
        let (lead, stop) = (Arc::clone(&lead), shutdown_rx.clone());
        supervisor.spawn("lead", move || Arc::clone(&lead).run(stop.clone()));
    }
    let hub = Arc::new(Hub {
        paths: paths.clone(),
        started_ms: now_ms(),
        store: store.clone(),
        jobs: jobs.clone(),
        config_tx,
        config_digest: Mutex::new(config_digest),
        control_latency: Latency::default(),
        dedupe: Dedupe::new(),
        shutdown: shutdown_rx.clone(),
        supervisor: supervisor.clone(),
        lead: Arc::clone(&lead),
        agent_start,
    });
    {
        let (hub, listener) = (Arc::clone(&hub), Arc::clone(&listener));
        supervisor.spawn("control", move || {
            socket::serve(Arc::clone(&hub), Arc::clone(&listener))
        });
    }
    let heartbeat = spawn_background(&supervisor, paths, omp, config_rx, &store, &shutdown_rx);
    let notifier = Arc::new(Notifier::from_env());
    let marker = paths.stall_marker();
    if let Err(e) = start_detector(heartbeat, Arc::clone(&notifier), marker.clone()) {
        log::warn("stall detector not started", &[("error", &e)]);
    }
    announce(&jobs, &marker, job_count).await;
    notifier.ready();
    log::info(
        "hub ready",
        &[("socket", &paths.socket.display()), ("jobs", &job_count)],
    );

    let code = wait_for_stop(&hub, signals, &mut exit_rx).await;
    notifier.stopping();
    log::info("hub stopping", &[("status", &code)]);
    let _ = shutdown_tx.send(true);
    // The lead closes cleanly so the next start resumes the same session (check 12).
    lead.shutdown().await;
    // Running job children get TERM, then KILL after the grace; their real end is recorded.
    jobs.drain(KILL_GRACE * 2 + Duration::from_secs(2)).await;
    let _ = std::fs::remove_file(&paths.socket);
    code
}

/// The standing lead with its headless runtime (DESIGN 4.2), started by its supervised task.
fn lead(
    paths: &Paths,
    jobs: &Jobs,
    omp: &Arc<Omp>,
    config: &watch::Receiver<Arc<Config>>,
    store: &Store,
    agent_start: &Latency,
) -> Arc<Lead<Headless>> {
    let gate = jobs.clone();
    let events = jobs.clone();
    let (state, socket, logs) = (
        paths.state_dir.clone(),
        paths.socket.clone(),
        paths.logs_dir.join("sessions"),
    );
    let agent_start = agent_start.clone();
    Lead::new(LeadEnv {
        config: config.clone(),
        omp: Arc::clone(omp),
        gate_held: Arc::new(move || gate.gate().held),
        events: Arc::new(move |emit: Emit| {
            let jobs = events.clone();
            tokio::spawn(async move {
                if let Err(e) = jobs.hub_event(emit).await {
                    log::warn("session event not recorded", &[("error", &e)]);
                }
            });
        }),
        store: store.clone(),
        state_dir: paths.state_dir.clone(),
        make: Box::new(move |config: &Config, exe: &Path| Headless {
            exe: exe.to_path_buf(),
            overlay: state.join("omp-overlay.yml"),
            socket: socket.clone(),
            logs: logs.clone(),
            credentials: config.sessions.credentials.clone(),
            extra_flags: config.sessions.extra_flags.clone(),
            spend_phrases: config.sessions.spend_phrases.clone(),
            agent_start: agent_start.clone(),
        }),
    })
}

/// The OMP probe, the heartbeat and store maintenance; returns the heartbeat the stall detector reads.
fn spawn_background(
    supervisor: &Supervisor,
    paths: &Paths,
    omp: Arc<Omp>,
    config: watch::Receiver<Arc<Config>>,
    store: &Store,
    shutdown: &watch::Receiver<bool>,
) -> Heartbeat {
    {
        // The probe runs after the socket answers, off the start-up path (DESIGN 2.3).
        let stop = shutdown.clone();
        let stderr_log = paths.logs_dir.join("omp-probe.err");
        supervisor.spawn("omp", move || {
            Arc::clone(&omp).watch(config.clone(), stderr_log.clone(), stop.clone())
        });
    }
    let heartbeat = Heartbeat::default();
    {
        let (beat, stop) = (heartbeat.clone(), shutdown.clone());
        supervisor.spawn("heartbeat", move || beat_loop(beat.clone(), stop.clone()));
    }
    {
        let (store, stop) = (store.clone(), shutdown.clone());
        supervisor.spawn("maintenance", move || {
            maintenance(store.clone(), stop.clone())
        });
    }
    heartbeat
}

async fn announce(jobs: &Jobs, marker: &Path, job_count: usize) {
    if let Ok(Some(bytes)) = crate::sys::fs::read_private(marker, 256) {
        let at = String::from_utf8_lossy(&bytes).trim().to_string();
        let emit = Emit::new(EventKind::HubStalled, Severity::Warning, false, "hub").with("at", at);
        if jobs.hub_event(emit).await.is_ok() {
            let _ = std::fs::remove_file(marker);
        }
    }
    let emit = Emit::new(EventKind::HubStarted, Severity::Info, false, "hub")
        .with("version", env!("CARGO_PKG_VERSION"))
        .with("jobs", job_count);
    if let Err(e) = jobs.hub_event(emit).await {
        log::warn("start event not recorded", &[("error", &e)]);
    }
}

/// Any failure of the store-writer thread ends the hub with status 70 (DESIGN 2.2).
fn watch_writer(writer: crate::store::WriterHandle) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("cataly-store-watch".into())
        .stack_size(64 * 1024)
        .spawn(move || {
            let failure = match writer.join() {
                Ok(Ok(())) => return,
                Ok(Err(e)) => e.to_string(),
                Err(_) => "store writer panicked".to_string(),
            };
            log::error("store writer failed; the hub exits", &[("error", &failure)]);
            std::process::exit(i32::from(exit::INTERNAL));
        })
        .map(drop)
}

fn start_detector(
    heartbeat: Heartbeat,
    notifier: Arc<Notifier>,
    marker: std::path::PathBuf,
) -> std::io::Result<()> {
    stall::spawn_detector(
        heartbeat,
        notifier,
        STALL_LIMIT,
        Duration::from_secs(1),
        move || {
            log::error(
                "main loop stalled; exiting so the service manager restarts the hub",
                &[],
            );
            let _ = write_atomic(&marker, jiff::Timestamp::now().to_string().as_bytes());
            std::process::exit(i32::from(exit::INTERNAL));
        },
    )
    .map(drop)
}

async fn beat_loop(beat: Heartbeat, mut stop: watch::Receiver<bool>) -> Result<(), String> {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = tick.tick() => beat.beat(),
            _ = stop.changed() => return Ok(()),
        }
    }
}

async fn maintenance(store: Store, mut stop: watch::Receiver<bool>) -> Result<(), String> {
    let mut wait = MAINTENANCE_FIRST;
    loop {
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            _ = stop.changed() => return Ok(()),
        }
        let files = store.maintain(now_ms()).await.map_err(|e| e.to_string())?;
        for file in &files {
            let _ = std::fs::remove_file(file);
        }
        log::info("store maintenance done", &[("files_removed", &files.len())]);
        wait = MAINTENANCE_EVERY;
    }
}

/// The signals the hub handles, installed before the socket answers so an early TERM is graceful.
struct Signals {
    term: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
}

impl Signals {
    fn install() -> std::io::Result<Self> {
        Ok(Self {
            term: signal(SignalKind::terminate())?,
            int: signal(SignalKind::interrupt())?,
            hup: signal(SignalKind::hangup())?,
        })
    }
}

/// Waits for a stop signal, a reload signal (handled in place), or an exit request from the supervisor.
async fn wait_for_stop(
    hub: &Hub,
    signals: Signals,
    exit_rx: &mut watch::Receiver<Option<u8>>,
) -> u8 {
    let Signals {
        mut term,
        mut int,
        mut hup,
    } = signals;
    loop {
        tokio::select! {
            _ = term.recv() => return exit::OK,
            _ = int.recv() => return exit::OK,
            _ = hup.recv() => {
                let _ = hub.reload().await;
            }
            changed = exit_rx.changed() => {
                if changed.is_err() {
                    return exit::INTERNAL;
                }
                if let Some(code) = *exit_rx.borrow() {
                    return code;
                }
            }
        }
    }
}
