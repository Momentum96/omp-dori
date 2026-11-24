//! The OMP rpc-ui transport (DESIGN 3.3, 10.1): newline-framed JSON within the advertised physical
//! frame limit, protocol v2 `rpc_chunk` reassembly within the advertised reassembly limit, and
//! request correlation by one reader task that owns stdout and a map of pending request IDs.
//! Events are forwarded to an optional bounded queue; when it is full they are dropped and counted.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;

/// Largest physical frame accepted, newline included (OMP's own cap).
pub(crate) const PHYSICAL_LIMIT: usize = 1024 * 1024;
/// Largest reassembled v2 frame.
pub(crate) const LOGICAL_LIMIT: usize = 64 * PHYSICAL_LIMIT;
/// Bound for one response and for one write to stdin (DESIGN 9.4).
pub(crate) const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);

/// A transport failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RpcError {
    /// Reading or writing the pipe failed.
    Io(String),
    /// OMP sent something that breaks the protocol.
    Protocol(String),
    /// OMP closed its stdout.
    Closed,
    /// No answer in time.
    Timeout(&'static str),
    /// A command would exceed the advertised frame limit.
    TooLarge,
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "rpc pipe: {e}"),
            Self::Protocol(e) => write!(f, "rpc protocol: {e}"),
            Self::Closed => f.write_str("OMP closed its rpc output"),
            Self::Timeout(what) => write!(f, "no {what} from OMP in time"),
            Self::TooLarge => f.write_str("rpc command exceeds OMP's frame limit"),
        }
    }
}

impl std::error::Error for RpcError {}

fn protocol(message: &str) -> RpcError {
    RpcError::Protocol(message.to_string())
}

/// What a frame is, from its `type` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Ready,
    Response,
    Chunk,
    /// Anything else: session events, dialogs, `rpc_frame_error`.
    Event,
}

/// One logical frame.
#[derive(Debug, Clone)]
pub(crate) struct Frame {
    pub(crate) kind: Kind,
    pub(crate) value: Value,
}

fn kind_of(value: &Value) -> Kind {
    match value.get("type").and_then(Value::as_str) {
        Some("ready") => Kind::Ready,
        Some("response") => Kind::Response,
        Some("rpc_chunk") => Kind::Chunk,
        _ => Kind::Event,
    }
}

/// Reads logical frames from OMP's stdout.
pub(crate) struct Frames<R> {
    reader: BufReader<R>,
    physical: usize,
    logical: usize,
    chunking: bool,
}

impl<R: AsyncRead + Unpin> Frames<R> {
    pub(crate) fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
            physical: PHYSICAL_LIMIT,
            logical: LOGICAL_LIMIT,
            chunking: false,
        }
    }

    /// Applies the limits the ready frame advertised; never above OMP's documented caps.
    pub(crate) fn set_limits(&mut self, physical: usize, logical: usize) -> Result<(), RpcError> {
        if physical == 0 || logical == 0 {
            return Err(protocol("OMP advertised invalid frame limits"));
        }
        self.physical = PHYSICAL_LIMIT.min(physical);
        self.logical = LOGICAL_LIMIT.min(logical);
        Ok(())
    }

    /// Accepts `rpc_chunk` sequences from now on (after protocol 2 was negotiated).
    pub(crate) fn enable_chunking(&mut self) {
        self.chunking = true;
    }

    pub(crate) async fn read(&mut self) -> Result<Frame, RpcError> {
        let line = read_line(&mut self.reader, self.physical).await?;
        let value: Value =
            serde_json::from_slice(&line).map_err(|_| protocol("a frame is not JSON"))?;
        let kind = kind_of(&value);
        if kind != Kind::Chunk {
            return Ok(Frame { kind, value });
        }
        if !self.chunking {
            return Err(protocol(
                "rpc_chunk frames before protocol 2 was negotiated",
            ));
        }
        let first = Chunk::from(&value)?;
        if first.byte_length > self.logical
            || first.id.is_empty()
            || first.index != 0
            || first.count == 0
            || first.count > first.byte_length
        {
            return Err(protocol("rpc_chunk sequence bounds"));
        }
        let mut bytes = Vec::new();
        first.append(&mut bytes)?;
        for index in 1..first.count {
            let line = read_line(&mut self.reader, self.physical).await?;
            let value: Value =
                serde_json::from_slice(&line).map_err(|_| protocol("a chunk is not JSON"))?;
            if kind_of(&value) != Kind::Chunk {
                return Err(protocol("interleaved rpc_chunk sequence"));
            }
            let chunk = Chunk::from(&value)?;
            if chunk.index != index
                || chunk.id != first.id
                || chunk.count != first.count
                || chunk.byte_length != first.byte_length
            {
                return Err(protocol("inconsistent rpc_chunk sequence"));
            }
            chunk.append(&mut bytes)?;
        }
        if bytes.len() != first.byte_length {
            return Err(protocol("incomplete rpc_chunk data"));
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| protocol("chunks are not UTF-8"))?;
        let value: Value =
            serde_json::from_str(text).map_err(|_| protocol("reassembled frame is not JSON"))?;
        let kind = kind_of(&value);
        if kind == Kind::Chunk {
            return Err(protocol("nested rpc_chunk sequence"));
        }
        Ok(Frame { kind, value })
    }
}

struct Chunk<'a> {
    id: &'a str,
    index: usize,
    count: usize,
    byte_length: usize,
    data: &'a str,
}

impl<'a> Chunk<'a> {
    fn from(value: &'a Value) -> Result<Self, RpcError> {
        let number = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| protocol("rpc_chunk fields"))
        };
        Ok(Self {
            id: value
                .get("chunkId")
                .and_then(Value::as_str)
                .ok_or_else(|| protocol("rpc_chunk fields"))?,
            index: number("index")?,
            count: number("count")?,
            byte_length: number("byteLength")?,
            data: value
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| protocol("rpc_chunk fields"))?,
        })
    }

    fn append(&self, bytes: &mut Vec<u8>) -> Result<(), RpcError> {
        let segment = base64_decode(self.data).ok_or_else(|| protocol("invalid base64 chunk"))?;
        if segment.is_empty() || bytes.len() + segment.len() > self.byte_length {
            return Err(protocol("rpc_chunk exceeds its declared length"));
        }
        bytes.extend_from_slice(&segment);
        Ok(())
    }
}

async fn read_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    max: usize,
) -> Result<Vec<u8>, RpcError> {
    let mut bytes = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|e| RpcError::Io(e.to_string()))?;
        if available.is_empty() {
            return Err(RpcError::Closed);
        }
        let end = available.iter().position(|b| *b == b'\n');
        let consumed = end.map_or(available.len(), |i| i + 1);
        if bytes.len() + consumed > max {
            return Err(protocol("a frame exceeds the physical frame limit"));
        }
        bytes.extend_from_slice(available.get(..end.unwrap_or(consumed)).unwrap_or_default());
        reader.consume(consumed);
        if end.is_some() {
            return Ok(bytes);
        }
    }
}

/// Strict standard base64 with padding, as OMP's chunks use it.
pub(crate) fn base64_decode(text: &str) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let (quads, rest) = text.as_bytes().as_chunks::<4>();
    if !rest.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(quads.len() * 3);
    let last = quads.len().checked_sub(1);
    for (i, quad) in quads.iter().enumerate() {
        let pad = quad.iter().rev().take_while(|c| **c == b'=').count();
        if pad > 2 || (pad > 0 && Some(i) != last) {
            return None;
        }
        let mut n: u32 = 0;
        for (j, c) in quad.iter().enumerate() {
            let v = if j >= 4 - pad { 0 } else { value(*c)? };
            n = (n << 6) | v;
        }
        let [_, a, b, c] = n.to_be_bytes();
        out.push(a);
        // Unused low bits must be zero, so every input has one decoding.
        match pad {
            0 => {
                out.push(b);
                out.push(c);
            }
            1 if c == 0 => out.push(b),
            2 if b == 0 && c == 0 => {}
            _ => return None,
        }
    }
    Some(out)
}

/// The ready frame's advertised facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ready {
    pub(crate) protocol_version: u64,
    pub(crate) supported: Vec<u64>,
    pub(crate) max_frame_bytes: Option<u64>,
    pub(crate) max_reassembled_frame_bytes: Option<u64>,
    pub(crate) bash_exclude_from_context: bool,
}

impl Ready {
    fn parse(value: &Value) -> Result<Self, RpcError> {
        let protocol_version = value
            .get("protocolVersion")
            .and_then(Value::as_u64)
            .ok_or_else(|| protocol("ready frame without protocolVersion"))?;
        Ok(Self {
            protocol_version,
            supported: value
                .get("supportedProtocolVersions")
                .and_then(Value::as_array)
                .map_or_else(Vec::new, |a| a.iter().filter_map(Value::as_u64).collect()),
            max_frame_bytes: value.get("maxFrameBytes").and_then(Value::as_u64),
            max_reassembled_frame_bytes: value
                .get("maxReassembledFrameBytes")
                .and_then(Value::as_u64),
            bash_exclude_from_context: value
                .get("supportsBashExcludeFromContext")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
}

/// A command's answer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Reply {
    pub(crate) success: bool,
    pub(crate) data: Option<Value>,
    pub(crate) error: Option<String>,
    /// The command name OMP echoed.
    pub(crate) command: Option<String>,
}

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Reply, RpcError>>>>>;
type Writer = Box<dyn AsyncWrite + Send + Unpin>;

/// An rpc-ui connection.
pub(crate) struct Rpc {
    writer: AsyncMutex<Option<Writer>>,
    pending: Pending,
    next_id: AtomicU64,
    physical: usize,
    reader: JoinHandle<()>,
    ready: Ready,
    negotiated: u64,
    dropped_events: Arc<AtomicU64>,
}

impl Rpc {
    /// Reads the ready frame (within `ready_within`), which must offer protocol 1, starts the reader
    /// task and negotiates protocol 2 when offered. Events go to `events` when given.
    pub(crate) async fn connect<R, W>(
        reader: R,
        writer: W,
        ready_within: Duration,
        events: Option<mpsc::Sender<Value>>,
    ) -> Result<Self, RpcError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut frames = Frames::new(reader);
        let first = timeout(ready_within, frames.read())
            .await
            .map_err(|_| RpcError::Timeout("ready frame"))??;
        if first.kind != Kind::Ready {
            return Err(protocol("the first frame is not a ready frame"));
        }
        let ready = Ready::parse(&first.value)?;
        if ready.protocol_version != 1 {
            return Err(RpcError::Protocol(format!(
                "OMP starts with rpc protocol {}, this hub needs 1",
                ready.protocol_version
            )));
        }
        let as_usize = |n: Option<u64>, default| {
            n.map_or(Some(default), |n| usize::try_from(n).ok())
                .unwrap_or(default)
        };
        frames.set_limits(
            as_usize(ready.max_frame_bytes, PHYSICAL_LIMIT),
            as_usize(ready.max_reassembled_frame_bytes, LOGICAL_LIMIT),
        )?;
        let physical = frames.physical;
        let pending: Pending = Arc::default();
        let dropped_events = Arc::new(AtomicU64::new(0));
        let reader = tokio::spawn(read_loop(
            frames,
            Arc::clone(&pending),
            events,
            Arc::clone(&dropped_events),
        ));
        let mut rpc = Self {
            writer: AsyncMutex::new(Some(Box::new(writer))),
            pending,
            next_id: AtomicU64::new(0),
            physical,
            reader,
            negotiated: 1,
            ready,
            dropped_events,
        };
        if rpc.ready.supported.contains(&2) {
            let mut command = Map::new();
            command.insert("type".into(), "negotiate_protocol".into());
            command.insert("protocolVersion".into(), 2.into());
            let reply = rpc.call(command, RESPONSE_TIMEOUT).await?;
            let confirmed = reply
                .data
                .as_ref()
                .and_then(|d| d.get("protocolVersion"))
                .and_then(Value::as_u64);
            if !reply.success || confirmed != Some(2) {
                return Err(protocol("OMP offered protocol 2 but did not confirm it"));
            }
            rpc.negotiated = 2;
        }
        Ok(rpc)
    }

    pub(crate) fn ready(&self) -> &Ready {
        &self.ready
    }

    /// The protocol in use: 1, or 2 after negotiation.
    pub(crate) fn protocol(&self) -> u64 {
        self.negotiated
    }

    /// Events dropped because the queue was full or absent.
    pub(crate) fn dropped_events(&self) -> u64 {
        self.dropped_events.load(Ordering::Relaxed)
    }

    /// Sends one command (an object with its `type`) and waits for the response with its ID.
    pub(crate) async fn call(
        &self,
        command: Map<String, Value>,
        within: Duration,
    ) -> Result<Reply, RpcError> {
        self.call_at(command, within).await.0
    }

    /// Like [`Rpc::call`], and also returns when the command line was completely written, which is
    /// `None` when nothing reached OMP's stdin. A reply whose echoed command differs from the one
    /// sent ends in a protocol error (HOST req. 30).
    pub(crate) async fn call_at(
        &self,
        mut command: Map<String, Value>,
        within: Duration,
    ) -> (Result<Reply, RpcError>, Option<Instant>) {
        let id = format!(
            "cataly-{}",
            self.next_id.fetch_add(1, Ordering::Relaxed) + 1
        );
        let kind = command
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        command.insert("id".into(), Value::String(id.clone()));
        let line = match encode_line(&command, self.physical) {
            Ok(line) => line,
            Err(e) => return (Err(e), None),
        };
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.clone(), tx);
        let forget = || {
            self.pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
        };
        if self.reader.is_finished() {
            forget();
            return (Err(RpcError::Closed), None);
        }
        if let Err(e) = self.write(&line).await {
            forget();
            return (Err(e), None);
        }
        let written = Instant::now();
        let result = match timeout(within, rx).await {
            Ok(Ok(Ok(reply))) if reply.command.as_deref() != Some(kind.as_str()) => Err(protocol(
                "a response echoed a different command than the one sent",
            )),
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(RpcError::Closed),
            Err(_) => {
                forget();
                Err(RpcError::Timeout("response"))
            }
        };
        (result, Some(written))
    }

    /// Writes one frame that has no response, such as an `extension_ui_response`.
    pub(crate) async fn send(&self, frame: &Map<String, Value>) -> Result<(), RpcError> {
        if self.reader.is_finished() {
            return Err(RpcError::Closed);
        }
        let line = encode_line(frame, self.physical)?;
        self.write(&line).await
    }

    /// Whether OMP's output ended (the reader task stopped).
    pub(crate) fn closed(&self) -> bool {
        self.reader.is_finished()
    }

    async fn write(&self, line: &[u8]) -> Result<(), RpcError> {
        let mut guard = self.writer.lock().await;
        let writer = guard.as_mut().ok_or(RpcError::Closed)?;
        timeout(WRITE_TIMEOUT, async {
            writer.write_all(line).await?;
            writer.flush().await
        })
        .await
        .map_err(|_| RpcError::Timeout("write to stdin"))?
        .map_err(|e| RpcError::Io(e.to_string()))
    }

    /// Closes stdin, OMP's signal to drain and exit. The reader task keeps draining stdout until OMP
    /// closes it.
    pub(crate) async fn end_input(&self) {
        let mut guard = self.writer.lock().await;
        if let Some(mut writer) = guard.take() {
            let _ = timeout(WRITE_TIMEOUT, writer.shutdown()).await;
        }
    }
}

impl Drop for Rpc {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    mut frames: Frames<R>,
    pending: Pending,
    events: Option<mpsc::Sender<Value>>,
    dropped: Arc<AtomicU64>,
) {
    let failure = loop {
        let frame = match frames.read().await {
            Ok(frame) => frame,
            Err(e) => break e,
        };
        match frame.kind {
            Kind::Response => {
                let Some(id) = frame.value.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let success = frame.value.get("success").and_then(Value::as_bool) == Some(true);
                let command = frame.value.get("command").and_then(Value::as_str);
                let data = frame.value.get("data").cloned();
                if success
                    && command == Some("negotiate_protocol")
                    && data
                        .as_ref()
                        .and_then(|d| d.get("protocolVersion"))
                        .and_then(Value::as_u64)
                        == Some(2)
                {
                    // OMP chunks only frames written after this response.
                    frames.enable_chunking();
                }
                let reply = Reply {
                    success,
                    data,
                    error: frame
                        .value
                        .get("error")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    command: command.map(str::to_string),
                };
                let waiter = pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(id);
                if let Some(waiter) = waiter {
                    let _ = waiter.send(Ok(reply));
                }
            }
            Kind::Event | Kind::Ready => {
                let delivered = events
                    .as_ref()
                    .is_some_and(|tx| tx.try_send(frame.value).is_ok());
                if !delivered {
                    dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            Kind::Chunk => {}
        }
    };
    let waiters: Vec<_> = pending
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .drain()
        .collect();
    for (_, waiter) in waiters {
        let _ = waiter.send(Err(failure.clone()));
    }
}

/// One command line: JSON plus newline, within the physical frame limit.
fn encode_line(frame: &Map<String, Value>, physical: usize) -> Result<Vec<u8>, RpcError> {
    let mut line = serde_json::to_vec(frame).map_err(|e| RpcError::Protocol(e.to_string()))?;
    line.push(b'\n');
    if line.len() > physical {
        return Err(RpcError::TooLarge);
    }
    Ok(line)
}

/// Builds a command object from its type and fields.
pub(crate) fn command(kind: &str, fields: &[(&str, Value)]) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("type".into(), Value::String(kind.into()));
    for (k, v) in fields {
        map.insert((*k).to_string(), v.clone());
    }
    map
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, duplex};

    pub(crate) fn encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    fn chunks(payload: &[u8], split: usize) -> [String; 2] {
        let (head, tail) = payload.split_at(split);
        [(0, head), (1, tail)].map(|(index, part)| {
            format!(
                "{}\n",
                json!({
                    "type": "rpc_chunk", "chunkId": "proof", "index": index,
                    "count": 2, "byteLength": payload.len(), "data": encode(part),
                })
            )
        })
    }

    #[test]
    fn base64_is_strict() {
        for sample in [&b""[..], b"f", b"fo", b"foo", b"foob", "한글".as_bytes()] {
            assert_eq!(base64_decode(&encode(sample)).unwrap(), sample);
        }
        assert_eq!(base64_decode("Zm9v").unwrap(), b"foo");
        for bad in ["Zm9", "Zm=v", "Zm9v=", "Zh==", "Zm9-", "Zg==Zg=="] {
            assert!(base64_decode(bad).is_none(), "{bad}");
        }
    }

    #[tokio::test]
    async fn reassembles_utf8_split_inside_a_character() {
        let payload = r#"{"type":"response","text":"한글"}"#.as_bytes();
        let split = payload.iter().position(|b| !b.is_ascii()).unwrap() + 1;
        let stream = chunks(payload, split).concat();
        let mut frames = Frames::new(stream.as_bytes());
        frames.enable_chunking();
        let frame = frames.read().await.unwrap();
        assert_eq!(frame.kind, Kind::Response);
        assert_eq!(frame.value["text"], "한글");
    }

    #[tokio::test]
    async fn rejects_an_event_between_chunks_and_chunks_before_negotiation() {
        let [first, second] = chunks(br#"{"type":"response"}"#, 4);
        let stream = format!("{first}{{\"type\":\"agent_end\"}}\n{second}");
        let mut frames = Frames::new(stream.as_bytes());
        frames.enable_chunking();
        let error = frames.read().await.unwrap_err();
        assert!(error.to_string().contains("interleaved"), "{error}");
        let mut plain = Frames::new(first.as_bytes());
        assert!(
            plain
                .read()
                .await
                .unwrap_err()
                .to_string()
                .contains("negotiated")
        );
    }

    #[tokio::test]
    async fn enforces_physical_and_reassembly_limits() {
        let plain = b"{\"type\":\"response\"}\n";
        let mut frames = Frames::new(plain.as_slice());
        frames.set_limits(plain.len() - 1, 128).unwrap();
        let error = frames.read().await.unwrap_err();
        assert!(error.to_string().contains("physical"), "{error}");
        let stream = chunks(&plain[..plain.len() - 1], 4).concat();
        let mut frames = Frames::new(stream.as_bytes());
        frames.set_limits(1024, plain.len() - 2).unwrap();
        frames.enable_chunking();
        let error = frames.read().await.unwrap_err();
        assert!(error.to_string().contains("sequence bounds"), "{error}");
        assert!(frames.set_limits(0, 1).is_err());
    }

    /// A scripted OMP on the far end of a duplex pipe: answers every command by its type, out of
    /// order when asked, and sends an event before each answer.
    pub(crate) async fn fake_omp(
        server: tokio::io::DuplexStream,
        ready: Value,
        answer: fn(&Value) -> Value,
    ) {
        let (read, mut write) = tokio::io::split(server);
        let mut lines = BufReader::new(read).lines();
        write
            .write_all(format!("{ready}\n").as_bytes())
            .await
            .unwrap();
        let mut held: Option<Value> = None;
        while let Ok(Some(line)) = lines.next_line().await {
            let cmd: Value = serde_json::from_str(&line).unwrap();
            let mut reply = answer(&cmd);
            reply["type"] = "response".into();
            reply["id"] = cmd["id"].clone();
            reply["command"] = cmd["type"].clone();
            write
                .write_all(b"{\"type\":\"agent_start\"}\n")
                .await
                .unwrap();
            if cmd["type"] == "hold" {
                held = Some(reply);
                continue;
            }
            write
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
            if let Some(h) = held.take() {
                write.write_all(format!("{h}\n").as_bytes()).await.unwrap();
            }
        }
    }

    fn ready() -> Value {
        json!({"type": "ready", "protocolVersion": 1, "supportedProtocolVersions": [1, 2],
               "maxFrameBytes": 1_048_576, "maxReassembledFrameBytes": 67_108_864})
    }

    fn answer(cmd: &Value) -> Value {
        match cmd["type"].as_str() {
            Some("negotiate_protocol") => json!({"success": true, "data": {"protocolVersion": 2}}),
            Some("nope") => json!({"success": false, "error": "Unknown command: nope"}),
            _ => json!({"success": true, "data": {"echo": cmd["type"]}}),
        }
    }

    #[tokio::test]
    async fn correlates_out_of_order_answers_and_forwards_events() {
        let (client, server) = duplex(1 << 16);
        tokio::spawn(fake_omp(server, ready(), answer));
        let (read, write) = tokio::io::split(client);
        let (tx, mut rx) = mpsc::channel(16);
        let rpc = Arc::new(
            Rpc::connect(read, write, Duration::from_secs(5), Some(tx))
                .await
                .unwrap(),
        );
        assert_eq!(rpc.protocol(), 2);
        assert_eq!(rpc.ready().max_frame_bytes, Some(1_048_576));
        let held = {
            let rpc = Arc::clone(&rpc);
            tokio::spawn(async move { rpc.call(command("hold", &[]), RESPONSE_TIMEOUT).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let second = rpc
            .call(command("get_state", &[]), RESPONSE_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(second.data.unwrap()["echo"], "get_state");
        let first = held.await.unwrap().unwrap();
        assert_eq!(
            first.data.unwrap()["echo"],
            "hold",
            "answered after the later call"
        );
        let refused = rpc
            .call(command("nope", &[]), RESPONSE_TIMEOUT)
            .await
            .unwrap();
        assert!(!refused.success && refused.error.unwrap().contains("Unknown"));
        assert_eq!(rx.recv().await.unwrap()["type"], "agent_start");
    }

    #[tokio::test]
    async fn closed_output_fails_pending_calls_and_v1_stays_v1() {
        let (client, server) = duplex(1 << 16);
        let (read, write) = tokio::io::split(client);
        let (server_read, mut server_write) = tokio::io::split(server);
        server_write
            .write_all(b"{\"type\":\"ready\",\"protocolVersion\":1}\n")
            .await
            .unwrap();
        let rpc = Rpc::connect(read, write, Duration::from_secs(5), None)
            .await
            .unwrap();
        assert_eq!(rpc.protocol(), 1);
        let call = rpc.call(command("get_state", &[]), RESPONSE_TIMEOUT);
        let close = async {
            let mut lines = BufReader::new(server_read).lines();
            lines.next_line().await.unwrap();
            drop(server_write);
        };
        let (result, ()) = tokio::join!(call, close);
        assert_eq!(result.unwrap_err(), RpcError::Closed);
    }

    #[tokio::test]
    async fn a_non_ready_first_frame_or_protocol_two_start_is_refused() {
        for first in [
            r#"{"type":"agent_start"}"#,
            r#"{"type":"ready","protocolVersion":2}"#,
        ] {
            let (client, server) = duplex(1 << 12);
            let (read, write) = tokio::io::split(client);
            let (_r, mut w) = tokio::io::split(server);
            w.write_all(format!("{first}\n").as_bytes()).await.unwrap();
            let result = Rpc::connect(read, write, Duration::from_secs(5), None).await;
            assert!(matches!(result, Err(RpcError::Protocol(_))), "{first}");
        }
        let (client, _server) = duplex(1 << 12);
        let (read, write) = tokio::io::split(client);
        let result = Rpc::connect(read, write, Duration::from_millis(50), None).await;
        assert!(matches!(result, Err(RpcError::Timeout(_))));
    }
}
