//! The single writer: one thread owns the only read-write connection, takes typed requests, runs the
//! ones that arrived together in one transaction (each in its own savepoint), and answers only after
//! the commit.

use std::time::Instant;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use tokio::sync::{mpsc, oneshot};

use super::read::{RUN_COLUMNS, run_from_row};
use super::{
    Apply, Delivery, EventRecord, JobPatch, JobRegistration, JobRow, MemoryWrite, NewEvent,
    Recovery, RunEnd, RunStart, StoreError,
};
use crate::sys::latency::Latency;

/// Requests waiting for the writer.
const QUEUE: usize = 256;
/// Most requests folded into one transaction.
const BATCH: usize = 64;
/// Events kept: everything from the last 7 days, and at least the newest 1,000.
const EVENT_KEEP_MS: i64 = 7 * 24 * 3600 * 1000;
const EVENT_KEEP_MIN: i64 = 1000;
/// Finished job runs kept for 30 days; the newest run of each job is always kept.
const RUN_KEEP_MS: i64 = 30 * 24 * 3600 * 1000;

pub(crate) type Reply<T> = oneshot::Sender<Result<T, StoreError>>;

/// Every mutation the store accepts.
pub(crate) enum Request {
    AppendEvents(Vec<NewEvent>, Reply<Vec<EventRecord>>),
    RegisterJob(JobRegistration, Reply<JobRow>),
    PatchJob(String, JobPatch, Reply<Option<JobRow>>),
    RunStarted(RunStart, Reply<i64>),
    RunPid(i64, i64, Reply<()>),
    RunEnded(RunEnd, Reply<()>),
    Apply(Apply, Reply<()>),
    Delivery {
        event_id: i64,
        destination: String,
        state: Delivery,
        detail: Option<String>,
        reply: Reply<()>,
    },
    DiscardDry(String, Reply<()>),
    Recover(i64, Reply<Recovery>),
    Maintain(i64, Reply<Vec<String>>),
    Host(Box<super::host::HostWrite>, Reply<super::host::HostDone>),
    Relay(
        Box<super::relay::RelayWrite>,
        Reply<super::relay::RelayDone>,
    ),
}

type Finisher = Box<dyn FnOnce(Option<&StoreError>) + Send>;

pub(crate) fn start(
    conn: Connection,
    latency: Latency,
) -> Result<(mpsc::Sender<Request>, super::WriterHandle), StoreError> {
    let (tx, rx) = mpsc::channel(QUEUE);
    let handle = std::thread::Builder::new()
        .name("cataly-store".into())
        .spawn(move || run(conn, rx, &latency))
        .map_err(|e| StoreError::Io(format!("writer thread: {e}")))?;
    Ok((tx, handle))
}

fn run(
    mut conn: Connection,
    mut rx: mpsc::Receiver<Request>,
    latency: &Latency,
) -> Result<(), StoreError> {
    let mut batch = Vec::with_capacity(BATCH);
    while let Some(first) = rx.blocking_recv() {
        batch.push(first);
        while batch.len() < BATCH {
            match rx.try_recv() {
                Ok(next) => batch.push(next),
                Err(_) => break,
            }
        }
        let started = Instant::now();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let finishers: Vec<Finisher> = batch.drain(..).map(|req| execute(&tx, req)).collect();
        let committed = tx.commit().map_err(StoreError::from);
        latency.record(started.elapsed());
        let failure = committed.as_ref().err();
        for finish in finishers {
            finish(failure);
        }
        committed?;
    }
    Ok(())
}

fn execute(tx: &Transaction<'_>, req: Request) -> Finisher {
    match req {
        Request::AppendEvents(events, r) => step(tx, r, |t| append_events(t, events)),
        Request::Host(write, r) => step(tx, r, |t| {
            super::host::write(t, &write, crate::sys::now_ms())
        }),
        Request::Relay(write, r) => step(tx, r, |t| {
            super::relay::write(t, &write, crate::sys::now_ms())
        }),
        Request::RegisterJob(reg, r) => step(tx, r, |t| register_job(t, &reg)),
        Request::PatchJob(name, patch, r) => step(tx, r, |t| patch_job(t, &name, &patch)),
        Request::RunStarted(start, r) => step(tx, r, |t| run_started(t, &start)),
        Request::RunPid(run, pid, r) => step(tx, r, |t| {
            t.execute(
                "UPDATE job_runs SET pid = ?2 WHERE id = ?1",
                params![run, pid],
            )
            .map(drop)
        }),
        Request::RunEnded(end, r) => step(tx, r, |t| run_ended(t, &end)),
        Request::Apply(apply, r) => step(tx, r, |t| apply_state(t, &apply)),
        Request::Delivery {
            event_id,
            destination,
            state,
            detail,
            reply,
        } => step(tx, reply, |t| {
            t.execute(
                "UPDATE event_deliveries SET state = ?3, detail = ?4, updated_ms = ?5 \
                 WHERE event_id = ?1 AND destination = ?2",
                params![
                    event_id,
                    destination,
                    state.as_str(),
                    detail,
                    crate::sys::now_ms()
                ],
            )
            .map(drop)
        }),
        Request::DiscardDry(job, r) => step(tx, r, |t| {
            t.execute("DELETE FROM job_memory WHERE job = ?1 AND dry = 1", [job])
                .map(drop)
        }),
        Request::Recover(now, r) => step(tx, r, |t| recover(t, now)),
        Request::Maintain(now, r) => step(tx, r, |t| maintain(t, now)),
    }
}

/// Runs one request inside a savepoint; its reply waits for the batch commit.
fn step<T: Send + 'static>(
    tx: &Transaction<'_>,
    reply: Reply<T>,
    f: impl FnOnce(&Transaction<'_>) -> rusqlite::Result<T>,
) -> Finisher {
    let result = tx
        .execute_batch("SAVEPOINT request")
        .and_then(|()| match f(tx) {
            Ok(value) => tx.execute_batch("RELEASE request").map(|()| value),
            Err(e) => {
                let _ = tx.execute_batch("ROLLBACK TO request; RELEASE request");
                Err(e)
            }
        });
    Box::new(move |commit_error| {
        let answer = match (result, commit_error) {
            (Err(e), _) => Err(StoreError::from(e)),
            (Ok(_), Some(e)) => Err(e.clone()),
            (Ok(value), None) => Ok(value),
        };
        let _ = reply.send(answer);
    })
}

fn append_events(
    tx: &Transaction<'_>,
    events: Vec<NewEvent>,
) -> rusqlite::Result<Vec<EventRecord>> {
    let mut insert = tx.prepare_cached(
        "INSERT INTO events (at_ms, job, subject, kind, severity, text, body, body_path, urgent, dry) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?;
    let mut route = tx.prepare_cached(
        "INSERT INTO event_deliveries (event_id, destination, state, updated_ms) VALUES (?1, ?2, 'waiting', ?3)",
    )?;
    let mut out = Vec::with_capacity(events.len());
    for e in events {
        insert.execute(params![
            e.at_ms,
            e.job,
            e.subject,
            e.kind,
            e.severity,
            e.text,
            e.body,
            e.body_path,
            e.urgent,
            e.dry
        ])?;
        let id = tx.last_insert_rowid();
        for dest in &e.destinations {
            route.execute(params![id, dest, e.at_ms])?;
        }
        out.push(EventRecord {
            id,
            at_ms: e.at_ms,
            job: e.job,
            subject: e.subject,
            kind: e.kind.to_string(),
            severity: e.severity.to_string(),
            text: e.text,
            body: e.body,
            body_path: e.body_path,
            lane: None,
            thread: None,
            urgent: e.urgent,
            dry: e.dry,
        });
    }
    Ok(out)
}

fn job_row(tx: &Transaction<'_>, name: &str) -> rusqlite::Result<Option<JobRow>> {
    tx.query_row(
        &format!(
            "SELECT {} FROM jobs WHERE name = ?1",
            super::read::JOB_COLUMNS
        ),
        [name],
        super::read::job_from_row,
    )
    .optional()
}

fn register_job(tx: &Transaction<'_>, reg: &JobRegistration) -> rusqlite::Result<JobRow> {
    match job_row(tx, &reg.name)? {
        Some(row) if row.trigger == reg.trigger && row.kind == reg.kind => Ok(row),
        Some(_) => {
            // A changed trigger starts the schedule afresh; one-shot consumption and holds stay.
            tx.execute(
                "UPDATE jobs SET kind = ?2, trigger = ?3, next_due_ms = ?4, consumed = 0 WHERE name = ?1",
                params![reg.name, reg.kind, reg.trigger, reg.next_due_ms],
            )?;
            job_row(tx, &reg.name)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
        }
        None => {
            tx.execute(
                "INSERT INTO jobs (name, kind, trigger, next_due_ms, created_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![reg.name, reg.kind, reg.trigger, reg.next_due_ms, reg.now_ms],
            )?;
            job_row(tx, &reg.name)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
        }
    }
}

fn patch_job(tx: &Transaction<'_>, name: &str, p: &JobPatch) -> rusqlite::Result<Option<JobRow>> {
    let Some(row) = job_row(tx, name)? else {
        return Ok(None);
    };
    tx.execute(
        "UPDATE jobs SET held = ?2, consumed = ?3, needs_ack = ?4, next_due_ms = ?5, skipped = skipped + ?6 \
         WHERE name = ?1",
        params![
            name,
            p.held.unwrap_or(row.held),
            p.consumed.unwrap_or(row.consumed),
            p.needs_ack.unwrap_or(row.needs_ack),
            p.next_due_ms.unwrap_or(row.next_due_ms),
            p.add_skipped,
        ],
    )?;
    job_row(tx, name)
}

fn run_started(tx: &Transaction<'_>, s: &RunStart) -> rusqlite::Result<i64> {
    tx.execute(
        "INSERT INTO job_runs (job, dry, scheduled_ms, started_ms, state, stdout_path, stderr_path) \
         VALUES (?1, ?2, ?3, ?4, 'running', ?5, ?6)",
        params![s.job, s.dry, s.scheduled_ms, s.started_ms, s.stdout_path, s.stderr_path],
    )?;
    let id = tx.last_insert_rowid();
    if s.consume {
        tx.execute(
            "UPDATE jobs SET consumed = 1, held = 1, next_due_ms = NULL WHERE name = ?1",
            [&s.job],
        )?;
    }
    Ok(id)
}

fn run_ended(tx: &Transaction<'_>, e: &RunEnd) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE job_runs SET ended_ms = ?2, exit_code = ?3, signal = ?4, timed_out = ?5, bytes = ?6, \
         output_hash = ?7, outcome = ?8, state = ?9 WHERE id = ?1",
        params![
            e.run,
            e.ended_ms,
            e.exit_code,
            e.signal,
            e.timed_out,
            e.bytes,
            e.output_hash,
            e.outcome,
            if e.broken { "broken" } else { "finished" },
        ],
    )?;
    if e.broken {
        tx.execute(
            "UPDATE jobs SET held = 1, needs_ack = 1 WHERE name = (SELECT job FROM job_runs WHERE id = ?1) \
             AND (SELECT dry FROM job_runs WHERE id = ?1) = 0",
            [e.run],
        )?;
    }
    Ok(())
}

fn write_memory(tx: &Transaction<'_>, writes: &[MemoryWrite]) -> rusqlite::Result<()> {
    let now = crate::sys::now_ms();
    for w in writes {
        match &w.value {
            Some(value) => tx.execute(
                "INSERT INTO job_memory (job, subject, condition, dry, value, updated_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT (job, subject, condition, dry) \
                 DO UPDATE SET value = excluded.value, updated_ms = excluded.updated_ms",
                params![w.job, w.subject, w.condition, w.dry, value, now],
            )?,
            None => tx.execute(
                "DELETE FROM job_memory WHERE job = ?1 AND subject = ?2 AND condition = ?3 AND dry = ?4",
                params![w.job, w.subject, w.condition, w.dry],
            )?,
        };
    }
    Ok(())
}

fn apply_state(tx: &Transaction<'_>, a: &Apply) -> rusqlite::Result<()> {
    write_memory(tx, &a.memory)?;
    if let Some(run) = a.run {
        tx.execute(
            "UPDATE job_runs SET state = 'applied', outcome = COALESCE(?2, outcome) WHERE id = ?1",
            params![run, a.outcome],
        )?;
    }
    for run in &a.outputs_dropped {
        tx.execute("UPDATE job_runs SET kept = 0 WHERE id = ?1", [run])?;
    }
    Ok(())
}

fn recover(tx: &Transaction<'_>, now: i64) -> rusqlite::Result<Recovery> {
    let select = |state: &str| -> rusqlite::Result<Vec<super::RunRecord>> {
        let mut stmt = tx.prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM job_runs WHERE state = ?1 ORDER BY id"
        ))?;
        stmt.query_map([state], run_from_row)?.collect()
    };
    let broken = select("running")?;
    for run in &broken {
        tx.execute(
            "UPDATE job_runs SET state = 'broken', ended_ms = COALESCE(ended_ms, ?2), \
             outcome = COALESCE(outcome, 'broken off') WHERE id = ?1",
            params![run.id, now],
        )?;
        if !run.dry {
            tx.execute(
                "UPDATE jobs SET held = 1, needs_ack = 1 WHERE name = ?1",
                [&run.job],
            )?;
        }
    }
    let unapplied = select("finished")?;
    let uncertain_deliveries = tx.execute(
        "UPDATE event_deliveries SET state = 'uncertain', detail = 'hub stopped during the attempt', \
         updated_ms = ?1 WHERE state = 'attempting'",
        [now],
    )?;
    Ok(Recovery {
        broken,
        unapplied,
        uncertain_deliveries,
    })
}

fn maintain(tx: &Transaction<'_>, now: i64) -> rusqlite::Result<Vec<String>> {
    super::host::maintain(tx, now)?;
    super::relay::maintain(tx, now)?;
    tx.execute(
        "DELETE FROM events WHERE at_ms < ?1 AND id <= (SELECT COALESCE(MAX(id), 0) FROM events) - ?2",
        params![now - EVENT_KEEP_MS, EVENT_KEEP_MIN],
    )?;
    let old = "FROM job_runs AS r WHERE r.state IN ('applied', 'broken') AND r.started_ms < ?1 \
               AND r.id < (SELECT MAX(id) FROM job_runs WHERE job = r.job)";
    let mut files = Vec::new();
    {
        let mut stmt = tx.prepare(&format!("SELECT stdout_path, stderr_path, kept {old}"))?;
        let rows = stmt.query_map([now - RUN_KEEP_MS], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, bool>(2)?,
            ))
        })?;
        for row in rows {
            let (out, err, kept) = row?;
            if kept {
                files.extend(out);
                files.extend(err);
            }
        }
    }
    tx.execute(&format!("DELETE {old}"), [now - RUN_KEEP_MS])?;
    Ok(files)
}
