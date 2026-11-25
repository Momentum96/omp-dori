//! `[coord]` (milestone 3b): named slots (LANE req. 21, COORD req. 42-46), the planner and queued
//! launches (COORD req. 26-41, LANE req. 23), the reviewer (COORD req. 53-56), event commands (COORD
//! req. 57-58), publish roots (EVR req. 25) and board destinations (EVR req. 51).

use std::path::PathBuf;
use std::time::Duration;

use super::{ConfigError, Section};

/// Placeholders an event-command element may hold (COORD req. 57).
pub(crate) const PLACEHOLDERS: [&str; 7] =
    ["key", "text", "evidence", "pr", "repo", "number", "head"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlotSettings {
    /// Slots per name unless `sizes` names it (LANE req. 21, default 1).
    pub(crate) count: u32,
    /// Grants wait while the one-minute load per core is at or above this per cent (default 80).
    pub(crate) load: u32,
    pub(crate) sizes: Vec<(String, u32)>,
}

impl SlotSettings {
    pub(crate) fn size(&self, name: &str) -> u32 {
        self.sizes
            .iter()
            .find(|(n, _)| n == name)
            .map_or(self.count, |(_, c)| *c)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannerSettings {
    pub(crate) enabled: bool,
    pub(crate) interval: Duration,
    pub(crate) max_open: u32,
    pub(crate) max_starts: u32,
    /// Local hour of the owner-notes refresh; checked by the planner, which reports a bad one as a
    /// finding (COORD req. 49).
    pub(crate) notes_hour: Option<i64>,
    pub(crate) import_label: String,
    pub(crate) decision_label: String,
    /// A queued launch expires after this (LANE req. 23).
    pub(crate) queue_expiry: Duration,
}

impl Default for PlannerSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            interval: Duration::from_hours(1),
            max_open: 3,
            max_starts: 2,
            notes_hour: None,
            import_label: "cataly".into(),
            decision_label: "needs-decision".into(),
            queue_expiry: Duration::from_hours(24),
        }
    }
}

/// Argv templates run on lane events (COORD req. 57).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EventCommands {
    pub(crate) finished: Option<Vec<String>>,
    pub(crate) report: Option<Vec<String>>,
    pub(crate) dead: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublishRoot {
    pub(crate) dir: PathBuf,
    pub(crate) url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CoordSettings {
    pub(crate) slots: SlotSettings,
    pub(crate) planner: PlannerSettings,
    /// OMP model of the built-in reviewer; `None` lets OMP choose.
    pub(crate) review_model: Option<String>,
    /// A custom reviewer replaces the built-in one (COORD req. 56).
    pub(crate) reviewer: Option<Vec<String>>,
    pub(crate) events: EventCommands,
    pub(crate) publish: Vec<PublishRoot>,
    /// Board destinations: `discord:<channel id>` (EVR req. 51).
    pub(crate) board: Vec<String>,
}

impl Default for SlotSettings {
    fn default() -> Self {
        Self {
            count: 1,
            load: 80,
            sizes: Vec::new(),
        }
    }
}

/// A slot name: a letter or digit, then letters, digits, `_`, `.` or `-`, 64 at most (COORD req. 42).
pub(crate) fn slot_name_ok(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// The placeholder names an element uses; `Err` names an unknown one or a stray brace.
pub(crate) fn placeholders(element: &str) -> Result<Vec<&str>, String> {
    let mut out = Vec::new();
    let mut rest = element;
    while let Some(open) = rest.find('{') {
        let after = rest.get(open + 1..).unwrap_or_default();
        let close = after.find('}').ok_or("a '{' without '}'")?;
        let name = after.get(..close).unwrap_or_default();
        if !PLACEHOLDERS.contains(&name) {
            return Err(format!(
                "unknown placeholder {{{name}}}; known: {}",
                PLACEHOLDERS.join(", ")
            ));
        }
        out.push(name);
        rest = after.get(close + 1..).unwrap_or_default();
    }
    Ok(out)
}

fn template(errors: &mut Vec<ConfigError>, s: &mut Section<'_>, key: &str) -> Option<Vec<String>> {
    s.get(key)?;
    let argv = s.argv(errors, key)?;
    for (i, element) in argv.iter().enumerate() {
        if let Err(e) = placeholders(element) {
            errors.push(ConfigError {
                path: format!("{}[{i}]", s.child(key)),
                message: e,
            });
            return None;
        }
    }
    Some(argv)
}

fn parse_slots(errors: &mut Vec<ConfigError>, s: &mut Section<'_>, slots: &mut SlotSettings) {
    if let Some(n) = s.integer(errors, "count", 1, 64) {
        slots.count = u32::try_from(n).unwrap_or(1);
    }
    if let Some(n) = s.integer(errors, "load", 1, 10_000) {
        slots.load = u32::try_from(n).unwrap_or(80);
    }
    if let Some(mut sizes) = s.table(errors, "sizes") {
        for (name, value) in sizes.entries() {
            match value.as_integer().and_then(|n| u32::try_from(n).ok()) {
                Some(n) if slot_name_ok(name) && (1..=64).contains(&n) => {
                    slots.sizes.push((name.clone(), n));
                }
                _ => errors.push(ConfigError {
                    path: sizes.child(name),
                    message: "expected a slot name and a count of 1 to 64".into(),
                }),
            }
        }
        sizes.finish(errors);
    }
}

fn parse_planner(
    errors: &mut Vec<ConfigError>,
    p: &mut Section<'_>,
    planner: &mut PlannerSettings,
) {
    if let Some(b) = p.bool(errors, "enabled") {
        planner.enabled = b;
    }
    if let Some(d) = p.duration(
        errors,
        "interval",
        Duration::from_mins(1),
        Duration::from_hours(24),
    ) {
        planner.interval = d;
    }
    if let Some(n) = p.integer(errors, "max_open", 0, 1000) {
        planner.max_open = u32::try_from(n).unwrap_or(3);
    }
    if let Some(n) = p.integer(errors, "max_starts", 0, 100) {
        planner.max_starts = u32::try_from(n).unwrap_or(2);
    }
    planner.notes_hour = p.typed(
        errors,
        "notes_hour",
        "a whole number",
        toml::Value::as_integer,
    );
    for (key, slot) in [
        ("import_label", &mut planner.import_label),
        ("decision_label", &mut planner.decision_label),
    ] {
        if let Some(v) = p.string(errors, key) {
            *slot = v.to_string();
        }
    }
    if let Some(d) = p.duration(
        errors,
        "queue_expiry",
        Duration::from_hours(1),
        Duration::from_hours(24 * 30),
    ) {
        planner.queue_expiry = d;
    }
}

pub(super) fn parse(
    errors: &mut Vec<ConfigError>,
    root: &mut Section<'_>,
    out: &mut CoordSettings,
) {
    let Some(mut c) = root.table(errors, "coord") else {
        return;
    };
    if let Some(mut s) = c.table(errors, "slots") {
        parse_slots(errors, &mut s, &mut out.slots);
        s.finish(errors);
    }
    if let Some(mut p) = c.table(errors, "planner") {
        parse_planner(errors, &mut p, &mut out.planner);
        p.finish(errors);
    }
    if let Some(mut r) = c.table(errors, "review") {
        out.review_model = r.string(errors, "model").map(str::to_string);
        out.reviewer = template(errors, &mut r, "command");
        r.finish(errors);
    }
    if let Some(mut e) = c.table(errors, "events") {
        out.events = EventCommands {
            finished: template(errors, &mut e, "finished"),
            report: template(errors, &mut e, "report"),
            dead: template(errors, &mut e, "dead"),
        };
        e.finish(errors);
    }
    for mut root in c.tables(errors, "publish").unwrap_or_default() {
        let dir = root.required_string(errors, "dir").unwrap_or_default();
        if !std::path::Path::new(dir).is_absolute() {
            errors.push(root.error("dir", "must be an absolute directory"));
        }
        let url = root.string(errors, "url").map(str::to_string);
        if let Some(u) = &url {
            let ok = (u.starts_with("https://") || u.starts_with("http://"))
                && !u.contains(['?', '#', '@'])
                && !u.chars().any(char::is_whitespace);
            if !ok {
                errors.push(root.error(
                    "url",
                    "expected http(s) without credentials, query or fragment",
                ));
            }
        }
        root.finish(errors);
        out.publish.push(PublishRoot {
            dir: PathBuf::from(dir),
            url,
        });
    }
    if let Some(dest) = c.strings(errors, "board") {
        for (i, d) in dest.iter().enumerate() {
            let ok = d.strip_prefix("discord:").is_some_and(|id| {
                (17..=20).contains(&id.len()) && id.chars().all(|c| c.is_ascii_digit())
            });
            if !ok {
                errors.push(ConfigError {
                    path: format!("coord.board[{i}]"),
                    message: "expected discord:<channel id>".into(),
                });
            }
        }
        out.board = dest;
    }
    c.finish(errors);
}

#[cfg(test)]
mod tests {
    #[test]
    fn coord_section_parses_and_names_errors() {
        let config = crate::config::parse(
            "[coord.slots]\ncount = 2\nsizes = { build = 1 }\n[coord.planner]\nmax_open = 4\n\
             [coord.events]\nfinished = [\"/bin/echo\", \"{key} {pr}\"]\n\
             [[coord.publish]]\ndir = \"/srv/pub\"\nurl = \"https://x.test/p\"\n",
        )
        .unwrap();
        assert_eq!(config.coord.slots.size("build"), 1);
        assert_eq!(config.coord.slots.size("test"), 2);
        assert_eq!(config.coord.planner.max_open, 4);
        assert!(config.coord.events.finished.is_some());
        let errors = crate::config::parse(
            "[coord.events]\nreport = [\"/bin/echo\", \"{nope}\"]\n[coord]\nboard = [\"x\"]\n",
        )
        .unwrap_err();
        let paths: Vec<&str> = errors.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"coord.events.report[1]"), "{paths:?}");
        assert!(paths.contains(&"coord.board[0]"), "{paths:?}");
    }
}
