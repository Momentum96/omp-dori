//! The runtime the hub starts sessions with (DESIGN 4.1; HOST-30): headless or tern, chosen from
//! the config for each new session. Callers see one [`Runtime`] and one [`Control`].

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{broadcast, watch};

use super::headless::{self, Headless};
use super::tern::{self, Tern};
use super::{
    Control, DeliverOutcome, Delivery, EventPage, Goal, Runtime, SessionEvent, Snapshot,
    StartError, StartSpec, Started, StopRecord,
};

/// The configured runtime of a start.
pub(crate) enum Hosted {
    Headless(Headless),
    Tern(Tern),
}

/// A session of either runtime.
pub(crate) enum Hosting {
    Headless(Arc<headless::Session>),
    Tern(Arc<tern::Session>),
}

impl Runtime for Hosted {
    type Session = Hosting;

    fn kind(&self) -> &'static str {
        match self {
            Self::Headless(r) => r.kind(),
            Self::Tern(r) => r.kind(),
        }
    }

    async fn start(&self, spec: StartSpec) -> Result<(Arc<Hosting>, Started), StartError> {
        match self {
            Self::Headless(r) => r
                .start(spec)
                .await
                .map(|(s, started)| (Arc::new(Hosting::Headless(s)), started)),
            Self::Tern(r) => r
                .start(spec)
                .await
                .map(|(s, started)| (Arc::new(Hosting::Tern(s)), started)),
        }
    }
}

/// Calls the same method on whichever session this is.
macro_rules! each {
    ($self:ident, $s:ident => $call:expr) => {
        match $self {
            Hosting::Headless($s) => $call,
            Hosting::Tern($s) => $call,
        }
    };
}

impl Control for Hosting {
    fn snapshot(&self) -> Arc<Snapshot> {
        each!(self, s => s.snapshot())
    }

    fn states(&self) -> watch::Receiver<Arc<Snapshot>> {
        each!(self, s => s.states())
    }

    fn subscribe(&self) -> broadcast::Receiver<(u64, SessionEvent)> {
        each!(self, s => s.subscribe())
    }

    fn events_after(&self, after: u64, limit: usize) -> EventPage {
        each!(self, s => s.events_after(after, limit))
    }

    fn alive(&self) -> bool {
        each!(self, s => s.alive())
    }

    async fn refresh(&self) -> Result<(), String> {
        each!(self, s => s.refresh().await)
    }

    async fn deliver(&self, delivery: Delivery) -> DeliverOutcome {
        each!(self, s => s.deliver(delivery).await)
    }

    async fn interrupt(&self) -> Result<String, String> {
        each!(self, s => s.interrupt().await)
    }

    async fn set_model(&self, selector: &str) -> Result<String, String> {
        each!(self, s => s.set_model(selector).await)
    }

    async fn goal(&self, op: &str, objective: Option<&str>) -> Result<Goal, String> {
        each!(self, s => s.goal(op, objective).await)
    }

    async fn answer(
        &self,
        session_id: &str,
        dialog_id: &str,
        answer: &Value,
    ) -> Result<(), String> {
        each!(self, s => s.answer(session_id, dialog_id, answer).await)
    }

    fn accept_model(&self) -> Result<(), String> {
        each!(self, s => s.accept_model())
    }

    async fn close(&self, force: bool) -> Result<StopRecord, String> {
        each!(self, s => s.close(force).await)
    }
}
