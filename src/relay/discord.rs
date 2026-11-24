//! Discord REST posting (DISC-OUT-01, 03, 06, 23, 24, 27): resolve a destination by ID or unique
//! name, refuse forum and media channels, apply the outgoing text checks, render tables, split, and
//! post the parts in order. Every create parses no mentions and carries a nonce Discord enforces,
//! derived from the logical message, so a repeat after an uncertain outcome returns the existing
//! message instead of a duplicate. Rate-limit buckets are honoured before sending.

use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroU64;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use hyper::Method;
use hyper::header::{AUTHORIZATION, HeaderValue};
use ring::digest;
use serde_json::{Value, json};
use tokio::time::Instant;

use super::http::{self, Base, Kind, Outcome, Policy, Response};
use super::text::{self, Checks, DISCORD_LIMIT};

/// The real API; tests use a loopback base instead (DISC-OUT req. 10).
pub(crate) const API: &str = "https://discord.com/api/v10";
/// Longest text accepted for one post (DISC-OUT req. 13).
const INPUT_MAX: usize = 256 * 1024;
/// Discord's nonce length limit.
const NONCE_LEN: usize = 25;
/// Channel types that take no plain post: forum, media.
const FORUM_TYPES: [u64; 2] = [15, 16];

/// A Discord ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Snowflake(NonZeroU64);

impl Snowflake {
    /// Parses the canonical decimal form only.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let id = text.parse::<NonZeroU64>().ok()?;
        (id.to_string() == text).then_some(Self(id))
    }
}

impl fmt::Display for Snowflake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A destination as configured: an exact channel or thread ID, or a channel name in the guild.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChannelRef {
    Id(Snowflake),
    Name(String),
}

impl ChannelRef {
    /// Digits are an ID; anything else is a name, with a leading `#` ignored.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        if let Some(id) = Snowflake::parse(text) {
            return Some(Self::Id(id));
        }
        let name = text.strip_prefix('#').unwrap_or(text).trim();
        (!name.is_empty() && !name.bytes().all(|b| b.is_ascii_digit()))
            .then(|| Self::Name(name.to_string()))
    }
}

/// How a post ended (MSG-05): confirmed parts are never deleted or re-sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Sent {
    Confirmed,
    /// Nothing more was created; the reason names status and Discord's error code, never a token.
    Rejected(String),
    /// The last part may or may not exist.
    Uncertain(String),
}

/// The result of one post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Posted {
    pub(crate) channel: Option<Snowflake>,
    /// Confirmed message IDs, in order.
    pub(crate) ids: Vec<Snowflake>,
    pub(crate) outcome: Sent,
}

#[derive(Default)]
struct Limits {
    routes: HashMap<String, Instant>,
    global: Option<Instant>,
}

/// A REST client for one bot.
pub(crate) struct Rest {
    base: Base,
    auth: HeaderValue,
    guild: Option<Snowflake>,
    limits: Mutex<Limits>,
    channel_types: Mutex<HashMap<Snowflake, u64>>,
}

impl fmt::Debug for Rest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rest")
            .field("guild", &self.guild)
            .finish_non_exhaustive()
    }
}

fn reason(resp: &Response) -> String {
    let code = serde_json::from_slice::<Value>(&resp.body)
        .ok()
        .and_then(|v| v.get("code").and_then(Value::as_u64));
    match code {
        Some(code) => format!("HTTP {}, Discord error {code}", resp.status),
        None => format!("HTTP {}", resp.status),
    }
}

/// The deterministic nonce of part `index` of a logical message.
pub(crate) fn nonce(seed: &str, index: usize) -> String {
    let hash = digest::digest(&digest::SHA256, format!("cataly:{seed}:{index}").as_bytes());
    let mut out = crate::sys::hex(hash.as_ref());
    out.truncate(NONCE_LEN);
    out
}

impl Rest {
    pub(crate) fn new(base: Base, token: &str, guild: Option<Snowflake>) -> Result<Self, String> {
        let mut auth = HeaderValue::from_str(&format!("Bot {}", token.trim()))
            .map_err(|_| "the Discord token contains invalid characters".to_string())?;
        auth.set_sensitive(true);
        Ok(Self {
            base,
            auth,
            guild,
            limits: Mutex::default(),
            channel_types: Mutex::default(),
        })
    }

    /// One request under the shared policy, waiting first for its route's bucket and the global
    /// limit, and recording the rate-limit headers of every answer.
    async fn request(
        &self,
        route: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
        kind: Kind,
    ) -> Outcome {
        let wait_until = {
            let limits = self.limits.lock().unwrap_or_else(PoisonError::into_inner);
            [limits.routes.get(route).copied(), limits.global]
                .into_iter()
                .flatten()
                .max()
        };
        if let Some(until) = wait_until {
            let wait = until.saturating_duration_since(Instant::now());
            if wait > Duration::from_secs(60) {
                return Outcome::RateLimited(wait);
            }
            tokio::time::sleep(wait).await;
        }
        let req = http::Request {
            method,
            path,
            headers: vec![(AUTHORIZATION, self.auth.clone())],
            json: body.map(|b| b.to_string().into_bytes()),
            kind,
        };
        http::call(&self.base, &req, Policy::RELAY, |resp| {
            self.observe(route, resp);
        })
        .await
    }

    fn observe(&self, route: &str, resp: &Response) {
        let seconds = |name: &str| {
            resp.header(name)
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|s| s.is_finite() && *s >= 0.0)
                .map(Duration::from_secs_f64)
        };
        let mut limits = self.limits.lock().unwrap_or_else(PoisonError::into_inner);
        if resp.header("x-ratelimit-remaining") == Some("0")
            && let Some(after) = seconds("x-ratelimit-reset-after")
        {
            limits
                .routes
                .insert(route.to_string(), Instant::now() + after);
        }
        let global = resp.header("x-ratelimit-global") == Some("true")
            || serde_json::from_slice::<Value>(&resp.body)
                .ok()
                .and_then(|v| v.get("global").and_then(Value::as_bool))
                == Some(true);
        if resp.status == 429 && global {
            let after = seconds("retry-after").unwrap_or(Duration::from_secs(1));
            limits.global = Some(Instant::now() + after);
        }
    }

    /// A read: the parsed body, or why nothing could be read.
    async fn get(&self, route: &str, path: &str) -> Result<Value, String> {
        match self
            .request(route, Method::GET, path, None, Kind::Read)
            .await
        {
            Outcome::Done(resp) => serde_json::from_slice(&resp.body)
                .map_err(|_| "Discord sent an unreadable answer".into()),
            Outcome::Rejected(resp) => Err(reason(&resp)),
            Outcome::RateLimited(d) => Err(format!("rate limited for {} s", d.as_secs())),
            Outcome::Uncertain(why) | Outcome::Failed(why) => Err(why),
        }
    }

    /// An exact ID, or the one channel in the guild with that name (DISC-OUT req. 12).
    async fn resolve(&self, dest: &ChannelRef) -> Result<Snowflake, String> {
        let name = match dest {
            ChannelRef::Id(id) => return Ok(*id),
            ChannelRef::Name(name) => name,
        };
        let guild = self
            .guild
            .ok_or("a channel name needs [discord] guild in the config")?;
        let list = self
            .get("GET guilds/channels", &format!("/guilds/{guild}/channels"))
            .await?;
        let matches: Vec<Snowflake> = list.as_array().map_or_else(Vec::new, |all| {
            all.iter()
                .filter(|c| c.get("name").and_then(Value::as_str) == Some(name.as_str()))
                .filter_map(|c| {
                    c.get("id")
                        .and_then(Value::as_str)
                        .and_then(Snowflake::parse)
                })
                .collect()
        });
        match matches.as_slice() {
            [one] => Ok(*one),
            [] => Err(format!("no channel named '{name}' in the guild")),
            _ => Err(format!(
                "{} channels are named '{name}'; use the ID",
                matches.len()
            )),
        }
    }

    async fn channel_type(&self, id: Snowflake) -> Result<u64, String> {
        if let Some(t) = self
            .channel_types
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
        {
            return Ok(*t);
        }
        let channel = self.get("GET channels", &format!("/channels/{id}")).await?;
        let kind = channel
            .get("type")
            .and_then(Value::as_u64)
            .ok_or("Discord did not report the channel type")?;
        self.channel_types
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, kind);
        Ok(kind)
    }

    /// Posts `text` to `dest` in parts; `seed` names the logical message for its nonces.
    pub(crate) async fn post(
        &self,
        dest: &ChannelRef,
        text: &str,
        seed: &str,
        checks: &Checks,
    ) -> Posted {
        let refuse = |channel, why: String| Posted {
            channel,
            ids: Vec::new(),
            outcome: Sent::Rejected(why),
        };
        let text = text.trim();
        if text.is_empty() {
            return refuse(None, "empty text".into());
        }
        if text.len() > INPUT_MAX {
            return refuse(None, "text over 256 KiB".into());
        }
        if let Err(why) = text::check(text, checks) {
            return refuse(None, format!("refused by the outgoing text checks: {why}"));
        }
        let channel = match self.resolve(dest).await {
            Ok(id) => id,
            Err(why) => return refuse(None, why),
        };
        match self.channel_type(channel).await {
            Ok(t) if FORUM_TYPES.contains(&t) => {
                return refuse(
                    Some(channel),
                    "a forum or media channel takes no plain post; target one of its threads"
                        .into(),
                );
            }
            Ok(_) => {}
            Err(why) => return refuse(Some(channel), format!("channel not readable: {why}")),
        }
        let rendered = text::render_tables(text);
        let Some(parts) = text::split(&rendered, DISCORD_LIMIT) else {
            return refuse(Some(channel), "empty text".into());
        };
        let mut posted = Posted {
            channel: Some(channel),
            ids: Vec::new(),
            outcome: Sent::Confirmed,
        };
        let path = format!("/channels/{channel}/messages");
        for (index, part) in parts.iter().enumerate() {
            let body = json!({
                "content": part,
                "allowed_mentions": { "parse": [], "replied_user": false },
                "nonce": nonce(seed, index),
                "enforce_nonce": true,
            });
            let outcome = self
                .request(
                    "POST channels/messages",
                    Method::POST,
                    &path,
                    Some(body),
                    Kind::Idempotent,
                )
                .await;
            let result = match outcome {
                Outcome::Done(resp) => confirmed(&resp, channel),
                Outcome::Rejected(resp) => Err(Sent::Rejected(reason(&resp))),
                Outcome::RateLimited(d) => Err(Sent::Rejected(format!(
                    "rate limited for {} s",
                    d.as_secs()
                ))),
                Outcome::Failed(why) => Err(Sent::Rejected(why)),
                Outcome::Uncertain(why) => Err(Sent::Uncertain(why)),
            };
            match result {
                Ok(id) => posted.ids.push(id),
                Err(outcome) => {
                    posted.outcome = outcome;
                    break;
                }
            }
        }
        posted
    }
}

/// A create is confirmed only by a body naming a message ID in the expected channel.
fn confirmed(resp: &Response, channel: Snowflake) -> Result<Snowflake, Sent> {
    let body: Value = serde_json::from_slice(&resp.body)
        .map_err(|_| Sent::Uncertain("Discord's acknowledgement was unreadable".into()))?;
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .and_then(Snowflake::parse);
    let same = body
        .get("channel_id")
        .and_then(Value::as_str)
        .and_then(Snowflake::parse)
        == Some(channel);
    match id {
        Some(id) if same => Ok(id),
        _ => Err(Sent::Uncertain(
            "Discord's acknowledgement named no message in this channel".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_names_and_nonces() {
        assert!(Snowflake::parse("123").is_some());
        assert!(Snowflake::parse("0123").is_none());
        assert!(Snowflake::parse("0").is_none());
        assert_eq!(
            ChannelRef::parse("#general"),
            Some(ChannelRef::Name("general".into()))
        );
        assert!(matches!(ChannelRef::parse("42"), Some(ChannelRef::Id(_))));
        assert_eq!(ChannelRef::parse("#"), None);
        let a = nonce("event:1:dest", 0);
        assert_eq!(a.len(), 25);
        assert_eq!(a, nonce("event:1:dest", 0), "deterministic");
        assert_ne!(a, nonce("event:1:dest", 1));
    }

    #[tokio::test]
    async fn checks_refuse_before_any_request() {
        let base = Base::parse("http://127.0.0.1:1").unwrap();
        let rest = Rest::new(base, "token", None).unwrap();
        let checks = Checks {
            placeholders: Vec::new(),
            banned: vec!['\u{2014}'],
        };
        let dest = ChannelRef::parse("42").unwrap();
        let posted = rest.post(&dest, "x \u{2014} y", "s", &checks).await;
        assert!(matches!(&posted.outcome, Sent::Rejected(w) if w.contains("U+2014")));
        let posted = rest.post(&dest, "hi {name}", "s", &checks).await;
        assert!(matches!(&posted.outcome, Sent::Rejected(w) if w.contains("{name}")));
        let posted = rest
            .post(&ChannelRef::Name("x".into()), "ok", "s", &checks)
            .await;
        assert!(matches!(&posted.outcome, Sent::Rejected(w) if w.contains("guild")));
        assert!(format!("{rest:?}").find("token").is_none());
    }
}
