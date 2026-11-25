//! Relays (DESIGN 5.1): one trait per chat platform and one shared pipeline. A relay normalises what
//! it receives into an [`Inbound`] owner message (MSG-01) after its owner gate (MSG-02); everything
//! after that is shared ([`pipeline`], [`intake`]): the durable arrival queue, hand-off to the lead
//! through its inbox, the read mark, the owed reply, the answer posted as a reply in parts, the
//! optional channel facts and earlier-conversation lines ([`context`]), and fencing a room when a
//! hand-off is in doubt (DESIGN 9.3). Discord, Telegram and Slack implement the same [`Relay`] trait.

pub(crate) mod boards;
pub(crate) mod context;
pub(crate) mod discord;
pub(crate) mod http;
pub(crate) mod intake;
pub(crate) mod pipeline;
pub(crate) mod slack;
pub(crate) mod telegram;
pub(crate) mod text;

/// One admitted owner message, normalised (MSG-01; DISC-IN req. 35-39).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Inbound {
    pub(crate) room: String,
    /// The watched parent of a followed thread.
    pub(crate) parent: Option<String>,
    pub(crate) message: String,
    /// Orders messages within the room.
    pub(crate) seq: i64,
    pub(crate) reply_to: Option<String>,
    /// The owner's own text, exactly.
    pub(crate) text: String,
    /// Lines the owner did not write (reply context): passed as data, never as the request
    /// (OPC-08, OPC-11).
    pub(crate) data: Vec<String>,
    /// One line per attachment.
    pub(crate) attachments: Vec<String>,
    /// No reference, no forward, no attachment (DISC-IN req. 39).
    pub(crate) plain: bool,
    /// The same message with expiring URL parts removed, for the hand-off digest (DISC-IN req. 47).
    pub(crate) stable: String,
}

/// How posting one part ended (MSG-05; DISC-OUT req. 5-6, 16, 26).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PartOutcome {
    /// Created; `linked` when it is a confirmed reply to the requested message.
    Posted { id: String, linked: bool },
    /// The message to reply to no longer exists: nothing more is posted for it.
    SourceGone,
    /// Nothing was created.
    Rejected(String),
    /// It may exist; a part with a deterministic enforced nonce may be posted again.
    Uncertain(String),
}

/// What a relay knows about one room, for the channel facts line (DISC-IN req. 53).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    pub(crate) name: Option<String>,
    pub(crate) topic: Option<String>,
    /// A direct conversation with the owner.
    pub(crate) dm: bool,
}

/// One chat platform (DESIGN 5.1).
pub(crate) trait Relay: Send + Sync + 'static {
    /// Store key and envelope label: `discord`, `telegram`, `slack`.
    const NAME: &'static str;
    /// A part whose post ended uncertain may be posted again: the platform enforces the nonce
    /// (Discord). Elsewhere an uncertain part is never repeated (MSG req. 8, DESIGN 5.1).
    const REPEATABLE: bool;
    /// A live message is never received live again once the hub restarts: one whose arrival did
    /// not commit before a crash comes back only through the catch-up read, as a message not read
    /// live, so the lead's transcript is searched before any send (Discord: a new Gateway session
    /// replays nothing). The round may then hand it off while its arrival commits (decision 15).
    /// Telegram and Slack redeliver what was not acknowledged as live messages: they commit first.
    const LIVE_ONCE: bool;

    /// The answer text as the parts this platform takes (outgoing checks applied, tables rendered,
    /// split at its limit), or why it cannot be posted.
    fn parts(&self, text: &str) -> Result<Vec<String>, String>;

    /// The deterministic nonce of part `index` of the reply to `source` in `room`.
    fn nonce(&self, room: &str, source: &str, index: usize) -> String;

    /// The queued raw message as the normalised owner message (MSG-01).
    fn normalise(&self, row: &crate::store::relay::QueueRow) -> Result<Inbound, String>;

    /// Posts one part; the first part of an answer replies to `reply_to`.
    fn post(
        &self,
        room: &str,
        reply_to: Option<&str>,
        text: &str,
        nonce: &str,
    ) -> impl Future<Output = PartOutcome> + Send;

    /// Adds the seen mark to an admitted message (MSG-04).
    fn seen(&self, room: &str, message: &str) -> impl Future<Output = Result<(), String>> + Send;

    /// Shows that an answer is being written; relays without such a signal say so.
    fn typing(&self, room: &str) -> impl Future<Output = Result<(), String>> + Send;

    /// The room's name, topic and kind, one bounded lookup (DISC-IN req. 53).
    fn facts(&self, room: &str) -> impl Future<Output = Result<Facts, String>> + Send;
}
