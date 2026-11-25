//! Typed reads over a read-only connection.

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::{EventRecord, JobRow, RunRecord};

pub(crate) const JOB_COLUMNS: &str =
    "name, kind, trigger, held, consumed, needs_ack, next_due_ms, skipped, created_ms";
pub(crate) const RUN_COLUMNS: &str = "id, job, dry, scheduled_ms, started_ms, pid, ended_ms, exit_code, signal, \
     timed_out, state, outcome, stdout_path, stderr_path, bytes, output_hash, kept";
const EVENT_COLUMNS: &str =
    "id, at_ms, job, subject, kind, severity, text, body, body_path, lane, thread, urgent, dry";

/// A pooled read-only connection with the store's queries.
pub(crate) struct Reads(pub(crate) Connection);

pub(crate) fn job_from_row(r: &Row<'_>) -> rusqlite::Result<JobRow> {
    Ok(JobRow {
        name: r.get(0)?,
        kind: r.get(1)?,
        trigger: r.get(2)?,
        held: r.get(3)?,
        consumed: r.get(4)?,
        needs_ack: r.get(5)?,
        next_due_ms: r.get(6)?,
        skipped: r.get(7)?,
        created_ms: r.get(8)?,
    })
}

pub(crate) fn run_from_row(r: &Row<'_>) -> rusqlite::Result<RunRecord> {
    Ok(RunRecord {
        id: r.get(0)?,
        job: r.get(1)?,
        dry: r.get(2)?,
        scheduled_ms: r.get(3)?,
        started_ms: r.get(4)?,
        pid: r.get(5)?,
        ended_ms: r.get(6)?,
        exit_code: r.get(7)?,
        signal: r.get(8)?,
        timed_out: r.get(9)?,
        state: r.get(10)?,
        outcome: r.get(11)?,
        stdout_path: r.get(12)?,
        stderr_path: r.get(13)?,
        bytes: r.get(14)?,
        output_hash: r.get(15)?,
        kept: r.get(16)?,
    })
}

fn event_from_row(r: &Row<'_>) -> rusqlite::Result<EventRecord> {
    Ok(EventRecord {
        id: r.get(0)?,
        at_ms: r.get(1)?,
        job: r.get(2)?,
        subject: r.get(3)?,
        kind: r.get(4)?,
        severity: r.get(5)?,
        text: r.get(6)?,
        body: r.get(7)?,
        body_path: r.get(8)?,
        lane: r.get(9)?,
        thread: r.get(10)?,
        urgent: r.get(11)?,
        dry: r.get(12)?,
    })
}

impl Reads {
    /// A consistent copy of the whole store at `path` through SQLite's online backup, for the home
    /// backup (owner decision 16): never the live file, its WAL or its journal.
    pub(crate) fn snapshot_into(&self, path: &std::path::Path) -> rusqlite::Result<()> {
        self.0.backup(rusqlite::MAIN_DB, path, None)
    }

    /// Events with an ID above `after`, oldest first.
    pub(crate) fn events_after(
        &self,
        after: i64,
        limit: u32,
    ) -> rusqlite::Result<Vec<EventRecord>> {
        let mut stmt = self.0.prepare_cached(&format!(
            "SELECT {EVENT_COLUMNS} FROM events WHERE id > ?1 ORDER BY id LIMIT ?2"
        ))?;
        stmt.query_map(params![after, limit], event_from_row)?
            .collect()
    }

    /// Events at or after `at_ms`, oldest first.
    pub(crate) fn events_since(
        &self,
        at_ms: i64,
        limit: u32,
    ) -> rusqlite::Result<Vec<EventRecord>> {
        let mut stmt = self.0.prepare_cached(&format!(
            "SELECT {EVENT_COLUMNS} FROM events WHERE at_ms >= ?1 ORDER BY id LIMIT ?2"
        ))?;
        stmt.query_map(params![at_ms, limit], event_from_row)?
            .collect()
    }

    /// The newest `limit` events, oldest first.
    pub(crate) fn events_latest(&self, limit: u32) -> rusqlite::Result<Vec<EventRecord>> {
        let mut stmt = self.0.prepare_cached(&format!(
            "SELECT * FROM (SELECT {EVENT_COLUMNS} FROM events ORDER BY id DESC LIMIT ?1) ORDER BY id"
        ))?;
        stmt.query_map([limit], event_from_row)?.collect()
    }

    pub(crate) fn last_event_id(&self) -> rusqlite::Result<i64> {
        self.0
            .query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |r| r.get(0))
    }

    pub(crate) fn event(&self, id: i64) -> rusqlite::Result<Option<EventRecord>> {
        self.0
            .query_row(
                &format!("SELECT {EVENT_COLUMNS} FROM events WHERE id = ?1"),
                [id],
                event_from_row,
            )
            .optional()
    }

    /// Deliveries not yet attempted, oldest first.
    pub(crate) fn waiting_deliveries(&self, limit: u32) -> rusqlite::Result<Vec<(i64, String)>> {
        let mut stmt = self.0.prepare_cached(
            "SELECT event_id, destination FROM event_deliveries WHERE state = 'waiting' \
             ORDER BY event_id LIMIT ?1",
        )?;
        stmt.query_map([limit], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect()
    }

    /// Delivery states of one event: (destination, state, detail).
    #[cfg(test)]
    pub(crate) fn deliveries(
        &self,
        event_id: i64,
    ) -> rusqlite::Result<Vec<(String, String, Option<String>)>> {
        let mut stmt = self.0.prepare_cached(
            "SELECT destination, state, detail FROM event_deliveries WHERE event_id = ?1 ORDER BY destination",
        )?;
        stmt.query_map([event_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect()
    }

    pub(crate) fn jobs(&self) -> rusqlite::Result<Vec<JobRow>> {
        let mut stmt = self
            .0
            .prepare_cached(&format!("SELECT {JOB_COLUMNS} FROM jobs ORDER BY name"))?;
        stmt.query_map([], job_from_row)?.collect()
    }

    pub(crate) fn job(&self, name: &str) -> rusqlite::Result<Option<JobRow>> {
        self.0
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE name = ?1"),
                [name],
                job_from_row,
            )
            .optional()
    }

    /// The account a relay's intake state is bound to (Slack: `<workspace>/<account>`).
    pub(crate) fn binding(&self, relay: &str) -> rusqlite::Result<Option<String>> {
        self.0
            .query_row(
                "SELECT account FROM bindings WHERE relay = ?1",
                [relay],
                |r| r.get(0),
            )
            .optional()
    }

    /// Transition memory of one job: (subject, condition) to value.
    pub(crate) fn memory(
        &self,
        job: &str,
        dry: bool,
    ) -> rusqlite::Result<Vec<(String, String, String)>> {
        let mut stmt = self.0.prepare_cached(
            "SELECT subject, condition, value FROM job_memory WHERE job = ?1 AND dry = ?2 ORDER BY subject, condition",
        )?;
        stmt.query_map(params![job, dry], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect()
    }

    /// The newest run of every job.
    pub(crate) fn last_runs(&self) -> rusqlite::Result<Vec<RunRecord>> {
        let mut stmt = self.0.prepare_cached(&format!(
            "SELECT {RUN_COLUMNS} FROM job_runs WHERE id IN (SELECT MAX(id) FROM job_runs GROUP BY job) ORDER BY job"
        ))?;
        stmt.query_map([], run_from_row)?.collect()
    }

    /// Earlier runs of a job that ended badly and still have output on disk.
    pub(crate) fn kept_bad_runs(
        &self,
        job: &str,
        dry: bool,
        before: i64,
    ) -> rusqlite::Result<Vec<RunRecord>> {
        let mut stmt = self.0.prepare_cached(&format!(
            "SELECT {RUN_COLUMNS} FROM job_runs WHERE job = ?1 AND dry = ?2 AND id < ?3 AND kept = 1 \
             AND outcome IS NOT NULL AND outcome NOT IN ('succeeded', 'fired') ORDER BY id"
        ))?;
        stmt.query_map(params![job, dry, before], run_from_row)?
            .collect()
    }
}
