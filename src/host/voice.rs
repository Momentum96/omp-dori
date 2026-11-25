//! The hub's side of voice (`DESIGN.md` 5.2, 10.1): [`HubVoice`] implements the voice crate's
//! `VoiceHost`. Network goes through `relay::http`; op 4 goes out on the one Gateway connection;
//! pass-to-lead is the lead's inbox with its receipt (VOICE-15); status reads come from the lanes;
//! notes post through the Discord client; the ledger, the hold and the reply queue are records in
//! the `coord` table.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cataly_voice::{
    Day, Io, LedgerError, Passed, Posted, Reply, StatusPart, StatusRead, VoiceHost,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use super::Runtime;
use super::inbox::{Message, ReceiptState};
use super::lead::{Lead, Phase};
use crate::config::Config;
use crate::log;
use crate::relay::discord::{ChannelRef, Rest, Sent, Snowflake};
use crate::relay::http::{self, Base, Kind, Outcome, Policy, Request};
use crate::store::Store;
use crate::store::coord::{CoordDone, CoordWrite};
use crate::sys::now_ms;
use hyper::Method;
use hyper::header::{AUTHORIZATION, HeaderValue};

/// Ledger rows, one per day in the configured zone (VOICE req. 81).
const DAY: &str = "voice_day";
/// The hold (VOICE req. 83), one record.
const HOLD: &str = "voice_hold";
/// Lead replies waiting to be spoken (VOICE-17), ids in arrival order.
const REPLY: &str = "voice_reply";
/// Proof of receipt must arrive within this long (VOICE req. 75).
const RECEIPT_WITHIN: Duration = Duration::from_secs(10);
/// How often the receipt is looked at while waiting.
const RECEIPT_POLL: Duration = Duration::from_millis(100);
/// Replies waiting at most; more are refused so the queue stays bounded.
const REPLIES_MAX: usize = 64;
/// Longest reply text and subject.
const REPLY_TEXT_MAX: usize = 8 * 1024;
const SUBJECT_MAX: usize = 120;

/// The stored form of a ledger row.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DayDoc {
    speech_seconds: f64,
    sessions: u32,
    spend_usd: f64,
    /// Today's cap notice went out (VOICE req. 82).
    #[serde(default)]
    notice: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyDoc {
    request_id: String,
    subject: String,
    text: String,
    spoken: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HoldDoc {
    reason: String,
}

/// The Discord client for the current config.
pub(crate) type RestNow = Arc<dyn Fn() -> Result<Arc<Rest>, String> + Send + Sync>;

/// What `VoiceHost` needs from the hub.
pub(crate) struct HubVoice<R: Runtime> {
    pub(crate) store: Store,
    pub(crate) lead: Arc<Lead<R>>,
    pub(crate) lanes: Arc<crate::lanes::Lanes<R>>,
    pub(crate) config: watch::Receiver<Arc<Config>>,
    pub(crate) rest: RestNow,
    /// Frames for the Gateway connection.
    pub(crate) ops: mpsc::Sender<Value>,
}

/// Today's date in `zone`, `YYYY-MM-DD`.
fn today(zone: &str) -> Result<String, LedgerError> {
    let tz =
        jiff::tz::TimeZone::get(zone).map_err(|e| LedgerError(format!("time zone {zone}: {e}")))?;
    Ok(jiff::Timestamp::now()
        .to_zoned(tz)
        .date()
        .strftime("%Y-%m-%d")
        .to_string())
}

fn ledger_error(e: impl std::fmt::Display) -> LedgerError {
    LedgerError(e.to_string())
}

impl<R: Runtime> HubVoice<R> {
    /// Every stored day: (day, version, row). A row that does not read is corruption.
    async fn days(&self) -> Result<Vec<(String, i64, DayDoc)>, LedgerError> {
        let docs = self
            .store
            .read(|r| r.coord_all(DAY))
            .await
            .map_err(ledger_error)?;
        docs.into_iter()
            .map(|d| {
                serde_json::from_str::<DayDoc>(&d.body)
                    .map(|row| (d.id.clone(), d.version, row))
                    .map_err(|e| LedgerError(format!("ledger row {} is damaged: {e}", d.id)))
            })
            .collect()
    }

    /// Changes today's row with compare-and-swap; a damaged row is never rewritten.
    async fn change_day(
        &self,
        zone: &str,
        change: impl FnOnce(&mut DayDoc) -> bool,
    ) -> Result<bool, LedgerError> {
        let day = today(zone)?;
        let id = day.clone();
        let stored = self
            .store
            .read(move |r| r.coord(DAY, &id))
            .await
            .map_err(ledger_error)?;
        let (mut row, version) = match stored {
            Some(d) => (
                serde_json::from_str::<DayDoc>(&d.body)
                    .map_err(|e| LedgerError(format!("ledger row {day} is damaged: {e}")))?,
                d.version,
            ),
            None => (DayDoc::default(), 0),
        };
        let sound = |r: &DayDoc| {
            r.speech_seconds.is_finite()
                && r.speech_seconds >= 0.0
                && r.spend_usd.is_finite()
                && r.spend_usd >= 0.0
        };
        if !sound(&row) {
            return Err(LedgerError(format!("ledger row {day} is damaged")));
        }
        if !change(&mut row) {
            return Ok(false);
        }
        if !sound(&row) {
            return Err(LedgerError(
                "refused a ledger write that is not sound".into(),
            ));
        }
        let doc = serde_json::to_string(&row).map_err(ledger_error)?;
        match self
            .store
            .coord(CoordWrite::Put {
                kind: DAY.into(),
                id: day,
                doc,
                expect: Some(version),
            })
            .await
            .map_err(ledger_error)?
        {
            CoordDone::Saved(_) => Ok(true),
            _ => Err(LedgerError("the ledger row changed underneath".into())),
        }
    }

    /// Queues a lead reply (VOICE-17); returns its queue id.
    pub(crate) async fn queue_reply(
        &self,
        request_id: &str,
        subject: &str,
        text: &str,
    ) -> Result<i64, String> {
        let text = text.trim();
        let subject = subject.trim();
        if !request_id.starts_with('v') || !super::inbox::valid_request_id(request_id) {
            return Err(format!("'{request_id}' is not a voice request ID"));
        }
        if text.is_empty() {
            return Err("an empty reply is refused".into());
        }
        if text.len() > REPLY_TEXT_MAX || subject.chars().count() > SUBJECT_MAX {
            return Err(format!(
                "a reply holds at most {REPLY_TEXT_MAX} bytes and a subject {SUBJECT_MAX} characters"
            ));
        }
        let waiting = self
            .store
            .read(|r| r.coord_all(REPLY))
            .await
            .map_err(|e| e.to_string())?;
        if waiting.len() >= REPLIES_MAX {
            return Err(format!("{REPLIES_MAX} replies already wait to be spoken"));
        }
        let last = waiting
            .iter()
            .filter_map(|d| d.id.parse::<i64>().ok())
            .max()
            .unwrap_or(0);
        let id = (now_ms() * 1000).max(last + 1);
        let doc = serde_json::to_string(&ReplyDoc {
            request_id: request_id.into(),
            subject: subject.into(),
            text: text.into(),
            spoken: 0,
        })
        .map_err(|e| e.to_string())?;
        match self
            .store
            .coord(CoordWrite::Put {
                kind: REPLY.into(),
                id: format!("{id:020}"),
                doc,
                expect: Some(0),
            })
            .await
            .map_err(|e| e.to_string())?
        {
            CoordDone::Saved(_) => Ok(id),
            _ => Err("a reply with that id exists; try again".into()),
        }
    }

    /// Lifts the hold (`cataly voice restart`).
    pub(crate) async fn lift(&self) -> Result<Option<String>, String> {
        let held = self.held().await.map_err(|e| e.0)?;
        self.store
            .coord(CoordWrite::Delete {
                kind: HOLD.into(),
                id: HOLD.into(),
            })
            .await
            .map_err(|e| e.to_string())?;
        Ok(held)
    }

    /// The lead's live session ID when it can take a voice request now (VOICE req. 73).
    async fn lead_ready(&self) -> Result<Option<String>, String> {
        let view = self.lead.view().await;
        if view.phase != Phase::Live {
            return Err("the lead is not running".into());
        }
        let Some(s) = view.session else {
            return Err("the lead has no session".into());
        };
        if !s.dialogs.is_empty() {
            return Err("the lead has a pending approval or question".into());
        }
        if s.input_blocked || s.quota_blocked.is_some() || s.reason.is_some() {
            return Err("the lead cannot take input now".into());
        }
        Ok(s.session_id)
    }

    /// Waits up to 10 s from `sent` for the request to show in the same lead session.
    async fn proven(&self, request_id: &str, session: Option<&str>, sent: Instant) -> bool {
        loop {
            if let Ok(Some(r)) = self.lead.receipt(request_id).await
                && r.state == ReceiptState::Delivered
            {
                let now = self.lead.snapshot().and_then(|s| s.session_id.clone());
                return session.is_some() && now.as_deref() == session;
            }
            if sent.elapsed() >= RECEIPT_WITHIN {
                return false;
            }
            tokio::time::sleep(RECEIPT_POLL).await;
        }
    }
}

impl<R: Runtime> VoiceHost for HubVoice<R> {
    async fn connect(&self, tls: bool, host: &str, port: u16) -> Result<Box<dyn Io>, String> {
        let io = http::connect(tls, host, port).await?;
        Ok(Box::new(io))
    }

    async fn post_json(&self, url: &str, key: &str, body: Vec<u8>, deadline: Duration) -> Posted {
        let split = url
            .find("://")
            .map(|i| i + 3)
            .and_then(|start| url.get(start..).map(|rest| (start, rest)))
            .map(|(start, rest)| start + rest.find('/').unwrap_or(rest.len()));
        let (Some(origin), Some(path)) = (
            split.and_then(|i| url.get(..i)),
            split.and_then(|i| url.get(i..)),
        ) else {
            return Posted::Failed("bad URL".into());
        };
        let base = match Base::parse(origin) {
            Ok(b) => b,
            Err(e) => return Posted::Failed(e),
        };
        let Ok(auth) = HeaderValue::from_str(&format!("Bearer {key}")) else {
            return Posted::Failed("the key is not a valid header value".into());
        };
        let req = Request {
            method: Method::POST,
            path,
            headers: vec![(AUTHORIZATION, auth)],
            json: Some(body),
            kind: Kind::Once,
        };
        let policy = Policy {
            attempt: deadline,
            attempts: 1,
            budget: Duration::ZERO,
        };
        match http::call(&base, &req, policy, |_| {}).await {
            Outcome::Done(r) => Posted::Done(r.body),
            Outcome::Rejected(r) => Posted::Rejected(r.status, r.body),
            Outcome::RateLimited(_) => Posted::Rejected(429, Vec::new()),
            Outcome::Uncertain(why) => Posted::Uncertain(why),
            Outcome::Failed(why) => Posted::Failed(why),
        }
    }

    async fn voice_state(&self, guild: u64, channel: Option<u64>) -> Result<(), String> {
        let op = json!({"op": 4, "d": {
            "guild_id": guild.to_string(),
            "channel_id": channel.map(|c| c.to_string()),
            "self_mute": false,
            "self_deaf": false,
        }});
        match tokio::time::timeout(Duration::from_secs(3), self.ops.send(op)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err("the Gateway connection is gone".into()),
            Err(_) => Err("the Gateway queue stayed full for 3 s".into()),
        }
    }

    async fn pass_to_lead(&self, request_id: &str, text: &str) -> Passed {
        let session = match self.lead_ready().await {
            Ok(s) => s,
            Err(why) => return Passed::Refused(why),
        };
        let sent = Instant::now();
        let receipt = self
            .lead
            .deliver(Message {
                sender: "voice".into(),
                text: text.into(),
                urgent: true,
                owner: true,
                identity: None,
                request_id: Some(request_id.into()),
            })
            .await;
        match receipt {
            // The inbox may have written it before failing: never resent, never claimed.
            Err(e) => {
                log::warn("voice request not confirmed", &[("error", &e)]);
                Passed::Unproven
            }
            Ok(r) if r.state == ReceiptState::Failed => Passed::Refused(
                r.reason
                    .unwrap_or_else(|| "the lead's inbox refused it".into()),
            ),
            Ok(r) if r.state == ReceiptState::Duplicate => {
                Passed::Refused("the same request is already with the lead".into())
            }
            Ok(_) => {
                if self.proven(request_id, session.as_deref(), sent).await {
                    Passed::Received
                } else {
                    Passed::Unproven
                }
            }
        }
    }

    async fn status(&self, part: StatusPart) -> StatusRead {
        let text = match part {
            StatusPart::Lanes => {
                let view = self.lead.view().await;
                let lead = match &view.session {
                    Some(s) => format!(
                        "lead: {}, since {}",
                        format!("{:?}", s.status).to_lowercase(),
                        crate::jobs::instant_text(s.updated_ms)
                    ),
                    None => format!("lead: {:?}", view.phase).to_lowercase(),
                };
                self.lanes.status_lines(&[lead]).await
            }
            StatusPart::Reports => self.lanes.newest_reports(8, cataly_voice::REPORT_MAX).await,
        };
        StatusRead {
            part,
            read_ms: now_ms(),
            text,
        }
    }

    async fn post(&self, channel: u64, text: &str) -> Result<(), String> {
        let rest = (self.rest)()?;
        let id = Snowflake::parse(&channel.to_string()).ok_or("bad channel ID")?;
        let checks = self.config.borrow().outgoing.clone();
        let seed = format!("voice-{}", super::random_hex(8));
        let posted = rest.post(&ChannelRef::Id(id), text, &seed, &checks).await;
        match posted.outcome {
            Sent::Confirmed => Ok(()),
            Sent::Rejected(why) => Err(why),
            Sent::Uncertain(why) => Err(format!("uncertain: {why}")),
        }
    }

    async fn ledger(&self, zone: &str) -> Result<(Day, f64), LedgerError> {
        let day = today(zone)?;
        let days = self.days().await?;
        let total = days.iter().map(|(_, _, r)| r.speech_seconds).sum();
        let row = days
            .into_iter()
            .find(|(d, _, _)| *d == day)
            .map_or_else(DayDoc::default, |(_, _, r)| r);
        Ok((
            Day {
                day,
                speech_seconds: row.speech_seconds,
                sessions: row.sessions,
                spend_usd: row.spend_usd,
            },
            total,
        ))
    }

    async fn record(
        &self,
        zone: &str,
        speech_seconds: f64,
        sessions: u32,
        spend_usd: f64,
    ) -> Result<(), LedgerError> {
        self.change_day(zone, |r| {
            r.speech_seconds += speech_seconds;
            r.sessions = r.sessions.saturating_add(sessions);
            r.spend_usd += spend_usd;
            true
        })
        .await
        .map(drop)
    }

    async fn held(&self) -> Result<Option<String>, LedgerError> {
        let doc = self
            .store
            .read(|r| r.coord(HOLD, HOLD))
            .await
            .map_err(ledger_error)?;
        match doc {
            None => Ok(None),
            Some(d) => serde_json::from_str::<HoldDoc>(&d.body)
                .map(|h| Some(h.reason))
                .map_err(|e| LedgerError(format!("the voice hold record is damaged: {e}"))),
        }
    }

    async fn hold(&self, reason: &str) -> Result<(), LedgerError> {
        let doc = serde_json::to_string(&HoldDoc {
            reason: reason.into(),
        })
        .map_err(ledger_error)?;
        log::warn("voice held", &[("reason", &reason)]);
        self.store
            .coord(CoordWrite::Put {
                kind: HOLD.into(),
                id: HOLD.into(),
                doc,
                expect: None,
            })
            .await
            .map(drop)
            .map_err(ledger_error)
    }

    async fn replies(&self) -> Result<Vec<Reply>, LedgerError> {
        let docs = self
            .store
            .read(|r| r.coord_all(REPLY))
            .await
            .map_err(ledger_error)?;
        docs.into_iter()
            .map(|d| {
                let id = d.id.parse::<i64>().map_err(ledger_error)?;
                let r: ReplyDoc = serde_json::from_str(&d.body).map_err(ledger_error)?;
                Ok(Reply {
                    id,
                    request_id: r.request_id,
                    subject: r.subject,
                    text: r.text,
                    spoken: r.spoken,
                })
            })
            .collect()
    }

    async fn spoken(&self, id: i64, spoken: usize) -> Result<(), LedgerError> {
        let key = format!("{id:020}");
        let k = key.clone();
        let Some(d) = self
            .store
            .read(move |r| r.coord(REPLY, &k))
            .await
            .map_err(ledger_error)?
        else {
            return Ok(());
        };
        let mut r: ReplyDoc = serde_json::from_str(&d.body).map_err(ledger_error)?;
        let write = if spoken >= r.text.len() {
            CoordWrite::Delete {
                kind: REPLY.into(),
                id: key,
            }
        } else {
            r.spoken = spoken;
            CoordWrite::Put {
                kind: REPLY.into(),
                id: key,
                doc: serde_json::to_string(&r).map_err(ledger_error)?,
                expect: Some(d.version),
            }
        };
        self.store
            .coord(write)
            .await
            .map(drop)
            .map_err(ledger_error)
    }

    fn log(&self, message: &str) {
        log::info("voice", &[("note", &message)]);
    }

    async fn cap_notice_due(&self, zone: &str) -> bool {
        self.change_day(zone, |r| !std::mem::replace(&mut r.notice, true))
            .await
            .unwrap_or(false)
    }
}
