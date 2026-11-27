//! Queued launches (LANE-11; LANE req. 22-27): an open that names predecessor lanes is validated and
//! its brief assembled now, then kept with the key reserved until the lead or the owner starts it.
//! The hub tells the lead once per change when it becomes ready or is held again, and never starts it
//! by itself, except a lane that waits only for its worker to be reachable (the owner's decision,
//! docs/design/workers.md 2.6): that one starts as soon as the worker answers.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::docs::{self, QUEUED};
use super::record::{self, OpenInput, Record, State};
use super::{Lanes, s};
use crate::host::Runtime;
use crate::jobs::event::{Emit, EventKind, Severity};
use crate::sys::now_ms;

const STARTING: &str = "starting";
const STARTED: &str = "started";
const PENDING: &str = "pending";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Queued {
    pub(crate) key: String,
    /// The open's inputs with the brief inline and the directory canonical.
    pub(crate) input: Value,
    pub(crate) after: Vec<String>,
    pub(crate) priority: u8,
    pub(crate) created_ms: i64,
    pub(crate) expires_ms: i64,
    pub(crate) state: String,
    /// What the lead was last told: `ready`, `held: ...` or `expired`.
    pub(crate) told: Option<String>,
    pub(crate) note: Option<String>,
    /// The kickoff line for its thread, posted when it starts (a lane queued for its worker).
    #[serde(default)]
    pub(crate) kickoff: Option<String>,
}

impl Queued {
    /// The worker the launch waits for, when it names one.
    fn worker(&self) -> Option<&str> {
        self.input.get("worker").and_then(Value::as_str)
    }

    /// It waits for nothing but its worker, so the hub starts it by itself.
    fn waits_for_worker(&self) -> bool {
        self.after.is_empty() && self.worker().is_some()
    }
}

impl Queued {
    pub(crate) fn reserves(&self) -> bool {
        self.state != STARTED
    }
}

/// Whether a predecessor closed completed with every completion signal passed (LANE req. 24).
pub(crate) fn completed(rec: &Record) -> Result<(), String> {
    if rec.state != State::Closed || rec.outcome.as_deref() != Some("completed") {
        return Err(format!("{} is {}", rec.key, rec.status()));
    }
    let passed = rec
        .receipt
        .iter()
        .filter(|l| l.starts_with("pass "))
        .count();
    if rec.receipt.iter().any(|l| l.starts_with("fail ")) || passed < rec.checks.len() {
        return Err(format!("{} closed without every signal passed", rec.key));
    }
    Ok(())
}

impl<R: Runtime> Lanes<R> {
    /// Stores an open that names predecessors (LANE req. 22-23), or whose worker cannot be reached.
    pub(super) async fn queue(
        &self,
        open: &OpenInput,
        valid: &record::Validated,
        mut input: Value,
        kickoff: Option<String>,
    ) -> Result<Value, String> {
        let key = &valid.key;
        if self.row(key).await?.is_some() {
            return Err(format!("the key {key} is already used by a lane"));
        }
        if docs::get::<Queued>(&self.env.store, QUEUED, key)
            .await?
            .is_some_and(|(q, _)| q.reserves())
        {
            return Err(format!("the key {key} is already queued"));
        }
        let mut after = open.after.clone();
        after.sort();
        after.dedup();
        if after.len() != open.after.len() || after.contains(key) {
            return Err("predecessors must be distinct and not the key itself".into());
        }
        for p in &after {
            let lane = self.row(p).await?.is_some();
            let queued = docs::get::<Queued>(&self.env.store, QUEUED, p)
                .await?
                .is_some();
            if !lane && !queued {
                return Err(format!(
                    "predecessor {p} is neither a lane nor a queued launch"
                ));
            }
        }
        // A cycle runs through queued launches only: lanes have no predecessors.
        let mut stack = after.clone();
        let mut seen = Vec::new();
        while let Some(p) = stack.pop() {
            if &p == key {
                return Err("the predecessors form a cycle".into());
            }
            if seen.contains(&p) {
                continue;
            }
            if let Some((q, _)) = docs::get::<Queued>(&self.env.store, QUEUED, &p).await? {
                stack.extend(q.after);
            }
            seen.push(p);
        }
        let hours = open
            .expires_hours
            .unwrap_or_else(|| self.cfg().coord.planner.queue_expiry.as_secs_f64() / 3600.0);
        if !hours.is_finite() || hours <= 0.0 {
            return Err("expires_hours must be a positive number".into());
        }
        if let Some(m) = input.as_object_mut() {
            for k in [
                "brief_file",
                "preview",
                "request_id",
                "after",
                "expires_hours",
            ] {
                m.remove(k);
            }
            m.insert("brief".into(), json!(valid.brief));
            m.insert("cwd".into(), json!(valid.cwd.display().to_string()));
        }
        let now = now_ms();
        #[allow(clippy::cast_possible_truncation)]
        let q = Queued {
            key: key.clone(),
            input,
            after,
            priority: valid.priority,
            created_ms: now,
            expires_ms: now + (hours * 3_600_000.0) as i64,
            state: PENDING.into(),
            told: None,
            note: None,
            kickoff,
        };
        // A started entry of an earlier lane under this key is replaced.
        docs::put(&self.env.store, QUEUED, key, &q, None).await?;
        self.nudge.notify_one();
        Ok(json!({ "key": key, "outcome": "queued", "after": q.after }))
    }

    /// Why a queued launch cannot start now; `Ok` when every predecessor is ready and its worker,
    /// if it names one, answers.
    async fn readiness(&self, q: &Queued, now: i64) -> Result<(), String> {
        if now >= q.expires_ms {
            return Err("expired".into());
        }
        let mut held = Vec::new();
        for p in &q.after {
            match self.load(p).await {
                Ok((rec, _)) => {
                    if let Err(e) = completed(&rec) {
                        held.push(e);
                    }
                }
                Err(_) => held.push(format!("{p} has no lane yet")),
            }
        }
        if held.is_empty()
            && let Some(worker) = q.worker()
            && let Err(why) = self.worker_up(worker).await
        {
            held.push(why);
        }
        if held.is_empty() {
            Ok(())
        } else {
            Err(held.join("; "))
        }
    }

    /// Tells the lead when a pending launch became ready, held or expired, once per change (LANE
    /// req. 25), and starts a launch that waited only for its worker once that worker answers.
    /// Runs with oversight.
    pub(super) async fn queue_sweep(self: &Arc<Self>) {
        let Ok((all, _)) = docs::all::<Queued>(&self.env.store, QUEUED).await else {
            return;
        };
        let now = now_ms();
        for (mut q, version) in all {
            if q.state != PENDING {
                continue;
            }
            let state = match self.readiness(&q, now).await {
                Ok(()) => "ready".to_string(),
                Err(e) if e == "expired" => e,
                Err(e) => format!("held: {e}"),
            };
            if state == "ready"
                && q.waits_for_worker()
                && !self.stopping.load(std::sync::atomic::Ordering::Acquire)
            {
                let detail = match self.start_queued(&q.key).await {
                    Ok(v) => format!(
                        "started on {}: {}",
                        q.worker().unwrap_or_default(),
                        s(&v, "outcome").unwrap_or_default()
                    ),
                    Err(e) => format!("did not start: {e}"),
                };
                self.emit(
                    Emit::new(EventKind::LaneQueued, Severity::Info, true, &q.key)
                        .with("lane", &q.key)
                        .with("detail", detail),
                );
                continue;
            }
            let changed = match (&q.told, state.as_str()) {
                (None, s) => s == "ready" || s == "expired",
                (Some(t), s) => t != s && !(t.starts_with("held") && s.starts_with("held")),
            };
            if !changed {
                continue;
            }
            q.told = Some(state.clone());
            if docs::put(&self.env.store, QUEUED, &q.key, &q, Some(version))
                .await
                .is_err()
            {
                continue;
            }
            let detail = match state.as_str() {
                "ready" => {
                    "is ready; start it with lane op start (the hub never starts it itself)".into()
                }
                "expired" => "expired; it can only be cancelled".into(),
                held => format!("is {held}"),
            };
            self.emit(
                Emit::new(
                    EventKind::LaneQueued,
                    Severity::Info,
                    state == "ready",
                    &q.key,
                )
                .with("lane", &q.key)
                .with("detail", detail),
            );
        }
    }

    /// Starts a queued launch for the lead or the owner after the re-checks (LANE req. 26).
    pub(super) async fn start_queued(self: &Arc<Self>, key: &str) -> Result<Value, String> {
        let (mut q, version) = docs::get::<Queued>(&self.env.store, QUEUED, key)
            .await?
            .ok_or_else(|| format!("no queued launch {key}"))?;
        if q.state != PENDING {
            return Err(format!(
                "queued launch {key} is {}; it is never retried automatically",
                q.state
            ));
        }
        let now = now_ms();
        self.readiness(&q, now)
            .await
            .map_err(|e| format!("not ready: {e}"))?;
        // Predecessors' signals read back live now, with their records unchanged meanwhile.
        for p in &q.after {
            let (rec, before) = self.load(p).await?;
            let results = self.read_checks(&rec).await;
            if let Some(f) = results.iter().find(|o| !o.pass) {
                return Err(format!("{p}: {} fails now: {}", f.signal, f.detail));
            }
            let (_, after) = self.load(p).await?;
            if after != before {
                return Err(format!("{p} changed during the checks; try again"));
            }
        }
        let open: OpenInput =
            serde_json::from_value(q.input.clone()).map_err(|e| format!("queued input: {e}"))?;
        let valid = record::validate(&open, &self.cfg().lanes)?;
        q.state = STARTING.into();
        let version = docs::put(&self.env.store, QUEUED, key, &q, Some(version)).await?;
        let digest = record::digest(&open);
        let outcome = self
            .open_one(valid, None, digest, None, q.kickoff.clone())
            .await
            .map(|(outcome, _)| outcome);
        match &outcome {
            // The kickoff may have reached the agent: left for inspection, never retried.
            Ok(o) if o.starts_with("unconfirmed") => q.note = Some(o.clone()),
            Ok(_) => q.state = STARTED.into(),
            // Nothing reached an agent: the entry is pending again.
            Err(e) => {
                q.state = PENDING.into();
                q.note = Some(e.clone());
            }
        }
        docs::put(&self.env.store, QUEUED, key, &q, Some(version)).await?;
        outcome.map(|o| json!({ "key": key, "outcome": o }))
    }

    /// Cancels a queued launch; refused once a lane record exists (LANE req. 27).
    pub(super) async fn cancel_queued(&self, key: &str) -> Result<String, String> {
        if self.row(key).await?.is_some() {
            return Err(format!("lane {key} exists; close it instead"));
        }
        docs::get::<Queued>(&self.env.store, QUEUED, key)
            .await?
            .ok_or_else(|| format!("no queued launch {key}"))?;
        docs::delete(&self.env.store, QUEUED, key).await?;
        Ok(format!("queued launch {key} cancelled"))
    }

    pub(super) async fn queued(&self, input: &Value) -> Result<Value, String> {
        let (all, damaged) = docs::all::<Queued>(&self.env.store, QUEUED).await?;
        let now = now_ms();
        let mut out = Vec::new();
        for (q, _) in all {
            if s(input, "key").is_some_and(|k| k != q.key) {
                continue;
            }
            let ready = self.readiness(&q, now).await;
            out.push(json!({
                "key": q.key,
                "state": q.state,
                "after": q.after,
                "priority": q.priority,
                "expires_ms": q.expires_ms,
                "ready": ready.is_ok(),
                "held": ready.err(),
                "note": q.note,
            }));
        }
        Ok(json!({ "queued": out, "damaged": damaged }))
    }
}
