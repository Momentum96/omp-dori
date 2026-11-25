//! `cataly setup` (COORD-51, COORD-52, IR-09): writes the settings it manages into the config file
//! without clobbering a hand edit. It snapshots the file (identity and SHA-256), previews the change,
//! re-checks the snapshot just before an atomic replace (or a create that never overwrites), reads
//! the file back, and reports for each managed setting where its effective value comes from.
//! Unknown keys and comments are kept: only the managed lines change.

use std::fmt::Write as _;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use serde_json::{Value, json};

use super::args::Parsed;
use super::{CALL_TIMEOUT, Failure, out};
use crate::config::{self, sha256_hex};
use crate::omp::transcript::model_selector;
use crate::proto::{Status, method};
use crate::sys::fs::{create_atomic, read_private, write_atomic};
use crate::sys::paths::Paths;

/// The settings setup manages: (section, key, flag).
const MANAGED: [(&str, &str, &str); 14] = [
    ("hub", "log_level", "--log-level"),
    ("omp", "path", "--omp"),
    ("discord", "token_file", "--discord-token-file"),
    ("discord", "owner", "--discord-owner"),
    ("discord", "channels", "--discord-channels"),
    ("telegram", "token_file", "--telegram-token-file"),
    ("telegram", "owner", "--telegram-owner"),
    ("telegram", "group", "--telegram-group"),
    ("telegram", "topic", "--telegram-topic"),
    ("slack", "token_file", "--slack-token-file"),
    ("slack", "role", "--slack-role"),
    ("slack", "owner", "--slack-owner"),
    ("fallback", "on", "--fallback-on"),
    ("fallback.chains", "default", "--fallback-chain"),
];
const CONFIG_MAX: u64 = 1024 * 1024;

/// Device, inode, mode, owner, group, size and the modification and change times.
type Identity = (u64, u64, u32, u32, u32, u64, i64, i64, i64, i64);

/// What identifies the file: any change of it means someone else wrote meanwhile.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    identity: Option<Identity>,
    bytes: Option<Vec<u8>>,
}

impl Snapshot {
    fn digest(&self) -> String {
        self.bytes
            .as_deref()
            .map_or_else(|| "none".to_string(), sha256_hex)
    }
}

fn snapshot(path: &Path) -> Result<Snapshot, String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Snapshot {
                identity: None,
                bytes: None,
            });
        }
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if !meta.is_file() {
        return Err(format!(
            "{} is not a regular file; setup refuses it",
            path.display()
        ));
    }
    if meta.uid() != crate::sys::own_uid() {
        return Err(format!(
            "{} is owned by another user; setup refuses it",
            path.display()
        ));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(format!(
            "{} is writable by group or others; run chmod 600 on it first",
            path.display()
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > CONFIG_MAX {
        return Err(format!("{} is larger than 1 MiB", path.display()));
    }
    Ok(Snapshot {
        identity: Some((
            meta.dev(),
            meta.ino(),
            meta.mode(),
            meta.uid(),
            meta.gid(),
            meta.size(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec(),
        )),
        bytes: Some(bytes),
    })
}

/// Why a managed owner, room or fallback value is not valid, if it is not.
fn invalid(section: &str, key: &str, v: &str) -> Option<&'static str> {
    let int = v.parse::<i64>().ok();
    let ok = match (section, key) {
        ("discord", "owner") => crate::relay::discord::Snowflake::parse(v).is_some(),
        ("telegram", "owner") => int.is_some_and(|n| n > 0),
        ("telegram", "group") => int.is_some_and(|n| n < 0),
        ("telegram", "topic") => int.is_some_and(|n| n > 0),
        ("slack", "owner") => v.starts_with(['U', 'W']) && crate::config::relays::slack_id(v),
        ("slack", "role") => matches!(v, "app" | "member"),
        ("fallback", _) => config::session::FALLBACK_ON.contains(&v),
        ("fallback.chains", _) => {
            !comma_list(v).is_empty() && comma_list(v).iter().all(|m| model_selector(m))
        }
        _ => true,
    };
    (!ok).then_some(match (section, key) {
        ("discord", _) => "expected the owner's Discord user ID",
        ("telegram", "owner") => "expected the owner's Telegram user ID (a positive number)",
        ("telegram", "group") => "expected the group's Telegram chat ID (a negative number)",
        ("telegram", _) => "expected the forum topic's ID (a positive number)",
        ("slack", "owner") => "expected the owner's Slack member ID, such as U0123ABCD",
        ("fallback", _) => "expected any, usage-limit or except-usage-limit",
        ("fallback.chains", _) => {
            "expected the models to fall back to, in order, as provider/id separated by commas"
        }
        _ => "expected app or member",
    })
}

/// The items of a comma-separated value, in order.
fn comma_list(v: &str) -> Vec<&str> {
    v.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// A TOML basic string.
fn toml_string(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Sets `key` in `[section]` to `value`, replacing its line or adding it; everything else is kept.
pub(crate) fn set_key(text: &str, section: &str, key: &str, value: &str) -> String {
    set_rendered(text, section, key, &toml_string(value))
}

/// [`set_key`] with a value already written as TOML.
pub(super) fn set_rendered(text: &str, section: &str, key: &str, rendered: &str) -> String {
    let line = format!("{key} = {rendered}");
    let header = format!("[{section}]");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let start = lines.iter().position(|l| l.trim() == header);
    let Some(start) = start else {
        let mut out = text.trim_end().to_string();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        let _ = write!(out, "{header}\n{line}\n");
        return out;
    };
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, l)| l.trim_start().starts_with('['))
        .map_or(lines.len(), |(i, _)| i);
    let existing = (start + 1..end).find(|i| {
        lines
            .get(*i)
            .and_then(|l| l.trim_start().strip_prefix(key))
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    });
    if let Some(i) = existing {
        if let Some(l) = lines.get_mut(i) {
            *l = line;
        }
    } else {
        let last = (start..end)
            .rev()
            .find(|i| lines.get(*i).is_some_and(|l| !l.trim().is_empty()))
            .unwrap_or(start);
        lines.insert(last + 1, line);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

const NEW_FILE: &str = "# Cataly configuration. Check it with `cataly config check`; apply it with\n\
                        # `cataly config reload`. Jobs and destinations go below.\n";

/// Where the effective value of each managed setting comes from.
fn sources(paths: &Paths, text: &str, hub: Option<&Status>, digest: &str) -> Vec<Value> {
    let parsed = config::parse(text).ok();
    let defaults = config::Config::default();
    let in_file = |section: &str, key: &str| {
        let table = toml::from_str::<toml::Table>(text).ok();
        let mut at = table.as_ref();
        for part in section.split('.') {
            at = at.and_then(|t| t.get(part)?.as_table());
        }
        at.and_then(|t| t.get(key)).is_some()
    };
    let effective = match hub {
        Some(s) if s.config_digest.as_deref() == Some(digest) => {
            "this file (the running hub applied it)"
        }
        Some(_) => {
            "this file, once applied: the running hub has a different version (run cataly config reload)"
        }
        None => "this file, when the hub starts (no hub is running)",
    };
    let mut out = Vec::new();
    let config = parsed.unwrap_or(defaults);
    for (section, key, _) in MANAGED {
        let from_file = in_file(section, key);
        let (value, note) = value_of(paths, &config, section, key);
        out.push(json!({
            "setting": format!("{section}.{key}"),
            "value": value,
            "source": if from_file { effective } else { "built-in default" },
            "note": note,
        }));
    }
    for (name, var, path) in [
        (
            "config file",
            "XDG_CONFIG_HOME",
            paths.config_file.display().to_string(),
        ),
        (
            "state directory",
            "XDG_STATE_HOME",
            paths.state_dir.display().to_string(),
        ),
    ] {
        let source = if std::env::var_os(var).is_some() {
            format!("environment override ({var})")
        } else {
            "default under HOME".to_string()
        };
        out.push(json!({ "setting": name, "value": path, "source": source, "note": null }));
    }
    out
}

/// The effective value of one managed setting and a note on it.
fn value_of(
    paths: &Paths,
    config: &config::Config,
    section: &str,
    key: &str,
) -> (String, Option<String>) {
    match (section, key) {
        ("hub", _) => (config.hub.log_level.as_str().to_string(), None),
        ("omp", _) => {
            let resolved = crate::omp::resolve(&config.omp.path);
            let note = resolved.map_or_else(
                || {
                    format!(
                        "unavailable: '{}' is not an executable file or on PATH",
                        config.omp.path
                    )
                },
                |p| format!("resolves to {}", p.display()),
            );
            (config.omp.path.clone(), Some(note))
        }
        ("discord", "owner") => (
            config
                .discord
                .owner
                .map_or_else(|| "-".to_string(), |o| o.to_string()),
            Some(if config.discord.intake() {
                "intake runs".to_string()
            } else {
                "intake is off until the owner and channels are set".to_string()
            }),
        ),
        ("discord", "channels") => (
            config
                .discord
                .channels
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(","),
            None,
        ),
        ("telegram" | "slack", _) => relay_value(paths, config, section, key),
        ("fallback", _) => (
            config.fallback.on.clone().unwrap_or_else(|| "-".to_string()),
            Some("when OMP falls back: any error, usage-limit or except-usage-limit; unset keeps OMP's own setting".to_string()),
        ),
        ("fallback.chains", _) => (
            config
                .fallback
                .chains
                .get("default")
                .map_or_else(|| "-".to_string(), |c| c.join(",")),
            Some(
                "the models OMP may move to, in order, when the model in use runs out; never a reset, credits or overage"
                    .to_string(),
            ),
        ),
        _ => {
            let path = crate::jobs::owner::health::token_path(config, &paths.config_file);
            let note = match read_private(&path, 4096) {
                Ok(Some(_)) => "token present".to_string(),
                Ok(None) => "unavailable: no token in that file yet".to_string(),
                Err(e) => format!("unavailable: {e}"),
            };
            (path.display().to_string(), Some(note))
        }
    }
}

/// [`value_of`] for the Telegram and Slack settings.
fn relay_value(
    paths: &Paths,
    config: &config::Config,
    section: &str,
    key: &str,
) -> (String, Option<String>) {
    match (section, key) {
        (_, "token_file") => {
            let (configured, name) = if section == "telegram" {
                (config.telegram.token_file.as_ref(), "telegram-token")
            } else {
                (config.slack.token_file.as_ref(), "slack-tokens")
            };
            let path = crate::relay::intake::secret_path(configured, &paths.config_file, name);
            let note = match crate::relay::intake::read_secret(&path) {
                Ok(_) => "token present".to_string(),
                Err(e) => format!("unavailable: {e}"),
            };
            (path.display().to_string(), Some(note))
        }
        ("telegram", key) => {
            let value = match key {
                "owner" => config.telegram.owner,
                "group" => config.telegram.group,
                _ => config.telegram.topic,
            };
            let note = (key == "owner").then(|| {
                if config.telegram.intake() {
                    "intake runs".to_string()
                } else {
                    "intake is off until the owner is set".to_string()
                }
            });
            (
                value.map_or_else(|| "-".to_string(), |v| v.to_string()),
                note,
            )
        }
        ("slack", "role") => (
            match config.slack.role {
                crate::config::relays::SlackRole::App => "app".to_string(),
                crate::config::relays::SlackRole::Member => "member".to_string(),
            },
            Some(
                "app: the bot user sees only conversations it is in; member: the app's user \
                     token sees what that member sees and needs a paid seat"
                    .to_string(),
            ),
        ),
        _ => (
            config
                .slack
                .owner
                .clone()
                .unwrap_or_else(|| "-".to_string()),
            Some(if config.slack.intake() {
                "intake runs; only the owner's messages are admitted unless [slack] allow lists others"
                        .to_string()
            } else {
                "intake is off until the owner is set".to_string()
            }),
        ),
    }
}

/// Checks one managed value given on the command line and writes it into `text`.
fn set_value(
    text: &str,
    (section, key, flag): (&str, &str, &str),
    v: &str,
) -> Result<String, Failure> {
    if key == "log_level" && crate::log::Level::parse(v).is_none() {
        return Err(Failure::Usage(format!(
            "{flag}: expected error, warn, info or debug"
        )));
    }
    if key == "token_file" && !Path::new(v).is_absolute() {
        return Err(Failure::Usage(format!("{flag}: expected an absolute path")));
    }
    if let Some(why) = invalid(section, key, v.trim()) {
        return Err(Failure::Usage(format!("{flag}: {why}")));
    }
    let list = |items: &[&str]| {
        let quoted: Vec<String> = items.iter().map(|s| toml_string(s)).collect();
        format!("[{}]", quoted.join(", "))
    };
    if key == "channels" {
        let ids = comma_list(v);
        let id = |s: &&str| crate::relay::discord::Snowflake::parse(s).is_some();
        if ids.is_empty() || !ids.iter().all(id) {
            return Err(Failure::Usage(format!(
                "{flag}: expected Discord channel IDs separated by commas"
            )));
        }
        return Ok(set_rendered(text, section, key, &list(&ids)));
    }
    if section == "fallback.chains" {
        return Ok(set_rendered(text, section, key, &list(&comma_list(v))));
    }
    Ok(set_key(text, section, key, v.trim()))
}

pub(super) fn run(p: &Parsed, paths: &Paths, as_json: bool) -> Result<(), Failure> {
    let path = &paths.config_file;
    let before = snapshot(path).map_err(Failure::Refused)?;
    let mut text = match &before.bytes {
        Some(b) => String::from_utf8(b.clone())
            .map_err(|_| Failure::Refused(format!("{} is not UTF-8", path.display())))?,
        None => NEW_FILE.to_string(),
    };
    let creating = before.bytes.is_none();
    text = managed(p, text, creating)?;
    let home = if p.flag("--backup") {
        Some(super::install::block_on(super::home::plan(p, paths))?)
    } else {
        None
    };
    if let Some(h) = &home {
        text = h.config(&text);
    }
    finish(p, paths, &before, creating, &text, home.as_ref(), as_json)
}

/// Sets the managed settings the flags name (a new file gets the ones OMP and the hub need).
fn managed(p: &Parsed, mut text: String, creating: bool) -> Result<String, Failure> {
    for (section, key, flag) in MANAGED {
        let value = match (p.value(flag), creating, section) {
            (Some(v), _, _) => Some(v.to_string()),
            // A new file gets the managed settings OMP and the hub need, with their defaults.
            (None, true, "hub") => Some("info".to_string()),
            (None, true, "omp") => Some("omp".to_string()),
            _ => None,
        };
        if let Some(v) = value {
            text = set_value(&text, (section, key, flag), &v)?;
        }
    }
    Ok(text)
}

/// Validates, writes when applying (after the home backup is prepared), and reports.
fn finish(
    p: &Parsed,
    paths: &Paths,
    before: &Snapshot,
    creating: bool,
    text: &str,
    home: Option<&super::home::Home>,
    as_json: bool,
) -> Result<(), Failure> {
    let path = &paths.config_file;
    let after = sha256_hex(text.as_bytes());
    let action = if creating {
        "create"
    } else if after == before.digest() {
        "keep"
    } else {
        "update"
    };
    let valid = config::parse(text).map_err(|errors| config::describe(&errors));
    let apply = p.flag("--apply");
    if apply {
        if let Err(e) = &valid {
            return Err(Failure::Refused(format!(
                "the result would not be a valid config: {e}"
            )));
        }
        if let Some(expect) = p.value("--expect")
            && expect != before.digest()
        {
            return Err(Failure::Refused(format!(
                "{} changed since its snapshot (expected {expect}, found {}); your edit is kept, run setup again",
                path.display(),
                before.digest()
            )));
        }
        if let Some(h) = home {
            super::install::block_on(h.apply(p.flag("--create"))).map_err(Failure::Refused)?;
        }
        if action != "keep" {
            publish(path, before, text.as_bytes())?;
        }
    }
    let hub = super::client::Client::connect(&paths.socket)
        .ok()
        .and_then(|mut c| c.call(method::STATUS, json!({}), CALL_TIMEOUT).ok())
        .and_then(|v| serde_json::from_value::<Status>(v).ok());
    let report = json!({
        "file": path.display().to_string(),
        "action": action,
        "applied": apply,
        "before": before.digest(),
        "after": after,
        "managed": MANAGED.iter().map(|(s, k, _)| format!("{s}.{k}")).collect::<Vec<_>>(),
        "unknown_keys": "kept as they are",
        "valid": valid.as_ref().map_or_else(|e| Value::String(e.clone()), |_| Value::Bool(true)),
        "sources": sources(paths, text, hub.as_ref(), &after),
        "backup": home.map(super::home::Home::report),
    });
    if as_json {
        out::json(&report);
        return Ok(());
    }
    let invalid = valid.as_ref().err().map(String::as_str);
    print_report(p, &report, apply, action, &before.digest(), &after, invalid);
    if let Some(h) = home {
        for l in h.lines(apply) {
            out::line(&l);
        }
    }
    Ok(())
}

fn print_report(
    p: &Parsed,
    report: &Value,
    apply: bool,
    action: &str,
    before: &str,
    after: &str,
    invalid: Option<&str>,
) {
    let path = report
        .get("file")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let verb = if apply { "done" } else { "preview" };
    out::line(&format!("setup {verb}: {action} {path}"));
    out::line(&format!("  before {before}"));
    out::line(&format!("  after  {after}"));
    out::line(
        "  managed: hub.log_level, omp.path, discord.token_file/owner/channels, telegram.token_file/owner/group/topic, \
         slack.token_file/role/owner; tokens stay in their owner-only files; other keys and comments are kept",
    );
    if let Some(e) = invalid {
        out::line(&format!("  the result is not a valid config: {e}"));
    }
    if !apply && (action != "keep" || p.flag("--backup")) {
        out::line(&format!(
            "  write it with: cataly setup --apply --expect {before}{}{}",
            MANAGED
                .iter()
                .filter_map(|(_, _, f)| p.value(f).map(|v| format!(" {f} {v}")))
                .collect::<String>(),
            if p.flag("--backup") {
                format!(
                    " --backup{}",
                    p.value("--backup-repo")
                        .map_or_else(String::new, |r| format!(" --backup-repo {r}"))
                )
            } else {
                String::new()
            }
        ));
    }
    let text = |s: &Value, k: &str| {
        s.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    if let Some(sources) = report.get("sources").and_then(Value::as_array) {
        for s in sources {
            let note = s
                .get("note")
                .and_then(Value::as_str)
                .map_or_else(String::new, |n| format!("; {n}"));
            out::line(&format!(
                "  {:<20} {} ({}{note})",
                text(s, "setting"),
                text(s, "value"),
                text(s, "source")
            ));
        }
    }
}

/// Re-checks the snapshot, writes atomically (never overwriting a file that appeared meanwhile), and
/// reads the result back.
fn publish(path: &Path, before: &Snapshot, bytes: &[u8]) -> Result<(), Failure> {
    let now = snapshot(path).map_err(Failure::Refused)?;
    if now != *before {
        return Err(Failure::Refused(format!(
            "{} changed since its snapshot; your edit is kept, run setup again",
            path.display()
        )));
    }
    if let Some(dir) = path.parent() {
        crate::sys::fs::ensure_private_dir(dir).map_err(|e| Failure::Refused(e.to_string()))?;
    }
    let written = if before.bytes.is_none() {
        create_atomic(path, bytes)
    } else {
        write_atomic(path, bytes)
    };
    written.map_err(|e| Failure::Refused(e.to_string()))?;
    match read_private(path, CONFIG_MAX) {
        Ok(Some(back)) if back == bytes => Ok(()),
        _ => Err(Failure::Refused(format!(
            "{} did not read back as written",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_key_replaces_adds_and_keeps_the_rest() {
        let text = "# mine\n[hub]\nlog_level = \"warn\" # loud\n\n[jobs.x]\nkind = \"once\"\n";
        let out = set_key(text, "hub", "log_level", "debug");
        assert_eq!(
            out,
            "# mine\n[hub]\nlog_level = \"debug\"\n\n[jobs.x]\nkind = \"once\"\n"
        );
        let out = set_key(&out, "omp", "path", "/opt/omp \"x\"");
        assert!(
            out.ends_with("[omp]\npath = \"/opt/omp \\\"x\\\"\"\n"),
            "{out}"
        );
        let out = set_key("[omp]\n# keep\n\n[hub]\n", "omp", "path", "omp");
        assert_eq!(out, "[omp]\n# keep\npath = \"omp\"\n\n[hub]\n");
        assert!(config::parse(&set_key(NEW_FILE, "hub", "log_level", "info")).is_ok());
    }
}
