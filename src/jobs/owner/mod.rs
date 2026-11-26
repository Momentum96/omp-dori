//! The owner job kinds (DESIGN 6.1) on the common tick rules, written once here (DESIGN 6.2, OJ req.
//! 3-10): a tick loads its job's transition memory (live or dry), runs the kind under the per-tick
//! deadline, and on success commits the kind's memory after its events entered the stream. A failed
//! or overrunning tick leaves the kind's state untouched and warns once per distinct reason; a
//! transient failure (the server or the way to it) is retried on the next tick and warns only when
//! it happens twice in a row. The first good tick after a reported failure emits one recovery note.
//! The first tick takes a baseline.

pub(crate) mod backup;
mod github;
pub(crate) mod health;
mod items;
pub(crate) mod pr;
pub(crate) mod pressure;
pub(crate) mod quota;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::timeout;

use super::event::{Emit, EventKind, Severity, build};
use super::memory::Memory;
use super::{Ctx, JobsError};
use crate::config::{Job, Owner};
use crate::store::{Apply, EventRecord};
use crate::sys::command::{self, Finished, KILL_GRACE, Output, Spec};
use crate::sys::{ErrorClass, now_ms};

/// Memory subject of the tick bookkeeping every kind shares.
pub(crate) const TICK: &str = "_tick";

/// One tick in progress.
pub(crate) struct Tick<'a> {
    pub(crate) ctx: &'a Ctx,
    pub(crate) job: &'a Job,
    pub(crate) dry: bool,
    /// No baseline yet: report nothing that already existed.
    pub(crate) first: bool,
    pub(crate) memory: Memory,
    pub(crate) emits: Vec<Emit>,
    pub(crate) now: i64,
    /// A kind sets this when it read nothing new but still has state to keep (the quota job's
    /// bands while usage data is unavailable).
    pub(crate) soft_failure: Option<String>,
}

impl Tick<'_> {
    pub(crate) fn emit(&mut self, emit: Emit) {
        self.emits.push(emit);
    }

    /// Runs one bounded command under the hub-wide command slots (OJ req. 9). Spawn failures, time
    /// limits and over-cap output are errors; the exit status is the caller's to judge.
    pub(crate) async fn command(
        &self,
        argv: &[String],
        cwd: Option<&Path>,
        env: &[(&str, &str)],
        limit: Duration,
        cap: u64,
    ) -> Result<Finished, String> {
        let _slot = self
            .ctx
            .command_slots
            .acquire()
            .await
            .map_err(|_| "the hub is stopping".to_string())?;
        let program = argv.first().map_or("command", String::as_str);
        let spec = Spec {
            argv,
            cwd,
            env,
            timeout: limit,
            kill_grace: KILL_GRACE,
            output_cap: cap,
            output: Output::Capture,
        };
        let done = command::spawn(&spec)
            .map_err(|e| e.to_string())?
            .wait(Some(self.ctx.shutdown.clone()))
            .await;
        if done.cancelled {
            return Err("the hub is stopping".into());
        }
        if done.timed_out {
            return Err(format!("{program} timed out after {} s", limit.as_secs()));
        }
        if done.truncated {
            return Err(format!("{program} wrote more than {cap} bytes"));
        }
        Ok(done)
    }
}

/// First line of a command's error output, masked and cut, for warnings.
pub(crate) fn said(done: &Finished) -> String {
    let text = if done.stderr.is_empty() {
        &done.stdout
    } else {
        &done.stderr
    };
    let first = String::from_utf8_lossy(text)
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default()
        .to_string();
    super::event::one_line(&crate::sys::secrets::mask(&first), 160)
}

/// How Cataly talks to `gh`, for owner jobs and lane done checks alike (decision 13): prompts and
/// colour off, the REST media type, the answer read as JSON. Each caller bounds the command its own
/// way: owner jobs inside a tick's command slot, done checks inside a lane action, which holds the
/// lane's turn and so must not wait for a slot.
pub(crate) const GH_ENV: [(&str, &str); 2] = [("GH_PROMPT_DISABLED", "1"), ("NO_COLOR", "1")];

/// The `gh` arguments of one REST read.
pub(crate) fn gh_api(path: &str) -> [&str; 4] {
    ["api", "-H", "Accept: application/vnd.github+json", path]
}

/// `gh`'s answer to `args`: its output as JSON, else why not.
pub(crate) fn gh_answer(args: &[&str], done: &Finished) -> Result<serde_json::Value, String> {
    if !done.success() {
        return Err(format!("gh {} failed: {}", args.join(" "), said(done)));
    }
    serde_json::from_slice(&done.stdout)
        .map_err(|_| format!("gh {} printed no JSON", args.join(" ")))
}

/// Network failures as `gh`, `git` and the hub's own deadlines word them, lower case.
const NETWORK: [&str; 11] = [
    "timed out",
    "timeout",
    "overran its",
    "connection refused",
    "connection reset",
    "no such host",
    "network is unreachable",
    "temporary failure in name resolution",
    "unexpected eof",
    "tls handshake",
    "error connecting to",
];

/// The HTTP statuses a reason names, as `gh` words them (`HTTP 404`, `(HTTP 502)`).
fn http_statuses(lower: &str) -> impl Iterator<Item = u16> + '_ {
    lower.match_indices("http ").filter_map(|(i, m)| {
        let rest = lower.get(i + m.len()..)?;
        let digits = rest.get(..3)?;
        let more = rest.as_bytes().get(3).is_some_and(u8::is_ascii_digit);
        if more || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok()
    })
}

/// The decision class of a failed tick's reason (DESIGN 9.5): transient when the server or the way
/// to it failed (an HTTP 408, 429 or 5xx, a time limit, a network error), so the next tick tries
/// again; rejected for anything else, such as a 4xx (404, 401) or a bad config.
pub(crate) fn failure_class(reason: &str) -> ErrorClass {
    let lower = reason.to_ascii_lowercase();
    let server =
        http_statuses(&lower).any(|s| crate::relay::http::status_class(s) == ErrorClass::Transient);
    if server || NETWORK.iter().any(|w| lower.contains(w)) {
        ErrorClass::Transient
    } else {
        ErrorClass::Rejected
    }
}

/// Whether a failure seen `failures` times in a row is reported (an event, and so a note for the
/// lead): a permanent one at once, a transient one only from the second in a row, the count that
/// opens a job alert in the ops room (`alerts.jobs`).
pub(crate) fn reported(failures: i64, reason: &str) -> bool {
    failures >= super::ops::JOB_FAILURES || failure_class(reason) != ErrorClass::Transient
}

/// Runs one tick of an owner job and returns its events.
pub(crate) async fn tick(
    ctx: &Ctx,
    job: &Arc<Job>,
    owner: &Owner,
    dry: bool,
) -> Result<Vec<EventRecord>, JobsError> {
    let (name, d) = (job.name.clone(), dry);
    let rows = ctx.store.read(move |r| r.memory(&name, d)).await?;
    let base = Memory::new(&job.name, dry, rows);
    let now = now_ms();
    let mut t = Tick {
        ctx,
        job,
        dry,
        first: base.get(TICK, "baseline").is_none(),
        memory: base.clone(),
        emits: Vec::new(),
        now,
        soft_failure: None,
    };
    let mut shutdown = ctx.shutdown.clone();
    let outcome = tokio::select! {
        result = timeout(job.timeout, run_kind(&mut t, owner)) => result.unwrap_or_else(|_| {
            Err(format!("tick overran its {} s deadline", job.timeout.as_secs()))
        }),
        _ = shutdown.wait_for(|stop| *stop) => return Err(JobsError::Stopped),
    };
    let failure = match outcome {
        Ok(()) => t.soft_failure.take(),
        Err(reason) => {
            // The kind's own changes are dropped: state stays as the last good tick left it.
            t.memory = base;
            t.emits.clear();
            Some(reason)
        }
    };
    let mut memory = t.memory;
    let mut emits = t.emits;
    if let Some(reason) = failure {
        let failures = memory
            .get(TICK, "failures")
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0)
            + 1;
        memory.transition(TICK, "failures", &failures.to_string());
        if reported(failures, &reason) && memory.transition(TICK, "error", &reason) {
            emits.push(
                Emit::new(EventKind::TickFailed, Severity::Warning, false, &job.name)
                    .with("job", &job.name)
                    .with("reason", &reason),
            );
        }
    } else {
        // Only a reported failure has a recovery to report.
        if let Some(failures) = memory.clear(TICK, "failures")
            && memory.clear(TICK, "error").is_some()
        {
            emits.push(
                Emit::new(EventKind::TickRecovered, Severity::Info, false, &job.name)
                    .with("job", &job.name)
                    .with("failures", failures),
            );
        }
        memory.transition(TICK, "baseline", "1");
        memory.transition(TICK, "last_good", &now.to_string());
    }
    let at = now_ms();
    let events: Vec<_> = emits
        .into_iter()
        .map(|e| build(&job.name, e, &job.templates, dry, &job.destinations, at))
        .collect();
    let records = ctx.stream.append(events).await?;
    ctx.store
        .apply(Apply {
            memory: memory.into_writes(),
            ..Apply::default()
        })
        .await?;
    Ok(records)
}

async fn run_kind(t: &mut Tick<'_>, owner: &Owner) -> Result<(), String> {
    match owner {
        Owner::Github(g) => github::tick(t, g).await,
        Owner::Pr(p) => pr::tick(t, p).await,
        Owner::Quota(q) => quota::tick(t, q).await,
        Owner::Backup(b) => backup::tick(t, b).await,
        Owner::Health(h) => health::tick(t, h).await,
        Owner::Items(i) => items::tick(t, i).await,
        Owner::Pressure(p) => pressure::tick(t, p).await,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::Work;
    use crate::jobs::tests::ctx_with;

    /// Runs one live tick of the named job and returns the kinds of its events.
    pub(crate) async fn live_tick(ctx: &Ctx, name: &str) -> Vec<String> {
        let job = ctx.config().jobs.get(name).cloned().unwrap();
        let Work::Owner(owner) = &job.work else {
            panic!("not an owner job");
        };
        tick(ctx, &job, owner, false)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect()
    }

    #[tokio::test]
    async fn failures_warn_once_per_reason_and_recover_once() {
        let dir = crate::sys::fs::tests::scratch("owner-fail");
        let script = dir.join("items.sh");
        let flag = dir.join("flag");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nif [ -e {f} ]; then printf 'a\\tfirst\\n'; else echo broken; exit 3; fi\n", f = flag.display()),
        )
        .unwrap();
        crate::sys::fs::tests::make_executable(&script);
        let cfg = format!(
            "[jobs.mail]\nkind = \"items\"\nargv = [\"{}\"]\n",
            script.display()
        );
        let (ctx, _stop, _dir) = ctx_with(&cfg, "owner-fail-ctx");
        assert_eq!(live_tick(&ctx, "mail").await, ["job.failed"]);
        assert!(
            live_tick(&ctx, "mail").await.is_empty(),
            "same reason: no repeat"
        );
        std::fs::write(&flag, "").unwrap();
        assert_eq!(live_tick(&ctx, "mail").await, ["job.recovered"]);
        let memory = ctx.store.read(|r| r.memory("mail", false)).await.unwrap();
        assert!(memory.iter().any(|(s, c, _)| s == TICK && c == "last_good"));
        assert!(
            memory.iter().any(|(s, c, _)| s == "a" && c == "seen"),
            "baseline recorded"
        );
    }

    #[test]
    fn failure_classes() {
        for transient in [
            "gh api notifications failed: gh: HTTP 500",
            "gh api x failed: gh: Server Error (HTTP 502)",
            "gh: API rate limit exceeded (HTTP 429)",
            "gh timed out after 30 s",
            "tick overran its 60 s deadline",
            "error connecting to api.github.com",
            "dial tcp: lookup api.github.com: no such host",
        ] {
            assert_eq!(
                failure_class(transient),
                ErrorClass::Transient,
                "{transient}"
            );
        }
        for rejected in [
            "gh api repos/o/r failed: gh: Not Found (HTTP 404)",
            "gh: Bad credentials (HTTP 401)",
            "HTTP 5000 is no status",
            "repos: required",
        ] {
            assert_eq!(failure_class(rejected), ErrorClass::Rejected, "{rejected}");
        }
    }

    /// A one-off server error is retried on the next tick and never becomes an event (so never a
    /// note for the lead); the same kind of failure twice in a row does. A 404 is reported at once.
    #[tokio::test]
    async fn a_transient_failure_is_reported_only_twice_in_a_row() {
        let dir = crate::sys::fs::tests::scratch("owner-transient");
        let script = dir.join("items.sh");
        let (ok, said) = (dir.join("ok"), dir.join("said"));
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nif [ -e {ok} ]; then printf 'a\\tfirst\\n'; else cat {said} >&2; exit 1; fi\n",
                ok = ok.display(),
                said = said.display()
            ),
        )
        .unwrap();
        crate::sys::fs::tests::make_executable(&script);
        let cfg = format!(
            "[jobs.mail]\nkind = \"items\"\nargv = [\"{s}\"]\n\n[jobs.gone]\nkind = \"items\"\nargv = [\"{s}\"]\n",
            s = script.display()
        );
        let (ctx, _stop, _dir) = ctx_with(&cfg, "owner-transient-ctx");
        let say = |text: &str| std::fs::write(&said, format!("{text}\n")).unwrap();
        say("gh api notifications failed: gh: HTTP 500");
        assert!(live_tick(&ctx, "mail").await.is_empty(), "one 500: retried");
        std::fs::write(&ok, "").unwrap();
        assert!(
            live_tick(&ctx, "mail").await.is_empty(),
            "nothing was reported, so nothing recovers"
        );
        std::fs::remove_file(&ok).unwrap();
        assert!(live_tick(&ctx, "mail").await.is_empty(), "one 500 again");
        say("gh api notifications failed: gh: Server Error (HTTP 502)");
        assert_eq!(
            live_tick(&ctx, "mail").await,
            ["job.failed"],
            "twice in a row"
        );
        std::fs::write(&ok, "").unwrap();
        assert_eq!(live_tick(&ctx, "mail").await, ["job.recovered"]);
        std::fs::remove_file(&ok).unwrap();
        say("gh api repos/o/r failed: gh: Not Found (HTTP 404)");
        assert_eq!(
            live_tick(&ctx, "gone").await,
            ["job.failed"],
            "a 404 is reported at once"
        );
    }
}
