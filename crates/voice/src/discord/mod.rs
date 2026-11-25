//! Discord voice (VOICE-03 to VOICE-07; VOICE req. 51-58): joining the configured channel through the
//! hub's one Gateway, the readiness deadline and rejoin rules, owner presence, and audibility.
//!
//! Output is audible only while the Gateway is up, the bot is seen in the channel and the voice
//! connection is ready; losing any of them mutes, discards queued audio and marks the owner absent.

mod conn;
mod receive;
mod rtp;

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use self::conn::{ConnEvent, Join};
use crate::host::{GatewayEvent, VoiceHost};
use crate::settings::Settings;

/// Every non-ready state shares one deadline for recovery (VOICE req. 53).
const READY: Duration = Duration::from_secs(30);
/// A disconnect lasting this long tears the connection down...
const DOWN: Duration = Duration::from_secs(5);
/// ...and the bot rejoins this long after.
const REJOIN: Duration = Duration::from_secs(3);
/// Discord's channel type of a guild voice channel.
const GUILD_VOICE: u64 = 2;

/// The voice side of the Discord connection. The flags are the audibility conditions of VOICE
/// req. 54, each set by a different event, so they stay separate.
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct Voice<H: VoiceHost> {
    settings: Arc<Settings>,
    host: Arc<H>,
    bot: Option<u64>,
    checked: bool,
    gateway_up: bool,
    owner_in: bool,
    bot_in: bool,
    session_id: Option<String>,
    server: Option<(String, String)>,
    conn: Option<(JoinHandle<()>, mpsc::Sender<Vec<i16>>)>,
    conn_ready: bool,
    not_ready_since: Option<Instant>,
    down_since: Option<Instant>,
    rejoin_at: Option<Instant>,
    joined: bool,
}

impl<H: VoiceHost> Voice<H> {
    pub(crate) fn new(settings: Arc<Settings>, host: Arc<H>) -> Self {
        Self {
            settings,
            host,
            bot: None,
            checked: false,
            gateway_up: false,
            owner_in: false,
            bot_in: false,
            session_id: None,
            server: None,
            conn: None,
            conn_ready: false,
            not_ready_since: None,
            down_since: None,
            rejoin_at: None,
            joined: false,
        }
    }

    fn audible(&self) -> bool {
        self.gateway_up && self.bot_in && self.conn_ready
    }

    /// Runs until shutdown; an error stops voice (VOICE req. 52-53).
    pub(crate) async fn run(
        mut self,
        mut gateway: mpsc::Receiver<GatewayEvent>,
        presence: watch::Sender<bool>,
        owner: mpsc::Sender<Vec<i16>>,
        mut play: mpsc::Receiver<Vec<i16>>,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), String> {
        let (events_tx, mut events) = mpsc::channel::<ConnEvent>(256);
        let result = loop {
            let wake = [
                self.not_ready_since.map(|t| conn::deadline(t, READY)),
                self.down_since.map(|t| conn::deadline(t, DOWN)),
                self.rejoin_at.map(|t| conn::deadline(t, Duration::ZERO)),
            ]
            .into_iter()
            .flatten()
            .min();
            tokio::select! {
                _ = shutdown.changed() => break Ok(()),
                g = gateway.recv() => {
                    let Some(g) = g else { break Err("the Gateway feed ended".to_string()) };
                    if let Err(e) = self.on_gateway(g, &events_tx).await {
                        break Err(e);
                    }
                }
                e = events.recv() => {
                    let Some(e) = e else { continue };
                    match e {
                        ConnEvent::Ready => {
                            self.conn_ready = true;
                            self.down_since = None;
                        }
                        ConnEvent::Audio(a) => {
                            if self.audible() && self.owner_in {
                                let _ = owner.try_send(a);
                            }
                        }
                        ConnEvent::Down(why) => {
                            self.host.log(&format!("voice connection down: {why}"));
                            self.conn_ready = false;
                            self.conn = None;
                            self.down_since.get_or_insert(Instant::now());
                            self.connect(&events_tx);
                        }
                        ConnEvent::Fatal(why) => break Err(why),
                    }
                }
                f = play.recv() => {
                    let Some(f) = f else { break Ok(()) };
                    if self.audible()
                        && let Some((_, tx)) = &self.conn
                    {
                        let _ = tx.try_send(f);
                    }
                }
                () = conn::until(wake) => {
                    let now = Instant::now();
                    if self.not_ready_since.is_some_and(|t| now >= t + READY) {
                        break Err("voice was not ready within 30 s".to_string());
                    }
                    if self.down_since.is_some_and(|t| now >= t + DOWN) {
                        // Tear down and rejoin after 3 s (VOICE req. 53).
                        self.down_since = None;
                        self.teardown().await;
                        self.rejoin_at = Some(now + REJOIN);
                    }
                    if self.rejoin_at.is_some_and(|t| now >= t) {
                        self.rejoin_at = None;
                        self.join().await;
                    }
                }
            }
            // Losing audibility discards queued audio and marks the owner absent.
            if self.audible() {
                self.not_ready_since = None;
            } else {
                while play.try_recv().is_ok() {}
                self.not_ready_since.get_or_insert(Instant::now());
            }
            let present = self.owner_in && self.audible();
            presence.send_if_modified(|p| {
                let changed = *p != present;
                *p = present;
                changed
            });
        };
        self.teardown().await;
        let _ = presence.send(false);
        result
    }

    async fn on_gateway(
        &mut self,
        event: GatewayEvent,
        events: &mpsc::Sender<ConnEvent>,
    ) -> Result<(), String> {
        let s = Arc::clone(&self.settings);
        match event {
            GatewayEvent::Ready { bot } => {
                if bot == s.owner {
                    return Err("the owner ID is the bot's own ID".into());
                }
                self.bot = Some(bot);
                self.gateway_up = true;
                if self.checked {
                    self.join().await;
                }
            }
            GatewayEvent::Guild {
                channel_type,
                voice_states,
            } => {
                if channel_type != Some(GUILD_VOICE) {
                    return Err("the voice channel is not a guild voice channel".into());
                }
                self.gateway_up = true;
                self.owner_in = voice_states
                    .iter()
                    .any(|(u, c, _)| *u == s.owner && *c == Some(s.voice_channel));
                if !self.checked {
                    self.checked = true;
                    self.join().await;
                }
            }
            GatewayEvent::VoiceState {
                user,
                channel,
                session_id,
            } => {
                if user == s.owner {
                    self.owner_in = channel == Some(s.voice_channel);
                }
                if Some(user) == self.bot {
                    match channel {
                        Some(c) if c == s.voice_channel => {
                            self.bot_in = true;
                            if self.session_id.as_deref() != Some(&session_id) {
                                self.session_id = Some(session_id);
                                self.connect(events);
                            }
                        }
                        Some(_) => return Err("the bot was moved to another channel".into()),
                        None => {
                            self.bot_in = false;
                            if self.joined {
                                self.down_since.get_or_insert(Instant::now());
                            }
                        }
                    }
                }
            }
            GatewayEvent::VoiceServer { token, endpoint } => {
                if let Some(e) = endpoint {
                    self.server = Some((token, e));
                    self.connect(events);
                } else {
                    self.conn_ready = false;
                }
            }
            GatewayEvent::Down => {
                self.gateway_up = false;
            }
        }
        Ok(())
    }

    async fn join(&mut self) {
        let Some(_) = self.bot else { return };
        self.joined = true;
        self.not_ready_since.get_or_insert(Instant::now());
        if let Err(e) = self
            .host
            .voice_state(self.settings.guild, Some(self.settings.voice_channel))
            .await
        {
            self.host.log(&format!("voice join not sent: {e}"));
        }
    }

    async fn teardown(&mut self) {
        if let Some((task, _)) = self.conn.take() {
            task.abort();
        }
        self.conn_ready = false;
        self.session_id = None;
        self.server = None;
        if self.joined {
            let _ = self.host.voice_state(self.settings.guild, None).await;
        }
    }

    /// Starts a connection once the session ID and the server are known.
    fn connect(&mut self, events: &mpsc::Sender<ConnEvent>) {
        let (Some(bot), Some(session_id), Some((token, endpoint))) =
            (self.bot, self.session_id.clone(), self.server.clone())
        else {
            return;
        };
        if let Some((task, _)) = self.conn.take() {
            task.abort();
        }
        self.conn_ready = false;
        let join = Join {
            endpoint,
            token,
            session_id,
            guild: self.settings.guild,
            bot,
            owner: self.settings.owner,
            channel: self.settings.voice_channel,
        };
        let (tx, rx) = mpsc::channel(16);
        let task = tokio::spawn(conn::run(Arc::clone(&self.host), join, events.clone(), rx));
        self.conn = Some((task, tx));
    }
}
