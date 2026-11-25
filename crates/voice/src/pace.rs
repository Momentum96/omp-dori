//! Input pacing (VOICE-10, VOICE req. 62): a token bucket refilling at 1.15x real time, holding at
//! most 3 s and starting full. When the stream falls over 100 ms behind real time with nothing
//! pending, silence fills the gap within the bucket. Over 3 s of pending input fails the session;
//! before the session starts, nothing drains input, so only the newest 3 s are kept instead.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::MODEL_BYTES_PER_SECOND;

/// Refill rate relative to real time.
pub(crate) const RATE: f64 = 1.15;
/// Bucket size and pending limit.
const HOLD: Duration = Duration::from_secs(3);
/// Gap after which silence is inserted.
const BEHIND: Duration = Duration::from_millis(100);

/// Pending input beyond the limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Overflow;

#[derive(Debug)]
pub(crate) struct Pacer {
    tokens: f64,
    refilled: Instant,
    pending: VecDeque<u8>,
    /// Audio time sent so far, anchored at the start.
    clock: Instant,
}

fn bytes_for(d: Duration) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let per = MODEL_BYTES_PER_SECOND as f64;
    d.as_secs_f64() * per
}

impl Pacer {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            tokens: bytes_for(HOLD),
            refilled: now,
            pending: VecDeque::new(),
            clock: now,
        }
    }

    /// Queues input.
    pub(crate) fn push(&mut self, pcm: &[u8]) -> Result<(), Overflow> {
        self.pending.extend(pcm);
        #[allow(clippy::cast_precision_loss)]
        if self.pending.len() as f64 > bytes_for(HOLD) {
            return Err(Overflow);
        }
        Ok(())
    }

    /// Queues input while nothing can drain it (the session is starting): beyond 3 s the oldest
    /// input is dropped instead of failing, and the clock moves past it so no silence replaces it.
    pub(crate) fn keep_newest(&mut self, pcm: &[u8]) {
        self.pending.extend(pcm);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let max = bytes_for(HOLD) as usize;
        // Whole samples only.
        let excess = (self.pending.len().saturating_sub(max) + 1) & !1;
        let dropped = excess.min(self.pending.len());
        self.pending.drain(..dropped);
        #[allow(clippy::cast_precision_loss)]
        let skipped = Duration::from_secs_f64(dropped as f64 / MODEL_BYTES_PER_SECOND as f64);
        self.clock += skipped;
    }

    /// Pending bytes.
    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.pending.len()
    }

    /// What may go out now (always an even number of bytes).
    pub(crate) fn pull(&mut self, now: Instant) -> Vec<u8> {
        let dt = now.saturating_duration_since(self.refilled);
        self.refilled = now;
        self.tokens = (self.tokens + bytes_for(dt) * RATE).min(bytes_for(HOLD));
        if self.pending.is_empty() && now.saturating_duration_since(self.clock) > BEHIND {
            let gap = now.saturating_duration_since(self.clock);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let fill = (bytes_for(gap).min(self.tokens) as usize) & !1;
            self.pending.extend(std::iter::repeat_n(0u8, fill));
            if fill == 0 {
                // No tokens left to pad: move the clock so the gap is not padded twice.
                self.clock = now;
            }
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let allowed = (self.tokens.max(0.0) as usize).min(self.pending.len()) & !1;
        #[allow(clippy::cast_precision_loss)]
        {
            self.tokens -= allowed as f64;
        }
        let out: Vec<u8> = self.pending.drain(..allowed).collect();
        #[allow(clippy::cast_precision_loss)]
        let sent = Duration::from_secs_f64(out.len() as f64 / MODEL_BYTES_PER_SECOND as f64);
        self.clock += sent;
        if now.saturating_duration_since(self.clock) > HOLD {
            self.clock = now.checked_sub(HOLD).unwrap_or(now);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_full_then_holds_the_rate() {
        let t0 = Instant::now();
        let mut p = Pacer::new(t0);
        // 2.5 s of lead-in goes out at once: the bucket starts full.
        p.push(&vec![1u8; 120_000]).unwrap();
        assert_eq!(p.pull(t0).len(), 120_000);
        // Feed 2x real time for 20 s; output may not exceed the remaining bucket plus 1.15x.
        let mut sent = 0usize;
        for i in 1..=1000u32 {
            let now = t0 + Duration::from_millis(20) * i;
            if p.pending() < 100_000 {
                p.push(&vec![1u8; 1920]).unwrap();
            }
            sent += p.pull(now).len();
        }
        let limit =
            bytes_for(Duration::from_secs(20)) * RATE + bytes_for(Duration::from_millis(500));
        #[allow(clippy::cast_precision_loss)]
        let sent_f = sent as f64;
        assert!(sent_f <= limit + 2.0, "sent {sent} limit {limit}");
        assert!(sent_f >= bytes_for(Duration::from_secs(20)) * 1.10);
    }

    #[test]
    fn pads_silence_when_behind_and_fails_over_three_seconds() {
        let t0 = Instant::now();
        let mut p = Pacer::new(t0);
        let out = p.pull(t0 + Duration::from_millis(500));
        assert!(out.len() >= 19_000 && out.iter().all(|&b| b == 0));
        assert_eq!(p.push(&vec![0u8; 150_000]), Err(Overflow));
    }

    #[test]
    fn keeps_the_newest_three_seconds_while_starting() {
        let t0 = Instant::now();
        let mut p = Pacer::new(t0);
        // 5 s of input while the session starts: the oldest 2 s go, nothing fails.
        for i in 0..250u8 {
            p.keep_newest(&[i; 960]);
        }
        assert_eq!(p.pending(), 144_000);
        let out = p.pull(t0 + Duration::from_secs(5));
        assert_eq!(out.len(), 144_000);
        assert_eq!(
            (out[0], out[143_999]),
            (100, 249),
            "the newest 3 s, in order"
        );
        // The dropped 2 s count as sent: no silence is padded in their place.
        assert_eq!(p.pull(t0 + Duration::from_millis(5020)).len(), 0);
    }
}
