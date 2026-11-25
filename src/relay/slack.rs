//! Slack (SLACK-01 to SLACK-10, SLACK-12, DISC-OUT-14): one installed app in Socket Mode, taking part
//! either as its bot user or as a member through the app's user token (owner decision recorded in
//! `docs/build-notes.md`: Slack's Events API delivers both bot events and events on behalf of users
//! to an app's Socket Mode connection, so no public endpoint is needed for either role).
//!
//! - Tokens (SLACK req. 35): one owner-only file holding `app=xapp-…` (opens the socket) and
//!   `api=xoxb-…` (app) or `api=xoxp-…` (member). Only the hub reads it.
//! - Start (SLACK req. 36): `auth.test` names the workspace and the account; 10 s, then unhealthy and
//!   tried again in the background. State is bound to (workspace/account, owner).
//! - Intake (SLACK req. 38-44): Socket Mode events for DMs, group DMs, mentions, and replies in
//!   threads the account posted in (thread memory: one room per thread, `<channel>:<root ts>`, whose
//!   position is the seen timestamp). Every envelope is acknowledged within 3 s, after the event was
//!   admitted or dropped. After start and every reconnect, remembered threads and DMs are read from
//!   their positions (200 at most, oldest first). Own and bot messages are dropped; a reply whose
//!   channel or root disagrees with where it was read stops that read. Only allowlisted senders are
//!   admitted (owner decision 10: the owner alone by default).
//! - Posts (SLACK req. 45-47): a post answers in the request's thread and records that thread; the
//!   answer must carry the channel and a `digits.digits` timestamp, else it is uncertain. Uploads
//!   allocate a ticket whose URL must be https on exactly `files.slack.com` under Slack's upload path
//!   before any byte is sent. The seen mark is the `eyes` reaction (SLACK req. 48).

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures_util::{SinkExt as _, StreamExt as _};
use hyper::Method;
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message as Frame;

use super::http::{self, Base, Kind, Outcome, Policy, Request};
use super::intake::{self, Env, Round, Shared, Stop};
use super::pipeline::Pipeline;
use super::text::{self, Checks};
use super::{Facts, Inbound, PartOutcome, Relay};
use crate::config::Config;
use crate::config::relays::{SlackRole, slack_channel};
use crate::host::Runtime;
use crate::log;
use crate::store::Store;
use crate::store::relay::{Arrived, QueueRow, RelayWrite};
use crate::sys::now_ms;

/// The relay's store key.
pub(crate) const NAME: &str = "slack";
/// The real Web API; tests use a loopback base instead.
pub(crate) const API: &str = "https://slack.com/api";
/// Answer parts (Slack advises 4,000 characters per message).
const LIMIT: usize = 4000;
const IDENTITY_TIMEOUT: Duration = Duration::from_secs(10);
/// Envelopes are acknowledged within 3 s (SLACK req. 38); admission gets this much of it.
const ADMIT_WITHIN: Duration = Duration::from_millis(2500);
const BACKOFF_FIRST: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const FRAME_MAX: usize = 1024 * 1024;
/// Catch-up reads take this many messages at most per room (SLACK req. 39).
const CATCH_UP_MAX: usize = 200;
/// Largest file uploaded; it is held in memory while it is sent.
pub(crate) const UPLOAD_MAX: u64 = 32 * 1024 * 1024;
/// How long an untagged thread reply whose thread is not recorded waits for posts in flight; well
/// inside [`ADMIT_WITHIN`].
const RECORD_WAIT: Duration = Duration::from_secs(1);
/// Held shared by every post until its thread is recorded (SLACK req. 40-41): the owner can answer a
/// post before its response arrives, and the reply must not be judged before the record exists.
static POSTING: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

/// A `digits.digits` message timestamp.
pub(crate) fn valid_ts(ts: &str) -> bool {
    ts.split_once('.').is_some_and(|(s, f)| {
        !s.is_empty()
            && !f.is_empty()
            && s.len() <= 12
            && f.len() <= 6
            && s.bytes().all(|b| b.is_ascii_digit())
            && f.bytes().all(|b| b.is_ascii_digit())
    })
}

/// A timestamp as an ordering number: microseconds.
pub(crate) fn ts_seq(ts: &str) -> Option<i64> {
    if !valid_ts(ts) {
        return None;
    }
    let (s, f) = ts.split_once('.')?;
    let micros: i64 = format!("{f:0<6}").parse().ok()?;
    s.parse::<i64>()
        .ok()?
        .checked_mul(1_000_000)?
        .checked_add(micros)
}

/// A position as a timestamp again.
fn seq_ts(seq: i64) -> String {
    format!(
        "{}.{:06}",
        seq.div_euclid(1_000_000),
        seq.rem_euclid(1_000_000)
    )
}

/// A room: `<channel>` or `<channel>:<thread root ts>`.
pub(crate) fn room_of(room: &str) -> Option<(&str, Option<&str>)> {
    match room.split_once(':') {
        Some((c, t)) if slack_channel(c) && valid_ts(t) => Some((c, Some(t))),
        None if slack_channel(room) => Some((room, None)),
        _ => None,
    }
}

/// A Web API failure. Never carries a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApiError {
    pub(crate) status: Option<u16>,
    /// Slack's error code, or a transport reason.
    pub(crate) why: String,
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

/// The two tokens of the token file.
struct Tokens {
    app: String,
    api: String,
}

fn tokens(text: &str, role: SlackRole) -> Result<Tokens, String> {
    let mut app = None;
    let mut api = None;
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        match line.split_once('=') {
            Some(("app", v)) => app = Some(v.trim().to_string()),
            Some(("api", v)) => api = Some(v.trim().to_string()),
            _ => return Err("the Slack token file holds a line that is not app=… or api=…".into()),
        }
    }
    let app = app
        .filter(|t| t.starts_with("xapp-"))
        .ok_or("the Slack token file needs app=xapp-… (the app-level token for Socket Mode)")?;
    let prefix = match role {
        SlackRole::App => "xoxb-",
        SlackRole::Member => "xoxp-",
    };
    let api = api
        .filter(|t| t.starts_with(prefix))
        .ok_or_else(|| format!("the Slack token file needs api={prefix}… for this role"))?;
    let shape = |t: &str| t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if !shape(&app) || !shape(&api) {
        return Err("a Slack token holds characters a token never has".into());
    }
    Ok(Tokens { app, api })
}

/// Percent-encodes a query value.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// A Web API client for one account.
pub(crate) struct Api {
    base: Base,
    tokens: Tokens,
    /// Workspace and account, once `auth.test` named them (SLACK-04).
    pub(crate) account: Mutex<Option<(String, String)>>,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

/// Slack errors that leave a write's effect unknown (SLACK req. 46).
const UNCERTAIN_ERRORS: [&str; 5] = [
    "internal_error",
    "fatal_error",
    "request_timeout",
    "service_unavailable",
    "timeout",
];

impl Api {
    /// The client for the current config's token file.
    pub(crate) fn from_config(
        config: &Config,
        config_file: &std::path::Path,
    ) -> Result<Self, String> {
        let path = intake::secret_path(
            config.slack.token_file.as_ref(),
            config_file,
            "slack-tokens",
        );
        let tokens = tokens(&intake::read_secret(&path)?, config.slack.role)?;
        let base = match &config.slack.api_base {
            Some(b) => b.clone(),
            None => Base::parse(API)?,
        };
        Ok(Self {
            base,
            tokens,
            account: Mutex::new(None),
        })
    }

    fn bearer(token: &str) -> Result<HeaderValue, ApiError> {
        HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| ApiError {
            status: None,
            why: "the token cannot be sent".into(),
            uncertain: false,
        })
    }

    /// One Web API method: a JSON body (POST) or query arguments (GET).
    async fn call(
        &self,
        method: &str,
        args: &Value,
        kind: Kind,
        app_token: bool,
    ) -> Result<Value, ApiError> {
        let token = if app_token {
            &self.tokens.app
        } else {
            &self.tokens.api
        };
        let mut headers = vec![(AUTHORIZATION, Self::bearer(token)?)];
        let (verb, path, json) = if kind == Kind::Read {
            let query: Vec<String> = args
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, v)| {
                    let v = v.as_str().map_or_else(|| v.to_string(), str::to_string);
                    format!("{k}={}", encode(&v))
                })
                .collect();
            (Method::GET, format!("/{method}?{}", query.join("&")), None)
        } else {
            headers.push((
                CONTENT_TYPE,
                HeaderValue::from_static("application/json; charset=utf-8"),
            ));
            (
                Method::POST,
                format!("/{method}"),
                Some(args.to_string().into_bytes()),
            )
        };
        let req = Request {
            method: verb,
            path: &path,
            headers,
            json,
            kind,
        };
        let writes = kind != Kind::Read;
        let fail = |status, why: String, uncertain| ApiError {
            status,
            why,
            uncertain,
        };
        match http::call(&self.base, &req, Policy::CHAT, |_| {}).await {
            Outcome::Done(resp) => {
                let v: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
                match v.get("ok").and_then(Value::as_bool) {
                    Some(true) => Ok(v),
                    Some(false) => {
                        let code = v.get("error").and_then(Value::as_str).unwrap_or("error");
                        Err(fail(
                            Some(resp.status),
                            code.to_string(),
                            writes && UNCERTAIN_ERRORS.contains(&code),
                        ))
                    }
                    None => Err(fail(Some(resp.status), "no success field".into(), writes)),
                }
            }
            Outcome::Rejected(resp) => Err(fail(Some(resp.status), "refused".into(), false)),
            Outcome::RateLimited(d) => Err(fail(
                Some(429),
                format!("rate limited for {} s", d.as_secs()),
                false,
            )),
            Outcome::Uncertain(why) => Err(fail(None, why, writes)),
            Outcome::Failed(why) => Err(fail(None, why, false)),
        }
    }

    /// `auth.test`: the workspace and the account (SLACK req. 36).
    async fn identity(&self) -> Result<(String, String), ApiError> {
        let v = self
            .call("auth.test", &json!({}), Kind::Read, false)
            .await?;
        let id = |k: &str| {
            v.get(k).and_then(Value::as_str).filter(|s| {
                !s.is_empty()
                    && s.len() <= 64
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            })
        };
        match (id("team_id"), id("user_id")) {
            (Some(team), Some(user)) => Ok((team.to_string(), user.to_string())),
            _ => Err(ApiError {
                status: None,
                why: "the identity check named no valid workspace and account".into(),
                uncertain: false,
            }),
        }
    }

    /// Posts one message; its timestamp (SLACK req. 45).
    async fn post_message(
        &self,
        channel: &str,
        thread: Option<&str>,
        text: &str,
    ) -> Result<String, ApiError> {
        let mut body = json!({"channel": channel, "text": text, "unfurl_links": false});
        if let (Some(t), Some(map)) = (thread, body.as_object_mut()) {
            map.insert("thread_ts".into(), json!(t));
        }
        let v = self
            .call("chat.postMessage", &body, Kind::Once, false)
            .await?;
        let ts = v.get("ts").and_then(Value::as_str).filter(|t| valid_ts(t));
        let same = v.get("channel").and_then(Value::as_str) == Some(channel);
        match ts {
            Some(ts) if same => Ok(ts.to_string()),
            _ => Err(ApiError {
                status: None,
                why: "the answer named no channel and timestamp".into(),
                uncertain: true,
            }),
        }
    }
}

/// Edits one message (SLACK req. 46): a bad target is refused before any request.
pub(crate) async fn edit(
    api: &Api,
    store: &Store,
    target: &str,
    ts: &str,
    text: &str,
) -> Result<(), ApiError> {
    let channel = resolve(api, store, target).await?;
    if !valid_ts(ts) {
        return Err(ApiError {
            status: None,
            why: "the edit target is not a channel and digits.digits timestamp".into(),
            uncertain: false,
        });
    }
    api.call(
        "chat.update",
        &json!({"channel": channel, "ts": ts, "text": text}),
        Kind::Idempotent,
        false,
    )
    .await
    .map(drop)
}

/// Records a thread the account posted in (SLACK req. 40): its room's position starts at the post;
/// a thread already recorded keeps its older position.
async fn remember(store: &Store, channel: &str, root: &str, posted: &str) -> Result<(), String> {
    let position = ts_seq(posted).ok_or("bad timestamp")?;
    store
        .relay(RelayWrite::Baseline {
            relay: NAME.into(),
            room: format!("{channel}:{root}"),
            position,
            parent: Some(channel.to_string()),
        })
        .await
        .map(drop)
        .map_err(|e| e.to_string())
}

/// Whether a thread room is recorded.
async fn recorded(store: &Store, room: &str) -> bool {
    let room = room.to_string();
    store
        .read(|s| s.positions(NAME))
        .await
        .is_ok_and(|p| p.iter().any(|p| p.room == room))
}

/// Posts one message and records its thread (SLACK req. 40, 45): a recording failure keeps the
/// confirmed post. Holds [`POSTING`] throughout, so a reply cannot be judged before its thread is
/// recorded.
async fn post_and_record(
    api: &Api,
    store: &Store,
    channel: &str,
    thread: Option<&str>,
    text: &str,
) -> Result<String, ApiError> {
    let _posting = POSTING.read().await;
    let ts = api.post_message(channel, thread, text).await?;
    if let Err(e) = remember(store, channel, thread.unwrap_or(&ts), &ts).await {
        log::warn("Slack thread not recorded; post kept", &[("error", &e)]);
    }
    Ok(ts)
}

/// A post to a destination `<workspace>/<conversation>` (job events, the board): split, the first
/// part may start or join a thread; every post records its thread. Confirmed timestamps, or the
/// failure with those already confirmed.
pub(crate) async fn send(
    api: &Api,
    store: &Store,
    target: &str,
    thread: Option<&str>,
    text: &str,
    checks: &Checks,
) -> Result<Vec<String>, (Vec<String>, ApiError)> {
    let refuse = |why: String| {
        (
            Vec::new(),
            ApiError {
                status: None,
                why,
                uncertain: false,
            },
        )
    };
    let channel = resolve(api, store, target)
        .await
        .map_err(|e| (Vec::new(), e))?;
    if thread.is_some_and(|t| !valid_ts(t)) {
        return Err(refuse("not a Slack conversation and thread".into()));
    }
    text::check(text, checks)
        .map_err(|e| refuse(format!("refused by the outgoing text checks: {e}")))?;
    let parts = text::split(&text::render_tables(text.trim()), LIMIT)
        .ok_or_else(|| refuse("empty text".into()))?;
    let mut done = Vec::new();
    for part in parts {
        match post_and_record(api, store, channel, thread, &part).await {
            Ok(ts) => done.push(ts),
            Err(e) => return Err((done, e)),
        }
    }
    Ok(done)
}

/// Whether an upload URL may receive bytes (SLACK req. 47): https on exactly `files.slack.com`, no
/// user or password, under Slack's upload path.
pub(crate) fn upload_url_ok(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    authority.eq_ignore_ascii_case("files.slack.com") && path.starts_with("/upload/")
}

/// A destination `<workspace>/<conversation>` (SLACK-04), such as `T0123/C0456`.
pub(crate) fn destination(target: &str) -> Option<(&str, &str)> {
    let (team, channel) = target.split_once('/')?;
    (team.starts_with('T') && crate::config::relays::slack_id(team) && slack_channel(channel))
        .then_some((team, channel))
}

/// The conversation of `target`, once its workspace is this hub's Slack account's: the one the
/// intake bound at its identity check, else what `auth.test` names now. Another workspace is refused
/// before any post or upload request (SLACK-04).
pub(crate) async fn resolve<'a>(
    api: &Api,
    store: &Store,
    target: &'a str,
) -> Result<&'a str, ApiError> {
    let refuse = |why: String| ApiError {
        status: None,
        why,
        uncertain: false,
    };
    let (team, channel) = destination(target)
        .ok_or_else(|| refuse("not a Slack destination <workspace>/<conversation>".into()))?;
    let bound = store
        .read(|r| r.binding(NAME))
        .await
        .map_err(|e| refuse(format!("the Slack binding is unreadable: {e}")))?
        .and_then(|b| b.split_once('/').map(|(t, _)| t.to_string()));
    let own = match bound {
        Some(t) => t,
        None => api.identity().await?.0,
    };
    if own == team {
        Ok(channel)
    } else {
        Err(refuse(format!(
            "{team} is not the workspace of this Slack account; nothing was sent"
        )))
    }
}

/// Uploads a file into a conversation (SLACK req. 47); the file ID.
pub(crate) async fn upload(
    api: &Api,
    store: &Store,
    target: &str,
    thread: Option<&str>,
    name: &str,
    bytes: Vec<u8>,
) -> Result<String, ApiError> {
    let refuse = |why: &str| ApiError {
        status: None,
        why: why.to_string(),
        uncertain: false,
    };
    let channel = resolve(api, store, target).await?;
    if thread.is_some_and(|t| !valid_ts(t)) {
        return Err(refuse("not a Slack conversation and thread"));
    }
    if bytes.is_empty()
        || bytes.len() as u64 > UPLOAD_MAX
        || name.trim().is_empty()
        || name.len() > 255
    {
        return Err(refuse("the file must be 1 byte to 32 MiB with a name"));
    }
    let ticket = api
        .call(
            "files.getUploadURLExternal",
            &json!({"filename": name, "length": bytes.len().to_string()}),
            Kind::Read,
            false,
        )
        .await?;
    let file = ticket
        .get("file_id")
        .and_then(Value::as_str)
        .filter(|f| {
            f.len() > 1
                && f.starts_with('F')
                && f.bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        })
        .ok_or_else(|| refuse("the upload ticket named no valid file ID"))?
        .to_string();
    let url = ticket
        .get("upload_url")
        .and_then(Value::as_str)
        .filter(|u| upload_url_ok(u))
        .ok_or_else(|| {
            refuse("the upload URL is not https://files.slack.com/upload/…; nothing was sent")
        })?;
    transfer(api, url, bytes).await?;
    let mut body = json!({"files": [{"id": file, "title": name}], "channel_id": channel});
    if let (Some(t), Some(map)) = (thread, body.as_object_mut()) {
        map.insert("thread_ts".into(), json!(t));
    }
    let done = api
        .call("files.completeUploadExternal", &body, Kind::Once, false)
        .await?;
    let listed = done
        .get("files")
        .and_then(Value::as_array)
        .is_some_and(|f| {
            f.iter()
                .any(|x| x.get("id").and_then(Value::as_str) == Some(file.as_str()))
        });
    if listed {
        Ok(file)
    } else {
        Err(ApiError {
            status: None,
            why: "the completion did not list the file".into(),
            uncertain: true,
        })
    }
}

/// Sends the file bytes to a checked upload URL; a loopback API base (tests) stands in for
/// files.slack.com.
async fn transfer(api: &Api, url: &str, bytes: Vec<u8>) -> Result<(), ApiError> {
    let refuse = |why: String| ApiError {
        status: None,
        why,
        uncertain: false,
    };
    let path = url.strip_prefix("https://files.slack.com").unwrap_or("/");
    let base = if api.base.is_loopback() {
        api.base.clone()
    } else {
        Base::parse("https://files.slack.com").map_err(refuse)?
    };
    let req = Request {
        method: Method::POST,
        path,
        headers: vec![(
            CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        )],
        json: Some(bytes),
        kind: Kind::Once,
    };
    match http::call(&base, &req, Policy::TRANSFER, |_| {}).await {
        Outcome::Done(resp) if resp.status == 200 => Ok(()),
        Outcome::Done(resp) | Outcome::Rejected(resp) => Err(ApiError {
            status: Some(resp.status),
            why: "the upload was not taken".into(),
            uncertain: false,
        }),
        Outcome::Uncertain(why) => Err(ApiError {
            status: None,
            why,
            uncertain: true,
        }),
        Outcome::RateLimited(_) | Outcome::Failed(_) => {
            Err(refuse("the upload was not sent".into()))
        }
    }
}

/// The Slack side of the relay trait.
pub(crate) struct SlackRelay {
    pub(crate) api: Arc<Api>,
    store: Store,
    checks: Checks,
    shared: Arc<Shared>,
    /// Cleared when the app lacks the reactions scope (SLACK req. 48).
    receipts: AtomicBool,
}

impl Relay for SlackRelay {
    const NAME: &'static str = NAME;
    const REPEATABLE: bool = false;
    const LIVE_ONCE: bool = false;

    fn parts(&self, answer: &str) -> Result<Vec<String>, String> {
        let answer = answer.trim();
        text::check(answer, &self.checks)
            .map_err(|e| format!("refused by the outgoing text checks: {e}"))?;
        text::split(&text::render_tables(answer), LIMIT).ok_or_else(|| "empty answer".into())
    }

    fn nonce(&self, room: &str, source: &str, index: usize) -> String {
        format!("{room}/{source}/{index}")
    }

    fn normalise(&self, row: &QueueRow) -> Result<Inbound, String> {
        let m: Value = serde_json::from_str(&row.raw).map_err(|e| e.to_string())?;
        let text = m
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let attachments: Vec<String> = m
            .get("files")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|f| {
                let id = f.get("id").and_then(Value::as_str).unwrap_or("?");
                let name = f.get("name").and_then(Value::as_str).unwrap_or("");
                match f.get("size").and_then(Value::as_i64) {
                    Some(n) => format!("file {id} {name} ({n} bytes)"),
                    None => format!("file {id} {name}"),
                }
            })
            .collect();
        Ok(Inbound {
            room: row.room.clone(),
            parent: row.parent.clone(),
            message: row.message.clone(),
            seq: row.seq,
            reply_to: None,
            plain: attachments.is_empty(),
            stable: format!("{}|{text}|{attachments:?}", row.message),
            text,
            data: Vec::new(),
            attachments,
        })
    }

    async fn post(
        &self,
        room: &str,
        reply_to: Option<&str>,
        text: &str,
        _nonce: &str,
    ) -> PartOutcome {
        let Some((channel, thread_ts)) = room_of(room) else {
            return PartOutcome::Rejected("bad room".into());
        };
        // An answer goes into the request's thread: the room's root, else under the request.
        let thread = thread_ts.or(reply_to.filter(|t| valid_ts(t)));
        match post_and_record(&self.api, &self.store, channel, thread, text).await {
            Ok(ts) => PartOutcome::Posted {
                id: ts,
                linked: thread.is_some(),
            },
            Err(e) if e.why == "thread_not_found" || e.why == "message_not_found" => {
                PartOutcome::SourceGone
            }
            Err(e) if e.uncertain => PartOutcome::Uncertain(e.to_string()),
            Err(e) => PartOutcome::Rejected(e.to_string()),
        }
    }

    async fn seen(&self, room: &str, message: &str) -> Result<(), String> {
        if !self.receipts.load(Ordering::Acquire) {
            return Ok(());
        }
        let (channel, _) = room_of(room).ok_or("bad room")?;
        let body = json!({"channel": channel, "timestamp": message, "name": "eyes"});
        match self
            .api
            .call("reactions.add", &body, Kind::Idempotent, false)
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.why == "already_reacted" => Ok(()),
            Err(e) if e.why == "missing_scope" || e.why == "not_allowed_token_type" => {
                self.receipts.store(false, Ordering::Release);
                self.shared.set(|h| {
                    h.last_error = Some("seen marks skipped: the app lacks reactions:write".into());
                });
                log::warn("Slack seen marks skipped: no reactions scope", &[]);
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    fn typing(&self, _room: &str) -> impl Future<Output = Result<(), String>> + Send {
        std::future::ready(Err("Slack shows no typing for answers".into()))
    }

    async fn facts(&self, room: &str) -> Result<Facts, String> {
        let (channel, thread_ts) = room_of(room).ok_or("bad room")?;
        if let Some(root) = thread_ts {
            return Ok(Facts {
                name: Some(format!("thread {root}")),
                topic: None,
                dm: false,
            });
        }
        let v = self
            .api
            .call(
                "conversations.info",
                &json!({"channel": channel}),
                Kind::Read,
                false,
            )
            .await
            .map_err(|e| e.to_string())?;
        let c = v.get("channel").cloned().unwrap_or(Value::Null);
        Ok(Facts {
            name: c.get("name").and_then(Value::as_str).map(str::to_string),
            topic: c
                .get("topic")
                .and_then(|t| t.get("value"))
                .and_then(Value::as_str)
                .filter(|t| !t.trim().is_empty())
                .map(str::to_string),
            dm: c.get("is_im").and_then(Value::as_bool) == Some(true),
        })
    }
}

/// The supervised Slack intake task.
pub(crate) async fn run<T: Runtime>(
    shared: Arc<Shared>,
    env: Arc<Env<T>>,
    shutdown: watch::Receiver<bool>,
) -> Result<(), String> {
    let off = |c: &Config| (!c.slack.intake()).then(|| "set [slack] owner to start intake".into());
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
    let settings = config.slack.clone();
    let Some(owner) = settings.owner.clone() else {
        return Stop::Fatal("no owner configured".into());
    };
    shared.set(|h| h.owner = Some(owner.clone()));
    let api = match Api::from_config(&config, &env.config_file) {
        Ok(api) => Arc::new(api),
        Err(e) => return Stop::Fatal(e),
    };
    let (team, account) = match tokio::time::timeout(IDENTITY_TIMEOUT, api.identity()).await {
        Ok(Ok(id)) => id,
        Ok(Err(e))
            if e.why == "invalid_auth" || e.why == "not_authed" || e.why == "token_revoked" =>
        {
            return Stop::Fatal(format!("the Slack token was refused ({})", e.why));
        }
        Ok(Err(e)) => return Stop::Retry(format!("identity check: {e}")),
        Err(_) => {
            shared.set(|h| h.state = "unhealthy".into());
            return Stop::Retry("identity check: no answer within 10 s".into());
        }
    };
    *api.account.lock().unwrap_or_else(PoisonError::into_inner) =
        Some((team.clone(), account.clone()));
    if let Err(stop) = intake::bind(&env.store, NAME, &format!("{team}/{account}"), &owner).await {
        return stop;
    }
    shared.set(|h| {
        h.bot = Some(format!("{team}/{account}"));
        h.reason = None;
        h.state = "connecting".into();
    });
    let relay = Arc::new(SlackRelay {
        api: Arc::clone(&api),
        store: env.store.clone(),
        checks: config.outgoing.clone(),
        shared: Arc::clone(&shared),
        receipts: AtomicBool::new(true),
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
        let socket = Socket {
            api,
            account,
            senders: settings.senders(),
            shared: Arc::clone(&shared),
            round: Arc::clone(&round),
            store: env.store.clone(),
            loopback: settings.api_base.as_ref().is_some_and(Base::is_loopback),
        };
        let stop = shutdown.clone();
        tokio::spawn(async move {
            let end = socket.run(stop).await;
            let _ = tx.send(end);
        });
    }
    round
        .serve(
            rx,
            env.config.clone(),
            |c| c.slack.clone(),
            &shared.kick,
            shutdown,
        )
        .await
}

/// Where a Socket Mode URL may point: Slack's own hosts, or loopback when the API base is loopback.
fn socket_url(url: &str, loopback: bool) -> Option<(bool, String, u16)> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("wss://") {
        (true, r)
    } else {
        (false, url.strip_prefix("ws://")?)
    };
    let authority = rest.split(['/', '?']).next()?;
    if authority.contains('@') {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_ascii_lowercase(), p.parse().ok()?),
        None => (authority.to_ascii_lowercase(), if tls { 443 } else { 80 }),
    };
    let is_loopback = host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    let ok = if loopback {
        is_loopback
    } else {
        tls && (host == "slack.com" || host.ends_with(".slack.com"))
    };
    ok.then_some((tls, host, port))
}

/// Why one Socket Mode connection ended.
enum Down {
    Again(String),
    Fatal(String),
    Shutdown,
}

/// The Socket Mode connection and its catch-up reads.
struct Socket<T: Runtime> {
    api: Arc<Api>,
    account: String,
    senders: Vec<String>,
    shared: Arc<Shared>,
    round: Arc<Round<SlackRelay, T>>,
    store: Store,
    loopback: bool,
}

impl<T: Runtime> Socket<T> {
    async fn run(self, mut shutdown: watch::Receiver<bool>) -> Stop {
        let mut backoff = BACKOFF_FIRST;
        loop {
            let started = Instant::now();
            let down = tokio::select! {
                d = self.connect() => d,
                _ = shutdown.changed() => Down::Shutdown,
            };
            match down {
                Down::Shutdown => return Stop::Shutdown,
                Down::Fatal(why) => return Stop::Fatal(why),
                Down::Again(why) => {
                    log::info("Slack socket reconnecting", &[("reason", &why)]);
                    self.shared.set(|h| {
                        h.state = "connecting".into();
                        h.caught_up = false;
                        h.last_error = Some(why);
                    });
                }
            }
            if started.elapsed() >= Duration::from_secs(60) {
                backoff = BACKOFF_FIRST;
            }
            tokio::select! {
                () = tokio::time::sleep(backoff) => {}
                _ = shutdown.changed() => return Stop::Shutdown,
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }

    async fn connect(&self) -> Down {
        let opened = match self
            .api
            .call("apps.connections.open", &json!({}), Kind::Once, true)
            .await
        {
            Ok(v) => v,
            Err(e) if e.why == "invalid_auth" || e.why == "not_authed" => {
                return Down::Fatal("the app-level token was refused".into());
            }
            Err(e) => return Down::Again(format!("opening the socket: {e}")),
        };
        let Some(url) = opened.get("url").and_then(Value::as_str) else {
            return Down::Again("no socket URL".into());
        };
        let Some((tls, host, port)) = socket_url(url, self.loopback) else {
            return Down::Fatal("the socket URL is not Slack's".into());
        };
        let mut ws = match http::websocket(tls, &host, port, url, FRAME_MAX).await {
            Ok(ws) => ws,
            Err(e) => return Down::Again(e),
        };
        let established = Instant::now();
        let mut caught_up = false;
        loop {
            let frame = match ws.next().await {
                Some(Ok(f)) => f,
                Some(Err(e)) => return Down::Again(format!("socket read: {e}")),
                None => return Down::Again("socket closed".into()),
            };
            let read_at = Instant::now();
            let text = match frame {
                Frame::Text(t) => t.to_string(),
                Frame::Close(_) => return Down::Again("socket closed".into()),
                _ => continue,
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            match v.get("type").and_then(Value::as_str) {
                Some("hello") => {
                    self.shared.set(|h| {
                        h.state = "connected".into();
                        h.connected_ms = Some(now_ms());
                        h.reason = None;
                    });
                    if !caught_up {
                        caught_up = true;
                        match self.catch_up().await {
                            Ok(()) => {
                                self.shared.catch_up_latency.record(established.elapsed());
                                self.shared.set(|h| h.caught_up = true);
                            }
                            Err(e) => {
                                log::warn("Slack catch-up read failed", &[("error", &e)]);
                                self.shared
                                    .set(|h| h.last_error = Some(format!("catch-up read: {e}")));
                            }
                        }
                    }
                }
                Some("disconnect") => {
                    let reason = v.get("reason").and_then(Value::as_str).unwrap_or("asked");
                    return Down::Again(format!("Slack asked to reconnect ({reason})"));
                }
                _ => {}
            }
            if let Some(envelope) = v.get("envelope_id").and_then(Value::as_str) {
                if let Some(event) = v
                    .get("payload")
                    .and_then(|p| p.get("event"))
                    .filter(|_| v.get("type").and_then(Value::as_str) == Some("events_api"))
                {
                    // Admitted or dropped first, acknowledged within 3 s either way.
                    let _ = tokio::time::timeout(ADMIT_WITHIN, self.event(event, read_at)).await;
                }
                let ack = json!({"envelope_id": envelope}).to_string();
                if let Err(e) = ws.send(Frame::text(ack)).await {
                    return Down::Again(format!("acknowledgement not sent: {e}"));
                }
            }
        }
    }

    /// One live event: gate, then the durable queue (SLACK req. 41-44).
    async fn event(&self, e: &Value, read_at: Instant) {
        intake::touched(&self.shared);
        let kind = e.get("type").and_then(Value::as_str);
        if !matches!(kind, Some("message" | "app_mention")) {
            return;
        }
        let Some(channel) = e
            .get("channel")
            .and_then(Value::as_str)
            .filter(|c| slack_channel(c))
        else {
            return;
        };
        let channel_type = e.get("channel_type").and_then(Value::as_str);
        let direct = matches!(channel_type, Some("im" | "mpim")) || channel.starts_with('D');
        self.take(channel, e, direct, read_at).await;
    }

    /// Admits an allowlisted human's message addressed to the account, or a reply in a remembered
    /// thread; anything else is dropped before any receipt (SLACK req. 41-44).
    async fn take(&self, channel: &str, m: &Value, direct: bool, read_at: Instant) {
        let subtype = m.get("subtype").and_then(Value::as_str);
        if subtype.is_some_and(|s| s != "file_share" && s != "thread_broadcast") {
            return;
        }
        let user = m.get("user").and_then(Value::as_str).unwrap_or("");
        if m.get("bot_id").is_some()
            || user == self.account
            || !self.senders.iter().any(|s| s == user)
        {
            return;
        }
        let Some(ts) = m.get("ts").and_then(Value::as_str).filter(|t| valid_ts(t)) else {
            return;
        };
        let thread_ts = m
            .get("thread_ts")
            .and_then(Value::as_str)
            .filter(|t| valid_ts(t) && *t != ts);
        let room = match thread_ts {
            Some(r) => format!("{channel}:{r}"),
            None => channel.to_string(),
        };
        let mention = m
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|t| t.contains(&format!("<@{}>", self.account)));
        let remembered = match thread_ts {
            Some(_) if !(direct || mention) => {
                if recorded(&self.store, &room).await {
                    true
                } else {
                    // The owner may answer a post before its thread is recorded: wait for the posts
                    // in flight, then look again.
                    let _ = tokio::time::timeout(RECORD_WAIT, POSTING.write()).await;
                    recorded(&self.store, &room).await
                }
            }
            _ => false,
        };
        if !(direct || mention || remembered) {
            return;
        }
        let Some(seq) = ts_seq(ts) else {
            return;
        };
        let parent = thread_ts.map(|_| channel.to_string());
        match self
            .round
            .arrive(&room, ts, seq, parent.as_deref(), m.to_string(), read_at)
            .await
        {
            Ok(Arrived::Full) => log::warn("Slack queue full", &[("message", &ts)]),
            Ok(_) => {}
            Err(e) => log::warn("Slack owner message not queued", &[("error", &e)]),
        }
    }

    /// Reads remembered threads and DMs from their positions (SLACK req. 39, 43).
    async fn catch_up(&self) -> Result<(), String> {
        let positions = self
            .store
            .read(|r| r.positions(NAME))
            .await
            .map_err(|e| e.to_string())?;
        let threads = positions.iter().filter(|p| p.room.contains(':')).count();
        self.shared.set(|h| h.followed_threads = threads);
        for p in positions.iter().filter(|p| p.held.is_none()) {
            let Some((channel, root)) = room_of(&p.room) else {
                continue;
            };
            let oldest = seq_ts(p.position);
            let (method, args) = match root {
                Some(root) => (
                    "conversations.replies",
                    json!({"channel": channel, "ts": root, "oldest": oldest, "limit": CATCH_UP_MAX}),
                ),
                None if channel.starts_with('D') => (
                    "conversations.history",
                    json!({"channel": channel, "oldest": oldest, "limit": CATCH_UP_MAX}),
                ),
                None => continue,
            };
            let v = self
                .api
                .call(method, &args, Kind::Read, false)
                .await
                .map_err(|e| e.to_string())?;
            let mut messages: Vec<Value> = v
                .get("messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            messages.retain(|m| {
                m.get("ts")
                    .and_then(Value::as_str)
                    .and_then(ts_seq)
                    .is_some_and(|s| s > p.position)
            });
            messages.sort_by_key(|m| m.get("ts").and_then(Value::as_str).and_then(ts_seq));
            messages.truncate(CATCH_UP_MAX);
            // A reply whose stated channel or root disagrees stops this read (SLACK req. 43).
            let consistent = messages.iter().all(|m| {
                let ch = m.get("channel").and_then(Value::as_str);
                let rt = m.get("thread_ts").and_then(Value::as_str);
                ch.is_none_or(|c| c == channel) && root.is_none_or(|r| rt == Some(r))
            });
            if !consistent {
                log::warn(
                    "Slack read stopped: inconsistent thread data",
                    &[("room", &p.room)],
                );
                continue;
            }
            let now = Instant::now();
            for m in &messages {
                self.take(channel, m, root.is_none(), now).await;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_rooms_and_urls() {
        assert!(valid_ts("1700000000.000100") && !valid_ts("1700000000") && !valid_ts("a.1"));
        assert_eq!(ts_seq("1.5"), Some(1_500_000));
        assert_eq!(seq_ts(1_500_000), "1.500000");
        assert_eq!(room_of("C1:1.000002"), Some(("C1", Some("1.000002"))));
        assert_eq!(room_of("U1"), None);
        assert!(upload_url_ok("https://files.slack.com/upload/v1/abc"));
        assert!(!upload_url_ok(
            "https://files.slack.com.evil.example/upload/v1/abc"
        ));
        assert!(!upload_url_ok(
            "https://user:pw@files.slack.com/upload/v1/abc"
        ));
        assert!(!upload_url_ok("http://files.slack.com/upload/v1/abc"));
        assert!(!upload_url_ok("https://files.slack.com/other"));
        assert!(socket_url("wss://wss-primary.slack.com/link/?ticket=1", false).is_some());
        assert!(socket_url("wss://evil.example/link", false).is_none());
        assert!(socket_url("ws://127.0.0.1:9/link", true).is_some());
        assert!(socket_url("ws://127.0.0.1:9/link", false).is_none());
    }

    #[test]
    fn token_file_names_both_tokens_for_the_role() {
        let app = format!("xapp-{}", "1-A1-1-abc");
        let bot = format!("xoxb-{}", "1-2-abc");
        let user = format!("xoxp-{}", "1-2-abc");
        let file = format!("app={app}\napi={bot}\n");
        assert!(tokens(&file, SlackRole::App).is_ok());
        assert!(tokens(&file, SlackRole::Member).is_err());
        assert!(tokens(&format!("app={app}\napi={user}"), SlackRole::Member).is_ok());
        assert!(tokens(&format!("api={bot}"), SlackRole::App).is_err());
        assert_eq!(encode("a b&c"), "a%20b%26c");
    }
}
