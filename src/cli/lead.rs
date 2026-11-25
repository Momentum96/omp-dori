//! `cataly lead ...` (COORD-01, COORD-13, COORD-18; HOST-14, HOST-17, HOST-18).

use std::time::Duration;

use serde_json::{Value, json};

use super::client::Client;
use super::{CALL_TIMEOUT, Failure, exit, out, show};
use crate::cli::args::Parsed;
use crate::proto::method;

/// A start binds within 30 s and a stop ends within 22 s (DESIGN 9.4); both with margin.
const LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(75);

fn text(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => "-".into(),
        Some(other) => other.to_string(),
    }
}

fn print_view(v: &Value) {
    let reason = v
        .get("reason")
        .and_then(Value::as_str)
        .map_or_else(String::new, |r| format!(" ({r})"));
    out::line(&format!("lead     {}{reason}", text(v, "phase")));
    if let Some(s) = v.get("session").filter(|s| s.is_object()) {
        out::line(&format!(
            "session  {} {} on {}, status {}",
            text(s, "session_id"),
            text(s, "session_file"),
            text(s, "model"),
            text(s, "status")
        ));
        if let Some(q) = s.get("quota_blocked").and_then(Value::as_str) {
            out::line(&format!("quota    blocked: {q}"));
        }
        let dialogs = s
            .get("dialogs")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if dialogs > 0 {
            out::line(&format!("dialogs  {dialogs} pending"));
        }
    }
    out::line(&format!(
        "inbox    {} waiting, {} sent, {} in doubt",
        text(v, "waiting"),
        text(v, "sent"),
        text(v, "in_doubt")
    ));
}

fn simple(hub: &mut Client, name: &str, params: Value, as_json: bool) -> Result<(), Failure> {
    let reply = hub.call(name, params, CALL_TIMEOUT)?;
    show(reply, as_json, |v: Value| match v {
        Value::String(s) => out::line(&s),
        Value::Object(m) if m.is_empty() => out::line("done"),
        other => out::line(&other.to_string()),
    })
}

fn print_discord(v: &Value) {
    let h = v.get("health").cloned().unwrap_or(Value::Null);
    let reason = h
        .get("reason")
        .and_then(Value::as_str)
        .map_or_else(String::new, |r| format!(" ({r})"));
    out::line(&format!(
        "discord  {}{reason}, {}",
        text(&h, "state"),
        if v.get("healthy") == Some(&Value::Bool(true)) {
            "healthy"
        } else {
            "not healthy"
        }
    ));
    out::line(&format!(
        "bot      {} for owner {}, buttons {}",
        text(&h, "bot"),
        text(&h, "owner"),
        if h.get("buttons") == Some(&Value::Bool(true)) {
            "on"
        } else {
            "off"
        }
    ));
    let c = v.get("counts").cloned().unwrap_or(Value::Null);
    out::line(&format!(
        "rooms    {} watched, {} followed threads, {} held",
        text(&c, "rooms"),
        text(&h, "followed_threads"),
        text(&c, "held")
    ));
    out::line(&format!(
        "queue    {} waiting, {} answers to post, {} owed replies open",
        text(&c, "queued"),
        text(&c, "answers_waiting"),
        text(&c, "owed_open")
    ));
    for held in v
        .get("held")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        out::line(&format!(
            "held     room {}: {}",
            text(held, "room"),
            text(held, "reason")
        ));
    }
    if let Some(e) = h.get("last_error").and_then(Value::as_str) {
        out::line(&format!("last     {e}"));
    }
}

/// `discord` verbs; `None` for anything else.
pub(super) fn discord_verbs(
    p: &Parsed,
    hub: &mut Client,
    as_json: bool,
) -> Option<Result<(), Failure>> {
    let result = match p.spec.path {
        ["discord", "status"] => hub
            .call(method::DISCORD_STATUS, json!({}), CALL_TIMEOUT)
            .map_err(Failure::from)
            .and_then(|v| show(v, as_json, |v: Value| print_discord(&v))),
        ["discord", "release"] => simple(
            hub,
            method::DISCORD_RELEASE,
            json!({ "room": p.args.first() }),
            as_json,
        ),
        ["discord", "reset"] => simple(hub, method::DISCORD_RESET, json!({}), as_json),
        _ => return None,
    };
    Some(result)
}

/// `lead` verbs; `None` for anything else.
pub(super) fn verbs(p: &Parsed, hub: &mut Client, as_json: bool) -> Option<Result<(), Failure>> {
    let arg = |n: usize| p.args.get(n).cloned().unwrap_or_default();
    let index = || {
        arg(0)
            .parse::<usize>()
            .map_err(|_| Failure::Usage(format!("'{}' is not an entry number", arg(0))))
    };
    let result = match p.spec.path {
        ["lead", verb @ ("start" | "stop")] => {
            let (m, params) = if *verb == "start" {
                (method::LEAD_START, json!({ "fresh": p.flag("--fresh") }))
            } else {
                (method::LEAD_STOP, json!({ "force": p.flag("--force") }))
            };
            hub.call(m, params, LIFECYCLE_TIMEOUT)
                .map_err(Failure::from)
                .and_then(|v| show(v, as_json, |v: Value| print_view(&v)))
        }
        ["lead", "status"] => hub
            .call(method::LEAD_STATUS, json!({}), CALL_TIMEOUT)
            .map_err(Failure::from)
            .and_then(|v| show(v, as_json, |v: Value| print_view(&v))),
        ["lead", "send"] => send(p, hub, as_json),
        ["lead", "inbox"] => hub
            .call(method::LEAD_INBOX, json!({}), CALL_TIMEOUT)
            .map_err(Failure::from)
            .and_then(|v| {
                show(v, as_json, |v: Value| {
                    let rows = v.as_array().cloned().unwrap_or_default();
                    if rows.is_empty() {
                        out::line("the lead's inbox is empty");
                    }
                    for r in &rows {
                        let at = r
                            .get("queued_ms")
                            .and_then(Value::as_i64)
                            .map_or_else(String::new, crate::jobs::instant_text);
                        out::line(&format!(
                            "{:>3}  {:<9} {:<12} {at}  {}",
                            text(r, "index"),
                            text(r, "state"),
                            text(r, "sender"),
                            text(r, "preview")
                        ));
                    }
                })
            }),
        ["lead", verb @ ("confirm" | "release")] => index().and_then(|n| {
            let m = if *verb == "confirm" {
                method::LEAD_CONFIRM
            } else {
                method::LEAD_RELEASE
            };
            simple(hub, m, json!({ "index": n }), as_json)
        }),
        ["lead", "interrupt"] => simple(hub, method::LEAD_INTERRUPT, json!({}), as_json),
        ["lead", "model"] => simple(hub, method::LEAD_MODEL, json!({ "model": arg(0) }), as_json),
        ["lead", "accept-model"] => simple(hub, method::LEAD_ACCEPT_MODEL, json!({}), as_json),
        ["lead", "answer"] => match serde_json::from_str::<Value>(&arg(1)) {
            Ok(answer) => simple(
                hub,
                method::LEAD_ANSWER,
                json!({ "dialog": arg(0), "answer": answer }),
                as_json,
            ),
            Err(e) => Err(Failure::Usage(format!("the answer is not JSON: {e}"))),
        },
        ["lead", "events"] => {
            let after = p.value("--after").map_or(Ok(0), str::parse::<u64>);
            match after {
                Ok(after) => hub
                    .call(method::LEAD_EVENTS, json!({ "after": after }), CALL_TIMEOUT)
                    .map_err(Failure::from)
                    .and_then(|v| {
                        show(v, as_json, |v: Value| {
                            if v.get("missed") == Some(&Value::Bool(true)) {
                                out::line("(older events were missed)");
                            }
                            for e in v
                                .get("events")
                                .and_then(Value::as_array)
                                .into_iter()
                                .flatten()
                            {
                                out::line(&format!(
                                    "{:>5}  {}",
                                    e.get(0).map_or_else(String::new, Value::to_string),
                                    e.get(1).map_or_else(String::new, Value::to_string)
                                ));
                            }
                        })
                    }),
                Err(_) => Err(Failure::Usage("--after takes a sequence number".into())),
            }
        }
        _ => return None,
    };
    Some(result)
}

/// Send results: delivered or waiting exit 0; in doubt exits 5 (COORD req. 25).
fn send(p: &Parsed, hub: &mut Client, as_json: bool) -> Result<(), Failure> {
    let params = json!({
        "text": p.args.first(),
        "request_id": p.value("--request-id"),
        "quiet": p.flag("--quiet"),
    });
    let receipt = hub.call(method::LEAD_SEND, params, CALL_TIMEOUT)?;
    let state = text(&receipt, "state");
    if as_json {
        out::json(&receipt);
    } else {
        let line = match state.as_str() {
            "sent" => "sent to the lead; the transcript entry is not confirmed yet".to_string(),
            "delivered" => "delivered to the lead".into(),
            "waiting" => "waiting in the lead's inbox; it goes out when the lead can read".into(),
            "duplicate" => "an identical message is pending or was delivered".into(),
            other => other.to_string(),
        };
        out::line(&line);
    }
    match state.as_str() {
        "in_doubt" => Err(Failure::Status(
            exit::IN_DOUBT,
            "in doubt: do not send it again; check, then `cataly lead confirm` or `release`".into(),
        )),
        "failed" => Err(Failure::Refused(
            "the message could not be delivered".into(),
        )),
        _ => Ok(()),
    }
}
