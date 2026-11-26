//! The ops room (`[ops]`): one live panel message and a few alerts, in the owner's language.
//!
//! Events routed to the ops destination are not posted as lines: the router hands them here. The
//! panel is the board post rule applied to an embed (created once with its banner and thumbnail,
//! pinned when the bot may, edited in place only on change and at most once a minute, never
//! recreated after an uncertain create). Alerts are separate small embeds for what is worth a ping:
//! the host over the alert level for five minutes, a quota hold, a job failing twice in a row, a
//! refused backup, GitHub news and new mail. An alert whose condition clears is edited to show it
//! resolved. Each alert kind posts at most once an hour; the owner's mute silences new alerts while
//! the panel keeps updating. Owner-only buttons refresh the panel, show the usage table and mute.

pub(crate) mod render;

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Notify;

use self::render::{Host, HostState, Issue, Level, Limit, Panel, People, Recent};
use super::{Ctx, Jobs, JobsError};
use crate::config::ops::{AlertKind, OpsSettings};
use crate::config::{Config, Destination, DiscordSettings, Language, Owner, Work, file};
use crate::jobs::owner::pressure::{self, ALERT_AFTER_MS};
use crate::lanes::BoxFut;
use crate::lanes::board::{self, Board, EVERY_MS, Post};
use crate::lanes::docs::{self, BOARD};
use crate::log;
use crate::proto::JobView;
use crate::relay::discord::{Rest, Snowflake, Upload, nonce};
use crate::store::{EventRecord, Store};
use crate::sys::now_ms;

/// Coordination record kind of the ops room's state and the owner's mute.
const STATE: &str = "ops";
const MUTE_ID: &str = "mute";
/// Samples and checks this often when nothing wakes the panel earlier.
const EVERY: Duration = Duration::from_secs(30);
const HOUR_MS: i64 = 3_600_000;
const RECENT_KEEP: usize = 3;
const INBOX_MAX: usize = 256;
/// Resolved alerts are forgotten after a day.
const RESOLVED_KEEP_MS: i64 = 24 * HOUR_MS;
/// Failed runs or ticks in a row that make a job alert.
const JOB_FAILURES: i64 = 2;

/// The lead and lanes as the panel shows them; set by the hub once both exist.
pub(crate) type PeopleFn = Arc<dyn Fn() -> BoxFut<'static, People> + Send + Sync>;

/// The hand-off between the router, the buttons and the panel task.
#[derive(Default)]
pub(crate) struct Ops {
    inbox: Mutex<VecDeque<EventRecord>>,
    wake: Notify,
    people: OnceLock<PeopleFn>,
    /// The owner asked for a refresh.
    refresh: AtomicBool,
    /// The last reason the panel could not be kept, logged once.
    trouble: Mutex<Option<String>>,
    /// Whether the panel is pinned, for this hub run.
    pin: Mutex<PinTrack>,
}

impl Ops {
    /// An event for the ops room (from the router).
    pub(crate) fn take(&self, event: EventRecord) {
        {
            let mut inbox = self.inbox.lock().unwrap_or_else(PoisonError::into_inner);
            if inbox.len() >= INBOX_MAX {
                inbox.pop_front();
            }
            inbox.push_back(event);
        }
        self.wake.notify_one();
    }

    pub(crate) fn set_people(&self, people: PeopleFn) {
        let _ = self.people.set(people);
    }

    fn drain(&self) -> Vec<EventRecord> {
        self.inbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
            .collect()
    }

    fn trouble(&self, why: Option<String>) {
        let mut last = self.trouble.lock().unwrap_or_else(PoisonError::into_inner);
        if *last != why {
            if let Some(w) = &why {
                log::warn("ops panel not kept", &[("reason", w)]);
            }
            *last = why;
        }
    }
}

/// The ops room's own record: alerts, when each kind last posted, and the recent list.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct State {
    alerts: BTreeMap<String, Alert>,
    posted: BTreeMap<String, i64>,
    recent: Vec<Recent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Alert {
    title: String,
    detail: String,
    opened_ms: i64,
    /// The alert post, when one was made.
    message: Option<String>,
    resolved_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Mute {
    until_ms: Option<i64>,
}

/// What an event means for the alerts.
enum Act {
    Open(String, AlertKind),
    Close(String),
    Once(AlertKind),
    Nothing,
}

fn act(e: &EventRecord) -> Act {
    let key = |what: &str| format!("{what}:{}", e.job);
    match e.kind.as_str() {
        "pressure.alert" => Act::Open("pressure".into(), AlertKind::Pressure),
        "pressure.clear" => Act::Close("pressure".into()),
        "quota.hold" => Act::Open(key("quota"), AlertKind::Quota),
        "quota.released" => Act::Close(key("quota")),
        "backup.refused" => Act::Open(key("backup"), AlertKind::Backup),
        "backup.clean" => Act::Close(key("backup")),
        "github.pr" | "github.notification" | "pr.feedback" | "pr.check_failed" | "pr.merged" => {
            Act::Once(AlertKind::Github)
        }
        "items.new" => Act::Once(AlertKind::Mail),
        _ => Act::Nothing,
    }
}

/// Events the recent list leaves out: the panel shows them already, or they are routine.
fn routine(e: &EventRecord) -> bool {
    matches!(
        e.kind.as_str(),
        "pressure.ready" | "pressure.busy" | "pr.baseline" | "pr.pushed" | "run.skipped"
    ) || (e.kind == "run.done" && e.severity == "info")
}

fn recent_of(e: &EventRecord) -> Recent {
    Recent {
        at_ms: e.at_ms,
        kind: e.kind.clone(),
        job: e.job.clone(),
        fields: e.fields.clone(),
        text: e.text.clone(),
    }
}

/// Facts an alert's words need besides its event.
#[derive(Debug, Clone, Default)]
pub(crate) struct Extra {
    /// The host line for a pressure alert.
    pub(crate) host: String,
    /// When the quota gate opens at the latest.
    pub(crate) quota_release_ms: Option<i64>,
}

/// One pass of the ops room for one destination, with time and messenger supplied.
pub(crate) struct Turn<'a, B: Board + ?Sized> {
    pub(crate) store: &'a Store,
    pub(crate) board: &'a B,
    pub(crate) settings: &'a OpsSettings,
    pub(crate) lang: Language,
    /// The destination name: the key of the room's state.
    pub(crate) dest: &'a str,
    pub(crate) channel: &'a str,
    pub(crate) now: i64,
    pub(crate) version: &'a str,
    /// The panel's pin state, kept for the hub run.
    pub(crate) pin: &'a Mutex<PinTrack>,
}

impl<B: Board + ?Sized> Turn<'_, B> {
    /// Takes the events, opens and resolves alerts, then keeps the panel. Returns when the panel is
    /// due again if a change waits for the minute.
    pub(crate) async fn run(
        &self,
        mut panel: Panel,
        events: Vec<EventRecord>,
        jobs: &[JobView],
        extra: &Extra,
        force: bool,
    ) -> Option<i64> {
        let mut state = match docs::get::<State>(self.store, STATE, self.dest).await {
            Ok(s) => s.map_or_default(|(s, _)| s),
            Err(e) => {
                log::warn("ops record unreadable", &[("error", &e)]);
                State::default()
            }
        };
        let before = state.clone();
        let muted = muted_until(self.store, self.now).await;
        for e in &events {
            let recent = recent_of(e);
            if !routine(e) {
                state.recent.push(recent.clone());
            }
            match act(e) {
                Act::Open(key, kind) => {
                    let (title, detail) = self.words(&recent, extra);
                    self.open(&mut state, &key, kind, &title, &detail, muted)
                        .await;
                }
                Act::Close(key) => self.close(&mut state, &key).await,
                Act::Once(kind) => {
                    let (title, detail) = self.words(&recent, extra);
                    self.post(
                        &mut state,
                        kind,
                        &title,
                        &detail,
                        self.now,
                        Level::Watch,
                        muted,
                    )
                    .await;
                }
                Act::Nothing => {}
            }
        }
        let excess = state.recent.len().saturating_sub(RECENT_KEEP);
        state.recent.drain(..excess);
        self.jobs(&mut state, jobs, muted).await;
        let now = self.now;
        state
            .alerts
            .retain(|_, a| a.resolved_ms.is_none_or(|r| now - r < RESOLVED_KEEP_MS));
        state.posted.retain(|_, at| now - *at < HOUR_MS);
        if state != before
            && let Err(e) = docs::put(self.store, STATE, self.dest, &state, None).await
        {
            log::warn("ops record not saved", &[("error", &e)]);
        }
        panel.recent.clone_from(&state.recent);
        panel.muted_until_ms = muted;
        let key = format!("ops:{}", self.channel);
        let message = self.message(&key).await;
        if let Some(m) = &message {
            panel.pin_refused = self.keep_pin(m).await;
        }
        let body = render::panel(&panel, self.lang, self.version).to_string();
        // The pin is kept below, retried while it fails.
        let due = board::publish(
            self.store,
            &NoPin(self.board),
            &key,
            self.channel,
            &body,
            now,
            force,
        )
        .await;
        if message.is_none()
            && let Some(m) = self.message(&key).await
        {
            self.keep_pin(&m).await;
        }
        due
    }

    /// The panel's message, once created.
    async fn message(&self, key: &str) -> Option<String> {
        match docs::get::<Post>(self.store, BOARD, key).await {
            Ok(found) => found.and_then(|(p, _)| p.message),
            Err(_) => None,
        }
    }

    /// Pins the panel while it is not known pinned: right away, then at most once an hour. Once
    /// pinned, an unpin by hand (seen at most once an hour) ends the tries for this hub run. Returns
    /// whether the panel is unpinned for lack of the permission.
    async fn keep_pin(&self, message: &str) -> bool {
        let lock = || self.pin.lock().unwrap_or_else(PoisonError::into_inner);
        let pinned = {
            let mut t = lock();
            if t.message.as_deref() != Some(message) {
                *t = PinTrack {
                    message: Some(message.to_string()),
                    refusals_logged: t.refusals_logged,
                    ..PinTrack::default()
                };
            }
            let due = t.last_ms.is_none_or(|at| self.now - at >= HOUR_MS);
            if t.stopped || !due {
                return t.refused();
            }
            t.last_ms = Some(self.now);
            t.pinned
        };
        if pinned {
            let shown = self.board.board_is_pinned(self.channel, message).await;
            let mut t = lock();
            if shown == Some(false) {
                t.stopped = true;
            }
            return t.refused();
        }
        let result = self.board.board_pin(self.channel, message).await;
        let mut t = lock();
        match result {
            Ok(()) => {
                t.pinned = true;
                t.refused_now = false;
            }
            Err((true, why)) => {
                t.refused_now = true;
                if t.refusals_logged == 0 {
                    t.refusals_logged += 1;
                    log::info(
                        "ops panel not pinned: the bot may not pin messages; tried again hourly",
                        &[("error", &why)],
                    );
                }
            }
            Err((false, why)) => log::info("ops panel not pinned", &[("error", &why)]),
        }
        t.refused()
    }
}

/// The panel's pin as this hub run knows it.
#[derive(Debug, Default)]
pub(crate) struct PinTrack {
    /// The message the rest is about.
    message: Option<String>,
    pinned: bool,
    /// Unpinned by hand after a pin: no more tries until the hub restarts.
    stopped: bool,
    /// The last try refused for lack of the permission.
    refused_now: bool,
    /// The last try or check.
    last_ms: Option<i64>,
    /// Refusals logged this run: at most one.
    refusals_logged: u32,
}

impl PinTrack {
    fn refused(&self) -> bool {
        self.refused_now && !self.pinned && !self.stopped
    }
}

/// A board whose posts are not pinned on create: the ops panel keeps its pin itself.
struct NoPin<'a, B: Board + ?Sized>(&'a B);

impl<B: Board + ?Sized> Board for NoPin<'_, B> {
    fn board_create<'a>(
        &'a self,
        channel: &'a str,
        text: &'a str,
        nonce: &'a str,
    ) -> BoxFut<'a, Result<String, (bool, String)>> {
        self.0.board_create(channel, text, nonce)
    }
    fn board_edit<'a>(
        &'a self,
        channel: &'a str,
        message: &'a str,
        text: &'a str,
    ) -> BoxFut<'a, Result<(), String>> {
        self.0.board_edit(channel, message, text)
    }
}

impl<B: Board + ?Sized> Turn<'_, B> {
    fn words(&self, r: &Recent, extra: &Extra) -> (String, String) {
        let title = render::line(r, self.lang);
        let detail = match r.kind.as_str() {
            "pressure.alert" => extra.host.clone(),
            "quota.hold" => match (self.lang, extra.quota_release_ms) {
                (Language::Korean, Some(at)) => format!(
                    "새 세션은 {}까지 기다려요. `cataly quota lift`로 풀 수 있어요.",
                    render::when(at, 'R')
                ),
                (Language::English, Some(at)) => format!(
                    "New sessions wait until {}. `cataly quota lift` lifts the hold.",
                    render::when(at, 'R')
                ),
                (Language::Korean, None) => "새 세션은 기다려요.".into(),
                (Language::English, None) => "New sessions wait.".into(),
            },
            _ => render::detail(r, self.lang),
        };
        (title, detail)
    }

    /// Opens an alert unless one is open under `key`; it is posted when allowed.
    async fn open(
        &self,
        state: &mut State,
        key: &str,
        kind: AlertKind,
        title: &str,
        detail: &str,
        muted: Option<i64>,
    ) {
        if state
            .alerts
            .get(key)
            .is_some_and(|a| a.resolved_ms.is_none())
        {
            return;
        }
        let opened = if kind == AlertKind::Pressure {
            // The host has been over the line for the five minutes before the alert.
            self.now - ALERT_AFTER_MS
        } else {
            self.now
        };
        let message = self
            .post(state, kind, title, detail, opened, Level::Act, muted)
            .await;
        state.alerts.insert(
            key.to_string(),
            Alert {
                title: title.to_string(),
                detail: detail.to_string(),
                opened_ms: opened,
                message,
                resolved_ms: None,
            },
        );
    }

    /// Marks the alert under `key` resolved and edits its post to say so.
    async fn close(&self, state: &mut State, key: &str) {
        let Some(alert) = state.alerts.get_mut(key) else {
            return;
        };
        if alert.resolved_ms.is_some() {
            return;
        }
        alert.resolved_ms = Some(self.now);
        let Some(message) = alert.message.clone() else {
            return;
        };
        let body = render::resolved(
            &alert.title,
            &alert.detail,
            alert.opened_ms,
            self.now,
            self.lang,
        );
        if let Err(e) = self
            .board
            .board_edit(self.channel, &message, &body.to_string())
            .await
        {
            log::warn("ops alert not marked resolved", &[("error", &e)]);
        }
    }

    /// Posts an alert when its kind is on, nothing is muted and the kind has not posted within
    /// the hour; returns the post.
    #[allow(clippy::too_many_arguments)]
    async fn post(
        &self,
        state: &mut State,
        kind: AlertKind,
        title: &str,
        detail: &str,
        opened: i64,
        level: Level,
        muted: Option<i64>,
    ) -> Option<String> {
        let recent = state
            .posted
            .get(kind.name())
            .is_some_and(|at| self.now - at < HOUR_MS);
        if !self.settings.alerts(kind) || muted.is_some() || recent {
            return None;
        }
        state.posted.insert(kind.name().to_string(), self.now);
        let body = render::alert(title, detail, opened, level, self.lang);
        let seed = format!("alert:{}:{}:{}", self.dest, kind.name(), self.now);
        match self
            .board
            .board_create(self.channel, &body.to_string(), &seed)
            .await
        {
            Ok(id) => Some(id),
            Err((_, why)) => {
                log::warn("ops alert not posted", &[("error", &why)]);
                None
            }
        }
    }

    /// A job routed here that failed twice in a row opens its alert; one that works again
    /// resolves it.
    async fn jobs(&self, state: &mut State, jobs: &[JobView], muted: Option<i64>) {
        for j in jobs
            .iter()
            .filter(|j| j.destinations.iter().any(|d| d == self.dest))
        {
            let key = format!("job:{}", j.name);
            if j.error_streak >= JOB_FAILURES {
                let r = Recent {
                    at_ms: self.now,
                    kind: "job.failed".into(),
                    job: j.name.clone(),
                    fields: None,
                    text: String::new(),
                };
                let title = render::line(&r, self.lang);
                let detail = render::job_detail(j.error_streak, j.last_error.as_deref(), self.lang);
                self.open(state, &key, AlertKind::Jobs, &title, &detail, muted)
                    .await;
            } else if j.error_streak == 0 {
                self.close(state, &key).await;
            }
        }
        let gone: Vec<String> = state
            .alerts
            .keys()
            .filter(|k| {
                k.strip_prefix("job:")
                    .is_some_and(|name| !jobs.iter().any(|j| j.name == name))
            })
            .cloned()
            .collect();
        for key in gone {
            self.close(state, &key).await;
        }
    }
}

async fn muted_until(store: &Store, now: i64) -> Option<i64> {
    match docs::get::<Mute>(store, STATE, MUTE_ID).await {
        Ok(Some((m, _))) => m.until_ms.filter(|u| *u > now),
        _ => None,
    }
}

/// The panel's messenger: embeds on Discord, the images uploaded with a create.
pub(crate) struct Embeds {
    pub(crate) rest: Arc<Rest>,
    /// The update time put on the panel.
    pub(crate) now: i64,
}

impl Embeds {
    fn body(&self, text: &str) -> Result<Value, String> {
        let mut body: Value =
            serde_json::from_str(text).map_err(|e| format!("not a message body: {e}"))?;
        render::stamp(&mut body, self.now);
        Ok(body)
    }
}

fn channel_id(text: &str) -> Result<Snowflake, String> {
    Snowflake::parse(text).ok_or_else(|| format!("'{text}' is not a Discord ID"))
}

impl Board for Embeds {
    fn board_create<'a>(
        &'a self,
        channel: &'a str,
        text: &'a str,
        seed: &'a str,
    ) -> BoxFut<'a, Result<String, (bool, String)>> {
        Box::pin(async move {
            let channel = channel_id(channel).map_err(|e| (false, e))?;
            let mut body = self.body(text).map_err(|e| (false, e))?;
            // Apart from the lanes' board nonces in the same channel.
            render::set(&mut body, "nonce", json!(nonce(&format!("ops:{seed}"), 0)));
            let mut files = Vec::new();
            for (name, bytes) in [
                (render::BANNER, render::BANNER_BYTES),
                (render::THUMB, render::THUMB_BYTES),
            ] {
                if text.contains(&format!("attachment://{name}")) {
                    files.push(Upload {
                        name,
                        mime: "image/jpeg",
                        bytes,
                    });
                }
            }
            let created = self
                .rest
                .create_with(channel, body, &files)
                .await
                .map_err(|e| (e.uncertain, e.why))?;
            created
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or((true, "no message ID".to_string()))
        })
    }

    fn board_edit<'a>(
        &'a self,
        channel: &'a str,
        message: &'a str,
        text: &'a str,
    ) -> BoxFut<'a, Result<(), String>> {
        Box::pin(async move {
            let (channel, message) = (channel_id(channel)?, channel_id(message)?);
            // Without `attachments` the edit keeps the uploaded images.
            self.rest
                .edit(channel, message, self.body(text)?)
                .await
                .map_err(|e| e.why)
        })
    }

    fn board_pin<'a>(
        &'a self,
        channel: &'a str,
        message: &'a str,
    ) -> BoxFut<'a, Result<(), (bool, String)>> {
        Box::pin(async move {
            let channel = channel_id(channel).map_err(|e| (false, e))?;
            let message = channel_id(message).map_err(|e| (false, e))?;
            self.rest.pin(channel, message).await.map_err(|e| {
                // Missing Permissions (50013) or plain Forbidden.
                let refused = e.code == Some(50013) || e.status == Some(403);
                (refused, e.to_string())
            })
        })
    }

    fn board_is_pinned<'a>(
        &'a self,
        channel: &'a str,
        message: &'a str,
    ) -> BoxFut<'a, Option<bool>> {
        Box::pin(async move {
            let (Ok(channel), Ok(message)) = (channel_id(channel), channel_id(message)) else {
                return None;
            };
            let found = self.rest.message(channel, message).await.ok()?;
            found.get("pinned").and_then(Value::as_bool)
        })
    }
}

/// The panel task: a pass on every hand-off and every 30 s.
pub(super) async fn run(ctx: Arc<Ctx>) -> Result<(), JobsError> {
    let mut shutdown = ctx.shutdown.clone();
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        pass(&ctx).await;
        tokio::select! {
            () = ctx.ops.wake.notified() => {}
            () = tokio::time::sleep(EVERY) => {}
            _ = shutdown.changed() => {}
        }
    }
}

/// The ops room's destination and its Discord client, or why there is none.
async fn room(
    ctx: &Ctx,
    config: &Config,
) -> Result<Option<(String, Arc<Rest>, Snowflake)>, String> {
    let Some(dest) = config.ops.destination.clone() else {
        return Ok(None);
    };
    let Some(Destination::Discord { channel }) = config.destination(&dest) else {
        return Ok(None);
    };
    let rest = ctx.discord(config)?;
    let channel = rest.resolve(&channel).await?;
    Ok(Some((dest, rest, channel)))
}

async fn pass(ctx: &Arc<Ctx>) {
    let config = ctx.config();
    let events = ctx.ops.drain();
    let (dest, rest, channel) = match room(ctx, &config).await {
        Ok(Some(r)) => r,
        Ok(None) => return ctx.ops.trouble(None),
        Err(e) => return ctx.ops.trouble(Some(e)),
    };
    ctx.ops.trouble(None);
    let now = now_ms();
    let jobs = match (Jobs {
        ctx: Arc::clone(ctx),
    })
    .list()
    .await
    {
        Ok(j) => j,
        Err(e) => {
            log::warn("ops panel: jobs unreadable", &[("error", &e)]);
            Vec::new()
        }
    };
    let (panel, extra) = gather(ctx, &config, &jobs, now).await;
    let board = Embeds { rest, now };
    let channel = channel.to_string();
    let turn = Turn {
        store: &ctx.store,
        board: &board,
        settings: &config.ops,
        lang: config.hub.language,
        dest: &dest,
        channel: &channel,
        now,
        version: env!("CARGO_PKG_VERSION"),
        pin: &ctx.ops.pin,
    };
    let force = ctx.ops.refresh.swap(false, Ordering::SeqCst);
    turn.run(panel, events, &jobs, &extra, force).await;
}

/// The panel's facts: the host, usage and the quota gate, people, and jobs.
async fn gather(ctx: &Ctx, config: &Config, jobs: &[JobView], now: i64) -> (Panel, Extra) {
    let job = config.jobs.values().find_map(|j| match &j.work {
        Work::Owner(Owner::Pressure(p)) => Some((j.name.clone(), p.clone())),
        _ => None,
    });
    let limits = job.as_ref().map_or_default(|(_, p)| p.clone());
    let sample = pressure::sample(&pressure::disk_path(&limits)).await;
    let people = match ctx.ops.people.get() {
        Some(f) => f().await,
        None => People::default(),
    };
    let state = match &job {
        Some((name, _)) => {
            let name = name.clone();
            let memory = ctx
                .store
                .read(move |r| r.memory(&name, false))
                .await
                .unwrap_or_default();
            let on = |c: &str| {
                memory
                    .iter()
                    .any(|(s, k, v)| s == "host" && k == c && v == "1")
            };
            if on("alert") {
                HostState::Alert
            } else if on("ready") {
                HostState::Ready
            } else {
                HostState::Busy
            }
        }
        None => match sample.load_percent {
            Some(l) if l > f64::from(limits.alert_load) => HostState::Alert,
            Some(l) if l < f64::from(limits.ready_load) => HostState::Ready,
            _ => HostState::Busy,
        },
    };
    let host = Host {
        load: sample.load_percent,
        memory_free: sample.memory_free_percent,
        disk_free_gb: sample.disk_free_gb,
        sessions: people.models.len(),
        max_sessions: limits.max_sessions,
        state,
    };
    let read = ctx
        .omp
        .usage
        .read(ctx.omp.path().as_deref(), config.limits.cache, false)
        .await;
    let gate = ctx.gate.view(now);
    let limits = read
        .snapshot
        .map(|s| worst_per_provider(&s.usage.limits, &gate.providers));
    let issues = jobs
        .iter()
        .filter(|j| j.error_streak > 0 || j.needs_ack)
        .map(|j| Issue {
            job: j.name.clone(),
            failures: j.error_streak,
            needs_ack: j.needs_ack,
        })
        .collect();
    let extra = Extra {
        host: render::host_line(&host, config.hub.language),
        quota_release_ms: gate.causes.iter().map(|c| c.release_ms).max(),
    };
    let panel = Panel {
        host,
        limits,
        held: gate.providers,
        people,
        jobs: jobs.len(),
        issues,
        recent: Vec::new(),
        muted_until_ms: None,
        mute_minutes: config.ops.mute.as_secs() / 60,
        pin_refused: false,
    };
    (panel, extra)
}

fn limit_of(l: &crate::omp::usage::Limit, held: &[String]) -> Limit {
    Limit {
        provider: l.provider.clone(),
        label: l.label.clone(),
        percent: l.used_percent,
        reset_ms: l.reset_ms,
        held: held.contains(&l.provider),
    }
}

/// One line per provider: its most used limit.
fn worst_per_provider(limits: &[crate::omp::usage::Limit], held: &[String]) -> Vec<Limit> {
    let mut out: Vec<Limit> = Vec::new();
    for l in limits {
        let line = limit_of(l, held);
        match out.iter_mut().find(|o| o.provider == line.provider) {
            Some(o) if o.percent.unwrap_or(-1.0) < line.percent.unwrap_or(-1.0) => *o = line,
            Some(_) => {}
            None => out.push(line),
        }
    }
    out
}

impl Jobs {
    /// The owner pressed a panel button (the caller checked it is the owner's press); returns the
    /// interaction answer.
    pub(crate) async fn ops_press(&self, action: &str, channel: Snowflake) -> Value {
        let ctx = &self.ctx;
        let config = ctx.config();
        let lang = config.hub.language;
        let now = now_ms();
        let t = |ko: &str, en: &str| match lang {
            Language::Korean => ko.to_string(),
            Language::English => en.to_string(),
        };
        match action {
            render::REFRESH => {
                let key = format!("ops:{channel}");
                let at = docs::get::<Post>(&ctx.store, BOARD, &key)
                    .await
                    .ok()
                    .flatten()
                    .map_or(0, |(p, _)| p.at_ms);
                let wait = (at + EVERY_MS - now).div_euclid(1000);
                if wait > 0 {
                    return render::private(&match lang {
                        Language::Korean => {
                            format!("⏳ 방금 갱신했어요. {wait}초 뒤에 다시 눌러 주세요.")
                        }
                        Language::English => {
                            format!("⏳ Just updated. Press again in {wait} s.")
                        }
                    });
                }
                ctx.ops.refresh.store(true, Ordering::SeqCst);
                ctx.ops.wake.notify_one();
                json!({ "type": 6 })
            }
            render::USAGE => {
                let snapshot = ctx.omp.usage.last();
                let gate = ctx.gate.view(now);
                let limits: Option<Vec<Limit>> = snapshot.as_ref().map(|s| {
                    s.usage
                        .limits
                        .iter()
                        .map(|l| limit_of(l, &gate.providers))
                        .collect()
                });
                render::usage_table(limits.as_deref(), snapshot.map(|s| s.fetched_ms), lang)
            }
            render::MUTE => {
                let muted = muted_until(&ctx.store, now).await.is_some();
                let until = (!muted)
                    .then(|| now + i64::try_from(config.ops.mute.as_millis()).unwrap_or(HOUR_MS));
                if let Err(e) =
                    docs::put(&ctx.store, STATE, MUTE_ID, &Mute { until_ms: until }, None).await
                {
                    log::warn("ops mute not saved", &[("error", &e)]);
                    return render::private(&t("저장하지 못했어요.", "Could not save that."));
                }
                ctx.ops.refresh.store(true, Ordering::SeqCst);
                ctx.ops.wake.notify_one();
                render::private(&match (lang, until) {
                    (_, None) => t("🔔 알림을 다시 켰어요.", "🔔 Alerts are on again."),
                    (Language::Korean, Some(u)) => format!(
                        "🔕 {} 동안 알림을 끌게요 ({}까지). 상황판은 계속 갱신돼요.",
                        render::minutes_words(config.ops.mute.as_secs() / 60, lang),
                        render::when(u, 't')
                    ),
                    (Language::English, Some(u)) => format!(
                        "🔕 Alerts muted for {} (until {}). The panel keeps updating.",
                        render::minutes_words(config.ops.mute.as_secs() / 60, lang),
                        render::when(u, 't')
                    ),
                })
            }
            render::WATCH => render::watch_picker(&config.discord.channels, lang),
            _ => render::private(&t("알 수 없는 버튼이에요.", "Unknown button.")),
        }
    }

    /// The owner submitted the watched-channel picker (the caller checked the owner and the
    /// server). The config file as it is now gets the chosen channels, then those the picker could
    /// not show, and a home that is still watched; the result is checked as `cataly config check`
    /// does and applied as `cataly config reload` does. An invalid result writes nothing.
    pub(crate) async fn watch_channels(&self, values: &[String]) -> Value {
        let lang = self.ctx.config().hub.language;
        let chosen: Vec<Snowflake> = values.iter().filter_map(|v| Snowflake::parse(v)).collect();
        if chosen.is_empty() {
            let why = match lang {
                Language::Korean => "고른 채널이 없어요.",
                Language::English => "no channel was chosen.",
            };
            return render::watch_failed(why, lang);
        }
        let mut applied = (Vec::new(), 0);
        let written = file::edit(&self.ctx.config_file, "", |text| {
            let now = crate::config::parse(text)
                .map_err(|e| {
                    format!(
                        "the config file is not valid now: {}",
                        crate::config::describe(&e)
                    )
                })?
                .discord;
            let (channels, home, kept) = picked(&now, &chosen);
            let mut text =
                file::set_rendered(text, "discord", "channels", &file::string_list(&channels));
            if let Some(home) = home {
                text = file::set_key(&text, "discord", "home", &home.to_string());
            }
            applied = (channels, kept);
            Ok(text)
        });
        if let Err(e) = written {
            log::warn("watched channels not written", &[("error", &e)]);
            return render::watch_failed(&e, lang);
        }
        match self.reload().await {
            Ok(_) => render::watching(&applied.0, applied.1, lang),
            Err(e) => render::watch_failed(&e, lang),
        }
    }
}

/// The watched list after the picker, the home to write (`None` leaves it) and how many channels
/// the picker could not show were kept. Those come after the chosen ones. The home stays while it
/// is watched; else, or when it was unset and several channels are now watched, it becomes the
/// current home if still watched, else the first chosen channel.
fn picked(
    now: &DiscordSettings,
    chosen: &[Snowflake],
) -> (Vec<Snowflake>, Option<Snowflake>, usize) {
    let mut channels: Vec<Snowflake> = Vec::with_capacity(chosen.len() + now.channels.len());
    for c in chosen {
        if !channels.contains(c) {
            channels.push(*c);
        }
    }
    let mut kept = 0;
    for c in now.channels.iter().skip(render::PICKER_MAX) {
        if !channels.contains(c) {
            channels.push(*c);
            kept += 1;
        }
    }
    let first = channels.first().copied();
    let home = match now.home {
        Some(h) if channels.contains(&h) => None,
        Some(_) => first,
        None if channels.len() > 1 => now.home().filter(|h| channels.contains(h)).or(first),
        None => None,
    };
    (channels, home, kept)
}

/// The refusal for anyone but the owner, in the owner's language.
pub(crate) fn refusal(lang: Language) -> &'static str {
    match lang {
        Language::Korean => "이 버튼은 주인만 누를 수 있어요.",
        Language::English => "Only the owner can use these buttons.",
    }
}

/// The refusal of a watched-channel change by anyone but the owner, or from another server.
pub(crate) fn watch_refusal(lang: Language) -> &'static str {
    match lang {
        Language::Korean => "감시 채널은 주인만 바꿀 수 있어요.",
        Language::English => "Only the owner may change the watched channels.",
    }
}

#[cfg(test)]
mod tests;
