//! `cataly setup discord`: the guided Discord setup. Discord leaves three steps to the owner, because
//! they need his own Discord login: making the application, copying its bot token, and adding the
//! bot to a server through the invite link. Everything else runs here on the bot's token: the token
//! is checked (`GET /users/@me`); the Message Content intent is read from the application and, with
//! the owner's consent, switched on (`PATCH /applications/@me` may set the intent flag of a bot in
//! fewer than 100 servers); the invite link carries exactly the permissions Cataly uses; the
//! servers the bot is in are read until it joins one; text channels named `cataly` and `cataly-ops`
//! are reused, or created after confirmation, and no other channel is touched; the owner's user ID
//! comes from the application's owner after confirmation, or is typed in. The config is written
//! through the preview, `--expect` and apply path of `cataly setup`. Every fact is read again on
//! each run and a setting is written only when it differs, so a second run changes nothing.
//!
//! At a terminal the setup asks; with `--json`, or when stdin is not a terminal, it never asks:
//! the flags answer, and a step that still needs the owner ends the run with exit status 1 and
//! names that step.

use std::io::BufRead as _;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};

use super::args::Parsed;
use super::setup::{self, Plan};
use super::{Failure, out};
use crate::config::{self, Config, Destination};
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
/// What Cataly does in its channels: add reactions, view channels, send messages, read message
/// history, manage threads, create public threads, send messages in threads.
const PERMISSIONS: u64 =
    (1 << 6) | (1 << 10) | (1 << 11) | (1 << 16) | (1 << 34) | (1 << 35) | (1 << 38);
/// Manage channels, asked for only when setup is to create its channels.
const MANAGE_CHANNELS: u64 = 1 << 4;
/// The lead's channel and the job notices' channel (also the name of the destination for it).
const LEAD: &str = "cataly";
const OPS: &str = "cataly-ops";
/// How often the server list is read while waiting for the bot to join.
const POLL: Duration = Duration::from_secs(3);
/// How long a run at a terminal waits for the bot to join, unless `--wait` says otherwise.
const JOIN_WAIT: Duration = Duration::from_mins(10);
/// The longest `--wait`.
const WAIT_MAX: Duration = Duration::from_hours(1);
/// How long one question waits for its answer.
const ANSWER_WAIT: Duration = Duration::from_mins(30);
/// Tries at one step before the run stops and asks for a new run.
const TRIES: usize = 5;

/// The owner's terminal. Answers are read on their own thread, so every wait for one is bounded.
struct Terminal {
    lines: mpsc::Receiver<String>,
}

impl Terminal {
    /// The terminal, when stdin is one and JSON was not asked for; scripts get none.
    fn open(as_json: bool) -> Option<Self> {
        if as_json || !rustix::termios::isatty(std::io::stdin()) {
            return None;
        }
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Some(Self { lines })
    }

    /// One answer, trimmed; `None` when stdin closed or no answer came in time.
    fn ask(&self, question: &str) -> Option<String> {
        out::prompt(&format!("{question} "));
        let answer = self.lines.recv_timeout(ANSWER_WAIT).ok();
        if answer.is_none() {
            out::line("");
        }
        answer.map(|a| a.trim().to_string())
    }

    fn yes(&self, question: &str) -> bool {
        match self.ask(&format!("{question} [Y/n]")).as_deref() {
            Some("") => true,
            Some(a) => matches!(a.to_ascii_lowercase().as_str(), "y" | "yes"),
            None => false,
        }
    }

    /// One answer typed without echo. `None` also when echo cannot be turned off: then nothing is
    /// asked, so a token never shows on the screen.
    fn hidden(&self, question: &str) -> Option<String> {
        use rustix::termios::{LocalModes, OptionalActions, tcgetattr, tcsetattr};
        let stdin = std::io::stdin();
        let saved = tcgetattr(&stdin).ok()?;
        let mut quiet = saved.clone();
        quiet.local_modes.remove(LocalModes::ECHO);
        tcsetattr(&stdin, OptionalActions::Now, &quiet).ok()?;
        let answer = self.ask(question);
        let _ = tcsetattr(&stdin, OptionalActions::Now, &saved);
        out::line("");
        answer
    }
}

/// What the run found, for the report.
#[derive(Default)]
struct Found {
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
    fn report(&self, next: Option<&str>) -> Value {
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

/// What the config gets once every step is done.
struct Ready {
    guild: Snowflake,
    lead: Snowflake,
    ops: Snowflake,
    owner: Snowflake,
}

/// The owner as `--owner` names him.
#[derive(Clone, Copy)]
enum OwnerFlag {
    Id(Snowflake),
    Application,
}

struct Flow<'a> {
    p: &'a Parsed,
    paths: &'a Paths,
    config: Config,
    term: Option<Terminal>,
    as_json: bool,
    wait: Option<Duration>,
    guild: Option<Snowflake>,
    owner: Option<OwnerFlag>,
    /// Whether setup creates missing channels, once known (the invite link depends on it).
    create: Option<bool>,
    found: Found,
}

/// Why the run stopped: the step that still needs the owner.
type Stop = String;

pub(super) fn run(p: &Parsed, paths: &Paths, as_json: bool) -> Result<(), Failure> {
    let wait = p
        .value("--wait")
        .map(|w| {
            config::parse_duration(w)
                .filter(|d| *d <= WAIT_MAX)
                .ok_or_else(|| {
                    Failure::Usage("--wait: expected a duration up to 1h, such as 5m".into())
                })
        })
        .transpose()?;
    let guild = p
        .value("--guild")
        .map(|g| {
            Snowflake::parse(g)
                .ok_or_else(|| Failure::Usage("--guild: expected the server's Discord ID".into()))
        })
        .transpose()?;
    let owner = p
        .value("--owner")
        .map(|o| match o {
            "app" => Ok(OwnerFlag::Application),
            id => Snowflake::parse(id).map(OwnerFlag::Id).ok_or_else(|| {
                Failure::Usage("--owner: expected your Discord user ID, or app".into())
            }),
        })
        .transpose()?;
    let (before, text) = setup::load(paths)?;
    let current = config::parse(&text).map_err(|e| {
        Failure::Refused(format!(
            "{} is not a valid config ({}); fix it first, cataly config check shows why",
            paths.config_file.display(),
            config::describe(&e)
        ))
    })?;
    let mut flow = Flow {
        p,
        paths,
        config: current,
        term: Terminal::open(as_json),
        as_json,
        wait,
        guild,
        owner,
        create: None,
        found: Found::default(),
    };
    let ready = match super::install::block_on(flow.discover()) {
        Ok(ready) => ready,
        Err(next) => {
            if as_json {
                out::json(&json!({ "discord": flow.found.report(Some(&next)) }));
            }
            return Err(Failure::Refused(next));
        }
    };
    flow.found.stage = "ready";
    let creating = before.bytes.is_none();
    let text = flow.config_text(setup::managed(p, text, creating)?, &ready);
    let digest = before.digest();
    let mut plan = Plan {
        apply: p.flag("--apply"),
        expect: p.value("--expect"),
        command: "setup discord",
        flags: format!(" --guild {} --owner {}", ready.guild, ready.owner),
        always_offer: false,
        extra: Some(("discord", flow.found.report(None))),
    };
    setup::finish(paths, &before, creating, &text, None, as_json, &plan)?;
    let changes = creating || before.bytes.as_deref() != Some(text.as_bytes());
    if !plan.apply
        && changes
        && config::parse(&text).is_ok()
        && flow.term.as_ref().is_some_and(|t| t.yes("Write it now?"))
    {
        plan.apply = true;
        plan.expect = Some(&digest);
        setup::finish(paths, &before, creating, &text, None, as_json, &plan)?;
    }
    Ok(())
}

impl Flow<'_> {
    fn say(&self, line: &str) {
        if !self.as_json {
            out::line(line);
        }
    }

    /// Yes from the flag, else from the owner at the terminal; no when nobody can answer.
    fn confirm(&self, flag: &str, question: &str) -> bool {
        self.p.flag(flag) || self.term.as_ref().is_some_and(|t| t.yes(question))
    }

    fn stop(&mut self, stage: &'static str, next: String) -> Stop {
        self.found.stage = stage;
        next
    }

    async fn discover(&mut self) -> Result<Ready, Stop> {
        let rest = self.token().await?;
        let app = self.intent(&rest).await?;
        let (guild, name) = self.server(&rest).await?;
        let (lead, ops) = self.channels(&rest, guild, &name).await?;
        let owner = self.owner_id(&app)?;
        Ok(Ready {
            guild,
            lead,
            ops,
            owner,
        })
    }

    /// A REST client on a token Discord accepts as a bot's.
    async fn token(&mut self) -> Result<Rest, Stop> {
        let path = crate::jobs::owner::health::token_path(&self.config, &self.paths.config_file);
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
            let Some(term) = &self.term else {
                let next = format!(
                    "make the bot in the Developer Portal ({PORTAL}) and save its token in {} (chmod 600), then run cataly setup discord again",
                    path.display()
                );
                return Err(self.stop("token", next));
            };
            let question = format!(
                "Paste the bot token (it is not shown), or press Enter once it is in {}:",
                path.display()
            );
            let Some(typed) = term.hidden(&question) else {
                return Err(self.stop(
                    "token",
                    format!(
                        "no token given; save it in {} and run setup again",
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
            "no token Discord accepts yet; run cataly setup discord again".into(),
        ))
    }

    /// The application, with its Message Content intent on.
    async fn intent(&mut self, rest: &Rest) -> Result<Value, Stop> {
        let me = rest
            .me()
            .await
            .map_err(|e| self.stop("token", format!("reading the bot: {e}")))?;
        let bot_id = me.get("id").and_then(Value::as_str).unwrap_or_default();
        let bot_name = me.get("username").and_then(Value::as_str).unwrap_or("?");
        self.found.bot = Some(json!({ "id": bot_id, "username": bot_name }));
        self.say(&format!(
            "bot: {bot_name} ({bot_id}), token from {}",
            self.found.token_file
        ));
        let mut app = self.application(rest).await?;
        if intent_on(&app) {
            self.found.message_content = Some("on");
            self.say("Message Content intent: on");
            return Ok(app);
        }
        self.found.message_content = Some("off");
        if self.confirm(
            "--enable-intent",
            "The bot's Message Content intent is off; Cataly needs it to read your messages. Switch it on now?",
        ) {
            let flags = app.get("flags").and_then(Value::as_u64).unwrap_or(0);
            match rest
                .set_application_flags((flags & LIMITED_INTENTS) | MESSAGE_CONTENT_LIMITED)
                .await
            {
                Ok(_) => app = self.application(rest).await?,
                Err(e) => self.say(&format!("Discord did not take the change ({e})")),
            }
            if intent_on(&app) {
                self.found.message_content = Some("switched on");
                self.say("Message Content intent: switched on");
                return Ok(app);
            }
        }
        let place = format!(
            "turn on the Message Content intent: Developer Portal ({PORTAL}), your application, Bot, \
             Privileged Gateway Intents, Message Content Intent, Save Changes"
        );
        for _ in 0..TRIES {
            let Some(term) = &self.term else { break };
            self.say(&format!("Please {place}."));
            if term.ask("Press Enter once it is on:").is_none() {
                break;
            }
            app = self.application(rest).await?;
            if intent_on(&app) {
                self.found.message_content = Some("on");
                self.say("Message Content intent: on");
                return Ok(app);
            }
        }
        Err(self.stop(
            "intent",
            format!("{place}; or run cataly setup discord --enable-intent"),
        ))
    }

    async fn application(&mut self, rest: &Rest) -> Result<Value, Stop> {
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

    /// The invite link: the bot scope with Cataly's permissions, and Manage Channels when setup is
    /// to create its channels.
    fn invite(&mut self, create: bool) -> String {
        let id = self
            .found
            .application
            .map_or_else(String::new, |a| a.to_string());
        let permissions = PERMISSIONS | if create { MANAGE_CHANNELS } else { 0 };
        let link = format!("{INVITE}?client_id={id}&scope=bot&permissions={permissions}");
        self.found.invite = Some(link.clone());
        link
    }

    fn create_channels(&mut self) -> bool {
        if let Some(c) = self.create {
            return c;
        }
        let c = self.confirm(
            "--create-channels",
            &format!(
                "Should setup create #{LEAD} (your talks with the lead) and #{OPS} (job notices) \
                 when the server has none? The bot then also gets Manage Channels."
            ),
        );
        self.create = Some(c);
        c
    }

    /// The server the bot is in, after the owner added it through the invite link.
    async fn server(&mut self, rest: &Rest) -> Result<(Snowflake, String), Stop> {
        let wanted = self.guild.or(self.config.discord.guild);
        let mut guilds = self.guild_list(rest).await?;
        let present = |list: &[(Snowflake, String)]| match wanted {
            Some(w) => list.iter().any(|(id, _)| *id == w),
            None => !list.is_empty(),
        };
        if present(&guilds) {
            let create = self.p.flag("--create-channels");
            self.invite(create);
        } else {
            let create = self.create_channels();
            let link = self.invite(create);
            self.say(&format!(
                "Add the bot to your server: open {link}, pick the server, then Authorize."
            ));
            let wait = self.wait.unwrap_or(if self.term.is_some() {
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
                    format!("{what}: open {link}, add it, then run cataly setup discord again"),
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
        self.say(&format!("server: {name} ({id})"));
        Ok((id, name))
    }

    async fn guild_list(&mut self, rest: &Rest) -> Result<Vec<(Snowflake, String)>, Stop> {
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
        let term = self.term.as_ref()?;
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

    /// `#cataly` and `#cataly-ops`: the text channels with those names, or new ones after
    /// confirmation. No other channel is changed.
    async fn channels(
        &mut self,
        rest: &Rest,
        guild: Snowflake,
        server: &str,
    ) -> Result<(Snowflake, Snowflake), Stop> {
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
                    self.say(&format!("#{name}: found ({one})"));
                    ids.push(Some(*one));
                }
                [] => ids.push(None),
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
        let missing: Vec<&str> = [LEAD, OPS]
            .into_iter()
            .zip(&ids)
            .filter(|(_, id)| id.is_none())
            .map(|(n, _)| n)
            .collect();
        if !missing.is_empty() {
            let list = missing
                .iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(" and ");
            let create = match self.create {
                Some(c) => c,
                None => self.confirm(
                    "--create-channels",
                    &format!("{server} has no {list}. Create them now?"),
                ),
            };
            if !create {
                return Err(self.stop(
                    "channels",
                    format!(
                        "create the text channels {list} in {server}, or run cataly setup discord --create-channels"
                    ),
                ));
            }
            for (name, id) in [LEAD, OPS].into_iter().zip(ids.iter_mut()) {
                if id.is_none() {
                    *id = Some(self.create_one(rest, guild, name).await?);
                }
            }
        }
        match ids.as_slice() {
            [Some(lead), Some(ops)] => Ok((*lead, *ops)),
            _ => Err(self.stop("channels", "the channels could not be settled".into())),
        }
    }

    async fn create_one(
        &mut self,
        rest: &Rest,
        guild: Snowflake,
        name: &'static str,
    ) -> Result<Snowflake, Stop> {
        let topic = if name == LEAD {
            "Talk with Cataly's lead here."
        } else {
            "Cataly's job notices."
        };
        match rest.create_text_channel(guild, name, topic).await {
            Ok(v) => {
                let id = v
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(Snowflake::parse)
                    .filter(|_| v.get("name").and_then(Value::as_str) == Some(name));
                let Some(id) = id else {
                    return Err(self.stop(
                        "channels",
                        format!("Discord's answer named no #{name}; run setup again, it finds the channel if it was made"),
                    ));
                };
                self.found
                    .channels
                    .push(json!({ "name": name, "id": id.to_string(), "state": "created" }));
                self.say(&format!("#{name}: created ({id})"));
                Ok(id)
            }
            Err(e) if e.status == Some(403) => {
                let link = self.invite(true);
                Err(self.stop(
                    "channels",
                    format!(
                        "the bot may not create channels ({e}): give it Manage Channels by opening {link}, \
                         or create #{name} yourself, then run setup again"
                    ),
                ))
            }
            Err(e) if e.uncertain => Err(self.stop(
                "channels",
                format!("Discord did not confirm #{name} ({e}); run setup again, it finds the channel if it was made"),
            )),
            Err(e) => Err(self.stop("channels", format!("creating #{name}: {e}"))),
        }
    }

    /// The owner's user ID: `--owner`, else the config's, else the application's owner after
    /// confirmation, else typed in.
    fn owner_id(&mut self, app: &Value) -> Result<Snowflake, Stop> {
        let candidate = app_owner(app);
        let (id, source) = match self.owner {
            Some(OwnerFlag::Id(id)) => (id, "--owner"),
            Some(OwnerFlag::Application) => match &candidate {
                Some((id, _)) => (*id, "application owner"),
                None => {
                    return Err(self.stop(
                        "owner",
                        "the application names no owner; pass --owner <your Discord user ID>"
                            .into(),
                    ));
                }
            },
            None => match self.config.discord.owner {
                Some(id) => (id, "config"),
                None => self.ask_owner(candidate.as_ref())?,
            },
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
        self.say(&format!("owner: {id} ({source})"));
        Ok(id)
    }

    fn ask_owner(
        &mut self,
        candidate: Option<&(Snowflake, String)>,
    ) -> Result<(Snowflake, &'static str), Stop> {
        let Some(term) = &self.term else {
            let next = match candidate {
                Some((id, name)) => format!(
                    "name the owner: --owner app takes the application's owner, {name} ({id}); or --owner <your Discord user ID>"
                ),
                None => "name the owner: --owner <your Discord user ID>".into(),
            };
            return Err(self.stop("owner", next));
        };
        if let Some((id, name)) = candidate
            && term.yes(&format!(
                "The application belongs to {name} ({id}). Is that your Discord account?"
            ))
        {
            return Ok((*id, "application owner"));
        }
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
        Err(self.stop(
            "owner",
            "no owner ID given; run setup again or pass --owner <your Discord user ID>".into(),
        ))
    }

    /// The config with the guild, owner, the lead's channel and the `cataly-ops` destination; a
    /// setting is written only when the file says something else.
    fn config_text(&self, mut text: String, ready: &Ready) -> String {
        let d = &self.config.discord;
        if d.guild != Some(ready.guild) {
            text = setup::set_key(&text, "discord", "guild", &ready.guild.to_string());
        }
        if d.owner != Some(ready.owner) {
            text = setup::set_key(&text, "discord", "owner", &ready.owner.to_string());
        }
        if !d.channels.contains(&ready.lead) {
            let mut channels = d.channels.clone();
            channels.push(ready.lead);
            let quoted: Vec<String> = channels
                .iter()
                .map(|c| setup::toml_string(&c.to_string()))
                .collect();
            text = setup::set_rendered(
                &text,
                "discord",
                "channels",
                &format!("[{}]", quoted.join(", ")),
            );
            if channels.len() > 1 && d.home.is_none() {
                text = setup::set_key(&text, "discord", "home", &ready.lead.to_string());
            }
        }
        let ops = Destination::Discord {
            channel: ChannelRef::Id(ready.ops),
        };
        if self.config.destinations.get(OPS) != Some(&ops) {
            let section = format!("destinations.{OPS}");
            text = setup::set_key(&text, &section, "kind", "discord");
            text = setup::set_key(&text, &section, "channel", &ready.ops.to_string());
        }
        text
    }
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
        "Setup does the rest: the Message Content intent, the invite link, the channels."
            .to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_permissions_are_the_ones_cataly_uses() {
        assert_eq!(PERMISSIONS, 326_417_583_168);
        assert_eq!(PERMISSIONS | MANAGE_CHANNELS, 326_417_583_184);
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
