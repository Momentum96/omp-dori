//! The away digest (OJ-20, OJ req. 51): since a start time, per watched room the counts and messages
//! from others and from the owner, then the GitHub events, session status changes and warnings from
//! the event log. Built from records and bot-readable rooms; it posts nothing itself.

use std::fmt::Write as _;
use std::sync::Arc;

use serde_json::Value;

use super::{Rest, Snowflake};
use crate::config::Config;
use crate::store::{EventRecord, Store};

/// Messages listed per room (OJ req. 51a).
const ROOM_MAX: usize = 500;
const PAGE: u8 = 100;
const TEXT_CUT: usize = 160;
const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;

/// One digest section: its title and the events it takes.
type Section = (&'static str, fn(&EventRecord) -> bool);

fn snowflake_ms(id: &str) -> Option<i64> {
    Snowflake::parse(id).map(|s| (s.seq() >> 22) + DISCORD_EPOCH_MS)
}

fn local(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms).map_or_else(
        |_| ms.to_string(),
        |t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        },
    )
}

/// One room's messages since `since_ms`, oldest first, at most 500.
async fn room(rest: &Rest, channel: Snowflake, since_ms: i64) -> Result<Vec<Value>, String> {
    let mut found = Vec::new();
    let mut before = None;
    loop {
        let page = rest
            .messages(channel, before, PAGE)
            .await
            .map_err(|e| e.to_string())?;
        let full = page.len() == usize::from(PAGE);
        for m in page {
            let id = m
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if snowflake_ms(&id).is_some_and(|at| at < since_ms) {
                found.reverse();
                return Ok(found);
            }
            before = Snowflake::parse(&id);
            found.push(m);
            if found.len() >= ROOM_MAX {
                found.reverse();
                return Ok(found);
            }
        }
        if !full || before.is_none() {
            found.reverse();
            return Ok(found);
        }
    }
}

fn message_line(m: &Value) -> String {
    let id = m.get("id").and_then(Value::as_str).unwrap_or_default();
    let author = m.get("author");
    let name = author
        .and_then(|a| {
            a.get("global_name")
                .and_then(Value::as_str)
                .or_else(|| a.get("username").and_then(Value::as_str))
        })
        .unwrap_or("someone");
    let text: String = m
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .chars()
        .take(TEXT_CUT)
        .collect();
    let files: Vec<&str> = m
        .get("attachments")
        .and_then(Value::as_array)
        .map_or_default(|a| {
            a.iter()
                .filter_map(|f| f.get("filename").and_then(Value::as_str))
                .collect()
        });
    let mut line = format!(
        "  {} {}: {}",
        snowflake_ms(id).map_or_default(local),
        name,
        text.replace('\n', " ")
    );
    if !files.is_empty() {
        let _ = write!(line, " [files: {}]", files.join(", "));
    }
    line
}

/// The digest text and its counts (rooms read, events listed).
pub(crate) async fn build(
    store: Store,
    rest: Option<Arc<Rest>>,
    config: &Config,
    since_ms: i64,
) -> (String, usize, usize) {
    let mut out = format!("[digest] Since {}:\n", local(since_ms));
    let owner = config.discord.owner;
    let mut rooms = 0;
    for channel in &config.discord.channels {
        let Some(rest) = rest.as_deref() else { break };
        match room(rest, *channel, since_ms).await {
            Ok(messages) => {
                rooms += 1;
                let mine = messages
                    .iter()
                    .filter(|m| {
                        m.get("author")
                            .and_then(|a| a.get("id"))
                            .and_then(Value::as_str)
                            .and_then(Snowflake::parse)
                            == owner
                    })
                    .count();
                let _ = writeln!(
                    out,
                    "Room {channel}: {} from others, {mine} from the owner",
                    messages.len() - mine
                );
                for m in &messages {
                    out.push_str(&message_line(m));
                    out.push('\n');
                }
            }
            Err(e) => {
                let _ = writeln!(out, "Room {channel}: could not be read ({e})");
            }
        }
    }
    let events: Vec<EventRecord> = store
        .read(move |r| r.events_since(since_ms, 2000))
        .await
        .unwrap_or_default();
    let mut count = 0;
    let sections: [Section; 3] = [
        ("GitHub", |e| {
            e.kind.starts_with("github.") || e.kind.starts_with("pr.")
        }),
        ("Sessions", |e| {
            e.kind.starts_with("lead.") || e.kind.starts_with("session.")
        }),
        ("Warnings and errors", |e| {
            matches!(e.severity.as_str(), "warning" | "error")
                && !e.kind.starts_with("github.")
                && !e.kind.starts_with("pr.")
                && !e.kind.starts_with("lead.")
                && !e.kind.starts_with("session.")
        }),
    ];
    for (title, pick) in sections {
        let picked: Vec<&EventRecord> = events.iter().filter(|e| pick(e)).collect();
        let _ = writeln!(out, "{title}: {}", picked.len());
        for e in &picked {
            let _ = writeln!(out, "  {} {}", local(e.at_ms), e.text);
        }
        count += picked.len();
    }
    (out, rooms, count)
}

impl<T: crate::host::Runtime> super::intake::Intake<T> {
    /// Keeps the unanswered entries of a conversation room (OJ req. 54): a message from anyone but
    /// the owner and the bot opens one; a message from either closes every earlier one.
    pub(super) async fn track_others(
        &self,
        raw: &Value,
        room: Snowflake,
        message: Snowflake,
        passed: bool,
    ) {
        use crate::store::lead::{LeadWrite, OtherRow};
        let author = raw.get("author");
        let id = author
            .and_then(|a| a.get("id"))
            .and_then(Value::as_str)
            .and_then(Snowflake::parse);
        let at_ms = snowflake_ms(&message.to_string()).unwrap_or_else(crate::sys::now_ms);
        let write = if id == Some(self.owner) || id == Some(self.bot) {
            LeadWrite::OthersAnswered {
                relay: super::intake::NAME.into(),
                room: room.to_string(),
                at_ms,
            }
        } else if passed && author.and_then(|a| a.get("bot")).and_then(Value::as_bool) != Some(true)
        {
            let name = author
                .and_then(|a| {
                    a.get("global_name")
                        .and_then(Value::as_str)
                        .or_else(|| a.get("username").and_then(Value::as_str))
                })
                .unwrap_or("someone");
            LeadWrite::Other {
                relay: super::intake::NAME.into(),
                row: OtherRow {
                    room: room.to_string(),
                    message: message.to_string(),
                    author: name.to_string(),
                    text: raw
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .chars()
                        .take(TEXT_CUT)
                        .collect(),
                    at_ms,
                },
            }
        } else {
            return;
        };
        if let Err(e) = self.store.lead(write).await {
            crate::log::warn("unanswered entry not recorded", &[("error", &e)]);
        }
    }

    /// An owner reply to a message bound to an open ledger question records the answer before the
    /// hand-off, and the hand-off says which question it answered (EVR req. 37).
    pub(super) async fn ledger_answer(
        &self,
        row: &crate::store::relay::QueueRow,
        mut inbound: crate::relay::Inbound,
    ) -> crate::relay::Inbound {
        use crate::store::lead::{LeadDone, LeadWrite};
        let Some(target) = inbound.reply_to.clone() else {
            return inbound;
        };
        let room = row.room.clone();
        let Ok(Some(q)) = self
            .store
            .read(move |r| r.question_at(&room, &target))
            .await
        else {
            return inbound;
        };
        let text = inbound.text.trim();
        let outcome = match text.parse::<usize>() {
            Ok(n) if (1..=q.options.len()).contains(&n) => Some(format!(
                "option {n} ({})",
                q.options.get(n - 1).map_or("", String::as_str)
            )),
            Ok(n) => {
                inbound.data.push(format!(
                    "this reply names option {n} of question {}, which has {} options; nothing was recorded",
                    q.id,
                    q.options.len()
                ));
                None
            }
            Err(_) => Some(format!(
                "free answer: {}",
                text.chars().take(1000).collect::<String>()
            )),
        };
        if let Some(outcome) = outcome {
            let write = LeadWrite::QuestionSettle {
                id: q.id.clone(),
                state: "answered".into(),
                outcome: format!("owner message {}: {outcome}", row.message),
            };
            if matches!(self.store.lead(write).await, Ok(LeadDone::Done)) {
                inbound.data.push(format!(
                    "this message answers question {} ({}): {outcome}",
                    q.id, q.text
                ));
            }
        }
        inbound
    }

    /// The owner is back after a long silence: the digest goes to the lead as one urgent event
    /// (OJ req. 50-52, acceptance check 10).
    pub(super) fn digest(self: &Arc<Self>, since_ms: i64) {
        use crate::jobs::event::{Emit, EventKind, Severity};
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let config = Config {
                discord: me.settings().clone(),
                ..Config::default()
            };
            let (text, rooms, events) = build(
                me.store.clone(),
                Some(Arc::clone(&me.rest)),
                &config,
                since_ms,
            )
            .await;
            let mut emit = Emit::new(EventKind::Digest, Severity::Info, true, "owner")
                .with("since", crate::jobs::instant_text(since_ms))
                .with("rooms", rooms)
                .with("events", events);
            emit.body = Some(text);
            (me.events)(emit);
        });
    }
}
