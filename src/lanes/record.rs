//! The lane record (LANE req. 28-35) and the pure rules of opening a lane (LANE req. 1-14): key,
//! model, origin, ceilings and brief validation, and the assembled brief with its instruction block.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::checks::{self, Signal};
use crate::config::lanes::LaneSettings;
use crate::config::session::THINKING;
use crate::omp::transcript::model_selector;

/// Words no lane key may be (LANE req. 3, COORD-48).
const RESERVED: [&str; 6] = ["lead", "owner", "hub", "cataly", "none", "all"];
const TITLE_MAX: usize = 200;
const BRIEF_MAX: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum State {
    Starting,
    Running,
    Held,
    Claimed,
    Rejected,
    Closing,
    Closed,
}

impl State {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Held => "held",
            Self::Claimed => "claimed",
            Self::Rejected => "rejected",
            Self::Closing => "closing",
            Self::Closed => "closed",
        }
    }
}

/// Ceilings of one lane; `None` is no ceiling (WS req. 24).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Ceilings {
    pub(crate) minutes: Option<f64>,
    pub(crate) tokens: Option<f64>,
    pub(crate) cost: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Claim {
    pub(crate) at_ms: i64,
    pub(crate) evidence: String,
    pub(crate) close_at_ms: i64,
}

/// A recorded close, park or abandon intent (LANE req. 51, 58, 62). `step` is the last step done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Intent {
    /// `completed`, `parked` or `abandoned`.
    pub(crate) outcome: String,
    pub(crate) note: String,
    pub(crate) condition: Option<String>,
    pub(crate) checks: Vec<String>,
    pub(crate) step: String,
    pub(crate) receipt: Vec<String>,
    pub(crate) lesson: Option<String>,
}

/// A one-attempt grant for one rule of one command (LANE req. 75).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Grant {
    pub(crate) rule: usize,
    pub(crate) digest: String,
    pub(crate) reason: String,
    pub(crate) used: bool,
}

/// Oversight's memory of one run (COORD req. 7-12, WS req. 11-28).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Watch {
    /// Start of the current progress round.
    pub(crate) round_ms: i64,
    pub(crate) reminders: u8,
    pub(crate) alerted: bool,
    pub(crate) stuck: bool,
    /// Latest moment the agent was seen working.
    pub(crate) active_ms: i64,
    pub(crate) ceiling: Option<String>,
    pub(crate) ceiling_warned: Vec<String>,
    pub(crate) dialogs: Vec<String>,
    /// A held lane started a turn and the lead heard it for this active stretch (LANE req. 47).
    pub(crate) active_noticed: bool,
    pub(crate) thread_post_ms: Option<i64>,
}

/// The lane record, stored as JSON (LANE req. 28, "State that must survive a restart").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub(crate) key: String,
    pub(crate) title: String,
    /// The assembled brief the lane receives (LANE req. 12).
    pub(crate) brief: String,
    pub(crate) checks: Vec<Signal>,
    pub(crate) thread: Option<String>,
    pub(crate) cwd: String,
    pub(crate) model: String,
    pub(crate) thinking: Option<String>,
    pub(crate) origin: Option<String>,
    pub(crate) look_only: bool,
    pub(crate) ceilings: Ceilings,
    pub(crate) worktrees: Vec<String>,
    pub(crate) state: State,
    pub(crate) prev: Option<State>,
    pub(crate) outcome: Option<String>,
    pub(crate) opened_ms: i64,
    pub(crate) closed_ms: Option<i64>,
    pub(crate) run_id: String,
    pub(crate) handle: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) session_file: Option<String>,
    pub(crate) earlier_ids: Vec<String>,
    pub(crate) pid: Option<u32>,
    /// `start held: model`, `unconfirmed`, or why the start did not finish.
    pub(crate) start_note: Option<String>,
    pub(crate) held_by: Option<String>,
    pub(crate) hold_reason: Option<String>,
    pub(crate) claim: Option<Claim>,
    pub(crate) objection: Vec<String>,
    pub(crate) intent: Option<Intent>,
    pub(crate) receipt: Vec<String>,
    pub(crate) lost: Option<String>,
    /// Shell rules snapshot taken at open: deny patterns and this lane's allow patterns.
    pub(crate) deny: Vec<String>,
    pub(crate) allow: Vec<String>,
    pub(crate) grants: Vec<Grant>,
    pub(crate) last_block: Option<(String, Vec<usize>)>,
    /// An in-doubt lead-to-lane delivery fences the lane's inbox (EVR req. 62).
    pub(crate) doubt: Option<String>,
    pub(crate) watch: Watch,
    /// Final spend reading of the run: tokens and cost (WS req. 29).
    pub(crate) spend: Option<(f64, f64)>,
    /// 1 runs first, 9 last; shared with items and slot waiters (COORD req. 41).
    #[serde(default = "default_priority")]
    pub(crate) priority: u8,
    /// The runtime the lane's session was started with; a resume or reopen stays on it (HOST req. 91).
    #[serde(default)]
    pub(crate) runtime: crate::config::session::RuntimeKind,
    /// The worker the lane runs on (docs/design/workers.md 2.6); `None` runs it on this machine.
    /// Its `cwd` and `worktrees` are then paths on that worker.
    #[serde(default)]
    pub(crate) worker: Option<String>,
}

pub(crate) const fn default_priority() -> u8 {
    5
}

impl Record {
    pub(crate) fn open(&self) -> bool {
        self.state != State::Closed
    }

    /// Where the lane's per-machine work runs: here, or on its worker.
    pub(crate) fn place(&self) -> crate::worker::Place {
        crate::worker::Place::of(self.worker.as_deref())
    }

    /// The text of the state for listings.
    pub(crate) fn status(&self) -> String {
        let mut s = self.state.name().to_string();
        if let Some(o) = &self.outcome {
            let _ = write!(s, " ({o})");
        }
        if let Some(n) = &self.start_note {
            let _ = write!(s, ", {n}");
        }
        if self.lost.is_some() {
            s.push_str(", lost");
        }
        s
    }
}

/// The inputs of an open (LANE req. 2).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct OpenInput {
    pub(crate) key: String,
    pub(crate) title: String,
    pub(crate) brief: Option<String>,
    pub(crate) brief_file: Option<String>,
    pub(crate) checks: Value,
    pub(crate) thread: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) origin: Option<String>,
    pub(crate) look_only: bool,
    pub(crate) ceilings: Option<Value>,
    pub(crate) preview: bool,
    pub(crate) request_id: Option<String>,
    pub(crate) worktree: Option<Worktree>,
    pub(crate) priority: Option<u8>,
    /// Predecessor lanes: the open is queued until they closed completed (LANE req. 22).
    pub(crate) after: Vec<String>,
    /// A queued launch expires after this many hours (LANE req. 23).
    pub(crate) expires_hours: Option<f64>,
    /// A configured worker to run the lane on (docs/design/workers.md 2.6).
    pub(crate) worker: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Worktree {
    pub(crate) path: String,
    pub(crate) branch: String,
    pub(crate) base: String,
}

/// An open that passed every check: the parts the record is built from.
#[derive(Debug, Clone)]
pub(crate) struct Validated {
    pub(crate) key: String,
    pub(crate) title: String,
    pub(crate) brief: String,
    pub(crate) checks: Vec<Signal>,
    pub(crate) thread: Option<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) model: String,
    pub(crate) thinking: Option<String>,
    pub(crate) origin: Option<String>,
    pub(crate) look_only: bool,
    pub(crate) ceilings: Ceilings,
    pub(crate) worktree: Option<Worktree>,
    pub(crate) priority: u8,
    pub(crate) warnings: Vec<String>,
    pub(crate) worker: Option<String>,
}

/// LANE req. 3.
pub(crate) fn key_error(key: &str) -> Option<String> {
    let len = key.chars().count();
    let first = key.chars().next();
    if !(2..=61).contains(&len)
        || !first.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        || !key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
    {
        return Some(
            "a key is 2 to 61 lowercase letters, digits, '.', '_' or '-', starting with a letter or digit"
                .into(),
        );
    }
    RESERVED
        .contains(&key)
        .then(|| format!("'{key}' is a reserved word"))
}

/// One exact `provider/id`, optionally `:thinking` (LANE req. 14). Aliases, lists and wildcards are
/// refused.
pub(crate) fn model_error(model: &str) -> Option<String> {
    let (base, thinking) = match model.rsplit_once(':') {
        Some((b, t)) if THINKING.contains(&t) => (b, Some(t)),
        _ => (model, None),
    };
    let _ = thinking;
    if !model_selector(base) || base.contains(['*', ',', '?', '[']) || base.ends_with('/') {
        return Some(format!(
            "'{model}' is not one exact provider/model (aliases, lists and wildcards are refused)"
        ));
    }
    None
}

fn digits(s: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&s.len()) && s.chars().all(|c| c.is_ascii_digit())
}

/// A name given with blanks around it, or nothing when it is blank.
fn named(text: Option<&str>) -> Option<String> {
    text.map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// LANE req. 5: the exact form of a Discord, Telegram or Slack message reference.
pub(crate) fn origin_ok(origin: &str) -> bool {
    let Some((relay, rest)) = origin.split_once(':') else {
        return false;
    };
    let Some((room, message)) = rest.split_once('/') else {
        return false;
    };
    match relay {
        "discord" => digits(room, 17, 20) && digits(message, 17, 20),
        "telegram" => {
            digits(room.strip_prefix('-').unwrap_or(room), 1, 20) && digits(message, 1, 12)
        }
        "slack" => {
            (2..=24).contains(&room.len())
                && room
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
                && message
                    .split_once('.')
                    .is_some_and(|(a, b)| digits(a, 1, 12) && digits(b, 1, 9))
        }
        _ => false,
    }
}

/// LANE req. 6 and WS req. 24: a number of at least 0, `"off"`, or absent (the configured default).
fn ceilings(
    input: Option<&Value>,
    defaults: &crate::config::lanes::Ceilings,
) -> Result<Ceilings, String> {
    let empty = serde_json::Map::new();
    let map = match input {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(m)) => m,
        Some(_) => return Err("ceilings must be an object of minutes, tokens and cost".into()),
    };
    if let Some(k) = map
        .keys()
        .find(|k| !["minutes", "tokens", "cost"].contains(&k.as_str()))
    {
        return Err(format!("unknown ceiling {k}"));
    }
    let one = |name: &str, default: Option<f64>| match map.get(name) {
        None => Ok(default),
        Some(Value::String(s)) if s == "off" => Ok(None),
        Some(v) => match v.as_f64() {
            Some(f) if f.is_finite() && f >= 0.0 => Ok(Some(f)),
            _ => Err(format!(
                "ceiling {name} must be a finite number of at least 0 or \"off\""
            )),
        },
    };
    Ok(Ceilings {
        minutes: one("minutes", defaults.minutes)?,
        tokens: one("tokens", defaults.tokens)?,
        cost: one("cost", defaults.cost)?,
    })
}

/// 1 runs first, 9 last; 5 when not given (COORD req. 41).
fn priority(given: Option<u8>) -> Result<u8, String> {
    let p = given.unwrap_or(default_priority());
    if (1..=9).contains(&p) {
        Ok(p)
    } else {
        Err("priority must be 1 to 9".into())
    }
}

/// Runs every open check without side effects (LANE req. 1-14). The brief file is read here.
pub(crate) fn validate(input: &OpenInput, settings: &LaneSettings) -> Result<Validated, String> {
    if let Some(e) = key_error(&input.key) {
        return Err(e);
    }
    let title = input.title.trim();
    if title.is_empty() || title.chars().count() > TITLE_MAX || title.contains('\n') {
        return Err("the title must be one line of 1 to 200 characters".into());
    }
    let brief = match (&input.brief, &input.brief_file) {
        (Some(text), None) => text.clone(),
        (None, Some(file)) => {
            let path = Path::new(file);
            if !path.is_absolute() {
                return Err("brief_file must be an absolute path".into());
            }
            let bytes =
                std::fs::read(path).map_err(|e| format!("the brief file is not readable: {e}"))?;
            String::from_utf8(bytes).map_err(|_| "the brief file is not UTF-8".to_string())?
        }
        _ => return Err("give exactly one of brief and brief_file".into()),
    };
    if brief.trim().is_empty() || brief.len() > BRIEF_MAX {
        return Err("the brief must hold 1 byte to 64 KiB of text".into());
    }
    let checks = checks::parse(&input.checks)?;
    let priority = priority(input.priority)?;
    if let Some(o) = &input.origin
        && !origin_ok(o)
    {
        return Err(format!(
            "origin '{o}' is not a Discord, Telegram or Slack message reference"
        ));
    }
    let model = input
        .model
        .clone()
        .or_else(|| settings.model.clone())
        .ok_or("no model: give one or set [lanes] model")?;
    if let Some(e) = model_error(&model) {
        return Err(e);
    }
    let (model, thinking) = match model.rsplit_once(':') {
        Some((b, t)) if THINKING.contains(&t) => (b.to_string(), Some(t.to_string())),
        _ => (model, None),
    };
    let ceilings = ceilings(input.ceilings.as_ref(), &settings.ceilings)?;
    let cwd = input
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .or_else(|| settings.cwd.clone())
        .ok_or("no working directory: give cwd or set [lanes] cwd")?;
    let cwd = std::fs::canonicalize(&cwd).map_err(|e| {
        format!(
            "the working directory {} does not exist: {e}",
            cwd.display()
        )
    })?;
    if !cwd.is_dir() {
        return Err(format!("{} is not a directory", cwd.display()));
    }
    let thread = match input.thread.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(t) => {
            let ok = t
                .strip_prefix("discord:")
                .is_some_and(|id| digits(id, 17, 20));
            if !ok {
                return Err("thread must be discord:<thread id>".into());
            }
            Some(t.to_string())
        }
    };
    if let Some(w) = &input.worktree
        && (!Path::new(&w.path).is_absolute()
            || w.branch.trim().is_empty()
            || w.base.trim().is_empty())
    {
        return Err("worktree needs an absolute path, a branch and a base".into());
    }
    let mut warnings = Vec::new();
    if thread.is_none() {
        warnings.push("no thread: reports stay with the lead".into());
    }
    Ok(Validated {
        key: input.key.clone(),
        title: title.to_string(),
        brief,
        checks,
        thread,
        cwd,
        model,
        thinking,
        origin: input.origin.clone(),
        look_only: input.look_only,
        ceilings,
        worktree: input.worktree.clone(),
        priority,
        warnings,
        worker: named(input.worker.as_deref()),
    })
}

/// A digest of the inputs for request-ID dedupe (LANE req. 10); the brief is only hashed.
pub(crate) fn digest(input: &OpenInput) -> String {
    let mut doc = serde_json::json!([
        input.key,
        input.title,
        input.brief,
        input.brief_file,
        input.checks,
        input.thread,
        input.cwd,
        input.model,
        input.origin,
        input.look_only,
        input.ceilings,
        input.worktree,
        input.priority,
        input.after,
    ]);
    // Added last and only when set, so digests of earlier local opens stay the same.
    if let (Some(worker), Some(list)) = (&input.worker, doc.as_array_mut()) {
        list.push(serde_json::json!(worker));
    }
    crate::config::sha256_hex(doc.to_string().as_bytes())
}

fn ceiling_text(c: &Ceilings) -> String {
    let one = |v: Option<f64>, unit: &str| {
        v.map_or_else(|| format!("no {unit} ceiling"), |v| format!("{v} {unit}"))
    };
    format!(
        "{}, {}, {}",
        one(c.minutes, "minutes"),
        one(c.tokens, "tokens"),
        one(c.cost, "USD")
    )
}

/// The lane's kickoff: the learned rules, the owner's brief, then the fixed instruction block (LANE
/// req. 12-13, OPC-60, OPC-93; EVR req. 47) and the steps already recorded (EVR req. 33).
pub(crate) fn kickoff(rec: &Record, rules: &str, steps: &[(String, String)]) -> String {
    let mut text = format!(
        "[lane {}] {}\n\n{rules}{}\n\n",
        rec.key,
        rec.title,
        rec.brief.trim()
    );
    let checks: Vec<String> = rec
        .checks
        .iter()
        .map(|s| serde_json::to_string(s).unwrap_or_default())
        .collect();
    let _ = write!(
        text,
        "Lane instructions\n\
         - You are lane {key}. Thread: {thread}. Done when every check passes: {checks}.\n\
         - The lead owns the thread's title and finished state; never rename, archive or mark it.\n\
         - Keep to the recorded directory {cwd}, its repository, branch and acceptance. Ask the lead \
         (report kind waiting) about missing or conflicting facts; never invent them.\n\
         - Report with the cataly_lane tool: op report (kind progress, stuck, waiting, stopped or \
         finished; text; optional summary; files and calls as evidence), op step after each external \
         effect, op claim with evidence when done. After a claim stay idle; a rejection means fix \
         and claim again with fresh evidence.\n\
         - Never redeem coupons or resets, spend or buy credits, enable overage, or switch account, \
         provider or model yourself. OMP's configured fallback may change the model for you; when \
         it is exhausted, stop, save progress and report.\n\
         - Ceilings: {ceilings}.\n\
         - Merging, deploying, publishing, spending and destroying data need an explicit owner grant.\n",
        key = rec.key,
        thread = rec.thread.as_deref().unwrap_or("none"),
        checks = checks.join("; "),
        cwd = rec.cwd,
        ceilings = ceiling_text(&rec.ceilings),
    );
    if rec.look_only {
        text.push_str("- Look only: read and write up findings; change nothing.\n");
    }
    if !steps.is_empty() {
        text.push_str(
            "\nSteps already recorded (do not do them again; confirm an effect whose record may be \
             missing on the remote system before retrying it):\n",
        );
        for (name, result) in steps {
            let _ = writeln!(text, "- {name}: {result}");
        }
    }
    text
}

/// The goal objective set through OMP's goal feature (LANE req. 15g).
pub(crate) fn objective(rec: &Record) -> String {
    format!(
        "Lane {}: {}. Stop when the done checks pass and the claim is made.",
        rec.key, rec.title
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input() -> OpenInput {
        OpenInput {
            key: "web-1".into(),
            title: "Fix it".into(),
            brief: Some("Do the thing".into()),
            checks: json!([{ "kind": "command", "argv": ["true"] }]),
            cwd: Some(std::env::temp_dir().display().to_string()),
            model: Some("p/m".into()),
            ..OpenInput::default()
        }
    }

    #[test]
    fn open_refuses_bad_keys_models_checks_and_origins() {
        let settings = LaneSettings::default();
        assert!(validate(&input(), &settings).is_ok());
        for key in ["Lead", "lead", "a", "-x", "x y", &"k".repeat(62)] {
            let mut i = input();
            i.key = key.into();
            assert!(validate(&i, &settings).is_err(), "{key}");
        }
        for model in ["sonnet", "p/*", "p/a,p/b", "p/"] {
            let mut i = input();
            i.model = Some(model.into());
            assert!(validate(&i, &settings).is_err(), "{model}");
        }
        let mut i = input();
        i.model = Some("p/m:high".into());
        let v = validate(&i, &settings).unwrap();
        assert_eq!(
            (v.model.as_str(), v.thinking.as_deref()),
            ("p/m", Some("high"))
        );
        let mut i = input();
        i.checks = json!([{ "kind": "pr_merged" }]);
        assert!(validate(&i, &settings).is_err());
        let mut i = input();
        i.origin = Some("discord:1/2".into());
        assert!(validate(&i, &settings).is_err());
        assert!(origin_ok("discord:123456789012345678/123456789012345679"));
        assert!(origin_ok("slack:C0123/1700000000.000100"));
        assert!(origin_ok("telegram:-100123/42"));
        let mut i = input();
        i.ceilings = Some(json!({ "cost": -1 }));
        assert!(validate(&i, &settings).is_err());
        i.ceilings = Some(json!({ "cost": "off", "minutes": 30 }));
        let v = validate(&i, &settings).unwrap();
        assert_eq!((v.ceilings.cost, v.ceilings.minutes), (None, Some(30.0)));
        assert_ne!(digest(&input()), digest(&i));
    }
}
