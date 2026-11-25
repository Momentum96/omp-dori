//! A fake GPT-Live realtime server on loopback for both transports (`DESIGN.md` 10.4):
//!
//! - WebSocket: `GET /v1/live/sessions` upgraded, `session.start` first, base64 PCM audio events.
//! - WebRTC: `POST /v1/live/sessions` with the session and an SDP offer, answered by a str0m peer
//!   (ICE lite) that carries Opus audio and the `oai-events` data channel.
//! - `POST /v1/responses`: the summary model.
//!
//! One scripted [`Fake`] session logic serves both transports, so a test runs the same script over
//! either and compares what the client did.

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::missing_panics_doc,
    // A fake server with synthetic signals: casts are bounded by construction, and the scripted
    // session reads best as one long match.
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    clippy::struct_excessive_bools,
    clippy::result_large_err,
    clippy::needless_pass_by_value,
    clippy::naive_bytecount,
    clippy::redundant_closure_for_method_calls,
    clippy::collapsible_match,
    clippy::chunks_exact_to_as_chunks
)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use audiopus::coder::{Decoder, Encoder};
use audiopus::packet::Packet;
use audiopus::{Application, Channels, MutSignals, SampleRate};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use str0m::change::SdpOffer;
use str0m::media::{Frequency, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// What the fake does.
#[derive(Debug, Clone, Default)]
pub struct Script {
    /// Output audio per owner turn, in ms (0: say nothing).
    pub reply_ms: u32,
    /// Output audio when the session opens with a greeting.
    pub greet_ms: u32,
    /// Function calls emitted in one delegation on the first owner turn.
    pub tools: Vec<(String, String)>,
    /// Spacing in ms between the calls of that delegation and before its completion (0: all at
    /// once), so a call can be answered while its response is still running.
    pub call_gap_ms: u32,
    /// An error event right after `session.started`.
    pub fail_after_start: Option<String>,
    /// Never send `session.started`.
    pub never_start: bool,
    /// Acknowledge commentary appends.
    pub ack_commentary: bool,
    /// Speak a transcript covering a commentary append (else only a few characters).
    pub cover_commentary: bool,
    /// Output audio played for a commentary append, in ms.
    pub commentary_ms: u32,
    /// Answer only the first owner turn with audio.
    pub reply_once: bool,
}

/// What the fake saw.
#[derive(Debug, Default)]
pub struct Log {
    /// Session configurations, one per session.
    pub sessions: Vec<Value>,
    /// Client events other than audio, with the session index.
    pub events: Vec<(usize, Value)>,
    /// Input audio arrivals: session index, time, bytes.
    pub input: Vec<(usize, Instant, usize)>,
    /// Input audio bytes per session.
    pub input_bytes: Vec<Vec<u8>>,
    /// Output audio chunks sent: session index, time.
    pub output: Vec<(usize, Instant)>,
    /// Authorization headers seen.
    pub auth: Vec<String>,
    /// Summary requests.
    pub summaries: Vec<Value>,
}

impl Log {
    pub fn events_of(&self, kind: &str) -> Vec<Value> {
        self.events
            .iter()
            .filter(|(_, v)| v["type"] == kind)
            .map(|(_, v)| v.clone())
            .collect()
    }
}

/// A running fake.
pub struct Server {
    pub base: String,
    pub log: Arc<Mutex<Log>>,
}

/// Starts the fake on loopback.
pub async fn start(script: Script) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Log::default()));
    let (script, shared) = (Arc::new(script), Arc::clone(&log));
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (script, log) = (Arc::clone(&script), Arc::clone(&shared));
            tokio::spawn(async move { serve(stream, script, log).await });
        }
    });
    Server {
        base: format!("http://{addr}"),
        log,
    }
}

async fn serve(stream: TcpStream, script: Arc<Script>, log: Arc<Mutex<Log>>) {
    let mut head = [0u8; 4];
    if stream.peek(&mut head).await.is_err() {
        return;
    }
    if &head == b"GET " {
        websocket(stream, script, log).await;
    } else {
        post(stream, script, log).await;
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let len = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    while buf.len() < head_end + len {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Some((head, buf[head_end..head_end + len].to_vec()))
}

async fn respond(stream: &mut TcpStream, status: u16, body: &str) {
    let text = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(text.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn auth_of(head: &str) -> String {
    head.lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("authorization")
                .then(|| v.trim().to_string())
        })
        .unwrap_or_default()
}

async fn post(mut stream: TcpStream, script: Arc<Script>, log: Arc<Mutex<Log>>) {
    let Some((head, body)) = read_request(&mut stream).await else {
        return;
    };
    log.lock().unwrap().auth.push(auth_of(&head));
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    if head.starts_with("POST /v1/responses") {
        log.lock().unwrap().summaries.push(body);
        let answer = json!({"output": [{"type": "message", "content": [
            {"type": "output_text", "text": "Owner asked for the build status.\nThe lead got it."}]}],
            "usage": {"input_tokens": 100, "output_tokens": 20}});
        respond(&mut stream, 200, &answer.to_string()).await;
        return;
    }
    if !head.starts_with("POST /v1/live/sessions") {
        respond(&mut stream, 404, "{}").await;
        return;
    }
    let sdp = body["transport"]["sdp"].as_str().unwrap_or("").to_string();
    let index = {
        let mut l = log.lock().unwrap();
        l.sessions.push(body["session"].clone());
        l.input_bytes.push(Vec::new());
        l.sessions.len() - 1
    };
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let local = socket.local_addr().unwrap();
    let mut rtc = Rtc::builder()
        .set_ice_lite(true)
        .clear_codecs()
        .enable_opus(true, false)
        .build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
    let offer = SdpOffer::from_sdp_string(&sdp).unwrap();
    let answer = rtc.sdp_api().accept_offer(offer).unwrap();
    let reply = json!({"session": {"id": "live_fake"},
        "transport": {"type": "webrtc", "sdp": answer.to_sdp_string()}});
    respond(&mut stream, 201, &reply.to_string()).await;
    let fake = Fake::new(script, Arc::clone(&log), index, body["session"].clone());
    tokio::spawn(rtc_loop(rtc, socket, local, fake));
}

async fn websocket(stream: TcpStream, script: Arc<Script>, log: Arc<Mutex<Log>>) {
    let seen = Arc::clone(&log);
    let callback = move |req: &tokio_tungstenite::tungstenite::handshake::server::Request, resp| {
        let auth = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        seen.lock().unwrap().auth.push(auth);
        Ok(resp)
    };
    let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
        return;
    };
    let Some(Ok(Message::Text(first))) = ws.next().await else {
        return;
    };
    let start: Value = serde_json::from_str(first.as_str()).unwrap();
    assert_eq!(start["type"], "session.start");
    let index = {
        let mut l = log.lock().unwrap();
        l.sessions.push(start["session"].clone());
        l.input_bytes.push(Vec::new());
        l.sessions.len() - 1
    };
    let mut fake = Fake::new(script, log, index, start["session"].clone());
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    fake.opened(Instant::now());
    loop {
        for out in fake.take() {
            let msg = match out {
                Out::Event(v) => v.to_string(),
                Out::Audio(s) => {
                    let bytes: Vec<u8> = s.iter().flat_map(|x| x.to_le_bytes()).collect();
                    json!({"type": "session.output_audio.delta", "delta": b64(&bytes)}).to_string()
                }
                Out::Close => {
                    let _ = ws.close(None).await;
                    return;
                }
            };
            if ws.send(Message::text(msg)).await.is_err() {
                return;
            }
        }
        tokio::select! {
            m = ws.next() => {
                let Some(Ok(m)) = m else { return };
                let Message::Text(t) = m else { continue };
                // Arrival is stamped before parsing: decoding a large frame takes time here.
                let arrived = Instant::now();
                let v: Value = serde_json::from_str(t.as_str()).unwrap();
                if v["type"] == "session.input_audio.append" {
                    let bytes = unb64(v["audio"].as_str().unwrap());
                    fake.audio(&bytes, arrived);
                } else {
                    fake.event(v, Instant::now());
                }
            }
            _ = tick.tick() => fake.tick(Instant::now()),
        }
    }
}

async fn rtc_loop(mut rtc: Rtc, socket: UdpSocket, local: SocketAddr, mut fake: Fake) {
    let mut buf = vec![0u8; 2000];
    let mut channel = None;
    let mut mid: Option<Mid> = None;
    let mut dec = Decoder::new(SampleRate::Hz48000, Channels::Stereo).unwrap();
    let enc = Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Voip).unwrap();
    let mut pending_out: Vec<i16> = Vec::new();
    let mut sent: u64 = 0;
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<Event>();
    loop {
        // Drain.
        let deadline = loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(t)) => break t,
                Ok(Output::Transmit(t)) => {
                    let _ = socket.send_to(&t.contents, t.destination).await;
                }
                Ok(Output::Event(e)) => {
                    let _ = ev_tx.send(e);
                }
                Err(_) => return,
            }
        };
        tokio::select! {
            () = tokio::time::sleep_until(deadline.into()) => {
                if rtc.handle_input(Input::Timeout(Instant::now())).is_err() { return; }
            }
            got = socket.recv_from(&mut buf) => {
                let Ok((n, source)) = got else { continue };
                let Ok(contents) = buf[..n].try_into() else { continue };
                let input = Input::Receive(Instant::now(), Receive {
                    proto: Protocol::Udp, source, destination: local, contents });
                if rtc.accepts(&input) && rtc.handle_input(input).is_err() { return; }
            }
            Some(e) = ev_rx.recv() => match e {
                Event::ChannelOpen(id, _) => {
                    channel = Some(id);
                    fake.opened(Instant::now());
                }
                Event::ChannelData(d) => {
                    let v: Value = serde_json::from_slice(&d.data).unwrap();
                    fake.event(v, Instant::now());
                }
                Event::MediaAdded(m) => mid = Some(m.mid),
                Event::MediaData(m) => {
                    let mut out = vec![0i16; 5760 * 2];
                    let Ok(packet) = Packet::try_from(&m.data[..]) else { continue };
                    let signals = MutSignals::try_from(&mut out[..]).unwrap();
                    if let Ok(n) = dec.decode(Some(packet), signals, false) {
                        let mono: Vec<i16> = out[..n * 2].chunks_exact(4)
                            .map(|q| ((i32::from(q[0]) + i32::from(q[1]) + i32::from(q[2]) + i32::from(q[3])) / 4) as i16)
                            .collect();
                        let bytes: Vec<u8> = mono.iter().flat_map(|x| x.to_le_bytes()).collect();
                        fake.audio(&bytes, Instant::now());
                    }
                }
                _ => {}
            },
            _ = tick.tick() => {
                fake.tick(Instant::now());
                for out in fake.take() {
                    match out {
                        Out::Event(v) => {
                            if let Some(mut ch) = channel.and_then(|c| rtc.channel(c)) {
                                let _ = ch.write(false, v.to_string().as_bytes());
                            }
                        }
                        Out::Audio(s) => {
                            for x in s { pending_out.extend_from_slice(&[x, x, x, x]); }
                        }
                        Out::Close => { rtc.disconnect(); return; }
                    }
                    // One mutation at a time: drain after each.
                    loop {
                        match rtc.poll_output() {
                            Ok(Output::Timeout(_)) => break,
                            Ok(Output::Transmit(t)) => { let _ = socket.send_to(&t.contents, t.destination).await; }
                            Ok(Output::Event(e)) => { let _ = ev_tx.send(e); }
                            Err(_) => return,
                        }
                    }
                }
                // Output audio as 20 ms Opus frames, all that is pending.
                while pending_out.len() >= 1920 {
                    let frame: Vec<i16> = pending_out.drain(..1920).collect();
                    let mut packet = vec![0u8; 1500];
                    let n = enc.encode(&frame, &mut packet).unwrap();
                    packet.truncate(n);
                    let Some(m) = mid else { break };
                    let Some(writer) = rtc.writer(m) else { break };
                    let Some(pt) = writer.payload_params().next().map(|p| p.pt()) else { break };
                    let _ = writer.write(pt, Instant::now(), MediaTime::new(sent, Frequency::FORTY_EIGHT_KHZ), packet);
                    sent += 960;
                    loop {
                        match rtc.poll_output() {
                            Ok(Output::Timeout(_)) => break,
                            Ok(Output::Transmit(t)) => { let _ = socket.send_to(&t.contents, t.destination).await; }
                            Ok(Output::Event(e)) => { let _ = ev_tx.send(e); }
                            Err(_) => return,
                        }
                    }
                }
            }
        }
    }
}

/// What the fake session emits.
pub enum Out {
    Event(Value),
    /// 24 kHz mono.
    Audio(Vec<i16>),
    Close,
}

/// The scripted session logic shared by both transports.
pub struct Fake {
    script: Arc<Script>,
    log: Arc<Mutex<Log>>,
    index: usize,
    greeting: bool,
    out: Vec<Out>,
    started: bool,
    loud_ms: u32,
    quiet_ms: u32,
    turns: u32,
    seconds: f64,
    opened_at: Option<Instant>,
    awaiting: Vec<String>,
    /// Events held back until their time (`Script::call_gap_ms`).
    later: Vec<(Instant, Value)>,
}

fn tone(ms: u32) -> Vec<i16> {
    (0..ms * 24)
        .map(|i| ((f64::from(i) * 0.13).sin() * 6000.0) as i16)
        .collect()
}

impl Fake {
    fn new(script: Arc<Script>, log: Arc<Mutex<Log>>, index: usize, session: Value) -> Self {
        Self {
            script,
            log,
            index,
            greeting: session.get("input").is_some(),
            out: Vec::new(),
            started: false,
            loud_ms: 0,
            quiet_ms: 0,
            turns: 0,
            seconds: 0.0,
            opened_at: None,
            awaiting: Vec::new(),
            later: Vec::new(),
        }
    }

    fn take(&mut self) -> Vec<Out> {
        std::mem::take(&mut self.out)
    }

    fn emit(&mut self, v: Value) {
        self.out.push(Out::Event(v));
    }

    /// Emits the `n`th event of a spaced sequence: at once without spacing, else at its time.
    fn emit_nth(&mut self, n: u32, v: Value) {
        match self.script.call_gap_ms * n {
            0 => self.emit(v),
            ms => {
                let at = Instant::now() + Duration::from_millis(u64::from(ms));
                self.later.push((at, v));
            }
        }
    }

    fn speak(&mut self, text: &str, ms: u32) {
        for word in text.split_inclusive(' ') {
            self.emit(json!({"type": "session.output_transcript.delta", "delta": word}));
        }
        if ms > 0 {
            let now = Instant::now();
            let audio = tone(ms);
            for chunk in audio.chunks(2400) {
                self.out.push(Out::Audio(chunk.to_vec()));
                self.log.lock().unwrap().output.push((self.index, now));
            }
        }
    }

    fn opened(&mut self, now: Instant) {
        self.opened_at = Some(now);
        if self.script.never_start {
            return;
        }
        self.started = true;
        self.emit(
            json!({"type": "session.started", "session": {"id": format!("live_{}", self.index)}}),
        );
        if let Some(code) = self.script.fail_after_start.clone() {
            self.emit(json!({"type": "session.input_transcript.delta", "delta": "hello"}));
            self.emit(
                json!({"type": "error", "error": {"type": "server_error", "code": code,
                "message": "scripted failure"}}),
            );
            return;
        }
        if self.greeting && self.script.greet_ms > 0 {
            let ms = self.script.greet_ms;
            self.speak("Hello. ", ms);
        }
    }

    fn audio(&mut self, bytes: &[u8], now: Instant) {
        {
            let mut l = self.log.lock().unwrap();
            l.input.push((self.index, now, bytes.len()));
            l.input_bytes[self.index].extend_from_slice(bytes);
        }
        let samples: Vec<i16> = bytes
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        for frame in samples.chunks(480) {
            let rms = (frame.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>()
                / frame.len().max(1) as f64)
                .sqrt();
            if rms > 600.0 {
                self.loud_ms += 20;
                self.quiet_ms = 0;
            } else {
                self.quiet_ms += 20;
                if self.loud_ms >= 200 && self.quiet_ms >= 300 {
                    self.loud_ms = 0;
                    self.turn();
                }
            }
        }
        self.seconds += samples.len() as f64 / 24_000.0;
    }

    fn turn(&mut self) {
        self.turns += 1;
        self.emit(json!({"type": "session.input_transcript.delta", "delta": "check the build "}));
        if self.turns == 1 && !self.script.tools.is_empty() {
            self.emit(
                json!({"type": "session.delegation.created", "delegation_id": "d1",
                "target": "responses", "response_id": "r1"}),
            );
            self.emit(json!({"type": "response.event", "delegation_id": "d1",
                "event": {"type": "response.created", "response": {"id": "r1"}}}));
            let mut n = 0;
            for (i, (name, args)) in self.script.tools.clone().iter().enumerate() {
                let call = format!("call_{i}");
                self.awaiting.push(call.clone());
                self.emit_nth(
                    n,
                    json!({"type": "response.event", "delegation_id": "d1", "event": {
                    "type": "response.output_item.done", "item": {"type": "function_call",
                    "call_id": call, "name": name, "arguments": args}}}),
                );
                n += 1;
            }
            self.emit_nth(
                n,
                json!({"type": "response.event", "delegation_id": "d1", "event": {
                "type": "response.completed", "response": {"id": "r1", "output": [],
                "usage": {"input_tokens": 1000, "input_tokens_details": {"cached_tokens": 200},
                    "output_tokens": 100}}}}),
            );
            return;
        }
        let ms = if self.script.reply_once && self.turns > 1 {
            0
        } else {
            self.script.reply_ms
        };
        if ms > 0 {
            self.speak("Sure, here you go. ", ms);
        }
    }

    fn event(&mut self, v: Value, _now: Instant) {
        self.log
            .lock()
            .unwrap()
            .events
            .push((self.index, v.clone()));
        match v["type"].as_str().unwrap_or("") {
            "session.close" => {
                let seconds = self.seconds.ceil();
                self.emit(
                    json!({"type": "session.closed", "reason": "close_requested",
                    "usage": {"seconds": seconds}}),
                );
                self.out.push(Out::Close);
            }
            "session.commentary.append" | "session.instructions.append" => {
                let id = v["event_id"].clone();
                let kind = if v["type"] == "session.commentary.append" {
                    "session.commentary.appended"
                } else {
                    "session.instructions.appended"
                };
                if kind == "session.instructions.appended" || self.script.ack_commentary {
                    self.emit(json!({"type": kind, "client_event_id": id}));
                }
                if kind == "session.commentary.appended" && self.script.ack_commentary {
                    let content = v["content"].as_str().unwrap_or("").to_string();
                    let said = if self.script.cover_commentary {
                        content.rsplit(": ").next().unwrap_or("").to_string()
                    } else {
                        "Okay.".to_string()
                    };
                    let ms = self.script.commentary_ms;
                    self.speak(&format!("{said} "), ms);
                }
            }
            "response.item.create" => {
                let call = v["item"]["call_id"].as_str().unwrap_or("").to_string();
                self.awaiting.retain(|c| *c != call);
            }
            "response.create" => {
                if self.awaiting.is_empty() {
                    self.emit(json!({"type": "response.event", "delegation_id": "d1",
                        "event": {"type": "response.created", "response": {"id": "r2"}}}));
                    self.emit(
                        json!({"type": "response.event", "delegation_id": "d1", "event": {
                        "type": "response.completed", "response": {"id": "r2", "output": [],
                        "usage": {"input_tokens": 500, "output_tokens": 50}}}}),
                    );
                    let ms = self.script.reply_ms;
                    self.speak("Done. ", ms);
                }
            }
            _ => {}
        }
    }

    fn tick(&mut self, now: Instant) {
        let due: Vec<Value> = {
            let (due, later) = std::mem::take(&mut self.later)
                .into_iter()
                .partition(|(at, _)| *at <= now);
            self.later = later;
            due.into_iter().map(|(_, v)| v).collect()
        };
        for v in due {
            self.emit(v);
        }
        if self.started
            && let Some(at) = self.opened_at
            && now.duration_since(at).as_millis() % 1000 < 20
        {
            let seconds = self.seconds.floor();
            self.emit(json!({"type": "session.usage.updated", "usage": {"seconds": seconds}}));
        }
    }
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64(bytes: &[u8]) -> String {
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn unb64(s: &str) -> Vec<u8> {
    let val = |c: u8| ALPHABET.iter().position(|&a| a == c).unwrap_or(0) as u32;
    let mut out = Vec::new();
    for q in s.as_bytes().chunks(4) {
        let pad = q.iter().filter(|&&c| c == b'=').count();
        let n = q.iter().fold(0u32, |acc, &c| {
            (acc << 6) | if c == b'=' { 0 } else { val(c) }
        });
        let b = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&b[..3 - pad]);
    }
    out
}
