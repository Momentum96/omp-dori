//! Running a job command and applying its outcome (WS req. 59-62, DESIGN 6.2). The order is fixed:
//! record the run, spawn, record its end, append the run's events, then commit memory and run state.
//! A crash after the append but before the commit makes start-up apply the run again, so an event may
//! repeat but is never lost.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::{Fire, Job, Trigger, Work};
use crate::store::{Apply, EventRecord, RunEnd, RunRecord, RunStart};
use crate::sys::command::{self, Exit, KILL_GRACE, Output, Spec};
use crate::sys::fs::{ensure_private_dir, read_private};
use crate::sys::now_ms;

use super::event::{BODY_INLINE_MAX, Emit, EventKind, Severity, build};
use super::memory::Memory;
use super::{Ctx, JobsError, instant_text};

/// Output kept per run file (DESIGN 7.1: child output capped at 1 MiB).
const OUTPUT_CAP: u64 = 1024 * 1024;

/// One run to perform.
pub(super) struct Request {
    pub(super) job: Arc<Job>,
    pub(super) scheduled_ms: i64,
    pub(super) dry: bool,
    /// One-shot: consume the job in the same transaction that records the run.
    pub(super) consume: bool,
}

/// How a run ended, rebuilt the same way from a live child or from the stored run record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Facts {
    pub(super) run: i64,
    pub(super) dry: bool,
    pub(super) scheduled_ms: i64,
    pub(super) started_ms: i64,
    pub(super) exit_code: Option<i64>,
    pub(super) signal: Option<i64>,
    pub(super) timed_out: bool,
    pub(super) bytes: i64,
    pub(super) hash: Option<String>,
    /// Spawn or output problem, when there was one.
    pub(super) error: Option<String>,
    pub(super) stdout_path: Option<String>,
}

/// The three outcomes of WS req. 61.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Class {
    Success,
    Failure(i64),
    Error(String),
}

impl Facts {
    fn from_record(r: &RunRecord) -> Self {
        let error = r
            .outcome
            .as_deref()
            .and_then(|o| o.strip_prefix("error: "))
            .filter(|_| r.exit_code.is_none() && r.signal.is_none() && !r.timed_out)
            .map(str::to_string);
        Self {
            run: r.id,
            dry: r.dry,
            scheduled_ms: r.scheduled_ms,
            started_ms: r.started_ms,
            exit_code: r.exit_code,
            signal: r.signal,
            timed_out: r.timed_out,
            bytes: r.bytes.unwrap_or(0),
            hash: r.output_hash.clone(),
            error,
            stdout_path: r.stdout_path.clone(),
        }
    }

    pub(super) fn class(&self, needs_hash: bool) -> Class {
        if let Some(e) = &self.error {
            return Class::Error(e.clone());
        }
        if self.timed_out {
            return Class::Error("time limit".into());
        }
        if let Some(s) = self.signal {
            return Class::Error(format!("killed by signal {s}"));
        }
        match self.exit_code {
            None => Class::Error("exit status unknown".into()),
            Some(0) if needs_hash && self.hash.is_none() => {
                Class::Error("output could not be hashed (over the 1 MiB cap or held open)".into())
            }
            Some(0) => Class::Success,
            Some(code) => Class::Failure(code),
        }
    }
}

fn outcome_text(class: &Class) -> String {
    match class {
        Class::Success => "succeeded".into(),
        Class::Failure(_) => "failed".into(),
        Class::Error(reason) => format!("error: {reason}"),
    }
}

/// Performs one run end to end and returns the events it produced.
pub(super) async fn execute(ctx: &Ctx, req: Request) -> Result<Vec<EventRecord>, JobsError> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let _slot = ctx
        .command_slots
        .acquire()
        .await
        .map_err(|_| JobsError::Stopped)?;
    let job = &req.job;
    let Work::Command { argv, cwd } = &job.work else {
        return Err(JobsError::Refused(format!(
            "job '{}' runs no command",
            job.name
        )));
    };
    let probe = matches!(job.trigger, Trigger::Probe { .. });
    let dir = ctx.files_dir.join("runs").join(&job.name);
    ensure_private_dir(&dir).map_err(JobsError::Fs)?;
    let started_ms = now_ms();
    let stem = format!("{started_ms}-{}", SEQ.fetch_add(1, Ordering::Relaxed));
    let stdout_path = dir.join(format!("{stem}.out"));
    let stderr_path = probe.then(|| dir.join(format!("{stem}.err")));
    let run = ctx
        .store
        .run_started(RunStart {
            job: job.name.clone(),
            dry: req.dry,
            scheduled_ms: req.scheduled_ms,
            started_ms,
            stdout_path: stdout_path.display().to_string(),
            stderr_path: stderr_path.as_ref().map(|p| p.display().to_string()),
            consume: req.consume,
        })
        .await?;
    let env = [
        ("CATALY_JOB", job.name.as_str()),
        ("CATALY_DRY", if req.dry { "1" } else { "0" }),
    ];
    let spec = Spec {
        argv,
        cwd: Some(cwd),
        env: &env,
        timeout: job.timeout,
        kill_grace: KILL_GRACE,
        output_cap: OUTPUT_CAP,
        output: Output::Files {
            stdout: &stdout_path,
            stderr: stderr_path.as_deref(),
        },
    };
    let mut facts = Facts {
        run,
        dry: req.dry,
        scheduled_ms: req.scheduled_ms,
        started_ms,
        exit_code: None,
        signal: None,
        timed_out: false,
        bytes: 0,
        hash: None,
        error: None,
        stdout_path: Some(stdout_path.display().to_string()),
    };
    let broken = run_child(ctx, &job.name, &spec, &mut facts).await?;
    let class = facts.class(probe);
    ctx.store
        .run_ended(RunEnd {
            run,
            ended_ms: now_ms(),
            exit_code: facts.exit_code,
            signal: facts.signal,
            timed_out: facts.timed_out,
            bytes: facts.bytes,
            output_hash: facts.hash.clone(),
            outcome: if broken {
                "broken off".into()
            } else {
                outcome_text(&class)
            },
            broken,
        })
        .await?;
    if broken {
        let emit = broken_emit(&job.name, run, started_ms);
        let event = build(
            &job.name,
            emit,
            &job.templates,
            req.dry,
            &job.destinations,
            now_ms(),
        );
        return Ok(ctx.stream.append(vec![event]).await?);
    }
    apply(ctx, Some(job), &facts).await
}

/// Spawns, records the PID, waits; fills `facts` and returns whether the run was broken off by the
/// hub stopping.
async fn run_child(
    ctx: &Ctx,
    job: &str,
    spec: &Spec<'_>,
    facts: &mut Facts,
) -> Result<bool, JobsError> {
    let running = match command::spawn(spec) {
        Ok(running) => running,
        Err(e) => {
            facts.error = Some(e.to_string());
            return Ok(false);
        }
    };
    if let Some(pid) = running.pid() {
        ctx.store.run_pid(facts.run, i64::from(pid)).await?;
    }
    let done = running.wait(Some(ctx.shutdown.clone())).await;
    match done.exit {
        Exit::Code(c) => facts.exit_code = Some(i64::from(c)),
        Exit::Signal(s) if !done.timed_out => facts.signal = Some(i64::from(s)),
        Exit::Signal(_) => {}
        Exit::Unknown => facts.error = Some("the child could not be reaped".into()),
    }
    facts.timed_out = done.timed_out;
    facts.bytes = i64::try_from(done.stdout_bytes).unwrap_or(i64::MAX);
    facts.hash = done.stdout_sha256.map(|h| crate::sys::hex(&h));
    if done.truncated || done.pipes_held {
        crate::log::warn(
            "job output incomplete",
            &[
                ("job", &job),
                ("run", &facts.run),
                ("over_cap", &done.truncated),
                ("held_open", &done.pipes_held),
            ],
        );
    }
    Ok(done.cancelled)
}

fn broken_emit(job: &str, run: i64, started_ms: i64) -> Emit {
    Emit::new(EventKind::RunBroken, Severity::Warning, true, job)
        .with("job", job)
        .with("run", run)
        .with("started", instant_text(started_ms))
}

/// Appends the run's events, then commits memory and run state. `job: None` (removed from the config)
/// only marks the run applied.
async fn apply(
    ctx: &Ctx,
    job: Option<&Arc<Job>>,
    facts: &Facts,
) -> Result<Vec<EventRecord>, JobsError> {
    let Some(job) = job else {
        ctx.store
            .apply(Apply {
                run: Some(facts.run),
                ..Apply::default()
            })
            .await?;
        return Ok(Vec::new());
    };
    let mut state = Apply {
        run: Some(facts.run),
        ..Apply::default()
    };
    let mut emits = Vec::new();
    let mut delete_own = false;
    let mut delete_earlier_bad = false;
    match &job.trigger {
        Trigger::Cron { .. } | Trigger::Once { .. } | Trigger::Every { .. } => {
            emits.push(timed_emit(&job.name, facts));
        }
        Trigger::Probe {
            fire,
            skip_identical,
            ..
        } => {
            let (name, dry) = (job.name.clone(), facts.dry);
            let rows = ctx.store.read(move |r| r.memory(&name, dry)).await?;
            let mut memory = Memory::new(&job.name, facts.dry, rows);
            let class = facts.class(true);
            let verdict = decide(
                &mut memory,
                &job.name,
                &class,
                facts.hash.as_deref(),
                *fire,
                *skip_identical,
            );
            if verdict.fired {
                state.outcome = Some("fired".into());
            }
            delete_own = matches!(class, Class::Success) && !verdict.fired;
            delete_earlier_bad = !matches!(class, Class::Success);
            emits.extend(verdict.emits.into_iter().map(|e| decorate(e, facts)));
            state.memory = memory.into_writes();
        }
    }
    let at = now_ms();
    let events: Vec<_> = emits
        .into_iter()
        .map(|e| {
            build(
                &job.name,
                e,
                &job.templates,
                facts.dry,
                &job.destinations,
                at,
            )
        })
        .collect();
    let records = ctx.stream.append(events).await?;
    #[cfg(test)]
    super::fault::point("after_event_append");
    if delete_own {
        remove_outputs(
            facts.stdout_path.as_deref(),
            facts.stdout_path.as_deref().map(err_path).as_deref(),
        );
        state.outputs_dropped.push(facts.run);
    }
    if delete_earlier_bad {
        let (name, dry, run) = (job.name.clone(), facts.dry, facts.run);
        for old in ctx
            .store
            .read(move |r| r.kept_bad_runs(&name, dry, run))
            .await?
        {
            remove_outputs(old.stdout_path.as_deref(), old.stderr_path.as_deref());
            state.outputs_dropped.push(old.id);
        }
    }
    ctx.store.apply(state).await?;
    Ok(records)
}

fn err_path(stdout: &str) -> String {
    stdout
        .strip_suffix(".out")
        .map_or_else(|| format!("{stdout}.err"), |stem| format!("{stem}.err"))
}

fn remove_outputs(stdout: Option<&str>, stderr: Option<&str>) {
    for path in [stdout, stderr].into_iter().flatten() {
        let _ = std::fs::remove_file(path);
    }
}

/// Adds the output (inline up to 64 KiB, else its path) to a fired probe or a timed run.
fn decorate(mut emit: Emit, facts: &Facts) -> Emit {
    if matches!(emit.kind, EventKind::ProbeFired | EventKind::RunDone) {
        attach_output(&mut emit, facts);
    }
    if emit.kind == EventKind::ProbeFired {
        emit = emit
            .with("bytes", facts.bytes)
            .with("log", facts.stdout_path.as_deref().unwrap_or("-"))
            .with("scheduled", instant_text(facts.scheduled_ms));
    }
    if emit.kind == EventKind::ProbeError {
        emit = emit
            .with("run", facts.run)
            .with("log", facts.stdout_path.as_deref().unwrap_or("-"));
    }
    emit
}

fn attach_output(emit: &mut Emit, facts: &Facts) {
    let Some(path) = facts.stdout_path.as_deref() else {
        return;
    };
    match read_private(Path::new(path), BODY_INLINE_MAX) {
        Ok(Some(bytes)) if !bytes.is_empty() => {
            emit.body = Some(String::from_utf8_lossy(&bytes).into_owned());
        }
        Ok(_) => {}
        Err(_) => emit.body_path = Some(path.to_string()),
    }
}

fn timed_emit(job: &str, facts: &Facts) -> Emit {
    let class = facts.class(false);
    let outcome = match &class {
        Class::Success => "succeeded".to_string(),
        Class::Failure(code) => format!("failed with exit {code}"),
        Class::Error(reason) if reason == "time limit" => "timed out".to_string(),
        Class::Error(reason) => format!("error: {reason}"),
    };
    let severity = match &class {
        Class::Success => Severity::Info,
        Class::Failure(_) => Severity::Warning,
        Class::Error(reason) if reason == "time limit" => Severity::Warning,
        // The command could not run at all.
        Class::Error(_) => Severity::Error,
    };
    let mut emit = Emit::new(EventKind::RunDone, severity, true, job)
        .with("job", job)
        .with("outcome", outcome)
        .with(
            "exit",
            facts
                .exit_code
                .map_or_else(|| "-".to_string(), |c| c.to_string()),
        )
        .with(
            "signal",
            facts
                .signal
                .map_or_else(|| "-".to_string(), |s| s.to_string()),
        )
        .with("timed_out", if facts.timed_out { "yes" } else { "no" })
        .with("bytes", facts.bytes)
        .with("log", facts.stdout_path.as_deref().unwrap_or("-"))
        .with("scheduled", instant_text(facts.scheduled_ms));
    attach_output(&mut emit, facts);
    emit
}

/// What a condition run means for the job.
#[derive(Debug)]
pub(super) struct Verdict {
    pub(super) emits: Vec<Emit>,
    pub(super) fired: bool,
}

/// WS req. 61: errors grow the streak and warn once per distinct reason; a failure primes; a success
/// fires when the rule is "every" or the job was primed (a new job starts primed), unless its output
/// equals the last fired output and identical output is skipped. The first non-error after errors
/// emits one recovery note.
pub(super) fn decide(
    memory: &mut Memory,
    job: &str,
    class: &Class,
    hash: Option<&str>,
    fire: Fire,
    skip_identical: bool,
) -> Verdict {
    let mut emits = Vec::new();
    let mut fired = false;
    if let Class::Error(reason) = class {
        let streak = memory
            .get(job, "errors")
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0)
            + 1;
        memory.transition(job, "errors", &streak.to_string());
        if memory.transition(job, "error", reason) {
            emits.push(
                Emit::new(EventKind::ProbeError, Severity::Warning, false, job)
                    .with("job", job)
                    .with("reason", reason),
            );
        }
        return Verdict { emits, fired };
    }
    if memory.clear(job, "error").is_some() {
        let errors = memory.clear(job, "errors").unwrap_or_else(|| "1".into());
        emits.push(
            Emit::new(EventKind::ProbeRecovered, Severity::Info, false, job)
                .with("job", job)
                .with("errors", errors),
        );
    }
    let primed = memory.get(job, "primed") != Some("0");
    match class {
        Class::Failure(_) => {
            memory.transition(job, "primed", "1");
        }
        Class::Success => {
            let same = skip_identical && hash.is_some() && memory.get(job, "fired_hash") == hash;
            if (fire == Fire::Every || primed) && !same {
                fired = true;
                if let Some(h) = hash {
                    memory.transition(job, "fired_hash", h);
                }
                emits.push(
                    Emit::new(EventKind::ProbeFired, Severity::Attention, true, job)
                        .with("job", job),
                );
            }
            memory.transition(job, "primed", "0");
        }
        Class::Error(_) => {}
    }
    Verdict { emits, fired }
}

/// Start-up recovery (WS req. 67): runs in progress become broken off with their jobs on hold; runs
/// that finished without their outcome applied are applied now (their events may repeat).
pub(super) async fn recover(ctx: &Ctx) -> Result<(), JobsError> {
    let found = ctx.store.recover(now_ms()).await?;
    let config = ctx.config();
    for run in &found.broken {
        let job = config.jobs.get(&run.job);
        let (templates, dests) = job.map_or((&[][..], &[][..]), |j| {
            (&j.templates[..], &j.destinations[..])
        });
        let event = build(
            &run.job,
            broken_emit(&run.job, run.id, run.started_ms),
            templates,
            run.dry,
            dests,
            now_ms(),
        );
        ctx.stream.append(vec![event]).await?;
    }
    for run in &found.unapplied {
        let job = config
            .jobs
            .get(&run.job)
            .filter(|j| matches!(j.work, Work::Command { .. }) && j.trigger.kind() == kind_of(run));
        apply(ctx, job, &Facts::from_record(run)).await?;
    }
    if found.uncertain_deliveries > 0 {
        crate::log::warn(
            "deliveries left uncertain by the previous hub",
            &[("count", &found.uncertain_deliveries)],
        );
    }
    Ok(())
}

/// Probe runs keep stderr apart; that tells the kinds apart in a stored record.
fn kind_of(run: &RunRecord) -> &'static str {
    if run.stderr_path.is_some() {
        "probe"
    } else {
        "timed"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::tests::{ctx_in, ctx_with, probe_config};
    use crate::store::Reads;

    fn memory() -> Memory {
        Memory::new("p", false, Vec::new())
    }

    fn kinds(v: &Verdict) -> Vec<EventKind> {
        v.emits.iter().map(|e| e.kind).collect()
    }

    #[test]
    fn fires_on_change_to_success() {
        let mut m = memory();
        let ok = Class::Success;
        let v = decide(&mut m, "p", &ok, Some("a"), Fire::Change, false);
        assert!(v.fired, "a new job starts primed");
        assert!(!decide(&mut m, "p", &ok, Some("b"), Fire::Change, false).fired);
        assert!(!decide(&mut m, "p", &Class::Failure(1), None, Fire::Change, false).fired);
        assert!(decide(&mut m, "p", &ok, Some("c"), Fire::Change, false).fired);
        assert!(decide(&mut m, "p", &ok, Some("c"), Fire::Every, false).fired);
    }

    #[test]
    fn skips_identical_output() {
        let mut m = memory();
        assert!(decide(&mut m, "p", &Class::Success, Some("h"), Fire::Every, true).fired);
        assert!(!decide(&mut m, "p", &Class::Success, Some("h"), Fire::Every, true).fired);
        assert!(decide(&mut m, "p", &Class::Success, Some("i"), Fire::Every, true).fired);
    }

    #[test]
    fn errors_warn_once_per_reason_and_recover_once() {
        let mut m = memory();
        let late = Class::Error("time limit".into());
        assert_eq!(
            kinds(&decide(&mut m, "p", &late, None, Fire::Change, false)),
            [EventKind::ProbeError]
        );
        assert_eq!(
            kinds(&decide(&mut m, "p", &late, None, Fire::Change, false)),
            []
        );
        let other = Class::Error("killed by signal 9".into());
        assert_eq!(
            kinds(&decide(&mut m, "p", &other, None, Fire::Change, false)),
            [EventKind::ProbeError]
        );
        assert_eq!(m.get("p", "errors"), Some("3"));
        // Errors leave priming as it was: the job is still primed, so this success fires too.
        let v = decide(&mut m, "p", &Class::Success, Some("x"), Fire::Change, false);
        assert_eq!(
            kinds(&v),
            [EventKind::ProbeRecovered, EventKind::ProbeFired]
        );
        assert_eq!(
            v.emits[0]
                .values
                .iter()
                .find(|(k, _)| *k == "errors")
                .unwrap()
                .1,
            "3"
        );
        let failure = decide(&mut m, "p", &Class::Failure(2), None, Fire::Change, false);
        assert_eq!(kinds(&failure), []);
    }

    #[test]
    fn facts_survive_the_store_round_trip() {
        let record = RunRecord {
            id: 4,
            job: "p".into(),
            dry: false,
            scheduled_ms: 1,
            started_ms: 2,
            pid: None,
            ended_ms: Some(3),
            exit_code: None,
            signal: None,
            timed_out: false,
            state: "finished".into(),
            outcome: Some("error: cannot start x: no such file".into()),
            stdout_path: Some("/f/1.out".into()),
            stderr_path: Some("/f/1.err".into()),
            bytes: Some(0),
            output_hash: None,
            kept: true,
        };
        let f = Facts::from_record(&record);
        assert_eq!(
            f.class(true),
            Class::Error("cannot start x: no such file".into())
        );
        assert_eq!(err_path("/f/1.out"), "/f/1.err");
        assert_eq!(kind_of(&record), "probe");
    }

    #[tokio::test]
    async fn timed_run_reports_output_and_failure() {
        let cfg = "[jobs.t]\nkind = \"once\"\nat = \"2030-01-01T00:00:00Z\"\nargv = [\"sh\", \"-c\", \"echo hello; exit 3\"]\ncwd = \"/\"\n";
        let (ctx, _stop, _dir) = ctx_with(cfg, "run-timed");
        let job = ctx.config().jobs.get("t").cloned().unwrap();
        let events = execute(
            &ctx,
            Request {
                job,
                scheduled_ms: 5,
                dry: false,
                consume: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(
            (e.kind.as_str(), e.severity.as_str(), e.urgent),
            ("run.done", "warning", true)
        );
        assert_eq!(e.text, "t: failed with exit 3, 6 bytes of output");
        assert_eq!(e.body.as_deref(), Some("hello\n"));
        let run = ctx.store.read(Reads::last_runs).await.unwrap().remove(0);
        assert_eq!((run.state.as_str(), run.exit_code), ("applied", Some(3)));
    }

    #[tokio::test]
    async fn probe_timeout_is_an_error_and_success_output_is_deleted() {
        let (ctx, _stop, _dir) = ctx_with(
            &probe_config(
                r#"["sh", "-c", "sleep 5"]"#,
                "timeout = \"1s\"\nskip_identical = true\n",
            ),
            "run-probe",
        );
        let job = ctx.config().jobs.get("p").cloned().unwrap();
        let req = |job: &Arc<Job>| Request {
            job: Arc::clone(job),
            scheduled_ms: 1,
            dry: false,
            consume: false,
        };
        let events = execute(&ctx, req(&job)).await.unwrap();
        assert_eq!(events[0].kind, "probe.error");
        assert!(events[0].text.ends_with("probe error: time limit"));
        // Same job with a quick command: first success fires (recovery note first), second is identical.
        let quick = Arc::new(Job {
            work: Work::Command {
                argv: vec!["echo".into(), "same".into()],
                cwd: "/".into(),
            },
            ..(*job).clone()
        });
        let kinds: Vec<String> = execute(&ctx, req(&quick))
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(kinds, ["probe.recovered", "probe.fired"]);
        assert_eq!(execute(&ctx, req(&quick)).await.unwrap(), []);
        let last = ctx.store.read(Reads::last_runs).await.unwrap().remove(0);
        assert!(!last.kept);
        assert!(!Path::new(last.stdout_path.as_deref().unwrap()).exists());
    }

    /// Child half of the crash test: dies at the fault point between event append and state commit.
    #[test]
    #[ignore = "run by crash_between_append_and_commit as a child process"]
    fn crash_child() {
        let Ok(dir) = std::env::var("CATALY_FAULT_DIR") else {
            return;
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let dir = std::path::PathBuf::from(dir);
            let (store, _writer) =
                crate::store::Store::open(&dir.join("cataly.db"), &dir.join("backup")).unwrap();
            let (ctx, _stop) = ctx_in(store, &dir, &probe_config(r#"["echo", "up"]"#, ""));
            let job = ctx.config().jobs.get("p").cloned().unwrap();
            let _ = execute(
                &ctx,
                Request {
                    job,
                    scheduled_ms: 1,
                    dry: false,
                    consume: false,
                },
            )
            .await;
        });
        unreachable!("the fault point aborts the process");
    }

    #[tokio::test]
    async fn crash_between_append_and_commit() {
        let dir = crate::sys::fs::tests::scratch("crash");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "jobs::run::tests::crash_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("CATALY_FAULT", "after_event_append")
            .env("CATALY_FAULT_DIR", dir.as_os_str())
            .status()
            .unwrap();
        assert!(!status.success(), "the child must die at the fault point");
        let (store, _writer) =
            crate::store::Store::open(&dir.join("cataly.db"), &dir.join("backup")).unwrap();
        let before = store.read(|r| r.events_after(0, 10)).await.unwrap();
        assert_eq!(
            before.len(),
            1,
            "the event entered the stream before the crash"
        );
        let memory = store.read(|r| r.memory("p", false)).await.unwrap();
        assert_eq!(memory, [], "state was not committed");
        let (ctx, _stop) = ctx_in(store, &dir, &probe_config(r#"["echo", "up"]"#, ""));
        recover(&ctx).await.unwrap();
        let after = ctx.store.read(|r| r.events_after(0, 10)).await.unwrap();
        let kinds: Vec<&str> = after.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, ["probe.fired", "probe.fired"], "repeated, none lost");
        let run = ctx.store.read(Reads::last_runs).await.unwrap().remove(0);
        assert_eq!(run.state, "applied");
        let memory = ctx.store.read(|r| r.memory("p", false)).await.unwrap();
        assert!(memory.iter().any(|(_, c, v)| c == "primed" && v == "0"));
    }
}
