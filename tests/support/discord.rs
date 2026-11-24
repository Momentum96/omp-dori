//! A fake Discord REST server on hyper, bound to loopback only (DISC-OUT-27). It answers the calls
//! job posting makes (channel read, message create), records every request, deduplicates creates by
//! their enforced nonce as Discord does, and can answer the next creates with scripted statuses.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};

/// One request as the server saw it.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
    pub body: Value,
}

#[derive(Default)]
struct State {
    seen: Vec<Seen>,
    /// Statuses for the next message creates, before normal answers resume.
    scripted: VecDeque<u16>,
    /// Enforced nonce -> message ID already created.
    nonces: HashMap<String, u64>,
    next_id: u64,
    /// Distinct messages created.
    created: usize,
}

pub struct FakeDiscord {
    pub port: u16,
    state: Arc<Mutex<State>>,
}

impl FakeDiscord {
    pub fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(State {
            next_id: 900_000_000_000_000_000,
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

    /// Answers the next message creates with these statuses (for example 500) after creating them.
    pub fn script(&self, statuses: &[u16]) {
        self.state.lock().unwrap().scripted.extend(statuses);
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.state.lock().unwrap().seen.clone()
    }

    /// Distinct messages that exist in the fake channel.
    pub fn messages(&self) -> usize {
        self.state.lock().unwrap().created
    }

    pub fn creates(&self) -> Vec<Seen> {
        self.seen()
            .into_iter()
            .filter(|s| s.method == "POST" && s.path.ends_with("/messages"))
            .collect()
    }

    /// Waits until at least `n` creates arrived.
    pub fn wait_creates(&self, n: usize, limit: Duration) -> Vec<Seen> {
        let deadline = std::time::Instant::now() + limit;
        loop {
            let creates = self.creates();
            if creates.len() >= n {
                return creates;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{n} creates not seen within {limit:?}; saw {creates:?}"
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

async fn answer(
    req: Request<Incoming>,
    state: Arc<Mutex<State>>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let authorization = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = req.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let mut s = state.lock().unwrap();
    s.seen.push(Seen {
        method: method.clone(),
        path: path.clone(),
        authorization,
        body: body.clone(),
    });
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let response = match (method.as_str(), parts.as_slice()) {
        ("GET", ["channels", id]) => reply(200, &json!({ "id": id, "type": 0 })),
        ("POST", ["channels", id, "messages"]) => {
            let nonce = body["nonce"].as_str().unwrap_or_default().to_string();
            let enforced = body["enforce_nonce"] == true;
            let existing = s.nonces.get(&nonce).copied().filter(|_| enforced);
            let message = if let Some(m) = existing {
                m
            } else {
                s.next_id += 1;
                s.created += 1;
                s.next_id
            };
            if enforced && !nonce.is_empty() {
                s.nonces.insert(nonce, message);
            }
            // A scripted status answers after the message exists: the sender cannot know.
            if let Some(status) = s.scripted.pop_front() {
                return Ok(reply(status, &json!({ "message": "scripted" })));
            }
            reply(
                200,
                &json!({ "id": message.to_string(), "channel_id": id, "content": body["content"] }),
            )
        }
        _ => reply(404, &json!({ "message": "Unknown", "code": 0 })),
    };
    Ok(response)
}
