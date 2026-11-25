//! Usage counts (COORD-50, req. 69-71; DESIGN 12.2 A26): off unless the owner opts in and names a
//! collector; there is no default one, and `DO_NOT_TRACK` always wins. Once an hour the hub counts
//! the events of a fixed list of kinds that entered its stream since the last send and posts them,
//! with the first-install and first-use-of-the-day records, as OTLP/HTTP JSON logs. Only counts and
//! fixed values leave the machine; a failed send is dropped and never slows anything.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::watch;

use super::Jobs;
use super::event::{Emit, EventKind, Severity};
use super::memory::Memory;
use crate::config::release::UsageSettings;
use crate::log;
use crate::relay::http::{self, Kind, Outcome, Policy, Request};
use crate::store::Apply;
use crate::sys::now_ms;

const JOB: &str = "usage";
const SUBJECT: &str = "counts";
/// One send an hour (req. 70: the hourly summary).
const EVERY: Duration = Duration::from_hours(1);
/// Every send: 2 s in all (req. 71).
const SEND_TIMEOUT: Duration = Duration::from_secs(2);
/// Events read per pass.
const PAGE: u32 = 10_000;
/// The kinds counted; nothing else about an event is sent.
const COUNTED: [&str; 9] = [
    "lane.started",
    "lane.claimed",
    "lane.closed",
    "lane.lost",
    "lane.alert",
    "lane.recap",
    "planner.run",
    "pr.merged",
    "tick.failed",
];

/// The common do-not-track convention: any value but empty or `0` means no.
fn do_not_track() -> bool {
    std::env::var("DO_NOT_TRACK").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Runs until the hub stops.
pub(crate) async fn run(
    jobs: Jobs,
    state_dir: PathBuf,
    mut stop: watch::Receiver<bool>,
) -> Result<(), String> {
    // A key the collector refused is not sent again until the hub restarts.
    let mut key_refused = false;
    loop {
        let config = Arc::clone(&jobs.ctx().config.borrow());
        let wait = if config.usage.enabled && !do_not_track() {
            send(&jobs, &config.usage, &state_dir, &mut key_refused).await?;
            EVERY
        } else {
            Duration::from_secs(30)
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            _ = stop.changed() => return Ok(()),
        }
    }
}

async fn send(
    jobs: &Jobs,
    settings: &UsageSettings,
    state_dir: &Path,
    key_refused: &mut bool,
) -> Result<(), String> {
    let Some(endpoint) = &settings.endpoint else {
        return Ok(());
    };
    let rows = jobs
        .ctx()
        .store
        .read(|r| r.memory(JOB, false))
        .await
        .map_err(|e| e.to_string())?;
    let mut memory = Memory::new(JOB, false, rows);
    if memory.transition(SUBJECT, "notice", "on") {
        let emit = Emit::new(EventKind::UsageOn, Severity::Info, false, "hub")
            .with("endpoint", "the collector named in [usage] endpoint");
        jobs.hub_event(emit).await.map_err(|e| e.to_string())?;
    }
    let after = memory
        .get(SUBJECT, "cursor")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    let events = jobs
        .ctx()
        .store
        .read(move |r| r.events_after(after, PAGE))
        .await
        .map_err(|e| e.to_string())?;
    let mut records = Vec::new();
    if memory.get(SUBJECT, "installed").is_none() {
        records.push(record("first_install", &[]));
    }
    let today = jiff::Timestamp::now().strftime("%Y-%m-%d").to_string();
    if memory.get(SUBJECT, "day") != Some(today.as_str()) {
        records.push(record("first_use_today", &[]));
    }
    let counts: Vec<(&str, i64)> = COUNTED
        .iter()
        .map(|k| (*k, events.iter().filter(|e| e.kind == *k).count()))
        .map(|(k, n)| (k, i64::try_from(n).unwrap_or(i64::MAX)))
        .collect();
    records.push(record("hourly_summary", &counts));
    let body = payload(&installation(state_dir), &records);
    let key = if *key_refused {
        None
    } else {
        settings
            .key_file
            .as_deref()
            .and_then(|f| crate::sys::fs::read_private(f, 4096).ok().flatten())
            .map(|k| String::from_utf8_lossy(&k).trim().to_string())
    };
    match post(endpoint, &body, key.as_deref()).await {
        Sent::Accepted(rejected) => {
            memory.transition(SUBJECT, "installed", "1");
            memory.transition(SUBJECT, "day", &today);
            if let Some(last) = events.last() {
                memory.transition(SUBJECT, "cursor", &last.id.to_string());
            }
            if rejected > 0 {
                let total = memory
                    .get(SUBJECT, "rejected")
                    .and_then(|v| v.parse::<i64>().ok())
                    .unwrap_or(0)
                    + rejected;
                memory.transition(SUBJECT, "rejected", &total.to_string());
            }
        }
        Sent::KeyRefused => *key_refused = true,
        Sent::Dropped => {}
    }
    jobs.ctx()
        .store
        .apply(Apply {
            memory: memory.into_writes(),
            ..Apply::default()
        })
        .await
        .map_err(|e| e.to_string())
}

/// SHA-256 of a random installation identifier kept in the state folder.
fn installation(state_dir: &Path) -> String {
    let path = state_dir.join("usage-id");
    let id = match crate::sys::fs::read_private(&path, 128) {
        Ok(Some(bytes)) if !bytes.is_empty() => String::from_utf8_lossy(&bytes).trim().to_string(),
        _ => {
            let id = crate::host::random_hex(32);
            let _ = crate::sys::fs::write_atomic(&path, id.as_bytes());
            id
        }
    };
    crate::sys::release::sha256(id.as_bytes())
}

fn attr(key: &str, value: Value) -> Value {
    let mut pair = serde_json::Map::new();
    pair.insert("key".into(), Value::String(key.into()));
    pair.insert("value".into(), value);
    Value::Object(pair)
}

/// One log record: its name and integer counts, and a random unique ID.
fn record(name: &str, counts: &[(&str, i64)]) -> Value {
    let mut attributes = vec![attr(
        "event.id",
        json!({ "stringValue": crate::host::random_hex(16) }),
    )];
    for (k, n) in counts {
        attributes.push(attr(
            &format!("count.{k}"),
            json!({ "intValue": n.to_string() }),
        ));
    }
    json!({
        "timeUnixNano": (i128::from(now_ms()) * 1_000_000).to_string(),
        "body": { "stringValue": name },
        "attributes": attributes,
    })
}

/// The OTLP/HTTP JSON logs document (req. 71).
fn payload(installation: &str, records: &[Value]) -> Value {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };
    let resource = vec![
        attr("service.name", json!({ "stringValue": "cataly" })),
        attr(
            "service.version",
            json!({ "stringValue": crate::sys::release::VERSION }),
        ),
        attr(
            "cataly.installation",
            json!({ "stringValue": installation }),
        ),
        attr(
            "cataly.ci",
            json!({ "boolValue": std::env::var_os("CI").is_some() }),
        ),
        attr("os.type", json!({ "stringValue": os })),
        attr("os.version", json!({ "stringValue": os_version() })),
        attr(
            "host.arch",
            json!({ "stringValue": std::env::consts::ARCH }),
        ),
    ];
    json!({
        "resourceLogs": [{
            "resource": { "attributes": resource },
            "scopeLogs": [{ "scope": { "name": "cataly.usage" }, "logRecords": records }],
        }]
    })
}

/// The kernel release, digits and dots only.
fn os_version() -> String {
    let text = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    text.trim()
        .split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .next()
        .unwrap_or_default()
        .to_string()
}

enum Sent {
    /// Accepted, with the number of records the collector rejected.
    Accepted(i64),
    /// The collector refused the key: it is dropped.
    KeyRefused,
    /// Anything else: dropped silently.
    Dropped,
}

async fn post(endpoint: &http::Base, body: &Value, key: Option<&str>) -> Sent {
    let mut headers = Vec::new();
    if let Some(k) = key
        && let Ok(v) = hyper::header::HeaderValue::from_str(&format!("Bearer {k}"))
    {
        headers.push((hyper::header::AUTHORIZATION, v));
    }
    let bytes = serde_json::to_vec(body).unwrap_or_default();
    let req = Request {
        method: hyper::Method::POST,
        path: "",
        headers,
        json: Some(bytes),
        kind: Kind::Once,
    };
    let policy = Policy {
        attempt: SEND_TIMEOUT,
        attempts: 1,
        budget: Duration::ZERO,
    };
    let outcome =
        tokio::time::timeout(SEND_TIMEOUT, http::call(endpoint, &req, policy, |_| {})).await;
    match outcome {
        Ok(Outcome::Done(r)) => Sent::Accepted(
            serde_json::from_slice::<Value>(&r.body)
                .ok()
                .and_then(|v| {
                    v.get("partialSuccess")?
                        .get("rejectedLogRecords")
                        .and_then(|n| n.as_i64().or_else(|| n.as_str()?.parse().ok()))
                })
                .unwrap_or(0),
        ),
        Ok(Outcome::Rejected(r)) if r.status == 401 || r.status == 403 => {
            log::warn("usage collector refused the key; it is not sent again", &[]);
            Sent::KeyRefused
        }
        _ => Sent::Dropped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_carries_counts_only() {
        let body = payload("abc", &[record("hourly_summary", &[("lane.started", 2)])]);
        let text = body.to_string();
        assert!(text.contains("\"count.lane.started\""));
        assert!(text.contains("\"intValue\":\"2\""));
        assert!(text.contains("\"service.name\""));
        assert!(!text.contains('/'), "no paths: {text}");
    }
}
