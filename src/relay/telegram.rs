//! Telegram (TELE-01 to TELE-10, TELE-15): a bot that takes the owner's messages from his DM or one
//! group and forum topic by long polling, marks them seen, and posts answers as replies in place.
//!
//! - Start (TELE req. 16-17): the token must name a bot with a positive ID and no webhook may be set
//!   (it is never changed); the poll state must be writable; state is bound to (bot, owner). Telegram
//!   intake refuses to start on Windows (owner decision 4).
//! - Poll (TELE req. 18-23): at most 100 new or edited messages, a 25 s wait, the next poll at once.
//!   The saved offset is sent only while the last non-empty batch is under 24 hours old by the local
//!   clock; a replay guard of the handed-off message identities in the latest batch (at most 100)
//!   skips repeats. Offset, batch time and guard are saved in one store commit after each update; a
//!   failed save stops polling until it succeeds (MSG req. 5). 409: unhealthy, again every 30 s.
//!   401: stopped until the owner acts. 429: the requested delay is waited out however long.
//! - Gate and normalise (TELE req. 24-28): the owner, not a bot, in his DM or the group and topic;
//!   live-location edits are dropped. An edit is handed off as its own request; the room is the chat,
//!   or `<chat>:<topic>` in a forum (General is topic 1); the queue order is the update ID.
//! - Send (TELE req. 30-33): plain text split at 4096 UTF-16 units with the parts joined equal to the
//!   input, only to the owner's DM, the configured group and topic or a listed chat; edits are never
//!   split; HTML goes as one request. An uncertain part is never sent again.

use std::time::{Duration, Instant};

use hyper::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::watch;

use super::http::{self, Base, Kind, Outcome, Policy, Request};
use super::intake::{self, Env, Round, Shared, Stop};
use super::pipeline::Pipeline;
use super::text::{self, Checks};
use super::{Facts, Inbound, PartOutcome, Relay};
use crate::config::Config;
use crate::config::relays::TelegramSettings;
use crate::host::Runtime;
use crate::log;
use crate::store::relay::{Arrived, QueueRow, RelayWrite};
use crate::sys::now_ms;

/// The relay's store key.
pub(crate) const NAME: &str = "telegram";
/// The real Bot API; tests use a loopback base instead.
pub(crate) const API: &str = "https://api.telegram.org";
/// Telegram's text limit in UTF-16 units (TELE req. 30).
pub(crate) const LIMIT: usize = 4096;
const POLL_WAIT: Duration = Duration::from_secs(25);
const CONFLICT_RETRY: Duration = Duration::from_secs(30);
const SAVE_RETRY: Duration = Duration::from_secs(60);
const BACKOFF_FIRST: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const OFFSET_VALID_MS: i64 = 24 * 3600 * 1000;
const GUARD_MAX: usize = 100;
const BATCH_MAX: usize = 100;
/// Largest safe JSON integer.
const SAFE_MAX: i64 = (1 << 53) - 1;
const PROFILE_TIMEOUT: Duration = Duration::from_secs(5);
const PRODUCT: &str = "Cataly";

/// A Bot API failure. Never carries the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApiError {
    pub(crate) status: Option<u16>,
    pub(crate) why: String,
    pub(crate) retry_after: Option<Duration>,
    /// The call may have taken effect.
    pub(crate) uncertain: bool,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(s) => write!(f, "HTTP {s}: {}", self.why),
            None => f.write_str(&self.why),
        }
    }
}

/// A Bot API client for one token.
pub(crate) struct Api {
    base: Base,
    token: String,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

/// A bot token: the bot ID, a colon, then letters, digits, `_` or `-`.
fn valid_token(token: &str) -> bool {
    token.split_once(':').is_some_and(|(id, secret)| {
        !id.is_empty()
            && id.bytes().all(|b| b.is_ascii_digit())
            && secret.len() >= 20
            && secret
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

impl Api {
    pub(crate) fn new(base: Base, token: &str) -> Result<Self, String> {
        if !valid_token(token) {
            return Err("the Telegram token file does not hold a bot token".into());
        }
        Ok(Self {
            base,
            token: token.to_string(),
        })
    }

    /// The client for the current config's token file.
    pub(crate) fn from_config(
        config: &Config,
        config_file: &std::path::Path,
    ) -> Result<Self, String> {
        let path = intake::secret_path(
            config.telegram.token_file.as_ref(),
            config_file,
            "telegram-token",
        );
        let token = intake::read_secret(&path)?;
        let base = match &config.telegram.api_base {
            Some(b) => b.clone(),
            None => Base::parse(API)?,
        };
        Self::new(base, &token)
    }

    async fn call(
        &self,
        method: &str,
        body: &Value,
        kind: Kind,
        policy: Policy,
    ) -> Result<Value, ApiError> {
        let path = format!("/bot{}/{method}", self.token);
        let req = Request {
            method: Method::POST,
            path: &path,
            headers: Vec::new(),
            json: Some(body.to_string().into_bytes()),
            kind,
        };
        let error = |status, why: String, uncertain| ApiError {
            status,
            why,
            retry_after: None,
            uncertain,
        };
        match http::call(&self.base, &req, policy, |_| {}).await {
            Outcome::Done(resp) => {
                let v: Option<Value> = serde_json::from_slice(&resp.body).ok();
                match v {
                    Some(v) if v.get("ok") == Some(&Value::Bool(true)) => {
                        Ok(v.get("result").cloned().unwrap_or(Value::Null))
                    }
                    // A 2xx whose body cannot be read: a write may have happened (MSG req. 12).
                    _ => Err(error(
                        Some(resp.status),
                        "unreadable answer".into(),
                        kind != Kind::Read,
                    )),
                }
            }
            Outcome::Rejected(resp) => {
                let v: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
                let why = v
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("refused")
                    .chars()
                    .take(300)
                    .collect();
                Err(ApiError {
                    retry_after: resp.retry_after(),
                    ..error(Some(resp.status), why, false)
                })
            }
            Outcome::RateLimited(d) => Err(ApiError {
                retry_after: Some(d),
                ..error(Some(429), "rate limited".into(), false)
            }),
            Outcome::Uncertain(why) => Err(error(None, why, kind != Kind::Read)),
            Outcome::Failed(why) => Err(error(None, why, false)),
        }
    }

    async fn read(&self, method: &str, body: &Value) -> Result<Value, ApiError> {
        self.call(method, body, Kind::Read, Policy::CHAT).await
    }

    async fn write(&self, method: &str, body: &Value) -> Result<Value, ApiError> {
        self.call(method, body, Kind::Once, Policy::CHAT).await
    }

    /// Sends one part; the created message ID, which must be positive (TELE req. 30).
    async fn send_part(
        &self,
        chat: i64,
        topic: Option<i64>,
        reply_to: Option<i64>,
        text: &str,
        html: bool,
    ) -> Result<i64, ApiError> {
        let mut body = json!({"chat_id": chat, "text": text});
        if let Some(map) = body.as_object_mut() {
            // General (topic 1) takes no thread ID.
            if let Some(t) = topic.filter(|t| *t > 1) {
                map.insert("message_thread_id".into(), json!(t));
            }
            if let Some(m) = reply_to {
                map.insert(
                    "reply_parameters".into(),
                    json!({"message_id": m, "allow_sending_without_reply": false}),
                );
            }
            if html {
                map.insert("parse_mode".into(), json!("HTML"));
            }
        }
        let v = self.write("sendMessage", &body).await?;
        v.get("message_id")
            .and_then(Value::as_i64)
            .filter(|id| *id > 0)
            .ok_or_else(|| ApiError {
                status: None,
                why: "the answer named no message ID".into(),
                retry_after: None,
                uncertain: true,
            })
    }
}

/// Where sends may go (TELE req. 31).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Scope {
    pub(crate) owner: i64,
    pub(crate) group: Option<i64>,
    pub(crate) topic: Option<i64>,
    pub(crate) chats: Vec<i64>,
}

impl Scope {
    pub(crate) fn of(settings: &TelegramSettings) -> Option<Self> {
        Some(Self {
            owner: settings.owner?,
            group: settings.group,
            topic: settings.topic,
            chats: settings.chats.clone(),
        })
    }

    /// Refuses a destination outside the owner's DM, the configured group and topic and the listed
    /// chats, before any request.
    pub(crate) fn check(&self, chat: i64, topic: Option<i64>) -> Result<(), String> {
        if topic.is_some_and(|t| t < 1) {
            return Err("a topic must be a positive number".into());
        }
        let in_group = Some(chat) == self.group
            && (self.topic.is_none() || topic.unwrap_or(1) == self.topic.unwrap_or(1));
        if (chat == self.owner && topic.is_none()) || in_group || self.chats.contains(&chat) {
            Ok(())
        } else {
            Err(format!(
                "chat {chat} is not the owner's DM, the configured group and topic, or a listed chat"
            ))
        }
    }
}

/// A sent text, part by part (MSG-05).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Sent {
    Confirmed(Vec<i64>),
    /// Nothing more was created; the IDs already confirmed.
    Rejected(Vec<i64>, String),
    Uncertain(Vec<i64>, String),
}

/// Sends a text to a chat in scope: split plain text, or one HTML request (TELE req. 30-33).
pub(crate) async fn send(
    api: &Api,
    scope: &Scope,
    chat: i64,
    topic: Option<i64>,
    reply_to: Option<i64>,
    text: &str,
    html: bool,
) -> Sent {
    if let Err(e) = scope.check(chat, topic) {
        return Sent::Rejected(Vec::new(), e);
    }
    let parts = if html {
        vec![text.to_string()]
    } else {
        text::split_plain(text, LIMIT)
    };
    let mut done = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let reply = if i == 0 { reply_to } else { None };
        match api.send_part(chat, topic, reply, part, html).await {
            Ok(id) => done.push(id),
            Err(e) if e.uncertain => return Sent::Uncertain(done, e.to_string()),
            Err(e) => return Sent::Rejected(done, e.to_string()),
        }
    }
    Sent::Confirmed(done)
}

/// Edits one message in place; over 4096 units is refused before any request (TELE req. 32-33).
pub(crate) async fn edit(
    api: &Api,
    scope: &Scope,
    chat: i64,
    message: i64,
    text: &str,
    html: bool,
) -> Result<(), ApiError> {
    let refuse = |why: String| ApiError {
        status: None,
        why,
        retry_after: None,
        uncertain: false,
    };
    scope.check(chat, None).or_else(|e| {
        // A group topic message is edited by chat and ID; the topic does not matter.
        if Some(chat) == scope.group {
            Ok(())
        } else {
            Err(refuse(e))
        }
    })?;
    if text::utf16_len(text) > LIMIT {
        return Err(refuse("an edit over 4096 units is never split".into()));
    }
    let mut body = json!({"chat_id": chat, "message_id": message, "text": text});
    if let (true, Some(map)) = (html, body.as_object_mut()) {
        map.insert("parse_mode".into(), json!("HTML"));
    }
    api.write("editMessageText", &body).await.map(drop)
}

/// A room: the chat, or `<chat>:<topic>` in a forum.
pub(crate) fn room_of(room: &str) -> Option<(i64, Option<i64>)> {
    match room.split_once(':') {
        Some((chat, topic)) => Some((chat.parse().ok()?, Some(topic.parse().ok()?))),
        None => Some((room.parse().ok()?, None)),
    }
}

/// The Telegram side of the relay trait.
pub(crate) struct TelegramRelay {
    pub(crate) api: Arc<Api>,
    pub(crate) scope: Scope,
    pub(crate) checks: Checks,
}

/// The message ID of a queue key: `<id>` for a new message, `<id>.<edit date>` for an edit.
fn message_id(key: &str) -> Option<i64> {
    key.split('.').next()?.parse().ok()
}

/// A board destination after `telegram:`: `<chat>` or `<chat>/<topic>`.
pub(crate) fn board_target(target: &str) -> Option<(i64, Option<i64>)> {
    let (chat, topic) = match target.split_once('/') {
        Some((c, t)) => (c, Some(t.parse::<i64>().ok().filter(|t| *t > 0)?)),
        None => (target, None),
    };
    Some((chat.parse::<i64>().ok().filter(|c| *c != 0)?, topic))
}

impl Relay for TelegramRelay {
    const NAME: &'static str = NAME;
    const REPEATABLE: bool = false;
    const LIVE_ONCE: bool = false;

    fn parts(&self, answer: &str) -> Result<Vec<String>, String> {
        let answer = answer.trim();
        if answer.is_empty() {
            return Err("empty answer".into());
        }
        text::check(answer, &self.checks)
            .map_err(|e| format!("refused by the outgoing text checks: {e}"))?;
        Ok(text::split_plain(answer, LIMIT))
    }

    fn nonce(&self, room: &str, source: &str, index: usize) -> String {
        format!("{room}/{source}/{index}")
    }

    fn normalise(&self, row: &QueueRow) -> Result<Inbound, String> {
        let raw: Value = serde_json::from_str(&row.raw).map_err(|e| e.to_string())?;
        normalise(&raw, row)
    }

    async fn post(
        &self,
        room: &str,
        reply_to: Option<&str>,
        text: &str,
        _nonce: &str,
    ) -> PartOutcome {
        let Some((chat, topic)) = room_of(room) else {
            return PartOutcome::Rejected("bad room".into());
        };
        if let Err(e) = self.scope.check(chat, topic) {
            return PartOutcome::Rejected(e);
        }
        let reply = reply_to.and_then(message_id);
        match self.api.send_part(chat, topic, reply, text, false).await {
            Ok(id) => PartOutcome::Posted {
                id: id.to_string(),
                linked: reply.is_some(),
            },
            Err(e) if e.why.contains("message to be replied not found") => PartOutcome::SourceGone,
            Err(e) if e.uncertain => PartOutcome::Uncertain(e.to_string()),
            Err(e) => PartOutcome::Rejected(e.to_string()),
        }
    }

    async fn seen(&self, room: &str, message: &str) -> Result<(), String> {
        // Edits get no seen mark (MSG req. 6).
        if message.contains('.') {
            return Ok(());
        }
        let (chat, _) = room_of(room).ok_or("bad room")?;
        let id = message_id(message).ok_or("bad message")?;
        let body = json!({"chat_id": chat, "message_id": id,
            "reaction": [{"type": "emoji", "emoji": "\u{1F440}"}]});
        // Setting the same reaction again changes nothing.
        self.api
            .call("setMessageReaction", &body, Kind::Idempotent, Policy::CHAT)
            .await
            .map(drop)
            .map_err(|e| e.to_string())
    }

    fn typing(&self, _room: &str) -> impl Future<Output = Result<(), String>> + Send {
        std::future::ready(Err("Telegram shows no typing for answers".into()))
    }

    async fn facts(&self, room: &str) -> Result<Facts, String> {
        let (chat, topic) = room_of(room).ok_or("bad room")?;
        if let Some(t) = topic {
            return Ok(Facts {
                name: Some(if t == 1 {
                    "General".into()
                } else {
                    format!("topic {t}")
                }),
                topic: None,
                dm: false,
            });
        }
        let v = self
            .api
            .read("getChat", &json!({"chat_id": chat}))
            .await
            .map_err(|e| e.to_string())?;
        Ok(Facts {
            name: v.get("title").and_then(Value::as_str).map(str::to_string),
            topic: v
                .get("description")
                .and_then(Value::as_str)
                .filter(|d| !d.trim().is_empty())
                .map(str::to_string),
            dm: v.get("type").and_then(Value::as_str) == Some("private"),
        })
    }
}

/// The first attachment (TELE req. 27): its kind, file handle and size; nothing is downloaded.
fn attachment(m: &Value) -> Option<String> {
    for kind in [
        "voice",
        "audio",
        "video",
        "video_note",
        "animation",
        "document",
    ] {
        if let Some(f) = m.get(kind) {
            return Some(file_line(kind, f));
        }
    }
    let photo = m.get("photo")?.as_array()?;
    let largest = photo.iter().max_by_key(|p| {
        p.get("width").and_then(Value::as_i64).unwrap_or(0)
            * p.get("height").and_then(Value::as_i64).unwrap_or(0)
    })?;
    Some(file_line("photo", largest))
}

fn file_line(kind: &str, f: &Value) -> String {
    let id = f.get("file_id").and_then(Value::as_str).unwrap_or("?");
    match f.get("file_size").and_then(Value::as_i64) {
        Some(size) => format!("{kind} {id} ({size} bytes)"),
        None => format!("{kind} {id}"),
    }
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// The topic of a message (TELE req. 25): its thread ID when Telegram marks it a topic message;
/// otherwise General (1) in a forum, none elsewhere.
fn topic_of(m: &Value) -> Option<i64> {
    let forum = m
        .get("chat")
        .and_then(|c| c.get("is_forum"))
        .and_then(Value::as_bool)
        == Some(true);
    if m.get("is_topic_message").and_then(Value::as_bool) == Some(true) {
        return m.get("message_thread_id").and_then(Value::as_i64);
    }
    forum.then_some(1)
}

/// The queued owner message as the shared event (TELE req. 26-27; MSG-01).
fn normalise(raw: &Value, row: &QueueRow) -> Result<Inbound, String> {
    let m = raw.get("message").ok_or("no message")?;
    let edit = raw.get("kind").and_then(Value::as_str) == Some("edit");
    let id = m
        .get("message_id")
        .and_then(Value::as_i64)
        .ok_or("the message has no ID")?;
    let topic = topic_of(m);
    let mut data = Vec::new();
    // A reply to the topic's own root is no reply (TELE req. 26).
    let reply_to = m
        .get("reply_to_message")
        .filter(|r| r.get("message_id").and_then(Value::as_i64) != topic.filter(|t| *t > 1))
        .and_then(|r| {
            let rid = r.get("message_id").and_then(Value::as_i64)?;
            let quoted = str_of(r, "text")
                .or_else(|| str_of(r, "caption"))
                .unwrap_or("");
            let cut: String = quoted.chars().take(300).collect();
            data.push(format!("replying to message {rid}: {cut}"));
            Some(rid.to_string())
        });
    let own = str_of(m, "text")
        .or_else(|| str_of(m, "caption"))
        .unwrap_or("")
        .to_string();
    let forwarded = m.get("forward_origin").is_some();
    let text = if forwarded {
        data.push(format!("a forwarded message: {own}"));
        String::new()
    } else {
        own
    };
    if edit {
        data.push(format!("this edits the owner's earlier message {id}"));
    }
    let attachments: Vec<String> = attachment(m).into_iter().collect();
    let plain = reply_to.is_none() && !forwarded && attachments.is_empty();
    let stable = format!("{}|{}|{:?}", row.message, text, attachments);
    Ok(Inbound {
        room: row.room.clone(),
        parent: row.parent.clone(),
        message: row.message.clone(),
        seq: row.seq,
        reply_to,
        text,
        data,
        attachments,
        plain,
        stable,
    })
}

/// What the gate does with one update's message (TELE req. 24, 28).
#[derive(Debug, PartialEq, Eq)]
enum Gate {
    /// The owner's, in scope: room, parent.
    Owner(String, Option<String>),
    Drop,
}

fn gate(m: &Value, edit: bool, settings: &TelegramSettings) -> Gate {
    let Some(owner) = settings.owner else {
        return Gate::Drop;
    };
    let from = m.get("from");
    let is_bot = from.and_then(|f| f.get("is_bot")).and_then(Value::as_bool) != Some(false);
    let sender = from.and_then(|f| f.get("id")).and_then(Value::as_i64);
    if is_bot || sender != Some(owner) {
        return Gate::Drop;
    }
    // A live location's refresh is an edit with a location and no words.
    if edit
        && m.get("location").is_some()
        && str_of(m, "text").is_none()
        && str_of(m, "caption").is_none()
    {
        return Gate::Drop;
    }
    let chat = m.get("chat");
    let chat_id = chat.and_then(|c| c.get("id")).and_then(Value::as_i64);
    let kind = chat.and_then(|c| c.get("type")).and_then(Value::as_str);
    match (kind, chat_id) {
        (Some("private"), Some(id)) if id == owner => Gate::Owner(id.to_string(), None),
        (Some("group" | "supergroup"), Some(id)) if Some(id) == settings.group => {
            let topic = topic_of(m);
            if settings.topic.is_some() && topic.unwrap_or(1) != settings.topic.unwrap_or(1) {
                return Gate::Drop;
            }
            match topic {
                Some(t) => Gate::Owner(format!("{id}:{t}"), Some(id.to_string())),
                None => Gate::Owner(id.to_string(), None),
            }
        }
        _ => Gate::Drop,
    }
}

/// The saved poll state (TELE req. 21-23), bound to the bot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct PollState {
    bot: i64,
    offset: Option<i64>,
    /// Local time of the last non-empty batch.
    batch_ms: Option<i64>,
    /// (chat, message, edit date) of handed-off messages in the latest batch.
    guard: Vec<(i64, i64, i64)>,
}

impl PollState {
    /// The offset to send: only while the last non-empty batch is under 24 hours old by the local
    /// clock, and never after the clock went backwards (TELE req. 21).
    fn offset(&self, now: i64) -> Option<i64> {
        let at = self.batch_ms?;
        (now >= at && now - at < OFFSET_VALID_MS).then_some(self.offset?)
    }
}

/// The supervised Telegram intake task.
pub(crate) async fn run<T: Runtime>(
    shared: Arc<Shared>,
    env: Arc<Env<T>>,
    shutdown: watch::Receiver<bool>,
) -> Result<(), String> {
    let off = |c: &Config| {
        if !c.telegram.intake() {
            Some("set [telegram] owner to start intake".into())
        } else if cfg!(windows) {
            Some("Telegram intake is not available on Windows".into())
        } else {
            None
        }
    };
    let (task, stop) = (Arc::clone(&shared), shutdown.clone());
    let events = Arc::clone(&env.events);
    intake::supervise(
        NAME,
        shared,
        env.config.clone(),
        shutdown,
        events,
        off,
        move |config| {
            let (shared, env, stop) = (Arc::clone(&task), Arc::clone(&env), stop.clone());
            async move { start(shared, env, config, stop).await }
        },
    )
    .await
}

async fn start<T: Runtime>(
    shared: Arc<Shared>,
    env: Arc<Env<T>>,
    config: Arc<Config>,
    shutdown: watch::Receiver<bool>,
) -> Stop {
    let settings = config.telegram.clone();
    let Some(scope) = Scope::of(&settings) else {
        return Stop::Fatal("no owner configured".into());
    };
    shared.set(|h| h.owner = Some(scope.owner.to_string()));
    let api = match Api::from_config(&config, &env.config_file) {
        Ok(api) => Arc::new(api),
        Err(e) => return Stop::Fatal(e),
    };
    let bot = match identity(&api).await {
        Ok(bot) => bot,
        Err(stop) => return stop,
    };
    if let Err(stop) =
        intake::bind(&env.store, NAME, &bot.to_string(), &scope.owner.to_string()).await
    {
        return stop;
    }
    let state = match load_state(&env, bot).await {
        Ok(s) => s,
        Err(stop) => return stop,
    };
    if settings.profile {
        profile(&api).await;
    }
    shared.set(|h| {
        h.bot = Some(bot.to_string());
        h.reason = None;
        h.state = "connecting".into();
    });
    let relay = Arc::new(TelegramRelay {
        api: Arc::clone(&api),
        scope,
        checks: config.outgoing.clone(),
    });
    let pipeline = Arc::new(Pipeline::new(
        relay,
        env.parts(),
        shared.wake_latency.clone(),
    ));
    let round = Arc::new(Round::new(
        pipeline,
        env.store.clone(),
        Arc::clone(&env.lead),
    ));
    let (tx, rx) = tokio::sync::oneshot::channel();
    {
        let poller = Poller {
            api,
            settings: settings.clone(),
            shared: Arc::clone(&shared),
            round: Arc::clone(&round),
            store: env.store.clone(),
            state,
        };
        let stop = shutdown.clone();
        tokio::spawn(async move {
            let end = poller.run(stop).await;
            let _ = tx.send(end);
        });
    }
    round
        .serve(
            rx,
            env.config.clone(),
            |c| c.telegram.clone(),
            &shared.kick,
            shutdown,
        )
        .await
}

/// The start-up checks (TELE req. 16): a bot with a positive ID and no webhook.
async fn identity(api: &Api) -> Result<i64, Stop> {
    let stop = |what: &str, e: &ApiError| match e.status {
        Some(401 | 404) => Stop::Fatal(format!("{what}: the bot token was refused")),
        Some(s) if (400..500).contains(&s) && s != 429 => Stop::Fatal(format!("{what}: {e}")),
        _ => Stop::Retry(format!("{what}: {e}")),
    };
    let me = api
        .read("getMe", &json!({}))
        .await
        .map_err(|e| stop("reading the bot", &e))?;
    let id = me.get("id").and_then(Value::as_i64).filter(|id| *id > 0);
    let Some(id) = id.filter(|_| me.get("is_bot").and_then(Value::as_bool) == Some(true)) else {
        return Err(Stop::Fatal("the token does not belong to a bot".into()));
    };
    let hook = api
        .read("getWebhookInfo", &json!({}))
        .await
        .map_err(|e| stop("reading the webhook", &e))?;
    if hook
        .get("url")
        .and_then(Value::as_str)
        .is_some_and(|u| !u.is_empty())
    {
        return Err(Stop::Fatal(
            "a webhook is set for this bot; another program takes its updates (Cataly never changes it)"
                .into(),
        ));
    }
    Ok(id)
}

/// The saved poll state; an unreadable one is left out once, another bot's refused (TELE req. 17).
async fn load_state<T: Runtime>(env: &Env<T>, bot: i64) -> Result<PollState, Stop> {
    let doc = env
        .store
        .read(|r| r.relay_state(NAME))
        .await
        .map_err(|e| Stop::Retry(e.to_string()))?;
    let state = match doc.map(|d| serde_json::from_str::<PollState>(&d)) {
        Some(Ok(s)) if s.bot == bot => s,
        Some(Ok(s)) => {
            return Err(Stop::Fatal(format!(
                "the saved position belongs to bot {}; `cataly telegram reset` starts afresh",
                s.bot
            )));
        }
        Some(Err(e)) => {
            log::warn(
                "Telegram position unreadable; starting without it",
                &[("error", &e)],
            );
            PollState {
                bot,
                ..PollState::default()
            }
        }
        None => PollState {
            bot,
            ..PollState::default()
        },
    };
    // The store must take a write before polling starts.
    save(&env.store, &state).await.map_err(Stop::Retry)?;
    Ok(state)
}

async fn save(store: &crate::store::Store, state: &PollState) -> Result<(), String> {
    let doc = serde_json::to_string(state).map_err(|e| e.to_string())?;
    store
        .relay(RelayWrite::State {
            relay: NAME.into(),
            doc,
        })
        .await
        .map(drop)
        .map_err(|e| e.to_string())
}

/// Sets an empty bot name to the product name (MSG req. 14); failures are logged once.
async fn profile(api: &Api) {
    let result = tokio::time::timeout(PROFILE_TIMEOUT, async {
        let v = api.read("getMyName", &json!({})).await?;
        if v.get("name")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            api.write("setMyName", &json!({"name": PRODUCT})).await?;
        }
        Ok::<(), ApiError>(())
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log::info("Telegram bot name not set", &[("error", &e)]),
        Err(_) => log::info("Telegram bot name not set", &[("error", &"timed out")]),
    }
}

/// The long-poll loop.
struct Poller<T: Runtime> {
    api: Arc<Api>,
    settings: TelegramSettings,
    shared: Arc<Shared>,
    round: Arc<Round<TelegramRelay, T>>,
    store: crate::store::Store,
    state: PollState,
}

impl<T: Runtime> Poller<T> {
    async fn run(mut self, mut shutdown: watch::Receiver<bool>) -> Stop {
        let mut backoff = BACKOFF_FIRST;
        let mut conflict_logged = false;
        loop {
            let mut body = json!({"timeout": POLL_WAIT.as_secs(), "limit": BATCH_MAX,
                "allowed_updates": ["message", "edited_message"]});
            if let (Some(offset), Some(map)) = (self.state.offset(now_ms()), body.as_object_mut()) {
                map.insert("offset".into(), json!(offset));
            }
            let polled = tokio::select! {
                r = self.api.call("getUpdates", &body, Kind::Read, Policy::poll(POLL_WAIT)) => r,
                _ = shutdown.changed() => return Stop::Shutdown,
            };
            let read_at = Instant::now();
            let wait = match polled {
                Ok(v) => match self.batch(&v, read_at).await {
                    Ok(()) => {
                        backoff = BACKOFF_FIRST;
                        conflict_logged = false;
                        None
                    }
                    Err(Some(wait)) => Some(wait),
                    Err(None) => {
                        let wait = backoff;
                        backoff = (backoff * 2).min(BACKOFF_MAX);
                        Some(wait)
                    }
                },
                Err(e) if e.status == Some(401) => {
                    return Stop::Fatal("the bot token was refused (401)".into());
                }
                Err(e) if e.status == Some(409) => {
                    if !conflict_logged {
                        log::warn(
                            "another consumer is polling this Telegram bot",
                            &[("error", &e)],
                        );
                        conflict_logged = true;
                    }
                    self.unhealthy("another consumer is polling this bot (409)");
                    Some(CONFLICT_RETRY)
                }
                Err(e) if e.status == Some(429) => {
                    let wait = e.retry_after.unwrap_or(backoff);
                    self.unhealthy(&format!("rate limited for {} s", wait.as_secs()));
                    Some(wait)
                }
                Err(e) => {
                    self.shared.set(|h| h.last_error = Some(e.to_string()));
                    let wait = backoff;
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                    Some(wait)
                }
            };
            if let Some(wait) = wait {
                tokio::select! {
                    () = tokio::time::sleep(wait) => {}
                    _ = shutdown.changed() => return Stop::Shutdown,
                }
            }
        }
    }

    fn unhealthy(&self, why: &str) {
        self.shared.set(|h| {
            h.state = "unhealthy".into();
            h.reason = Some(why.to_string());
        });
    }

    /// One poll response. `Err(None)`: a protocol error, retried with backoff; `Err(Some(wait))`: a
    /// failed save, retried after the wait.
    async fn batch(&mut self, v: &Value, read_at: Instant) -> Result<(), Option<Duration>> {
        let Some(updates) = v.as_array().filter(|a| a.len() <= BATCH_MAX) else {
            self.protocol("the update list is not an array of at most 100");
            return Err(None);
        };
        let ids: Option<Vec<i64>> = updates
            .iter()
            .map(|u| {
                u.get("update_id")
                    .and_then(Value::as_i64)
                    .filter(|id| (0..=SAFE_MAX).contains(id))
            })
            .collect();
        let Some(ids) = ids else {
            self.protocol("an update ID is not a safe non-negative integer");
            return Err(None);
        };
        self.shared.set(|h| {
            if h.state != "connected" {
                h.state = "connected".into();
                h.connected_ms = Some(now_ms());
            }
            h.caught_up = true;
            h.reason = None;
            h.last_event_ms = Some(now_ms());
        });
        if updates.is_empty() {
            return Ok(());
        }
        let old_guard = std::mem::take(&mut self.state.guard);
        let mut guard = Vec::new();
        for (u, id) in updates.iter().zip(ids) {
            if let Some(identity) = self.update(u, id, read_at, &old_guard).await {
                guard.push(identity);
                if guard.len() > GUARD_MAX {
                    guard.remove(0);
                }
            }
            self.state.offset = Some(id + 1);
            self.state.batch_ms = Some(now_ms());
            self.state.guard.clone_from(&guard);
            // Never poll past an unsaved position (MSG req. 5).
            while let Err(e) = save(&self.store, &self.state).await {
                log::warn("Telegram position not saved", &[("error", &e)]);
                self.unhealthy("the position could not be saved");
                tokio::time::sleep(SAVE_RETRY).await;
            }
        }
        Ok(())
    }

    fn protocol(&self, why: &str) {
        log::warn("Telegram protocol error", &[("error", &why)]);
        self.shared.set(|h| h.last_error = Some(why.to_string()));
    }

    /// One update: gate, guard, queue. The identity when it was handed off (or already was).
    async fn update(
        &self,
        u: &Value,
        update_id: i64,
        read_at: Instant,
        guard: &[(i64, i64, i64)],
    ) -> Option<(i64, i64, i64)> {
        let (m, edit) = match (u.get("message"), u.get("edited_message")) {
            (Some(m), _) => (m, false),
            (None, Some(m)) => (m, true),
            _ => return None,
        };
        let chat = m.get("chat")?.get("id")?.as_i64()?;
        let mid = m.get("message_id")?.as_i64()?;
        let edit_date = if edit {
            m.get("edit_date").and_then(Value::as_i64).unwrap_or(0)
        } else {
            0
        };
        let identity = (chat, mid, edit_date);
        if guard.contains(&identity) {
            return Some(identity);
        }
        let Gate::Owner(room, parent) = gate(m, edit, &self.settings) else {
            return None;
        };
        if let Some(date) = m.get("date").and_then(Value::as_i64) {
            let lag = (now_ms() - date * 1000).max(0);
            self.shared
                .receive_lag
                .record(Duration::from_millis(u64::try_from(lag).unwrap_or(0)));
        }
        let key = if edit {
            format!("{mid}.{edit_date}")
        } else {
            mid.to_string()
        };
        let raw = json!({"kind": if edit { "edit" } else { "new" }, "message": m}).to_string();
        match self
            .round
            .arrive(&room, &key, update_id, parent.as_deref(), raw, read_at)
            .await
        {
            Ok(Arrived::Full) => {
                log::warn(
                    "Telegram queue full; message left for later",
                    &[("message", &key)],
                );
                None
            }
            Ok(_) => Some(identity),
            Err(e) => {
                log::warn("Telegram owner message not queued", &[("error", &e)]);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> TelegramSettings {
        TelegramSettings {
            owner: Some(7),
            group: Some(-100),
            topic: Some(5),
            ..TelegramSettings::default()
        }
    }

    fn msg(chat: &Value, from: i64, extra: &Value) -> Value {
        let mut m = json!({"message_id": 10, "date": 1, "chat": chat,
            "from": {"id": from, "is_bot": false}, "text": "hi"});
        if let (Some(m), Some(e)) = (m.as_object_mut(), extra.as_object()) {
            m.extend(e.clone());
        }
        m
    }

    #[test]
    fn gate_takes_the_owner_in_scope_only() {
        let s = settings();
        let dm = json!({"id": 7, "type": "private"});
        assert_eq!(
            gate(&msg(&dm, 7, &json!({})), false, &s),
            Gate::Owner("7".into(), None)
        );
        assert_eq!(gate(&msg(&dm, 8, &json!({})), false, &s), Gate::Drop);
        let bot = json!({"from": {"id": 7, "is_bot": true}});
        assert_eq!(gate(&msg(&dm, 7, &bot), false, &s), Gate::Drop);
        let forum = json!({"id": -100, "type": "supergroup", "is_forum": true});
        let in_topic = json!({"is_topic_message": true, "message_thread_id": 5});
        assert_eq!(
            gate(&msg(&forum, 7, &in_topic), false, &s),
            Gate::Owner("-100:5".into(), Some("-100".into()))
        );
        // General is topic 1, and a reply's thread ID there is a reply root, not a topic.
        let general_reply = json!({"message_thread_id": 5, "reply_to_message": {"message_id": 5}});
        assert_eq!(gate(&msg(&forum, 7, &general_reply), false, &s), Gate::Drop);
        let other = json!({"id": -200, "type": "supergroup"});
        assert_eq!(gate(&msg(&other, 7, &json!({})), false, &s), Gate::Drop);
        // A live-location refresh is dropped; a text edit is not.
        let live = json!({"location": {"latitude": 1.0, "longitude": 2.0}});
        let mut m = msg(&dm, 7, &live);
        m.as_object_mut().map(|o| o.remove("text"));
        assert_eq!(gate(&m, true, &s), Gate::Drop);
        assert_eq!(
            gate(&msg(&dm, 7, &json!({})), true, &s),
            Gate::Owner("7".into(), None)
        );
    }

    #[test]
    fn offset_is_valid_for_24_hours_by_the_local_clock() {
        let s = PollState {
            bot: 1,
            offset: Some(42),
            batch_ms: Some(1_000),
            guard: Vec::new(),
        };
        assert_eq!(s.offset(1_000), Some(42));
        assert_eq!(s.offset(1_000 + OFFSET_VALID_MS - 1), Some(42));
        assert_eq!(s.offset(1_000 + OFFSET_VALID_MS), None, "exactly 24 hours");
        assert_eq!(s.offset(999), None, "the clock went backwards");
        assert_eq!(
            PollState {
                batch_ms: None,
                ..s
            }
            .offset(1_000),
            None
        );
    }

    #[test]
    fn sends_stay_in_scope_and_topics_are_positive() {
        let scope = Scope::of(&settings()).unwrap();
        assert!(scope.check(7, None).is_ok());
        assert!(scope.check(-100, Some(5)).is_ok());
        assert!(scope.check(-100, Some(6)).is_err());
        assert!(scope.check(-100, Some(0)).is_err());
        assert!(scope.check(99, None).is_err());
        assert_eq!(room_of("-100:5"), Some((-100, Some(5))));
        assert_eq!(message_id("12.1700"), Some(12));
        assert!(valid_token("123:abcdefghijklmnopqrstuvwxyz") && !valid_token("abc:def"));
    }

    #[test]
    fn plain_split_joins_to_the_input() {
        let text = format!("{}\n{}", "a".repeat(3000), "b ".repeat(3000));
        let parts = text::split_plain(&text, LIMIT);
        assert!(parts.len() > 1 && parts.iter().all(|p| text::utf16_len(p) <= LIMIT));
        assert_eq!(parts.concat(), text);
        let emoji = "\u{1F600}".repeat(3000);
        let parts = text::split_plain(&emoji, LIMIT);
        assert_eq!(parts.concat(), emoji);
        assert!(parts.iter().all(|p| text::utf16_len(p) <= LIMIT));
    }

    #[test]
    fn normalise_labels_replies_forwards_and_edits() {
        let row = QueueRow {
            room: "7".into(),
            message: "10.99".into(),
            seq: 5,
            parent: None,
            position_at: 0,
            raw: String::new(),
            created_ms: 0,
        };
        let m = msg(
            &json!({"id": 7, "type": "private"}),
            7,
            &json!({"reply_to_message": {"message_id": 3, "text": "older"},
                   "photo": [{"file_id": "s", "width": 1, "height": 1}, {"file_id": "L", "width": 9, "height": 9}]}),
        );
        let i = normalise(&json!({"kind": "edit", "message": m}), &row).unwrap();
        assert_eq!(i.reply_to.as_deref(), Some("3"));
        assert_eq!(i.text, "hi");
        assert_eq!(i.attachments, vec!["photo L".to_string()]);
        assert!(i.data.iter().any(|d| d.contains("edits")));
        assert!(!i.plain);
    }
}
