//! The narrow seam to the hub (`DESIGN.md` 10.1): network connections under the hub's HTTP and TLS
//! policy, the one Discord Gateway, lead delivery with receipt proof, status reads, note posts and
//! the durable voice records. `cataly` implements it; tests use a fake.

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};

use crate::ledger::Day;

/// A connected byte stream (TCP, or TLS over TCP).
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// How an HTTP POST ended.
#[derive(Debug, Clone)]
pub enum Posted {
    /// A 2xx answer and its body.
    Done(Vec<u8>),
    /// A non-2xx answer: nothing happened.
    Rejected(u16, Vec<u8>),
    /// The request may or may not have taken effect.
    Uncertain(String),
    /// Nothing was sent.
    Failed(String),
}

/// What pass-to-lead reports; exactly one of the three (VOICE req. 73-75).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Passed {
    /// Not sent; the reason says why. Never retried.
    Refused(String),
    /// OMP reported a user message with the request ID in the same lead session within 10 s.
    Received,
    /// Sent once; no proof arrived in time. Never resent.
    Unproven,
}

impl Passed {
    /// The words the voice model is told.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Refused(why) => format!("not delivered: {why}"),
            Self::Received => "delivered: the lead has the request".into(),
            Self::Unproven => {
                "sent, receipt unproven: treat it as not delivered and do not repeat it".into()
            }
        }
    }
}

/// One part of a status read (VOICE req. 69).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusPart {
    /// Every open lane with its live state.
    Lanes,
    /// The newest reports.
    Reports,
}

/// A status part's text with its read time, or why it failed.
#[derive(Debug, Clone)]
pub struct StatusRead {
    /// Which part.
    pub part: StatusPart,
    /// When it was read, in Unix milliseconds.
    pub read_ms: i64,
    /// The text, or why the read failed.
    pub text: Result<String, String>,
}

/// A queued lead reply (VOICE-17).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// The queue row.
    pub id: i64,
    /// The voice request the reply answers.
    pub request_id: String,
    /// A short subject.
    pub subject: String,
    /// The reply.
    pub text: String,
    /// Bytes of `text` already spoken.
    pub spoken: usize,
}

/// The ledger could not be read or written; voice stops (VOICE req. 82-83).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerError(pub String);

/// A Discord Gateway fact voice needs (VOICE-03): only the configured guild's snapshot and
/// voice-state updates reach it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayEvent {
    /// READY: the bot's user ID.
    Ready {
        /// The bot's user ID.
        bot: u64,
    },
    /// The guild snapshot: the configured channel's type, and who sits in which voice channel.
    Guild {
        /// Discord's type of the configured voice channel, if it exists.
        channel_type: Option<u64>,
        /// User, channel and voice session ID of everyone in a voice channel.
        voice_states: Vec<(u64, Option<u64>, String)>,
    },
    /// A voice-state update: user, channel (none when they left), voice session ID.
    VoiceState {
        /// Whose state changed.
        user: u64,
        /// Their channel, none when they left.
        channel: Option<u64>,
        /// Their voice session ID.
        session_id: String,
    },
    /// A voice-server update: token and endpoint (`None` while Discord moves the call).
    VoiceServer {
        /// The voice token.
        token: String,
        /// The voice server, `None` while Discord moves the call.
        endpoint: Option<String>,
    },
    /// The Gateway connection went down.
    Down,
}

/// Everything voice needs from the hub.
pub trait VoiceHost: Send + Sync + 'static {
    /// Opens a TCP connection, with TLS when `tls`; plain only to loopback.
    fn connect(
        &self,
        tls: bool,
        host: &str,
        port: u16,
    ) -> impl Future<Output = Result<Box<dyn Io>, String>> + Send;

    /// POSTs a JSON body with a bearer key, one attempt bounded by `deadline`, never repeated.
    fn post_json(
        &self,
        url: &str,
        key: &str,
        body: Vec<u8>,
        deadline: Duration,
    ) -> impl Future<Output = Posted> + Send;

    /// Sends Gateway op 4 (join with `Some(channel)`, leave with `None`), neither muted nor deafened.
    fn voice_state(
        &self,
        guild: u64,
        channel: Option<u64>,
    ) -> impl Future<Output = Result<(), String>> + Send;

    /// Passes one cleaned request to the lead and proves its receipt (VOICE-15).
    fn pass_to_lead(&self, request_id: &str, text: &str) -> impl Future<Output = Passed> + Send;

    /// Reads one status part (VOICE-14).
    fn status(&self, part: StatusPart) -> impl Future<Output = StatusRead> + Send;

    /// Posts text to a Discord channel.
    fn post(&self, channel: u64, text: &str) -> impl Future<Output = Result<(), String>> + Send;

    /// Today's row in `zone` and the speech seconds of every stored day.
    fn ledger(&self, zone: &str) -> impl Future<Output = Result<(Day, f64), LedgerError>> + Send;

    /// Adds usage to today's row in `zone`.
    fn record(
        &self,
        zone: &str,
        speech_seconds: f64,
        sessions: u32,
        spend_usd: f64,
    ) -> impl Future<Output = Result<(), LedgerError>> + Send;

    /// The durable hold reason, if voice is held (VOICE req. 83).
    fn held(&self) -> impl Future<Output = Result<Option<String>, LedgerError>> + Send;

    /// Records a hold; it survives restarts until the owner restarts voice.
    fn hold(&self, reason: &str) -> impl Future<Output = Result<(), LedgerError>> + Send;

    /// Lead replies in arrival order.
    fn replies(&self) -> impl Future<Output = Result<Vec<Reply>, LedgerError>> + Send;

    /// Records how far a reply was spoken; at its full length the reply leaves the queue.
    fn spoken(
        &self,
        id: i64,
        spoken: usize,
    ) -> impl Future<Output = Result<(), LedgerError>> + Send;

    /// Writes one log line (no secrets, no audio, no transcript).
    fn log(&self, message: &str);

    /// Whether today's cap notice was already posted, marking it posted (one notice per day).
    fn cap_notice_due(&self, zone: &str) -> impl Future<Output = bool> + Send;
}
