//! A fake Slack on loopback only: the Web API methods Cataly calls (identity, socket open, post,
//! update, reactions, replies and history, conversation info, the external upload steps and the byte
//! transfer) and a Socket Mode WebSocket that greets, pushes event envelopes and times every
//! acknowledgement. A hang switch stalls every Web API call.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt as _, StreamExt as _};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

pub const TEAM: &str = "T0TEAM";
pub const ACCOUNT: &str = "U0CATALY";
pub const OWNER: &str = "U0OWNER";

/// One Web API request.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub query: String,
    pub auth: Option<String>,
    pub body: Value,
    pub bytes: usize,
}

#[derive(Default)]
struct State {
    seen: Vec<Seen>,
    /// Channel -> messages, oldest first.
    history: HashMap<String, Vec<Value>>,
    next_ts: u64,
    hang: bool,
    upload_url: String,
    connections: usize,
    socket: Option<mpsc::UnboundedSender<Value>>,
    sent: HashMap<String, Instant>,
    acks: Vec<(String, Duration)>,
    next_envelope: u64,
}

pub struct FakeSlack {
    pub port: u16,
    pub ws_port: u16,
    state: Arc<Mutex<State>>,
}

impl FakeSlack {
    pub fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let ws = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        ws.set_nonblocking(true).unwrap();
        let ws_port = ws.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(State {
            next_ts: 1_700_000_000_000_100,
            upload_url: "https://files.slack.com/upload/v1/ABC".into(),
            ..State::default()
        }));
        let shared = Arc::clone(&state);
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let ws = tokio::net::TcpListener::from_std(ws).unwrap();
                let ws_state = Arc::clone(&shared);
                tokio::spawn(async move {
                    loop {
                        let Ok((stream, _)) = ws.accept().await else {
                            continue;
                        };
                        tokio::spawn(socket(stream, Arc::clone(&ws_state)));
                    }
                });
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        continue;
                    };
                    let state = Arc::clone(&shared);
                    tokio::spawn(async move {
                        let service =
                            service_fn(move |req| answer(req, Arc::clone(&state), ws_port));
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            });
        });
        Self {
            port,
            ws_port,
            state,
        }
    }

    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The `[slack]` section for this fake (app role, the owner only).
    pub fn config(&self, token_file: &std::path::Path) -> String {
        format!(
            "[slack]\ntoken_file = \"{}\"\napi_base = \"{}\"\nowner = \"{OWNER}\"\n",
            token_file.display(),
            self.base()
        )
    }

    /// The token file's text.
    pub fn tokens() -> String {
        format!("app=xapp-{}\napi=xoxb-{}\n", "1-A0APP-1-fake", "1-2-fake")
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.state.lock().unwrap().seen.clone()
    }

    pub fn calls(&self, method: &str) -> Vec<Seen> {
        self.seen()
            .into_iter()
            .filter(|s| s.method == method)
            .collect()
    }

    pub fn set_hang(&self, on: bool) {
        self.state.lock().unwrap().hang = on;
    }

    pub fn set_upload_url(&self, url: &str) {
        self.state.lock().unwrap().upload_url = url.into();
    }

    pub fn connections(&self) -> usize {
        self.state.lock().unwrap().connections
    }

    pub fn acks(&self) -> Vec<(String, Duration)> {
        self.state.lock().unwrap().acks.clone()
    }

    /// A new message timestamp.
    pub fn ts(&self) -> String {
        let mut s = self.state.lock().unwrap();
        s.next_ts += 100;
        let n = s.next_ts;
        format!("{}.{:06}", n / 1_000_000, n % 1_000_000)
    }

    /// A message from `user`, kept in history and pushed live as an event envelope; its timestamp.
    pub fn say(&self, channel: &str, user: &str, text: &str, thread: Option<&str>) -> String {
        let ts = self.ts();
        let mut m = json!({"type": "message", "channel": channel, "user": user, "text": text, "ts": ts,
            "channel_type": if channel.starts_with('D') { "im" } else { "channel" }});
        if let Some(t) = thread {
            m["thread_ts"] = json!(t);
        }
        self.state
            .lock()
            .unwrap()
            .history
            .entry(channel.to_string())
            .or_default()
            .push(m.clone());
        self.push(m);
        ts
    }

    /// Pushes one event in an envelope.
    pub fn push(&self, event: Value) -> String {
        let mut s = self.state.lock().unwrap();
        s.next_envelope += 1;
        let id = format!("env-{}", s.next_envelope);
        s.sent.insert(id.clone(), Instant::now());
        let mut envelope = json!({"envelope_id": id, "type": "events_api",
            "payload": {"type": "event_callback"}});
        envelope["payload"]["event"] = event;
        if let Some(tx) = &s.socket {
            let _ = tx.send(envelope);
        }
        id
    }

    /// The messages the account posted in a channel, oldest first.
    pub fn posts(&self, channel: &str) -> Vec<Value> {
        self.state
            .lock()
            .unwrap()
            .history
            .get(channel)
            .map_or_else(Vec::new, |l| {
                l.iter().filter(|m| m["user"] == ACCOUNT).cloned().collect()
            })
    }

    /// Polls `f` every 20 ms for at most `limit`.
    pub fn wait<T>(&self, limit: Duration, mut f: impl FnMut(&Self) -> Option<T>) -> T {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(v) = f(self) {
                return v;
            }
            assert!(
                Instant::now() < deadline,
                "condition not met within {limit:?}; requests: {:?}",
                self.seen()
                    .iter()
                    .map(|s| s.method.clone())
                    .collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn reply(status: u16, body: &Value) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

fn ok(mut body: Value) -> Response<Full<Bytes>> {
    body["ok"] = json!(true);
    reply(200, &body)
}

fn arg(query: &str, key: &str) -> Option<String> {
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
        .map(|v| v.replace("%2E", ".").replace("%20", " "))
}

async fn answer(
    req: Request<Incoming>,
    state: Arc<Mutex<State>>,
    ws_port: u16,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let auth = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = req
        .into_body()
        .collect()
        .await
        .map_or_default(http_body_util::Collected::to_bytes);
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let method = path.trim_start_matches('/').to_string();
    let hang = {
        let mut s = state.lock().unwrap();
        s.seen.push(Seen {
            method: method.clone(),
            query: query.clone(),
            auth: auth.clone(),
            body: body.clone(),
            bytes: bytes.len(),
        });
        s.hang
    };
    if hang {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
    if auth.as_deref().is_none_or(|a| !a.starts_with("Bearer x")) && !method.starts_with("upload/")
    {
        return Ok(ok(json!({"ok": false, "error": "not_authed"})));
    }
    Ok(match method.as_str() {
        "auth.test" => ok(json!({"team_id": TEAM, "user_id": ACCOUNT})),
        "apps.connections.open" => {
            ok(json!({"url": format!("ws://127.0.0.1:{ws_port}/link/?ticket=t1")}))
        }
        "chat.postMessage" => {
            let channel = body["channel"].as_str().unwrap_or("").to_string();
            let ts = {
                let mut s = state.lock().unwrap();
                s.next_ts += 100;
                let n = s.next_ts;
                format!("{}.{:06}", n / 1_000_000, n % 1_000_000)
            };
            let mut m = json!({"type": "message", "user": ACCOUNT, "bot_id": "B1", "text": body["text"],
                "ts": ts, "channel": channel});
            if let Some(t) = body["thread_ts"].as_str() {
                m["thread_ts"] = json!(t);
            }
            state
                .lock()
                .unwrap()
                .history
                .entry(channel.clone())
                .or_default()
                .push(m);
            ok(json!({"channel": channel, "ts": ts}))
        }
        "chat.update" | "reactions.add" => ok(json!({})),
        "conversations.info" => ok(
            json!({"channel": {"id": arg(&query, "channel"), "name": "general",
            "topic": {"value": "the release room"}, "is_im": false}}),
        ),
        "conversations.replies" | "conversations.history" => {
            let channel = arg(&query, "channel").unwrap_or_default();
            let root = arg(&query, "ts");
            let oldest = arg(&query, "oldest").unwrap_or_default();
            let key = |t: &str| t.replace('.', "").parse::<u64>().unwrap_or(0);
            let messages: Vec<Value> = state
                .lock()
                .unwrap()
                .history
                .get(&channel)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|m| key(m["ts"].as_str().unwrap_or("0")) > key(&oldest))
                .filter(|m| {
                    root.as_deref().is_none_or(|r| {
                        m["thread_ts"].as_str() == Some(r) || m["ts"].as_str() == Some(r)
                    })
                })
                .collect();
            ok(json!({"messages": messages}))
        }
        "files.getUploadURLExternal" => {
            let url = state.lock().unwrap().upload_url.clone();
            ok(json!({"upload_url": url, "file_id": "F0FILE1"}))
        }
        "files.completeUploadExternal" => ok(json!({"files": [{"id": body["files"][0]["id"]}]})),
        m if m.starts_with("upload/") => reply(200, &json!("OK")),
        _ => ok(json!({"ok": false, "error": "unknown_method"})),
    })
}

async fn socket(stream: tokio::net::TcpStream, state: Arc<Mutex<State>>) {
    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    {
        let mut s = state.lock().unwrap();
        s.connections += 1;
        s.socket = Some(tx);
    }
    let hello = json!({"type": "hello", "num_connections": 1});
    if ws.send(Message::text(hello.to_string())).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            incoming = ws.next() => {
                let Some(Ok(Message::Text(text))) = incoming else { return };
                let Ok(v) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                if let Some(id) = v["envelope_id"].as_str() {
                    let mut s = state.lock().unwrap();
                    if let Some(at) = s.sent.remove(id) {
                        s.acks.push((id.to_string(), at.elapsed()));
                    }
                }
            }
            out = rx.recv() => {
                let Some(v) = out else { return };
                if ws.send(Message::text(v.to_string())).await.is_err() {
                    return;
                }
            }
        }
    }
}
