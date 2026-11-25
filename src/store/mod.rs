//! The store (DESIGN 7): one SQLite file in WAL mode with `synchronous=FULL`, embedded migrations, a
//! single writer thread that takes typed requests and batches them into one transaction, and a small
//! pool of read-only connections.

pub(crate) mod coord;
pub(crate) mod host;
pub(crate) mod lanes;
pub(crate) mod lead;
mod read;
pub(crate) mod relay;
mod schema;
mod writer;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::sys::ErrorClass;
use crate::sys::latency::Latency;

pub(crate) use read::Reads;
use writer::Request;

/// Read connections kept open.
const READERS: usize = 2;
/// Page cache per connection, in KiB (DESIGN 7.2: 2 MiB).
const CACHE_KIB: i64 = 2048;

/// A store operation failed.
#[derive(Debug, Clone)]
pub(crate) enum StoreError {
    Sql(String),
    Io(String),
    /// The writer thread is gone; the hub must exit.
    WriterGone,
    /// The schema on disk needs a newer binary.
    TooNew {
        found: i64,
        known: i64,
        backup: PathBuf,
    },
}

impl StoreError {
    pub(crate) fn class(&self) -> ErrorClass {
        match self {
            Self::Sql(_) | Self::Io(_) => ErrorClass::Transient,
            Self::WriterGone | Self::TooNew { .. } => ErrorClass::Fatal,
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(e) => write!(f, "store: {e}"),
            Self::Io(e) => write!(f, "store file: {e}"),
            Self::WriterGone => f.write_str("store writer stopped"),
            Self::TooNew {
                found,
                known,
                backup,
            } => write!(
                f,
                "store schema v{found} needs a newer cataly (this one knows v{known}); install the newer \
                 version or restore {}",
                backup.display()
            ),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(e.to_string())
    }
}

/// One stored event, as the stream, the CLI and the event log see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EventRecord {
    pub(crate) id: i64,
    pub(crate) at_ms: i64,
    pub(crate) job: String,
    pub(crate) subject: String,
    pub(crate) kind: String,
    pub(crate) severity: String,
    pub(crate) text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) body_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) lane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) thread: Option<String>,
    pub(crate) urgent: bool,
    pub(crate) dry: bool,
}

/// An event to append, with the destinations it is routed to.
#[derive(Debug, Clone)]
pub(crate) struct NewEvent {
    pub(crate) at_ms: i64,
    pub(crate) job: String,
    pub(crate) subject: String,
    pub(crate) kind: &'static str,
    pub(crate) severity: &'static str,
    pub(crate) text: String,
    pub(crate) body: Option<String>,
    pub(crate) body_path: Option<String>,
    pub(crate) urgent: bool,
    pub(crate) dry: bool,
    pub(crate) destinations: Vec<String>,
}

/// A job's durable scheduling state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobRow {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) trigger: String,
    pub(crate) held: bool,
    pub(crate) consumed: bool,
    pub(crate) needs_ack: bool,
    pub(crate) next_due_ms: Option<i64>,
    pub(crate) skipped: i64,
    pub(crate) created_ms: i64,
}

/// Changes to a job row; `None` leaves a field as it is.
#[derive(Debug, Clone, Default)]
pub(crate) struct JobPatch {
    pub(crate) held: Option<bool>,
    pub(crate) consumed: Option<bool>,
    pub(crate) needs_ack: Option<bool>,
    /// `Some(None)` clears the due time; `None` leaves it.
    #[allow(clippy::option_option)]
    pub(crate) next_due_ms: Option<Option<i64>>,
    pub(crate) add_skipped: i64,
}

/// Registers a configured job; a changed trigger replaces the next due time.
#[derive(Debug, Clone)]
pub(crate) struct JobRegistration {
    pub(crate) name: String,
    pub(crate) kind: &'static str,
    pub(crate) trigger: String,
    pub(crate) next_due_ms: Option<i64>,
    pub(crate) now_ms: i64,
}

/// One run of a job command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunRecord {
    pub(crate) id: i64,
    pub(crate) job: String,
    pub(crate) dry: bool,
    pub(crate) scheduled_ms: i64,
    pub(crate) started_ms: i64,
    pub(crate) pid: Option<i64>,
    pub(crate) ended_ms: Option<i64>,
    pub(crate) exit_code: Option<i64>,
    pub(crate) signal: Option<i64>,
    pub(crate) timed_out: bool,
    pub(crate) state: String,
    pub(crate) outcome: Option<String>,
    pub(crate) stdout_path: Option<String>,
    pub(crate) stderr_path: Option<String>,
    pub(crate) bytes: Option<i64>,
    pub(crate) output_hash: Option<String>,
    pub(crate) kept: bool,
}

/// Durable record written before a run's child is spawned.
#[derive(Debug, Clone)]
pub(crate) struct RunStart {
    pub(crate) job: String,
    pub(crate) dry: bool,
    pub(crate) scheduled_ms: i64,
    pub(crate) started_ms: i64,
    pub(crate) stdout_path: String,
    pub(crate) stderr_path: Option<String>,
    /// One-shot jobs are consumed (held for good) in the same transaction.
    pub(crate) consume: bool,
}

/// How a run's child ended.
#[derive(Debug, Clone)]
pub(crate) struct RunEnd {
    pub(crate) run: i64,
    pub(crate) ended_ms: i64,
    pub(crate) exit_code: Option<i64>,
    pub(crate) signal: Option<i64>,
    pub(crate) timed_out: bool,
    pub(crate) bytes: i64,
    pub(crate) output_hash: Option<String>,
    pub(crate) outcome: String,
    /// The run was broken off (hub stopping); its job goes on hold and needs an acknowledgement.
    pub(crate) broken: bool,
}

/// One transition-memory change; `value: None` deletes the entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemoryWrite {
    pub(crate) job: String,
    pub(crate) subject: String,
    pub(crate) condition: String,
    pub(crate) dry: bool,
    pub(crate) value: Option<String>,
}

/// The state commit that follows a run's events: transition memory, run state and output bookkeeping.
#[derive(Debug, Clone, Default)]
pub(crate) struct Apply {
    /// The run this commit applies; it becomes `applied`.
    pub(crate) run: Option<i64>,
    /// Replaces the run's recorded outcome (a probe success that fired).
    pub(crate) outcome: Option<String>,
    pub(crate) memory: Vec<MemoryWrite>,
    /// Runs whose output files were deleted.
    pub(crate) outputs_dropped: Vec<i64>,
}

/// Delivery states of an event at one destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    Attempting,
    Sent,
    Failed,
    Uncertain,
}

impl Delivery {
    fn as_str(self) -> &'static str {
        match self {
            Self::Attempting => "attempting",
            Self::Sent => "sent",
            Self::Failed => "failed",
            Self::Uncertain => "uncertain",
        }
    }
}

/// What start-up recovery found.
#[derive(Debug, Clone, Default)]
pub(crate) struct Recovery {
    /// Runs that were in progress when the hub stopped; now broken off, their jobs on hold.
    pub(crate) broken: Vec<RunRecord>,
    /// Runs that finished but whose outcome was not applied; applied again, so events may repeat.
    pub(crate) unapplied: Vec<RunRecord>,
    /// Deliveries that were being attempted; now uncertain, never retried.
    pub(crate) uncertain_deliveries: usize,
}

/// Handle to the store: cheap to clone.
#[derive(Clone)]
pub(crate) struct Store {
    tx: mpsc::Sender<Request>,
    readers: Arc<ReaderPool>,
    commit_latency: Latency,
}

struct ReaderPool {
    path: PathBuf,
    idle: Mutex<Vec<Reads>>,
}

/// Joins the writer thread; `Err` means the writer failed and the hub must exit.
pub(crate) type WriterHandle = std::thread::JoinHandle<Result<(), StoreError>>;

impl Store {
    /// Opens (creating when missing, mode 0600) and migrates the store, then starts the writer thread.
    pub(crate) fn open(db: &Path, backup_dir: &Path) -> Result<(Self, WriterHandle), StoreError> {
        prepare_file(db)?;
        let mut conn = Connection::open_with_flags(
            db,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.execute_batch(&format!(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; \
             PRAGMA busy_timeout=0; PRAGMA cache_size=-{CACHE_KIB};"
        ))?;
        schema::migrate(&mut conn, backup_dir)?;
        let commit_latency = Latency::default();
        let (tx, handle) = writer::start(conn, commit_latency.clone())?;
        let readers = Arc::new(ReaderPool {
            path: db.to_path_buf(),
            idle: Mutex::new(Vec::new()),
        });
        Ok((
            Self {
                tx,
                readers,
                commit_latency,
            },
            handle,
        ))
    }

    pub(crate) fn commit_latency(&self) -> &Latency {
        &self.commit_latency
    }

    /// Queues one request in the writer's order; the receiver answers after its commit.
    async fn queue<T>(
        &self,
        make: impl FnOnce(writer::Reply<T>) -> Request,
    ) -> Result<oneshot::Receiver<Result<T, StoreError>>, StoreError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(make(reply))
            .await
            .map_err(|_| StoreError::WriterGone)?;
        Ok(rx)
    }

    async fn call<T>(
        &self,
        make: impl FnOnce(writer::Reply<T>) -> Request,
    ) -> Result<T, StoreError> {
        self.queue(make)
            .await?
            .await
            .map_err(|_| StoreError::WriterGone)?
    }

    pub(crate) async fn append_events(
        &self,
        events: Vec<NewEvent>,
    ) -> Result<Vec<EventRecord>, StoreError> {
        self.call(|r| Request::AppendEvents(events, r)).await
    }

    pub(crate) async fn register_job(&self, reg: JobRegistration) -> Result<JobRow, StoreError> {
        self.call(|r| Request::RegisterJob(reg, r)).await
    }

    pub(crate) async fn patch_job(
        &self,
        name: String,
        patch: JobPatch,
    ) -> Result<Option<JobRow>, StoreError> {
        self.call(|r| Request::PatchJob(name, patch, r)).await
    }

    pub(crate) async fn run_started(&self, start: RunStart) -> Result<i64, StoreError> {
        self.call(|r| Request::RunStarted(start, r)).await
    }

    pub(crate) async fn run_pid(&self, run: i64, pid: i64) -> Result<(), StoreError> {
        self.call(|r| Request::RunPid(run, pid, r)).await
    }

    pub(crate) async fn run_ended(&self, end: RunEnd) -> Result<(), StoreError> {
        self.call(|r| Request::RunEnded(end, r)).await
    }

    pub(crate) async fn apply(&self, apply: Apply) -> Result<(), StoreError> {
        self.call(|r| Request::Apply(apply, r)).await
    }

    pub(crate) async fn delivery(
        &self,
        event_id: i64,
        destination: String,
        state: Delivery,
        detail: Option<String>,
    ) -> Result<(), StoreError> {
        self.call(|r| Request::Delivery {
            event_id,
            destination,
            state,
            detail,
            reply: r,
        })
        .await
    }

    pub(crate) async fn discard_dry(&self, job: String) -> Result<(), StoreError> {
        self.call(|r| Request::DiscardDry(job, r)).await
    }

    pub(crate) async fn recover(&self, now_ms: i64) -> Result<Recovery, StoreError> {
        self.call(|r| Request::Recover(now_ms, r)).await
    }

    /// Applies retention; returns the output files of removed runs so the caller can delete them.
    pub(crate) async fn maintain(&self, now_ms: i64) -> Result<Vec<String>, StoreError> {
        self.call(|r| Request::Maintain(now_ms, r)).await
    }

    /// Runs a read on a pooled read-only connection, off the async workers.
    pub(crate) async fn read<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Reads) -> Result<T, rusqlite::Error> + Send + 'static,
    ) -> Result<T, StoreError> {
        let pool = Arc::clone(&self.readers);
        tokio::task::spawn_blocking(move || {
            let reads = pool.take()?;
            let result = f(&reads);
            pool.give(reads);
            result.map_err(StoreError::from)
        })
        .await
        .map_err(|e| StoreError::Io(format!("read task: {e}")))?
    }

    /// One session-host mutation (sessions, inbox, deliveries, receipts).
    pub(crate) async fn host(&self, write: host::HostWrite) -> Result<host::HostDone, StoreError> {
        self.call(|r| Request::Host(Box::new(write), r)).await
    }

    /// One relay mutation (positions, arrival queue, hand-offs, replies).
    pub(crate) async fn relay(
        &self,
        write: relay::RelayWrite,
    ) -> Result<relay::RelayDone, StoreError> {
        self.call(|r| Request::Relay(Box::new(write), r)).await
    }

    /// One relay mutation queued now, its answer awaited later. The writer applies requests in
    /// the order they were queued, so a write queued after this one never commits before it: a
    /// live arrival's hand-off can start while the arrival commits (decision 15).
    pub(crate) async fn relay_queued(
        &self,
        write: relay::RelayWrite,
    ) -> Result<impl Future<Output = Result<relay::RelayDone, StoreError>> + use<>, StoreError>
    {
        let rx = self.queue(|r| Request::Relay(Box::new(write), r)).await?;
        Ok(async move { rx.await.map_err(|_| StoreError::WriterGone)? })
    }

    /// One lead-record mutation (request log, questions, unanswered, stream jobs, reminders).
    pub(crate) async fn lead(&self, write: lead::LeadWrite) -> Result<lead::LeadDone, StoreError> {
        self.call(|r| Request::Lead(Box::new(write), r)).await
    }

    /// One lane mutation (records, reports, steps, thread journal).
    pub(crate) async fn lanes(
        &self,
        write: lanes::LaneWrite,
    ) -> Result<lanes::LaneDone, StoreError> {
        self.call(|r| Request::Lanes(Box::new(write), r)).await
    }

    /// One coordination-record mutation (queued launches, items, planner, verdicts, files, rules,
    /// board posts).
    pub(crate) async fn coord(
        &self,
        write: coord::CoordWrite,
    ) -> Result<coord::CoordDone, StoreError> {
        self.call(|r| Request::Coord(Box::new(write), r)).await
    }
}

impl ReaderPool {
    fn take(&self) -> Result<Reads, StoreError> {
        if let Some(reads) = self
            .idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop()
        {
            return Ok(reads);
        }
        let conn = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.execute_batch(
            "PRAGMA busy_timeout=250; PRAGMA cache_size=-512; PRAGMA query_only=ON;",
        )?;
        Ok(Reads(conn))
    }

    fn give(&self, conn: Reads) {
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        if idle.len() < READERS {
            idle.push(conn);
        }
    }
}

/// Creates the database file owner-only before SQLite does (its WAL and shared-memory files copy the
/// main file's mode), and refuses an existing file that is not the owner's.
fn prepare_file(db: &Path) -> Result<(), StoreError> {
    match std::fs::symlink_metadata(db) {
        Ok(meta) => {
            crate::sys::fs::check_private_file(db, &meta).map_err(|e| StoreError::Io(e.to_string()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => crate::sys::fs::create_private(db)
            .map(drop)
            .map_err(|e| StoreError::Io(e.to_string())),
        Err(e) => Err(StoreError::Io(format!("{}: {e}", db.display()))),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::sys::fs::tests::{Scratch, scratch};

    pub(crate) fn open_temp(tag: &str) -> (Store, WriterHandle, Scratch) {
        let dir = scratch(tag);
        let (store, handle) = Store::open(&dir.join("cataly.db"), &dir.join("backup")).unwrap();
        (store, handle, dir)
    }

    pub(crate) fn event(job: &str, kind: &'static str, destinations: &[&str]) -> NewEvent {
        NewEvent {
            at_ms: 1,
            job: job.into(),
            subject: job.into(),
            kind,
            severity: "info",
            text: format!("{job} {kind}"),
            body: None,
            body_path: None,
            urgent: false,
            dry: false,
            destinations: destinations.iter().map(|d| (*d).to_string()).collect(),
        }
    }

    #[tokio::test]
    async fn pragmas_and_modes() {
        use std::os::unix::fs::MetadataExt;
        let (store, _h, dir) = open_temp("store-open");
        let mode = store
            .read(|r| {
                r.0.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
            })
            .await
            .unwrap();
        assert_eq!(mode, "wal");
        assert_eq!(
            std::fs::metadata(dir.join("cataly.db")).unwrap().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn batched_writes_and_reads() {
        let (store, _h, _dir) = open_temp("store-batch");
        let writes = (0..20).map(|i| {
            let s = store.clone();
            tokio::spawn(async move {
                s.append_events(vec![event(&format!("j{i}"), "run.done", &["x"])])
                    .await
            })
        });
        for w in writes {
            assert_eq!(w.await.unwrap().unwrap().len(), 1);
        }
        let all = store.read(|r| r.events_after(0, 100)).await.unwrap();
        assert_eq!(all.len(), 20);
        let pending = store.read(|r| r.waiting_deliveries(100)).await.unwrap();
        assert_eq!(pending.len(), 20);
        assert!(store.commit_latency().summary().is_some());
    }

    #[tokio::test]
    async fn failed_request_does_not_spoil_its_batch() {
        let (store, _h, _dir) = open_temp("store-savepoint");
        // A severity outside the CHECK list fails its own request; the append beside it survives.
        let mut broken = event("bad", "run.done", &[]);
        broken.severity = "loud";
        let bad = store.append_events(vec![broken]);
        let good = store.append_events(vec![event("j", "run.done", &[])]);
        let (bad, good) = tokio::join!(bad, good);
        assert!(matches!(bad, Err(StoreError::Sql(_))));
        assert_eq!(good.unwrap().len(), 1);
        assert_eq!(
            store.read(|r| r.events_after(0, 10)).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn writer_ends_when_handles_drop() {
        let (store, handle, _dir) = open_temp("store-drop");
        drop(store);
        assert!(
            tokio::task::spawn_blocking(move || handle.join())
                .await
                .unwrap()
                .unwrap()
                .is_ok()
        );
    }
}
