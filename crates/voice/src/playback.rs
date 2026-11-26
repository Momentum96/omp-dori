//! Bounded playback with generations and barge-in (VOICE-08, VOICE req. 59-60).
//!
//! Provider audio is upsampled to 48 kHz stereo and played in 20 ms frames. At most 5 s is held;
//! overflow clears the queue and fails the session. Every clear starts a new generation, which is
//! how a lead reply proves its frames played uninterrupted (VOICE req. 78).
//!
//! The provider streams output audio continuously, silence included, so only audible audio (at or
//! above -55 dBFS) counts as output: the played count, the last-played time and draining all refer
//! to it, and quiet audio between replies never holds off a pause.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::audio::{FRAME_STEREO, Up, dbfs};

/// Most audio held, in 48 kHz stereo samples (5 s).
const HOLD: usize = 5 * 48_000 * 2;
/// Owner speech this long above the wake level clears playback...
const BARGE_SPEECH: Duration = Duration::from_millis(400);
/// ...while more than this is queued.
const BARGE_QUEUED: Duration = Duration::from_millis(400);
/// Provider audio below this level is dropped while more than 200 ms is queued.
const QUIET_DBFS: f64 = -55.0;
const QUIET_QUEUED: Duration = Duration::from_millis(200);
/// A partial last frame is padded after this long without new output.
const PAD_AFTER: Duration = Duration::from_secs(1);

/// Whether provider audio is audible output rather than the silence streamed between replies.
pub(crate) fn audible(mono: &[i16]) -> bool {
    dbfs(mono) >= QUIET_DBFS
}

/// The queue overflowed; the session fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Overflow;

#[derive(Debug)]
pub(crate) struct Playback {
    queue: VecDeque<i16>,
    up: Up,
    generation: u64,
    last_input: Option<Instant>,
    /// When the owner's current run above the wake level began.
    speech_since: Option<Instant>,
    /// When the last audible frame was played; output-quiet checks use it.
    last_played: Option<Instant>,
    /// Audible frames played in the current generation.
    played: u64,
    /// Queued samples up to the end of the last audible audio.
    audible: usize,
}

fn queued(samples: usize) -> Duration {
    Duration::from_micros(samples as u64 * 1_000_000 / 96_000)
}

impl Playback {
    pub(crate) fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            up: Up::default(),
            generation: 0,
            last_input: None,
            speech_since: None,
            last_played: None,
            played: 0,
            audible: 0,
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn played(&self) -> u64 {
        self.played
    }

    /// Queued audio as time.
    pub(crate) fn queued(&self) -> Duration {
        queued(self.queue.len())
    }

    /// Queued audio up to the end of the last audible audio, as time.
    pub(crate) fn audible_queued(&self) -> Duration {
        queued(self.audible)
    }

    /// No audible audio is left to play.
    pub(crate) fn drained(&self) -> bool {
        self.audible == 0
    }

    /// When the last audible frame played.
    pub(crate) fn last_played(&self) -> Option<Instant> {
        self.last_played
    }

    /// Adds provider audio (24 kHz mono). Quiet audio is dropped while over 200 ms is queued.
    pub(crate) fn push(&mut self, mono: &[i16], now: Instant) -> Result<(), Overflow> {
        let audible = audible(mono);
        if self.queued() > QUIET_QUEUED && !audible {
            return Ok(());
        }
        let stereo = self.up.push(mono);
        self.queue.extend(stereo);
        if audible {
            self.audible = self.queue.len();
        }
        self.last_input = Some(now);
        if self.queue.len() > HOLD {
            self.clear();
            return Err(Overflow);
        }
        Ok(())
    }

    /// Drops everything queued and starts a new generation.
    pub(crate) fn clear(&mut self) {
        self.queue.clear();
        self.audible = 0;
        self.up.reset();
        self.generation += 1;
        self.played = 0;
    }

    /// Feeds the owner's level for one 20 ms frame; returns true when it barged in (cleared).
    pub(crate) fn owner_level(&mut self, level: f64, wake_dbfs: f64, now: Instant) -> bool {
        if level < wake_dbfs {
            self.speech_since = None;
            return false;
        }
        let since = *self.speech_since.get_or_insert(now);
        if now.saturating_duration_since(since) + Duration::from_millis(20) >= BARGE_SPEECH
            && self.queued() > BARGE_QUEUED
        {
            self.clear();
            self.speech_since = None;
            return true;
        }
        false
    }

    /// The next 20 ms frame to play, if one is ready. A partial frame is padded with silence once no
    /// output arrived for 1 s.
    pub(crate) fn next_frame(&mut self, now: Instant) -> Option<Vec<i16>> {
        if self.queue.len() >= FRAME_STEREO {
            self.played_frame(FRAME_STEREO, now);
            return Some(self.queue.drain(..FRAME_STEREO).collect());
        }
        let stale = self
            .last_input
            .is_some_and(|t| now.saturating_duration_since(t) >= PAD_AFTER);
        if !self.queue.is_empty() && stale {
            let mut frame: Vec<i16> = self.queue.drain(..).collect();
            self.played_frame(frame.len(), now);
            frame.resize(FRAME_STEREO, 0);
            return Some(frame);
        }
        None
    }

    /// Bookkeeping for a frame of `samples` leaving the queue: it counts when it holds audible audio.
    fn played_frame(&mut self, samples: usize, now: Instant) {
        if self.audible > 0 {
            self.audible = self.audible.saturating_sub(samples);
            self.last_played = Some(now);
            self.played += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loud(ms: usize) -> Vec<i16> {
        (0..ms * 24)
            .map(|i| if i % 2 == 0 { 8000 } else { -8000 })
            .collect()
    }

    #[test]
    fn barge_in_clears_and_bumps_generation() {
        let t0 = Instant::now();
        let mut p = Playback::new();
        p.push(&loud(1000), t0).unwrap();
        let g = p.generation();
        let mut cleared = false;
        for i in 0..25u32 {
            cleared |= p.owner_level(-20.0, -45.0, t0 + Duration::from_millis(20) * i);
        }
        assert!(cleared);
        assert!(p.drained());
        assert_eq!(p.generation(), g + 1);
    }

    #[test]
    fn no_barge_in_when_little_is_queued_or_speech_is_short() {
        let t0 = Instant::now();
        let mut p = Playback::new();
        p.push(&loud(300), t0).unwrap();
        for i in 0..30u32 {
            assert!(!p.owner_level(-20.0, -45.0, t0 + Duration::from_millis(20) * i));
        }
        p.push(&loud(1000), t0).unwrap();
        for i in 0..10u32 {
            assert!(!p.owner_level(-20.0, -45.0, t0 + Duration::from_millis(20) * i));
        }
        assert!(!p.owner_level(-60.0, -45.0, t0));
    }

    #[test]
    fn overflow_fails_and_quiet_audio_is_dropped() {
        let t0 = Instant::now();
        let mut p = Playback::new();
        p.push(&loud(300), t0).unwrap();
        p.push(&vec![10i16; 24_000], t0).unwrap();
        assert!(p.queued() < Duration::from_millis(400));
        let mut q = Playback::new();
        assert_eq!(q.push(&loud(5100), t0), Err(Overflow));
        assert!(q.drained());
    }

    #[test]
    fn partial_frame_padded_after_a_second() {
        let t0 = Instant::now();
        let mut p = Playback::new();
        p.push(&loud(30), t0).unwrap();
        assert!(p.next_frame(t0).is_some());
        assert!(p.next_frame(t0).is_none());
        let f = p.next_frame(t0 + Duration::from_secs(1)).unwrap();
        assert_eq!(f.len(), FRAME_STEREO);
        assert!(p.drained());
    }
}
