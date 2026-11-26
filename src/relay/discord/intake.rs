//! Discord intake (DISC-IN-01, 02, 04-08, 11-13, 15, 16, 19-21, 23-25): the start-up checks of bot,
//! owner and application, the (bot, owner) binding, the watch scope of listed channels and followed
//! threads, the live Gateway feeding the durable arrival queue, catch-up reads over REST after every
//! READY or RESUMED, and the handling round that drains the queue through the shared pipeline, posts
//! answers, keeps questions in step with the lead's dialogs and drives the typing indicator.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{Notify, broadcast, watch};

use super::gateway::{self, Gateway, Handler, WsUrl};
use super::questions::Cards;
use super::tap::{DISPATCHES, Seen, Tap};
use super::types::{self, Gate};
use super::{Rest, RestError, Snowflake, nonce};
use crate::config::{Config, DiscordSettings};
use crate::host::lead::{Events, Lead};
use crate::host::{Runtime, SessionEvent};
use crate::jobs::event::{Emit, EventKind, Severity};
use crate::log;
pub(crate) use crate::relay::intake::Shared;
use crate::relay::intake::{Stop, supervise};
use crate::relay::pipeline::{Arrival, Pipeline, Step};
use crate::relay::text::{self, Checks, DISCORD_LIMIT};
use crate::relay::{Facts, Inbound, PartOutcome, Relay};
use crate::store::Store;
use crate::store::relay::{QueueRow, RelayDone, RelayWrite};
use crate::sys::now_ms;

/// The relay's store key.
pub(crate) const NAME: &str = "discord";
/// Catch-up pages of 100 messages read at most per room (DISC-IN req. 32).
const PAGES_MAX: usize = 100;
/// Followed threads at most per listing (DISC-IN req. 8).
const THREADS_MAX: usize = 128;
/// Cached thread lookups (DISC-IN req. 10).
const LOOKUPS_MAX: usize = 256;
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
/// The handling round runs at least this often while work is open.
const TICK: Duration = Duration::from_secs(3);
const IDLE_TICK: Duration = Duration::from_secs(30);
/// Typing lasts about 10 s on Discord; it is renewed while a turn runs.
const TYPING_EVERY: Duration = Duration::from_secs(8);
/// Catch-up retry backoff (DISC-IN req. 34).
const CATCH_UP_FIRST: Duration = Duration::from_secs(2);
const CATCH_UP_MAX: Duration = Duration::from_secs(30);
/// The REST client for a config.
pub(crate) type RestFor = Arc<dyn Fn(&Config) -> Result<Arc<Rest>, String> + Send + Sync>;

/// What intake needs from the hub.
pub(crate) struct Env<T: Runtime> {
    pub(crate) config: watch::Receiver<Arc<Config>>,
    pub(crate) store: Store,
    pub(crate) lead: Arc<Lead<T>>,
    pub(crate) lanes: Arc<crate::lanes::Lanes<T>>,
    pub(crate) events: Events,
    /// The REST client for the token the config names (shared with job posting).
    pub(crate) rest: RestFor,
    /// The job engine, for the ops panel's buttons.
    pub(crate) jobs: crate::jobs::Jobs,
    pub(crate) spill_dir: std::path::PathBuf,
    /// Voice's tap on the Gateway connection (voice builds only).
    pub(crate) tap: Option<Arc<Tap>>,
}

/// The Discord side of the relay trait.
pub(crate) struct DiscordRelay {
    pub(crate) rest: Arc<Rest>,
    pub(crate) bot: Snowflake,
    pub(crate) checks: Checks,
}

fn id(text: &str) -> Result<Snowflake, String> {
    Snowflake::parse(text).ok_or_else(|| format!("'{text}' is not a Discord ID"))
}

impl Relay for DiscordRelay {
    const NAME: &'static str = NAME;
    const REPEATABLE: bool = true;
    const LIVE_ONCE: bool = true;

    fn parts(&self, answer: &str) -> Result<Vec<String>, String> {
        let answer = answer.trim();
        text::check(answer, &self.checks)
            .map_err(|e| format!("refused by the outgoing text checks: {e}"))?;
        text::split(&text::render_tables(answer), DISCORD_LIMIT)
            .ok_or_else(|| "empty answer".into())
    }

    fn nonce(&self, room: &str, source: &str, index: usize) -> String {
        nonce(&format!("reply:{}:{room}:{source}", self.bot), index)
    }

    fn normalise(&self, row: &QueueRow) -> Result<Inbound, String> {
        let raw: Value = serde_json::from_str(&row.raw).map_err(|e| e.to_string())?;
        let parent = row.parent.as_deref().and_then(Snowflake::parse);
        types::normalise(&raw, parent)
    }

    async fn post(
        &self,
        room: &str,
        reply_to: Option<&str>,
        text: &str,
        nonce: &str,
    ) -> PartOutcome {
        let Ok(channel) = id(room) else {
            return PartOutcome::Rejected("bad room".into());
        };
        let reply_to = reply_to.and_then(Snowflake::parse);
        self.rest.reply_part(channel, reply_to, text, nonce).await
    }

    async fn seen(&self, room: &str, message: &str) -> Result<(), String> {
        self.rest
            .react_seen(id(room)?, id(message)?)
            .await
            .map_err(|e| e.to_string())
    }

    async fn typing(&self, room: &str) -> Result<(), String> {
        self.rest.typing(id(room)?).await.map_err(|e| e.to_string())
    }

    async fn facts(&self, room: &str) -> Result<Facts, String> {
        let info = self
            .rest
            .channel(id(room)?)
            .await
            .map_err(|e| e.to_string())?;
        let kind = info.get("type").and_then(Value::as_u64);
        Ok(Facts {
            name: str_field(&info, "name").map(str::to_string),
            topic: str_field(&info, "topic")
                .filter(|t| !t.trim().is_empty())
                .map(str::to_string),
            dm: matches!(kind, Some(1 | 3)),
        })
    }
}

/// A watched channel.
#[derive(Debug, Clone, Copy)]
struct Listed {
    kind: u64,
    guild: Option<Snowflake>,
}

impl Listed {
    /// Forum and media channels hold no messages of their own (DISC-IN req. 7).
    fn forum(self) -> bool {
        matches!(self.kind, 15 | 16)
    }
}

/// The watch scope (DISC-IN req. 5-10).
#[derive(Default)]
struct Scope {
    listed: HashMap<Snowflake, Listed>,
    /// Followed thread -> its listed parent.
    threads: HashMap<Snowflake, Snowflake>,
    /// Recent lookups of unknown channels: the parent when it is a thread under a listed channel.
    lookups: VecDeque<(Snowflake, Option<Snowflake>)>,
    /// The first active-thread listing of this run is done.
    listed_once: bool,
}

impl Scope {
    /// Whether the channel is in scope, and its listed parent when it is a followed thread.
    fn room(&self, channel: Snowflake) -> InScope {
        if self.listed.get(&channel).is_some_and(|l| !l.forum()) {
            return InScope::Listed;
        }
        self.threads
            .get(&channel)
            .map_or(InScope::No, |p| InScope::Thread(*p))
    }
}

/// Where a channel stands in the watch scope.
enum InScope {
    Listed,
    Thread(Snowflake),
    No,
}

/// Maps a REST failure at start to how intake stops.
fn rest_stop(what: &str, e: &RestError) -> Stop {
    if e.status == Some(401) {
        Stop::Fatal(format!("{what}: the bot token was refused (401)"))
    } else if e
        .status
        .is_some_and(|s| s != 429 && (400..500).contains(&s))
    {
        Stop::Fatal(format!("{what}: {e}"))
    } else {
        Stop::Retry(format!("{what}: {e}"))
    }
}

/// The supervised intake task: runs while the config enables intake; fatal conditions wait for the
/// owner instead of ending the task (DISC-IN req. 1-3, 20).
pub(crate) async fn run<T: Runtime>(
    shared: Arc<Shared>,
    env: Arc<Env<T>>,
    shutdown: watch::Receiver<bool>,
) -> Result<(), String> {
    let off = |c: &Config| {
        (!c.discord.intake()).then(|| "set [discord] owner and channels to start intake".into())
    };
    let (task, stop, rx) = (Arc::clone(&shared), shutdown.clone(), env.config.clone());
    supervise(
        NAME,
        shared,
        env.config.clone(),
        shutdown,
        Arc::clone(&env.events),
        off,
        move |config: Arc<Config>| {
            let (shared, env, stop, rx) = (
                Arc::clone(&task),
                Arc::clone(&env),
                stop.clone(),
                rx.clone(),
            );
            async move {
                shared.set(|h| h.owner = config.discord.owner.map(|o| o.to_string()));
                match Intake::start(&shared, &env, &config).await {
                    Ok(intake) => intake.serve(stop, rx).await,
                    Err(stop) => stop,
                }
            }
        },
    )
    .await
}

/// One running intake.
pub(crate) struct Intake<T: Runtime> {
    /// The running `[discord]` settings; the watched channels and home change in place
    /// ([`Intake::rewatch`]), any other change restarts intake.
    settings: Mutex<DiscordSettings>,
    /// The owner's language for fixed card wording (DESIGN 12.2 A19).
    pub(super) language: crate::config::Language,
    pub(super) rest: Arc<Rest>,
    pub(super) bot: Snowflake,
    pub(super) owner: Snowflake,
    pub(super) application: Snowflake,
    /// Buttons work: the application has no interactions endpoint URL (DISC-OUT req. 68).
    pub(super) buttons: bool,
    pub(super) pipeline: Arc<Pipeline<DiscordRelay, T>>,
    pub(super) store: Store,
    pub(super) lead: Arc<Lead<T>>,
    pub(super) jobs: crate::jobs::Jobs,
    shared: Arc<Shared>,
    gateway_url: WsUrl,
    token_intents: (String, u64),
    scope: Mutex<Scope>,
    pub(super) wake: Notify,
    started_ms: i64,
    /// When the current connection was established, for the catch-up latency.
    established: Mutex<Option<Instant>>,
    catch_up_backoff: Mutex<(Duration, Option<Instant>)>,
    turn_running: AtomicBool,
    /// A turn ended (no continuation) since the last turn start.
    turn_ended: AtomicBool,
    typed: Mutex<Option<Instant>>,
    pub(super) cards: Cards,
    pub(super) events: Events,
    /// Voice's tap and whether the config wanted it at start.
    tap: (Option<Arc<Tap>>, bool),
}

/// A task aborted when its handle is dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl<T: Runtime> Intake<T> {
    fn active_tap(&self) -> Option<&Tap> {
        self.tap.0.as_deref().filter(|_| self.tap.1)
    }
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

impl<T: Runtime> Intake<T> {
    /// Start-up checks (DISC-IN req. 1-3, 5, 11; DISC-OUT req. 85) and the first-run baseline.
    async fn start(
        shared: &Arc<Shared>,
        env: &Arc<Env<T>>,
        config: &Config,
    ) -> Result<Arc<Self>, Stop> {
        let settings = config.discord.clone();
        let owner = settings
            .owner
            .ok_or_else(|| Stop::Fatal("no owner configured".into()))?;
        let rest = (env.rest)(config).map_err(Stop::Fatal)?;
        let (bot, application, buttons) = account(&rest, owner).await?;
        let bound = env
            .store
            .relay(RelayWrite::Bind {
                relay: NAME.into(),
                account: bot.to_string(),
                owner: owner.to_string(),
            })
            .await
            .map_err(|e| Stop::Retry(e.to_string()))?;
        if let RelayDone::Bound(Some((account, was))) = bound {
            return Err(Stop::Fatal(format!(
                "intake state belongs to bot {account} and owner {was}; `cataly discord reset` starts afresh"
            )));
        }
        let scope = listed(&rest, &settings.channels).await?;
        let gateway_url = if settings.api_base.is_some() {
            let url = rest
                .gateway_url()
                .await
                .map_err(|e| rest_stop("reading the Gateway URL", &e))?;
            WsUrl::parse(&url).map_err(Stop::Fatal)?
        } else {
            WsUrl::parse("wss://gateway.discord.gg").map_err(Stop::Fatal)?
        };
        let token = rest.token().to_string();
        let relay = Arc::new(DiscordRelay {
            rest: Arc::clone(&rest),
            bot,
            checks: config.outgoing.clone(),
        });
        let pipeline = Arc::new(Pipeline::new(
            relay,
            crate::relay::pipeline::Parts {
                store: env.store.clone(),
                lead: Arc::clone(&env.lead),
                lanes: Arc::clone(&env.lanes),
                spill_dir: env.spill_dir.clone(),
                events: Arc::clone(&env.events),
                config: env.config.clone(),
            },
            shared.wake_latency.clone(),
        ));
        shared.set(|h| {
            h.bot = Some(bot.to_string());
            h.buttons = buttons;
            h.reason = None;
        });
        let voice = env.tap.as_ref().is_some_and(|t| (t.wanted)(config));
        let intake = Arc::new(Self {
            token_intents: (token, gateway::intents(settings.follow_threads, voice)),
            tap: (env.tap.clone(), voice),
            events: Arc::clone(&env.events),
            settings: Mutex::new(settings.clone()),
            language: config.hub.language,
            rest,
            bot,
            owner,
            application,
            buttons,
            pipeline,
            store: env.store.clone(),
            lead: Arc::clone(&env.lead),
            jobs: env.jobs.clone(),
            shared: Arc::clone(shared),
            gateway_url,
            scope: Mutex::new(scope),
            wake: Notify::new(),
            started_ms: now_ms(),
            established: Mutex::new(None),
            catch_up_backoff: Mutex::new((CATCH_UP_FIRST, None)),
            turn_running: AtomicBool::new(false),
            turn_ended: AtomicBool::new(false),
            typed: Mutex::new(None),
            cards: Cards::default(),
        });
        intake.close_left_open().await;
        intake.baseline_listed().await?;
        // A queue left from before is drained after a full catch-up read (DISC-IN req. 28).
        let queued = env
            .store
            .read(|r| r.queue(NAME))
            .await
            .map_err(|e| Stop::Retry(e.to_string()))?;
        if !queued.is_empty() {
            intake.flag(true).await;
        }
        Ok(intake)
    }

    fn scope(&self) -> MutexGuard<'_, Scope> {
        self.scope.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn flag(&self, on: bool) {
        let write = RelayWrite::Flag {
            relay: NAME.into(),
            flag: "catch_up".into(),
            on,
        };
        if let Err(e) = self.store.relay(write).await {
            log::warn("catch-up flag not recorded", &[("error", &e)]);
        }
    }

    async fn write(&self, write: RelayWrite) -> Result<RelayDone, String> {
        self.store.relay(write).await.map_err(|e| e.to_string())
    }

    /// The newest message of a channel, 0 when it has none.
    async fn newest(&self, channel: Snowflake) -> Result<i64, RestError> {
        let page = self.rest.messages(channel, None, 1).await?;
        Ok(page
            .first()
            .and_then(types::ids)
            .map_or(0, |(id, _)| id.seq()))
    }

    /// First run: a listed channel's position is its newest message (DISC-IN req. 30).
    async fn baseline_listed(&self) -> Result<(), Stop> {
        let known: HashSet<String> = self
            .store
            .read(|r| r.positions(NAME))
            .await
            .map_err(|e| Stop::Retry(e.to_string()))?
            .into_iter()
            .map(|p| p.room)
            .collect();
        let rooms: Vec<Snowflake> = self
            .scope()
            .listed
            .iter()
            .filter(|(c, l)| !l.forum() && !known.contains(&c.to_string()))
            .map(|(c, _)| *c)
            .collect();
        for channel in rooms {
            let position = self
                .newest(channel)
                .await
                .map_err(|e| rest_stop(&format!("reading channel {channel}"), &e))?;
            self.write(RelayWrite::Baseline {
                relay: NAME.into(),
                room: channel.to_string(),
                position,
                parent: None,
            })
            .await
            .map_err(Stop::Retry)?;
        }
        Ok(())
    }

    pub(super) fn settings(&self) -> MutexGuard<'_, DiscordSettings> {
        self.settings.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether `new` differs from the running settings in the watched channels and home only.
    fn rooms_only(&self, new: &DiscordSettings) -> bool {
        let now = self.settings();
        let mut same = new.clone();
        same.channels.clone_from(&now.channels);
        same.home = now.home;
        same == *now
    }

    /// Applies a new list of watched channels on the running connection (the ops panel's picker
    /// or `config reload`). An added channel is checked like a listed one at start and read from
    /// the moment the config was applied (or its newest message, if older), so a message sent
    /// while this runs is caught up; its threads are followed from the next listing, which the
    /// catch-up read starts. A removed channel leaves scope at once with its threads, so its
    /// queued messages are dropped.
    async fn rewatch(&self, new: &DiscordSettings) -> Result<(), Stop> {
        let unchanged = {
            let now = self.settings();
            now.channels == new.channels && now.home == new.home
        };
        if unchanged {
            return Ok(());
        }
        let added: Vec<Snowflake> = {
            let scope = self.scope();
            new.channels
                .iter()
                .filter(|c| !scope.listed.contains_key(c))
                .copied()
                .collect()
        };
        let found = listed(&self.rest, &added).await?;
        let applied = Snowflake::at_ms(self.jobs.ctx().config_applied_ms()) - 1;
        for (channel, kind) in &found.listed {
            if kind.forum() {
                continue;
            }
            let newest = self
                .newest(*channel)
                .await
                .map_err(|e| rest_stop(&format!("reading channel {channel}"), &e))?;
            let position = newest.min(applied);
            let room = channel.to_string();
            // A channel watched again starts afresh too: its position moves up to this one.
            for write in [
                RelayWrite::Baseline {
                    relay: NAME.into(),
                    room: room.clone(),
                    position,
                    parent: None,
                },
                RelayWrite::Pass {
                    relay: NAME.into(),
                    room: room.clone(),
                    seq: position,
                },
            ] {
                self.write(write).await.map_err(Stop::Retry)?;
            }
        }
        {
            let mut scope = self.scope();
            let Scope {
                listed,
                threads,
                lookups,
                listed_once,
            } = &mut *scope;
            listed.retain(|c, _| new.channels.contains(c));
            listed.extend(found.listed);
            threads.retain(|_, parent| listed.contains_key(parent));
            lookups.clear();
            // Threads under an added channel are met at their newest message, as on a first run.
            *listed_once &= added.is_empty();
        }
        {
            let mut settings = self.settings();
            settings.channels.clone_from(&new.channels);
            settings.home = new.home;
        }
        log::info(
            "Discord watched channels changed",
            &[("channels", &new.channels.len()), ("added", &added.len())],
        );
        self.flag(true).await;
        self.wake.notify_one();
        Ok(())
    }

    /// Runs the connection and the handling round until something ends them.
    async fn serve(
        self: Arc<Self>,
        mut shutdown: watch::Receiver<bool>,
        mut config: watch::Receiver<Arc<Config>>,
    ) -> Stop {
        let gateway = Gateway {
            url: self.gateway_url.clone(),
            token: self.token_intents.0.clone(),
            intents: self.token_intents.1,
            ops: self.active_tap().map(|t| Arc::clone(&t.ops)),
        };
        let (fatal_tx, mut fatal_rx) = tokio::sync::oneshot::channel();
        // The connection ends with this run, so a restarted intake never meets an old one.
        let _connection = {
            let me = Arc::clone(&self);
            let stop = shutdown.clone();
            AbortOnDrop(tokio::spawn(async move {
                let end = gateway.run(&me, stop).await;
                let _ = fatal_tx.send(end);
            }))
        };
        self.shared.set(|h| h.state = "connecting".into());
        let mut events = self.lead.follow();
        loop {
            self.round().await;
            let tick = if self.busy().await { TICK } else { IDLE_TICK };
            tokio::select! {
                () = self.wake.notified() => {}
                event = events.recv() => match event {
                    Ok(event) => self.on_event(&event).await,
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => events = self.lead.follow(),
                },
                () = tokio::time::sleep(tick) => {}
                end = &mut fatal_rx => {
                    return match end {
                        Ok(Some(reason)) => Stop::Fatal(reason),
                        _ => Stop::Shutdown,
                    };
                }
                _ = config.changed() => {
                    let config = Arc::clone(&config.borrow_and_update());
                    let voice = self.tap.0.as_ref().is_some_and(|t| (t.wanted)(&config));
                    if voice != self.tap.1 || !self.rooms_only(&config.discord) {
                        return Stop::Again;
                    }
                    if let Err(stop) = self.rewatch(&config.discord).await {
                        return stop;
                    }
                }
                () = self.shared.kick.notified() => return Stop::Again,
                _ = shutdown.changed() => return Stop::Shutdown,
            }
        }
    }

    /// Whether the round should come back soon: queue, answers, typing or catch-up work is open.
    async fn busy(&self) -> bool {
        self.turn_running.load(Ordering::Acquire)
            || self
                .store
                .read(|r| {
                    Ok(!r.queue(NAME)?.is_empty()
                        || !r.answers_open(NAME)?.is_empty()
                        || r.flag(NAME, "catch_up")?)
                })
                .await
                .unwrap_or(true)
    }

    async fn on_event(self: &Arc<Self>, event: &SessionEvent) {
        self.pipeline.on_event(event);
        match event {
            SessionEvent::TurnStarted => {
                self.turn_running.store(true, Ordering::Release);
                self.turn_ended.store(false, Ordering::Release);
                self.typing().await;
            }
            SessionEvent::TurnEnded {
                will_continue: false,
            }
            | SessionEvent::Settled { .. }
            | SessionEvent::Quiet => {
                self.turn_running.store(false, Ordering::Release);
                self.turn_ended.store(true, Ordering::Release);
                *self.typed.lock().unwrap_or_else(PoisonError::into_inner) = None;
                self.wake.notify_one();
            }
            SessionEvent::DialogOpened { .. }
            | SessionEvent::DialogClosed { .. }
            | SessionEvent::Stopped { .. } => {
                self.wake.notify_one();
            }
            _ => {}
        }
    }

    /// The handling round: catch-up when owed, the queue, answers, questions, typing.
    async fn round(self: &Arc<Self>) {
        if self
            .store
            .read(|r| r.flag(NAME, "catch_up"))
            .await
            .unwrap_or(false)
        {
            self.catch_up().await;
        }
        if let Err(e) = self.drain().await {
            log::warn("Discord queue not drained", &[("error", &e)]);
        }
        let turn_over =
            !self.turn_running.load(Ordering::Acquire) && self.turn_ended.load(Ordering::Acquire);
        if let Err(e) = self.pipeline.answers(turn_over).await {
            log::warn("Discord answers not posted", &[("error", &e)]);
        }
        self.sync_cards().await;
        self.typing().await;
    }

    /// Renews the typing indicator while a lead turn runs (DESIGN 12.2 C4).
    async fn typing(&self) {
        if !self.turn_running.load(Ordering::Acquire) {
            return;
        }
        let due = self
            .typed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none_or(|t| t.elapsed() >= TYPING_EVERY);
        if !due {
            return;
        }
        let Some(room) = self.pipeline.typing_room().await else {
            return;
        };
        *self.typed.lock().unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
        if let Err(e) = self.pipeline.relay.typing(&room).await {
            log::debug("typing not shown", &[("error", &e)]);
        }
    }

    /// Lists active threads and follows those under listed channels (DISC-IN req. 8).
    async fn refresh_threads(&self) -> Result<(), RestError> {
        if !self.settings().follow_threads {
            return Ok(());
        }
        let guilds: HashSet<Snowflake> = self
            .scope()
            .listed
            .values()
            .filter_map(|l| l.guild)
            .collect();
        let mut found: Vec<(Snowflake, Snowflake)> = Vec::new();
        for guild in guilds {
            let v = self.rest.active_threads(guild).await?;
            for t in v
                .get("threads")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let archived = t
                    .get("thread_metadata")
                    .and_then(|m| m.get("archived"))
                    .and_then(Value::as_bool)
                    == Some(true);
                if let (Some(id), Some(parent)) = (
                    str_field(t, "id").and_then(Snowflake::parse),
                    str_field(t, "parent_id").and_then(Snowflake::parse),
                ) && !archived
                    && self.scope().listed.contains_key(&parent)
                {
                    found.push((id, parent));
                }
            }
        }
        if found.len() > THREADS_MAX {
            log::warn(
                "too many active threads; following held this round",
                &[("threads", &found.len())],
            );
            self.shared.set(|h| {
                h.last_error = Some(format!(
                    "{} active threads exceed 128: only listed channels are read",
                    found.len()
                ));
            });
            self.scope().threads.clear();
            return Ok(());
        }
        let first = !self.scope().listed_once;
        let known: HashSet<String> = self.store.read(|r| r.positions(NAME)).await.map_or_else(
            |_| HashSet::new(),
            |p| p.into_iter().map(|p| p.room).collect(),
        );
        for (thread, parent) in &found {
            if !known.contains(&thread.to_string()) {
                let position = if first {
                    self.newest(*thread).await?
                } else {
                    thread.seq().max(Snowflake::at_ms(self.started_ms))
                };
                let _ = self
                    .write(RelayWrite::Baseline {
                        relay: NAME.into(),
                        room: thread.to_string(),
                        position,
                        parent: Some(parent.to_string()),
                    })
                    .await;
            }
        }
        let mut scope = self.scope();
        scope.threads = found.into_iter().collect();
        scope.listed_once = true;
        let count = scope.threads.len();
        drop(scope);
        self.shared.set(|h| h.followed_threads = count);
        Ok(())
    }

    /// The catch-up read of every room in scope, one at a time (DISC-IN req. 23, 31-34, 62).
    async fn catch_up(self: &Arc<Self>) {
        {
            let backoff = self
                .catch_up_backoff
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if backoff.1.is_some_and(|at| Instant::now() < at) {
                return;
            }
        }
        let result = async {
            self.refresh_threads().await?;
            let rooms: Vec<(Snowflake, Option<Snowflake>)> = {
                let scope = self.scope();
                let listed = scope
                    .listed
                    .iter()
                    .filter(|(_, l)| !l.forum())
                    .map(|(c, _)| (*c, None));
                let threads = scope.threads.iter().map(|(t, p)| (*t, Some(*p)));
                listed.chain(threads).collect()
            };
            for (room, parent) in rooms {
                match self.read_room(room, parent).await {
                    Err(e) if parent.is_some() && matches!(e.status, Some(403 | 404)) => {
                        // A followed thread that cannot be read leaves scope until listed again.
                        self.scope().threads.remove(&room);
                    }
                    other => other?,
                }
            }
            Ok::<(), RestError>(())
        }
        .await;
        match result {
            Ok(()) => {
                self.flag(false).await;
                *self
                    .catch_up_backoff
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = (CATCH_UP_FIRST, None);
                if let Some(at) = self
                    .established
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take()
                {
                    self.shared.catch_up_latency.record(at.elapsed());
                }
                self.shared.set(|h| h.caught_up = true);
            }
            Err(e) if e.status == Some(401) => {
                self.shared.set(|h| {
                    h.last_error = Some(format!("catch-up read: the token was refused ({e})"));
                });
                self.shared.kick.notify_one();
            }
            Err(e) => {
                let mut backoff = self
                    .catch_up_backoff
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let wait = backoff.0;
                *backoff = ((wait * 2).min(CATCH_UP_MAX), Some(Instant::now() + wait));
                drop(backoff);
                log::info(
                    "catch-up read retried later",
                    &[("error", &e), ("after_s", &wait.as_secs())],
                );
                self.shared
                    .set(|h| h.last_error = Some(format!("catch-up read: {e}")));
            }
        }
    }

    /// Reads one room from its position forward and queues or passes what it finds (DISC-IN req. 31).
    async fn read_room(&self, room: Snowflake, parent: Option<Snowflake>) -> Result<(), RestError> {
        let positions = self
            .store
            .read(|r| r.positions(NAME))
            .await
            .unwrap_or_default();
        let Some(pos) = positions.iter().find(|p| p.room == room.to_string()) else {
            let position = self.newest(room).await?;
            let _ = self
                .write(RelayWrite::Baseline {
                    relay: NAME.into(),
                    room: room.to_string(),
                    position,
                    parent: parent.map(|p| p.to_string()),
                })
                .await;
            return Ok(());
        };
        if pos.held.is_some() {
            return Ok(());
        }
        let mut gathered: Vec<Value> = Vec::new();
        let mut before: Option<Snowflake> = None;
        let mut pages = 0;
        loop {
            let page = self.rest.messages(room, before, 100).await?;
            let len = page.len();
            let mut reached = false;
            let mut oldest: Option<Snowflake> = None;
            for m in page {
                let Some((id, _)) = types::ids(&m) else {
                    continue;
                };
                if before.is_some_and(|b| id >= b) || oldest.is_some_and(|o| id >= o) {
                    return Err(RestError {
                        status: None,
                        code: None,
                        why: "a history page was not strictly older than the previous one".into(),
                        uncertain: false,
                    });
                }
                oldest = Some(id);
                if id.seq() <= pos.position {
                    reached = true;
                } else {
                    gathered.push(m);
                }
            }
            pages += 1;
            if reached || len < 100 {
                break;
            }
            if pages >= PAGES_MAX {
                let reason = "over 10 000 unread messages; the owner may rebaseline it".to_string();
                let _ = self
                    .write(RelayWrite::Hold {
                        relay: NAME.into(),
                        room: room.to_string(),
                        reason: Some(reason.clone()),
                    })
                    .await;
                self.alert_held(room, &reason);
                return Ok(());
            }
            before = oldest;
        }
        gathered.reverse();
        for m in gathered {
            self.take(&m, room, parent, None).await;
        }
        Ok(())
    }

    fn alert_held(&self, room: Snowflake, reason: &str) {
        (self.events)(
            Emit::new(EventKind::RelayHeld, Severity::Warning, true, NAME)
                .with("relay", NAME)
                .with("room", room)
                .with("reason", reason),
        );
    }

    /// Queues an owner message or passes anything else (DISC-IN req. 12-14, 22, 24). `read_at` is a
    /// live message's frame read time, `None` for one found by a catch-up read. A queued message
    /// wakes the round from the pipeline ([`Pipeline::arrive`]).
    async fn take(
        &self,
        raw: &Value,
        room: Snowflake,
        parent: Option<Snowflake>,
        read_at: Option<Instant>,
    ) {
        let Some((message, _)) = types::ids(raw) else {
            return;
        };
        let gate = types::gate(raw, self.owner);
        let others = self.settings().others.contains(&room);
        if others {
            self.track_others(raw, room, message, gate == Gate::Pass)
                .await;
        }
        match gate {
            Gate::Pass => {
                let _ = self
                    .write(RelayWrite::Pass {
                        relay: NAME.into(),
                        room: room.to_string(),
                        seq: message.seq(),
                    })
                    .await;
            }
            Gate::NoContent => {
                let reason = "an owner message arrived without text: enable Message Content Intent"
                    .to_string();
                let _ = self
                    .write(RelayWrite::Hold {
                        relay: NAME.into(),
                        room: room.to_string(),
                        reason: Some(reason.clone()),
                    })
                    .await;
                self.alert_held(room, &reason);
            }
            Gate::Owner => {
                let arrival = Arrival {
                    room: room.to_string(),
                    message: message.to_string(),
                    seq: message.seq(),
                    parent: parent.map(|p| p.to_string()),
                    raw: raw.to_string(),
                };
                if let Err(e) = self.pipeline.arrive(arrival, read_at, &self.wake).await {
                    log::warn("owner message not queued", &[("error", &e)]);
                    self.flag(true).await;
                }
            }
        }
    }

    /// A live message (DISC-IN req. 22): scope, gate, then the durable queue, which the hand-off
    /// may overlap (decision 15, [`Pipeline::arrive`]).
    async fn arrive(&self, raw: Value, read_at: Instant) {
        let Some((_, channel)) = types::ids(&raw) else {
            return;
        };
        let room = self.scope().room(channel);
        let parent = match room {
            InScope::Listed => None,
            InScope::Thread(parent) => Some(parent),
            InScope::No => match self.adopt(channel, &raw).await {
                Some(parent) => Some(parent),
                None => return,
            },
        };
        self.take(&raw, channel, parent, Some(read_at)).await;
        self.shared.set(|h| h.last_event_ms = Some(now_ms()));
    }

    /// An owner message from an unknown channel: one bounded lookup; a thread under a listed parent
    /// is adopted (DISC-IN req. 10). Results are cached for 256 channels.
    async fn adopt(&self, channel: Snowflake, raw: &Value) -> Option<Snowflake> {
        if !self.settings().follow_threads || types::gate(raw, self.owner) != Gate::Owner {
            return None;
        }
        let cached = {
            let mut scope = self.scope();
            let hit = scope.lookups.iter().position(|(c, _)| *c == channel);
            hit.and_then(|i| scope.lookups.remove(i))
                .inspect(|e| scope.lookups.push_back(*e))
        };
        let parent = if let Some((_, parent)) = cached {
            parent
        } else {
            {
                let parent =
                    match tokio::time::timeout(LOOKUP_TIMEOUT, self.rest.channel(channel)).await {
                        Ok(Ok(info)) => {
                            let thread =
                                matches!(info.get("type").and_then(Value::as_u64), Some(10..=12));
                            str_field(&info, "parent_id")
                                .and_then(Snowflake::parse)
                                .filter(|p| thread && self.scope().listed.contains_key(p))
                        }
                        _ => None,
                    };
                let mut scope = self.scope();
                if scope.lookups.len() >= LOOKUPS_MAX {
                    scope.lookups.pop_front();
                }
                scope.lookups.push_back((channel, parent));
                parent
            }
        }?;
        let mark = channel.seq().max(Snowflake::at_ms(self.started_ms));
        let message = types::ids(raw).map_or(i64::MAX, |(m, _)| m.seq());
        let _ = self
            .write(RelayWrite::Baseline {
                relay: NAME.into(),
                room: channel.to_string(),
                position: mark.min(message - 1),
                parent: Some(parent.to_string()),
            })
            .await;
        self.scope().threads.insert(channel, parent);
        Some(parent)
    }

    /// Thread changes (DISC-IN req. 9): a new thread under a listed parent is followed from its
    /// start; an archived or deleted one is no longer followed (its queue still drains).
    async fn thread_changed(&self, kind: &str, data: &Value) {
        if !self.settings().follow_threads {
            return;
        }
        let Some(thread) = str_field(data, "id").and_then(Snowflake::parse) else {
            return;
        };
        let parent = str_field(data, "parent_id").and_then(Snowflake::parse);
        let archived = data
            .get("thread_metadata")
            .and_then(|m| m.get("archived"))
            .and_then(Value::as_bool)
            == Some(true);
        if kind == "THREAD_DELETE" || archived {
            self.scope().threads.remove(&thread);
            return;
        }
        let Some(parent) = parent.filter(|p| self.scope().listed.contains_key(p)) else {
            return;
        };
        if self.scope().threads.insert(thread, parent).is_none() {
            let _ = self
                .write(RelayWrite::Baseline {
                    relay: NAME.into(),
                    room: thread.to_string(),
                    position: thread.seq().max(Snowflake::at_ms(self.started_ms)),
                    parent: Some(parent.to_string()),
                })
                .await;
        }
    }

    /// Hands queued messages on, lowest first per room (DISC-IN req. 26-27, 40, 43, 49).
    async fn drain(self: &Arc<Self>) -> Result<(), String> {
        let me = Arc::clone(self);
        self.pipeline
            .drain(move |row| {
                let me = Arc::clone(&me);
                async move {
                    if !me.in_scope(&row) {
                        log::info("queued message left scope; dropped", &[("room", &row.room)]);
                        me.pipeline.finish(&row.room, &row.message, None).await?;
                        return Ok(Step::Done);
                    }
                    me.handle(&row).await
                }
            })
            .await
    }

    fn in_scope(&self, row: &QueueRow) -> bool {
        let scope = self.scope();
        let listed = |s: &str| Snowflake::parse(s).is_some_and(|c| scope.listed.contains_key(&c));
        listed(&row.room) || row.parent.as_deref().is_some_and(listed)
    }

    async fn handle(self: &Arc<Self>, row: &QueueRow) -> Result<Step, String> {
        let read_at = self.pipeline.read_at(&row.room, &row.message);
        let inbound = match self.pipeline.relay.normalise(row) {
            Ok(i) => i,
            Err(fault) => {
                log::warn(
                    "owner message unreadable; skipped",
                    &[("message", &row.message), ("fault", &fault)],
                );
                self.pipeline.finish(&row.room, &row.message, None).await?;
                return Ok(Step::Done);
            }
        };
        if let Some(step) = self.pipeline.resume(row, &inbound, read_at).await? {
            return Ok(step);
        }
        // An answer to an open question goes there first (DISC-IN req. 40; DISC-OUT req. 71).
        if self.answer_by_text(row, &inbound).await {
            self.pipeline.taken(row).await?;
            return Ok(Step::Done);
        }
        let inbound = self.ledger_answer(row, inbound).await;
        let previous = self
            .store
            .read(|r| r.meta("owner_last_ms"))
            .await
            .ok()
            .flatten()
            .and_then(|v| v.parse::<i64>().ok());
        let away = crate::jobs::duration_ms(self.settings().away);
        let step = self.pipeline.hand_off(row, &inbound, read_at, None).await;
        if let Some(previous) = previous
            && now_ms() - previous >= away
        {
            self.digest(previous);
        }
        step
    }
}

/// The bot account and application checks (DISC-IN req. 1-2; DISC-OUT req. 68): the bot ID, the
/// application ID, and whether buttons arrive over the Gateway.
async fn account(rest: &Rest, owner: Snowflake) -> Result<(Snowflake, Snowflake, bool), Stop> {
    let me = rest
        .me()
        .await
        .map_err(|e| rest_stop("reading the bot account", &e))?;
    let bot = str_field(&me, "id")
        .and_then(Snowflake::parse)
        .ok_or_else(|| Stop::Retry("Discord named no bot ID".into()))?;
    if me.get("bot").and_then(Value::as_bool) != Some(true) {
        return Err(Stop::Fatal("the token is not a bot account".into()));
    }
    if bot == owner {
        return Err(Stop::Fatal("the configured owner is the bot itself".into()));
    }
    let app = rest
        .application()
        .await
        .map_err(|e| rest_stop("reading the application", &e))?;
    let flags = app.get("flags").and_then(Value::as_u64).unwrap_or(0);
    if flags & ((1 << 18) | (1 << 19)) == 0 {
        return Err(Stop::Fatal(
            "enable Message Content Intent for the bot in the Discord Developer Portal".into(),
        ));
    }
    let application = str_field(&app, "id")
        .and_then(Snowflake::parse)
        .unwrap_or(bot);
    let buttons = str_field(&app, "interactions_endpoint_url").is_none_or(str::is_empty);
    Ok((bot, application, buttons))
}

/// The listed channels with their kinds and guilds; one that cannot be read stops intake (DISC-IN
/// req. 11).
async fn listed(rest: &Rest, channels: &[Snowflake]) -> Result<Scope, Stop> {
    let mut scope = Scope::default();
    for channel in channels {
        let info = match rest.channel(*channel).await {
            Ok(v) => v,
            Err(e) if matches!(e.status, Some(403 | 404)) => {
                return Err(Stop::Fatal(format!(
                    "channel {channel} cannot be read ({e}): the bot needs View Channel and Read \
                     Message History there"
                )));
            }
            Err(e) => return Err(rest_stop(&format!("reading channel {channel}"), &e)),
        };
        scope.listed.insert(
            *channel,
            Listed {
                kind: info.get("type").and_then(Value::as_u64).unwrap_or(0),
                guild: str_field(&info, "guild_id").and_then(Snowflake::parse),
            },
        );
    }
    Ok(scope)
}

impl<T: Runtime> Handler for Arc<Intake<T>> {
    async fn established(&self, resumed: bool) {
        *self
            .established
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
        self.flag(true).await;
        *self
            .catch_up_backoff
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = (CATCH_UP_FIRST, None);
        self.shared.set(|h| {
            h.state = "connected".into();
            h.connected_ms = Some(now_ms());
            h.caught_up = false;
            h.reason = None;
        });
        log::info("Discord Gateway ready", &[("resumed", &resumed)]);
        if let (Some(tap), Ok(bot)) = (self.active_tap(), self.bot.to_string().parse::<u64>()) {
            (tap.seen)(Seen::Up(bot));
        }
        self.wake.notify_one();
    }

    async fn dispatch(&self, kind: &str, data: Value, read_at: Instant) {
        match kind {
            "MESSAGE_CREATE" => self.arrive(data, read_at).await,
            "THREAD_CREATE" | "THREAD_UPDATE" | "THREAD_DELETE" => {
                self.thread_changed(kind, &data).await;
            }
            "INTERACTION_CREATE" => {
                if let Some(press) = types::Press::parse(&data) {
                    let me = Arc::clone(self);
                    tokio::spawn(async move { me.press(press).await });
                }
            }
            kind if DISPATCHES.contains(&kind) => {
                if let Some(tap) = self.active_tap() {
                    (tap.seen)(Seen::Dispatch(kind, &data));
                }
            }
            // Edits and deletions are never delivered (DISC-IN req. 15).
            _ => {}
        }
    }

    fn link(&self, down: Option<&str>) {
        if let (Some(tap), Some(_)) = (self.active_tap(), down) {
            (tap.seen)(Seen::Down);
        }
        self.shared.set(|h| match down {
            Some(why) => {
                if h.state == "connected" {
                    h.state = "connecting".into();
                }
                h.caught_up = false;
                h.last_error = Some(why.to_string());
            }
            None => h.state = "connected".into(),
        });
    }
}
