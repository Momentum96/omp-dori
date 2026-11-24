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
}

const ALL: [EventKind; 10] = [
    EventKind::HubStarted,
    EventKind::HubStalled,
    EventKind::ConfigInvalid,
    EventKind::ConfigValid,
    EventKind::RunDone,
    EventKind::RunSkipped,
    EventKind::RunBroken,
    EventKind::ProbeFired,
    EventKind::ProbeError,
    EventKind::ProbeRecovered,
];

impl EventKind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::HubStarted => "hub.started",
            Self::HubStalled => "hub.stalled",
            Self::ConfigInvalid => "config.invalid",
            Self::ConfigValid => "config.valid",
            Self::RunDone => "run.done",
            Self::RunSkipped => "run.skipped",
            Self::RunBroken => "run.broken",
            Self::ProbeFired => "probe.fired",
            Self::ProbeError => "probe.error",
            Self::ProbeRecovered => "probe.recovered",
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        ALL.into_iter().find(|k| k.name() == name)
    }

    /// Fields a template for this kind may name.
    pub(crate) fn fields(self) -> &'static [&'static str] {
        match self {
            Self::HubStarted => &["version", "jobs"],
            Self::HubStalled => &["at"],
            Self::ConfigInvalid => &["error"],
            Self::ConfigValid => &[],
            Self::RunDone => &[
                "job",
                "outcome",
                "exit",
                "signal",
                "timed_out",
                "bytes",
                "log",
                "scheduled",
            ],
            Self::RunSkipped => &["job", "count", "reason"],
            Self::RunBroken => &["job", "run", "started"],
            Self::ProbeFired => &["job", "bytes", "log", "scheduled"],
            Self::ProbeError => &["job", "reason", "run", "log"],
            Self::ProbeRecovered => &["job", "errors"],
        }
    }

    fn default_template(self) -> &'static str {
        match self {
            Self::HubStarted => "hub {version} started with {jobs} jobs",
            Self::HubStalled => "hub restarted after its main loop stalled at {at}",
            Self::ConfigInvalid => "config not applied, the previous one stays in force: {error}",
            Self::ConfigValid => "config valid again and applied",
            Self::RunDone => "{job}: {outcome}, {bytes} bytes of output",
            Self::RunSkipped => "{job}: {count} occurrence(s) skipped ({reason})",
            Self::RunBroken => {
                "{job}: run {run} was broken off; the job is on hold until acknowledged"
            }
            Self::ProbeFired => "{job}: condition met, {bytes} bytes of output",
            Self::ProbeError => "{job}: probe error: {reason}",
            Self::ProbeRecovered => "{job}: probe works again after {errors} error(s)",
        }
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
        for kind in ALL {
            Template::parse(kind.default_template(), kind).unwrap();
            assert_eq!(EventKind::from_name(kind.name()), Some(kind));
        }
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
