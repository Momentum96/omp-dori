//! Lane threads on Discord (DISC-OUT-11 to 13, DISC-IN-22, OJ-28, EVR-14; DESIGN 12.2 C17): report
//! posts with deterministic nonces, the hub-set status word in the title (read fresh each time,
//! never from a saved copy) with archive on done, and thread creation from an owner message after
//! its source is proven. Thread discovery for intake stays in `intake` (DISC-IN req. 8-10).

use std::sync::Arc;

use hyper::Method;
use serde_json::{Value, json};
use tokio::sync::watch;

use super::{ChannelRef, Rest, Sent, Snowflake};
use crate::config::Config;
use crate::lanes::{BoxFut, Posted, Threads};
use crate::relay::http::Kind;

/// Status words the hub puts in front of a lane thread's title.
const WORDS: [&str; 3] = ["[working] ", "[waiting] ", "[done] "];
const TITLE_MAX: usize = 100;

/// Builds the Discord client from the config in force.
pub(crate) type MakeRest = Arc<dyn Fn(&Config) -> Result<Arc<Rest>, String> + Send + Sync>;

pub(crate) struct DiscordThreads {
    pub(crate) config: watch::Receiver<Arc<Config>>,
    pub(crate) rest: MakeRest,
}

fn id(text: &str) -> Result<Snowflake, String> {
    Snowflake::parse(text).ok_or_else(|| format!("'{text}' is not a Discord ID"))
}

/// The title with the status word: earlier words removed, `None` when the result would be empty or
/// over 100 characters (DISC-OUT req. 51).
pub(crate) fn titled(current: &str, word: &str) -> Option<String> {
    let mut base = current;
    while let Some(rest) = WORDS.iter().find_map(|w| base.strip_prefix(w)) {
        base = rest;
    }
    let title = format!("[{word}] {base}");
    (!base.trim().is_empty() && title.chars().count() <= TITLE_MAX).then_some(title)
}

impl DiscordThreads {
    fn client(&self) -> Result<(Arc<Rest>, Arc<Config>), String> {
        let config = Arc::clone(&self.config.borrow());
        Ok(((self.rest)(&config)?, config))
    }

    async fn post_now(&self, thread: &str, text: &str, seed: &str) -> Posted {
        let (rest, config) = match self.client() {
            Ok(c) => c,
            Err(e) => return Posted::Rejected(e),
        };
        let Ok(thread) = id(thread) else {
            return Posted::Rejected("bad thread ID".into());
        };
        match rest
            .post(&ChannelRef::Id(thread), text, seed, &config.outgoing)
            .await
            .outcome
        {
            Sent::Confirmed => Posted::Confirmed,
            Sent::Rejected(why) => Posted::Rejected(why),
            Sent::Uncertain(why) => Posted::Uncertain(why),
        }
    }

    async fn status_now(&self, thread: &str, word: &str, archive: bool) -> Result<(), String> {
        let (rest, _) = self.client()?;
        let thread = id(thread)?;
        let channel = rest.channel(thread).await.map_err(|e| e.why)?;
        let name = channel
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let archived = channel
            .pointer("/thread_metadata/archived")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let title = titled(name, word)
            .ok_or("the status word would empty the title or make it longer than 100")?;
        let mut body = serde_json::Map::new();
        if title != name {
            body.insert("name".into(), json!(title));
        }
        if archive != archived {
            body.insert("archived".into(), json!(archive));
        }
        if body.is_empty() {
            return Ok(());
        }
        rest.call(
            "PATCH channels",
            Method::PATCH,
            &format!("/channels/{thread}"),
            Some(Value::Object(body)),
            Kind::Idempotent,
        )
        .await
        .map(drop)
        .map_err(|e| e.why)
    }

    async fn create_now(
        &self,
        room: &str,
        message: &str,
        title: &str,
    ) -> Result<String, (bool, String)> {
        let (rest, config) = self.client().map_err(|e| (false, e))?;
        let (room, message) = (
            id(room).map_err(|e| (false, e))?,
            id(message).map_err(|e| (false, e))?,
        );
        let owner = config
            .discord
            .owner
            .ok_or((false, "no [discord] owner".to_string()))?;
        // A message inside a thread under a listed channel: Discord nests no threads, so the new
        // thread starts in that listed channel without a starter message.
        let listed = if config.discord.channels.contains(&room) {
            None
        } else {
            let channel = rest.channel(room).await.map_err(|e| (false, e.why))?;
            let thread = matches!(channel.get("type").and_then(Value::as_u64), Some(11 | 12));
            let parent = channel
                .get("parent_id")
                .and_then(Value::as_str)
                .and_then(Snowflake::parse)
                .filter(|p| thread && config.discord.channels.contains(p));
            Some(parent.ok_or((
                false,
                "the source message's channel is not in scope".to_string(),
            ))?)
        };
        // Proof of source (DISC-IN req. 59): the exact message, written by the owner.
        let source = rest
            .message(room, message)
            .await
            .map_err(|e| (false, e.why))?;
        let author = source
            .pointer("/author/id")
            .and_then(Value::as_str)
            .and_then(Snowflake::parse);
        let bot = source.pointer("/author/bot").and_then(Value::as_bool) == Some(true);
        if author != Some(owner) || bot {
            return Err((false, "the source is not an owner message".into()));
        }
        let (home, call, path, body) = match listed {
            None => (
                room,
                "POST channels/messages/threads",
                format!("/channels/{room}/messages/{message}/threads"),
                json!({ "name": title, "auto_archive_duration": 10080 }),
            ),
            Some(p) => (
                p,
                "POST channels/threads",
                format!("/channels/{p}/threads"),
                json!({ "name": title, "auto_archive_duration": 10080, "type": 11 }),
            ),
        };
        let created = rest
            .call(call, Method::POST, &path, Some(body), Kind::Once)
            .await
            .map_err(|e| (e.uncertain, e.why))?;
        let thread = created
            .get("id")
            .and_then(Value::as_str)
            .and_then(Snowflake::parse);
        let parent = created
            .get("parent_id")
            .and_then(Value::as_str)
            .and_then(Snowflake::parse);
        let Some(thread) = thread.filter(|_| parent == Some(home)) else {
            return Err((
                true,
                "Discord's answer named no thread under the source channel".into(),
            ));
        };
        if config.lanes.thread_member {
            let _ = rest
                .call(
                    "PUT channels/thread-members",
                    Method::PUT,
                    &format!("/channels/{thread}/thread-members/{owner}"),
                    None,
                    Kind::Idempotent,
                )
                .await;
        }
        Ok(thread.to_string())
    }
}

impl Threads for DiscordThreads {
    fn post<'a>(&'a self, thread: &'a str, text: &'a str, seed: &'a str) -> BoxFut<'a, Posted> {
        Box::pin(self.post_now(thread, text, seed))
    }

    fn status<'a>(
        &'a self,
        thread: &'a str,
        word: &'a str,
        archive: bool,
    ) -> BoxFut<'a, Result<(), String>> {
        Box::pin(self.status_now(thread, word, archive))
    }

    fn create<'a>(
        &'a self,
        room: &'a str,
        message: &'a str,
        title: &'a str,
    ) -> BoxFut<'a, Result<String, (bool, String)>> {
        Box::pin(self.create_now(room, message, title))
    }
}

/// The board post (EVR-30): a plain message created with a nonce, then edited in place.
impl crate::lanes::board::Board for DiscordThreads {
    fn board_create<'a>(
        &'a self,
        channel: &'a str,
        text: &'a str,
        nonce: &'a str,
    ) -> BoxFut<'a, Result<String, (bool, String)>> {
        Box::pin(async move {
            let (rest, _) = self.client().map_err(|e| (false, e))?;
            let channel = id(channel).map_err(|e| (false, e))?;
            let nonce: String = crate::config::sha256_hex(nonce.as_bytes())
                .chars()
                .take(25)
                .collect();
            let v = rest
                .create(channel, json!({ "content": text, "nonce": nonce }))
                .await
                .map_err(|e| (e.uncertain, e.why))?;
            v.get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or((true, "no message ID in Discord's answer".to_string()))
        })
    }

    fn board_edit<'a>(
        &'a self,
        channel: &'a str,
        message: &'a str,
        text: &'a str,
    ) -> BoxFut<'a, Result<(), String>> {
        Box::pin(async move {
            let (rest, _) = self.client()?;
            rest.edit(id(channel)?, id(message)?, json!({ "content": text }))
                .await
                .map_err(|e| e.why)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_words_replace_each_other_and_never_overflow() {
        assert_eq!(
            titled("Fix login", "working").as_deref(),
            Some("[working] Fix login")
        );
        assert_eq!(
            titled("[working] Fix login", "done").as_deref(),
            Some("[done] Fix login")
        );
        assert_eq!(
            titled("[done] [waiting] x", "working").as_deref(),
            Some("[working] x")
        );
        assert_eq!(titled("[done] ", "working"), None);
        assert_eq!(titled(&"x".repeat(95), "working"), None);
    }
}
