//! Worker mode (design 2.2, 2.4, 2.5): `cataly worker serve --hub <id>`, started by the worker's sshd
//! as the forced command of one hub's key. It speaks the worker protocol on channel 0 of the stream
//! on its stdin and stdout, runs bounded commands and OMP children for the hub, moves bytes of files
//! under its root, and carries every connection to its bridge socket as a channel to the hub. When
//! the stream ends (end of input, a write error, 45 s of silence) it closes its OMP children by the
//! HOST-20 rule (end input, 15 s, TERM, 5 s, KILL) and exits.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::{Notify, mpsc};

use super::mux::{Mux, Timing};
use super::{FILE_MAX, Hello, LINE_MAX, MARKER, PROTOCOL, Probed, method};
use crate::proto::{self, Line, Response, code};
use crate::sys::command::{self, KILL_GRACE, Output, Spec};
use crate::sys::paths::Paths;

/// After end of input before TERM (HOST-20).
const CLOSE_WAIT: Duration = Duration::from_secs(15);
/// An older `serve` for the same hub gets this long to end after TERM (design 2.4).
const TAKEOVER: Duration = Duration::from_secs(30);
/// OMP child stderr kept per spawn.
const STDERR_CAP: u64 = 1024 * 1024;
/// Largest captured output of one `exec`.
const EXEC_CAP: u64 = 4 * 1024 * 1024;
/// Longest `exec`.
const EXEC_MAX: Duration = Duration::from_hours(2);
/// File access bound (design 2.2).
const FILE_TIMEOUT: Duration = Duration::from_secs(10);
/// Children of one hub connection at once.
const CHILDREN_MAX: usize = 16;

/// `<config>/worker.toml`, written by `worker init`: where the worker's OMP is and the `PATH` its
/// commands get (sshd starts the forced command with a short default `PATH`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct WorkerFile {
    pub(crate) omp: Option<String>,
    pub(crate) path: Option<String>,
}

impl WorkerFile {
    pub(crate) fn path(paths: &Paths) -> PathBuf {
        paths
            .config_file
            .parent()
            .map_or_else(|| PathBuf::from("worker.toml"), |d| d.join("worker.toml"))
    }

    pub(crate) fn load(paths: &Paths) -> Result<Self, String> {
        let path = Self::path(paths);
        match crate::sys::fs::read_private(&path, 64 * 1024) {
            Ok(None) => Ok(Self::default()),
            Ok(Some(bytes)) => {
                let text = String::from_utf8(bytes)
                    .map_err(|_| format!("{} is not UTF-8", path.display()))?;
                toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// The file's text.
    pub(crate) fn render(&self) -> String {
        let mut out = String::from("# Written by `cataly worker init`.\n");
        if let Some(omp) = &self.omp {
            let _ = writeln!(out, "omp = {}", crate::config::file::toml_string(omp));
        }
        if let Some(path) = &self.path {
            let _ = writeln!(out, "path = {}", crate::config::file::toml_string(path));
        }
        out
    }
}

/// `<state>/worker/<hub>`: the root of everything this hub's lanes keep on the worker.
pub(crate) fn root(paths: &Paths, hub: &str) -> PathBuf {
    paths.state_dir.join("worker").join(hub)
}

/// `worker-<hub>.sock` in the runtime directory: the bridge socket of this hub's sessions.
pub(crate) fn socket(paths: &Paths, hub: &str) -> PathBuf {
    paths.runtime_dir.join(format!("worker-{hub}.sock"))
}

/// `cataly worker serve --hub <id>`; returns the exit status.
pub(crate) fn run(hub: &str, paths: &Paths) -> u8 {
    let refuse = |m: &str| {
        crate::cli::out::error_line(&format!("cataly worker serve: {m}"));
        crate::cli::exit::FAILED
    };
    if rustix::termios::isatty(std::io::stdin()) {
        return refuse(
            "worker mode runs only as the forced command of a hub's key; to enrol this machine run \
             `cataly worker init`",
        );
    }
    if std::env::var("SSH_ORIGINAL_COMMAND").as_deref() != Ok(MARKER) {
        return refuse("this key runs only Cataly worker mode for its hub");
    }
    if !super::enrol::hub_id_ok(hub) {
        return refuse("--hub takes the six-digit hub ID of the key's line");
    }
    let file = match WorkerFile::load(paths) {
        Ok(f) => f,
        Err(e) => return refuse(&e),
    };
    let root = root(paths, hub);
    for dir in [
        &paths.state_dir,
        &paths.state_dir.join("worker"),
        &root,
        &root.join("logs"),
        &paths.runtime_dir,
    ] {
        if let Err(e) = crate::sys::fs::ensure_private_dir(dir) {
            return refuse(&e.to_string());
        }
    }
    crate::log::set_level(crate::log::Level::Warn);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(8)
        .thread_name("cataly-worker")
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return refuse(&format!("runtime: {e}")),
    };
    let worker = Worker {
        hub: hub.to_string(),
        root,
        socket: socket(paths, hub),
        omp: file.omp.unwrap_or_else(|| "omp".into()),
        path: file.path,
        children: Mutex::new(HashMap::new()),
    };
    let code = runtime.block_on(async move {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        serve(Arc::new(worker), stdin, stdout, Timing::STANDARD).await
    });
    runtime.shutdown_timeout(Duration::from_secs(1));
    code
}

/// One hub connection's worker.
pub(crate) struct Worker {
    pub(crate) hub: String,
    pub(crate) root: PathBuf,
    pub(crate) socket: PathBuf,
    /// The OMP program `spawn` and `exec` run for `omp`.
    pub(crate) omp: String,
    /// The `PATH` commands get, when `worker init` captured one.
    pub(crate) path: Option<String>,
    /// Spawned children by channel.
    children: Mutex<HashMap<u32, Child>>,
}

/// A spawned OMP: its PID and the signal that closes it.
type Child = (Option<u32>, Arc<Notify>);

impl Worker {
    fn children(&self) -> std::sync::MutexGuard<'_, HashMap<u32, Child>> {
        self.children.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The program for `argv[0]`: `omp` is the configured OMP.
    fn program(&self, argv: &[String]) -> Vec<String> {
        let mut argv = argv.to_vec();
        if let Some(first) = argv.first_mut()
            && first == "omp"
        {
            first.clone_from(&self.omp);
        }
        argv
    }

    /// The environment of a command: the captured `PATH`, then the hub's variables, with the bridge
    /// socket always this connection's.
    fn env(&self, extra: &HashMap<String, String>) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = self
            .path
            .iter()
            .map(|p| ("PATH".to_string(), p.clone()))
            .collect();
        env.extend(
            extra
                .iter()
                .filter(|(k, _)| k.as_str() != "CATALY_SOCKET")
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        env.push(("CATALY_SOCKET".into(), self.socket.display().to_string()));
        env
    }

    /// A path under the root: relative paths are taken from it; `..` and anything that leaves the
    /// root through a link are refused.
    fn confine(&self, path: &str) -> Result<PathBuf, String> {
        let p = Path::new(path);
        if p.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(format!("{path}: '..' is not allowed"));
        }
        let full = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        };
        let root = std::fs::canonicalize(&self.root).map_err(|e| format!("worker root: {e}"))?;
        // The nearest existing ancestor must resolve inside the root.
        let mut probe = full.as_path();
        let real = loop {
            match std::fs::canonicalize(probe) {
                Ok(real) => break real,
                Err(_) => match probe.parent() {
                    Some(parent) => probe = parent,
                    None => return Err(format!("{path} is outside the worker root")),
                },
            }
        };
        if !real.starts_with(&root) {
            return Err(format!("{path} is outside the worker root"));
        }
        Ok(full)
    }
}

/// Serves one hub connection until the stream ends.
pub(crate) async fn serve<R, W>(worker: Arc<Worker>, read: R, write: W, timing: Timing) -> u8
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let _lock = match take_lock(&worker.root.join("serve.lock")).await {
        Ok(lock) => lock,
        Err(e) => {
            crate::cli::out::error_line(&format!("cataly worker serve: {e}"));
            return crate::cli::exit::FAILED;
        }
    };
    let (mux, control, _accepted) = Mux::start(read, write, timing);
    let listener = match crate::hub::socket::bind(&worker.socket) {
        Ok(l) => Some(l),
        Err(e) => {
            crate::log::warn("worker bridge socket", &[("error", &e)]);
            None
        }
    };
    let bridge = listener.map(|l| tokio::spawn(bridges(l, mux.clone())));
    let (read, mut write) = tokio::io::split(control);
    let (tx, mut rx) = mpsc::channel::<Response>(64);
    let replies = tokio::spawn(async move {
        while let Some(reply) = rx.recv().await {
            let line = reply.to_line();
            if write.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    let mut reader = BufReader::new(read);
    loop {
        let line = tokio::select! {
            l = proto::read_line(&mut reader, LINE_MAX) => l,
            () = mux.closed() => break,
        };
        let line = match line {
            Line::Text(l) => l,
            Line::TooLong => {
                let _ = tx
                    .send(Response::err(0, code::BAD_REQUEST, "request line too long"))
                    .await;
                break;
            }
            Line::Closed => break,
        };
        let request = match proto::decode(&line) {
            Ok(r) => r,
            Err(reply) => {
                let _ = tx.send(*reply).await;
                continue;
            }
        };
        let (worker, mux, tx) = (Arc::clone(&worker), mux.clone(), tx.clone());
        tokio::spawn(async move {
            let id = request.id;
            let reply = match handle(&worker, &mux, &request.method, request.params).await {
                Ok(value) => Response::ok(id, value),
                Err(message) => Response::err(id, code::REFUSED, message),
            };
            let _ = tx.send(reply).await;
        });
    }
    mux.close();
    if let Some(task) = bridge {
        task.abort();
    }
    replies.abort();
    let _ = std::fs::remove_file(&worker.socket);
    // Every OMP child ends by the HOST-20 rule; this waits for the last.
    let stops: Vec<Arc<Notify>> = worker
        .children()
        .values()
        .map(|(_, n)| Arc::clone(n))
        .collect();
    for stop in &stops {
        stop.notify_one();
    }
    let deadline = tokio::time::Instant::now() + CLOSE_WAIT + KILL_GRACE + Duration::from_secs(5);
    while !worker.children().is_empty() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    crate::cli::exit::OK
}

/// Takes `serve.lock`; an older `serve` of the same hub that still holds it (a half-open
/// connection) gets TERM and up to 30 s to end (design 2.4).
async fn take_lock(path: &Path) -> Result<crate::sys::lock::HubLock, String> {
    use crate::sys::lock::{LockOutcome, acquire};
    let deadline = tokio::time::Instant::now() + TAKEOVER;
    let mut signalled = false;
    loop {
        match acquire(path).map_err(|e| e.to_string())? {
            LockOutcome::Acquired(lock) => return Ok(lock),
            LockOutcome::Held { pid } => {
                if !signalled {
                    signalled = true;
                    if let Some(p) = pid
                        .and_then(|p| i32::try_from(p).ok())
                        .and_then(rustix::process::Pid::from_raw)
                    {
                        let _ = rustix::process::kill_process(p, rustix::process::Signal::TERM);
                    }
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err("an older worker serve for this hub still runs".into());
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

/// Carries each connection to the bridge socket as a channel to the hub. The hub decides what each
/// may do; this side only checks that the peer is this user.
async fn bridges(listener: tokio::net::UnixListener, mux: Mux) {
    let own = crate::sys::own_uid();
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        if crate::sys::peer::check_peer(&stream, own).is_err() {
            continue;
        }
        let Ok((_, mut channel)) = mux.open(&json!({ "kind": "bridge" })).await else {
            return;
        };
        tokio::spawn(async move {
            let _ = tokio::io::copy_bidirectional(&mut stream, &mut channel).await;
        });
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExecParams {
    argv: Vec<String>,
    cwd: Option<String>,
    env: HashMap<String, String>,
    timeout_ms: u64,
    cap: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FileParams {
    path: String,
    data: Option<String>,
    max: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SignalParams {
    channel: u32,
    signal: String,
}

fn parse<T: serde::de::DeserializeOwned + Default>(params: Value) -> Result<T, String> {
    if params.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(params).map_err(|e| format!("bad parameters: {e}"))
}

async fn handle(
    worker: &Arc<Worker>,
    mux: &Mux,
    name: &str,
    params: Value,
) -> Result<Value, String> {
    match name {
        method::HELLO => Ok(serde_json::to_value(hello(worker).await).unwrap_or_default()),
        method::STATUS => status(worker).await,
        method::EXEC => exec(worker, parse(params)?).await,
        method::SPAWN => spawn(worker, mux, parse(params)?).await,
        method::SIGNAL => signal(worker, &parse(params)?),
        method::READ | method::STAT | method::WRITE => {
            let p: FileParams = parse(params)?;
            let (worker, name) = (Arc::clone(worker), name.to_string());
            tokio::time::timeout(
                FILE_TIMEOUT,
                tokio::task::spawn_blocking(move || file(&worker, &name, &p)),
            )
            .await
            .map_err(|_| "the file operation took longer than 10 s".to_string())?
            .map_err(|e| e.to_string())?
        }
        method::FORGET => forget(&worker.hub),
        method::FIND | method::VERIFY => {
            let (worker, name) = (Arc::clone(worker), name.to_string());
            tokio::time::timeout(
                FILE_TIMEOUT,
                tokio::task::spawn_blocking(move || transcript(&worker, &name, &params)),
            )
            .await
            .map_err(|_| "the session file check took longer than 10 s".to_string())?
            .map_err(|e| e.to_string())?
        }
        other => Err(format!("unknown worker method {other}")),
    }
}

/// The first line of `<program> --version`, bounded to 5 s.
async fn version_of(argv: Vec<String>, worker: &Worker) -> Probed {
    let env = worker.env(&HashMap::new());
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let done = command::run(&Spec {
        argv: &argv,
        cwd: None,
        env: &env,
        timeout: Duration::from_secs(5),
        kill_grace: Duration::from_secs(1),
        output_cap: 4096,
        output: Output::Capture,
    })
    .await;
    match done {
        Ok(d) if d.success() => Probed {
            version: String::from_utf8_lossy(&d.stdout)
                .lines()
                .next()
                .map(|l| l.trim().to_string()),
            error: None,
        },
        Ok(d) => Probed {
            version: None,
            error: Some(format!("{} ended with {:?}", argv.join(" "), d.exit)),
        },
        Err(e) => Probed {
            version: None,
            error: Some(e.to_string()),
        },
    }
}

async fn hello(worker: &Worker) -> Hello {
    let omp = version_of(worker.program(&["omp".into(), "--version".into()]), worker);
    let git = version_of(vec!["git".into(), "--version".into()], worker);
    let (omp, git) = tokio::join!(omp, git);
    Hello {
        protocol: PROTOCOL,
        bridge: crate::hub::bridge::PROTOCOL,
        version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        root: worker.root.display().to_string(),
        omp,
        git,
        bridge_link: crate::omp::extension::check().err(),
        socket: worker.socket.display().to_string(),
    }
}

async fn status(worker: &Worker) -> Result<Value, String> {
    use crate::sys::host;
    Ok(json!({
        "memory": host::memory().await.ok().map(crate::proto::HostMemory::from),
        "load": host::load().await.ok().map(crate::proto::HostLoad::from),
        "disk": host::disk(&worker.root).ok().map(crate::proto::HostDisk::from),
    }))
}

async fn exec(worker: &Worker, p: ExecParams) -> Result<Value, String> {
    if p.argv.is_empty() {
        return Err("exec needs a program".into());
    }
    let argv = worker.program(&p.argv);
    let cwd = match &p.cwd {
        Some(c) if Path::new(c).is_absolute() => PathBuf::from(c),
        Some(c) => worker.root.join(c),
        None => worker.root.clone(),
    };
    let env = worker.env(&p.env);
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let timeout = Duration::from_millis(p.timeout_ms).clamp(Duration::from_secs(1), EXEC_MAX);
    let cap = if p.cap == 0 {
        EXEC_CAP
    } else {
        p.cap.min(EXEC_CAP)
    };
    let done = command::run(&Spec {
        argv: &argv,
        cwd: Some(&cwd),
        env: &env,
        timeout,
        kill_grace: KILL_GRACE,
        output_cap: cap,
        output: Output::Capture,
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(super::link::exec_reply(&done))
}

/// Starts the worker's own OMP on a new channel: its stdout flows to the hub, the hub's bytes to its
/// stdin; the hub's end of input closes it by the HOST-20 rule.
async fn spawn(worker: &Arc<Worker>, mux: &Mux, p: ExecParams) -> Result<Value, String> {
    if p.argv.first().map(String::as_str) != Some("omp") {
        return Err("spawn runs only omp".into());
    }
    if worker.children().len() >= CHILDREN_MAX {
        return Err(format!(
            "{CHILDREN_MAX} OMP children already run for this hub"
        ));
    }
    let argv = worker.program(&p.argv);
    let cwd = match &p.cwd {
        Some(c) => worker.confine(c)?,
        None => worker.root.clone(),
    };
    let env = worker.env(&p.env);
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let token = crate::host::random_hex(6);
    let stderr = worker.root.join("logs").join(format!("omp-{token}.err"));
    let mut child = command::attach(&argv, Some(&cwd), &env, Some(&stderr), STDERR_CAP)
        .map_err(|e| e.to_string())?;
    let (Some(mut stdin), Some(mut stdout)) = (child.take_stdin(), child.take_stdout()) else {
        child.close(CLOSE_WAIT, KILL_GRACE).await;
        return Err("OMP's pipes are not available".into());
    };
    let pid = child.pid();
    let (channel, stream) = match mux.open(&json!({ "kind": "spawn", "pid": pid })).await {
        Ok(opened) => opened,
        Err(e) => {
            child.close(Duration::ZERO, KILL_GRACE).await;
            return Err(e);
        }
    };
    let stop = Arc::new(Notify::new());
    worker.children().insert(channel, (pid, Arc::clone(&stop)));
    let (mut from_hub, mut to_hub) = tokio::io::split(stream);
    let input = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut from_hub, &mut stdin).await;
    });
    let output = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut stdout, &mut to_hub).await;
        let _ = to_hub.shutdown().await;
    });
    let (worker2, mux2) = (Arc::clone(worker), mux.clone());
    tokio::spawn(async move {
        // The hub's end of input closes the child; so do the stream's end and a stop.
        let mut input = input;
        tokio::select! {
            _ = &mut input => {}
            () = stop.notified() => input.abort(),
            () = mux2.closed() => input.abort(),
        }
        child.close(CLOSE_WAIT, KILL_GRACE).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), output).await;
        worker2.children().remove(&channel);
    });
    Ok(json!({ "channel": channel, "pid": pid, "stderr": stderr.display().to_string() }))
}

/// TERM or KILL to one of this connection's own children.
fn signal(worker: &Worker, p: &SignalParams) -> Result<Value, String> {
    let sig = match p.signal.as_str() {
        "TERM" => rustix::process::Signal::TERM,
        "KILL" => rustix::process::Signal::KILL,
        other => return Err(format!("signal {other} is not TERM or KILL")),
    };
    let pid = worker
        .children()
        .get(&p.channel)
        .and_then(|(pid, _)| *pid)
        .ok_or("no child of this hub on that channel")?;
    let pid = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or("bad child PID")?;
    rustix::process::kill_process(pid, sig).map_err(|e| e.to_string())?;
    Ok(json!({}))
}

/// `read`, `stat` and `write` under the root.
fn file(worker: &Worker, name: &str, p: &FileParams) -> Result<Value, String> {
    let path = worker.confine(&p.path)?;
    match name {
        method::STAT => Ok(match std::fs::metadata(&path) {
            Ok(m) => json!({
                "exists": true,
                "kind": if m.is_dir() { "dir" } else if m.is_file() { "file" } else { "other" },
                "size": m.len(),
                "path": path.display().to_string(),
                "real": std::fs::canonicalize(&path).ok().map(|r| r.display().to_string()),
            }),
            Err(_) => json!({ "exists": false, "path": path.display().to_string() }),
        }),
        method::READ => {
            use std::io::Read as _;
            let max = p.max.unwrap_or(FILE_MAX).min(FILE_MAX);
            let file =
                std::fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let size = file.metadata().map_or(0, |m| m.len());
            let mut bytes = Vec::new();
            file.take(max)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(json!({
                "data": crate::sys::base64::encode(&bytes),
                "size": size,
                "truncated": size > max,
            }))
        }
        _ => {
            let data = p.data.as_deref().ok_or("write needs data")?;
            let bytes = crate::sys::base64::decode(data).ok_or("data is not base64")?;
            if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > FILE_MAX {
                return Err("larger than 8 MiB".into());
            }
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            crate::sys::fs::write_atomic(&path, &bytes).map_err(|e| e.to_string())?;
            Ok(json!({ "size": bytes.len() }))
        }
    }
}

/// `~/.ssh/authorized_keys`.
pub(crate) fn authorized_keys() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| h.is_absolute())
        .map(|h| h.join(".ssh").join("authorized_keys"))
        .ok_or_else(|| "HOME is not set to an absolute path".into())
}

/// Removes this hub's line from `authorized_keys` (design 2.2 `forget`).
fn forget(hub: &str) -> Result<Value, String> {
    let path = authorized_keys()?;
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let kept = super::enrol::replace_line(&text, hub, None);
    let removed = kept != text;
    if removed {
        crate::sys::fs::write_atomic(&path, kept.as_bytes()).map_err(|e| e.to_string())?;
    }
    Ok(json!({ "removed": removed }))
}

/// `find` and `verify`: the hub's own session-file parsers, run on the worker where the file is.
fn transcript(worker: &Worker, name: &str, params: &Value) -> Result<Value, String> {
    use crate::omp::transcript::{Folder, Found, exact, find_marker};
    let text = |key: &str| params.get(key).and_then(Value::as_str).unwrap_or_default();
    let path = worker.confine(text("path"))?;
    if name == method::FIND {
        let found = match find_marker(&path, text("marker")).map_err(|e| e.to_string())? {
            Found::Yes => "yes",
            Found::No => "no",
            Found::Missing => "missing",
            Found::OutOfTime => "out_of_time",
        };
        return Ok(json!({ "found": found }));
    }
    let root = worker.confine(text("root"))?;
    let folder = match (params.get("exactly"), params.get("recorded")) {
        (Some(Value::String(dir)), _) => Folder::Exactly(PathBuf::from(dir)),
        (_, Some(Value::String(dir))) => Folder::Recorded(PathBuf::from(dir)),
        _ => return Err("verify needs exactly or recorded".into()),
    };
    let verified = exact(&path, &root, text("session"), &folder).map_err(|e| e.to_string())?;
    Ok(verified.map_or(
        Value::Null,
        |t| json!({ "model": t.model, "cwd": t.cwd.display().to_string() }),
    ))
}
