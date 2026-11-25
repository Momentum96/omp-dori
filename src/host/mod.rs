//! The session host (DESIGN 4; HOST-01 to HOST-29): the `Runtime` trait with its headless
//! implementation, the session actor, the per-target inbox with delivery confirmation from session
//! files, and the standing lead.
//!
//! `Runtime::start` returns a session handle implementing [`Control`]; the terminal runtime of
//! milestone 6 implements the same two traits over the bridge connection instead of rpc-ui.

pub(crate) mod bridge;
pub(crate) mod dialogs;
pub(crate) mod headless;
pub(crate) mod hosted;
pub(crate) mod inbox;
pub(crate) mod lead;
pub(crate) mod tern;
#[cfg(test)]
pub(crate) mod testing;
#[cfg(feature = "voice")]
pub(crate) mod voice;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;

pub(crate) use dialogs::{Dialog, Entry, Goal, Status};

use crate::omp::launch::Target;

/// What a session is for (HOST req. 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Role {
    Lead,
    Lane,
}

/// One session start (HOST req. 6, 9).
#[derive(Clone, Debug)]
pub(crate) struct StartSpec {
    pub(crate) handle: String,
    pub(crate) role: Role,
    pub(crate) cwd: PathBuf,
    /// `provider/id`.
    pub(crate) model: String,
    pub(crate) thinking: Option<String>,
    pub(crate) target: Target,
    /// The OMP session ID a resume must bind (HOST req. 10).
    pub(crate) expected_id: Option<String>,
    pub(crate) instructions: Option<PathBuf>,
    /// `--max-cost` in US dollars: OMP's own backstop to a lane's cost ceiling (WS-18).
    pub(crate) max_cost: Option<f64>,
    /// `--tools`: the look-only tool set of a review lane (OPC-46).
    pub(crate) tools: Option<String>,
}

/// A bound session (DESIGN 4.1 `Started`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Started {
    pub(crate) session_id: String,
    pub(crate) file: String,
    pub(crate) model: Option<String>,
    pub(crate) pid: Option<u32>,
    /// Held after binding (model mismatch): no input until the owner acts (HOST req. 27).
    pub(crate) held: Option<String>,
}

/// A start that did not bind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StartError {
    /// Refused before anything was spawned.
    Refused(String),
    /// The child was spawned and closed again.
    Failed(String),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(r) => write!(f, "refused: {r}"),
            Self::Failed(r) => write!(f, "failed: {r}"),
        }
    }
}

/// When a delivery may go in (HOST req. 49).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Timing {
    Idle,
    Working,
    Any,
}

/// One delivery into a session (HOST req. 49-53).
#[derive(Clone, Debug)]
pub(crate) struct Delivery {
    pub(crate) expected_session: String,
    pub(crate) text: String,
    pub(crate) timing: Timing,
    /// Steer the running turn instead of waking or queueing.
    pub(crate) steer: bool,
    /// The caller read the state back ([`Control::refresh`]) after this delivery was asked for; the
    /// checks use that readback unless an event made it stale since (HOST req. 50).
    pub(crate) fresh: bool,
}

/// The result of a delivery (HOST req. 54).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeliverOutcome {
    /// OMP admitted it; `written` is when the command line was written.
    Delivered {
        turn_started: bool,
        written: Option<Instant>,
    },
    NotReady(Status),
    Refused(String),
    /// Written, outcome unknown: never resent automatically.
    InDoubt(String),
}

/// How a session ended (HOST req. 69).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct StopRecord {
    /// `clean`, `forced`, `term`, `kill`, `exited`, `failed`.
    pub(crate) how: String,
    pub(crate) detail: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) at_ms: i64,
}

/// Session events (HOST req. 77), numbered by a sequence that only increases.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SessionEvent {
    TurnStarted,
    TurnEnded {
        will_continue: bool,
    },
    Message {
        role: String,
        text: String,
        truncated: bool,
    },
    DialogOpened {
        dialog: Dialog,
    },
    DialogClosed {
        id: String,
    },
    /// A prompt's outcome (`prompt_result`).
    Settled {
        status: String,
        error: Option<String>,
    },
    /// Nothing can wake the session any more (`session_settled`).
    Quiet,
    ModelChanged {
        model: String,
    },
    /// OMP moved to the next model of the owner's fallback chain.
    Fallback {
        from: String,
        to: String,
        reason: String,
    },
    /// OMP restored the requested model.
    FallbackRecovered {
        model: String,
    },
    /// A turn ended on a model that is neither the requested one nor a fallback OMP reported.
    ModelUnexpected {
        model: String,
        requested: String,
    },
    /// A spend offer was cancelled (DESIGN 9.2).
    SpendOffer {
        title: String,
        count: usize,
    },
    /// A turn ended with the usage limit exhausted and no fallback left.
    QuotaBlocked {
        message: String,
    },
    /// The session is faulted or fenced; nothing more is delivered.
    Fault {
        reason: String,
    },
    InterruptUnconfirmed,
    /// The terminal runtime's bridge was silent for 45 s: the session is lost until the same OMP
    /// process reconnects with the same session (HOST req. 97).
    LinkLost,
    /// The lost bridge came back.
    LinkRestored,
    Stopped {
        record: StopRecord,
    },
}

/// The status snapshot (HOST req. 35-41). Reads never wait for it.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Snapshot {
    pub(crate) handle: String,
    pub(crate) role: Role,
    pub(crate) runtime: &'static str,
    pub(crate) cwd: String,
    pub(crate) started_ms: i64,
    pub(crate) updated_ms: i64,
    pub(crate) status: Status,
    pub(crate) input_blocked: bool,
    /// False when the status is the last confirmed one rather than a fresh readback.
    pub(crate) fresh: bool,
    pub(crate) pid: Option<u32>,
    pub(crate) session_id: Option<String>,
    pub(crate) session_file: Option<String>,
    pub(crate) goal: Option<Goal>,
    pub(crate) model: Option<String>,
    pub(crate) requested_model: String,
    pub(crate) turn_started_ms: Option<i64>,
    /// Fault or hold reason.
    pub(crate) reason: Option<String>,
    pub(crate) quota_blocked: Option<String>,
    pub(crate) dialogs: Vec<Dialog>,
    pub(crate) recent: Vec<Entry>,
    /// Changes with every child start (HOST req. 78).
    pub(crate) view: u64,
    pub(crate) stop: Option<StopRecord>,
    pub(crate) last_event: u64,
}

impl Snapshot {
    /// Why this session may not be stopped now: a running turn or an open dialog (IR req. 33).
    pub(crate) fn busy(&self) -> Option<&'static str> {
        if !self.dialogs.is_empty() {
            Some("waits on a dialog")
        } else if self.status == Status::Working {
            Some("is working")
        } else {
            None
        }
    }
}

/// A page of numbered events; `missed` when the caller's number is older than the oldest kept.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct EventPage {
    pub(crate) events: Vec<(u64, SessionEvent)>,
    pub(crate) missed: bool,
}

/// Starts sessions of one runtime (DESIGN 4.1).
pub(crate) trait Runtime: Send + Sync + 'static {
    type Session: Control;

    /// The runtime's name as stored with each session (`headless`, `tern`).
    fn kind(&self) -> &'static str;

    /// Spawns and binds a session; returns only after the handshake completed or failed (HOST req. 28).
    fn start(
        &self,
        spec: StartSpec,
    ) -> impl Future<Output = Result<(Arc<Self::Session>, Started), StartError>> + Send;
}

/// One live session. State-changing calls run one at a time; a second is refused as busy (HOST
/// req. 3). [`Control::snapshot`] never waits.
pub(crate) trait Control: Send + Sync + 'static {
    fn snapshot(&self) -> Arc<Snapshot>;
    /// Follows the snapshot: changes when a readback or an event moves the state.
    fn states(&self) -> tokio::sync::watch::Receiver<Arc<Snapshot>>;
    fn subscribe(&self) -> broadcast::Receiver<(u64, SessionEvent)>;
    fn events_after(&self, after: u64, limit: usize) -> EventPage;
    /// Whether the child is still running.
    fn alive(&self) -> bool;
    /// Reads the state back (HOST req. 5); a caller may overlap it with its own work.
    fn refresh(&self) -> impl Future<Output = Result<(), String>> + Send;
    fn deliver(&self, delivery: Delivery) -> impl Future<Output = DeliverOutcome> + Send;
    fn interrupt(&self) -> impl Future<Output = Result<String, String>> + Send;
    fn set_model(&self, selector: &str) -> impl Future<Output = Result<String, String>> + Send;
    /// One OMP goal operation (`create`, `pause`, `resume`, `drop`, `get`) followed by a fresh
    /// readback; returns the goal the readback shows (HOST-19).
    fn goal(
        &self,
        op: &str,
        objective: Option<&str>,
    ) -> impl Future<Output = Result<Goal, String>> + Send;
    fn answer(
        &self,
        session_id: &str,
        dialog_id: &str,
        answer: &Value,
    ) -> impl Future<Output = Result<(), String>> + Send;
    /// Accepts the observed model after a mismatch hold (HOST req. 27).
    fn accept_model(&self) -> Result<(), String>;
    fn close(&self, force: bool) -> impl Future<Output = Result<StopRecord, String>> + Send;
}

/// A random lower-case hex token of `bytes` bytes.
pub(crate) fn random_hex(bytes: usize) -> String {
    use ring::rand::SecureRandom as _;
    let mut buf = vec![0u8; bytes];
    if ring::rand::SystemRandom::new().fill(&mut buf).is_err() {
        // The system generator does not fail on supported platforms; fall back to time and PID.
        let seed = format!("{}-{}", crate::sys::now_ms(), std::process::id());
        let digest = ring::digest::digest(&ring::digest::SHA256, seed.as_bytes());
        buf.iter_mut()
            .zip(digest.as_ref().iter().cycle())
            .for_each(|(b, d)| *b = *d);
    }
    crate::sys::hex(&buf)
}
