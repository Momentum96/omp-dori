//! The worker-mode stream (docs/design/workers.md 2.2): one SSH session carries frames
//! `u32 length | u32 channel | u8 kind | payload`, the payload at most 1 MiB. Channel 0 is the worker
//! protocol; the worker opens every other channel (a bridge connection, a spawned OMP). Each channel
//! has 1 MiB of credit per direction, so one slow reader cannot stall the others or grow memory.
//! Both ends ping every 15 s; 45 s without a frame closes the stream, and with it every channel.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::sync::{Semaphore, mpsc, watch};

/// Largest payload of one frame.
pub(crate) const MAX_PAYLOAD: usize = 1024 * 1024;
/// Length, channel and kind.
const HEADER: usize = 9;
/// Bytes one side may send on a channel before the other side returns credit.
pub(crate) const CREDIT: u32 = 1024 * 1024;
/// Largest data frame this side sends: small frames keep channels fair on one stream.
const CHUNK: usize = 64 * 1024;
/// Frames queued for the writer.
const OUT_QUEUE: usize = 64;
/// Channels the other side may open that nobody has taken yet.
const ACCEPT_QUEUE: usize = 16;
/// The control channel.
pub(crate) const CONTROL: u32 = 0;

/// Ping and silence timings.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    pub(crate) ping: Duration,
    pub(crate) silent: Duration,
}

impl Timing {
    /// The bridge's own heartbeat rule.
    pub(crate) const STANDARD: Self = Self {
        ping: Duration::from_secs(15),
        silent: Duration::from_secs(45),
    };
}

/// A frame kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A new channel; the payload is a JSON header naming what it carries.
    Open,
    Data,
    /// Credit returned: the payload is a big-endian `u32` byte count.
    Credit,
    /// The sender writes no more on this channel.
    Eof,
    /// The channel is gone in both directions.
    Close,
    Ping,
}

impl Kind {
    fn byte(self) -> u8 {
        match self {
            Self::Open => 1,
            Self::Data => 2,
            Self::Credit => 3,
            Self::Eof => 4,
            Self::Close => 5,
            Self::Ping => 6,
        }
    }

    fn of(byte: u8) -> Option<Self> {
        Some(match byte {
            1 => Self::Open,
            2 => Self::Data,
            3 => Self::Credit,
            4 => Self::Eof,
            5 => Self::Close,
            6 => Self::Ping,
            _ => return None,
        })
    }
}

/// One frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Frame {
    pub(crate) channel: u32,
    pub(crate) kind: Kind,
    pub(crate) payload: Vec<u8>,
}

impl Frame {
    fn new(channel: u32, kind: Kind, payload: Vec<u8>) -> Self {
        Self {
            channel,
            kind,
            payload,
        }
    }
}

/// Why the stream cannot be read.
#[derive(Debug)]
pub(crate) enum FrameError {
    TooLarge(usize),
    UnknownKind(u8),
    Io(std::io::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(n) => write!(f, "a frame of {n} bytes is over the 1 MiB limit"),
            Self::UnknownKind(k) => write!(f, "unknown frame kind {k}"),
            Self::Io(e) => write!(f, "stream: {e}"),
        }
    }
}

/// Appends one encoded frame to `out`.
pub(crate) fn encode(frame: &Frame, out: &mut Vec<u8>) -> Result<(), FrameError> {
    let len = frame.payload.len();
    if len > MAX_PAYLOAD {
        return Err(FrameError::TooLarge(len));
    }
    let len = u32::try_from(len).map_err(|_| FrameError::TooLarge(len))?;
    out.reserve(HEADER + frame.payload.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&frame.channel.to_be_bytes());
    out.push(frame.kind.byte());
    out.extend_from_slice(&frame.payload);
    Ok(())
}

/// Reads one frame; `None` at a clean end of the stream between frames.
pub(crate) async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Frame>, FrameError> {
    let mut header = [0u8; HEADER];
    let mut got = 0;
    while got < HEADER {
        let n = reader
            .read(header.get_mut(got..).unwrap_or_default())
            .await
            .map_err(FrameError::Io)?;
        if n == 0 {
            return if got == 0 {
                Ok(None)
            } else {
                Err(FrameError::Io(std::io::ErrorKind::UnexpectedEof.into()))
            };
        }
        got += n;
    }
    let [l0, l1, l2, l3, c0, c1, c2, c3, kind] = header;
    let len = usize::try_from(u32::from_be_bytes([l0, l1, l2, l3])).unwrap_or(usize::MAX);
    if len > MAX_PAYLOAD {
        return Err(FrameError::TooLarge(len));
    }
    let kind = Kind::of(kind).ok_or(FrameError::UnknownKind(kind))?;
    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(FrameError::Io)?;
    Ok(Some(Frame::new(
        u32::from_be_bytes([c0, c1, c2, c3]),
        kind,
        payload,
    )))
}

/// What arrives for a channel's reader.
enum Inbound {
    Data(Vec<u8>),
    Eof,
}

/// One channel's state in the table.
struct Chan {
    inbound: mpsc::UnboundedSender<Inbound>,
    /// Credit this side may still spend sending.
    credit: Arc<Semaphore>,
    /// Bytes received and not yet handed on: the other side may never exceed [`CREDIT`].
    unread: Arc<AtomicU32>,
}

struct Shared {
    out: mpsc::Sender<Frame>,
    channels: Mutex<HashMap<u32, Chan>>,
    next: AtomicU32,
    closed: watch::Sender<bool>,
    accept: mpsc::Sender<(u32, Value, DuplexStream)>,
}

impl Shared {
    fn table(&self) -> std::sync::MutexGuard<'_, HashMap<u32, Chan>> {
        self.channels.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Ends the stream: every channel sees its end, every waiter wakes.
    fn shut(&self) {
        let _ = self.closed.send(true);
        for (_, chan) in self.table().drain() {
            chan.credit.close();
        }
    }

    fn forget(&self, id: u32) {
        if let Some(chan) = self.table().remove(&id) {
            chan.credit.close();
        }
    }
}

/// A channel opened by the other side.
pub(crate) struct Accepted {
    pub(crate) channel: u32,
    pub(crate) header: Value,
    pub(crate) stream: DuplexStream,
}

/// One end of a worker-mode stream.
#[derive(Clone)]
pub(crate) struct Mux {
    shared: Arc<Shared>,
}

impl Mux {
    /// Starts the reader and writer over the stream. Returns the mux, the control channel and the
    /// channels the other side opens.
    pub(crate) fn start<R, W>(
        read: R,
        write: W,
        timing: Timing,
    ) -> (Self, DuplexStream, mpsc::Receiver<Accepted>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out, out_rx) = mpsc::channel(OUT_QUEUE);
        let (accept, accept_rx) = mpsc::channel(ACCEPT_QUEUE);
        let shared = Arc::new(Shared {
            out,
            channels: Mutex::new(HashMap::new()),
            next: AtomicU32::new(1),
            closed: watch::channel(false).0,
            accept,
        });
        let mux = Self { shared };
        let control = mux.attach(CONTROL);
        tokio::spawn(writer(write, out_rx, Arc::clone(&mux.shared), timing.ping));
        tokio::spawn(reader(read, Arc::clone(&mux.shared), timing.silent));
        let (tx, rx) = mpsc::channel(ACCEPT_QUEUE);
        let mut incoming = accept_rx;
        tokio::spawn(async move {
            while let Some((channel, header, stream)) = incoming.recv().await {
                let accepted = Accepted {
                    channel,
                    header,
                    stream,
                };
                if tx.send(accepted).await.is_err() {
                    break;
                }
            }
        });
        (mux, control, rx)
    }

    /// Opens a channel with a header naming what it carries.
    pub(crate) async fn open(&self, header: &Value) -> Result<(u32, DuplexStream), String> {
        if self.is_closed() {
            return Err("the worker stream is closed".into());
        }
        let id = self.shared.next.fetch_add(1, Ordering::Relaxed);
        let payload = serde_json::to_vec(header).map_err(|e| e.to_string())?;
        let stream = self.attach(id);
        self.shared
            .out
            .send(Frame::new(id, Kind::Open, payload))
            .await
            .map_err(|_| "the worker stream is closed".to_string())?;
        Ok((id, stream))
    }

    /// Whether the stream has ended.
    pub(crate) fn is_closed(&self) -> bool {
        *self.shared.closed.borrow()
    }

    /// Waits until the stream ends.
    pub(crate) async fn closed(&self) {
        let mut rx = self.shared.closed.subscribe();
        let _ = rx.wait_for(|c| *c).await;
    }

    /// Ends the stream now.
    pub(crate) fn close(&self) {
        self.shared.shut();
    }

    /// Registers channel `id` and starts its two pumps; returns the user's end.
    fn attach(&self, id: u32) -> DuplexStream {
        let (user, far) = tokio::io::duplex(CHUNK);
        let (far_read, far_write) = tokio::io::split(far);
        let (inbound, inbound_rx) = mpsc::unbounded_channel();
        let credit = Arc::new(Semaphore::new(CREDIT as usize));
        let unread = Arc::new(AtomicU32::new(0));
        self.shared.table().insert(
            id,
            Chan {
                inbound,
                credit: Arc::clone(&credit),
                unread: Arc::clone(&unread),
            },
        );
        // Each direction's end counts once; the second removes the channel.
        let ends = Arc::new(AtomicU8::new(0));
        tokio::spawn(deliver(
            id,
            inbound_rx,
            far_write,
            (Arc::clone(&self.shared), unread),
            Arc::clone(&ends),
        ));
        tokio::spawn(send(id, far_read, Arc::clone(&self.shared), credit, ends));
        user
    }
}

/// One direction of channel `id` ended; the second end removes it from the table.
fn ended(shared: &Shared, id: u32, ends: &AtomicU8) {
    if ends.fetch_add(1, Ordering::AcqRel) == 1 {
        shared.forget(id);
    }
}

/// Hands received bytes to the channel's user, returning credit once the user took them.
async fn deliver(
    id: u32,
    mut inbound: mpsc::UnboundedReceiver<Inbound>,
    mut far: tokio::io::WriteHalf<DuplexStream>,
    (shared, unread): (Arc<Shared>, Arc<AtomicU32>),
    ends: Arc<AtomicU8>,
) {
    while let Some(item) = inbound.recv().await {
        match item {
            Inbound::Data(bytes) => {
                let n = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
                if far.write_all(&bytes).await.is_err() {
                    // The user dropped its end: the other side stops sending.
                    let _ = shared
                        .out
                        .send(Frame::new(id, Kind::Close, Vec::new()))
                        .await;
                    shared.forget(id);
                    return;
                }
                unread.fetch_sub(n, Ordering::AcqRel);
                let credit = Frame::new(id, Kind::Credit, n.to_be_bytes().to_vec());
                if shared.out.send(credit).await.is_err() {
                    break;
                }
            }
            Inbound::Eof => {
                let _ = far.shutdown().await;
                ended(&shared, id, &ends);
                return;
            }
        }
    }
    // The channel was closed, or the stream ended: the user reads the end.
    let _ = far.shutdown().await;
}

/// Sends what the channel's user writes, within the credit the other side gave.
async fn send(
    id: u32,
    mut far: tokio::io::ReadHalf<DuplexStream>,
    shared: Arc<Shared>,
    credit: Arc<Semaphore>,
    ends: Arc<AtomicU8>,
) {
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = match far.read(&mut buf).await {
            Ok(0) | Err(_) => {
                let _ = shared.out.send(Frame::new(id, Kind::Eof, Vec::new())).await;
                ended(&shared, id, &ends);
                return;
            }
            Ok(n) => n,
        };
        let Ok(permits) = credit
            .acquire_many(u32::try_from(n).unwrap_or(u32::MAX))
            .await
        else {
            // The channel or the stream is gone.
            return;
        };
        permits.forget();
        let data = buf.get(..n).unwrap_or_default().to_vec();
        if shared
            .out
            .send(Frame::new(id, Kind::Data, data))
            .await
            .is_err()
        {
            return;
        }
    }
}

async fn writer<W: AsyncWrite + Unpin>(
    mut write: W,
    mut frames: mpsc::Receiver<Frame>,
    shared: Arc<Shared>,
    ping: Duration,
) {
    let mut closed = shared.closed.subscribe();
    let mut bytes = Vec::with_capacity(CHUNK + HEADER);
    loop {
        let frame = tokio::select! {
            f = frames.recv() => match f {
                Some(f) => f,
                None => break,
            },
            () = tokio::time::sleep(ping) => Frame::new(CONTROL, Kind::Ping, Vec::new()),
            _ = closed.wait_for(|c| *c) => break,
        };
        bytes.clear();
        if encode(&frame, &mut bytes).is_err() {
            break;
        }
        let written = tokio::time::timeout(ping * 3, async {
            write.write_all(&bytes).await?;
            write.flush().await
        })
        .await;
        if !matches!(written, Ok(Ok(()))) {
            break;
        }
    }
    let _ = write.shutdown().await;
    shared.shut();
}

async fn reader<R: AsyncRead + Unpin>(mut read: R, shared: Arc<Shared>, silent: Duration) {
    let mut closed = shared.closed.subscribe();
    loop {
        let frame = tokio::select! {
            f = tokio::time::timeout(silent, read_frame(&mut read)) => match f {
                Ok(Ok(Some(frame))) => frame,
                // End of stream, a bad frame, or silence: the stream is over.
                _ => break,
            },
            _ = closed.wait_for(|c| *c) => break,
        };
        if !dispatch(&shared, frame).await {
            break;
        }
    }
    shared.shut();
}

/// Applies one received frame; `false` when the other side broke the protocol.
async fn dispatch(shared: &Arc<Shared>, frame: Frame) -> bool {
    let Frame {
        channel,
        kind,
        payload,
    } = frame;
    match kind {
        Kind::Ping => true,
        Kind::Open => {
            if shared.table().contains_key(&channel) {
                return false;
            }
            let header = serde_json::from_slice(&payload).unwrap_or(Value::Null);
            let mux = Mux {
                shared: Arc::clone(shared),
            };
            let stream = mux.attach(channel);
            if shared.accept.try_send((channel, header, stream)).is_err() {
                let _ = shared
                    .out
                    .send(Frame::new(channel, Kind::Close, Vec::new()))
                    .await;
                shared.forget(channel);
            }
            true
        }
        Kind::Data => {
            let table = shared.table();
            let Some(chan) = table.get(&channel) else {
                // Data racing a close this side sent.
                return true;
            };
            let n = u32::try_from(payload.len()).unwrap_or(u32::MAX);
            let unread = chan.unread.fetch_add(n, Ordering::AcqRel).saturating_add(n);
            if unread > CREDIT {
                return false;
            }
            let _ = chan.inbound.send(Inbound::Data(payload));
            true
        }
        Kind::Credit => {
            let Ok(bytes) = <[u8; 4]>::try_from(payload.as_slice()) else {
                return false;
            };
            let n = u32::from_be_bytes(bytes) as usize;
            if let Some(chan) = shared.table().get(&channel) {
                if chan.credit.available_permits().saturating_add(n) > CREDIT as usize {
                    return false;
                }
                chan.credit.add_permits(n);
            }
            true
        }
        Kind::Eof => {
            if let Some(chan) = shared.table().get(&channel) {
                let _ = chan.inbound.send(Inbound::Eof);
            }
            true
        }
        Kind::Close => {
            shared.forget(channel);
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fast() -> Timing {
        Timing {
            ping: Duration::from_millis(50),
            silent: Duration::from_millis(400),
        }
    }

    /// Two muxes joined by in-memory pipes: (worker, hub, hub's accepted channels).
    fn pair(
        timing: Timing,
    ) -> (
        (Mux, DuplexStream),
        (Mux, DuplexStream),
        mpsc::Receiver<Accepted>,
    ) {
        let (a, b) = tokio::io::duplex(256 * 1024);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let (worker, wc, _) = Mux::start(ar, aw, timing);
        let (hub, hc, accepted) = Mux::start(br, bw, timing);
        ((worker, wc), (hub, hc), accepted)
    }

    #[tokio::test]
    async fn frames_round_trip_and_oversize_is_refused() {
        for (channel, kind, len) in [
            (0, Kind::Ping, 0),
            (7, Kind::Data, 1),
            (u32::MAX, Kind::Open, 300),
            (3, Kind::Data, MAX_PAYLOAD),
        ] {
            let frame = Frame::new(channel, kind, vec![0xA5; len]);
            let mut bytes = Vec::new();
            encode(&frame, &mut bytes).unwrap();
            assert_eq!(bytes.len(), HEADER + len);
            let mut slice = bytes.as_slice();
            assert_eq!(read_frame(&mut slice).await.unwrap().unwrap(), frame);
            assert!(read_frame(&mut slice).await.unwrap().is_none());
        }
        let big = Frame::new(1, Kind::Data, vec![0; MAX_PAYLOAD + 1]);
        assert!(matches!(
            encode(&big, &mut Vec::new()),
            Err(FrameError::TooLarge(_))
        ));
        let mut forged = Vec::new();
        forged.extend_from_slice(&u32::try_from(MAX_PAYLOAD + 1).unwrap().to_be_bytes());
        forged.extend_from_slice(&[0, 0, 0, 1, 2]);
        assert!(matches!(
            read_frame(&mut forged.as_slice()).await,
            Err(FrameError::TooLarge(_))
        ));
        let unknown = [0, 0, 0, 0, 0, 0, 0, 1, 99];
        assert!(matches!(
            read_frame(&mut unknown.as_slice()).await,
            Err(FrameError::UnknownKind(99))
        ));
        let cut = [0, 0, 0, 4, 0, 0];
        assert!(matches!(
            read_frame(&mut cut.as_slice()).await,
            Err(FrameError::Io(_))
        ));
    }

    #[tokio::test]
    async fn channels_carry_bytes_both_ways_and_end_with_eof() {
        let ((worker, mut wc), (_hub, mut hc), mut accepted) = pair(Timing::STANDARD);
        wc.write_all(b"hello\n").await.unwrap();
        let mut buf = [0u8; 6];
        hc.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello\n");
        let (id, mut opened) = worker.open(&json!({ "kind": "bridge" })).await.unwrap();
        let mut got = accepted.recv().await.unwrap();
        assert_eq!(
            (got.channel, got.header["kind"].as_str()),
            (id, Some("bridge"))
        );
        got.stream.write_all(b"ping").await.unwrap();
        got.stream.shutdown().await.unwrap();
        let mut text = String::new();
        opened.read_to_string(&mut text).await.unwrap();
        assert_eq!(text, "ping");
        opened.write_all(b"pong").await.unwrap();
        drop(opened);
        let mut back = String::new();
        got.stream.read_to_string(&mut back).await.unwrap();
        assert_eq!(back, "pong");
    }

    /// A reader that takes nothing holds the sender at 1 MiB on that channel, while another
    /// channel still flows; once read, everything arrives in order.
    #[tokio::test]
    async fn credit_bounds_one_channel_without_stalling_others() {
        let ((worker, _wc), (_hub, mut hc), mut accepted) = pair(Timing::STANDARD);
        let (_, mut slow) = worker.open(&json!({})).await.unwrap();
        let mut slow_far = accepted.recv().await.unwrap().stream;
        let total = 3 * CREDIT as usize;
        let writer = tokio::spawn(async move {
            let data: Vec<u8> = (0..total).map(|i| u8::try_from(i % 251).unwrap()).collect();
            slow.write_all(&data).await.unwrap();
            slow.shutdown().await.unwrap();
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!writer.is_finished(), "the sender waits for credit");
        let (_, mut other) = worker.open(&json!({})).await.unwrap();
        let mut other_far = accepted.recv().await.unwrap().stream;
        other.write_all(b"through").await.unwrap();
        let mut buf = [0u8; 7];
        other_far.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"through");
        let mut received = Vec::new();
        slow_far.read_to_end(&mut received).await.unwrap();
        assert_eq!(received.len(), total);
        assert!(
            received
                .iter()
                .enumerate()
                .all(|(i, b)| usize::from(*b) == i % 251)
        );
        writer.await.unwrap();
        // The control channel is untouched.
        drop(hc.write_all(b"x").await);
    }

    #[tokio::test]
    async fn silence_closes_the_stream_and_every_channel() {
        let (a, b) = tokio::io::duplex(1024);
        let (ar, aw) = tokio::io::split(a);
        let (mux, mut control, _) = Mux::start(ar, aw, fast());
        // The far end never writes a frame (not even a ping).
        let _keep = b;
        tokio::time::timeout(Duration::from_secs(5), mux.closed())
            .await
            .unwrap();
        assert!(mux.is_closed());
        let mut rest = Vec::new();
        control.read_to_end(&mut rest).await.unwrap();
        assert!(mux.open(&json!({})).await.is_err());
    }

    #[tokio::test]
    async fn pings_keep_an_idle_stream_open() {
        let ((worker, _wc), (hub, _hc), _accepted) = pair(fast());
        tokio::time::sleep(Duration::from_millis(1000)).await;
        assert!(!worker.is_closed() && !hub.is_closed());
        hub.close();
        tokio::time::timeout(Duration::from_secs(5), worker.closed())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_sender_over_its_credit_breaks_the_stream() {
        let (a, b) = tokio::io::duplex(4 * 1024 * 1024);
        let (ar, aw) = tokio::io::split(a);
        let (mux, _control, _) = Mux::start(ar, aw, Timing::STANDARD);
        let (_br, mut bw) = tokio::io::split(b);
        let mut bytes = Vec::new();
        for _ in 0..17 {
            encode(&Frame::new(CONTROL, Kind::Data, vec![1; CHUNK]), &mut bytes).unwrap();
        }
        bw.write_all(&bytes).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), mux.closed())
            .await
            .unwrap();
    }
}
