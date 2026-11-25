//! The headless runtime (DESIGN 4.2): OMP as the hub's child in rpc-ui mode. One event task per
//! session owns the event stream and applies every change to the session state in order; operations
//! (deliver, interrupt, model, answer, close) run one at a time, refuse a second at once as busy, and
//! publish the new snapshot on a watch channel so status reads never wait (HOST-21).
//!
//! The state mutex is only ever held for synchronous updates, never across an await.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc, watch};
use tokio::time::timeout;

use super::dialogs::{
    self, AnswerFrame, Dialog, Entry, Facts, ReadState, Status, UiFrame, read_state, same_model,
    spend_offer,
};
use super::{
    Control, DeliverOutcome, Delivery, EventPage, Role, Runtime, SessionEvent, Snapshot,
    StartError, StartSpec, Started, StopRecord, Timing,
};
use crate::log;
use crate::omp::launch::{Launch, Target, session_env};
use crate::omp::rpc::{self, RESPONSE_TIMEOUT, Rpc, RpcError};
use crate::sys::command::{self, Attached, Exit, KILL_GRACE, Step};
use crate::sys::latency::Latency;
use crate::sys::now_ms;

/// Ready frame (HOST req. 23).
const READY_WITHIN: Duration = Duration::from_secs(15);
/// Whole start, ready to bound identity (HOST req. 28).
const START_BUDGET: Duration = Duration::from_secs(30);
/// After end of input before TERM (DESIGN 4.2, decision 8).
const CLOSE_WAIT: Duration = Duration::from_secs(15);
/// A forced close waits this long for the interrupted turn to stop (HOST req. 66).
const FORCE_IDLE_WAIT: Duration = Duration::from_secs(15);
/// Child stderr kept per session (HOST req. 2).
const STDERR_CAP: u64 = 1024 * 1024;
/// Events queued between OMP's reader and the session's event task. A full queue drops frames, which
/// ends trust in the stream (a dropped frame could hide an approval).
const EVENT_QUEUE: usize = 4096;
/// Output ring entries (HOST req. 40).
const RING: usize = 40;
/// Numbered events kept per session (HOST req. 76).
const EVENT_KEEP: usize = 256;
/// Event page size on demand (HOST req. 76).
pub(crate) const EVENT_PAGE: usize = 40;
/// Message text in events (HOST req. 77).
const MESSAGE_CUT: usize = 1536;
/// Largest delivery (HOST req. 50).
const TEXT_MAX: usize = 256 * 1024;
/// Spend offers within this window fence the session at the limit (DESIGN 9.2, check 8).
const SPEND_WINDOW_MS: i64 = 10 * 60 * 1000;
const SPEND_LIMIT: usize = 3;

/// How the hub launches headless sessions.
#[derive(Clone, Debug)]
pub(crate) struct Headless {
    pub(crate) exe: PathBuf,
    pub(crate) overlay: PathBuf,
    pub(crate) socket: PathBuf,
    /// Directory of the per-session stderr logs.
    pub(crate) logs: PathBuf,
    pub(crate) credentials: Vec<String>,
    pub(crate) extra_flags: Vec<String>,
    pub(crate) spend_phrases: Vec<String>,
    /// Prompt line written to the turn's `agent_start`.
    pub(crate) agent_start: Latency,
}

impl Runtime for Headless {
    type Session = Session;

    async fn start(&self, spec: StartSpec) -> Result<(Arc<Session>, Started), StartError> {
        let token = super::random_hex(16);
        let argv = Launch {
            exe: &self.exe,
            model: &spec.model,
            thinking: spec.thinking.as_deref(),
            target: &spec.target,
            overlay: &self.overlay,
            instructions: spec.instructions.as_deref(),
            extra: &self.extra_flags,
        }
        .argv();
        let env = session_env(&self.credentials, &self.socket, &token);
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        crate::sys::fs::ensure_private_dir(&self.logs)
            .map_err(|e| StartError::Refused(e.to_string()))?;
        let stderr = self.logs.join(format!("session-{}.err", spec.handle));
        let mut child = command::attach(&argv, Some(&spec.cwd), &env, Some(&stderr), STDERR_CAP)
            .map_err(|e| StartError::Refused(e.to_string()))?;
        let (Some(stdin), Some(stdout)) = (child.take_stdin(), child.take_stdout()) else {
            child.close(CLOSE_WAIT, KILL_GRACE).await;
            return Err(StartError::Failed("OMP's pipes are not available".into()));
        };
        let (tx, rx) = mpsc::channel(EVENT_QUEUE);
        let rpc = match Rpc::connect(stdout, stdin, READY_WITHIN, Some(tx)).await {
            Ok(rpc) => rpc,
            Err(e) => {
                child.close(CLOSE_WAIT, KILL_GRACE).await;
                return Err(StartError::Failed(format!("no rpc-ui ready frame: {e}")));
            }
        };
        let settings = Settings {
            spend_phrases: self.spend_phrases.clone(),
            agent_start: self.agent_start.clone(),
        };
        Session::bind(rpc, rx, Some(child), spec, settings).await
    }
}

/// Per-session settings that are not part of the start spec.
#[derive(Clone, Debug, Default)]
pub(crate) struct Settings {
    pub(crate) spend_phrases: Vec<String>,
    pub(crate) agent_start: Latency,
}

#[derive(Default)]
struct State {
    bound: Option<(String, String)>,
    latest: Option<ReadState>,
    fresh: bool,
    fault: Option<String>,
    held: Option<String>,
    stopping: bool,
    stop: Option<StopRecord>,
    dialogs: Vec<Dialog>,
    ring: VecDeque<Entry>,
    turn_started_ms: Option<i64>,
    /// A turn completed since the last idle readback (HOST req. 25).
    turn_done: bool,
    quota_blocked: Option<String>,
    spend_offers: VecDeque<i64>,
    cancelled_offers: HashSet<String>,
    /// The fallback model OMP moved to and the hub reported, until the requested model is back.
    fallback_to: Option<String>,
    /// OMP's latest fallback report, until a readback shows its model in use.
    fallback_report: Option<FallbackReport>,
    /// When the last delivery's line was written, until its turn starts.
    wake_written: Option<Instant>,
    seq: u64,
    log: VecDeque<(u64, SessionEvent)>,
    updated_ms: i64,
}

/// One `retry_fallback_applied` report.
struct FallbackReport {
    from: String,
    to: String,
    reason: String,
}

/// Who ended the child.
enum Proc {
    Child(Box<Attached>),
    /// Tests: the transport is in-process and ends with the fake.
    Fake,
}

/// The view token: changes with every child start (HOST req. 78).
static VIEWS: AtomicU64 = AtomicU64::new(0);

struct Inner {
    handle: String,
    role: Role,
    cwd: String,
    started_ms: i64,
    requested_model: String,
    view: u64,
    pid: Option<u32>,
    rpc: Rpc,
    proc: AsyncMutex<Option<Proc>>,
    state: Mutex<State>,
    snapshot: watch::Sender<Arc<Snapshot>>,
    events: broadcast::Sender<(u64, SessionEvent)>,
    busy: AtomicBool,
    readback_gate: AsyncMutex<()>,
    readbacks: AtomicU64,
    readback_queued: AtomicBool,
    settings: Settings,
}

/// A headless session.
pub(crate) struct Session {
    inner: Arc<Inner>,
}

/// Clears the busy flag when an operation ends.
struct Busy<'a>(&'a AtomicBool);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn cut(text: &str) -> (String, bool) {
    let clean = dialogs::clean(text);
    let kept = dialogs::last_units(&clean, MESSAGE_CUT);
    (kept.to_string(), kept.len() < clean.len())
}

fn is_usage_limit(error: &Value) -> bool {
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if code == "cost_cap" {
        return false;
    }
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    matches!(
        code,
        "usage_limit" | "usage_limit_exceeded" | "quota_exceeded" | "insufficient_quota"
    ) || ["usage limit", "quota", "limit reached", "limit exceeded"]
        .iter()
        .any(|p| message.contains(p))
}

/// Whether an OMP model selector (`provider/id`, optionally `:<thinking>`) names `model`.
fn names_model(selector: &str, model: &str) -> bool {
    let rest = selector.as_bytes().get(model.len()..).unwrap_or_default();
    matches!(rest.first(), None | Some(b':'))
        && selector
            .get(..model.len())
            .is_some_and(|base| same_model(base, model))
}

impl Session {
    /// Runs the start handshake over a connected rpc stream (HOST req. 23-28) and starts the event
    /// task. On failure the child is closed.
    pub(crate) async fn bind(
        rpc: Rpc,
        events: mpsc::Receiver<Value>,
        child: Option<Attached>,
        spec: StartSpec,
        settings: Settings,
    ) -> Result<(Arc<Self>, Started), StartError> {
        let pid = child.as_ref().and_then(Attached::pid);
        let now = now_ms();
        let inner = Arc::new(Inner {
            handle: spec.handle.clone(),
            role: spec.role,
            cwd: spec.cwd.display().to_string(),
            started_ms: now,
            requested_model: spec.model.clone(),
            view: VIEWS.fetch_add(1, Ordering::Relaxed) + 1,
            pid,
            rpc,
            proc: AsyncMutex::new(Some(child.map_or(Proc::Fake, |c| Proc::Child(Box::new(c))))),
            state: Mutex::new(State {
                updated_ms: now,
                ..State::default()
            }),
            snapshot: watch::channel(Arc::new(placeholder(&spec, now))).0,
            events: broadcast::channel(EVENT_KEEP).0,
            busy: AtomicBool::new(false),
            readback_gate: AsyncMutex::new(()),
            readbacks: AtomicU64::new(0),
            readback_queued: AtomicBool::new(false),
            settings,
        });
        tokio::spawn(event_loop(Arc::clone(&inner), events));
        let session = Arc::new(Self { inner });
        let elapsed = Duration::from_millis(u64::try_from(now_ms() - now).unwrap_or(0));
        let budget = START_BUDGET.saturating_sub(READY_WITHIN.min(elapsed));
        let bound = timeout(budget, session.handshake(&spec))
            .await
            .unwrap_or_else(|_| Err("the start took longer than 30 s".into()));
        match bound {
            Ok(started) => Ok((session, started)),
            Err(reason) => {
                session.inner.fault(&reason);
                session.inner.end("failed", Some(reason.clone())).await;
                Err(StartError::Failed(reason))
            }
        }
    }

    async fn handshake(&self, spec: &StartSpec) -> Result<Started, String> {
        let inner = &self.inner;
        // Structured ask dialogs; an OMP without them keeps using select and editor (HOST req. 16).
        let _ = inner
            .call("set_ask_dialog", &[("enabled", json!(true))])
            .await;
        let first = inner.get_state().await?;
        let second = inner.get_state().await?;
        if (first.session_id.as_str(), first.session_file.as_str())
            != (second.session_id.as_str(), second.session_file.as_str())
        {
            return Err("two get_state answers named different sessions".into());
        }
        if !Path::new(&first.session_file).is_absolute() {
            return Err("OMP reported a transcript path that is not absolute".into());
        }
        if let (Some(expected), Target::Resume(file)) = (&spec.expected_id, &spec.target) {
            let reported = std::fs::canonicalize(&first.session_file).ok();
            let requested = std::fs::canonicalize(file).ok();
            if &first.session_id != expected || reported.is_none() || reported != requested {
                return Err(format!(
                    "OMP resumed a different session: expected {expected} at {}, got {} at {}",
                    file.display(),
                    first.session_id,
                    first.session_file
                ));
            }
        }
        let held = match &first.model {
            Some(model) if same_model(model, &spec.model) => spec
                .thinking
                .as_ref()
                .filter(|t| first.thinking.as_ref() != Some(*t))
                .map(|t| {
                    format!(
                        "model mismatch: thinking {} requested, OMP reports {}",
                        t,
                        first.thinking.as_deref().unwrap_or("none")
                    )
                }),
            Some(model) => Some(format!(
                "model mismatch: {} requested, OMP reports {model}",
                spec.model
            )),
            None => Some(format!(
                "model mismatch: {} requested, OMP reports none",
                spec.model
            )),
        };
        {
            let mut st = inner.lock();
            st.bound = Some((first.session_id.clone(), first.session_file.clone()));
            st.held.clone_from(&held);
            st.latest = Some(first.clone());
            st.fresh = true;
            inner.publish(&mut st);
        }
        Ok(Started {
            session_id: first.session_id,
            file: first.session_file,
            model: first.model,
            pid: inner.pid,
            held,
        })
    }

    fn begin(&self) -> Result<Busy<'_>, String> {
        if self.inner.busy.swap(true, Ordering::AcqRel) {
            return Err("busy: another change is in flight".into());
        }
        Ok(Busy(&self.inner.busy))
    }

    async fn deliver_now(&self, d: Delivery) -> DeliverOutcome {
        let inner = &self.inner;
        // A readback the caller made after asking stands unless an event made it stale since.
        let fresh = if d.fresh && inner.lock().fresh {
            Ok(())
        } else {
            inner.readback().await
        };
        let (status, checks) = {
            let st = inner.lock();
            (Inner::status_of(&st).0, ready_for(&st, &d.expected_session))
        };
        if fresh.is_err() || checks.is_err() {
            return DeliverOutcome::NotReady(status);
        }
        if d.text.trim().is_empty() {
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
        // Text starting with "/" would run as an OMP command (HOST req. 53).
        let text = if d.text.trim_start().starts_with('/') {
            format!("[message]\n{}", d.text)
        } else {
            d.text
        };
        let idle = status == Status::Idle;
        let command = if d.steer {
            if idle {
                return DeliverOutcome::Refused("no running turn to steer".into());
            }
            rpc::command("steer", &[("message", json!(text))])
        } else if idle {
            rpc::command(
                "prompt",
                &[
                    ("message", json!(text)),
                    ("streamingBehavior", json!("followUp")),
                ],
            )
        } else {
            rpc::command("follow_up", &[("message", json!(text))])
        };
        let (result, written) = inner.rpc.call_at(command, RESPONSE_TIMEOUT).await;
        match result {
            Ok(reply) if reply.success => {
                if idle && !d.steer {
                    inner.lock().wake_written = written;
                }
                DeliverOutcome::Delivered {
                    turn_started: idle && !d.steer,
                    written,
                }
            }
            Ok(reply) => DeliverOutcome::Refused(
                reply
                    .error
                    .unwrap_or_else(|| "OMP refused the input".into()),
            ),
            Err(RpcError::TooLarge) => DeliverOutcome::Refused("over OMP's frame limit".into()),
            Err(e) if written.is_none() && matches!(e, RpcError::Closed) => {
                DeliverOutcome::NotReady(Status::Error)
            }
            Err(e) => {
                let reason = format!("delivery unconfirmed, do not resend: {e}");
                inner.fault(&reason);
                DeliverOutcome::InDoubt(reason)
            }
        }
    }
}

fn placeholder(spec: &StartSpec, now: i64) -> Snapshot {
    Snapshot {
        handle: spec.handle.clone(),
        role: spec.role,
        runtime: "headless",
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
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn status_of(st: &State) -> (Status, bool) {
        dialogs::status(&Facts {
            bound: st.bound.is_some(),
            state: st.latest.as_ref().filter(|_| st.fresh),
            fault: st.fault.is_some(),
            held: st.held.is_some(),
            stopping: st.stopping || st.stop.is_some(),
            dialogs: !st.dialogs.is_empty(),
        })
    }

    fn publish(&self, st: &mut State) {
        st.updated_ms = now_ms();
        let (status, input_blocked) = Self::status_of(st);
        let identity = st.bound.as_ref().filter(|_| st.fault.is_none());
        let latest = st.latest.as_ref().filter(|_| identity.is_some());
        let snapshot = Snapshot {
            handle: self.handle.clone(),
            role: self.role,
            runtime: "headless",
            cwd: self.cwd.clone(),
            started_ms: self.started_ms,
            updated_ms: st.updated_ms,
            status,
            input_blocked,
            fresh: st.fresh,
            pid: self.pid.filter(|_| st.stop.is_none()),
            session_id: identity.map(|(id, _)| id.clone()),
            session_file: identity.map(|(_, file)| file.clone()),
            goal: latest.map(|s| s.goal.clone()),
            model: latest.and_then(|s| s.model.clone()),
            requested_model: self.requested_model.clone(),
            turn_started_ms: st.turn_started_ms,
            reason: st.fault.clone().or_else(|| st.held.clone()),
            quota_blocked: st.quota_blocked.clone(),
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

    /// Faults the session: its status becomes error and nothing more is delivered (HOST req. 33).
    fn fault(&self, reason: &str) {
        let mut st = self.lock();
        if st.fault.is_some() {
            return;
        }
        log::warn(
            "session faulted",
            &[("handle", &self.handle), ("reason", &reason)],
        );
        st.fault = Some(reason.to_string());
        self.emit(
            &mut st,
            SessionEvent::Fault {
                reason: reason.to_string(),
            },
        );
        self.publish(&mut st);
    }

    async fn call(&self, kind: &str, fields: &[(&str, Value)]) -> Result<rpc::Reply, RpcError> {
        self.rpc
            .call(rpc::command(kind, fields), RESPONSE_TIMEOUT)
            .await
    }

    async fn get_state(&self) -> Result<ReadState, String> {
        match self.call("get_state", &[]).await {
            Ok(reply) if reply.success => read_state(reply.data.as_ref())
                .ok_or_else(|| "get_state answered without a session identity".to_string()),
            Ok(reply) => Err(format!(
                "OMP refused get_state: {}",
                reply.error.unwrap_or_default()
            )),
            Err(e) => Err(format!("get_state: {e}")),
        }
    }

    /// A fresh readback, shared with callers that were already waiting for one (HOST req. 5).
    async fn readback(&self) -> Result<(), String> {
        let seen = self.readbacks.load(Ordering::Acquire);
        let _gate = self.readback_gate.lock().await;
        // A readback that finished while this caller waited is shared, unless an event made the
        // state stale again since.
        if self.readbacks.load(Ordering::Acquire) != seen && self.lock().fresh {
            return Ok(());
        }
        let result = self.get_state().await;
        self.readbacks.fetch_add(1, Ordering::AcqRel);
        match result {
            Ok(state) => self.apply(state),
            Err(e) => {
                if self.rpc.closed() || e.contains("rpc protocol") {
                    self.fault(&e);
                }
                Err(e)
            }
        }
    }

    /// Compares a readback with the bound identity and the spend rules.
    fn apply(&self, state: ReadState) -> Result<(), String> {
        let fault = {
            let mut st = self.lock();
            let mut fault = None;
            if let Some((id, file)) = &st.bound
                && (id != &state.session_id || file != &state.session_file)
            {
                fault = Some(format!(
                    "OMP switched transcripts: bound {id}, now {} at {}",
                    state.session_id, state.session_file
                ));
            } else if state.extra_usage {
                fault = Some(
                    "the usage limit reports extra usage; fenced before a request could bill overage"
                        .to_string(),
                );
            } else if state.idle()
                && std::mem::take(&mut st.turn_done)
                && !Path::new(&state.session_file).exists()
            {
                fault = Some("the transcript is missing after a completed turn".into());
            }
            if state.idle() {
                st.turn_started_ms = None;
            }
            let previous = st.latest.as_ref().and_then(|s| s.model.clone());
            st.latest = Some(state);
            st.fresh = true;
            self.track_model(&mut st, previous);
            self.publish(&mut st);
            fault
        };
        match fault {
            Some(reason) => {
                self.fault(&reason);
                Err(reason)
            }
            None => Ok(()),
        }
    }

    /// Model events from what readbacks show (HOST req. 77): one `ModelChanged` per change, one
    /// `Fallback` per change OMP reported as a fallback (a repeated report of the same move adds
    /// nothing), one `FallbackRecovered` when the requested model is back.
    fn track_model(&self, st: &mut State, previous: Option<String>) {
        let Some(model) = st.latest.as_ref().and_then(|s| s.model.clone()) else {
            return;
        };
        if previous.is_some_and(|p| p != model) {
            self.emit(
                st,
                SessionEvent::ModelChanged {
                    model: model.clone(),
                },
            );
        }
        self.settle_fallback(st);
    }

    /// Matches a pending fallback report against the model in use.
    fn settle_fallback(&self, st: &mut State) {
        let Some(model) = st.latest.as_ref().and_then(|s| s.model.clone()) else {
            return;
        };
        if st.fallback_to.is_some() && same_model(&model, &self.requested_model) {
            st.fallback_to = None;
            self.emit(
                st,
                SessionEvent::FallbackRecovered {
                    model: model.clone(),
                },
            );
        }
        if !st
            .fallback_report
            .as_ref()
            .is_some_and(|r| names_model(&r.to, &model))
        {
            return;
        }
        let Some(report) = st.fallback_report.take() else {
            return;
        };
        if st.fallback_to.as_deref() == Some(model.as_str()) {
            return;
        }
        st.fallback_to = Some(model);
        self.emit(
            st,
            SessionEvent::Fallback {
                from: report.from,
                to: report.to,
                reason: report.reason,
            },
        );
    }

    /// Ends the child: end of input, 15 s, TERM, 5 s, KILL (decision 8), and writes the stop record.
    async fn end(&self, how: &str, detail: Option<String>) -> StopRecord {
        if let Some(record) = self.lock().stop.clone() {
            return record;
        }
        self.lock().stopping = true;
        self.rpc.end_input().await;
        let proc = self.proc.lock().await.take();
        let (how, detail) = match proc {
            Some(Proc::Child(child)) => {
                let (exit, step) = child.close(CLOSE_WAIT, KILL_GRACE).await;
                match (step, exit) {
                    (Step::Input, Exit::Code(0)) => (how.to_string(), detail),
                    (Step::Input, exit) => ("exited".into(), Some(format!("{exit:?}"))),
                    (Step::Term, exit) => ("term".into(), Some(format!("needed TERM ({exit:?})"))),
                    (Step::Kill, exit) => ("kill".into(), Some(format!("needed KILL ({exit:?})"))),
                }
            }
            Some(Proc::Fake) | None => (how.to_string(), detail),
        };
        self.record_stop(how, detail)
    }

    fn record_stop(&self, how: String, detail: Option<String>) -> StopRecord {
        let mut st = self.lock();
        if let Some(record) = st.stop.clone() {
            return record;
        }
        let record = StopRecord {
            how,
            detail,
            session_id: st.bound.as_ref().map(|(id, _)| id.clone()),
            at_ms: now_ms(),
        };
        st.stop = Some(record.clone());
        st.dialogs.clear();
        self.emit(
            &mut st,
            SessionEvent::Stopped {
                record: record.clone(),
            },
        );
        self.publish(&mut st);
        record
    }

    fn queue_readback(self: &Arc<Self>) {
        if self.readback_queued.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move {
            me.readback_queued.store(false, Ordering::Release);
            let _ = me.readback().await;
        });
    }

    async fn on_frame(self: &Arc<Self>, frame: &Value) {
        let kind = frame
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "agent_start" => {
                let mut st = self.lock();
                st.fresh = false;
                st.turn_started_ms.get_or_insert_with(now_ms);
                if let Some(written) = st.wake_written.take() {
                    self.settings.agent_start.record(written.elapsed());
                }
                self.emit(&mut st, SessionEvent::TurnStarted);
                self.publish(&mut st);
            }
            "agent_end" => {
                let will_continue = frame.get("isTerminal") == Some(&Value::Bool(false))
                    || frame.get("awaitingAsyncWork") == Some(&Value::Bool(true));
                let mut st = self.lock();
                st.fresh = false;
                st.turn_done = true;
                self.emit(&mut st, SessionEvent::TurnEnded { will_continue });
                self.publish(&mut st);
                drop(st);
                self.queue_readback();
            }
            "message_end" => self.on_message(frame),
            "command_output" => {
                if let Some(entry) = dialogs::entry_of(frame) {
                    let mut st = self.lock();
                    push_ring(&mut st, entry);
                    self.publish(&mut st);
                }
            }
            "extension_ui_request" => self.on_ui(frame).await,
            "prompt_result" => self.on_prompt_result(frame),
            "session_settled" => {
                let mut st = self.lock();
                st.fresh = false;
                self.emit(&mut st, SessionEvent::Quiet);
                drop(st);
                self.queue_readback();
            }
            "retry_fallback_applied" => {
                let text = |k: &str| {
                    frame.get(k).map_or_else(String::new, |v| {
                        v.as_str().map_or_else(|| v.to_string(), str::to_string)
                    })
                };
                // The report is checked against the model in use: OMP may repeat it for one move,
                // and its advisors report their own fallbacks, which do not move the session.
                let mut st = self.lock();
                st.fallback_report = Some(FallbackReport {
                    from: text("from"),
                    to: text("to"),
                    reason: text("reason"),
                });
                self.settle_fallback(&mut st);
                st.fresh = false;
                self.publish(&mut st);
                drop(st);
                self.queue_readback();
            }
            "model_changed" => {
                // OMP 19.1.1 sends this frame without the model; the readback names it.
                let mut st = self.lock();
                st.fresh = false;
                self.publish(&mut st);
                drop(st);
                self.queue_readback();
            }
            "rpc_frame_error" => {
                self.fault("OMP lost a frame (rpc_frame_error); trust in the stream ended");
            }
            _ => {}
        }
        if self.rpc.dropped_events() > 0 {
            self.fault("session events were dropped; trust in the stream ended");
        }
    }

    fn on_message(&self, frame: &Value) {
        let Some(message) = frame.get("message") else {
            return;
        };
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if role != "assistant" && role != "user" {
            return;
        }
        let Some(text) = dialogs::message_text(message).filter(|t| !t.trim().is_empty()) else {
            return;
        };
        let mut st = self.lock();
        if let Some(entry) = dialogs::entry_of(frame) {
            push_ring(&mut st, entry);
        }
        let (text, truncated) = cut(&text);
        self.emit(
            &mut st,
            SessionEvent::Message {
                role: role.to_string(),
                text,
                truncated,
            },
        );
        self.publish(&mut st);
    }

    async fn on_ui(&self, frame: &Value) {
        match dialogs::ui_frame(frame) {
            UiFrame::Open(dialog) if spend_offer(&dialog, &self.settings.spend_phrases) => {
                self.cancel_offer(dialog).await;
            }
            UiFrame::Open(dialog) => {
                let mut st = self.lock();
                match st.dialogs.iter_mut().find(|d| d.id == dialog.id) {
                    Some(same) => *same = dialog.clone(),
                    None => st.dialogs.push(dialog.clone()),
                }
                self.emit(&mut st, SessionEvent::DialogOpened { dialog });
                self.publish(&mut st);
            }
            UiFrame::Cancel(id) => {
                let mut st = self.lock();
                let before = st.dialogs.len();
                st.dialogs.retain(|d| d.id != id);
                if st.dialogs.len() != before {
                    self.emit(&mut st, SessionEvent::DialogClosed { id });
                    self.publish(&mut st);
                }
            }
            UiFrame::Notice(_) => {
                if let Some(entry) = dialogs::entry_of(frame) {
                    let mut st = self.lock();
                    push_ring(&mut st, entry);
                    self.publish(&mut st);
                }
            }
            UiFrame::Ignore => {}
        }
    }

    /// Cancels a spend offer at once (DESIGN 9.2): never "no", never a remembered choice. A repeat of a
    /// cancelled dialog, or the third offer in 10 minutes, fences the session.
    async fn cancel_offer(&self, dialog: Dialog) {
        let sent = self
            .rpc
            .send(&AnswerFrame::cancel(&dialog.id).to_map())
            .await;
        let now = now_ms();
        let (count, repeat) = {
            let mut st = self.lock();
            let repeat = !st.cancelled_offers.insert(dialog.id.clone());
            st.spend_offers.push_back(now);
            while st
                .spend_offers
                .front()
                .is_some_and(|t| now - t > SPEND_WINDOW_MS)
            {
                st.spend_offers.pop_front();
            }
            let count = st.spend_offers.len();
            let title = dialog
                .title
                .clone()
                .or_else(|| dialog.message.clone())
                .unwrap_or_else(|| dialog.id.clone());
            self.emit(&mut st, SessionEvent::SpendOffer { title, count });
            self.publish(&mut st);
            (count, repeat)
        };
        log::warn(
            "spend offer cancelled",
            &[("handle", &self.handle), ("count", &count)],
        );
        if let Err(e) = sent {
            self.fault(&format!("a spend offer could not be cancelled: {e}"));
        } else if repeat {
            self.fault("a spend offer stayed after it was cancelled");
        } else if count >= SPEND_LIMIT {
            self.fault("three spend offers within 10 minutes");
        }
    }

    fn on_prompt_result(self: &Arc<Self>, frame: &Value) {
        let status = frame
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let error = frame.get("error");
        let message = error
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut st = self.lock();
        st.fresh = false;
        if status == "error" && error.is_some_and(is_usage_limit) {
            let text = message
                .clone()
                .unwrap_or_else(|| "usage limit reached".into());
            if st.quota_blocked.is_none() {
                st.quota_blocked = Some(text.clone());
                self.emit(&mut st, SessionEvent::QuotaBlocked { message: text });
            }
        } else if status == "completed" {
            st.quota_blocked = None;
        }
        self.emit(
            &mut st,
            SessionEvent::Settled {
                status,
                error: message,
            },
        );
        self.publish(&mut st);
        drop(st);
        self.queue_readback();
    }
}

/// The delivery checks of HOST req. 50 that read the session state, in order.
fn ready_for(st: &State, expected: &str) -> Result<(), String> {
    let Some((id, _)) = &st.bound else {
        return Err("not bound".into());
    };
    if id != expected {
        return Err(format!("the session is {id}, not the expected {expected}"));
    }
    if let Some(reason) = st.fault.as_ref().or(st.held.as_ref()) {
        return Err(reason.clone());
    }
    if st.stopping || st.stop.is_some() {
        return Err("stopping".into());
    }
    if !st.dialogs.is_empty() {
        return Err("a dialog is pending".into());
    }
    let goal = st.latest.as_ref().map(|s| s.goal.status);
    if matches!(
        goal,
        Some(dialogs::GoalStatus::Paused | dialogs::GoalStatus::Blocked)
    ) {
        return Err("the goal is paused or blocked".into());
    }
    Ok(())
}

fn push_ring(st: &mut State, entry: Entry) {
    st.ring.push_back(entry);
    while st.ring.len() > RING {
        st.ring.pop_front();
    }
}

/// Applies every event in order. When OMP's output ends without a close request, the child exited
/// by itself: it is reaped and the stop record says so (HOST req. 70).
async fn event_loop(inner: Arc<Inner>, mut events: mpsc::Receiver<Value>) {
    while let Some(frame) = events.recv().await {
        inner.on_frame(&frame).await;
    }
    let asked = inner.lock().stopping;
    if asked {
        return;
    }
    let proc = inner.proc.lock().await.take();
    let detail = match proc {
        Some(Proc::Child(child)) => {
            let (exit, _) = child.close(Duration::from_secs(1), KILL_GRACE).await;
            format!("{exit:?}")
        }
        Some(Proc::Fake) | None => "output closed".into(),
    };
    log::warn(
        "session child exited",
        &[("handle", &inner.handle), ("detail", &detail)],
    );
    inner.record_stop("exited".into(), Some(detail));
}

impl Control for Session {
    fn snapshot(&self) -> Arc<Snapshot> {
        Arc::clone(&self.inner.snapshot.borrow())
    }

    fn states(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.inner.snapshot.subscribe()
    }

    fn subscribe(&self) -> broadcast::Receiver<(u64, SessionEvent)> {
        self.inner.events.subscribe()
    }

    fn events_after(&self, after: u64, limit: usize) -> EventPage {
        let st = self.inner.lock();
        let oldest = st.log.front().map_or(st.seq + 1, |(n, _)| *n);
        EventPage {
            missed: after + 1 < oldest,
            events: st
                .log
                .iter()
                .filter(|(n, _)| *n > after)
                .take(limit.clamp(1, EVENT_PAGE))
                .cloned()
                .collect(),
        }
    }

    fn alive(&self) -> bool {
        self.inner.lock().stop.is_none()
    }

    async fn refresh(&self) -> Result<(), String> {
        self.inner.readback().await
    }

    async fn deliver(&self, delivery: Delivery) -> DeliverOutcome {
        let _busy = match self.begin() {
            Ok(b) => b,
            Err(reason) => return DeliverOutcome::Refused(reason),
        };
        self.deliver_now(delivery).await
    }

    async fn interrupt(&self) -> Result<String, String> {
        let _busy = self.begin()?;
        let inner = &self.inner;
        inner.readback().await?;
        let running = inner.lock().latest.as_ref().is_some_and(|s| s.foreground);
        if !running {
            return Err("nothing to interrupt".into());
        }
        match inner.call("abort", &[]).await {
            Ok(reply) if reply.success => {}
            Ok(reply) => return Err(reply.error.unwrap_or_else(|| "OMP refused abort".into())),
            Err(e) => {
                inner.fault(&format!("abort unconfirmed: {e}"));
                return Err("abort sent, outcome unknown".into());
            }
        }
        let stopped = inner.readback().await.is_ok()
            && !inner.lock().latest.as_ref().is_some_and(|s| s.foreground);
        if stopped {
            Ok("interrupted; background jobs are not targeted".into())
        } else {
            let mut st = inner.lock();
            inner.emit(&mut st, SessionEvent::InterruptUnconfirmed);
            Err("abort sent, stop unconfirmed".into())
        }
    }

    async fn set_model(&self, selector: &str) -> Result<String, String> {
        let _busy = self.begin()?;
        let inner = &self.inner;
        if selector.len() > 256 || !crate::omp::transcript::model_selector(selector) {
            return Err("expected provider/id without spaces, at most 256 characters".into());
        }
        let Some((provider, id)) = selector.split_once('/') else {
            return Err("expected provider/id".into());
        };
        inner.readback().await?;
        let current = {
            let st = inner.lock();
            if st.fault.is_some() || st.held.is_some() || !st.dialogs.is_empty() {
                return Err("the session is held".into());
            }
            if !st.latest.as_ref().is_some_and(ReadState::idle) {
                return Err("the session is not idle".into());
            }
            st.latest.as_ref().and_then(|s| s.model.clone())
        };
        if current.as_deref().is_some_and(|m| same_model(m, selector)) {
            return Ok(selector.to_string());
        }
        match inner
            .call(
                "set_model",
                &[("provider", json!(provider)), ("modelId", json!(id))],
            )
            .await
        {
            Ok(reply) if reply.success => {}
            Ok(reply) => {
                return Err(reply
                    .error
                    .unwrap_or_else(|| "OMP refused the model".into()));
            }
            Err(e) => {
                inner.fault(&format!("model change unconfirmed: {e}"));
                return Err("model change unconfirmed; check the current model".into());
            }
        }
        {
            // A change asked for here is no fallback; the readback after it must be new.
            let mut st = inner.lock();
            st.fallback_report = None;
            st.fresh = false;
            inner.publish(&mut st);
        }
        inner.readback().await?;
        let now = inner.lock().latest.as_ref().and_then(|s| s.model.clone());
        match now {
            Some(m) if same_model(&m, selector) => Ok(m),
            other => Err(format!(
                "model readback shows {}",
                other.unwrap_or_else(|| "none".into())
            )),
        }
    }

    async fn answer(
        &self,
        session_id: &str,
        dialog_id: &str,
        answer: &Value,
    ) -> Result<(), String> {
        let _busy = self.begin()?;
        let inner = &self.inner;
        let frame = {
            let mut st = inner.lock();
            if st.bound.as_ref().map(|(id, _)| id.as_str()) != Some(session_id) {
                return Err("the answer names another session".into());
            }
            if st.fault.is_some() {
                return Err("the session is faulted".into());
            }
            let Some(dialog) = st.dialogs.iter().find(|d| d.id == dialog_id).cloned() else {
                return Err("no such pending dialog".into());
            };
            let Some(frame) = dialogs::answer_frame(&dialog, answer) else {
                return Err("this answer is not allowed for the dialog".into());
            };
            // Removed before the write (HOST req. 48).
            st.dialogs.retain(|d| d.id != dialog_id);
            inner.emit(
                &mut st,
                SessionEvent::DialogClosed {
                    id: dialog_id.to_string(),
                },
            );
            inner.publish(&mut st);
            frame
        };
        if let Err(e) = inner.rpc.send(&frame.to_map()).await {
            inner.fault(&format!("answer unconfirmed: {e}"));
            return Err("ambiguous; do not resend".into());
        }
        Ok(())
    }

    fn accept_model(&self) -> Result<(), String> {
        let mut st = self.inner.lock();
        match &st.held {
            Some(reason) if reason.starts_with("model mismatch") => {
                st.held = None;
                self.inner.publish(&mut st);
                Ok(())
            }
            _ => Err("the session is not held for a model mismatch".into()),
        }
    }

    async fn close(&self, force: bool) -> Result<StopRecord, String> {
        let _busy = self.begin()?;
        let inner = &self.inner;
        if let Some(record) = inner.lock().stop.clone() {
            return Ok(record);
        }
        let faulted = inner.lock().fault.is_some();
        if !faulted {
            let _ = inner.readback().await;
            let (idle, dialogs) = {
                let st = inner.lock();
                (
                    st.latest.as_ref().is_some_and(ReadState::idle),
                    !st.dialogs.is_empty(),
                )
            };
            if dialogs {
                return Err("a dialog is pending; answer it first".into());
            }
            if !idle {
                if !force {
                    return Err("the session is working; close with force to interrupt it".into());
                }
                let _ = inner.call("abort", &[]).await;
                let deadline = Instant::now() + FORCE_IDLE_WAIT;
                loop {
                    let _ = inner.readback().await;
                    let (dialog, done) = {
                        let st = inner.lock();
                        (
                            !st.dialogs.is_empty(),
                            st.latest.as_ref().is_some_and(ReadState::idle),
                        )
                    };
                    if dialog {
                        return Err("a dialog appeared while stopping; answer it first".into());
                    }
                    if done || Instant::now() >= deadline {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        }
        let how = if force || faulted { "forced" } else { "clean" };
        Ok(inner.end(how, None).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::testing::{Fake, spec};

    async fn started(fake: &Fake) -> Arc<Session> {
        let (session, started) = fake.bind(spec("h1", None)).await.unwrap();
        assert_eq!(started.session_id, fake.session_id());
        assert_eq!(started.held, None);
        session
    }

    #[tokio::test]
    async fn handshake_binds_identity_and_delivers_a_wake() {
        let fake = Fake::start();
        let session = started(&fake).await;
        assert_eq!(session.snapshot().status, Status::Idle);
        let outcome = session
            .deliver(Delivery {
                expected_session: fake.session_id(),
                text: "/help me".into(),
                timing: Timing::Any,
                steer: false,
                fresh: false,
            })
            .await;
        assert!(
            matches!(
                outcome,
                DeliverOutcome::Delivered {
                    turn_started: true,
                    written: Some(_)
                }
            ),
            "{outcome:?}"
        );
        let prompt = fake.seen("prompt").pop().unwrap();
        assert_eq!(prompt["streamingBehavior"], "followUp");
        assert!(
            prompt["message"]
                .as_str()
                .unwrap()
                .starts_with("[message]\n/help")
        );
        let wrong = session
            .deliver(Delivery {
                expected_session: "other".into(),
                text: "x".into(),
                timing: Timing::Any,
                steer: false,
                fresh: false,
            })
            .await;
        assert!(matches!(wrong, DeliverOutcome::NotReady(_)), "{wrong:?}");
    }

    #[tokio::test]
    async fn a_busy_session_gets_a_follow_up_and_steer_needs_a_turn() {
        let fake = Fake::start();
        let session = started(&fake).await;
        let steer = session
            .deliver(Delivery {
                expected_session: fake.session_id(),
                text: "x".into(),
                timing: Timing::Any,
                steer: true,
                fresh: false,
            })
            .await;
        assert_eq!(
            steer,
            DeliverOutcome::Refused("no running turn to steer".into())
        );
        fake.set(|s| s.streaming = true);
        let follow = session
            .deliver(Delivery {
                expected_session: fake.session_id(),
                text: "second".into(),
                timing: Timing::Any,
                steer: false,
                fresh: false,
            })
            .await;
        assert!(
            matches!(
                follow,
                DeliverOutcome::Delivered {
                    turn_started: false,
                    ..
                }
            ),
            "{follow:?}"
        );
        assert_eq!(fake.seen("follow_up").len(), 1);
        let idle_only = session
            .deliver(Delivery {
                expected_session: fake.session_id(),
                text: "x".into(),
                timing: Timing::Idle,
                steer: false,
                fresh: false,
            })
            .await;
        assert_eq!(idle_only, DeliverOutcome::NotReady(Status::Working));
    }

    /// Acceptance check 6: a transcript switch, a model mismatch at start and an `rpc_frame_error`
    /// each fence the session; nothing is delivered afterwards.
    #[tokio::test]
    async fn switch_mismatch_and_frame_error_fence() {
        let deliver = |session: Arc<Session>, id: String| async move {
            session
                .deliver(Delivery {
                    expected_session: id,
                    text: "hello".into(),
                    timing: Timing::Any,
                    steer: false,
                    fresh: false,
                })
                .await
        };
        let fake = Fake::start();
        let session = started(&fake).await;
        fake.set(|s| s.session_id = "switched".into());
        let outcome = deliver(Arc::clone(&session), fake.session_id()).await;
        assert!(
            matches!(outcome, DeliverOutcome::NotReady(Status::Error)),
            "{outcome:?}"
        );
        assert!(
            session
                .snapshot()
                .reason
                .clone()
                .unwrap()
                .contains("switched transcripts")
        );
        assert_eq!(fake.seen("prompt").len(), 0);

        let fake = Fake::start();
        fake.set(|s| s.model = ("other".into(), "m".into()));
        let (session, bound) = fake.bind(spec("h2", None)).await.unwrap();
        assert!(bound.held.unwrap().contains("model mismatch"));
        let outcome = deliver(Arc::clone(&session), fake.session_id()).await;
        assert!(
            matches!(outcome, DeliverOutcome::NotReady(_)),
            "{outcome:?}"
        );
        assert_eq!(fake.seen("prompt").len(), 0);

        let fake = Fake::start();
        let session = started(&fake).await;
        fake.push(json!({"type": "rpc_frame_error", "error": "too large"}));
        let snapshot = fake.wait_for(&session, |s| s.status == Status::Error).await;
        assert!(snapshot.reason.clone().unwrap().contains("rpc_frame_error"));
        let outcome = deliver(Arc::clone(&session), fake.session_id()).await;
        assert!(
            matches!(outcome, DeliverOutcome::NotReady(Status::Error)),
            "{outcome:?}"
        );
        assert_eq!(fake.seen("prompt").len(), 0);
    }

    /// Acceptance check 8: a spend offer is cancelled within 1 s and reported; three in 10 minutes fence.
    #[tokio::test]
    async fn spend_offers_are_cancelled_and_fence_at_three() {
        let fake = Fake::start();
        let session = started(&fake).await;
        let mut events = session.subscribe();
        for n in 1..=3 {
            let opened = Instant::now();
            fake.push(json!({
                "type": "extension_ui_request", "id": format!("offer-{n}"), "method": "select",
                "title": "Usage limit reached", "options": ["Redeem saved reset", "Wait"],
            }));
            let cancel = fake.wait_seen("extension_ui_response", n).await;
            assert!(opened.elapsed() < Duration::from_secs(1));
            assert_eq!(cancel["cancelled"], true);
            assert_eq!(cancel["id"], format!("offer-{n}"));
            assert!(cancel.get("value").is_none());
        }
        let snapshot = fake.wait_for(&session, |s| s.status == Status::Error).await;
        assert!(
            snapshot
                .reason
                .clone()
                .unwrap()
                .contains("three spend offers")
        );
        let mut offers = 0;
        while let Ok((_, event)) = events.try_recv() {
            if matches!(event, SessionEvent::SpendOffer { .. }) {
                offers += 1;
            }
        }
        assert_eq!(offers, 3);
        assert!(snapshot.dialogs.is_empty(), "offers never stay pending");
    }

    /// Acceptance check 9 (session side): one fallback event per move OMP makes, however often it
    /// reports it, and one restore; a usage-limit `prompt_result` marks the session quota-blocked;
    /// `extraUsage` fences.
    #[tokio::test]
    async fn fallback_quota_and_extra_usage() {
        let fake = Fake::start();
        let session = started(&fake).await;
        let mut events = session.subscribe();
        let report = json!({"type": "retry_fallback_applied", "from": "fake/primary:high", "to": "fake/backup:high", "role": "default", "reason": "usage limit"});
        // A message after the reports: once it shows, the reports before it are handled.
        let sync = json!({"type": "message_end", "message": {"role": "assistant", "content": [{"type": "text", "text": "sync"}]}});
        // OMP switches the model (a bare `model_changed`), then reports the fallback.
        fake.set(|s| s.model = ("fake".into(), "backup".into()));
        fake.push(json!({"type": "model_changed"}));
        fake.push(report.clone());
        fake.wait_for(&session, |s| s.model.as_deref() == Some("fake/backup"))
            .await;
        // The same move reported again, and an advisor's report that moves nothing.
        fake.push(report);
        fake.push(json!({"type": "model_changed"}));
        fake.push(json!({"type": "retry_fallback_applied", "from": "fake/primary", "to": "fake/other", "role": "advisor", "reason": "usage limit"}));
        fake.push(sync);
        fake.wait_for(&session, |s| s.recent.iter().any(|e| e.text == "sync"))
            .await;
        // OMP restores the requested model.
        fake.set(|s| s.model = ("fake".into(), "primary".into()));
        fake.push(json!({"type": "model_changed"}));
        fake.wait_for(&session, |s| s.model.as_deref() == Some("fake/primary"))
            .await;
        fake.push(json!({"type": "prompt_result", "id": "x", "agentInvoked": true, "status": "error",
            "error": {"message": "You have hit your usage limit", "retryable": false}, "sessionSettled": true}));
        let snapshot = fake.wait_for(&session, |s| s.quota_blocked.is_some()).await;
        assert!(
            snapshot
                .quota_blocked
                .clone()
                .unwrap()
                .contains("usage limit")
        );
        let mut kinds = Vec::new();
        while let Ok((_, event)) = events.try_recv() {
            if matches!(
                event,
                SessionEvent::Fallback { .. }
                    | SessionEvent::FallbackRecovered { .. }
                    | SessionEvent::ModelChanged { .. }
                    | SessionEvent::QuotaBlocked { .. }
            ) {
                kinds.push(event);
            }
        }
        assert_eq!(kinds.len(), 5, "{kinds:?}");
        assert!(
            matches!(&kinds[0], SessionEvent::ModelChanged { model } if model == "fake/backup")
        );
        assert!(
            matches!(&kinds[1], SessionEvent::Fallback { from, to, .. } if from == "fake/primary:high" && to == "fake/backup:high")
        );
        assert!(
            matches!(&kinds[2], SessionEvent::ModelChanged { model } if model == "fake/primary")
        );
        assert!(matches!(&kinds[3], SessionEvent::FallbackRecovered { .. }));
        assert!(matches!(&kinds[4], SessionEvent::QuotaBlocked { .. }));
        fake.set(|s| s.extra_usage = true);
        let outcome = session
            .deliver(Delivery {
                expected_session: fake.session_id(),
                text: "owner request".into(),
                timing: Timing::Any,
                steer: false,
                fresh: false,
            })
            .await;
        assert!(
            matches!(outcome, DeliverOutcome::NotReady(Status::Error)),
            "{outcome:?}"
        );
        assert!(
            session
                .snapshot()
                .reason
                .clone()
                .unwrap()
                .contains("extra usage")
        );
    }

    #[tokio::test]
    async fn dialogs_are_answered_once_and_close_refuses_while_pending() {
        let fake = Fake::start();
        let session = started(&fake).await;
        fake.push(
            json!({"type": "extension_ui_request", "id": "a1", "method": "select",
            "title": "Run bash?", "options": ["Approve Once", "Approve for this session", "Deny"]}),
        );
        fake.wait_for(&session, |s| s.status == Status::Blocked)
            .await;
        assert!(session.close(false).await.unwrap_err().contains("dialog"));
        let id = fake.session_id();
        assert!(
            session
                .answer(&id, "a1", &json!({"value": "Approve for this session"}))
                .await
                .is_err()
        );
        assert!(
            session
                .answer("other", "a1", &json!({"value": "Deny"}))
                .await
                .is_err()
        );
        session
            .answer(&id, "a1", &json!({"value": "Deny"}))
            .await
            .unwrap();
        assert_eq!(
            fake.wait_seen("extension_ui_response", 1).await["value"],
            "Deny"
        );
        assert!(
            session
                .answer(&id, "a1", &json!({"value": "Deny"}))
                .await
                .is_err()
        );
        let record = session.close(false).await.unwrap();
        assert_eq!(record.how, "clean");
        assert_eq!(record.session_id, Some(id));
        assert!(!session.alive());
    }

    #[tokio::test]
    async fn interrupt_and_model_change_read_back() {
        let fake = Fake::start();
        let session = started(&fake).await;
        assert_eq!(
            session.interrupt().await.unwrap_err(),
            "nothing to interrupt"
        );
        fake.set(|s| s.streaming = true);
        assert!(
            session
                .interrupt()
                .await
                .unwrap()
                .starts_with("interrupted")
        );
        assert_eq!(fake.seen("abort").len(), 1);
        assert_eq!(
            session.set_model("fake/primary").await.unwrap(),
            "fake/primary"
        );
        assert_eq!(fake.seen("set_model").len(), 0);
        assert_eq!(session.set_model("fake/other").await.unwrap(), "fake/other");
        assert!(session.set_model("bad model").await.is_err());
    }

    #[tokio::test]
    async fn an_unasked_exit_writes_a_stop_record() {
        let fake = Fake::start();
        let session = started(&fake).await;
        fake.exit();
        let snapshot = fake.wait_for(&session, |s| s.stop.is_some()).await;
        assert_eq!(snapshot.stop.clone().unwrap().how, "exited");
        let page = session.events_after(0, 100);
        assert!(!page.missed);
        assert!(matches!(
            page.events.last(),
            Some((_, SessionEvent::Stopped { .. }))
        ));
    }
}
