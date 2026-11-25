//! Lane lifecycle after the start (LANE req. 43-70; decision 9): hold and continue with goal
//! readback, park and abandon, the claim with its objection window, the crash-safe close in recorded
//! steps, reopen on the exact session file, death of a lane's process, and the hub-start pass that
//! brings running lanes back from their session files.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::record::{Claim, Intent, Record, State, Watch};
use super::{Lanes, git};
use crate::host::dialogs::GoalStatus;
use crate::host::inbox::ReceiptState;
use crate::host::{Control, Runtime, SessionEvent, Status};
use crate::jobs::event::{Emit, EventKind, Severity};
use crate::log;
use crate::store::lanes::{LaneDone, LaneWrite, Revision};
use crate::sys::now_ms;

/// A close waits this long for the process to exit before it is terminated (LANE req. 58).
const EXIT_WAIT: Duration = Duration::from_secs(30);
/// A hub-start restart waits this long for the lease to be free (decision 9).
const LEASE_WAIT: Duration = Duration::from_secs(30);
/// A tern hold waits this long in all for the running turn to end and the goal pause to land.
const TURN_END_WAIT: Duration = Duration::from_secs(15);
/// How soon a tern hold asks again after OMP refused the pause "busy".
const PAUSE_RETRY: Duration = Duration::from_millis(200);

/// Goal states that count as stopped for a hold (LANE req. 44).
const STOPPED: [GoalStatus; 4] = [
    GoalStatus::Paused,
    GoalStatus::None,
    GoalStatus::Complete,
    GoalStatus::Dropped,
];
/// Goal states a continue accepts (LANE req. 45).
const GOING: [GoalStatus; 4] = [
    GoalStatus::Active,
    GoalStatus::None,
    GoalStatus::Complete,
    GoalStatus::Dropped,
];

/// Whether the session is idle after a turn-end event. Every such event makes the snapshot stale
/// and queues a readback, and only that readback can show the session idle; it emits no event of
/// its own, so the answer waits for the next fresh snapshot (bounded by the rpc response time).
/// A snapshot read at once could still be the stale one, and the end of the active stretch would
/// then be missed for good.
async fn idle_after_readback<S: Control>(session: &S) -> bool {
    let mut states = session.states();
    let fresh = tokio::time::timeout(
        crate::omp::rpc::RESPONSE_TIMEOUT,
        states.wait_for(|s| s.fresh || s.stop.is_some()),
    )
    .await;
    matches!(fresh, Ok(Ok(s)) if s.status == Status::Idle)
}

/// Runs a goal operation; when OMP refuses it (no goal, already in that state), the readback
/// decides. `Ok` only when the goal reads back in one of `ok`.
async fn goal_to<S: Control>(
    session: &S,
    op: &str,
    ok: &[GoalStatus],
) -> Result<GoalStatus, String> {
    let goal = match session.goal(op, None).await {
        Ok(g) => g,
        Err(_) => session.goal("get", None).await?,
    };
    if ok.contains(&goal.status) {
        Ok(goal.status)
    } else {
        Err(format!("the goal reads back {:?}", goal.status).to_lowercase())
    }
}

/// A tern hold (LANE req. 44 in a terminal session). OMP refuses a goal change while a turn runs
/// there (docs/notes/tern.md), so the turn is interrupted first and the pause follows once the
/// session reads idle; a goal continuation that starts another turn in between is interrupted
/// again. Everything is bounded by [`TURN_END_WAIT`]; `Ok` only when the goal reads back stopped.
async fn pause_after_turn<S: Control>(session: &S) -> Result<GoalStatus, String> {
    let until = Instant::now() + TURN_END_WAIT;
    loop {
        if session.snapshot().status == Status::Working {
            session
                .interrupt()
                .await
                .map_err(|e| format!("the running turn was not interrupted: {e}"))?;
            let mut states = session.states();
            let left = until.saturating_duration_since(Instant::now());
            let ended = tokio::time::timeout(
                left,
                states.wait_for(|s| s.status != Status::Working || s.stop.is_some()),
            )
            .await;
            if !matches!(ended, Ok(Ok(_))) {
                return Err(format!(
                    "the interrupted turn did not end within {} s",
                    TURN_END_WAIT.as_secs()
                ));
            }
        }
        match session.goal("pause", None).await {
            Err(e) if e.starts_with("busy") && Instant::now() < until => {
                tokio::time::sleep(PAUSE_RETRY).await;
            }
            Err(e) => {
                let goal = session.goal("get", None).await?;
                return if STOPPED.contains(&goal.status) {
                    Ok(goal.status)
                } else {
                    Err(format!("the goal was not paused ({e})"))
                };
            }
            Ok(goal) if STOPPED.contains(&goal.status) => return Ok(goal.status),
            Ok(goal) => {
                return Err(format!("the goal reads back {:?}", goal.status).to_lowercase());
            }
        }
    }
}

impl<R: Runtime> Lanes<R> {
    /// Hold (LANE req. 43-46): goal paused and confirmed first, then the record, then the turn is
    /// interrupted. A tern lane's turn ends before the pause ([`pause_after_turn`]); when a step
    /// fails the record is left as it was and the caller is told "not held".
    pub(super) async fn pause(&self, key: &str, by: &str, reason: &str) -> Result<String, String> {
        let _turn = self.turn(key).await?;
        let (rec, rev) = self.load(key).await?;
        self.pause_locked(rec, rev, by, reason).await
    }

    async fn pause_locked(
        &self,
        mut rec: Record,
        rev: Revision,
        by: &str,
        reason: &str,
    ) -> Result<String, String> {
        match rec.state {
            State::Held => return Ok("already held".into()),
            State::Running | State::Rejected => {}
            other => return Err(format!("a {} lane cannot be held", other.name())),
        }
        if rec.intent.is_some() {
            return Err("a close is in progress".into());
        }
        let session = self
            .session(&rec.key)
            .ok_or("not held: the lane has no live session")?;
        let paused = if rec.runtime == crate::config::session::RuntimeKind::Tern {
            pause_after_turn(&*session).await
        } else {
            goal_to(&*session, "pause", &STOPPED).await
        };
        paused.map_err(|e| format!("not held: {e}"))?;
        rec.prev = Some(rec.state);
        rec.state = State::Held;
        rec.held_by = Some(by.to_string());
        rec.hold_reason = Some(reason.to_string());
        rec.watch.active_noticed = false;
        self.save(&rec, Some(rev), Some(format!("held by {by}: {reason}")))
            .await?;
        if session.snapshot().status == Status::Working {
            let _ = session.interrupt().await;
        }
        self.status_word(&rec).await;
        Ok("held; goal paused".into())
    }

    /// Continue (LANE req. 45): goal resumed and confirmed, state restored, quiet clock restarted.
    pub(super) async fn resume(&self, key: &str) -> Result<String, String> {
        let _turn = self.turn(key).await?;
        let (rec, rev) = self.load(key).await?;
        self.resume_locked(rec, rev).await
    }

    async fn resume_locked(&self, mut rec: Record, rev: Revision) -> Result<String, String> {
        if rec.state != State::Held {
            return Err(format!(
                "only a held lane continues; this one is {}",
                rec.state.name()
            ));
        }
        if rec.intent.is_some() {
            return Err("a close is in progress".into());
        }
        let session = self
            .session(&rec.key)
            .ok_or("the lane has no live session; reopen it")?;
        let goal = goal_to(&*session, "resume", &GOING).await?;
        rec.state = rec.prev.take().unwrap_or(State::Running);
        rec.held_by = None;
        rec.hold_reason = None;
        rec.start_note = None;
        let now = now_ms();
        rec.watch = Watch {
            round_ms: now,
            active_ms: now,
            thread_post_ms: rec.watch.thread_post_ms,
            ..Watch::default()
        };
        self.save(
            &rec,
            Some(rev),
            Some(format!("continued; goal {goal:?}").to_lowercase()),
        )
        .await?;
        let _ = self.mailbox(&rec.key).flush(Some(&*session), false).await;
        self.status_word(&rec).await;
        self.nudge.notify_one();
        Ok("continued".into())
    }

    /// Park or abandon (LANE req. 48-52).
    pub(super) async fn park(
        &self,
        key: &str,
        op: &str,
        reason: &str,
        condition: Option<&str>,
    ) -> Result<String, String> {
        let outcome = if op == "park" { "parked" } else { "abandoned" };
        let _turn = self.turn(key).await?;
        let (mut rec, mut rev) = self.load(key).await?;
        if let Some(intent) = &rec.intent {
            if intent.outcome != outcome {
                return Err(format!(
                    "the recorded intent is {}; a different outcome is refused",
                    intent.outcome
                ));
            }
            return self.finish_close(rec, rev).await;
        }
        match rec.state {
            State::Running | State::Rejected | State::Held => {}
            State::Claimed => return Err("settle the completion claim first".into()),
            other => return Err(format!("a {} lane cannot be {outcome}", other.name())),
        }
        let pending = self.undelivered(key).await;
        if pending > 0 {
            return Err(format!(
                "{pending} accepted message(s) for the lane are undelivered; wait"
            ));
        }
        if outcome == "parked" {
            let (Some(id), Some(file)) = (rec.session_id.clone(), rec.session_file.clone()) else {
                return Err("park needs a recorded session".into());
            };
            let root = self.env.state_dir.join("sessions").join("lanes").join(key);
            let verified = crate::omp::transcript::verify(
                PathBuf::from(file),
                root,
                id,
                PathBuf::from(&rec.cwd),
            )
            .await;
            if !matches!(verified, Ok(Some(_))) {
                return Err(
                    "the saved session file does not resolve to the recorded session and directory"
                        .into(),
                );
            }
        }
        if rec.state != State::Held && self.session(key).is_some() {
            self.pause_locked(rec, rev, "lead", reason).await?;
            (rec, rev) = self.load(key).await?;
        }
        rec.intent = Some(Intent {
            outcome: outcome.into(),
            note: reason.to_string(),
            condition: Some(condition.unwrap_or("explicit instruction").to_string()),
            checks: Vec::new(),
            step: "intent".into(),
            receipt: vec![format!("reason: {reason}"), "worktrees kept".into()],
            lesson: None,
        });
        if rec.state != State::Held {
            rec.prev = Some(rec.state);
            rec.state = State::Held;
        }
        rev = self
            .save(&rec, Some(rev), Some(format!("{op} intent: {reason}")))
            .await?;
        self.finish_close(rec, rev).await
    }

    /// Messages accepted for the lane and not yet in its session (LANE req. 49, 56): everything
    /// written and unconfirmed, and every pending wake (the lead's and the owner's messages). A
    /// pending quiet note (the claim notice) is no accepted message: it waits for the next turn, which
    /// a lane told to stay idle never starts, so counting it would postpone every settle for ever.
    async fn undelivered(&self, key: &str) -> usize {
        self.mailbox(key).list().await.map_or(usize::MAX, |rows| {
            rows.iter()
                .filter(|(l, _)| l.state == "sent" || (l.state == "pending" && l.wakes))
                .count()
        })
    }

    /// A completion claim (LANE req. 53-54).
    pub(super) async fn claim(&self, key: &str, evidence: &str) -> Result<String, String> {
        if evidence.chars().count() > 4000 || evidence.contains("/home/") {
            return Err("claim evidence is report IDs and shareable receipts, at most 4000 characters, no local paths".into());
        }
        let _turn = self.turn(key).await?;
        let (mut rec, rev) = self.load(key).await?;
        if !matches!(rec.state, State::Running | State::Rejected) || rec.intent.is_some() {
            return Err(format!("a {} lane cannot claim", rec.state.name()));
        }
        let now = now_ms();
        let window = i64::try_from(self.cfg().lanes.objection.as_millis()).unwrap_or(i64::MAX);
        let close_at = now.saturating_add(window);
        rec.claim = Some(Claim {
            at_ms: now,
            evidence: evidence.to_string(),
            close_at_ms: close_at,
        });
        rec.objection.clear();
        rec.state = State::Claimed;
        rec.watch.round_ms = now;
        rec.watch.reminders = 0;
        rec.watch.alerted = false;
        self.save(&rec, Some(rev), Some("claimed completion".into()))
            .await?;
        let at = crate::jobs::instant_text(close_at);
        self.emit(
            Emit::new(EventKind::LaneClaimed, Severity::Info, true, key)
                .with("lane", key)
                .with("closes", &at)
                .with("evidence", evidence),
        );
        self.tell(
            key,
            format!("[claim recorded] The lane closes automatically at {at} unless the lead objects. Stay idle."),
            Some(format!("claim:{}:{now}", rec.run_id)),
            false,
        )
        .await;
        self.status_word(&rec).await;
        self.nudge.notify_one();
        Ok(format!(
            "claim recorded; closes at {at} unless the lead objects"
        ))
    }

    /// An objection (LANE req. 55).
    pub(super) async fn object(&self, key: &str, reasons: &[String]) -> Result<String, String> {
        let _turn = self.turn(key).await?;
        let (rec, rev) = self.load(key).await?;
        self.reject(rec, rev, reasons, "the lead").await
    }

    async fn reject(
        &self,
        mut rec: Record,
        rev: Revision,
        reasons: &[String],
        by: &str,
    ) -> Result<String, String> {
        if rec.state != State::Claimed || rec.intent.is_some() {
            return Err("no open claim to object to (or the close is already decided)".into());
        }
        rec.state = State::Rejected;
        rec.claim = None;
        rec.objection = reasons.to_vec();
        self.save(
            &rec,
            Some(rev),
            Some(format!("claim rejected by {by}: {}", reasons.join("; "))),
        )
        .await?;
        self.tell(
            &rec.key,
            format!(
                "[objection] Your completion claim was rejected: {}. Fix this, then claim again with fresh evidence.",
                reasons.join("; ")
            ),
            None,
            true,
        )
        .await;
        self.status_word(&rec).await;
        Ok("objection recorded; the lane was told".into())
    }

    /// The checks before a close, in order (LANE req. 56): unsent work, live signals, undelivered
    /// messages. `Ok(lines)` to close; `Err(Some(reasons))` objects; `Err(None)` postpones.
    async fn close_checks(&self, rec: &Record) -> Result<Vec<String>, Option<Vec<String>>> {
        let mut reasons = Vec::new();
        let dirs: Vec<PathBuf> = if rec.worktrees.is_empty() {
            vec![PathBuf::from(&rec.cwd)]
        } else {
            rec.worktrees.iter().map(PathBuf::from).collect()
        };
        for dir in dirs {
            if let Some(why) = git::unsent(&dir).await {
                reasons.push(format!("unsent work: {why}"));
            }
        }
        let identities: Vec<(String, String)> = self.cfg().lanes.identities.clone();
        let results =
            super::checks::run_all(&rec.checks, std::path::Path::new(&rec.cwd), &identities).await;
        let lines: Vec<String> = results
            .iter()
            .map(|o| {
                format!(
                    "{} {}: {}",
                    if o.pass { "pass" } else { "fail" },
                    o.signal,
                    o.detail
                )
            })
            .collect();
        reasons.extend(
            results
                .iter()
                .filter(|o| !o.pass)
                .map(|o| format!("check failed: {} ({})", o.signal, o.detail)),
        );
        if !reasons.is_empty() {
            return Err(Some(reasons));
        }
        if self.undelivered(&rec.key).await > 0 {
            return Err(None);
        }
        if let Some(s) = self.session(&rec.key) {
            let snap = s.snapshot();
            if snap.status == Status::Working || !snap.dialogs.is_empty() {
                return Err(None);
            }
        }
        Ok(lines)
    }

    /// A claim whose window passed (LANE req. 56); postponed ones are tried again by oversight.
    pub(super) async fn settle(&self, key: &str) {
        let Ok(_turn) = self.turn(key).await else {
            return;
        };
        let Ok((rec, rev)) = self.load(key).await else {
            return;
        };
        if rec.state != State::Claimed
            || rec.claim.as_ref().is_none_or(|c| c.close_at_ms > now_ms())
        {
            return;
        }
        match self.close_checks(&rec).await {
            Ok(lines) => {
                if let Err(e) = self
                    .commit_close(rec, rev, "closed after an unopposed claim", lines, None)
                    .await
                {
                    log::warn("lane settle failed", &[("lane", &key), ("error", &e)]);
                }
            }
            Err(Some(reasons)) => {
                let _ = self.reject(rec, rev, &reasons, "the close checks").await;
                self.alert(
                    key,
                    "claim turned into an objection",
                    &reasons.join("; "),
                    true,
                );
            }
            Err(None) => {}
        }
    }

    /// A close by the lead (LANE req. 57): the same checks, without the window.
    pub(super) async fn close(
        &self,
        key: &str,
        note: &str,
        lesson: Option<&str>,
    ) -> Result<String, String> {
        let _turn = self.turn(key).await?;
        let (rec, rev) = self.load(key).await?;
        if rec.intent.is_some() {
            return self.finish_close(rec, rev).await;
        }
        if !matches!(
            rec.state,
            State::Running | State::Rejected | State::Claimed | State::Held
        ) {
            return Err(format!("a {} lane cannot be closed", rec.state.name()));
        }
        match self.close_checks(&rec).await {
            Ok(lines) => {
                self.commit_close(rec, rev, note, lines, lesson.map(str::to_string))
                    .await
            }
            Err(Some(reasons)) => Err(format!("not closed: {}", reasons.join("; "))),
            Err(None) => {
                Err("not closed: messages are undelivered or the lane is busy; try again".into())
            }
        }
    }

    /// Records the close intent; from here no claim, objection or message changes it (LANE req. 58).
    async fn commit_close(
        &self,
        mut rec: Record,
        rev: Revision,
        note: &str,
        lines: Vec<String>,
        lesson: Option<String>,
    ) -> Result<String, String> {
        if let Some(s) = self.session(&rec.key)
            && s.snapshot().session_id != rec.session_id
        {
            return Err("the live session does not match the record".into());
        }
        rec.intent = Some(Intent {
            outcome: "completed".into(),
            note: note.to_string(),
            condition: None,
            checks: lines.clone(),
            step: "intent".into(),
            receipt: lines,
            lesson,
        });
        rec.prev = Some(rec.state);
        rec.state = State::Closing;
        let rev = self
            .save(&rec, Some(rev), Some(format!("close intent: {note}")))
            .await?;
        #[cfg(debug_assertions)]
        crate::sys::fault::point("lane_after_close_intent");
        self.finish_close(rec, rev).await
    }

    /// The close steps after the intent, each recorded; a crash resumes from the step reached
    /// (LANE req. 51, 58-62).
    async fn finish_close(&self, mut rec: Record, mut rev: Revision) -> Result<String, String> {
        let Some(mut intent) = rec.intent.clone() else {
            return Err("no recorded intent".into());
        };
        if intent.step == "intent" {
            let live = self.session(&rec.key);
            if let Some(s) = &live {
                if let Some(snap) = s.snapshot().goal.as_ref()
                    && snap.status == GoalStatus::Active
                {
                    let _ = goal_to(&**s, "pause", &STOPPED).await;
                }
                match tokio::time::timeout(EXIT_WAIT, s.close(false)).await {
                    Ok(Ok(_)) => {}
                    _ => {
                        let _ = s.close(true).await;
                    }
                }
                if s.alive() {
                    return Err("the lane's process is still running; the close is held".into());
                }
            }
            self.live_map().remove(&rec.key);
            intent
                .receipt
                .push(format!("session was live: {}", live.is_some()));
            intent.step = "gone".into();
            rec.intent = Some(intent.clone());
            rev = self.save(&rec, Some(rev), None).await?;
        }
        if intent.step == "gone" {
            if intent.outcome == "completed" {
                let line = self.thread_finished(&rec).await;
                intent.receipt.push(line);
            }
            let spend = self.read_spend(&rec);
            intent.receipt.push(match &spend {
                Ok((t, c)) => format!("spend: {t} tokens, {c:.4} USD (estimate)"),
                Err(e) => format!("spend unknown: {e}"),
            });
            rec.spend = spend.ok();
            intent.step = "hook".into();
            rec.intent = Some(intent.clone());
            rev = self.save(&rec, Some(rev), None).await?;
        }
        if intent.step == "hook" {
            if intent.outcome == "completed" {
                for w in rec.worktrees.clone() {
                    // A worktree that holds a published file stays (EVR-19).
                    if self.holds_published(&w).await {
                        intent
                            .receipt
                            .push(format!("worktree {w} kept: it holds a published file"));
                        continue;
                    }
                    intent.receipt.push(match git::remove_worktree(&w).await {
                        Ok(()) => format!("worktree {w} removed"),
                        Err(e) => e,
                    });
                }
            }
            intent.step = "cleaned".into();
            rec.intent = Some(intent.clone());
            rev = self.save(&rec, Some(rev), None).await?;
        }
        // Notes still waiting (the claim notice) belong to this run; a reopen starts empty.
        match self.mailbox(&rec.key).retire().await {
            Ok(0) => {}
            Ok(n) => intent.receipt.push(format!("{n} unsent note(s) retired")),
            Err(e) => return Err(format!("unsent notes not retired: {e}")),
        }
        rec.state = State::Closed;
        rec.outcome = Some(intent.outcome.clone());
        rec.closed_ms = Some(now_ms());
        rec.receipt = intent.receipt.clone();
        rec.intent = None;
        rec.prev = None;
        self.save(
            &rec,
            Some(rev),
            Some(format!("closed {}: {}", intent.outcome, intent.note)),
        )
        .await?;
        self.spend
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&rec.key);
        self.status_word(&rec).await;
        self.emit(
            Emit::new(EventKind::LaneClosed, Severity::Info, true, &rec.key)
                .with("lane", &rec.key)
                .with("outcome", &intent.outcome)
                .with("detail", rec.receipt.join("; ")),
        );
        Ok(format!("closed {}", intent.outcome))
    }

    /// The thread-finished hook (LANE req. 42): once, only when no other lane on the thread is open.
    async fn thread_finished(&self, rec: &Record) -> String {
        let Some(thread) = rec.thread.clone() else {
            return "no thread".into();
        };
        let _one = self.closing.lock().await;
        let others = self.env.store.read(|r| r.lanes(true)).await.map(|rows| {
            rows.iter()
                .filter(|r| r.thread.as_deref() == Some(thread.as_str()) && r.key != rec.key)
                .count()
        });
        match others {
            Ok(0) => match self
                .env
                .threads
                .status(
                    thread.strip_prefix("discord:").unwrap_or(&thread),
                    "done",
                    true,
                )
                .await
            {
                Ok(()) => "thread marked done and archived".into(),
                Err(e) => format!("thread-finished hook failed: {e}"),
            },
            Ok(n) => format!("thread left open: {n} other lane(s) on it"),
            Err(e) => format!("thread-finished hook skipped: {e}"),
        }
    }

    /// Reopen on the exact saved session (LANE req. 63-66).
    pub(super) async fn reopen(
        self: &Arc<Self>,
        key: &str,
        note: Option<&str>,
    ) -> Result<String, String> {
        let _turn = self.turn(key).await?;
        let (mut rec, rev) = self.load(key).await?;
        let parked_held =
            rec.state == State::Held && rec.intent.as_ref().is_some_and(|i| i.outcome == "parked");
        if rec.state != State::Closed && !parked_held && rec.lost.is_none() {
            return Err("only a closed, lost or parked lane is reopened".into());
        }
        let (Some(id), Some(file)) = (rec.session_id.clone(), rec.session_file.clone()) else {
            return Err("no recorded session ID".into());
        };
        let root = self.env.state_dir.join("sessions").join("lanes").join(key);
        let verified = crate::omp::transcript::verify(
            PathBuf::from(&file),
            root,
            id.clone(),
            PathBuf::from(&rec.cwd),
        )
        .await;
        if !matches!(verified, Ok(Some(_))) {
            return Err("the saved session file is missing or its header names another session or directory".into());
        }
        if self.session(key).is_some() {
            return Err("a live session already runs this session".into());
        }
        if self.lease_free(&id).await != Some(true) {
            return Err("another OMP holds this session's lease".into());
        }
        let config = self.cfg();
        let exe = self
            .admission(&config, std::path::Path::new(&rec.cwd), &rec.model)
            .await?;
        self.tern_spend(&config, &rec).await?;
        let now = now_ms();
        new_run(&mut rec, now);
        let write = LaneWrite::Save {
            key: rec.key.clone(),
            state: rec.state.name().into(),
            outcome: None,
            thread: rec.thread.clone(),
            opened_ms: now,
            closed_ms: None,
            record: serde_json::to_string(&rec).map_err(|e| e.to_string())?,
            expect: Some(rev),
            note: Some(format!("reopened with run {}", rec.run_id)),
            reopen: true,
        };
        let rev = match self
            .env
            .store
            .lanes(write)
            .await
            .map_err(|e| e.to_string())?
        {
            LaneDone::Saved(r) => r,
            LaneDone::Refused(why) => return Err(why),
            other => return Err(format!("unexpected store answer {other:?}")),
        };
        let outcome = self
            .start(
                rec,
                rev,
                exe,
                &config,
                Some((id.clone(), PathBuf::from(&file))),
            )
            .await;
        let (rec, rev) = self.load(key).await?;
        if outcome != "started" || rec.session_id.as_deref() != Some(id.as_str()) {
            return Err(format!(
                "reopen did not confirm the session ({outcome}); the lane stays held for inspection"
            ));
        }
        self.resume_locked(rec.clone(), rev).await?;
        if let Some(note) = note {
            let session = self.session(key).ok_or("the lane's session ended")?;
            let deadline = Instant::now() + Duration::from_secs(30);
            let state = self
                .deliver_wait(
                    key,
                    &session,
                    note,
                    &format!("reopen-note:{}", rec.run_id),
                    deadline,
                )
                .await;
            if state != ReceiptState::Delivered {
                return Ok(
                    format!("reopened; the note is {state:?} and is never resent").to_lowercase(),
                );
            }
        }
        Ok(format!("reopened on session {id} with run {}", rec.run_id))
    }

    /// Whether OMP shows the session's lease free; `None` when unknown.
    async fn lease_free(&self, id: &str) -> Option<bool> {
        let path = self.env.omp.path()?;
        match crate::omp::usage::lease(&path, &[id]).await {
            Ok(l) if l.held.iter().any(|h| h == id) => Some(false),
            Ok(l) if l.unknown.iter().any(|u| u == id) => None,
            Ok(_) => Some(true),
            Err(_) => None,
        }
    }

    /// Follows one lane session: lost on an exit not asked for (LANE req. 67), the held-lane turn
    /// notice once per active stretch (LANE req. 47), inbox flushes at turn end, oversight wakes.
    pub(super) async fn watch(self: Arc<Self>, key: String, session: Arc<R::Session>) {
        let mut events = session.subscribe();
        loop {
            let event = match events.recv().await {
                Ok((_, e)) => e,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            };
            match event {
                SessionEvent::TurnStarted => {
                    let now = now_ms();
                    let mut tell = false;
                    self.keep(&key, |r| {
                        r.watch.active_ms = now;
                        if r.state == State::Held && !r.watch.active_noticed && r.intent.is_none() {
                            r.watch.active_noticed = true;
                            tell = true;
                        }
                    })
                    .await;
                    if tell {
                        self.alert(
                            &key,
                            "held lane active",
                            "a held lane started a turn; it stays held until you continue it",
                            true,
                        );
                    }
                }
                SessionEvent::TurnEnded { .. }
                | SessionEvent::Quiet
                | SessionEvent::Settled { .. } => {
                    let idle = idle_after_readback(&*session).await;
                    let now = now_ms();
                    self.keep(&key, |r| {
                        r.watch.active_ms = now;
                        if idle {
                            r.watch.active_noticed = false;
                        }
                    })
                    .await;
                    let _ = self.mailbox(&key).flush(Some(&*session), false).await;
                    self.nudge.notify_one();
                }
                SessionEvent::DialogOpened { .. } | SessionEvent::DialogClosed { .. } => {
                    self.nudge.notify_one();
                }
                SessionEvent::LinkLost => {
                    let why = "the terminal session's bridge was silent for 45 s";
                    self.keep(&key, |r| r.lost = Some(why.into())).await;
                    self.emit(
                        Emit::new(EventKind::LaneLost, Severity::Warning, true, &key)
                            .with("lane", &key)
                            .with("detail", why),
                    );
                }
                SessionEvent::LinkRestored => {
                    self.keep(&key, |r| r.lost = None).await;
                    self.alert(
                        &key,
                        "lane restored",
                        "the terminal session's bridge reconnected with the same session",
                        false,
                    );
                    let _ = self.mailbox(&key).flush(Some(&*session), false).await;
                }
                SessionEvent::Stopped { record } => {
                    self.lost(&key, &session, &record.how, record.detail.as_deref())
                        .await;
                    return;
                }
                _ => {}
            }
        }
    }

    async fn lost(&self, key: &str, session: &Arc<R::Session>, how: &str, detail: Option<&str>) {
        let Ok(_turn) = self.turn(key).await else {
            return;
        };
        let Ok((mut rec, rev)) = self.load(key).await else {
            return;
        };
        // An exit the hub asked for (close, park) or of an earlier run is no death.
        let current = self
            .live_map()
            .get(key)
            .is_some_and(|s| Arc::ptr_eq(s, session));
        if rec.intent.is_some() || !rec.open() || !current {
            return;
        }
        self.live_map().remove(key);
        let last: Vec<String> = session
            .snapshot()
            .recent
            .iter()
            .rev()
            .take(3)
            .map(|e| super::evidence::cut(&super::evidence::display(&e.text), 160))
            .collect();
        let why = format!(
            "{how}{}",
            detail.map_or_else(String::new, |d| format!(" ({d})"))
        );
        rec.lost = Some(why.clone());
        let _ = self
            .save(&rec, Some(rev), Some(format!("lost: {why}")))
            .await;
        self.emit(
            Emit::new(EventKind::LaneLost, Severity::Warning, true, key)
                .with("lane", key)
                .with("detail", format!("{why}; last lines: {}", last.join(" | "))),
        );
    }

    /// Hub start (LANE req. 70; decision 9): finish recorded intents, bring every lane that was
    /// running back on its exact file once the lease is free, resume goals that were active, leave
    /// held lanes held, then give the lead one recap.
    pub(crate) async fn recover(self: &Arc<Self>) {
        let rows = match self.env.store.read(|r| r.lanes(true)).await {
            Ok(rows) => rows,
            Err(e) => {
                log::warn("lane recovery skipped", &[("error", &e)]);
                return;
            }
        };
        let mut lines = Vec::new();
        for row in rows {
            let line = self.recover_one(&row.key).await;
            lines.push(format!("{}: {line}", row.key));
        }
        if !lines.is_empty() {
            self.emit(
                Emit::new(EventKind::LaneRecap, Severity::Info, true, "lanes")
                    .with("detail", lines.join("; ")),
            );
        }
        let resumed = self.live_map().values().filter(|s| s.alive()).count();
        let _ = self.resumed.set(resumed);
        self.nudge.notify_one();
    }

    async fn recover_one(self: &Arc<Self>, key: &str) -> String {
        let Ok(_turn) = self.turn(key).await else {
            return "busy".into();
        };
        let Ok((mut rec, rev)) = self.load(key).await else {
            return "record unreadable".into();
        };
        if rec.intent.is_some() {
            return match self.finish_close(rec, rev).await {
                Ok(done) => format!("{done} (finished the recorded close)"),
                Err(e) => format!("close held: {e}"),
            };
        }
        let (Some(id), Some(file)) = (rec.session_id.clone(), rec.session_file.clone()) else {
            if rec.state == State::Starting {
                let _ = self
                    .start_failed(rec, rev, None, "the hub stopped during the start")
                    .await;
                return "start failed during the hub stop; record removed".into();
            }
            return "no session to restart".into();
        };
        if rec.state == State::Starting {
            let _ = self
                .unconfirmed(rec, rev, "the hub stopped during the start")
                .await;
            return "start unconfirmed; held for inspection".into();
        }
        let was_active = matches!(rec.state, State::Running | State::Rejected | State::Claimed);
        let deadline = Instant::now() + LEASE_WAIT;
        loop {
            match self.lease_free(&id).await {
                Some(true) => break,
                _ if Instant::now() >= deadline => {
                    rec.lost = Some("the lease stayed held after the hub restart".into());
                    let _ = self
                        .save(&rec, Some(rev), Some("lost: lease held at recovery".into()))
                        .await;
                    return "lost: the session's lease stayed held".into();
                }
                _ => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        }
        let config = self.cfg();
        let admitted = match self
            .admission(&config, std::path::Path::new(&rec.cwd), &rec.model)
            .await
        {
            Ok(exe) => self.tern_spend(&config, &rec).await.map(|()| exe),
            Err(e) => Err(e),
        };
        let exe = match admitted {
            Ok(exe) => exe,
            Err(e) => {
                rec.lost = Some(format!("not restarted: {e}"));
                let _ = self.save(&rec, Some(rev), Some(format!("lost: {e}"))).await;
                return format!("lost: {e}");
            }
        };
        rec.lost = None;
        let rev = match self.save(&rec, Some(rev), None).await {
            Ok(r) => r,
            Err(e) => return format!("not restarted: {e}"),
        };
        let outcome = self
            .start(
                rec,
                rev,
                exe,
                &config,
                Some((id.clone(), PathBuf::from(&file))),
            )
            .await;
        let Some(session) = self.session(key) else {
            return format!("not restarted: {outcome}");
        };
        if session.snapshot().session_id.as_deref() != Some(id.as_str()) {
            return "restarted on another session; held for inspection".into();
        }
        if !was_active {
            return "restarted; stays held (goal paused)".into();
        }
        match goal_to(
            &*session,
            "resume",
            &[GoalStatus::Active, GoalStatus::Complete],
        )
        .await
        {
            Ok(g) => format!("restarted; goal {g:?}").to_lowercase(),
            Err(e) => {
                self.keep(key, |r| r.lost = Some(format!("goal not resumed: {e}")))
                    .await;
                format!("restarted; goal not resumed: {e}")
            }
        }
    }

    /// Hub shutdown: lanes are closed with end of input so their files stay theirs; the next hub
    /// brings them back (decision 9). Records keep their state.
    pub(crate) async fn shutdown(&self) {
        let sessions: Vec<(String, Arc<R::Session>)> = self.live_map().drain().collect();
        let closes = sessions.iter().map(|(_, s)| async move {
            let _ = tokio::time::timeout(crate::host::lead::STOP_BUDGET, s.close(true)).await;
        });
        futures_util::future::join_all(closes).await;
    }
}

/// Resets a reopened record to a new run, held until the start confirms its session.
fn new_run(rec: &mut Record, now: i64) {
    rec.opened_ms = now;
    rec.closed_ms = None;
    rec.run_id = crate::host::random_hex(8);
    rec.claim = None;
    rec.objection.clear();
    rec.receipt.clear();
    rec.intent = None;
    rec.outcome = None;
    rec.lost = None;
    rec.doubt = None;
    rec.spend = None;
    rec.start_note = None;
    rec.prev = Some(State::Running);
    rec.state = State::Held;
    rec.held_by = Some("reopen".into());
    rec.watch = Watch {
        round_ms: now,
        active_ms: now,
        ..Watch::default()
    };
}
