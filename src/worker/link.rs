//! The hub's side of a worker (design 2.1, 2.2, 2.9): one `ssh` child per worker in use, closed
//! after 5 minutes with no remote lane and no pending operation; the channel-0 client; and the table
//! of every configured worker with its state, as `worker list`, `status` and the panel show it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::{mpsc, oneshot, watch};

use super::enrol::{self, Destination};
use super::mux::{Accepted, Mux, Timing};
use super::{Hello, LINE_MAX, method};
use crate::config::Config;
use crate::config::workers::WorkerSettings;
use crate::log;
use crate::proto::{Line, Request, Response};
use crate::store::Store;
use crate::store::coord::CoordWrite;
use crate::sys::command::{self, Attached, Exit, Finished};
use crate::sys::now_ms;

/// `ssh` connects within this time (`ConnectTimeout`), and `hello` answers within this time.
const CONNECT: Duration = Duration::from_secs(10);
const HELLO: Duration = Duration::from_secs(10);
/// A connection with no remote lane and no pending operation closes after this time (design 2.1).
pub(crate) const IDLE: Duration = Duration::from_mins(5);
/// How often idle connections are looked for.
const SWEEP: Duration = Duration::from_secs(30);
/// `ssh` stderr kept per connection, for the refusal reason.
const STDERR_CAP: u64 = 64 * 1024;
/// The store kind of worker records.
const KIND: &str = "worker";

/// The `ssh` argument vector (design 2.2): no config file, only this key and this pin, no agent, no
/// forwarding, batch mode, keepalives; the command asked for is the marker.
pub(crate) fn ssh_argv(key: &Path, known_hosts: &Path, dest: &Destination) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "ssh".into(),
        "-F".into(),
        "none".into(),
        "-T".into(),
        "-i".into(),
        key.display().to_string(),
        "-p".into(),
        dest.port.to_string(),
    ];
    for option in [
        "IdentitiesOnly=yes".to_string(),
        "IdentityAgent=none".into(),
        format!("UserKnownHostsFile={}", known_hosts.display()),
        "GlobalKnownHostsFile=/dev/null".into(),
        "StrictHostKeyChecking=yes".into(),
        "CheckHostIP=no".into(),
        "UpdateHostKeys=no".into(),
        "BatchMode=yes".into(),
        "ClearAllForwardings=yes".into(),
        format!("ConnectTimeout={}", CONNECT.as_secs()),
        "ServerAliveInterval=15".into(),
        "ServerAliveCountMax=3".into(),
    ] {
        argv.push("-o".into());
        argv.push(option);
    }
    argv.push("--".into());
    argv.push(dest.target());
    argv.push(super::MARKER.into());
    argv
}

/// Why a connection did not come up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConnectError {
    /// The pinned host key does not match, or there is no pin (design 2.9).
    HostKey,
    /// The marker command ran: the forced command is not applied (design 2.2).
    NoForcedCommand,
    /// The worker refused the hub's key.
    Denied,
    /// Speaks another protocol (design 2.8).
    Incompatible(String),
    /// Anything else: the network, the worker's sshd, a timeout.
    Unreachable(String),
}

impl ConnectError {
    /// A refusal stays until the owner acts; the rest may pass on its own.
    pub(crate) fn refused(&self) -> bool {
        !matches!(self, Self::Unreachable(_))
    }
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HostKey => f.write_str(
                "refused: host key changed or not pinned; run `cataly worker init` on the worker \
                 again to see its fingerprint, then `cataly worker add … --host-key SHA256:…` here",
            ),
            Self::NoForcedCommand => f.write_str(
                "refused: no forced command: the worker ran the hub's command itself. When Tailscale \
                 SSH takes port 22 of the tailnet, run OpenSSH on another port bound to the tailnet \
                 address (for example 2222) and enrol user@host:2222",
            ),
            Self::Denied => f.write_str(
                "refused: the worker did not accept this hub's key; run the `cataly worker init` \
                 line `worker add` printed on the worker",
            ),
            Self::Incompatible(why) | Self::Unreachable(why) => f.write_str(why),
        }
    }
}

/// Reads `ssh`'s stderr after a failed connection.
pub(crate) fn classify(stderr: &str, exit: Option<Exit>) -> ConnectError {
    let has = |needle: &str| stderr.contains(needle);
    if has("Host key verification failed")
        || has("REMOTE HOST IDENTIFICATION HAS CHANGED")
        || has("host key is known for")
        || has("No ED25519 host key is known")
    {
        return ConnectError::HostKey;
    }
    if has(super::MARKER) || exit == Some(Exit::Code(127)) {
        return ConnectError::NoForcedCommand;
    }
    if has("Permission denied") {
        return ConnectError::Denied;
    }
    let last = stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map_or_else(
            || "the connection ended before the worker answered".to_string(),
            str::to_string,
        );
    ConnectError::Unreachable(format!("unreachable: {last}"))
}

/// What `exec` returns on the wire: the same facts as a local [`Finished`].
pub(crate) fn exec_reply(done: &Finished) -> Value {
    let exit = match done.exit {
        Exit::Code(c) => json!({ "code": c }),
        Exit::Signal(s) => json!({ "signal": s }),
        Exit::Unknown => Value::Null,
    };
    json!({
        "exit": exit,
        "timed_out": done.timed_out,
        "stdout": crate::sys::base64::encode(&done.stdout),
        "stderr": crate::sys::base64::encode(&done.stderr),
        "stdout_bytes": done.stdout_bytes,
        "stdout_sha256": done.stdout_sha256.map(|h| crate::sys::hex(&h)),
        "truncated": done.truncated,
        "pipes_held": done.pipes_held,
    })
}

/// The hub's copy of a worker's `exec` result.
pub(crate) fn finished_of(value: &Value) -> Result<Finished, String> {
    let bytes = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .and_then(crate::sys::base64::decode)
            .ok_or_else(|| format!("exec reply without {key}"))
    };
    let flag = |key: &str| value.get(key).and_then(Value::as_bool).unwrap_or(false);
    let exit = match value.get("exit") {
        Some(e) if e.get("code").is_some() => Exit::Code(
            e.get("code")
                .and_then(Value::as_i64)
                .and_then(|c| i32::try_from(c).ok())
                .unwrap_or(-1),
        ),
        Some(e) if e.get("signal").is_some() => Exit::Signal(
            e.get("signal")
                .and_then(Value::as_i64)
                .and_then(|c| i32::try_from(c).ok())
                .unwrap_or(0),
        ),
        _ => Exit::Unknown,
    };
    let sha = value
        .get("stdout_sha256")
        .and_then(Value::as_str)
        .and_then(|h| {
            let raw: Vec<u8> = (0..h.len())
                .step_by(2)
                .filter_map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok())
                .collect();
            <[u8; 32]>::try_from(raw).ok()
        });
    Ok(Finished {
        exit,
        timed_out: flag("timed_out"),
        cancelled: false,
        stdout: bytes("stdout")?,
        stderr: bytes("stderr")?,
        stdout_bytes: value
            .get("stdout_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        stdout_sha256: sha,
        truncated: flag("truncated"),
        pipes_held: flag("pipes_held"),
    })
}

/// The channel-0 client: one request per call, answers matched by ID.
pub(crate) struct Client {
    lines: mpsc::Sender<String>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Response>>>>,
    next: AtomicU64,
}

impl Client {
    fn start(stream: DuplexStream) -> Self {
        let (read, mut write) = tokio::io::split(stream);
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Response>>>> = Arc::default();
        let (lines, mut rx) = mpsc::channel::<String>(16);
        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if write.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let answers = Arc::clone(&pending);
        tokio::spawn(async move {
            let mut reader = BufReader::new(read);
            while let Line::Text(line) = crate::proto::read_line(&mut reader, LINE_MAX).await {
                let Ok(reply) = serde_json::from_str::<Response>(&line) else {
                    continue;
                };
                let waiter = answers
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&reply.id);
                if let Some(w) = waiter {
                    let _ = w.send(reply);
                }
            }
            // Every caller still waiting learns the connection ended.
            answers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clear();
        });
        Self {
            lines,
            pending,
            next: AtomicU64::new(1),
        }
    }

    /// One request, bounded by `limit`.
    pub(crate) async fn call(
        &self,
        name: &str,
        params: Value,
        limit: Duration,
    ) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut line = serde_json::to_string(&Request {
            v: crate::proto::VERSION,
            id,
            method: name,
            params,
            launch: None,
        })
        .map_err(|e| e.to_string())?;
        line.push('\n');
        if line.len() > LINE_MAX {
            return Err("request too large".into());
        }
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, tx);
        let ended = || "the worker connection ended".to_string();
        if self.lines.send(line).await.is_err() {
            self.forget(id);
            return Err(ended());
        }
        let reply = match tokio::time::timeout(limit, rx).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => return Err(ended()),
            Err(_) => {
                self.forget(id);
                return Err(format!("{name} took longer than {} s", limit.as_secs()));
            }
        };
        match (reply.ok, reply.err) {
            (Some(v), _) => Ok(v),
            (None, Some(e)) => Err(e.message),
            (None, None) => Err("an empty answer".into()),
        }
    }

    fn forget(&self, id: u64) {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
    }
}

/// What connects to one worker.
#[derive(Debug, Clone)]
pub(crate) struct Target {
    pub(crate) name: String,
    pub(crate) dest: Destination,
    pub(crate) key: PathBuf,
    pub(crate) known_hosts: PathBuf,
    /// `ssh`'s stderr of the latest connection.
    pub(crate) stderr: PathBuf,
}

impl Target {
    pub(crate) fn of(
        name: &str,
        settings: &WorkerSettings,
        config_dir: &Path,
        logs: &Path,
    ) -> Result<Self, String> {
        Ok(Self {
            name: name.to_string(),
            dest: Destination::parse(&format!("{}:{}", settings.destination, settings.port))?,
            key: enrol::key_path(config_dir, name),
            known_hosts: enrol::known_hosts(config_dir),
            stderr: logs.join(format!("worker-{name}.err")),
        })
    }
}

/// One live connection to a worker.
pub(crate) struct Link {
    pub(crate) name: String,
    pub(crate) hello: Hello,
    pub(crate) mux: Mux,
    client: Client,
    child: tokio::sync::Mutex<Option<Attached>>,
    used: Mutex<Instant>,
    /// Operations in flight.
    pending: AtomicUsize,
    /// Remote lanes running on it; a [`LaneHold`] counts one.
    pub(crate) lanes: AtomicUsize,
    /// Channels of spawned OMP children the worker opened, until their `spawn` answer takes them.
    spawned: Mutex<HashMap<u32, DuplexStream>>,
    spawned_ready: tokio::sync::Notify,
}

/// One remote lane's hold on its connection: it is not idle while a lane runs on it.
pub(crate) struct LaneHold(Arc<Link>);

impl LaneHold {
    pub(crate) fn link(&self) -> &Arc<Link> {
        &self.0
    }
}

impl Drop for LaneHold {
    fn drop(&mut self) {
        self.0.lanes.fetch_sub(1, Ordering::AcqRel);
        self.0.touch();
    }
}

impl Link {
    /// Starts `ssh`, the stream and `hello`. The channels the worker opens arrive on the receiver.
    pub(crate) async fn connect(
        target: &Target,
    ) -> Result<(Arc<Self>, mpsc::Receiver<Accepted>), ConnectError> {
        let argv = ssh_argv(&target.key, &target.known_hosts, &target.dest);
        if let Some(dir) = target.stderr.parent() {
            crate::sys::fs::ensure_private_dir(dir)
                .map_err(|e| ConnectError::Unreachable(e.to_string()))?;
        }
        let mut child = command::attach(&argv, None, &[], Some(&target.stderr), STDERR_CAP)
            .map_err(|e| ConnectError::Unreachable(format!("cannot start ssh: {e}")))?;
        let (Some(stdin), Some(stdout)) = (child.take_stdin(), child.take_stdout()) else {
            child.close(Duration::ZERO, Duration::from_secs(1)).await;
            return Err(ConnectError::Unreachable(
                "ssh's pipes are not available".into(),
            ));
        };
        let (mux, control, accepted) = Mux::start(stdout, stdin, Timing::STANDARD);
        let client = Client::start(control);
        let hello = client
            .call(method::HELLO, json!({}), CONNECT + HELLO)
            .await
            .and_then(|v| serde_json::from_value::<Hello>(v).map_err(|e| e.to_string()));
        let hello = match hello {
            Ok(h) => h,
            Err(why) => {
                mux.close();
                let (exit, _) = child
                    .close(Duration::from_secs(2), Duration::from_secs(1))
                    .await;
                let stderr = std::fs::read_to_string(&target.stderr).unwrap_or_default();
                let mut error = classify(&stderr, Some(exit));
                if let ConnectError::Unreachable(text) = &mut error
                    && stderr.trim().is_empty()
                {
                    *text = format!("unreachable: {why}");
                }
                return Err(error);
            }
        };
        if let Some(why) = hello.incompatible(&target.name) {
            mux.close();
            child
                .close(Duration::from_secs(2), Duration::from_secs(1))
                .await;
            return Err(ConnectError::Incompatible(why));
        }
        let link = Arc::new(Self {
            name: target.name.clone(),
            hello,
            mux,
            client,
            child: tokio::sync::Mutex::new(Some(child)),
            used: Mutex::new(Instant::now()),
            pending: AtomicUsize::new(0),
            lanes: AtomicUsize::new(0),
            spawned: Mutex::new(HashMap::new()),
            spawned_ready: tokio::sync::Notify::new(),
        });
        Ok((link, accepted))
    }

    /// One worker-protocol call; counts as use.
    pub(crate) async fn call(
        &self,
        name: &str,
        params: Value,
        limit: Duration,
    ) -> Result<Value, String> {
        self.pending.fetch_add(1, Ordering::AcqRel);
        let out = self.client.call(name, params, limit).await;
        self.pending.fetch_sub(1, Ordering::AcqRel);
        self.touch();
        out
    }

    fn touch(&self) {
        *self.used.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    /// Whether the connection has had no lane and no operation for `idle`.
    fn idle_for(&self, idle: Duration) -> bool {
        self.pending.load(Ordering::Acquire) == 0
            && self.lanes.load(Ordering::Acquire) == 0
            && self
                .used
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .elapsed()
                >= idle
    }

    /// One bounded command on the worker, as [`command::run`] runs one here.
    pub(crate) async fn exec(
        &self,
        argv: &[String],
        cwd: Option<&str>,
        env: &[(&str, &str)],
        timeout: Duration,
    ) -> Result<Finished, String> {
        let env: HashMap<&str, &str> = env.iter().copied().collect();
        let params = json!({
            "argv": argv,
            "cwd": cwd,
            "env": env,
            "timeout_ms": u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
        });
        let reply = self
            .call(method::EXEC, params, timeout + Duration::from_secs(15))
            .await?;
        finished_of(&reply)
    }

    /// A file under the worker root, at most `max` bytes.
    pub(crate) async fn read(&self, path: &str, max: u64) -> Result<Vec<u8>, String> {
        let reply = self
            .call(
                method::READ,
                json!({ "path": path, "max": max }),
                Duration::from_secs(30),
            )
            .await?;
        reply
            .get("data")
            .and_then(Value::as_str)
            .and_then(crate::sys::base64::decode)
            .ok_or_else(|| "read answer without data".into())
    }

    /// Writes a file under the worker root.
    pub(crate) async fn write(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        self.call(
            method::WRITE,
            json!({ "path": path, "data": crate::sys::base64::encode(bytes) }),
            Duration::from_secs(30),
        )
        .await
        .map(|_| ())
    }

    /// Starts the worker's OMP with `argv` (the first word `omp`); returns its channel and stream:
    /// the child's stdout to read, its stdin to write. Ending the stream's input closes the child.
    pub(crate) async fn spawn(
        self: &Arc<Self>,
        argv: &[String],
        cwd: &str,
        env: &[(String, String)],
    ) -> Result<(u32, DuplexStream, LaneHold), String> {
        let env: HashMap<&str, &str> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let reply = self
            .call(
                method::SPAWN,
                json!({ "argv": argv, "cwd": cwd, "env": env }),
                Duration::from_secs(30),
            )
            .await?;
        let channel = reply
            .get("channel")
            .and_then(Value::as_u64)
            .and_then(|c| u32::try_from(c).ok())
            .ok_or("spawn answer without a channel")?;
        // The open frame came first on the stream; its hand-over may still be on its way.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let ready = self.spawned_ready.notified();
            if let Some(stream) = self
                .spawned
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&channel)
            {
                self.lanes.fetch_add(1, Ordering::AcqRel);
                return Ok((channel, stream, LaneHold(Arc::clone(self))));
            }
            if tokio::time::timeout_at(deadline, ready).await.is_err() {
                return Err("the spawned OMP's channel did not arrive".into());
            }
        }
    }

    /// TERM or KILL to a spawned child.
    pub(crate) async fn signal(&self, channel: u32, signal: &str) -> Result<(), String> {
        self.call(
            method::SIGNAL,
            json!({ "channel": channel, "signal": signal }),
            Duration::from_secs(5),
        )
        .await
        .map(|_| ())
    }

    /// A delivery marker in a session file on the worker.
    pub(crate) async fn find(&self, path: &str, marker: &str) -> Result<Value, String> {
        self.call(
            method::FIND,
            json!({ "path": path, "marker": marker }),
            Duration::from_secs(10),
        )
        .await
    }

    /// The resume check of a session file on the worker.
    pub(crate) async fn verify(&self, params: Value) -> Result<Value, String> {
        self.call(method::VERIFY, params, Duration::from_secs(20))
            .await
    }
    /// Ends the connection: the stream, then `ssh`.
    pub(crate) async fn close(&self) {
        self.mux.close();
        if let Some(child) = self.child.lock().await.take() {
            child
                .close(Duration::from_secs(5), Duration::from_secs(2))
                .await;
        }
    }
}

/// A worker's state as the hub knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum State {
    /// Not connected, nothing wrong known: it connects when needed.
    Idle,
    Online,
    /// The latest connection failed or dropped.
    Offline,
    /// Host key, forced command or key refused: needs the owner.
    Refused,
    Incompatible,
}

/// The stored record of a worker (store kind `worker`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Record {
    pub(crate) state: State,
    #[serde(default)]
    pub(crate) reason: Option<String>,
    #[serde(default)]
    pub(crate) hello: Option<Hello>,
    #[serde(default)]
    pub(crate) seen_ms: Option<i64>,
    pub(crate) changed_ms: i64,
}

impl Default for Record {
    fn default() -> Self {
        Self {
            state: State::Idle,
            reason: None,
            hello: None,
            seen_ms: None,
            changed_ms: now_ms(),
        }
    }
}

/// One worker as `worker list`, `status` and the panel show it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct View {
    pub(crate) name: String,
    pub(crate) display: String,
    pub(crate) destination: String,
    pub(crate) port: u16,
    pub(crate) state: State,
    #[serde(default)]
    pub(crate) reason: Option<String>,
    #[serde(default)]
    pub(crate) version: Option<String>,
    #[serde(default)]
    pub(crate) os: Option<String>,
    #[serde(default)]
    pub(crate) arch: Option<String>,
    #[serde(default)]
    pub(crate) omp: Option<String>,
    pub(crate) lanes: usize,
    pub(crate) lanes_max: u32,
    #[serde(default)]
    pub(crate) seen_ms: Option<i64>,
    #[serde(default)]
    pub(crate) repos: Vec<String>,
    pub(crate) tailscale: bool,
}

/// What a bridge channel a worker opens is handed to: the hub's bridge.
pub(crate) type ChannelHook = Arc<dyn Fn(String, Accepted) + Send + Sync>;

/// The hub's one worker table, for the per-machine work of remote lanes (design 2.1 `Place`).
static REGISTRY: OnceLock<Arc<Workers>> = OnceLock::new();

/// The live connection to worker `name`, connecting when there is none.
pub(crate) async fn connect(name: &str) -> Result<Arc<Link>, String> {
    let workers = REGISTRY
        .get()
        .ok_or("no worker table: workers run only under the hub")?;
    workers.connect(name).await
}

/// The hub's worker table.
pub(crate) fn registry() -> Option<&'static Arc<Workers>> {
    REGISTRY.get()
}

/// How long a failed connection is not tried again for a reachability question.
const RETRY: Duration = Duration::from_mins(1);

/// Every configured worker: its connection, if any, and its state.
pub(crate) struct Workers {
    config: watch::Receiver<Arc<Config>>,
    config_dir: PathBuf,
    logs: PathBuf,
    store: Store,
    links: Mutex<HashMap<String, Arc<Link>>>,
    records: Mutex<HashMap<String, Record>>,
    gates: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    hook: OnceLock<ChannelHook>,
    /// When a connection last failed, per worker: reachability is asked again only after [`RETRY`].
    failed: Mutex<HashMap<String, Instant>>,
    /// Counts connections made and lost, so lanes hear a worker come or go.
    changed: watch::Sender<u64>,
    /// Set at hub stop: no connection is made any more.
    stopped: std::sync::atomic::AtomicBool,
}

impl Workers {
    pub(crate) fn new(
        config: watch::Receiver<Arc<Config>>,
        config_file: &Path,
        logs: PathBuf,
        store: Store,
    ) -> Arc<Self> {
        let workers = Arc::new(Self {
            config,
            config_dir: config_file
                .parent()
                .map_or_else(PathBuf::new, Path::to_path_buf),
            logs,
            store,
            links: Mutex::new(HashMap::new()),
            records: Mutex::new(HashMap::new()),
            gates: Mutex::new(HashMap::new()),
            hook: OnceLock::new(),
            failed: Mutex::new(HashMap::new()),
            changed: watch::channel(0).0,
            stopped: std::sync::atomic::AtomicBool::new(false),
        });
        let _ = REGISTRY.set(Arc::clone(&workers));
        workers
    }

    /// Whether `name` answers now: a live connection, or a new one. A failure is not tried again
    /// within a minute, so a queue sweep never waits on an offline worker each time.
    pub(crate) async fn reachable(self: &Arc<Self>, name: &str) -> bool {
        if self.live(name).is_some() {
            return true;
        }
        let recent = self
            .failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .is_some_and(|at| at.elapsed() < RETRY);
        if recent {
            return false;
        }
        let ok = self.connect(name).await.is_ok();
        let mut failed = self.failed.lock().unwrap_or_else(PoisonError::into_inner);
        if ok {
            failed.remove(name);
        } else {
            failed.insert(name.to_string(), Instant::now());
        }
        ok
    }

    /// Changes whenever a connection is made or lost.
    pub(crate) fn changes(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// Where channels the workers open go.
    pub(crate) fn set_hook(&self, hook: ChannelHook) {
        let _ = self.hook.set(hook);
    }

    fn links(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Link>>> {
        self.links.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn records(&self) -> std::sync::MutexGuard<'_, HashMap<String, Record>> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Reads the stored records, then closes idle connections until the hub stops.
    pub(crate) async fn run(
        self: Arc<Self>,
        mut stop: watch::Receiver<bool>,
    ) -> Result<(), String> {
        if let Ok(docs) = self.store.read(|r| r.coord_all(KIND)).await {
            let mut records = self.records();
            for doc in docs {
                if let Ok(mut record) = serde_json::from_str::<Record>(&doc.body) {
                    // No connection survives a hub restart.
                    if record.state == State::Online {
                        record.state = State::Idle;
                    }
                    records.entry(doc.id).or_insert(record);
                }
            }
        }
        loop {
            tokio::select! {
                () = tokio::time::sleep(SWEEP) => {}
                _ = stop.changed() => break,
            }
            let idle: Vec<Arc<Link>> = self
                .links()
                .values()
                .filter(|l| l.idle_for(IDLE))
                .cloned()
                .collect();
            for link in idle {
                log::info(
                    "worker idle, closing its connection",
                    &[("worker", &link.name)],
                );
                self.links().remove(&link.name);
                link.close().await;
                self.set(&link.name, State::Idle, None, None).await;
            }
        }
        Ok(())
    }

    /// Hub stop, after the lanes ended their input: no new connection, and every one closed.
    pub(crate) async fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
        let all: Vec<Arc<Link>> = self.links().drain().map(|(_, l)| l).collect();
        for link in all {
            link.close().await;
        }
    }

    async fn set(&self, name: &str, state: State, reason: Option<String>, hello: Option<Hello>) {
        let now = now_ms();
        let record = {
            let mut records = self.records();
            let record = records.entry(name.to_string()).or_default();
            if record.state != state || record.reason != reason {
                record.changed_ms = now;
            }
            record.state = state;
            record.reason = reason;
            if let Some(h) = hello {
                record.hello = Some(h);
            }
            if state == State::Online {
                record.seen_ms = Some(now);
            }
            record.clone()
        };
        let Ok(doc) = serde_json::to_string(&record) else {
            return;
        };
        let write = CoordWrite::Put {
            kind: KIND.into(),
            id: name.into(),
            doc,
            expect: None,
        };
        if let Err(e) = self.store.coord(write).await {
            log::warn(
                "worker record not stored",
                &[("worker", &name), ("error", &e)],
            );
        }
    }

    /// The live connection to `name`, connecting when there is none.
    pub(crate) async fn connect(self: &Arc<Self>, name: &str) -> Result<Arc<Link>, String> {
        let config = Arc::clone(&self.config.borrow());
        let settings = config
            .workers
            .get(name)
            .ok_or_else(|| format!("no worker named {name}"))?;
        let gate = Arc::clone(
            self.gates
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(name.to_string())
                .or_default(),
        );
        let _one = gate.lock().await;
        if self.stopped.load(Ordering::Acquire) {
            return Err(format!("{name}: the hub is stopping"));
        }
        if let Some(link) = self.links().get(name).filter(|l| !l.mux.is_closed()) {
            link.touch();
            return Ok(Arc::clone(link));
        }
        let target = Target::of(name, settings, &self.config_dir, &self.logs)?;
        match Link::connect(&target).await {
            Ok((link, accepted)) => {
                self.links().insert(name.to_string(), Arc::clone(&link));
                self.set(name, State::Online, None, Some(link.hello.clone()))
                    .await;
                log::info("worker connected", &[("worker", &name)]);
                self.failed
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(name);
                self.changed.send_modify(|n| *n += 1);
                tokio::spawn(Arc::clone(self).watch(Arc::clone(&link), accepted));
                Ok(link)
            }
            Err(e) => {
                let state = match &e {
                    ConnectError::Incompatible(_) => State::Incompatible,
                    e if e.refused() => State::Refused,
                    _ => State::Offline,
                };
                let text = e.to_string();
                log::warn(
                    "worker connection failed",
                    &[("worker", &name), ("reason", &text)],
                );
                self.set(name, state, Some(text.clone()), None).await;
                Err(format!("{name}: {text}"))
            }
        }
    }

    /// Hands channels the worker opens to the hook, and marks the worker offline when its stream
    /// drops.
    async fn watch(self: Arc<Self>, link: Arc<Link>, mut accepted: mpsc::Receiver<Accepted>) {
        loop {
            tokio::select! {
                got = accepted.recv() => match got {
                    Some(channel) => match channel.header.get("kind").and_then(Value::as_str) {
                        Some("spawn") => {
                            link.spawned
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .insert(channel.channel, channel.stream);
                            link.spawned_ready.notify_waiters();
                        }
                        _ => match self.hook.get() {
                            Some(hook) => hook(link.name.clone(), channel),
                            None => drop(channel),
                        },
                    },
                    None => break,
                },
                () = link.mux.closed() => break,
            }
        }
        let current = self
            .links()
            .get(&link.name)
            .is_some_and(|l| Arc::ptr_eq(l, &link));
        if current {
            self.links().remove(&link.name);
            link.close().await;
            self.set(
                &link.name,
                State::Offline,
                Some(format!(
                    "connection lost at {}",
                    crate::jobs::instant_text(now_ms())
                )),
                None,
            )
            .await;
            log::warn("worker connection lost", &[("worker", &link.name)]);
            self.changed.send_modify(|n| *n += 1);
        }
    }

    /// Ends the connection to `name`, if any.
    pub(crate) async fn disconnect(&self, name: &str) {
        let link = self.links().remove(name);
        if let Some(link) = link {
            link.close().await;
        }
    }

    /// The live connection to `name`, without connecting.
    pub(crate) fn live(&self, name: &str) -> Option<Arc<Link>> {
        self.links()
            .get(name)
            .filter(|l| !l.mux.is_closed())
            .cloned()
    }

    /// Every configured worker.
    pub(crate) fn list(&self) -> Vec<View> {
        let config = Arc::clone(&self.config.borrow());
        let links = self.links().clone();
        let records = self.records().clone();
        config
            .workers
            .iter()
            .map(|(name, s)| {
                let record = records.get(name).cloned().unwrap_or_default();
                let link = links.get(name).filter(|l| !l.mux.is_closed());
                let hello = link.map(|l| l.hello.clone()).or(record.hello);
                View {
                    name: name.clone(),
                    display: s.name.clone().unwrap_or_else(|| name.clone()),
                    destination: s.destination.clone(),
                    port: s.port,
                    state: if link.is_some() {
                        State::Online
                    } else {
                        record.state
                    },
                    reason: if link.is_some() { None } else { record.reason },
                    version: hello.as_ref().map(|h| h.version.clone()),
                    os: hello.as_ref().map(|h| h.os.clone()),
                    arch: hello.as_ref().map(|h| h.arch.clone()),
                    omp: hello.as_ref().and_then(|h| h.omp.version.clone()),
                    lanes: link.map_or(0, |l| l.lanes.load(Ordering::Acquire)),
                    lanes_max: s.lanes,
                    seen_ms: if link.is_some() {
                        Some(now_ms())
                    } else {
                        record.seen_ms
                    },
                    repos: s.repos.clone(),
                    tailscale: s.tailscale,
                }
            })
            .collect()
    }

    /// `worker doctor`: connects and checks what a lane will need (design 4.2).
    pub(crate) async fn doctor(self: &Arc<Self>, name: &str) -> Result<Vec<Check>, String> {
        let config = Arc::clone(&self.config.borrow());
        let settings = config
            .workers
            .get(name)
            .ok_or_else(|| format!("no worker named {name}"))?
            .clone();
        let mut checks = Vec::new();
        if settings.tailscale {
            let machine = settings.name.clone().unwrap_or_else(|| name.to_string());
            let host = settings
                .destination
                .split_once('@')
                .map_or(settings.destination.as_str(), |(_, h)| h);
            checks.push(match enrol::tailscale_ip(&machine).await {
                Ok(ip) if ip == host => Check::ok("tailnet address", &ip),
                Ok(ip) => Check::fail(
                    "tailnet address",
                    &format!("{machine} is now {ip}, enrolled as {host}; enrol it again"),
                ),
                Err(e) => Check::fail("tailnet address", &e),
            });
        }
        let link = match self.connect(name).await {
            Ok(l) => l,
            Err(e) => {
                checks.push(Check::fail("connection", &e));
                return Ok(checks);
            }
        };
        let h = &link.hello;
        checks.push(Check::ok(
            "connection",
            "host key pinned, forced command applied, key accepted",
        ));
        checks.push(Check::ok(
            "protocols",
            &format!(
                "cataly {} · worker protocol {} · bridge protocol {} ({} {})",
                h.version, h.protocol, h.bridge, h.os, h.arch
            ),
        ));
        checks.push(match &h.omp.version {
            Some(v) => Check::ok("omp", v),
            None => Check::fail("omp", &h.omp.text()),
        });
        checks.push(match &h.bridge_link {
            None => Check::ok("bridge link", "OMP loads this worker's bridge"),
            Some(e) => Check::fail("bridge link", e),
        });
        checks.push(match &h.git.version {
            Some(v) => Check::ok("git", v),
            None => Check::fail("git", &h.git.text()),
        });
        match link
            .call(method::STATUS, json!({}), Duration::from_secs(5))
            .await
        {
            Ok(status) => {
                let free = status
                    .pointer("/disk/available")
                    .and_then(Value::as_u64)
                    .map_or_else(
                        || "unknown".into(),
                        |b| format!("{} GiB free", b / (1 << 30)),
                    );
                checks.push(Check::ok("disk", &free));
            }
            Err(e) => checks.push(Check::fail("status", &e)),
        }
        for repo in &settings.repos {
            let argv = vec![
                "git".to_string(),
                "ls-remote".into(),
                "--exit-code".into(),
                format!("https://{repo}.git"),
                "HEAD".into(),
            ];
            let env = [("GIT_TERMINAL_PROMPT", "0")];
            let done = link.exec(&argv, None, &env, Duration::from_secs(30)).await;
            checks.push(match done {
                Ok(d) if d.success() => Check::ok(&format!("git access {repo}"), "readable"),
                Ok(d) => Check::fail(
                    &format!("git access {repo}"),
                    String::from_utf8_lossy(&d.stderr)
                        .lines()
                        .last()
                        .unwrap_or("refused"),
                ),
                Err(e) => Check::fail(&format!("git access {repo}"), &e),
            });
        }
        Ok(checks)
    }

    /// `worker remove`: refused while lanes live; asks the worker to forget this hub's line when it
    /// answers; drops the key, the pin and the config section. Returns what the owner still has to
    /// do on the worker, if anything.
    pub(crate) async fn remove(
        self: &Arc<Self>,
        name: &str,
        config_file: &Path,
    ) -> Result<Option<String>, String> {
        let config = Arc::clone(&self.config.borrow());
        let settings = config
            .workers
            .get(name)
            .ok_or_else(|| format!("no worker named {name}"))?
            .clone();
        if let Some(link) = self.live(name)
            && link.lanes.load(Ordering::Acquire) > 0
        {
            return Err(format!("{name} still runs lanes; close or move them first"));
        }
        let hub = enrol::hub_id(&self.config_dir)?;
        let forgot = match self.connect(name).await {
            Ok(link) => link
                .call(method::FORGET, json!({}), Duration::from_secs(5))
                .await
                .is_ok(),
            Err(_) => false,
        };
        self.disconnect(name).await;
        let dest = Destination::parse(&format!("{}:{}", settings.destination, settings.port))?;
        remove_files(&self.config_dir, name, &dest)?;
        crate::config::file::edit(config_file, "", |text| {
            Ok(crate::config::file::remove_section(
                text,
                &format!("workers.{name}"),
            ))
        })?;
        self.records().remove(name);
        let _ = self
            .store
            .coord(CoordWrite::Delete {
                kind: KIND.into(),
                id: name.into(),
            })
            .await;
        Ok((!forgot).then(|| {
            format!(
                "{name} did not answer: on it, run `cataly worker init --remove --hub-key '{}'`, or \
                 delete the line ending in {} from ~/.ssh/authorized_keys",
                enrol::comment(&hub),
                enrol::comment(&hub)
            )
        }))
    }
}

/// Drops a worker's key pair and its host-key pin.
pub(crate) fn remove_files(
    config_dir: &Path,
    name: &str,
    dest: &Destination,
) -> Result<(), String> {
    let key = enrol::key_path(config_dir, name);
    for path in [key.clone(), PathBuf::from(format!("{}.pub", key.display()))] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    let pins = enrol::known_hosts(config_dir);
    if let Ok(text) = std::fs::read_to_string(&pins) {
        let kept = enrol::drop_pin(&text, &dest.pin_name());
        if kept != text {
            crate::sys::fs::write_atomic(&pins, kept.as_bytes()).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// One `worker doctor` finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Check {
    pub(crate) name: String,
    pub(crate) ok: bool,
    pub(crate) detail: String,
}

impl Check {
    fn ok(name: &str, detail: &str) -> Self {
        Self {
            name: name.into(),
            ok: true,
            detail: detail.into(),
        }
    }

    fn fail(name: &str, detail: &str) -> Self {
        Self {
            name: name.into(),
            ok: false,
            detail: detail.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ssh_command_reads_no_config_and_asks_for_the_marker() {
        let dest = Destination::parse("dev@100.101.102.103:2222").unwrap();
        let argv = ssh_argv(
            Path::new("/c/secrets/worker-box"),
            Path::new("/c/known_hosts"),
            &dest,
        );
        let line = argv.join(" ");
        assert!(line.starts_with("ssh -F none -T -i /c/secrets/worker-box -p 2222 "));
        for option in [
            "IdentitiesOnly=yes",
            "IdentityAgent=none",
            "UserKnownHostsFile=/c/known_hosts",
            "GlobalKnownHostsFile=/dev/null",
            "StrictHostKeyChecking=yes",
            "CheckHostIP=no",
            "UpdateHostKeys=no",
            "BatchMode=yes",
            "ClearAllForwardings=yes",
            "ConnectTimeout=10",
            "ServerAliveInterval=15",
            "ServerAliveCountMax=3",
        ] {
            assert!(line.contains(&format!("-o {option} ")), "{option}");
        }
        assert!(line.ends_with("-- dev@100.101.102.103 cataly-worker-v1"));
        assert!(!line.contains("ProxyJump") && !line.contains("ControlMaster"));
    }

    #[test]
    fn ssh_failures_are_named() {
        assert_eq!(
            classify(
                "@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@\nHost key verification failed.\n",
                Some(Exit::Code(255))
            ),
            ConnectError::HostKey
        );
        assert_eq!(
            classify(
                "No ED25519 host key is known for [h]:2222 and you have requested strict checking.\n",
                None
            ),
            ConnectError::HostKey
        );
        assert_eq!(
            classify(
                "bash: line 1: cataly-worker-v1: command not found\n",
                Some(Exit::Code(127))
            ),
            ConnectError::NoForcedCommand
        );
        assert_eq!(
            classify("", Some(Exit::Code(127))),
            ConnectError::NoForcedCommand
        );
        assert_eq!(
            classify(
                "dev@h: Permission denied (publickey).\n",
                Some(Exit::Code(255))
            ),
            ConnectError::Denied
        );
        let other = classify(
            "ssh: connect to host h port 22: Connection refused\n",
            Some(Exit::Code(255)),
        );
        assert!(matches!(&other, ConnectError::Unreachable(t) if t.contains("Connection refused")));
        assert!(!other.refused() && ConnectError::HostKey.refused());
        assert!(ConnectError::NoForcedCommand.to_string().contains("2222"));
    }

    #[test]
    fn exec_results_cross_the_wire_unchanged() {
        let done = Finished {
            exit: Exit::Signal(9),
            timed_out: true,
            cancelled: false,
            stdout: b"out\xff".to_vec(),
            stderr: b"err".to_vec(),
            stdout_bytes: 4,
            stdout_sha256: Some([7; 32]),
            truncated: true,
            pipes_held: false,
        };
        let back = finished_of(&exec_reply(&done)).unwrap();
        assert_eq!(back.exit, Exit::Signal(9));
        assert!(back.timed_out && back.truncated && !back.success());
        assert_eq!((back.stdout_sha256, back.stdout_bytes), (Some([7; 32]), 4));
        assert_eq!((back.stdout, back.stderr), (done.stdout, done.stderr));
        let ok = finished_of(&json!({"exit": {"code": 0}, "stdout": "", "stderr": ""})).unwrap();
        assert!(ok.success());
        assert!(finished_of(&json!({"exit": {"code": 0}})).is_err());
    }

    #[test]
    fn compatibility_needs_both_protocols() {
        let hello = Hello {
            protocol: super::super::PROTOCOL,
            bridge: crate::hub::bridge::PROTOCOL,
            ..Hello::default()
        };
        assert_eq!(hello.incompatible("box"), None);
        let newer = Hello {
            protocol: 2,
            ..hello.clone()
        };
        assert!(
            newer
                .incompatible("box")
                .unwrap()
                .contains("run cataly update on box")
        );
        let bridge = Hello { bridge: 2, ..hello };
        assert!(
            bridge
                .incompatible("box")
                .unwrap()
                .contains("bridge protocol 2")
        );
    }
}
