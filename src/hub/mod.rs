//! The hub (DESIGN 2): start-up in a fixed order, the single-instance lock, the control socket,
//! supervision, liveness and graceful shutdown.
//!
//! Start-up order: directories, hub log, lock, config, store (with migrations), socket, job engine
//! (recovery first), supervised tasks, stall detector, then `READY=1`.

pub(crate) mod bridge;
pub(crate) mod dispatch;
mod onboarding;
pub(crate) mod socket;
mod streams;
pub(crate) mod supervisor;
mod to_lead;
mod tool;
#[cfg(feature = "voice")]
pub(crate) mod voice;

use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, watch};

use crate::cli::exit;
use crate::config::{self, Config};
use crate::host::headless::Headless;
use crate::host::lead::{Env as LeadEnv, Events, Lead};
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
    lanes: Arc<crate::lanes::Lanes<Headless>>,
    agent_start: Latency,
    pub(crate) discord: Arc<crate::relay::intake::Shared>,
    pub(crate) telegram: Arc<crate::relay::intake::Shared>,
    pub(crate) slack: Arc<crate::relay::intake::Shared>,
    links: Arc<crate::host::bridge::Links>,
    streams: Arc<streams::Streams>,
    #[cfg(feature = "voice")]
    pub(crate) voice: Arc<voice::Voice<Headless>>,
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
            discord_wake_latency: self.discord.wake_latency.summary().map(Into::into),
            discord_catch_up_latency: self.discord.catch_up_latency.summary().map(Into::into),
            telegram_wake_latency: self.telegram.wake_latency.summary().map(Into::into),
            telegram_receive_lag: self.telegram.receive_lag.summary().map(Into::into),
            slack_wake_latency: self.slack.wake_latency.summary().map(Into::into),
            slack_catch_up_latency: self.slack.catch_up_latency.summary().map(Into::into),
            lane_start_latency: self.lanes.start_latency.summary().map(Into::into),
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

    fn config(&self) -> Arc<Config> {
        Arc::clone(&self.config_tx.borrow())
    }

    /// Stops the hub in order unless a session works or waits on a dialog (IR req. 33); busy
    /// sessions are never stopped.
    fn request_stop(&self) -> Result<(), String> {
        let mut busy: Vec<String> = self
            .lead
            .busy()
            .map(|why| format!("the lead {why}"))
            .into_iter()
            .collect();
        busy.extend(
            self.lanes
                .busy()
                .into_iter()
                .map(|(key, why)| format!("lane {key} {why}")),
        );
        if !busy.is_empty() {
            return Err(format!("busy: {}", busy.join("; ")));
        }
        log::info("orderly stop requested", &[]);
        self.supervisor.request_exit(exit::OK);
        Ok(())
    }

    /// The Discord client when a token is set up; `None` otherwise.
    fn discord_rest(&self) -> Option<Arc<crate::relay::discord::Rest>> {
        self.jobs.ctx().discord(&self.config()).ok()
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
    // A voice build's voice task reports this itself, with the missing key named too.
    #[cfg(not(feature = "voice"))]
    if let Some(why) = config.voice.refusal() {
        log::warn("voice is off", &[("reason", &why)]);
    }
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

/// Signal handlers first (an early TERM is still graceful), then the control socket. `Err` is the
/// exit status after the refusal was reported.
fn open(paths: &Paths) -> Result<(Signals, Arc<tokio::net::UnixListener>), u8> {
    let signals = Signals::install().map_err(|e| {
        refuse(&format!("cataly hub: signal handlers: {e}"));
        exit::FAILED
    })?;
    match socket::bind(&paths.socket) {
        Ok(l) => Ok((signals, Arc::new(l))),
        Err(socket::BindError::Live) => {
            refuse(&format!(
                "cataly hub is already running (socket {} answers)",
                paths.socket.display()
            ));
            Err(exit::ALREADY_RUNNING)
        }
        Err(e) => {
            refuse(&format!("cataly hub: {e}"));
            Err(exit::FAILED)
        }
    }
}

/// Where the lead's stream jobs run: the lead's working directory, else home, else `/`.
fn stream_cwd(config: &Config) -> std::path::PathBuf {
    config
        .lead
        .as_ref()
        .and_then(|l| l.cwd.clone())
        .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from))
        .unwrap_or_else(|| std::path::PathBuf::from("/"))
}

#[allow(clippy::too_many_lines)]
async fn serve(paths: &Paths, config: Config, store: Store) -> u8 {
    let (signals, listener) = match open(paths) {
        Ok(opened) => opened,
        Err(code) => return code,
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
    let links = Arc::new(crate::host::bridge::Links::default());
    let lead_cwd = stream_cwd(&config_rx.borrow());
    let streams = streams::Streams::new(
        jobs.clone(),
        store.clone(),
        lead_cwd,
        paths.files_dir.clone(),
    );
    let (lead, lanes, agent_start, relays) = sessions(
        paths,
        &jobs,
        &omp,
        &config_rx,
        &store,
        &supervisor,
        &shutdown_rx,
        &links,
    );
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
        lanes: Arc::clone(&lanes),
        agent_start,
        discord: relays.discord,
        telegram: relays.telegram,
        slack: relays.slack,
        links,
        streams: Arc::clone(&streams),
        #[cfg(feature = "voice")]
        voice: relays.voice,
    });
    {
        let (hub, listener) = (Arc::clone(&hub), Arc::clone(&listener));
        supervisor.spawn("control", move || {
            socket::serve(Arc::clone(&hub), Arc::clone(&listener))
        });
    }
    spawn_lead_tasks(&hub, &supervisor, &shutdown_rx);
    spawn_release_tasks(&jobs, &supervisor, paths, &shutdown_rx);
    streams.resume().await;
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
    // Lanes end their input; the next hub brings them back on their files (decision 9).
    lanes.shutdown().await;
    // Running job children get TERM, then KILL after the grace; their real end is recorded.
    jobs.drain(KILL_GRACE * 2 + Duration::from_secs(2)).await;
    let _ = std::fs::remove_file(&paths.socket);
    code
}

/// The intake state of each relay.
pub(crate) struct RelayShares {
    discord: Arc<crate::relay::intake::Shared>,
    telegram: Arc<crate::relay::intake::Shared>,
    slack: Arc<crate::relay::intake::Shared>,
    #[cfg(feature = "voice")]
    voice: Arc<voice::Voice<Headless>>,
}

/// What `sessions` starts: the lead, the lanes, the `agent_start` ring and the relays' shared state.
type Sessions = (
    Arc<Lead<Headless>>,
    Arc<crate::lanes::Lanes<Headless>>,
    Latency,
    RelayShares,
);

/// The lead, the lanes and Discord intake, each under its supervised task; returns the lead, the
/// lanes, the `agent_start` latency ring and intake's shared state.
#[allow(clippy::too_many_arguments)]
fn sessions(
    paths: &Paths,
    jobs: &Jobs,
    omp: &Arc<Omp>,
    config: &watch::Receiver<Arc<Config>>,
    store: &Store,
    supervisor: &Supervisor,
    shutdown: &watch::Receiver<bool>,
    links: &Arc<crate::host::bridge::Links>,
) -> Sessions {
    let agent_start = Latency::default();
    let events = hub_events(jobs);
    let gate = jobs.clone();
    let lead = Lead::new(LeadEnv {
        config: config.clone(),
        omp: Arc::clone(omp),
        gate_held: Arc::new(move || gate.gate().held),
        bridge_link: Arc::new(crate::omp::extension::check),
        events: Arc::clone(&events),
        store: store.clone(),
        state_dir: paths.state_dir.clone(),
        make: runtime(paths, &agent_start, links),
    });
    {
        let (lead, stop) = (Arc::clone(&lead), shutdown.clone());
        supervisor.spawn("lead", move || Arc::clone(&lead).run(stop.clone()));
    }
    let lanes = lanes(
        paths,
        jobs,
        omp,
        config,
        store,
        &agent_start,
        &events,
        links,
        &lead,
    );
    {
        let (lanes, stop) = (Arc::clone(&lanes), shutdown.clone());
        supervisor.spawn("lanes", move || Arc::clone(&lanes).run(stop.clone()));
    }
    {
        let (slots, stop) = (Arc::clone(&lanes.slots), shutdown.clone());
        supervisor.spawn("slots", move || Arc::clone(&slots).run(stop.clone()));
        let (lanes, stop) = (Arc::clone(&lanes), shutdown.clone());
        supervisor.spawn("planner", move || {
            Arc::clone(&lanes).plan_loop(stop.clone())
        });
    }
    #[cfg(feature = "voice")]
    let (voice, tap) = voice_task(
        paths,
        jobs,
        (&lead, &lanes),
        config,
        store,
        supervisor,
        shutdown,
    );
    #[cfg(not(feature = "voice"))]
    let tap = None;
    let discord = discord(
        paths,
        jobs,
        (&lead, &lanes),
        &events,
        config,
        store,
        (supervisor, tap),
        shutdown,
    );
    let env = Arc::new(crate::relay::intake::Env {
        config: config.clone(),
        store: store.clone(),
        lead: Arc::clone(&lead),
        lanes: Arc::clone(&lanes),
        events: Arc::clone(&events),
        spill_dir: paths.files_dir.join("relay"),
        config_file: paths.config_file.clone(),
    });
    let telegram = Arc::new(crate::relay::intake::Shared::default());
    {
        let (task, env, stop) = (Arc::clone(&telegram), Arc::clone(&env), shutdown.clone());
        supervisor.spawn("telegram", move || {
            crate::relay::telegram::run(Arc::clone(&task), Arc::clone(&env), stop.clone())
        });
    }
    let slack = Arc::new(crate::relay::intake::Shared::default());
    {
        let (task, stop) = (Arc::clone(&slack), shutdown.clone());
        supervisor.spawn("slack", move || {
            crate::relay::slack::run(Arc::clone(&task), Arc::clone(&env), stop.clone())
        });
    }
    (
        lead,
        lanes,
        agent_start,
        RelayShares {
            discord,
            telegram,
            slack,
            #[cfg(feature = "voice")]
            voice,
        },
    )
}

/// The voice task and its tap on the Discord Gateway connection (DESIGN 5.2).
#[cfg(feature = "voice")]
fn voice_task(
    paths: &Paths,
    jobs: &Jobs,
    (lead, lanes): (&Arc<Lead<Headless>>, &Arc<crate::lanes::Lanes<Headless>>),
    config: &watch::Receiver<Arc<Config>>,
    store: &Store,
    supervisor: &Supervisor,
    shutdown: &watch::Receiver<bool>,
) -> (
    Arc<voice::Voice<Headless>>,
    Option<Arc<crate::relay::discord::tap::Tap>>,
) {
    let (ops_tx, ops) = mpsc::channel(16);
    let (rest_jobs, rest_config) = (jobs.clone(), config.clone());
    let voice = voice::Voice::new(
        config.clone(),
        paths.config_file.clone(),
        crate::host::voice::HubVoice {
            store: store.clone(),
            lead: Arc::clone(lead),
            lanes: Arc::clone(lanes),
            config: config.clone(),
            rest: Arc::new(move || rest_jobs.ctx().discord(&rest_config.borrow())),
            ops: ops_tx,
        },
    );
    let tap = voice.tap(ops);
    let (task, stop) = (Arc::clone(&voice), shutdown.clone());
    supervisor.spawn("voice", move || Arc::clone(&task).run(stop.clone()));
    (voice, Some(tap))
}

/// How the hub starts a headless session (DESIGN 4.2); the lead and every lane use it.
fn runtime(
    paths: &Paths,
    agent_start: &Latency,
    links: &Arc<crate::host::bridge::Links>,
) -> crate::host::lead::Factory<Headless> {
    let (state, socket, logs) = (
        paths.state_dir.clone(),
        paths.socket.clone(),
        paths.logs_dir.join("sessions"),
    );
    let agent_start = agent_start.clone();
    let links = Arc::clone(links);
    Box::new(move |config: &Config, exe: &Path| Headless {
        exe: exe.to_path_buf(),
        state: state.clone(),
        socket: socket.clone(),
        logs: logs.clone(),
        credentials: config.sessions.credentials.clone(),
        extra_flags: config.sessions.extra_flags.clone(),
        spend_phrases: config.sessions.spend_phrases.clone(),
        agent_start: agent_start.clone(),
        links: Arc::clone(&links),
    })
}

/// The lanes (milestone 3a) with their oversight task.
#[allow(clippy::too_many_arguments)]
fn lanes(
    paths: &Paths,
    jobs: &Jobs,
    omp: &Arc<Omp>,
    config: &watch::Receiver<Arc<Config>>,
    store: &Store,
    agent_start: &Latency,
    events: &Events,
    links: &Arc<crate::host::bridge::Links>,
    lead: &Arc<Lead<Headless>>,
) -> Arc<crate::lanes::Lanes<Headless>> {
    let (gate, rest, lead) = (jobs.clone(), jobs.clone(), Arc::clone(lead));
    crate::lanes::Lanes::new(crate::lanes::Env {
        config: config.clone(),
        omp: Arc::clone(omp),
        store: store.clone(),
        state_dir: paths.state_dir.clone(),
        events: Arc::clone(events),
        make: runtime(paths, agent_start, links),
        gate_held: Arc::new(move || gate.gate().held),
        bridge_link: Arc::new(crate::omp::extension::check),
        lead_live: Arc::new(move || lead.snapshot().is_some()),
        threads: Arc::new(crate::relay::discord::threads::DiscordThreads {
            config: config.clone(),
            rest: Arc::new(move |config: &Config| rest.ctx().discord(config)),
        }),
        boards: Arc::new(crate::relay::boards::ChatBoards {
            config: config.clone(),
            config_file: paths.config_file.clone(),
            store: store.clone(),
        }),
    })
}

/// Events to the lead, owed-reply reminders and the unanswered watch, each a supervised task.
fn spawn_lead_tasks(hub: &Arc<Hub>, supervisor: &Supervisor, shutdown: &watch::Receiver<bool>) {
    {
        let (hub, stop) = (Arc::clone(hub), shutdown.clone());
        supervisor.spawn("to-lead", move || {
            to_lead::route(Arc::clone(&hub), stop.clone())
        });
    }
    {
        let (hub, stop) = (Arc::clone(hub), shutdown.clone());
        supervisor.spawn("reminders", move || {
            to_lead::remind(Arc::clone(&hub), stop.clone())
        });
    }
    {
        let (hub, stop) = (Arc::clone(hub), shutdown.clone());
        supervisor.spawn("unanswered", move || {
            to_lead::others(Arc::clone(&hub), stop.clone())
        });
    }
}

/// Events of the lead and the relays, recorded one at a time in the order they were emitted (a task
/// per event could commit `session.stopped` after the `lead.started` that followed it).
fn hub_events(jobs: &Jobs) -> Events {
    let (tx, mut rx) = mpsc::unbounded_channel::<Emit>();
    let jobs = jobs.clone();
    tokio::spawn(async move {
        while let Some(emit) = rx.recv().await {
            if let Err(e) = jobs.hub_event(emit).await {
                log::warn("hub event not recorded", &[("error", &e)]);
            }
        }
    });
    Arc::new(move |emit: Emit| {
        // Closed only when the runtime shuts down.
        let _ = tx.send(emit);
    })
}

/// Discord intake and replies (DESIGN 5.1), a supervised task that idles until the config names the
/// owner and channels.
#[allow(clippy::too_many_arguments)]
fn discord(
    paths: &Paths,
    jobs: &Jobs,
    (lead, lanes): (&Arc<Lead<Headless>>, &Arc<crate::lanes::Lanes<Headless>>),
    events: &Events,
    config: &watch::Receiver<Arc<Config>>,
    store: &Store,
    (supervisor, tap): (&Supervisor, Option<Arc<crate::relay::discord::tap::Tap>>),
    shutdown: &watch::Receiver<bool>,
) -> Arc<crate::relay::intake::Shared> {
    use crate::relay::discord::intake::{Env, Shared, run};
    let shared = Arc::new(Shared::default());
    let rest = jobs.clone();
    let env = Arc::new(Env {
        config: config.clone(),
        store: store.clone(),
        lead: Arc::clone(lead),
        lanes: Arc::clone(lanes),
        events: Arc::clone(events),
        rest: Arc::new(move |config: &Config| rest.ctx().discord(config)),
        spill_dir: paths.files_dir.join("relay"),
        tap,
    });
    let (task, stop) = (Arc::clone(&shared), shutdown.clone());
    supervisor.spawn("discord", move || {
        run(Arc::clone(&task), Arc::clone(&env), stop.clone())
    });
    shared
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

/// The daily release notice and the opt-in usage counts.
fn spawn_release_tasks(
    jobs: &Jobs,
    supervisor: &Supervisor,
    paths: &Paths,
    shutdown: &watch::Receiver<bool>,
) {
    let root = crate::sys::install::Layout::of_running().map_or_default(|l| l.root);
    {
        let (jobs, stop) = (jobs.clone(), shutdown.clone());
        supervisor.spawn("update", move || {
            crate::jobs::update::run(jobs.clone(), root.clone(), stop.clone())
        });
    }
    let (jobs, stop, state) = (jobs.clone(), shutdown.clone(), paths.state_dir.clone());
    supervisor.spawn("usage", move || {
        crate::jobs::usage::run(jobs.clone(), state.clone(), stop.clone())
    });
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
