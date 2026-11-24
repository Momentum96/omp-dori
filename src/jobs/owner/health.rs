//! Health (OJ-16, OJ-18, WS-34): one list of checks, each OK, INFO or WARN with a next step, shown by
//! `cataly health`, and the `health` job that reports a WARN only after the configured number of
//! consecutive bad ticks (two by default) and its clearing once.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{TICK, Tick};
use crate::config::owner::{FileAge, Health};
use crate::config::{Config, Destination, Owner, Work};
use crate::jobs::event::{Emit, EventKind, Severity};
use crate::jobs::{Ctx, JobsError, duration_ms};

/// One check's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Check {
    pub(crate) name: String,
    /// `ok`, `info` or `warn`.
    pub(crate) level: String,
    pub(crate) detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) next: Option<String>,
}

fn check(name: &str, level: &str, detail: impl Into<String>, next: Option<&str>) -> Check {
    Check {
        name: name.to_string(),
        level: level.to_string(),
        detail: detail.into(),
        next: next.map(str::to_string),
    }
}

/// The Discord token file the config names, or its default next to the config file.
pub(crate) fn token_path(config: &Config, config_file: &Path) -> std::path::PathBuf {
    config.discord.token_file.clone().unwrap_or_else(|| {
        config_file
            .parent()
            .unwrap_or(Path::new("/"))
            .join("secrets")
            .join("discord-token")
    })
}

/// The config, OMP and Discord token checks.
fn hub_checks(ctx: &Ctx, config: &Config) -> Vec<Check> {
    let mut out = Vec::new();
    match ctx.config_error() {
        Some(e) => out.push(check(
            "config",
            "warn",
            format!("the file on disk is not applied: {e}"),
            Some("fix the file, then run cataly config reload"),
        )),
        None => out.push(check("config", "ok", "valid and applied", None)),
    }
    out.push(match ctx.omp.report() {
        None => check("omp", "info", "not probed yet", None),
        Some(r) => match r.sessions() {
            Err(e) => check(
                "omp",
                "warn",
                format!("{e} ({})", r.summary()),
                Some("install OMP 19.1.1 or newer, or set [omp] path"),
            ),
            Ok(()) if r.untested => check("omp", "info", r.summary(), None),
            Ok(()) => check("omp", "ok", r.summary(), None),
        },
    });
    if config
        .destinations
        .values()
        .any(|d| matches!(d, Destination::Discord { .. }))
    {
        let path = token_path(config, &ctx.config_file);
        out.push(match crate::sys::fs::read_private(&path, 4096) {
            Ok(Some(t)) if !t.trim_ascii().is_empty() => {
                check("discord.token", "ok", "present", None)
            }
            Ok(_) => check(
                "discord.token",
                "warn",
                format!("absent: {}", path.display()),
                Some("put the bot token in that file with mode 0600"),
            ),
            Err(e) => check(
                "discord.token",
                "warn",
                e.to_string(),
                Some("chmod 600 the token file"),
            ),
        });
    }
    out
}

/// Every check, in a fixed order.
pub(crate) async fn checks(ctx: &Ctx) -> Result<Vec<Check>, JobsError> {
    let config = ctx.config();
    let now = crate::sys::now_ms();
    let mut out = hub_checks(ctx, &config);
    let last_runs = ctx.store.read(crate::store::Reads::last_runs).await?;
    let rows = ctx.store.read(crate::store::Reads::jobs).await?;
    for (name, job) in &config.jobs {
        let Some(every) = job.trigger.every().filter(|_| job.enabled) else {
            continue;
        };
        let key = name.clone();
        let memory = ctx.store.read(move |r| r.memory(&key, false)).await?;
        let last_good = match &job.work {
            Work::Owner(_) => memory
                .iter()
                .find(|(s, c, _)| s == TICK && c == "last_good")
                .and_then(|(_, _, v)| v.parse::<i64>().ok()),
            Work::Command { .. } => last_runs
                .iter()
                .find(|r| {
                    &r.job == name
                        && !r.dry
                        && r.outcome
                            .as_deref()
                            .is_some_and(|o| !o.starts_with("error"))
                })
                .and_then(|r| r.ended_ms),
        };
        let since = last_good
            .or_else(|| rows.iter().find(|r| &r.name == name).map(|r| r.created_ms))
            .unwrap_or(now);
        let held = rows
            .iter()
            .any(|r| &r.name == name && (r.held || r.consumed));
        let limit = duration_ms(every).saturating_mul(2);
        let label = format!("job.{name}");
        out.push(if held {
            check(&label, "info", "on hold", None)
        } else if now - since > limit {
            check(
                &label,
                "warn",
                format!(
                    "no good tick for {} s (cadence {} s)",
                    (now - since) / 1000,
                    every.as_secs()
                ),
                Some("see cataly job list and the hub log"),
            )
        } else {
            check(&label, "ok", "ticking", None)
        });
    }
    for job in config.jobs.values() {
        if let Work::Owner(Owner::Health(h)) = &job.work {
            for f in &h.files {
                out.push(file_age(f, now));
            }
        }
    }
    Ok(out)
}

fn file_age(f: &FileAge, now: i64) -> Check {
    let label = format!("file.{}", f.path.display());
    let modified = std::fs::metadata(&f.path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_millis()).ok());
    match modified {
        None => check(
            &label,
            "warn",
            "missing or unreadable",
            Some("check the path in the health job"),
        ),
        Some(at) if now - at > duration_ms(f.max_age) => check(
            &label,
            "warn",
            format!(
                "unchanged for {} min (limit {} min)",
                (now - at) / 60_000,
                f.max_age.as_secs() / 60
            ),
            None,
        ),
        Some(_) => check(&label, "ok", "fresh", None),
    }
}

pub(super) async fn tick(t: &mut Tick<'_>, h: &Health) -> Result<(), String> {
    let all = checks(t.ctx).await.map_err(|e| e.to_string())?;
    let ignore = t.ctx.config().health.ignore.clone();
    let mut seen: HashSet<String> = HashSet::new();
    for c in all.into_iter().filter(|c| !ignore.contains(&c.name)) {
        seen.insert(c.name.clone());
        if c.level == "warn" {
            let strikes = t
                .memory
                .get(&c.name, "strikes")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(0)
                .saturating_add(1);
            t.memory
                .transition(&c.name, "strikes", &strikes.min(99).to_string());
            if strikes >= h.strikes && t.memory.get(&c.name, "reported").is_none() {
                t.memory.transition(&c.name, "reported", &c.detail);
                t.emit(
                    Emit::new(EventKind::HealthProblem, Severity::Warning, true, &c.name)
                        .with("check", &c.name)
                        .with("detail", &c.detail),
                );
            }
        } else {
            t.memory.clear(&c.name, "strikes");
            if t.memory.clear(&c.name, "reported").is_some() {
                t.emit(
                    Emit::new(EventKind::HealthCleared, Severity::Info, false, &c.name)
                        .with("check", &c.name),
                );
            }
        }
    }
    // Checks that no longer exist (a removed job or file) are forgotten silently.
    for (subject, _) in t.memory.with_condition("strikes") {
        if !seen.contains(&subject) {
            t.memory.clear(&subject, "strikes");
            t.memory.clear(&subject, "reported");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::jobs::owner::tests::live_tick;
    use crate::jobs::tests::ctx_with;

    #[tokio::test]
    async fn a_problem_counts_after_two_bad_ticks_and_clears_once() {
        let dir = crate::sys::fs::tests::scratch("health");
        let note = dir.join("note.md");
        let cfg = format!(
            "[jobs.h]\nkind = \"health\"\nfiles = [{{ path = \"{}\", max_age = \"1h\" }}]\n[health]\nignore = [\"omp\"]\n",
            note.display()
        );
        let (ctx, _stop, _dir) = ctx_with(&cfg, "health-ctx");
        assert!(
            live_tick(&ctx, "h").await.is_empty(),
            "first bad tick: not yet"
        );
        assert_eq!(live_tick(&ctx, "h").await, ["health.problem"]);
        assert!(live_tick(&ctx, "h").await.is_empty(), "reported once");
        std::fs::write(&note, "fresh").unwrap();
        assert_eq!(live_tick(&ctx, "h").await, ["health.cleared"]);
        assert!(live_tick(&ctx, "h").await.is_empty(), "cleared once");
        std::fs::remove_file(&note).unwrap();
        assert!(
            live_tick(&ctx, "h").await.is_empty(),
            "one bad tick after clearing is not enough"
        );
        std::fs::write(&note, "again").unwrap();
        assert!(
            live_tick(&ctx, "h").await.is_empty(),
            "a single strike never reports"
        );
        let events = ctx.store.read(|r| r.events_after(0, 10)).await.unwrap();
        assert!(events[0].text.contains("missing"), "{}", events[0].text);
    }
}
