//! Discord REST client: no redirects, bounded bodies, one bounded 429 retry, enforced nonces,
//! reply-reference verification and deleted-source (10008) detection.

use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, Method, StatusCode, Url, header};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use super::discord_types::{BotProfile, Interaction, Message, Snowflake};

const UNKNOWN_MESSAGE: u64 = 10_008;
const PART_DEADLINE: Duration = Duration::from_secs(8);
const RESPONSE_LIMIT: usize = 2 * 1024 * 1024;

pub(crate) struct DiscordApi {
    client: Client,
    base: Url,
    authorization: header::HeaderValue,
}

#[derive(Deserialize)]
pub(crate) struct Receipt {
    pub(crate) id: Snowflake,
    channel_id: Snowflake,
    message_reference: Option<Reference>,
    #[serde(rename = "type")]
    kind: Option<u8>,
}

#[derive(Deserialize)]
struct Reference {
    message_id: Snowflake,
    channel_id: Option<Snowflake>,
}

impl Receipt {
    pub(crate) fn references(&self, channel: Snowflake, message: Snowflake) -> bool {
        self.message_reference.as_ref().is_some_and(|reference| {
            reference.message_id == message && reference.channel_id == Some(channel)
        })
    }

    pub(crate) fn confirms_reply(&self, channel: Snowflake, message: Snowflake) -> bool {
        self.kind == Some(19)
            && self.message_reference.as_ref().is_some_and(|reference| {
                reference.message_id == message
                    && reference.channel_id.is_none_or(|id| id == channel)
            })
    }
}

impl DiscordApi {
    pub(crate) fn new(base: Url, token: &str) -> Result<Self> {
        let mut authorization = header::HeaderValue::from_str(&format!("Bot {token}"))
            .context("invalid Discord bot token")?;
        authorization.set_sensitive(true);
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(8))
            .build()?;
        Ok(Self {
            client,
            base,
            authorization,
        })
    }

    pub(crate) async fn identity(&self) -> Result<BotProfile> {
        self.get("users/@me").await
    }

    pub(crate) async fn require_message_content(&self) -> Result<()> {
        #[derive(Deserialize)]
        struct Application {
            flags: u64,
        }
        let app: Application = self.get("applications/@me").await?;
        ensure!(
            (app.flags & ((1 << 18) | (1 << 19))) != 0,
            "Enable Discord Message Content Intent before starting native intake"
        );
        Ok(())
    }

    pub(crate) async fn history(
        &self,
        channel: Snowflake,
        before: Option<Snowflake>,
        limit: u8,
    ) -> Result<Vec<Message>> {
        let mut path = format!("channels/{channel}/messages?limit={limit}");
        if let Some(before) = before {
            use std::fmt::Write as _;
            write!(path, "&before={before}")?;
        }
        let page: Vec<Message> = self.get(&path).await?;
        ensure!(
            page.len() <= usize::from(limit)
                && page.iter().all(|message| message.channel_id == channel),
            "Discord history response did not match its channel or page limit"
        );
        Ok(page)
    }

    pub(crate) async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        serde_json::from_slice(&self.call(Method::GET, path, None, true).await?)
            .context("invalid Discord REST response")
    }

    pub(crate) async fn reply(
        &self,
        origin: Destination,
        content: &str,
        nonce: &str,
    ) -> Result<Snowflake> {
        let mut body = text_body(origin.channel, Some(origin.message), content);
        body["nonce"] = json!(nonce);
        body["enforce_nonce"] = json!(true);
        let receipt = self.post(origin.channel, &body).await?;
        ensure!(
            receipt.references(origin.channel, origin.message),
            "Discord reply destination did not match its source"
        );
        Ok(receipt.id)
    }

    pub(crate) async fn post_text(
        &self,
        channel: Snowflake,
        reference: Option<Snowflake>,
        content: &str,
    ) -> Result<Receipt> {
        self.post(channel, &text_body(channel, reference, content))
            .await
    }

    async fn post(&self, channel: Snowflake, body: &Value) -> Result<Receipt> {
        let path = format!("channels/{channel}/messages");
        let receipt: Receipt =
            serde_json::from_slice(&self.call(Method::POST, &path, Some(body), true).await?)
                .context("Discord did not confirm the posted message")?;
        ensure!(
            receipt.channel_id == channel,
            "Discord returned a message in another channel"
        );
        Ok(receipt)
    }

    pub(crate) async fn edit_text(
        &self,
        channel: Snowflake,
        message: Snowflake,
        content: &str,
    ) -> Result<()> {
        let path = format!("channels/{channel}/messages/{message}");
        let receipt: Receipt = serde_json::from_slice(
            &self
                .call(
                    Method::PATCH,
                    &path,
                    Some(&text_body(channel, None, content)),
                    true,
                )
                .await?,
        )
        .context("Discord did not confirm the edited message")?;
        ensure!(
            receipt.channel_id == channel && receipt.id == message,
            "Discord returned another edited message"
        );
        Ok(())
    }

    pub(crate) async fn clear(&self, channel: Snowflake, message: Snowflake) -> Result<()> {
        self.call(
            Method::PATCH,
            &format!("channels/{channel}/messages/{message}"),
            Some(&json!({"components": []})),
            true,
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn callback(&self, event: &Interaction, accepted: bool) -> Result<()> {
        let path = format!("interactions/{}/{}/callback", event.id, event.token);
        let body = if accepted {
            json!({"type": 7, "data": {"components": []}})
        } else {
            json!({"type": 4, "data": {"content": "This approval is inactive or not yours.", "flags": 64}})
        };
        // The callback uses its own token, not the bot authorization header.
        tokio::time::timeout(
            Duration::from_millis(2_500),
            self.call(Method::POST, &path, Some(&body), false),
        )
        .await??;
        Ok(())
    }

    /// One request; a non-success status is returned as a `RestStatus` error after one bounded 429 retry.
    pub(crate) async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        auth: bool,
    ) -> Result<Vec<u8>> {
        let url = self.base.join(path)?;
        for trial in 0..2 {
            let mut request = self.client.request(method.clone(), url.clone());
            if auth {
                request = request.header(header::AUTHORIZATION, self.authorization.clone());
            }
            if let Some(body) = body {
                request = request.json(body);
            }
            let mut response = request
                .send()
                .await
                .map_err(reqwest::Error::without_url)
                .context("Discord REST I/O failed")?;
            let status = response.status();
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(reqwest::Error::without_url)?
            {
                ensure!(
                    bytes.len() + chunk.len() <= RESPONSE_LIMIT,
                    "Discord REST response exceeded its limit"
                );
                bytes.extend_from_slice(&chunk);
            }
            if status.is_success() {
                return Ok(bytes);
            }
            if status == StatusCode::TOO_MANY_REQUESTS && trial == 0 && auth {
                #[derive(Deserialize)]
                struct RateLimit {
                    retry_after: f64,
                }
                if let Ok(rate) = serde_json::from_slice::<RateLimit>(&bytes)
                    && (0.0..=5.0).contains(&rate.retry_after)
                {
                    tokio::time::sleep(Duration::from_secs_f64(rate.retry_after)).await;
                    continue;
                }
            }
            return Err(RestStatus {
                status: status.as_u16(),
                code: error_code(&bytes),
            }
            .into());
        }
        bail!("Discord REST request did not complete")
    }

    /// Adds the bot's 👀 reaction to an owner message.
    pub(crate) async fn react_seen(
        &self,
        channel: Snowflake,
        message: Snowflake,
    ) -> Result<()> {
        self.call(
            Method::PUT,
            &format!("channels/{channel}/messages/{message}/reactions/%F0%9F%91%80/@me"),
            None,
            true,
        )
        .await?;
        Ok(())
    }

    /// The raw channel object; a failed status surfaces as a `RestStatus` error.
    pub(crate) async fn channel_info(&self, channel: Snowflake) -> Result<Value> {
        let bytes = self
            .call(Method::GET, &format!("channels/{channel}"), None, true)
            .await?;
        serde_json::from_slice(&bytes).context("invalid Discord REST response")
    }

    /// Opens (or returns) the bot's DM channel with `recipient`.
    pub(crate) async fn dm_channel(&self, recipient: Snowflake) -> Result<Value> {
        let bytes = self
            .call(
                Method::POST,
                "users/@me/channels",
                Some(&json!({ "recipient_id": recipient.to_string() })),
                true,
            )
            .await?;
        serde_json::from_slice(&bytes).context("invalid Discord REST response")
    }

    /// Posts one reply part with an enforced nonce. A deleted source (404/10008, or 400/404 whose source GET is
    /// 404/10008) is `SourceMissing`; every other failure is the POST error. The POST and its check share 8 s.
    pub(crate) async fn post_part(
        &self,
        channel: Snowflake,
        reference: Snowflake,
        text: &str,
        nonce: &str,
    ) -> Result<PartPost> {
        tokio::time::timeout(
            PART_DEADLINE,
            self.post_part_now(channel, reference, text, nonce),
        )
        .await
        .context("Discord reply part timed out")?
    }

    async fn post_part_now(
        &self,
        channel: Snowflake,
        reference: Snowflake,
        text: &str,
        nonce: &str,
    ) -> Result<PartPost> {
        #[derive(Deserialize)]
        struct Reply {
            id: Option<Value>,
            channel_id: Option<Value>,
        }
        let messages = format!("channels/{channel}/messages");
        let body = part_body(reference, text, nonce);
        match self.call(Method::POST, &messages, Some(&body), true).await {
            Ok(bytes) => {
                let reply: Reply = serde_json::from_slice(&bytes)
                    .context("Discord did not confirm the reply part destination")?;
                let id = match reply.id {
                    Some(Value::String(id)) => Snowflake::try_from(id).ok(),
                    _ => None,
                };
                let same = matches!(&reply.channel_id, Some(Value::String(at)) if *at == channel.to_string());
                match id {
                    Some(id) if same => Ok(PartPost::Sent(id)),
                    _ => anyhow::bail!("Discord did not confirm the reply part destination"),
                }
            }
            Err(error) => {
                let Some(status) = error.downcast_ref::<RestStatus>() else {
                    return Err(error);
                };
                if status.status != 400 && status.status != 404 {
                    return Err(error);
                }
                if status.unknown_message() {
                    return Ok(PartPost::SourceMissing);
                }
                match self
                    .call(Method::GET, &format!("{messages}/{reference}"), None, true)
                    .await
                {
                    Err(check)
                        if check
                            .downcast_ref::<RestStatus>()
                            .is_some_and(RestStatus::unknown_message) =>
                    {
                        Ok(PartPost::SourceMissing)
                    }
                    _ => Err(error),
                }
            }
        }
    }
}

fn text_body(channel: Snowflake, reference: Option<Snowflake>, content: &str) -> Value {
    let mut body =
        json!({ "content": content, "allowed_mentions": { "parse": [], "replied_user": false } });
    if let Some(message) = reference {
        body["message_reference"] =
            json!({ "message_id": message, "channel_id": channel, "fail_if_not_exists": true });
    }
    body
}

#[derive(Clone, Copy, Serialize)]
pub struct Destination {
    pub(crate) channel: Snowflake,
    pub(crate) message: Snowflake,
}

/// A non-success Discord REST status with the JSON error code when the body carried one.
#[derive(Debug)]
pub(crate) struct RestStatus {
    pub(crate) status: u16,
    pub(crate) code: Option<u64>,
}

impl std::fmt::Display for RestStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Discord REST returned HTTP {}", self.status)
    }
}

impl std::error::Error for RestStatus {}

impl RestStatus {
    fn unknown_message(&self) -> bool {
        self.status == 404 && self.code == Some(UNKNOWN_MESSAGE)
    }
}

/// The outcome of posting one reply part.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PartPost {
    Sent(Snowflake),
    /// The referenced owner message no longer exists (Discord 404 code 10008).
    SourceMissing,
}

fn error_code(bytes: &[u8]) -> Option<u64> {
    #[derive(Deserialize)]
    struct Body {
        code: Value,
    }
    let body: Body = serde_json::from_slice(bytes).ok()?;
    match body.code {
        Value::Number(code) => code.as_u64(),
        Value::String(code) => code.parse().ok(),
        _ => None,
    }
}

fn part_body(reference: Snowflake, text: &str, nonce: &str) -> Value {
    json!({
        "content": text,
        "allowed_mentions": { "parse": [], "replied_user": false },
        "nonce": nonce,
        "enforce_nonce": true,
        "message_reference": { "message_id": reference.to_string(), "fail_if_not_exists": true },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_message_needs_404_and_10008() {
        assert_eq!(error_code(br#"{"code":10008,"message":"x"}"#), Some(10_008));
        assert_eq!(error_code(br#"{"code":"10008"}"#), Some(10_008));
        assert_eq!(error_code(b"nope"), None);
        assert!(
            RestStatus {
                status: 404,
                code: Some(10_008)
            }
            .unknown_message()
        );
        assert!(
            !RestStatus {
                status: 400,
                code: Some(10_008)
            }
            .unknown_message()
        );
        assert!(
            !RestStatus {
                status: 404,
                code: Some(10_003)
            }
            .unknown_message()
        );
    }
}
