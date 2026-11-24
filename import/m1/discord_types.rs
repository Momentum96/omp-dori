//! Discord ID newtype, message origin and the serde types read from REST and the Gateway.

use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct Snowflake(NonZeroU64);

impl TryFrom<String> for Snowflake {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let id = value
            .parse::<NonZeroU64>()
            .map_err(|_| "invalid Discord ID")?;
        if id.to_string() != value {
            return Err("invalid Discord ID");
        }
        Ok(Self(id))
    }
}

impl From<Snowflake> for String {
    fn from(value: Snowflake) -> Self {
        value.0.to_string()
    }
}
impl std::fmt::Display for Snowflake {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}
impl Snowflake {
    pub(crate) const FLOOR: Self = Self(NonZeroU64::MIN);

    pub(crate) fn preceding(self) -> Option<Self> {
        NonZeroU64::new(self.0.get() - 1).map(Self)
    }

    pub(crate) const fn timestamp_ms(self) -> u64 {
        (self.0.get() >> 22) + 1_420_070_400_000
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Origin {
    pub(crate) bot: Snowflake,
    pub(crate) owner: Snowflake,
    pub(crate) channel: Snowflake,
    pub(crate) message: Snowflake,
}

#[derive(Deserialize)]
pub(crate) struct User {
    id: Snowflake,
    #[serde(default)]
    bot: bool,
    #[serde(default)]
    system: bool,
}

#[derive(Deserialize)]
pub(crate) struct BotProfile {
    #[serde(flatten)]
    user: User,
    avatar: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct Message {
    id: Snowflake,
    channel_id: Snowflake,
    #[serde(rename = "type")]
    kind: u8,
    author: User,
    webhook_id: Option<Snowflake>,
    content: Option<String>,
    #[serde(rename = "message_reference")]
    reference: Option<MessageReference>,
    #[serde(
        rename = "referenced_message",
        default,
        deserialize_with = "field_present"
    )]
    has_reference_snapshot: bool,
    #[serde(
        rename = "message_snapshots",
        default,
        deserialize_with = "field_present"
    )]
    forwarded: bool,
    #[serde(default)]
    attachments: Vec<serde::de::IgnoredAny>,
}

fn field_present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    serde::de::IgnoredAny::deserialize(deserializer)?;
    Ok(true)
}

#[derive(Clone, Copy, Deserialize)]
pub(crate) struct MessageReference {
    #[serde(rename = "type", default)]
    kind: u8,
    message_id: Option<Snowflake>,
    channel_id: Option<Snowflake>,
}

#[derive(Deserialize)]
pub struct Interaction {
    id: Snowflake,
    token: String,
    application_id: Snowflake,
    channel_id: Snowflake,
    #[serde(rename = "type")]
    kind: u8,
    data: Component,
    message: InteractionMessage,
    user: Option<User>,
    member: Option<Member>,
}

#[derive(Deserialize)]
struct Component {
    component_type: u8,
    custom_id: String,
}

#[derive(Deserialize)]
struct InteractionMessage {
    id: Snowflake,
}

#[derive(Deserialize)]
struct Member {
    user: User,
}

impl Interaction {
    pub(crate) fn decision(
        &self,
        button: &ApprovalCard,
    ) -> Option<Decision> {
        let user = self
            .member
            .as_ref()
            .map(|member| &member.user)
            .or(self.user.as_ref())?;
        if user.id != button.origin.owner
            || user.bot
            || user.system
            || self.application_id != button.origin.bot
            || self.channel_id != button.origin.channel
            || self.message.id != button.message
        {
            return None;
        }
        let key = self.data.custom_id.strip_prefix(CUSTOM_ID_PREFIX)?;
        let (stamp, decision) = key.rsplit_once(':')?;
        if stamp != button.stamp {
            return None;
        }
        match decision {
            "approve" => Some(Decision::Approve),
            "deny" => Some(Decision::Deny),
            _ => None,
        }
    }

    fn valid(&self) -> bool {
        self.kind == 3
            && self.data.component_type == 2
            && self.data.custom_id.starts_with(CUSTOM_ID_PREFIX)
            && self.token.len() <= 512
            && !self.token.is_empty()
            && self
                .token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    }
}
