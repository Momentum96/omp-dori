//! The host sampler and pressure notices (LANE-08, LANE-09, WS-36). The admission rule is a library
//! used by this job now and by session starts later: any failing check holds a start, and an unknown
//! value always holds. The job tells the lead once when the host enters alert and once when it
//! clears, and separately once when it becomes ready for heavy compute and once when it is busy again.

use std::path::{Path, PathBuf};

use super::Tick;
use crate::config::owner::Pressure;
use crate::jobs::event::{Emit, EventKind, Severity};
use crate::sys::host;

/// Container memory headroom.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) enum Headroom {
    /// Could not be read.
    #[default]
    Unknown,
    /// No container limit applies.
    NoLimit,
    /// Headroom under the limit, in per cent (cgroups: Linux only).
    #[cfg(target_os = "linux")]
    Percent(f64),
}

/// One sample of the host. `None` means unknown.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Sample {
    /// One-minute load per core, in per cent.
    pub(crate) load_percent: Option<f64>,
    /// Free memory share in per cent (Linux available/total; macOS the kernel's memory level).
    pub(crate) memory_free_percent: Option<f64>,
    pub(crate) headroom: Headroom,
    pub(crate) disk_free_gb: Option<f64>,
    pub(crate) sessions: Option<u32>,
}

/// Why a session start must wait now (LANE req. 18); empty when it may go ahead.
pub(crate) fn admission(s: &Sample, p: &Pressure) -> Vec<String> {
    let mut held = Vec::new();
    let min = f64::from(p.min_free_memory);
    match s.memory_free_percent {
        Some(m) if m < min => held.push(format!("free memory {m:.0}% below {min:.0}%")),
        Some(_) => {}
        None => held.push("free memory unknown".into()),
    }
    match s.headroom {
        #[cfg(target_os = "linux")]
        Headroom::Percent(h) if h < min => {
            held.push(format!("container headroom {h:.0}% below {min:.0}%"));
        }
        #[cfg(target_os = "linux")]
        Headroom::Percent(_) => {}
        Headroom::NoLimit => {}
        Headroom::Unknown => held.push("container headroom unknown".into()),
    }
    #[allow(clippy::cast_precision_loss)]
    let disk_min = p.min_free_disk_gb as f64;
    match s.disk_free_gb {
        Some(d) if d < disk_min => held.push(format!("free disk {d:.0} GB below {disk_min:.0} GB")),
        Some(_) => {}
        None => held.push("free disk unknown".into()),
    }
    match s.sessions {
        Some(n) if p.max_sessions > 0 && n >= p.max_sessions => {
            held.push(format!(
                "{n} live sessions, the maximum is {}",
                p.max_sessions
            ));
        }
        Some(_) => {}
        None => held.push("session count unknown".into()),
    }
    held
}

/// Takes one sample. Live lane sessions do not exist before milestone 2, so their count is 0.
pub(crate) async fn sample(disk_path: &Path) -> Sample {
    #[allow(clippy::cast_precision_loss)]
    let load_percent = host::load()
        .await
        .ok()
        .map(|l| l.one / l.cores.max(1) as f64 * 100.0);
    #[allow(clippy::cast_precision_loss)]
    let memory_free_percent = host::memory()
        .await
        .ok()
        .and_then(|m| match m.level_percent {
            Some(level) => Some(f64::from(level)),
            None if m.total > 0 => Some(m.available as f64 / m.total as f64 * 100.0),
            None => None,
        });
    #[allow(clippy::cast_precision_loss)]
    let disk_free_gb = host::disk(disk_path).ok().map(|d| d.available as f64 / 1e9);
    Sample {
        load_percent,
        memory_free_percent,
        headroom: headroom(),
        disk_free_gb,
        sessions: Some(0),
    }
}

/// Container memory headroom (LANE req. 19); an unreadable cgroup is unknown, not "no limit".
#[cfg(target_os = "linux")]
fn headroom() -> Headroom {
    match host::headroom() {
        Ok(Some(h)) => Headroom::Percent(h),
        Ok(None) => Headroom::NoLimit,
        Err(_) => Headroom::Unknown,
    }
}

/// Container memory headroom: cgroups exist on Linux only, so no container limit applies.
#[cfg(not(target_os = "linux"))]
const fn headroom() -> Headroom {
    Headroom::NoLimit
}

fn disk_path(p: &Pressure) -> PathBuf {
    p.disk_path
        .clone()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub(super) async fn tick(t: &mut Tick<'_>, p: &Pressure) -> Result<(), String> {
    let s = sample(&disk_path(p)).await;
    evaluate(t, p, &s);
    Ok(())
}

fn evaluate(t: &mut Tick<'_>, p: &Pressure, s: &Sample) {
    let mut reasons = admission(s, p);
    match s.load_percent {
        Some(l) if l > f64::from(p.alert_load) => {
            reasons.insert(0, format!("load {l:.0}% above {}%", p.alert_load));
        }
        Some(_) => {}
        None => reasons.insert(0, "load unknown".into()),
    }
    let alert = if reasons.is_empty() { "0" } else { "1" };
    let was_alert = t.memory.get("host", "alert").map(str::to_string);
    t.memory.transition("host", "alert", alert);
    let load = s
        .load_percent
        .map_or_else(|| "unknown".to_string(), |l| format!("{l:.0}"));
    if !t.first && was_alert.as_deref() != Some(alert) {
        if alert == "1" {
            t.emit(
                Emit::new(EventKind::PressureAlert, Severity::Warning, true, "host")
                    .with("reasons", reasons.join("; ")),
            );
        } else {
            t.emit(Emit::new(
                EventKind::PressureClear,
                Severity::Info,
                false,
                "host",
            ));
        }
    }
    let ready = if s.load_percent.is_some_and(|l| l < f64::from(p.ready_load)) {
        "1"
    } else {
        "0"
    };
    let was_ready = t.memory.get("host", "ready").map(str::to_string);
    t.memory.transition("host", "ready", ready);
    if !t.first && was_ready.as_deref() != Some(ready) {
        let kind = if ready == "1" {
            EventKind::PressureReady
        } else {
            EventKind::PressureBusy
        };
        t.emit(Emit::new(kind, Severity::Info, false, "host").with("load", load));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::memory::Memory;
    use crate::jobs::tests::ctx_with;

    fn limits() -> Pressure {
        Pressure {
            alert_load: 150,
            ready_load: 80,
            min_free_memory: 20,
            min_free_disk_gb: 50,
            disk_path: None,
            max_sessions: 2,
        }
    }

    fn good() -> Sample {
        Sample {
            load_percent: Some(40.0),
            memory_free_percent: Some(60.0),
            headroom: Headroom::NoLimit,
            disk_free_gb: Some(200.0),
            sessions: Some(0),
        }
    }

    #[test]
    fn admission_holds_on_any_failure_and_on_unknown() {
        assert_eq!(admission(&good(), &limits()), Vec::<String>::new());
        let low = Sample {
            memory_free_percent: Some(10.0),
            ..good()
        };
        assert_eq!(admission(&low, &limits()), ["free memory 10% below 20%"]);
        let unknown = Sample {
            headroom: Headroom::Unknown,
            disk_free_gb: None,
            ..good()
        };
        assert_eq!(admission(&unknown, &limits()).len(), 2);
        let full = Sample {
            sessions: Some(2),
            ..good()
        };
        assert_eq!(admission(&full, &limits()).len(), 1);
        #[cfg(target_os = "linux")]
        {
            let low = Sample {
                headroom: Headroom::Percent(5.0),
                ..full
            };
            assert_eq!(admission(&low, &limits()).len(), 2);
        }
    }

    #[tokio::test]
    async fn notices_only_on_change_after_the_baseline() {
        let (ctx, _stop, _dir) = ctx_with("[jobs.host]\nkind = \"pressure\"\n", "pressure-ctx");
        let job = ctx.config().jobs.get("host").cloned().unwrap();
        let mut t = Tick {
            ctx: &ctx,
            job: &job,
            dry: false,
            first: true,
            memory: Memory::new("host", false, Vec::new()),
            emits: Vec::new(),
            now: 0,
            soft_failure: None,
        };
        let busy = Sample {
            load_percent: Some(200.0),
            ..good()
        };
        evaluate(&mut t, &limits(), &busy);
        assert!(t.emits.is_empty(), "baseline");
        t.first = false;
        evaluate(&mut t, &limits(), &busy);
        assert!(t.emits.is_empty(), "unchanged");
        evaluate(&mut t, &limits(), &good());
        let kinds: Vec<EventKind> = t.emits.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, [EventKind::PressureClear, EventKind::PressureReady]);
        evaluate(&mut t, &limits(), &busy);
        let kinds: Vec<EventKind> = t.emits.iter().skip(2).map(|e| e.kind).collect();
        assert_eq!(kinds, [EventKind::PressureAlert, EventKind::PressureBusy]);
        let live = sample(Path::new("/")).await;
        assert!(live.load_percent.is_some() && live.disk_free_gb.is_some());
    }
}
