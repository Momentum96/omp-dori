//! First run and onboarding (OPC-66 to OPC-73): the consent state lives in the hub, not in the
//! lead's memory. Onboarding starts with one consent question; a "no" is stored and
//! `onboarding.start` refuses to ask again unless the owner raises it himself. Tools are asked about
//! one at a time and each answer is kept. While onboarding runs it is read-only: the lead's tool
//! refuses every operation that would post to a messenger or start work. The lead's shell never runs
//! a `cataly` command that writes, onboarding or not (shell rule 0, `lanes::shell`).

use serde_json::{Value, json};

use super::Hub;
use crate::jobs::memory::Memory;
use crate::store::Apply;
use crate::sys::now_ms;

/// The memory key; job names start with a letter or digit, so no job shares it.
const JOB: &str = "@onboarding";
const PHASE: &str = "phase";
const CONSENT: &str = "consent";
/// A tool name: letters, digits, `.`, `-`, `_`, at most 64.
const NAME_MAX: usize = 64;
const TEXT_MAX: usize = 2000;

/// Lane operations that only read; every other lane operation, and posting a question, writes.
const LANE_READS: [&str; 12] = [
    "list",
    "board",
    "read",
    "files",
    "rules",
    "admission",
    "slots",
    "spend",
    "drift",
    "queued",
    "items",
    "plan-status",
];

async fn load(hub: &Hub) -> Result<Memory, String> {
    let rows = hub
        .store
        .read(|r| r.memory(JOB, false))
        .await
        .map_err(|e| e.to_string())?;
    Ok(Memory::new(JOB, false, rows))
}

async fn save(hub: &Hub, memory: Memory) -> Result<(), String> {
    hub.store
        .apply(Apply {
            memory: memory.into_writes(),
            ..Apply::default()
        })
        .await
        .map_err(|e| e.to_string())
}

fn text<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

/// Whether onboarding runs; an unreadable record counts as running (fail closed).
async fn running(hub: &Hub) -> bool {
    load(hub)
        .await
        .map_or(true, |m| m.get(PHASE, "value") == Some("running"))
}

/// Why `op` is refused now, if it is: onboarding runs and `op` would write to a messenger or start
/// work (OPC-68).
pub(crate) async fn blocks(hub: &Hub, op: &str) -> Option<String> {
    let writes = match op.strip_prefix("lane.") {
        Some(lane_op) => !LANE_READS.contains(&lane_op),
        None => matches!(op, "question.ask" | "question.choose" | "job.start"),
    };
    (writes && running(hub).await).then(|| {
        format!(
            "{op} is refused while onboarding runs: onboarding is read-only (no posts, reactions, \
             lanes or jobs). Finish it with onboarding.finish first."
        )
    })
}

/// The first-run line for the recap: what the lead does about onboarding now.
pub(crate) async fn recap_line(hub: &Hub) -> String {
    let Ok(memory) = load(hub).await else {
        return "Onboarding: its record is unreadable; treat onboarding as running (read-only)."
            .into();
    };
    match (memory.get(PHASE, "value"), memory.get(CONSENT, "answer")) {
        (None, _) => "First run: greet the owner through the bot before anything else, then call \
                      onboarding.start and ask its one consent question before reading anything."
            .into(),
        (Some("running"), None) => {
            "Onboarding runs: the consent question is unanswered; read nothing yet.".into()
        }
        (Some("running"), _) => {
            "Onboarding runs (read-only): ask before each tool, finish with onboarding.finish."
                .into()
        }
        (_, Some("no")) => {
            "Onboarding: the owner declined; do not ask again unless the owner raises it.".into()
        }
        _ => "Onboarding: done.".into(),
    }
}

/// Answers the `onboarding` operations and refuses the writes onboarding forbids; `None` lets any
/// other operation through.
pub(crate) async fn call(hub: &Hub, op: &str, input: &Value) -> Option<Result<String, String>> {
    let verb = match op {
        "onboarding" => "",
        other => match other.strip_prefix("onboarding.") {
            Some(verb) => verb,
            None => return blocks(hub, op).await.map(Err),
        },
    };
    Some(run(hub, verb, input).await)
}

async fn run(hub: &Hub, verb: &str, input: &Value) -> Result<String, String> {
    let mut memory = load(hub).await?;
    let phase = memory.get(PHASE, "value").unwrap_or("new").to_string();
    let consent = memory.get(CONSENT, "answer").map(str::to_string);
    let reply = match verb {
        "" => Ok(view(&memory)),
        "start" => start(&mut memory, &phase, consent.as_deref(), input),
        "consent" => {
            if phase != "running" {
                return Err("onboarding is not running; call onboarding.start first".into());
            }
            let answer = text(input, "answer").ok_or("answer is yes or no")?;
            let message =
                text(input, "message").ok_or("message: the owner's message with the answer")?;
            if !matches!(answer, "yes" | "no") {
                return Err("answer is yes or no".into());
            }
            memory.transition(CONSENT, "answer", answer);
            memory.transition(CONSENT, "message", message);
            memory.transition(CONSENT, "at", &now_ms().to_string());
            if answer == "no" {
                memory.transition(PHASE, "value", "done");
                Ok("Stored: the owner said no. Read nothing; do not ask again unless the owner raises it.".into())
            } else {
                Ok("Stored: the owner agreed. Ask about each tool before reading it (onboarding.tool).".into())
            }
        }
        "tool" => tool(&mut memory, &phase, consent.as_deref(), input),
        "finish" => {
            if phase != "running" {
                return Err("onboarding is not running".into());
            }
            let summary = text(input, "text").ok_or(
                "text: the short summary of what is known, tools used or skipped, and gaps",
            )?;
            if summary.chars().count() > TEXT_MAX {
                return Err(format!("text is longer than {TEXT_MAX} characters"));
            }
            memory.transition(PHASE, "value", "done");
            memory.transition(PHASE, "summary", summary);
            Ok("Onboarding is done; post the summary to the owner.".into())
        }
        other => Err(format!("unknown onboarding operation '{other}'")),
    };
    if reply.is_ok() {
        save(hub, memory).await?;
    }
    reply
}

fn start(
    memory: &mut Memory,
    phase: &str,
    consent: Option<&str>,
    input: &Value,
) -> Result<String, String> {
    let reopen = input
        .get("reopen")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let message = text(input, "message");
    if phase == "running" {
        return Ok("Onboarding already runs.".into());
    }
    if (consent == Some("no") || phase == "done") && !(reopen && message.is_some()) {
        return Err(if consent == Some("no") {
            "The owner declined onboarding. Do not ask again unless the owner raises it; then call \
             onboarding.start with reopen true and the owner's message."
                .into()
        } else {
            "Onboarding is done. Start it again only when the owner asks: reopen true and the owner's message.".into()
        });
    }
    if reopen {
        memory.clear(CONSENT, "answer");
        memory.clear(CONSENT, "message");
        memory.transition(PHASE, "reopened_by", message.unwrap_or_default());
    }
    memory.transition(PHASE, "value", "running");
    Ok("Onboarding runs. Before reading anything, ask the owner one consent question that says why; \
        record the answer with onboarding.consent. Until onboarding.finish nothing may be posted, \
        reacted to, edited, marked read, accepted or sent, and no lane or job starts."
        .into())
}

fn tool(
    memory: &mut Memory,
    phase: &str,
    consent: Option<&str>,
    input: &Value,
) -> Result<String, String> {
    if phase != "running" || consent != Some("yes") {
        return Err("ask about tools only while onboarding runs with the owner's consent".into());
    }
    let name = text(input, "name").ok_or("name: the integration")?;
    if name.len() > NAME_MAX
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return Err(format!(
            "name is 1-{NAME_MAX} letters, digits, '.', '-' or '_'"
        ));
    }
    let answer = text(input, "answer").ok_or("answer is allowed or declined")?;
    if !matches!(answer, "allowed" | "declined") {
        return Err("answer is allowed or declined".into());
    }
    let reads = text(input, "reads").ok_or("reads: what it would read and why")?;
    let message = text(input, "message").ok_or("message: the owner's message with the answer")?;
    let subject = format!("tool:{name}");
    memory.transition(&subject, "answer", answer);
    memory.transition(&subject, "reads", reads);
    memory.transition(&subject, "message", message);
    Ok(format!("Stored: {name} {answer}."))
}

fn view(memory: &Memory) -> String {
    let tools: Vec<Value> = memory
        .with_condition("answer")
        .into_iter()
        .filter_map(|(subject, answer)| {
            let name = subject.strip_prefix("tool:")?.to_string();
            let reads = memory
                .get(&subject, "reads")
                .unwrap_or_default()
                .to_string();
            Some(json!({ "name": name, "answer": answer, "reads": reads }))
        })
        .collect();
    json!({
        "phase": memory.get(PHASE, "value").unwrap_or("new"),
        "consent": memory.get(CONSENT, "answer"),
        "tools": tools,
    })
    .to_string()
}
