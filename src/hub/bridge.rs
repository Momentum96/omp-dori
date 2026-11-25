//! The hub side of the bridge link (DESIGN 3.5, 8; EXT req. 3-5, 10-18, 41): a control connection
//! whose first request is `bridge.hello` becomes a bridge connection. The hub assigns the role by
//! launch token, reconnect key or the lead's session identity; anything else is told `none` and
//! stays inert. Over an accepted connection the bridge sends events, tool calls and the stop
//! question; the hub sends heartbeats and answers. Headless sessions are never controlled from here.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncWriteExt};
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::mpsc;

use super::Hub;
use super::socket::{Line, read_line};
use crate::host::Role;
use crate::host::inbox::{HUB_SENDER, Message};
use crate::jobs::event::{Emit, EventKind, Severity};
use crate::log;
use crate::proto::{Response, code};
use crate::store::lead::{LeadDone, LeadWrite, REMINDERS_MAX};
use crate::store::relay::OWED_WINDOW_MS;
use crate::sys::now_ms;

/// The bridge protocol this hub speaks (DESIGN 8 "Versioning").
pub(crate) const PROTOCOL: u64 = 1;
/// The first request of a bridge connection.
pub(crate) const HELLO: &str = "bridge.hello";
const HEARTBEAT: Duration = Duration::from_secs(15);
const SILENT: Duration = Duration::from_secs(45);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// The guidance appended to the lead's system prompt (EXT req. 36-37, OPC list).
pub(crate) const LEAD_GUIDANCE: &str = include_str!("../../assets/lead.md");
/// Obligations listed in one stop notice (DISC-OUT req. 35).
const STOP_LIST: usize = 5;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Hello {
    ext: u64,
    token: Option<String>,
    key: Option<String>,
    session: String,
    agent: String,
    marker: bool,
}

/// Who the hello is, if anyone: the session handle and role, and whether this is a fresh start.
fn decide(hub: &Hub, hello: &Hello) -> Result<(String, Role, bool), String> {
    if hello.ext != PROTOCOL {
        return Err(format!(
            "bridge protocol {} where this hub speaks {PROTOCOL}; install the matching bridge",
            hello.ext
        ));
    }
    if hello.agent == "sub" {
        return Err("subagents have no role".into());
    }
    if let Some(token) = hello.token.as_deref().filter(|t| !t.is_empty()) {
        return hub
            .links
            .redeem(token)
            .map(|(h, r)| (h, r, true))
            .ok_or_else(|| "a used or unknown launch token".into());
    }
    if let Some(found) = hello.key.as_deref().and_then(|k| hub.links.by_key(k)) {
        return Ok((found.0, found.1, false));
    }
    // A resume by hand carries the marker; the hub decides by the session's identity (EXT-02).
    if hello.marker
        && let Some(s) = hub.lead.snapshot()
        && !hello.session.is_empty()
        && s.session_id.as_deref() == Some(hello.session.as_str())
    {
        return Ok((s.handle.clone(), Role::Lead, true));
    }
    Err("not a session of this hub".into())
}

async fn write(writer: &mut OwnedWriteHalf, response: &Response) -> bool {
    let line = response.to_line();
    matches!(
        tokio::time::timeout(WRITE_TIMEOUT, writer.write_all(line.as_bytes())).await,
        Ok(Ok(()))
    )
}

/// Serves one bridge connection from its hello until it closes or falls silent (EXT req. 15).
pub(crate) async fn serve<R: AsyncBufRead + Unpin>(
    hub: &Arc<Hub>,
    id: u64,
    params: Value,
    reader: &mut R,
    mut writer: OwnedWriteHalf,
) {
    let hello: Hello = serde_json::from_value(params).unwrap_or_default();
    let (handle, role, fresh) = match decide(hub, &hello) {
        Ok(found) => found,
        Err(reason) => {
            if hello.ext != PROTOCOL {
                let emit = Emit::new(EventKind::BridgeRefused, Severity::Warning, false, "bridge")
                    .with("reason", &reason);
                let _ = hub.jobs.hub_event(emit).await;
            }
            log::debug("bridge refused", &[("reason", &reason)]);
            let _ = write(
                &mut writer,
                &Response::ok(id, json!({ "role": "none", "reason": reason })),
            )
            .await;
            return;
        }
    };
    let key = crate::host::random_hex(16);
    hub.links.connect(&handle, role, &key, &hello.session);
    let answer = json!({
        "role": "lead",
        "key": key,
        "guidance": LEAD_GUIDANCE,
        "tools": [super::tool::definition()],
        "status": "",
    });
    if !write(&mut writer, &Response::ok(id, answer)).await {
        hub.links.disconnect(&handle, &key);
        return;
    }
    log::info(
        "bridge accepted",
        &[("session", &hello.session), ("handle", &handle)],
    );
    if fresh {
        recap(hub, &handle, "start").await;
    }
    let (tx, mut rx) = mpsc::channel::<Response>(64);
    let writer_task = tokio::spawn(async move {
        let mut beat = tokio::time::interval(HEARTBEAT);
        beat.tick().await;
        loop {
            let response = tokio::select! {
                next = rx.recv() => match next { Some(r) => r, None => return },
                _ = beat.tick() => Response::item(0, json!({ "ping": true })),
            };
            if !write(&mut writer, &response).await {
                return;
            }
        }
    });
    loop {
        let line = match tokio::time::timeout(SILENT, read_line(reader)).await {
            Ok(Line::Text(line)) => line,
            Ok(Line::TooLong | Line::Closed) | Err(_) => break,
        };
        let Ok(request) = crate::proto::decode(&line) else {
            continue;
        };
        match request.method.as_str() {
            "bridge.ping" => {}
            "bridge.event" => event(hub, &handle, &request.params).await,
            "bridge.switch" => {
                let session = request.params.get("session").and_then(Value::as_str);
                hub.links.observe(&handle, 0, session, 0);
                let _ = tx
                    .send(Response::ok(request.id, json!({ "role": "lead" })))
                    .await;
            }
            "bridge.tool" | "bridge.stop" => {
                let (hub, tx) = (Arc::clone(hub), tx.clone());
                tokio::spawn(async move {
                    let reply = if request.method == "bridge.tool" {
                        tool(&hub, &request.params).await
                    } else {
                        let answered =
                            request.params.get("answered").and_then(Value::as_bool) == Some(true);
                        json!({ "context": stop_gate(&hub, answered).await })
                    };
                    let _ = tx.send(Response::ok(request.id, reply)).await;
                });
            }
            other => {
                let reply = Response::err(
                    request.id,
                    code::UNKNOWN_METHOD,
                    format!("unknown bridge method {other}"),
                );
                let _ = tx.send(reply).await;
            }
        }
    }
    writer_task.abort();
    hub.links.disconnect(&handle, &key);
    log::info("bridge closed", &[("handle", &handle)]);
}

async fn event(hub: &Arc<Hub>, handle: &str, params: &Value) {
    let seq = params.get("seq").and_then(Value::as_u64).unwrap_or(0);
    let session = params.get("session").and_then(Value::as_str);
    let kind = params
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let dropped = if kind == "dropped" {
        params.get("count").and_then(Value::as_u64).unwrap_or(0)
    } else {
        0
    };
    hub.links.observe(handle, seq, session, dropped);
    if kind == "session_compact" {
        recap(hub, handle, &format!("compact:{seq}")).await;
    }
}

/// One tool call, answered once per call ID (EXT req. 18).
async fn tool(hub: &Arc<Hub>, params: &Value) -> Value {
    let call = params
        .get("call")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if !call.is_empty()
        && let Some(text) = hub.links.answered(&call)
    {
        return json!({ "text": text });
    }
    let input = params.get("input").cloned().unwrap_or(Value::Null);
    let (text, error) = match super::tool::call(hub, &input).await {
        Ok(text) => (text, false),
        Err(text) => (text, true),
    };
    if !call.is_empty() && !error {
        hub.links.remember(&call, &text);
    }
    json!({ "text": text, "error": error })
}

/// Puts the recap in front of the lead as a note that goes out with the next wake (EVR req. 48,
/// OPC req. 57); `reason` keeps each start and each compaction its own.
async fn recap(hub: &Arc<Hub>, handle: &str, reason: &str) {
    let text = super::tool::recap(hub).await;
    let message = Message {
        sender: HUB_SENDER.into(),
        text,
        urgent: false,
        owner: false,
        identity: Some(format!("recap:{handle}:{reason}")),
        request_id: None,
    };
    if let Err(e) = hub.lead.deliver(message).await {
        log::warn("recap not delivered", &[("error", &e)]);
    }
}

/// The stop gate (DISC-OUT req. 35-37, DESIGN 12.2 C8): when the lead stops without a publishable
/// answer and owner messages are still owed, it continues once with a notice; each obligation
/// takes one of its three reminders. Quota-blocked leads get nothing (OPC-64).
pub(crate) async fn stop_gate(hub: &Hub, answered: bool) -> Option<String> {
    if answered
        || hub
            .lead
            .snapshot()
            .is_some_and(|s| s.quota_blocked.is_some())
    {
        return None;
    }
    let since = now_ms() - OWED_WINDOW_MS;
    let open = hub
        .store
        .read(move |r| r.owed_open("discord", since))
        .await
        .ok()?;
    let open: Vec<_> = open
        .into_iter()
        .filter(|o| o.reminders < REMINDERS_MAX)
        .collect();
    let mut lines = Vec::new();
    for o in open.iter().take(STOP_LIST) {
        let took = hub
            .store
            .lead(LeadWrite::Remind {
                relay: "discord".into(),
                room: o.room.clone(),
                message: o.message.clone(),
            })
            .await;
        if matches!(took, Ok(LeadDone::Count(1))) {
            lines.push(owed_line(&o.room, &o.message, o.admitted_ms, &o.text));
        }
    }
    if lines.is_empty() {
        return None;
    }
    let more = open.len().saturating_sub(STOP_LIST);
    let mut text = format!(
        "[reminder] These owner messages have no answer yet:\n{}",
        lines.join("\n")
    );
    if more > 0 {
        let _ = write!(text, "\nand {more} more");
    }
    text.push_str("\nAnswer each, or say in one line what happens next.");
    Some(text)
}

/// One owed message in a reminder. It carries the request's tag, so the answer the reminder brings
/// is captured and posted as the linked reply to that message.
pub(crate) fn owed_line(room: &str, message: &str, at_ms: i64, text: &str) -> String {
    let quoted: String = text.chars().take(200).collect();
    format!(
        "- [discord {room}/{message}] at {}: \"{}\"",
        crate::jobs::instant_text(at_ms),
        quoted.replace('\n', " ")
    )
}
