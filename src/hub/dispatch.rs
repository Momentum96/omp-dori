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

/// Who is calling. Every CLI connection of the owner's user is the owner in this slice; session roles
/// arrive with the bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Authority {
    Owner,
}

struct Method {
    name: &'static str,
    needs: Authority,
    mutating: bool,
}

const METHODS: [Method; 31] = [
    Method {
        name: method::STATUS,
        needs: Authority::Owner,
        mutating: false,
    },
    Method {
        name: method::STATS,
        needs: Authority::Owner,
        mutating: false,
    },
    Method {
        name: method::EVENTS_SINCE,
        needs: Authority::Owner,
        mutating: false,
    },
    Method {
        name: method::JOB_LIST,
        needs: Authority::Owner,
        mutating: false,
    },
    Method {
        name: method::JOB_RUN,
        needs: Authority::Owner,
        mutating: true,
    },
    Method {
        name: method::JOB_ENABLE,
        needs: Authority::Owner,
        mutating: true,
    },
    Method {
        name: method::JOB_DISABLE,
        needs: Authority::Owner,
        mutating: true,
    },
    Method {
        name: method::CONFIG_RELOAD,
        needs: Authority::Owner,
        mutating: true,
    },
    Method {
        name: method::HEALTH,
        needs: Authority::Owner,
        mutating: false,
    },
    Method {
        name: method::LIMITS,
        needs: Authority::Owner,
        mutating: false,
    },
    Method {
        name: method::QUOTA,
        needs: Authority::Owner,
        mutating: false,
    },
    // Only the owner lifts the quota gate (OJ req. 25).
    Method {
        name: method::QUOTA_LIFT,
        needs: Authority::Owner,
        mutating: true,
    },
    Method {
        name: method::PR_FOLLOW,
        needs: Authority::Owner,
        mutating: true,
    },
    Method {
        name: method::PR_UNFOLLOW,
        needs: Authority::Owner,
        mutating: true,
    },
    Method {
        name: method::PR_LIST,
        needs: Authority::Owner,
        mutating: false,
    },
    Method {
        name: method::LEAD_START,
        needs: Authority::Owner,
        mutating: true,
    },
    Method {
        name: method::LEAD_STOP,
        needs: Authority::Owner,
        mutating: true,
    },
    Method {
        name: method::LEAD_STATUS,
        needs: Authority::Owner,
        mutating: false,
    },
    // Its `request_id` is the receipt key with its own digest rule (HOST req. 57), not the
    // dispatcher's reply dedupe.
    Method {
        name: method::LEAD_SEND,
        needs: Authority::Owner,
        mutating: false,
    },
    Method {
        name: method::LEAD_RECEIPT,
        needs: Authority::Owner,
        mutating: false,
    },
    Method {
        name: method::LEAD_INBOX,
        needs: Authority::Owner,
        mutating: false,
    },
    Method {
        name: method::LEAD_CONFIRM,
        needs: Authority::Owner,
        mutating: true,
    },
    Method {
        name: method::LEAD_RELEASE,
        needs: Authority::Owner,
        mutating: true,
    },
    Method {
        name: method::LEAD_INTERRUPT,
        needs: Authority::Owner,
        mutating: true,
    },
    Method {
        name: method::LEAD_MODEL,
        needs: Authority::Owner,
        mutating: true,
    },
    Method {
        name: method::LEAD_ACCEPT_MODEL,
        needs: Authority::Owner,
        mutating: true,
    },
    Method {
        name: method::LEAD_ANSWER,
        needs: Authority::Owner,
        mutating: true,
    },
    Method {
        name: method::LEAD_EVENTS,
        needs: Authority::Owner,
        mutating: false,
    },
    Method {
        name: method::DISCORD_STATUS,
        needs: Authority::Owner,
        mutating: false,
    },
    Method {
        name: method::DISCORD_RELEASE,
        needs: Authority::Owner,
        mutating: true,
    },
    Method {
        name: method::DISCORD_RESET,
        needs: Authority::Owner,
        mutating: true,
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
    if caller != spec.needs {
        return Response::err(
            id,
            ErrorClass::Rejected.as_str(),
            "not allowed for this caller",
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
            let jobs = hub.reload().await.map_err(|message| Failure {
                code: code::REFUSED,
                message,
            })?;
            ok(&proto::Reloaded { jobs })
        }
        method::HEALTH => {
            let checks = hub.jobs.health().await?;
            let ignored = hub.config_tx.borrow().health.ignore.clone();
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
        method::QUOTA => ok(&hub.jobs.gate()),
        method::QUOTA_LIFT => {
            let _: Mutation = params(raw)?;
            ok(&hub.jobs.lift().await?)
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
        discord if discord.starts_with("discord.") => discord_method(hub, discord, raw).await,
        _ => Err(Failure {
            code: code::UNKNOWN_METHOD,
            message: format!("unknown method '{name}'"),
        }),
    }
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

/// The `discord.*` methods: intake status (DISC-IN req. 60), releasing a held room, and the reset of
/// intake state bound to another bot or owner (DISC-IN req. 3).
async fn discord_method(hub: &Hub, name: &str, raw: Value) -> Result<Value, Failure> {
    use crate::store::relay::{OWED_WINDOW_MS, RelayWrite};
    let p: proto::DiscordParams = params(raw)?;
    let relay = crate::relay::discord::intake::NAME;
    let store_failure = |e: crate::store::StoreError| Failure {
        code: code::INTERNAL,
        message: e.to_string(),
    };
    match name {
        method::DISCORD_STATUS => {
            let since = crate::sys::now_ms() - OWED_WINDOW_MS;
            let (counts, positions, owed) = hub
                .store
                .read(move |r| {
                    Ok((
                        r.relay_counts(relay, since)?,
                        r.positions(relay)?,
                        r.owed(relay, since)?,
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
            let health = hub.discord.health();
            let healthy = health.state == "connected" && health.caught_up && counts.queued == 0;
            ok(&serde_json::json!({
                "health": health,
                "healthy": healthy,
                "counts": counts,
                "held": held,
                "rooms": rooms,
                "owed": open,
            }))
        }
        method::DISCORD_RELEASE => {
            let room = p
                .room
                .filter(|r| crate::relay::discord::Snowflake::parse(r).is_some())
                .ok_or(Failure {
                    code: code::BAD_REQUEST,
                    message: "name the room (a Discord channel or thread ID)".into(),
                })?;
            hub.store
                .relay(RelayWrite::Hold {
                    relay: relay.into(),
                    room,
                    reason: None,
                })
                .await
                .map_err(store_failure)?;
            hub.discord.kick.notify_one();
            Ok(Value::Object(serde_json::Map::new()))
        }
        method::DISCORD_RESET => {
            hub.store
                .relay(RelayWrite::Reset {
                    relay: relay.into(),
                })
                .await
                .map_err(store_failure)?;
            hub.discord.kick.notify_one();
            Ok(Value::Object(serde_json::Map::new()))
        }
        _ => Err(Failure {
            code: code::UNKNOWN_METHOD,
            message: format!("unknown method '{name}'"),
        }),
    }
}

/// The account-limits view (WS req. 37-39): off unless enabled; never retries or changes anything.
async fn limits(hub: &Hub, refresh: bool) -> Result<Limits, Failure> {
    let settings = hub.config_tx.borrow().limits.clone();
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
                .find(|a| a.provider == l.provider_shown() && a.account == l.account)
            {
                Some(a) => a.windows.push(window),
                None => accounts.push(LimitAccount {
                    provider: l.provider_shown().to_string(),
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
            .filter(|l| l.provider_shown() == a.provider && l.account == a.account)
            .filter_map(|l| l.window_ms)
            .collect()
    })
}
