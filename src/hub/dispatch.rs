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
    self, Events, EventsParams, Incoming, JobList, JobParams, Mutation, Response, code, method,
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

const METHODS: [Method; 8] = [
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
        _ => Err(Failure {
            code: code::UNKNOWN_METHOD,
            message: format!("unknown method '{name}'"),
        }),
    }
}
