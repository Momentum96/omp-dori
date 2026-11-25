//! The supervised voice task (`DESIGN.md` 5.2): it runs a call while the config holds valid voice
//! settings and the provider key reads, and stays off naming the field otherwise. It feeds voice
//! the configured guild's Gateway facts (VOICE-03), keeping the last READY and guild snapshot so a
//! call started after the connection came up still sees them, and answers `cataly voice status`,
//! `restart` and `reply`.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use cataly_voice::{Control, GatewayEvent, VoiceHost as _};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc, oneshot, watch};

use crate::config::Config;
use crate::config::voice::{VoiceConfig, VoiceOn};
use crate::host::Runtime;
use crate::host::voice::HubVoice;
use crate::log;
use crate::relay::discord::tap::{Seen, Tap};

/// A call winds down (accounting and summary) within this long after it is told to stop.
const WIND_DOWN: Duration = Duration::from_secs(90);
/// Gateway frames queued for voice at most.
const FEED: usize = 256;

/// The Gateway facts kept for the next call.
#[derive(Default)]
struct Gate {
    bot: Option<u64>,
    up: bool,
    /// Channel types of the configured guild, from its snapshot.
    channels: HashMap<u64, u64>,
    /// Voice states: user to (channel, voice session ID).
    states: BTreeMap<u64, (Option<u64>, String)>,
    snapshot: bool,
    /// The running call's feed and its voice channel.
    feed: Option<(mpsc::Sender<GatewayEvent>, u64)>,
}

impl Gate {
    fn guild_event(&self, channel: u64) -> GatewayEvent {
        GatewayEvent::Guild {
            channel_type: self.channels.get(&channel).copied(),
            voice_states: self
                .states
                .iter()
                .map(|(u, (c, s))| (*u, *c, s.clone()))
                .collect(),
        }
    }

    fn send(&self, event: GatewayEvent) {
        if let Some((tx, _)) = &self.feed
            && tx.try_send(event).is_err()
        {
            log::warn("voice Gateway feed full; event dropped", &[]);
        }
    }
}

/// Where voice stands, for `cataly voice status`.
#[derive(Debug, Clone)]
struct State {
    state: &'static str,
    reason: Option<String>,
}

/// The voice task's shared state.
pub(crate) struct Voice<R: Runtime> {
    config: watch::Receiver<Arc<Config>>,
    config_file: PathBuf,
    host: Arc<HubVoice<R>>,
    gate: Mutex<Gate>,
    control: Mutex<Option<mpsc::Sender<Control>>>,
    state: Mutex<State>,
    wake: Notify,
}

fn id(v: &Value) -> Option<u64> {
    v.as_str().and_then(|s| s.parse().ok())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How one call ended.
enum Ended {
    Shutdown,
    /// The voice settings changed: start again with the new ones.
    Changed,
    Failed(String),
}

#[derive(Deserialize)]
struct ReplyParams {
    request: Option<String>,
    subject: Option<String>,
    reply: Option<String>,
}

impl<R: Runtime> Voice<R> {
    pub(crate) fn new(
        config: watch::Receiver<Arc<Config>>,
        config_file: PathBuf,
        host: HubVoice<R>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            config_file,
            host: Arc::new(host),
            gate: Mutex::default(),
            control: Mutex::default(),
            state: Mutex::new(State {
                state: "off",
                reason: None,
            }),
            wake: Notify::new(),
        })
    }

    /// The tap for the Discord Gateway connection; `ops` receives the frames voice sends.
    pub(crate) fn tap(self: &Arc<Self>, ops: mpsc::Receiver<Value>) -> Arc<Tap> {
        let me = Arc::clone(self);
        Arc::new(Tap {
            wanted: Box::new(|c: &Config| matches!(c.voice, VoiceConfig::On(_))),
            seen: Box::new(move |s| me.seen(s)),
            ops: Arc::new(tokio::sync::Mutex::new(ops)),
        })
    }

    fn guild(&self) -> Option<u64> {
        self.config
            .borrow()
            .discord
            .guild
            .and_then(|g| g.to_string().parse().ok())
    }

    fn seen(&self, seen: Seen<'_>) {
        let guild = self.guild();
        let mut gate = lock(&self.gate);
        match seen {
            Seen::Up(bot) => {
                gate.bot = Some(bot);
                gate.up = true;
                gate.send(GatewayEvent::Ready { bot });
            }
            Seen::Down => {
                gate.up = false;
                gate.send(GatewayEvent::Down);
            }
            Seen::Dispatch(kind, d) => {
                let for_guild = |key: &str| guild.is_some() && id(&d[key]) == guild;
                match kind {
                    "GUILD_CREATE" if for_guild("id") => {
                        gate.channels = d["channels"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|c| Some((id(&c["id"])?, c["type"].as_u64()?)))
                            .collect();
                        gate.states = d["voice_states"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|v| {
                                Some((
                                    id(&v["user_id"])?,
                                    (id(&v["channel_id"]), v["session_id"].as_str()?.to_string()),
                                ))
                            })
                            .collect();
                        gate.snapshot = true;
                        if let Some((_, channel)) = gate.feed {
                            let event = gate.guild_event(channel);
                            gate.send(event);
                        }
                    }
                    "VOICE_STATE_UPDATE" if for_guild("guild_id") => {
                        let (Some(user), Some(session)) =
                            (id(&d["user_id"]), d["session_id"].as_str())
                        else {
                            return;
                        };
                        let channel = id(&d["channel_id"]);
                        gate.states.insert(user, (channel, session.to_string()));
                        gate.send(GatewayEvent::VoiceState {
                            user,
                            channel,
                            session_id: session.to_string(),
                        });
                    }
                    "VOICE_SERVER_UPDATE" if for_guild("guild_id") => {
                        let Some(token) = d["token"].as_str() else {
                            return;
                        };
                        gate.send(GatewayEvent::VoiceServer {
                            token: token.to_string(),
                            endpoint: d["endpoint"].as_str().map(str::to_string),
                        });
                    }
                    _ => {}
                }
            }
        }
    }

    fn set(&self, state: &'static str, reason: Option<String>) {
        *lock(&self.state) = State { state, reason };
    }

    /// The valid settings and the provider key, or why voice stays off, naming the field.
    fn prepare(&self, config: &Config) -> Result<(VoiceOn, String), String> {
        let on = match &config.voice {
            VoiceConfig::Off => return Err("voice.enabled is not true".into()),
            VoiceConfig::Refused(errors) => return Err(crate::config::describe(errors)),
            VoiceConfig::On(on) => (**on).clone(),
        };
        if !config.discord.intake() {
            return Err(
                "discord.channels: voice needs Discord intake ([discord] owner and channels)"
                    .into(),
            );
        }
        let path =
            crate::relay::intake::secret_path(on.key_file.as_ref(), &self.config_file, "voice-key");
        let key =
            crate::relay::intake::read_secret(&path).map_err(|e| format!("voice.key_file: {e}"))?;
        Ok((on, key))
    }

    /// The supervised task: one call at a time while the settings allow it.
    pub(crate) async fn run(
        self: Arc<Self>,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), String> {
        let mut config = self.config.clone();
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            let current = Arc::clone(&config.borrow_and_update());
            match self.prepare(&current) {
                Err(reason) => {
                    if matches!(current.voice, VoiceConfig::On(_) | VoiceConfig::Refused(_)) {
                        log::warn("voice is off", &[("reason", &reason)]);
                    }
                    self.set("off", Some(reason));
                }
                Ok((on, key)) => {
                    match Box::pin(self.call(on, key, &mut config, &mut shutdown)).await {
                        Ended::Shutdown => return Ok(()),
                        Ended::Changed => continue,
                        Ended::Failed(why) => {
                            log::warn("voice stopped", &[("reason", &why)]);
                            self.set("stopped", Some(why));
                        }
                    }
                }
            }
            tokio::select! {
                _ = config.changed() => {}
                () = self.wake.notified() => {}
                _ = shutdown.changed() => {}
            }
        }
    }

    async fn call(
        &self,
        on: VoiceOn,
        key: String,
        config: &mut watch::Receiver<Arc<Config>>,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Ended {
        let (feed, gateway) = mpsc::channel(FEED);
        {
            let mut gate = lock(&self.gate);
            gate.feed = Some((feed, on.settings.voice_channel));
            if let (true, Some(bot)) = (gate.up, gate.bot) {
                gate.send(GatewayEvent::Ready { bot });
            }
            if gate.snapshot {
                let event = gate.guild_event(on.settings.voice_channel);
                gate.send(event);
            }
        }
        let (control_tx, control) = mpsc::channel(16);
        *lock(&self.control) = Some(control_tx);
        let (stop_tx, stop) = watch::channel(false);
        self.set("running", None);
        log::info(
            "voice started",
            &[("transport", &on.settings.transport.name())],
        );
        let run = cataly_voice::run(
            on.settings.clone(),
            key,
            Arc::clone(&self.host),
            gateway,
            control,
            stop,
        );
        tokio::pin!(run);
        let ended = loop {
            tokio::select! {
                r = &mut run => break Ended::Failed(match r {
                    Ok(()) => "the call ended".into(),
                    Err(e) => e,
                }),
                _ = config.changed() => {
                    let now = Arc::clone(&config.borrow_and_update());
                    let same = matches!(&now.voice, VoiceConfig::On(n) if **n == on)
                        && now.discord.intake();
                    if !same {
                        let _ = stop_tx.send(true);
                        let _ = tokio::time::timeout(WIND_DOWN, &mut run).await;
                        break Ended::Changed;
                    }
                }
                _ = shutdown.changed() => {
                    let _ = stop_tx.send(true);
                    let _ = tokio::time::timeout(WIND_DOWN, &mut run).await;
                    break Ended::Shutdown;
                }
            }
        };
        *lock(&self.control) = None;
        lock(&self.gate).feed = None;
        ended
    }

    fn control(&self) -> Option<mpsc::Sender<Control>> {
        lock(&self.control).clone()
    }

    /// `voice.status`, `voice.restart` and `voice.reply`.
    pub(crate) async fn call_method(&self, method: &str, params: Value) -> Result<Value, String> {
        match method {
            crate::proto::method::VOICE_STATUS => Ok(self.status().await),
            crate::proto::method::VOICE_RESTART => {
                let lifted = self.host.lift().await?;
                match self.control() {
                    Some(c) => {
                        c.send(Control::Restart)
                            .await
                            .map_err(|_| "voice is not running".to_string())?;
                    }
                    None => self.wake.notify_one(),
                }
                Ok(json!({ "lifted": lifted }))
            }
            crate::proto::method::VOICE_REPLY => {
                let p: ReplyParams = serde_json::from_value(params).map_err(|e| e.to_string())?;
                let (Some(request), Some(reply)) = (p.request, p.reply) else {
                    return Err("a reply needs the request ID and the text".into());
                };
                let subject = p.subject.unwrap_or_default();
                let id = self.host.queue_reply(&request, &subject, &reply).await?;
                if let Some(c) = self.control() {
                    let _ = c.send(Control::Replies).await;
                }
                Ok(json!({ "queued": id, "request": request }))
            }
            other => Err(format!("unknown voice method {other}")),
        }
    }

    async fn status(&self) -> Value {
        let state = lock(&self.state).clone();
        let mut out = serde_json::Map::new();
        let mut put = |k: &str, v: Value| {
            out.insert(k.to_string(), v);
        };
        put("state", json!(state.state));
        put("reason", json!(state.reason));
        put("gateway", json!(lock(&self.gate).up));
        let config = Arc::clone(&self.config.borrow());
        if let VoiceConfig::On(on) = &config.voice {
            put("transport", json!(on.settings.transport.name()));
            match self.host.held().await {
                Ok(h) => put("held", json!(h)),
                Err(e) => put("held", json!(format!("unreadable: {}", e.0))),
            }
            match self.host.ledger(&on.settings.zone).await {
                Ok((day, total)) => {
                    put(
                        "today",
                        json!({
                            "day": day.day,
                            "speech_seconds": day.speech_seconds,
                            "sessions": day.sessions,
                            "spend_usd": day.spend_usd,
                        }),
                    );
                    put("total_speech_seconds", json!(total));
                }
                Err(e) => put("ledger_error", json!(e.0)),
            }
            let waiting = self.host.replies().await.map_or(0, |r| r.len());
            put("replies_waiting", json!(waiting));
        }
        if let Some(c) = self.control() {
            let (tx, rx) = oneshot::channel();
            if c.send(Control::Status(tx)).await.is_ok()
                && let Ok(Ok(s)) = tokio::time::timeout(Duration::from_secs(2), rx).await
            {
                put("call", json!(s.state));
                put("present", json!(s.present));
            }
        }
        Value::Object(out)
    }
}
