//! The Discord Gateway connection (DISC-IN req. 16-23; DISC-OUT req. 84): API v10 with JSON, HELLO
//! within 10 s, identify or resume, READY or RESUMED within 20 s of HELLO, a jittered heartbeat with
//! reconnect on a missed ACK, op 1, 7 and 9, the close-code table, bounded frames (1 MiB) and writes
//! (3 s), the resume URL check, and reconnect backoff (1 s doubling to 30 s, reset after 60 s of a
//! good connection, no identify sooner than 6 s after the last). One connection carries messages,
//! thread changes, button presses and presence. Dispatches go to a [`Handler`], which persists an
//! owner message before the next frame is read; heartbeats never wait for delivery work.

use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::time::{Instant, sleep, sleep_until, timeout};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use crate::relay::http::{self, Io};

/// Largest incoming frame (DISC-IN req. 17).
const FRAME_MAX: usize = 1024 * 1024;
const CONNECT: Duration = Duration::from_secs(10);
const HELLO: Duration = Duration::from_secs(10);
const HANDSHAKE: Duration = Duration::from_secs(20);
const WRITE: Duration = Duration::from_secs(3);
const BACKOFF_FIRST: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A connection up this long resets the backoff (DISC-IN req. 21).
const STABLE: Duration = Duration::from_secs(60);
/// No identify sooner than this after the previous one.
const IDENTIFY_GAP: Duration = Duration::from_secs(6);
/// Close 4008 (rate limited) waits at least this long.
const RATE_LIMITED: Duration = Duration::from_secs(60);

type Socket = WebSocketStream<Box<dyn Io>>;

/// Intents: guild messages, direct messages, message content, and guilds when threads are followed.
pub(crate) fn intents(follow_threads: bool) -> u64 {
    (1 << 9) | (1 << 12) | (1 << 15) | u64::from(follow_threads)
}

/// A WebSocket URL: `wss://host[:port][/path]`, or `ws://` for a loopback host only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WsUrl {
    tls: bool,
    host: String,
    port: u16,
    path: String,
}

impl WsUrl {
    pub(crate) fn parse(url: &str) -> Result<Self, String> {
        let (tls, rest) = if let Some(rest) = url.strip_prefix("wss://") {
            (true, rest)
        } else if let Some(rest) = url.strip_prefix("ws://") {
            (false, rest)
        } else {
            return Err("the Gateway URL must start with wss://".into());
        };
        let rest = rest.split(['?', '#']).next().unwrap_or_default();
        if rest.contains('@') || rest.chars().any(char::is_whitespace) {
            return Err("the Gateway URL must not carry credentials".into());
        }
        let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (
                h,
                p.parse::<u16>()
                    .map_err(|_| "bad Gateway port".to_string())?,
            ),
            None => (authority, if tls { 443 } else { 80 }),
        };
        if host.is_empty() {
            return Err("the Gateway URL has no host".into());
        }
        let url = Self {
            tls,
            host: host.to_ascii_lowercase(),
            port,
            path: path.trim_end_matches('/').to_string(),
        };
        if !tls && !url.loopback() {
            return Err("ws:// is allowed only for a loopback host".into());
        }
        Ok(url)
    }

    fn loopback(&self) -> bool {
        self.host == "localhost"
            || self
                .host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    }

    /// Discord's own Gateway host.
    fn discord(&self) -> bool {
        self.tls && (self.host == "discord.gg" || self.host.ends_with(".discord.gg"))
    }

    fn text(&self) -> String {
        let scheme = if self.tls { "wss" } else { "ws" };
        format!(
            "{scheme}://{}:{}{}/?v=10&encoding=json",
            self.host, self.port, self.path
        )
    }
}

/// Whether a resume URL from READY may be used (DISC-IN req. 19). With a loopback test Gateway only
/// loopback resume URLs are, so the real Gateway is never contacted (DISC-OUT req. 10).
pub(crate) fn resume_url(first: &WsUrl, offered: &str) -> Option<WsUrl> {
    let url = WsUrl::parse(offered).ok()?;
    if first.loopback() {
        url.loopback().then_some(url)
    } else {
        url.discord().then_some(url)
    }
}

/// Close codes that stop intake, with their meaning (DISC-IN req. 20).
pub(crate) fn fatal_close(code: u16) -> Option<&'static str> {
    Some(match code {
        4004 => "the bot token was refused (4004 authentication failed)",
        4010 => "invalid shard (4010)",
        4011 => "sharding required (4011)",
        4012 => "invalid API version (4012)",
        4013 => "invalid intents (4013)",
        4014 => "disallowed intents: enable Message Content Intent in the Developer Portal (4014)",
        _ => return None,
    })
}

/// How one connection ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum End {
    /// Resume when a session is known (socket error, missed ACK, op 7, other closes).
    Resume(String),
    /// Forget the session and identify again.
    Identify(String),
    /// Op 9: wait a random 1 to 5 s, then resume (`true`) or identify.
    Invalid(bool),
    /// Close 4008: wait at least 60 s.
    RateLimited,
    /// Stop intake.
    Fatal(String),
    /// The hub is stopping.
    Shutdown,
}

/// The classification of a close frame (DISC-IN req. 20).
pub(crate) fn close_end(code: u16) -> End {
    if let Some(meaning) = fatal_close(code) {
        return End::Fatal(meaning.into());
    }
    match code {
        4008 => End::RateLimited,
        1000 | 1001 | 4003 | 4005 | 4007 | 4009 => End::Identify(format!("closed with {code}")),
        _ => End::Resume(format!("closed with {code}")),
    }
}

/// What the connection reports to its owner.
pub(crate) trait Handler: Send + Sync + 'static {
    /// READY (`resumed` false) or RESUMED: a catch-up read is owed now (DISC-IN req. 23).
    fn established(&self, resumed: bool) -> impl Future<Output = ()> + Send;
    /// One dispatch after READY or during a resume replay; `read_at` is when its frame was read.
    fn dispatch(
        &self,
        kind: &str,
        data: Value,
        read_at: std::time::Instant,
    ) -> impl Future<Output = ()> + Send;
    /// The connection went down (`Some` reason) or came up (`None`).
    fn link(&self, down: Option<&str>);
}

/// The known session for a resume.
#[derive(Debug, Clone)]
struct Session {
    url: WsUrl,
    id: String,
    seq: u64,
}

#[derive(Deserialize)]
struct Frame {
    op: u8,
    s: Option<u64>,
    t: Option<String>,
    #[serde(default)]
    d: Value,
}

/// A random fraction in [0, 1) from the system generator.
fn random_fraction() -> f64 {
    use ring::rand::SecureRandom as _;
    let mut b = [0u8; 4];
    let _ = ring::rand::SystemRandom::new().fill(&mut b);
    f64::from(u32::from_le_bytes(b)) / (f64::from(u32::MAX) + 1.0)
}

/// The Gateway client of one bot.
pub(crate) struct Gateway {
    pub(crate) url: WsUrl,
    pub(crate) token: String,
    pub(crate) intents: u64,
}

impl Gateway {
    /// Runs until a fatal close or shutdown; returns the fatal reason (`None` on shutdown).
    pub(crate) async fn run<H: Handler>(
        &self,
        handler: &H,
        mut shutdown: watch::Receiver<bool>,
    ) -> Option<String> {
        let mut session: Option<Session> = None;
        let mut backoff = BACKOFF_FIRST;
        let mut last_identify: Option<Instant> = None;
        loop {
            if *shutdown.borrow() {
                return None;
            }
            if session.is_none()
                && let Some(at) = last_identify
            {
                sleep_until(at + IDENTIFY_GAP).await;
            }
            if session.is_none() {
                last_identify = Some(Instant::now());
            }
            let started = Instant::now();
            let end = tokio::select! {
                end = self.connect(handler, &mut session) => end,
                _ = shutdown.changed() => End::Shutdown,
            };
            let why = format!("{end:?}");
            handler.link(Some(&why));
            if started.elapsed() >= STABLE {
                backoff = BACKOFF_FIRST;
            }
            let mut wait = backoff;
            match end {
                End::Shutdown => return None,
                End::Fatal(reason) => return Some(reason),
                End::Resume(reason) => {
                    crate::log::info("Gateway reconnecting", &[("reason", &reason)]);
                }
                End::Identify(reason) => {
                    crate::log::info("Gateway identifying again", &[("reason", &reason)]);
                    session = None;
                }
                End::Invalid(resumable) => {
                    if !resumable {
                        session = None;
                    }
                    wait = Duration::from_secs_f64(1.0 + 4.0 * random_fraction());
                }
                End::RateLimited => wait = wait.max(RATE_LIMITED),
            }
            tokio::select! {
                () = sleep(wait) => {}
                _ = shutdown.changed() => return None,
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }

    async fn open(url: &WsUrl) -> Result<Socket, String> {
        http::websocket(url.tls, &url.host, url.port, &url.text(), FRAME_MAX).await
    }

    fn identify(&self) -> Value {
        json!({"op": 2, "d": {
            "token": self.token,
            "intents": self.intents,
            "properties": {"os": std::env::consts::OS, "browser": "cataly", "device": "cataly"},
            "presence": presence(),
        }})
    }

    /// Reads HELLO and sends identify or resume; the heartbeat interval, or how the connection ended.
    async fn greet(&self, ws: &mut Socket, session: Option<&Session>) -> Result<Duration, End> {
        let interval = match timeout(HELLO, ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                match serde_json::from_str::<Frame>(text.as_str()) {
                    Ok(f) if f.op == 10 => f.d.get("heartbeat_interval").and_then(Value::as_u64),
                    _ => None,
                }
            }
            Ok(Some(Ok(Message::Close(Some(close))))) => {
                return Err(close_end(u16::from(close.code)));
            }
            Err(_) => return Err(End::Resume("no HELLO within 10 s".into())),
            _ => None,
        };
        let Some(interval) = interval.filter(|ms| (100..=120_000).contains(ms)) else {
            return Err(End::Resume("bad HELLO".into()));
        };
        let hello = match session {
            Some(s) => {
                json!({"op": 6, "d": {"token": self.token, "session_id": s.id, "seq": s.seq}})
            }
            None => self.identify(),
        };
        send(ws, &hello).await.map_err(End::Resume)?;
        Ok(Duration::from_millis(interval))
    }

    /// The session READY names, with its resume URL checked (DISC-IN req. 19).
    fn ready(&self, frame: &Frame) -> Result<Session, End> {
        let id = frame
            .d
            .get("session_id")
            .and_then(Value::as_str)
            .ok_or_else(|| End::Identify("READY without a session".into()))?;
        let offered = frame
            .d
            .get("resume_gateway_url")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let url = resume_url(&self.url, offered)
            .ok_or_else(|| End::Identify("refused resume URL".into()))?;
        Ok(Session {
            url,
            id: id.to_string(),
            seq: frame.s.unwrap_or(0),
        })
    }

    /// One connection: returns how it ended.
    async fn connect<H: Handler>(&self, handler: &H, session: &mut Option<Session>) -> End {
        let url = session.as_ref().map_or(&self.url, |s| &s.url);
        let mut ws = match timeout(CONNECT, Self::open(url)).await {
            Ok(Ok(ws)) => ws,
            Ok(Err(e)) => return End::Resume(e),
            Err(_) => return End::Resume("connect timed out".into()),
        };
        let interval = match self.greet(&mut ws, session.as_ref()).await {
            Ok(interval) => interval,
            Err(end) => return end,
        };
        let resuming = session.is_some();
        let mut next_beat = Instant::now() + interval.mul_f64(random_fraction());
        let mut awaiting_ack = false;
        let handshake = Instant::now() + HANDSHAKE;
        let mut ready = false;
        loop {
            tokio::select! {
                () = sleep_until(handshake), if !ready => {
                    return End::Resume("no READY or RESUMED within 20 s".into());
                }
                () = sleep_until(next_beat) => {
                    if awaiting_ack {
                        return End::Resume("heartbeat ACK missed".into());
                    }
                    let seq = session.as_ref().map(|s| s.seq);
                    if let Err(e) = send(&mut ws, &json!({"op": 1, "d": seq})).await {
                        return End::Resume(e);
                    }
                    awaiting_ack = true;
                    next_beat = Instant::now() + interval;
                }
                incoming = ws.next() => {
                    let read_at = std::time::Instant::now();
                    let text = match incoming {
                        None => return End::Resume("the Gateway closed the socket".into()),
                        Some(Err(e)) => return End::Resume(format!("socket error: {e}")),
                        Some(Ok(Message::Close(Some(close)))) => return close_end(u16::from(close.code)),
                        Some(Ok(Message::Close(None))) => return End::Resume("closed".into()),
                        Some(Ok(Message::Text(text))) => text,
                        Some(Ok(_)) => continue,
                    };
                    let Ok(frame) = serde_json::from_str::<Frame>(text.as_str()) else {
                        return End::Resume("unreadable frame".into());
                    };
                    match frame.op {
                        0 => {
                            if let (Some(s), Some(seq)) = (session.as_mut(), frame.s) {
                                s.seq = s.seq.max(seq);
                            }
                            match frame.t.as_deref() {
                                Some("READY") => {
                                    match self.ready(&frame) {
                                        Ok(s) => *session = Some(s),
                                        Err(end) => {
                                            *session = None;
                                            return end;
                                        }
                                    }
                                    ready = true;
                                    handler.link(None);
                                    handler.established(false).await;
                                }
                                Some("RESUMED") => {
                                    ready = true;
                                    handler.link(None);
                                    // Presence is sent again after a resume (DISC-OUT req. 84).
                                    if let Err(e) = send(&mut ws, &json!({"op": 3, "d": presence()})).await {
                                        return End::Resume(e);
                                    }
                                    handler.established(true).await;
                                }
                                Some(kind) if ready || resuming => {
                                    handler.dispatch(kind, frame.d, read_at).await;
                                }
                                _ => {}
                            }
                        }
                        1 => {
                            let seq = session.as_ref().map(|s| s.seq);
                            if let Err(e) = send(&mut ws, &json!({"op": 1, "d": seq})).await {
                                return End::Resume(e);
                            }
                            awaiting_ack = true;
                        }
                        7 => return End::Resume("reconnect requested (op 7)".into()),
                        9 => return End::Invalid(frame.d == Value::Bool(true)),
                        11 => awaiting_ack = false,
                        _ => {}
                    }
                }
            }
        }
    }
}

fn presence() -> Value {
    json!({"status": "online", "since": null, "activities": [], "afk": false})
}

async fn send(ws: &mut Socket, value: &Value) -> Result<(), String> {
    match timeout(WRITE, ws.send(Message::text(value.to_string()))).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("write failed: {e}")),
        Err(_) => Err("write took over 3 s".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    #[test]
    fn urls_and_close_codes() {
        let real = WsUrl::parse("wss://gateway.discord.gg").unwrap();
        assert!(resume_url(&real, "wss://gateway-us-east1-b.discord.gg").is_some());
        assert!(resume_url(&real, "wss://evil.example").is_none());
        assert!(resume_url(&real, "wss://user@gateway.discord.gg").is_none());
        assert!(resume_url(&real, "ws://gateway.discord.gg").is_none());
        assert!(WsUrl::parse("ws://example.com").is_err());
        let fake = WsUrl::parse("ws://127.0.0.1:9").unwrap();
        assert!(resume_url(&fake, "wss://gateway.discord.gg").is_none());
        assert!(resume_url(&fake, "ws://127.0.0.1:10").is_some());
        assert_eq!(fake.text(), "ws://127.0.0.1:9/?v=10&encoding=json");
        assert!(matches!(close_end(4004), End::Fatal(_)));
        assert!(matches!(close_end(4014), End::Fatal(m) if m.contains("Message Content")));
        assert_eq!(close_end(4008), End::RateLimited);
        assert!(matches!(close_end(4009), End::Identify(_)));
        assert!(matches!(close_end(4000), End::Resume(_)));
        assert_eq!(intents(true), (1 << 9) | (1 << 12) | (1 << 15) | 1);
    }

    /// What the fake server does after HELLO on each connection.
    #[derive(Clone, Copy)]
    enum Script {
        /// READY with a loopback resume URL, then op 7.
        ReadyThenReconnect,
        /// Expects a resume; RESUMED, then op 9 false.
        ResumedThenInvalid,
        /// Expects an identify; READY, never ACKs heartbeats.
        ReadyNoAck,
        /// Expects a resume; closes with 4004.
        Close4004,
    }

    #[derive(Default)]
    struct Seen(Mutex<Vec<String>>);

    impl Handler for Arc<Seen> {
        async fn established(&self, resumed: bool) {
            self.0
                .lock()
                .unwrap()
                .push(format!("established resumed={resumed}"));
        }
        async fn dispatch(&self, kind: &str, _data: Value, _read_at: std::time::Instant) {
            self.0.lock().unwrap().push(kind.to_string());
        }
        fn link(&self, _down: Option<&str>) {}
    }

    async fn fake(scripts: Vec<Script>, ops: Arc<Mutex<Vec<u64>>>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            for script in scripts {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                ws.send(Message::text(
                    json!({"op": 10, "d": {"heartbeat_interval": 300}}).to_string(),
                ))
                .await
                .unwrap();
                let first: Value = loop {
                    let Some(Ok(Message::Text(t))) = ws.next().await else {
                        return;
                    };
                    let v: Value = serde_json::from_str(t.as_str()).unwrap();
                    if v["op"] != 1 {
                        break v;
                    }
                };
                ops.lock().unwrap().push(first["op"].as_u64().unwrap());
                let ready = json!({"op": 0, "s": 1, "t": "READY", "d": {"session_id": "s1",
                    "resume_gateway_url": format!("ws://127.0.0.1:{port}")}});
                match script {
                    Script::ReadyThenReconnect => {
                        ws.send(Message::text(ready.to_string())).await.unwrap();
                        ws.send(Message::text(
                            json!({"op": 0, "s": 2, "t": "MESSAGE_CREATE", "d": {}}).to_string(),
                        ))
                        .await
                        .unwrap();
                        ws.send(Message::text(json!({"op": 7, "d": null}).to_string()))
                            .await
                            .unwrap();
                    }
                    Script::ResumedThenInvalid => {
                        ws.send(Message::text(
                            json!({"op": 0, "s": 3, "t": "RESUMED", "d": {}}).to_string(),
                        ))
                        .await
                        .unwrap();
                        ws.send(Message::text(json!({"op": 9, "d": false}).to_string()))
                            .await
                            .unwrap();
                    }
                    Script::ReadyNoAck => {
                        ws.send(Message::text(ready.to_string())).await.unwrap();
                    }
                    Script::Close4004 => {
                        let close = tokio_tungstenite::tungstenite::protocol::CloseFrame {
                            code: 4004.into(),
                            reason: "bad".into(),
                        };
                        let _ = ws.send(Message::Close(Some(close))).await;
                    }
                }
                // Keep reading (heartbeats, presence) until the client goes.
                while let Some(Ok(_)) = ws.next().await {}
            }
        });
        port
    }

    /// Check 5 (part): op 7 resumes, op 9 false identifies again after 1-5 s, a missed ACK resumes,
    /// close 4004 stops with its meaning.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconnect_rules() {
        let ops = Arc::new(Mutex::new(Vec::new()));
        let scripts = vec![
            Script::ReadyThenReconnect,
            Script::ResumedThenInvalid,
            Script::ReadyNoAck,
            Script::Close4004,
        ];
        let port = fake(scripts, Arc::clone(&ops)).await;
        let gateway = Gateway {
            url: WsUrl::parse(&format!("ws://127.0.0.1:{port}")).unwrap(),
            token: "t".into(),
            intents: intents(true),
        };
        let seen = Arc::new(Seen::default());
        let (_tx, rx) = watch::channel(false);
        let started = Instant::now();
        let fatal = timeout(Duration::from_secs(40), gateway.run(&seen, rx))
            .await
            .unwrap();
        assert!(fatal.unwrap().contains("4004"));
        // identify, resume (after op 7), identify (op 9 false), resume (after the missed ACK).
        assert_eq!(*ops.lock().unwrap(), vec![2, 6, 2, 6]);
        let seen = seen.0.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                "established resumed=false",
                "MESSAGE_CREATE",
                "established resumed=true",
                "established resumed=false",
            ]
        );
        // The second identify waited at least 6 s after the first.
        assert!(started.elapsed() >= IDENTIFY_GAP);
    }
}
