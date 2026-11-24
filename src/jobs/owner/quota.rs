//! The quota watch and the quota gate (OJ-07, OJ-08, OJ-09; DESIGN 9.2). Each limit, identified by
//! provider, account ordinal and limit ID, has a band: the highest alert level its used share
//! reaches. A rise warns once; a fall from at or above the gate level to below it sends one recovery
//! note. While any watched limit is at or above the gate level, the gate holds new session starts
//! (sessions arrive in milestone 2; here the flag, its persistence and its lifting). The gate opens
//! when fresh data shows every watched limit below the level, or when every limit that closed it
//! passed its reset time, whichever comes first, so unavailable data never keeps it closed for ever.
//! When OMP reports no reset time (the field is unverified), the window length after the fetch is
//! used, and five hours when that is unknown too.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use super::Tick;
use crate::config::owner::Quota;
use crate::jobs::event::{Emit, EventKind, Severity};
use crate::omp::usage::Limit;

/// Release fallback when neither a reset time nor a window length is known.
const FALLBACK_RELEASE_MS: i64 = 5 * 3_600_000;
const GATE: &str = "gate";

/// One limit holding the gate, with the time it releases the gate at the latest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Cause {
    pub(crate) limit: String,
    pub(crate) release_ms: i64,
}

/// The hub-wide gate: causes per quota job, plus the owner's lift.
#[derive(Debug, Default)]
pub(crate) struct Gate {
    state: Mutex<GateState>,
}

#[derive(Debug, Default, Clone)]
struct GateState {
    causes: BTreeMap<String, Vec<Cause>>,
    lifted_until_ms: Option<i64>,
}

/// The gate as `cataly quota` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GateView {
    /// New session starts wait.
    pub(crate) held: bool,
    pub(crate) causes: Vec<Cause>,
    /// The owner lifted the gate until this instant.
    pub(crate) lifted_until_ms: Option<i64>,
}

impl Gate {
    fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Replaces one job's causes; once no cause is left, an owner lift ends too.
    pub(crate) fn set(&self, job: &str, causes: Vec<Cause>) {
        let mut s = self.lock();
        if causes.is_empty() {
            s.causes.remove(job);
            if s.causes.is_empty() {
                s.lifted_until_ms = None;
            }
        } else {
            s.causes.insert(job.to_string(), causes);
        }
    }

    pub(crate) fn view(&self, now: i64) -> GateView {
        let s = self.lock();
        let causes: Vec<Cause> = s.causes.values().flatten().cloned().collect();
        let lifted = s.lifted_until_ms.filter(|until| *until > now);
        GateView {
            held: !causes.is_empty() && lifted.is_none(),
            causes,
            lifted_until_ms: lifted,
        }
    }

    /// The owner lifts the gate until every current cause's window resets (OJ req. 25). Only the
    /// owner's control requests reach this. Returns the view afterwards.
    pub(crate) fn lift(&self, now: i64) -> GateView {
        {
            let mut s = self.lock();
            s.lifted_until_ms = s.causes.values().flatten().map(|c| c.release_ms).max();
        }
        self.view(now)
    }

    /// Restores the lift from the store at start.
    pub(crate) fn restore_lift(&self, until_ms: Option<i64>) {
        self.lock().lifted_until_ms = until_ms;
    }

    pub(crate) fn lifted_until(&self) -> Option<i64> {
        self.lock().lifted_until_ms
    }
}

/// The band of a used share: the highest level reached, 0 below all.
fn band(percent: f64, levels: &[u8]) -> u8 {
    levels
        .iter()
        .copied()
        .filter(|l| percent >= f64::from(*l))
        .max()
        .unwrap_or(0)
}

fn duration_text(ms: i64) -> String {
    if ms <= 0 {
        return "now".into();
    }
    let minutes = ms / 60_000;
    match (minutes / 1440, minutes % 1440 / 60, minutes % 60) {
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h{m}m"),
        (d, h, _) => format!("{d}d{h}h"),
    }
}

fn release_of(limit: &Limit, now: i64) -> i64 {
    limit.reset_ms.unwrap_or_else(|| {
        limit.fetched_ms.unwrap_or(now) + limit.window_ms.unwrap_or(FALLBACK_RELEASE_MS)
    })
}

/// Parses this job's stored causes.
pub(crate) fn stored_causes(value: Option<&str>) -> Vec<Cause> {
    value
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or_default()
}

pub(super) async fn tick(t: &mut Tick<'_>, q: &Quota) -> Result<(), String> {
    let omp = t.ctx.omp.path();
    let read = t
        .ctx
        .omp
        .usage
        .read(omp.as_deref(), t.ctx.config().limits.cache, true)
        .await;
    let mut causes = stored_causes(t.memory.get(GATE, "hold"));
    let had_hold = !causes.is_empty();
    let fresh = match (&read.error, &read.snapshot) {
        (None, Some(snapshot)) => Some(snapshot.usage.limits.clone()),
        _ => None,
    };
    let mut released = None;
    if let Some(limits) = fresh {
        let watched: Vec<&Limit> = limits
            .iter()
            .filter(|l| q.providers.is_empty() || q.providers.contains(&l.provider))
            .collect();
        for l in &watched {
            bands(t, q, l);
        }
        let above: Vec<Cause> = watched
            .iter()
            .filter(|l| l.used_percent.is_some_and(|p| p >= f64::from(q.hold)))
            .map(|l| Cause {
                limit: l.key(),
                release_ms: release_of(l, t.now),
            })
            .filter(|c| c.release_ms > t.now)
            .collect();
        if above.is_empty() && had_hold {
            released = Some(format!(
                "fresh usage data shows every watched limit below {}%",
                q.hold
            ));
        }
        causes = above;
    } else {
        // Unavailable data keeps the last bands; it never holds past the reported reset times.
        let before = causes.len();
        causes.retain(|c| c.release_ms > t.now);
        if before > 0 && causes.is_empty() {
            released = Some("the reported reset time passed".into());
        }
        t.soft_failure = Some(read.error.unwrap_or_else(|| "no usage data".into()));
    }
    let value = serde_json::to_string(&causes).map_err(|e| e.to_string())?;
    if causes.is_empty() {
        t.memory.clear(GATE, "hold");
    } else {
        t.memory.transition(GATE, "hold", &value);
    }
    if !had_hold && !causes.is_empty() {
        let limits: Vec<&str> = causes.iter().map(|c| c.limit.as_str()).collect();
        t.emit(
            Emit::new(EventKind::QuotaHold, Severity::Warning, true, GATE)
                .with("limits", limits.join(", "))
                .with("level", q.hold),
        );
    }
    if let Some(reason) = released {
        t.emit(
            Emit::new(EventKind::QuotaReleased, Severity::Info, false, GATE).with("reason", reason),
        );
    }
    // A dry tick takes no action: the hub-wide gate is left alone.
    if !t.dry {
        t.ctx.gate.set(&t.job.name, causes);
    }
    Ok(())
}

fn bands(t: &mut Tick<'_>, q: &Quota, l: &Limit) {
    let Some(percent) = l.used_percent else {
        return;
    };
    let key = l.key();
    let now_band = band(percent, &q.levels);
    let before: u8 = t
        .memory
        .get(&key, "band")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    t.memory.transition(&key, "band", &now_band.to_string());
    if t.first {
        return;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let shown = percent.round().clamp(0.0, 1000.0) as u32;
    if now_band > before {
        let reset_in = l
            .reset_ms
            .map_or_else(|| "unknown".to_string(), |r| duration_text(r - t.now));
        let window = l.window_ms.map_or_else(|| l.label.clone(), duration_text);
        t.emit(
            Emit::new(EventKind::QuotaLevel, Severity::Warning, true, &key)
                .with("limit", &l.id)
                .with("provider", l.provider_shown())
                .with("account", l.account)
                .with("percent", shown)
                .with("window", window)
                .with("reset_in", reset_in),
        );
    } else if before >= q.hold && now_band < q.hold {
        t.emit(
            Emit::new(EventKind::QuotaRecovered, Severity::Info, false, &key)
                .with("limit", &l.id)
                .with("provider", l.provider_shown())
                .with("account", l.account)
                .with("percent", shown),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_and_gate_rules() {
        assert_eq!(band(69.9, &[70, 85, 95]), 0);
        assert_eq!(band(86.0, &[70, 85, 95]), 85);
        let gate = Gate::default();
        assert!(!gate.view(0).held);
        gate.set(
            "q",
            vec![Cause {
                limit: "x#1/five".into(),
                release_ms: 1000,
            }],
        );
        assert!(gate.view(10).held);
        let view = gate.lift(10);
        assert!(!view.held && view.lifted_until_ms == Some(1000));
        assert!(gate.view(1000).held, "the lift ends when the window resets");
        gate.set("q", Vec::new());
        assert!(!gate.view(1000).held);
        assert_eq!(duration_text(90 * 60_000), "1h30m");
    }
}
