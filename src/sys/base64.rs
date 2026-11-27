//! Standard base64 with padding (RFC 4648 section 4): the one encoder and the one strict decoder,
//! used for OMP's chunks, Discord profile images, worker file transfers and SSH key fingerprints.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub(crate) fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk.first().copied().unwrap_or(0),
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                let index = usize::try_from((n >> shift) & 63).unwrap_or(0);
                out.push(char::from(ALPHABET.get(index).copied().unwrap_or(b'A')));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Strict standard base64 with padding: every input has at most one decoding.
pub(crate) fn decode(text: &str) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let (quads, rest) = text.as_bytes().as_chunks::<4>();
    if !rest.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(quads.len() * 3);
    let last = quads.len().checked_sub(1);
    for (i, quad) in quads.iter().enumerate() {
        let pad = quad.iter().rev().take_while(|c| **c == b'=').count();
        if pad > 2 || (pad > 0 && Some(i) != last) {
            return None;
        }
        let mut n: u32 = 0;
        for (j, c) in quad.iter().enumerate() {
            let v = if j >= 4 - pad { 0 } else { value(*c)? };
            n = (n << 6) | v;
        }
        let [_, a, b, c] = n.to_be_bytes();
        out.push(a);
        // Unused low bits must be zero, so every input has one decoding.
        match pad {
            0 => {
                out.push(b);
                out.push(c);
            }
            1 if c == 0 => out.push(b),
            2 if b == 0 && c == 0 => {}
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_matches_the_standard() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn decode_is_strict() {
        for sample in [&b""[..], b"f", b"fo", b"foo", b"foob", "한글".as_bytes()] {
            assert_eq!(decode(&encode(sample)).unwrap(), sample);
        }
        assert_eq!(decode("Zm9v").unwrap(), b"foo");
        for bad in ["Zm9", "Zm=v", "Zm9v=", "Zh==", "Zm9-", "Zg==Zg=="] {
            assert!(decode(bad).is_none(), "{bad}");
        }
    }
}
