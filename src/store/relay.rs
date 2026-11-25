//! Store rows of the relays (DESIGN 5.1, 7.3 "Relays" and "Owner"): the account binding, read
//! positions, the arrival queue, hand-offs, read marks, owed replies, the reply outbox and open
//! questions. Each write is one savepoint in the writer's transaction, so a hand-off's queue removal,
//! position move and receipt commit together (DISC-IN req. 49, MSG-03).

use rusqlite::{OptionalExtension, Row, Transaction, params};

use super::Reads;

/// The whole arrival queue of one relay is capped at 256 MiB (DISC-IN req. 24).
pub(crate) const QUEUE_CAP_BYTES: i64 = 256 * 1024 * 1024;
/// Hand-offs, read marks and reply receipts are kept at least this long (DISC-IN req. 52,
/// DISC-OUT req. 29).
const KEEP_MS: i64 = 7 * 24 * 3600 * 1000;
/// Owed replies kept (DISC-OUT req. 34).
const OWED_KEEP: i64 = 200;
/// Owed replies older than this are no longer reported (DISC-OUT req. 34).
pub(crate) const OWED_WINDOW_MS: i64 = 30 * 60 * 1000;

/// A room's read position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PositionRow {
    pub(crate) room: String,
    pub(crate) position: i64,
    pub(crate) parent: Option<String>,
    pub(crate) held: Option<String>,
}

/// One queued owner message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueueRow {
    pub(crate) room: String,
    pub(crate) message: String,
    pub(crate) seq: i64,
    pub(crate) parent: Option<String>,
    pub(crate) position_at: i64,
    pub(crate) raw: String,
    pub(crate) created_ms: i64,
}

/// One hand-off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HandoffRow {
    pub(crate) room: String,
    pub(crate) message: String,
    pub(crate) seq: i64,
    pub(crate) target: String,
    pub(crate) request_id: String,
    pub(crate) digest: String,
    pub(crate) state: String,
    pub(crate) answer: String,
    pub(crate) created_ms: i64,
}

/// One outbox part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PartRow {
    pub(crate) idx: i64,
    pub(crate) text: String,
    pub(crate) nonce: String,
    pub(crate) remote: Option<String>,
    pub(crate) state: String,
}

/// One owed reply.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct OwedRow {
    pub(crate) room: String,
    pub(crate) message: String,
    pub(crate) admitted_ms: i64,
    pub(crate) text: String,
    pub(crate) settled_ms: Option<i64>,
}

/// Counts for `cataly discord status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Counts {
    pub(crate) rooms: i64,
    pub(crate) held: i64,
    pub(crate) queued: i64,
    pub(crate) answers_waiting: i64,
    pub(crate) owed_open: i64,
    pub(crate) owed_settled: i64,
}

/// What a new arrival became.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arrived {
    Queued,
    /// The same key is already queued; the first copy wins (DISC-IN req. 25).
    Duplicate,
    /// Already handed off, or not newer than the room's position.
    Handled,
    /// The queue is full; a catch-up read is owed instead (DISC-IN req. 24).
    Full,
}

/// Every relay mutation.
#[derive(Debug, Clone)]
pub(crate) enum RelayWrite {
    /// Records the (account, owner) pair, or reports the one already recorded.
    Bind {
        relay: String,
        account: String,
        owner: String,
    },
    /// Forgets all state of a relay (the owner's explicit reset).
    Reset { relay: String },
    /// A room's first-run position; kept when one exists. Queued messages pull it below themselves.
    Baseline {
        relay: String,
        room: String,
        position: i64,
        parent: Option<String>,
    },
    /// A message that needs no hand-off (not the owner's, an edit): the position moves past it
    /// unless an older owner message of the room is still queued.
    Pass {
        relay: String,
        room: String,
        seq: i64,
    },
    Arrive {
        relay: String,
        room: String,
        message: String,
        seq: i64,
        parent: Option<String>,
        raw: String,
    },
    /// The hand-off is recorded before the target's inbox gets it (DISC-IN req. 47), with the owed
    /// reply when it goes to the lead.
    Handed {
        relay: String,
        room: String,
        message: String,
        seq: i64,
        target: String,
        request_id: String,
        digest: String,
        owed: Option<String>,
    },
    /// The queue item is finished: removed, the position moved, and the hand-off (if any) set to
    /// `state` (DISC-IN req. 49).
    Finish {
        relay: String,
        room: String,
        message: String,
        state: Option<String>,
    },
    /// A hand-off ended in doubt or failed; the queue item stays and the room is held.
    Doubt {
        relay: String,
        room: String,
        message: String,
        state: String,
        reason: String,
    },
    /// Holds a room (fence) or releases it (`None`).
    Hold {
        relay: String,
        room: String,
        reason: Option<String>,
    },
    Flag {
        relay: String,
        flag: String,
        on: bool,
    },
    ReadMark {
        relay: String,
        room: String,
        message: String,
    },
    /// Stores the parts of an answer before the first post; nothing when parts already exist.
    Outbox {
        relay: String,
        room: String,
        source: String,
        parts: Vec<(String, String)>,
    },
    /// One part confirmed with its remote ID; `linked` for a confirmed linked first part.
    PartDone {
        relay: String,
        room: String,
        source: String,
        idx: i64,
        remote: String,
        linked: bool,
    },
    /// The answer of a hand-off: `abandoned`, or `dropped` with its pending parts discarded.
    Answer {
        relay: String,
        room: String,
        message: String,
        answer: String,
    },
    Question {
        relay: String,
        room: String,
        message: String,
        open: bool,
    },
}

/// What a relay write returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RelayDone {
    Done,
    Arrived(Arrived),
    /// The binding already recorded, when it differs from the one asked for.
    Bound(Option<(String, String)>),
}

const POSITION_COLUMNS: &str = "room, position, parent, held";
const QUEUE_COLUMNS: &str = "room, message, seq, parent, position_at, raw, created_ms";
const HANDOFF_COLUMNS: &str =
    "room, message, seq, target, request_id, digest, state, answer, created_ms";

fn position_from_row(r: &Row<'_>) -> rusqlite::Result<PositionRow> {
    Ok(PositionRow {
        room: r.get(0)?,
        position: r.get(1)?,
        parent: r.get(2)?,
        held: r.get(3)?,
    })
}

fn queue_from_row(r: &Row<'_>) -> rusqlite::Result<QueueRow> {
    Ok(QueueRow {
        room: r.get(0)?,
        message: r.get(1)?,
        seq: r.get(2)?,
        parent: r.get(3)?,
        position_at: r.get(4)?,
        raw: r.get(5)?,
        created_ms: r.get(6)?,
    })
}

fn handoff_from_row(r: &Row<'_>) -> rusqlite::Result<HandoffRow> {
    Ok(HandoffRow {
        room: r.get(0)?,
        message: r.get(1)?,
        seq: r.get(2)?,
        target: r.get(3)?,
        request_id: r.get(4)?,
        digest: r.get(5)?,
        state: r.get(6)?,
        answer: r.get(7)?,
        created_ms: r.get(8)?,
    })
}

/// Moves a room's position to `seq` unless an older owner message of the room is still queued.
fn advance(
    tx: &Transaction<'_>,
    relay: &str,
    room: &str,
    seq: i64,
    now: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE positions SET position = ?3, updated_ms = ?4 WHERE relay = ?1 AND room = ?2 \
         AND position < ?3 AND NOT EXISTS (SELECT 1 FROM arrival_queue \
         WHERE relay = ?1 AND room = ?2 AND seq < ?3)",
        params![relay, room, seq, now],
    )
    .map(drop)
}

/// Settles an owed reply once both its read mark and a confirmed linked reply exist (DISC-OUT req. 33).
fn settle(
    tx: &Transaction<'_>,
    relay: &str,
    room: &str,
    message: &str,
    now: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE owed_replies SET settled_ms = ?4 WHERE relay = ?1 AND room = ?2 AND message = ?3 \
         AND settled_ms IS NULL \
         AND EXISTS (SELECT 1 FROM read_marks m WHERE m.relay = ?1 AND m.room = ?2 AND m.message = ?3 \
                     AND m.at_ms >= owed_replies.admitted_ms) \
         AND EXISTS (SELECT 1 FROM outbox_parts p WHERE p.relay = ?1 AND p.room = ?2 AND p.source = ?3 \
                     AND p.idx = 0 AND p.state = 'confirmed' AND p.linked = 1 \
                     AND p.updated_ms >= owed_replies.admitted_ms)",
        params![relay, room, message, now],
    )
    .map(drop)
}

fn bind(
    tx: &Transaction<'_>,
    relay: &str,
    account: &str,
    owner: &str,
    now: i64,
) -> rusqlite::Result<RelayDone> {
    let found: Option<(String, String)> = tx
        .query_row(
            "SELECT account, owner FROM bindings WHERE relay = ?1",
            [relay],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match found {
        Some((a, o)) if a == account && o == owner => Ok(RelayDone::Bound(None)),
        Some(other) => Ok(RelayDone::Bound(Some(other))),
        None => {
            tx.execute(
                "INSERT INTO bindings (relay, account, owner, created_ms) VALUES (?1, ?2, ?3, ?4)",
                params![relay, account, owner, now],
            )?;
            Ok(RelayDone::Bound(None))
        }
    }
}

fn reset(tx: &Transaction<'_>, relay: &str) -> rusqlite::Result<()> {
    for table in [
        "bindings",
        "positions",
        "relay_flags",
        "arrival_queue",
        "handoffs",
        "read_marks",
        "owed_replies",
        "outbox_parts",
        "open_questions",
    ] {
        tx.execute(&format!("DELETE FROM {table} WHERE relay = ?1"), [relay])?;
    }
    Ok(())
}

fn arrive(
    tx: &Transaction<'_>,
    w: (&str, &str, &str, i64, Option<&str>, &str),
    now: i64,
) -> rusqlite::Result<Arrived> {
    let (relay, room, message, seq, parent, raw) = w;
    let handled: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM handoffs WHERE relay = ?1 AND room = ?2 AND message = ?3 \
             AND state = 'delivered')",
        params![relay, room, message],
        |r| r.get(0),
    )?;
    let position: Option<i64> = tx
        .query_row(
            "SELECT position FROM positions WHERE relay = ?1 AND room = ?2",
            params![relay, room],
            |r| r.get(0),
        )
        .optional()?;
    if handled || position.is_some_and(|p| p >= seq) {
        return Ok(Arrived::Handled);
    }
    let queued: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM arrival_queue WHERE relay = ?1 AND room = ?2 AND message = ?3)",
        params![relay, room, message],
        |r| r.get(0),
    )?;
    if queued {
        return Ok(Arrived::Duplicate);
    }
    let bytes = i64::try_from(raw.len()).unwrap_or(i64::MAX);
    let used: i64 = tx.query_row(
        "SELECT COALESCE(SUM(bytes), 0) FROM arrival_queue WHERE relay = ?1",
        [relay],
        |r| r.get(0),
    )?;
    if used.saturating_add(bytes) > QUEUE_CAP_BYTES {
        tx.execute(
            "INSERT OR IGNORE INTO relay_flags (relay, flag) VALUES (?1, 'catch_up')",
            [relay],
        )?;
        return Ok(Arrived::Full);
    }
    // A room without a position starts just below its oldest queued message (DISC-IN req. 30).
    if position.is_none() {
        tx.execute(
            "INSERT INTO positions (relay, room, position, parent, held, updated_ms) \
             VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
            params![relay, room, seq - 1, parent, now],
        )?;
    }
    tx.execute(
        "INSERT INTO arrival_queue (relay, room, message, seq, parent, position_at, raw, bytes, created_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![relay, room, message, seq, parent, position.unwrap_or(seq - 1), raw, bytes, now],
    )?;
    Ok(Arrived::Queued)
}

pub(super) fn write(
    tx: &Transaction<'_>,
    change: &RelayWrite,
    now: i64,
) -> rusqlite::Result<RelayDone> {
    match change {
        RelayWrite::Bind { .. }
        | RelayWrite::Reset { .. }
        | RelayWrite::Baseline { .. }
        | RelayWrite::Pass { .. }
        | RelayWrite::Arrive { .. }
        | RelayWrite::Handed { .. } => write_rows(tx, change, now),
        RelayWrite::Finish { .. }
        | RelayWrite::Doubt { .. }
        | RelayWrite::Hold { .. }
        | RelayWrite::Flag { .. } => write_queue(tx, change, now),
        RelayWrite::ReadMark { .. } | RelayWrite::Outbox { .. } | RelayWrite::PartDone { .. } => {
            write_replies(tx, change, now)
        }
        RelayWrite::Answer { .. } | RelayWrite::Question { .. } => write_marks(tx, change, now),
    }
}

fn write_rows(tx: &Transaction<'_>, change: &RelayWrite, now: i64) -> rusqlite::Result<RelayDone> {
    match change {
        RelayWrite::Bind {
            relay,
            account,
            owner,
        } => bind(tx, relay, account, owner, now),
        RelayWrite::Reset { relay } => reset(tx, relay).map(|()| RelayDone::Done),
        RelayWrite::Baseline {
            relay,
            room,
            position,
            parent,
        } => {
            let below: Option<i64> = tx.query_row(
                "SELECT MIN(seq) - 1 FROM arrival_queue WHERE relay = ?1 AND room = ?2",
                params![relay, room],
                |r| r.get(0),
            )?;
            let position = below.map_or(*position, |b| b.min(*position));
            tx.execute(
                "INSERT OR IGNORE INTO positions (relay, room, position, parent, held, updated_ms) \
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
                params![relay, room, position, parent, now],
            )?;
            Ok(RelayDone::Done)
        }
        RelayWrite::Pass { relay, room, seq } => {
            advance(tx, relay, room, *seq, now).map(|()| RelayDone::Done)
        }
        RelayWrite::Arrive {
            relay,
            room,
            message,
            seq,
            parent,
            raw,
        } => arrive(
            tx,
            (relay, room, message, *seq, parent.as_deref(), raw),
            now,
        )
        .map(RelayDone::Arrived),
        RelayWrite::Handed {
            relay,
            room,
            message,
            seq,
            target,
            request_id,
            digest,
            owed,
        } => {
            tx.execute(
                "INSERT OR IGNORE INTO handoffs (relay, room, message, seq, target, request_id, digest, \
                 state, answer, created_ms, updated_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'handed', 'waiting', ?8, ?8)",
                params![relay, room, message, seq, target, request_id, digest, now],
            )?;
            if let Some(text) = owed {
                tx.execute(
                    "INSERT OR IGNORE INTO owed_replies (relay, room, message, admitted_ms, text) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![relay, room, message, now, text],
                )?;
                // The request log entry opens on admission (DESIGN 12.2 C16, OPC req. 56).
                super::lead::open_request(tx, text, request_id, now)?;
                tx.execute(
                    "INSERT INTO meta (key, value) VALUES ('owner_last_ms', ?1) \
                     ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                    [now.to_string()],
                )?;
            }
            Ok(RelayDone::Done)
        }
        _ => Ok(RelayDone::Done),
    }
}

fn write_queue(tx: &Transaction<'_>, change: &RelayWrite, now: i64) -> rusqlite::Result<RelayDone> {
    match change {
        RelayWrite::Finish {
            relay,
            room,
            message,
            state,
        } => {
            let seq: Option<i64> = tx
                .query_row(
                    "DELETE FROM arrival_queue WHERE relay = ?1 AND room = ?2 AND message = ?3 RETURNING seq",
                    params![relay, room, message],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(state) = state {
                tx.execute(
                    "UPDATE handoffs SET state = ?4, updated_ms = ?5 WHERE relay = ?1 AND room = ?2 \
                     AND message = ?3",
                    params![relay, room, message, state, now],
                )?;
            }
            if let Some(seq) = seq {
                advance(tx, relay, room, seq, now)?;
            }
            Ok(RelayDone::Done)
        }
        RelayWrite::Doubt {
            relay,
            room,
            message,
            state,
            reason,
        } => {
            tx.execute(
                "UPDATE handoffs SET state = ?4, updated_ms = ?5 WHERE relay = ?1 AND room = ?2 AND message = ?3",
                params![relay, room, message, state, now],
            )?;
            tx.execute(
                "UPDATE positions SET held = ?3, updated_ms = ?4 WHERE relay = ?1 AND room = ?2",
                params![relay, room, reason, now],
            )?;
            Ok(RelayDone::Done)
        }
        RelayWrite::Hold {
            relay,
            room,
            reason,
        } => {
            tx.execute(
                "UPDATE positions SET held = ?3, updated_ms = ?4 WHERE relay = ?1 AND room = ?2",
                params![relay, room, reason, now],
            )?;
            if reason.is_none() {
                // A released room's doubtful hand-offs are given up: their messages count as handled.
                let doubtful: Vec<String> = {
                    let mut stmt = tx.prepare(
                        "SELECT message FROM handoffs WHERE relay = ?1 AND room = ?2 \
                         AND state IN ('in_doubt', 'failed')",
                    )?;
                    stmt.query_map(params![relay, room], |r| r.get(0))?
                        .collect::<rusqlite::Result<_>>()?
                };
                for message in doubtful {
                    write(
                        tx,
                        &RelayWrite::Finish {
                            relay: relay.clone(),
                            room: room.clone(),
                            message,
                            state: None,
                        },
                        now,
                    )?;
                }
            }
            Ok(RelayDone::Done)
        }
        RelayWrite::Flag { relay, flag, on } => {
            if *on {
                tx.execute(
                    "INSERT OR IGNORE INTO relay_flags (relay, flag) VALUES (?1, ?2)",
                    params![relay, flag],
                )?;
            } else {
                tx.execute(
                    "DELETE FROM relay_flags WHERE relay = ?1 AND flag = ?2",
                    params![relay, flag],
                )?;
            }
            Ok(RelayDone::Done)
        }
        _ => Ok(RelayDone::Done),
    }
}

fn write_replies(
    tx: &Transaction<'_>,
    change: &RelayWrite,
    now: i64,
) -> rusqlite::Result<RelayDone> {
    match change {
        RelayWrite::ReadMark {
            relay,
            room,
            message,
        } => {
            tx.execute(
                "INSERT OR IGNORE INTO read_marks (relay, room, message, at_ms) VALUES (?1, ?2, ?3, ?4)",
                params![relay, room, message, now],
            )?;
            settle(tx, relay, room, message, now).map(|()| RelayDone::Done)
        }
        RelayWrite::Outbox {
            relay,
            room,
            source,
            parts,
        } => {
            let existing: i64 = tx.query_row(
                "SELECT COUNT(*) FROM outbox_parts WHERE relay = ?1 AND room = ?2 AND source = ?3",
                params![relay, room, source],
                |r| r.get(0),
            )?;
            if existing == 0 {
                for (idx, (nonce, text)) in parts.iter().enumerate() {
                    tx.execute(
                        "INSERT INTO outbox_parts (relay, room, source, idx, text, nonce, state, created_ms, \
                         updated_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?7)",
                        params![
                            relay,
                            room,
                            source,
                            i64::try_from(idx).unwrap_or(i64::MAX),
                            text,
                            nonce,
                            now
                        ],
                    )?;
                }
            }
            tx.execute(
                "UPDATE handoffs SET answer = 'posting', updated_ms = ?4 WHERE relay = ?1 AND room = ?2 \
                 AND message = ?3 AND answer = 'waiting'",
                params![relay, room, source, now],
            )?;
            Ok(RelayDone::Done)
        }
        RelayWrite::PartDone {
            relay,
            room,
            source,
            idx,
            remote,
            linked,
        } => {
            tx.execute(
                "UPDATE outbox_parts SET state = 'confirmed', remote = ?5, linked = ?6, updated_ms = ?7 \
                 WHERE relay = ?1 AND room = ?2 AND source = ?3 AND idx = ?4",
                params![relay, room, source, idx, remote, linked, now],
            )?;
            tx.execute(
                "UPDATE handoffs SET answer = 'posted', updated_ms = ?4 WHERE relay = ?1 AND room = ?2 \
                 AND message = ?3 AND NOT EXISTS (SELECT 1 FROM outbox_parts WHERE relay = ?1 \
                 AND room = ?2 AND source = ?3 AND state = 'pending')",
                params![relay, room, source, now],
            )?;
            settle(tx, relay, room, source, now).map(|()| RelayDone::Done)
        }
        _ => Ok(RelayDone::Done),
    }
}

fn write_marks(tx: &Transaction<'_>, change: &RelayWrite, now: i64) -> rusqlite::Result<RelayDone> {
    match change {
        RelayWrite::Answer {
            relay,
            room,
            message,
            answer,
        } => {
            tx.execute(
                "UPDATE handoffs SET answer = ?4, updated_ms = ?5 WHERE relay = ?1 AND room = ?2 AND message = ?3",
                params![relay, room, message, answer, now],
            )?;
            if answer == "dropped" {
                tx.execute(
                    "UPDATE outbox_parts SET state = 'dropped', updated_ms = ?4 WHERE relay = ?1 \
                     AND room = ?2 AND source = ?3 AND state = 'pending'",
                    params![relay, room, message, now],
                )?;
            }
            Ok(RelayDone::Done)
        }
        RelayWrite::Question {
            relay,
            room,
            message,
            open,
        } => {
            if *open {
                tx.execute(
                    "INSERT OR IGNORE INTO open_questions (relay, room, message, created_ms) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![relay, room, message, now],
                )?;
            } else {
                tx.execute(
                    "DELETE FROM open_questions WHERE relay = ?1 AND room = ?2 AND message = ?3",
                    params![relay, room, message],
                )?;
            }
            Ok(RelayDone::Done)
        }
        _ => Ok(RelayDone::Done),
    }
}

/// Retention (DISC-IN req. 52, DISC-OUT req. 29, 34).
pub(super) fn maintain(tx: &Transaction<'_>, now: i64) -> rusqlite::Result<()> {
    let old = now - KEEP_MS;
    tx.execute(
        "DELETE FROM handoffs AS h WHERE h.updated_ms < ?1 AND h.state = 'delivered' \
         AND h.answer NOT IN ('waiting', 'posting') AND NOT EXISTS (SELECT 1 FROM arrival_queue q \
         WHERE q.relay = h.relay AND q.room = h.room AND q.message = h.message) \
         AND EXISTS (SELECT 1 FROM positions p WHERE p.relay = h.relay AND p.room = h.room \
         AND p.position >= h.seq)",
        [old],
    )?;
    tx.execute(
        "DELETE FROM outbox_parts AS p WHERE p.updated_ms < ?1 AND NOT EXISTS (SELECT 1 FROM \
         outbox_parts o WHERE o.relay = p.relay AND o.room = p.room AND o.source = p.source \
         AND o.state = 'pending')",
        [old],
    )?;
    tx.execute("DELETE FROM read_marks WHERE at_ms < ?1", [old])?;
    tx.execute(
        "DELETE FROM owed_replies WHERE rowid NOT IN (SELECT rowid FROM owed_replies \
         ORDER BY admitted_ms DESC LIMIT ?1)",
        [OWED_KEEP],
    )
    .map(drop)
}

impl Reads {
    pub(crate) fn positions(&self, relay: &str) -> rusqlite::Result<Vec<PositionRow>> {
        let mut stmt = self.0.prepare(&format!(
            "SELECT {POSITION_COLUMNS} FROM positions WHERE relay = ?1 ORDER BY room"
        ))?;
        stmt.query_map([relay], position_from_row)?.collect()
    }

    /// The queue, lowest message first (DISC-IN req. 26).
    pub(crate) fn queue(&self, relay: &str) -> rusqlite::Result<Vec<QueueRow>> {
        let mut stmt = self.0.prepare(&format!(
            "SELECT {QUEUE_COLUMNS} FROM arrival_queue WHERE relay = ?1 ORDER BY seq, id"
        ))?;
        stmt.query_map([relay], queue_from_row)?.collect()
    }

    pub(crate) fn flag(&self, relay: &str, flag: &str) -> rusqlite::Result<bool> {
        self.0.query_row(
            "SELECT EXISTS (SELECT 1 FROM relay_flags WHERE relay = ?1 AND flag = ?2)",
            params![relay, flag],
            |r| r.get(0),
        )
    }

    pub(crate) fn handoff(
        &self,
        relay: &str,
        room: &str,
        message: &str,
    ) -> rusqlite::Result<Option<HandoffRow>> {
        self.0
            .query_row(
                &format!(
                    "SELECT {HANDOFF_COLUMNS} FROM handoffs WHERE relay = ?1 AND room = ?2 AND message = ?3"
                ),
                params![relay, room, message],
                handoff_from_row,
            )
            .optional()
    }

    /// Hand-offs whose answer is still to be read or posted, oldest first.
    pub(crate) fn answers_open(&self, relay: &str) -> rusqlite::Result<Vec<HandoffRow>> {
        let mut stmt = self.0.prepare(&format!(
            "SELECT {HANDOFF_COLUMNS} FROM handoffs WHERE relay = ?1 AND answer IN ('waiting', 'posting') \
             AND state IN ('handed', 'delivered') ORDER BY created_ms, seq"
        ))?;
        stmt.query_map([relay], handoff_from_row)?.collect()
    }

    pub(crate) fn parts(
        &self,
        relay: &str,
        room: &str,
        source: &str,
    ) -> rusqlite::Result<Vec<PartRow>> {
        let mut stmt = self.0.prepare(
            "SELECT idx, text, nonce, remote, state FROM outbox_parts WHERE relay = ?1 AND room = ?2 \
             AND source = ?3 ORDER BY idx",
        )?;
        stmt.query_map(params![relay, room, source], |r| {
            Ok(PartRow {
                idx: r.get(0)?,
                text: r.get(1)?,
                nonce: r.get(2)?,
                remote: r.get(3)?,
                state: r.get(4)?,
            })
        })?
        .collect()
    }

    pub(crate) fn read_marked(
        &self,
        relay: &str,
        room: &str,
        message: &str,
    ) -> rusqlite::Result<bool> {
        self.0.query_row(
            "SELECT EXISTS (SELECT 1 FROM read_marks WHERE relay = ?1 AND room = ?2 AND message = ?3)",
            params![relay, room, message],
            |r| r.get(0),
        )
    }

    /// Owed replies admitted since `since_ms`, newest first.
    pub(crate) fn owed(&self, relay: &str, since_ms: i64) -> rusqlite::Result<Vec<OwedRow>> {
        let mut stmt = self.0.prepare(
            "SELECT room, message, admitted_ms, text, settled_ms FROM owed_replies WHERE relay = ?1 \
             AND admitted_ms >= ?2 ORDER BY admitted_ms DESC LIMIT 200",
        )?;
        stmt.query_map(params![relay, since_ms], |r| {
            Ok(OwedRow {
                room: r.get(0)?,
                message: r.get(1)?,
                admitted_ms: r.get(2)?,
                text: r.get(3)?,
                settled_ms: r.get(4)?,
            })
        })?
        .collect()
    }

    pub(crate) fn open_questions(&self, relay: &str) -> rusqlite::Result<Vec<(String, String)>> {
        let mut stmt = self
            .0
            .prepare("SELECT room, message FROM open_questions WHERE relay = ?1")?;
        stmt.query_map([relay], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect()
    }

    pub(crate) fn relay_counts(&self, relay: &str, since_ms: i64) -> rusqlite::Result<Counts> {
        let count = |sql: &str, extra: Option<i64>| -> rusqlite::Result<i64> {
            match extra {
                Some(v) => self.0.query_row(sql, params![relay, v], |r| r.get(0)),
                None => self.0.query_row(sql, [relay], |r| r.get(0)),
            }
        };
        Ok(Counts {
            rooms: count("SELECT COUNT(*) FROM positions WHERE relay = ?1", None)?,
            held: count(
                "SELECT COUNT(*) FROM positions WHERE relay = ?1 AND held IS NOT NULL",
                None,
            )?,
            queued: count("SELECT COUNT(*) FROM arrival_queue WHERE relay = ?1", None)?,
            answers_waiting: count(
                "SELECT COUNT(*) FROM handoffs WHERE relay = ?1 AND answer IN ('waiting', 'posting')",
                None,
            )?,
            owed_open: count(
                "SELECT COUNT(*) FROM owed_replies WHERE relay = ?1 AND settled_ms IS NULL \
                 AND admitted_ms >= ?2",
                Some(since_ms),
            )?,
            owed_settled: count(
                "SELECT COUNT(*) FROM owed_replies WHERE relay = ?1 AND settled_ms IS NOT NULL",
                None,
            )?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::open_temp;

    fn arrive(room: &str, seq: i64) -> RelayWrite {
        RelayWrite::Arrive {
            relay: "discord".into(),
            room: room.into(),
            message: seq.to_string(),
            seq,
            parent: None,
            raw: "{}".into(),
        }
    }

    #[tokio::test]
    async fn queue_positions_and_settling() {
        let (store, _w, _dir) = open_temp("relay-queue");
        let r = |w| store.relay(w);
        let base = RelayWrite::Baseline {
            relay: "discord".into(),
            room: "c".into(),
            position: 10,
            parent: None,
        };
        r(base).await.unwrap();
        // Not newer than the position: handled; first copy wins; duplicate ignored.
        assert_eq!(
            r(arrive("c", 9)).await.unwrap(),
            RelayDone::Arrived(Arrived::Handled)
        );
        assert_eq!(
            r(arrive("c", 12)).await.unwrap(),
            RelayDone::Arrived(Arrived::Queued)
        );
        assert_eq!(
            r(arrive("c", 12)).await.unwrap(),
            RelayDone::Arrived(Arrived::Duplicate)
        );
        // A later non-owner message cannot move the position past the queued owner message.
        r(RelayWrite::Pass {
            relay: "discord".into(),
            room: "c".into(),
            seq: 15,
        })
        .await
        .unwrap();
        let pos = store.read(|q| q.positions("discord")).await.unwrap();
        assert_eq!(pos[0].position, 10);
        r(RelayWrite::Handed {
            relay: "discord".into(),
            room: "c".into(),
            message: "12".into(),
            seq: 12,
            target: "lead".into(),
            request_id: "discord:c:12".into(),
            digest: "d".into(),
            owed: Some("hi".into()),
        })
        .await
        .unwrap();
        r(RelayWrite::Finish {
            relay: "discord".into(),
            room: "c".into(),
            message: "12".into(),
            state: Some("delivered".into()),
        })
        .await
        .unwrap();
        let pos = store.read(|q| q.positions("discord")).await.unwrap();
        assert_eq!(pos[0].position, 12);
        assert_eq!(store.read(|q| q.queue("discord")).await.unwrap().len(), 0);
        // Delivered once: a later copy is handled.
        assert_eq!(
            r(arrive("c", 12)).await.unwrap(),
            RelayDone::Arrived(Arrived::Handled)
        );
    }

    /// Settling needs the read mark and a confirmed linked first part (DISC-OUT req. 33).
    #[tokio::test]
    async fn owed_replies_settle() {
        let (store, _w, _dir) = open_temp("relay-settle");
        let r = |w| store.relay(w);
        r(arrive("c", 12)).await.unwrap();
        r(RelayWrite::Handed {
            relay: "discord".into(),
            room: "c".into(),
            message: "12".into(),
            seq: 12,
            target: "lead".into(),
            request_id: "discord:c:12".into(),
            digest: "d".into(),
            owed: Some("hi".into()),
        })
        .await
        .unwrap();
        r(RelayWrite::Outbox {
            relay: "discord".into(),
            room: "c".into(),
            source: "12".into(),
            parts: vec![("n0".into(), "a".into()), ("n1".into(), "b".into())],
        })
        .await
        .unwrap();
        r(RelayWrite::PartDone {
            relay: "discord".into(),
            room: "c".into(),
            source: "12".into(),
            idx: 0,
            remote: "100".into(),
            linked: true,
        })
        .await
        .unwrap();
        let owed = store.read(|q| q.owed("discord", 0)).await.unwrap();
        assert_eq!(owed[0].settled_ms, None);
        r(RelayWrite::ReadMark {
            relay: "discord".into(),
            room: "c".into(),
            message: "12".into(),
        })
        .await
        .unwrap();
        let owed = store.read(|q| q.owed("discord", 0)).await.unwrap();
        assert!(owed[0].settled_ms.is_some());
        let h = store
            .read(|q| q.handoff("discord", "c", "12"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(h.answer, "posting");
        r(RelayWrite::PartDone {
            relay: "discord".into(),
            room: "c".into(),
            source: "12".into(),
            idx: 1,
            remote: "101".into(),
            linked: false,
        })
        .await
        .unwrap();
        let h = store
            .read(|q| q.handoff("discord", "c", "12"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(h.answer, "posted");
    }

    #[tokio::test]
    async fn binding_and_first_run() {
        let (store, _w, _dir) = open_temp("relay-bind");
        let bind = |a: &str| RelayWrite::Bind {
            relay: "discord".into(),
            account: a.into(),
            owner: "o".into(),
        };
        assert_eq!(
            store.relay(bind("a")).await.unwrap(),
            RelayDone::Bound(None)
        );
        assert_eq!(
            store.relay(bind("a")).await.unwrap(),
            RelayDone::Bound(None)
        );
        assert_eq!(
            store.relay(bind("b")).await.unwrap(),
            RelayDone::Bound(Some(("a".into(), "o".into())))
        );
        // A room without a position starts just below its first queued message.
        store.relay(arrive("t", 50)).await.unwrap();
        store
            .relay(RelayWrite::Baseline {
                relay: "discord".into(),
                room: "t".into(),
                position: 70,
                parent: None,
            })
            .await
            .unwrap();
        let pos = store.read(|q| q.positions("discord")).await.unwrap();
        assert_eq!(pos[0].position, 49);
        store
            .relay(RelayWrite::Reset {
                relay: "discord".into(),
            })
            .await
            .unwrap();
        // After a reset another bot may bind.
        assert_eq!(
            store.relay(bind("c")).await.unwrap(),
            RelayDone::Bound(None)
        );
        assert_eq!(store.read(|q| q.queue("discord")).await.unwrap().len(), 0);
    }
}
