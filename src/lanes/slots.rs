//! Named exclusive slots around heavy commands (LANE-10, COORD-31, COORD-33; LANE req. 21, COORD
//! req. 42-46): a fair in-hub queue ordered by priority, then strict arrival. A holder keeps its
//! slot until both its client connection and its command have exited. After a hub start no slot is
//! granted for 5 s, so clients whose commands still run can re-assert their hold first. Grants and
//! queues are not stored (COORD "State that must survive a restart").

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::watch;

use crate::config::Config;
use crate::sys::now_ms;

/// No grant this long after the hub started (COORD req. 46).
pub(crate) const GRACE: Duration = Duration::from_secs(5);
/// Waiters are looked at again this often (load may have dropped, LANE req. 21).
const RECHECK: Duration = Duration::from_secs(15);
/// A holder whose client left is checked this often for its command's exit.
const ORPHAN_CHECK: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
struct Holder {
    token: u64,
    lane: String,
    since_ms: i64,
    pid: Option<u32>,
    client: bool,
}

#[derive(Debug, Clone)]
struct Waiter {
    token: u64,
    lane: String,
    priority: u8,
    seq: u64,
}

#[derive(Debug, Default)]
struct Slot {
    holders: Vec<Holder>,
    waiters: Vec<Waiter>,
}

#[derive(Debug, Default)]
struct Inner {
    next: u64,
    slots: BTreeMap<String, Slot>,
}

/// Where a request stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    /// 1-based place in the queue.
    Waiting(usize),
    Granted,
    Gone,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct HolderView {
    pub(crate) lane: String,
    pub(crate) since_ms: i64,
    pub(crate) pid: Option<u32>,
    pub(crate) client: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct SlotView {
    pub(crate) name: String,
    pub(crate) size: u32,
    pub(crate) holders: Vec<HolderView>,
    /// Waiting lanes in grant order.
    pub(crate) queue: Vec<String>,
}

/// The hub's slots.
pub(crate) struct Slots {
    inner: Mutex<Inner>,
    started: Instant,
    config: watch::Receiver<Arc<Config>>,
    /// Bumped on every change; requests watch it for their place.
    changed: watch::Sender<u64>,
}

fn alive(pid: u32) -> bool {
    i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .is_some_and(|p| rustix::process::test_kill_process(p).is_ok())
}

impl Slots {
    pub(crate) fn new(config: watch::Receiver<Arc<Config>>) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            started: Instant::now(),
            config,
            changed: watch::channel(0).0,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn bump(&self) {
        self.changed.send_modify(|g| *g = g.wrapping_add(1));
    }

    pub(crate) fn changes(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// Queues a request; grants happen in [`Self::evaluate`].
    pub(crate) fn request(&self, name: &str, lane: &str, priority: u8) -> u64 {
        let token = {
            let mut inner = self.lock();
            inner.next += 1;
            let token = inner.next;
            inner
                .slots
                .entry(name.to_string())
                .or_default()
                .waiters
                .push(Waiter {
                    token,
                    lane: lane.to_string(),
                    priority,
                    seq: token,
                });
            token
        };
        self.bump();
        token
    }

    /// A client whose command still runs re-asserts its hold after a hub restart (COORD req. 46).
    pub(crate) fn hold(&self, name: &str, lane: &str, pid: u32) -> u64 {
        let token = {
            let mut inner = self.lock();
            inner.next += 1;
            let token = inner.next;
            inner
                .slots
                .entry(name.to_string())
                .or_default()
                .holders
                .push(Holder {
                    token,
                    lane: lane.to_string(),
                    since_ms: now_ms(),
                    pid: Some(pid),
                    client: true,
                });
            token
        };
        self.bump();
        token
    }

    pub(crate) fn place(&self, name: &str, token: u64) -> Place {
        let inner = self.lock();
        let Some(slot) = inner.slots.get(name) else {
            return Place::Gone;
        };
        if slot.holders.iter().any(|h| h.token == token) {
            return Place::Granted;
        }
        let mut order: Vec<&Waiter> = slot.waiters.iter().collect();
        order.sort_by_key(|w| (w.priority, w.seq));
        order
            .iter()
            .position(|w| w.token == token)
            .map_or(Place::Gone, |i| Place::Waiting(i + 1))
    }

    /// The command of a granted request started.
    pub(crate) fn started(&self, name: &str, token: u64, pid: u32) {
        if let Some(h) = self
            .lock()
            .slots
            .get_mut(name)
            .and_then(|s| s.holders.iter_mut().find(|h| h.token == token))
        {
            h.pid = Some(pid);
        }
    }

    /// The client's connection ended: a waiter withdraws; a holder whose command still runs keeps
    /// the slot until the command exits (COORD req. 44).
    pub(crate) fn client_gone(&self, name: &str, token: u64) {
        {
            let mut inner = self.lock();
            if let Some(slot) = inner.slots.get_mut(name) {
                slot.waiters.retain(|w| w.token != token);
                if let Some(h) = slot.holders.iter_mut().find(|h| h.token == token) {
                    h.client = false;
                }
                slot.holders
                    .retain(|h| h.client || h.pid.is_some_and(alive));
            }
            inner
                .slots
                .retain(|_, s| !s.holders.is_empty() || !s.waiters.is_empty());
        }
        self.bump();
    }

    fn in_grace(&self) -> bool {
        self.started.elapsed() < GRACE
    }

    /// Grants free slots to the best waiters while the load allows (LANE req. 21).
    pub(crate) fn evaluate(&self, load_ok: bool) -> bool {
        if self.in_grace() || !load_ok {
            return false;
        }
        let config = Arc::clone(&self.config.borrow());
        let mut granted = false;
        {
            let mut inner = self.lock();
            for (name, slot) in &mut inner.slots {
                let size = usize::try_from(config.coord.slots.size(name)).unwrap_or(1);
                while slot.holders.len() < size && !slot.waiters.is_empty() {
                    slot.waiters.sort_by_key(|w| (w.priority, w.seq));
                    let w = slot.waiters.remove(0);
                    slot.holders.push(Holder {
                        token: w.token,
                        lane: w.lane,
                        since_ms: now_ms(),
                        pid: None,
                        client: true,
                    });
                    granted = true;
                }
            }
        }
        if granted {
            self.bump();
        }
        granted
    }

    /// Drops holders whose client left and whose command exited.
    fn sweep(&self) -> bool {
        let mut freed = false;
        {
            let mut inner = self.lock();
            for slot in inner.slots.values_mut() {
                let before = slot.holders.len();
                slot.holders
                    .retain(|h| h.client || h.pid.is_some_and(alive));
                freed |= slot.holders.len() != before;
            }
            inner
                .slots
                .retain(|_, s| !s.holders.is_empty() || !s.waiters.is_empty());
        }
        if freed {
            self.bump();
        }
        freed
    }

    fn state(&self) -> (bool, bool) {
        let inner = self.lock();
        let waiting = inner.slots.values().any(|s| !s.waiters.is_empty());
        let orphans = inner
            .slots
            .values()
            .any(|s| s.holders.iter().any(|h| !h.client));
        (waiting, orphans)
    }

    /// The one-minute load per core is below the configured level; unknown holds (LANE req. 18).
    async fn load_ok(&self) -> bool {
        let level = f64::from(self.config.borrow().coord.slots.load);
        #[allow(clippy::cast_precision_loss)]
        crate::sys::host::load()
            .await
            .is_ok_and(|l| l.one / l.cores.max(1) as f64 * 100.0 < level)
    }

    /// The slots task: grants on change, after the grace, every 15 s while someone waits, and frees
    /// orphaned holds when their command exits.
    pub(crate) async fn run(
        self: Arc<Self>,
        mut stop: watch::Receiver<bool>,
    ) -> Result<(), String> {
        let mut changes = self.changes();
        loop {
            if *stop.borrow() {
                return Ok(());
            }
            self.sweep();
            let (waiting, _) = self.state();
            if waiting {
                let ok = self.load_ok().await;
                self.evaluate(ok);
            }
            let (waiting, orphans) = self.state();
            let mut sleep = if orphans { ORPHAN_CHECK } else { RECHECK };
            if waiting && self.in_grace() {
                sleep = sleep
                    .min(GRACE.saturating_sub(self.started.elapsed()) + Duration::from_millis(20));
            }
            changes.borrow_and_update();
            tokio::select! {
                _ = changes.changed() => {}
                () = tokio::time::sleep(sleep) => {}
                _ = stop.changed() => {}
            }
        }
    }

    pub(crate) fn view(&self) -> Vec<SlotView> {
        let config = Arc::clone(&self.config.borrow());
        let inner = self.lock();
        inner
            .slots
            .iter()
            .map(|(name, s)| {
                let mut order: Vec<&Waiter> = s.waiters.iter().collect();
                order.sort_by_key(|w| (w.priority, w.seq));
                SlotView {
                    name: name.clone(),
                    size: config.coord.slots.size(name),
                    holders: s
                        .holders
                        .iter()
                        .map(|h| HolderView {
                            lane: h.lane.clone(),
                            since_ms: h.since_ms,
                            pid: h.pid,
                            client: h.client,
                        })
                        .collect(),
                    queue: order
                        .iter()
                        .map(|w| format!("{} (p{})", w.lane, w.priority))
                        .collect(),
                }
            })
            .collect()
    }

    /// One line per slot with holder, age and queue, "+N more" past three waiters (COORD req. 68).
    pub(crate) fn lines(&self) -> Vec<String> {
        let now = now_ms();
        self.view()
            .iter()
            .map(|v| {
                let held: Vec<String> = v
                    .holders
                    .iter()
                    .map(|h| format!("{} {}m", h.lane, (now - h.since_ms) / 60_000))
                    .collect();
                let mut line = format!(
                    "slot {} ({}/{}): {}",
                    v.name,
                    v.holders.len(),
                    v.size,
                    if held.is_empty() {
                        "free".into()
                    } else {
                        held.join(", ")
                    }
                );
                if !v.queue.is_empty() {
                    let shown: Vec<&str> = v.queue.iter().take(3).map(String::as_str).collect();
                    let _ = write!(line, " | queue {}", shown.join(", "));
                    if v.queue.len() > 3 {
                        let _ = write!(line, " +{} more", v.queue.len() - 3);
                    }
                }
                line
            })
            .collect()
    }

    #[cfg(test)]
    fn since_start(started: Instant, config: watch::Receiver<Arc<Config>>) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            started,
            config,
            changed: watch::channel(0).0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slots(in_grace: bool) -> Slots {
        let config = watch::channel(Arc::new(Config::default())).1;
        let started = if in_grace {
            Instant::now()
        } else {
            Instant::now()
                .checked_sub(GRACE * 2)
                .unwrap_or_else(Instant::now)
        };
        Slots::since_start(started, config)
    }

    #[test]
    fn priority_then_arrival_and_holds_until_both_exit() {
        let s = slots(false);
        let first = s.request("build", "a1", 5);
        let low = s.request("build", "a2", 7);
        let high = s.request("build", "a3", 2);
        let same = s.request("build", "a4", 2);
        assert_eq!(s.place("build", high), Place::Waiting(1));
        assert_eq!(s.place("build", same), Place::Waiting(2));
        assert!(s.evaluate(true));
        assert_eq!(s.place("build", high), Place::Granted);
        assert_eq!(s.place("build", first), Place::Waiting(2));
        // The client leaves while its command (this test process) still runs: the slot stays held.
        s.started("build", high, std::process::id());
        s.client_gone("build", high);
        assert_eq!(s.place("build", high), Place::Granted);
        assert!(!s.evaluate(true));
        // A command that has exited frees it.
        if let Some(h) = s
            .lock()
            .slots
            .get_mut("build")
            .and_then(|sl| sl.holders.first_mut())
        {
            h.pid = Some(u32::MAX - 1);
        }
        assert!(s.sweep());
        assert!(s.evaluate(true));
        assert_eq!(s.place("build", same), Place::Granted);
        s.client_gone("build", low);
        assert_eq!(s.place("build", low), Place::Gone);
        assert!(!s.evaluate(false), "a high load grants nothing");
        assert_eq!(s.lines().len(), 1);
    }

    #[test]
    fn nothing_is_granted_in_the_grace_but_holds_are_kept() {
        let s = slots(true);
        let w = s.request("test", "b1", 5);
        let h = s.hold("test", "b2", std::process::id());
        assert!(!s.evaluate(true));
        assert_eq!(s.place("test", w), Place::Waiting(1));
        assert_eq!(s.place("test", h), Place::Granted);
    }
}
