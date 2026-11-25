//! Routing (WS req. 7, OJ req. 7): every event goes to the rotating owner-readable event log; events
//! with destinations get one attempt per destination. An attempt is marked durably before it starts,
//! so a hub that stops mid-attempt leaves it "uncertain" and it is never repeated (DESIGN 9.3).
//! Destinations: local commands and Discord channels.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::timeout;

use crate::config::Destination;
use crate::log;
use crate::relay::discord::{ChannelRef, Sent};
use crate::relay::text::{DISCORD_LIMIT, utf16_len};
use crate::store::{Delivery, EventRecord, Store};
use crate::sys::ErrorClass;
use crate::sys::command::{self, KILL_GRACE, Output, Spec};
use crate::sys::fs::open_append_private;

use super::{Ctx, JobsError};

/// The event log rotates to `<name>.1` at this size, so the last 5 to 10 MiB stay readable.
const LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024;
/// Tail read to find the last logged event.
const LOG_TAIL: u64 = 64 * 1024;
/// Event commands: 30 s each (DESIGN 9.4).
const DESTINATION_TIMEOUT: Duration = Duration::from_secs(30);
const DESTINATION_OUTPUT_CAP: u64 = 64 * 1024;
/// One messenger delivery, all parts and retries included (WS req. 64).
const DISCORD_DEADLINE: Duration = Duration::from_secs(30);
const PAGE: u32 = 256;

pub(super) async fn router(ctx: Arc<Ctx>) -> Result<(), JobsError> {
    let mut log = EventLog::open(ctx.event_log.clone())?;
    let mut shutdown = ctx.shutdown.clone();
    loop {
        log.catch_up(&ctx.store).await?;
        deliver_waiting(&ctx).await?;
        if *shutdown.borrow() {
            return Ok(());
        }
        tokio::select! {
            () = ctx.route_wake.notified() => {}
            _ = shutdown.changed() => {}
        }
    }
}

/// The owner-readable JSON-lines event log.
struct EventLog {
    path: PathBuf,
    file: File,
    size: u64,
    last_id: i64,
}

impl EventLog {
    fn open(path: PathBuf) -> Result<Self, JobsError> {
        let fail = |e: crate::sys::fs::FsError| JobsError::Refused(format!("event log: {e}"));
        let mut file = open_append_private(&path).map_err(fail)?;
        let size = file.metadata().map_or(0, |m| m.len());
        let last_id = last_logged_id(&mut file, size);
        Ok(Self {
            path,
            file,
            size,
            last_id,
        })
    }

    async fn catch_up(&mut self, store: &Store) -> Result<(), JobsError> {
        loop {
            let after = self.last_id;
            let events = store.read(move |r| r.events_after(after, PAGE)).await?;
            let Some(last) = events.last().map(|e| e.id) else {
                return Ok(());
            };
            let mut text = String::new();
            for event in &events {
                if let Ok(line) = serde_json::to_string(event) {
                    text.push_str(&line);
                    text.push('\n');
                }
            }
            self.rotate_if_full();
            if let Err(e) = self.file.write_all(text.as_bytes()) {
                log::warn(
                    "event log write failed",
                    &[("path", &self.path.display()), ("error", &e)],
                );
            }
            self.size = self
                .size
                .saturating_add(u64::try_from(text.len()).unwrap_or(0));
            self.last_id = last;
        }
    }

    fn rotate_if_full(&mut self) {
        if self.size < LOG_ROTATE_BYTES {
            return;
        }
        let mut old = self.path.clone().into_os_string();
        old.push(".1");
        if std::fs::rename(&self.path, &old).is_ok()
            && let Ok(fresh) = open_append_private(&self.path)
        {
            self.file = fresh;
            self.size = 0;
        }
    }
}

/// ID of the last complete line in the log, so a restart appends exactly what is missing.
fn last_logged_id(file: &mut File, size: u64) -> i64 {
    let start = size.saturating_sub(LOG_TAIL);
    let mut tail = String::new();
    if file.seek(SeekFrom::Start(start)).is_err()
        || file.take(LOG_TAIL).read_to_string(&mut tail).is_err()
    {
        return 0;
    }
    tail.lines()
        .rev()
        .find_map(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()?
                .get("id")?
                .as_i64()
        })
        .unwrap_or(0)
}

async fn deliver_waiting(ctx: &Arc<Ctx>) -> Result<(), JobsError> {
    let waiting = ctx.store.read(|r| r.waiting_deliveries(PAGE)).await?;
    if waiting.is_empty() {
        return Ok(());
    }
    let config = ctx.config();
    for (event_id, destination) in waiting {
        let Some(dest) = config.destinations.get(&destination).cloned() else {
            let detail = Some("destination no longer configured".to_string());
            ctx.store
                .delivery(event_id, destination, Delivery::Failed, detail)
                .await?;
            continue;
        };
        let Some(event) = ctx.store.read(move |r| r.event(event_id)).await? else {
            continue;
        };
        ctx.store
            .delivery(event_id, destination.clone(), Delivery::Attempting, None)
            .await?;
        let task_ctx = Arc::clone(ctx);
        let slots = Arc::clone(&ctx.destination_slots);
        tokio::spawn(async move {
            let Ok(_slot) = slots.acquire_owned().await else {
                return;
            };
            let (state, detail) = attempt(&task_ctx, &destination, &dest, &event).await;
            if let Err(e) = task_ctx
                .store
                .delivery(event_id, destination.clone(), state, detail)
                .await
            {
                log::warn(
                    "delivery result not recorded",
                    &[("destination", &destination), ("error", &e)],
                );
            }
        });
    }
    Ok(())
}

/// Why an attempt did not end confirmed.
struct Undelivered {
    class: ErrorClass,
    detail: String,
}

/// One attempt; the error class decides the receipt: in doubt is recorded as uncertain and never
/// repeated, anything else as failed. A sent post keeps its message IDs as the receipt.
async fn attempt(
    ctx: &Ctx,
    name: &str,
    dest: &Destination,
    event: &EventRecord,
) -> (Delivery, Option<String>) {
    let result = match dest {
        Destination::Command { argv } => run_command(argv, event).await.map(|()| None),
        Destination::Discord { channel } => post_discord(ctx, name, channel, event).await.map(Some),
        Destination::Telegram { chat, topic } => {
            post_telegram(ctx, *chat, *topic, event).await.map(Some)
        }
        Destination::Slack { channel, thread } => {
            post_slack(ctx, channel, thread.as_deref(), event)
                .await
                .map(Some)
        }
    };
    match result {
        Ok(receipt) => (Delivery::Sent, receipt),
        Err(u) if u.class == ErrorClass::InDoubt => (Delivery::Uncertain, Some(u.detail)),
        Err(u) => (Delivery::Failed, Some(u.detail)),
    }
}

/// The text of a job post: the event line, then the body in a code block when the whole fits one
/// Discord message, else a pointer to where the body is (WS req. 64).
fn post_text(event: &EventRecord, log: &Path) -> String {
    let mut text = event.text.clone();
    if let Some(body) = event.body.as_deref().filter(|b| !b.trim().is_empty()) {
        let with_body = format!(
            "{text}\n```\n{}\n```",
            body.trim_end().replace("```", "'''")
        );
        if utf16_len(&with_body) <= DISCORD_LIMIT {
            return with_body;
        }
        let _ = write!(text, "\n(output in {}, event {})", log.display(), event.id);
    } else if let Some(path) = &event.body_path {
        let _ = write!(text, "\n(output in {path})");
    }
    text
}

/// One Discord post within the delivery deadline. Mentions are never parsed; the nonce comes from
/// the event and destination, so a retry after an uncertain answer cannot duplicate the post.
async fn post_discord(
    ctx: &Ctx,
    name: &str,
    channel: &ChannelRef,
    event: &EventRecord,
) -> Result<String, Undelivered> {
    let config = ctx.config();
    let rest = ctx.discord(&config).map_err(|detail| Undelivered {
        class: ErrorClass::Rejected,
        detail,
    })?;
    let text = post_text(event, &ctx.event_log);
    let seed = format!("event:{}:{name}", event.id);
    let posted = timeout(
        DISCORD_DEADLINE,
        rest.post(channel, &text, &seed, &config.outgoing),
    )
    .await
    .map_err(|_| Undelivered {
        class: ErrorClass::InDoubt,
        detail: "no answer within the 30 s delivery deadline".into(),
    })?;
    let ids: Vec<String> = posted.ids.iter().map(ToString::to_string).collect();
    match posted.outcome {
        Sent::Confirmed => Ok(format!("message {}", ids.join(" "))),
        Sent::Rejected(why) => Err(Undelivered {
            class: ErrorClass::Rejected,
            detail: with_ids(why, &ids),
        }),
        Sent::Uncertain(why) => Err(Undelivered {
            class: ErrorClass::InDoubt,
            detail: with_ids(why, &ids),
        }),
    }
}

/// One Telegram post within the delivery deadline, to the owner's DM or the configured group and
/// topic only (TELE req. 31). Telegram enforces no nonce, so an uncertain part is never repeated.
async fn post_telegram(
    ctx: &Ctx,
    chat: i64,
    topic: Option<i64>,
    event: &EventRecord,
) -> Result<String, Undelivered> {
    use crate::relay::telegram::{self, Api, Scope};
    let config = ctx.config();
    let refused = |detail: String| Undelivered {
        class: ErrorClass::Rejected,
        detail,
    };
    let api = Api::from_config(&config, &ctx.config_file).map_err(refused)?;
    let scope = Scope::of(&config.telegram)
        .ok_or_else(|| refused("set [telegram] owner before posting to Telegram".into()))?;
    let text = post_text(event, &ctx.event_log);
    crate::relay::text::check(&text, &config.outgoing)
        .map_err(|e| refused(format!("refused by the outgoing text checks: {e}")))?;
    let sent = timeout(
        DISCORD_DEADLINE,
        telegram::send(&api, &scope, chat, topic, None, &text, false),
    )
    .await
    .map_err(|_| Undelivered {
        class: ErrorClass::InDoubt,
        detail: "no answer within the 30 s delivery deadline".into(),
    })?;
    let ids = |v: &[i64]| v.iter().map(ToString::to_string).collect::<Vec<_>>();
    match sent {
        telegram::Sent::Confirmed(v) => Ok(format!("message {}", ids(&v).join(" "))),
        telegram::Sent::Rejected(v, why) => Err(refused(with_ids(why, &ids(&v)))),
        telegram::Sent::Uncertain(v, why) => Err(Undelivered {
            class: ErrorClass::InDoubt,
            detail: with_ids(why, &ids(&v)),
        }),
    }
}

/// One Slack post within the delivery deadline; the thread it starts or joins is remembered.
async fn post_slack(
    ctx: &Ctx,
    channel: &str,
    thread: Option<&str>,
    event: &EventRecord,
) -> Result<String, Undelivered> {
    use crate::relay::slack::{self, Api};
    let config = ctx.config();
    let api = Api::from_config(&config, &ctx.config_file).map_err(|detail| Undelivered {
        class: ErrorClass::Rejected,
        detail,
    })?;
    let text = post_text(event, &ctx.event_log);
    let sent = timeout(
        DISCORD_DEADLINE,
        slack::send(&api, &ctx.store, channel, thread, &text, &config.outgoing),
    )
    .await
    .map_err(|_| Undelivered {
        class: ErrorClass::InDoubt,
        detail: "no answer within the 30 s delivery deadline".into(),
    })?;
    match sent {
        Ok(ts) => Ok(format!("message {}", ts.join(" "))),
        Err((ts, e)) => Err(Undelivered {
            class: if e.uncertain {
                ErrorClass::InDoubt
            } else {
                ErrorClass::Rejected
            },
            detail: with_ids(e.to_string(), &ts),
        }),
    }
}

fn with_ids(why: String, ids: &[String]) -> String {
    if ids.is_empty() {
        why
    } else {
        format!("{why} (confirmed before it: message {})", ids.join(" "))
    }
}

async fn run_command(argv: &[String], event: &EventRecord) -> Result<(), Undelivered> {
    let json = serde_json::to_string(event).unwrap_or_default();
    let env = [("CATALY_EVENT", json.as_str())];
    let spec = Spec {
        argv,
        cwd: Some(Path::new("/")),
        env: &env,
        timeout: DESTINATION_TIMEOUT,
        kill_grace: KILL_GRACE,
        output_cap: DESTINATION_OUTPUT_CAP,
        output: Output::Capture,
    };
    let done = command::run(&spec).await.map_err(|e| Undelivered {
        class: e.class(),
        detail: e.to_string(),
    })?;
    if done.success() {
        return Ok(());
    }
    if done.timed_out {
        // The command may have acted before its time ran out.
        return Err(Undelivered {
            class: ErrorClass::InDoubt,
            detail: "time limit reached".into(),
        });
    }
    let said = if done.stderr.is_empty() {
        &done.stdout
    } else {
        &done.stderr
    };
    let first = String::from_utf8_lossy(said)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    let detail = format!("{:?}: {}", done.exit, super::event::one_line(&first, 200));
    Err(Undelivered {
        class: ErrorClass::Rejected,
        detail: crate::sys::secrets::mask(&detail).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::tests::ctx_with;
    use crate::store::tests::event;

    #[tokio::test]
    async fn log_and_command_destination() {
        let cfg = "[destinations.sink]\nkind = \"command\"\nargv = [\"sh\", \"-c\", \"printf %s \\\"$CATALY_EVENT\\\" > \\\"$0.out\\\"\", \"PLACEHOLDER\"]\n\
                   [destinations.broken]\nkind = \"command\"\nargv = [\"sh\", \"-c\", \"echo nope >&2; exit 4\"]\n";
        let (_c, _s, dir) = ctx_with("", "route-tmp");
        let target = dir.join("delivered");
        let cfg = cfg.replace("PLACEHOLDER", target.to_str().unwrap());
        let (ctx, stop, _dir) = ctx_with(&cfg, "route");
        ctx.stream
            .append(vec![event("j", "run.done", &["sink", "broken", "gone"])])
            .await
            .unwrap();
        let task = tokio::spawn(router(Arc::clone(&ctx)));
        let mut states = Vec::new();
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            states = ctx.store.read(|r| r.deliveries(1)).await.unwrap();
            if states
                .iter()
                .all(|(_, s, _)| s != "waiting" && s != "attempting")
            {
                break;
            }
        }
        let by = |d: &str| states.iter().find(|(n, _, _)| n == d).cloned().unwrap();
        assert_eq!(by("sink").1, "sent");
        assert_eq!(by("broken").1, "failed");
        assert!(by("broken").2.unwrap().contains("nope"));
        assert_eq!(by("gone").1, "failed");
        let delivered: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("delivered.out")).unwrap())
                .unwrap();
        assert_eq!(delivered["kind"], "run.done");
        let log = std::fs::read_to_string(&ctx.event_log).unwrap();
        assert_eq!(log.lines().count(), 1);
        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
        // A new router appends only what is missing.
        ctx.stream
            .append(vec![event("j", "run.done", &[])])
            .await
            .unwrap();
        let mut log2 = EventLog::open(ctx.event_log.clone()).unwrap();
        assert_eq!(log2.last_id, 1);
        log2.catch_up(&ctx.store).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&ctx.event_log)
                .unwrap()
                .lines()
                .count(),
            2
        );
    }
}
