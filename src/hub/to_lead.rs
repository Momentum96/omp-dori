//! What reaches the lead without the owner writing (DESIGN 6.1 routing, 12.2 C3, C8): events of the
//! kinds the lead acts on, delivered through its inbox (urgent wakes, quiet waits); the 60 s
//! reminder for owner messages still owed; and the unanswered-message watch (OJ-22).

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, watch};

use super::Hub;
use crate::host::Status;
use crate::host::inbox::{HUB_SENDER, Message};
use crate::jobs::event::{Emit, EventKind, Severity, one_line};
use crate::log;
use crate::store::EventRecord;
use crate::store::lead::{LeadDone, LeadWrite, REMINDERS_MAX};
use crate::store::relay::OWED_WINDOW_MS;
use crate::sys::now_ms;

/// Event kinds the lead hears (WS req. 3, A24, OJ req. 7, 52). A held room is told to the lead once
/// (DESIGN 9.3, COORD-13), besides the reply the relay posts in the room; intake stops are the
/// owner's to fix and stay with him.
const TO_LEAD: [EventKind; 25] = [
    EventKind::ModelFallback,
    EventKind::ModelRestored,
    EventKind::ModelUnexpected,
    EventKind::SpendOffer,
    EventKind::Digest,
    EventKind::Unanswered,
    EventKind::StreamLine,
    EventKind::StreamEnded,
    EventKind::PrFeedback,
    EventKind::PrCheckFailed,
    EventKind::PrMerged,
    EventKind::PrClosed,
    EventKind::RelayHeld,
    EventKind::LaneStarted,
    EventKind::LaneReport,
    EventKind::LaneClaimed,
    EventKind::LaneClosed,
    EventKind::LaneLost,
    EventKind::LaneAlert,
    EventKind::LaneRecap,
    EventKind::LaneQueued,
    EventKind::PlannerRun,
    EventKind::ItemQuestion,
    EventKind::NotesRefresh,
    EventKind::UsageOn,
];
/// A body over this size goes to a private file (WS req. 5, OJ req. 52).
const BODY_INLINE: usize = 64 * 1024;
const PAGE: u32 = 256;
/// The first reminder comes 60 s after admission (OPC req. 9); later ones wait four times longer.
const FIRST_REMINDER_MS: i64 = 60_000;
/// How often owed replies are checked.
const OWED_TICK: Duration = Duration::from_secs(15);

fn routed(kind: &str) -> bool {
    TO_LEAD.iter().any(|k| k.name() == kind)
}

/// Whether an event wakes the lead. A held room does not: the owner was told in the room, only he
/// can release it, and a session that just left a hand-off unconfirmed should get no further prompt
/// on its own account; the notice goes out with the lead's next wake.
fn wakes_lead(e: &EventRecord) -> bool {
    e.urgent && e.kind != EventKind::RelayHeld.name()
}

/// The inbox identity of an event. Recurring summaries carry one replacing identity per kind and
/// subject, so an undelivered earlier one is retired instead of piling up while no lead runs; every
/// other event is once-only by its ID.
fn identity_of(e: &EventRecord) -> String {
    const LATEST: [EventKind; 4] = [
        EventKind::PlannerRun,
        EventKind::LaneRecap,
        EventKind::NotesRefresh,
        EventKind::Digest,
    ];
    if LATEST.iter().any(|k| k.name() == e.kind) {
        format!(
            "{}{}:{}",
            crate::store::host::LATEST_PREFIX,
            e.kind,
            e.subject
        )
    } else {
        format!("event:{}", e.id)
    }
}

async fn cursor(hub: &Hub) -> Result<i64, String> {
    let stored = hub
        .store
        .read(|r| r.meta("lead_cursor"))
        .await
        .map_err(|e| e.to_string())?;
    match stored.and_then(|v| v.parse().ok()) {
        Some(at) => Ok(at),
        // First run: history stays where it is.
        None => hub
            .store
            .read(crate::store::Reads::last_event_id)
            .await
            .map_err(|e| e.to_string()),
    }
}

/// The text the lead receives for one event.
fn text_of(hub: &Hub, e: &EventRecord) -> String {
    let mut text = format!("[event {}] {}", e.kind, e.text);
    if let Some(body) = e.body.as_deref().filter(|b| !b.trim().is_empty()) {
        if body.len() <= BODY_INLINE {
            text.push('\n');
            text.push_str(body);
        } else {
            let dir = hub.paths.files_dir.join("lead");
            let path = dir.join(format!("event-{}.txt", e.id));
            let saved = crate::sys::fs::ensure_private_dir(&dir)
                .and_then(|()| crate::sys::fs::write_atomic(&path, body.as_bytes()));
            match saved {
                Ok(()) => {
                    let _ = write!(
                        text,
                        "\n(body of {} bytes in {})",
                        body.len(),
                        path.display()
                    );
                }
                Err(e) => {
                    let _ = write!(text, "\n(body not saved: {e})");
                }
            }
        }
    } else if let Some(path) = &e.body_path {
        let _ = write!(text, "\n(body in {path})");
    }
    text
}

/// Follows the event stream and hands the lead's kinds to its inbox, once each, in order.
pub(crate) async fn route(hub: Arc<Hub>, mut stop: watch::Receiver<bool>) -> Result<(), String> {
    let mut live = hub.jobs.stream().subscribe();
    let mut at = cursor(&hub).await?;
    loop {
        let page = hub
            .store
            .read(move |r| r.events_after(at, PAGE))
            .await
            .map_err(|e| e.to_string())?;
        let moved = page.last().map(|e| e.id);
        for e in page.iter().filter(|e| routed(&e.kind) && !e.dry) {
            let message = Message {
                sender: HUB_SENDER.into(),
                text: text_of(&hub, e),
                urgent: wakes_lead(e),
                owner: false,
                identity: Some(identity_of(e)),
                request_id: None,
            };
            if let Err(why) = hub.lead.deliver(message).await {
                log::warn(
                    "event not handed to the lead",
                    &[("event", &e.id), ("error", &why)],
                );
            }
        }
        if let Some(last) = moved {
            at = last;
            let write = LeadWrite::Meta {
                key: "lead_cursor".into(),
                value: at.to_string(),
            };
            hub.store.lead(write).await.map_err(|e| e.to_string())?;
            continue;
        }
        tokio::select! {
            got = live.recv() => if let Err(broadcast::error::RecvError::Closed) = got { return Ok(()) },
            _ = stop.changed() => return Ok(()),
        }
    }
}

/// The 60 s reminder for owed owner messages (OPC req. 9-10, DESIGN 12.2 C8): only to an idle lead
/// that is not quota-blocked; at 60 s, 4 min and 16 min, sharing the cap of 3 with the stop gate.
pub(crate) async fn remind(hub: Arc<Hub>, mut stop: watch::Receiver<bool>) -> Result<(), String> {
    loop {
        tokio::select! {
            () = tokio::time::sleep(OWED_TICK) => {}
            _ = stop.changed() => return Ok(()),
        }
        let Some(snap) = hub.lead.snapshot() else {
            continue;
        };
        if snap.status != Status::Idle || snap.quota_blocked.is_some() {
            continue;
        }
        let now = now_ms();
        let since = now - OWED_WINDOW_MS;
        let Ok(open) = hub.store.read(move |r| r.owed_open("discord", since)).await else {
            continue;
        };
        for o in open.into_iter().filter(|o| o.reminders < REMINDERS_MAX) {
            let wait = FIRST_REMINDER_MS << (2 * o.reminders.clamp(0, 2));
            if now < o.admitted_ms + wait {
                continue;
            }
            let took = hub
                .store
                .lead(LeadWrite::Remind {
                    relay: "discord".into(),
                    room: o.room.clone(),
                    message: o.message.clone(),
                })
                .await;
            if !matches!(took, Ok(LeadDone::Count(1))) {
                continue;
            }
            let line = super::bridge::owed_line(&o.room, &o.message, o.admitted_ms, &o.text);
            let message = Message {
                sender: HUB_SENDER.into(),
                text: format!(
                    "[reminder] This owner message has no answer yet:\n{line}\nAnswer it, or say in one line what happens next."
                ),
                urgent: true,
                owner: false,
                identity: Some(format!("owed:{}:{}:{}", o.room, o.message, o.reminders + 1)),
                request_id: None,
            };
            if let Err(e) = hub.lead.deliver(message).await {
                log::warn("reminder not delivered", &[("error", &e)]);
            }
        }
    }
}

/// The unanswered-message watch (OJ req. 53-54): entries older than the alert age are reported
/// once; entries older than the drop age go silently.
pub(crate) async fn others(hub: Arc<Hub>, mut stop: watch::Receiver<bool>) -> Result<(), String> {
    loop {
        let settings = hub.config().discord.clone();
        let tick =
            (settings.others_alert / 4).clamp(Duration::from_secs(1), Duration::from_mins(5));
        tokio::select! {
            () = tokio::time::sleep(tick) => {}
            _ = stop.changed() => return Ok(()),
        }
        if settings.others.is_empty() {
            continue;
        }
        let now = now_ms();
        let drop_before = now - crate::jobs::duration_ms(settings.others_drop);
        let _ = hub
            .store
            .lead(LeadWrite::OthersDrop {
                before_ms: drop_before,
            })
            .await;
        let before = now - crate::jobs::duration_ms(settings.others_alert);
        let Ok(due) = hub
            .store
            .read(move |r| r.others_due("discord", before))
            .await
        else {
            continue;
        };
        for row in due {
            let minutes = (now - row.at_ms).max(0) / 60_000;
            let age = if minutes >= 60 {
                format!("{} h {} min", minutes / 60, minutes % 60)
            } else {
                format!("{minutes} min")
            };
            let link = match settings.guild {
                Some(g) => format!(
                    "https://discord.com/channels/{g}/{}/{}",
                    row.room, row.message
                ),
                None => format!(
                    "https://discord.com/channels/@me/{}/{}",
                    row.room, row.message
                ),
            };
            let emit = Emit::new(EventKind::Unanswered, Severity::Attention, true, &row.room)
                .with("room", &row.room)
                .with("author", &row.author)
                .with("age", age)
                .with("excerpt", one_line(&row.text, 140))
                .with("link", link);
            if hub.jobs.hub_event(emit).await.is_ok() {
                let _ = hub
                    .store
                    .lead(LeadWrite::OtherReported {
                        relay: "discord".into(),
                        room: row.room,
                        message: row.message,
                    })
                    .await;
            }
        }
    }
}
