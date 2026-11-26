//! The one request dispatcher (DESIGN 8, 9.1): every method declares the authority it needs and
//! whether it mutates; mutating calls may carry a caller request ID for dedupe.

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::Hub;
use crate::jobs::JobsError;
use crate::proto::{
    self, Events, EventsParams, Incoming, JobList, JobParams, LimitAccount, LimitWindow, Limits,
    LimitsParams, Mutation, PrList, Response, TargetParams, code, method,
};
use crate::sys::ErrorClass;

/// Who is calling (DESIGN 8, 9.1). A control connection is the owner unless its process is, or
/// descends from, the OMP process of a session the hub runs: then it has that session's role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Authority {
    Owner,
    /// A process of the lead's session.
    Lead,
    /// A process of a lane's session, with the lane's key.
    Lane(String),
    /// A process that carries a session's launch token but whose session the hub cannot place
    /// (its ancestry is unknown or leads to no session): it is never the owner.
    Unplaced,
}

impl Authority {
    /// The role's name in `whoami` and refusals.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Lead => "lead",
            Self::Lane(_) => "lane",
            Self::Unplaced => "unplaced",
        }
    }
}

/// What a method needs from its caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Needs {
    /// `Owner` only: the method changes, sends, answers or authorises something.
    Owner,
    /// `Owner`, `Lead`, `Lane` or `Unplaced`: the method only reads.
    Any,
}

struct Method {
    name: &'static str,
    needs: Needs,
    mutating: bool,
}

const METHODS: [Method; 42] = [
    Method {
        name: method::STATUS,
        needs: Needs::Any,
        mutating: false,
    },
    Method {
        name: method::STATS,
        needs: Needs::Any,
        mutating: false,
    },
    Method {
        name: method::EVENTS_SINCE,
        needs: Needs::Any,
        mutating: false,
    },
    Method {
        name: method::JOB_LIST,
        needs: Needs::Any,
        mutating: false,
    },
    Method {
        name: method::JOB_RUN,
        needs: Needs::Owner,
        mutating: true,
    },
    Method {
        name: method::JOB_ENABLE,
        needs: Needs::Owner,
        mutating: true,
    },
    Method {
        name: method::JOB_DISABLE,
        needs: Needs::Owner,
        mutating: true,
    },
    Method {
        name: method::CONFIG_RELOAD,
        needs: Needs::Owner,
        mutating: true,
    },
    Method {
        name: method::HEALTH,
        needs: Needs::Any,
        mutating: false,
    },
    Method {
        name: method::LIMITS,
        needs: Needs::Any,
        mutating: false,
    },
    Method {
        name: method::PR_FOLLOW,
        needs: Needs::Owner,
        mutating: true,
    },
    Method {
        name: method::PR_UNFOLLOW,
        needs: Needs::Owner,
        mutating: true,
    },
    Method {
        name: method::PR_LIST,
        needs: Needs::Any,
        mutating: false,
    },
    Method {
        name: method::LEAD_START,
        needs: Needs::Owner,
        mutating: true,
    },
    Method {
        name: method::LEAD_STOP,
        needs: Needs::Owner,
        mutating: true,
    },
    Method {
        name: method::LEAD_STATUS,
        needs: Needs::Any,
        mutating: false,
    },
    // Its `request_id` is the receipt key with its own digest rule (HOST req. 57), not the
    // dispatcher's reply dedupe.
    Method {
        name: method::LEAD_SEND,
        needs: Needs::Owner,
        mutating: false,
    },
    Method {
        name: method::LEAD_RECEIPT,
        needs: Needs::Owner,
        mutating: false,
    },
    Method {
        name: method::LEAD_INBOX,
        needs: Needs::Owner,
        mutating: false,
    },
    Method {
        name: method::LEAD_CONFIRM,
        needs: Needs::Owner,
        mutating: true,
    },
    Method {
        name: method::LEAD_RELEASE,
        needs: Needs::Owner,
        mutating: true,
    },
    Method {
        name: method::LEAD_INTERRUPT,
        needs: Needs::Owner,
        mutating: true,
    },
    Method {
        name: method::LEAD_MODEL,
        needs: Needs::Owner,
        mutating: true,
    },
    Method {
        name: method::LEAD_ACCEPT_MODEL,
        needs: Needs::Owner,
        mutating: true,
    },
    Method {
        name: method::LEAD_ANSWER,
        needs: Needs::Owner,
        mutating: true,
    },
    Method {
        name: method::LEAD_EVENTS,
        needs: Needs::Owner,
        mutating: false,
    },
    Method {
        name: method::DISCORD_STATUS,
        needs: Needs::Any,
        mutating: false,
    },
    Method {
        name: method::DISCORD_RELEASE,
        needs: Needs::Owner,
        mutating: true,
    },
    Method {
        name: method::DISCORD_RESET,
        needs: Needs::Owner,
        mutating: true,
    },
    Method {
        name: method::TELEGRAM_STATUS,
        needs: Needs::Any,
        mutating: false,
    },
    Method {
        name: method::TELEGRAM_RELEASE,
        needs: Needs::Owner,
        mutating: true,
    },
    Method {
        name: method::TELEGRAM_RESET,
        needs: Needs::Owner,
        mutating: true,
    },
    Method {
        name: method::SLACK_STATUS,
        needs: Needs::Any,
        mutating: false,
    },
    Method {
        name: method::SLACK_RELEASE,
        needs: Needs::Owner,
        mutating: true,
    },
    Method {
        name: method::SLACK_RESET,
        needs: Needs::Owner,
        mutating: true,
    },
    Method {
        name: method::SLACK_UPLOAD,
        needs: Needs::Owner,
        mutating: true,
    },
    Method {
        name: method::VOICE_STATUS,
        needs: Needs::Any,
        mutating: false,
    },
    // The hold is lifted only by the owner (VOICE req. 83).
    Method {
        name: method::VOICE_RESTART,
        needs: Needs::Owner,
        mutating: true,
    },
    Method {
        name: method::VOICE_REPLY,
        needs: Needs::Owner,
        mutating: true,
    },
    // A lane open's `request_id` is the lane's own dedupe with its digest rule (LANE req. 10), not
    // the dispatcher's reply dedupe, which would answer other inputs with the earlier reply.
    Method {
        name: method::LANE,
        needs: Needs::Owner,
        mutating: false,
    },
    // The orderly stop the restart option asks for (IR req. 33): refused while a session works.
    Method {
        name: method::HUB_STOP,
        needs: Needs::Owner,
        mutating: false,
    },
    Method {
        name: method::WHOAMI,
        needs: Needs::Any,
        mutating: false,
    },
];

/// Replies of recent mutating calls, by (method, caller request ID).
pub(crate) struct Dedupe(Mutex<VecDeque<(String, String, Response)>>);

const DEDUPE_KEEP: usize = 256;

impl Dedupe {
    pub(crate) fn new() -> Self {
        Self(Mutex::new(VecDeque::new()))
    }

    fn get(&self, method: &str, request: &str) -> Option<Response> {
        let seen = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        seen.iter()
            .find(|(m, r, _)| m == method && r == request)
            .map(|(_, _, reply)| reply.clone())
    }

    fn put(&self, method: &str, request: &str, reply: &Response) {
        let mut seen = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if seen.len() >= DEDUPE_KEEP {
            seen.pop_front();
        }
        seen.push_back((method.to_string(), request.to_string(), reply.clone()));
    }
}

/// An error on its way to the wire.
struct Failure {
    code: &'static str,
    message: String,
}

impl From<JobsError> for Failure {
    fn from(e: JobsError) -> Self {
        let code = match &e {
            JobsError::NotFound(_) => code::NOT_FOUND,
            JobsError::Busy(_) => code::BUSY,
            JobsError::Refused(_) => code::REFUSED,
            JobsError::Store(_) | JobsError::Fs(_) | JobsError::Stopped => e.class().as_str(),
        };
        Self {
            code,
            message: e.to_string(),
        }
    }
}

impl From<crate::store::StoreError> for Failure {
    fn from(e: crate::store::StoreError) -> Self {
        Self {
            code: e.class().as_str(),
            message: e.to_string(),
        }
    }
}

fn params<T: DeserializeOwned>(value: Value) -> Result<T, Failure> {
    let value = if value.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        value
    };
    serde_json::from_value(value).map_err(|e| Failure {
        code: code::BAD_REQUEST,
        message: e.to_string(),
    })
}

fn ok<T: Serialize>(value: &T) -> Result<Value, Failure> {
    serde_json::to_value(value).map_err(|e| Failure {
        code: code::INTERNAL,
        message: e.to_string(),
    })
}

/// Answers one request.
pub(crate) async fn dispatch(hub: &Hub, request: Incoming, caller: Authority) -> Response {
    let Incoming {
        id,
        method: name,
        params: raw,
        ..
    } = request;
    let Some(spec) = METHODS.iter().find(|m| m.name == name) else {
        return Response::err(id, code::UNKNOWN_METHOD, format!("unknown method '{name}'"));
    };
    if spec.needs == Needs::Owner && caller != Authority::Owner {
        return Response::err(
            id,
            ErrorClass::Rejected.as_str(),
            format!(
                "{name} is the owner's alone; this caller runs inside a Cataly session ({}) and \
                 cannot act with the owner's authority",
                caller.name()
            ),
        );
    }
    if spec.name == method::WHOAMI {
        let lane = match &caller {
            Authority::Lane(key) => Some(key.clone()),
            _ => None,
        };
        return Response::ok(
            id,
            serde_json::json!({ "role": caller.name(), "lane": lane }),
        );
    }
    let request_id = spec
        .mutating
        .then(|| {
            raw.get("request_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .flatten();
    if let Some(rid) = &request_id
        && let Some(mut earlier) = hub.dedupe.get(spec.name, rid)
    {
        earlier.id = id;
        return earlier;
    }
    let reply = match handle(hub, spec.name, raw).await {
        Ok(value) => Response::ok(id, value),
        Err(f) => Response::err(id, f.code, f.message),
    };
    if let Some(rid) = &request_id {
        hub.dedupe.put(spec.name, rid, &reply);
    }
    reply
}

async fn handle(hub: &Hub, name: &str, raw: Value) -> Result<Value, Failure> {
    match name {
        method::STATUS => ok(&hub.status().await?),
        method::LANE => hub
            .lanes
            .call(crate::lanes::Caller::Owner, &raw)
            .await
            .map_err(refused),
        method::STATS => ok(&hub.stats().await),
        method::EVENTS_SINCE => {
            let p: EventsParams = params(raw)?;
            let limit = p.limit.unwrap_or(20).clamp(1, 1000);
            let events = hub
                .store
                .read(move |r| match (p.after, p.since_ms) {
                    (Some(after), _) => r.events_after(after, limit),
                    (None, Some(since)) => r.events_since(since, limit),
                    (None, None) => r.events_latest(limit),
                })
                .await?;
            ok(&Events { events })
        }
        method::JOB_LIST => ok(&JobList {
            jobs: hub.jobs.list().await?,
        }),
        method::JOB_RUN => {
            let p: JobParams = params(raw)?;
            ok(&Events {
                events: hub.jobs.run_once(&p.name).await?,
            })
        }
        method::JOB_ENABLE => {
            let p: JobParams = params(raw)?;
            hub.jobs.resume(&p.name, p.ack).await?;
            Ok(Value::Object(serde_json::Map::new()))
        }
        method::JOB_DISABLE => {
            let p: JobParams = params(raw)?;
            hub.jobs.hold(&p.name).await?;
            Ok(Value::Object(serde_json::Map::new()))
        }
        method::CONFIG_RELOAD => {
            let _: Mutation = params(raw)?;
            let jobs = hub.jobs.reload().await.map_err(|message| Failure {
                code: code::REFUSED,
                message,
            })?;
            ok(&proto::Reloaded { jobs })
        }
        method::HEALTH => {
            let checks = hub.jobs.health().await?;
            let ignored = hub.config().health.ignore.clone();
            let warn = checks
                .iter()
                .any(|c| c.level == "warn" && !ignored.contains(&c.name));
            ok(&proto::Health {
                checks,
                ignored,
                warn,
            })
        }
        method::LIMITS => {
            let p: LimitsParams = params(raw)?;
            ok(&limits(hub, p.refresh).await?)
        }
        method::PR_FOLLOW | method::PR_UNFOLLOW => {
            let p: TargetParams = params(raw)?;
            let target = p.target.unwrap_or_default();
            let pr = if name == method::PR_FOLLOW {
                hub.jobs.pr_follow(&target, p.job.as_deref()).await?
            } else {
                hub.jobs.pr_unfollow(&target, p.job.as_deref()).await?
            };
            ok(&serde_json::json!({ "pr": pr }))
        }
        method::PR_LIST => ok(&PrList {
            prs: hub.jobs.pr_list().await?,
        }),
        lead if lead.starts_with("lead.") => lead_method(hub, lead, raw).await,
        relay
            if relay.starts_with("discord.")
                || relay.starts_with("telegram.")
                || relay.starts_with("slack.") =>
        {
            relay_method(hub, relay, raw).await
        }
        // The stop begins once this acceptance is written (`socket::connection`).
        method::HUB_STOP => hub.may_stop().map(|()| Value::Null).map_err(refused),
        voice if voice.starts_with("voice.") => voice_method(hub, voice, raw).await,
        _ => Err(Failure {
            code: code::UNKNOWN_METHOD,
            message: format!("unknown method '{name}'"),
        }),
    }
}

#[cfg(feature = "voice")]
async fn voice_method(hub: &Hub, name: &str, raw: Value) -> Result<Value, Failure> {
    hub.voice.call_method(name, raw).await.map_err(refused)
}

/// A build without voice answers status with why voice is off and refuses the rest.
#[cfg(not(feature = "voice"))]
#[allow(clippy::unused_async)]
async fn voice_method(hub: &Hub, name: &str, _raw: Value) -> Result<Value, Failure> {
    let config = hub.config();
    let reason = config.voice.refusal().unwrap_or_else(|| {
        "this build has no voice support (build with the `voice` feature)".into()
    });
    if name == method::VOICE_STATUS {
        return ok(&serde_json::json!({ "state": "off", "reason": reason }));
    }
    Err(refused(format!("voice is off: {reason}")))
}

fn refused(message: String) -> Failure {
    let code = if message.starts_with("busy") {
        code::BUSY
    } else {
        code::REFUSED
    };
    Failure { code, message }
}

/// The `lead.*` methods (COORD-01, COORD-13, COORD-18, HOST-14, HOST-17, HOST-18).
async fn lead_method(hub: &Hub, name: &str, raw: Value) -> Result<Value, Failure> {
    let p: proto::LeadParams = params(raw)?;
    let lead = &hub.lead;
    let done = || Ok(Value::Object(serde_json::Map::new()));
    match name {
        method::LEAD_START => ok(&lead.start(p.fresh).await.map_err(refused)?),
        method::LEAD_STOP => ok(&lead.stop(p.force).await.map_err(refused)?),
        method::LEAD_STATUS => {
            let view = lead.view().await;
            let mut value = serde_json::to_value(&view).map_err(|e| refused(e.to_string()))?;
            let bridge = view
                .session
                .as_ref()
                .map_or_default(|s| hub.links.view(&s.handle));
            if let Value::Object(map) = &mut value {
                map.insert(
                    "bridge".into(),
                    serde_json::to_value(bridge).unwrap_or(Value::Null),
                );
            }
            Ok(value)
        }
        method::LEAD_SEND => {
            let request = crate::host::inbox::Message {
                sender: "owner".into(),
                text: p.text.unwrap_or_default(),
                urgent: !p.quiet,
                owner: true,
                identity: None,
                request_id: p.request_id,
            };
            ok(&lead.deliver(request).await.map_err(refused)?)
        }
        method::LEAD_RECEIPT => {
            let rid = p.request_id.unwrap_or_default();
            match lead.receipt(&rid).await.map_err(refused)? {
                Some(receipt) => ok(&receipt),
                None => Err(Failure {
                    code: code::NOT_FOUND,
                    message: format!("no receipt for request ID '{rid}'"),
                }),
            }
        }
        method::LEAD_INBOX => ok(&lead.inbox().await.map_err(refused)?),
        method::LEAD_CONFIRM | method::LEAD_RELEASE => {
            let index = p.index.unwrap_or(0);
            lead.settle(index, name == method::LEAD_CONFIRM)
                .await
                .map_err(refused)?;
            done()
        }
        method::LEAD_INTERRUPT => ok(&lead.interrupt().await.map_err(refused)?),
        method::LEAD_MODEL => ok(&lead
            .set_model(p.model.as_deref().unwrap_or_default())
            .await
            .map_err(refused)?),
        method::LEAD_ACCEPT_MODEL => {
            lead.accept_model().map_err(refused)?;
            done()
        }
        method::LEAD_ANSWER => {
            let answer = p.answer.unwrap_or(Value::Null);
            lead.answer(p.dialog.as_deref().unwrap_or_default(), &answer)
                .await
                .map_err(refused)?;
            done()
        }
        method::LEAD_EVENTS => ok(&lead.events(p.after.unwrap_or(0)).map_err(refused)?),
        _ => Err(Failure {
            code: code::UNKNOWN_METHOD,
            message: format!("unknown method '{name}'"),
        }),
    }
}

/// The `<relay>.*` methods: intake status (DISC-IN req. 60, MSG req. 15), releasing a held room, the
/// reset of intake state bound to another account or owner (DISC-IN req. 3), and `slack.upload`.
async fn relay_method(hub: &Hub, name: &str, raw: Value) -> Result<Value, Failure> {
    use crate::store::relay::{OWED_WINDOW_MS, RelayWrite};
    let p: proto::DiscordParams = params(raw)?;
    let (relay, verb) = name.split_once('.').unwrap_or((name, ""));
    let shared = match relay {
        "discord" => &hub.discord,
        "telegram" => &hub.telegram,
        _ => &hub.slack,
    };
    let store_failure = |e: crate::store::StoreError| Failure {
        code: code::INTERNAL,
        message: e.to_string(),
    };
    let relay_name = relay.to_string();
    match verb {
        "status" => {
            let since = crate::sys::now_ms() - OWED_WINDOW_MS;
            let r = relay_name.clone();
            let (counts, positions, owed) = hub
                .store
                .read(move |s| {
                    Ok((
                        s.relay_counts(&r, since)?,
                        s.positions(&r)?,
                        s.owed(&r, since)?,
                    ))
                })
                .await
                .map_err(store_failure)?;
            let held: Vec<Value> = positions
                .iter()
                .filter_map(|p| {
                    p.held
                        .as_ref()
                        .map(|why| serde_json::json!({"room": p.room, "reason": why}))
                })
                .collect();
            let rooms: Vec<Value> = positions
                .iter()
                .map(|p| serde_json::json!({"room": p.room, "position": p.position.to_string(), "parent": p.parent}))
                .collect();
            let open: Vec<_> = owed
                .into_iter()
                .filter(|o| o.settled_ms.is_none())
                .collect();
            let health = shared.health();
            let healthy = health.state == "connected" && health.caught_up && counts.queued == 0;
            let lag = shared
                .receive_lag
                .summary()
                .map(proto::LatencySummary::from);
            ok(&serde_json::json!({
                "health": health,
                "healthy": healthy,
                "counts": counts,
                "held": held,
                "rooms": rooms,
                "owed": open,
                "receive_lag": lag,
            }))
        }
        "release" => {
            let room = p
                .room
                .filter(|r| !r.is_empty() && r.len() <= 128)
                .ok_or(Failure {
                    code: code::BAD_REQUEST,
                    message: "name the room".into(),
                })?;
            hub.store
                .relay(RelayWrite::Hold {
                    relay: relay_name,
                    room,
                    reason: None,
                })
                .await
                .map_err(store_failure)?;
            shared.kick.notify_one();
            Ok(Value::Object(serde_json::Map::new()))
        }
        "reset" => {
            hub.store
                .relay(RelayWrite::Reset { relay: relay_name })
                .await
                .map_err(store_failure)?;
            shared.kick.notify_one();
            Ok(Value::Object(serde_json::Map::new()))
        }
        "upload" if relay == "slack" => slack_upload(hub, p).await,
        _ => Err(Failure {
            code: code::UNKNOWN_METHOD,
            message: format!("unknown method '{name}'"),
        }),
    }
}

/// `slack.upload`: one file into a conversation (SLACK req. 47); its file ID.
async fn slack_upload(hub: &Hub, p: proto::DiscordParams) -> Result<Value, Failure> {
    let bad = |message: String| Failure {
        code: code::BAD_REQUEST,
        message,
    };
    let channel = p
        .channel
        .ok_or_else(|| bad("name the conversation".into()))?;
    let file = std::path::PathBuf::from(p.file.ok_or_else(|| bad("name the file".into()))?);
    if !file.is_absolute() {
        return Err(bad("the file path must be absolute".into()));
    }
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| bad("the path names no file".into()))?;
    let read = file.clone();
    let bytes = tokio::task::spawn_blocking(move || {
        let size = std::fs::metadata(&read)?.len();
        if size > crate::relay::slack::UPLOAD_MAX {
            return Err(std::io::Error::from(std::io::ErrorKind::FileTooLarge));
        }
        std::fs::read(&read)
    })
    .await
    .map_err(|e| bad(e.to_string()))?
    .map_err(|e| bad(format!("{}: {}", file.display(), e.kind())))?;
    let config = hub.config();
    let api =
        crate::relay::slack::Api::from_config(&config, &hub.paths.config_file).map_err(bad)?;
    let id = crate::relay::slack::upload(
        &api,
        &hub.store,
        &channel,
        p.thread.as_deref(),
        &name,
        bytes,
    )
    .await
    .map_err(|e| Failure {
        code: code::INTERNAL,
        message: if e.uncertain {
            format!("the upload may or may not have been posted: {e}")
        } else {
            format!("not uploaded: {e}")
        },
    })?;
    ok(&serde_json::json!({ "file": id }))
}

/// The account-limits view (WS req. 37-39): off unless enabled; never retries or changes anything.
async fn limits(hub: &Hub, refresh: bool) -> Result<Limits, Failure> {
    let settings = hub.config().limits.clone();
    if !settings.enabled {
        return Err(Failure {
            code: code::REFUSED,
            message: "the account-limits view is off; set [limits] enabled = true".into(),
        });
    }
    let omp = hub.jobs.ctx().omp.clone();
    let read = omp
        .usage
        .read(omp.path().as_deref(), settings.cache, refresh)
        .await;
    let mut accounts: Vec<LimitAccount> = Vec::new();
    let now = crate::sys::now_ms();
    let (mut without_usage, mut disabled, mut fetched_ms) = (0, 0, None);
    let mut stale = read.stale;
    if let Some(snapshot) = &read.snapshot {
        fetched_ms = Some(snapshot.fetched_ms);
        let age = now - snapshot.fetched_ms;
        stale |= age < 0 || age > crate::jobs::duration_ms(settings.cache);
        without_usage = snapshot.usage.accounts_without_data;
        disabled = snapshot.usage.disabled_credentials;
        for l in &snapshot.usage.limits {
            let window = LimitWindow {
                id: l.id.clone(),
                label: l.label.clone(),
                used_percent: l.used_percent,
                reset_ms: l.reset_ms,
                headroom_percent: settings
                    .warn_percent
                    .zip(l.used_percent)
                    .map(|(w, u)| f64::from(w) - u),
            };
            match accounts
                .iter_mut()
                .find(|a| a.provider == l.provider && a.account == l.account)
            {
                Some(a) => a.windows.push(window),
                None => accounts.push(LimitAccount {
                    provider: l.provider.clone(),
                    account: l.account,
                    windows: vec![window],
                    notes: Vec::new(),
                }),
            }
        }
    }
    let hour = 3_600_000;
    for a in &mut accounts {
        let lengths: Vec<i64> = snapshot_windows(read.snapshot.as_ref(), a);
        if !lengths.contains(&(5 * hour)) {
            a.notes.push("no five-hour window reported".into());
        }
        if !lengths.contains(&(7 * 24 * hour)) {
            a.notes.push("no weekly window reported".into());
        }
    }
    Ok(Limits {
        accounts,
        stale,
        fetched_ms,
        error: read.error,
        warn_percent: settings.warn_percent,
        without_usage,
        disabled,
    })
}

/// Window lengths of one account's limits.
fn snapshot_windows(snapshot: Option<&crate::omp::usage::Snapshot>, a: &LimitAccount) -> Vec<i64> {
    snapshot.map_or_else(Vec::new, |s| {
        s.usage
            .limits
            .iter()
            .filter(|l| l.provider == a.provider && l.account == a.account)
            .filter_map(|l| l.window_ms)
            .collect()
    })
}
