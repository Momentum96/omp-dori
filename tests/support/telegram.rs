//! A fake Telegram Bot API on loopback only: identity, webhook info, long-poll updates with offsets,
//! scripted poll statuses (409, 429, 401), a hang switch, message send and edit with message IDs,
//! reactions, bot name and chat lookups. Every request is recorded with its time.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};

pub const BOT: i64 = 4242;
pub const TOKEN: &str = "4242:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// One request as the server saw it.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub body: Value,
    pub at: Instant,
}

#[derive(Default)]
struct State {
    seen: Vec<Seen>,
    updates: Vec<Value>,
    next_update: i64,
    next_message: i64,
    /// Statuses and bodies for the next polls.
    scripted: VecDeque<(u16, Value)>,
    hang: bool,
    not_bot: bool,
    webhook: String,
    /// Polls answer every update from this ID on, whatever the offset (a replay).
    replay_from: Option<i64>,
}

pub struct FakeTelegram {
    pub port: u16,
    state: Arc<Mutex<State>>,
}

impl FakeTelegram {
    pub fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(State {
            next_update: 1000,
            next_message: 5000,
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
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        continue;
                    };
                    let state = Arc::clone(&shared);
                    tokio::spawn(async move {
                        let service = service_fn(move |req| answer(req, Arc::clone(&state)));
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            });
        });
        Self { port, state }
    }

    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The `[telegram]` section for this fake: owner 7, group -100 with topic 5.
    pub fn config(&self, token_file: &std::path::Path) -> String {
        format!(
            "[telegram]\ntoken_file = \"{}\"\napi_base = \"{}\"\nowner = 7\ngroup = -100\ntopic = 5\n",
            token_file.display(),
            self.base()
        )
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

    pub fn script(&self, status: u16, body: Value) {
        self.state
            .lock()
            .unwrap()
            .scripted
            .push_back((status, body));
    }

    pub fn set_hang(&self, on: bool) {
        self.state.lock().unwrap().hang = on;
    }

    pub fn set_not_bot(&self) {
        self.state.lock().unwrap().not_bot = true;
    }

    pub fn set_webhook(&self, url: &str) {
        self.state.lock().unwrap().webhook = url.into();
    }

    pub fn replay_from(&self, id: i64) {
        self.state.lock().unwrap().replay_from = Some(id);
    }

    /// Queues one update holding `message` (or `edited_message`); its update ID.
    pub fn push(&self, kind: &str, message: Value) -> i64 {
        let mut s = self.state.lock().unwrap();
        s.next_update += 1;
        let id = s.next_update;
        let mut update = json!({ "update_id": id });
        update[kind] = message;
        s.updates.push(update);
        id
    }

    /// A text message from `from` in a chat.
    pub fn message(id: i64, chat: Value, from: i64, text: &str) -> Value {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut m = json!({"message_id": id, "date": now,
            "from": {"id": from, "is_bot": false, "first_name": "o"}, "text": text});
        m["chat"] = chat;
        m
    }

    pub fn dm() -> Value {
        json!({"id": 7, "type": "private"})
    }

    pub fn forum() -> Value {
        json!({"id": -100, "type": "supergroup", "is_forum": true})
    }

    /// Polls `f` every 20 ms for at most `limit`.
    pub fn wait<T>(&self, limit: Duration, mut f: impl FnMut(&Self) -> Option<T>) -> T {
        let limit = super::patience(limit);
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

fn ok(result: Value) -> Response<Full<Bytes>> {
    let mut body = json!({ "ok": true });
    body["result"] = result;
    reply(200, &body)
}

async fn answer(
    req: Request<Incoming>,
    state: Arc<Mutex<State>>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let path = req.uri().path().to_string();
    let bytes = req
        .into_body()
        .collect()
        .await
        .map_or_default(http_body_util::Collected::to_bytes);
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let Some(rest) = path.strip_prefix(&format!("/bot{TOKEN}/")) else {
        return Ok(reply(
            401,
            &json!({"ok": false, "error_code": 401, "description": "Unauthorized"}),
        ));
    };
    let method = rest.to_string();
    let hang = {
        let mut s = state.lock().unwrap();
        s.seen.push(Seen {
            method: method.clone(),
            body: body.clone(),
            at: Instant::now(),
        });
        s.hang
    };
    if hang {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
    Ok(match method.as_str() {
        "getMe" => {
            let not_bot = state.lock().unwrap().not_bot;
            ok(json!({"id": BOT, "is_bot": !not_bot, "first_name": "bot"}))
        }
        "getWebhookInfo" => ok(json!({"url": state.lock().unwrap().webhook})),
        "getMyName" => ok(json!({"name": ""})),
        "setMyName" | "setMessageReaction" | "editMessageText" => ok(json!(true)),
        "getChat" => ok(json!({"id": body["chat_id"], "type": "private"})),
        "sendMessage" => {
            let id = {
                let mut s = state.lock().unwrap();
                s.next_message += 1;
                s.next_message
            };
            ok(json!({"message_id": id, "chat": {"id": body["chat_id"]}, "text": body["text"]}))
        }
        "getUpdates" => poll(&state, &body).await,
        _ => reply(404, &json!({"ok": false, "description": "Not Found"})),
    })
}

async fn poll(state: &Arc<Mutex<State>>, body: &Value) -> Response<Full<Bytes>> {
    if let Some((status, b)) = state.lock().unwrap().scripted.pop_front() {
        return reply(status, &b);
    }
    let offset = body["offset"].as_i64().unwrap_or(0);
    let wait = Duration::from_secs(body["timeout"].as_u64().unwrap_or(0));
    let limit = usize::try_from(body["limit"].as_u64().unwrap_or(100)).unwrap();
    let deadline = Instant::now() + wait;
    loop {
        let found: Vec<Value> = {
            let mut s = state.lock().unwrap();
            let from = s.replay_from.take().unwrap_or(offset);
            s.updates
                .iter()
                .filter(|u| u["update_id"].as_i64().unwrap() >= from)
                .take(limit)
                .cloned()
                .collect()
        };
        if !found.is_empty() || Instant::now() >= deadline {
            return ok(Value::Array(found));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
