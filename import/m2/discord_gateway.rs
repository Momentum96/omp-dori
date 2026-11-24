//! Discord Gateway: HELLO, jittered heartbeat, missed-ACK reconnect, identify/resume, op 1/7/9/11, close-code
//! table, bounded frames and writes, reconnect backoff; READY/RESUMED/MESSAGE_CREATE/INTERACTION_CREATE/THREAD_*
//! decode.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt as _, StreamExt as _};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    net::TcpStream,
    sync::mpsc,
    time::{Instant, sleep, sleep_until, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};

use super::discord_threads::{Routes, ThreadMetadata};
use super::discord_types::{Interaction, Snowflake, User};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Deserialize)]
struct Frame {
    op: u8,
    s: Option<u64>,
    t: Option<String>,
    #[serde(default)]
    d: Value,
}

#[derive(Deserialize)]
struct Hello {
    heartbeat_interval: u64,
}

struct Resume {
    url: Url,
    id: String,
    sequence: u64,
}

enum Outcome {
    Reconnect,
    Restart,
    RateLimited,
    Fatal(u16),
}

pub(super) struct Link<'a> {
    pub(super) url: &'a Url,
    pub(super) token: &'a str,
    pub(super) intake: IntakeLink<'a>,
}

pub(super) struct IntakeLink<'a> {
    pub(super) channels: &'a [Snowflake],
    pub(super) owner: Snowflake,
    pub(super) wake: &'a Wake,
    pub(super) threads: Option<&'a Routes>,
    pub(super) inbox: &'a super::inbox::Inbox,
    pub(super) interactions: Option<mpsc::Sender<Interaction>>,
}

impl Link<'_> {
    pub(super) async fn run(self) -> Result<()> {
        let mut resume = None;
        let base_delay = 1;
        let max_delay = 30;
        let mut delay = base_delay;
        loop {
            let started = Instant::now();
            let mut minimum = 0;
            let outcome = self.connect(&mut resume).await;
            match outcome {
                Ok(Outcome::Fatal(code)) => {
                    bail!("Discord Gateway closed with non-retryable code {code}")
                }
                Ok(Outcome::Restart) => resume = None,
                Ok(Outcome::Reconnect) | Err(_) => {}
                Ok(Outcome::RateLimited) => minimum = 60,
            }
            if started.elapsed() >= Duration::from_secs(30) {
                delay = base_delay;
            }
            let minimum = minimum.max(if resume.is_some() { 1 } else { 6 });
            let wait = delay.max(minimum);
            sleep(Duration::from_secs(wait)).await;
            delay = (delay * 2).min(max_delay);
        }
    }

    async fn connect(&self, resume: &mut Option<Resume>) -> Result<Outcome> {
        let url = resume.as_ref().map_or(self.url, |state| &state.url);
        let config = WebSocketConfig::default()
            .max_message_size(Some(1024 * 1024))
            .max_frame_size(Some(1024 * 1024));
        let (mut ws, _) = timeout(
            Duration::from_secs(10),
            connect_async_with_config(url.as_str(), Some(config), true),
        )
        .await??;
        let hello = timeout(Duration::from_secs(10), ws.next())
            .await?
            .context("Discord Gateway closed before HELLO")??;
        let Message::Text(hello) = hello else {
            bail!("expected Discord Gateway HELLO");
        };
        let hello: Frame = serde_json::from_str(hello.as_str())?;
        ensure!(hello.op == 10, "expected Discord Gateway HELLO");
        let hello: Hello = serde_json::from_value(hello.d)?;
        ensure!(
            (100..=120_000).contains(&hello.heartbeat_interval),
            "invalid Discord Gateway heartbeat interval"
        );
        let interval = Duration::from_millis(hello.heartbeat_interval);
        let first = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
            % u128::from(hello.heartbeat_interval);
        let mut heartbeat = Instant::now() + Duration::from_millis(u64::try_from(first)?);
        let mut awaiting_ack = false;
        let handshake = Instant::now() + Duration::from_secs(20);
        let mut ready = false;
        send(&mut ws, &self.authenticate(resume.as_ref())).await?;
        loop {
            tokio::select! {
                () = sleep_until(handshake), if !ready => return Ok(Outcome::Reconnect),
                () = sleep_until(heartbeat) => {
                    if awaiting_ack { return Ok(Outcome::Reconnect); }
                    beat(&mut ws, resume.as_ref()).await?;
                    awaiting_ack = true;
                    heartbeat = Instant::now() + interval;
                }
                incoming = ws.next() => {
                    let Some(incoming) = incoming else { return Ok(Outcome::Reconnect); };
                    match incoming? {
                        Message::Text(text) => {
                            let frame: Frame = serde_json::from_str(text.as_str())?;
                            match frame.op {
                                0 => {
                                    let sequence = frame.s;
                                    self.dispatch(frame, resume, &mut ready)?;
                                    if let (Some(state), Some(sequence)) = (resume.as_mut(), sequence) {
                                        state.sequence = state.sequence.max(sequence);
                                    }
                                }
                                1 => {
                                    beat(&mut ws, resume.as_ref()).await?;
                                    awaiting_ack = true;
                                }
                                7 => return Ok(Outcome::Reconnect),
                                9 if frame.d == Value::Bool(true) => return Ok(Outcome::Reconnect),
                                9 => return Ok(Outcome::Restart),
                                11 if awaiting_ack => awaiting_ack = false,
                                _ => {}
                            }
                        }
                        Message::Ping(_) => timeout(Duration::from_secs(3), ws.flush()).await??,
                        Message::Close(Some(close)) => return Ok(match u16::from(close.code) {
                            code @ (4004 | 4010..=4014) => Outcome::Fatal(code),
                            4008 => Outcome::RateLimited,
                            1000 | 1001 | 4003 | 4005 | 4007 | 4009 => Outcome::Restart,
                            _ => Outcome::Reconnect,
                        }),
                        Message::Close(None) => return Ok(Outcome::Reconnect),
                        _ => {}
                    }
                }
            }
        }
    }

    fn authenticate(&self, resume: Option<&Resume>) -> Value {
        resume.map_or_else(
            || {
                let guilds = u32::from(self.intake.threads.is_some());
                json!({"op": 2, "d": {"token": self.token,
                    "intents": (1 << 9) | (1 << 12) | (1 << 15) | guilds,
                    "properties": {"os": std::env::consts::OS, "browser": "cataly", "device": "cataly"}}})
            },
            |state| json!({"op": 6, "d": {"token": self.token, "session_id": state.id, "seq": state.sequence}}),
        )
    }
}

async fn beat(ws: &mut Socket, resume: Option<&Resume>) -> Result<()> {
    send(
        ws,
        &json!({"op": 1, "d": resume.map(|state| state.sequence)}),
    )
    .await
}

async fn send(ws: &mut Socket, value: &Value) -> Result<()> {
    timeout(
        Duration::from_secs(3),
        ws.send(Message::text(value.to_string())),
    )
    .await??;
    Ok(())
}

#[derive(Deserialize)]
struct Ready {
    session_id: String,
    resume_gateway_url: String,
}

#[derive(Deserialize)]
struct ThreadEvent {
    id: Snowflake,
    parent_id: Option<Snowflake>,
    thread_metadata: Option<ThreadMetadata>,
}

impl Link<'_> {
    pub(super) fn dispatch(
        &self,
        frame: Frame,
        resume: &mut Option<Resume>,
        ready: &mut bool,
    ) -> Result<()> {
        match frame.t.as_deref() {
            Some("READY") => {
                let event: Ready = serde_json::from_value(frame.d)?;
                let url = if self.url.scheme() == "ws" {
                    self.url.clone()
                } else {
                    let mut url = Url::parse(&event.resume_gateway_url)
                        .context("invalid Discord resume URL")?;
                    ensure!(
                        url.scheme() == "wss"
                            && url
                                .host_str()
                                .is_some_and(|host| host.ends_with(".discord.gg")
                                    || host == "gateway.discord.gg")
                            && url.username().is_empty()
                            && url.password().is_none(),
                        "invalid Discord resume host"
                    );
                    url.set_query(Some("v=10&encoding=json"));
                    url
                };
                let sequence = frame.s.context("missing Discord sequence")?;
                *resume = Some(Resume {
                    url,
                    id: event.session_id,
                    sequence,
                });
                *ready = true;
                self.established();
            }
            Some("RESUMED") if resume.is_some() => {
                *ready = true;
                self.established();
            }
            Some("MESSAGE_CREATE") if *ready || resume.is_some() => {
                self.intake.message_created(&frame.d)?;
            }
            Some("INTERACTION_CREATE") if *ready || resume.is_some() => {
                let intake = &self.intake;
                if let Ok(event) = serde_json::from_value::<Interaction>(frame.d)
                    && event.valid()
                    && let Some(interactions) = &intake.interactions
                    && (intake.channels.contains(&event.channel_id)
                        || intake
                            .threads
                            .is_some_and(|routes| routes.contains(event.channel_id)))
                {
                    // Never block heartbeats on user work or a slow callback; a full queue drops the press.
                    drop(interactions.try_send(event));
                }
            }
            Some(kind @ ("THREAD_CREATE" | "THREAD_UPDATE" | "THREAD_DELETE"))
                if *ready || resume.is_some() =>
            {
                if let Ok(event) = serde_json::from_value::<ThreadEvent>(frame.d) {
                    self.intake.thread_changed(kind, event);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// READY/RESUMED: replay every channel through REST before the queue is considered synced.
    fn established(&self) {
        if let Some(routes) = self.intake.threads {
            routes.refresh_soon();
        }
        self.intake.wake.send(Wake::ALL);
    }
}

#[derive(Deserialize)]
struct Notice {
    id: Snowflake,
    channel_id: Snowflake,
    author: User,
    webhook_id: Option<Snowflake>,
    #[serde(rename = "type")]
    kind: u8,
}

impl IntakeLink<'_> {
    fn message_created(&self, raw: &serde_json::Value) -> Result<()> {
        let Ok(event) = Notice::deserialize(raw) else {
            return Ok(());
        };
        if event.author.id != self.owner
            || event.author.bot
            || event.author.system
            || event.webhook_id.is_some()
            || !matches!(event.kind, 0 | 19)
        {
            return Ok(());
        }
        let exact = self.channels.binary_search(&event.channel_id).ok();
        if exact.is_none()
            && !self
                .threads
                .is_some_and(|routes| routes.contains(event.channel_id))
        {
            return Ok(());
        }
        self.inbox
            .push(event.channel_id, event.id, raw)
            .context("persisting Discord Gateway event")?;
        if let Some(index) = exact {
            self.wake.send(1 << index);
        } else if self
            .threads
            .is_some_and(|routes| routes.wake(event.channel_id))
        {
            self.wake.send(0);
        }
        Ok(())
    }

    fn thread_changed(&self, kind: &str, event: ThreadEvent) {
        let Some(routes) = self.threads else { return };
        let known = routes.contains(event.id);
        if !known
            && !event
                .parent_id
                .is_some_and(|parent| self.channels.contains(&parent))
        {
            return;
        }
        if kind == "THREAD_DELETE" || event.thread_metadata.is_some_and(|meta| meta.archived) {
            routes.invalidate(event.id);
        } else if !known {
            routes.refresh_soon();
        } else {
            return;
        }
        self.wake.send(0);
    }
}
