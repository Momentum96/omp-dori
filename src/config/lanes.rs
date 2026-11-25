//! `[lanes]` (LANE req. 2, 15, 18, 56, 72; WS req. 12-28; COORD req. 6-12; EVR req. 19): defaults
//! for opening lanes, the start deadline and objection window, ceilings, the oversight timings,
//! thread posting policy and the per-lane shell rules.

use std::path::PathBuf;
use std::time::Duration;

use super::{ConfigError, Section};
use crate::omp::transcript::model_selector;

const SECOND: Duration = Duration::from_secs(1);
const DAY: Duration = Duration::from_hours(24);

/// One allow rule: lane keys matching `lanes` (only `*` is special) may run commands that match one
/// of `patterns` whole (LANE req. 72).
#[derive(Debug, Clone)]
pub(crate) struct Allow {
    pub(crate) lanes: String,
    pub(crate) patterns: Vec<regex::Regex>,
}

/// Oversight timings (COORD req. 8-10, WS req. 22). Different timings are configuration only (C6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Oversight {
    pub(crate) enabled: bool,
    pub(crate) first_reminder: Duration,
    pub(crate) second_reminder: Duration,
    pub(crate) stall: Duration,
    pub(crate) busy: Duration,
    pub(crate) quiet: Duration,
    pub(crate) sweep: Duration,
    /// A progress report posts to the thread only after this quiet time (WS req. 14, C7).
    pub(crate) thread_quiet: Duration,
}

impl Default for Oversight {
    fn default() -> Self {
        Self {
            enabled: true,
            first_reminder: Duration::from_mins(10),
            second_reminder: Duration::from_mins(20),
            stall: Duration::from_mins(40),
            busy: Duration::from_mins(20),
            quiet: Duration::from_mins(20),
            sweep: Duration::from_mins(5),
            thread_quiet: Duration::from_mins(20),
        }
    }
}

/// Default ceilings; `None` is no ceiling (WS req. 24).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Ceilings {
    pub(crate) minutes: Option<f64>,
    pub(crate) tokens: Option<f64>,
    pub(crate) cost: Option<f64>,
}

/// `[lanes]`.
#[derive(Debug, Clone)]
pub(crate) struct LaneSettings {
    pub(crate) model: Option<String>,
    pub(crate) cwd: Option<PathBuf>,
    /// Live lanes at most; 0 disables the check (LANE req. 18).
    pub(crate) max: u32,
    pub(crate) start_budget: Duration,
    pub(crate) objection: Duration,
    pub(crate) ceilings: Ceilings,
    pub(crate) oversight: Oversight,
    /// Report kinds that post to the lane's thread automatically (EVR req. 19).
    pub(crate) post_kinds: Vec<String>,
    /// Raw report text may be posted (else only owner summaries).
    pub(crate) post_raw: bool,
    /// Add the owner to a lane thread the hub creates (OJ-28).
    pub(crate) thread_member: bool,
    pub(crate) deny: Vec<regex::Regex>,
    pub(crate) allow: Vec<Allow>,
    /// Repository (`owner/name`) to the commit identity the policy check expects (EVR req. 58).
    pub(crate) identities: Vec<(String, String)>,
}

pub(crate) const KINDS: [&str; 5] = ["progress", "stuck", "waiting", "stopped", "finished"];

impl Default for LaneSettings {
    fn default() -> Self {
        Self {
            model: None,
            cwd: None,
            max: 20,
            start_budget: Duration::from_secs(180),
            objection: Duration::from_mins(5),
            ceilings: Ceilings::default(),
            oversight: Oversight::default(),
            post_kinds: KINDS.iter().map(|k| (*k).to_string()).collect(),
            post_raw: true,
            thread_member: true,
            deny: Vec::new(),
            allow: Vec::new(),
            identities: Vec::new(),
        }
    }
}

#[allow(clippy::cast_precision_loss)]
fn ceiling(errors: &mut Vec<ConfigError>, s: &mut Section<'_>, key: &str) -> Option<f64> {
    let v = s.get(key)?;
    match v.as_float().or_else(|| v.as_integer().map(|i| i as f64)) {
        Some(f) if f.is_finite() && f >= 0.0 => Some(f),
        _ => {
            errors.push(s.error(key, "expected a finite number of at least 0"));
            None
        }
    }
}

fn patterns(errors: &mut Vec<ConfigError>, s: &mut Section<'_>, key: &str) -> Vec<regex::Regex> {
    let mut out = Vec::new();
    for (i, text) in s
        .strings(errors, key)
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        match regex::Regex::new(text) {
            Ok(re) => out.push(re),
            Err(e) => errors.push(ConfigError {
                path: format!("{}[{i}]", s.child(key)),
                message: format!("invalid pattern: {e}"),
            }),
        }
    }
    out
}

pub(super) fn parse(
    errors: &mut Vec<ConfigError>,
    root: &mut Section<'_>,
    lanes: &mut LaneSettings,
) {
    let Some(mut s) = root.table(errors, "lanes") else {
        return;
    };
    if let Some(m) = s.string(errors, "model") {
        if model_selector(m) {
            lanes.model = Some(m.to_string());
        } else {
            errors.push(s.error("model", "expected provider/id"));
        }
    }
    if let Some(cwd) = s.string(errors, "cwd") {
        if std::path::Path::new(cwd).is_absolute() {
            lanes.cwd = Some(PathBuf::from(cwd));
        } else {
            errors.push(s.error("cwd", "must be an absolute path"));
        }
    }
    if let Some(max) = s.integer(errors, "max", 0, 1000) {
        lanes.max = u32::try_from(max).unwrap_or(20);
    }
    if let Some(d) = s.duration(
        errors,
        "start_budget",
        Duration::from_secs(10),
        Duration::from_mins(30),
    ) {
        lanes.start_budget = d;
    }
    if let Some(d) = s.duration(errors, "objection", SECOND, DAY) {
        lanes.objection = d;
    }
    lanes.ceilings = Ceilings {
        minutes: ceiling(errors, &mut s, "ceiling_minutes"),
        tokens: ceiling(errors, &mut s, "ceiling_tokens"),
        cost: ceiling(errors, &mut s, "ceiling_cost"),
    };
    if let Some(kinds) = s.strings(errors, "post_kinds") {
        if kinds.iter().all(|k| KINDS.contains(&k.as_str())) {
            lanes.post_kinds = kinds;
        } else {
            errors.push(s.error(
                "post_kinds",
                &format!("expected kinds from {}", KINDS.join(", ")),
            ));
        }
    }
    if let Some(raw) = s.bool(errors, "post_raw") {
        lanes.post_raw = raw;
    }
    if let Some(member) = s.bool(errors, "thread_member") {
        lanes.thread_member = member;
    }
    if let Some(mut o) = s.table(errors, "oversight") {
        parse_oversight(errors, &mut o, &mut lanes.oversight);
        o.finish(errors);
    }
    if let Some(mut ids) = s.table(errors, "identities") {
        for (repo, value) in ids.entries() {
            match value.as_str() {
                Some(email) if email.contains('@') => {
                    lanes.identities.push((repo.clone(), email.to_string()));
                }
                _ => errors.push(ConfigError {
                    path: ids.child(repo),
                    message: "expected an email address".into(),
                }),
            }
        }
        ids.finish(errors);
    }
    if let Some(mut sh) = s.table(errors, "shell") {
        lanes.deny = patterns(errors, &mut sh, "deny");
        for mut a in sh.tables(errors, "allow").unwrap_or_default() {
            let glob = a.required_string(errors, "lanes").unwrap_or("").to_string();
            let list = patterns(errors, &mut a, "patterns");
            a.finish(errors);
            lanes.allow.push(Allow {
                lanes: glob,
                patterns: list,
            });
        }
        sh.finish(errors);
    }
    s.finish(errors);
}

fn parse_oversight(errors: &mut Vec<ConfigError>, o: &mut Section<'_>, ov: &mut Oversight) {
    if let Some(b) = o.bool(errors, "enabled") {
        ov.enabled = b;
    }
    for (key, slot) in [
        ("first_reminder", &mut ov.first_reminder),
        ("second_reminder", &mut ov.second_reminder),
        ("stall", &mut ov.stall),
        ("busy", &mut ov.busy),
        ("quiet", &mut ov.quiet),
        ("sweep", &mut ov.sweep),
        ("thread_quiet", &mut ov.thread_quiet),
    ] {
        if let Some(d) = o.duration(errors, key, SECOND, DAY) {
            *slot = d;
        }
    }
    if ov.second_reminder <= ov.first_reminder || ov.stall <= ov.second_reminder {
        errors.push(o.error(
            "stall",
            "the reminders and the stall alert must come in this order",
        ));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn lanes_section_parses_and_names_errors() {
        let config = crate::config::parse(
            "[lanes]\nmodel = \"p/m\"\nobjection = \"2s\"\nceiling_cost = 1.5\n\
             [lanes.oversight]\nfirst_reminder = \"1s\"\nsecond_reminder = \"2s\"\nstall = \"4s\"\n\
             [lanes.shell]\ndeny = [\"git push\"]\n[[lanes.shell.allow]]\nlanes = \"web-*\"\npatterns = [\"git push origin x\"]\n",
        )
        .unwrap();
        assert_eq!(config.lanes.model.as_deref(), Some("p/m"));
        assert_eq!(config.lanes.ceilings.cost, Some(1.5));
        assert_eq!(config.lanes.deny.len(), 1);
        assert_eq!(config.lanes.allow.len(), 1);
        let errors = crate::config::parse(
            "[lanes]\nmodel = \"bad\"\nceiling_cost = -1\nbogus = 1\n[lanes.shell]\ndeny = [\"(\"]\n",
        )
        .unwrap_err();
        let paths: Vec<&str> = errors.iter().map(|e| e.path.as_str()).collect();
        for p in [
            "lanes.model",
            "lanes.ceiling_cost",
            "lanes.bogus",
            "lanes.shell.deny[0]",
        ] {
            assert!(paths.contains(&p), "{p}: {paths:?}");
        }
    }
}
