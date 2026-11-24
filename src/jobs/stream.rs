//! The event stream: durable append to the store, then live fan-out to followers and a wake-up for the
//! router. An event is "in the stream" once its append committed.

use std::sync::Arc;

use tokio::sync::{Notify, broadcast};

use crate::store::{EventRecord, NewEvent, Store, StoreError};

/// Live followers each get this many events of slack before they fall back to reading the store.
const LIVE_SLACK: usize = 256;

#[derive(Clone)]
pub(crate) struct Stream {
    store: Store,
    live: broadcast::Sender<EventRecord>,
    route_wake: Arc<Notify>,
}

impl Stream {
    pub(crate) fn new(store: Store, route_wake: Arc<Notify>) -> Self {
        let (live, _) = broadcast::channel(LIVE_SLACK);
        Self {
            store,
            live,
            route_wake,
        }
    }

    /// Appends events in one transaction and returns them with their IDs.
    pub(crate) async fn append(
        &self,
        events: Vec<NewEvent>,
    ) -> Result<Vec<EventRecord>, StoreError> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let records = self.store.append_events(events).await?;
        for record in &records {
            let _ = self.live.send(record.clone());
        }
        self.route_wake.notify_one();
        Ok(records)
    }

    /// Subscribes to events appended from now on.
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<EventRecord> {
        self.live.subscribe()
    }
}
