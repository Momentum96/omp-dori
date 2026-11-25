//! The terminal runtime (DESIGN 4.3; HOST-30, HOST-31, EXT-20, EXT-21, EXT-25, EXT-26, EXT-30):
//! OMP runs its own TUI in a Tern block the owner can watch, and the hub controls it through the
//! bridge connection instead of rpc-ui.
//!
//! A start asks `tern new tab` for a block running `/usr/bin/env CATALY_LAUNCH=<token>
//! CATALY_SOCKET=<path> omp <flags>` and waits for that OMP's bridge hello with the single-use
//! token. From then on the session's [`Port`] holds the connection: events from the bridge move the
//! state, and commands (state, deliver, interrupt, model, goal, form answer, shutdown) go out as
//! numbered items the bridge answers with `bridge.reply`. Dialogs belong to the person at the
//! terminal: approvals block the session and are never answered here; only forms the bridge mirrors
//! may be answered remotely, first answer wins. A link silent for 45 s marks the session lost; the
//! same OMP process reconnecting with the same session restores it.
//!
//! Typed-but-not-submitted text (COORD-14) cannot arise: a delivery is one bridge call into the
//! session it names, never keystrokes on a screen.
//!
//! The state mutex is only ever held for synchronous updates, never across an await.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::{Instant, timeout};

use super::dialogs::{
    self, Dialog, DialogKind, Entry, EntryKind, Facts, Goal, GoalStatus, ReadState, Status,
    same_model,
};
use super::{
    Control, DeliverOutcome, Delivery, EventPage, Role, Runtime, SessionEvent, Snapshot,
    StartError, StartSpec, Started, StopRecord, Timing,
};
use crate::log;
use crate::omp::launch::{Launch, Target};
use crate::proto::Response;
use crate::sys::command::{self, KILL_GRACE, Output, Spec};
use crate::sys::latency::Latency;
use crate::sys::now_ms;

/// Shown when the terminal runtime is configured but Tern is not installed (HOST req. 92).
pub(crate) const INSTALL_HINT: &str = "the tern runtime is configured but the Tern executable was not \
found; install Tern and put `tern` on PATH, or set [sessions] tern to its absolute path, or set \
[sessions] runtime = \"headless\"";
/// A block's launch token is visible in the process list, so it is good for 60 s (DESIGN 4.3).
const TOKEN_LIFE_MS: i64 = 60_000;
/// `tern new` itself.
const TERN_CALL: Duration = Duration::from_secs(10);
/// Block opened to bridge hello (HOST req. 28).
const START_BUDGET: Duration = Duration::from_secs(30);
/// A command the bridge must answer.
const COMMAND_WAIT: Duration = Duration::from_secs(10);
/// After a shutdown request, how long the hub waits for OMP to go (DESIGN 4.2 close wait).
const CLOSE_WAIT: Duration = Duration::from_secs(15);
/// A link without any line for this long is lost (DESIGN 4.3, EXT req. 15).
const LOST_AFTER: Duration = Duration::from_secs(45);
/// The bridge takes at most this much text per delivery; longer text goes to a file (EXT req. 25).
const BRIDGE_TEXT_MAX: usize = 64 * 1024;
/// Largest delivery (HOST req. 50).
const TEXT_MAX: usize = 256 * 1024;
/// Output ring entries and kept events (HOST req. 40, 76).
const RING: usize = 40;
const EVENT_KEEP: usize = 256;
const MESSAGE_CUT: usize = 1536;

/// How the hub opens terminal sessions.
#[derive(Clone, Debug)]
pub(crate) struct Tern {
    /// The resolved Tern executable; `None` refuses every start with [`INSTALL_HINT`].
    pub(crate) program: Option<PathBuf>,
    /// The Tern session new blocks open in.
    pub(crate) block_session: Option<String>,
    pub(crate) omp: PathBuf,
    pub(crate) state: PathBuf,
    pub(crate) socket: PathBuf,
    /// Where deliveries over 64 KiB are written for the agent to read.
    pub(crate) spill: PathBuf,
    pub(crate) extra_flags: Vec<String>,
    /// Command sent to the turn's `agent_start` (tern wake latency, hub part).
    pub(crate) agent_start: Latency,
    pub(crate) links: Arc<super::bridge::Links>,
}

impl Tern {
    /// `tern new tab [SESSION] --cwd DIR -- /usr/bin/env CATALY_LAUNCH=.. CATALY_SOCKET=.. omp ..`.
    fn argv(&self, tern: &Path, spec: &StartSpec, token: &str) -> Vec<String> {
        let overlay = crate::omp::launch::overlay_path(&self.state, spec.role == Role::Lane);
        let omp = Launch {
            exe: &self.omp,
            model: &spec.model,
            thinking: spec.thinking.as_deref(),
            target: &spec.target,
            overlay: &overlay,
            instructions: spec.instructions.as_deref(),
            extra: &self.extra_flags,
            max_cost: spec.max_cost,
            tools: spec.tools.as_deref(),
        }
        .tui_argv();
        let mut argv = vec![tern.display().to_string(), "new".into(), "tab".into()];
        argv.extend(self.block_session.iter().cloned());
        argv.extend([
            "--json".into(),
            "--cwd".into(),
            spec.cwd.display().to_string(),
            "--".into(),
            "/usr/bin/env".into(),
            format!("CATALY_LAUNCH={token}"),
            format!("CATALY_SOCKET={}", self.socket.display()),
        ]);
        // The probe's variables (notifications off, profile overrides), as headless sessions get them.
        argv.extend(
            crate::omp::child_env()
                .into_iter()
                .map(|(k, v)| format!("{k}={v}")),
        );
        argv.extend(omp);
        argv
    }

    /// Asks Tern to close a block that never said hello.
    async fn close_block(tern: &Path, block: Option<&str>) {
        let Some(block) = block else { return };
        let argv = [tern.display().to_string(), "close".into(), block.into()];
        let _ = command::run(&tern_spec(&argv, None)).await;
    }
}

fn tern_spec<'a>(argv: &'a [String], cwd: Option<&'a Path>) -> Spec<'a> {
    Spec {
        argv,
        cwd,
        env: &[],
        timeout: TERN_CALL,
        kill_grace: KILL_GRACE,
        output_cap: 64 * 1024,
        output: Output::Capture,
    }
}

/// The block ID `tern new --json` printed, if any.
fn block_id(stdout: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(stdout).ok()?;
    let id = value.get("id").or_else(|| value.pointer("/block/id"))?;
    id.as_str()
        .map(str::to_string)
        .or_else(|| id.as_u64().map(|n| n.to_string()))
}

impl Runtime for Tern {
    type Session = Session;

    fn kind(&self) -> &'static str {
        "tern"
    }

    async fn start(&self, spec: StartSpec) -> Result<(Arc<Session>, Started), StartError> {
        let Some(tern) = self.program.clone() else {
            return Err(StartError::Refused(INSTALL_HINT.into()));
        };
        crate::sys::fs::ensure_private_dir(&self.spill)
            .map_err(|e| StartError::Refused(e.to_string()))?;
        let token = super::random_hex(16);
        self.links
            .issue_for(&token, &spec.handle, spec.role, TOKEN_LIFE_MS);
        let (port, attached) = Port::new(
            &spec,
            self.spill.clone(),
            self.agent_start.clone(),
            LOST_AFTER,
        );
        self.links.add_port(&spec.handle, Arc::clone(&port));
        {
            // The port leaves the registry with the session.
            let (links, handle, mut events) = (
                Arc::clone(&self.links),
                spec.handle.clone(),
                port.events.subscribe(),
            );
            tokio::spawn(async move {
                loop {
                    match events.recv().await {
                        Ok((_, SessionEvent::Stopped { .. }))
                        | Err(broadcast::error::RecvError::Closed) => break,
                        _ => {}
                    }
                }
                links.remove_port(&handle);
            });
        }
        let argv = self.argv(&tern, &spec, &token);
        let opened = command::run(&tern_spec(&argv, Some(&spec.cwd))).await;
        let block = match opened {
            Ok(done) if done.success() => block_id(&done.stdout),
            Ok(done) => {
                self.links.remove_port(&spec.handle);
                let err = String::from_utf8_lossy(&done.stderr).trim().to_string();
                return Err(StartError::Refused(format!(
                    "tern did not open a block ({:?}): {err}",
                    done.exit
                )));
            }
            Err(e) => {
                self.links.remove_port(&spec.handle);
                return Err(StartError::Refused(format!("tern: {e}")));
            }
        };
        let bound = match timeout(START_BUDGET, attached).await {
            Ok(Ok(())) => port.bind(&spec),
            _ => Err("no bridge hello from the Tern block within 30 s".into()),
        };
        match bound {
            Ok(started) => {
                port.block.lock().await.clone_from(&block);
                Ok((Arc::new(Session { port }), started))
            }
            Err(reason) => {
                self.links.remove_port(&spec.handle);
                port.record_stop("failed".into(), Some(reason.clone()));
                Self::close_block(&tern, block.as_deref()).await;
                Err(StartError::Failed(reason))
            }
        }
    }
}

/// The open bridge connection of a terminal session.
struct Wire {
    key: String,
    tx: mpsc::Sender<Response>,
}

// Independent facts about the link and the session; none implies another.
#[allow(clippy::struct_excessive_bools)]
#[derive(Default)]
struct State {
    wire: Option<Wire>,
    /// Bumped on every attach, so a stale lost timer does nothing.
    generation: u64,
    last_rx: Option<Instant>,
    lost: bool,
    pid: Option<u32>,
    /// The identity bound at start.
    bound: Option<(String, String)>,
    /// The session the bridge says is current (a user may switch).
    current: Option<(String, String)>,
    /// The bridge reported its state since the link came up.
    known: bool,
    idle: bool,
    model: Option<String>,
    thinking: Option<String>,
    goal: Option<Goal>,
    fault: Option<String>,
    held: Option<String>,
    asked_model: Option<String>,
    stopping: bool,
    stop: Option<StopRecord>,
    dialogs: Vec<Dialog>,
    /// Dialog IDs that are mirrored forms, the only ones the hub may answer.
    forms: HashSet<String>,
    ring: VecDeque<Entry>,
    turn_started_ms: Option<i64>,
    wake_written: Option<std::time::Instant>,
    pending: HashMap<u64, oneshot::Sender<Value>>,
    next: u64,
    seq: u64,
    log: VecDeque<(u64, SessionEvent)>,
    updated_ms: i64,
    attached: Option<oneshot::Sender<()>>,
}

/// The view token: changes with every start (HOST req. 78).
static VIEWS: AtomicU64 = AtomicU64::new(0);

/// The hub's side of one terminal session: the bridge feeds it, [`Session`] reads and drives it.
pub(crate) struct Port {
    handle: String,
    role: Role,
    cwd: String,
    started_ms: i64,
    requested_model: String,
    view: u64,
    spill: PathBuf,
    agent_start: Latency,
    /// A link silent this long marks the session lost (45 s; shorter in tests).
    lost_after: Duration,
    state: Mutex<State>,
    snapshot: watch::Sender<Arc<Snapshot>>,
    events: broadcast::Sender<(u64, SessionEvent)>,
    busy: AtomicBool,
    block: tokio::sync::Mutex<Option<String>>,
}

impl std::fmt::Debug for Port {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Port")
    }
}

fn text_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// The goal as the bridge reports it from OMP's session modes.
fn goal_from(v: Option<&Value>) -> Option<Goal> {
    let v = v?;
    let status = match v.get("status").and_then(Value::as_str)? {
        "inactive" | "none" => GoalStatus::None,
        "active" => GoalStatus::Active,
        "paused" => GoalStatus::Paused,
        "complete" => GoalStatus::Complete,
        "dropped" => GoalStatus::Dropped,
        "budget-limited" => GoalStatus::Blocked,
        _ => return None,
    };
    Some(Goal {
        status,
        objective: text_of(v, "objective").map(str::to_string),
    })
}

/// The delivery ID a batch carries in its first-line marker, `[cataly:<id>]`.
fn delivery_id(text: &str) -> Option<&str> {
    text.strip_prefix("[cataly:")?
        .split_once(']')
        .map(|(id, _)| id)
        .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric()))
}

impl Port {
    fn new(
        spec: &StartSpec,
        spill: PathBuf,
        agent_start: Latency,
        lost_after: Duration,
    ) -> (Arc<Self>, oneshot::Receiver<()>) {
        let now = now_ms();
        let (tx, rx) = oneshot::channel();
        let port = Arc::new(Self {
            handle: spec.handle.clone(),
            role: spec.role,
            cwd: spec.cwd.display().to_string(),
            started_ms: now,
            requested_model: spec.model.clone(),
            view: VIEWS.fetch_add(1, Ordering::Relaxed) + 1,
            spill,
            agent_start,
            lost_after,
            state: Mutex::new(State {
                updated_ms: now,
                attached: Some(tx),
                ..State::default()
            }),
            snapshot: watch::channel(Arc::new(Snapshot {
                handle: spec.handle.clone(),
                role: spec.role,
                runtime: "tern",
                cwd: spec.cwd.display().to_string(),
                started_ms: now,
                updated_ms: now,
                status: Status::Unknown,
                input_blocked: true,
                fresh: false,
                pid: None,
                session_id: None,
                session_file: None,
                goal: None,
                model: None,
                requested_model: spec.model.clone(),
                turn_started_ms: None,
                reason: None,
                quota_blocked: None,
                dialogs: Vec::new(),
                recent: Vec::new(),
                view: 0,
                stop: None,
                last_event: 0,
            }))
            .0,
            events: broadcast::channel(EVENT_KEEP).0,
            busy: AtomicBool::new(false),
            block: tokio::sync::Mutex::new(None),
        });
        (port, rx)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn status_of(st: &State) -> (Status, bool) {
        let read = st
            .current
            .as_ref()
            .filter(|_| st.wire.is_some() && st.known)
            .map(|(id, file)| ReadState {
                session_id: id.clone(),
                session_file: file.clone(),
                goal: st.goal.clone().unwrap_or(Goal {
                    status: GoalStatus::None,
                    objective: None,
                }),
                settled: st.idle,
                foreground: !st.idle,
                streaming: !st.idle,
                queued: 0,
                model: st.model.clone(),
                thinking: st.thinking.clone(),
                extra_usage: false,
            });
        dialogs::status(&Facts {
            bound: st.bound.is_some(),
            state: read.as_ref(),
            fault: st.fault.is_some(),
            held: st.held.is_some(),
            stopping: st.stopping || st.stop.is_some(),
            dialogs: !st.dialogs.is_empty(),
        })
    }

    fn publish(&self, st: &mut State) {
        st.updated_ms = now_ms();
        let (status, input_blocked) = Self::status_of(st);
        let current = st.current.as_ref().filter(|_| st.fault.is_none());
        let snapshot = Snapshot {
            handle: self.handle.clone(),
            role: self.role,
            runtime: "tern",
            cwd: self.cwd.clone(),
            started_ms: self.started_ms,
            updated_ms: st.updated_ms,
            status,
            input_blocked,
            fresh: st.wire.is_some() && st.known,
            pid: st.pid.filter(|_| st.stop.is_none()),
            session_id: current.map(|(id, _)| id.clone()),
            session_file: current.map(|(_, file)| file.clone()),
            goal: st.goal.clone().filter(|_| current.is_some()),
            model: st.model.clone(),
            requested_model: self.requested_model.clone(),
            turn_started_ms: st.turn_started_ms,
            reason: st.fault.clone().or_else(|| st.held.clone()).or_else(|| {
                st.lost
                    .then(|| "lost: the bridge was silent for 45 s".to_string())
            }),
            quota_blocked: None,
            dialogs: st.dialogs.clone(),
            recent: st.ring.iter().cloned().collect(),
            view: self.view,
            stop: st.stop.clone(),
            last_event: st.seq,
        };
        self.snapshot.send_replace(Arc::new(snapshot));
    }

    fn emit(&self, st: &mut State, event: SessionEvent) {
        st.seq += 1;
        st.log.push_back((st.seq, event.clone()));
        while st.log.len() > EVENT_KEEP {
            st.log.pop_front();
        }
        let _ = self.events.send((st.seq, event));
    }

    fn notice(st: &mut State, text: String) {
        st.ring.push_back(Entry {
            kind: EntryKind::Notice,
            text,
        });
        while st.ring.len() > RING {
            st.ring.pop_front();
        }
    }

    fn record_stop(&self, how: String, detail: Option<String>) -> StopRecord {
        let mut st = self.lock();
        if let Some(record) = st.stop.clone() {
            return record;
        }
        let record = StopRecord {
            how,
            detail,
            session_id: st.current.as_ref().map(|(id, _)| id.clone()),
            at_ms: now_ms(),
        };
        st.stop = Some(record.clone());
        st.dialogs.clear();
        st.forms.clear();
        st.wire = None;
        for (_, waiter) in st.pending.drain() {
            drop(waiter);
        }
        self.emit(
            &mut st,
            SessionEvent::Stopped {
                record: record.clone(),
            },
        );
        self.publish(&mut st);
        record
    }

    /// A bridge hello for this session (EXT req. 3-5; HOST req. 94, 97). The first one binds the
    /// identity; a reconnect must come from the same OMP process with the same session, and one
    /// with another session ends the old identity.
    pub(crate) fn attach(
        self: &Arc<Self>,
        hello: &Value,
        key: &str,
        tx: mpsc::Sender<Response>,
    ) -> Result<(), String> {
        let pid = hello
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|p| u32::try_from(p).ok());
        let session = text_of(hello, "session").unwrap_or_default().to_string();
        let file = text_of(hello, "file").unwrap_or_default().to_string();
        let mut st = self.lock();
        if st.stop.is_some() {
            return Err("this session has ended".into());
        }
        if st.bound.is_some() {
            if pid.is_none() || st.pid != pid {
                return Err("a reconnect from another OMP process".into());
            }
            if st.current.as_ref().map(|(id, _)| id.as_str()) != Some(session.as_str()) {
                drop(st);
                self.record_stop(
                    "exited".into(),
                    Some(format!(
                        "the bridge came back with another session ({session})"
                    )),
                );
                return Err("a different session: the old identity is ended".into());
            }
        } else {
            st.bound = Some((session.clone(), file.clone()));
        }
        st.pid = pid;
        st.current = Some((session, file));
        st.wire = Some(Wire {
            key: key.to_string(),
            tx,
        });
        st.generation += 1;
        st.last_rx = Some(Instant::now());
        Self::read_facts(&mut st, hello);
        if std::mem::take(&mut st.lost) {
            Self::notice(&mut st, "the bridge reconnected".into());
            self.emit(&mut st, SessionEvent::LinkRestored);
        }
        if let Some(waiter) = st.attached.take() {
            let _ = waiter.send(());
        }
        self.publish(&mut st);
        Ok(())
    }

    /// Idle, model, thinking and goal from a hello, snapshot or state answer.
    fn read_facts(st: &mut State, v: &Value) {
        if let Some(idle) = v.get("idle").and_then(Value::as_bool) {
            st.idle = idle;
            st.known = true;
        }
        if let Some(model) = text_of(v, "model") {
            st.model = Some(model.to_string());
        }
        if let Some(thinking) = text_of(v, "thinking") {
            st.thinking = Some(thinking.to_string());
        }
        if let Some(goal) = goal_from(v.get("goal")) {
            st.goal = Some(goal);
        }
    }

    /// The connection with `key` closed; the session is lost unless the bridge is back within 45 s
    /// of its last line (HOST req. 97).
    pub(crate) fn detach(self: &Arc<Self>, key: &str) {
        let (generation, due) = {
            let mut st = self.lock();
            if st.wire.as_ref().is_none_or(|w| w.key != key) {
                return;
            }
            st.wire = None;
            for (_, waiter) in st.pending.drain() {
                drop(waiter);
            }
            self.publish(&mut st);
            (
                st.generation,
                st.last_rx.unwrap_or_else(Instant::now) + self.lost_after,
            )
        };
        let port = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep_until(due).await;
            let mut st = port.lock();
            if st.generation != generation || st.wire.is_some() || st.stop.is_some() || st.lost {
                return;
            }
            st.lost = true;
            log::warn("tern session lost", &[("handle", &port.handle)]);
            Self::notice(&mut st, "the bridge was silent for 45 s".into());
            port.emit(&mut st, SessionEvent::LinkLost);
            port.publish(&mut st);
        });
    }

    /// Any line from the bridge keeps the link alive.
    pub(crate) fn heard(&self) {
        self.lock().last_rx = Some(Instant::now());
    }

    /// One `bridge.reply` to a numbered command.
    pub(crate) fn reply(&self, params: &Value) {
        let n = params.get("n").and_then(Value::as_u64).unwrap_or(0);
        let waiter = {
            let mut st = self.lock();
            st.last_rx = Some(Instant::now());
            st.pending.remove(&n)
        };
        if let Some(waiter) = waiter {
            let _ = waiter.send(params.clone());
        }
    }

    /// One `bridge.event` (or `bridge.switch`) from the session's main agent.
    pub(crate) fn event(&self, params: &Value) {
        let kind = params
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("session_switch");
        let mut st = self.lock();
        st.last_rx = Some(Instant::now());
        match kind {
            "snapshot" => Self::read_facts(&mut st, params),
            "agent_start" => {
                st.idle = false;
                st.turn_started_ms = Some(now_ms());
                if let Some(written) = st.wake_written.take() {
                    self.agent_start.record(written.elapsed());
                }
                self.emit(&mut st, SessionEvent::TurnStarted);
            }
            "agent_end" => {
                let will_continue =
                    params.get("will_continue").and_then(Value::as_bool) == Some(true);
                st.idle = !will_continue;
                st.turn_started_ms = None;
                self.emit(&mut st, SessionEvent::TurnEnded { will_continue });
                if !will_continue {
                    self.emit(&mut st, SessionEvent::Quiet);
                }
            }
            "message_end" => self.message(&mut st, params),
            "tool_approval_requested" | "plan_review_requested" => {
                // Belongs to the person at the terminal: blocks, never answerable here (HOST req. 96).
                let id = text_of(params, "call").unwrap_or(kind).to_string();
                let dialog = Dialog {
                    id,
                    title: text_of(params, "tool").map(str::to_string),
                    message: None,
                    kind: DialogKind::Unsupported,
                    options: None,
                    secret: false,
                    questions: None,
                    html: None,
                };
                st.dialogs.retain(|d| d.id != dialog.id);
                st.dialogs.push(dialog.clone());
                self.emit(&mut st, SessionEvent::DialogOpened { dialog });
            }
            "tool_approval_resolved" | "plan_review_resolved" | "form_closed" => {
                let id = text_of(params, "call").unwrap_or(kind).to_string();
                self.close_dialog(&mut st, &id);
            }
            "goal_updated" => {
                st.goal = goal_from(Some(params));
            }
            "session_switch" | "session_branch" | "session_tree" | "session_current" => {
                if let (Some(id), Some(file)) =
                    (text_of(params, "session"), text_of(params, "file"))
                {
                    let pair = (id.to_string(), file.to_string());
                    if st.current.as_ref() != Some(&pair) {
                        Self::notice(&mut st, format!("the terminal moved to session {id}"));
                        st.current = Some(pair);
                    }
                }
            }
            "session_clear" => Self::notice(&mut st, "the transcript was cleared".into()),
            "session_shutdown" => {
                let asked = st.stopping;
                drop(st);
                let how = if asked { "clean" } else { "exited" };
                self.record_stop(how.into(), Some("OMP in the Tern block shut down".into()));
                return;
            }
            _ => return,
        }
        self.publish(&mut st);
    }

    fn close_dialog(&self, st: &mut State, id: &str) {
        let before = st.dialogs.len();
        st.dialogs.retain(|d| d.id != id);
        st.forms.remove(id);
        if st.dialogs.len() != before {
            self.emit(st, SessionEvent::DialogClosed { id: id.to_string() });
        }
    }

    fn message(&self, st: &mut State, params: &Value) {
        let role = text_of(params, "role").unwrap_or_default().to_string();
        let raw = text_of(params, "text").unwrap_or_default();
        let clean = dialogs::clean(raw);
        let kept = dialogs::last_units(&clean, MESSAGE_CUT).to_string();
        let truncated = kept.len() < clean.len()
            || params.get("truncated").and_then(Value::as_bool) == Some(true);
        if role == "assistant" && !kept.trim().is_empty() {
            st.ring.push_back(Entry {
                kind: EntryKind::Assistant,
                text: kept.clone(),
            });
            while st.ring.len() > RING {
                st.ring.pop_front();
            }
        }
        self.emit(
            st,
            SessionEvent::Message {
                role,
                text: kept,
                truncated,
            },
        );
    }

    /// A form the bridge offers to mirror (EXT req. 42): the `ask` tool's questions become an ask
    /// dialog the relays may post; anything malformed stays local only.
    pub(crate) fn form(&self, params: &Value) -> Value {
        let Some(call) = text_of(params, "call") else {
            return json!({ "mirrored": false });
        };
        let questions: Vec<_> = params
            .pointer("/input/questions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(dialogs::question_of)
            .collect();
        let mut st = self.lock();
        st.last_rx = Some(Instant::now());
        if questions.is_empty() || st.stop.is_some() {
            return json!({ "mirrored": false });
        }
        let dialog = Dialog {
            id: call.to_string(),
            title: None,
            message: None,
            kind: DialogKind::Ask,
            options: None,
            secret: false,
            questions: Some(questions),
            html: None,
        };
        st.dialogs.retain(|d| d.id != call);
        st.dialogs.push(dialog.clone());
        st.forms.insert(call.to_string());
        self.emit(&mut st, SessionEvent::DialogOpened { dialog });
        self.publish(&mut st);
        json!({ "mirrored": true })
    }

    /// Sends one numbered command and waits for its reply; `Err` when nothing came back.
    async fn command(&self, op: &str, mut fields: Value) -> Result<Value, String> {
        let (tx, rx) = oneshot::channel();
        let (wire, n) = {
            let mut st = self.lock();
            let Some(wire) = st.wire.as_ref().map(|w| w.tx.clone()) else {
                return Err("the bridge is not connected".into());
            };
            st.next += 1;
            let n = st.next;
            st.pending.insert(n, tx);
            if let Some(map) = fields.as_object_mut() {
                map.insert("n".into(), json!(n));
                map.insert("op".into(), json!(op));
                if let Some((id, _)) = &st.current {
                    map.entry("session").or_insert_with(|| json!(id));
                }
            }
            (wire, n)
        };
        let sent = timeout(
            COMMAND_WAIT,
            wire.send(Response::item(0, json!({ "cmd": fields }))),
        )
        .await;
        if !matches!(sent, Ok(Ok(()))) {
            self.lock().pending.remove(&n);
            return Err("the bridge connection did not take the command".into());
        }
        if let Ok(Ok(reply)) = timeout(COMMAND_WAIT, rx).await {
            return Ok(reply);
        }
        self.lock().pending.remove(&n);
        Err(format!("the bridge did not answer {op} within 10 s"))
    }

    /// The identity check of a start (HOST req. 26-27): a resume must bind the session asked for,
    /// and a model other than the requested one holds the session.
    fn bind(&self, spec: &StartSpec) -> Result<Started, String> {
        let mut st = self.lock();
        let (id, file) = st.bound.clone().ok_or("no identity in the hello")?;
        if id.is_empty() || !Path::new(&file).is_absolute() {
            return Err("the bridge reported no session ID or no absolute transcript path".into());
        }
        if let (Some(expected), Target::Resume(want)) = (&spec.expected_id, &spec.target) {
            let same = std::fs::canonicalize(&file).ok() == std::fs::canonicalize(want).ok();
            if &id != expected || !same {
                return Err(format!(
                    "OMP resumed a different session: expected {expected} at {}, got {id} at {file}",
                    want.display()
                ));
            }
        }
        let held = match &st.model {
            Some(model) if same_model(model, &spec.model) => None,
            Some(model) => Some(format!(
                "model mismatch: {} requested, OMP reports {model}",
                spec.model
            )),
            None => Some(format!(
                "model mismatch: {} requested, OMP reports none",
                spec.model
            )),
        };
        st.held.clone_from(&held);
        self.publish(&mut st);
        Ok(Started {
            session_id: id,
            file,
            model: st.model.clone(),
            pid: st.pid,
            held,
        })
    }

    /// Text for the bridge: at most 64 KiB, the rest written to a file the agent is pointed at.
    fn fit(&self, text: String, id: &str) -> Result<String, String> {
        if text.len() <= BRIDGE_TEXT_MAX {
            return Ok(text);
        }
        let path = self.spill.join(format!("delivery-{id}.txt"));
        crate::sys::fs::write_atomic(&path, text.as_bytes()).map_err(|e| e.to_string())?;
        Ok(format!(
            "[cataly:{id}]\nThis message is over 64 KiB; read it from {}.",
            path.display()
        ))
    }
}

/// A terminal session.
pub(crate) struct Session {
    port: Arc<Port>,
}

/// Clears the busy flag when an operation ends.
struct Busy<'a>(&'a AtomicBool);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Session {
    fn begin(&self) -> Result<Busy<'_>, String> {
        if self.port.busy.swap(true, Ordering::AcqRel) {
            return Err("busy: another change is in flight".into());
        }
        Ok(Busy(&self.port.busy))
    }

    fn refused(reply: &Value) -> Option<String> {
        text_of(reply, "refused").map(str::to_string)
    }
}

impl Control for Session {
    fn snapshot(&self) -> Arc<Snapshot> {
        Arc::clone(&self.port.snapshot.borrow())
    }

    fn states(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.port.snapshot.subscribe()
    }

    fn subscribe(&self) -> broadcast::Receiver<(u64, SessionEvent)> {
        self.port.events.subscribe()
    }

    fn events_after(&self, after: u64, limit: usize) -> EventPage {
        let st = self.port.lock();
        let oldest = st.log.front().map_or(st.seq + 1, |(n, _)| *n);
        EventPage {
            missed: after + 1 < oldest,
            events: st
                .log
                .iter()
                .filter(|(n, _)| *n > after)
                .take(limit.clamp(1, super::headless::EVENT_PAGE))
                .cloned()
                .collect(),
        }
    }

    fn alive(&self) -> bool {
        self.port.lock().stop.is_none()
    }

    async fn refresh(&self) -> Result<(), String> {
        let reply = self.port.command("state", json!({})).await?;
        let mut st = self.port.lock();
        Port::read_facts(&mut st, &reply);
        if let (Some(id), Some(file)) = (text_of(&reply, "session"), text_of(&reply, "file")) {
            st.current = Some((id.to_string(), file.to_string()));
        }
        self.port.publish(&mut st);
        Ok(())
    }

    async fn deliver(&self, d: Delivery) -> DeliverOutcome {
        let snapshot = self.snapshot();
        let Ok(_busy) = self.begin() else {
            return DeliverOutcome::NotReady(snapshot.status);
        };
        let status = snapshot.status;
        if snapshot.input_blocked
            || snapshot.session_id.as_deref() != Some(d.expected_session.as_str())
            || d.text.trim().is_empty()
        {
            return DeliverOutcome::NotReady(status);
        }
        if d.text.len() > TEXT_MAX {
            return DeliverOutcome::Refused("text over 256 KiB".into());
        }
        let timing_ok = match d.timing {
            Timing::Idle => status == Status::Idle,
            Timing::Working => status == Status::Working,
            Timing::Any => matches!(status, Status::Idle | Status::Working),
        };
        if !timing_ok {
            return DeliverOutcome::NotReady(status);
        }
        let idle = status == Status::Idle;
        if d.steer && idle {
            return DeliverOutcome::Refused("no running turn to steer".into());
        }
        let id = delivery_id(&d.text).map_or_else(|| super::random_hex(8), str::to_string);
        let text = match self.port.fit(d.text, &id) {
            Ok(text) => text,
            Err(e) => return DeliverOutcome::Refused(format!("spill file: {e}")),
        };
        let mode = if d.steer { "steer" } else { "wake" };
        let written = std::time::Instant::now();
        let fields = json!({ "id": id, "text": text, "mode": mode, "session": d.expected_session });
        match self.port.command("deliver", fields).await {
            Ok(reply) => match Self::refused(&reply) {
                // The user is switching or another session is current: kept pending (PATCH-01 rule 7).
                Some(r) if r == "not current" => DeliverOutcome::NotReady(status),
                Some(r) => DeliverOutcome::Refused(r),
                None => {
                    let turn_started = idle && !d.steer;
                    if turn_started {
                        self.port.lock().wake_written = Some(written);
                    }
                    DeliverOutcome::Delivered {
                        turn_started,
                        written: Some(written),
                    }
                }
            },
            Err(e) => DeliverOutcome::InDoubt(format!("delivery unconfirmed, do not resend: {e}")),
        }
    }

    async fn interrupt(&self) -> Result<String, String> {
        let _busy = self.begin()?;
        let reply = self.port.command("interrupt", json!({})).await?;
        if let Some(r) = Self::refused(&reply) {
            return Err(r);
        }
        Ok(
            if reply.get("idle").and_then(Value::as_bool) == Some(true) {
                "idle".into()
            } else {
                "interrupted".into()
            },
        )
    }

    async fn set_model(&self, selector: &str) -> Result<String, String> {
        let _busy = self.begin()?;
        if self.snapshot().status != Status::Idle {
            return Err("refused while a turn is running".into());
        }
        let reply = self
            .port
            .command("model", json!({ "model": selector }))
            .await?;
        if let Some(r) = Self::refused(&reply) {
            return Err(r);
        }
        let observed = text_of(&reply, "model").unwrap_or_default().to_string();
        let base = selector.split(':').next().unwrap_or(selector);
        if !same_model(&observed, base) {
            return Err(format!(
                "OMP reports {observed} after asking for {selector}"
            ));
        }
        let mut st = self.port.lock();
        st.model = Some(observed.clone());
        st.asked_model = Some(selector.to_string());
        self.port.emit(
            &mut st,
            SessionEvent::ModelChanged {
                model: observed.clone(),
            },
        );
        self.port.publish(&mut st);
        Ok(observed)
    }

    async fn goal(&self, op: &str, _objective: Option<&str>) -> Result<Goal, String> {
        if op == "get" {
            self.refresh().await?;
        } else if op == "pause" || op == "resume" {
            let _busy = self.begin()?;
            let reply = self.port.command("goal", json!({ "action": op })).await?;
            if let Some(r) = Self::refused(&reply) {
                return Err(r);
            }
            let mut st = self.port.lock();
            if let Some(goal) = goal_from(reply.get("goal")) {
                st.goal = Some(goal);
            }
            self.port.publish(&mut st);
        } else {
            return Err(format!(
                "goal {op}: a terminal session takes only pause and resume from the hub"
            ));
        }
        self.snapshot()
            .goal
            .clone()
            .ok_or_else(|| "the goal was not reported".into())
    }

    async fn answer(
        &self,
        session_id: &str,
        dialog_id: &str,
        answer: &Value,
    ) -> Result<(), String> {
        let dialog = {
            let st = self.port.lock();
            if st.current.as_ref().map(|(id, _)| id.as_str()) != Some(session_id) {
                return Err("not the current session".into());
            }
            if !st.forms.contains(dialog_id) {
                return Err(
                    "a dialog in a terminal session belongs to the person at the terminal".into(),
                );
            }
            st.dialogs
                .iter()
                .find(|d| d.id == dialog_id)
                .cloned()
                .ok_or("no such dialog")?
        };
        let frame =
            dialogs::answer_frame(&dialog, answer).ok_or("not a valid answer to this form")?;
        let reply = self
            .port
            .command(
                "form",
                json!({ "call": dialog_id, "answer": frame.to_map() }),
            )
            .await?;
        if let Some(r) = Self::refused(&reply) {
            return Err(r);
        }
        let mut st = self.port.lock();
        self.port.close_dialog(&mut st, dialog_id);
        self.port.publish(&mut st);
        Ok(())
    }

    fn accept_model(&self) -> Result<(), String> {
        let mut st = self.port.lock();
        if st.held.take().is_none() {
            return Err("the session is not held".into());
        }
        self.port.publish(&mut st);
        Ok(())
    }

    async fn close(&self, force: bool) -> Result<StopRecord, String> {
        if let Some(record) = self.port.lock().stop.clone() {
            return Ok(record);
        }
        let _busy = self.begin()?;
        let reply = self
            .port
            .command("shutdown", json!({ "force": force }))
            .await;
        match reply {
            Ok(r) => {
                if let Some(why) = Self::refused(&r) {
                    return Err(why);
                }
            }
            Err(e) if self.port.lock().wire.is_some() => return Err(e),
            Err(_) => {}
        }
        self.port.lock().stopping = true;
        let mut states = self.states();
        let gone = timeout(CLOSE_WAIT, async {
            loop {
                if states.borrow_and_update().stop.is_some() {
                    return;
                }
                let detached = self.port.lock().wire.is_none();
                if detached || states.changed().await.is_err() {
                    return;
                }
            }
        })
        .await;
        let detail = gone
            .is_err()
            .then(|| "OMP did not go within 15 s".to_string());
        Ok(self.port.record_stop("clean".into(), detail))
    }
}

#[cfg(test)]
mod tests;
