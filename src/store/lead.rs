//! Store rows of the lead's records (DESIGN 7.3 "Owner"): the request log (EVR req. 44), the owner
//! question ledger (EVR req. 34-43), messages from others left unanswered (OJ req. 53-54), the
//! lead's stream jobs (WS req. 75), owed-reply reminders (DISC-OUT req. 35, OPC req. 9) and small
//! `meta` values. Each write is one savepoint in the writer's transaction.

use rusqlite::{OptionalExtension, Row, Transaction, params};
use serde::Serialize;

use super::Reads;

/// Reminders per owed reply, shared by the 60 s wake and the stop gate (DESIGN 12.2 C8).
pub(crate) const REMINDERS_MAX: i64 = 3;
/// A note longer than this ends the log's notes (EVR req. 44 caps each at 1000).
const NOTES_MAX: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RequestRow {
    pub(crate) id: i64,
    pub(crate) title: String,
    pub(crate) source: String,
    pub(crate) thread: Option<String>,
    pub(crate) next: Option<String>,
    pub(crate) notes: String,
    pub(crate) result: Option<String>,
    pub(crate) created_ms: i64,
    pub(crate) closed_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct QuestionRow {
    pub(crate) id: String,
    pub(crate) text: String,
    pub(crate) source: String,
    pub(crate) thread: Option<String>,
    pub(crate) options: Vec<String>,
    pub(crate) recommended: Option<i64>,
    pub(crate) lead_may: bool,
    pub(crate) request: Option<i64>,
    pub(crate) room: Option<String>,
    pub(crate) message: Option<String>,
    pub(crate) state: String,
    pub(crate) outcome: Option<String>,
    pub(crate) created_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OtherRow {
    pub(crate) room: String,
    pub(crate) message: String,
    pub(crate) author: String,
    pub(crate) text: String,
    pub(crate) at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwedOpen {
    pub(crate) room: String,
    pub(crate) message: String,
    pub(crate) admitted_ms: i64,
    pub(crate) text: String,
    pub(crate) reminders: i64,
}

/// Every lead-record mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LeadWrite {
    RequestAdd {
        title: String,
        source: String,
        thread: Option<String>,
    },
    RequestNext {
        id: i64,
        text: String,
    },
    RequestNote {
        id: i64,
        text: String,
    },
    RequestClose {
        id: i64,
        result: String,
    },
    QuestionAsk(Box<QuestionRow>),
    /// Settles an open question: `answered`, `chosen` or `withdrawn` with its outcome text; a note
    /// goes to its open request-log item (EVR req. 40-41).
    QuestionSettle {
        id: String,
        state: String,
        outcome: String,
    },
    Other {
        relay: String,
        row: OtherRow,
    },
    /// A message of the owner or the bot closes every earlier entry of the room (OJ req. 54).
    OthersAnswered {
        relay: String,
        room: String,
        at_ms: i64,
    },
    OtherReported {
        relay: String,
        room: String,
        message: String,
    },
    OthersDrop {
        before_ms: i64,
    },
    StreamSave {
        name: String,
        spec: String,
    },
    StreamRemove {
        name: String,
    },
    Meta {
        key: String,
        value: String,
    },
    /// Takes one reminder of an open owed reply, if any are left.
    Remind {
        relay: String,
        room: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LeadDone {
    Done,
    Id(i64),
    /// Refused with the reason (a closed item, a settled question, a bound message).
    Refused(String),
    /// Rows changed.
    Count(usize),
}

/// Opens a request-log entry for an admitted owner request; its title is the first line of the
/// request, cut to 200 characters (EVR req. 44).
pub(crate) fn open_request(
    tx: &Transaction<'_>,
    text: &str,
    source: &str,
    now: i64,
) -> rusqlite::Result<()> {
    let first = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("(no text)");
    let title: String = first.trim().chars().take(200).collect();
    tx.execute(
        "INSERT OR IGNORE INTO requests (title, source, created_ms, updated_ms) VALUES (?1, ?2, ?3, ?3)",
        params![title, source, now],
    )
    .map(drop)
}

fn changed_or(n: usize, why: &str) -> LeadDone {
    if n == 0 {
        LeadDone::Refused(why.into())
    } else {
        LeadDone::Done
    }
}

const NOT_OPEN: &str = "no open request-log item with that ID (closed items do not change)";

pub(crate) fn write(tx: &Transaction<'_>, w: &LeadWrite, now: i64) -> rusqlite::Result<LeadDone> {
    Ok(match w {
        LeadWrite::RequestAdd {
            title,
            source,
            thread,
        } => {
            tx.execute(
                "INSERT OR IGNORE INTO requests (title, source, thread, created_ms, updated_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?4)",
                params![title, source, thread, now],
            )?;
            LeadDone::Id(tx.query_row("SELECT id FROM requests WHERE source = ?1", [source], |r| {
                r.get(0)
            })?)
        }
        LeadWrite::RequestNext { id, text } => changed_or(
            tx.execute(
                "UPDATE requests SET next = ?2, updated_ms = ?3 WHERE id = ?1 AND closed_ms IS NULL",
                params![id, text, now],
            )?,
            NOT_OPEN,
        ),
        LeadWrite::RequestNote { id, text } => changed_or(note(tx, *id, text, now)?, NOT_OPEN),
        LeadWrite::RequestClose { id, result } => changed_or(
            tx.execute(
                "UPDATE requests SET result = ?2, closed_ms = ?3, updated_ms = ?3 \
                 WHERE id = ?1 AND closed_ms IS NULL",
                params![id, result, now],
            )?,
            NOT_OPEN,
        ),
        LeadWrite::QuestionAsk(q) => question_ask(tx, q, now)?,
        LeadWrite::QuestionSettle { id, state, outcome } => {
            question_settle(tx, id, state, outcome, now)?
        }
        LeadWrite::Other { relay, row } => LeadDone::Count(tx.execute(
            "INSERT OR IGNORE INTO unanswered (relay, room, message, author, text, at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![relay, row.room, row.message, row.author, row.text, row.at_ms],
        )?),
        LeadWrite::OthersAnswered { relay, room, at_ms } => LeadDone::Count(tx.execute(
            "DELETE FROM unanswered WHERE relay = ?1 AND room = ?2 AND at_ms <= ?3",
            params![relay, room, at_ms],
        )?),
        LeadWrite::OtherReported {
            relay,
            room,
            message,
        } => LeadDone::Count(tx.execute(
            "UPDATE unanswered SET reported = 1 WHERE relay = ?1 AND room = ?2 AND message = ?3",
            params![relay, room, message],
        )?),
        LeadWrite::OthersDrop { before_ms } => LeadDone::Count(
            tx.execute("DELETE FROM unanswered WHERE at_ms < ?1", [before_ms])?,
        ),
        LeadWrite::StreamSave { name, spec } => LeadDone::Count(tx.execute(
            "INSERT INTO stream_jobs (name, spec, created_ms) VALUES (?1, ?2, ?3) \
             ON CONFLICT (name) DO UPDATE SET spec = excluded.spec",
            params![name, spec, now],
        )?),
        LeadWrite::StreamRemove { name } => {
            LeadDone::Count(tx.execute("DELETE FROM stream_jobs WHERE name = ?1", [name])?)
        }
        LeadWrite::Meta { key, value } => LeadDone::Count(tx.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?),
        LeadWrite::Remind {
            relay,
            room,
            message,
        } => LeadDone::Count(tx.execute(
            "UPDATE owed_replies SET reminders = reminders + 1 WHERE relay = ?1 AND room = ?2 \
             AND message = ?3 AND settled_ms IS NULL AND reminders < ?4",
            params![relay, room, message, REMINDERS_MAX],
        )?),
    })
}

fn question_ask(tx: &Transaction<'_>, q: &QuestionRow, now: i64) -> rusqlite::Result<LeadDone> {
    let options = serde_json::to_string(&q.options).unwrap_or_else(|_| "[]".into());
    let n = tx.execute(
        "INSERT OR IGNORE INTO questions (id, text, source, thread, options, recommended, \
         lead_may, request, room, message, state, created_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'open', ?11)",
        params![
            q.id,
            q.text,
            q.source,
            q.thread,
            options,
            q.recommended,
            q.lead_may,
            q.request,
            q.room,
            q.message,
            now
        ],
    )?;
    Ok(changed_or(
        n,
        "that message is already bound to an open question",
    ))
}

fn question_settle(
    tx: &Transaction<'_>,
    id: &str,
    state: &str,
    outcome: &str,
    now: i64,
) -> rusqlite::Result<LeadDone> {
    let found: Option<(String, Option<i64>)> = tx
        .query_row(
            "SELECT state, request FROM questions WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(match found {
        None => LeadDone::Refused(format!("no question {id}")),
        Some((current, _)) if current != "open" => {
            LeadDone::Refused(format!("question {id} is already {current}"))
        }
        Some((_, request)) => {
            tx.execute(
                "UPDATE questions SET state = ?2, outcome = ?3, settled_ms = ?4 WHERE id = ?1",
                params![id, state, outcome, now],
            )?;
            if let Some(item) = request {
                note(tx, item, &format!("question {id} {state}: {outcome}"), now)?;
            }
            LeadDone::Done
        }
    })
}

fn note(tx: &Transaction<'_>, id: i64, text: &str, now: i64) -> rusqlite::Result<usize> {
    tx.execute(
        "UPDATE requests SET notes = substr(CASE WHEN notes = '' THEN ?2 ELSE notes || char(10) || ?2 END, \
         1, ?4), updated_ms = ?3 WHERE id = ?1 AND closed_ms IS NULL",
        params![id, text, now, i64::try_from(NOTES_MAX).unwrap_or(i64::MAX)],
    )
}

const REQUEST_COLUMNS: &str =
    "id, title, source, thread, next, notes, result, created_ms, closed_ms";
const QUESTION_COLUMNS: &str = "id, text, source, thread, options, recommended, lead_may, request, \
     room, message, state, outcome, created_ms";

fn request_of(r: &Row<'_>) -> rusqlite::Result<RequestRow> {
    Ok(RequestRow {
        id: r.get(0)?,
        title: r.get(1)?,
        source: r.get(2)?,
        thread: r.get(3)?,
        next: r.get(4)?,
        notes: r.get(5)?,
        result: r.get(6)?,
        created_ms: r.get(7)?,
        closed_ms: r.get(8)?,
    })
}

fn question_of(r: &Row<'_>) -> rusqlite::Result<QuestionRow> {
    let options: String = r.get(4)?;
    Ok(QuestionRow {
        id: r.get(0)?,
        text: r.get(1)?,
        source: r.get(2)?,
        thread: r.get(3)?,
        options: serde_json::from_str(&options).unwrap_or_default(),
        recommended: r.get(5)?,
        lead_may: r.get(6)?,
        request: r.get(7)?,
        room: r.get(8)?,
        message: r.get(9)?,
        state: r.get(10)?,
        outcome: r.get(11)?,
        created_ms: r.get(12)?,
    })
}

impl Reads {
    /// Request-log items, newest first: open ones only, or the latest 200.
    pub(crate) fn requests(&self, open_only: bool) -> rusqlite::Result<Vec<RequestRow>> {
        let filter = if open_only {
            "WHERE closed_ms IS NULL"
        } else {
            ""
        };
        let sql =
            format!("SELECT {REQUEST_COLUMNS} FROM requests {filter} ORDER BY id DESC LIMIT 200");
        let mut stmt = self.0.prepare(&sql)?;
        stmt.query_map([], request_of)?.collect()
    }

    pub(crate) fn questions(&self, open_only: bool) -> rusqlite::Result<Vec<QuestionRow>> {
        let filter = if open_only {
            "WHERE state = 'open'"
        } else {
            ""
        };
        let sql = format!(
            "SELECT {QUESTION_COLUMNS} FROM questions {filter} ORDER BY created_ms DESC LIMIT 200"
        );
        let mut stmt = self.0.prepare(&sql)?;
        stmt.query_map([], question_of)?.collect()
    }

    pub(crate) fn question(&self, id: &str) -> rusqlite::Result<Option<QuestionRow>> {
        let sql = format!("SELECT {QUESTION_COLUMNS} FROM questions WHERE id = ?1");
        self.0.query_row(&sql, [id], question_of).optional()
    }

    /// The open question bound to a posted message (EVR req. 37).
    pub(crate) fn question_at(
        &self,
        room: &str,
        message: &str,
    ) -> rusqlite::Result<Option<QuestionRow>> {
        let sql = format!(
            "SELECT {QUESTION_COLUMNS} FROM questions WHERE room = ?1 AND message = ?2 AND state = 'open'"
        );
        self.0
            .query_row(&sql, [room, message], question_of)
            .optional()
    }

    /// Entries not reported yet that arrived before `before_ms`.
    pub(crate) fn others_due(
        &self,
        relay: &str,
        before_ms: i64,
    ) -> rusqlite::Result<Vec<OtherRow>> {
        let mut stmt = self.0.prepare(
            "SELECT room, message, author, text, at_ms FROM unanswered WHERE relay = ?1 \
             AND reported = 0 AND at_ms <= ?2 ORDER BY at_ms LIMIT 100",
        )?;
        stmt.query_map(params![relay, before_ms], |r| {
            Ok(OtherRow {
                room: r.get(0)?,
                message: r.get(1)?,
                author: r.get(2)?,
                text: r.get(3)?,
                at_ms: r.get(4)?,
            })
        })?
        .collect()
    }

    pub(crate) fn stream_jobs(&self) -> rusqlite::Result<Vec<(String, String)>> {
        let mut stmt = self
            .0
            .prepare("SELECT name, spec FROM stream_jobs ORDER BY created_ms")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect()
    }

    pub(crate) fn meta(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.0
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()
    }

    /// Owed replies still open, admitted since `since_ms`, oldest first.
    pub(crate) fn owed_open(&self, relay: &str, since_ms: i64) -> rusqlite::Result<Vec<OwedOpen>> {
        let mut stmt = self.0.prepare(
            "SELECT o.room, o.message, o.admitted_ms, o.text, o.reminders FROM owed_replies o WHERE o.relay = ?1 \
             AND o.settled_ms IS NULL AND o.admitted_ms >= ?2 AND EXISTS (SELECT 1 FROM handoffs h \
             WHERE h.relay = o.relay AND h.room = o.room AND h.message = o.message \
             AND h.state IN ('handed', 'delivered') AND h.answer = 'waiting') \
             ORDER BY o.admitted_ms LIMIT 200",
        )?;
        stmt.query_map(params![relay, since_ms], |r| {
            Ok(OwedOpen {
                room: r.get(0)?,
                message: r.get(1)?,
                admitted_ms: r.get(2)?,
                text: r.get(3)?,
                reminders: r.get(4)?,
            })
        })?
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::open_temp;

    #[tokio::test]
    async fn request_log_and_question_ledger() {
        let (store, _h, _dir) = open_temp("lead-records");
        let add = LeadWrite::RequestAdd {
            title: "fix the build".into(),
            source: "discord:1:2".into(),
            thread: None,
        };
        let LeadDone::Id(id) = store.lead(add.clone()).await.unwrap() else {
            panic!("no id")
        };
        assert_eq!(store.lead(add).await.unwrap(), LeadDone::Id(id));
        store
            .lead(LeadWrite::RequestNext {
                id,
                text: "run tests".into(),
            })
            .await
            .unwrap();
        let q = QuestionRow {
            id: "0000abcd".into(),
            text: "Which?".into(),
            source: "discord:1:2".into(),
            thread: None,
            options: vec!["a".into(), "b".into()],
            recommended: Some(1),
            lead_may: false,
            request: Some(id),
            room: Some("1".into()),
            message: Some("9".into()),
            state: "open".into(),
            outcome: None,
            created_ms: 0,
        };
        assert_eq!(
            store
                .lead(LeadWrite::QuestionAsk(Box::new(q.clone())))
                .await
                .unwrap(),
            LeadDone::Done
        );
        let mut twice = q;
        twice.id = "0000abce".into();
        assert!(matches!(
            store
                .lead(LeadWrite::QuestionAsk(Box::new(twice)))
                .await
                .unwrap(),
            LeadDone::Refused(_)
        ));
        let settle = LeadWrite::QuestionSettle {
            id: "0000abcd".into(),
            state: "answered".into(),
            outcome: "option 2".into(),
        };
        assert_eq!(store.lead(settle.clone()).await.unwrap(), LeadDone::Done);
        assert!(matches!(
            store.lead(settle).await.unwrap(),
            LeadDone::Refused(_)
        ));
        let open = store.read(|r| r.requests(true)).await.unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].next.as_deref(), Some("run tests"));
        assert!(open[0].notes.contains("question 0000abcd answered"));
        store
            .lead(LeadWrite::RequestClose {
                id,
                result: "done".into(),
            })
            .await
            .unwrap();
        assert!(matches!(
            store
                .lead(LeadWrite::RequestNote {
                    id,
                    text: "late".into()
                })
                .await
                .unwrap(),
            LeadDone::Refused(_)
        ));
        assert_eq!(store.read(|r| r.requests(true)).await.unwrap(), Vec::new());
    }
}
