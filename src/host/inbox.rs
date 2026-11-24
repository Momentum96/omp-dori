//! The per-target inbox (COORD-10, COORD-12, COORD-13, COORD-15, COORD-18) and delivery confirmation
//! from session files (PATCH-01 headless rules 1-5, 8-11, 15-17; DESIGN 9.3).
//!
//! A message is stored first, then delivered when its target can read: idle gets a wake (`prompt`),
//! working gets a follow-up, anything else waits. Quiet notes never wake a session; they go out with
//! the next wake. Several pending messages go out as one numbered batch. Every batch is a delivery
//! whose text starts with the marker `[cataly:<id>]`; it is delivered only when a user entry carrying
//! the marker is found in the target session file. While a written batch is unconfirmed nothing else
//! is sent to that target. After 30 s without the entry, with the same OMP process still running, the
//! batch is in doubt: never resent, the lead told once, the owner alerted. When the process that held
//! it is gone and the file lacks the entry, it is resent once with the same ID into the same session.

use std::fmt::Write as _;
use std::time::Instant;

use serde::Serialize;

use super::{Control, DeliverOutcome, Delivery, Status, Timing};
use crate::omp::transcript::{self, Found};
use crate::store::host::{Admit, Admitted, DeliveryRow, HostDone, HostWrite, InboxRow};
use crate::store::{Store, StoreError};
use crate::sys::now_ms;

/// The lead's mailbox.
pub(crate) const LEAD: &str = "lead";
/// A written batch without its entry is in doubt after this long (PATCH-01 rules 4 and 10).
pub(crate) const CONFIRM_WITHIN_MS: i64 = 30_000;
/// Sender label of the hub's own notices.
pub(crate) const HUB_SENDER: &str = "cataly";
/// Identity prefix of in-doubt notices: such a batch in doubt raises no further notice.
const IN_DOUBT_PREFIX: &str = "in-doubt:";
/// Identity prefix of notes that may reach a quota-blocked lead.
pub(crate) const QUOTA_PREFIX: &str = "quota-blocked:";
/// Largest message text.
const TEXT_MAX: usize = 256 * 1024;

/// A message for a target's inbox.
#[derive(Clone, Debug)]
pub(crate) struct Message {
    /// Who it is from, shown to the agent: `owner`, a lane key, `cataly`.
    pub(crate) sender: String,
    pub(crate) text: String,
    /// Wakes an idle session; quiet notes wait for the next wake.
    pub(crate) urgent: bool,
    /// The owner's own message (still delivered while the session is quota-blocked).
    pub(crate) owner: bool,
    /// Once-only identity (COORD req. 19).
    pub(crate) identity: Option<String>,
    /// Caller request ID for an idempotent submission (HOST req. 56-57).
    pub(crate) request_id: Option<String>,
}

/// Where a message stands (COORD req. 25, HOST req. 58).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReceiptState {
    /// Stored; it goes out when the target can read.
    Waiting,
    /// Written to OMP and admitted; the session-file entry is not confirmed yet.
    Sent,
    /// Found in the target session file.
    Delivered,
    /// It may have arrived: never resent; confirm or release it.
    InDoubt,
    Failed,
    /// An identical once-only message is pending or was delivered recently.
    Duplicate,
}

/// The receipt of one submission. It states admission, never which reply belongs to it (HOST req. 61).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Receipt {
    pub(crate) request_id: Option<String>,
    pub(crate) inbox_id: Option<i64>,
    pub(crate) state: ReceiptState,
    pub(crate) delivery_id: Option<String>,
    pub(crate) reason: Option<String>,
}

/// An inbox entry as `cataly lead inbox` lists it (COORD req. 21).
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Listed {
    pub(crate) index: usize,
    pub(crate) state: String,
    pub(crate) sender: String,
    pub(crate) queued_ms: i64,
    pub(crate) preview: String,
}

/// Something the inbox owner must report or measure.
#[derive(Clone, Debug)]
pub(crate) enum Note {
    /// A batch was written; when, for the wake latency.
    Sent { written: Option<Instant> },
    /// A batch was found in the session file this long after it was written.
    Confirmed { after_ms: i64 },
    /// A batch is in doubt; `notify` unless it was itself an in-doubt notice.
    InDoubt {
        delivery: String,
        rows: Vec<InboxRow>,
        reason: String,
        notify: bool,
    },
    Failed {
        delivery: String,
        rows: usize,
        reason: String,
    },
}

/// Caller request IDs: 1-128 characters, a letter or digit first, then letters, digits, `.`, `_`, `:`
/// or `-` (HOST req. 56).
pub(crate) fn valid_request_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && id.len() <= 128
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
}

/// SHA-256 of the delivery kind and text (HOST req. 57).
pub(crate) fn digest(kind: &str, text: &str) -> String {
    let mut bytes = Vec::with_capacity(kind.len() + 1 + text.len());
    bytes.extend_from_slice(kind.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(text.as_bytes());
    crate::config::sha256_hex(&bytes)
}

/// The marker that identifies a delivery in the session file.
pub(crate) fn marker(delivery_id: &str) -> String {
    format!("[cataly:{delivery_id}]")
}

fn clock(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map_or_else(|_| String::new(), |t| t.strftime("%H:%M UTC").to_string())
}

/// One message as its text; several as one prompt that says how many arrived while the agent was
/// busy, then each numbered with its sender, in arrival order (COORD req. 17).
pub(crate) fn batch_text(delivery_id: &str, rows: &[InboxRow]) -> String {
    let head = marker(delivery_id);
    match rows {
        [one] => format!("{head} from {}\n{}", one.sender, one.text),
        many => {
            let mut text = format!(
                "{head} {} messages arrived while you were busy:",
                many.len()
            );
            for (n, row) in many.iter().enumerate() {
                let _ = write!(
                    text,
                    "\n\n{}. from {} at {}:\n{}",
                    n + 1,
                    row.sender,
                    clock(row.created_ms),
                    row.text
                );
            }
            text
        }
    }
}

/// The first 60 characters, escaped, with an ellipsis when longer (COORD req. 21).
fn preview(text: &str) -> String {
    let escaped: String = text.chars().flat_map(char::escape_default).collect();
    let mut out: String = escaped.chars().take(60).collect();
    if escaped.chars().count() > 60 {
        out.push('…');
    }
    out
}

fn receipt_of(request_id: Option<String>, row: Option<&InboxRow>) -> Receipt {
    let state = match row.map(|r| r.state.as_str()) {
        Some("pending") => ReceiptState::Waiting,
        Some("sent") => ReceiptState::Sent,
        Some("in_doubt") => ReceiptState::InDoubt,
        Some("failed") => ReceiptState::Failed,
        // Delivered, or delivered and since removed by retention.
        _ => ReceiptState::Delivered,
    };
    Receipt {
        request_id,
        inbox_id: row.map(|r| r.id),
        state,
        delivery_id: row.and_then(|r| r.delivery_id.clone()),
        reason: None,
    }
}

/// What a check of a written batch found.
enum Check {
    Delivered(i64),
    /// Not yet: keep waiting, and send nothing else meanwhile.
    Wait,
    Resend,
    InDoubt(String),
    Failed(String),
}

/// One target's inbox over the store.
#[derive(Clone)]
pub(crate) struct Mailbox {
    pub(crate) target: String,
    pub(crate) store: Store,
}

impl Mailbox {
    /// Stores a message (COORD req. 15, 19; HOST req. 57-59). `Err` is a refusal; nothing was stored.
    pub(crate) async fn admit(&self, req: &Message) -> Result<(Receipt, Option<i64>), String> {
        if let Some(id) = &req.request_id
            && !valid_request_id(id)
        {
            return Err("request IDs are 1-128 letters, digits, '.', '_', ':' or '-', starting with a letter or digit".into());
        }
        if req.text.trim().is_empty() {
            return Err("the text is blank".into());
        }
        if req.text.len() > TEXT_MAX {
            return Err("the text is over 256 KiB".into());
        }
        let done = self
            .store
            .host(HostWrite::Admit(Admit {
                target: self.target.clone(),
                request_id: req.request_id.clone(),
                digest: digest("prompt", &req.text),
                sender: req.sender.clone(),
                text: req.text.clone(),
                urgent: req.urgent,
                owner: req.owner,
                identity: req.identity.clone(),
            }))
            .await
            .map_err(|e| e.to_string())?;
        let rid = req.request_id.clone();
        match done {
            HostDone::Admitted(Admitted::Stored(id)) => Ok((
                receipt_of(rid, None).with_state(ReceiptState::Waiting, id),
                Some(id),
            )),
            HostDone::Admitted(Admitted::Earlier(id)) => {
                let row = match id {
                    Some(id) => self.row(id).await.map_err(|e| e.to_string())?,
                    None => None,
                };
                Ok((receipt_of(rid, row.as_ref()), None))
            }
            HostDone::Admitted(Admitted::Mismatch) => {
                Err("this request ID was used before with different text".into())
            }
            HostDone::Admitted(Admitted::Full) => {
                Err("the receipt store of this target is full (512)".into())
            }
            HostDone::Admitted(Admitted::Seen) => Ok((
                Receipt {
                    request_id: rid,
                    inbox_id: None,
                    state: ReceiptState::Duplicate,
                    delivery_id: None,
                    reason: Some(
                        "an identical once-only message is pending or was delivered".into(),
                    ),
                },
                None,
            )),
            other => Err(format!("unexpected store answer {other:?}")),
        }
    }

    async fn row(&self, id: i64) -> Result<Option<InboxRow>, StoreError> {
        self.store.read(move |r| r.inbox_row(id)).await
    }

    /// The receipt of an inbox row now.
    pub(crate) async fn receipt(&self, request_id: Option<String>, id: i64) -> Receipt {
        let row = self.row(id).await.ok().flatten();
        receipt_of(request_id, row.as_ref())
    }

    /// Open entries with their 1-based index.
    pub(crate) async fn list(&self) -> Result<Vec<(Listed, InboxRow)>, StoreError> {
        let target = self.target.clone();
        let rows = self.store.read(move |r| r.inbox_open(&target)).await?;
        Ok(rows
            .into_iter()
            .enumerate()
            .map(|(n, row)| {
                (
                    Listed {
                        index: n + 1,
                        state: row.state.clone(),
                        sender: row.sender.clone(),
                        queued_ms: row.created_ms,
                        preview: preview(&row.text),
                    },
                    row,
                )
            })
            .collect())
    }

    /// Confirms (it arrived) or releases (deliver it again) an in-doubt entry (COORD req. 22).
    pub(crate) async fn settle(&self, index: usize, confirm: bool) -> Result<(), String> {
        let list = self.list().await.map_err(|e| e.to_string())?;
        let count = list.len();
        let Some((_, row)) = list.into_iter().nth(index.wrapping_sub(1)) else {
            return Err(format!("no entry {index}; the inbox has {count}"));
        };
        if row.state != "in_doubt" {
            return Err(format!(
                "entry {index} is {}, only in-doubt entries can be confirmed or released",
                row.state
            ));
        }
        self.store
            .host(HostWrite::Settle {
                target: self.target.clone(),
                inbox_id: row.id,
                confirm,
            })
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Whether a written batch still waits for its entry.
    pub(crate) async fn unconfirmed(&self) -> bool {
        let target = self.target.clone();
        self.store
            .read(move |r| r.sent_deliveries(&target))
            .await
            .is_ok_and(|d| !d.is_empty())
    }

    /// Resolves written batches, then sends what may go now (PATCH-01 rules 4, 8-11, 16).
    /// `quota_blocked` holds back everything but owner messages and quota notes.
    pub(crate) async fn flush<S: Control>(
        &self,
        session: Option<&S>,
        quota_blocked: bool,
    ) -> Result<Vec<Note>, StoreError> {
        let mut notes = Vec::new();
        let target = self.target.clone();
        let sent = self.store.read(move |r| r.sent_deliveries(&target)).await?;
        for delivery in sent {
            match self.check(&delivery, session).await {
                Check::Delivered(after_ms) => {
                    self.resolve(&delivery.id, "delivered", None).await?;
                    notes.push(Note::Confirmed { after_ms });
                }
                Check::Wait => return Ok(notes),
                Check::Resend => match self.resend(&delivery, session).await? {
                    Some(note) => {
                        notes.push(note);
                        return Ok(notes);
                    }
                    None => return Ok(notes),
                },
                Check::InDoubt(reason) => {
                    notes.push(self.in_doubt(&delivery.id, reason).await?);
                }
                Check::Failed(reason) => {
                    self.resolve(&delivery.id, "failed", Some(reason.clone()))
                        .await?;
                    let id = delivery.id.clone();
                    let rows = self
                        .store
                        .read(move |r| Ok(r.inbox_rows_of(&id)?.len()))
                        .await?;
                    notes.push(Note::Failed {
                        delivery: delivery.id.clone(),
                        rows,
                        reason,
                    });
                }
            }
        }
        if let Some(note) = self.send(session, quota_blocked).await? {
            notes.push(note);
        }
        Ok(notes)
    }

    async fn resolve(
        &self,
        id: &str,
        state: &str,
        detail: Option<String>,
    ) -> Result<(), StoreError> {
        self.store
            .host(HostWrite::Resolve {
                id: id.to_string(),
                state: state.to_string(),
                detail,
            })
            .await
            .map(drop)
    }

    async fn in_doubt(&self, id: &str, reason: String) -> Result<Note, StoreError> {
        self.resolve(id, "in_doubt", Some(reason.clone())).await?;
        let owned = id.to_string();
        let rows = self.store.read(move |r| r.inbox_rows_of(&owned)).await?;
        let notify = !rows.iter().all(|r| {
            r.identity
                .as_deref()
                .is_some_and(|i| i.starts_with(IN_DOUBT_PREFIX))
        });
        Ok(Note::InDoubt {
            delivery: id.to_string(),
            rows,
            reason,
            notify,
        })
    }

    async fn check<S: Control>(&self, d: &DeliveryRow, session: Option<&S>) -> Check {
        let age = now_ms() - d.sent_ms;
        let found = transcript::search(d.session_file.clone().into(), marker(&d.id)).await;
        if found == Ok(Found::Yes) {
            return Check::Delivered(age);
        }
        let snapshot = session.map(Control::snapshot);
        let same_process = snapshot
            .as_ref()
            .is_some_and(|s| s.handle == d.handle && s.stop.is_none());
        if same_process {
            return if age < CONFIRM_WITHIN_MS {
                Check::Wait
            } else {
                Check::InDoubt("no entry in the session file within 30 s".into())
            };
        }
        // The process that held the batch is gone, so its file is final.
        if d.resent {
            return Check::Failed("resent once and still not in the session file".into());
        }
        match snapshot {
            None => Check::Wait,
            Some(s) if s.stop.is_some() => Check::Wait,
            Some(s) if s.session_id.as_deref() == Some(d.session_id.as_str()) => Check::Resend,
            Some(_) => Check::InDoubt(
                "its session is gone and the new one is another session; not resent there".into(),
            ),
        }
    }

    /// Resends a batch once with the same ID into the same session (DISC-IN req. 50, DESIGN 9.3).
    async fn resend<S: Control>(
        &self,
        d: &DeliveryRow,
        session: Option<&S>,
    ) -> Result<Option<Note>, StoreError> {
        let Some(session) = session else {
            return Ok(None);
        };
        let outcome = session
            .deliver(Delivery {
                expected_session: d.session_id.clone(),
                text: d.text.clone(),
                timing: Timing::Any,
                steer: false,
            })
            .await;
        match outcome {
            DeliverOutcome::Delivered { written, .. } => {
                self.store
                    .host(HostWrite::Resolve {
                        id: d.id.clone(),
                        state: "sent".into(),
                        detail: Some("resent once".into()),
                    })
                    .await?;
                // The resend belongs to a new process: rebind the record to it.
                let id = d.id.clone();
                let handle = session.snapshot().handle.clone();
                self.store.host(HostWrite::Rebind { id, handle }).await?;
                Ok(Some(Note::Sent { written }))
            }
            DeliverOutcome::NotReady(_) | DeliverOutcome::Refused(_) => Ok(None),
            DeliverOutcome::InDoubt(reason) => self.in_doubt(&d.id, reason).await.map(Some),
        }
    }

    async fn send<S: Control>(
        &self,
        session: Option<&S>,
        quota_blocked: bool,
    ) -> Result<Option<Note>, StoreError> {
        let Some(session) = session.filter(|s| s.alive()) else {
            return Ok(None);
        };
        let snapshot = session.snapshot();
        let (Some(session_id), Some(file)) = (&snapshot.session_id, &snapshot.session_file) else {
            return Ok(None);
        };
        // The timing matches what the snapshot showed; a change in between makes it not ready, and
        // the batch waits for the next turn end.
        let (kind, timing) = match snapshot.status {
            Status::Idle => ("wake", Timing::Idle),
            Status::Working => ("follow_up", Timing::Working),
            _ => return Ok(None),
        };
        let target = self.target.clone();
        let rows: Vec<InboxRow> = self
            .store
            .read(move |r| r.inbox_open(&target))
            .await?
            .into_iter()
            .filter(|r| r.state == "pending")
            .filter(|r| {
                !quota_blocked
                    || r.owner
                    || r.identity
                        .as_deref()
                        .is_some_and(|i| i.starts_with(QUOTA_PREFIX))
            })
            .collect();
        // Quiet notes never wake a session (COORD-10).
        if !rows.iter().any(|r| r.urgent || r.owner) {
            return Ok(None);
        }
        let id = super::random_hex(8);
        let text = batch_text(&id, &rows);
        let now = now_ms();
        self.store
            .host(HostWrite::Send {
                delivery: DeliveryRow {
                    id: id.clone(),
                    target: self.target.clone(),
                    handle: snapshot.handle.clone(),
                    session_id: session_id.clone(),
                    session_file: file.clone(),
                    kind: kind.into(),
                    text: text.clone(),
                    state: "sent".into(),
                    resent: false,
                    created_ms: now,
                    sent_ms: now,
                    detail: None,
                },
                inbox: rows.iter().map(|r| r.id).collect(),
            })
            .await?;
        let outcome = session
            .deliver(Delivery {
                expected_session: session_id.clone(),
                text,
                timing,
                steer: false,
            })
            .await;
        match outcome {
            DeliverOutcome::Delivered { written, .. } => Ok(Some(Note::Sent { written })),
            DeliverOutcome::NotReady(status) => {
                self.resolve(&id, "superseded", Some(format!("not ready: {status:?}")))
                    .await?;
                Ok(None)
            }
            DeliverOutcome::Refused(reason) => {
                crate::log::warn(
                    "delivery refused by the session",
                    &[("target", &self.target), ("reason", &reason)],
                );
                self.resolve(&id, "superseded", Some(reason)).await?;
                Ok(None)
            }
            DeliverOutcome::InDoubt(reason) => self.in_doubt(&id, reason).await.map(Some),
        }
    }
}

impl Receipt {
    fn with_state(mut self, state: ReceiptState, id: i64) -> Self {
        self.state = state;
        self.inbox_id = Some(id);
        self
    }
}

/// The lead's one notice about an in-doubt batch (COORD req. 20).
pub(crate) fn in_doubt_notice(target: &str, rows: &[InboxRow], reason: &str) -> String {
    let mut text = format!(
        "[in-doubt delivery] {} message(s) for {target} may or may not have arrived ({reason}). \
         They are kept and will not be resent:",
        rows.len()
    );
    for row in rows {
        let _ = write!(
            text,
            "\n- from {} queued {}: {}",
            row.sender,
            clock(row.created_ms),
            preview(&row.text)
        );
    }
    text.push_str(
        "\nCheck the conversation, then `cataly lead inbox` and `cataly lead confirm <n>` if it \
         arrived or `cataly lead release <n>` to deliver it again.",
    );
    text
}

/// The identity of the notice about one delivery.
pub(crate) fn in_doubt_identity(delivery: &str) -> String {
    format!("{IN_DOUBT_PREFIX}{delivery}")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::host::headless::Session;
    use crate::host::testing::{Fake, spec};
    use crate::store::tests::open_temp;

    fn owner(text: &str, rid: Option<&str>) -> Message {
        Message {
            sender: "owner".into(),
            text: text.into(),
            urgent: true,
            owner: true,
            identity: None,
            request_id: rid.map(Into::into),
        }
    }

    fn row(sender: &str, text: &str, at: i64) -> InboxRow {
        InboxRow {
            id: 1,
            sender: sender.into(),
            text: text.into(),
            urgent: true,
            owner: false,
            identity: None,
            state: "pending".into(),
            delivery_id: None,
            created_ms: at,
        }
    }

    #[test]
    fn batches_number_their_messages() {
        assert_eq!(
            batch_text("ab", &[row("owner", "hi", 0)]),
            "[cataly:ab] from owner\nhi"
        );
        let two = batch_text(
            "ab",
            &[row("owner", "one", 0), row("lane x", "two", 60_000)],
        );
        assert!(two.starts_with("[cataly:ab] 2 messages arrived while you were busy:"));
        assert!(two.contains("\n\n1. from owner at 00:00 UTC:\none"));
        assert!(two.contains("\n\n2. from lane x at 00:01 UTC:\ntwo"));
        assert!(valid_request_id("discord:123.4_x-y") && !valid_request_id("-x"));
        assert_eq!(preview("a\nb"), "a\\nb");
    }

    async fn box_and_session(tag: &str) -> (Mailbox, Fake, Arc<Session>) {
        let (store, _writer, _dir) = open_temp(tag);
        let fake = Fake::start();
        let (session, _) = fake.bind(spec("h1", None)).await.unwrap();
        (
            Mailbox {
                target: LEAD.into(),
                store,
            },
            fake,
            session,
        )
    }

    #[tokio::test]
    async fn owner_message_is_delivered_and_confirmed_from_the_file() {
        let (mailbox, fake, session) = box_and_session("inbox-deliver").await;
        let (receipt, id) = mailbox.admit(&owner("hello", Some("r1"))).await.unwrap();
        assert_eq!(receipt.state, ReceiptState::Waiting);
        let notes = mailbox.flush(Some(&*session), false).await.unwrap();
        assert!(
            matches!(notes.as_slice(), [Note::Sent { written: Some(_) }]),
            "{notes:?}"
        );
        let prompt = fake.seen("prompt").pop().unwrap();
        assert!(
            prompt["message"]
                .as_str()
                .unwrap()
                .contains("from owner\nhello")
        );
        let notes = mailbox.flush(Some(&*session), false).await.unwrap();
        assert!(
            matches!(notes.as_slice(), [Note::Confirmed { .. }]),
            "{notes:?}"
        );
        let receipt = mailbox.receipt(Some("r1".into()), id.unwrap()).await;
        assert_eq!(receipt.state, ReceiptState::Delivered);
        // The same request ID returns the stored receipt and sends nothing.
        let (again, stored) = mailbox.admit(&owner("hello", Some("r1"))).await.unwrap();
        assert_eq!((again.state, stored), (ReceiptState::Delivered, None));
        assert!(mailbox.admit(&owner("other", Some("r1"))).await.is_err());
        assert_eq!(fake.seen("prompt").len(), 1);
    }

    #[tokio::test]
    async fn quiet_notes_wait_for_a_wake_and_go_in_one_batch() {
        let (mailbox, fake, session) = box_and_session("inbox-quiet").await;
        let note = Message {
            sender: HUB_SENDER.into(),
            text: "fyi".into(),
            urgent: false,
            owner: false,
            identity: Some("note:1".into()),
            request_id: None,
        };
        mailbox.admit(&note).await.unwrap();
        assert!(
            mailbox
                .flush(Some(&*session), false)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(fake.seen("prompt").len(), 0);
        let (dup, _) = mailbox.admit(&note).await.unwrap();
        assert_eq!(dup.state, ReceiptState::Duplicate);
        mailbox.admit(&owner("now", None)).await.unwrap();
        mailbox.flush(Some(&*session), false).await.unwrap();
        let text = fake.seen("prompt").pop().unwrap()["message"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(text.contains("2 messages arrived"), "{text}");
        // Quota-blocked: only owner messages go.
        mailbox.flush(Some(&*session), false).await.unwrap();
        mailbox
            .admit(
                &Message {
                    urgent: true,
                    ..note.clone()
                }
                .with_identity("note:2"),
            )
            .await
            .unwrap();
        assert!(
            mailbox
                .flush(Some(&*session), true)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// Acceptance check 4, session side: the hub dies after the prompt line is written and before the
    /// confirmation. The file search confirms it; with the process gone and no entry it is resent
    /// once; with the process still there it ends in doubt and is not resent.
    #[tokio::test]
    async fn unconfirmed_batches_are_found_resent_once_or_in_doubt() {
        // Found in the file after a restart: delivered, nothing resent.
        let (mailbox, fake, session) = box_and_session("inbox-found").await;
        mailbox.admit(&owner("a", None)).await.unwrap();
        mailbox.flush(Some(&*session), false).await.unwrap();
        let fake2 = Fake::start();
        fake2.set(|s| s.file = fake.file());
        let (after_restart, _) = fake2.bind(spec("h2", None)).await.unwrap();
        let notes = mailbox.flush(Some(&*after_restart), false).await.unwrap();
        assert!(
            matches!(notes.as_slice(), [Note::Confirmed { .. }]),
            "{notes:?}"
        );
        assert_eq!(fake2.seen("prompt").len(), 0);

        // Process gone, no entry: resent once with the same ID into the same session.
        let (mailbox, fake, session) = box_and_session("inbox-resend").await;
        fake.set(|s| s.write_entries = false);
        mailbox.admit(&owner("b", None)).await.unwrap();
        mailbox.flush(Some(&*session), false).await.unwrap();
        let first = fake.seen("prompt").pop().unwrap()["message"].clone();
        session.close(false).await.unwrap();
        let fake2 = Fake::start();
        fake2.set(|s| {
            s.file = fake.file();
            s.write_entries = false;
        });
        let (resumed, _) = fake2.bind(spec("h2", None)).await.unwrap();
        mailbox.flush(Some(&*resumed), false).await.unwrap();
        assert_eq!(fake2.seen("prompt").pop().unwrap()["message"], first);
        // Gone again without an entry: failed, never a third write.
        resumed.close(false).await.unwrap();
        let fake3 = Fake::start();
        fake3.set(|s| s.file = fake.file());
        let (third, _) = fake3.bind(spec("h3", None)).await.unwrap();
        let notes = mailbox.flush(Some(&*third), false).await.unwrap();
        assert!(
            notes.iter().any(|n| matches!(n, Note::Failed { .. })),
            "{notes:?}"
        );
        assert_eq!(fake3.seen("prompt").len(), 0);

        // Same process, no entry within 30 s: in doubt, the lead notified, nothing resent.
        let (mailbox, fake, session) = box_and_session("inbox-doubt").await;
        fake.set(|s| s.write_entries = false);
        // The transcript exists but never gets the entry (a missing file would fault the session).
        std::fs::write(fake.file(), "{\"type\":\"session\"}\n").unwrap();
        mailbox.admit(&owner("c", None)).await.unwrap();
        mailbox.flush(Some(&*session), false).await.unwrap();
        assert!(
            mailbox
                .flush(Some(&*session), false)
                .await
                .unwrap()
                .is_empty()
        );
        mailbox
            .store
            .host(HostWrite::Age {
                target: LEAD.into(),
                by_ms: CONFIRM_WITHIN_MS + 1,
            })
            .await
            .unwrap();
        let notes = mailbox.flush(Some(&*session), false).await.unwrap();
        assert!(
            matches!(notes.as_slice(), [Note::InDoubt { notify: true, .. }]),
            "{notes:?}"
        );
        assert_eq!(fake.seen("prompt").len(), 1);
        let list = mailbox.list().await.unwrap();
        assert_eq!(list[0].0.state, "in_doubt");
        assert!(mailbox.settle(2, true).await.is_err());
        mailbox.settle(1, false).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1)).await;
        mailbox.flush(Some(&*session), false).await.unwrap();
        assert_eq!(
            fake.seen("prompt").len(),
            2,
            "released entries go out again"
        );
    }

    impl Message {
        fn with_identity(mut self, identity: &str) -> Self {
            self.identity = Some(identity.into());
            self
        }
    }
}
