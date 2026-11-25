//! The call: presence, wake on speech, playback and barge-in, the ledger and caps, holds, and the
//! end-of-session summary (VOICE-05, VOICE-08, VOICE-09, VOICE-18 to VOICE-21).
//!
//! [`run`] wires the Discord voice connection to the call; [`run_with_audio`] takes the audio side
//! directly, which is how the transport tests drive a call without Discord.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval, timeout};

use crate::audio::{LeadIn, bytes, dbfs};
use crate::host::{GatewayEvent, Posted, VoiceHost};
use crate::ledger::{self, Caps, Day, Spend};
use crate::link::Endpoint;
use crate::playback::Playback;
use crate::session::{Effect, Session, State, ToolDone};
use crate::settings::Settings;

/// Owner audio this long above the wake level opens a session (VOICE req. 61).
const WAKE: Duration = Duration::from_millis(200);
/// Deadline of the summary call (VOICE req. 84).
const SUMMARY: Duration = Duration::from_secs(60);
/// Summary output tokens at most.
const SUMMARY_TOKENS: u32 = 600;
/// Posts are cut into pieces of at most this many characters.
const POST_MAX: usize = 1900;
/// Opening a session may take this long before it counts as a start timeout.
const OPEN: Duration = Duration::from_secs(30);

/// The audio side of a call: owner frames in (24 kHz mono, 20 ms each), playback frames out (48 kHz
/// stereo, 20 ms each).
pub struct CallAudio {
    /// Owner audio from Discord.
    pub owner: mpsc::Receiver<Vec<i16>>,
    /// Frames to play in the channel.
    pub play: mpsc::Sender<Vec<i16>>,
}

/// Whether the owner is present: seen in the channel while output is audible (VOICE req. 55).
pub type Presence = watch::Receiver<bool>;

/// Requests from the hub.
#[derive(Debug)]
pub enum Control {
    /// The owner restarted voice after a hold; the hub already cleared the stored hold.
    Restart,
    /// A lead reply was queued.
    Replies,
    /// The current status.
    Status(oneshot::Sender<VoiceStatus>),
}

/// What `cataly voice status` shows.
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceStatus {
    /// `idle` or the session state.
    pub state: String,
    /// Why voice is held, if it is.
    pub held: Option<String>,
    /// The configured transport.
    pub transport: String,
    /// Whether the owner is present and output audible.
    pub present: bool,
    /// Today's ledger row, once read.
    pub today: Option<Day>,
}

/// Runs voice with the Discord voice connection until shutdown or a fatal error.
///
/// # Errors
/// Voice stops: a setting Discord refuses, a missed readiness deadline, a move to another channel.
pub async fn run<H: VoiceHost>(
    settings: Settings,
    key: String,
    host: Arc<H>,
    gateway: mpsc::Receiver<GatewayEvent>,
    control: mpsc::Receiver<Control>,
    shutdown: watch::Receiver<bool>,
) -> Result<(), String> {
    let settings = Arc::new(settings);
    let (presence_tx, presence) = watch::channel(false);
    let (owner_tx, owner) = mpsc::channel(256);
    let (play, play_rx) = mpsc::channel(16);
    let discord = crate::discord::Voice::new(Arc::clone(&settings), Arc::clone(&host));
    let mut stop = shutdown.clone();
    let connection =
        tokio::spawn(discord.run(gateway, presence_tx, owner_tx, play_rx, shutdown.clone()));
    let call = run_with_audio(
        Arc::clone(&settings),
        key,
        host,
        CallAudio { owner, play },
        presence,
        control,
        shutdown,
    );
    tokio::select! {
        r = call => r,
        r = connection => match r {
            Ok(r) => r,
            Err(e) => Err(format!("voice connection task failed: {e}")),
        },
        _ = stop.changed() => Ok(()),
    }
}

struct Core<H: VoiceHost> {
    settings: Arc<Settings>,
    key: String,
    host: Arc<H>,
    caps: Caps,
    session: Option<Session<H>>,
    playback: Playback,
    lead_in: LeadIn,
    loud_since: Option<Instant>,
    present: bool,
    held: Option<String>,
    today: Option<Day>,
    total: f64,
    capped: bool,
    failed: Option<String>,
    finishing: Option<JoinHandle<()>>,
    tools_tx: mpsc::Sender<ToolDone>,
}

/// Runs a call with a given audio side and presence signal.
///
/// # Errors
/// The API base is invalid, the voice records cannot be read, or the audio side ended.
pub async fn run_with_audio<H: VoiceHost>(
    settings: Arc<Settings>,
    key: String,
    host: Arc<H>,
    mut audio: CallAudio,
    mut presence: Presence,
    mut control: mpsc::Receiver<Control>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), String> {
    Endpoint::parse(&settings.api_base)?;
    let (tools_tx, mut tools_rx) = mpsc::channel(64);
    let held = host
        .held()
        .await
        .map_err(|e| format!("voice records: {}", e.0))?;
    let mut core = Core {
        caps: Caps {
            daily: settings.daily_cap,
            total: settings.total_cap,
            spending_usd: settings.spending_cap_usd,
        },
        settings,
        key,
        host,
        session: None,
        playback: Playback::new(),
        lead_in: LeadIn::default(),
        loud_since: None,
        present: false,
        held,
        today: None,
        total: 0.0,
        capped: false,
        failed: None,
        finishing: None,
        tools_tx,
    };
    let mut tick = interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if let Some(s) = core.session.as_mut() {
                    s.close("shutdown");
                }
                core.settle().await;
                return Ok(());
            }
            changed = presence.changed() => {
                if changed.is_err() {
                    return Err("the voice connection ended".into());
                }
                let now_present = *presence.borrow_and_update();
                core.presence(now_present).await;
            }
            frame = audio.owner.recv() => {
                let Some(frame) = frame else {
                    return Err("the voice connection ended".into());
                };
                core.owner(&frame).await;
            }
            event = recv(&mut core.session) => {
                let now = Instant::now();
                if let Some(s) = core.session.as_mut() {
                    s.on_link(event, &mut core.playback, now);
                }
                core.effects().await;
            }
            done = tools_rx.recv() => {
                if let (Some(done), Some(s)) = (done, core.session.as_mut()) {
                    s.on_tool(done, Instant::now());
                    core.effects().await;
                }
            }
            _ = tick.tick() => {
                let now = Instant::now();
                if let Some(frame) = core.playback.next_frame(now) {
                    let _ = audio.play.try_send(frame);
                }
                if let Some(s) = core.session.as_mut() {
                    s.tick(&core.playback, now);
                }
                core.effects().await;
                if core.finishing.as_ref().is_some_and(JoinHandle::is_finished) {
                    core.finishing = None;
                }
            }
            c = control.recv() => match c {
                None => return Ok(()),
                Some(Control::Restart) => {
                    core.held = None;
                    core.failed = None;
                    if core.present && core.session.is_none() {
                        core.start(true).await;
                    }
                }
                Some(Control::Replies) => {
                    if let (Ok(r), Some(s)) = (core.host.replies().await, core.session.as_mut()) {
                        s.set_replies(r);
                    }
                }
                Some(Control::Status(tx)) => {
                    let _ = tx.send(core.status());
                }
            },
        }
    }
}

async fn recv<H: VoiceHost>(session: &mut Option<Session<H>>) -> crate::link::LinkEvent {
    match session.as_mut() {
        Some(s) => s.recv().await,
        None => std::future::pending().await,
    }
}

impl<H: VoiceHost> Core<H> {
    fn status(&self) -> VoiceStatus {
        VoiceStatus {
            state: self
                .session
                .as_ref()
                .map_or("idle", |s| s.state().name())
                .to_string(),
            held: self.held.clone(),
            transport: self.settings.transport.name().to_string(),
            present: self.present,
            today: self.today.clone(),
        }
    }

    async fn presence(&mut self, present: bool) {
        self.present = present;
        if present {
            if self.session.is_none() {
                self.start(true).await;
            }
        } else {
            self.lead_in.clear();
            self.loud_since = None;
            self.playback.clear();
            if let Some(s) = self.session.as_mut() {
                s.close("owner left");
            }
            self.effects().await;
        }
    }

    async fn owner(&mut self, frame: &[i16]) {
        if !self.present {
            return;
        }
        let now = Instant::now();
        let level = dbfs(frame);
        let loud = level >= self.settings.wake_dbfs;
        let pcm = bytes(frame);
        if let Some(s) = self.session.as_mut()
            && !matches!(s.state(), State::Closing | State::Closed)
        {
            self.playback
                .owner_level(level, self.settings.wake_dbfs, now);
            s.input(&pcm, loud, now);
            self.effects().await;
            return;
        }
        self.lead_in.push(&pcm);
        if !loud {
            self.loud_since = None;
            return;
        }
        let since = *self.loud_since.get_or_insert(now);
        if now.saturating_duration_since(since) + Duration::from_millis(20) >= WAKE {
            self.loud_since = None;
            self.start(false).await;
        }
    }

    /// Whether a session may start; posts the cap notice or holds on a ledger failure.
    async fn may_start(&mut self) -> bool {
        if self.held.is_some() || self.finishing.is_some() || self.session.is_some() {
            return false;
        }
        match self.host.ledger(&self.settings.zone).await {
            Err(e) => {
                self.fail(&format!("ledger unreadable: {}", e.0)).await;
                false
            }
            Ok((day, _)) if !day.sound() => {
                self.fail("ledger corrupt").await;
                false
            }
            Ok((day, total)) => {
                let reached = ledger::reached(&day, total, &self.caps);
                self.today = Some(day);
                self.total = total;
                if let Some(cap) = reached {
                    if self.host.cap_notice_due(&self.settings.zone).await {
                        let _ = self
                            .host
                            .post(
                                self.settings.text_channel,
                                &format!("Voice is off for now: the {cap} is reached."),
                            )
                            .await;
                    }
                    return false;
                }
                true
            }
        }
    }

    async fn start(&mut self, greet: bool) {
        if !self.may_start().await {
            return;
        }
        let replies = self.host.replies().await.unwrap_or_default();
        let greeting = greet.then(|| crate::text::greeting(replies.len()));
        let lead_in = if greet {
            Vec::new()
        } else {
            self.lead_in.take()
        };
        self.lead_in.clear();
        self.capped = false;
        self.failed = None;
        let opened = timeout(
            OPEN,
            Session::open(
                &self.host,
                &self.settings,
                &self.key,
                greeting,
                &lead_in,
                replies,
                self.tools_tx.clone(),
            ),
        )
        .await;
        match opened {
            Ok(Ok(s)) => {
                self.session = Some(s);
                if let Err(e) = self.host.record(&self.settings.zone, 0.0, 1, 0.0).await {
                    self.fail(&format!("ledger write failed: {}", e.0)).await;
                }
            }
            Ok(Err(e)) => self.fail(&format!("the session did not start: {e}")).await,
            Err(_) => {
                self.fail("the session did not start within 30 s (start timeout)")
                    .await;
            }
        }
    }

    /// Holds voice: no session until the owner restarts it (VOICE req. 83).
    async fn fail(&mut self, reason: &str) {
        if let Some(s) = self.session.as_mut() {
            s.fail(reason.to_string());
        }
        self.failed = Some(reason.to_string());
        if self.held.is_some() {
            return;
        }
        self.held = Some(reason.to_string());
        let _ = self.host.hold(reason).await;
        let _ = self
            .host
            .post(
                self.settings.text_channel,
                &format!("Voice is held: {reason}. It stays off until `cataly voice restart`."),
            )
            .await;
    }

    async fn effects(&mut self) {
        loop {
            let effects = match self.session.as_mut() {
                Some(s) => s.effects(),
                None => return,
            };
            if effects.is_empty() {
                return;
            }
            for e in effects {
                match e {
                    Effect::Record { seconds, spend } => self.record(seconds, spend).await,
                    Effect::Spoken { reply, upto } => {
                        if let Err(e) = self.host.spoken(reply, upto).await {
                            self.fail(&format!("reply queue write failed: {}", e.0))
                                .await;
                        }
                    }
                    Effect::Fail(reason) => self.fail(&reason).await,
                    Effect::Ended => self.ended(),
                }
            }
        }
    }

    async fn record(&mut self, seconds: f64, spend: f64) {
        if let Err(e) = self
            .host
            .record(&self.settings.zone, seconds, 0, spend)
            .await
        {
            self.fail(&format!("ledger write failed: {}", e.0)).await;
            return;
        }
        let day = self.today.get_or_insert_with(Day::default);
        day.speech_seconds += seconds;
        day.spend_usd += spend;
        self.total += seconds;
        if !self.capped && ledger::reached(day, self.total, &self.caps).is_some() {
            self.capped = true;
            if let Some(s) = self.session.as_mut() {
                s.wind_down();
            }
        }
    }

    /// The session is over: accounting is done; the summary runs before the next session may start.
    fn ended(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        self.playback.clear();
        if !session.eventful() {
            return;
        }
        let blocked = if let Some(f) = &self.failed {
            Some(format!("no summary: the session failed ({f})"))
        } else if self.capped {
            Some("no summary: a voice cap was reached".to_string())
        } else {
            None
        };
        let (host, settings, key) = (
            Arc::clone(&self.host),
            Arc::clone(&self.settings),
            self.key.clone(),
        );
        let said = format!(
            "Owner: {}\nAssistant: {}",
            session.owner_said, session.model_said
        );
        let outcomes = session.outcomes.clone();
        drop(session);
        self.finishing = Some(tokio::spawn(async move {
            summarize(&host, &settings, &key, &said, &outcomes, blocked).await;
        }));
    }

    /// Waits for a closing session and the summary (shutdown).
    async fn settle(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(16);
        while self.session.is_some() && Instant::now() < deadline {
            let now = Instant::now();
            if let Some(s) = self.session.as_mut() {
                s.tick(&self.playback, now);
                tokio::select! {
                    ev = s.recv() => s.on_link(ev, &mut self.playback, now),
                    () = tokio::time::sleep(Duration::from_millis(20)) => {}
                }
            }
            self.effects().await;
        }
        if let Some(f) = self.finishing.take() {
            let _ = timeout(SUMMARY + Duration::from_secs(5), f).await;
        }
    }
}

/// Asks the summary model for 3 to 5 lines, records its usage, and posts the summary, every tool
/// outcome and the cost estimate (VOICE req. 84). Transcripts are dropped afterwards.
async fn summarize<H: VoiceHost>(
    host: &Arc<H>,
    settings: &Settings,
    key: &str,
    said: &str,
    outcomes: &[String],
    blocked: Option<String>,
) {
    let summary = match blocked {
        Some(why) => why,
        None => match summary_call(host, settings, key, said).await {
            Ok((text, spend)) => {
                if let Err(e) = host.record(&settings.zone, 0.0, 0, spend).await {
                    let _ = host.hold(&format!("ledger write failed: {}", e.0)).await;
                }
                text
            }
            Err(e) => format!("no summary: {e}"),
        },
    };
    let mut post = format!("Voice session summary:\n{summary}");
    if !outcomes.is_empty() {
        post.push_str("\nTool outcomes:");
        for o in outcomes {
            post.push_str("\n- ");
            post.push_str(o);
        }
    }
    if let (Some(rate), Ok((day, _))) = (
        settings.speech_price_per_minute,
        host.ledger(&settings.zone).await,
    ) {
        let estimate = day.speech_seconds / 60.0 * rate + day.spend_usd;
        let _ = write!(post, "\nEstimated cost today: {estimate:.2} USD");
    }
    for piece in pieces(&post, POST_MAX) {
        let _ = host.post(settings.text_channel, &piece).await;
    }
}

async fn summary_call<H: VoiceHost>(
    host: &Arc<H>,
    settings: &Settings,
    key: &str,
    said: &str,
) -> Result<(String, f64), String> {
    let endpoint = Endpoint::parse(&settings.api_base)?;
    let body = json!({
        "model": settings.summary_model,
        "instructions": "Summarise this voice conversation in 3 to 5 short lines, in the language \
            the owner spoke. Report only what was said and done.",
        "input": said,
        "max_output_tokens": SUMMARY_TOKENS,
        "store": false,
    });
    match host
        .post_json(
            &endpoint.url("/v1/responses"),
            key,
            body.to_string().into_bytes(),
            SUMMARY,
        )
        .await
    {
        Posted::Done(bytes) => {
            let v: Value =
                serde_json::from_slice(&bytes).map_err(|_| "unreadable summary".to_string())?;
            let spend = Spend::from_usage(v.get("usage").unwrap_or(&Value::Null))
                .cost(&settings.summary_prices)
                .map_err(|_| "invalid summary usage".to_string())?;
            let text: String = v
                .get("output")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .flat_map(|o| o["content"].as_array().into_iter().flatten())
                .filter(|c| c["type"] == "output_text")
                .filter_map(|c| c["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if text.trim().is_empty() {
                return Err("the summary was empty".into());
            }
            Ok((text.trim().to_string(), spend))
        }
        Posted::Rejected(status, _) => Err(format!("the summary call was refused ({status})")),
        Posted::Uncertain(e) | Posted::Failed(e) => Err(format!("the summary call failed: {e}")),
    }
}

/// Cuts text into pieces of at most `max` characters, at line ends where possible.
fn pieces(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for line in text.split('\n') {
        let mut line = line.to_string();
        loop {
            let room = max - current.chars().count().min(max);
            let needed = line.chars().count() + usize::from(!current.is_empty());
            if needed <= room {
                if !current.is_empty() {
                    current.push('\n');
                }
                current.push_str(&line);
                break;
            }
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
                continue;
            }
            let head: String = line.chars().take(max).collect();
            line = line.chars().skip(max).collect();
            out.push(head);
            if line.is_empty() {
                break;
            }
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posts_are_cut_at_1900() {
        let text = format!("{}\n{}\nend", "a".repeat(1000), "b".repeat(2500));
        let p = pieces(&text, POST_MAX);
        assert!(p.iter().all(|x| x.chars().count() <= POST_MAX));
        assert_eq!(p.concat().replace('\n', ""), text.replace('\n', ""));
    }
}
