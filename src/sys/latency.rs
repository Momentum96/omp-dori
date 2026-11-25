//! A fixed ring of the last 1,000 latency samples with percentile summaries (DESIGN 2.3).

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::Serialize;

const SAMPLES: usize = 1000;

/// Percentiles over the kept samples, in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct Summary {
    pub(crate) count: usize,
    pub(crate) p50_us: u64,
    pub(crate) p95_us: u64,
    pub(crate) p99_us: u64,
    pub(crate) max_us: u64,
}

/// The ring itself; share it through [`Latency`].
#[derive(Debug, Default)]
struct Ring {
    samples: Vec<u64>,
    next: usize,
}

/// A shared latency ring.
#[derive(Debug, Clone, Default)]
pub(crate) struct Latency(Arc<Mutex<Ring>>);

impl Latency {
    pub(crate) fn record(&self, elapsed: Duration) {
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        let mut guard = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let ring = &mut *guard;
        if ring.samples.len() < SAMPLES {
            ring.samples.push(micros);
        } else if let Some(slot) = ring.samples.get_mut(ring.next) {
            *slot = micros;
        }
        ring.next = (ring.next + 1) % SAMPLES;
    }

    pub(crate) fn summary(&self) -> Option<Summary> {
        let mut sorted = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .samples
            .clone();
        if sorted.is_empty() {
            return None;
        }
        sorted.sort_unstable();
        let at = |pct: usize| {
            sorted
                .get((sorted.len() - 1) * pct / 100)
                .copied()
                .unwrap_or(0)
        };
        Some(Summary {
            count: sorted.len(),
            p50_us: at(50),
            p95_us: at(95),
            p99_us: at(99),
            max_us: at(100),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_and_ring_bound() {
        let l = Latency::default();
        assert!(l.summary().is_none());
        for us in 1..=100 {
            l.record(Duration::from_micros(us));
        }
        let s = l.summary().unwrap();
        assert_eq!((s.count, s.p50_us, s.p99_us, s.max_us), (100, 50, 99, 100));
        for _ in 0..2000 {
            l.record(Duration::from_micros(7));
        }
        let s = l.summary().unwrap();
        assert_eq!((s.count, s.max_us), (SAMPLES, 7));
    }
}
