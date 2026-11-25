//! Format conversion and level metering (VOICE-08, VOICE-09, VOICE req. 59-61).
//!
//! Discord audio is 48 kHz stereo 16-bit; the provider speaks 24 kHz mono 16-bit little-endian PCM.
//! Down: the four samples of each stereo pair of frames are averaged into one. Up: linear
//! interpolation between consecutive provider samples, carried across chunk edges, then duplicated to
//! both channels. Both transports use these functions, so the session never sees a difference.

use std::collections::VecDeque;

/// Samples per channel in one 20 ms Discord frame.
pub(crate) const FRAME_48K: usize = 960;
/// Interleaved stereo samples in one 20 ms Discord frame.
pub(crate) const FRAME_STEREO: usize = FRAME_48K * 2;
/// Provider samples in 20 ms.
pub(crate) const FRAME_24K: usize = 480;
/// Bytes of 2.5 s of provider audio (VOICE req. 61).
pub(crate) const LEAD_IN_BYTES: usize = 120_000;

/// 48 kHz interleaved stereo to 24 kHz mono: each output sample averages two stereo frames.
pub(crate) fn down(stereo: &[i16]) -> Vec<i16> {
    let (quads, _) = stereo.as_chunks::<4>();
    quads
        .iter()
        .map(|q| {
            let sum: i32 = q.iter().map(|&s| i32::from(s)).sum();
            i16::try_from(sum / 4).unwrap_or(0)
        })
        .collect()
}

/// 24 kHz mono to 48 kHz stereo with interpolation that continues across chunks.
#[derive(Debug, Default)]
pub(crate) struct Up {
    last: Option<i16>,
}

impl Up {
    /// Converts one chunk. Each input sample yields two output frames: the midpoint between the
    /// previous sample and this one, then this one.
    pub(crate) fn push(&mut self, mono: &[i16]) -> Vec<i16> {
        let mut out = Vec::with_capacity(mono.len() * 4);
        for &s in mono {
            let prev = self.last.unwrap_or(s);
            let mid = i16::midpoint(prev, s);
            out.extend_from_slice(&[mid, mid, s, s]);
            self.last = Some(s);
        }
        out
    }

    /// Forgets the carried sample (after a playback clear).
    pub(crate) fn reset(&mut self) {
        self.last = None;
    }
}

/// Little-endian PCM bytes to samples (a trailing odd byte is ignored).
pub(crate) fn samples(bytes: &[u8]) -> Vec<i16> {
    let (pairs, _) = bytes.as_chunks::<2>();
    pairs.iter().map(|&b| i16::from_le_bytes(b)).collect()
}

/// Samples to little-endian PCM bytes.
pub(crate) fn bytes(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// RMS level in dBFS; silence is -120.
pub(crate) fn dbfs(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return -120.0;
    }
    let sum: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    #[allow(clippy::cast_precision_loss)]
    let rms = (sum / samples.len() as f64).sqrt();
    if rms < 1.0 {
        return -120.0;
    }
    (20.0 * (rms / 32768.0).log10()).max(-120.0)
}

/// The last 2.5 s of owner audio while no session runs (VOICE req. 61).
#[derive(Debug, Default)]
pub(crate) struct LeadIn {
    buf: VecDeque<u8>,
}

impl LeadIn {
    pub(crate) fn push(&mut self, pcm: &[u8]) {
        self.buf.extend(pcm);
        while self.buf.len() > LEAD_IN_BYTES {
            self.buf.pop_front();
        }
    }

    pub(crate) fn take(&mut self) -> Vec<u8> {
        self.buf.drain(..).collect()
    }

    pub(crate) fn clear(&mut self) {
        self.buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn down_averages_four_samples() {
        assert_eq!(down(&[100, 200, 300, 400, -4, -4, -4, -4]), vec![250, -4]);
    }

    #[test]
    fn up_interpolates_across_chunks() {
        let mut up = Up::default();
        assert_eq!(up.push(&[100]), vec![100, 100, 100, 100]);
        assert_eq!(up.push(&[300]), vec![200, 200, 300, 300]);
        up.reset();
        assert_eq!(up.push(&[-50]), vec![-50, -50, -50, -50]);
    }

    #[test]
    fn levels() {
        assert!((dbfs(&[0; 480]) + 120.0).abs() < f64::EPSILON);
        let full: Vec<i16> = (0..480)
            .map(|i| if i % 2 == 0 { 32767 } else { -32767 })
            .collect();
        assert!(dbfs(&full) > -0.1);
        let quiet = vec![100i16; 480];
        assert!((dbfs(&quiet) - (-50.3)).abs() < 0.1);
    }

    #[test]
    fn lead_in_keeps_two_and_a_half_seconds() {
        let mut l = LeadIn::default();
        l.push(&vec![1u8; 100_000]);
        l.push(&vec![2u8; 100_000]);
        let got = l.take();
        assert_eq!(got.len(), LEAD_IN_BYTES);
        assert_eq!(got.first(), Some(&1));
        assert_eq!(got.last(), Some(&2));
    }
}
