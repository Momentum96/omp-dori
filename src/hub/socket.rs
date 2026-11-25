//! The control socket (DESIGN 8, HOST req. 71-75): one Unix stream socket, mode 0600, in a 0700
//! directory; peer-credential check; newline JSON with a 1 MiB line cap; 64 connections; 60 s idle
//! close; a request runs to completion even if its caller disconnects.

use std::fmt;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, broadcast};

use super::Hub;
use super::dispatch::{self, Authority};
use crate::host::Role;
use crate::log;
use crate::proto::{self, EventsParams, MAX_LINE, Response, code, method};
use crate::store::EventRecord;
use crate::sys::peer::check_peer;

const MAX_CONNECTIONS: usize = 64;
const IDLE: Duration = Duration::from_secs(60);
/// A follower that does not take a line within this time is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const FOLLOW_PAGE: u32 = 256;

/// `cataly slot`: one request held for the life of the connection (LANE req. 21, COORD req. 42-46).
/// Items report the place in the queue and the grant; after the grant the client sends `{"pid": n}`
/// once its command started. The end of the connection withdraws a waiter, and frees a grant once
/// its command has exited too.
async fn slot(
    hub: &Arc<Hub>,
    id: u64,
    params: &serde_json::Value,
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &mut OwnedWriteHalf,
) {
    use crate::lanes::slots::Place;
    let refuse = |m: String| Response::err(id, code::REFUSED, m);
    let name = params
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    if !crate::config::coord::slot_name_ok(&name) {
        let _ = send(writer, &refuse("a slot name is a letter or digit, then letters, digits, '_', '.' or '-', 64 at most".into())).await;
        return;
    }
    let lane = params
        .get("lane")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    if let Some(e) = lane.as_deref().and_then(crate::lanes::record::key_error) {
        let _ = send(writer, &refuse(e)).await;
        return;
    }
    let priority = match params.get("priority").and_then(serde_json::Value::as_u64) {
        Some(p @ 1..=9) => u8::try_from(p).unwrap_or(5),
        Some(_) => {
            let _ = send(writer, &refuse("priority must be 1 to 9".into())).await;
            return;
        }
        None => match &lane {
            Some(k) => hub.lanes.priority_of(k).await.unwrap_or(5),
            None => 5,
        },
    };
    let label = lane.unwrap_or_else(|| "local".into());
    let slots = Arc::clone(&hub.lanes.slots);
    let pid = params
        .get("pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|p| u32::try_from(p).ok());
    let token = match pid {
        Some(p) => slots.hold(&name, &label, p),
        None => slots.request(&name, &label, priority),
    };
    let mut changes = slots.changes();
    let mut last = None;
    let mut probe = [0u8; 1];
    loop {
        let place = slots.place(&name, token);
        if last != Some(place) {
            let item = match place {
                Place::Waiting(n) => serde_json::json!({ "state": "waiting", "place": n }),
                Place::Granted => serde_json::json!({ "state": "granted" }),
                Place::Gone => serde_json::json!({ "state": "gone" }),
            };
            if !send(writer, &Response::item(id, item)).await || place == Place::Gone {
                slots.client_gone(&name, token);
                return;
            }
            last = Some(place);
        }
        if place == Place::Granted {
            break;
        }
        changes.borrow_and_update();
        tokio::select! {
            _ = changes.changed() => {}
            // Input before the grant, or the end of input, withdraws the request.
            _ = tokio::io::AsyncReadExt::read(reader, &mut probe) => {
                slots.client_gone(&name, token);
                return;
            }
        }
    }
    loop {
        match read_line(reader).await {
            Line::Text(text) => {
                let pid = serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|v| v.get("pid").and_then(serde_json::Value::as_u64))
                    .and_then(|p| u32::try_from(p).ok());
                if let Some(pid) = pid {
                    slots.started(&name, token, pid);
                }
            }
            Line::TooLong | Line::Closed => {
                slots.client_gone(&name, token);
                return;
            }
        }
    }
}

/// Binding failed.
#[derive(Debug)]
pub(crate) enum BindError {
    /// Another hub answers on the socket.
    Live,
    Io(std::io::Error),
    Fs(crate::sys::fs::FsError),
}

impl fmt::Display for BindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Live => f.write_str("another hub answers on the socket"),
            Self::Io(e) => write!(f, "cannot bind the socket: {e}"),
            Self::Fs(e) => e.fmt(f),
        }
    }
}

/// Binds the socket. An existing socket file is removed only when connecting to it is refused.
pub(crate) fn bind(path: &Path) -> Result<UnixListener, BindError> {
    if std::fs::symlink_metadata(path).is_ok() {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => return Err(BindError::Live),
            Err(e) if matches!(e.kind(), ErrorKind::ConnectionRefused | ErrorKind::NotFound) => {
                log::info(
                    "replacing a stale socket file",
                    &[("path", &path.display())],
                );
                std::fs::remove_file(path).map_err(BindError::Io)?;
            }
            Err(e) => return Err(BindError::Io(e)),
        }
    }
    let listener = UnixListener::bind(path).map_err(BindError::Io)?;
    crate::sys::fs::set_owner_only(path).map_err(BindError::Fs)?;
    Ok(listener)
}

/// The accept loop; ends when the hub shuts down.
pub(crate) async fn serve(
    hub: Arc<Hub>,
    listener: Arc<UnixListener>,
) -> Result<(), std::io::Error> {
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let own_uid = crate::sys::own_uid();
    let mut shutdown = hub.shutdown.clone();
    loop {
        let (stream, _) = tokio::select! {
            accepted = listener.accept() => accepted?,
            _ = shutdown.changed() => return Ok(()),
        };
        let peer = match check_peer(&stream, own_uid) {
            Ok(peer) => peer,
            Err(refused) => {
                // Closed without a reply.
                log::warn("control connection refused", &[("reason", &refused)]);
                continue;
            }
        };
        let Ok(permit) = Arc::clone(&slots).try_acquire_owned() else {
            let mut stream = stream;
            let line = Response::err(0, code::BUSY, "too many connections").to_line();
            let _ = tokio::time::timeout(Duration::from_secs(1), stream.write_all(line.as_bytes()))
                .await;
            continue;
        };
        log::debug("control connection", &[("pid", &peer.pid.unwrap_or(0))]);
        let hub = Arc::clone(&hub);
        tokio::spawn(async move {
            connection(&hub, stream, peer.pid).await;
            drop(permit);
        });
    }
}

pub(super) enum Line {
    Text(String),
    TooLong,
    Closed,
}

/// Reads one line of at most `MAX_LINE` bytes.
pub(super) async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> Line {
    let mut buf = Vec::new();
    loop {
        let chunk = match reader.fill_buf().await {
            Ok([]) | Err(_) => return Line::Closed,
            Ok(chunk) => chunk,
        };
        let (take, done) = match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        buf.extend_from_slice(chunk.get(..take).unwrap_or_default());
        reader.consume(take);
        if buf.len() > MAX_LINE + 1 {
            return Line::TooLong;
        }
        if done {
            buf.pop();
            return String::from_utf8(buf).map_or(Line::TooLong, Line::Text);
        }
    }
}

async fn send(writer: &mut OwnedWriteHalf, response: &Response) -> bool {
    let line = response.to_line();
    matches!(
        tokio::time::timeout(WRITE_TIMEOUT, writer.write_all(line.as_bytes())).await,
        Ok(Ok(()))
    )
}

/// Where a control connection's process stands (DESIGN 8): inside a session the hub runs, by its
/// own process or an ancestor being that session's OMP process, or outside every session.
struct Placement(Option<Authority>);

impl Placement {
    async fn of(hub: &Hub, pid: Option<i32>) -> Self {
        let Some(pid) = pid.and_then(|p| u32::try_from(p).ok()) else {
            return Self(None);
        };
        let chain = crate::sys::ancestry::chain(pid).await;
        if !chain.complete {
            log::debug("control caller ancestry incomplete", &[("pid", &pid)]);
        }
        Self(match hub.links.place(&chain.pids) {
            Some((_, Role::Lead)) => Some(Authority::Lead),
            Some((handle, Role::Lane)) => Some(Authority::Lane(
                hub.lanes.key_of(&handle).await.unwrap_or(handle),
            )),
            None => None,
        })
    }

    /// The caller's authority. A caller that carries a launch token runs inside a session; when its
    /// session cannot be placed it fails closed and is not the owner.
    fn authority(&self, launch: bool) -> Authority {
        match &self.0 {
            Some(role) => role.clone(),
            None if launch => Authority::Unplaced,
            None => Authority::Owner,
        }
    }
}

async fn connection(hub: &Arc<Hub>, stream: UnixStream, pid: Option<i32>) {
    let (read, writer) = stream.into_split();
    let mut writer = writer;
    let mut reader = BufReader::new(read);
    let mut placement: Option<Placement> = None;
    loop {
        let line = match tokio::time::timeout(IDLE, read_line(&mut reader)).await {
            Ok(Line::Text(line)) => line,
            Ok(Line::TooLong) => {
                let _ = send(
                    &mut writer,
                    &Response::err(0, code::BAD_REQUEST, "request line over 1 MiB"),
                )
                .await;
                return;
            }
            Ok(Line::Closed) | Err(_) => return,
        };
        if line.trim().is_empty() {
            continue;
        }
        let request = match proto::decode(&line) {
            Ok(request) => request,
            Err(reply) => {
                if !send(&mut writer, &reply).await {
                    return;
                }
                continue;
            }
        };
        if request.method == method::EVENTS_FOLLOW {
            follow(hub, request.id, request.params, &mut reader, &mut writer).await;
            return;
        }
        if request.method == method::SLOT {
            slot(hub, request.id, &request.params, &mut reader, &mut writer).await;
            return;
        }
        if request.method == super::bridge::HELLO {
            super::bridge::serve(hub, request.id, request.params, &mut reader, writer, pid).await;
            return;
        }
        if placement.is_none() {
            placement = Some(Placement::of(hub, pid).await);
        }
        let caller = placement.as_ref().map_or(Authority::Unplaced, |p| {
            p.authority(request.launch.is_some())
        });
        let started = Instant::now();
        let task_hub = Arc::clone(hub);
        let reply =
            tokio::spawn(async move { dispatch::dispatch(&task_hub, request, caller).await })
                .await
                .unwrap_or_else(|_| Response::err(0, code::INTERNAL, "request handler failed"));
        hub.control_latency.record(started.elapsed());
        if !send(&mut writer, &reply).await {
            return;
        }
    }
}

/// Streams events until the caller closes the connection: first those after `after` from the store,
/// then live ones. A follower that falls behind the live channel catches up from the store.
async fn follow(
    hub: &Arc<Hub>,
    id: u64,
    params: serde_json::Value,
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &mut OwnedWriteHalf,
) {
    let params: EventsParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => {
            let _ = send(writer, &Response::err(id, code::BAD_REQUEST, e.to_string())).await;
            return;
        }
    };
    let mut live = hub.jobs.stream().subscribe();
    let mut last = match params.after {
        Some(after) => after,
        None => match hub.store.read(crate::store::Reads::last_event_id).await {
            Ok(id) => id,
            Err(e) => {
                let _ = send(
                    writer,
                    &Response::err(id, e.class().as_str(), e.to_string()),
                )
                .await;
                return;
            }
        },
    };
    let mut catching_up = true;
    let mut closed = [0u8; 1];
    let mut stop = hub.shutdown.clone();
    loop {
        if *hub.shutdown.borrow() {
            return;
        }
        if catching_up {
            let after = last;
            let Ok(page) = hub
                .store
                .read(move |r| r.events_after(after, FOLLOW_PAGE))
                .await
            else {
                return;
            };
            catching_up = u32::try_from(page.len()).unwrap_or(u32::MAX) == FOLLOW_PAGE;
            for event in page {
                if !emit(writer, id, &event, &mut last).await {
                    return;
                }
            }
            continue;
        }
        tokio::select! {
            received = live.recv() => match received {
                Ok(event) if event.id > last => {
                    if !emit(writer, id, &event, &mut last).await {
                        return;
                    }
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => catching_up = true,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            // Any input or end of input from the follower ends the stream.
            _ = tokio::io::AsyncReadExt::read(reader, &mut closed) => return,
            _ = stop.changed() => return,
        }
    }
}

async fn emit(writer: &mut OwnedWriteHalf, id: u64, event: &EventRecord, last: &mut i64) -> bool {
    *last = event.id;
    let item = serde_json::to_value(event).unwrap_or_default();
    send(writer, &Response::item(id, item)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::fs::tests::scratch;
    use std::os::unix::fs::PermissionsExt;

    /// The child of [`stale_socket_replaced_live_one_refused`]: binds the socket and exits, leaving
    /// the file behind as a crashed hub would.
    #[test]
    #[ignore = "run by stale_socket_replaced_live_one_refused as a child process"]
    fn stale_socket_child() {
        let Ok(path) = std::env::var("CATALY_STALE_SOCKET") else {
            return;
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _listener = rt.block_on(async { bind(Path::new(&path)).unwrap() });
    }

    #[tokio::test]
    async fn stale_socket_replaced_live_one_refused() {
        let dir = scratch("sock");
        let live = dir.join("live.sock");
        let first = bind(&live).unwrap();
        assert!(matches!(bind(&live), Err(BindError::Live)));
        drop(first);
        // The stale file comes from a process that has exited. A listener closed in this process
        // is not reliably gone: another test thread that forks at that moment holds a copy of
        // every descriptor until its child calls exec, and the socket still accepts meanwhile.
        let path = dir.join("hub.sock");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "hub::socket::tests::stale_socket_child",
                "--ignored",
                "--test-threads=1",
            ])
            .env("CATALY_STALE_SOCKET", &path)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(path.exists(), "the file remains after its process is gone");
        let second = bind(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(second);
    }

    #[tokio::test]
    async fn lines_are_bounded() {
        let mut ok = BufReader::new(&b"{\"a\":1}\nrest"[..]);
        assert!(matches!(read_line(&mut ok).await, Line::Text(t) if t == "{\"a\":1}"));
        let long = vec![b'x'; MAX_LINE + 10];
        let mut big = BufReader::new(&long[..]);
        assert!(matches!(read_line(&mut big).await, Line::TooLong));
        let mut empty = BufReader::new(&b""[..]);
        assert!(matches!(read_line(&mut empty).await, Line::Closed));
    }
}
