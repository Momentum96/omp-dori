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

/// Takes one sample with a session count of 0; a lane start puts the live count in its place.
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

/// The file system the disk check reads: the configured path, else home, else `/`.
pub(crate) fn disk_path(p: &Pressure) -> PathBuf {
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

/// Load margin, in percentage points, the load must cross back over before a state changes again,
/// so a load near a threshold cannot flip it every tick.
pub(crate) const HYSTERESIS: f64 = 10.0;
/// How long the host stays failing a check before the alert is raised.
pub(crate) const ALERT_AFTER_MS: i64 = 5 * 60_000;

fn evaluate(t: &mut Tick<'_>, p: &Pressure, s: &Sample) {
    let was_alert = t.memory.get("host", "alert") == Some("1");
    let over_since = t
        .memory
        .get("host", "over_since")
        .and_then(|v| v.parse::<i64>().ok());
    let mut reasons = admission(s, p);
    // Once over the alert line, the load must fall a margin below it to count as under.
    let line = f64::from(p.alert_load)
        - if over_since.is_some() {
            HYSTERESIS
        } else {
            0.0
        };
    match s.load_percent {
        Some(l) if l > line => {
            reasons.insert(0, format!("load {l:.0}% above {}%", p.alert_load));
        }
        Some(_) => {}
        None => reasons.insert(0, "load unknown".into()),
    }
    let since = if reasons.is_empty() {
        t.memory.clear("host", "over_since");
        None
    } else {
        let since = over_since.unwrap_or(t.now);
        t.memory
            .transition("host", "over_since", &since.to_string());
        Some(since)
    };
    // The alert is raised after five minutes over and lasts until the host is under again.
    let alert = since.is_some_and(|s| was_alert || t.now - s >= ALERT_AFTER_MS);
    t.memory
        .transition("host", "alert", if alert { "1" } else { "0" });
    if !t.first && alert != was_alert {
        if alert {
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
    let was_ready = t.memory.get("host", "ready") == Some("1");
    let ready_line = f64::from(p.ready_load) + if was_ready { HYSTERESIS } else { 0.0 };
    let ready = s.load_percent.is_some_and(|l| l < ready_line);
    let known = t.memory.get("host", "ready").is_some();
    t.memory
        .transition("host", "ready", if ready { "1" } else { "0" });
    if !t.first && known && ready != was_ready {
        let kind = if ready {
            EventKind::PressureReady
        } else {
            EventKind::PressureBusy
        };
        let load = s
            .load_percent
            .map_or_else(|| "unknown".to_string(), |l| format!("{l:.0}"));
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

    /// The default rule (no pressure job configured) holds a start below 50 GB of free disk; the
    /// integration tests set their own floors so they never depend on the host's disk.
    #[test]
    fn the_default_rule_refuses_a_small_disk() {
        let small = Sample {
            disk_free_gb: Some(12.0),
            ..good()
        };
        assert_eq!(
            admission(&small, &Pressure::default()),
            ["free disk 12 GB below 50 GB"]
        );
        let enough = Sample {
            disk_free_gb: Some(50.0),
            ..good()
        };
        assert_eq!(
            admission(&enough, &Pressure::default()),
            Vec::<String>::new()
        );
    }

    #[tokio::test]
    async fn notices_after_five_minutes_with_hysteresis() {
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
        let load = |l: f64| Sample {
            load_percent: Some(l),
            ..good()
        };
        let at = |t: &mut Tick<'_>, minute: i64, l: f64| -> Vec<EventKind> {
            t.now = minute * 60_000;
            let before = t.emits.len();
            evaluate(t, &limits(), &load(l));
            t.emits.iter().skip(before).map(|e| e.kind).collect()
        };
        assert!(at(&mut t, 0, 200.0).is_empty(), "baseline");
        t.first = false;
        assert!(at(&mut t, 1, 200.0).is_empty(), "not five minutes yet");
        assert!(
            at(&mut t, 4, 145.0).is_empty(),
            "inside the margin stays over"
        );
        assert_eq!(at(&mut t, 5, 200.0), [EventKind::PressureAlert]);
        assert!(at(&mut t, 6, 200.0).is_empty(), "unchanged");
        assert_eq!(
            at(&mut t, 7, 40.0),
            [EventKind::PressureClear, EventKind::PressureReady]
        );
        // Short spikes over the alert line never last five minutes: nothing is said.
        for minute in 8..30 {
            let l = if minute % 2 == 0 { 160.0 } else { 100.0 };
            let said = at(&mut t, minute, l);
            assert!(
                said.iter().all(|k| *k != EventKind::PressureAlert),
                "{said:?}"
            );
        }
        // A load wobbling around the ready line does not flip ready and busy.
        at(&mut t, 30, 40.0);
        for minute in 32..50 {
            let l = if minute % 2 == 0 { 79.0 } else { 86.0 };
            assert!(at(&mut t, minute, l).is_empty(), "minute {minute}");
        }
        assert_eq!(at(&mut t, 50, 95.0), [EventKind::PressureBusy]);
        assert!(at(&mut t, 51, 85.0).is_empty(), "busy until under the line");
        assert_eq!(at(&mut t, 52, 79.0), [EventKind::PressureReady]);
        let live = sample(Path::new("/")).await;
        assert!(live.load_percent.is_some() && live.disk_free_gb.is_some());
    }
}
