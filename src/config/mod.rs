//! The owner's TOML configuration (WS req. 79-82, OJ req. 2): `[hub]`, `[omp]`, `[discord]`,
//! `[outgoing]`, `[limits]`, `[health]`, `[destinations.<name>]` and `[jobs.<name>]`. Validation is
//! strict and every error names its setting path. A missing file means built-in defaults; an invalid
//! file is never applied.

pub(crate) mod owner;
pub(crate) mod session;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use toml::{Table, Value};

use crate::jobs::cron::{self, Cron};
use crate::jobs::event::{EventKind, Template, Templates};
use crate::log::Level;
use crate::relay::discord::{ChannelRef, Snowflake};
use crate::relay::http::Base;
use crate::relay::text::Checks;
pub(crate) use owner::Owner;

/// Largest config file accepted.
const FILE_MAX: u64 = 1024 * 1024;
/// The built-in destination that keeps events locally (store and event log only).
pub(crate) const LOG_DESTINATION: &str = "log";
const PROBE_MIN: Duration = Duration::from_secs(30);
const DAY: Duration = Duration::from_hours(24);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Account-limits cache period: default and minimum (WS req. 38).
const LIMITS_CACHE: Duration = Duration::from_mins(5);
const LIMITS_CACHE_MIN: Duration = Duration::from_mins(1);

/// A validated configuration.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) hub: HubSettings,
    pub(crate) omp: OmpSettings,
    pub(crate) discord: DiscordSettings,
    pub(crate) outgoing: Checks,
    pub(crate) limits: LimitsSettings,
    pub(crate) health: HealthSettings,
    pub(crate) sessions: session::SessionSettings,
    pub(crate) lead: Option<session::LeadSettings>,
    pub(crate) fallback: session::FallbackSettings,
    pub(crate) destinations: BTreeMap<String, Destination>,
    pub(crate) jobs: BTreeMap<String, Arc<Job>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hub: HubSettings {
                log_level: Level::Info,
            },
            omp: OmpSettings { path: "omp".into() },
            discord: DiscordSettings::default(),
            outgoing: Checks::default(),
            limits: LimitsSettings {
                enabled: false,
                cache: LIMITS_CACHE,
                warn_percent: None,
            },
            health: HealthSettings::default(),
            sessions: session::SessionSettings::default(),
            lead: None,
            fallback: session::FallbackSettings::default(),
            destinations: BTreeMap::new(),
            jobs: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct HubSettings {
    pub(crate) log_level: Level,
}

/// `[omp]`: the executable, an absolute path or a name looked up on `PATH` (HOST req. 17).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OmpSettings {
    pub(crate) path: String,
}

/// `[discord]`: where the bot token is, the guild for channel names, and the loopback-only test
/// override of the API base (DISC-OUT req. 10).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DiscordSettings {
    /// Default `<config dir>/secrets/discord-token`.
    pub(crate) token_file: Option<PathBuf>,
    pub(crate) guild: Option<Snowflake>,
    pub(crate) api_base: Option<Base>,
}

/// `[limits]`: the account-limits view (WS req. 37-38, 81).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LimitsSettings {
    pub(crate) enabled: bool,
    pub(crate) cache: Duration,
    pub(crate) warn_percent: Option<u8>,
}

/// `[health]`: check names left out of the report's status (WS req. 78).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct HealthSettings {
    pub(crate) ignore: Vec<String>,
}

/// Where events can go besides the store and the event log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Destination {
    /// A local command run with the event as JSON in `CATALY_EVENT`.
    Command { argv: Vec<String> },
    /// A Discord channel or thread.
    Discord { channel: ChannelRef },
}

/// One configured job.
#[derive(Debug, Clone)]
pub(crate) struct Job {
    pub(crate) name: String,
    pub(crate) enabled: bool,
    pub(crate) dry_run: bool,
    pub(crate) destinations: Vec<String>,
    pub(crate) templates: Templates,
    /// The command's time limit, or the owner kind's per-tick deadline.
    pub(crate) timeout: Duration,
    pub(crate) trigger: Trigger,
    pub(crate) work: Work,
}

/// What a job does when it runs.
#[derive(Debug, Clone)]
pub(crate) enum Work {
    /// Timed and condition jobs: one owner command.
    Command { argv: Vec<String>, cwd: PathBuf },
    /// A built-in owner job kind.
    Owner(Owner),
}

impl Job {
    /// The kind name shown and stored: `cron`, `once`, `probe`, or the owner kind.
    pub(crate) fn kind(&self) -> &'static str {
        match &self.work {
            Work::Command { .. } => self.trigger.kind(),
            Work::Owner(owner) => owner.name(),
        }
    }
}

/// What makes a job run.
#[derive(Debug, Clone)]
pub(crate) enum Trigger {
    Cron {
        cron: Cron,
        expr: String,
        zone: TimeZone,
        zone_name: String,
    },
    Once {
        at: Timestamp,
    },
    Probe {
        every: Duration,
        fire: Fire,
        skip_identical: bool,
    },
    /// Owner job kinds: a tick every interval, the first at registration.
    Every {
        every: Duration,
    },
}

/// When a condition job fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fire {
    /// On the first success after a failure (a new job starts primed).
    Change,
    /// On every success.
    Every,
}

impl Trigger {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Cron { .. } => "cron",
            Self::Once { .. } => "once",
            Self::Probe { .. } => "probe",
            Self::Every { .. } => "every",
        }
    }

    /// The interval of interval-driven jobs (condition jobs and owner kinds).
    pub(crate) fn every(&self) -> Option<Duration> {
        match self {
            Self::Probe { every, .. } | Self::Every { every } => Some(*every),
            Self::Cron { .. } | Self::Once { .. } => None,
        }
    }

    /// Canonical text; a change of it restarts the job's schedule.
    pub(crate) fn canonical(&self) -> String {
        match self {
            Self::Cron {
                expr, zone_name, ..
            } => format!("cron {expr} {zone_name}"),
            Self::Once { at } => format!("once {at}"),
            Self::Probe { every, .. } => format!("probe every {}s", every.as_secs()),
            Self::Every { every } => format!("every {}s", every.as_secs()),
        }
    }
}

/// One validation error with the setting path it concerns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigError {
    pub(crate) path: String,
    pub(crate) message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// All errors of one file, joined for display.
pub(crate) fn describe(errors: &[ConfigError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// SHA-256 (hex) of the config file's bytes; `None` when it is missing or unreadable.
pub(crate) fn file_digest(path: &Path) -> Option<String> {
    let bytes = crate::sys::fs::read_private(path, FILE_MAX).ok()??;
    Some(sha256_hex(&bytes))
}

/// Lower-case hex SHA-256.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    crate::sys::hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

/// Reads and validates the file; a missing file yields the defaults.
pub(crate) fn load(path: &Path) -> Result<Config, Vec<ConfigError>> {
    let file_error = |message: String| {
        vec![ConfigError {
            path: path.display().to_string(),
            message,
        }]
    };
    match crate::sys::fs::read_private(path, FILE_MAX) {
        Ok(None) => Ok(Config::default()),
        Ok(Some(bytes)) => {
            let text = String::from_utf8(bytes).map_err(|_| file_error("not UTF-8".into()))?;
            parse(&text)
        }
        Err(e) => Err(file_error(e.to_string())),
    }
}

/// Validates TOML text.
pub(crate) fn parse(text: &str) -> Result<Config, Vec<ConfigError>> {
    let table: Table = toml::from_str(text).map_err(|e| {
        vec![ConfigError {
            path: "(file)".into(),
            message: e.to_string().replace('\n', " ").trim().to_string(),
        }]
    })?;
    let mut errors = Vec::new();
    let mut root = Section::new(String::new(), &table);
    let mut config = Config::default();
    if let Some(mut hub) = root.table(&mut errors, "hub") {
        if let Some(level) = hub.string(&mut errors, "log_level") {
            match Level::parse(level) {
                Some(l) => config.hub.log_level = l,
                None => {
                    errors.push(hub.error("log_level", "expected one of error, warn, info, debug"));
                }
            }
        }
        hub.finish(&mut errors);
    }
    sections(&mut errors, &mut root, &mut config);
    if let Some(mut dests) = root.table(&mut errors, "destinations") {
        for (name, value) in dests.entries() {
            let path = dests.child(name);
            if let Some(d) = destination(&mut errors, &path, name, value, &config.discord) {
                config.destinations.insert(name.clone(), d);
            }
        }
    }
    if let Some(mut jobs) = root.table(&mut errors, "jobs") {
        for (name, value) in jobs.entries() {
            let path = jobs.child(name);
            if let Some(j) = job(&mut errors, &path, name, value, &config.destinations) {
                config.jobs.insert(name.clone(), Arc::new(j));
            }
        }
    }
    root.finish(&mut errors);
    if errors.is_empty() {
        Ok(config)
    } else {
        Err(errors)
    }
}

/// Job and destination names: 1 to 128 of letters, digits, dot, at sign, underscore or hyphen,
/// starting with a letter or digit (WS req. 50).
pub(crate) fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.len() <= 128
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '@' | '_' | '-'))
}

fn destination(
    errors: &mut Vec<ConfigError>,
    path: &str,
    name: &str,
    value: &Value,
    discord: &DiscordSettings,
) -> Option<Destination> {
    if !valid_name(name) || name == LOG_DESTINATION {
        errors.push(ConfigError {
            path: path.into(),
            message: "invalid or reserved destination name".into(),
        });
        return None;
    }
    let Some(table) = value.as_table() else {
        errors.push(ConfigError {
            path: path.into(),
            message: "expected a table".into(),
        });
        return None;
    };
    let mut s = Section::new(path.to_string(), table);
    let kind = s.required_string(errors, "kind");
    let dest = match kind {
        Some("command") => s
            .argv(errors, "argv")
            .map(|argv| Destination::Command { argv }),
        Some("discord") => match s.required_string(errors, "channel").map(ChannelRef::parse) {
            Some(Some(ChannelRef::Name(_))) if discord.guild.is_none() => {
                errors.push(s.error(
                    "channel",
                    "a channel name needs [discord] guild; or give the channel ID",
                ));
                None
            }
            Some(Some(channel)) => Some(Destination::Discord { channel }),
            Some(None) => {
                errors.push(s.error(
                    "channel",
                    "expected a channel or thread ID, or a channel name",
                ));
                None
            }
            None => None,
        },
        Some(other) => {
            errors.push(s.error(
                "kind",
                &format!("unknown destination kind '{other}' (this build knows: command, discord)"),
            ));
            None
        }
        None => None,
    };
    s.finish(errors);
    dest
}

/// `[omp]`, `[discord]`, `[outgoing]`, `[limits]` and `[health]`.
fn sections(errors: &mut Vec<ConfigError>, root: &mut Section<'_>, config: &mut Config) {
    if let Some(mut omp) = root.table(errors, "omp") {
        if let Some(path) = omp.string(errors, "path") {
            if path.is_empty() || (path.contains('/') && !Path::new(path).is_absolute()) {
                errors.push(omp.error("path", "expected an absolute path or a command name"));
            } else {
                config.omp.path = path.to_string();
            }
        }
        omp.finish(errors);
    }
    if let Some(mut d) = root.table(errors, "discord") {
        if let Some(file) = d.string(errors, "token_file") {
            if Path::new(file).is_absolute() {
                config.discord.token_file = Some(PathBuf::from(file));
            } else {
                errors.push(d.error("token_file", "must be an absolute path"));
            }
        }
        if let Some(guild) = d.string(errors, "guild") {
            config.discord.guild = Snowflake::parse(guild);
            if config.discord.guild.is_none() {
                errors.push(d.error("guild", "expected a Discord ID"));
            }
        }
        if let Some(base) = d.string(errors, "api_base") {
            match Base::parse(base) {
                Ok(b) if b.is_loopback() => config.discord.api_base = Some(b),
                Ok(_) => {
                    errors.push(d.error("api_base", "the test override must point at localhost"));
                }
                Err(e) => errors.push(d.error("api_base", &e)),
            }
        }
        d.finish(errors);
    }
    if let Some(mut o) = root.table(errors, "outgoing") {
        config.outgoing.placeholders = o.strings(errors, "placeholder_words").unwrap_or_default();
        for (i, c) in o
            .strings(errors, "banned_characters")
            .unwrap_or_default()
            .iter()
            .enumerate()
        {
            let mut chars = c.chars();
            match (chars.next(), chars.next()) {
                (Some(one), None) => config.outgoing.banned.push(one),
                _ => errors.push(ConfigError {
                    path: format!("{}[{i}]", o.child("banned_characters")),
                    message: "expected exactly one character".into(),
                }),
            }
        }
        o.finish(errors);
    }
    if let Some(mut l) = root.table(errors, "limits") {
        config.limits.enabled = l.bool(errors, "enabled").unwrap_or(false);
        if let Some(cache) = l.duration(errors, "cache", LIMITS_CACHE_MIN, DAY) {
            config.limits.cache = cache;
        }
        config.limits.warn_percent = l
            .integer(errors, "warn_percent", 1, 100)
            .and_then(|p| u8::try_from(p).ok());
        l.finish(errors);
    }
    if let Some(mut h) = root.table(errors, "health") {
        config.health.ignore = h.strings(errors, "ignore").unwrap_or_default();
        h.finish(errors);
    }
    session::parse(
        errors,
        root,
        &mut config.sessions,
        &mut config.lead,
        &mut config.fallback,
    );
}

fn job(
    errors: &mut Vec<ConfigError>,
    path: &str,
    name: &str,
    value: &Value,
    destinations: &BTreeMap<String, Destination>,
) -> Option<Job> {
    if !valid_name(name) {
        errors.push(ConfigError {
            path: path.into(),
            message: "job names are 1-128 letters, digits, '.', '@', '_' or '-', starting with a letter or digit"
                .into(),
        });
        return None;
    }
    let Some(table) = value.as_table() else {
        errors.push(ConfigError {
            path: path.into(),
            message: "expected a table".into(),
        });
        return None;
    };
    let before = errors.len();
    let mut s = Section::new(path.to_string(), table);
    let kind = s.required_string(errors, "kind");
    let (trigger, work, default_timeout) = match kind {
        Some(k @ ("cron" | "once" | "probe")) => {
            let trigger = match k {
                "cron" => cron_trigger(errors, &mut s),
                "once" => once_trigger(errors, &mut s),
                _ => probe_trigger(errors, &mut s),
            };
            let argv = s.argv(errors, "argv");
            let cwd = s.required_string(errors, "cwd").map(PathBuf::from);
            if cwd.as_ref().is_some_and(|c| !c.is_absolute()) {
                errors.push(s.error("cwd", "must be an absolute path"));
            }
            let work = argv.zip(cwd).map(|(argv, cwd)| Work::Command { argv, cwd });
            (trigger, work, DEFAULT_TIMEOUT)
        }
        Some(other) => match owner::parse(other, errors, &mut s) {
            Some(parsed) => (
                Some(Trigger::Every {
                    every: parsed.every,
                }),
                Some(Work::Owner(parsed.owner)),
                parsed.timeout,
            ),
            None if owner::is_kind(other) => (None, None, DEFAULT_TIMEOUT),
            None => {
                errors.push(s.error(
                    "kind",
                    &format!(
                        "unknown job kind '{other}' (this build knows: cron, once, probe, {})",
                        owner::KIND_NAMES.join(", ")
                    ),
                ));
                (None, None, DEFAULT_TIMEOUT)
            }
        },
        None => (None, None, DEFAULT_TIMEOUT),
    };
    let enabled = s.bool(errors, "enabled").unwrap_or(true);
    let dry_run = s.bool(errors, "dry_run").unwrap_or(false);
    let timeout = s
        .duration(errors, "timeout", Duration::from_secs(1), DAY)
        .unwrap_or(default_timeout);
    let dests = s.strings(errors, "destinations").unwrap_or_default();
    for (i, d) in dests.iter().enumerate() {
        if d != LOG_DESTINATION && !destinations.contains_key(d) {
            errors.push(ConfigError {
                path: format!("{path}.destinations[{i}]"),
                message: format!("unknown destination '{d}'"),
            });
        }
    }
    let templates = match (&trigger, &work) {
        (Some(t), Some(w)) => templates(errors, &mut s, allowed_events(t, w)),
        _ => Vec::new(),
    };
    s.finish(errors);
    if errors.len() > before {
        return None;
    }
    Some(Job {
        name: name.to_string(),
        enabled,
        dry_run,
        destinations: dests.into_iter().filter(|d| d != LOG_DESTINATION).collect(),
        templates,
        timeout,
        trigger: trigger?,
        work: work?,
    })
}

/// Event kinds a job may emit, so templates for any other kind are refused.
fn allowed_events(trigger: &Trigger, work: &Work) -> &'static [EventKind] {
    match (trigger, work) {
        (_, Work::Owner(owner)) => owner.events(),
        (Trigger::Probe { .. }, _) => &[
            EventKind::ProbeFired,
            EventKind::ProbeError,
            EventKind::ProbeRecovered,
            EventKind::RunBroken,
        ],
        _ => &[
            EventKind::RunDone,
            EventKind::RunSkipped,
            EventKind::RunBroken,
        ],
    }
}

fn cron_trigger(errors: &mut Vec<ConfigError>, s: &mut Section<'_>) -> Option<Trigger> {
    let expr = s.required_string(errors, "cron");
    let zone_name = s.required_string(errors, "zone");
    let cron = expr.and_then(|e| {
        cron::parse(e)
            .map_err(|m| errors.push(s.error("cron", &m)))
            .ok()
    });
    let zone = zone_name.and_then(|z| {
        TimeZone::get(z)
            .map_err(|_| errors.push(s.error("zone", &format!("unknown IANA time zone '{z}'"))))
            .ok()
    });
    let (cron, zone, expr, zone_name) = (cron?, zone?, expr?, zone_name?);
    if cron.next_after(&zone, Timestamp::now()).is_none() {
        errors.push(s.error("cron", "never fires"));
        return None;
    }
    Some(Trigger::Cron {
        cron,
        expr: expr.to_string(),
        zone,
        zone_name: zone_name.to_string(),
    })
}

fn once_trigger(errors: &mut Vec<ConfigError>, s: &mut Section<'_>) -> Option<Trigger> {
    let text = s.required_string(errors, "at")?;
    let has_seconds = text
        .split_once('T')
        .and_then(|(_, time)| time.get(..8))
        .is_some_and(|hms| {
            hms.as_bytes().get(2) == Some(&b':') && hms.as_bytes().get(5) == Some(&b':')
        });
    match text.parse::<Timestamp>() {
        Ok(at) if has_seconds => Some(Trigger::Once { at }),
        _ => {
            errors.push(s.error(
                "at",
                "expected an instant with seconds and an offset, like 2026-12-01T09:00:00Z",
            ));
            None
        }
    }
}

fn probe_trigger(errors: &mut Vec<ConfigError>, s: &mut Section<'_>) -> Option<Trigger> {
    let every = s.duration(errors, "every", PROBE_MIN, DAY);
    if every.is_none() && s.get("every").is_none() {
        errors.push(s.error("every", "required"));
    }
    let fire = match s.string(errors, "fire") {
        None | Some("change") => Fire::Change,
        Some("every") => Fire::Every,
        Some(_) => {
            errors.push(s.error("fire", "expected 'change' or 'every'"));
            Fire::Change
        }
    };
    let skip_identical = s.bool(errors, "skip_identical").unwrap_or(false);
    Some(Trigger::Probe {
        every: every?,
        fire,
        skip_identical,
    })
}

fn templates(
    errors: &mut Vec<ConfigError>,
    s: &mut Section<'_>,
    allowed: &[EventKind],
) -> Templates {
    let Some(mut t) = s.table(errors, "templates") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (key, value) in t.entries() {
        let path = t.child(key);
        let Some(kind) = EventKind::from_name(key).filter(|k| allowed.contains(k)) else {
            let names: Vec<&str> = allowed.iter().map(|k| k.name()).collect();
            errors.push(ConfigError {
                path,
                message: format!("not an event of this job (known: {})", names.join(", ")),
            });
            continue;
        };
        match value.as_str().map(|text| Template::parse(text, kind)) {
            Some(Ok(template)) => out.push((kind, template)),
            Some(Err(message)) => errors.push(ConfigError { path, message }),
            None => errors.push(ConfigError {
                path,
                message: "expected a string".into(),
            }),
        }
    }
    out
}

/// Parses `30s`, `5m`, `2h`, `1d` and sums like `1h30m`.
pub(crate) fn parse_duration(text: &str) -> Option<Duration> {
    let mut total: u64 = 0;
    let mut number: Option<u64> = None;
    for c in text.chars() {
        if let Some(d) = c.to_digit(10) {
            number = Some(
                number
                    .unwrap_or(0)
                    .checked_mul(10)?
                    .checked_add(u64::from(d))?,
            );
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            _ => return None,
        };
        total = total.checked_add(number.take()?.checked_mul(unit)?)?;
    }
    if number.is_some() || text.is_empty() {
        return None;
    }
    Some(Duration::from_secs(total))
}

/// A table being read, tracking which keys were used so the rest can be refused.
struct Section<'a> {
    path: String,
    table: &'a Table,
    used: Vec<&'a str>,
}

impl<'a> Section<'a> {
    fn new(path: String, table: &'a Table) -> Self {
        Self {
            path,
            table,
            used: Vec::new(),
        }
    }

    fn child(&self, key: &str) -> String {
        if self.path.is_empty() {
            key.to_string()
        } else {
            format!("{}.{key}", self.path)
        }
    }

    fn error(&self, key: &str, message: &str) -> ConfigError {
        ConfigError {
            path: self.child(key),
            message: message.to_string(),
        }
    }

    fn get(&mut self, key: &str) -> Option<&'a Value> {
        let (k, v) = self.table.get_key_value(key)?;
        self.used.push(k.as_str());
        Some(v)
    }

    /// Every entry, all counted as used.
    fn entries(&mut self) -> Vec<(&'a String, &'a Value)> {
        let all: Vec<_> = self.table.iter().collect();
        self.used.extend(all.iter().map(|(k, _)| k.as_str()));
        all
    }

    fn typed<T>(
        &mut self,
        errors: &mut Vec<ConfigError>,
        key: &str,
        what: &str,
        f: impl Fn(&'a Value) -> Option<T>,
    ) -> Option<T> {
        let value = self.get(key)?;
        let out = f(value);
        if out.is_none() {
            errors.push(self.error(key, &format!("expected {what}")));
        }
        out
    }

    fn string(&mut self, errors: &mut Vec<ConfigError>, key: &str) -> Option<&'a str> {
        self.typed(errors, key, "a string", Value::as_str)
    }

    fn required_string(&mut self, errors: &mut Vec<ConfigError>, key: &str) -> Option<&'a str> {
        if self.table.get(key).is_none() {
            errors.push(self.error(key, "required"));
            return None;
        }
        self.string(errors, key)
    }

    fn bool(&mut self, errors: &mut Vec<ConfigError>, key: &str) -> Option<bool> {
        self.typed(errors, key, "true or false", Value::as_bool)
    }

    fn integer(
        &mut self,
        errors: &mut Vec<ConfigError>,
        key: &str,
        min: i64,
        max: i64,
    ) -> Option<i64> {
        let n = self.typed(errors, key, "a whole number", Value::as_integer)?;
        if (min..=max).contains(&n) {
            Some(n)
        } else {
            errors.push(self.error(key, &format!("must be between {min} and {max}")));
            None
        }
    }

    /// An array of tables, each as its own section with path `key[i]`.
    fn tables(&mut self, errors: &mut Vec<ConfigError>, key: &str) -> Option<Vec<Section<'a>>> {
        let path = self.child(key);
        let items = self.typed(errors, key, "a list of tables", Value::as_array)?;
        let mut out = Vec::with_capacity(items.len());
        for (i, item) in items.iter().enumerate() {
            match item.as_table() {
                Some(t) => out.push(Section::new(format!("{path}[{i}]"), t)),
                None => errors.push(ConfigError {
                    path: format!("{path}[{i}]"),
                    message: "expected a table".into(),
                }),
            }
        }
        Some(out)
    }

    fn strings(&mut self, errors: &mut Vec<ConfigError>, key: &str) -> Option<Vec<String>> {
        self.typed(errors, key, "a list of strings", |v| {
            v.as_array()?
                .iter()
                .map(|i| i.as_str().map(str::to_string))
                .collect()
        })
    }

    fn argv(&mut self, errors: &mut Vec<ConfigError>, key: &str) -> Option<Vec<String>> {
        if self.table.get(key).is_none() {
            errors.push(self.error(key, "required"));
            return None;
        }
        let argv = self.strings(errors, key)?;
        if argv.is_empty() || argv.iter().any(String::is_empty) {
            errors.push(self.error(key, "expected a non-empty list of non-empty strings"));
            return None;
        }
        Some(argv)
    }

    fn duration(
        &mut self,
        errors: &mut Vec<ConfigError>,
        key: &str,
        min: Duration,
        max: Duration,
    ) -> Option<Duration> {
        let text = self.typed(
            errors,
            key,
            "a duration string like \"30s\", \"5m\" or \"1h30m\"",
            Value::as_str,
        )?;
        match parse_duration(text) {
            Some(d) if d >= min && d <= max => Some(d),
            Some(_) => {
                errors.push(self.error(
                    key,
                    &format!("must be between {}s and {}s", min.as_secs(), max.as_secs()),
                ));
                None
            }
            None => {
                errors.push(self.error(
                    key,
                    "expected a duration string like \"30s\", \"5m\" or \"1h30m\"",
                ));
                None
            }
        }
    }

    fn table(&mut self, errors: &mut Vec<ConfigError>, key: &str) -> Option<Section<'a>> {
        let path = self.child(key);
        let table = self.typed(errors, key, "a table", Value::as_table)?;
        Some(Section::new(path, table))
    }

    fn finish(self, errors: &mut Vec<ConfigError>) {
        for key in self.table.keys() {
            if !self.used.contains(&key.as_str()) {
                errors.push(self.error(key, "unknown setting"));
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const VALID: &str = r#"
[hub]
log_level = "debug"

[destinations.phone]
kind = "command"
argv = ["/bin/true"]

[jobs.nightly]
kind = "cron"
cron = "0 3 * * *"
zone = "Europe/Berlin"
argv = ["/bin/echo", "hi"]
cwd = "/tmp"
timeout = "10m"
destinations = ["phone", "log"]
templates = { "run.done" = "{job} done: {outcome}" }

[jobs.reminder]
kind = "once"
at = "2030-01-01T09:00:00Z"
argv = ["/bin/true"]
cwd = "/"

[jobs.site]
kind = "probe"
every = "1m"
fire = "every"
skip_identical = true
argv = ["/bin/true"]
cwd = "/"
dry_run = true
"#;

    fn errors(text: &str) -> Vec<String> {
        parse(text)
            .unwrap_err()
            .into_iter()
            .map(|e| e.to_string())
            .collect()
    }

    #[test]
    fn valid_config_parses() {
        let c = parse(VALID).unwrap();
        assert_eq!(c.hub.log_level, Level::Debug);
        assert_eq!(c.jobs.len(), 3);
        let nightly = &c.jobs["nightly"];
        assert_eq!(nightly.destinations, ["phone"]);
        assert_eq!(nightly.timeout, Duration::from_secs(600));
        assert_eq!(nightly.templates.len(), 1);
        assert_eq!(nightly.trigger.canonical(), "cron 0 3 * * * Europe/Berlin");
        assert!(matches!(
            c.jobs["site"].trigger,
            Trigger::Probe {
                fire: Fire::Every,
                skip_identical: true,
                ..
            }
        ));
        assert!(c.jobs["site"].dry_run);
        assert_eq!(c.jobs["reminder"].timeout, DEFAULT_TIMEOUT);
        assert_eq!(parse("").unwrap().jobs.len(), 0);
    }

    #[test]
    fn errors_name_the_setting_path() {
        let probe = "[jobs.p]\nkind = \"probe\"\nargv = [\"/bin/true\"]\ncwd = \"/\"\n";
        assert_eq!(
            errors(&format!("{probe}every = \"10s\"\n")),
            ["jobs.p.every: must be between 30s and 86400s"]
        );
        assert_eq!(
            errors(&format!("{probe}every = \"1m\"\nevry = 1\n")),
            ["jobs.p.evry: unknown setting"]
        );
        assert_eq!(
            errors(&format!(
                "{probe}every = \"1m\"\ndestinations = [\"pager\"]\n"
            )),
            ["jobs.p.destinations[0]: unknown destination 'pager'"]
        );
        let tpl = errors(&format!(
            "{probe}every = \"1m\"\ntemplates = {{ \"probe.fired\" = \"{{job}} {{nope}}\" }}\n"
        ));
        assert_eq!(tpl.len(), 1);
        assert!(
            tpl[0].starts_with("jobs.p.templates.probe.fired: unknown field '{nope}'"),
            "{tpl:?}"
        );
        assert_eq!(
            errors("[hub]\nlog_levle = \"x\"\n"),
            ["hub.log_levle: unknown setting"]
        );
        assert_eq!(errors("color = 1\n"), ["color: unknown setting"]);
        let cron = errors(
            "[jobs.c]\nkind = \"cron\"\ncron = \"* * ? * *\"\nzone = \"Mars/Base\"\nargv = [\"x\"]\ncwd = \"rel\"\n",
        );
        assert!(
            cron.contains(&"jobs.c.cron: day of month: '?' is not supported, use '*'".to_string()),
            "{cron:?}"
        );
        assert!(cron.contains(&"jobs.c.zone: unknown IANA time zone 'Mars/Base'".to_string()));
        assert!(cron.contains(&"jobs.c.cwd: must be an absolute path".to_string()));
        let once = errors(
            "[jobs.o]\nkind = \"once\"\nat = \"2030-01-01T09:00Z\"\nargv = [\"x\"]\ncwd = \"/\"\n",
        );
        assert!(once[0].starts_with("jobs.o.at: expected an instant with seconds"));
        assert!(
            errors("[destinations.log]\nkind = \"command\"\nargv = [\"x\"]\n")[0]
                .starts_with("destinations.log")
        );
        assert_eq!(
            errors("[destinations.d]\nkind = \"discord\"\n"),
            ["destinations.d.channel: required"]
        );
        assert!(
            errors("[destinations.d]\nkind = \"slack\"\n")[0].contains("unknown destination kind")
        );
        assert!(errors("[jobs.\"bad name\"]\nkind = \"once\"\n")[0].starts_with("jobs.bad name"));
        assert!(errors("not toml =")[0].starts_with("(file)"));
    }

    #[test]
    fn durations_and_names() {
        assert_eq!(parse_duration("1h30m"), Some(Duration::from_mins(90)));
        assert_eq!(parse_duration("45s"), Some(Duration::from_secs(45)));
        assert_eq!(parse_duration("2d"), Some(Duration::from_hours(48)));
        for bad in ["", "5", "m", "5x", "1.5h"] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
        assert!(valid_name("a.b@c_d-e"));
        assert!(
            !valid_name("-a")
                && !valid_name("")
                && !valid_name(&"a".repeat(129))
                && !valid_name("a b")
        );
    }
}
