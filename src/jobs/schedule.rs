//! The scheduler: one task holding a min-heap of due times (DESIGN 6.2, WS req. 56-58). It sleeps until
//! the next due time, waking early for commands and config changes. Each sleep is capped at 60 s so a
//! wall-clock jump is noticed within a minute; due times are wall-clock instants.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::config::{Config, Job, Trigger};
use crate::store::{JobPatch, JobRegistration, JobRow};
use crate::sys::now_ms;

use super::cron::Cron;
use super::event::{Emit, EventKind, Severity, build};
use super::{Command, Ctx, JobsError, duration_ms, log_failure};

/// An occurrence this late or later is skipped, never replayed.
const LATE_MS: i64 = 60_000;
/// Most skipped occurrences counted per evaluation.
const SKIP_COUNT_MAX: u64 = 1000;
/// Longest single sleep.
const MAX_SLEEP: Duration = Duration::from_secs(60);
/// Superseded due times the heap may hold beyond two per job before they are dropped (each
/// reschedule, config reload or command leaves the earlier one until its time comes).
const HEAP_SLACK: usize = 16;

/// What a cron evaluation decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CronDue {
    pub(super) run: Option<i64>,
    pub(super) late: u64,
    pub(super) overlap: u64,
    pub(super) next: Option<i64>,
}

/// Walks every occurrence from `due` up to `now`: one less than 60 s late runs unless the previous run
/// is unfinished (then it is skipped as an overlap); 60 s or more late is skipped; at most 1,000 are
/// counted, after which the walk jumps to the first occurrence after now.
pub(super) fn cron_due(cron: &Cron, tz: &TimeZone, due: i64, now: i64, busy: bool) -> CronDue {
    let mut out = CronDue {
        run: None,
        late: 0,
        overlap: 0,
        next: None,
    };
    let next_after = |ms: i64| {
        Timestamp::from_millisecond(ms)
            .ok()
            .and_then(|t| cron.next_after(tz, t))
            .map(Timestamp::as_millisecond)
    };
    let mut occurrence = Some(due);
    while let Some(at) = occurrence.filter(|at| *at <= now) {
        if now - at >= LATE_MS {
            out.late += 1;
        } else if busy || out.run.is_some() {
            out.overlap += 1;
        } else {
            out.run = Some(at);
        }
        occurrence = if out.late + out.overlap >= SKIP_COUNT_MAX {
            next_after(now)
        } else {
            next_after(at)
        };
    }
    out.next = occurrence;
    out
}

/// A condition job's next probe: the later of finish and scheduled time, plus the interval. A
/// backwards clock step therefore never probes early.
pub(super) fn probe_next(scheduled: i64, finished: i64, every: Duration) -> i64 {
    scheduled.max(finished).saturating_add(duration_ms(every))
}

/// The first due time of a newly registered (or changed) job.
fn fresh_due(job: &Job, now: i64) -> Option<i64> {
    match &job.trigger {
        Trigger::Cron { cron, zone, .. } => Timestamp::from_millisecond(now)
            .ok()
            .and_then(|t| cron.next_after(zone, t))
            .map(Timestamp::as_millisecond),
        Trigger::Once { at } => Some(at.as_millisecond()),
        // The first probe or owner tick runs at creation.
        Trigger::Probe { .. } | Trigger::Every { .. } => Some(now),
    }
}

struct Slot {
    job: Arc<Job>,
    row: JobRow,
    generation: u64,
}

#[derive(Default)]
struct Sched {
    slots: HashMap<String, Slot>,
    heap: BinaryHeap<Reverse<(i64, u64, String)>>,
    generation: u64,
}

pub(super) async fn scheduler(ctx: Arc<Ctx>) -> Result<(), JobsError> {
    let mut commands = ctx.commands.lock().await;
    let mut config_rx = ctx.config.clone();
    let mut shutdown = ctx.shutdown.clone();
    let mut sched = Sched::default();
    let mut config_open = true;
    let initial = Arc::clone(&config_rx.borrow_and_update());
    sched.reconcile(&ctx, &initial, None).await?;
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        let wait = sched.sleep_for(now_ms());
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            command = commands.recv() => match command {
                Some(command) => sched.command(&ctx, command).await?,
                None => return Ok(()),
            },
            changed = config_rx.changed(), if config_open => {
                if changed.is_err() {
                    // No more config changes can come; keep scheduling what is configured.
                    config_open = false;
                    continue;
                }
                let config = Arc::clone(&config_rx.borrow_and_update());
                let previous: HashMap<String, Arc<Job>> =
                    sched.slots.iter().map(|(k, s)| (k.clone(), Arc::clone(&s.job))).collect();
                sched.reconcile(&ctx, &config, Some(&previous)).await?;
            }
            _ = shutdown.changed() => {}
        }
        if !*shutdown.borrow() {
            sched.fire_due(&ctx, now_ms()).await?;
        }
    }
}

impl Sched {
    fn sleep_for(&self, now: i64) -> Duration {
        self.heap.peek().map_or(MAX_SLEEP, |Reverse((due, _, _))| {
            Duration::from_millis(u64::try_from(due.saturating_sub(now)).unwrap_or(0))
                .min(MAX_SLEEP)
        })
    }

    fn schedulable(slot: &Slot) -> bool {
        slot.job.enabled && !slot.row.held && !slot.row.consumed && slot.row.next_due_ms.is_some()
    }

    fn put(&mut self, name: &str, job: Arc<Job>, row: JobRow) {
        self.generation += 1;
        let slot = Slot {
            job,
            row,
            generation: self.generation,
        };
        if Self::schedulable(&slot)
            && let Some(due) = slot.row.next_due_ms
        {
            self.heap
                .push(Reverse((due, slot.generation, name.to_string())));
        }
        self.slots.insert(name.to_string(), slot);
        if self.heap.len() > 2 * self.slots.len() + HEAP_SLACK {
            let slots = &self.slots;
            self.heap.retain(|Reverse((_, generation, name))| {
                slots.get(name).is_some_and(|s| s.generation == *generation)
            });
        }
    }

    /// Registers every configured job; with `previous`, a job switched from disabled to enabled restarts
    /// its schedule from now, and a job going live discards its dry-run state.
    async fn reconcile(
        &mut self,
        ctx: &Ctx,
        config: &Config,
        previous: Option<&HashMap<String, Arc<Job>>>,
    ) -> Result<(), JobsError> {
        let now = now_ms();
        self.slots.retain(|name, _| config.jobs.contains_key(name));
        for (name, job) in &config.jobs {
            let fresh = fresh_due(job, now);
            let mut row = ctx
                .store
                .register_job(JobRegistration {
                    name: name.clone(),
                    kind: job.kind(),
                    trigger: job.trigger.canonical(),
                    next_due_ms: fresh,
                    now_ms: now,
                })
                .await?;
            if let Some(old) = previous.and_then(|p| p.get(name)) {
                if !old.enabled && job.enabled && !matches!(job.trigger, Trigger::Once { .. }) {
                    let patch = JobPatch {
                        next_due_ms: Some(fresh),
                        ..JobPatch::default()
                    };
                    row = ctx
                        .store
                        .patch_job(name.clone(), patch)
                        .await?
                        .unwrap_or(row);
                }
                if old.dry_run && !job.dry_run {
                    ctx.store.discard_dry(name.clone()).await?;
                }
            }
            self.put(name, Arc::clone(job), row);
        }
        Ok(())
    }

    async fn command(&mut self, ctx: &Ctx, command: Command) -> Result<(), JobsError> {
        match command {
            Command::Refresh(name) => self.refresh(ctx, &name).await,
            Command::Finished {
                name,
                scheduled_ms,
                finished_ms,
            } => {
                let Some(slot) = self.slots.get(&name) else {
                    return Ok(());
                };
                let Some(every) = slot.job.trigger.every() else {
                    return Ok(());
                };
                let next = probe_next(scheduled_ms, finished_ms, every);
                let patch = JobPatch {
                    next_due_ms: Some(Some(next)),
                    ..JobPatch::default()
                };
                ctx.store.patch_job(name.clone(), patch).await?;
                self.refresh(ctx, &name).await
            }
        }
    }

    async fn refresh(&mut self, ctx: &Ctx, name: &str) -> Result<(), JobsError> {
        let Some(job) = self.slots.get(name).map(|s| Arc::clone(&s.job)) else {
            return Ok(());
        };
        let key = name.to_string();
        if let Some(row) = ctx.store.read(move |r| r.job(&key)).await? {
            self.put(name, job, row);
        }
        Ok(())
    }

    async fn fire_due(&mut self, ctx: &Arc<Ctx>, now: i64) -> Result<(), JobsError> {
        while let Some(Reverse((due, generation, name))) = self.heap.peek().cloned() {
            if due > now {
                break;
            }
            self.heap.pop();
            let Some(slot) = self.slots.get(&name) else {
                continue;
            };
            if slot.generation != generation || !Self::schedulable(slot) {
                continue;
            }
            let job = Arc::clone(&slot.job);
            self.due(ctx, &job, due, now).await?;
        }
        Ok(())
    }

    async fn due(
        &mut self,
        ctx: &Arc<Ctx>,
        job: &Arc<Job>,
        due: i64,
        now: i64,
    ) -> Result<(), JobsError> {
        let name = job.name.as_str();
        let busy = ctx.is_running(name);
        let (run_at, next, skips, consume) = match &job.trigger {
            Trigger::Cron { cron, zone, .. } => {
                let d = cron_due(cron, zone, due, now, busy);
                let mut skips = Vec::new();
                if d.late > 0 {
                    skips.push((d.late, "60 s or more late"));
                }
                if d.overlap > 0 {
                    skips.push((d.overlap, "previous run unfinished"));
                }
                (d.run, d.next, skips, false)
            }
            Trigger::Once { .. } if now - due >= LATE_MS => {
                (None, None, vec![(1, "60 s or more late")], true)
            }
            Trigger::Once { .. } => (Some(due), None, Vec::new(), true),
            Trigger::Probe { every, .. } | Trigger::Every { every } if busy => (
                None,
                Some(now.saturating_add(duration_ms(*every))),
                vec![(1, "previous run unfinished")],
                false,
            ),
            // The next probe or tick is scheduled when this one finishes.
            Trigger::Probe { .. } | Trigger::Every { .. } => (Some(due), None, Vec::new(), false),
        };
        let skipped: u64 = skips.iter().map(|(n, _)| n).sum();
        if !skips.is_empty() {
            let events = skips
                .iter()
                .map(|(count, reason)| {
                    let emit = Emit::new(EventKind::RunSkipped, Severity::Info, false, name)
                        .with("job", name)
                        .with("count", count)
                        .with("reason", reason);
                    build(
                        name,
                        emit,
                        &job.templates,
                        job.dry_run,
                        &job.destinations,
                        now,
                    )
                })
                .collect();
            ctx.stream.append(events).await?;
        }
        let probe = job.trigger.every().is_some();
        if run_at.is_some() && (consume || probe) {
            // Nothing is written here. A one-shot is consumed in the same transaction that records its
            // run, before the child starts; a probe keeps its stored due time until it finishes, so a
            // crash mid-run probes again at the next start.
            if let Some(slot) = self.slots.get_mut(name) {
                slot.row.consumed |= consume;
                slot.row.held |= consume;
                slot.row.next_due_ms = None;
            }
        } else {
            let patch = JobPatch {
                next_due_ms: Some(next),
                add_skipped: i64::try_from(skipped).unwrap_or(i64::MAX),
                consumed: consume.then_some(true),
                held: consume.then_some(true),
                ..JobPatch::default()
            };
            if let Some(row) = ctx.store.patch_job(name.to_string(), patch).await? {
                self.put(name, Arc::clone(job), row);
            }
        }
        if let Some(scheduled) = run_at {
            start_run(ctx, job, scheduled, consume);
        }
        Ok(())
    }
}

/// Starts a run task; the registry keeps runs of one job from overlapping.
fn start_run(ctx: &Arc<Ctx>, job: &Arc<Job>, scheduled_ms: i64, consume: bool) {
    if !ctx.claim(&job.name) {
        return;
    }
    let ctx = Arc::clone(ctx);
    let job = Arc::clone(job);
    tokio::spawn(async move {
        let dry = job.dry_run;
        let probe = job.trigger.every().is_some();
        if let Err(e) = super::perform(&ctx, &job, scheduled_ms, dry, consume).await {
            log_failure("job run failed", &job.name, &e);
        }
        ctx.release(&job.name);
        if probe {
            let finished = Command::Finished {
                name: job.name.clone(),
                scheduled_ms,
                finished_ms: now_ms(),
            };
            let _ = ctx.commands_tx.send(finished).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::cron::parse;
    use crate::jobs::tests::ctx_with;

    /// Reschedules of a job with a far due time leave no pile of superseded heap entries.
    #[tokio::test]
    async fn superseded_due_times_are_dropped() {
        let cfg = "[jobs.m]\nkind = \"cron\"\ncron = \"0 3 1 * *\"\nzone = \"UTC\"\nargv = [\"true\"]\ncwd = \"/\"\n";
        let (ctx, _stop, _dir) = ctx_with(cfg, "sched-heap");
        let job = Arc::clone(&ctx.config.borrow().jobs["m"]);
        let mut sched = Sched::default();
        for n in 0..1000 {
            let row = JobRow {
                name: "m".into(),
                kind: "cron".into(),
                trigger: String::new(),
                held: false,
                consumed: false,
                needs_ack: false,
                next_due_ms: Some(now_ms() + 86_400_000 + n),
                skipped: 0,
                created_ms: 0,
            };
            sched.put("m", Arc::clone(&job), row);
        }
        assert!(sched.heap.len() <= 2 + HEAP_SLACK, "{}", sched.heap.len());
        let current = sched.slots["m"].generation;
        assert!(sched.heap.iter().any(|Reverse((_, g, _))| *g == current));
    }

    fn ms(s: &str) -> i64 {
        s.parse::<Timestamp>().unwrap().as_millisecond()
    }

    #[test]
    fn cron_runs_slightly_late_and_skips_very_late() {
        let c = parse("* * * * *").unwrap();
        let utc = TimeZone::UTC;
        let due = ms("2026-01-01T10:00:00Z");
        let d = cron_due(&c, &utc, due, due + 59_999, false);
        assert_eq!(
            (d.run, d.late, d.overlap, d.next),
            (Some(due), 0, 0, Some(due + 60_000))
        );
        let d = cron_due(&c, &utc, due, due + 60_000, false);
        assert_eq!(
            (d.run, d.late),
            (Some(due + 60_000), 1),
            "the 10:00 is skipped, 10:01 is on time"
        );
        let d = cron_due(&c, &utc, due, due + 3 * 3_600_000 + 30_000, false);
        assert_eq!((d.late, d.run.is_some()), (180, true));
        let d = cron_due(&c, &utc, due, due + 10_000, true);
        assert_eq!((d.run, d.overlap), (None, 1), "runs never overlap");
        let d = cron_due(&c, &utc, due, due + 30 * 86_400_000, false);
        assert_eq!(d.late, SKIP_COUNT_MAX);
        assert!(d.next.unwrap() > due + 30 * 86_400_000);
    }

    #[test]
    fn probe_next_never_early() {
        let every = Duration::from_secs(30);
        assert_eq!(probe_next(1_000, 5_000, every), 35_000);
        assert_eq!(
            probe_next(9_000, 5_000, every),
            39_000,
            "a clock stepped back keeps the scheduled time"
        );
    }

    #[tokio::test]
    async fn one_shot_is_consumed_before_it_runs() {
        let at = Timestamp::from_millisecond(now_ms() + 200).unwrap();
        let cfg = format!(
            "[jobs.o]\nkind = \"once\"\nat = \"{}\"\nargv = [\"sh\", \"-c\", \"sleep 0.5\"]\ncwd = \"/\"\n",
            at.strftime("%Y-%m-%dT%H:%M:%S%.3fZ")
        );
        let (ctx, stop, _dir) = ctx_with(&cfg, "sched-once");
        let task = tokio::spawn(scheduler(Arc::clone(&ctx)));
        let mut seen_running = false;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let recorded = ctx
                .store
                .read(crate::store::Reads::last_runs)
                .await
                .unwrap();
            if ctx.is_running("o") && recorded.first().is_some_and(|r| r.state == "running") {
                let row = ctx.store.read(|r| r.job("o")).await.unwrap().unwrap();
                assert!(
                    row.consumed && row.held,
                    "consumed in the run's own first record"
                );
                seen_running = true;
                break;
            }
        }
        assert!(seen_running);
        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn late_one_shot_is_skipped_and_probe_runs_at_creation() {
        let cfg = "[jobs.o]\nkind = \"once\"\nat = \"2020-01-01T00:00:00Z\"\nargv = [\"true\"]\ncwd = \"/\"\n\
                   [jobs.p]\nkind = \"probe\"\nevery = \"1h\"\nargv = [\"echo\", \"x\"]\ncwd = \"/\"\n";
        let (ctx, stop, _dir) = ctx_with(cfg, "sched-late");
        let before = now_ms();
        let task = tokio::spawn(scheduler(Arc::clone(&ctx)));
        let mut kinds = Vec::new();
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            kinds = ctx
                .store
                .read(|r| r.events_after(0, 10))
                .await
                .unwrap()
                .into_iter()
                .map(|e| e.kind)
                .collect();
            if kinds.len() >= 2 {
                break;
            }
        }
        kinds.sort();
        assert_eq!(kinds, ["probe.fired", "run.skipped"]);
        let once = ctx.store.read(|r| r.job("o")).await.unwrap().unwrap();
        assert!(once.consumed && once.skipped == 1);
        // The probe's next run is one interval after it finished. The finish reaches the
        // scheduler after the fired event, so wait for the reschedule; the creation due time is
        // below `before + 1h`.
        let hour = 3_600_000;
        let mut next = None;
        for _ in 0..500 {
            next = ctx
                .store
                .read(|r| r.job("p"))
                .await
                .unwrap()
                .unwrap()
                .next_due_ms;
            if next.is_some_and(|n| n >= before + hour) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let next = next.unwrap();
        assert!(
            next >= before + hour,
            "rescheduled one interval after the run"
        );
        assert!(
            next <= now_ms() + hour,
            "not later than one interval from now"
        );
        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
    }
}
