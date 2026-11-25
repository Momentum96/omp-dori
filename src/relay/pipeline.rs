//! The shared relay pipeline (DESIGN 5.1): after a relay's owner gate has queued a message durably,
//! this hands it to the lead through the lead's inbox (owner messages, `owner: true`, the relay
//! message ID as request ID), adds the seen mark in parallel (DESIGN 12.2 C9), records the owed
//! reply, finishes the queue item in one transaction once the session file confirms the hand-off
//! (DISC-IN req. 49), holds the room when the hand-off is in doubt and tells the owner (DESIGN 9.3),
//! and posts the lead's final answer as a linked reply in parts from a durable outbox (DISC-OUT req.
//! 20-29, DESIGN 12.2 A16).

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use tokio::sync::{Notify, watch};

use super::context::{self, Context};
use super::{Inbound, PartOutcome, Relay};
use crate::config::Config;
use crate::host::inbox::{self, Message, ReceiptState};
use crate::host::lead::{Events, Lead};
use crate::host::{Runtime, SessionEvent, Status};
use crate::jobs::event::{Emit, EventKind, Severity};
use crate::log;
use crate::omp::transcript::{self, Reply};
use crate::store::Store;
use crate::store::relay::{Arrived, HandoffRow, OWED_WINDOW_MS, QueueRow, RelayDone, RelayWrite};
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
    /// Not handed yet and to be tried again next round; later messages of its room wait behind it.
    Deferred,
}

/// An owner message for the arrival queue, live or found by a catch-up read.
pub(crate) struct Arrival {
    pub(crate) room: String,
    pub(crate) message: String,
    pub(crate) seq: i64,
    pub(crate) parent: Option<String>,
    pub(crate) raw: String,
}

/// A live message not handed off yet.
struct Live {
    /// When its frame or poll response was read, for the wake latency.
    read_at: Instant,
    /// Its queue item while the arrival commit runs (decision 15); `None` once committed.
    row: Option<QueueRow>,
}

/// The pipeline of one relay.
pub(crate) struct Pipeline<R: Relay, T: Runtime> {
    pub(crate) relay: Arc<R>,
    store: Store,
    lead: Arc<Lead<T>>,
    /// The owning lane of a thread takes its owner messages (DISC-IN-14).
    lanes: Arc<crate::lanes::Lanes<T>>,
    spill_dir: PathBuf,
    events: Events,
    /// `[context]` is read from here at each hand-off.
    config: watch::Receiver<Arc<Config>>,
    context: Context,
    /// Sources whose stuck delivery was logged (once per request, DISC-OUT req. 27).
    stuck: Mutex<HashSet<String>>,
    /// A failed seen mark is logged once per run (MSG-04).
    seen_failed: AtomicBool,
    /// Live messages not handed off yet, by room and message.
    live: Mutex<HashMap<(String, String), Live>>,
    /// Frame read time of a wake still waiting for its turn to start.
    wake: Mutex<Option<Instant>>,
    /// Relay frame read to OMP `agent_start` (DESIGN 2.3, wake end to end).
    pub(crate) wake_latency: Latency,
}

/// What a relay's pipeline needs from the hub besides the relay itself.
pub(crate) struct Parts<T: Runtime> {
    pub(crate) store: Store,
    pub(crate) lead: Arc<Lead<T>>,
    pub(crate) lanes: Arc<crate::lanes::Lanes<T>>,
    pub(crate) spill_dir: PathBuf,
    pub(crate) events: Events,
    pub(crate) config: watch::Receiver<Arc<Config>>,
}

impl<R: Relay, T: Runtime> Pipeline<R, T> {
    pub(crate) fn new(relay: Arc<R>, parts: Parts<T>, wake_latency: Latency) -> Self {
        Self {
            relay,
            store: parts.store,
            lead: parts.lead,
            lanes: parts.lanes,
            spill_dir: parts.spill_dir,
            events: parts.events,
            config: parts.config,
            context: Context::default(),
            stuck: Mutex::default(),
            seen_failed: AtomicBool::new(false),
            live: Mutex::default(),
            wake: Mutex::new(None),
            wake_latency,
        }
    }

    fn live(&self) -> MutexGuard<'_, HashMap<(String, String), Live>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues an owner message durably (DISC-IN req. 22, MSG req. 4): its position moves only after
    /// the hand-off. `read_at` is when a live message's frame or poll response was read, `None`
    /// for one found by a catch-up read; `wake` starts the handling round.
    ///
    /// Decision 15: on a relay whose live messages never come back live after a restart
    /// ([`Relay::LIVE_ONCE`]), a live message the store would queue now goes to the round at once
    /// and its arrival commits alongside the hand-off. The arrival is queued in the writer before
    /// the round can see the message, so the hand-off record and the lead's records commit with or
    /// after it, never before. A crash before it commits leaves the room's position below the
    /// message: the catch-up read finds it again, and the lead's transcript decides before any send.
    pub(crate) async fn arrive(
        &self,
        arrival: Arrival,
        read_at: Option<Instant>,
        wake: &Notify,
    ) -> Result<Arrived, String> {
        let Arrival {
            room,
            message,
            seq,
            parent,
            raw,
        } = arrival;
        let key = (room.clone(), message.clone());
        let early = match read_at {
            Some(_) if R::LIVE_ONCE && !Self::commit_first() => {
                let (r, m, bytes) = (room.clone(), message.clone(), raw.len());
                self.store
                    .read(move |reads| reads.arrives_at_once(R::NAME, &r, &m, seq, bytes))
                    .await
                    .ok()
                    .flatten()
            }
            _ => None,
        };
        let row = early.map(|position_at| QueueRow {
            room: room.clone(),
            message: message.clone(),
            seq,
            parent: parent.clone(),
            position_at,
            raw: raw.clone(),
            created_ms: now_ms(),
        });
        let write = RelayWrite::Arrive {
            relay: R::NAME.into(),
            room,
            message,
            seq,
            parent,
            raw,
        };
        let done = match (row, read_at) {
            (Some(row), Some(at)) => {
                // Debug builds only: the crash test of a hand-off written before its arrival
                // commits holds the arrival back.
                #[cfg(debug_assertions)]
                if crate::sys::fault::named("arrival_after_write") {
                    self.live().insert(
                        key,
                        Live {
                            read_at: at,
                            row: Some(row),
                        },
                    );
                    wake.notify_one();
                    return std::future::pending().await;
                }
                let committed = self
                    .store
                    .relay_queued(write)
                    .await
                    .map_err(|e| e.to_string())?;
                self.live().insert(
                    key.clone(),
                    Live {
                        read_at: at,
                        row: Some(row),
                    },
                );
                wake.notify_one();
                committed.await
            }
            _ => self.store.relay(write).await,
        };
        let arrived = done.map(|d| match d {
            RelayDone::Arrived(a) => a,
            _ => Arrived::Handled,
        });
        // Debug builds only: the crash test of acceptance check 4 stops here, after the arrival
        // commit and before the hand-off.
        #[cfg(debug_assertions)]
        if matches!(arrived, Ok(Arrived::Queued)) {
            crate::sys::fault::point("relay_after_arrival");
        }
        self.settle_live(
            key,
            arrived.as_ref().ok().copied(),
            read_at,
            early.is_some(),
            wake,
        );
        arrived.map_err(|e| e.to_string())
    }

    /// Keeps a live message's entry in step with what its arrival became (`None`: the store
    /// failed). `early`: it went to the round before the commit.
    fn settle_live(
        &self,
        key: (String, String),
        arrived: Option<Arrived>,
        read_at: Option<Instant>,
        early: bool,
        wake: &Notify,
    ) {
        let mut live = self.live();
        match (arrived, read_at) {
            // Committed: the stored item stands in for it, unless the round took it already.
            (Some(Arrived::Queued), Some(_)) if early => {
                if let Some(l) = live.get_mut(&key) {
                    l.row = None;
                }
            }
            (Some(Arrived::Queued), Some(at)) => {
                live.insert(
                    key,
                    Live {
                        read_at: at,
                        row: None,
                    },
                );
                drop(live);
                wake.notify_one();
            }
            // Not queued after all: a round that has not taken it never will.
            _ if early => {
                live.remove(&key);
            }
            _ => {}
        }
    }

    /// Debug builds only: the crash test that stops after the arrival commit and before the
    /// hand-off needs that order.
    #[cfg(debug_assertions)]
    fn commit_first() -> bool {
        crate::sys::fault::named("relay_after_arrival")
    }

    #[cfg(not(debug_assertions))]
    const fn commit_first() -> bool {
        false
    }

    /// The read time of a live message, taken by its hand-off; `None` for anything else.
    pub(crate) fn read_at(&self, room: &str, message: &str) -> Option<Instant> {
        self.live()
            .remove(&(room.to_string(), message.to_string()))
            .map(|l| l.read_at)
    }

    /// Hands queued messages on, lowest first per room; a room with a held or failed message is
    /// skipped for the rest of the round (DISC-IN req. 26-27, 43). `handle` takes one queue row.
    /// Live messages whose arrival still commits take their place in the queue (decision 15).
    pub(crate) async fn drain<F, Fut>(&self, mut handle: F) -> Result<(), String>
    where
        F: FnMut(QueueRow) -> Fut,
        Fut: Future<Output = Result<Step, String>>,
    {
        let (mut queue, positions) = self
            .store
            .read(|r| Ok((r.queue(R::NAME)?, r.positions(R::NAME)?)))
            .await
            .map_err(|e| e.to_string())?;
        let committing: Vec<QueueRow> = self
            .live()
            .values()
            .filter_map(|l| l.row.clone())
            .filter(|row| {
                !queue
                    .iter()
                    .any(|q| q.room == row.room && q.message == row.message)
            })
            .collect();
        if !committing.is_empty() {
            queue.extend(committing);
            queue.sort_by_key(|r| r.seq);
        }
        let mut blocked: HashSet<String> = positions
            .into_iter()
            .filter(|p| p.held.is_some())
            .map(|p| p.room)
            .collect();
        for row in queue {
            if blocked.contains(&row.room) {
                continue;
            }
            let room = row.room.clone();
            match handle(row).await {
                Ok(Step::Done | Step::Waiting) => {}
                Ok(Step::Held(_) | Step::Deferred) => {
                    blocked.insert(room);
                }
                Err(e) => {
                    log::warn(
                        "hand-off not made",
                        &[("relay", &R::NAME), ("room", &room), ("error", &e)],
                    );
                    blocked.insert(room);
                }
            }
        }
        Ok(())
    }

    /// A queued message whose hand-off exists: its receipt is read again, it is finished, or it is
    /// handed off again. `None` when it has no hand-off yet.
    pub(crate) async fn resume(
        self: &Arc<Self>,
        row: &QueueRow,
        inbound: &Inbound,
        read_at: Option<Instant>,
    ) -> Result<Option<Step>, String> {
        let found = {
            let (room, message) = (row.room.clone(), row.message.clone());
            self.store
                .read(move |r| r.handoff(R::NAME, &room, &message))
                .await
                .map_err(|e| e.to_string())?
        };
        Ok(Some(match found {
            Some(h) if h.state == "handed" => self.recheck(row, &h, inbound).await?,
            Some(h) if h.state == "delivered" => {
                self.finish(&row.room, &row.message, Some("delivered"))
                    .await?;
                Step::Done
            }
            Some(h) => self.hand_off(row, inbound, read_at, Some(&h)).await?,
            None => return Ok(None),
        }))
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

    /// The envelope with the optional context lines (DISC-IN req. 53-58).
    async fn text_for(&self, inbound: &Inbound) -> String {
        let settings = self.config.borrow().context.clone();
        let mut text = Self::envelope(inbound);
        let mut extra = Vec::new();
        if settings.facts {
            extra.push(
                self.context
                    .facts_line(&*self.relay, &settings, inbound)
                    .await,
            );
        }
        if settings.lookup && context::refers_back(&inbound.text) {
            let (space, since) = (
                context::space(inbound).to_string(),
                now_ms() - settings.lookup_days * 86_400_000,
            );
            let rows = self
                .store
                .read(move |r| r.recall(R::NAME, &space, since, context::RANKED_ROWS))
                .await;
            match rows {
                Ok(rows) => {
                    let picked = context::rank(&inbound.text, &inbound.message, &rows);
                    if let Some(line) = context::recall_line(&picked, &inbound.text) {
                        extra.push(line);
                    }
                }
                Err(e) => extra.push(format!("[earlier] error {e}")),
            }
        }
        for line in extra {
            text.push('\n');
            text.push_str(&line);
        }
        text
    }

    /// Stores one owner message or lead reply for the earlier-conversation lookup when it is on
    /// (DISC-IN req. 55-56); the space is the room's listed parent, else the room.
    async fn remember(&self, room: &str, message: &str, who: &str, text: &str) {
        let settings = self.config.borrow().context.clone();
        if !settings.lookup {
            return;
        }
        let space = {
            let room = room.to_string();
            self.store
                .read(move |r| r.positions(R::NAME))
                .await
                .ok()
                .and_then(|p| p.into_iter().find(|p| p.room == room)?.parent)
                .unwrap_or_else(|| room.clone())
        };
        let write = RelayWrite::Recall {
            relay: R::NAME.into(),
            space,
            who: who.into(),
            conversation: room.into(),
            message: message.into(),
            text: context::stored(text),
            keep_ms: settings.lookup_days * 86_400_000,
        };
        if let Err(e) = self.store.relay(write).await {
            log::warn("earlier conversation not stored", &[("error", &e)]);
        }
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
    /// time when it came live, for the wake latency; `found` is its hand-off record as the caller
    /// just read it (`None`: it has none).
    pub(crate) async fn hand_off(
        self: &Arc<Self>,
        row: &QueueRow,
        inbound: &Inbound,
        read_at: Option<Instant>,
        found: Option<&HandoffRow>,
    ) -> Result<Step, String> {
        let digest = inbox::digest("relay", &inbound.stable);
        if let Some(h) = found {
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
        let text = self.spill(inbound, self.text_for(inbound).await);
        self.remember(&inbound.room, &inbound.message, "owner", &inbound.text)
            .await;
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
        // A thread one open lane names goes to that lane; two claimants hold the room (DISC-IN
        // req. 41-42). Lanes never answer the owner, so no reply is owed for it (OPC-57).
        let route = self.lanes.route(R::NAME, &row.room).await;
        if let Some(Err(why)) = &route {
            return self.hold(&row.room, &row.message, "failed", why).await;
        }
        let lane = route.and_then(Result::ok);
        let handed = RelayWrite::Handed {
            relay: R::NAME.into(),
            room: row.room.clone(),
            message: row.message.clone(),
            seq: row.seq,
            target: lane.clone().unwrap_or_else(|| inbox::LEAD.into()),
            request_id: request_id.clone(),
            digest,
            owed: lane.is_none().then_some(owed),
        };
        if let Some(key) = lane {
            self.store.relay(handed).await.map_err(|e| e.to_string())?;
            self.mark_seen(&row.room, &row.message);
            let message = Message {
                sender: "owner".into(),
                text,
                urgent: true,
                owner: true,
                identity: None,
                request_id: Some(request_id),
            };
            return match self.lanes.hand_off(&key, message).await {
                Ok(r) => {
                    self.apply(&row.room, &row.message, r.state, r.reason.as_deref())
                        .await
                }
                Err(refused) => self.hold(&row.room, &row.message, "failed", &refused).await,
            };
        }
        self.lead_hand_off(row, handed, text, request_id, read_at)
            .await
    }

    /// The lead's part of a hand-off. Decision 15: the lead's wake can be written before any record
    /// of it lands, so a message not read live in this run (a restart, a catch-up, a hand-off made
    /// again) may already be in the lead's transcript, and the transcript decides before any send.
    /// Otherwise the hand-off record commits alongside the wake instead of before it.
    async fn lead_hand_off(
        self: &Arc<Self>,
        row: &QueueRow,
        handed: RelayWrite,
        text: String,
        request_id: String,
        read_at: Option<Instant>,
    ) -> Result<Step, String> {
        if read_at.is_none() {
            match self.lead.written_earlier(&request_id).await {
                Err(e) => {
                    let why = format!("the lead's transcript could not be searched ({e})");
                    return self.hold(&row.room, &row.message, "in_doubt", &why).await;
                }
                Ok(None) => return Ok(Step::Deferred),
                Ok(Some(true)) => {
                    self.store.relay(handed).await.map_err(|e| e.to_string())?;
                    self.mark_seen(&row.room, &row.message);
                    self.finish(&row.room, &row.message, Some("delivered"))
                        .await?;
                    return Ok(Step::Done);
                }
                Ok(Some(false)) => {}
            }
        }
        self.mark_seen(&row.room, &row.message);
        let (recorded, step) = tokio::join!(
            self.store.relay(handed),
            self.to_lead(row, text, request_id, read_at)
        );
        recorded.map_err(|e| e.to_string())?;
        step
    }

    /// Delivers a handed-off owner message to the lead and records its receipt. The lead may write
    /// it before its records commit: it was read live in this run, or the transcript showed it was
    /// not written earlier (decision 15).
    async fn to_lead(
        self: &Arc<Self>,
        row: &QueueRow,
        text: String,
        request_id: String,
        read_at: Option<Instant>,
    ) -> Result<Step, String> {
        let idle = self
            .lead
            .snapshot()
            .is_some_and(|s| s.status == Status::Idle);
        let receipt = self
            .lead
            .deliver_live(Message {
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
            let mut wake = self.wake.lock().unwrap_or_else(PoisonError::into_inner);
            *wake = Some(at);
            // The turn may have started while the delivery's records committed, its event seen
            // before this wake was set.
            self.settle_wake(&mut wake);
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
        let receipt = if h.target == inbox::LEAD {
            self.lead.receipt(&h.request_id).await?
        } else {
            self.lanes.receipt(&h.target, &h.request_id).await
        };
        match receipt {
            Some(r) => {
                self.apply(&row.room, &row.message, r.state, r.reason.as_deref())
                    .await
            }
            None => self.hand_off(row, inbound, None, Some(h)).await,
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

    /// Follows the lead's events: a turn start after a wake records the wake latency, up to when
    /// the lead read OMP's `agent_start` (this round may follow its events later). A turn start
    /// from before the wake leaves it waiting for the next one.
    pub(crate) fn on_event(&self, event: &SessionEvent) {
        if *event != SessionEvent::TurnStarted {
            return;
        }
        let mut wake = self.wake.lock().unwrap_or_else(PoisonError::into_inner);
        self.settle_wake(&mut wake);
    }

    /// Records the wake latency once the lead's turn started at or after the wake.
    fn settle_wake(&self, wake: &mut Option<Instant>) {
        let Some(at) = *wake else {
            return;
        };
        if let Some(started) = self.lead.turn_started().filter(|s| *s >= at) {
            *wake = None;
            self.wake_latency.record(started - at);
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
        let still: HashSet<&str> = posting
            .iter()
            .filter(|h| h.answer == "posting")
            .map(|h| h.message.as_str())
            .collect();
        keep_only(&self.stuck, &still);
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
                    self.remember(&h.room, &format!("{}:answer", h.message), "lead", &text)
                        .await;
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
                PartOutcome::Uncertain(why) if !R::REPEATABLE => {
                    // No enforced nonce: a repeat could post the part twice (MSG req. 8).
                    log::warn(
                        "answer part uncertain; not posted again",
                        &[
                            ("relay", &R::NAME),
                            ("message", &h.message),
                            ("part", &part.idx),
                            ("reason", &why),
                        ],
                    );
                    self.store
                        .relay(RelayWrite::Answer {
                            relay: R::NAME.into(),
                            room: h.room.clone(),
                            message: h.message.clone(),
                            answer: "abandoned".into(),
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

/// Keeps only the sources still being posted: a delivery that finished or was given up leaves no
/// entry behind.
fn keep_only(stuck: &Mutex<HashSet<String>>, still: &HashSet<&str>) {
    stuck
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .retain(|m| still.contains(m.as_str()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stuck_deliveries_are_forgotten_once_settled() {
        let stuck = Mutex::new(HashSet::new());
        for n in 0..1000 {
            let message = format!("m{n}");
            stuck.lock().unwrap().insert(message.clone());
            keep_only(&stuck, &HashSet::from([message.as_str()]));
        }
        assert_eq!(stuck.lock().unwrap().len(), 1);
        keep_only(&stuck, &HashSet::new());
        assert!(stuck.lock().unwrap().is_empty());
    }
}
