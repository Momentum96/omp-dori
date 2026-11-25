//! A fake Discord voice server on loopback: the voice WebSocket (gateway version 8: hello, ready,
//! session description, heartbeat ACKs, speaking) and the UDP side (IP discovery, transport-encrypted
//! RTP with `aead_aes256_gcm_rtpsize`). It can announce a DAVE protocol version without ever forming
//! the group, so frames stay unencrypted end to end. Tests send the owner's Opus frames, other
//! speakers and undecodable frames, and count the bot's packets.

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::missing_panics_doc,
    clippy::cast_possible_truncation
)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt as _, StreamExt as _};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use serde_json::{Value, json};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const MODE: &str = "aead_aes256_gcm_rtpsize";
const KEY: [u8; 32] = [7; 32];
/// The bot's SSRC.
pub const BOT_SSRC: u32 = 1;

#[derive(Default)]
struct State {
    identifies: Vec<Value>,
    selects: Vec<Value>,
    wants: Vec<Value>,
    key_packages: usize,
    client: Option<SocketAddr>,
    /// RTP packets from the bot, with their arrival time.
    out: Vec<Instant>,
    ws: Option<mpsc::UnboundedSender<Value>>,
    connections: usize,
}

/// A running fake voice server.
pub struct VoiceServer {
    /// `host:port` as Discord's voice-server update names it.
    pub endpoint: String,
    state: Arc<Mutex<State>>,
    udp: Arc<UdpSocket>,
    runtime: tokio::runtime::Handle,
    seq: Mutex<(u16, u32, u32)>,
}

impl VoiceServer {
    /// Starts the fake on `runtime`; `dave` is the DAVE protocol version it announces (0: none).
    pub fn start(runtime: &tokio::runtime::Runtime, dave: u16) -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let (tcp, udp) = runtime.block_on(async {
            (
                TcpListener::bind("127.0.0.1:0").await.unwrap(),
                Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap()),
            )
        });
        let endpoint = tcp.local_addr().unwrap().to_string();
        let udp_port = udp.local_addr().unwrap().port();
        let shared = Arc::clone(&state);
        runtime.spawn(async move {
            while let Ok((stream, _)) = tcp.accept().await {
                let state = Arc::clone(&shared);
                tokio::spawn(socket(stream, state, udp_port, dave));
            }
        });
        let (shared, socket_udp) = (Arc::clone(&state), Arc::clone(&udp));
        runtime.spawn(async move {
            let mut buf = vec![0u8; 2048];
            while let Ok((n, from)) = socket_udp.recv_from(&mut buf).await {
                let p = &buf[..n];
                if n == 74 && p[..2] == [0, 1] {
                    let mut r = vec![0u8; 74];
                    r[..2].copy_from_slice(&[0, 2]);
                    r[2..4].copy_from_slice(&70u16.to_be_bytes());
                    r[4..8].copy_from_slice(&p[4..8]);
                    let ip = from.ip().to_string();
                    r[8..8 + ip.len()].copy_from_slice(ip.as_bytes());
                    r[72..74].copy_from_slice(&from.port().to_be_bytes());
                    shared.lock().unwrap().client = Some(from);
                    let _ = socket_udp.send_to(&r, from).await;
                } else {
                    shared.lock().unwrap().out.push(Instant::now());
                }
            }
        });
        Self {
            endpoint,
            state,
            udp,
            runtime: runtime.handle().clone(),
            seq: Mutex::new((0, 0, 0)),
        }
    }

    /// Sends a frame on the voice WebSocket.
    pub fn send(&self, frame: Value) {
        if let Some(tx) = &self.state.lock().unwrap().ws {
            let _ = tx.send(frame);
        }
    }

    /// Op 5: `user` speaks (or stops) with `ssrc`.
    pub fn speaking(&self, user: &str, ssrc: u32, on: bool) {
        self.send(json!({"op": 5, "d": {"user_id": user, "ssrc": ssrc, "speaking": u8::from(on)}}));
    }

    /// Whether the client finished the handshake (its UDP address is known).
    pub fn ready(&self) -> bool {
        self.state.lock().unwrap().client.is_some()
    }

    pub fn identifies(&self) -> Vec<Value> {
        self.state.lock().unwrap().identifies.clone()
    }

    pub fn wants(&self) -> Vec<Value> {
        self.state.lock().unwrap().wants.clone()
    }

    pub fn key_packages(&self) -> usize {
        self.state.lock().unwrap().key_packages
    }

    pub fn connections(&self) -> usize {
        self.state.lock().unwrap().connections
    }

    /// Arrival times of the bot's RTP packets.
    pub fn out(&self) -> Vec<Instant> {
        self.state.lock().unwrap().out.clone()
    }

    /// Sends Opus frames from `ssrc`, transport-encrypted, one every 20 ms.
    pub fn play(&self, ssrc: u32, frames: &[Vec<u8>]) {
        let to = self.state.lock().unwrap().client.expect("client address");
        let packets: Vec<Vec<u8>> = frames.iter().map(|f| self.seal(ssrc, f)).collect();
        let udp = Arc::clone(&self.udp);
        self.runtime.block_on(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(20));
            for p in packets {
                tick.tick().await;
                let _ = udp.send_to(&p, to).await;
            }
        });
    }

    fn seal(&self, ssrc: u32, frame: &[u8]) -> Vec<u8> {
        let (seq, ts, counter) = {
            let mut s = self.seq.lock().unwrap();
            s.0 = s.0.wrapping_add(1);
            s.1 = s.1.wrapping_add(960);
            s.2 = s.2.wrapping_add(1);
            *s
        };
        let mut header = [0u8; 12];
        header[0] = 0x80;
        header[1] = 0x78;
        header[2..4].copy_from_slice(&seq.to_be_bytes());
        header[4..8].copy_from_slice(&ts.to_be_bytes());
        header[8..12].copy_from_slice(&ssrc.to_be_bytes());
        let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &KEY).unwrap());
        let mut nonce = [0u8; 12];
        nonce[..4].copy_from_slice(&counter.to_be_bytes());
        let mut body = frame.to_vec();
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(&header),
            &mut body,
        )
        .unwrap();
        let mut packet = header.to_vec();
        packet.extend_from_slice(&body);
        packet.extend_from_slice(&counter.to_be_bytes());
        packet
    }
}

async fn socket(stream: tokio::net::TcpStream, state: Arc<Mutex<State>>, udp_port: u16, dave: u16) {
    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    {
        let mut s = state.lock().unwrap();
        s.ws = Some(tx);
        s.connections += 1;
    }
    let hello = json!({"op": 8, "d": {"heartbeat_interval": 5000.0}});
    if ws.send(Message::text(hello.to_string())).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            incoming = ws.next() => {
                let Some(Ok(msg)) = incoming else { return };
                let reply = match msg {
                    Message::Binary(b) => {
                        if b.first() == Some(&26) {
                            state.lock().unwrap().key_packages += 1;
                        }
                        None
                    }
                    Message::Text(t) => {
                        let Ok(v) = serde_json::from_str::<Value>(t.as_str()) else { continue };
                        let d = v["d"].clone();
                        match v["op"].as_u64() {
                            Some(0) => {
                                state.lock().unwrap().identifies.push(d);
                                Some(json!({"op": 2, "d": {"ssrc": BOT_SSRC, "ip": "127.0.0.1",
                                    "port": udp_port, "modes": [MODE, "xsalsa20_poly1305"]}}))
                            }
                            Some(1) => {
                                state.lock().unwrap().selects.push(d);
                                Some(json!({"op": 4, "d": {"mode": MODE, "secret_key": KEY.to_vec(),
                                    "dave_protocol_version": dave}}))
                            }
                            Some(3) => Some(json!({"op": 6, "d": {"t": d["t"]}})),
                            Some(15) => {
                                state.lock().unwrap().wants.push(d);
                                None
                            }
                            _ => None,
                        }
                    }
                    Message::Close(_) => return,
                    _ => None,
                };
                if let Some(r) = reply
                    && ws.send(Message::text(r.to_string())).await.is_err()
                {
                    return;
                }
            }
            out = rx.recv() => {
                let Some(out) = out else { return };
                if ws.send(Message::text(out.to_string())).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// The recording as Opus: `ms` of the owner's speech (`loud`) or room noise, 48 kHz stereo frames.
pub fn opus(ms: u32, loud: bool, start: usize) -> Vec<Vec<u8>> {
    use audiopus::coder::Encoder;
    use audiopus::{Application, Channels, SampleRate};
    let encoder = Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Voip).unwrap();
    (0..(ms / 20) as usize)
        .map(|i| {
            let mono = super::voice_host::frame(loud, start + i);
            let stereo: Vec<i16> = mono.iter().flat_map(|s| [*s, *s, *s, *s]).collect();
            let mut out = vec![0u8; 1500];
            let n = encoder.encode(&stereo, &mut out).unwrap();
            out.truncate(n);
            out
        })
        .collect()
}
