//! Discord voice packets: RTP framing, transport encryption in the two modes Discord offers
//! (`aead_aes256_gcm_rtpsize` through ring, preferred; `aead_xchacha20_poly1305_rtpsize`, which every
//! voice server supports), and UDP IP discovery.
//!
//! In the "rtpsize" modes the unencrypted header is the fixed 12 bytes, the CSRCs and, when the
//! extension bit is set, the 4-byte extension preamble; the extension body is encrypted with the
//! payload. A 32-bit counter, appended to the packet, forms the nonce.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};

pub(crate) const MODE_AES: &str = "aead_aes256_gcm_rtpsize";
pub(crate) const MODE_XCHACHA: &str = "aead_xchacha20_poly1305_rtpsize";
const VERSION_FLAGS: u8 = 0x80;
const PAYLOAD_TYPE: u8 = 0x78;
/// Bytes of an IP discovery packet.
pub(crate) const DISCOVERY_LEN: usize = 74;

/// The transport cipher of one voice session.
pub(crate) enum Cipher {
    Aes(Box<LessSafeKey>),
    XChaCha(Box<XChaCha20Poly1305>),
}

/// Picks AES-GCM when offered, else XChaCha20-Poly1305.
pub(crate) fn choose(modes: &[String]) -> Option<&'static str> {
    if modes.iter().any(|m| m == MODE_AES) {
        Some(MODE_AES)
    } else if modes.iter().any(|m| m == MODE_XCHACHA) {
        Some(MODE_XCHACHA)
    } else {
        None
    }
}

impl Cipher {
    pub(crate) fn new(mode: &str, key: &[u8]) -> Option<Self> {
        match mode {
            MODE_AES => UnboundKey::new(&AES_256_GCM, key)
                .ok()
                .map(|k| Self::Aes(Box::new(LessSafeKey::new(k)))),
            MODE_XCHACHA => XChaCha20Poly1305::new_from_slice(key)
                .ok()
                .map(|c| Self::XChaCha(Box::new(c))),
            _ => None,
        }
    }

    fn seal(&self, aad: &[u8], plain: &[u8], counter: u32) -> Option<Vec<u8>> {
        match self {
            Self::Aes(key) => {
                let mut nonce = [0u8; 12];
                nonce[..4].copy_from_slice(&counter.to_be_bytes());
                let mut buf = plain.to_vec();
                key.seal_in_place_append_tag(
                    Nonce::assume_unique_for_key(nonce),
                    Aad::from(aad),
                    &mut buf,
                )
                .ok()?;
                Some(buf)
            }
            Self::XChaCha(c) => {
                let mut nonce = [0u8; 24];
                nonce[..4].copy_from_slice(&counter.to_be_bytes());
                c.encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad })
                    .ok()
            }
        }
    }

    fn open(&self, aad: &[u8], sealed: &[u8], counter: [u8; 4]) -> Option<Vec<u8>> {
        match self {
            Self::Aes(key) => {
                let mut nonce = [0u8; 12];
                nonce[..4].copy_from_slice(&counter);
                let mut buf = sealed.to_vec();
                let n = key
                    .open_in_place(
                        Nonce::assume_unique_for_key(nonce),
                        Aad::from(aad),
                        &mut buf,
                    )
                    .ok()?
                    .len();
                buf.truncate(n);
                Some(buf)
            }
            Self::XChaCha(c) => {
                let mut nonce = [0u8; 24];
                nonce[..4].copy_from_slice(&counter);
                c.decrypt(XNonce::from_slice(&nonce), Payload { msg: sealed, aad })
                    .ok()
            }
        }
    }
}

/// An outgoing packet: header, sealed Opus frame, counter.
pub(crate) fn seal(
    cipher: &Cipher,
    seq: u16,
    timestamp: u32,
    ssrc: u32,
    frame: &[u8],
    counter: u32,
) -> Option<Vec<u8>> {
    let mut header = [0u8; 12];
    header[0] = VERSION_FLAGS;
    header[1] = PAYLOAD_TYPE;
    header[2..4].copy_from_slice(&seq.to_be_bytes());
    header[4..8].copy_from_slice(&timestamp.to_be_bytes());
    header[8..12].copy_from_slice(&ssrc.to_be_bytes());
    let sealed = cipher.seal(&header, frame, counter)?;
    let mut packet = Vec::with_capacity(12 + sealed.len() + 4);
    packet.extend_from_slice(&header);
    packet.extend_from_slice(&sealed);
    packet.extend_from_slice(&counter.to_be_bytes());
    Some(packet)
}

/// A received audio packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Received {
    pub(crate) ssrc: u32,
    pub(crate) seq: u16,
    /// The (possibly end-to-end encrypted) Opus frame.
    pub(crate) frame: Vec<u8>,
}

/// Why a packet was not turned into audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dropped {
    /// RTCP or not RTP at all: ignored quietly.
    NotAudio,
    /// Transport decryption failed.
    Decrypt,
}

/// Parses and decrypts one incoming packet.
pub(crate) fn open(cipher: &Cipher, packet: &[u8]) -> Result<Received, Dropped> {
    let first = *packet.first().ok_or(Dropped::NotAudio)?;
    let second = *packet.get(1).ok_or(Dropped::NotAudio)?;
    if first >> 6 != 2 || (72..=76).contains(&(second & 0x7f)) || packet.len() < 12 + 4 {
        return Err(Dropped::NotAudio);
    }
    let csrc = usize::from(first & 0x0f);
    let extension = first & 0x10 != 0;
    let header_len = 12 + 4 * csrc + if extension { 4 } else { 0 };
    let body_end = packet.len().checked_sub(4).ok_or(Dropped::NotAudio)?;
    if header_len > body_end {
        return Err(Dropped::NotAudio);
    }
    let aad = packet.get(..header_len).ok_or(Dropped::NotAudio)?;
    let sealed = packet.get(header_len..body_end).ok_or(Dropped::NotAudio)?;
    let counter: [u8; 4] = packet
        .get(body_end..)
        .and_then(|c| c.try_into().ok())
        .ok_or(Dropped::NotAudio)?;
    let mut plain = cipher.open(aad, sealed, counter).ok_or(Dropped::Decrypt)?;
    if extension {
        let words = packet
            .get(header_len - 2..header_len)
            .and_then(|b| b.try_into().ok())
            .map_or(0, |b: [u8; 2]| usize::from(u16::from_be_bytes(b)));
        if words * 4 > plain.len() {
            return Err(Dropped::NotAudio);
        }
        plain.drain(..words * 4);
    }
    let seq = packet
        .get(2..4)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u16::from_be_bytes);
    let ssrc = packet
        .get(8..12)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u32::from_be_bytes);
    Ok(Received {
        ssrc,
        seq,
        frame: plain,
    })
}

/// The IP discovery request for `ssrc`.
pub(crate) fn discovery_request(ssrc: u32) -> [u8; DISCOVERY_LEN] {
    let mut p = [0u8; DISCOVERY_LEN];
    p[0..2].copy_from_slice(&1u16.to_be_bytes());
    p[2..4].copy_from_slice(&70u16.to_be_bytes());
    p[4..8].copy_from_slice(&ssrc.to_be_bytes());
    p
}

/// The address and port of an IP discovery response.
pub(crate) fn discovery_response(p: &[u8]) -> Option<(String, u16)> {
    if p.len() < DISCOVERY_LEN || p.get(0..2)? != [0, 2] {
        return None;
    }
    let addr = p.get(8..72)?;
    let end = addr.iter().position(|&b| b == 0)?;
    let ip = std::str::from_utf8(addr.get(..end)?).ok()?.to_string();
    let port = u16::from_be_bytes([*p.get(72)?, *p.get(73)?]);
    Some((ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_modes_round_trip() {
        for mode in [MODE_AES, MODE_XCHACHA] {
            let c = Cipher::new(mode, &[7u8; 32]).unwrap();
            let p = seal(&c, 9, 960, 42, b"opus-frame", 5).unwrap();
            let r = open(&c, &p).unwrap();
            assert_eq!(
                (r.ssrc, r.seq, r.frame.as_slice()),
                (42, 9, &b"opus-frame"[..])
            );
            let mut bad = p.clone();
            bad[14] ^= 1;
            assert_eq!(open(&c, &bad), Err(Dropped::Decrypt));
        }
        assert_eq!(
            choose(&[MODE_XCHACHA.into(), MODE_AES.into()]),
            Some(MODE_AES)
        );
        assert_eq!(choose(&["xsalsa20_poly1305".into()]), None);
    }

    #[test]
    fn rtcp_is_ignored_and_discovery_parses() {
        let c = Cipher::new(MODE_AES, &[1u8; 32]).unwrap();
        let mut rtcp = vec![0x80, 0xc9];
        rtcp.resize(40, 0);
        assert_eq!(open(&c, &rtcp), Err(Dropped::NotAudio));
        let mut resp = discovery_request(3);
        resp[0..2].copy_from_slice(&2u16.to_be_bytes());
        resp[8..17].copy_from_slice(b"127.0.0.1");
        resp[72..74].copy_from_slice(&5000u16.to_be_bytes());
        assert_eq!(discovery_response(&resp), Some(("127.0.0.1".into(), 5000)));
    }
}
