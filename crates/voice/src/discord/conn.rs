//! One Discord voice connection: the voice WebSocket (gateway version 8, heartbeats with `seq_ack`),
//! the UDP socket with IP discovery, transport encryption, DAVE end-to-end encryption through davey,
//! Opus both ways and the owner's receive stream.
//!
//! Discord only supports end-to-end encrypted calls since 2026-03-01 (docs.discord.com, "Voice",
//! DAVE section), so Identify always offers the DAVE protocol version davey implements.

use std::collections::BTreeSet;
use std::num::NonZeroU16;
use std::sync::Arc;
use std::time::{Duration, Instant};

use davey::{DaveSession, MediaType, ProposalsOperationType};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval, sleep_until, timeout};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use super::receive::{Notice, Receiver};
use super::rtp::{self, Cipher};
use crate::host::{Io, VoiceHost};
use crate::opus::Encode;

const CONNECT: Duration = Duration::from_secs(10);
const WRITE: Duration = Duration::from_secs(3);
const DISCOVERY: Duration = Duration::from_secs(5);
const FRAME_MAX: usize = 1024 * 1024;
/// Silence frames sent when output stops (Discord's guidance against interpolation).
const SILENCE_FRAMES: u8 = 5;

type Socket = WebSocketStream<Box<dyn Io>>;

/// What starting a connection needs.
#[derive(Debug, Clone)]
pub(crate) struct Join {
    pub(crate) endpoint: String,
    pub(crate) token: String,
    pub(crate) session_id: String,
    pub(crate) guild: u64,
    pub(crate) bot: u64,
    pub(crate) owner: u64,
    pub(crate) channel: u64,
}

/// What the connection reports.
#[derive(Debug)]
pub(crate) enum ConnEvent {
    /// Session description received: audio can flow.
    Ready,
    /// Owner audio, 24 kHz mono, 20 ms.
    Audio(Vec<i16>),
    /// The connection ended; the manager decides about rejoining.
    Down(String),
    /// Voice cannot work (for example the server refuses DAVE or the encryption mode).
    Fatal(String),
}

/// Host and port of a voice endpoint; plain WebSocket only for a loopback host (tests).
fn split(endpoint: &str) -> (bool, String, u16) {
    let (host, port) = match endpoint.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(443)),
        None => (endpoint.to_string(), 443),
    };
    let tls = !matches!(host.as_str(), "127.0.0.1" | "localhost");
    (tls, host, port)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0))
}

struct Conn {
    ws: Socket,
    join: Join,
    seq_ack: i64,
    udp: Option<UdpSocket>,
    ssrc: u32,
    cipher: Option<Cipher>,
    dave: Option<DaveSession>,
    dave_version: u16,
    known: BTreeSet<u64>,
    receiver: Receiver,
    encode: Encode,
    seq: u16,
    timestamp: u32,
    counter: u32,
    silence_left: u8,
    last_frame: Option<Instant>,
    events: mpsc::Sender<ConnEvent>,
    log: Arc<dyn Fn(&str) + Send + Sync>,
}

/// Runs one connection until it ends; the last event says how.
pub(crate) async fn run<H: VoiceHost>(
    host: Arc<H>,
    join: Join,
    events: mpsc::Sender<ConnEvent>,
    mut frames: mpsc::Receiver<Vec<i16>>,
) {
    let end = match open(&host, join, events.clone()).await {
        Ok(mut conn) => conn.serve(&mut frames).await,
        Err(e) => ConnEvent::Down(e),
    };
    let _ = events.send(end).await;
}

async fn open<H: VoiceHost>(
    host: &Arc<H>,
    join: Join,
    events: mpsc::Sender<ConnEvent>,
) -> Result<Conn, String> {
    let (tls, h, port) = split(&join.endpoint);
    let io = timeout(CONNECT, host.connect(tls, &h, port))
        .await
        .map_err(|_| "voice connect timed out".to_string())??;
    let scheme = if tls { "wss" } else { "ws" };
    let url = format!("{scheme}://{h}:{port}/?v=8");
    let config = WebSocketConfig::default()
        .max_message_size(Some(FRAME_MAX))
        .max_frame_size(Some(FRAME_MAX));
    let (ws, _) = timeout(
        CONNECT,
        tokio_tungstenite::client_async_with_config(url, io, Some(config)),
    )
    .await
    .map_err(|_| "voice handshake timed out".to_string())?
    .map_err(|e| format!("voice handshake failed: {e}"))?;
    Ok(Conn {
        ws,
        join,
        seq_ack: -1,
        udp: None,
        ssrc: 0,
        cipher: None,
        dave: None,
        dave_version: 0,
        known: BTreeSet::new(),
        receiver: Receiver::new(),
        encode: Encode::new()?,
        seq: 0,
        timestamp: 0,
        counter: 0,
        silence_left: 0,
        last_frame: None,
        events,
        log: {
            let host = Arc::clone(host);
            Arc::new(move |m: &str| host.log(m))
        },
    })
}

impl Conn {
    async fn send(&mut self, v: &Value) -> Result<(), String> {
        match timeout(WRITE, self.ws.send(Message::text(v.to_string()))).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!("voice write failed: {e}")),
            Err(_) => Err("voice write took over 3 s".into()),
        }
    }

    async fn send_binary(&mut self, op: u8, payload: &[u8]) -> Result<(), String> {
        let mut msg = Vec::with_capacity(1 + payload.len());
        msg.push(op);
        msg.extend_from_slice(payload);
        match timeout(WRITE, self.ws.send(Message::binary(msg))).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!("voice write failed: {e}")),
            Err(_) => Err("voice write took over 3 s".into()),
        }
    }

    async fn serve(&mut self, frames: &mut mpsc::Receiver<Vec<i16>>) -> ConnEvent {
        let mut beat: Option<tokio::time::Interval> = None;
        let mut second = interval(Duration::from_secs(1));
        second.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut buf = vec![0u8; 2048];
        let mut silence = interval(Duration::from_millis(20));
        silence.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = async {
                    match beat.as_mut() {
                        Some(b) => { b.tick().await; }
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    let v = json!({"op": 3, "d": {"t": now_ms(), "seq_ack": self.seq_ack}});
                    if let Err(e) = self.send(&v).await {
                        return ConnEvent::Down(e);
                    }
                }
                incoming = self.ws.next() => {
                    let msg = match incoming {
                        None => return ConnEvent::Down("the voice server closed the socket".into()),
                        Some(Err(e)) => return ConnEvent::Down(format!("voice socket error: {e}")),
                        Some(Ok(m)) => m,
                    };
                    let step = match msg {
                        Message::Text(t) => self.on_text(t.as_str(), &mut beat).await,
                        Message::Binary(b) => self.on_binary(&b).await,
                        Message::Close(c) => {
                            let code = c.map_or(0, |c| u16::from(c.code));
                            return close_event(code);
                        }
                        _ => Ok(()),
                    };
                    if let Err(end) = step {
                        return end;
                    }
                }
                got = recv_udp(self.udp.as_ref(), &mut buf) => {
                    if let Some(n) = got
                        && let Some(packet) = buf.get(..n)
                    {
                        let packet = packet.to_vec();
                        self.on_packet(&packet);
                    }
                }
                frame = frames.recv() => {
                    let Some(frame) = frame else {
                        return ConnEvent::Down("the call ended".into());
                    };
                    self.play(&frame).await;
                }
                _ = silence.tick() => {
                    let idle = self.last_frame.is_some_and(|t| t.elapsed() >= Duration::from_millis(60));
                    if idle && self.silence_left > 0 {
                        self.silence_left -= 1;
                        self.send_opus(&davey::OPUS_SILENCE_PACKET).await;
                    }
                }
                _ = second.tick() => {
                    self.receiver.tick(Instant::now());
                    for n in self.receiver.notices() {
                        match n {
                            Notice::Stalled => (self.log)("voice receive stalled; stream restarted"),
                            Notice::DecryptBurst(k) => (self.log)(&format!("voice decryption failed {k} times")),
                            Notice::Resubscribed => {}
                        }
                    }
                }
            }
        }
    }

    async fn on_text(
        &mut self,
        text: &str,
        beat: &mut Option<tokio::time::Interval>,
    ) -> Result<(), ConnEvent> {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return Ok(());
        };
        if let Some(s) = v.get("seq").and_then(Value::as_i64) {
            self.seq_ack = s;
        }
        let d = v.get("d").unwrap_or(&Value::Null);
        match v.get("op").and_then(Value::as_u64) {
            Some(8) => {
                let ms = d["heartbeat_interval"]
                    .as_f64()
                    .unwrap_or(41_250.0)
                    .max(1000.0);
                let mut b = interval(Duration::from_secs_f64(ms / 1000.0));
                b.set_missed_tick_behavior(MissedTickBehavior::Delay);
                *beat = Some(b);
                let identify = json!({"op": 0, "d": {
                    "server_id": self.join.guild.to_string(),
                    "user_id": self.join.bot.to_string(),
                    "session_id": self.join.session_id,
                    "token": self.join.token,
                    "max_dave_protocol_version": davey::DAVE_PROTOCOL_VERSION,
                }});
                self.send(&identify).await.map_err(ConnEvent::Down)?;
            }
            Some(2) => self.on_ready(d).await?,
            Some(4) => self.on_description(d).await?,
            Some(5) => {
                let user = id(&d["user_id"]);
                if user == Some(self.join.owner)
                    && let Some(ssrc) = d["ssrc"].as_u64().and_then(|s| u32::try_from(s).ok())
                {
                    self.receiver.owner_ssrc(ssrc, Instant::now());
                    self.receiver
                        .speaking(d["speaking"].as_u64().unwrap_or(0) & 1 != 0);
                    let wants = json!({"op": 15, "d": {"any": 0, ssrc.to_string(): 100}});
                    self.send(&wants).await.map_err(ConnEvent::Down)?;
                }
            }
            Some(11) => {
                for u in d["user_ids"].as_array().into_iter().flatten() {
                    if let Some(u) = id(u) {
                        self.known.insert(u);
                    }
                }
            }
            Some(13) => {
                if let Some(u) = id(&d["user_id"]) {
                    self.known.remove(&u);
                }
            }
            Some(21) => {
                let tid = transition(d);
                let version = d["protocol_version"].as_u64().unwrap_or(0);
                if let Some(dave) = self.dave.as_mut() {
                    let _ = dave.prepare_transition(tid, u16::try_from(version).unwrap_or(0));
                }
                if tid != 0 {
                    self.ready_for(tid).await?;
                }
            }
            Some(22) => {
                let tid = transition(d);
                if let Some(dave) = self.dave.as_mut()
                    && dave.execute_transition(tid).unwrap_or(false)
                {
                    self.receiver.group_changed(Instant::now());
                }
            }
            Some(24) => {
                let version =
                    u16::try_from(d["protocol_version"].as_u64().unwrap_or(0)).unwrap_or(0);
                if d["epoch"].as_u64() == Some(1) {
                    self.dave_version = version;
                    self.restart_dave().await?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn ready_for(&mut self, tid: u16) -> Result<(), ConnEvent> {
        self.send(&json!({"op": 23, "d": {"transition_id": tid}}))
            .await
            .map_err(ConnEvent::Down)
    }

    /// A fresh local group and key package (new call, invalid commit or welcome).
    async fn restart_dave(&mut self) -> Result<(), ConnEvent> {
        let Some(version) = NonZeroU16::new(self.dave_version) else {
            self.dave = None;
            return Ok(());
        };
        let mut session = match self.dave.take() {
            Some(mut s) => match s.reinit(version, self.join.bot, self.join.channel, None) {
                Ok(()) => s,
                Err(e) => return Err(ConnEvent::Fatal(format!("DAVE reinit failed: {e}"))),
            },
            None => DaveSession::new(version, self.join.bot, self.join.channel, None)
                .map_err(|e| ConnEvent::Fatal(format!("DAVE setup failed: {e}")))?,
        };
        let package = session
            .create_key_package()
            .map_err(|e| ConnEvent::Fatal(format!("DAVE key package failed: {e}")))?;
        self.dave = Some(session);
        self.send_binary(26, &package)
            .await
            .map_err(ConnEvent::Down)
    }

    fn recognized(&self) -> Vec<u64> {
        let mut users: Vec<u64> = self.known.iter().copied().collect();
        users.push(self.join.bot);
        users
    }

    async fn on_binary(&mut self, msg: &[u8]) -> Result<(), ConnEvent> {
        let (Some(seq), Some(&op)) = (msg.get(..2), msg.get(2)) else {
            return Ok(());
        };
        self.seq_ack = i64::from(u16::from_be_bytes([
            seq.first().copied().unwrap_or(0),
            seq.get(1).copied().unwrap_or(0),
        ]));
        let payload = msg.get(3..).unwrap_or(&[]);
        let users = self.recognized();
        match op {
            25 => {
                if let Some(dave) = self.dave.as_mut() {
                    if dave.set_external_sender(payload).is_err() {
                        (self.log)("DAVE external sender refused");
                    }
                    if let Ok(package) = dave.create_key_package() {
                        self.send_binary(26, &package)
                            .await
                            .map_err(ConnEvent::Down)?;
                    }
                }
            }
            27 => {
                let (Some(&kind), Some(rest)) = (payload.first(), payload.get(1..)) else {
                    return Ok(());
                };
                let kind = if kind == 1 {
                    ProposalsOperationType::REVOKE
                } else {
                    ProposalsOperationType::APPEND
                };
                if let Some(dave) = self.dave.as_mut()
                    && let Ok(Some(cw)) = dave.process_proposals(kind, rest, Some(&users))
                {
                    let mut out = cw.commit;
                    if let Some(w) = cw.welcome {
                        out.extend_from_slice(&w);
                    }
                    self.send_binary(28, &out).await.map_err(ConnEvent::Down)?;
                }
            }
            29 | 30 => {
                let (Some(t), Some(rest)) = (payload.get(..2), payload.get(2..)) else {
                    return Ok(());
                };
                let tid = u16::from_be_bytes([
                    t.first().copied().unwrap_or(0),
                    t.get(1).copied().unwrap_or(0),
                ]);
                let version = self.dave_version;
                let ok = match self.dave.as_mut() {
                    Some(dave) => {
                        let processed = if op == 29 {
                            dave.process_commit(rest).map(|_| ()).is_ok()
                        } else {
                            dave.process_welcome(rest, Some(&users)).is_ok()
                        };
                        processed && dave.prepare_transition(tid, version).is_ok()
                    }
                    None => false,
                };
                if ok {
                    if tid == 0 {
                        self.receiver.group_changed(Instant::now());
                    } else {
                        self.ready_for(tid).await?;
                    }
                } else {
                    self.send(&json!({"op": 31, "d": {"transition_id": tid}}))
                        .await
                        .map_err(ConnEvent::Down)?;
                    self.restart_dave().await?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn on_ready(&mut self, d: &Value) -> Result<(), ConnEvent> {
        let ssrc = d["ssrc"].as_u64().and_then(|s| u32::try_from(s).ok());
        let ip = d["ip"].as_str();
        let port = d["port"].as_u64().and_then(|p| u16::try_from(p).ok());
        let (Some(ssrc), Some(ip), Some(port)) = (ssrc, ip, port) else {
            return Err(ConnEvent::Down("incomplete voice READY".into()));
        };
        let modes: Vec<String> = d["modes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m.as_str().map(str::to_string))
            .collect();
        let Some(mode) = rtp::choose(&modes) else {
            return Err(ConnEvent::Fatal(
                "no supported voice encryption mode".into(),
            ));
        };
        let udp = UdpSocket::bind(if ip.contains(':') {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        })
        .await
        .map_err(|e| ConnEvent::Down(format!("UDP bind failed: {}", e.kind())))?;
        udp.connect((ip, port))
            .await
            .map_err(|e| ConnEvent::Down(format!("UDP connect failed: {}", e.kind())))?;
        let request = rtp::discovery_request(ssrc);
        let mut buf = [0u8; 128];
        let discovered = timeout(DISCOVERY, async {
            udp.send(&request).await.ok()?;
            let n = udp.recv(&mut buf).await.ok()?;
            rtp::discovery_response(buf.get(..n)?)
        })
        .await
        .ok()
        .flatten();
        let Some((address, local_port)) = discovered else {
            return Err(ConnEvent::Down("UDP IP discovery failed".into()));
        };
        self.ssrc = ssrc;
        self.udp = Some(udp);
        let select = json!({"op": 1, "d": {"protocol": "udp",
            "data": {"address": address, "port": local_port, "mode": mode}}});
        self.send(&select).await.map_err(ConnEvent::Down)
    }

    async fn on_description(&mut self, d: &Value) -> Result<(), ConnEvent> {
        let mode = d["mode"].as_str().unwrap_or("");
        let key: Vec<u8> = d["secret_key"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|b| b.as_u64().and_then(|b| u8::try_from(b).ok()))
            .collect();
        let Some(cipher) = Cipher::new(mode, &key) else {
            return Err(ConnEvent::Fatal(
                "unusable voice encryption key or mode".into(),
            ));
        };
        self.cipher = Some(cipher);
        self.dave_version =
            u16::try_from(d["dave_protocol_version"].as_u64().unwrap_or(0)).unwrap_or(0);
        if self.dave_version > 0 {
            self.restart_dave().await?;
        }
        let speaking = json!({"op": 5, "d": {"speaking": 1, "delay": 0, "ssrc": self.ssrc}});
        self.send(&speaking).await.map_err(ConnEvent::Down)?;
        self.events
            .send(ConnEvent::Ready)
            .await
            .map_err(|_| ConnEvent::Down("the call ended".into()))
    }

    fn on_packet(&mut self, packet: &[u8]) {
        let Some(cipher) = self.cipher.as_ref() else {
            return;
        };
        let Ok(r) = rtp::open(cipher, packet) else {
            return;
        };
        let owner = self.join.owner;
        let now = Instant::now();
        let audio = match self.dave.as_mut().filter(|d| d.is_ready()) {
            Some(dave) => {
                let mut decrypt =
                    |p: &[u8]| -> Option<Vec<u8>> { dave.decrypt(owner, MediaType::AUDIO, p).ok() };
                self.receiver
                    .frame(r.ssrc, &r.frame, Some(&mut decrypt), now)
            }
            None => self.receiver.frame(r.ssrc, &r.frame, None, now),
        };
        if let Some(a) = audio {
            let _ = self.events.try_send(ConnEvent::Audio(a));
        }
    }

    async fn play(&mut self, frame: &[i16]) {
        let Some(opus) = self.encode.encode(frame) else {
            return;
        };
        self.last_frame = Some(Instant::now());
        self.silence_left = SILENCE_FRAMES;
        self.send_opus(&opus).await;
    }

    async fn send_opus(&mut self, opus: &[u8]) {
        let (Some(cipher), Some(udp)) = (self.cipher.as_ref(), self.udp.as_ref()) else {
            return;
        };
        let frame = match self.dave.as_mut() {
            Some(dave) if dave.can_encrypt() => match dave.encrypt_opus(opus) {
                Ok(f) => f.into_owned(),
                Err(_) => return,
            },
            // DAVE negotiated but not ready yet: send nothing rather than plaintext.
            Some(_) => return,
            None => opus.to_vec(),
        };
        self.seq = self.seq.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(960);
        self.counter = self.counter.wrapping_add(1);
        if let Some(p) = rtp::seal(
            cipher,
            self.seq,
            self.timestamp,
            self.ssrc,
            &frame,
            self.counter,
        ) {
            let _ = udp.send(&p).await;
        }
    }
}

async fn recv_udp(udp: Option<&UdpSocket>, buf: &mut [u8]) -> Option<usize> {
    match udp {
        Some(u) => u.recv(buf).await.ok(),
        None => std::future::pending().await,
    }
}

fn id(v: &Value) -> Option<u64> {
    v.as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| v.as_u64())
}

fn transition(d: &Value) -> u16 {
    d["transition_id"]
        .as_u64()
        .and_then(|t| u16::try_from(t).ok())
        .unwrap_or(0)
}

/// Close codes: DAVE or mode refusals and authentication failures are fatal; others are a drop the
/// manager may recover from by rejoining.
fn close_event(code: u16) -> ConnEvent {
    match code {
        4004 => ConnEvent::Fatal("voice authentication failed (4004)".into()),
        4016 => ConnEvent::Fatal("unknown encryption mode (4016)".into()),
        4017 => ConnEvent::Fatal("the channel requires DAVE (4017)".into()),
        4021 => ConnEvent::Fatal("voice rate limited (4021)".into()),
        4022 => ConnEvent::Fatal("call terminated (4022)".into()),
        c => ConnEvent::Down(format!("voice socket closed ({c})")),
    }
}

/// When to give up waiting: used by the manager's deadline.
pub(crate) fn deadline(from: Instant, after: Duration) -> tokio::time::Instant {
    tokio::time::Instant::from_std(from + after)
}

/// Sleeps until `at` or forever.
pub(crate) async fn until(at: Option<tokio::time::Instant>) {
    match at {
        Some(t) => sleep_until(t).await,
        None => std::future::pending().await,
    }
}
