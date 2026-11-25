//! Store rows of lanes (DESIGN 7.3 "Lanes" and "Evidence"): the lane record with its history and
//! revision (LANE req. 28-30), open dedupe (LANE req. 10), reports with evidence novelty (EVR req.
//! 1-16), step records (EVR req. 30-32) and the lane thread journal (DISC-OUT req. 54-57). Each
//! write is one step of the writer's transaction, so a reader never sees half a lane.

use rusqlite::{OptionalExtension, Row, Transaction, params};
use serde::Serialize;

use super::Reads;

/// One lane row: the JSON record plus the columns listings filter on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaneRow {
    pub(crate) key: String,
    pub(crate) state: String,
    pub(crate) outcome: Option<String>,
    pub(crate) thread: Option<String>,
    pub(crate) opened_ms: i64,
    pub(crate) closed_ms: Option<i64>,
    pub(crate) revision: i64,
    pub(crate) record: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct HistoryRow {
    pub(crate) at_ms: i64,
    pub(crate) state: String,
    pub(crate) note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ReportRow {
    pub(crate) key: String,
    pub(crate) number: i64,
    pub(crate) run_id: String,
    pub(crate) at_ms: i64,
    pub(crate) kind: String,
    pub(crate) text: String,
    pub(crate) summary: Option<String>,
    pub(crate) new_items: i64,
    pub(crate) relay: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StepRow {
    pub(crate) name: String,
    pub(crate) run_id: Option<String>,
    pub(crate) at_ms: i64,
    pub(crate) result: String,
    pub(crate) evidence: String,
    pub(crate) receipts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JournalRow {
    pub(crate) source: String,
    pub(crate) lane: String,
    pub(crate) parent: String,
    pub(crate) operation: String,
    pub(crate) state: String,
    pub(crate) thread: Option<String>,
}

/// The revision a write expects, (`opened_ms`, history length).
pub(crate) type Revision = (i64, i64);

/// Every lane mutation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LaneWrite {
    /// Writes the record. `expect: None` creates it (refused when the key exists); otherwise the
    /// stored revision must equal `expect`. `note` appends a history entry (a state change).
    Save {
        key: String,
        state: String,
        outcome: Option<String>,
        thread: Option<String>,
        opened_ms: i64,
        closed_ms: Option<i64>,
        record: String,
        expect: Option<Revision>,
        note: Option<String>,
        /// A reopen: a new revision starts (history kept, `opened_ms` replaced).
        reopen: bool,
    },
    /// A start that failed before anything reached the agent (LANE req. 16).
    Delete {
        key: String,
        expect: Revision,
    },
    Dedupe {
        request_id: String,
        digest: String,
        key: String,
    },
    DedupeOutcome {
        request_id: String,
        outcome: String,
    },
    Report {
        key: String,
        run_id: String,
        kind: String,
        text: String,
        summary: Option<String>,
        evidence: String,
        /// (identity, hash) of each attached item.
        items: Vec<(String, String)>,
        relay: String,
    },
    Relay {
        key: String,
        number: i64,
        relay: String,
    },
    Step {
        key: String,
        name: String,
        run_id: Option<String>,
        result: String,
        evidence: String,
        receipts: Vec<String>,
        items: Vec<(String, String)>,
    },
    Journal {
        source: String,
        lane: String,
        parent: String,
        operation: String,
        state: String,
        thread: Option<String>,
    },
    JournalDrop {
        source: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LaneDone {
    Done,
    /// The revision now stored.
    Saved(Revision),
    /// A report: its number and how many attached items were new.
    Report {
        number: i64,
        new_items: i64,
    },
    /// A step: `true` when it was stored now, `false` when the same step was recorded before.
    Step(bool),
    Refused(String),
}

/// The refusal of a write that met another revision (LANE req. 30).
pub(crate) const MOVED_ON: &str = "the lane moved on";

fn revision(tx: &Transaction<'_>, key: &str) -> rusqlite::Result<Option<Revision>> {
    tx.query_row(
        "SELECT opened_ms, revision FROM lanes WHERE key = ?1",
        [key],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

/// Counts attached items whose hash differs from the one last recorded for this run, and records
/// them (EVR req. 15).
fn novelty(
    tx: &Transaction<'_>,
    key: &str,
    run: &str,
    items: &[(String, String)],
) -> rusqlite::Result<i64> {
    let mut new = 0;
    for (ident, hash) in items {
        let seen: Option<String> = tx
            .query_row(
                "SELECT hash FROM evidence_seen WHERE key = ?1 AND run_id = ?2 AND ident = ?3",
                params![key, run, ident],
                |r| r.get(0),
            )
            .optional()?;
        if seen.as_deref() != Some(hash.as_str()) {
            new += 1;
            tx.execute(
                "INSERT INTO evidence_seen (key, run_id, ident, hash) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (key, run_id, ident) DO UPDATE SET hash = excluded.hash",
                params![key, run, ident, hash],
            )?;
        }
    }
    Ok(new)
}

#[allow(clippy::too_many_lines)]
pub(crate) fn write(tx: &Transaction<'_>, w: &LaneWrite, now: i64) -> rusqlite::Result<LaneDone> {
    Ok(match w {
        LaneWrite::Save {
            key,
            state,
            outcome,
            thread,
            opened_ms,
            closed_ms,
            record,
            expect,
            note,
            reopen,
        } => {
            let stored = revision(tx, key)?;
            let mut length = match (stored, expect) {
                (None, None) => 0,
                (Some(_), None) => return Ok(LaneDone::Refused("the key is in use".into())),
                (Some(s), Some(e)) if s == *e => s.1,
                _ => return Ok(LaneDone::Refused(MOVED_ON.into())),
            };
            if let Some(note) = note {
                length += 1;
                tx.execute(
                    "INSERT INTO lane_history (key, seq, at_ms, state, note) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![key, length, now, state, note],
                )?;
            }
            // A reopen starts a new revision: the history continues, the open time is new.
            let _ = reopen;
            tx.execute(
                "INSERT INTO lanes (key, state, outcome, thread, opened_ms, closed_ms, revision, record) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT (key) DO UPDATE SET state = excluded.state, outcome = excluded.outcome, \
                 thread = excluded.thread, opened_ms = excluded.opened_ms, closed_ms = excluded.closed_ms, \
                 revision = excluded.revision, record = excluded.record",
                params![key, state, outcome, thread, opened_ms, closed_ms, length, record],
            )?;
            LaneDone::Saved((*opened_ms, length))
        }
        LaneWrite::Delete { key, expect } => {
            if revision(tx, key)? != Some(*expect) {
                return Ok(LaneDone::Refused(MOVED_ON.into()));
            }
            tx.execute("DELETE FROM lanes WHERE key = ?1", [key])?;
            tx.execute("DELETE FROM lane_history WHERE key = ?1", [key])?;
            LaneDone::Done
        }
        LaneWrite::Dedupe {
            request_id,
            digest,
            key,
        } => {
            tx.execute(
                "INSERT OR IGNORE INTO open_dedupe (request_id, digest, key, created_ms) VALUES (?1, ?2, ?3, ?4)",
                params![request_id, digest, key, now],
            )?;
            LaneDone::Done
        }
        LaneWrite::DedupeOutcome {
            request_id,
            outcome,
        } => {
            tx.execute(
                "UPDATE open_dedupe SET outcome = ?2 WHERE request_id = ?1",
                params![request_id, outcome],
            )?;
            LaneDone::Done
        }
        LaneWrite::Report {
            key,
            run_id,
            kind,
            text,
            summary,
            evidence,
            items,
            relay,
        } => {
            let number: i64 = tx.query_row(
                "SELECT COALESCE(MAX(number), 0) + 1 FROM reports WHERE key = ?1",
                [key],
                |r| r.get(0),
            )?;
            let new_items = novelty(tx, key, run_id, items)?;
            tx.execute(
                "INSERT INTO reports (key, number, run_id, at_ms, kind, text, summary, evidence, new_items, relay) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![key, number, run_id, now, kind, text, summary, evidence, new_items, relay],
            )?;
            LaneDone::Report { number, new_items }
        }
        LaneWrite::Relay { key, number, relay } => {
            tx.execute(
                "UPDATE reports SET relay = ?3 WHERE key = ?1 AND number = ?2",
                params![key, number, relay],
            )?;
            LaneDone::Done
        }
        LaneWrite::Step {
            key,
            name,
            run_id,
            result,
            evidence,
            receipts,
            items,
        } => {
            let receipts_json = serde_json::to_string(receipts).unwrap_or_default();
            let earlier: Option<(String, String, String)> = tx
                .query_row(
                    "SELECT result, evidence, receipts FROM steps WHERE key = ?1 AND name = ?2",
                    params![key, name],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            if let Some((r, e, rc)) = earlier {
                return Ok(if (&r, &e, &rc) == (result, evidence, &receipts_json) {
                    LaneDone::Step(false)
                } else {
                    LaneDone::Refused(format!("step '{name}' is recorded with other content"))
                });
            }
            let mut stmt = tx.prepare("SELECT name, receipts FROM steps WHERE key = ?1")?;
            let rows = stmt.query_map([key], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (other, list) = row?;
                let list: Vec<String> = serde_json::from_str(&list).unwrap_or_default();
                if let Some(taken) = receipts.iter().find(|x| list.contains(x)) {
                    return Ok(LaneDone::Refused(format!(
                        "receipt {taken} is already recorded under step '{other}'"
                    )));
                }
            }
            let run = run_id.as_deref().unwrap_or("unverified");
            let new_items = novelty(tx, key, run, items)?;
            tx.execute(
                "INSERT INTO steps (key, name, run_id, at_ms, result, evidence, receipts, new_items) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![key, name, run_id, now, result, evidence, receipts_json, new_items],
            )?;
            LaneDone::Step(true)
        }
        LaneWrite::Journal {
            source,
            lane,
            parent,
            operation,
            state,
            thread,
        } => {
            tx.execute(
                "INSERT INTO thread_journal (source, lane, parent, operation, state, thread, at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT (source) DO UPDATE SET state = excluded.state, thread = excluded.thread",
                params![source, lane, parent, operation, state, thread, now],
            )?;
            LaneDone::Done
        }
        LaneWrite::JournalDrop { source } => {
            tx.execute("DELETE FROM thread_journal WHERE source = ?1", [source])?;
            LaneDone::Done
        }
    })
}

const LANE_COLUMNS: &str = "key, state, outcome, thread, opened_ms, closed_ms, revision, record";

fn lane_row(r: &Row<'_>) -> rusqlite::Result<LaneRow> {
    Ok(LaneRow {
        key: r.get(0)?,
        state: r.get(1)?,
        outcome: r.get(2)?,
        thread: r.get(3)?,
        opened_ms: r.get(4)?,
        closed_ms: r.get(5)?,
        revision: r.get(6)?,
        record: r.get(7)?,
    })
}

const REPORT_COLUMNS: &str = "key, number, run_id, at_ms, kind, text, summary, new_items, relay";

fn report_row(r: &Row<'_>) -> rusqlite::Result<ReportRow> {
    Ok(ReportRow {
        key: r.get(0)?,
        number: r.get(1)?,
        run_id: r.get(2)?,
        at_ms: r.get(3)?,
        kind: r.get(4)?,
        text: r.get(5)?,
        summary: r.get(6)?,
        new_items: r.get(7)?,
        relay: r.get(8)?,
    })
}

impl Reads {
    pub(crate) fn lane(&self, key: &str) -> rusqlite::Result<Option<LaneRow>> {
        self.0
            .query_row(
                &format!("SELECT {LANE_COLUMNS} FROM lanes WHERE key = ?1"),
                [key],
                lane_row,
            )
            .optional()
    }

    /// Lanes ordered by thread then key; open ones only, or all.
    pub(crate) fn lanes(&self, open_only: bool) -> rusqlite::Result<Vec<LaneRow>> {
        let filter = if open_only {
            "WHERE state != 'closed'"
        } else {
            ""
        };
        let mut stmt = self.0.prepare(&format!(
            "SELECT {LANE_COLUMNS} FROM lanes {filter} ORDER BY COALESCE(thread, ''), key"
        ))?;
        stmt.query_map([], lane_row)?.collect()
    }

    pub(crate) fn lane_history(&self, key: &str) -> rusqlite::Result<Vec<HistoryRow>> {
        let mut stmt = self
            .0
            .prepare("SELECT at_ms, state, note FROM lane_history WHERE key = ?1 ORDER BY seq")?;
        stmt.query_map([key], |r| {
            Ok(HistoryRow {
                at_ms: r.get(0)?,
                state: r.get(1)?,
                note: r.get(2)?,
            })
        })?
        .collect()
    }

    /// (digest, lane key, outcome) of an earlier open with this request ID.
    pub(crate) fn open_dedupe(
        &self,
        request_id: &str,
    ) -> rusqlite::Result<Option<(String, String, Option<String>)>> {
        self.0
            .query_row(
                "SELECT digest, key, outcome FROM open_dedupe WHERE request_id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
    }

    /// The newest reports of a lane, oldest first; `run` limits them to one run.
    pub(crate) fn reports(
        &self,
        key: &str,
        run: Option<&str>,
        limit: u32,
    ) -> rusqlite::Result<Vec<ReportRow>> {
        let mut stmt = self.0.prepare(&format!(
            "SELECT {REPORT_COLUMNS} FROM (SELECT * FROM reports WHERE key = ?1 \
             AND (?2 IS NULL OR run_id = ?2) ORDER BY number DESC LIMIT ?3) ORDER BY number"
        ))?;
        stmt.query_map(params![key, run, limit], report_row)?
            .collect()
    }

    /// The last-progress mark of a run: the newest acceptance with new evidence (EVR req. 16).
    pub(crate) fn progress_mark(&self, key: &str, run: &str) -> rusqlite::Result<Option<i64>> {
        self.0.query_row(
            "SELECT MAX(at) FROM (SELECT MAX(at_ms) AS at FROM reports WHERE key = ?1 AND run_id = ?2 \
             AND new_items > 0 UNION ALL SELECT MAX(at_ms) FROM steps WHERE key = ?1 AND run_id = ?2 \
             AND new_items > 0)",
            params![key, run],
            |r| r.get(0),
        )
    }

    pub(crate) fn steps(&self, key: &str) -> rusqlite::Result<Vec<StepRow>> {
        let mut stmt = self.0.prepare(
            "SELECT name, run_id, at_ms, result, evidence, receipts FROM steps WHERE key = ?1 \
             ORDER BY at_ms, name",
        )?;
        stmt.query_map([key], |r| {
            let receipts: String = r.get(5)?;
            Ok(StepRow {
                name: r.get(0)?,
                run_id: r.get(1)?,
                at_ms: r.get(2)?,
                result: r.get(3)?,
                evidence: r.get(4)?,
                receipts: serde_json::from_str(&receipts).unwrap_or_default(),
            })
        })?
        .collect()
    }

    pub(crate) fn journal(&self, source: &str) -> rusqlite::Result<Option<JournalRow>> {
        self.0
            .query_row(
                "SELECT source, lane, parent, operation, state, thread FROM thread_journal WHERE source = ?1",
                [source],
                |r| {
                    Ok(JournalRow {
                        source: r.get(0)?,
                        lane: r.get(1)?,
                        parent: r.get(2)?,
                        operation: r.get(3)?,
                        state: r.get(4)?,
                        thread: r.get(5)?,
                    })
                },
            )
            .optional()
    }

    /// A journal entry that already names this lane (one creation per lane).
    pub(crate) fn journal_of_lane(&self, lane: &str) -> rusqlite::Result<Option<String>> {
        self.0
            .query_row(
                "SELECT source FROM thread_journal WHERE lane = ?1",
                [lane],
                |r| r.get(0),
            )
            .optional()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save(key: &str, expect: Option<Revision>, note: Option<&str>) -> LaneWrite {
        LaneWrite::Save {
            key: key.into(),
            state: "running".into(),
            outcome: None,
            thread: None,
            opened_ms: 10,
            closed_ms: None,
            record: "{}".into(),
            expect,
            note: note.map(str::to_string),
            reopen: false,
        }
    }

    #[tokio::test]
    async fn revisions_novelty_and_steps() {
        let (store, _w, _dir) = crate::store::tests::open_temp("lanes");
        let lanes = |w| {
            let store = store.clone();
            async move { store.lanes(w).await.unwrap() }
        };
        assert_eq!(
            lanes(save("a", None, Some("open"))).await,
            LaneDone::Saved((10, 1))
        );
        assert!(matches!(
            lanes(save("a", None, None)).await,
            LaneDone::Refused(_)
        ));
        assert_eq!(
            lanes(save("a", Some((10, 0)), Some("x"))).await,
            LaneDone::Refused(MOVED_ON.into())
        );
        assert_eq!(
            lanes(save("a", Some((10, 1)), None)).await,
            LaneDone::Saved((10, 1))
        );
        let report = |items: Vec<(&str, &str)>| LaneWrite::Report {
            key: "a".into(),
            run_id: "r1".into(),
            kind: "progress".into(),
            text: "t".into(),
            summary: None,
            evidence: "[]".into(),
            items: items
                .into_iter()
                .map(|(i, h)| (i.to_string(), h.to_string()))
                .collect(),
            relay: "not_posted".into(),
        };
        assert_eq!(
            lanes(report(vec![("file:/x", "h1")])).await,
            LaneDone::Report {
                number: 1,
                new_items: 1
            }
        );
        // A restated report and an identical file add nothing new (EVR req. 15).
        assert_eq!(
            lanes(report(vec![("file:/x", "h1")])).await,
            LaneDone::Report {
                number: 2,
                new_items: 0
            }
        );
        assert_eq!(
            lanes(report(vec![("file:/x", "h2"), ("tool:f", "f")])).await,
            LaneDone::Report {
                number: 3,
                new_items: 2
            }
        );
        let step = |name: &str, receipt: &str| LaneWrite::Step {
            key: "a".into(),
            name: name.into(),
            run_id: Some("r1".into()),
            result: "pushed".into(),
            evidence: "link".into(),
            receipts: vec![receipt.into()],
            items: vec![],
        };
        assert_eq!(lanes(step("push", "c1")).await, LaneDone::Step(true));
        assert_eq!(lanes(step("push", "c1")).await, LaneDone::Step(false));
        assert!(matches!(
            lanes(step("push", "c2")).await,
            LaneDone::Refused(_)
        ));
        assert!(matches!(
            lanes(step("other", "c1")).await,
            LaneDone::Refused(_)
        ));
        let mark = store.read(|r| r.progress_mark("a", "r1")).await.unwrap();
        assert!(mark.is_some());
    }
}
