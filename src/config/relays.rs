//! `[telegram]`, `[slack]` and `[context]` (milestone 4): the Telegram bot's owner, group and topic
//! (TELE-01, TELE-04, TELE req. 31), the Slack account role and sender allowlist (SLACK-01, SLACK-08,
//! owner decision 10), and the optional channel facts and earlier-conversation lines for every relay
//! (DISC-IN req. 53-58). Tokens never appear here: each relay reads its own owner-only file.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::{ConfigError, Section};
use crate::relay::http::Base;

/// `[telegram]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TelegramSettings {
    /// Default `<config dir>/secrets/telegram-token`.
    pub(crate) token_file: Option<PathBuf>,
    /// Loopback-only test override of the Bot API base.
    pub(crate) api_base: Option<Base>,
    /// The owner's user ID; intake runs only with it.
    pub(crate) owner: Option<i64>,
    /// The one group or supergroup taken besides the owner's DM.
    pub(crate) group: Option<i64>,
    /// The one forum topic of that group (1 is General).
    pub(crate) topic: Option<i64>,
    /// Further chats sends may go to (TELE req. 31).
    pub(crate) chats: Vec<i64>,
    /// Set an empty bot name at start (MSG-11; the owner may turn it off).
    pub(crate) profile: bool,
}

impl Default for TelegramSettings {
    fn default() -> Self {
        Self {
            token_file: None,
            api_base: None,
            owner: None,
            group: None,
            topic: None,
            chats: Vec::new(),
            profile: true,
        }
    }
}

impl TelegramSettings {
    pub(crate) fn intake(&self) -> bool {
        self.owner.is_some()
    }
}

/// How the Slack account takes part (SLACK-01).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum SlackRole {
    /// The app's bot user: sees only conversations it is in.
    #[default]
    App,
    /// A member through the app's user token: sees what that member sees.
    Member,
}

/// `[slack]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SlackSettings {
    /// Default `<config dir>/secrets/slack-tokens`: `app=xapp-…` and `api=xoxb-…` or `api=xoxp-…`.
    pub(crate) token_file: Option<PathBuf>,
    pub(crate) api_base: Option<Base>,
    pub(crate) role: SlackRole,
    /// The owner's member ID; intake runs only with it.
    pub(crate) owner: Option<String>,
    /// Further members whose messages are admitted (owner decision 10: none by default).
    pub(crate) allow: Vec<String>,
}

impl SlackSettings {
    pub(crate) fn intake(&self) -> bool {
        self.owner.is_some()
    }

    /// The sender allowlist: the owner, then the listed members (SLACK-08).
    pub(crate) fn senders(&self) -> Vec<String> {
        self.owner.iter().chain(&self.allow).cloned().collect()
    }
}

/// `[context]`: extra lines for lead-bound requests, both off by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContextSettings {
    /// The channel facts line (DISC-IN-17).
    pub(crate) facts: bool,
    /// The owner's one-line notes, by `<relay>:<room>`.
    pub(crate) notes: BTreeMap<String, String>,
    /// The earlier-conversation lookup (DISC-IN-18).
    pub(crate) lookup: bool,
    /// Stored rows are kept this many days (1 to 3650).
    pub(crate) lookup_days: i64,
}

impl Default for ContextSettings {
    fn default() -> Self {
        Self {
            facts: false,
            notes: BTreeMap::new(),
            lookup: false,
            lookup_days: 30,
        }
    }
}

/// A Slack member, user or bot ID: an upper-case letter, then upper-case letters and digits.
pub(crate) fn slack_id(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && (2..=32).contains(&text.len())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// A Slack conversation ID: C, G or D, then upper-case letters and digits (SLACK req. 37).
pub(crate) fn slack_channel(text: &str) -> bool {
    text.starts_with(['C', 'G', 'D']) && slack_id(text)
}

/// An owner note: 1 to 500 characters after trimming, one line, no control, line-separator or
/// direction-override characters (DISC-IN req. 54).
pub(crate) fn valid_note(note: &str) -> bool {
    let trimmed = note.trim();
    (1..=500).contains(&trimmed.chars().count())
        && !trimmed.chars().any(|c| {
            c.is_control()
                || matches!(c, '\u{2028}' | '\u{2029}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
        })
}

fn token_file(errors: &mut Vec<ConfigError>, s: &mut Section<'_>) -> Option<PathBuf> {
    let file = s.string(errors, "token_file")?;
    if std::path::Path::new(file).is_absolute() {
        Some(PathBuf::from(file))
    } else {
        errors.push(s.error("token_file", "must be an absolute path"));
        None
    }
}

fn api_base(errors: &mut Vec<ConfigError>, s: &mut Section<'_>) -> Option<Base> {
    let base = s.string(errors, "api_base")?;
    match Base::parse(base) {
        Ok(b) if b.is_loopback() => Some(b),
        Ok(_) => {
            errors.push(s.error("api_base", "the test override must point at localhost"));
            None
        }
        Err(e) => {
            errors.push(s.error("api_base", &e));
            None
        }
    }
}

/// A Telegram ID given as a string or a number.
fn tg_id(errors: &mut Vec<ConfigError>, s: &mut Section<'_>, key: &str) -> Option<i64> {
    let value = s.get(key)?;
    let id = value
        .as_integer()
        .or_else(|| value.as_str().and_then(|t| t.trim().parse().ok()))
        .filter(|n| *n != 0 && n.unsigned_abs() < (1 << 53));
    if id.is_none() {
        errors.push(s.error(key, "expected a Telegram ID"));
    }
    id
}

/// `[telegram]`, `[slack]` and `[context]`.
pub(super) fn parse(
    errors: &mut Vec<ConfigError>,
    root: &mut Section<'_>,
    telegram: &mut TelegramSettings,
    slack: &mut SlackSettings,
    context: &mut ContextSettings,
) {
    if let Some(mut t) = root.table(errors, "telegram") {
        telegram.token_file = token_file(errors, &mut t);
        telegram.api_base = api_base(errors, &mut t);
        telegram.owner = tg_id(errors, &mut t, "owner");
        if telegram.owner.is_some_and(|o| o < 0) {
            errors.push(t.error("owner", "the owner is a user: a positive ID"));
        }
        telegram.group = tg_id(errors, &mut t, "group");
        if telegram.group.is_some_and(|g| g > 0) {
            errors.push(t.error("group", "a group's ID is negative"));
        }
        telegram.topic = tg_id(errors, &mut t, "topic");
        if telegram.topic.is_some_and(|n| n < 1) {
            errors.push(t.error("topic", "a topic is a positive number"));
        } else if telegram.topic.is_some() && telegram.group.is_none() {
            errors.push(t.error("topic", "a topic needs [telegram] group"));
        }
        if let Some(chats) = t.get("chats") {
            let list: Option<Vec<i64>> = chats.as_array().and_then(|a| {
                a.iter()
                    .map(|v| {
                        v.as_integer()
                            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    })
                    .collect()
            });
            match list {
                Some(l) if l.iter().all(|n| *n != 0) => telegram.chats = l,
                _ => errors.push(t.error("chats", "expected a list of Telegram chat IDs")),
            }
        }
        if let Some(p) = t.bool(errors, "profile") {
            telegram.profile = p;
        }
        t.finish(errors);
    }
    if let Some(mut s) = root.table(errors, "slack") {
        slack.token_file = token_file(errors, &mut s);
        slack.api_base = api_base(errors, &mut s);
        match s.string(errors, "role") {
            Some("app") | None => slack.role = SlackRole::App,
            Some("member") => slack.role = SlackRole::Member,
            Some(_) => errors.push(s.error("role", "expected \"app\" or \"member\"")),
        }
        if let Some(owner) = s.string(errors, "owner") {
            if owner.starts_with(['U', 'W']) && slack_id(owner) {
                slack.owner = Some(owner.to_string());
            } else {
                errors.push(s.error("owner", "expected a Slack member ID such as U0123ABCD"));
            }
        }
        if let Some(allow) = s.strings(errors, "allow") {
            if allow.iter().all(|a| slack_id(a)) {
                slack.allow = allow;
            } else {
                errors.push(s.error("allow", "expected Slack member IDs"));
            }
        }
        s.finish(errors);
    }
    if let Some(mut c) = root.table(errors, "context") {
        context.facts = c.bool(errors, "facts").unwrap_or(false);
        context.lookup = c.bool(errors, "lookup").unwrap_or(false);
        if let Some(days) = c.integer(errors, "lookup_days", 1, 3650) {
            context.lookup_days = days;
        }
        if let Some(mut notes) = c.table(errors, "notes") {
            for (room, value) in notes.entries() {
                match value.as_str() {
                    Some(note)
                        if valid_note(note)
                            && room.split_once(':').is_some_and(|(r, id)| {
                                matches!(r, "discord" | "telegram" | "slack") && !id.is_empty()
                            }) =>
                    {
                        context.notes.insert(room.clone(), note.trim().to_string());
                    }
                    _ => errors.push(notes.error(
                        room,
                        "expected \"<relay>:<room>\" = one line of 1 to 500 characters",
                    )),
                }
            }
            notes.finish(errors);
        }
        c.finish(errors);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_notes() {
        assert!(slack_id("U0123ABCD") && !slack_id("u01") && !slack_id("U"));
        assert!(slack_channel("C0123") && slack_channel("D9") && !slack_channel("U0123"));
        assert!(valid_note(" the release room ") && !valid_note("") && !valid_note("a\nb"));
        assert!(!valid_note("x\u{202E}y") && !valid_note(&"n".repeat(501)));
    }

    #[test]
    fn sections_parse_and_refuse() {
        let ok = crate::config::parse(
            "[telegram]\nowner = 42\ngroup = \"-1001\"\ntopic = 7\n[slack]\nrole = \"member\"\n\
             owner = \"U0OWNER\"\n[context]\nfacts = true\nlookup = true\nlookup_days = 5\n\
             [context.notes]\n\"telegram:42\" = \"my DM\"\n",
        )
        .unwrap();
        assert_eq!(ok.telegram.owner, Some(42));
        assert_eq!(ok.telegram.group, Some(-1001));
        assert_eq!(ok.slack.senders(), vec!["U0OWNER".to_string()]);
        assert_eq!(ok.slack.role, SlackRole::Member);
        assert!(ok.context.facts && ok.context.lookup && ok.context.lookup_days == 5);
        let bad = crate::config::parse(
            "[telegram]\nowner = -5\ntopic = 3\n[slack]\nowner = \"nobody\"\n[context]\nlookup_days = 0\n",
        )
        .unwrap_err();
        let paths: Vec<_> = bad.iter().map(|e| e.path.as_str()).collect();
        for p in [
            "telegram.owner",
            "telegram.topic",
            "slack.owner",
            "context.lookup_days",
        ] {
            assert!(paths.contains(&p), "{paths:?}");
        }
    }
}
