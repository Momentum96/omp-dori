//! The lead's sleep: after `[lead] idle_sleep` fully idle, the hub stops its OMP the orderly way
//! and keeps its session; a message that starts a turn resumes the same session, and the message
//! waits in the inbox until that session is accepted. The timer rides on the lead's own task.
//!
//! Idle means, for the whole time: no turn running and no dialog open (an approval or a question
//! card); nothing in the lead's inbox (a waking message, a quiet note for the next wake, a written
//! batch awaiting its entry, an in-doubt delivery); no open owner question; and no open lane. Every
//! session event, delivery, settled in-doubt entry and start counts as activity; an idle check that
//! finds the lead busy restarts the count.

use std::sync::{MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::{Asleep, LEAD, Lead, Phase};
use crate::host::{Control, Runtime, Status};
use crate::log;
use crate::store::host::SessionUpdate;

/// The stop the session record keeps for a sleeping lead; its detail is how the close went.
pub(super) const ASLEEP: &str = "asleep";

impl<R: Runtime> Lead<R> {
    pub(super) fn sleep(&self) -> MutexGuard<'_, Option<Asleep>> {
        self.asleep.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The lead did something: the idle time counts from now.
    pub(super) fn touch(&self) {
        *self.active.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    fn quiet_for(&self) -> Duration {
        self.active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .elapsed()
    }

    /// The configured idle time before sleep; `None` when sleep is off. Debug builds take
    /// `CATALY_IDLE_SLEEP` instead while sleep is on, so tests need not wait the 5-minute minimum.
    fn idle_sleep(&self) -> Option<Duration> {
        let configured = self.env.config.borrow().lead.as_ref()?.idle_sleep?;
        Some(test_idle_sleep().unwrap_or(configured))
    }

    /// The kept session while the lead is asleep.
    pub(crate) fn asleep(&self) -> Option<Asleep> {
        let asleep = self.sleep().clone();
        asleep.filter(|_| self.phase().0 == Phase::Asleep)
    }

    /// Whether the session `handle` is the one being put to sleep or asleep.
    pub(super) fn sleeps(&self, handle: &str) -> bool {
        self.sleep().as_ref().is_some_and(|a| a.handle == handle)
    }

    fn fall_asleep(&self, asleep: Asleep) {
        *self.phase() = (Phase::Asleep, None);
        *self.sleep() = Some(asleep);
    }

    /// The lead sleeps and a message that starts a turn waits for it.
    pub(super) async fn wants_wake(&self) -> bool {
        self.phase().0 == Phase::Asleep && self.mailbox.waking_pending().await
    }

    /// At hub start: a lead the previous run put to sleep stays asleep until it is needed.
    pub(super) async fn keep_asleep(&self) {
        if self.env.config.borrow().lead.is_none() || self.phase().0 != Phase::NotStarted {
            return;
        }
        let last = match self.env.store.read(|r| r.latest_session("lead")).await {
            Ok(last) => last,
            Err(e) => {
                log::warn("lead sleep not read", &[("error", &e)]);
                return;
            }
        };
        let Some(row) =
            last.filter(|r| r.state == "stopped" && r.stop_how.as_deref() == Some(ASLEEP))
        else {
            return;
        };
        let Some(session_id) = row.session_id else {
            return;
        };
        log::info("lead stays asleep", &[("session", &session_id)]);
        self.fall_asleep(Asleep {
            handle: row.handle,
            session_id,
            since_ms: row.stop_ms.unwrap_or(row.updated_ms),
        });
    }

    /// How long until the idle timer is due, while a live lead may sleep.
    pub(super) fn sleep_wait(&self) -> Option<Duration> {
        let after = self.idle_sleep()?;
        (self.phase().0 == Phase::Live).then(|| after.saturating_sub(self.quiet_for()))
    }

    /// Puts the lead to sleep once it has been idle for `idle_sleep`.
    pub(super) async fn sleep_if_idle(&self) {
        let Some(after) = self.idle_sleep() else {
            return;
        };
        if self.phase().0 != Phase::Live || self.quiet_for() < after || !self.idle_now().await {
            return;
        }
        // One start and one delivery at a time: whichever got in first wins.
        let _start = self.starting.lock().await;
        let _flush = self.flushing.lock().await;
        if self.quiet_for() < after || !self.idle_now().await {
            return;
        }
        self.hibernate(after).await;
    }

    /// Whether the lead is idle now; a busy lead restarts the idle count. A failed read is busy.
    async fn idle_now(&self) -> bool {
        let session_idle = self.phase().0 == Phase::Live
            && self.current().filter(|s| s.alive()).is_some_and(|s| {
                let snapshot = s.snapshot();
                snapshot.status == Status::Idle && snapshot.dialogs.is_empty()
            });
        let idle = session_idle
            && matches!(
                self.env
                    .store
                    .read(|r| Ok(r.inbox_open(LEAD)?.is_empty()
                        && r.questions(true)?.is_empty()
                        && r.lanes(true)?.is_empty()))
                    .await,
                Ok(true)
            );
        if !idle {
            self.touch();
        }
        idle
    }

    /// The orderly stop of the update path without force: it never interrupts a turn, and with the
    /// inbox empty nothing is in doubt. The record keeps the session for the wake.
    async fn hibernate(&self, after: Duration) {
        let Some(session) = self.current().filter(|s| s.alive()) else {
            return;
        };
        let snapshot = session.snapshot();
        let Some(session_id) = snapshot.session_id.clone() else {
            return;
        };
        let handle = snapshot.handle.clone();
        *self.sleep() = Some(Asleep {
            handle: handle.clone(),
            session_id: session_id.clone(),
            since_ms: crate::sys::now_ms(),
        });
        self.closing
            .store(true, std::sync::atomic::Ordering::Release);
        let record = match session.close(false).await {
            Ok(record) => record,
            Err(e) => {
                *self.sleep() = None;
                self.closing
                    .store(false, std::sync::atomic::Ordering::Release);
                self.touch();
                log::warn("lead not put to sleep", &[("error", &e)]);
                return;
            }
        };
        self.update(SessionUpdate {
            handle: handle.clone(),
            state: Some("stopped".into()),
            stop: Some((ASLEEP.into(), Some(record.how.clone()))),
            ..SessionUpdate::default()
        })
        .await;
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = None;
        self.fall_asleep(Asleep {
            handle,
            session_id: session_id.clone(),
            since_ms: record.at_ms,
        });
        log::info(
            "lead asleep",
            &[
                ("session", &session_id),
                ("idle_s", &after.as_secs()),
                ("close", &record.how),
            ],
        );
    }

    /// `lead stop` while the lead sleeps: it stays stopped, and its record shows the clean stop a
    /// live `lead stop` leaves. `false` when the lead is not asleep.
    pub(super) async fn stop_asleep(&self) -> bool {
        let _one = self.starting.lock().await;
        let Some(asleep) = self.asleep() else {
            return false;
        };
        self.update(SessionUpdate {
            handle: asleep.handle,
            stop: Some((
                "clean".into(),
                Some("stopped by the owner while asleep".into()),
            )),
            ..SessionUpdate::default()
        })
        .await;
        self.set_phase(Phase::Stopped, Some("stopped while asleep".into()));
        true
    }
}

#[cfg(debug_assertions)]
fn test_idle_sleep() -> Option<Duration> {
    std::env::var("CATALY_IDLE_SLEEP")
        .ok()
        .and_then(|v| crate::config::parse_duration(&v))
}

#[cfg(not(debug_assertions))]
const fn test_idle_sleep() -> Option<Duration> {
    None
}
