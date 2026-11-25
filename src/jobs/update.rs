//! The release notice (IR-21, IR-22; req. 36-39; owner decision 12): once a day the hub reads
//! GitHub's latest stable release and tells the owner, once per version, when it is newer than this
//! binary. It never downloads, installs or restarts anything; `cataly update` does that when the
//! owner runs it. A version the owner rolled back from is never announced again. With the check off
//! no request is made.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use super::Jobs;
use super::event::{Emit, EventKind, Severity};
use super::memory::Memory;
use crate::log;
use crate::store::Apply;
use crate::sys::now_ms;
use crate::sys::release::{self, Version};

/// The memory this task keeps (survives a restart, IR req. 39).
const JOB: &str = "update";
const SUBJECT: &str = "release";
/// One check per day; the next is due a day after the attempt, whatever its result (IR req. 36).
const EVERY_MS: i64 = 24 * 60 * 60 * 1000;
/// How often the task looks at the config and the clock while it waits.
const LOOK: Duration = Duration::from_secs(30);

/// Runs until the hub stops. `root` is the program folder holding the silenced version.
pub(crate) async fn run(
    jobs: Jobs,
    root: PathBuf,
    mut stop: watch::Receiver<bool>,
) -> Result<(), String> {
    let started = Instant::now();
    loop {
        let config = Arc::clone(&jobs.ctx().config.borrow());
        let mut memory = load(&jobs).await?;
        let last = memory
            .get(SUBJECT, "last_check_ms")
            .and_then(|v| v.parse::<i64>().ok());
        let since_start = started.elapsed();
        let wait_start = config.update.first_after.saturating_sub(since_start);
        let wait_day = last.map_or(Duration::ZERO, |l| {
            Duration::from_millis(u64::try_from(l + EVERY_MS - now_ms()).unwrap_or(0))
        });
        let wait = if config.update.check {
            wait_start.max(wait_day)
        } else {
            LOOK
        };
        if !wait.is_zero() {
            tokio::select! {
                () = tokio::time::sleep(wait.min(LOOK)) => continue,
                _ = stop.changed() => return Ok(()),
            }
        }
        memory.transition(SUBJECT, "last_check_ms", &now_ms().to_string());
        let api = config.update.api()?;
        let fetched = tokio::select! {
            r = release::fetch_release(&api, None) => r,
            _ = stop.changed() => return Ok(()),
        };
        let emit = match fetched {
            Ok(Some(latest)) => notice(&mut memory, &latest.version, silenced(&root).as_ref()),
            Ok(None) => None,
            Err(e) => {
                log::warn("release check failed", &[("error", &e)]);
                None
            }
        };
        if let Some(emit) = emit {
            jobs.hub_event(emit).await.map_err(|e| e.to_string())?;
        }
        jobs.ctx()
            .store
            .apply(Apply {
                memory: memory.into_writes(),
                ..Apply::default()
            })
            .await
            .map_err(|e| e.to_string())?;
    }
}

async fn load(jobs: &Jobs) -> Result<Memory, String> {
    let rows = jobs
        .ctx()
        .store
        .read(|r| r.memory(JOB, false))
        .await
        .map_err(|e| e.to_string())?;
    Ok(Memory::new(JOB, false, rows))
}

/// The version the owner rolled back from, if any.
fn silenced(root: &Path) -> Option<Version> {
    let bytes = crate::sys::fs::read_private(&root.join("silenced"), 64).ok()??;
    Version::parse(String::from_utf8_lossy(&bytes).trim())
}

/// The notice for `latest`, once per version, never for a silenced one or an older one (IR req. 37).
fn notice(memory: &mut Memory, latest: &Version, silenced: Option<&Version>) -> Option<Emit> {
    let installed = Version::parse(release::VERSION)?;
    if !latest.stable() || *latest <= installed || silenced == Some(latest) {
        return None;
    }
    if !memory.transition(SUBJECT, "announced", &latest.to_string()) {
        return None;
    }
    Some(
        Emit::new(
            EventKind::UpdateAvailable,
            Severity::Attention,
            false,
            "hub",
        )
        .with("version", latest.to_string())
        .with("installed", installed.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announces_once_and_never_a_silenced_version() {
        let mut memory = Memory::new(JOB, false, Vec::new());
        let v = |t: &str| Version::parse(t).unwrap();
        assert!(notice(&mut memory, &v("0.0.1"), None).is_none(), "older");
        assert!(
            notice(&mut memory, &v("99.0.0-rc.1"), None).is_none(),
            "prerelease"
        );
        assert!(
            notice(&mut memory, &v("99.0.0"), Some(&v("99.0.0"))).is_none(),
            "silenced"
        );
        assert!(notice(&mut memory, &v("99.0.0"), None).is_some());
        assert!(notice(&mut memory, &v("99.0.0"), None).is_none(), "once");
        assert!(notice(&mut memory, &v("99.1.0"), None).is_some());
    }
}
