//! One realtime session (VOICE-11, VOICE-13 to VOICE-18; VOICE req. 63-80), written once against a
//! [`Link`] so both transports share every rule: states and deadlines, delegated backend work and
//! tool calls, partial status, reply pieces with playback proof, the limit announcement, idle close
//! and usage.
//!
//! The call loop drives it: provider events through [`Session::on_link`], owner audio through
//! [`Session::input`], finished tools through [`Session::on_tool`], and a 20 ms [`Session::tick`].
//! What the call must act on comes back as [`Effect`]s.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::host::{Passed, Reply, StatusPart, StatusRead, VoiceHost};
use crate::ledger::Spend;
use crate::link::{self, Endpoint, Link, LinkEvent};
use crate::pace::Pacer;
use crate::playback::Playback;
use crate::settings::{Settings, Transport};
use crate::text;
use crate::tools::{self, Call};

/// Starting longer than this closes the session with a timeout (VOICE req. 64).
const START: Duration = Duration::from_secs(30);
/// Closing longer than this terminates the transport.
const CLOSE: Duration = Duration::from_secs(15);
/// The limit announcement closes the session this long after it is sent (VOICE req. 80).
const WIND: Duration = Duration::from_secs(8);
/// A running delegation silent this long is a stall (VOICE req. 67).
const STALL: Duration = Duration::from_secs(120);
/// Finished work keeps blocking idle close for this long.
const AFTER_WORK: Duration = Duration::from_secs(10);
/// Pause rules for offers (VOICE req. 76, 78).
const OWNER_QUIET: Duration = Duration::from_millis(1500);
const OUTPUT_QUIET: Duration = Duration::from_millis(700);
const QUEUED_MAX: Duration = Duration::from_millis(100);
/// Reply piece deadlines (VOICE req. 79).
const ACK: Duration = Duration::from_secs(15);
const FINISH: Duration = Duration::from_secs(30);
/// Usage is recorded at least this often (VOICE req. 81).
const RECORD: Duration = Duration::from_secs(1);

/// Session states (VOICE req. 63).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    Starting,
    Live,
    WindingDown,
    Closing,
    Closed,
}

impl State {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Live => "live",
            Self::WindingDown => "winding down",
            Self::Closing => "closing",
            Self::Closed => "closed",
        }
    }
}

/// What the call must do.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Effect {
    /// Add usage to the ledger.
    Record { seconds: f64, spend: f64 },
    /// A reply piece was spoken up to this byte offset.
    Spoken { reply: i64, upto: usize },
    /// A provider or safety failure: voice is held (VOICE req. 83).
    Fail(String),
    /// The session is over.
    Ended,
}

/// A finished tool.
#[derive(Debug)]
pub(crate) struct ToolDone {
    delegation: String,
    call_id: String,
    output: String,
    /// The line kept for the summary post.
    outcome: String,
    /// An early partial status (VOICE req. 76).
    partial: Option<String>,
    status: bool,
}

#[derive(Debug)]
struct Delegation {
    id: String,
    response: Option<String>,
    /// A backend response is in progress.
    responding: bool,
    failed: bool,
    last_event: Instant,
    /// Calls not yet answered, by call ID, with whether each is a status read.
    open_calls: Vec<(String, bool)>,
    /// Calls answered since the last continuation request.
    answered: usize,
    finished: Option<Instant>,
}

impl Delegation {
    fn running(&self) -> bool {
        self.responding || !self.open_calls.is_empty()
    }
}

#[derive(Debug)]
struct Offer {
    event_id: String,
    reply: i64,
    upto: usize,
    needed: usize,
    sent: Instant,
    acked: bool,
    generation: u64,
    played_at_ack: u64,
    covered: usize,
    drained_since: Option<Instant>,
}

/// One realtime session.
pub(crate) struct Session<H: VoiceHost> {
    host: Arc<H>,
    settings: Arc<Settings>,
    link: Link,
    state: State,
    opened: Instant,
    closing_since: Option<Instant>,
    wind_since: Option<Instant>,
    pacer: Pacer,
    provider_seconds: f64,
    recorded_seconds: f64,
    last_record: Instant,
    unrecorded_spend: f64,
    counted: HashSet<String>,
    delegations: Vec<Delegation>,
    queue: VecDeque<(String, String, Call)>,
    tool_running: bool,
    tools_tx: mpsc::Sender<ToolDone>,
    pub(crate) owner_said: String,
    pub(crate) model_said: String,
    pub(crate) outcomes: Vec<String>,
    replies: VecDeque<Reply>,
    offer: Option<Offer>,
    partial: Option<(String, String)>,
    last_owner_speech: Instant,
    last_output: Instant,
    next_id: u64,
    effects: Vec<Effect>,
    ended: bool,
}

/// The `session` object shared by `session.start` (WebSocket) and the WebRTC POST.
pub(crate) fn config(settings: &Settings, greeting: Option<&str>) -> Value {
    // WebRTC negotiates the audio format in SDP; only the WebSocket names it.
    let audio = if settings.transport == Transport::WebSocket {
        json!({"output": {"voice": settings.voice},
            "format": {"type": "audio/pcm", "rate": 24000}})
    } else {
        json!({"output": {"voice": settings.voice}})
    };
    let mut session = serde_json::Map::new();
    session.insert("model".into(), json!(settings.speech_model));
    session.insert(
        "instructions".into(),
        json!(text::instructions(&settings.preferences)),
    );
    session.insert("store".into(), json!(false));
    session.insert("audio".into(), audio);
    session.insert(
        "delegation".into(),
        json!({
            "type": "responses",
            "responses": {
                "model": settings.backend_model,
                "instructions": text::backend_instructions(),
                "tools": tools::definitions(),
                "tool_choice": "auto",
                "parallel_tool_calls": true,
                "reasoning": {"effort": settings.reasoning_effort},
            }
        }),
    );
    if let Some(g) = greeting {
        session.insert(
            "input".into(),
            json!([{
                "type": "message",
                "role": "developer",
                "content": [{"type": "input_text", "text": g}],
            }]),
        );
    }
    Value::Object(session)
}

impl<H: VoiceHost> Session<H> {
    /// Opens the configured transport. `lead_in` (wake on speech) goes out first.
    pub(crate) async fn open(
        host: &Arc<H>,
        settings: &Arc<Settings>,
        key: &str,
        greeting: Option<String>,
        lead_in: &[u8],
        replies: Vec<Reply>,
        tools_tx: mpsc::Sender<ToolDone>,
    ) -> Result<Self, String> {
        let endpoint = Endpoint::parse(&settings.api_base)?;
        let now = Instant::now();
        let link = link::open(
            settings.transport,
            host,
            &endpoint,
            key,
            config(settings, greeting.as_deref()),
        )
        .await?;
        let mut pacer = Pacer::new(now);
        let _ = pacer.push(lead_in);
        Ok(Self {
            host: Arc::clone(host),
            settings: Arc::clone(settings),
            link,
            state: State::Starting,
            opened: now,
            closing_since: None,
            wind_since: None,
            pacer,
            provider_seconds: 0.0,
            recorded_seconds: 0.0,
            last_record: now,
            unrecorded_spend: 0.0,
            counted: HashSet::new(),
            delegations: Vec::new(),
            queue: VecDeque::new(),
            tool_running: false,
            tools_tx,
            owner_said: String::new(),
            model_said: String::new(),
            outcomes: Vec::new(),
            replies: replies.into(),
            offer: None,
            partial: None,
            last_owner_speech: now,
            last_output: now,
            next_id: 0,
            effects: Vec::new(),
            ended: false,
        })
    }

    pub(crate) fn state(&self) -> State {
        self.state
    }

    /// Whether anything was said or done (the summary rule of VOICE req. 84).
    pub(crate) fn eventful(&self) -> bool {
        !self.owner_said.is_empty() || !self.model_said.is_empty() || !self.outcomes.is_empty()
    }

    /// Effects produced since the last call.
    pub(crate) fn effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    /// The next provider event.
    pub(crate) async fn recv(&mut self) -> LinkEvent {
        self.link.recv().await
    }

    /// The reply queue changed (a new lead reply arrived).
    pub(crate) fn set_replies(&mut self, replies: Vec<Reply>) {
        let offered = self.offer.as_ref().map(|o| o.reply);
        self.replies = replies.into();
        if let Some(id) = offered
            && !self.replies.iter().any(|r| r.id == id)
        {
            self.offer = None;
        }
    }

    fn id(&mut self, kind: &str) -> String {
        self.next_id += 1;
        format!("cataly_{kind}_{}", self.next_id)
    }

    fn send(&mut self, event: Value) {
        if !self.link.send(event) {
            self.fail("the realtime link is stuck or gone".into());
        }
    }

    /// A failure: close at once, hold voice.
    pub(crate) fn fail(&mut self, reason: String) {
        if self.ended {
            return;
        }
        self.effects.push(Effect::Fail(reason));
        self.finish();
    }

    fn finish(&mut self) {
        if self.ended {
            return;
        }
        self.ended = true;
        self.state = State::Closed;
        self.link.terminate();
        self.record(Instant::now(), true);
        self.effects.push(Effect::Ended);
    }

    /// Asks the provider to close; the transport ends at `session.closed` or after 15 s.
    pub(crate) fn close(&mut self, why: &str) {
        match self.state {
            State::Closing | State::Closed => {}
            State::Starting => self.finish(),
            State::Live | State::WindingDown => {
                self.state = State::Closing;
                self.closing_since = Some(Instant::now());
                let _ = why;
                let id = self.id("close");
                self.send(json!({"type": "session.close", "event_id": id}));
            }
        }
    }

    /// A cap was reached: announce it and close 8 s later; only audio goes out meanwhile.
    pub(crate) fn wind_down(&mut self) {
        if self.state != State::Live {
            if self.state == State::Starting {
                self.finish();
            }
            return;
        }
        self.state = State::WindingDown;
        self.wind_since = Some(Instant::now());
        self.offer = None;
        self.partial = None;
        let id = self.id("limit");
        self.send(
            json!({"type": "session.instructions.append", "event_id": id,
            "delegation_id": null, "content": text::LIMIT}),
        );
    }

    /// Owner audio for the provider (24 kHz mono PCM bytes). `loud` is whether it was above the wake
    /// level.
    pub(crate) fn input(&mut self, pcm: &[u8], loud: bool, now: Instant) {
        if loud {
            self.last_owner_speech = now;
        }
        if matches!(self.state, State::Closing | State::Closed) {
            return;
        }
        if self.pacer.push(pcm).is_err() {
            self.fail("over 3 s of input audio pending (buffer overflow)".into());
        }
    }

    /// Whether anything blocks idle close (VOICE req. 67).
    pub(crate) fn busy(&self, now: Instant) -> bool {
        self.tool_running
            || !self.queue.is_empty()
            || self.delegations.iter().any(|d| {
                d.running()
                    || d.finished
                        .is_some_and(|f| now.saturating_duration_since(f) < AFTER_WORK)
            })
    }

    fn backend_blocking(&self) -> bool {
        self.tool_running
            || !self.queue.is_empty()
            || self.delegations.iter().any(Delegation::running)
    }

    /// Owner and output both quiet, under 100 ms queued (VOICE req. 76, 78).
    fn pause(&self, playback: &Playback, now: Instant) -> bool {
        now.saturating_duration_since(self.last_owner_speech) >= OWNER_QUIET
            && now.saturating_duration_since(self.last_output) >= OUTPUT_QUIET
            && playback
                .last_played()
                .is_none_or(|t| now.saturating_duration_since(t) >= OUTPUT_QUIET)
            && playback.queued() < QUEUED_MAX
    }

    /// Records usage when a second passed or at close (VOICE req. 81).
    fn record(&mut self, now: Instant, force: bool) {
        if !force && now.saturating_duration_since(self.last_record) < RECORD {
            return;
        }
        self.last_record = now;
        let wall = now.saturating_duration_since(self.opened).as_secs_f64();
        let total = wall.max(self.provider_seconds);
        let seconds = (total - self.recorded_seconds).max(0.0);
        self.recorded_seconds = total.max(self.recorded_seconds);
        let spend = std::mem::take(&mut self.unrecorded_spend);
        if seconds > 0.0 || spend > 0.0 {
            self.effects.push(Effect::Record { seconds, spend });
        }
    }

    /// The 20 ms tick: input pacing, deadlines, offers and usage.
    pub(crate) fn tick(&mut self, playback: &Playback, now: Instant) {
        if self.ended {
            return;
        }
        if matches!(self.state, State::Live | State::WindingDown) {
            let out = self.pacer.pull(now);
            if !out.is_empty() && !self.link.audio(out) {
                self.fail("the realtime link is stuck or gone".into());
                return;
            }
        }
        match self.state {
            State::Starting if now.saturating_duration_since(self.opened) > START => {
                self.fail("the session did not start within 30 s (start timeout)".into());
                return;
            }
            State::Closing
                if self
                    .closing_since
                    .is_some_and(|t| now.saturating_duration_since(t) > CLOSE) =>
            {
                // Force-terminated: final usage stays the latest observed (VOICE req. 64).
                self.finish();
                return;
            }
            State::WindingDown
                if self
                    .wind_since
                    .is_some_and(|t| now.saturating_duration_since(t) >= WIND) =>
            {
                self.close("limit reached");
            }
            _ => {}
        }
        if let Some(d) = self
            .delegations
            .iter()
            .find(|d| d.running() && now.saturating_duration_since(d.last_event) > STALL)
        {
            let id = d.id.clone();
            self.fail(format!("delegation {id} silent for 120 s (stall)"));
            return;
        }
        if self.state == State::Live {
            let idle = self.settings.idle_close;
            if !self.busy(now)
                && self.offer.is_none()
                && now.saturating_duration_since(self.last_owner_speech) >= idle
                && now.saturating_duration_since(self.last_output) >= idle
            {
                self.close("idle");
            }
            self.check_offer(playback, now);
            if self.state == State::Live && self.pause(playback, now) {
                self.offer_partial();
                if !self.backend_blocking() && self.offer.is_none() {
                    self.offer_reply(playback, now);
                }
            }
        }
        self.record(now, false);
    }

    fn offer_partial(&mut self) {
        let Some((delegation, text)) = self.partial.take() else {
            return;
        };
        let pending = self
            .delegations
            .iter()
            .any(|d| d.id == delegation && d.open_calls.iter().any(|(_, status)| !status))
            || self.queue.iter().any(|(d, _, _)| *d == delegation);
        if !pending {
            return;
        }
        let id = self.id("partial");
        self.send(json!({"type": "session.commentary.append", "event_id": id,
            "delegation_id": null, "content": text::partial_status(&text)}));
    }

    fn offer_reply(&mut self, playback: &Playback, now: Instant) {
        let Some(reply) = self.replies.front() else {
            return;
        };
        let Some((piece, upto)) = text::piece(&reply.text, reply.spoken, text::PIECE_MAX) else {
            return;
        };
        let last = upto >= reply.text.len();
        let content = text::reply_piece(&reply.subject, &piece, last);
        let reply_id = reply.id;
        let id = self.id("reply");
        self.offer = Some(Offer {
            event_id: id.clone(),
            reply: reply_id,
            upto,
            needed: text::coverage_needed(&piece),
            sent: now,
            acked: false,
            generation: playback.generation(),
            played_at_ack: 0,
            covered: 0,
            drained_since: None,
        });
        self.send(json!({"type": "session.commentary.append", "event_id": id,
            "delegation_id": null, "content": content}));
    }

    /// The playback proof of VOICE req. 78-79.
    fn check_offer(&mut self, playback: &Playback, now: Instant) {
        let Some(offer) = self.offer.as_mut() else {
            return;
        };
        if playback.generation() != offer.generation {
            // A clear (barge-in) puts the piece back for a later pause.
            self.offer = None;
            return;
        }
        if !offer.acked {
            if now.saturating_duration_since(offer.sent) > ACK {
                self.offer = None;
            }
            return;
        }
        // Drained: nothing queued and no output for the output-quiet time, so a pause between
        // chunks of a reply that is still arriving does not count.
        let quiet = now.saturating_duration_since(self.last_output) >= OUTPUT_QUIET;
        if playback.drained() && quiet {
            let since = *offer.drained_since.get_or_insert(now);
            let played = playback.played() > offer.played_at_ack;
            if played && offer.covered >= offer.needed {
                let (reply, upto) = (offer.reply, offer.upto);
                self.offer = None;
                if let Some(front) = self.replies.front_mut()
                    && front.id == reply
                {
                    front.spoken = upto;
                    if upto >= front.text.len() {
                        self.replies.pop_front();
                    }
                }
                self.effects.push(Effect::Spoken { reply, upto });
            } else if now.saturating_duration_since(since) > FINISH {
                self.offer = None;
            }
        } else {
            offer.drained_since = None;
        }
    }

    /// One provider event.
    pub(crate) fn on_link(&mut self, event: LinkEvent, playback: &mut Playback, now: Instant) {
        if self.ended {
            return;
        }
        match event {
            LinkEvent::Closed(why) => {
                if self.state == State::Closing {
                    self.finish();
                } else {
                    self.fail(format!("the realtime link ended: {why}"));
                }
            }
            LinkEvent::Malformed => self.fail("malformed provider event".into()),
            LinkEvent::Audio(mono) => {
                self.last_output = now;
                if playback.push(&mono, now).is_err() {
                    self.fail("playback buffer overflow (over 5 s queued)".into());
                }
            }
            LinkEvent::Event(v) => self.on_event(&v, playback, now),
        }
    }

    fn on_event(&mut self, v: &Value, playback: &Playback, now: Instant) {
        let kind = v["type"].as_str().unwrap_or("");
        match kind {
            "session.started" => {
                if self.state == State::Starting {
                    self.state = State::Live;
                }
            }
            "session.closed" => {
                if let Some(s) = v.pointer("/usage/seconds").and_then(Value::as_f64) {
                    self.provider_seconds = self.provider_seconds.max(s);
                }
                self.finish();
            }
            "session.usage.updated" => {
                if let Some(s) = v.pointer("/usage/seconds").and_then(Value::as_f64) {
                    self.provider_seconds = self.provider_seconds.max(s);
                }
            }
            "session.input_transcript.delta" => {
                self.owner_said.push_str(v["delta"].as_str().unwrap_or(""));
            }
            "session.output_transcript.delta" => {
                let delta = v["delta"].as_str().unwrap_or("");
                self.model_said.push_str(delta);
                self.last_output = now;
                if let Some(o) = self.offer.as_mut()
                    && o.acked
                {
                    o.covered += text::visible(delta);
                }
            }
            "session.commentary.appended" | "session.instructions.appended" => {
                let client = v["client_event_id"].as_str().unwrap_or("");
                if let Some(o) = self.offer.as_mut()
                    && o.event_id == client
                {
                    o.acked = true;
                    o.played_at_ack = playback.played();
                }
            }
            "session.delegation.created" => {
                let id = v["delegation_id"]
                    .as_str()
                    .or_else(|| v.pointer("/delegation/id").and_then(Value::as_str))
                    .unwrap_or("")
                    .to_string();
                if !id.is_empty() && !self.delegations.iter().any(|d| d.id == id) {
                    self.delegations.push(Delegation {
                        id,
                        response: v["response_id"].as_str().map(str::to_string),
                        responding: true,
                        failed: false,
                        last_event: now,
                        open_calls: Vec::new(),
                        answered: 0,
                        finished: None,
                    });
                }
            }
            "response.event" => self.on_backend(v, now),
            "error" => self.on_error(v),
            _ => {}
        }
    }

    /// Matches a backend event to a delegation: by ID, then response ID, then the single running
    /// candidate (VOICE req. 65).
    fn delegation_for(&self, v: &Value) -> Option<usize> {
        let event = &v["event"];
        if let Some(id) = v["delegation_id"].as_str()
            && let Some(i) = self.delegations.iter().position(|d| d.id == id)
        {
            return Some(i);
        }
        let response = event
            .pointer("/response/id")
            .and_then(Value::as_str)
            .or_else(|| event["response_id"].as_str());
        if let Some(r) = response
            && let Some(i) = self
                .delegations
                .iter()
                .position(|d| d.response.as_deref() == Some(r))
        {
            return Some(i);
        }
        let mut running = self
            .delegations
            .iter()
            .enumerate()
            .filter(|(_, d)| d.running());
        match (running.next(), running.next()) {
            (Some((i, _)), None) => Some(i),
            _ => None,
        }
    }

    fn on_backend(&mut self, v: &Value, now: Instant) {
        let event = &v["event"];
        let kind = event["type"].as_str().unwrap_or("");
        // Usage counts once per response, even when no delegation matches (VOICE req. 65).
        if matches!(
            kind,
            "response.completed" | "response.done" | "response.failed" | "response.incomplete"
        ) && let Some(id) = event.pointer("/response/id").and_then(Value::as_str)
            && let Some(usage) = event.pointer("/response/usage").filter(|u| !u.is_null())
            && self.counted.insert(id.to_string())
        {
            let Ok(c) = Spend::from_usage(usage).cost(&self.settings.backend_prices) else {
                self.fail("usage report with cached and written tokens above input".into());
                return;
            };
            self.unrecorded_spend += c;
        }
        let Some(i) = self.delegation_for(v) else {
            return;
        };
        let Some(d) = self.delegations.get_mut(i) else {
            return;
        };
        d.last_event = now;
        let delegation = d.id.clone();
        match kind {
            "response.created" => {
                d.response = event
                    .pointer("/response/id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                d.responding = true;
                d.failed = false;
            }
            "response.completed" | "response.done" => {
                d.responding = false;
                if d.open_calls.is_empty() {
                    if d.answered > 0 {
                        // Every call of this response was answered while it still ran.
                        self.resume(&delegation);
                    } else {
                        d.finished = Some(now);
                    }
                }
            }
            "response.failed" | "response.incomplete" | "response.cancelled" => {
                d.responding = false;
                d.failed = true;
                d.finished = Some(now);
            }
            "response.output_item.done" => {
                let item = &event["item"];
                match item["type"].as_str() {
                    Some("function_call") => {
                        let call_id = item["call_id"].as_str().unwrap_or("").to_string();
                        let name = item["name"].as_str().unwrap_or("");
                        let args = item["arguments"].as_str().unwrap_or("");
                        self.on_call(&delegation, call_id, name, args);
                    }
                    Some("web_search_call") if item["status"] == "completed" => {
                        self.unrecorded_spend += self.settings.search_price;
                        self.outcomes.push("web search: completed".into());
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn on_call(&mut self, delegation: &str, call_id: String, name: &str, args: &str) {
        if call_id.is_empty() {
            return;
        }
        let failed = self
            .delegations
            .iter()
            .any(|d| d.id == delegation && d.failed);
        let parsed = if failed {
            Err("the backend response failed; nothing was run".to_string())
        } else if matches!(
            self.state,
            State::Closing | State::Closed | State::WindingDown
        ) {
            Err("the session is ending; nothing was run".to_string())
        } else {
            tools::parse(name, args)
        };
        let status = matches!(parsed, Ok(Call::Status { .. }));
        if let Some(d) = self.delegations.iter_mut().find(|d| d.id == delegation) {
            d.open_calls.push((call_id.clone(), status));
            d.finished = None;
        }
        match parsed {
            Err(why) => {
                self.outcomes.push(format!("{name}: refused ({why})"));
                self.answer(
                    delegation,
                    &call_id,
                    &json!({"ok": false, "error": why}).to_string(),
                );
            }
            Ok(call) if call.is_status() => self.spawn(delegation.to_string(), call_id, call),
            Ok(call) => {
                self.queue
                    .push_back((delegation.to_string(), call_id, call));
                self.run_next();
            }
        }
    }

    /// Runs the next queued non-status tool, one at a time in arrival order (VOICE req. 66).
    fn run_next(&mut self) {
        if self.tool_running {
            return;
        }
        if let Some((d, c, call)) = self.queue.pop_front() {
            self.tool_running = true;
            self.spawn(d, c, call);
        }
    }

    fn spawn(&self, delegation: String, call_id: String, call: Call) {
        let (host, settings, tx) = (
            Arc::clone(&self.host),
            Arc::clone(&self.settings),
            self.tools_tx.clone(),
        );
        tokio::spawn(async move {
            let done = execute(&host, &settings, delegation, call_id, call).await;
            let _ = tx.send(done).await;
        });
    }

    /// A tool finished: its output goes back, and when nothing is left the backend continues.
    pub(crate) fn on_tool(&mut self, done: ToolDone, now: Instant) {
        if !done.status {
            self.tool_running = false;
        }
        self.outcomes.push(done.outcome);
        if let Some(p) = done.partial {
            self.partial = Some((done.delegation.clone(), p));
        }
        if self.ended {
            return;
        }
        if let Some(d) = self
            .delegations
            .iter_mut()
            .find(|d| d.id == done.delegation)
        {
            d.last_event = now;
        }
        self.answer(&done.delegation, &done.call_id, &done.output);
        self.run_next();
    }

    fn answer(&mut self, delegation: &str, call_id: &str, output: &str) {
        if matches!(self.state, State::Closing | State::Closed) {
            return;
        }
        let id = self.id("result");
        self.send(json!({"type": "response.item.create", "event_id": id,
            "item": {"type": "function_call_output", "call_id": call_id, "output": output}}));
        let mut resume = false;
        if let Some(d) = self.delegations.iter_mut().find(|d| d.id == delegation) {
            d.open_calls.retain(|(c, _)| c != call_id);
            d.answered += 1;
            // A response still running may name more calls; it continues once, after its end.
            resume = d.open_calls.is_empty() && !d.responding;
        }
        if resume {
            self.resume(delegation);
        }
    }

    /// Asks the backend to continue a delegation whose calls are all answered.
    fn resume(&mut self, delegation: &str) {
        if let Some(d) = self.delegations.iter_mut().find(|d| d.id == delegation) {
            d.answered = 0;
            d.responding = true;
        }
        if self.state == State::Live {
            let id = self.id("continue");
            self.send(json!({"type": "response.create", "event_id": id,
                "delegation_id": delegation}));
        }
    }

    fn on_error(&mut self, v: &Value) {
        let err = &v["error"];
        let code = err["code"].as_str().unwrap_or("");
        let client = err["client_event_id"].as_str();
        let tolerated = matches!(
            code,
            "context_injection_incomplete" | "context_length_exceeded"
        ) && client.is_some();
        if tolerated {
            if let Some(o) = &self.offer
                && Some(o.event_id.as_str()) == client
            {
                self.offer = None;
            }
            return;
        }
        let message: String = err["message"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(160)
            .collect();
        self.fail(format!("provider error {code}: {message}"));
    }
}

/// Runs one tool against the host.
async fn execute<H: VoiceHost>(
    host: &Arc<H>,
    settings: &Settings,
    delegation: String,
    call_id: String,
    call: Call,
) -> ToolDone {
    let kind = call.kind();
    match call {
        Call::Status { parts, early } => {
            let mut reads: Vec<StatusRead> = Vec::new();
            for part in parts {
                reads.push(host.status(part).await);
            }
            let mut out = Vec::new();
            let mut partial = String::new();
            for r in &reads {
                let (name, max) = match r.part {
                    StatusPart::Lanes => ("lanes", tools::LANES_MAX),
                    StatusPart::Reports => ("reports", tools::LANES_MAX),
                };
                let value = match &r.text {
                    Ok(t) => {
                        json!({"part": name, "read_ms": r.read_ms, "text": tools::cut(t, max)})
                    }
                    Err(e) => json!({"part": name, "read_ms": r.read_ms, "error": e}),
                };
                if let Ok(t) = &r.text
                    && early
                {
                    partial.push_str(t);
                    partial.push(' ');
                }
                out.push(value);
            }
            let partial = early.then(|| {
                let mut p = partial.trim().to_string();
                let mut cut = p.len().min(text::PIECE_MAX);
                while !p.is_char_boundary(cut) {
                    cut -= 1;
                }
                p.truncate(cut);
                p
            });
            ToolDone {
                delegation,
                call_id,
                output: json!({"ok": true, "status": out, "data_only": true}).to_string(),
                outcome: format!("{kind}: read"),
                partial: partial.filter(|p| !p.is_empty()),
                status: true,
            }
        }
        Call::Note(note) => {
            let posted = host
                .post(settings.text_channel, &format!("[from voice] {note}"))
                .await;
            let (output, outcome) = match posted {
                Ok(()) => (json!({"ok": true}), format!("{kind}: posted")),
                Err(e) => (
                    json!({"ok": false, "error": e}),
                    format!("{kind}: not posted ({e})"),
                ),
            };
            ToolDone {
                delegation,
                call_id,
                output: output.to_string(),
                outcome,
                partial: None,
                status: false,
            }
        }
        Call::Pass { request, high_risk } => {
            let request_id = format!("v{}", random_id());
            let passed = host
                .pass_to_lead(
                    &request_id,
                    &text::lead_request(&request_id, &request, high_risk),
                )
                .await;
            let ok = passed == Passed::Received;
            ToolDone {
                delegation,
                call_id,
                output: json!({"delivered": ok, "result": passed.describe()}).to_string(),
                outcome: format!("{kind} {request_id}: {}", passed.describe()),
                partial: None,
                status: false,
            }
        }
    }
}

/// A fresh 12-hex-digit request ID from the system generator.
pub(crate) fn random_id() -> String {
    use ring::rand::SecureRandom as _;
    let mut b = [0u8; 6];
    let _ = ring::rand::SystemRandom::new().fill(&mut b);
    b.iter().fold(String::new(), |mut s, x| {
        use std::fmt::Write as _;
        let _ = write!(s, "{x:02x}");
        s
    })
}
