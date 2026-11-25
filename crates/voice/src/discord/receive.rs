//! The owner's receive stream (VOICE-06, VOICE-07; VOICE req. 56-58).
//!
//! Only the owner's SSRC is decoded; every other speaker is dropped before decryption. A stream
//! that ends is resubscribed at once if it lived over 10 s, else after a delay doubling from 100 ms
//! to 2 s. Once a second, an owner Discord reports as speaking with no audio decoded for 3 s gets a
//! fresh stream. With DAVE active, unencrypted owner frames are accepted only within 30 s of an
//! encryption-group change; end-to-end decryption failures are counted per burst (1 s quiet ends
//! one).

use std::time::{Duration, Instant};

use crate::audio::down;
use crate::opus::Decode;

const LIVED: Duration = Duration::from_secs(10);
const FIRST_DELAY: Duration = Duration::from_millis(100);
const MAX_DELAY: Duration = Duration::from_secs(2);
const STALL: Duration = Duration::from_secs(3);
const STALL_LOG: Duration = Duration::from_secs(10);
const PLAINTEXT_WINDOW: Duration = Duration::from_secs(30);
const BURST_QUIET: Duration = Duration::from_secs(1);
/// Undecodable frames in a row that end a stream.
const DEAD_AFTER: u32 = 50;
/// The DAVE protocol frame marker.
const MAGIC: [u8; 2] = [0xFA, 0xFA];

struct Stream {
    opened: Instant,
    decoder: Decode,
    last_audio: Instant,
    bad_run: u32,
}

/// Something worth one log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Notice {
    Stalled,
    DecryptBurst(u32),
    Resubscribed,
}

/// End-to-end decryption of one frame; `None` when it fails.
pub(crate) type Decrypt<'a> = dyn FnMut(&[u8]) -> Option<Vec<u8>> + 'a;

/// The owner's receive side.
pub(crate) struct Receiver {
    owner_ssrc: Option<u32>,
    stream: Option<Stream>,
    resubscribe_at: Option<Instant>,
    delay: Duration,
    group_changed: Option<Instant>,
    speaking: bool,
    stall_logged: Option<Instant>,
    burst: Option<(Instant, u32)>,
    notices: Vec<Notice>,
    /// Streams opened so far (for tests and status).
    pub(crate) opened: u32,
}

impl Receiver {
    pub(crate) fn new() -> Self {
        Self {
            owner_ssrc: None,
            stream: None,
            resubscribe_at: None,
            delay: FIRST_DELAY,
            group_changed: None,
            speaking: false,
            stall_logged: None,
            burst: None,
            notices: Vec::new(),
            opened: 0,
        }
    }

    /// The owner's SSRC from a speaking event.
    pub(crate) fn owner_ssrc(&mut self, ssrc: u32, now: Instant) {
        if self.owner_ssrc != Some(ssrc) {
            self.owner_ssrc = Some(ssrc);
            self.end(now);
        }
    }

    pub(crate) fn speaking(&mut self, on: bool) {
        self.speaking = on;
    }

    /// An encryption-group change (DAVE transition executed).
    pub(crate) fn group_changed(&mut self, now: Instant) {
        self.group_changed = Some(now);
    }

    pub(crate) fn notices(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.notices)
    }

    fn end(&mut self, now: Instant) {
        if let Some(s) = self.stream.take() {
            let lived = now.saturating_duration_since(s.opened);
            if lived > LIVED {
                self.delay = FIRST_DELAY;
                self.resubscribe_at = Some(now);
            } else {
                self.resubscribe_at = Some(now + self.delay);
                self.delay = (self.delay * 2).min(MAX_DELAY);
            }
        }
    }

    fn ensure(&mut self, now: Instant) -> bool {
        if self.stream.is_some() {
            return true;
        }
        if self.resubscribe_at.is_some_and(|t| now < t) {
            return false;
        }
        let Ok(decoder) = Decode::new() else {
            return false;
        };
        if self.opened > 0 {
            self.notices.push(Notice::Resubscribed);
        }
        self.opened += 1;
        self.resubscribe_at = None;
        self.stream = Some(Stream {
            opened: now,
            decoder,
            last_audio: now,
            bad_run: 0,
        });
        true
    }

    /// One frame from `ssrc` after transport decryption. `dave` is `Some(decrypt)` while DAVE is
    /// active. Returns 20 ms of owner audio at 24 kHz mono.
    pub(crate) fn frame(
        &mut self,
        ssrc: u32,
        frame: &[u8],
        dave: Option<&mut Decrypt<'_>>,
        now: Instant,
    ) -> Option<Vec<i16>> {
        if self.owner_ssrc != Some(ssrc) {
            return None;
        }
        if !self.ensure(now) {
            return None;
        }
        let opus = match dave {
            None => frame.to_vec(),
            Some(decrypt) if frame.ends_with(&MAGIC) => {
                if let Some(plain) = decrypt(frame) {
                    plain
                } else {
                    self.burst = Some(match self.burst {
                        Some((_, n)) => (now, n + 1),
                        None => (now, 1),
                    });
                    return None;
                }
            }
            Some(_) => {
                let window = self
                    .group_changed
                    .is_some_and(|t| now.saturating_duration_since(t) <= PLAINTEXT_WINDOW);
                if !window {
                    return None;
                }
                frame.to_vec()
            }
        };
        let stream = self.stream.as_mut()?;
        if let Some(stereo) = stream.decoder.decode(&opus) {
            stream.bad_run = 0;
            stream.last_audio = now;
            return Some(down(&stereo));
        }
        stream.bad_run += 1;
        if stream.bad_run >= DEAD_AFTER {
            self.end(now);
        }
        None
    }

    /// The once-a-second check (stall) and burst bookkeeping.
    pub(crate) fn tick(&mut self, now: Instant) {
        if let Some((last, n)) = self.burst
            && now.saturating_duration_since(last) >= BURST_QUIET
        {
            self.notices.push(Notice::DecryptBurst(n));
            self.burst = None;
        }
        let stalled = self.speaking
            && self
                .stream
                .as_ref()
                .is_some_and(|s| now.saturating_duration_since(s.last_audio) >= STALL);
        if stalled {
            if self
                .stall_logged
                .is_none_or(|t| now.saturating_duration_since(t) >= STALL_LOG)
            {
                self.stall_logged = Some(now);
                self.notices.push(Notice::Stalled);
            }
            self.end(now);
        }
        if self.stream.is_none() && self.owner_ssrc.is_some() {
            let _ = self.ensure(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::FRAME_STEREO;
    use crate::opus::Encode;

    fn opus() -> Vec<u8> {
        let frame: Vec<i16> = (0..FRAME_STEREO)
            .map(|i| if i % 4 < 2 { 6000 } else { -6000 })
            .collect();
        Encode::new().unwrap().encode(&frame).unwrap()
    }

    /// Check 2: only the owner's audio is received.
    #[test]
    fn only_the_owner_is_decoded() {
        let t0 = Instant::now();
        let mut r = Receiver::new();
        r.owner_ssrc(7, t0);
        let f = opus();
        assert!(r.frame(8, &f, None, t0).is_none());
        let audio = r.frame(7, &f, None, t0).unwrap();
        assert_eq!(audio.len(), 480);
    }

    /// Check 2: a dead stream is resubscribed, at once after 10 s, else with a doubling delay.
    #[test]
    fn dead_streams_are_resubscribed() {
        let t0 = Instant::now();
        let mut r = Receiver::new();
        r.owner_ssrc(7, t0);
        r.speaking(true);
        let f = opus();
        assert!(r.frame(7, &f, None, t0).is_some());
        assert_eq!(r.opened, 1);
        // Discord says the owner speaks but nothing decodes for 3 s: the stream restarts.
        r.tick(t0 + Duration::from_secs(3));
        assert_eq!(r.notices(), vec![Notice::Stalled]);
        // It lived under 10 s: resubscribed after 100 ms, not before.
        assert!(
            r.frame(7, &f, None, t0 + Duration::from_millis(3050))
                .is_none()
        );
        assert!(
            r.frame(7, &f, None, t0 + Duration::from_millis(3100))
                .is_some()
        );
        assert_eq!(r.opened, 2);
        assert_eq!(r.notices(), vec![Notice::Resubscribed]);
        // Undecodable frames are skipped; 50 in a row end the stream, next delay 200 ms.
        let t1 = t0 + Duration::from_millis(3200);
        for _ in 0..DEAD_AFTER {
            assert!(r.frame(7, &[0xff, 0xff, 0xff], None, t1).is_none());
        }
        assert!(
            r.frame(7, &f, None, t1 + Duration::from_millis(150))
                .is_none()
        );
        assert!(
            r.frame(7, &f, None, t1 + Duration::from_millis(200))
                .is_some()
        );
        // A stream that lived over 10 s is resubscribed at once.
        let t2 = t1 + Duration::from_secs(12);
        for _ in 0..DEAD_AFTER {
            let _ = r.frame(7, &[0xff, 0xff, 0xff], None, t2);
        }
        assert!(r.frame(7, &f, None, t2).is_some());
    }

    /// Check 2: unencrypted frames only within 30 s of an encryption change; failures per burst.
    #[test]
    fn plaintext_window_and_decrypt_bursts() {
        let t0 = Instant::now();
        let mut r = Receiver::new();
        r.owner_ssrc(7, t0);
        let f = opus();
        let mut never = |_: &[u8]| -> Option<Vec<u8>> { None };
        assert!(r.frame(7, &f, Some(&mut never), t0).is_none());
        r.group_changed(t0);
        assert!(
            r.frame(7, &f, Some(&mut never), t0 + Duration::from_secs(30))
                .is_some()
        );
        assert!(
            r.frame(7, &f, Some(&mut never), t0 + Duration::from_secs(31))
                .is_none()
        );
        let mut sealed = f.clone();
        sealed.extend_from_slice(&MAGIC);
        let mut strip = |p: &[u8]| -> Option<Vec<u8>> { p.get(..p.len() - 2).map(<[u8]>::to_vec) };
        assert!(
            r.frame(7, &sealed, Some(&mut strip), t0 + Duration::from_secs(40))
                .is_some()
        );
        let t = t0 + Duration::from_secs(41);
        for _ in 0..3 {
            assert!(r.frame(7, &sealed, Some(&mut never), t).is_none());
        }
        r.tick(t + Duration::from_millis(500));
        assert_eq!(r.notices(), Vec::<Notice>::new());
        r.tick(t + Duration::from_secs(1));
        assert_eq!(r.notices(), vec![Notice::DecryptBurst(3)]);
    }
}
