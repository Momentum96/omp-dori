//! The shared relay pipeline (DESIGN 5.1): after a relay's owner gate has queued a message durably,
//! this hands it to the lead through the lead's inbox (owner messages, `owner: true`, the relay
//! message ID as request ID), adds the seen mark in parallel (DESIGN 12.2 C9), records the owed
//! reply, finishes the queue item in one transaction once the session file confirms the hand-off
//! (DISC-IN req. 49), holds the room when the hand-off is in doubt and tells the owner (DESIGN 9.3),
//! and posts the lead's final answer as a linked reply in parts from a durable outbox (DISC-OUT req.
//! 20-29, DESIGN 12.2 A16).

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use super::{Inbound, PartOutcome, Relay};
use crate::host::inbox::{self, Message, ReceiptState};
use crate::host::lead::{Events, Lead};
use crate::host::{Runtime, SessionEvent, Status};
use crate::jobs::event::{Emit, EventKind, Severity};
use crate::log;
use crate::omp::transcript::{self, Reply};
use crate::store::Store;
use crate::store::relay::{HandoffRow, OWED_WINDOW_MS, QueueRow, RelayWrite};
use crate::sys::latency::Latency;
use crate::sys::now_ms;

/// Delivered text over this size goes to a private file (DISC-IN req. 45).
const SPILL_BYTES: usize = 64 * 1024;
/// The owed reply keeps this much of the request (DISC-OUT req. 32).
const OWED_UNITS: usize = 500;

/// Where a hand-off stands after a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    /// Confirmed in the session file and finished.
    Done,
    /// In the lead's inbox or written; not confirmed yet.
    Waiting,
    /// In doubt, failed or conflicting: the room is held.
    Held(String),
}

/// The pipeline of one relay.
pub(crate) struct Pipeline<R: Relay, T: Runtime> {
    pub(crate) relay: Arc<R>,
    store: Store,
    lead: Arc<Lead<T>>,
    spill_dir: PathBuf,
    events: Events,
    /// Sources whose stuck delivery was logged (once per request, DISC-OUT req. 27).
    stuck: Mutex<HashSet<String>>,
    /// A failed seen mark is logged once per run (MSG-04).
    seen_failed: AtomicBool,
    /// Frame read time of a wake still waiting for its turn to start.
    wake: Mutex<Option<Instant>>,
    /// Relay frame read to OMP `agent_start` (DESIGN 2.3, wake end to end).
    pub(crate) wake_latency: Latency,
}

impl<R: Relay, T: Runtime> Pipeline<R, T> {
    pub(crate) fn new(
        relay: Arc<R>,
        store: Store,
        lead: Arc<Lead<T>>,
        spill_dir: PathBuf,
        events: Events,
        wake_latency: Latency,
    ) -> Self {
        Self {
            relay,
            store,
            lead,
            spill_dir,
            events,
            stuck: Mutex::default(),
            seen_failed: AtomicBool::new(false),
            wake: Mutex::new(None),
            wake_latency,
        }
    }

    /// The tag that names a request in the envelope and the session file.
    pub(crate) fn tag(room: &str, message: &str) -> String {
        format!("[{} {room}/{message}]", R::NAME)
    }

    fn prefix() -> String {
        format!("[{} ", R::NAME)
    }

    fn request_id(room: &str, message: &str) -> String {
        format!("{}:{room}:{message}", R::NAME)
    }

    /// The text the lead receives (DISC-IN req. 35-38): the tag and the owner's authenticated
    /// origin, data lines the owner did not write, attachments, then the owner's own words quoted.
    pub(crate) fn envelope(inbound: &Inbound) -> String {
        let mut text = format!(
            "{} request from the owner (authenticated), room {}",
            Self::tag(&inbound.room, &inbound.message),
            inbound.room
        );
        if let Some(parent) = &inbound.parent {
            let _ = write!(text, " (a thread of {parent})");
        }
        for line in &inbound.data {
            text.push_str("\n[data] ");
            text.push_str(line);
        }
        for line in &inbound.attachments {
            text.push_str("\n[file] ");
            text.push_str(line);
        }
        text.push_str("\nThe owner wrote:\n");
        text.push_str(&super::discord::types::quote(&inbound.text));
        text
    }

    /// Writes an oversized text to a private file and says where it is.
    fn spill(&self, inbound: &Inbound, text: String) -> String {
        if text.len() <= SPILL_BYTES {
            return text;
        }
        let path = self.spill_dir.join(format!(
            "{}-{}-{}.txt",
            R::NAME,
            inbound.room,
            inbound.message
        ));
        let written = crate::sys::fs::ensure_private_dir(&self.spill_dir)
            .and_then(|()| crate::sys::fs::write_atomic(&path, text.as_bytes()));
        match written {
            Ok(()) => format!(
                "{} request from the owner (authenticated), room {}\nThe message is {} bytes; read it \
                 from {}",
                Self::tag(&inbound.room, &inbound.message),
                inbound.room,
                text.len(),
                path.display()
            ),
            Err(e) => {
                log::warn("oversized message not saved", &[("error", &e)]);
                let mut cut = text;
                let mut end = SPILL_BYTES;
                while !cut.is_char_boundary(end) {
                    end -= 1;
                }
                cut.truncate(end);
                cut.push_str("\n[cut: the message was too long and could not be saved]");
                cut
            }
        }
    }

    /// Adds the seen mark in the background and records it (MSG-04, DISC-OUT req. 30).
    pub(crate) fn mark_seen(self: &Arc<Self>, room: &str, message: &str) {
        let me = Arc::clone(self);
        let (room, message) = (room.to_string(), message.to_string());
        tokio::spawn(async move {
            let marked = me
                .store
                .read({
                    let (room, message) = (room.clone(), message.clone());
                    move |r| r.read_marked(R::NAME, &room, &message)
                })
                .await
                .unwrap_or(false);
            if marked {
                return;
            }
            match me.relay.seen(&room, &message).await {
                Ok(()) => {
                    let write = RelayWrite::ReadMark {
                        relay: R::NAME.into(),
                        room,
                        message,
                    };
                    if let Err(e) = me.store.relay(write).await {
                        log::warn("read mark not recorded", &[("error", &e)]);
                    }
                }
                Err(e) => {
                    if !me.seen_failed.swap(true, Ordering::AcqRel) {
                        log::warn("seen mark failed", &[("relay", &R::NAME), ("error", &e)]);
                    }
                }
            }
        });
    }

    /// A queued message taken by another route (an answer to a question): finished, marked seen.
    pub(crate) async fn taken(self: &Arc<Self>, row: &QueueRow) -> Result<(), String> {
        self.mark_seen(&row.room, &row.message);
        self.finish(&row.room, &row.message, None).await
    }

    /// Removes a queue item and moves the position; `state` sets the hand-off's.
    pub(crate) async fn finish(
        &self,
        room: &str,
        message: &str,
        state: Option<&str>,
    ) -> Result<(), String> {
        self.store
            .relay(RelayWrite::Finish {
                relay: R::NAME.into(),
                room: room.into(),
                message: message.into(),
                state: state.map(str::to_string),
            })
            .await
            .map(drop)
            .map_err(|e| e.to_string())
    }

    /// Hands a queued owner message to the lead (DISC-IN req. 46-47). `read_at` is its frame's read
    /// time when it came live, for the wake latency.
    pub(crate) async fn hand_off(
        self: &Arc<Self>,
        row: &QueueRow,
        inbound: &Inbound,
        read_at: Option<Instant>,
    ) -> Result<Step, String> {
        let digest = inbox::digest("relay", &inbound.stable);
        let found = {
            let (room, message) = (row.room.clone(), row.message.clone());
            self.store
                .read(move |r| r.handoff(R::NAME, &room, &message))
                .await
                .map_err(|e| e.to_string())?
        };
        if let Some(h) = &found {
            if h.digest != digest {
                // The same message with other content: never resent silently (DISC-IN req. 51).
                return self
                    .hold(
                        &row.room,
                        &row.message,
                        "failed",
                        "the message changed since its hand-off",
                    )
                    .await;
            }
            if matches!(h.state.as_str(), "in_doubt" | "failed") {
                return Ok(Step::Held("an earlier hand-off is in doubt".into()));
            }
        }
        let text = self.spill(inbound, Self::envelope(inbound));
        let request_id = Self::request_id(&row.room, &row.message);
        let owed: String = {
            let mut units = 0;
            inbound
                .text
                .chars()
                .take_while(|c| {
                    units += c.len_utf16();
                    units <= OWED_UNITS
                })
                .collect()
        };
        self.store
            .relay(RelayWrite::Handed {
                relay: R::NAME.into(),
                room: row.room.clone(),
                message: row.message.clone(),
                seq: row.seq,
                target: inbox::LEAD.into(),
                request_id: request_id.clone(),
                digest,
                owed: Some(owed),
            })
            .await
            .map_err(|e| e.to_string())?;
        self.mark_seen(&row.room, &row.message);
        let idle = self
            .lead
            .snapshot()
            .is_some_and(|s| s.status == Status::Idle);
        let receipt = self
            .lead
            .deliver(Message {
                sender: "owner".into(),
                text,
                urgent: true,
                owner: true,
                identity: None,
                request_id: Some(request_id),
            })
            .await;
        let receipt = match receipt {
            Ok(r) => r,
            Err(refused) => {
                return self.hold(&row.room, &row.message, "failed", &refused).await;
            }
        };
        if idle
            && let Some(at) = read_at
            && matches!(receipt.state, ReceiptState::Sent | ReceiptState::Delivered)
        {
            *self.wake.lock().unwrap_or_else(PoisonError::into_inner) = Some(at);
        }
        self.apply(
            &row.room,
            &row.message,
            receipt.state,
            receipt.reason.as_deref(),
        )
        .await
    }

    /// Reads a hand-off's receipt again; a hand-off the inbox never got is made again.
    pub(crate) async fn recheck(
        self: &Arc<Self>,
        row: &QueueRow,
        h: &HandoffRow,
        inbound: &Inbound,
    ) -> Result<Step, String> {
        match self.lead.receipt(&h.request_id).await? {
            Some(r) => {
                self.apply(&row.room, &row.message, r.state, r.reason.as_deref())
                    .await
            }
            None => self.hand_off(row, inbound, None).await,
        }
    }

    async fn apply(
        &self,
        room: &str,
        message: &str,
        state: ReceiptState,
        reason: Option<&str>,
    ) -> Result<Step, String> {
        match state {
            ReceiptState::Delivered | ReceiptState::Duplicate => {
                self.finish(room, message, Some("delivered")).await?;
                Ok(Step::Done)
            }
            ReceiptState::Waiting | ReceiptState::Sent => Ok(Step::Waiting),
            ReceiptState::InDoubt => {
                self.hold(
                    room,
                    message,
                    "in_doubt",
                    reason.unwrap_or("the lead may or may not have it"),
                )
                .await
            }
            ReceiptState::Failed => {
                self.hold(
                    room,
                    message,
                    "failed",
                    reason.unwrap_or("the hand-off failed"),
                )
                .await
            }
        }
    }

    /// Holds the room and tells the owner once, in the room and as an urgent event (DESIGN 9.3).
    async fn hold(
        &self,
        room: &str,
        message: &str,
        state: &str,
        reason: &str,
    ) -> Result<Step, String> {
        let why = format!("message {message}: {reason}");
        self.store
            .relay(RelayWrite::Doubt {
                relay: R::NAME.into(),
                room: room.into(),
                message: message.into(),
                state: state.into(),
                reason: why.clone(),
            })
            .await
            .map_err(|e| e.to_string())?;
        log::warn(
            "relay room held",
            &[("relay", &R::NAME), ("room", &room), ("reason", &why)],
        );
        (self.events)(
            Emit::new(EventKind::RelayHeld, Severity::Warning, true, R::NAME)
                .with("relay", R::NAME)
                .with("room", room)
                .with("reason", &why),
        );
        let note = format!(
            "This message may not have reached the lead ({reason}). Nothing more from this room goes \
             to the lead until you release it with `cataly {} release {room}`.",
            R::NAME
        );
        let nonce = self.relay.nonce(room, message, 9_999);
        if let PartOutcome::Rejected(e) | PartOutcome::Uncertain(e) =
            self.relay.post(room, Some(message), &note, &nonce).await
        {
            log::warn("hold notice not posted", &[("error", &e)]);
        }
        Ok(Step::Held(why))
    }

    /// Follows the lead's events: a turn start after a wake records the wake latency.
    pub(crate) fn on_event(&self, event: &SessionEvent) {
        if *event == SessionEvent::TurnStarted
            && let Some(at) = self
                .wake
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
        {
            self.wake_latency.record(at.elapsed());
        }
    }

    /// The room of the newest request whose answer is still being written, for typing.
    pub(crate) async fn typing_room(&self) -> Option<String> {
        let open = self.store.read(|r| r.answers_open(R::NAME)).await.ok()?;
        open.into_iter()
            .rev()
            .find(|h| h.answer == "waiting")
            .map(|h| h.room)
    }

    /// Reads finished answers from the lead's session file and posts them (DISC-OUT req. 21-28).
    /// `turn_over` is true when the lead's last turn ended and nothing started since, which the
    /// idle readback may not show yet.
    pub(crate) async fn answers(&self, turn_over: bool) -> Result<(), String> {
        let open = self
            .store
            .read(|r| r.answers_open(R::NAME))
            .await
            .map_err(|e| e.to_string())?;
        if open.is_empty() {
            return Ok(());
        }
        let waiting: Vec<&HandoffRow> = open
            .iter()
            // A hand-off not yet confirmed by its receipt counts once its tag is in a user entry of
            // the session file: that entry is the confirmation itself.
            .filter(|h| h.answer == "waiting" && matches!(h.state.as_str(), "delivered" | "handed"))
            .collect();
        if !waiting.is_empty()
            && let Some(snap) = self.lead.snapshot()
            && let Some(file) = snap.session_file.clone()
        {
            let settled = turn_over || snap.status == Status::Idle;
            let tags = waiting
                .iter()
                .map(|h| Self::tag(&h.room, &h.message))
                .collect();
            let replies =
                transcript::read_replies(PathBuf::from(file), tags, Self::prefix(), settled)
                    .await
                    .map_err(|e| e.to_string())?;
            for (h, reply) in waiting.iter().zip(replies) {
                self.settle_reply(h, reply).await?;
            }
        }
        // One delivery at a time, in request order (DISC-OUT req. 28).
        let posting = self
            .store
            .read(|r| r.answers_open(R::NAME))
            .await
            .map_err(|e| e.to_string())?;
        for h in posting.iter().filter(|h| h.answer == "posting") {
            if !self.post_parts(h).await? {
                break;
            }
        }
        Ok(())
    }

    async fn settle_reply(&self, h: &HandoffRow, reply: Reply) -> Result<(), String> {
        let answer = |answer: &str| RelayWrite::Answer {
            relay: R::NAME.into(),
            room: h.room.clone(),
            message: h.message.clone(),
            answer: answer.into(),
        };
        match reply {
            Reply::Answer(text) => match self.relay.parts(&text) {
                Ok(parts) => {
                    let parts = parts
                        .into_iter()
                        .enumerate()
                        .map(|(i, p)| (self.relay.nonce(&h.room, &h.message, i), p))
                        .collect();
                    self.store
                        .relay(RelayWrite::Outbox {
                            relay: R::NAME.into(),
                            room: h.room.clone(),
                            source: h.message.clone(),
                            parts,
                        })
                        .await
                        .map_err(|e| e.to_string())?;
                }
                Err(why) => {
                    log::warn(
                        "answer not posted",
                        &[("message", &h.message), ("reason", &why)],
                    );
                    self.store
                        .relay(answer("abandoned"))
                        .await
                        .map_err(|e| e.to_string())?;
                }
            },
            Reply::Abandoned => {
                self.store
                    .relay(answer("abandoned"))
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Reply::NotFound | Reply::Pending => {
                if now_ms() - h.created_ms > OWED_WINDOW_MS {
                    self.store
                        .relay(answer("abandoned"))
                        .await
                        .map_err(|e| e.to_string())?;
                }
            }
        }
        Ok(())
    }

    /// Posts the pending parts of one answer in order; `false` when it stopped on a failure.
    async fn post_parts(&self, h: &HandoffRow) -> Result<bool, String> {
        let parts = {
            let (room, source) = (h.room.clone(), h.message.clone());
            self.store
                .read(move |r| r.parts(R::NAME, &room, &source))
                .await
                .map_err(|e| e.to_string())?
        };
        for part in parts.iter().filter(|p| p.state == "pending") {
            let reply_to = (part.idx == 0).then_some(h.message.as_str());
            match self
                .relay
                .post(&h.room, reply_to, &part.text, &part.nonce)
                .await
            {
                PartOutcome::Posted { id, linked } => {
                    self.store
                        .relay(RelayWrite::PartDone {
                            relay: R::NAME.into(),
                            room: h.room.clone(),
                            source: h.message.clone(),
                            idx: part.idx,
                            remote: id,
                            linked,
                        })
                        .await
                        .map_err(|e| e.to_string())?;
                }
                PartOutcome::SourceGone => {
                    log::info(
                        "answer dropped: its request was deleted",
                        &[("message", &h.message)],
                    );
                    self.store
                        .relay(RelayWrite::Answer {
                            relay: R::NAME.into(),
                            room: h.room.clone(),
                            message: h.message.clone(),
                            answer: "dropped".into(),
                        })
                        .await
                        .map_err(|e| e.to_string())?;
                    return Ok(true);
                }
                PartOutcome::Rejected(why) | PartOutcome::Uncertain(why) => {
                    let first = self
                        .stuck
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(h.message.clone());
                    if first {
                        log::warn(
                            "answer part not posted; retrying with the same nonce",
                            &[
                                ("message", &h.message),
                                ("part", &part.idx),
                                ("reason", &why),
                            ],
                        );
                    }
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
}
