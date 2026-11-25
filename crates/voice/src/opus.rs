//! Opus at 48 kHz stereo, 20 ms frames: Discord audio both ways and the WebRTC transport's media.

use audiopus::coder::{Decoder, Encoder};
use audiopus::packet::Packet;
use audiopus::{Application, Bitrate, Channels, MutSignals, SampleRate};

use crate::audio::FRAME_STEREO;

/// Largest decoded packet: 120 ms of stereo.
const DECODED_MAX: usize = 5760 * 2;
/// Largest encoded packet written.
const ENCODED_MAX: usize = 1500;

/// A decoder for one incoming stream.
pub(crate) struct Decode(Decoder);

impl Decode {
    pub(crate) fn new() -> Result<Self, String> {
        Decoder::new(SampleRate::Hz48000, Channels::Stereo)
            .map(Self)
            .map_err(|e| format!("Opus decoder: {e}"))
    }

    /// Interleaved stereo samples, or `None` for an undecodable packet (skipped, VOICE req. 56).
    pub(crate) fn decode(&mut self, packet: &[u8]) -> Option<Vec<i16>> {
        let packet = Packet::try_from(packet).ok()?;
        let mut out = vec![0i16; DECODED_MAX];
        let signals = MutSignals::try_from(&mut out[..]).ok()?;
        let n = self.0.decode(Some(packet), signals, false).ok()?;
        out.truncate(n * 2);
        Some(out)
    }
}

/// An encoder for one outgoing stream.
pub(crate) struct Encode(Encoder);

impl Encode {
    pub(crate) fn new() -> Result<Self, String> {
        let mut e = Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Voip)
            .map_err(|e| format!("Opus encoder: {e}"))?;
        e.set_bitrate(Bitrate::BitsPerSecond(64_000))
            .map_err(|e| format!("Opus encoder: {e}"))?;
        Ok(Self(e))
    }

    /// One 20 ms frame of interleaved stereo.
    pub(crate) fn encode(&self, frame: &[i16]) -> Option<Vec<u8>> {
        if frame.len() != FRAME_STEREO {
            return None;
        }
        let mut out = vec![0u8; ENCODED_MAX];
        let n = self.0.encode(frame, &mut out).ok()?;
        out.truncate(n);
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_keeps_level() {
        let enc = Encode::new().unwrap();
        let mut dec = Decode::new().unwrap();
        let mut level = -120.0;
        for k in 0..10 {
            let frame: Vec<i16> = (0..FRAME_STEREO)
                .map(|i| {
                    let t = u32::try_from(k * 960 + i / 2).unwrap();
                    let v = (f64::from(t) * 0.05).sin() * 8000.0;
                    #[allow(clippy::cast_possible_truncation)]
                    let s = v as i16;
                    s
                })
                .collect();
            let packet = enc.encode(&frame).unwrap();
            let pcm = dec.decode(&packet).unwrap();
            assert_eq!(pcm.len(), FRAME_STEREO);
            level = crate::audio::dbfs(&pcm);
        }
        assert!(level > -30.0, "level {level}");
        assert!(dec.decode(&[0xff; 3]).is_none() || dec.decode(&[]).is_none());
    }
}
