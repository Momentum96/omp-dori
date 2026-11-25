//! Relays (DESIGN 5.1): one trait per chat platform and one shared pipeline. A relay normalises what
//! it receives into an [`Inbound`] owner message (MSG-01) after its owner gate (MSG-02); everything
//! after that is shared ([`pipeline`]): the durable arrival queue, hand-off to the lead through its
//! inbox, the read mark, the owed reply, the answer posted as a linked reply in parts, and fencing a
//! room when a hand-off is in doubt (DESIGN 9.3). Discord is the first relay; Telegram and Slack
//! implement the same [`Relay`] trait later.

pub(crate) mod discord;
pub(crate) mod http;
pub(crate) mod pipeline;
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

/// One chat platform (DESIGN 5.1).
pub(crate) trait Relay: Send + Sync + 'static {
    /// Store key and envelope label: `discord`, `telegram`, `slack`.
    const NAME: &'static str;

    /// The answer text as the parts this platform takes (outgoing checks applied, tables rendered,
    /// split at its limit), or why it cannot be posted.
    fn parts(&self, text: &str) -> Result<Vec<String>, String>;

    /// The deterministic nonce of part `index` of the reply to `source` in `room`.
    fn nonce(&self, room: &str, source: &str, index: usize) -> String;

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
}
