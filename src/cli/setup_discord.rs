//! The Discord part of `cataly setup`. Discord leaves three steps to the owner, because they need
//! his own Discord login: making the application, copying its bot token, and adding the bot to a
//! server through the invite link. Everything else runs here on the bot's token: the token is
//! checked (`GET /users/@me`); the Message Content intent is read from the application and switched
//! on (`PATCH /applications/@me` may set the intent flag of a bot in fewer than 100 servers); the
//! invite link carries exactly the permissions Cataly uses; the servers the bot is in are read until
//! it joins one; text channels named `cataly` and `cataly-ops` are found, and missing ones are
//! created only once the owner said yes to the setup; no other channel is touched. The owner is the
//! application's owner unless `--owner` or the config names someone. Every fact is read again on
//! each run and a setting is written only when it differs, so a second run changes nothing.
//!
//! At a terminal the token is asked for without echo; a script saves it in the token file. A step
//! that still needs the owner ends the run with exit status 1 and names that step.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use super::args::Parsed;
use super::out;
use super::setup::Terminal;
use crate::config::file;
use crate::config::{self, Config, Destination, sha256_hex};
use crate::relay::discord::{API, ChannelRef, Rest, Snowflake};
use crate::relay::http::Base;
use crate::sys::paths::Paths;

const PORTAL: &str = "https://discord.com/developers/applications";
const INVITE: &str = "https://discord.com/oauth2/authorize";
/// The Message Content intent: the flag for bots in 100 or more servers (granted after review),
/// and the one for bots in fewer.
const MESSAGE_CONTENT: u64 = 1 << 18;
const MESSAGE_CONTENT_LIMITED: u64 = 1 << 19;
/// The application flags a bot may set itself: the presence, members and message content intents
/// of a bot in fewer than 100 servers.
const LIMITED_INTENTS: u64 = (1 << 13) | (1 << 15) | MESSAGE_CONTENT_LIMITED;
/// What Cataly does in its server: manage channels (setup creates its two), add reactions, view
/// channels, send messages, read message history, connect and speak in a voice channel (voice
/// calls), manage threads, create public threads, send messages in threads, pin messages (the ops
/// panel pins its message; it works unpinned without).
const PERMISSIONS: u64 = (1 << 4)
    | (1 << 6)
    | VIEW_CHANNEL
    | (1 << 11)
    | (1 << 16)
    | CONNECT
    | SPEAK
    | (1 << 34)
    | (1 << 35)
    | (1 << 38)
    | (1 << 51);
const ADMINISTRATOR: u64 = 1 << 3;
const VIEW_CHANNEL: u64 = 1 << 10;
const CONNECT: u64 = 1 << 20;
const SPEAK: u64 = 1 << 21;
/// What the bot needs in the voice channel, with Discord's names.
const VOICE_NEEDS: [(u64, &str); 3] = [
    (VIEW_CHANNEL, "View Channel"),
    (CONNECT, "Connect"),
    (SPEAK, "Speak"),
];
/// The lead's channel and the job notices' channel (also the name of the destination for it).
pub(super) const LEAD: &str = "cataly";
pub(super) const OPS: &str = "cataly-ops";
/// How often the server list is read while waiting for the bot to join.
const POLL: Duration = Duration::from_secs(3);
/// How long a run at a terminal waits for the bot to join, unless `--wait` says otherwise.
const JOIN_WAIT: Duration = Duration::from_mins(10);
/// The longest `--wait`.
const WAIT_MAX: Duration = Duration::from_hours(1);
/// Tries at one step before the run stops and asks for a new run.
const TRIES: usize = 5;

/// What the run found, for the report.
#[derive(Default)]
pub(super) struct Found {
    stage: &'static str,
    token_file: String,
    bot: Option<Value>,
    application: Option<Snowflake>,
    message_content: Option<&'static str>,
    invite: Option<String>,
    guilds: Vec<Value>,
    guild: Option<Value>,
    channels: Vec<Value>,
    owner: Option<Value>,
}

impl Found {
    pub(super) fn report(&self, next: Option<&str>) -> Value {
        json!({
            "stage": self.stage,
            "next": next,
            "token_file": self.token_file,
            "bot": self.bot,
            "application": self.application.map(|a| a.to_string()),
            "message_content": self.message_content,
            "invite": self.invite,
            "guilds": self.guilds,
            "guild": self.guild,
            "channels": self.channels,
            "owner": self.owner,
        })
    }
}

/// The owner as `--owner` names him.
#[derive(Clone, Copy)]
enum OwnerFlag {
    Id(Snowflake),
    Application,
}

/// The `--guild`, `--owner` and `--wait` flags, checked.
pub(super) struct Flags {
    guild: Option<Snowflake>,
    owner: Option<OwnerFlag>,
    wait: Option<Duration>,
}

impl Flags {
    pub(super) fn parse(p: &Parsed) -> Result<Self, String> {
        let wait = p
            .value("--wait")
            .map(|w| {
                config::parse_duration(w)
                    .filter(|d| *d <= WAIT_MAX)
                    .ok_or("--wait: expected a duration up to 1h, such as 5m")
            })
            .transpose()?;
        let guild = p
            .value("--guild")
            .map(|g| Snowflake::parse(g).ok_or("--guild: expected the server's Discord ID"))
            .transpose()?;
        let owner = p
            .value("--owner")
            .map(|o| match o {
                "app" => Ok(OwnerFlag::Application),
                id => Snowflake::parse(id)
                    .map(OwnerFlag::Id)
                    .ok_or("--owner: expected your Discord user ID, or app"),
            })
            .transpose()?;
        Ok(Self { guild, owner, wait })
    }
}

/// Discord once every step that needs the owner is done: what the config gets.
pub(super) struct Discord {
    rest: Rest,
    pub(super) found: Found,
    pub(super) bot: String,
    /// The bot account as Discord reported it (its avatar and banner hashes among it).
    me: Value,
    /// The application as Discord reported it (its icon hash among it).
    app: Value,
    pub(super) guild: Snowflake,
    pub(super) server: String,
    /// The two channels; `None` until created.
    pub(super) lead: Option<Snowflake>,
    pub(super) ops: Option<Snowflake>,
    pub(super) owner: Snowflake,
    pub(super) owner_source: &'static str,
}

/// Why the run stopped: the report so far and the step that still needs the owner.
pub(super) type Stop = (Value, String);

struct Flow<'a> {
    paths: &'a Paths,
    config: &'a Config,
    term: Option<&'a Terminal>,
    as_json: bool,
    flags: &'a Flags,
    found: Found,
}

/// Runs the Discord steps against `config` (the config file as it is now).
pub(super) async fn discover(
    paths: &Paths,
    config: &Config,
    term: Option<&Terminal>,
    as_json: bool,
    flags: &Flags,
) -> Result<Discord, Stop> {
    let mut flow = Flow {
        paths,
        config,
        term,
        as_json,
        flags,
        found: Found::default(),
    };
    match flow.steps().await {
        Ok((rest, (app, me), (guild, server), (lead, ops), (owner, owner_source))) => {
            flow.found.stage = "ready";
            Ok(Discord {
                rest,
                found: flow.found,
                bot: me
                    .get("username")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string(),
                me,
                app,
                guild,
                server,
                lead,
                ops,
                owner,
                owner_source,
            })
        }
        Err(next) => Err((flow.found.report(Some(&next)), next)),
    }
}

type Steps = (
    Rest,
    // The application and the bot account.
    (Value, Value),
    (Snowflake, String),
    (Option<Snowflake>, Option<Snowflake>),
    (Snowflake, &'static str),
);

impl Flow<'_> {
    async fn steps(&mut self) -> Result<Steps, String> {
        let rest = self.token().await?;
        let (app, me) = self.intent(&rest).await?;
        let server = self.server(&rest).await?;
        let channels = self.channels(&rest, server.0).await?;
        let owner = self.owner_id(&app)?;
        Ok((rest, (app, me), server, channels, owner))
    }

    fn say(&self, line: &str) {
        if !self.as_json {
            out::line(line);
        }
    }

    fn stop(&mut self, stage: &'static str, next: String) -> String {
        self.found.stage = stage;
        next
    }

    /// A REST client on a token Discord accepts as a bot's.
    async fn token(&mut self) -> Result<Rest, String> {
        let path = crate::jobs::owner::health::token_path(self.config, &self.paths.config_file);
        self.found.token_file = path.display().to_string();
        let base = match &self.config.discord.api_base {
            Some(b) => b.clone(),
            None => Base::parse(API).map_err(|e| self.stop("token", e))?,
        };
        let mut told = false;
        for _ in 0..TRIES {
            let stored = crate::sys::fs::read_private(&path, 64 * 1024);
            let token = match stored {
                Ok(Some(bytes)) => {
                    let Ok(text) = String::from_utf8(bytes) else {
                        let why = format!("{} is not text", path.display());
                        return Err(self.stop("token", why));
                    };
                    text.trim().to_string()
                }
                Ok(None) => String::new(),
                Err(e) => return Err(self.stop("token", format!("token file: {e}"))),
            };
            if !token.is_empty() {
                match bot(&base, &token).await {
                    Ok(rest) => return Ok(rest),
                    Err((true, why)) => {
                        let line = format!(
                            "Discord refused the token in {} ({why}). In the Developer Portal: \
                             Bot, Reset Token, then copy the new one.",
                            path.display()
                        );
                        if self.term.is_none() {
                            return Err(self.stop("token", line));
                        }
                        self.say(&line);
                    }
                    Err((false, why)) => {
                        return Err(self.stop("token", format!("cannot reach Discord: {why}")));
                    }
                }
            } else if !told {
                told = true;
                for line in portal_steps(&path, self.term.is_some()) {
                    self.say(&line);
                }
            }
            let Some(term) = self.term else {
                let next = format!(
                    "make the bot in the Developer Portal ({PORTAL}) and save its token in {} (chmod 600), then run cataly setup again",
                    path.display()
                );
                return Err(self.stop("token", next));
            };
            let Some(typed) = term.hidden("Bot token (not shown):") else {
                return Err(self.stop(
                    "token",
                    format!(
                        "no token given; save it in {} and run cataly setup again",
                        path.display()
                    ),
                ));
            };
            if typed.is_empty() {
                continue;
            }
            match bot(&base, &typed).await {
                Ok(rest) => {
                    store_token(&path, &typed).map_err(|e| self.stop("token", e))?;
                    self.say(&format!("token saved in {}", path.display()));
                    return Ok(rest);
                }
                Err((true, why)) => self.say(&format!("Discord refused that token ({why})")),
                Err((false, why)) => {
                    return Err(self.stop("token", format!("cannot reach Discord: {why}")));
                }
            }
        }
        Err(self.stop(
            "token",
            "no token Discord accepts yet; run cataly setup again".into(),
        ))
    }

    /// The application, with its Message Content intent switched on, and the bot's account.
    async fn intent(&mut self, rest: &Rest) -> Result<(Value, Value), String> {
        let me = rest
            .me()
            .await
            .map_err(|e| self.stop("token", format!("reading the bot: {e}")))?;
        let bot_id = me.get("id").and_then(Value::as_str).unwrap_or_default();
        let bot_name = me
            .get("username")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        self.found.bot = Some(json!({ "id": bot_id, "username": bot_name }));
        let mut app = self.application(rest).await?;
        if intent_on(&app) {
            self.found.message_content = Some("on");
            return Ok((app, me));
        }
        // Cataly cannot read the owner's messages without it: switch it on, no question.
        let flags = app.get("flags").and_then(Value::as_u64).unwrap_or(0);
        match rest
            .set_application_flags((flags & LIMITED_INTENTS) | MESSAGE_CONTENT_LIMITED)
            .await
        {
            Ok(_) => app = self.application(rest).await?,
            Err(e) => self.say(&format!(
                "Discord did not take the Message Content intent ({e})"
            )),
        }
        if intent_on(&app) {
            self.found.message_content = Some("switched on");
            self.say("Message Content intent: switched on");
            return Ok((app, me));
        }
        self.found.message_content = Some("off");
        let place = format!(
            "turn on the Message Content intent: Developer Portal ({PORTAL}), your application, Bot, \
             Privileged Gateway Intents, Message Content Intent, Save Changes"
        );
        for _ in 0..TRIES {
            let Some(term) = self.term else { break };
            self.say(&format!("Please {place}."));
            if term.ask("Press Enter once it is on:").is_none() {
                break;
            }
            app = self.application(rest).await?;
            if intent_on(&app) {
                self.found.message_content = Some("on");
                return Ok((app, me));
            }
        }
        Err(self.stop("intent", format!("{place}; then run cataly setup again")))
    }

    async fn application(&mut self, rest: &Rest) -> Result<Value, String> {
        let app = rest
            .application()
            .await
            .map_err(|e| self.stop("intent", format!("reading the application: {e}")))?;
        self.found.application = app
            .get("id")
            .and_then(Value::as_str)
            .and_then(Snowflake::parse);
        Ok(app)
    }

    /// The invite link: the bot scope with Cataly's permissions.
    fn invite(&mut self) -> String {
        let link = invite_link(self.found.application);
        self.found.invite = Some(link.clone());
        link
    }

    /// The server the bot is in, after the owner added it through the invite link.
    async fn server(&mut self, rest: &Rest) -> Result<(Snowflake, String), String> {
        let wanted = self.flags.guild.or(self.config.discord.guild);
        let mut guilds = self.guild_list(rest).await?;
        let present = |list: &[(Snowflake, String)]| match wanted {
            Some(w) => list.iter().any(|(id, _)| *id == w),
            None => !list.is_empty(),
        };
        let link = self.invite();
        if !present(&guilds) {
            self.say(&format!(
                "Add the bot to your server: open {link}, pick the server, then Authorize."
            ));
            self.say(
                "The link asks for Pin Messages so the ops panel can pin its message; without it the panel still works, unpinned.",
            );
            let wait = self.flags.wait.unwrap_or(if self.term.is_some() {
                JOIN_WAIT
            } else {
                Duration::ZERO
            });
            if !wait.is_zero() {
                let secs = wait.as_secs();
                let span = if secs < 120 {
                    format!("{secs} s")
                } else {
                    format!("{} min", secs.div_ceil(60))
                };
                self.say(&format!(
                    "Waiting up to {span} for the bot to join (Ctrl-C stops; run setup again later)."
                ));
            }
            let deadline = tokio::time::Instant::now() + wait;
            while !present(&guilds) && tokio::time::Instant::now() + POLL <= deadline {
                tokio::time::sleep(POLL).await;
                guilds = self.guild_list(rest).await?;
            }
            if !present(&guilds) {
                let what = wanted.map_or_else(
                    || "the bot is in no server yet".to_string(),
                    |w| format!("the bot is not in server {w}"),
                );
                return Err(self.stop(
                    "guild",
                    format!("{what}: open {link}, add it, then run cataly setup again"),
                ));
            }
        }
        let chosen = match (wanted, guilds.as_slice()) {
            (Some(w), list) => list.iter().find(|(id, _)| *id == w).cloned(),
            (None, [one]) => Some(one.clone()),
            (None, list) => self.pick(list),
        };
        let Some((id, name)) = chosen else {
            let names: Vec<String> = guilds.iter().map(|(id, n)| format!("{n} ({id})")).collect();
            return Err(self.stop(
                "guild",
                format!(
                    "the bot is in {} servers: {}; name yours with --guild <id>",
                    guilds.len(),
                    names.join(", ")
                ),
            ));
        };
        self.found.guild = Some(json!({ "id": id.to_string(), "name": name }));
        Ok((id, name))
    }

    async fn guild_list(&mut self, rest: &Rest) -> Result<Vec<(Snowflake, String)>, String> {
        let list = rest
            .guilds()
            .await
            .map_err(|e| self.stop("guild", format!("reading the bot's servers: {e}")))?;
        let guilds: Vec<(Snowflake, String)> = list
            .iter()
            .filter_map(|g| {
                let id = g
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(Snowflake::parse)?;
                let name = g.get("name").and_then(Value::as_str).unwrap_or("?");
                Some((id, name.to_string()))
            })
            .collect();
        self.found.guilds = guilds
            .iter()
            .map(|(id, name)| json!({ "id": id.to_string(), "name": name }))
            .collect();
        Ok(guilds)
    }

    /// The owner's choice among several servers, at the terminal.
    fn pick(&self, list: &[(Snowflake, String)]) -> Option<(Snowflake, String)> {
        let term = self.term?;
        self.say("The bot is in several servers:");
        for (i, (id, name)) in list.iter().enumerate() {
            self.say(&format!("  {}. {name} ({id})", i + 1));
        }
        for _ in 0..TRIES {
            let answer = term.ask(&format!("Which one is yours? [1-{}]", list.len()))?;
            if let Some(g) = answer
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .and_then(|i| list.get(i))
            {
                return Some(g.clone());
            }
        }
        None
    }

    /// `#cataly` and `#cataly-ops`: the text channels with those names; `None` for one to create.
    async fn channels(
        &mut self,
        rest: &Rest,
        guild: Snowflake,
    ) -> Result<(Option<Snowflake>, Option<Snowflake>), String> {
        let all = rest
            .guild_channels(guild)
            .await
            .map_err(|e| self.stop("channels", format!("reading the server's channels: {e}")))?;
        let mut ids = Vec::new();
        for name in [LEAD, OPS] {
            let same: Vec<Snowflake> = all
                .iter()
                .filter(|c| {
                    c.get("type").and_then(Value::as_u64) == Some(0)
                        && c.get("name").and_then(Value::as_str) == Some(name)
                })
                .filter_map(|c| {
                    c.get("id")
                        .and_then(Value::as_str)
                        .and_then(Snowflake::parse)
                })
                .collect();
            match same.as_slice() {
                [one] => {
                    self.found
                        .channels
                        .push(json!({ "name": name, "id": one.to_string(), "state": "existing" }));
                    ids.push(Some(*one));
                }
                [] => {
                    self.found
                        .channels
                        .push(json!({ "name": name, "id": null, "state": "missing" }));
                    ids.push(None);
                }
                more => {
                    return Err(self.stop(
                        "channels",
                        format!(
                            "{} text channels are named #{name}; rename all but one, then run setup again",
                            more.len()
                        ),
                    ));
                }
            }
        }
        Ok((
            ids.first().copied().flatten(),
            ids.get(1).copied().flatten(),
        ))
    }

    /// The owner's user ID: `--owner`, else the config's, else the application's owner, else
    /// typed in at the terminal.
    fn owner_id(&mut self, app: &Value) -> Result<(Snowflake, &'static str), String> {
        let candidate = app_owner(app);
        let (id, source) = match (self.flags.owner, self.config.discord.owner, &candidate) {
            (Some(OwnerFlag::Id(id)), _, _) => (id, "--owner"),
            (None, Some(id), _) => (id, "config"),
            (_, _, Some((id, _))) => (*id, "application owner"),
            (Some(OwnerFlag::Application), _, None) => {
                return Err(self.stop(
                    "owner",
                    "the application names no owner; pass --owner <your Discord user ID>".into(),
                ));
            }
            (None, None, None) => self.ask_owner()?,
        };
        let bot = self
            .found
            .bot
            .as_ref()
            .and_then(|b| b.get("id"))
            .and_then(Value::as_str)
            .and_then(Snowflake::parse);
        if bot == Some(id) {
            return Err(self.stop("owner", "the owner cannot be the bot itself".into()));
        }
        self.found.owner = Some(json!({ "id": id.to_string(), "source": source }));
        Ok((id, source))
    }

    fn ask_owner(&mut self) -> Result<(Snowflake, &'static str), String> {
        if let Some(term) = self.term {
            for _ in 0..TRIES {
                let Some(answer) = term.ask(
                    "Your Discord user ID (Discord settings, Advanced, Developer Mode on; then right-click your name, Copy User ID):",
                ) else {
                    break;
                };
                if let Some(id) = Snowflake::parse(&answer) {
                    return Ok((id, "entered"));
                }
            }
        }
        Err(self.stop(
            "owner",
            "the application names no owner; pass --owner <your Discord user ID>".into(),
        ))
    }
}

impl Discord {
    /// Creates the channels that are missing (after the owner's yes). No other channel is changed.
    pub(super) async fn create_missing(&mut self) -> Result<(Snowflake, Snowflake), String> {
        if self.lead.is_none() {
            self.lead = Some(self.create_one(LEAD).await?);
        }
        if self.ops.is_none() {
            self.ops = Some(self.create_one(OPS).await?);
        }
        match (self.lead, self.ops) {
            (Some(lead), Some(ops)) => Ok((lead, ops)),
            _ => Err("the channels could not be settled".into()),
        }
    }

    async fn create_one(&mut self, name: &'static str) -> Result<Snowflake, String> {
        let topic = if name == LEAD {
            "Talk with Cataly's lead here."
        } else {
            "Cataly's status panel and alerts."
        };
        match self.rest.create_text_channel(self.guild, name, topic).await {
            Ok(v) => {
                let id = v
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(Snowflake::parse)
                    .filter(|_| v.get("name").and_then(Value::as_str) == Some(name))
                    .ok_or_else(|| {
                        format!("Discord's answer named no #{name}; run setup again, it finds the channel if it was made")
                    })?;
                for c in &mut self.found.channels {
                    if c.get("name").and_then(Value::as_str) == Some(name) {
                        *c = json!({ "name": name, "id": id.to_string(), "state": "created" });
                    }
                }
                Ok(id)
            }
            Err(e) if e.status == Some(403) => Err(format!(
                "the bot may not create channels ({e}): give it Manage Channels by opening {}, \
                 or create #{name} yourself, then run setup again",
                self.found.invite.as_deref().unwrap_or(INVITE)
            )),
            Err(e) if e.uncertain => Err(format!(
                "Discord did not confirm #{name} ({e}); run setup again, it finds the channel if it was made"
            )),
            Err(e) => Err(format!("creating #{name}: {e}")),
        }
    }

    /// The server's voice channels, as (ID, name), read now.
    pub(super) async fn voice_channels(&self) -> Result<Vec<(Snowflake, String)>, String> {
        let all = self
            .rest
            .guild_channels(self.guild)
            .await
            .map_err(|e| format!("reading the server's channels: {e}"))?;
        Ok(all
            .iter()
            .filter(|c| c.get("type").and_then(Value::as_u64) == Some(2))
            .filter_map(|c| {
                let id = c
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(Snowflake::parse)?;
                let name = c.get("name").and_then(Value::as_str).unwrap_or_default();
                Some((id, name.to_string()))
            })
            .collect())
    }

    /// Creates the voice channel `name` (after the owner's yes). Never repeated after an uncertain
    /// outcome: the next run finds it by name.
    pub(super) async fn create_voice(&self, name: &str) -> Result<Snowflake, String> {
        match self.rest.create_voice_channel(self.guild, name).await {
            Ok(v) => v
                .get("id")
                .and_then(Value::as_str)
                .and_then(Snowflake::parse)
                .filter(|_| v.get("name").and_then(Value::as_str) == Some(name))
                .ok_or_else(|| {
                    format!("Discord's answer named no voice channel {name}; run setup again, it finds the channel if it was made")
                }),
            Err(e) if e.status == Some(403) => Err(format!(
                "the bot may not create channels ({e}): give it Manage Channels by opening {}, \
                 or create the voice channel {name} yourself, then run setup again",
                self.found.invite.as_deref().unwrap_or(INVITE)
            )),
            Err(e) if e.uncertain => Err(format!(
                "Discord did not confirm the voice channel {name} ({e}); run setup again, it finds the channel if it was made"
            )),
            Err(e) => Err(format!("creating the voice channel {name}: {e}")),
        }
    }

    /// Whether the bot may join and speak in the voice channel `channel` (a channel still to be
    /// created gets the server-wide permissions). `Err` says what is missing and how to fix it.
    pub(super) async fn voice_access(
        &self,
        channel: Option<Snowflake>,
        shown: &str,
    ) -> Result<(), String> {
        let bot = self
            .me
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let read = |e: crate::relay::discord::RestError| {
            format!("reading the bot's permissions in {}: {e}", self.server)
        };
        let guild = self.rest.guild(self.guild).await.map_err(read)?;
        let member = self.rest.member(self.guild, bot).await.map_err(read)?;
        let overwrites = match channel {
            Some(id) => self
                .rest
                .guild_channels(self.guild)
                .await
                .map_err(read)?
                .into_iter()
                .find(|c| c.get("id").and_then(Value::as_str) == Some(id.to_string().as_str()))
                .and_then(|c| c.get("permission_overwrites").cloned())
                .unwrap_or(Value::Null),
            None => Value::Null,
        };
        let app = self
            .app
            .get("id")
            .and_then(Value::as_str)
            .and_then(Snowflake::parse);
        match voice_blocked(&guild, &member, bot, &overwrites) {
            None => Ok(()),
            Some(Blocked::Server(missing)) => Err(format!(
                "the bot lacks {missing} in {}; open {} to invite it again with the permissions \
                 voice needs (inviting again updates its role), or give its role {missing}; then run \
                 `cataly setup --voice on` again. Voice was not enabled",
                self.server,
                invite_link(app)
            )),
            Some(Blocked::Overwrite(missing, by)) => Err(format!(
                "the voice channel {shown} denies the bot {missing} through its permission \
                 overwrite for {by}; allow it there (Edit Channel, Permissions), then run \
                 `cataly setup --voice on` again. Voice was not enabled"
            )),
        }
    }

    /// What the profile step does with the bot's avatar and banner and the application's icon. An
    /// image is set when there is none, when it is the one Cataly set earlier but Cataly's default
    /// changed since, or always with `force` (`--profile`); one the owner set by hand is kept.
    pub(super) fn profile_plan(&self, state_dir: &Path, force: bool) -> ProfilePlan {
        let record = read_record(state_dir, &self.me);
        let mut plan = ProfilePlan::default();
        for (field, bytes) in IMAGES {
            let holder = if field == APP_ICON {
                &self.app
            } else {
                &self.me
            };
            let current = holder.get(field).and_then(Value::as_str);
            let mine = record.get(field);
            let ours = current.is_some()
                && mine.and_then(|m| m.get("hash")).and_then(Value::as_str) == current;
            let fresh = mine.and_then(|m| m.get("image")).and_then(Value::as_str)
                == Some(sha256_hex(bytes).as_str());
            if force || current.is_none() || (ours && !fresh) {
                plan.set.push(field);
            } else if ours {
                plan.cataly.push(field);
            } else {
                plan.kept.push(field);
            }
        }
        plan
    }

    /// Sets Cataly's images for `fields` (the bot's through its account, the icon through the
    /// application) and records the hashes Discord gave them, so a later run can tell them from
    /// images the owner set himself.
    pub(super) async fn set_profile(
        &self,
        state_dir: &Path,
        fields: &[&'static str],
    ) -> Result<(), String> {
        let data = |bytes: &[u8]| {
            Value::String(format!(
                "data:image/jpeg;base64,{}",
                crate::sys::base64::encode(bytes)
            ))
        };
        let chosen = || IMAGES.iter().filter(|(f, _)| fields.contains(f));
        let mut user = serde_json::Map::new();
        let mut app = serde_json::Map::new();
        for (field, bytes) in chosen() {
            let body = if *field == APP_ICON {
                &mut app
            } else {
                &mut user
            };
            body.insert((*field).to_string(), data(bytes));
        }
        let mut set = serde_json::Map::new();
        for (body, is_app) in [(user, false), (app, true)] {
            if body.is_empty() {
                continue;
            }
            let answer = if is_app {
                self.rest.set_application(Value::Object(body)).await
            } else {
                self.rest.set_profile(Value::Object(body)).await
            };
            if let Value::Object(v) = answer.map_err(|e| e.to_string())? {
                set.extend(v);
            }
        }
        let mut record = read_record(state_dir, &self.me);
        record.insert(
            "bot".into(),
            self.me.get("id").cloned().unwrap_or(Value::Null),
        );
        for (field, bytes) in chosen() {
            record.insert(
                (*field).to_string(),
                json!({ "hash": set.get(*field), "image": sha256_hex(bytes) }),
            );
        }
        let text = serde_json::to_string(&record).map_err(|e| e.to_string())?;
        crate::sys::fs::write_atomic(&state_dir.join(PROFILE_RECORD), text.as_bytes())
            .map_err(|e| e.to_string())
    }
}

/// Cataly's default profile images: Discord's field and the bundled JPEG. The application's icon
/// is the avatar image.
const IMAGES: [(&str, &[u8]); 3] = [
    ("avatar", include_bytes!("../../assets/avatar.jpg")),
    ("banner", include_bytes!("../../assets/banner.jpg")),
    (APP_ICON, include_bytes!("../../assets/avatar.jpg")),
];
/// The image set on the application rather than the bot account.
const APP_ICON: &str = "icon";
/// The file in the state folder that records which images Cataly set on which bot.
const PROFILE_RECORD: &str = "discord-profile.json";

/// The profile step for the avatar and the banner (Discord's field names).
#[derive(Default)]
pub(super) struct ProfilePlan {
    /// Cataly sets its image.
    pub(super) set: Vec<&'static str>,
    /// Cataly's image is there already.
    pub(super) cataly: Vec<&'static str>,
    /// The owner's own image stays.
    pub(super) kept: Vec<&'static str>,
}

impl ProfilePlan {
    /// One line for the setup summary.
    pub(super) fn summary(&self) -> String {
        let mut parts = Vec::new();
        for (fields, what) in [
            (&self.set, "Cataly's (to set)"),
            (&self.cataly, "Cataly's"),
            (&self.kept, "yours, kept (--profile sets Cataly's)"),
        ] {
            if !fields.is_empty() {
                parts.push(format!("{}: {what}", fields.join(" and ")));
            }
        }
        parts.join("; ")
    }
}

/// What Cataly recorded setting on this bot; empty for another bot or no record.
fn read_record(state_dir: &Path, me: &Value) -> serde_json::Map<String, Value> {
    crate::sys::fs::read_private(&state_dir.join(PROFILE_RECORD), 64 * 1024)
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Map<String, Value>>(&bytes).ok())
        .filter(|r| r.get("bot").is_some() && r.get("bot") == me.get("id"))
        .unwrap_or_default()
}

/// The config with the guild, owner, the lead's channel and the `cataly-ops` destination; a
/// setting is written only when the file says something else.
pub(super) fn config_text(
    config: &Config,
    mut text: String,
    d: &Discord,
    lead: Snowflake,
    ops: Snowflake,
) -> String {
    let c = &config.discord;
    if c.guild != Some(d.guild) {
        text = file::set_key(&text, "discord", "guild", &d.guild.to_string());
    }
    if c.owner != Some(d.owner) {
        text = file::set_key(&text, "discord", "owner", &d.owner.to_string());
    }
    if !c.channels.contains(&lead) {
        let mut channels = c.channels.clone();
        channels.push(lead);
        text = file::set_rendered(&text, "discord", "channels", &file::string_list(&channels));
        if channels.len() > 1 && c.home.is_none() {
            text = file::set_key(&text, "discord", "home", &lead.to_string());
        }
    }
    let dest = Destination::Discord {
        channel: ChannelRef::Id(ops),
    };
    if config.destinations.get(OPS) != Some(&dest) {
        let section = format!("destinations.{OPS}");
        text = file::set_key(&text, &section, "kind", "discord");
        text = file::set_key(&text, &section, "channel", &ops.to_string());
    }
    text
}

/// The invite link for the application `app`: the bot scope with Cataly's permissions.
fn invite_link(app: Option<Snowflake>) -> String {
    let id = app.map_or_else(String::new, |a| a.to_string());
    format!("{INVITE}?client_id={id}&scope=bot&permissions={PERMISSIONS}")
}

/// Why the bot cannot join a voice channel.
#[derive(Debug, PartialEq, Eq)]
enum Blocked {
    /// Its roles lack these permissions server-wide.
    Server(String),
    /// The channel's overwrites for these holders take these permissions away.
    Overwrite(String, String),
}

/// Discord's permission computation for the bot in one channel (server roles, then the channel's
/// overwrites for `@everyone`, the bot's roles and the bot), reporting what voice lacks.
fn voice_blocked(guild: &Value, member: &Value, bot: &str, overwrites: &Value) -> Option<Blocked> {
    let bits = |v: Option<&Value>| {
        v.and_then(Value::as_str)
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0)
    };
    let everyone = guild.get("id").and_then(Value::as_str).unwrap_or_default();
    let mine: Vec<&str> = member
        .get("roles")
        .and_then(Value::as_array)
        .map_or_default(|r| r.iter().filter_map(Value::as_str).collect());
    let roles = guild.get("roles").and_then(Value::as_array);
    let role = |id: &str| roles.and_then(|r| r.iter().find(|x| x["id"] == id));
    let mut base = bits(role(everyone).and_then(|r| r.get("permissions")));
    for id in &mine {
        base |= bits(role(id).and_then(|r| r.get("permissions")));
    }
    if base & ADMINISTRATOR != 0 || guild.get("owner_id").and_then(Value::as_str) == Some(bot) {
        return None;
    }
    let lacking = |have: u64| -> u64 {
        VOICE_NEEDS
            .iter()
            .filter(|(b, _)| have & b == 0)
            .map(|(b, _)| *b)
            .sum()
    };
    let names = |bits: u64| {
        let list: Vec<&str> = VOICE_NEEDS
            .iter()
            .filter(|(b, _)| bits & b != 0)
            .map(|(_, n)| *n)
            .collect();
        list.join(" and ")
    };
    if lacking(base) != 0 {
        return Some(Blocked::Server(names(lacking(base))));
    }
    let list = overwrites.as_array().map_or_default(Vec::as_slice);
    let of = |id: &str| list.iter().find(|o| o["id"] == id);
    // Each step: who it is, its deny, its allow.
    let mut steps = Vec::new();
    if let Some(o) = of(everyone) {
        steps.push((
            "@everyone".to_string(),
            bits(o.get("deny")),
            bits(o.get("allow")),
        ));
    }
    let (mut deny, mut allow, mut holders) = (0, 0, Vec::new());
    for id in mine.iter().filter(|id| **id != everyone) {
        if let Some(o) = of(id) {
            deny |= bits(o.get("deny"));
            allow |= bits(o.get("allow"));
            let name = role(id).and_then(|r| r["name"].as_str()).unwrap_or(id);
            holders.push(format!("role {name}"));
        }
    }
    if !holders.is_empty() {
        steps.push((holders.join(", "), deny, allow));
    }
    if let Some(o) = of(bot) {
        steps.push((
            "the bot".to_string(),
            bits(o.get("deny")),
            bits(o.get("allow")),
        ));
    }
    let mut have = base;
    for (_, deny, allow) in &steps {
        have = (have & !deny) | allow;
    }
    let missing = lacking(have);
    if missing == 0 {
        return None;
    }
    let by: Vec<&str> = steps
        .iter()
        .filter(|(_, deny, _)| deny & missing != 0)
        .map(|(who, _, _)| who.as_str())
        .collect();
    Some(Blocked::Overwrite(names(missing), by.join(", ")))
}

/// Whether the application has either Message Content flag.
fn intent_on(app: &Value) -> bool {
    app.get("flags")
        .and_then(Value::as_u64)
        .is_some_and(|f| f & (MESSAGE_CONTENT | MESSAGE_CONTENT_LIMITED) != 0)
}

/// The application's owner: the team's owner for a team application, else its owning user.
fn app_owner(app: &Value) -> Option<(Snowflake, String)> {
    if let Some(team) = app.get("team").filter(|t| !t.is_null()) {
        let id = team
            .get("owner_user_id")
            .and_then(Value::as_str)
            .and_then(Snowflake::parse)?;
        let name = team
            .get("members")
            .and_then(Value::as_array)
            .and_then(|m| {
                m.iter().find_map(|member| {
                    let user = member.get("user")?;
                    (user.get("id").and_then(Value::as_str) == Some(id.to_string().as_str()))
                        .then(|| user.get("username").and_then(Value::as_str))
                        .flatten()
                })
            })
            .unwrap_or("the team owner");
        return Some((id, name.to_string()));
    }
    let owner = app.get("owner")?;
    let id = owner
        .get("id")
        .and_then(Value::as_str)
        .and_then(Snowflake::parse)?;
    let name = owner.get("username").and_then(Value::as_str).unwrap_or("?");
    Some((id, name.to_string()))
}

/// A client for `token` once Discord accepted it as a bot's. The error says whether Discord
/// refused the token (`true`) or could not be asked.
async fn bot(base: &Base, token: &str) -> Result<Rest, (bool, String)> {
    let rest = Rest::new(base.clone(), token, None).map_err(|e| (true, e))?;
    match rest.me().await {
        Ok(me) if me.get("bot").and_then(Value::as_bool) == Some(true) => Ok(rest),
        Ok(_) => Err((true, "it is not a bot's token".into())),
        Err(e) if e.status == Some(401) => Err((true, e.why)),
        Err(e) => Err((false, e.why)),
    }
}

/// Saves the token in its owner-only file, in an owner-only folder.
fn store_token(path: &Path, token: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        crate::sys::fs::ensure_private_dir(dir).map_err(|e| e.to_string())?;
    }
    let bytes = format!("{token}\n");
    let written = if std::fs::symlink_metadata(path).is_ok() {
        crate::sys::fs::write_atomic(path, bytes.as_bytes())
    } else {
        crate::sys::fs::create_atomic(path, bytes.as_bytes())
    };
    written.map_err(|e| e.to_string())
}

/// The Developer Portal steps only the owner can take.
fn portal_steps(path: &Path, at_terminal: bool) -> Vec<String> {
    let last = if at_terminal {
        "Paste it below; setup checks it and saves it in".to_string()
    } else {
        "Save it, alone, in".to_string()
    };
    vec![
        format!("Make Cataly's bot in the Discord Developer Portal, {PORTAL}:"),
        "  1. New Application, give it a name, Create.".to_string(),
        "  2. Bot, Reset Token, copy the token.".to_string(),
        format!("  3. {last} {} (only you may read it).", path.display()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_permissions_are_the_ones_cataly_uses() {
        assert_eq!(PERMISSIONS, 2_252_126_234_414_160);
        assert_eq!(PERMISSIONS & (CONNECT | SPEAK), CONNECT | SPEAK);
    }

    #[test]
    fn voice_access_follows_roles_then_channel_overwrites() {
        let guild = |everyone: u64| {
            json!({ "id": "1", "owner_id": "9", "roles": [
                { "id": "1", "name": "@everyone", "permissions": everyone.to_string() },
                { "id": "2", "name": "Cataly", "permissions": SPEAK.to_string() },
            ]})
        };
        let member = json!({ "roles": ["2"] });
        let all = VIEW_CHANNEL | CONNECT | SPEAK;
        let none = Value::Null;
        assert_eq!(voice_blocked(&guild(all), &member, "5", &none), None);
        // Speak comes from the bot's role; Connect is missing server-wide.
        assert_eq!(
            voice_blocked(&guild(VIEW_CHANNEL), &member, "5", &none),
            Some(Blocked::Server("Connect".into()))
        );
        let admin = json!({ "id": "1", "roles": [
            { "id": "1", "permissions": ADMINISTRATOR.to_string() } ] });
        assert_eq!(voice_blocked(&admin, &member, "5", &none), None);
        // An @everyone deny is lifted by an allow for the bot's role, not by nothing.
        let deny = json!([{ "id": "1", "type": 0, "allow": "0", "deny": CONNECT.to_string() }]);
        assert_eq!(
            voice_blocked(&guild(all), &member, "5", &deny),
            Some(Blocked::Overwrite("Connect".into(), "@everyone".into()))
        );
        let lifted = json!([
            { "id": "1", "type": 0, "allow": "0", "deny": CONNECT.to_string() },
            { "id": "2", "type": 0, "allow": CONNECT.to_string(), "deny": "0" },
        ]);
        assert_eq!(voice_blocked(&guild(all), &member, "5", &lifted), None);
        let bot = json!([{ "id": "5", "type": 1, "allow": "0", "deny": SPEAK.to_string() }]);
        assert_eq!(
            voice_blocked(&guild(all), &member, "5", &bot),
            Some(Blocked::Overwrite("Speak".into(), "the bot".into()))
        );
    }

    #[test]
    fn the_images_are_jpegs() {
        for (field, bytes) in IMAGES {
            assert!(bytes.starts_with(&[0xFF, 0xD8, 0xFF]), "{field}");
            assert!(bytes.len() <= 512 * 1024, "{field}");
        }
    }

    #[test]
    fn the_owner_comes_from_the_team_or_the_user() {
        let user =
            json!({ "owner": { "id": "100000000000000001", "username": "me" }, "team": null });
        assert_eq!(
            app_owner(&user).map(|(id, n)| (id.to_string(), n)),
            Some(("100000000000000001".into(), "me".into()))
        );
        let team = json!({ "owner": { "id": "100000000000000009", "username": "team123" },
            "team": { "owner_user_id": "100000000000000002",
                "members": [{ "user": { "id": "100000000000000002", "username": "lead" } }] } });
        assert_eq!(
            app_owner(&team).map(|(id, n)| (id.to_string(), n)),
            Some(("100000000000000002".into(), "lead".into()))
        );
        assert!(intent_on(&json!({ "flags": MESSAGE_CONTENT_LIMITED })));
        assert!(!intent_on(&json!({ "flags": 1 << 23 })));
    }
}
