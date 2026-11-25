//! The typed event (WS req. 2, DESIGN 6.1): kinds with their named fields, default wording, owner
//! templates (OJ req. 13), and the one-line text rule.

use crate::store::NewEvent;

/// Longest event text, in characters.
pub(crate) const TEXT_MAX: usize = 400;
/// Bodies up to this size travel inline; larger ones are given by path.
pub(crate) const BODY_INLINE_MAX: u64 = 64 * 1024;

/// How serious an event is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    Info,
    Attention,
    Warning,
    Error,
}

impl Severity {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Attention => "attention",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// Every event kind this hub emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventKind {
    HubStarted,
    HubStalled,
    ConfigInvalid,
    ConfigValid,
    RunDone,
    RunSkipped,
    RunBroken,
    ProbeFired,
    ProbeError,
    ProbeRecovered,
    /// An owner job's tick failed (one per distinct reason) or overran its deadline.
    TickFailed,
    TickRecovered,
    GithubPr,
    GithubNotification,
    PrBaseline,
    PrFeedback,
    PrCheckFailed,
    PrPushed,
    PrMerged,
    PrClosed,
    PrReopened,
    QuotaLevel,
    QuotaRecovered,
    QuotaHold,
    QuotaReleased,
    BackupRefused,
    BackupClean,
    BackupPushFailed,
    HealthProblem,
    HealthCleared,
    ItemNew,
    PressureAlert,
    PressureClear,
    PressureReady,
    PressureBusy,
    LeadStarted,
    LeadNotStarted,
    SessionStopped,
    SessionFenced,
    SpendOffer,
    ModelFallback,
    ModelRestored,
    QuotaBlocked,
    DeliveryInDoubt,
    DeliveryFailed,
    RelayHeld,
    RelayStopped,
    /// A session's model is neither the requested one nor a fallback OMP reported (DESIGN 6.1).
    ModelUnexpected,
    BridgeRefused,
    /// The owner returned after a long silence (OJ-20, OJ-21).
    Digest,
    /// A message from someone else waited past the alert age (OJ-22).
    Unanswered,
    /// A stream job's output line matched (WS-02).
    StreamLine,
    StreamEnded,
    /// Lanes (LANE, EVR, WS oversight; milestone 3a).
    LaneStarted,
    LaneReport,
    LaneClaimed,
    LaneClosed,
    LaneLost,
    LaneAlert,
    LaneRecap,
    /// A queued launch became ready, held or expired (LANE req. 25).
    LaneQueued,
    /// A planner pass summary (COORD req. 34).
    PlannerRun,
    /// An item question for the owner, through the lead (COORD req. 32).
    ItemQuestion,
    /// The daily owner-notes refresh request (COORD req. 49).
    NotesRefresh,
    /// A newer stable release is out (IR req. 37).
    UpdateAvailable,
    /// Usage counts are on (COORD req. 69).
    UsageOn,
}

/// Kind, wire name, template fields and default wording.
type KindRow = (
    EventKind,
    &'static str,
    &'static [&'static str],
    &'static str,
);

const KINDS: [KindRow; 66] = [
    (
        EventKind::HubStarted,
        "hub.started",
        &["version", "jobs"],
        "hub {version} started with {jobs} jobs",
    ),
    (
        EventKind::HubStalled,
        "hub.stalled",
        &["at"],
        "hub restarted after its main loop stalled at {at}",
    ),
    (
        EventKind::ConfigInvalid,
        "config.invalid",
        &["error"],
        "config not applied, the previous one stays in force: {error}",
    ),
    (
        EventKind::ConfigValid,
        "config.valid",
        &[],
        "config valid again and applied",
    ),
    (
        EventKind::RunDone,
        "run.done",
        &[
            "job",
            "outcome",
            "exit",
            "signal",
            "timed_out",
            "bytes",
            "log",
            "scheduled",
        ],
        "{job}: {outcome}, {bytes} bytes of output",
    ),
    (
        EventKind::RunSkipped,
        "run.skipped",
        &["job", "count", "reason"],
        "{job}: {count} occurrence(s) skipped ({reason})",
    ),
    (
        EventKind::RunBroken,
        "run.broken",
        &["job", "run", "started"],
        "{job}: run {run} was broken off; the job is on hold until acknowledged",
    ),
    (
        EventKind::ProbeFired,
        "probe.fired",
        &["job", "bytes", "log", "scheduled"],
        "{job}: condition met, {bytes} bytes of output",
    ),
    (
        EventKind::ProbeError,
        "probe.error",
        &["job", "reason", "run", "log"],
        "{job}: probe error: {reason}",
    ),
    (
        EventKind::ProbeRecovered,
        "probe.recovered",
        &["job", "errors"],
        "{job}: probe works again after {errors} error(s)",
    ),
    (
        EventKind::TickFailed,
        "job.failed",
        &["job", "reason"],
        "{job}: tick failed: {reason}",
    ),
    (
        EventKind::TickRecovered,
        "job.recovered",
        &["job", "failures"],
        "{job}: works again after {failures} failed tick(s)",
    ),
    (
        EventKind::GithubPr,
        "github.pr",
        &["repo", "number", "title", "state", "author", "url"],
        "new PR {repo}#{number} by {author} ({state}): {title}",
    ),
    (
        EventKind::GithubNotification,
        "github.notification",
        &["repo", "reason", "title", "kind"],
        "{repo}: {reason} on {kind} \"{title}\"",
    ),
    (
        EventKind::PrBaseline,
        "pr.baseline",
        &["pr", "title", "state"],
        "following {pr} ({state}): {title}",
    ),
    (
        EventKind::PrFeedback,
        "pr.feedback",
        &["pr", "author", "what", "excerpt", "url"],
        "{pr}: {what} by {author}: {excerpt}",
    ),
    (
        EventKind::PrCheckFailed,
        "pr.check_failed",
        &["pr", "check", "conclusion", "head"],
        "{pr}: check {check} {conclusion} on {head}",
    ),
    (
        EventKind::PrPushed,
        "pr.pushed",
        &["pr", "authors", "count", "old", "new", "rewritten"],
        "{pr}: head moved {old} -> {new} ({count} new commit(s) by {authors}; history rewritten: {rewritten})",
    ),
    (
        EventKind::PrMerged,
        "pr.merged",
        &["pr", "commit"],
        "{pr} merged as {commit}",
    ),
    (
        EventKind::PrClosed,
        "pr.closed",
        &["pr"],
        "{pr} closed without merging",
    ),
    (
        EventKind::PrReopened,
        "pr.reopened",
        &["pr"],
        "{pr} reopened",
    ),
    (
        EventKind::QuotaLevel,
        "quota.level",
        &[
            "limit", "provider", "account", "percent", "window", "reset_in",
        ],
        "quota {provider} account {account} {limit}: {percent}% used of the {window} window, resets in {reset_in}",
    ),
    (
        EventKind::QuotaRecovered,
        "quota.recovered",
        &["limit", "provider", "account", "percent"],
        "quota {provider} account {account} {limit} is back to {percent}%",
    ),
    (
        EventKind::QuotaHold,
        "quota.hold",
        &["limits", "level"],
        "quota gate closed at {level}%: new session starts wait ({limits})",
    ),
    (
        EventKind::QuotaReleased,
        "quota.released",
        &["reason"],
        "quota gate open: {reason}",
    ),
    (
        EventKind::BackupRefused,
        "backup.refused",
        &["files", "rule"],
        "backup refused, nothing committed: {rule} in {files}",
    ),
    (
        EventKind::BackupClean,
        "backup.clean",
        &["commit"],
        "backup committed cleanly again ({commit})",
    ),
    (
        EventKind::BackupPushFailed,
        "backup.push_failed",
        &["reason"],
        "backup push failed, the commit stays local: {reason}",
    ),
    (
        EventKind::HealthProblem,
        "health.problem",
        &["check", "detail"],
        "health: {check}: {detail}",
    ),
    (
        EventKind::HealthCleared,
        "health.cleared",
        &["check"],
        "health: {check} is fine again",
    ),
    (
        EventKind::ItemNew,
        "items.new",
        &["job", "id", "title"],
        "{job}: {title}",
    ),
    (
        EventKind::PressureAlert,
        "pressure.alert",
        &["reasons"],
        "host under pressure: {reasons}",
    ),
    (
        EventKind::PressureClear,
        "pressure.clear",
        &[],
        "host pressure cleared",
    ),
    (
        EventKind::PressureReady,
        "pressure.ready",
        &["load"],
        "host ready for heavy compute (load {load}%)",
    ),
    (
        EventKind::PressureBusy,
        "pressure.busy",
        &["load"],
        "host busy again (load {load}%)",
    ),
    (
        EventKind::LeadStarted,
        "lead.started",
        &["session", "model", "how"],
        "lead started ({how}) on {model}, session {session}",
    ),
    (
        EventKind::LeadNotStarted,
        "lead.refused",
        &["reason"],
        "lead not started: {reason}",
    ),
    (
        EventKind::SessionStopped,
        "session.stopped",
        &["role", "how", "detail"],
        "{role} session stopped by itself ({how} {detail})",
    ),
    (
        EventKind::SessionFenced,
        "session.fenced",
        &["role", "reason"],
        "{role} session fenced, nothing more is delivered: {reason}",
    ),
    (
        EventKind::SpendOffer,
        "session.spend_offer",
        &["role", "dialog", "count"],
        "{role} session: spend offer cancelled ({count} in 10 minutes): {dialog}",
    ),
    (
        EventKind::ModelFallback,
        "session.fallback",
        &["role", "from", "to", "reason"],
        "{role} session fell back from {from} to {to} ({reason})",
    ),
    (
        EventKind::ModelRestored,
        "session.fallback_recovered",
        &["role", "model"],
        "{role} session is back on {model}",
    ),
    (
        EventKind::QuotaBlocked,
        "session.quota_blocked",
        &["role", "message"],
        "{role} session is quota-blocked, no fallback left: {message}",
    ),
    (
        EventKind::DeliveryInDoubt,
        "delivery.in_doubt",
        &["target", "count", "reason"],
        "{count} message(s) for {target} in doubt, not resent: {reason}",
    ),
    (
        EventKind::DeliveryFailed,
        "delivery.failed",
        &["target", "count", "reason"],
        "{count} message(s) for {target} not delivered: {reason}",
    ),
    (
        EventKind::RelayHeld,
        "relay.held",
        &["relay", "room", "reason"],
        "{relay} room {room} is held: {reason}",
    ),
    (
        EventKind::RelayStopped,
        "relay.stopped",
        &["relay", "reason"],
        "{relay} intake stopped: {reason}",
    ),
    (
        EventKind::ModelUnexpected,
        "session.model_unexpected",
        &["role", "model", "requested"],
        "{role} session runs on {model}, which OMP did not report as a fallback of {requested}",
    ),
    (
        EventKind::BridgeRefused,
        "bridge.refused",
        &["reason"],
        "an OMP bridge was refused: {reason}",
    ),
    (
        EventKind::Digest,
        "owner.digest",
        &["since", "rooms", "events"],
        "the owner is back after a while: digest since {since} ({rooms} room(s), {events} event(s))",
    ),
    (
        EventKind::Unanswered,
        "owner.unanswered",
        &["room", "author", "age", "excerpt", "link"],
        "{author} has waited {age} for an answer in {room}: {excerpt} ({link})",
    ),
    (
        EventKind::StreamLine,
        "stream.line",
        &["job", "line"],
        "{job}: {line}",
    ),
    (
        EventKind::StreamEnded,
        "stream.ended",
        &["job", "reason"],
        "{job} stopped: {reason}",
    ),
    (
        EventKind::LaneStarted,
        "lane.started",
        &["lane", "detail"],
        "lane {lane} {detail}",
    ),
    (
        EventKind::LaneReport,
        "lane.report",
        &["lane", "kind", "report", "text"],
        "lane {lane} {kind} report {report}: {text}",
    ),
    (
        EventKind::LaneClaimed,
        "lane.claimed",
        &["lane", "closes", "evidence"],
        "lane {lane} claims completion ({evidence}); it closes at {closes} unless you object",
    ),
    (
        EventKind::LaneClosed,
        "lane.closed",
        &["lane", "outcome", "detail"],
        "lane {lane} closed {outcome}: {detail}",
    ),
    (
        EventKind::LaneLost,
        "lane.lost",
        &["lane", "detail"],
        "lane {lane} lost its process: {detail}; reopen it to continue",
    ),
    (
        EventKind::LaneAlert,
        "lane.alert",
        &["lane", "what", "detail"],
        "lane {lane} {what}: {detail}",
    ),
    (
        EventKind::LaneRecap,
        "lane.recap",
        &["detail"],
        "lanes after the hub start: {detail}",
    ),
    (
        EventKind::LaneQueued,
        "lane.queued",
        &["lane", "detail"],
        "queued launch {lane} {detail}",
    ),
    (
        EventKind::PlannerRun,
        "planner.run",
        &["summary"],
        "{summary}",
    ),
    (
        EventKind::ItemQuestion,
        "item.question",
        &["item", "question"],
        "item {item}: {question}",
    ),
    (
        EventKind::NotesRefresh,
        "notes.refresh",
        &["date", "procedure"],
        "owner-notes refresh for {date}: follow {procedure}",
    ),
    (
        EventKind::UpdateAvailable,
        "update.available",
        &["version", "installed"],
        "Cataly {version} is available (installed: {installed}); install it with `cataly update`",
    ),
    (
        EventKind::UsageOn,
        "usage.on",
        &["endpoint"],
        "usage counts are on and go to {endpoint}; set [usage] enabled = false to turn them off",
    ),
];

impl EventKind {
    fn row(self) -> &'static KindRow {
        // Every variant has exactly one row; the test below checks the table is complete.
        KINDS.iter().find(|r| r.0 == self).unwrap_or(&KINDS[0])
    }

    pub(crate) fn name(self) -> &'static str {
        self.row().1
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        KINDS.iter().find(|r| r.1 == name).map(|r| r.0)
    }

    /// Fields a template for this kind may name.
    pub(crate) fn fields(self) -> &'static [&'static str] {
        self.row().2
    }

    fn default_template(self) -> &'static str {
        self.row().3
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Text(String),
    Field(&'static str),
}

/// Event wording with `{field}` slots; `{{` and `}}` stand for literal braces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Template(Vec<Part>);

impl Template {
    /// Parses `text` for `kind`; the error names an unknown field or a stray brace.
    pub(crate) fn parse(text: &str, kind: EventKind) -> Result<Self, String> {
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    literal.push('{');
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    literal.push('}');
                }
                '{' => {
                    let name: String = chars.by_ref().take_while(|c| *c != '}').collect();
                    let field = kind.fields().iter().find(|f| **f == name).ok_or_else(|| {
                        format!(
                            "unknown field '{{{name}}}' for {} (known: {})",
                            kind.name(),
                            kind.fields().join(", ")
                        )
                    })?;
                    if !literal.is_empty() {
                        parts.push(Part::Text(std::mem::take(&mut literal)));
                    }
                    parts.push(Part::Field(field));
                }
                '}' => return Err("unmatched '}' (write '}}' for a literal brace)".into()),
                c => literal.push(c),
            }
        }
        if !literal.is_empty() {
            parts.push(Part::Text(literal));
        }
        Ok(Self(parts))
    }

    fn render(&self, values: &[(&'static str, String)]) -> String {
        let mut out = String::new();
        for part in &self.0 {
            match part {
                Part::Text(t) => out.push_str(t),
                Part::Field(f) => {
                    out.push_str(
                        values
                            .iter()
                            .find(|(k, _)| k == f)
                            .map_or("", |(_, v)| v.as_str()),
                    );
                }
            }
        }
        out
    }
}

/// Owner wording overrides of one job.
pub(crate) type Templates = Vec<(EventKind, Template)>;

/// What a job or the hub wants to say, before wording and routing.
#[derive(Debug, Clone)]
pub(crate) struct Emit {
    pub(crate) kind: EventKind,
    pub(crate) severity: Severity,
    pub(crate) urgent: bool,
    pub(crate) subject: String,
    pub(crate) values: Vec<(&'static str, String)>,
    pub(crate) body: Option<String>,
    pub(crate) body_path: Option<String>,
}

impl Emit {
    pub(crate) fn new(kind: EventKind, severity: Severity, urgent: bool, subject: &str) -> Self {
        Self {
            kind,
            severity,
            urgent,
            subject: subject.to_string(),
            values: Vec::new(),
            body: None,
            body_path: None,
        }
    }

    /// Adds a template field. Taking the value by value lets callers pass numbers and strings alike.
    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn with(mut self, field: &'static str, value: impl ToString) -> Self {
        self.values.push((field, value.to_string()));
        self
    }
}

/// Words and routes an emit. Dry events go nowhere but the store and the event log.
pub(crate) fn build(
    job: &str,
    emit: Emit,
    templates: &[(EventKind, Template)],
    dry: bool,
    destinations: &[String],
    at_ms: i64,
) -> NewEvent {
    let text = match templates.iter().find(|(k, _)| *k == emit.kind) {
        Some((_, t)) => t.render(&emit.values),
        None => Template::parse(emit.kind.default_template(), emit.kind).map_or_else(
            |_| emit.kind.default_template().to_string(),
            |t| t.render(&emit.values),
        ),
    };
    NewEvent {
        at_ms,
        job: job.to_string(),
        subject: emit.subject,
        kind: emit.kind.name(),
        severity: emit.severity.as_str(),
        text: one_line(&text, TEXT_MAX),
        body: emit.body,
        body_path: emit.body_path,
        urgent: emit.urgent,
        dry,
        destinations: if dry {
            Vec::new()
        } else {
            destinations.to_vec()
        },
    }
}

/// Flattens control characters, line separators and bidirectional overrides to spaces and cuts to
/// `max` characters, ending a cut with an ellipsis.
pub(crate) fn one_line(text: &str, max: usize) -> String {
    let clean = |c: char| {
        if c.is_control()
            || matches!(c, '\u{2028}' | '\u{2029}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            ' '
        } else {
            c
        }
    };
    let count = text.chars().count();
    if count <= max {
        return text.chars().map(clean).collect();
    }
    let mut out: String = text
        .chars()
        .take(max.saturating_sub(1))
        .map(clean)
        .collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse_and_names_round_trip() {
        let mut names: Vec<&str> = KINDS.iter().map(|r| r.1).collect();
        for (kind, name, _, _) in KINDS {
            assert_eq!(kind.row().1, name, "one row per kind");
            Template::parse(kind.default_template(), kind).unwrap();
            assert_eq!(EventKind::from_name(kind.name()), Some(kind));
        }
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), KINDS.len(), "names are unique");
        assert_eq!(EventKind::from_name("run.nope"), None);
    }

    #[test]
    fn templates_render_and_refuse_unknown_fields() {
        let t = Template::parse("{{{job}}} exit={exit}", EventKind::RunDone).unwrap();
        assert_eq!(
            t.render(&[("job", "a".into()), ("exit", "0".into())]),
            "{a} exit=0"
        );
        let err = Template::parse("{nope}", EventKind::RunDone).unwrap_err();
        assert!(err.contains("unknown field '{nope}'"));
        assert!(Template::parse("x}", EventKind::RunDone).is_err());
    }

    #[test]
    fn build_words_cuts_and_routes() {
        let emit = Emit::new(EventKind::RunSkipped, Severity::Info, false, "j")
            .with("job", "j")
            .with("count", 2)
            .with("reason", "late\nby a lot");
        let e = build("j", emit.clone(), &[], false, &["d".into()], 5);
        assert_eq!(e.text, "j: 2 occurrence(s) skipped (late by a lot)");
        assert_eq!(e.destinations, ["d"]);
        let dry = build("j", emit, &[], true, &["d".into()], 5);
        assert_eq!(dry.destinations, Vec::<String>::new());
        assert!(dry.dry);
        let long = one_line(&"x".repeat(500), TEXT_MAX);
        assert_eq!(long.chars().count(), TEXT_MAX);
        assert!(long.ends_with('…'));
        assert_eq!(one_line("a\u{202e}b", 10), "a b");
    }
}
