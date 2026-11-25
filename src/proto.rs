//! Control-socket messages (DESIGN 8): newline-delimited JSON, at most 1 MiB per line. Request
//! `{v, id, method, params}`; reply `{v, id, ok}` or `{v, id, err: {code, message}}`; streams send
//! `{v, id, item}` lines. `v` is the protocol major; fields are only added within a major and unknown
//! fields are ignored.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::jobs::owner::health::Check;
use crate::jobs::owner::quota::GateView;
use crate::omp::probe::Report;
use crate::store::EventRecord;
use crate::sys::host::{Disk, Load, Memory};
use crate::sys::latency::Summary;

/// Control protocol major.
pub(crate) const VERSION: u32 = 1;
/// Longest line either side sends.
pub(crate) const MAX_LINE: usize = 1024 * 1024;

/// Method names.
pub(crate) mod method {
    pub(crate) const STATUS: &str = "status";
    /// Every lane operation; `params.op` names it (LANE, EVR, COORD of milestone 3a).
    pub(crate) const LANE: &str = "lane";
    pub(crate) const STATS: &str = "stats";
    pub(crate) const EVENTS_SINCE: &str = "events.since";
    pub(crate) const EVENTS_FOLLOW: &str = "events.follow";
    pub(crate) const JOB_LIST: &str = "job.list";
    pub(crate) const JOB_RUN: &str = "job.run";
    pub(crate) const JOB_ENABLE: &str = "job.enable";
    pub(crate) const JOB_DISABLE: &str = "job.disable";
    pub(crate) const CONFIG_RELOAD: &str = "config.reload";
    pub(crate) const HEALTH: &str = "health";
    pub(crate) const LIMITS: &str = "limits";
    pub(crate) const QUOTA: &str = "quota";
    pub(crate) const QUOTA_LIFT: &str = "quota.lift";
    pub(crate) const PR_FOLLOW: &str = "pr.follow";
    pub(crate) const PR_UNFOLLOW: &str = "pr.unfollow";
    pub(crate) const PR_LIST: &str = "pr.list";
    pub(crate) const LEAD_START: &str = "lead.start";
    pub(crate) const LEAD_STOP: &str = "lead.stop";
    pub(crate) const LEAD_STATUS: &str = "lead.status";
    pub(crate) const LEAD_SEND: &str = "lead.send";
    pub(crate) const LEAD_RECEIPT: &str = "lead.receipt";
    pub(crate) const LEAD_INBOX: &str = "lead.inbox";
    pub(crate) const LEAD_CONFIRM: &str = "lead.confirm";
    pub(crate) const LEAD_RELEASE: &str = "lead.release";
    pub(crate) const LEAD_INTERRUPT: &str = "lead.interrupt";
    pub(crate) const LEAD_MODEL: &str = "lead.model";
    pub(crate) const LEAD_ACCEPT_MODEL: &str = "lead.accept_model";
    pub(crate) const LEAD_ANSWER: &str = "lead.answer";
    pub(crate) const LEAD_EVENTS: &str = "lead.events";
    pub(crate) const DISCORD_STATUS: &str = "discord.status";
    pub(crate) const DISCORD_RELEASE: &str = "discord.release";
    pub(crate) const DISCORD_RESET: &str = "discord.reset";
    pub(crate) const TELEGRAM_STATUS: &str = "telegram.status";
    pub(crate) const TELEGRAM_RELEASE: &str = "telegram.release";
    pub(crate) const TELEGRAM_RESET: &str = "telegram.reset";
    pub(crate) const SLACK_STATUS: &str = "slack.status";
    pub(crate) const SLACK_RELEASE: &str = "slack.release";
    pub(crate) const SLACK_RESET: &str = "slack.reset";
    pub(crate) const SLACK_UPLOAD: &str = "slack.upload";
    /// The orderly stop the restart option of `cataly update` asks for.
    pub(crate) const HUB_STOP: &str = "hub.stop";
    pub(crate) const VOICE_STATUS: &str = "voice.status";
    pub(crate) const VOICE_RESTART: &str = "voice.restart";
    pub(crate) const VOICE_REPLY: &str = "voice.reply";
    /// A held slot request (a stream for the life of the connection).
    pub(crate) const SLOT: &str = "slot";
    /// The caller's authority as the hub assigned it (DESIGN 8): `owner`, `lead`, `lane` or
    /// `unplaced`. Commands that act locally with the owner's authority ask it first.
    pub(crate) const WHOAMI: &str = "whoami";
}

/// Error codes besides the decision classes.
pub(crate) mod code {
    pub(crate) const VERSION_MISMATCH: &str = "version_mismatch";
    pub(crate) const UNKNOWN_METHOD: &str = "unknown_method";
    pub(crate) const BAD_REQUEST: &str = "bad_request";
    pub(crate) const NOT_FOUND: &str = "not_found";
    pub(crate) const BUSY: &str = "busy";
    pub(crate) const REFUSED: &str = "refused";
    pub(crate) const INTERNAL: &str = "internal";
}

/// A request as sent on the wire.
#[derive(Debug, Serialize)]
pub(crate) struct Request<'a> {
    pub(crate) v: u32,
    pub(crate) id: u64,
    pub(crate) method: &'a str,
    pub(crate) params: Value,
    /// The launch token the caller's environment carries (`CATALY_LAUNCH`): it runs inside a session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) launch: Option<&'a str>,
}

/// A request as the hub reads it, after [`decode`] checked its `v`.
#[derive(Debug, Deserialize)]
pub(crate) struct Incoming {
    pub(crate) id: u64,
    pub(crate) method: String,
    #[serde(default)]
    pub(crate) params: Value,
    /// Present when the caller carries a session's launch token.
    #[serde(default)]
    pub(crate) launch: Option<String>,
}

/// An error body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ErrorBody {
    pub(crate) code: String,
    pub(crate) message: String,
}

/// Any line the hub sends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Response {
    pub(crate) v: u32,
    pub(crate) id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ok: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) err: Option<ErrorBody>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) item: Option<Value>,
}

impl Response {
    pub(crate) fn ok(id: u64, value: Value) -> Self {
        Self {
            v: VERSION,
            id,
            ok: Some(value),
            err: None,
            item: None,
        }
    }

    pub(crate) fn err(id: u64, code: &str, message: impl Into<String>) -> Self {
        Self {
            v: VERSION,
            id,
            ok: None,
            err: Some(ErrorBody {
                code: code.into(),
                message: message.into(),
            }),
            item: None,
        }
    }

    pub(crate) fn item(id: u64, value: Value) -> Self {
        Self {
            v: VERSION,
            id,
            ok: None,
            err: None,
            item: Some(value),
        }
    }

    /// One JSON line with its newline.
    pub(crate) fn to_line(&self) -> String {
        let mut line = serde_json::to_string(self).unwrap_or_else(|e| {
            format!(
                r#"{{"v":{VERSION},"id":{},"err":{{"code":"internal","message":"{e}"}}}}"#,
                self.id
            )
        });
        line.push('\n');
        line
    }
}

/// Decodes a request line; on failure returns the error reply to send.
pub(crate) fn decode(line: &str) -> Result<Incoming, Box<Response>> {
    let refuse = |id, code, message: String| Box::new(Response::err(id, code, message));
    let value: Value = serde_json::from_str(line)
        .map_err(|e| refuse(0, code::BAD_REQUEST, format!("not JSON: {e}")))?;
    let id = value.get("id").and_then(Value::as_u64).unwrap_or(0);
    let v = value.get("v").and_then(Value::as_u64);
    if v != Some(u64::from(VERSION)) {
        let found = v.map_or_else(|| "none".to_string(), |v| v.to_string());
        return Err(refuse(
            id,
            code::VERSION_MISMATCH,
            format!(
                "this hub speaks control protocol v{VERSION}, the request is v{found}; restart the hub so it \
                 matches the installed cataly"
            ),
        ));
    }
    serde_json::from_value(value).map_err(|e| refuse(id, code::BAD_REQUEST, e.to_string()))
}

/// `status` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Status {
    pub(crate) version: String,
    pub(crate) protocol: u32,
    pub(crate) pid: u32,
    pub(crate) started_ms: i64,
    pub(crate) uptime_s: i64,
    pub(crate) socket: String,
    pub(crate) state_dir: String,
    pub(crate) config_file: String,
    /// Set while the file on disk is invalid and the previous config stays in force.
    #[serde(default)]
    pub(crate) config_error: Option<String>,
    pub(crate) jobs: usize,
    pub(crate) jobs_running: usize,
    pub(crate) last_event_id: i64,
    /// Restarts of supervised tasks since start.
    #[serde(default)]
    pub(crate) task_restarts: u64,
    /// Panics caught in supervised tasks since start.
    #[serde(default)]
    pub(crate) panics: u64,
    /// SHA-256 of the config file as last applied.
    #[serde(default)]
    pub(crate) config_digest: Option<String>,
    /// The last OMP probe (HOST-35, PATCH-13).
    #[serde(default)]
    pub(crate) omp: Option<Report>,
    /// Why no session may start with this OMP, if none may (PATCH-09).
    #[serde(default)]
    pub(crate) sessions_refused: Option<String>,
    pub(crate) quota_gate: GateView,
    /// The lead's model and phase; `None` when the config has no `[lead]`.
    #[serde(default)]
    pub(crate) lead: Option<String>,
    /// The sessions this hub brought back at start (DESIGN decision 9); `None` while recovering.
    #[serde(default)]
    pub(crate) resumed: Option<Resumed>,
}

/// The sessions a hub brought back at start. `lead` and `lanes` keep the shape a 0.1.1 `cataly update`
/// reads from the new hub's status; `lead` is true only when the lead resumed its earlier session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Resumed {
    pub(crate) lead: bool,
    pub(crate) lanes: usize,
    /// How the lead came back; `None` when the config has no `[lead]`.
    #[serde(default)]
    pub(crate) lead_start: Option<LeadStart>,
    /// The lanes running again, by name.
    #[serde(default)]
    pub(crate) lane_names: Vec<String>,
}

/// How the lead came back when the hub started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LeadStart {
    /// It resumed its earlier session.
    Resumed,
    /// Fresh: there was no earlier lead session.
    First,
    /// Fresh: the owner asked for a fresh lead.
    Asked,
    /// Fresh: the earlier session's transcript is gone.
    Gone,
    /// Fresh: the earlier transcript did not pass the resume check.
    Unverified,
    /// Fresh: the earlier session ended in a state that is not resumed.
    Ended,
    /// It is not running.
    NotRunning,
}

/// `stats` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Stats {
    pub(crate) rss_bytes: Option<u64>,
    pub(crate) host_memory: Option<HostMemory>,
    pub(crate) host_load: Option<HostLoad>,
    pub(crate) state_disk: Option<HostDisk>,
    pub(crate) control_latency: Option<LatencySummary>,
    pub(crate) store_commit_latency: Option<LatencySummary>,
    /// Internal `deliver` call to the prompt line written to OMP (wake, hub part).
    #[serde(default)]
    pub(crate) wake_latency: Option<LatencySummary>,
    /// Prompt line written to its entry found in the session file.
    #[serde(default)]
    pub(crate) confirm_latency: Option<LatencySummary>,
    /// Prompt line written to OMP's `agent_start`.
    #[serde(default)]
    pub(crate) agent_start_latency: Option<LatencySummary>,
    /// Discord Gateway frame read to OMP's `agent_start` (wake, end to end).
    #[serde(default)]
    pub(crate) discord_wake_latency: Option<LatencySummary>,
    /// Discord Gateway connection established to its catch-up read finished.
    #[serde(default)]
    pub(crate) discord_catch_up_latency: Option<LatencySummary>,
    /// Telegram poll response read to OMP's `agent_start`.
    #[serde(default)]
    pub(crate) telegram_wake_latency: Option<LatencySummary>,
    /// Telegram receive lag: receive time minus the message's date (TELE-07).
    #[serde(default)]
    pub(crate) telegram_receive_lag: Option<LatencySummary>,
    /// Slack Socket Mode frame read to OMP's `agent_start`.
    #[serde(default)]
    pub(crate) slack_wake_latency: Option<LatencySummary>,
    /// Slack socket established to its catch-up read finished.
    #[serde(default)]
    pub(crate) slack_catch_up_latency: Option<LatencySummary>,
    /// Lane spawn to its kickoff confirmed in the session file.
    #[serde(default)]
    pub(crate) lane_start_latency: Option<LatencySummary>,
}

/// Parameters of the `<relay>.*` methods.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct DiscordParams {
    pub(crate) room: Option<String>,
    /// `slack.upload`: the conversation, an optional thread, and the file.
    pub(crate) channel: Option<String>,
    pub(crate) thread: Option<String>,
    pub(crate) file: Option<String>,
}

/// Parameters of the `lead.*` methods; each method reads the fields it needs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct LeadParams {
    pub(crate) fresh: bool,
    pub(crate) force: bool,
    pub(crate) text: Option<String>,
    /// The receipt key of `lead.send` (HOST req. 56).
    pub(crate) request_id: Option<String>,
    /// A quiet note: it waits for the next wake.
    pub(crate) quiet: bool,
    pub(crate) index: Option<usize>,
    pub(crate) model: Option<String>,
    pub(crate) dialog: Option<String>,
    pub(crate) answer: Option<Value>,
    pub(crate) after: Option<u64>,
}

/// Wire copies of the host figures (kept apart so the readers stay internal types).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HostMemory {
    pub(crate) total: u64,
    pub(crate) available: u64,
    pub(crate) cgroup_limit: Option<u64>,
    pub(crate) cgroup_used: Option<u64>,
    pub(crate) level_percent: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HostLoad {
    pub(crate) one: f64,
    pub(crate) five: f64,
    pub(crate) fifteen: f64,
    pub(crate) cores: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HostDisk {
    pub(crate) total: u64,
    pub(crate) available: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LatencySummary {
    pub(crate) count: usize,
    pub(crate) p50_us: u64,
    #[serde(default)]
    pub(crate) p90_us: u64,
    #[serde(default)]
    pub(crate) p95_us: u64,
    pub(crate) p99_us: u64,
    pub(crate) max_us: u64,
}

impl From<Memory> for HostMemory {
    fn from(m: Memory) -> Self {
        Self {
            total: m.total,
            available: m.available,
            cgroup_limit: m.cgroup_limit,
            cgroup_used: m.cgroup_used,
            level_percent: m.level_percent,
        }
    }
}

impl From<Load> for HostLoad {
    fn from(l: Load) -> Self {
        Self {
            one: l.one,
            five: l.five,
            fifteen: l.fifteen,
            cores: l.cores,
        }
    }
}

impl From<Disk> for HostDisk {
    fn from(d: Disk) -> Self {
        Self {
            total: d.total,
            available: d.available,
        }
    }
}

impl From<Summary> for LatencySummary {
    fn from(s: Summary) -> Self {
        Self {
            count: s.count,
            p50_us: s.p50_us,
            p90_us: s.p90_us,
            p95_us: s.p95_us,
            p99_us: s.p99_us,
            max_us: s.max_us,
        }
    }
}

/// `events.since` and `events.follow` parameters. With neither `after` nor `since_ms`, `since`
/// returns the newest `limit` events and `follow` starts from now.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct EventsParams {
    #[serde(default)]
    pub(crate) after: Option<i64>,
    #[serde(default)]
    pub(crate) since_ms: Option<i64>,
    #[serde(default)]
    pub(crate) limit: Option<u32>,
}

/// `events.since` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Events {
    pub(crate) events: Vec<EventRecord>,
}

/// Parameters naming one job; `request_id` deduplicates a repeated mutating call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct JobParams {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) ack: bool,
    #[serde(default)]
    pub(crate) request_id: Option<String>,
}

/// Parameters of a mutating call that names nothing else.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Mutation {
    #[serde(default)]
    pub(crate) request_id: Option<String>,
}

/// One job in `job.list`. The flags mirror independent stored facts.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JobView {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) trigger: String,
    pub(crate) enabled: bool,
    pub(crate) dry_run: bool,
    pub(crate) state: String,
    pub(crate) held: bool,
    pub(crate) consumed: bool,
    pub(crate) needs_ack: bool,
    pub(crate) next_due_ms: Option<i64>,
    pub(crate) skipped: i64,
    pub(crate) error_streak: i64,
    /// Owner kinds: the last good tick and the current failure reason.
    #[serde(default)]
    pub(crate) last_good_ms: Option<i64>,
    #[serde(default)]
    pub(crate) last_error: Option<String>,
    pub(crate) destinations: Vec<String>,
    pub(crate) last_run: Option<RunView>,
}

/// A job's latest run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunView {
    pub(crate) id: i64,
    pub(crate) dry: bool,
    pub(crate) started_ms: i64,
    pub(crate) ended_ms: Option<i64>,
    pub(crate) outcome: Option<String>,
    pub(crate) exit_code: Option<i64>,
    pub(crate) stdout_path: Option<String>,
    pub(crate) stderr_path: Option<String>,
    /// The time limit ends only the direct child; anything it detached keeps running.
    pub(crate) note: String,
}

/// `job.list` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JobList {
    pub(crate) jobs: Vec<JobView>,
}

/// `config.reload` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Reloaded {
    pub(crate) jobs: usize,
}

/// `health` reply: every check, which ones the config ignores, and whether a WARN remains.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Health {
    pub(crate) checks: Vec<Check>,
    pub(crate) ignored: Vec<String>,
    pub(crate) warn: bool,
}

/// `pr.*` parameters.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct TargetParams {
    /// A PR reference: `owner/repo#N` or its URL.
    #[serde(default)]
    pub(crate) target: Option<String>,
    /// The `pr` job to use when several exist.
    #[serde(default)]
    pub(crate) job: Option<String>,
    #[serde(default)]
    pub(crate) request_id: Option<String>,
}

/// One followed PR.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PrView {
    pub(crate) pr: String,
    pub(crate) job: String,
    pub(crate) since_ms: i64,
    pub(crate) state: String,
    #[serde(default)]
    pub(crate) error: Option<String>,
}

/// `pr.list` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PrList {
    pub(crate) prs: Vec<PrView>,
}

/// `limits` parameters.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct LimitsParams {
    /// Bypass the cache.
    #[serde(default)]
    pub(crate) refresh: bool,
}

/// `limits` reply: the account-limits view (WS req. 37-38).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Limits {
    pub(crate) accounts: Vec<LimitAccount>,
    /// The answer is older than the cache period, dated in the future, or the fetch failed.
    pub(crate) stale: bool,
    pub(crate) fetched_ms: Option<i64>,
    pub(crate) error: Option<String>,
    pub(crate) warn_percent: Option<u8>,
    /// Accounts OMP lists without usage data, and disabled credentials.
    pub(crate) without_usage: usize,
    pub(crate) disabled: usize,
}

/// One account in the limits view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LimitAccount {
    pub(crate) provider: String,
    pub(crate) account: usize,
    pub(crate) windows: Vec<LimitWindow>,
    /// For example "no five-hour window reported".
    pub(crate) notes: Vec<String>,
}

/// One window of one account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LimitWindow {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) used_percent: Option<f64>,
    pub(crate) reset_ms: Option<i64>,
    /// Headroom to the configured warning percentage, when set.
    pub(crate) headroom_percent: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_and_checks_version() {
        let ok = decode(r#"{"v":1,"id":7,"method":"status","extra":true}"#).unwrap();
        assert_eq!((ok.id, ok.method.as_str()), (7, "status"));
        let wrong = decode(r#"{"v":2,"id":9,"method":"status"}"#).unwrap_err();
        let err = wrong.err.unwrap();
        assert_eq!((wrong.id, err.code.as_str()), (9, code::VERSION_MISMATCH));
        assert!(err.message.contains("restart the hub"));
        assert_eq!(
            decode("nope").unwrap_err().err.unwrap().code,
            code::BAD_REQUEST
        );
        assert_eq!(decode(r#"{"v":1,"id":3}"#).unwrap_err().id, 3);
    }

    #[test]
    fn responses_are_single_lines() {
        let line = Response::ok(1, serde_json::json!({"a": "x\ny"})).to_line();
        assert_eq!(line.matches('\n').count(), 1);
        let back: Response = serde_json::from_str(&line).unwrap();
        assert_eq!(back.ok.unwrap()["a"], "x\ny");
        let item: Response =
            serde_json::from_str(&Response::item(2, Value::Null).to_line()).unwrap();
        assert!(item.ok.is_none() && item.err.is_none());
    }
}
