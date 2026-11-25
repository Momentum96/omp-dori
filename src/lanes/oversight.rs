//! Reports, steps and oversight (EVR-01 to EVR-20; WS-10 to WS-21; COORD-06 to COORD-09): report
//! acceptance with evidence novelty, the lead's wake line and the thread relay, step records, and the
//! per-lane watch with reminders, the stall alert, the stuck verdict, ceilings and dialog holds.
//! Oversight sleeps until the next lane deadline (at most the sweep) and wakes early on events.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::watch;

use super::evidence::{self, ReportInput};
use super::record::{Record, State};
use super::{Lanes, Posted};
use crate::config::lanes::Oversight;
use crate::host::{Control, Runtime, Status};
use crate::jobs::event::{Emit, EventKind, Severity};
use crate::log;
use crate::store::lanes::{LaneDone, LaneWrite, ReportRow};
use crate::sys::now_ms;

/// Kinds that post to the thread at once; progress waits for the quiet rule (DESIGN 12.2 C7).
const AT_ONCE: [&str; 4] = ["stuck", "waiting", "stopped", "finished"];
/// Kinds that stop reminders and the stuck verdict (WS req. 12, 22).
const OBSTACLE: [&str; 4] = ["stuck", "waiting", "stopped", "finished"];

/// What oversight decides for one lane at one moment; pure so the rules are testable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Due {
    /// Reminder number to send now (1 or 2).
    pub(crate) remind: Option<u8>,
    pub(crate) stall: bool,
    pub(crate) stuck: bool,
    /// Earliest future deadline, for the sleep.
    pub(crate) next_ms: Option<i64>,
}

/// The facts one evaluation reads.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Facts<'a> {
    pub(crate) now: i64,
    pub(crate) progress_ms: i64,
    pub(crate) activity_ms: i64,
    pub(crate) idle: bool,
    pub(crate) working_since: Option<i64>,
    pub(crate) last_kind: Option<&'a str>,
    /// The work sample is complete (only asked when the other stuck conditions hold).
    pub(crate) sample_complete: bool,
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// COORD req. 8-10 and WS req. 22 for a lane that is running or rejected.
pub(crate) fn decide(w: &super::record::Watch, f: &Facts<'_>, o: &Oversight) -> Due {
    let mut due = Due::default();
    let next = |at: i64, due: &mut Due| {
        if at > f.now {
            due.next_ms = Some(due.next_ms.map_or(at, |n: i64| n.min(at)));
        }
    };
    let progress = f.progress_ms.max(w.round_ms);
    let stall_at = progress.saturating_add(ms(o.stall));
    if !w.alerted {
        if f.now >= stall_at {
            due.stall = true;
        } else {
            next(stall_at, &mut due);
        }
    }
    let obstacle = f.last_kind.is_some_and(|k| OBSTACLE.contains(&k));
    if !w.alerted && !due.stall && w.reminders < 2 && !obstacle {
        let quiet_from = progress.max(f.activity_ms);
        let after = if w.reminders == 0 {
            o.first_reminder
        } else {
            o.second_reminder
        };
        let at = quiet_from.saturating_add(ms(after));
        if f.idle && f.now >= at {
            due.remind = Some(w.reminders + 1);
        } else {
            next(at, &mut due);
        }
    }
    if !w.stuck
        && !f.last_kind.is_some_and(|k| OBSTACLE.contains(&k))
        && let Some(since) = f.working_since
    {
        let at = since
            .saturating_add(ms(o.busy))
            .max(progress.saturating_add(ms(o.quiet)));
        if f.now >= at {
            due.stuck = f.sample_complete;
        } else {
            next(at, &mut due);
        }
    }
    due
}

impl<R: Runtime> Lanes<R> {
    /// One report (EVR req. 1-8, 14-22): checked, evidence read, stored, then relayed.
    pub(super) async fn report(&self, rec: &Record, input: &Value) -> Result<String, String> {
        let r: ReportInput =
            serde_json::from_value(input.clone()).map_err(|e| format!("report: {e}"))?;
        evidence::check_report(&r)?;
        let mut items = Vec::new();
        for f in &r.files {
            items.push(evidence::file_item(Path::new(&rec.cwd), f)?);
        }
        items.extend(evidence::tool_items(&r.calls, input.get("evidence"))?);
        let total = items.len();
        let write = LaneWrite::Report {
            key: rec.key.clone(),
            run_id: rec.run_id.clone(),
            kind: r.kind.clone(),
            text: r.text.trim().to_string(),
            summary: r.summary.clone(),
            evidence: serde_json::to_string(&json!({ "files": r.files, "calls": r.calls }))
                .unwrap_or_default(),
            items,
            relay: "not posted".into(),
        };
        let (number, new_items) = match self
            .env
            .store
            .lanes(write)
            .await
            .map_err(|e| e.to_string())?
        {
            LaneDone::Report { number, new_items } => (number, new_items),
            other => return Err(format!("report not stored: {other:?}")),
        };
        let id = format!("{}#{number}", rec.key);
        let shown = evidence::cut(&evidence::display(r.text.trim()), super::WAKE_TEXT);
        self.emit(
            Emit::new(
                EventKind::LaneReport,
                Severity::Info,
                r.kind != "progress",
                &rec.key,
            )
            .with("lane", &rec.key)
            .with("kind", &r.kind)
            .with("report", &id)
            .with("text", &shown),
        );
        if new_items > 0 {
            // New evidence starts a new progress round (COORD req. 7).
            self.keep(&rec.key, |x| {
                x.watch.round_ms = now_ms();
                x.watch.reminders = 0;
                x.watch.alerted = false;
                if x.watch.stuck {
                    x.watch.stuck = false;
                }
            })
            .await;
        }
        let row = ReportRow {
            key: rec.key.clone(),
            number,
            run_id: rec.run_id.clone(),
            at_ms: now_ms(),
            kind: r.kind.clone(),
            text: r.text.trim().to_string(),
            summary: r.summary.clone(),
            new_items,
            relay: "not posted".into(),
        };
        if AT_ONCE.contains(&r.kind.as_str()) {
            self.relay(rec, &row).await;
        }
        self.nudge.notify_one();
        let attached = match (total, new_items) {
            (0, _) => "nothing attached".to_string(),
            (_, 0) => "all attached items were seen before".to_string(),
            (m, n) => format!("{n} of {m} attached items count as new"),
        };
        Ok(format!(
            "report {id} recorded at {}; {attached}",
            crate::jobs::instant_text(row.at_ms)
        ))
    }

    /// Posts a report to the lane's thread by policy (EVR req. 19-24); each report at most once.
    async fn relay(&self, rec: &Record, row: &ReportRow) {
        let settings = self.cfg().lanes.clone();
        let held = if rec.thread.is_none() {
            Some("no thread")
        } else if !settings.post_kinds.contains(&row.kind) {
            Some("kind not posted automatically")
        } else if !settings.post_raw && row.summary.is_none() {
            Some("no owner summary")
        } else {
            None
        };
        let set = |relay: String| LaneWrite::Relay {
            key: row.key.clone(),
            number: row.number,
            relay,
        };
        if let Some(reason) = held {
            if rec.thread.is_some() {
                self.alert(
                    &rec.key,
                    "report held",
                    &format!("report {}#{}: {reason}", row.key, row.number),
                    false,
                );
            }
            let _ = self.env.store.lanes(set(format!("held: {reason}"))).await;
            return;
        }
        let Some(thread) = rec
            .thread
            .as_deref()
            .and_then(|t| t.strip_prefix("discord:"))
        else {
            return;
        };
        let body = if settings.post_raw {
            row.summary.as_deref().unwrap_or(&row.text)
        } else {
            row.summary.as_deref().unwrap_or_default()
        };
        let home = std::env::var("HOME").ok();
        let text = format!(
            "Lane update: {}",
            evidence::thread_text(body, home.as_deref())
        );
        let seed = format!("lane-report:{}:{}", row.key, row.number);
        let outcome = self.env.threads.post(thread, &text, &seed).await;
        let relay = match &outcome {
            Posted::Confirmed => "posted".to_string(),
            Posted::Rejected(why) => format!("failed: {why}"),
            Posted::Uncertain(why) => format!("uncertain: {why}"),
        };
        let _ = self.env.store.lanes(set(relay)).await;
        match outcome {
            Posted::Confirmed => {
                let now = now_ms();
                self.keep(&rec.key, |r| r.watch.thread_post_ms = Some(now))
                    .await;
            }
            Posted::Rejected(why) | Posted::Uncertain(why) => {
                self.alert(
                    &rec.key,
                    "report not posted",
                    &format!("report {}#{}: {why}", row.key, row.number),
                    false,
                );
            }
        }
    }

    /// A step record (EVR req. 30-32).
    pub(super) async fn step(
        &self,
        rec: &Record,
        run: Option<String>,
        input: &Value,
    ) -> Result<String, String> {
        let text = |k: &str, max: usize| -> Result<String, String> {
            let t = input.get(k).and_then(Value::as_str).map_or("", str::trim);
            if t.is_empty() || t.chars().count() > max {
                return Err(format!("{k} must hold 1 to {max} characters"));
            }
            Ok(t.to_string())
        };
        let name = text("name", 200)?;
        let result = text("result", 1000)?;
        let evidence_ref = text("evidence", 1000)?;
        let strings = |k: &str| -> Vec<String> {
            input
                .get(k)
                .and_then(Value::as_array)
                .map_or_else(Vec::new, |a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
        };
        let receipts = strings("receipts");
        if receipts
            .iter()
            .any(|r| r.is_empty() || r.chars().count() > 500)
        {
            return Err("each receipt holds 1 to 500 characters".into());
        }
        let mut items = Vec::new();
        if let Some(files) = input.get("files").and_then(Value::as_array) {
            for f in files.iter().filter_map(Value::as_str) {
                items.push(evidence::file_item(Path::new(&rec.cwd), f)?);
            }
        }
        let calls = strings("calls");
        items.extend(evidence::tool_items(&calls, input.get("evidence"))?);
        let unverified = run.is_none();
        let write = LaneWrite::Step {
            key: rec.key.clone(),
            name: name.clone(),
            run_id: run,
            result,
            evidence: evidence_ref,
            receipts,
            items,
        };
        match self
            .env
            .store
            .lanes(write)
            .await
            .map_err(|e| e.to_string())?
        {
            LaneDone::Step(true) => {
                self.nudge.notify_one();
                Ok(format!(
                    "step '{name}' recorded{}",
                    if unverified { " (run unverified)" } else { "" }
                ))
            }
            LaneDone::Step(false) => Ok(format!("step '{name}' unchanged (recorded before)")),
            LaneDone::Refused(why) => Err(why),
            other => Err(format!("step not stored: {other:?}")),
        }
    }

    /// The oversight task (COORD-06, COORD-09 switch is `[lanes.oversight] enabled`).
    pub(crate) async fn run(
        self: Arc<Self>,
        mut stop: watch::Receiver<bool>,
    ) -> Result<(), String> {
        // Restarts need the OMP probe (DESIGN 3.4); wait for it as the lead does.
        let mut probed = self.env.omp.report_changes();
        while self.env.omp.report().is_none() {
            tokio::select! {
                _ = probed.changed() => {}
                () = tokio::time::sleep(Duration::from_secs(30)) => {}
                _ = stop.changed() => return Ok(()),
            }
        }
        self.recover().await;
        loop {
            if *stop.borrow() {
                return Ok(());
            }
            let o = self.cfg().lanes.oversight.clone();
            let next = if o.enabled { self.pass(&o).await } else { None };
            let now = now_ms();
            let sleep = next
                .map_or(o.sweep, |at| {
                    Duration::from_millis(u64::try_from(at - now).unwrap_or(0))
                })
                .min(o.sweep)
                .max(Duration::from_millis(200));
            tokio::select! {
                () = self.nudge.notified() => {}
                () = tokio::time::sleep(sleep) => {}
                _ = stop.changed() => {}
            }
        }
    }

    /// One evaluation of every open lane; returns the earliest next deadline.
    async fn pass(self: &Arc<Self>, o: &Oversight) -> Option<i64> {
        let rows = match self.env.store.read(|r| r.lanes(true)).await {
            Ok(rows) => rows,
            Err(e) => {
                log::warn("oversight pass skipped", &[("error", &e)]);
                return None;
            }
        };
        let mut next: Option<i64> = None;
        let mut soonest = |at: Option<i64>| {
            if let Some(at) = at {
                next = Some(next.map_or(at, |n| n.min(at)));
            }
        };
        for row in rows {
            let Ok(rec) = serde_json::from_str::<Record>(&row.record) else {
                continue;
            };
            soonest(self.oversee(&rec, o).await);
        }
        next
    }

    async fn oversee(self: &Arc<Self>, rec: &Record, o: &Oversight) -> Option<i64> {
        let now = now_ms();
        if rec.state == State::Claimed {
            let at = rec.claim.as_ref()?.close_at_ms;
            if now >= at {
                self.settle(&rec.key).await;
                // A postponed settle is retried every 30 s (LANE req. 56).
                return Some(now + 30_000);
            }
            return Some(at);
        }
        let session = self.session(&rec.key)?;
        if rec.lost.is_some() || rec.intent.is_some() {
            return None;
        }
        let snap = session.snapshot();
        self.dialogs(rec, &snap.dialogs).await;
        if !matches!(rec.state, State::Running | State::Rejected) {
            return None;
        }
        if self.ceilings(rec, now).await {
            return None;
        }
        let (k, run) = (rec.key.clone(), rec.run_id.clone());
        let progress = self
            .env
            .store
            .read(move |r| r.progress_mark(&k, &run))
            .await
            .ok()
            .flatten();
        let (k, run) = (rec.key.clone(), rec.run_id.clone());
        let last = self
            .env
            .store
            .read(move |r| r.reports(&k, Some(&run), 1))
            .await
            .ok()
            .and_then(|v| v.into_iter().next());
        let activity = rec
            .watch
            .active_ms
            .max(last.as_ref().map_or(0, |r| r.at_ms))
            .max(rec.opened_ms);
        let working_since = (snap.status == Status::Working)
            .then_some(snap.turn_started_ms)
            .flatten();
        let mut facts = Facts {
            now,
            progress_ms: progress.unwrap_or(rec.opened_ms).max(rec.opened_ms),
            activity_ms: activity,
            idle: snap.status == Status::Idle,
            working_since,
            last_kind: last.as_ref().map(|r| r.kind.as_str()),
            sample_complete: true,
        };
        let mut due = decide(&rec.watch, &facts, o);
        if due.stuck {
            facts.sample_complete = super::git::sample(Path::new(&rec.cwd)).await.is_some();
            due = decide(&rec.watch, &facts, o);
        }
        self.thread_quiet(rec, last.as_ref(), o, now).await;
        if let Some(n) = due.remind {
            self.remind(rec, n, facts.progress_ms, now).await;
        }
        if due.stall {
            self.stall(rec, facts.progress_ms, now, last.as_ref()).await;
        }
        if due.stuck {
            self.alert(
                &rec.key,
                "stuck",
                "busy on one turn past the busy threshold with no new evidence",
                true,
            );
            self.keep(&rec.key, |r| r.watch.stuck = true).await;
        }
        due.next_ms
    }

    async fn remind(&self, rec: &Record, n: u8, progress: i64, now: i64) {
        let quiet = (now - progress) / 60_000;
        let text = format!(
            "[reminder {n}] No new evidence for {quiet} min. A report without evidence does not reset the clock; \
             40 minutes goes to the lead. Ways forward: claim completion with evidence, report progress with \
             evidence, or report a wait (kind waiting) naming what you wait on; else continue. An exhausted \
             provider quota or credit budget means stop and report; never use a coupon, credit, top-up, paid \
             overage or an account, provider or model switch to get round it."
        );
        self.tell(
            &rec.key,
            text,
            Some(format!(
                "reminder:{}:{}:{n}",
                rec.run_id, rec.watch.round_ms
            )),
            true,
        )
        .await;
        self.keep(&rec.key, |r| r.watch.reminders = r.watch.reminders.max(n))
            .await;
    }

    async fn stall(&self, rec: &Record, progress: i64, now: i64, last: Option<&ReportRow>) {
        let last_text = last.map_or("none".to_string(), |r| {
            evidence::cut(&evidence::display(&r.text), 48)
        });
        let detail = format!(
            "no new evidence for {} min (since {}); reminders sent {}; last report {last_text}; session {}. \
             Next: check it, re-brief it, or close it.",
            (now - progress) / 60_000,
            crate::jobs::instant_text(progress),
            rec.watch.reminders,
            rec.session_id.as_deref().unwrap_or("unknown"),
        );
        self.alert(&rec.key, "stall", &detail, true);
        self.keep(&rec.key, |r| r.watch.alerted = true).await;
    }

    /// A progress report posts only after the thread was quiet for the send interval (WS req. 14).
    async fn thread_quiet(&self, rec: &Record, last: Option<&ReportRow>, o: &Oversight, now: i64) {
        let Some(row) = last.filter(|r| r.kind == "progress" && r.relay == "not posted") else {
            return;
        };
        let quiet_since = rec
            .watch
            .thread_post_ms
            .unwrap_or(rec.opened_ms)
            .max(row.at_ms);
        if now - quiet_since >= ms(o.thread_quiet) {
            self.relay(rec, row).await;
        }
    }

    /// Dialog hold notices (WS req. 18): one per distinct set, cleared silently.
    async fn dialogs(&self, rec: &Record, dialogs: &[crate::host::Dialog]) {
        let mut set: Vec<String> = dialogs
            .iter()
            .map(|d| format!("{:?} {}", d.kind, d.id).to_lowercase())
            .collect();
        set.sort();
        if set == rec.watch.dialogs {
            return;
        }
        if !set.is_empty() {
            self.alert(&rec.key, "held by a dialog", &set.join(", "), true);
        }
        self.keep(&rec.key, |r| r.watch.dialogs = set).await;
    }

    /// Ceilings (WS req. 24-27): reached ones hold the lane; unknown totals warn once per kind.
    async fn ceilings(&self, rec: &Record, now: i64) -> bool {
        let c = rec.ceilings;
        let mut reached = Vec::new();
        #[allow(clippy::cast_precision_loss)]
        let minutes = (now - rec.opened_ms) as f64 / 60_000.0;
        if let Some(limit) = c.minutes
            && minutes >= limit
        {
            reached.push(format!("wall {minutes:.0} of {limit} minutes"));
        }
        if c.tokens.is_some() || c.cost.is_some() {
            match self.read_spend(rec) {
                Ok((tokens, cost)) => {
                    if let Some(limit) = c.tokens
                        && tokens >= limit
                    {
                        reached.push(format!("{tokens} of {limit} tokens"));
                    }
                    if let Some(limit) = c.cost
                        && cost >= limit
                    {
                        reached.push(format!("{cost:.4} of {limit} USD (estimate)"));
                    }
                }
                Err(issue) => {
                    let kinds: Vec<&str> = [("tokens", c.tokens), ("cost", c.cost)]
                        .iter()
                        .filter(|(_, v)| v.is_some())
                        .map(|(k, _)| *k)
                        .filter(|k| !rec.watch.ceiling_warned.iter().any(|w| w == k))
                        .collect();
                    if !kinds.is_empty() {
                        self.alert(
                            &rec.key,
                            "ceiling unchecked",
                            &format!("{} total unknown: {issue}", kinds.join(" and ")),
                            false,
                        );
                        let kinds: Vec<String> = kinds.iter().map(|k| (*k).to_string()).collect();
                        self.keep(&rec.key, |r| r.watch.ceiling_warned.extend(kinds))
                            .await;
                    }
                }
            }
        }
        if reached.is_empty() {
            return false;
        }
        let text = format!("ceiling reached: {}", reached.join(", "));
        if rec.watch.ceiling.as_deref() != Some(text.as_str()) {
            let _ = self
                .env
                .store
                .lanes(LaneWrite::Report {
                    key: rec.key.clone(),
                    run_id: rec.run_id.clone(),
                    kind: "stuck".into(),
                    text: text.clone(),
                    summary: None,
                    evidence: "{}".into(),
                    items: Vec::new(),
                    relay: "held: hub report".into(),
                })
                .await;
            self.alert(
                &rec.key,
                "ceiling",
                &format!("{text}; the lane is held. Raise or remove the ceiling and continue it."),
                true,
            );
            let t = text.clone();
            self.keep(&rec.key, |r| r.watch.ceiling = Some(t)).await;
        }
        if let Err(e) = self.pause(&rec.key, "oversight", &text).await {
            log::warn("ceiling hold failed", &[("lane", &rec.key), ("error", &e)]);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lanes::record::Watch;

    fn o() -> Oversight {
        Oversight {
            first_reminder: Duration::from_secs(10),
            second_reminder: Duration::from_secs(20),
            stall: Duration::from_secs(40),
            busy: Duration::from_secs(20),
            quiet: Duration::from_secs(20),
            ..Oversight::default()
        }
    }

    fn facts(now_s: i64) -> Facts<'static> {
        Facts {
            now: now_s * 1000,
            progress_ms: 0,
            activity_ms: 0,
            idle: true,
            working_since: None,
            last_kind: None,
            sample_complete: true,
        }
    }

    #[test]
    fn reminders_only_while_idle_then_one_stall_alert() {
        let w = Watch::default();
        assert_eq!(decide(&w, &facts(5), &o()).remind, None);
        assert_eq!(decide(&w, &facts(5), &o()).next_ms, Some(10_000));
        assert_eq!(decide(&w, &facts(10), &o()).remind, Some(1));
        let busy = Facts {
            idle: false,
            ..facts(10)
        };
        assert_eq!(decide(&w, &busy, &o()).remind, None);
        let one = Watch {
            reminders: 1,
            ..Watch::default()
        };
        assert_eq!(decide(&one, &facts(15), &o()).remind, None);
        assert_eq!(decide(&one, &facts(20), &o()).remind, Some(2));
        let two = Watch {
            reminders: 2,
            ..Watch::default()
        };
        assert_eq!(
            decide(&two, &facts(39), &o()),
            Due {
                next_ms: Some(40_000),
                ..Due::default()
            }
        );
        let d = decide(&two, &facts(40), &o());
        assert!(d.stall && d.remind.is_none());
        let alerted = Watch {
            reminders: 2,
            alerted: true,
            ..Watch::default()
        };
        assert_eq!(decide(&alerted, &facts(100), &o()), Due::default());
        // A waiting lane is never reminded.
        let waiting = Facts {
            last_kind: Some("stopped"),
            ..facts(12)
        };
        assert_eq!(decide(&w, &waiting, &o()).remind, None);
    }

    #[test]
    fn stuck_needs_a_complete_sample() {
        let w = Watch {
            alerted: true,
            ..Watch::default()
        };
        let busy = Facts {
            idle: false,
            working_since: Some(0),
            ..facts(25)
        };
        assert!(decide(&w, &busy, &o()).stuck);
        let incomplete = Facts {
            sample_complete: false,
            ..busy
        };
        assert!(!decide(&w, &incomplete, &o()).stuck);
        let early = Facts {
            working_since: Some(10_000),
            ..busy
        };
        assert!(!decide(&w, &early, &o()).stuck);
        let obstacle = Facts {
            last_kind: Some("stuck"),
            ..busy
        };
        assert!(!decide(&w, &obstacle, &o()).stuck);
    }
}
