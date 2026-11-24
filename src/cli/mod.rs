//! `cataly <verb>`: parse, send one request to the hub, print the answer as text or `--json`.

mod args;
mod client;
pub(crate) mod out;

use std::fmt::Write as _;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config;
use crate::jobs::instant_text;
use crate::proto::{self, Events, JobList, Stats, Status, method};
use crate::store::EventRecord;
use crate::sys::paths::Paths;
use args::{Invocation, Parsed};
use client::{Client, ClientError};

/// Exit statuses, distinct per kind of refusal (COORD req. 75).
pub(crate) mod exit {
    pub(crate) const OK: u8 = 0;
    /// The request was refused or failed.
    pub(crate) const FAILED: u8 = 1;
    /// The command line itself was refused.
    pub(crate) const USAGE: u8 = 2;
    /// No hub answers.
    pub(crate) const NO_HUB: u8 = 3;
    /// A hub already runs.
    pub(crate) const ALREADY_RUNNING: u8 = 4;
    /// Internal failure: supervision gave up, the store writer failed, or the main loop stalled.
    pub(crate) const INTERNAL: u8 = 70;
}

/// Bound for ordinary calls.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound for `job run`: the longest job time limit plus the kill grace and margin.
const RUN_TIMEOUT: Duration = Duration::from_mins(24 * 60 + 2);

/// Entry point of the binary.
pub(crate) fn main() -> ExitCode {
    let words: Vec<String> = std::env::args().skip(1).collect();
    let parsed = match args::parse(&words) {
        Ok(Invocation::Run(parsed)) => parsed,
        Ok(Invocation::Help(text)) => {
            out::text(&text);
            return ExitCode::from(exit::OK);
        }
        Err(refusal) => {
            out::error_line(&format!("cataly: {refusal}"));
            return ExitCode::from(exit::USAGE);
        }
    };
    let paths = match Paths::from_env() {
        Ok(p) => p,
        Err(e) => {
            out::error_line(&format!("cataly: {e}"));
            return ExitCode::from(exit::FAILED);
        }
    };
    if parsed.is(&["hub"]) {
        return crate::hub::run(&paths);
    }
    ExitCode::from(match execute(&parsed, &paths) {
        Ok(()) => exit::OK,
        Err(Failure::Usage(m)) => fail(exit::USAGE, &m),
        Err(Failure::Refused(m)) => fail(exit::FAILED, &m),
        Err(Failure::Client(ClientError::NoHub)) => fail(
            exit::NO_HUB,
            &format!(
                "the hub is not running (no answer on {})",
                paths.socket.display()
            ),
        ),
        Err(Failure::Client(e)) => fail(exit::FAILED, &e.to_string()),
    })
}

fn fail(code: u8, message: &str) -> u8 {
    out::error_line(&format!("cataly: {message}"));
    code
}

enum Failure {
    Usage(String),
    Refused(String),
    Client(ClientError),
}

impl From<ClientError> for Failure {
    fn from(e: ClientError) -> Self {
        Self::Client(e)
    }
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, Failure> {
    serde_json::from_value(value).map_err(|e| Failure::Refused(format!("unexpected reply: {e}")))
}

fn execute(p: &Parsed, paths: &Paths) -> Result<(), Failure> {
    let as_json = p.flag("--json");
    if p.is(&["config", "check"]) {
        let file = p
            .value("--file")
            .map_or(paths.config_file.as_path(), Path::new);
        return config_check(file, as_json);
    }
    let mut hub = Client::connect(&paths.socket)?;
    match p.spec.path {
        ["status"] => {
            let reply = hub.call(method::STATUS, json!({}), CALL_TIMEOUT)?;
            if as_json {
                out::json(&reply);
            } else {
                print_status(&decode(reply)?);
            }
        }
        ["stats"] => {
            let reply = hub.call(method::STATS, json!({}), CALL_TIMEOUT)?;
            if as_json {
                out::json(&reply);
            } else {
                print_stats(&decode(reply)?);
            }
        }
        ["events"] => events(p, &mut hub, as_json)?,
        ["job", "list"] => {
            let reply = hub.call(method::JOB_LIST, json!({}), CALL_TIMEOUT)?;
            if as_json {
                out::json(&reply);
            } else {
                print_jobs(&decode(reply)?);
            }
        }
        ["job", "run"] => {
            let reply = hub.call(
                method::JOB_RUN,
                json!({ "name": p.args.first() }),
                RUN_TIMEOUT,
            )?;
            if as_json {
                out::json(&reply);
            } else {
                let events: Events = decode(reply)?;
                if events.events.is_empty() {
                    out::line("dry run done; no events (nothing changed)");
                }
                for e in &events.events {
                    print_event(e);
                }
            }
        }
        ["job", verb @ ("enable" | "disable")] => {
            let name = p.args.first().cloned().unwrap_or_default();
            let (m, params) = if *verb == "enable" {
                (
                    method::JOB_ENABLE,
                    json!({ "name": name, "ack": p.flag("--ack") }),
                )
            } else {
                (method::JOB_DISABLE, json!({ "name": name }))
            };
            let reply = hub.call(m, params, CALL_TIMEOUT)?;
            if as_json {
                out::json(&reply);
            } else {
                out::line(&format!(
                    "job {name} {}",
                    if *verb == "enable" {
                        "resumed"
                    } else {
                        "on hold"
                    }
                ));
            }
        }
        ["config", "reload"] => {
            let reply = hub.call(method::CONFIG_RELOAD, json!({}), CALL_TIMEOUT)?;
            if as_json {
                out::json(&reply);
            } else {
                let r: proto::Reloaded = decode(reply)?;
                out::line(&format!("config applied: {} jobs", r.jobs));
            }
        }
        other => {
            return Err(Failure::Usage(format!(
                "'{}' is not handled here",
                other.join(" ")
            )));
        }
    }
    Ok(())
}

fn events(p: &Parsed, hub: &mut Client, as_json: bool) -> Result<(), Failure> {
    let mut params = proto::EventsParams::default();
    if let Some(after) = p.value("--after") {
        params.after = Some(
            after
                .parse()
                .map_err(|_| Failure::Usage(format!("--after: '{after}' is not an event id")))?,
        );
    }
    if let Some(since) = p.value("--since") {
        params.since_ms = Some(since_ms(since).ok_or_else(|| {
            Failure::Usage(format!("--since: '{since}' is neither a duration like 10m nor an instant like 2026-11-25T08:00:00Z"))
        })?);
    }
    if let Some(limit) = p.value("--limit") {
        params.limit = Some(
            limit
                .parse()
                .map_err(|_| Failure::Usage(format!("--limit: '{limit}' is not a number")))?,
        );
    }
    let print = |value: Value| {
        if as_json {
            out::json(&value);
        } else if let Ok(e) = serde_json::from_value::<EventRecord>(value) {
            print_event(&e);
        }
    };
    if p.flag("--follow") {
        if params.after.is_none() && params.since_ms.is_some() {
            // Show what happened since then, then keep following from the newest shown.
            let reply = hub.call(
                method::EVENTS_SINCE,
                serde_json::to_value(&params).unwrap_or_default(),
                CALL_TIMEOUT,
            )?;
            let past: Events = decode(reply)?;
            params.after = Some(past.events.last().map_or(0, |e| e.id));
            past.events
                .into_iter()
                .filter_map(|e| serde_json::to_value(e).ok())
                .for_each(print);
        }
        let follow = proto::EventsParams {
            after: params.after,
            since_ms: None,
            limit: None,
        };
        hub.follow(
            method::EVENTS_FOLLOW,
            serde_json::to_value(&follow).unwrap_or_default(),
            print,
        )?;
        return Ok(());
    }
    let reply = hub.call(
        method::EVENTS_SINCE,
        serde_json::to_value(&params).unwrap_or_default(),
        CALL_TIMEOUT,
    )?;
    let events: Events = decode(reply)?;
    events
        .events
        .into_iter()
        .filter_map(|e| serde_json::to_value(e).ok())
        .for_each(print);
    Ok(())
}

/// `10m`-style durations count back from now; anything else must be an instant with an offset.
fn since_ms(text: &str) -> Option<i64> {
    if let Some(d) = config::parse_duration(text) {
        return Some(crate::sys::now_ms().saturating_sub(crate::jobs::duration_ms(d)));
    }
    text.parse::<jiff::Timestamp>()
        .ok()
        .map(jiff::Timestamp::as_millisecond)
}

fn config_check(path: &Path, as_json: bool) -> Result<(), Failure> {
    match config::load(path) {
        Ok(c) => {
            let soon: Vec<String> = c
                .jobs
                .values()
                .filter_map(|j| match &j.trigger {
                    config::Trigger::Once { at } => {
                        let left = at.as_millisecond() - crate::sys::now_ms();
                        (left > 0 && left < 120_000)
                            .then(|| format!("jobs.{}.at is less than two minutes away", j.name))
                    }
                    _ => None,
                })
                .collect();
            if as_json {
                out::json(&json!({
                    "valid": true, "file": path.display().to_string(), "jobs": c.jobs.len(),
                    "destinations": c.destinations.len(), "warnings": soon,
                }));
            } else {
                out::line(&format!(
                    "config valid: {} ({} jobs, {} destinations)",
                    path.display(),
                    c.jobs.len(),
                    c.destinations.len()
                ));
                for w in &soon {
                    out::line(&format!("warning: {w}"));
                }
            }
            Ok(())
        }
        Err(errors) => {
            if as_json {
                let list: Vec<Value> = errors
                    .iter()
                    .map(|e| json!({"path": e.path, "message": e.message}))
                    .collect();
                out::json(
                    &json!({ "valid": false, "file": path.display().to_string(), "errors": list }),
                );
            } else {
                for e in &errors {
                    out::error_line(&format!("{e}"));
                }
            }
            Err(Failure::Refused(format!(
                "config invalid: {} error(s) in {}",
                errors.len(),
                path.display()
            )))
        }
    }
}

fn duration_text(seconds: i64) -> String {
    let (d, h, m) = (
        seconds / 86_400,
        seconds % 86_400 / 3600,
        seconds % 3600 / 60,
    );
    match (d, h, m) {
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h{m}m"),
        (d, h, _) => format!("{d}d{h}h"),
    }
}

fn print_status(s: &Status) {
    out::line(&format!(
        "hub {} running, pid {}, up {}",
        s.version,
        s.pid,
        duration_text(s.uptime_s)
    ));
    out::line(&format!("socket   {}", s.socket));
    out::line(&format!("state    {}", s.state_dir));
    match &s.config_error {
        None => out::line(&format!("config   {}", s.config_file)),
        Some(e) => out::line(&format!(
            "config   {} NOT APPLIED (previous config in force): {e}",
            s.config_file
        )),
    }
    out::line(&format!(
        "jobs     {} configured, {} running",
        s.jobs, s.jobs_running
    ));
    out::line(&format!("events   last id {}", s.last_event_id));
    if s.task_restarts > 0 || s.panics > 0 {
        out::line(&format!(
            "tasks    {} restarts, {} panics",
            s.task_restarts, s.panics
        ));
    }
}

fn mib(bytes: u64) -> String {
    format!(
        "{}.{} MiB",
        bytes / 1_048_576,
        bytes % 1_048_576 * 10 / 1_048_576
    )
}

fn print_stats(s: &Stats) {
    out::line(&format!(
        "hub memory      {}",
        s.rss_bytes.map_or_else(|| "unavailable".into(), mib)
    ));
    if let Some(m) = &s.host_memory {
        let mut line = format!(
            "host memory     {} available of {}",
            mib(m.available),
            mib(m.total)
        );
        if let Some(limit) = m.cgroup_limit {
            let _ = write!(line, ", cgroup limit {}", mib(limit));
        }
        out::line(&line);
    }
    if let Some(l) = &s.host_load {
        out::line(&format!(
            "host load       {:.2} {:.2} {:.2} on {} cores",
            l.one, l.five, l.fifteen, l.cores
        ));
    }
    if let Some(d) = &s.state_disk {
        out::line(&format!(
            "state disk      {} free of {}",
            mib(d.available),
            mib(d.total)
        ));
    }
    for (name, lat) in [
        ("control request", &s.control_latency),
        ("store commit", &s.store_commit_latency),
    ] {
        match lat {
            Some(l) => out::line(&format!(
                "{name:<15} p50 {} us, p99 {} us, max {} us over {} samples",
                l.p50_us, l.p99_us, l.max_us, l.count
            )),
            None => out::line(&format!("{name:<15} no samples yet")),
        }
    }
}

fn print_jobs(list: &JobList) {
    if list.jobs.is_empty() {
        out::line("no jobs configured");
        return;
    }
    for j in &list.jobs {
        let due = j.next_due_ms.map_or_else(|| "-".into(), instant_text);
        let outcome = j
            .last_run
            .as_ref()
            .and_then(|r| r.outcome.clone())
            .unwrap_or_else(|| "-".into());
        let mut line = format!(
            "{:<20} {:<6} {:<20} next {due}  last {outcome}",
            j.name, j.kind, j.state
        );
        if j.dry_run {
            line.push_str("  [dry run]");
        }
        if j.needs_ack {
            line.push_str("  [needs --ack]");
        }
        if j.skipped > 0 {
            let _ = write!(line, "  skipped {}", j.skipped);
        }
        if j.error_streak > 0 {
            let _ = write!(line, "  errors {}", j.error_streak);
        }
        out::line(&line);
    }
}

fn print_event(e: &EventRecord) {
    let mut marks = String::new();
    if e.urgent {
        marks.push_str(" urgent");
    }
    if e.dry {
        marks.push_str(" dry");
    }
    out::line(&format!(
        "#{} {} {:<9}{marks} {}",
        e.id,
        instant_text(e.at_ms),
        e.severity,
        e.text
    ));
    if let Some(path) = &e.body_path {
        out::line(&format!("    body: {path}"));
    }
}
