//! Settings of the built-in owner job kinds (DESIGN 6.1; OJ req. 15-49; WS-24, WS-25; LANE-08,
//! LANE-09). Everything personal (repositories, logins, thresholds, commands, paths) is set here and
//! nowhere else. Each kind has a default and a minimum cadence and a default per-tick deadline.

use std::path::PathBuf;
use std::time::Duration;

use regex::Regex;

use super::{ConfigError, DAY, Section};
use crate::jobs::event::EventKind;

/// One owner job's settings.
#[derive(Debug, Clone)]
pub(crate) enum Owner {
    Github(Github),
    Pr(Pr),
    Quota(Quota),
    Backup(Backup),
    Health(Health),
    Items(Items),
    Pressure(Pressure),
}

/// Who counts as the owner on GitHub: own activity is never reported as someone else's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Own {
    /// Own login; when absent the authenticated `gh` user is read once.
    pub(crate) login: Option<String>,
    pub(crate) bots: Vec<String>,
    pub(crate) emails: Vec<String>,
}

/// `kind = "github"` (OJ-05, OJ-06).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Github {
    pub(crate) gh: String,
    pub(crate) repos: Vec<WatchedRepo>,
    pub(crate) notifications: bool,
    pub(crate) own: Own,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WatchedRepo {
    /// `owner/name`, lower case.
    pub(crate) repo: String,
    pub(crate) authors: Vec<String>,
}

/// `kind = "pr"`: follows PRs named with `cataly pr follow` (WS-24, WS-25).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pr {
    pub(crate) gh: String,
    pub(crate) own: Own,
}

/// `kind = "quota"` (OJ-07, OJ-08, WS-23).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Quota {
    /// Providers watched; empty means all that OMP reports.
    pub(crate) providers: Vec<String>,
    /// Alert levels in per cent, ascending.
    pub(crate) levels: Vec<u8>,
    /// The quota gate closes at this level.
    pub(crate) hold: u8,
}

/// `kind = "backup"` (OJ-15).
#[derive(Debug, Clone)]
pub(crate) struct Backup {
    pub(crate) dir: PathBuf,
    pub(crate) remote: String,
    pub(crate) branch: String,
    pub(crate) name: String,
    pub(crate) email: String,
    /// Owner secret patterns, beside the built-in token shapes.
    pub(crate) patterns: Vec<Regex>,
    pub(crate) max_file_bytes: u64,
    /// The Cataly home backup (owner decision 16): before each tick a copy of the config file and a
    /// consistent store snapshot are written into the tree.
    pub(crate) include_cataly: bool,
}

/// `kind = "health"` (OJ-16, OJ-18, WS-34).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Health {
    /// Consecutive bad ticks before a problem counts (1 to 5).
    pub(crate) strikes: u32,
    pub(crate) files: Vec<FileAge>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileAge {
    pub(crate) path: PathBuf,
    pub(crate) max_age: Duration,
}

/// `kind = "items"`: new items from an owner command, such as a mail search (OJ-19).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Items {
    pub(crate) argv: Vec<String>,
    pub(crate) cwd: PathBuf,
}

/// `kind = "pressure"`: host sampler and notices (LANE-08, LANE-09, WS-36).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pressure {
    /// Load per core in per cent above which the host is in alert.
    pub(crate) alert_load: u32,
    /// Load per core in per cent below which heavy compute is welcome.
    pub(crate) ready_load: u32,
    /// Free memory and container headroom below this share hold starts.
    pub(crate) min_free_memory: u32,
    pub(crate) min_free_disk_gb: u64,
    /// The file system checked for disk space; default the home directory.
    pub(crate) disk_path: Option<PathBuf>,
    /// Live lane sessions at or above this hold starts; 0 disables the check.
    pub(crate) max_sessions: u32,
}

impl Pressure {
    /// The memory rule session starts use when no pressure job is configured (HOST req. 90).
    pub(crate) fn admission_default() -> Self {
        Self {
            alert_load: 150,
            ready_load: 80,
            min_free_memory: 20,
            min_free_disk_gb: 0,
            disk_path: None,
            max_sessions: 0,
        }
    }
}

/// Kind names, default cadence, minimum cadence, default per-tick deadline.
const KINDS: [(&str, Duration, Duration, Duration); 7] = [
    (
        "github",
        Duration::from_mins(3),
        Duration::from_mins(1),
        Duration::from_mins(3),
    ),
    (
        "pr",
        Duration::from_mins(3),
        Duration::from_mins(1),
        Duration::from_mins(3),
    ),
    (
        "quota",
        Duration::from_mins(5),
        Duration::from_mins(2),
        Duration::from_secs(30),
    ),
    (
        "backup",
        Duration::from_mins(30),
        Duration::from_mins(5),
        Duration::from_mins(10),
    ),
    (
        "health",
        Duration::from_mins(2),
        Duration::from_mins(1),
        Duration::from_secs(60),
    ),
    (
        "items",
        Duration::from_mins(30),
        Duration::from_mins(5),
        Duration::from_secs(60),
    ),
    (
        "pressure",
        Duration::from_secs(60),
        Duration::from_secs(30),
        Duration::from_secs(30),
    ),
];

pub(crate) const KIND_NAMES: [&str; 7] = [
    "github", "pr", "quota", "backup", "health", "items", "pressure",
];

pub(crate) fn is_kind(name: &str) -> bool {
    KIND_NAMES.contains(&name)
}

/// What [`parse`] found.
pub(super) struct Parsed {
    pub(crate) every: Duration,
    pub(crate) timeout: Duration,
    pub(crate) owner: Owner,
}

const COMMON: [EventKind; 2] = [EventKind::TickFailed, EventKind::TickRecovered];

impl Owner {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Github(_) => "github",
            Self::Pr(_) => "pr",
            Self::Quota(_) => "quota",
            Self::Backup(_) => "backup",
            Self::Health(_) => "health",
            Self::Items(_) => "items",
            Self::Pressure(_) => "pressure",
        }
    }

    /// The event kinds this job may emit.
    pub(crate) fn events(&self) -> &'static [EventKind] {
        const PR: [EventKind; 9] = [
            COMMON[0],
            COMMON[1],
            EventKind::PrBaseline,
            EventKind::PrFeedback,
            EventKind::PrCheckFailed,
            EventKind::PrPushed,
            EventKind::PrMerged,
            EventKind::PrClosed,
            EventKind::PrReopened,
        ];
        const GITHUB: [EventKind; 11] = [
            COMMON[0],
            COMMON[1],
            EventKind::GithubPr,
            EventKind::GithubNotification,
            EventKind::PrBaseline,
            EventKind::PrFeedback,
            EventKind::PrCheckFailed,
            EventKind::PrPushed,
            EventKind::PrMerged,
            EventKind::PrClosed,
            EventKind::PrReopened,
        ];
        match self {
            Self::Github(_) => &GITHUB,
            Self::Pr(_) => &PR,
            Self::Quota(_) => &[
                COMMON[0],
                COMMON[1],
                EventKind::QuotaLevel,
                EventKind::QuotaRecovered,
                EventKind::QuotaHold,
                EventKind::QuotaReleased,
            ],
            Self::Backup(_) => &[
                COMMON[0],
                COMMON[1],
                EventKind::BackupRefused,
                EventKind::BackupClean,
                EventKind::BackupPushFailed,
            ],
            Self::Health(_) => &[
                COMMON[0],
                COMMON[1],
                EventKind::HealthProblem,
                EventKind::HealthCleared,
            ],
            Self::Items(_) => &[COMMON[0], COMMON[1], EventKind::ItemNew],
            Self::Pressure(_) => &[
                COMMON[0],
                COMMON[1],
                EventKind::PressureAlert,
                EventKind::PressureClear,
                EventKind::PressureReady,
                EventKind::PressureBusy,
            ],
        }
    }
}

/// Parses an owner kind's settings; `None` when `kind` is not an owner kind or a setting is invalid.
pub(super) fn parse(
    kind: &str,
    errors: &mut Vec<ConfigError>,
    s: &mut Section<'_>,
) -> Option<Parsed> {
    let (_, every_default, every_min, timeout) = KINDS.iter().find(|k| k.0 == kind).copied()?;
    let before = errors.len();
    let every = s
        .duration(errors, "every", every_min, DAY)
        .unwrap_or(every_default);
    let owner = match kind {
        "github" => Owner::Github(github(errors, s)),
        "pr" => Owner::Pr(Pr {
            gh: s.string(errors, "gh").unwrap_or("gh").to_string(),
            own: own(errors, s),
        }),
        "quota" => Owner::Quota(quota(errors, s)),
        "backup" => Owner::Backup(backup(errors, s)?),
        "health" => Owner::Health(health(errors, s)),
        "items" => Owner::Items(items(errors, s)?),
        _ => Owner::Pressure(pressure(errors, s)),
    };
    (errors.len() == before).then_some(Parsed {
        every,
        timeout,
        owner,
    })
}

fn own(errors: &mut Vec<ConfigError>, s: &mut Section<'_>) -> Own {
    Own {
        login: s.string(errors, "login").map(str::to_string),
        bots: s.strings(errors, "bots").unwrap_or_default(),
        emails: s.strings(errors, "emails").unwrap_or_default(),
    }
}

/// `owner/name` with GitHub's allowed characters.
fn repo_name(text: &str) -> Option<String> {
    let (owner, name) = text.split_once('/')?;
    let ok = |p: &str| {
        !p.is_empty()
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    (ok(owner) && ok(name)).then(|| text.to_ascii_lowercase())
}

fn github(errors: &mut Vec<ConfigError>, s: &mut Section<'_>) -> Github {
    let mut repos = Vec::new();
    if s.get("repos").is_none() {
        errors.push(s.error("repos", "required"));
    }
    for mut r in s.tables(errors, "repos").unwrap_or_default() {
        let repo = r.required_string(errors, "repo");
        match repo.and_then(repo_name) {
            Some(repo) => repos.push(WatchedRepo {
                repo,
                authors: r.strings(errors, "authors").unwrap_or_default(),
            }),
            None if repo.is_some() => errors.push(r.error("repo", "expected owner/name")),
            None => {}
        }
        r.finish(errors);
    }
    Github {
        gh: s.string(errors, "gh").unwrap_or("gh").to_string(),
        repos,
        notifications: s.bool(errors, "notifications").unwrap_or(true),
        own: own(errors, s),
    }
}

fn quota(errors: &mut Vec<ConfigError>, s: &mut Section<'_>) -> Quota {
    let levels: Vec<u8> = match s.typed(errors, "levels", "a list of whole numbers", |v| {
        v.as_array()?
            .iter()
            .map(|n| n.as_integer().and_then(|n| u8::try_from(n).ok()))
            .collect::<Option<Vec<u8>>>()
    }) {
        Some(mut l) => {
            l.sort_unstable();
            l.dedup();
            if l.is_empty() || l.iter().any(|p| !(1..=100).contains(p)) {
                errors.push(s.error("levels", "expected per cent values between 1 and 100"));
            }
            l
        }
        None => vec![70, 85, 95],
    };
    Quota {
        providers: s.strings(errors, "providers").unwrap_or_default(),
        levels,
        hold: s
            .integer(errors, "hold", 1, 100)
            .and_then(|n| u8::try_from(n).ok())
            .unwrap_or(85),
    }
}

fn backup(errors: &mut Vec<ConfigError>, s: &mut Section<'_>) -> Option<Backup> {
    let dir = s.required_string(errors, "dir").map(PathBuf::from);
    if dir.as_ref().is_some_and(|d| !d.is_absolute()) {
        errors.push(s.error("dir", "must be an absolute path"));
    }
    let name = s.required_string(errors, "name").map(str::to_string);
    let email = s.required_string(errors, "email").map(str::to_string);
    let mut patterns = Vec::new();
    for (i, p) in s
        .strings(errors, "patterns")
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        match Regex::new(p) {
            Ok(re) => patterns.push(re),
            Err(e) => errors.push(ConfigError {
                path: format!("{}[{i}]", s.child("patterns")),
                message: format!(
                    "not a valid pattern: {}",
                    e.to_string().lines().last().unwrap_or("")
                ),
            }),
        }
    }
    let max_mib = s.integer(errors, "max_file_mib", 1, 4096).unwrap_or(50);
    Some(Backup {
        dir: dir?,
        remote: s.string(errors, "remote").unwrap_or("origin").to_string(),
        branch: s.string(errors, "branch").unwrap_or("main").to_string(),
        name: name?,
        email: email?,
        patterns,
        include_cataly: s.bool(errors, "include_cataly").unwrap_or(false),
        max_file_bytes: u64::try_from(max_mib).unwrap_or(50) * 1024 * 1024,
    })
}

fn health(errors: &mut Vec<ConfigError>, s: &mut Section<'_>) -> Health {
    let mut files = Vec::new();
    for mut f in s.tables(errors, "files").unwrap_or_default() {
        let path = f.required_string(errors, "path").map(PathBuf::from);
        if path.as_ref().is_some_and(|p| !p.is_absolute()) {
            errors.push(f.error("path", "must be an absolute path"));
        }
        let max_age = f
            .duration(
                errors,
                "max_age",
                Duration::from_mins(1),
                Duration::from_hours(24 * 365),
            )
            .unwrap_or(Duration::from_hours(12));
        if let Some(path) = path {
            files.push(FileAge { path, max_age });
        }
        f.finish(errors);
    }
    Health {
        strikes: s
            .integer(errors, "strikes", 1, 5)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(2),
        files,
    }
}

fn items(errors: &mut Vec<ConfigError>, s: &mut Section<'_>) -> Option<Items> {
    let argv = s.argv(errors, "argv");
    let cwd = s
        .string(errors, "cwd")
        .map_or_else(|| PathBuf::from("/"), PathBuf::from);
    if !cwd.is_absolute() {
        errors.push(s.error("cwd", "must be an absolute path"));
    }
    Some(Items { argv: argv?, cwd })
}

fn pressure(errors: &mut Vec<ConfigError>, s: &mut Section<'_>) -> Pressure {
    let int = |s: &mut Section<'_>, errors: &mut Vec<ConfigError>, key, min, max, default: u32| {
        s.integer(errors, key, min, max)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(default)
    };
    let disk_path = s.string(errors, "disk_path").map(PathBuf::from);
    if disk_path.as_ref().is_some_and(|p| !p.is_absolute()) {
        errors.push(s.error("disk_path", "must be an absolute path"));
    }
    Pressure {
        alert_load: int(s, errors, "alert_load", 1, 10_000, 150),
        ready_load: int(s, errors, "ready_load", 1, 10_000, 80),
        min_free_memory: int(s, errors, "min_free_memory", 0, 100, 20),
        min_free_disk_gb: s
            .integer(errors, "min_free_disk_gb", 0, 1_000_000)
            .and_then(|n| u64::try_from(n).ok())
            .unwrap_or(50),
        disk_path,
        max_sessions: int(s, errors, "max_sessions", 0, 1000, 20),
    }
}

#[cfg(test)]
mod tests {
    use crate::config::{Work, parse};

    #[test]
    fn owner_kinds_parse_with_defaults() {
        let text = r#"
[jobs.gh]
kind = "github"
repos = [{ repo = "Me/Tool", authors = ["me"] }]
bots = ["ci-bot"]

[jobs.q]
kind = "quota"

[jobs.b]
kind = "backup"
dir = "/srv/state"
name = "Backup"
email = "b@example.org"
patterns = ["sec[r]et-[0-9]+"]

[jobs.h]
kind = "health"
files = [{ path = "/tmp/note.md", max_age = "6h" }]

[jobs.mail]
kind = "items"
argv = ["/usr/local/bin/new-mail"]

[jobs.host]
kind = "pressure"
"#;
        let c = parse(text).unwrap();
        let Work::Owner(super::Owner::Github(g)) = &c.jobs["gh"].work else {
            panic!("github kind");
        };
        assert_eq!(g.repos[0].repo, "me/tool");
        assert!(g.notifications);
        assert_eq!(
            c.jobs["gh"].trigger.every(),
            Some(std::time::Duration::from_mins(3))
        );
        let Work::Owner(super::Owner::Quota(q)) = &c.jobs["q"].work else {
            panic!("quota kind");
        };
        assert_eq!((q.levels.as_slice(), q.hold), (&[70, 85, 95][..], 85));
        assert_eq!(c.jobs["b"].kind(), "backup");
        assert_eq!(
            c.jobs["host"].trigger.every(),
            Some(std::time::Duration::from_secs(60))
        );
    }

    #[test]
    fn owner_settings_are_validated_with_paths() {
        let errs = |t: &str| -> Vec<String> {
            parse(t)
                .unwrap_err()
                .into_iter()
                .map(|e| e.to_string())
                .collect()
        };
        assert_eq!(
            errs("[jobs.q]\nkind = \"quota\"\nevery = \"1m\"\n"),
            ["jobs.q.every: must be between 120s and 86400s"]
        );
        assert_eq!(
            errs("[jobs.g]\nkind = \"github\"\n"),
            ["jobs.g.repos: required"]
        );
        assert_eq!(
            errs("[jobs.g]\nkind = \"github\"\nrepos = [{ repo = \"nope\" }]\n"),
            ["jobs.g.repos[0].repo: expected owner/name"]
        );
        let bad = errs(
            "[jobs.b]\nkind = \"backup\"\ndir = \"rel\"\nname = \"n\"\nemail = \"e\"\npatterns = [\"(\"]\n",
        );
        assert!(
            bad.contains(&"jobs.b.dir: must be an absolute path".to_string()),
            "{bad:?}"
        );
        assert!(
            bad.iter()
                .any(|e| e.starts_with("jobs.b.patterns[0]: not a valid pattern")),
            "{bad:?}"
        );
        assert_eq!(
            errs("[jobs.h]\nkind = \"health\"\nstrikes = 9\n"),
            ["jobs.h.strikes: must be between 1 and 5"]
        );
        assert_eq!(
            errs("[jobs.h]\nkind = \"health\"\ntemplates = { \"run.done\" = \"x\" }\n")[0]
                .split(" (")
                .next()
                .unwrap(),
            "jobs.h.templates.run.done: not an event of this job"
        );
    }
}
