//! The one bounded subprocess runner (DESIGN 6.2, 9.4; WS req. 59-60): argument vector only, no shell,
//! stdin closed, empty environment plus a short pass-through list, a time limit, an output cap, and
//! TERM then KILL after a grace period. Signals only ever go to the runner's own unreaped child.

use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ring::digest;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::watch;
use tokio::time::{Instant, sleep_until, timeout};

use super::ErrorClass;
use super::fs::{FsError, create_private};

/// Environment variables a child inherits from the hub; everything else starts empty.
const PASS_ENV: [&str; 8] = [
    "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "TMPDIR", "TZ",
];
/// How long a child's pipes may stay open after it exited (a detached grandchild may hold them).
const PIPE_DRAIN: Duration = Duration::from_secs(2);
/// Grace between TERM and KILL.
pub(crate) const KILL_GRACE: Duration = Duration::from_secs(5);

/// Where the child's output goes.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Output<'a> {
    /// Kept in memory, up to the cap per stream.
    Capture,
    /// Written to new owner-only files; `stderr: None` merges stderr into the stdout file.
    Files {
        stdout: &'a Path,
        stderr: Option<&'a Path>,
    },
}

/// One bounded command.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Spec<'a> {
    pub(crate) argv: &'a [String],
    pub(crate) cwd: Option<&'a Path>,
    pub(crate) env: &'a [(&'a str, &'a str)],
    pub(crate) timeout: Duration,
    pub(crate) kill_grace: Duration,
    /// Byte cap per output (per file, or per captured stream).
    pub(crate) output_cap: u64,
    pub(crate) output: Output<'a>,
}

/// How the child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exit {
    Code(i32),
    Signal(i32),
    /// The child could not be reaped after KILL.
    Unknown,
}

/// The result of a finished command. The flags are independent facts about one run.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub(crate) struct Finished {
    pub(crate) exit: Exit,
    pub(crate) timed_out: bool,
    pub(crate) cancelled: bool,
    /// Captured stdout (capture mode only), at most the cap.
    pub(crate) stdout: Vec<u8>,
    /// Captured stderr (capture mode only), at most the cap.
    pub(crate) stderr: Vec<u8>,
    /// Bytes the child wrote to stdout (merged mode: both streams), including any beyond the cap.
    pub(crate) stdout_bytes: u64,
    /// SHA-256 of exactly the stdout bytes, absent when stdout went over the cap.
    pub(crate) stdout_sha256: Option<[u8; 32]>,
    /// Some output went over the cap and was dropped.
    pub(crate) truncated: bool,
    /// The child exited but something it left behind kept its output open.
    pub(crate) pipes_held: bool,
}

impl Finished {
    pub(crate) fn success(&self) -> bool {
        self.exit == Exit::Code(0) && !self.timed_out && !self.cancelled
    }
}

/// A command could not be started.
#[derive(Debug)]
pub(crate) enum CommandError {
    EmptyArgv,
    Output(FsError),
    Spawn {
        program: String,
        source: std::io::Error,
    },
}

impl CommandError {
    pub(crate) fn class(&self) -> ErrorClass {
        match self {
            Self::EmptyArgv | Self::Spawn { .. } => ErrorClass::Rejected,
            Self::Output(e) => e.class(),
        }
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyArgv => f.write_str("empty argument vector"),
            Self::Output(e) => write!(f, "output file: {e}"),
            Self::Spawn { program, source } => write!(f, "cannot start {program}: {source}"),
        }
    }
}

impl std::error::Error for CommandError {}

/// A started child with its output readers.
pub(crate) struct Running {
    child: Child,
    pid: Option<u32>,
    deadline: Instant,
    kill_grace: Duration,
    stdout: tokio::task::JoinHandle<Sink>,
    stderr: tokio::task::JoinHandle<Sink>,
}

/// Starts the command. The caller may record [`Running::pid`] before awaiting [`Running::wait`].
pub(crate) fn spawn(spec: &Spec<'_>) -> Result<Running, CommandError> {
    let (out_sink, err_sink) = sinks(spec)?;
    let (mut cmd, program) = command(spec.argv, spec.cwd, spec.env)?;
    cmd.stdin(Stdio::null());
    let mut child = cmd.spawn().map_err(|source| CommandError::Spawn {
        program: program.to_string(),
        source,
    })?;
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    Ok(Running {
        pid,
        deadline: Instant::now() + spec.timeout,
        kill_grace: spec.kill_grace,
        stdout: tokio::spawn(pump(stdout, out_sink)),
        stderr: tokio::spawn(pump(stderr, err_sink)),
        child,
    })
}

/// The one place a child command is built: argument vector, empty environment plus the pass-through
/// list and the caller's variables, stdout and stderr piped, killed when dropped.
fn command<'a>(
    argv: &'a [String],
    cwd: Option<&Path>,
    env: &[(&str, &str)],
) -> Result<(Command, &'a str), CommandError> {
    let (program, rest) = argv.split_first().ok_or(CommandError::EmptyArgv)?;
    let mut cmd = Command::new(program);
    cmd.args(rest)
        .env_clear()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for key in PASS_ENV {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    for (key, value) in env {
        cmd.env(key, OsStr::new(value));
    }
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    Ok((cmd, program))
}

/// Spawns and waits in one call, with no cancellation.
pub(crate) async fn run(spec: &Spec<'_>) -> Result<Finished, CommandError> {
    Ok(spawn(spec)?.wait(None).await)
}

/// A child the caller talks to over its stdin and stdout (OMP in rpc-ui mode). Its stderr goes to an
/// owner-only file capped at `stderr_cap` bytes, or is discarded.
pub(crate) struct Attached {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: tokio::task::JoinHandle<Sink>,
}

/// Starts an attached child. An existing `stderr_file` is replaced.
pub(crate) fn attach(
    argv: &[String],
    cwd: Option<&Path>,
    env: &[(&str, &str)],
    stderr_file: Option<&Path>,
    stderr_cap: u64,
) -> Result<Attached, CommandError> {
    let file = match stderr_file {
        Some(path) => {
            let _ = std::fs::remove_file(path);
            Some(create_private(path).map_err(CommandError::Output)?)
        }
        None => None,
    };
    let sink = Sink {
        file,
        buffer: Vec::new(),
        capture: false,
        hash: None,
        cap: stderr_cap,
        counter: Arc::default(),
        write_failed: false,
    };
    let (mut cmd, program) = command(argv, cwd, env)?;
    cmd.stdin(Stdio::piped());
    let mut child = cmd.spawn().map_err(|source| CommandError::Spawn {
        program: program.to_string(),
        source,
    })?;
    let stderr = child.stderr.take();
    Ok(Attached {
        stdin: child.stdin.take(),
        stdout: child.stdout.take(),
        stderr: tokio::spawn(pump(stderr, sink)),
        child,
    })
}

impl Attached {
    pub(crate) fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.stdin.take()
    }

    pub(crate) fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }

    /// Closes stdin (the child's signal to finish), waits up to `wait`, then sends TERM and KILL after
    /// `grace` (DESIGN 4.2). The caller keeps reading stdout meanwhile, so a full pipe cannot block
    /// the exit.
    pub(crate) async fn close(mut self, wait: Duration, grace: Duration) -> Exit {
        drop(self.stdin.take());
        drop(self.stdout.take());
        let status = match timeout(wait, self.child.wait()).await {
            Ok(Ok(status)) => Some(status),
            _ => terminate(&mut self.child, grace).await,
        };
        let mut held = false;
        let _ = join_sink(&mut self.stderr, Instant::now() + PIPE_DRAIN, &mut held).await;
        status.map_or(Exit::Unknown, exit_of)
    }
}

enum Ended {
    Exited(Option<ExitStatus>),
    TimedOut,
    Cancelled,
}

impl Running {
    pub(crate) fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Waits for the child; on the time limit or cancellation sends TERM, then KILL after the grace.
    pub(crate) async fn wait(mut self, cancel: Option<watch::Receiver<bool>>) -> Finished {
        let ended = tokio::select! {
            status = self.child.wait() => Ended::Exited(status.ok()),
            () = sleep_until(self.deadline) => Ended::TimedOut,
            () = cancelled(cancel) => Ended::Cancelled,
        };
        let (status, timed_out, was_cancelled) = match ended {
            Ended::Exited(status) => (status, false, false),
            Ended::TimedOut => (
                terminate(&mut self.child, self.kill_grace).await,
                true,
                false,
            ),
            Ended::Cancelled => (
                terminate(&mut self.child, self.kill_grace).await,
                false,
                true,
            ),
        };
        let exit = status.map_or(Exit::Unknown, exit_of);
        let drain_until = Instant::now() + PIPE_DRAIN;
        let mut pipes_held = false;
        let mut out = join_sink(&mut self.stdout, drain_until, &mut pipes_held).await;
        let err = join_sink(&mut self.stderr, drain_until, &mut pipes_held).await;
        let truncated = out.over_cap() || err.over_cap();
        let stdout_sha256 = if out.over_cap() {
            None
        } else {
            out.hash.take().map(finish_hash)
        };
        Finished {
            exit,
            timed_out,
            cancelled: was_cancelled,
            stdout: std::mem::take(&mut out.buffer),
            stderr: err.buffer,
            stdout_bytes: out.counter.seen.load(Ordering::Relaxed),
            stdout_sha256,
            truncated,
            pipes_held,
        }
    }
}

async fn cancelled(cancel: Option<watch::Receiver<bool>>) {
    let Some(mut rx) = cancel else {
        return std::future::pending().await;
    };
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            return std::future::pending().await;
        }
    }
}

async fn terminate(child: &mut Child, grace: Duration) -> Option<ExitStatus> {
    // `id()` is `None` once the child was reaped, so a reused PID is never signalled.
    if let Some(pid) = child
        .id()
        .and_then(|p| i32::try_from(p).ok())
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
    }
    if let Ok(Ok(status)) = timeout(grace, child.wait()).await {
        return Some(status);
    }
    let _ = child.start_kill();
    timeout(grace, child.wait()).await.ok().and_then(Result::ok)
}

fn exit_of(status: ExitStatus) -> Exit {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => Exit::Code(code),
        (None, Some(signal)) => Exit::Signal(signal),
        (None, None) => Exit::Unknown,
    }
}

async fn join_sink(
    handle: &mut tokio::task::JoinHandle<Sink>,
    until: Instant,
    held: &mut bool,
) -> Sink {
    match tokio::time::timeout_at(until, &mut *handle).await {
        Ok(Ok(sink)) => sink,
        Ok(Err(_)) => Sink::empty(),
        Err(_) => {
            handle.abort();
            *held = true;
            Sink::empty()
        }
    }
}

fn finish_hash(ctx: digest::Context) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(ctx.finish().as_ref());
    out
}

/// Shared byte accounting for one destination (two streams share it when merged).
#[derive(Debug, Default)]
struct Counter {
    seen: AtomicU64,
    kept: AtomicU64,
}

struct Sink {
    file: Option<File>,
    buffer: Vec<u8>,
    capture: bool,
    hash: Option<digest::Context>,
    cap: u64,
    counter: Arc<Counter>,
    write_failed: bool,
}

impl Sink {
    fn empty() -> Self {
        Self {
            file: None,
            buffer: Vec::new(),
            capture: false,
            hash: None,
            cap: 0,
            counter: Arc::default(),
            write_failed: false,
        }
    }

    fn over_cap(&self) -> bool {
        self.write_failed || self.counter.seen.load(Ordering::Relaxed) > self.cap
    }

    fn take(&mut self, chunk: &[u8]) {
        let len = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
        self.counter.seen.fetch_add(len, Ordering::Relaxed);
        let before = self.counter.kept.fetch_add(len, Ordering::Relaxed);
        let room = self.cap.saturating_sub(before);
        let keep = usize::try_from(room.min(len)).unwrap_or(0);
        if keep < chunk.len() {
            self.counter.kept.store(self.cap, Ordering::Relaxed);
        }
        let Some(kept) = chunk.get(..keep) else {
            return;
        };
        if kept.is_empty() {
            return;
        }
        if let Some(hash) = self.hash.as_mut() {
            hash.update(kept);
        }
        if self.capture {
            self.buffer.extend_from_slice(kept);
        }
        if let Some(file) = self.file.as_mut()
            && file.write_all(kept).is_err()
        {
            self.write_failed = true;
            self.file = None;
        }
    }
}

fn sinks(spec: &Spec<'_>) -> Result<(Sink, Sink), CommandError> {
    let make = |file: Option<File>, capture: bool, hash: bool, counter: Arc<Counter>| Sink {
        file,
        buffer: Vec::new(),
        capture,
        hash: hash.then(|| digest::Context::new(&digest::SHA256)),
        cap: spec.output_cap,
        counter,
        write_failed: false,
    };
    match spec.output {
        Output::Capture => Ok((
            make(None, true, true, Arc::default()),
            make(None, true, false, Arc::default()),
        )),
        Output::Files {
            stdout,
            stderr: None,
        } => {
            let file = create_private(stdout).map_err(CommandError::Output)?;
            let second = file.try_clone().map_err(|source| {
                CommandError::Output(FsError::Io {
                    path: PathBuf::from(stdout),
                    source,
                })
            })?;
            let counter = Arc::<Counter>::default();
            Ok((
                make(Some(file), false, true, Arc::clone(&counter)),
                make(Some(second), false, false, counter),
            ))
        }
        Output::Files {
            stdout,
            stderr: Some(stderr),
        } => Ok((
            make(
                Some(create_private(stdout).map_err(CommandError::Output)?),
                false,
                true,
                Arc::default(),
            ),
            make(
                Some(create_private(stderr).map_err(CommandError::Output)?),
                false,
                false,
                Arc::default(),
            ),
        )),
    }
}

async fn pump<R: AsyncRead + Unpin>(reader: Option<R>, mut sink: Sink) -> Sink {
    let Some(mut reader) = reader else {
        return sink;
    };
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if let Some(chunk) = buf.get(..n) {
                    sink.take(chunk);
                }
            }
        }
    }
    if let Some(file) = sink.file.as_mut()
        && file.flush().and_then(|()| file.sync_data()).is_err()
    {
        sink.write_failed = true;
    }
    sink
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::fs::tests::scratch;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_string()).collect()
    }

    fn spec(argv: &[String], timeout: Duration) -> Spec<'_> {
        Spec {
            argv,
            cwd: None,
            env: &[],
            timeout,
            kill_grace: Duration::from_millis(500),
            output_cap: 1024,
            output: Output::Capture,
        }
    }

    #[tokio::test]
    async fn captures_output_and_exit_code() {
        let a = argv(&["sh", "-c", "printf hello; printf oops >&2; exit 3"]);
        let done = run(&spec(&a, Duration::from_secs(10))).await.unwrap();
        assert_eq!(done.exit, Exit::Code(3));
        assert_eq!(done.stdout, b"hello");
        assert_eq!(done.stderr, b"oops");
        assert!(!done.timed_out && !done.truncated);
        let expected = digest::digest(&digest::SHA256, b"hello");
        assert_eq!(done.stdout_sha256.unwrap().as_slice(), expected.as_ref());
    }

    #[tokio::test]
    async fn environment_starts_empty() {
        let a = argv(&["sh", "-c", "printf %s \"${CATALY_SECRET_TEST:-unset}:$X\""]);
        let mut s = spec(&a, Duration::from_secs(10));
        s.env = &[("X", "y")];
        // The parent's variable must not reach the child.
        let done = run(&s).await.unwrap();
        assert_eq!(done.stdout, b"unset:y");
    }

    #[tokio::test]
    async fn timeout_sends_term() {
        let a = argv(&["sleep", "30"]);
        let start = std::time::Instant::now();
        let done = run(&spec(&a, Duration::from_millis(200))).await.unwrap();
        assert!(done.timed_out);
        assert_eq!(done.exit, Exit::Signal(15));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn term_ignored_then_kill() {
        let a = argv(&["sh", "-c", "trap '' TERM; while :; do sleep 0.05; done"]);
        let start = std::time::Instant::now();
        let done = run(&spec(&a, Duration::from_millis(200))).await.unwrap();
        assert!(done.timed_out);
        assert_eq!(done.exit, Exit::Signal(9));
        assert!(
            start.elapsed() >= Duration::from_millis(650),
            "KILL only after the grace"
        );
    }

    #[tokio::test]
    async fn cancellation_stops_the_child() {
        let a = argv(&["sleep", "30"]);
        let (tx, rx) = watch::channel(false);
        let running = spawn(&spec(&a, Duration::from_secs(30))).unwrap();
        assert!(running.pid().is_some());
        let waiter = tokio::spawn(running.wait(Some(rx)));
        tokio::time::sleep(Duration::from_millis(100)).await;
        tx.send(true).unwrap();
        let done = waiter.await.unwrap();
        assert!(done.cancelled && !done.success());
    }

    #[tokio::test]
    async fn output_cap_truncates_and_drops_hash() {
        let a = argv(&["sh", "-c", "head -c 5000 /dev/zero"]);
        let done = run(&spec(&a, Duration::from_secs(10))).await.unwrap();
        assert_eq!(done.stdout.len(), 1024);
        assert_eq!(done.stdout_bytes, 5000);
        assert!(done.truncated);
        assert!(done.stdout_sha256.is_none());
    }

    #[tokio::test]
    async fn files_are_private_and_merged_or_split() {
        use std::os::unix::fs::MetadataExt;
        let dir = scratch("cmd");
        let a = argv(&["sh", "-c", "printf out; printf err >&2"]);
        let merged = dir.join("merged");
        let mut s = spec(&a, Duration::from_secs(10));
        s.output = Output::Files {
            stdout: &merged,
            stderr: None,
        };
        let done = run(&s).await.unwrap();
        assert_eq!(done.stdout_bytes, 6);
        let text = std::fs::read_to_string(&merged).unwrap();
        assert!(text.contains("out") && text.contains("err"));
        assert_eq!(std::fs::metadata(&merged).unwrap().mode() & 0o777, 0o600);
        let (o, e) = (dir.join("o"), dir.join("e"));
        s.output = Output::Files {
            stdout: &o,
            stderr: Some(&e),
        };
        run(&s).await.unwrap();
        assert_eq!(std::fs::read(&o).unwrap(), b"out");
        assert_eq!(std::fs::read(&e).unwrap(), b"err");
        assert!(
            matches!(run(&s).await, Err(CommandError::Output(_))),
            "never overwrites"
        );
    }

    #[tokio::test]
    async fn spawn_failure_is_an_error() {
        let a = argv(&["/nonexistent/cataly-test-program"]);
        assert!(matches!(
            run(&spec(&a, Duration::from_secs(1))).await,
            Err(CommandError::Spawn { .. })
        ));
        assert!(matches!(
            run(&spec(&[], Duration::from_secs(1))).await,
            Err(CommandError::EmptyArgv)
        ));
    }

    #[tokio::test]
    async fn detached_grandchild_holding_pipes_does_not_block() {
        let a = argv(&["sh", "-c", "sleep 20 & printf done"]);
        let start = std::time::Instant::now();
        let done = run(&spec(&a, Duration::from_secs(10))).await.unwrap();
        assert_eq!(done.exit, Exit::Code(0));
        assert!(done.pipes_held);
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
