//! A fake model provider on loopback that speaks the `OpenAI` chat-completions API OMP uses for a
//! `models.yml` custom provider with `api: openai-completions`. Never a real model.
//!
//! The reply depends on the last user text: `SLOW` streams for about 4 s (follow-up, steer and abort
//! cases), `TOOL` asks for a `bash` call first (an approval dialog under always-ask), anything else
//! answers `ok` at once. `ASK` calls the `ask` tool with one question whose recommended choice is
//! the second. The model ID `limited` answers 429 with a usage-limit error.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use http_body_util::BodyExt;
use hyper::body::{Bytes, Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// A response body fed from a channel, so chunks can be spaced in time.
pub struct ChannelBody(mpsc::Receiver<Bytes>);

impl hyper::body::Body for ChannelBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.0.poll_recv(cx).map(|b| b.map(|b| Ok(Frame::data(b))))
    }
}

pub struct FakeModel {
    pub port: u16,
    /// Model IDs of the requests seen, in order.
    seen: Arc<Mutex<Vec<String>>>,
    /// The request bodies, in order (system prompt, tools, messages).
    bodies: Arc<Mutex<Vec<Value>>>,
}

/// The `models.yml` naming this provider with the given model IDs.
pub fn models_yml(port: u16, ids: &[&str]) -> String {
    let mut text = format!(
        "providers:\n  fake:\n    baseUrl: http://127.0.0.1:{port}/v1\n    api: openai-completions\n    auth: none\n    models:\n"
    );
    for id in ids {
        let _ = std::fmt::Write::write_fmt(
            &mut text,
            format_args!("      - id: {id}\n        name: Fake {id}\n"),
        );
        text.push_str("        contextWindow: 32768\n        maxTokens: 1024\n");
    }
    text
}

impl FakeModel {
    pub fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let shared = (Arc::clone(&seen), Arc::clone(&bodies));
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
                    let shared = shared.clone();
                    tokio::spawn(async move {
                        let service = service_fn(move |req| answer(req, shared.clone()));
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            });
        });
        Self { port, seen, bodies }
    }

    pub fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    pub fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().unwrap().clone()
    }
}

fn chunk(model: &str, delta: &Value, finish: Option<&str>) -> Bytes {
    let mut body = json!({
        "id": "fake-1", "object": "chat.completion.chunk", "created": 0, "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    });
    if finish.is_some() {
        body["usage"] = json!({"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12});
    }
    Bytes::from(format!("data: {body}\n\n"))
}

fn last_user_text(body: &Value) -> String {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let last = messages.iter().rev().find(|m| m["role"] == "user");
    match last.map(|m| &m["content"]) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// What the server records: model IDs and request bodies.
type Records = (Arc<Mutex<Vec<String>>>, Arc<Mutex<Vec<Value>>>);

async fn answer(
    req: Request<Incoming>,
    (seen, bodies): Records,
) -> Result<Response<ChannelBody>, Infallible> {
    let bytes = req.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let model = body["model"].as_str().unwrap_or_default().to_string();
    seen.lock().unwrap().push(model.clone());
    bodies.lock().unwrap().push(body.clone());
    let (tx, rx) = mpsc::channel(64);
    if model == "limited" {
        let error = json!({"error": {
            "message": "You have hit your usage limit for this plan",
            "type": "usage_limit_reached", "code": "usage_limit_reached",
        }});
        tx.send(Bytes::from(error.to_string())).await.unwrap();
        return Ok(Response::builder()
            .status(429)
            .header("content-type", "application/json")
            .body(ChannelBody(rx))
            .unwrap());
    }
    let text = last_user_text(&body);
    let after_tool = body["messages"]
        .as_array()
        .is_some_and(|m| m.last().is_some_and(|l| l["role"] == "tool"));
    tokio::spawn(async move {
        let role = json!({"role": "assistant", "content": ""});
        let _ = tx.send(chunk(&model, &role, None)).await;
        if text.contains("ASK") && !after_tool {
            let args = json!({"questions": [{"id": "color", "question": "Pick a color",
                "options": [{"label": "Red"}, {"label": "Blue"}], "recommended": 1}]});
            let call = json!({"tool_calls": [{"index": 0, "id": "call_ask", "type": "function",
                "function": {"name": "ask", "arguments": args.to_string()}}]});
            let _ = tx.send(chunk(&model, &call, None)).await;
            let _ = tx.send(chunk(&model, &json!({}), Some("tool_calls"))).await;
        } else if text.contains("TOOL") && !after_tool {
            let call = json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                "function": {"name": "bash", "arguments": "{\"command\":\"echo cataly-contract\"}"}}]});
            let _ = tx.send(chunk(&model, &call, None)).await;
            let _ = tx.send(chunk(&model, &json!({}), Some("tool_calls"))).await;
        } else {
            let steps = if text.contains("SLOW") { 8 } else { 1 };
            for n in 0..steps {
                if n > 0 {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                let part = json!({"content": if n == 0 { "ok" } else { "." }});
                if tx.send(chunk(&model, &part, None)).await.is_err() {
                    return;
                }
            }
            let _ = tx.send(chunk(&model, &json!({}), Some("stop"))).await;
        }
        let _ = tx.send(Bytes::from_static(b"data: [DONE]\n\n")).await;
    });
    Ok(Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(ChannelBody(rx))
        .unwrap())
}
