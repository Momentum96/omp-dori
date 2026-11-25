//! Store rows of the session host (DESIGN 7.3 "Sessions"): session records, the inbox, deliveries,
//! receipts and once-only identities. Writes run on the writer thread inside its transaction; reads
//! on the read pool.

use rusqlite::{OptionalExtension, Row, Transaction, params};

use super::Reads;

/// Receipts kept per target (HOST req. 59).
pub(crate) const RECEIPT_CAP: i64 = 512;
/// Once-only identities are remembered this long (COORD req. 19).
const ONCE_KEEP_MS: i64 = 30 * 24 * 3600 * 1000;
/// Finished inbox rows, deliveries and their receipts are kept this long (DESIGN 7.2).
const FINISHED_KEEP_MS: i64 = 7 * 24 * 3600 * 1000;
/// An identity with this prefix names the latest note of a recurring kind (planner summaries,
/// recaps, digests): a new one retires the undelivered earlier one with the same identity instead of
/// being refused as a repeat, and it may come again after delivery.
pub(crate) const LATEST_PREFIX: &str = "latest:";
/// Undelivered messages per target that are not the owner's. Overflow rule: admitting one more
/// retires the oldest undelivered non-owner message (state `failed`, kept 7 days for its receipt).
/// Owner messages are never retired and do not count.
pub(crate) const INBOX_CAP: i64 = 200;

/// One session record (HOST req. 6, 69).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionRow {
    pub(crate) handle: String,
    pub(crate) role: String,
    pub(crate) runtime: String,
    pub(crate) cwd: String,
    pub(crate) name: String,
    pub(crate) model: String,
    pub(crate) thinking: Option<String>,
    /// `fresh` or `resume`.
    pub(crate) mode: String,
    pub(crate) expected_id: Option<String>,
    pub(crate) expected_file: Option<String>,
    pub(crate) state: String,
    pub(crate) session_id: Option<String>,
    pub(crate) session_file: Option<String>,
    pub(crate) pid: Option<i64>,
    pub(crate) started_ms: i64,
    pub(crate) updated_ms: i64,
    /// How it ended: `clean`, `term`, `kill`, `exited`, `failed`, `lost`.
    pub(crate) stop_how: Option<String>,
    pub(crate) stop_detail: Option<String>,
    pub(crate) stop_ms: Option<i64>,
    pub(crate) fault: Option<String>,
}

const SESSION_COLUMNS: &str = "handle, role, runtime, cwd, name, model, thinking, mode, expected_id, \
     expected_file, state, session_id, session_file, pid, started_ms, updated_ms, stop_how, stop_detail, \
     stop_ms, fault";

fn session_from_row(r: &Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        handle: r.get(0)?,
        role: r.get(1)?,
        runtime: r.get(2)?,
        cwd: r.get(3)?,
        name: r.get(4)?,
        model: r.get(5)?,
        thinking: r.get(6)?,
        mode: r.get(7)?,
        expected_id: r.get(8)?,
        expected_file: r.get(9)?,
        state: r.get(10)?,
        session_id: r.get(11)?,
        session_file: r.get(12)?,
        pid: r.get(13)?,
        started_ms: r.get(14)?,
        updated_ms: r.get(15)?,
        stop_how: r.get(16)?,
        stop_detail: r.get(17)?,
        stop_ms: r.get(18)?,
        fault: r.get(19)?,
    })
}

/// A change to a session record; `None` leaves a field as it is.
#[derive(Debug, Clone, Default)]
pub(crate) struct SessionUpdate {
    pub(crate) handle: String,
    pub(crate) state: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) session_file: Option<String>,
    pub(crate) pid: Option<i64>,
    pub(crate) fault: Option<String>,
    /// The stop record: how, detail.
    pub(crate) stop: Option<(String, Option<String>)>,
}

/// A message offered to a target's inbox.
#[derive(Debug, Clone)]
pub(crate) struct Admit {
    pub(crate) target: String,
    pub(crate) request_id: Option<String>,
    /// Digest of the delivery kind and text.
    pub(crate) digest: String,
    pub(crate) sender: String,
    pub(crate) text: String,
    pub(crate) urgent: bool,
    pub(crate) owner: bool,
    pub(crate) identity: Option<String>,
}

/// What the inbox did with an offered message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Admitted {
    Stored(i64),
    /// The request ID was used before with the same text; its inbox row, if it was stored.
    Earlier(Option<i64>),
    /// The request ID was used before with different text.
    Mismatch,
    /// The target holds the maximum number of receipts.
    Full,
    /// The stable identity is pending or was delivered within 30 days.
    Seen,
}

/// One inbox row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InboxRow {
    pub(crate) id: i64,
    pub(crate) sender: String,
    pub(crate) text: String,
    pub(crate) urgent: bool,
    pub(crate) owner: bool,
    pub(crate) identity: Option<String>,
    pub(crate) state: String,
    pub(crate) delivery_id: Option<String>,
    pub(crate) created_ms: i64,
}

const INBOX_COLUMNS: &str =
    "id, sender, text, urgent, owner, identity, state, delivery_id, created_ms";

fn inbox_from_row(r: &Row<'_>) -> rusqlite::Result<InboxRow> {
    Ok(InboxRow {
        id: r.get(0)?,
        sender: r.get(1)?,
        text: r.get(2)?,
        urgent: r.get(3)?,
        owner: r.get(4)?,
        identity: r.get(5)?,
        state: r.get(6)?,
        delivery_id: r.get(7)?,
        created_ms: r.get(8)?,
    })
}

/// One delivery (PATCH-01 rule 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeliveryRow {
    pub(crate) id: String,
    pub(crate) target: String,
    pub(crate) handle: String,
    pub(crate) session_id: String,
    pub(crate) session_file: String,
    /// `wake`, `follow_up` or `steer`.
    pub(crate) kind: String,
    pub(crate) text: String,
    pub(crate) state: String,
    pub(crate) resent: bool,
    pub(crate) created_ms: i64,
    pub(crate) sent_ms: i64,
    pub(crate) detail: Option<String>,
}

const DELIVERY_COLUMNS: &str = "id, target, handle, session_id, session_file, kind, text, state, resent, \
     created_ms, sent_ms, detail";

fn delivery_from_row(r: &Row<'_>) -> rusqlite::Result<DeliveryRow> {
    Ok(DeliveryRow {
        id: r.get(0)?,
        target: r.get(1)?,
        handle: r.get(2)?,
        session_id: r.get(3)?,
        session_file: r.get(4)?,
        kind: r.get(5)?,
        text: r.get(6)?,
        state: r.get(7)?,
        resent: r.get(8)?,
        created_ms: r.get(9)?,
        sent_ms: r.get(10)?,
        detail: r.get(11)?,
    })
}

/// Every host mutation.
#[derive(Debug, Clone)]
pub(crate) enum HostWrite {
    /// Inserts a new session record; an existing handle fails.
    Record(SessionRow),
    Session(SessionUpdate),
    Admit(Admit),
    /// Records a delivery as sent and moves its inbox rows to `sent`, before the write to OMP.
    Send {
        delivery: DeliveryRow,
        inbox: Vec<i64>,
    },
    /// [`HostWrite::Admit`], and when the message is stored, [`HostWrite::Send`] of the batch that
    /// carries it (`inbox` lists the other rows): one commit on the wake path (DESIGN 2.3, 7.2).
    AdmitSend {
        admit: Admit,
        delivery: DeliveryRow,
        inbox: Vec<i64>,
    },
    /// Ends or re-opens a delivery: `delivered`, `in_doubt`, `failed`, `misdelivered`, `superseded`
    /// (never written: its rows become pending again), or `sent` again for the one resend.
    Resolve {
        id: String,
        state: String,
        detail: Option<String>,
    },
    /// The owner or lead confirms (it arrived) or releases (it did not) an in-doubt inbox row.
    Settle {
        target: String,
        inbox_id: i64,
        confirm: bool,
    },
    /// Ends a target's unsent rows: they belonged to a run that is over (a closed lane's notes).
    Retire {
        target: String,
    },
    /// A resent delivery now belongs to the process it was resent into.
    Rebind {
        id: String,
        handle: String,
    },
    /// Tests: makes a target's written deliveries older.
    #[cfg(test)]
    Age {
        target: String,
        by_ms: i64,
    },
}

/// What a host write returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostDone {
    Done,
    Admitted(Admitted),
    /// Rows changed.
    Rows(usize),
}

fn record(tx: &Transaction<'_>, row: &SessionRow) -> rusqlite::Result<HostDone> {
    tx.execute(
        &format!(
            "INSERT INTO sessions ({SESSION_COLUMNS}) VALUES \
             (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)"
        ),
        params![
            row.handle,
            row.role,
            row.runtime,
            row.cwd,
            row.name,
            row.model,
            row.thinking,
            row.mode,
            row.expected_id,
            row.expected_file,
            row.state,
            row.session_id,
            row.session_file,
            row.pid,
            row.started_ms,
            row.updated_ms,
            row.stop_how,
            row.stop_detail,
            row.stop_ms,
            row.fault
        ],
    )?;
    Ok(HostDone::Done)
}

fn update(tx: &Transaction<'_>, u: &SessionUpdate, now: i64) -> rusqlite::Result<HostDone> {
    let (how, detail) = u.stop.clone().map_or((None, None), |(h, d)| (Some(h), d));
    let stop_ms = how.as_ref().map(|_| now);
    let rows = tx.execute(
        "UPDATE sessions SET state = COALESCE(?2, state), session_id = COALESCE(?3, session_id), \
         session_file = COALESCE(?4, session_file), pid = COALESCE(?5, pid), \
         fault = COALESCE(?6, fault), stop_how = COALESCE(?7, stop_how), \
         stop_detail = COALESCE(?8, stop_detail), stop_ms = COALESCE(?9, stop_ms), updated_ms = ?10 \
         WHERE handle = ?1",
        params![
            u.handle,
            u.state,
            u.session_id,
            u.session_file,
            u.pid,
            u.fault,
            how,
            detail,
            stop_ms,
            now
        ],
    )?;
    Ok(HostDone::Rows(rows))
}

pub(super) fn write(
    tx: &Transaction<'_>,
    write: &HostWrite,
    now: i64,
) -> rusqlite::Result<HostDone> {
    match write {
        HostWrite::Record(row) => record(tx, row),
        HostWrite::Session(u) => update(tx, u, now),
        HostWrite::Admit(a) => admit(tx, a, now).map(HostDone::Admitted),
        HostWrite::Send { delivery, inbox } => {
            send(tx, delivery, inbox, now)?;
            Ok(HostDone::Done)
        }
        HostWrite::AdmitSend {
            admit: a,
            delivery,
            inbox,
        } => {
            let admitted = admit(tx, a, now)?;
            if let Admitted::Stored(id) = admitted {
                send(tx, delivery, inbox, now)?;
                send_rows(tx, &delivery.id, &[id], now)?;
            }
            Ok(HostDone::Admitted(admitted))
        }
        HostWrite::Resolve { id, state, detail } => resolve(tx, id, state, detail.as_deref(), now),
        HostWrite::Settle {
            target,
            inbox_id,
            confirm,
        } => {
            let (to, delivery) = if *confirm {
                ("delivered", "delivery_id")
            } else {
                ("pending", "NULL")
            };
            let rows = tx.execute(
                &format!(
                    "UPDATE inbox SET state = '{to}', delivery_id = {delivery}, updated_ms = ?3 \
                     WHERE id = ?1 AND target = ?2 AND state = 'in_doubt'"
                ),
                params![inbox_id, target, now],
            )?;
            if *confirm {
                remember_identities(tx, &format!("id = {inbox_id}"), now)?;
            }
            Ok(HostDone::Rows(rows))
        }
        HostWrite::Retire { target } => tx
            .execute(
                "UPDATE inbox SET state = 'failed', updated_ms = ?2 \
                 WHERE target = ?1 AND state = 'pending'",
                params![target, now],
            )
            .map(HostDone::Rows),
        HostWrite::Rebind { id, handle } => tx
            .execute(
                "UPDATE deliveries SET handle = ?2 WHERE id = ?1",
                params![id, handle],
            )
            .map(HostDone::Rows),
        #[cfg(test)]
        HostWrite::Age { target, by_ms } => tx
            .execute(
                "UPDATE deliveries SET sent_ms = sent_ms - ?2 WHERE target = ?1",
                params![target, by_ms],
            )
            .map(HostDone::Rows),
    }
}

fn send(tx: &Transaction<'_>, d: &DeliveryRow, inbox: &[i64], now: i64) -> rusqlite::Result<()> {
    tx.execute(
        &format!(
            "INSERT INTO deliveries ({DELIVERY_COLUMNS}) VALUES \
             (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'sent', 0, ?8, ?9, NULL)"
        ),
        params![
            d.id,
            d.target,
            d.handle,
            d.session_id,
            d.session_file,
            d.kind,
            d.text,
            d.created_ms,
            d.sent_ms
        ],
    )?;
    send_rows(tx, &d.id, inbox, now)
}

fn send_rows(
    tx: &Transaction<'_>,
    delivery: &str,
    inbox: &[i64],
    now: i64,
) -> rusqlite::Result<()> {
    for id in inbox {
        tx.execute(
            "UPDATE inbox SET state = 'sent', delivery_id = ?2, updated_ms = ?3 \
             WHERE id = ?1 AND state = 'pending'",
            params![id, delivery, now],
        )?;
    }
    Ok(())
}

fn admit(tx: &Transaction<'_>, a: &Admit, now: i64) -> rusqlite::Result<Admitted> {
    if let Some(rid) = &a.request_id {
        let earlier: Option<(String, Option<i64>)> = tx
            .query_row(
                "SELECT digest, inbox_id FROM receipts WHERE target = ?1 AND request_id = ?2",
                params![a.target, rid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((digest, inbox)) = earlier {
            return Ok(if digest == a.digest {
                Admitted::Earlier(inbox)
            } else {
                Admitted::Mismatch
            });
        }
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM receipts WHERE target = ?1",
            [&a.target],
            |r| r.get(0),
        )?;
        if count >= RECEIPT_CAP {
            return Ok(Admitted::Full);
        }
    }
    if let Some(identity) = &a.identity {
        if identity.starts_with(LATEST_PREFIX) {
            tx.execute(
                "UPDATE inbox SET state = 'failed', updated_ms = ?3 \
                 WHERE target = ?1 AND identity = ?2 AND state = 'pending' AND owner = 0",
                params![a.target, identity, now],
            )?;
        } else {
            let seen: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM once_ids WHERE target = ?1 AND identity = ?2 AND at_ms >= ?3) \
                 OR EXISTS (SELECT 1 FROM inbox WHERE target = ?1 AND identity = ?2 \
                            AND state IN ('pending', 'sent', 'in_doubt'))",
                params![a.target, identity, now - ONCE_KEEP_MS],
                |r| r.get(0),
            )?;
            if seen {
                return Ok(Admitted::Seen);
            }
        }
    }
    if !a.owner {
        tx.execute(
            "UPDATE inbox SET state = 'failed', updated_ms = ?3 WHERE id IN ( \
                 SELECT id FROM inbox WHERE target = ?1 AND state = 'pending' AND owner = 0 \
                 ORDER BY id DESC LIMIT -1 OFFSET ?2)",
            params![a.target, INBOX_CAP - 1, now],
        )?;
    }
    tx.execute(
        "INSERT INTO inbox (target, sender, text, urgent, owner, identity, state, created_ms, updated_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?7)",
        params![
            a.target, a.sender, a.text, a.urgent, a.owner, a.identity, now
        ],
    )?;
    let id = tx.last_insert_rowid();
    if let Some(rid) = &a.request_id {
        tx.execute(
            "INSERT INTO receipts (target, request_id, digest, inbox_id, created_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![a.target, rid, a.digest, id, now],
        )?;
    }
    Ok(Admitted::Stored(id))
}

fn remember_identities(tx: &Transaction<'_>, filter: &str, now: i64) -> rusqlite::Result<()> {
    tx.execute(
        &format!(
            "INSERT OR REPLACE INTO once_ids (target, identity, at_ms) \
             SELECT target, identity, ?1 FROM inbox WHERE identity IS NOT NULL AND {filter}"
        ),
        [now],
    )
    .map(drop)
}

fn resolve(
    tx: &Transaction<'_>,
    id: &str,
    state: &str,
    detail: Option<&str>,
    now: i64,
) -> rusqlite::Result<HostDone> {
    let confirmed = (state == "delivered").then_some(now);
    let sent = (state == "sent").then_some(now);
    let rows = tx.execute(
        "UPDATE deliveries SET state = ?2, detail = COALESCE(?3, detail), \
         confirmed_ms = COALESCE(?4, confirmed_ms), sent_ms = COALESCE(?5, sent_ms), \
         resent = CASE WHEN ?2 = 'sent' THEN 1 ELSE resent END WHERE id = ?1",
        params![id, state, detail, confirmed, sent],
    )?;
    let inbox_state = match state {
        "delivered" => Some("delivered"),
        "in_doubt" | "misdelivered" => Some("in_doubt"),
        "failed" => Some("failed"),
        "superseded" => Some("pending"),
        "sent" => Some("sent"),
        _ => None,
    };
    if let Some(to) = inbox_state {
        let keep_link = to != "pending";
        tx.execute(
            &format!(
                "UPDATE inbox SET state = ?2, updated_ms = ?3{} WHERE delivery_id = ?1",
                if keep_link {
                    ""
                } else {
                    ", delivery_id = NULL"
                }
            ),
            params![id, to, now],
        )?;
    }
    if state == "delivered" {
        tx.execute(
            "INSERT OR REPLACE INTO once_ids (target, identity, at_ms) \
             SELECT target, identity, ?2 FROM inbox WHERE identity IS NOT NULL AND delivery_id = ?1",
            params![id, now],
        )?;
    }
    Ok(HostDone::Rows(rows))
}

/// Retention (DESIGN 7.2): finished inbox rows, deliveries and receipts after 7 days; once-only
/// identities after 30 days; stopped session records after 30 days except each role's newest.
pub(super) fn maintain(tx: &Transaction<'_>, now: i64) -> rusqlite::Result<()> {
    let finished = now - FINISHED_KEEP_MS;
    tx.execute(
        "DELETE FROM inbox WHERE state IN ('delivered', 'failed') AND updated_ms < ?1",
        [finished],
    )?;
    tx.execute(
        "DELETE FROM deliveries WHERE state IN ('delivered', 'failed', 'superseded', 'misdelivered') \
         AND sent_ms < ?1",
        [finished],
    )?;
    tx.execute(
        "DELETE FROM receipts WHERE created_ms < ?1 AND (inbox_id IS NULL OR inbox_id NOT IN (SELECT id FROM inbox))",
        [finished],
    )?;
    tx.execute(
        "DELETE FROM once_ids WHERE at_ms < ?1",
        [now - ONCE_KEEP_MS],
    )?;
    tx.execute(
        "DELETE FROM sessions AS s WHERE s.state IN ('stopped', 'failed') AND s.updated_ms < ?1 \
         AND s.started_ms < (SELECT MAX(started_ms) FROM sessions WHERE role = s.role)",
        [now - ONCE_KEEP_MS],
    )
    .map(drop)
}

impl Reads {
    /// The newest session record of a role.
    pub(crate) fn latest_session(&self, role: &str) -> rusqlite::Result<Option<SessionRow>> {
        self.0
            .query_row(
                &format!(
                    "SELECT {SESSION_COLUMNS} FROM sessions WHERE role = ?1 \
                     ORDER BY started_ms DESC, rowid DESC LIMIT 1"
                ),
                [role],
                session_from_row,
            )
            .optional()
    }

    /// Records left starting or live (recovery, HOST req. 85).
    pub(crate) fn open_sessions(&self) -> rusqlite::Result<Vec<SessionRow>> {
        let mut stmt = self.0.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions WHERE state IN ('starting', 'live') ORDER BY started_ms"
        ))?;
        stmt.query_map([], session_from_row)?.collect()
    }

    /// Pending, sent and in-doubt inbox rows of a target, oldest first.
    pub(crate) fn inbox_open(&self, target: &str) -> rusqlite::Result<Vec<InboxRow>> {
        let mut stmt = self.0.prepare(&format!(
            "SELECT {INBOX_COLUMNS} FROM inbox WHERE target = ?1 \
             AND state IN ('pending', 'sent', 'in_doubt') ORDER BY id"
        ))?;
        stmt.query_map([target], inbox_from_row)?.collect()
    }

    /// One inbox row.
    pub(crate) fn inbox_row(&self, id: i64) -> rusqlite::Result<Option<InboxRow>> {
        self.0
            .query_row(
                &format!("SELECT {INBOX_COLUMNS} FROM inbox WHERE id = ?1"),
                [id],
                inbox_from_row,
            )
            .optional()
    }

    /// The inbox rows of one delivery.
    pub(crate) fn inbox_rows_of(&self, delivery: &str) -> rusqlite::Result<Vec<InboxRow>> {
        let mut stmt = self.0.prepare(&format!(
            "SELECT {INBOX_COLUMNS} FROM inbox WHERE delivery_id = ?1 ORDER BY id"
        ))?;
        stmt.query_map([delivery], inbox_from_row)?.collect()
    }

    /// The receipt a request ID left, `None` for an unknown ID.
    pub(crate) fn receipt(
        &self,
        target: &str,
        request_id: &str,
    ) -> rusqlite::Result<Option<StoredReceipt>> {
        self.0
            .query_row(
                "SELECT inbox_id FROM receipts WHERE target = ?1 AND request_id = ?2",
                params![target, request_id],
                |r| {
                    Ok(StoredReceipt {
                        inbox_id: r.get(0)?,
                    })
                },
            )
            .optional()
    }
}

/// A stored receipt: the inbox row it stored, if it stored one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StoredReceipt {
    pub(crate) inbox_id: Option<i64>,
}

impl Reads {
    /// Deliveries of a target still `sent` (written, not yet confirmed), oldest first.
    pub(crate) fn sent_deliveries(&self, target: &str) -> rusqlite::Result<Vec<DeliveryRow>> {
        let mut stmt = self.0.prepare(&format!(
            "SELECT {DELIVERY_COLUMNS} FROM deliveries WHERE target = ?1 AND state = 'sent' ORDER BY sent_ms"
        ))?;
        stmt.query_map([target], delivery_from_row)?.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::open_temp;

    fn admit(target: &str, rid: Option<&str>, text: &str, identity: Option<&str>) -> HostWrite {
        HostWrite::Admit(Admit {
            target: target.into(),
            request_id: rid.map(Into::into),
            digest: format!("d-{text}"),
            sender: "owner".into(),
            text: text.into(),
            urgent: true,
            owner: true,
            identity: identity.map(Into::into),
        })
    }

    #[tokio::test]
    async fn receipts_once_ids_and_resolution() {
        let (store, _writer, _dir) = open_temp("host-rows");
        let first = store
            .host(admit("lead", Some("r1"), "hi", None))
            .await
            .unwrap();
        let HostDone::Admitted(Admitted::Stored(id)) = first else {
            panic!("{first:?}")
        };
        assert_eq!(
            store
                .host(admit("lead", Some("r1"), "hi", None))
                .await
                .unwrap(),
            HostDone::Admitted(Admitted::Earlier(Some(id)))
        );
        assert_eq!(
            store
                .host(admit("lead", Some("r1"), "other", None))
                .await
                .unwrap(),
            HostDone::Admitted(Admitted::Mismatch)
        );
        let once = store
            .host(admit("lead", None, "n", Some("x:1")))
            .await
            .unwrap();
        assert!(matches!(once, HostDone::Admitted(Admitted::Stored(_))));
        assert_eq!(
            store
                .host(admit("lead", None, "n", Some("x:1")))
                .await
                .unwrap(),
            HostDone::Admitted(Admitted::Seen)
        );
        let delivery = DeliveryRow {
            id: "d1".into(),
            target: "lead".into(),
            handle: "h".into(),
            session_id: "s".into(),
            session_file: "/f".into(),
            kind: "wake".into(),
            text: "[cataly:d1]".into(),
            state: "sent".into(),
            resent: false,
            created_ms: 1,
            sent_ms: 1,
            detail: None,
        };
        let open = store.read(|r| r.inbox_open("lead")).await.unwrap();
        store
            .host(HostWrite::Send {
                delivery,
                inbox: open.iter().map(|r| r.id).collect(),
            })
            .await
            .unwrap();
        assert_eq!(
            store
                .read(|r| r.sent_deliveries("lead"))
                .await
                .unwrap()
                .len(),
            1
        );
        store
            .host(HostWrite::Resolve {
                id: "d1".into(),
                state: "in_doubt".into(),
                detail: Some("no answer".into()),
            })
            .await
            .unwrap();
        let rows = store.read(|r| r.inbox_open("lead")).await.unwrap();
        assert!(rows.iter().all(|r| r.state == "in_doubt"), "{rows:?}");
        let settled = store
            .host(HostWrite::Settle {
                target: "lead".into(),
                inbox_id: id,
                confirm: false,
            })
            .await
            .unwrap();
        assert_eq!(settled, HostDone::Rows(1));
        let row = store.read(move |r| r.inbox_row(id)).await.unwrap().unwrap();
        assert_eq!((row.state.as_str(), row.delivery_id), ("pending", None));
        store
            .host(HostWrite::Settle {
                target: "lead".into(),
                inbox_id: once_id(&rows, "x:1"),
                confirm: true,
            })
            .await
            .unwrap();
        // Confirmed identities stay remembered after the row is gone.
        assert_eq!(
            store
                .host(admit("lead", None, "n", Some("x:1")))
                .await
                .unwrap(),
            HostDone::Admitted(Admitted::Seen)
        );
    }

    fn once_id(rows: &[InboxRow], identity: &str) -> i64 {
        rows.iter()
            .find(|r| r.identity.as_deref() == Some(identity))
            .unwrap()
            .id
    }
}
