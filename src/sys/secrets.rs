//! Secret-shape masking: the one place that recognises token-shaped strings (DESIGN 10.1). Built-in
//! shapes only; owner patterns arrive with the backup job.

use std::borrow::Cow;

/// Replacement text for a masked secret.
pub(crate) const MASK: &str = "[masked]";

type Class = fn(u8) -> bool;

fn alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}
fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}
fn url_safe(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}
fn slack(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-'
}
fn upper_digit(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit()
}
fn bearer(b: u8) -> bool {
    url_safe(b) || matches!(b, b'.' | b'~' | b'+' | b'/' | b'=')
}
fn jwt(b: u8) -> bool {
    url_safe(b) || b == b'.'
}

/// Prefix, the characters allowed after it, and the minimum number of them.
const PREFIXED: [(&[u8], Class, usize, bool); 18] = [
    (b"github_pat_", word, 30, true),
    (b"ghp_", alnum, 30, true),
    (b"gho_", alnum, 30, true),
    (b"ghu_", alnum, 30, true),
    (b"ghs_", alnum, 30, true),
    (b"ghr_", alnum, 30, true),
    (b"glpat-", url_safe, 20, true),
    (b"xoxb-", slack, 10, true),
    (b"xoxp-", slack, 10, true),
    (b"xoxa-", slack, 10, true),
    (b"xoxs-", slack, 10, true),
    (b"xoxe-", slack, 10, true),
    (b"xapp-", slack, 10, true),
    (b"sk-", url_safe, 20, true),
    (b"AKIA", upper_digit, 16, true),
    (b"AIza", url_safe, 30, true),
    (b"eyJ", jwt, 30, true),
    // The prefix itself stays visible; only the credential after it is masked.
    (b"Bearer ", bearer, 20, false),
];

const PEM_BEGIN: &[u8] = b"-----BEGIN ";
const PEM_END: &[u8] = b"-----END ";
const PEM_TAIL: &[u8] = b"PRIVATE KEY-----";

/// Replaces every secret-shaped span with [`MASK`]; borrows when nothing matched.
pub(crate) fn mask(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let Some(first) = next_secret(bytes, 0) else {
        return Cow::Borrowed(text);
    };
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut span = Some(first);
    while let Some((start, end)) = span {
        out.push_str(text.get(copied..start).unwrap_or_default());
        out.push_str(MASK);
        copied = end;
        span = next_secret(bytes, end);
    }
    out.push_str(text.get(copied..).unwrap_or_default());
    Cow::Owned(out)
}

fn boundary(bytes: &[u8], at: usize) -> bool {
    at == 0 || bytes.get(at - 1).is_none_or(|b| !url_safe(*b))
}

fn run_len(bytes: &[u8], from: usize, class: Class) -> usize {
    bytes
        .get(from..)
        .map_or(0, |rest| rest.iter().take_while(|b| class(**b)).count())
}

fn next_secret(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    (from..bytes.len()).find_map(|at| match_at(bytes, at))
}

fn match_at(bytes: &[u8], at: usize) -> Option<(usize, usize)> {
    let rest = bytes.get(at..)?;
    if rest.starts_with(PEM_BEGIN) {
        return pem(bytes, at);
    }
    if !boundary(bytes, at) {
        return None;
    }
    for (prefix, class, min, mask_prefix) in PREFIXED {
        if rest.starts_with(prefix) {
            let body = at + prefix.len();
            let n = run_len(bytes, body, class);
            if n >= min {
                return Some((if mask_prefix { at } else { body }, body + n));
            }
        }
    }
    discord(bytes, at).or_else(|| telegram(bytes, at))
}

fn pem(bytes: &[u8], at: usize) -> Option<(usize, usize)> {
    let header_end = at + find(bytes.get(at..)?, b"\n").unwrap_or(bytes.len() - at);
    find(bytes.get(at..header_end)?, PEM_TAIL)?;
    let end_marker = at + find(bytes.get(at..)?, PEM_END)?;
    let tail = end_marker + find(bytes.get(end_marker..)?, PEM_TAIL)? + PEM_TAIL.len();
    Some((at, tail))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Discord bot token: 24-28, a dot, 6-7, a dot, 27-40 URL-safe characters.
fn discord(bytes: &[u8], at: usize) -> Option<(usize, usize)> {
    let a = run_len(bytes, at, url_safe);
    if !(24..=28).contains(&a) || bytes.get(at + a) != Some(&b'.') {
        return None;
    }
    let b_start = at + a + 1;
    let b = run_len(bytes, b_start, url_safe);
    if !(6..=7).contains(&b) || bytes.get(b_start + b) != Some(&b'.') {
        return None;
    }
    let c_start = b_start + b + 1;
    let c = run_len(bytes, c_start, url_safe);
    (27..=40).contains(&c).then_some((at, c_start + c))
}

/// Telegram bot token: 8-10 digits, a colon, 35 URL-safe characters.
fn telegram(bytes: &[u8], at: usize) -> Option<(usize, usize)> {
    let digits = run_len(bytes, at, |b| b.is_ascii_digit());
    if !(8..=10).contains(&digits) || bytes.get(at + digits) != Some(&b':') {
        return None;
    }
    let start = at + digits + 1;
    let n = run_len(bytes, start, url_safe);
    (n >= 35).then_some((at, start + n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_known_shapes() {
        let gh = format!("token ghp_{} end", "a1".repeat(18));
        assert_eq!(mask(&gh), "token [masked] end");
        let pat = format!("github_pat_{}", "A_b9".repeat(10));
        assert_eq!(mask(&pat), MASK);
        assert_eq!(mask(&format!("xoxb-{}", "1234-abcd".repeat(3))), MASK);
        assert_eq!(
            mask(&format!("key=sk-ant-{}", "x".repeat(30))),
            "key=[masked]"
        );
        assert_eq!(mask(&format!("AKIA{}", "Q7".repeat(8))), MASK);
        assert_eq!(
            mask(&format!("Authorization: Bearer {}", "t0k".repeat(10))),
            "Authorization: Bearer [masked]"
        );
        let discord = format!("{}.{}.{}", "M".repeat(26), "abcdef", "z".repeat(30));
        assert_eq!(mask(&discord), MASK);
        let telegram = format!("123456789:{}", "A".repeat(35));
        assert_eq!(mask(&telegram), MASK);
        let kind = "RSA PRIVATE KEY-----";
        let pem = format!(
            "a\n-----BEGIN {kind}\n{}\n-----END {kind}\nb",
            "QUJD".repeat(16)
        );
        assert_eq!(mask(&pem), "a\n[masked]\nb");
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        for text in [
            "plain words and a path /home/o/x",
            "ghp_short",
            "task-123 is done",
            "-----BEGIN PUBLIC KEY-----",
            "risk-free sk-abc",
            "",
        ] {
            assert!(matches!(mask(text), Cow::Borrowed(_)), "{text}");
        }
        assert_eq!(mask(&format!("x ghs_{}", "9".repeat(36))), "x [masked]");
        // A prefix inside a longer word is not a token start.
        assert!(matches!(
            mask(&format!("mask-{}", "x".repeat(30))),
            Cow::Borrowed(_)
        ));
    }
}
