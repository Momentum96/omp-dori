//! The lead's `cataly` tool (DESIGN 10.6; WS-02, EVR-22 to EVR-25, EVR-28, OPC-79/80): one tool
//! whose `op` names the operation, so new operations need no bridge release. The bridge forwards
//! each call; every rule is applied here. The same module builds the recap (EVR req. 48-49).

use std::fmt::Write as _;
use std::sync::Arc;

use serde_json::{Value, json};

use super::Hub;
use crate::jobs::instant_text;
use crate::store::lead::{LeadDone, LeadWrite, QuestionRow, RequestRow};
use crate::sys::now_ms;

/// The recap is at most this long (EVR req. 48).
const RECAP_MAX: usize = 8 * 1024;
/// The profile is capped (OPC req. 55).
pub(crate) const PROFILE_MAX: usize = 16 * 1024;
/// Profile versions kept for undo.
const PROFILE_KEEP: usize = 30;

/// The tool as the bridge registers it: name, description and JSON Schema from the hub.
pub(crate) fn definition() -> Value {
    let text = |d: &str| json!({ "type": "string", "description": d });
    let int = |d: &str| json!({ "type": "integer", "description": d });
    json!({
        "name": "cataly",
        "label": "Cataly",
        "description": "Cataly's records and watches. op is one of: requests (all?), request.add (title, source, thread?), \
    request.next (id, text), request.note (id, text), request.close (id, text); questions, question.ask (text, source, \
    options, recommended?, settle owner|lead, thread?, request?, room?), question.answer (id, text, message), \
    question.choose (id, option, reason, message), question.withdraw (id, reason); jobs, job.start (name, argv, \
    filter, restarts?, delay?, silence?, low?), job.stop (name), job.run (name: one dry run of an owner job); \
    digest (since?); recap; profile.read, profile.save (text), profile.undo. Lanes: lane.open (key, title, brief or \
    brief_file, checks: list of {kind: pr_merged|issue_closed|command|file|web|pr_policy, ...}, thread? or \
    thread_from? discord:<room>/<message>, cwd?, model?, origin?, look_only?, ceilings?, worktree?, preview?), \
    lane.list (all?), lane.board (key?), lane.pause|resume (key, reason?), lane.park|abandon (key, reason, \
    condition?), lane.object (key, reason), lane.close (key, note?, lesson?), lane.reopen (key, note?), lane.send \
    (key, text, id?, steer?), lane.release (key, note), lane.read (key, count?), lane.answer (key, dialog, answer), \
    lane.grant (key, rule, reason), lane.drift, lane.spend (key?).",
        "parameters": {
            "type": "object",
            "properties": {
                "op": text("the operation"),
                "id": text("request-log item number or question ID"),
                "text": text("the text: next step, note, result, question, answer or profile"),
                "title": text("one line"),
                "source": text("where it came from, such as discord:<room>:<message>"),
                "thread": text("the thread or room it belongs to"),
                "room": text("the Discord channel to post a question in (default the home channel)"),
                "message": text("the owner's or the announcing message ID"),
                "reason": text("why"),
                "options": { "type": "array", "items": { "type": "string" } },
                "recommended": int("the recommended option, from 1"),
                "option": int("the chosen option, from 1"),
                "settle": text("owner (default) or lead"),
                "request": int("the request-log item the question belongs to"),
                "name": text("job name"),
                "argv": { "type": "array", "items": { "type": "string" } },
                "filter": text("regular expression an output line must match"),
                "restarts": int("restarts allowed, 0 to 100 (default 5)"),
                "delay": int("first restart delay in seconds (default 10)"),
                "silence": int("alert after this many quiet minutes"),
                "low": { "type": "boolean", "description": "matches wait for the next wake" },
                "since": text("ISO instant the digest starts at"),
                "all": { "type": "boolean" },
                "calls": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["op"]
        }
    })
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).map(str::trim)
}

fn i(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(|x| {
        x.as_i64()
            .or_else(|| x.as_str().and_then(|t| t.trim().parse().ok()))
    })
}

/// A required text of 1 to `max` characters; `one_line` refuses line breaks.
fn need<'a>(v: &'a Value, key: &str, max: usize, one_line: bool) -> Result<&'a str, String> {
    let text = s(v, key)
        .filter(|t| !t.is_empty())
        .ok_or(format!("{key} is required"))?;
    if text.chars().count() > max {
        return Err(format!("{key} is longer than {max} characters"));
    }
    if one_line && text.contains('\n') {
        return Err(format!("{key} must be one line"));
    }
    Ok(text)
}

fn done(d: LeadDone, ok: String) -> Result<String, String> {
    match d {
        LeadDone::Refused(why) => Err(why),
        _ => Ok(ok),
    }
}

/// Runs one call; `Err` is shown to the model as a tool error.
pub(crate) async fn call(hub: &Arc<Hub>, input: &Value) -> Result<String, String> {
    let op = s(input, "op").unwrap_or_default();
    if let Some(lane_op) = op.strip_prefix("lane.") {
        let mut lane_input = input.clone();
        if let Some(map) = lane_input.as_object_mut() {
            map.insert("op".into(), json!(lane_op));
        }
        return hub
            .lanes
            .call(crate::lanes::Caller::Lead, &lane_input)
            .await
            .map(|v| match v {
                Value::String(s) => s,
                other => other.to_string(),
            });
    }
    match op {
        "requests" => {
            let all = input.get("all").and_then(Value::as_bool).unwrap_or(false);
            let rows = hub
                .store
                .read(move |r| r.requests(!all))
                .await
                .map_err(|e| e.to_string())?;
            Ok(list_requests(&rows, usize::MAX))
        }
        "request.add" => {
            let write = LeadWrite::RequestAdd {
                title: need(input, "title", 200, true)?.into(),
                source: need(input, "source", 400, true)?.into(),
                thread: s(input, "thread").map(str::to_string),
            };
            match hub.store.lead(write).await.map_err(|e| e.to_string())? {
                LeadDone::Id(id) => Ok(format!("request-log item {id} open")),
                other => done(other, String::new()),
            }
        }
        "request.next" | "request.note" | "request.close" => {
            let id = i(input, "id").ok_or("id is required")?;
            let text = need(input, "text", 1000, false)?.to_string();
            let (write, said) = match op {
                "request.next" => (LeadWrite::RequestNext { id, text }, "next step set"),
                "request.note" => (LeadWrite::RequestNote { id, text }, "note added"),
                _ => (LeadWrite::RequestClose { id, result: text }, "closed"),
            };
            let d = hub.store.lead(write).await.map_err(|e| e.to_string())?;
            done(d, format!("item {id}: {said}"))
        }
        "questions" => {
            let rows = hub
                .store
                .read(|r| r.questions(true))
                .await
                .map_err(|e| e.to_string())?;
            Ok(list_questions(&rows, usize::MAX))
        }
        "question.ask" => ask(hub, input).await,
        "question.answer" | "question.choose" | "question.withdraw" => settle(hub, op, input).await,
        "jobs" => Ok(hub.streams.list()),
        "job.start" => hub.streams.start(input).await,
        "job.stop" => hub.streams.stop(need(input, "name", 64, true)?).await,
        "job.run" => {
            let name = need(input, "name", 64, true)?.to_string();
            let events = hub.jobs.run_once(&name).await.map_err(|e| e.to_string())?;
            let lines: Vec<String> = events
                .iter()
                .map(|e| format!("- {} {}", e.kind, e.text))
                .collect();
            Ok(format!(
                "{name}: one dry run, {} event(s)\n{}",
                lines.len(),
                lines.join("\n")
            ))
        }
        "digest" => {
            let since = match s(input, "since") {
                Some(t) => t
                    .parse::<jiff::Timestamp>()
                    .map_err(|e| format!("since: {e}"))?
                    .as_millisecond(),
                None => now_ms() - 24 * 3_600_000,
            };
            Ok(crate::relay::discord::away::build(
                hub.store.clone(),
                hub.discord_rest(),
                &hub.config(),
                since,
            )
            .await
            .0)
        }
        "recap" => Ok(recap(hub).await),
        "profile.read" => profile_read(hub),
        "profile.save" => profile_save(hub, need(input, "text", PROFILE_MAX, false)?),
        "profile.undo" => profile_undo(hub),
        "" => Err("op is required".into()),
        other => Err(format!("unknown op {other}; see the tool description")),
    }
}

/// Records a question and posts it, bound to its message (EVR req. 34-36).
async fn ask(hub: &Arc<Hub>, input: &Value) -> Result<String, String> {
    let text = need(input, "text", 1000, true)?;
    let source = need(input, "source", 400, true)?;
    let options: Vec<String> = input
        .get("options")
        .and_then(Value::as_array)
        .map_or_default(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|o| o.trim().to_string())
                .collect()
        });
    if options.is_empty() || options.len() > 9 {
        return Err("options: 1 to 9 are required".into());
    }
    if options
        .iter()
        .any(|o| o.is_empty() || o.chars().count() > 200 || o.contains('\n'))
    {
        return Err("each option is one line of 1 to 200 characters".into());
    }
    let recommended = i(input, "recommended");
    if recommended.is_some_and(|r| r < 1 || r > i64::try_from(options.len()).unwrap_or(0)) {
        return Err("recommended must name one of the options".into());
    }
    let lead_may = s(input, "settle") == Some("lead");
    let thread = s(input, "thread").map(str::to_string);
    if lead_may && (options.len() < 2 || thread.is_none()) {
        return Err("a question the lead may settle needs at least 2 options and a thread".into());
    }
    let id = crate::host::random_hex(4);
    let neutral = crate::relay::discord::questions::neutral;
    let mut body = format!("**Question {id}**\n{}", neutral(text));
    for (n, o) in options.iter().enumerate() {
        let mark = if recommended == i64::try_from(n + 1).ok() {
            " (recommended)"
        } else {
            ""
        };
        let _ = write!(body, "\n{}. {}{mark}", n + 1, neutral(o));
    }
    body.push_str("\nReply to this message with a number or your answer.");
    let config = hub.config();
    let room = s(input, "room")
        .and_then(crate::relay::discord::Snowflake::parse)
        .or_else(|| config.discord.home());
    let posted = match (room, hub.discord_rest()) {
        (Some(room), Some(rest)) => {
            let nonce = crate::relay::discord::nonce(&format!("ledger:{id}"), 0);
            let created = rest
                .create(room, json!({ "content": body, "nonce": nonce }))
                .await;
            created.ok().and_then(|v| {
                v.get("id")
                    .and_then(Value::as_str)
                    .map(|m| (room.to_string(), m.to_string()))
            })
        }
        _ => None,
    };
    let row = QuestionRow {
        id: id.clone(),
        text: text.into(),
        source: source.into(),
        thread,
        options,
        recommended,
        lead_may,
        request: i(input, "request"),
        room: posted.as_ref().map(|p| p.0.clone()),
        message: posted.as_ref().map(|p| p.1.clone()),
        state: "open".into(),
        outcome: None,
        created_ms: 0,
    };
    let d = hub
        .store
        .lead(LeadWrite::QuestionAsk(Box::new(row)))
        .await
        .map_err(|e| e.to_string())?;
    let said = match &posted {
        Some((room, message)) => {
            format!("question {id} recorded and posted in {room} as message {message}")
        }
        None => format!(
            "question {id} recorded; it could not be posted, so ask it yourself and quote its ID"
        ),
    };
    done(d, said)
}

/// Answer by hand, the lead's own choice, or withdrawal (EVR req. 38-40).
async fn settle(hub: &Arc<Hub>, op: &str, input: &Value) -> Result<String, String> {
    let id = need(input, "id", 16, true)?.to_string();
    let q = {
        let id = id.clone();
        hub.store
            .read(move |r| r.question(&id))
            .await
            .map_err(|e| e.to_string())?
    }
    .ok_or(format!("no question {id}"))?;
    let (state, outcome) = match op {
        "question.answer" => {
            let message = need(input, "message", 64, true)?;
            let text = need(input, "text", 1000, false)?;
            ("answered", format!("owner message {message}: {text}"))
        }
        "question.choose" => {
            if !q.lead_may {
                return Err(format!("question {id} is the owner's to settle"));
            }
            let option = i(input, "option").ok_or("option is required")?;
            let picked = usize::try_from(option - 1)
                .ok()
                .and_then(|n| q.options.get(n))
                .ok_or(format!("option must be 1 to {}", q.options.len()))?;
            let reason = need(input, "reason", 1000, false)?;
            let message = need(input, "message", 64, true)?;
            if s(input, "thread").is_some_and(|t| Some(t) != q.thread.as_deref()) {
                return Err("the choice must be announced in the question's own thread".into());
            }
            (
                "chosen",
                format!("option {option} ({picked}) by the lead, announced in {message}: {reason}"),
            )
        }
        _ => ("withdrawn", need(input, "reason", 1000, false)?.to_string()),
    };
    let d = hub
        .store
        .lead(LeadWrite::QuestionSettle {
            id: id.clone(),
            state: state.into(),
            outcome,
        })
        .await
        .map_err(|e| e.to_string())?;
    done(d, format!("question {id} {state}"))
}

fn age_hours(created_ms: i64) -> i64 {
    (now_ms() - created_ms).max(0) / 3_600_000
}

fn list_requests(rows: &[RequestRow], max: usize) -> String {
    if rows.is_empty() {
        return "no open request-log items".into();
    }
    let mut out = String::new();
    for r in rows {
        let line = match &r.closed_ms {
            Some(_) => format!(
                "- {} [closed] {}: {}",
                r.id,
                r.title,
                r.result.as_deref().unwrap_or("")
            ),
            None => format!(
                "- {} {} (from {}, {} h); next: {}",
                r.id,
                r.title,
                r.source,
                age_hours(r.created_ms),
                r.next.as_deref().unwrap_or("not set")
            ),
        };
        if out.len() + line.len() > max {
            let _ = write!(out, "\nand {} more", rows.len() - out.lines().count());
            break;
        }
        out.push_str(&line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

fn list_questions(rows: &[QuestionRow], max: usize) -> String {
    if rows.is_empty() {
        return "no open owner questions".into();
    }
    let mut out = String::new();
    for q in rows {
        let who = if q.lead_may {
            "lead may settle"
        } else {
            "owner only"
        };
        let options: Vec<String> = q
            .options
            .iter()
            .enumerate()
            .map(|(n, o)| {
                let star = if q.recommended == i64::try_from(n + 1).ok() {
                    "*"
                } else {
                    ""
                };
                format!("{}{star}. {o}", n + 1)
            })
            .collect();
        let line = format!(
            "- {} ({} h, {who}{}) {} [{}]",
            q.id,
            age_hours(q.created_ms),
            q.thread
                .as_deref()
                .map_or_default(|t| format!(", thread {t}")),
            q.text,
            options.join("; ")
        );
        if out.len() + line.len() > max {
            let _ = write!(out, "\nand {} more", rows.len() - out.lines().count());
            break;
        }
        out.push_str(&line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// The recap (EVR req. 48-49): lanes, open request-log items with next steps, open questions, open
/// owner obligations and damaged records, at most 8 KiB. Producing it changes nothing.
pub(crate) async fn recap(hub: &Hub) -> String {
    let store = &hub.store;
    let requests = store.read(|r| r.requests(true)).await;
    let questions = store.read(|r| r.questions(true)).await;
    let since = now_ms() - crate::store::relay::OWED_WINDOW_MS;
    let owed = store.read(move |r| r.owed_open("discord", since)).await;
    let mut damaged = Vec::new();
    let mut out = String::from(
        "[recap] Cataly's records now (read them before acting on recollection).\nLanes: none (lanes arrive later).\n",
    );
    let budget = RECAP_MAX / 3;
    out.push_str("Open requests:\n");
    match requests {
        Ok(rows) => out.push_str(&list_requests(&rows, budget)),
        Err(e) => damaged.push(format!("request log ({e})")),
    }
    out.push_str("\nOpen owner questions:\n");
    match questions {
        Ok(rows) => out.push_str(&list_questions(&rows, budget)),
        Err(e) => damaged.push(format!("question ledger ({e})")),
    }
    match owed {
        Ok(rows) => {
            let _ = write!(
                out,
                "\nOwner messages without a linked answer: {}",
                rows.len()
            );
        }
        Err(e) => damaged.push(format!("owed replies ({e})")),
    }
    if !damaged.is_empty() {
        let _ = write!(out, "\nUnreadable records: {}", damaged.join(", "));
    }
    if out.len() > RECAP_MAX {
        let mut end = RECAP_MAX;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out
}

fn profile_paths(hub: &Hub) -> (std::path::PathBuf, std::path::PathBuf) {
    (
        hub.paths.state_dir.join("profile.md"),
        hub.paths.files_dir.join("profile"),
    )
}

fn profile_read(hub: &Hub) -> Result<String, String> {
    let (file, _) = profile_paths(hub);
    match crate::sys::fs::read_private(&file, PROFILE_MAX as u64 + 1) {
        Ok(Some(bytes)) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Ok(None) => Ok("the profile is empty".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// Saves the whole profile, keeping the previous version for undo (OPC req. 55). Credential-shaped
/// strings are masked first (OPC req. 53).
fn profile_save(hub: &Hub, text: &str) -> Result<String, String> {
    let (file, dir) = profile_paths(hub);
    let text = crate::sys::secrets::mask(text);
    if text.len() > PROFILE_MAX {
        return Err(
            "the profile is capped at 16 KiB; move the oldest part to an archive note first".into(),
        );
    }
    crate::sys::fs::ensure_private_dir(&dir).map_err(|e| e.to_string())?;
    if let Ok(Some(old)) = crate::sys::fs::read_private(&file, PROFILE_MAX as u64 + 1) {
        crate::sys::fs::write_atomic(&dir.join(format!("{}.md", now_ms())), &old)
            .map_err(|e| e.to_string())?;
        let mut kept: Vec<_> = std::fs::read_dir(&dir)
            .map_or_default(|d| d.filter_map(Result::ok).map(|e| e.path()).collect());
        kept.sort();
        let excess = kept.len().saturating_sub(PROFILE_KEEP);
        for old in kept.iter().take(excess) {
            let _ = std::fs::remove_file(old);
        }
    }
    crate::sys::fs::write_atomic(&file, text.as_bytes()).map_err(|e| e.to_string())?;
    Ok(format!(
        "profile saved ({} bytes); profile.undo restores the previous version",
        text.len()
    ))
}

fn profile_undo(hub: &Hub) -> Result<String, String> {
    let (file, dir) = profile_paths(hub);
    let mut kept: Vec<_> = std::fs::read_dir(&dir)
        .map_or_default(|d| d.filter_map(Result::ok).map(|e| e.path()).collect());
    kept.sort();
    let last = kept.pop().ok_or("no earlier profile version")?;
    let bytes = crate::sys::fs::read_private(&last, PROFILE_MAX as u64 + 1)
        .map_err(|e| e.to_string())?
        .ok_or("the earlier version is gone")?;
    crate::sys::fs::write_atomic(&file, &bytes).map_err(|e| e.to_string())?;
    std::fs::remove_file(&last).map_err(|e| e.to_string())?;
    Ok(format!(
        "profile restored to the version of {}",
        instant_text(
            last.file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
        )
    ))
}
