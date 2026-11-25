//! Optional extra lines on lead-bound requests, for every relay (DISC-IN-17, DISC-IN-18; DESIGN
//! 12.2 A15), both off by default under `[context]`:
//!
//! - **Channel facts** (DISC-IN req. 53-54): the room's ID, name, topic and the owner's note, plus the
//!   parent's for a thread or topic, or a DM marker. Each lookup is bounded at 5 s and cached for
//!   60 s (500 rooms at most); a failure becomes an error field and delivery proceeds.
//! - **Earlier conversation** (DISC-IN req. 55-58, DESIGN 12.2 C20): owner messages and lead replies
//!   are stored per space, masked and cut; an owner message that refers back in time gets up to four
//!   earlier rows of its space ranked by shared rare words.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::{Facts, Inbound, Relay};
use crate::config::relays::ContextSettings;
use crate::store::relay::RecallRow;

const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const CACHE_FOR: Duration = Duration::from_secs(60);
const CACHE_MAX: usize = 500;
/// Stored text is cut to this many characters (DISC-IN req. 56).
const STORED_MAX: usize = 2000;
/// Rows ranked per lookup (DISC-IN req. 57).
pub(crate) const RANKED_ROWS: u32 = 2000;
const SHOWN_ROWS: usize = 4;
const ROW_TEXT_MAX: usize = 700;
const LINE_MAX: usize = 3 * 1024;

/// Phrases that refer back in time (DISC-IN req. 57), matched lower-cased.
const BACK: [&str; 24] = [
    "last time",
    "you said",
    "we discussed",
    "we talked",
    "earlier",
    "yesterday",
    "previously",
    "remember when",
    "as i said",
    "the other day",
    "before,",
    "지난번",
    "저번",
    "어제",
    "아까",
    "전에",
    "말했",
    "前回",
    "この前",
    "昨日",
    "さっき",
    "以前",
    "言った",
    "先日",
];

/// Words that carry no topic.
const COMMON: [&str; 40] = [
    "the",
    "a",
    "an",
    "and",
    "or",
    "but",
    "to",
    "of",
    "in",
    "on",
    "at",
    "for",
    "with",
    "is",
    "are",
    "was",
    "were",
    "be",
    "it",
    "this",
    "that",
    "i",
    "you",
    "we",
    "me",
    "my",
    "your",
    "our",
    "do",
    "did",
    "can",
    "what",
    "about",
    "said",
    "last",
    "time",
    "earlier",
    "yesterday",
    "there",
    "they",
];

/// Korean particles stripped from the end of a word.
const PARTICLES: [&str; 18] = [
    "에서", "으로", "에게", "한테", "께서", "까지", "부터", "은", "는", "이", "가", "을", "를",
    "에", "의", "와", "과", "도",
];

/// One cached lookup: the room, when it was made, and its result.
type Cached = (String, Instant, Result<Facts, String>);

/// The facts cache of one relay.
#[derive(Default)]
pub(crate) struct Context {
    cache: Mutex<VecDeque<Cached>>,
}

impl Context {
    async fn lookup<R: Relay>(&self, relay: &R, room: &str) -> Result<Facts, String> {
        {
            let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some((_, _, facts)) = cache
                .iter()
                .find(|(r, at, _)| r == room && at.elapsed() < CACHE_FOR)
            {
                return facts.clone();
            }
        }
        let facts = tokio::time::timeout(LOOKUP_TIMEOUT, relay.facts(room))
            .await
            .unwrap_or_else(|_| Err("lookup timed out".into()));
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        cache.retain(|(r, at, _)| r != room && at.elapsed() < CACHE_FOR);
        if cache.len() >= CACHE_MAX {
            cache.pop_front();
        }
        cache.push_back((room.to_string(), Instant::now(), facts.clone()));
        facts
    }

    /// The channel facts line (DISC-IN req. 53).
    pub(crate) async fn facts_line<R: Relay>(
        &self,
        relay: &R,
        settings: &ContextSettings,
        inbound: &Inbound,
    ) -> String {
        let mut line = "[channel]".to_string();
        self.describe(relay, settings, &inbound.room, &mut line)
            .await;
        if let Some(parent) = &inbound.parent {
            line.push_str("; in");
            self.describe(relay, settings, parent, &mut line).await;
        }
        line
    }

    async fn describe<R: Relay>(
        &self,
        relay: &R,
        settings: &ContextSettings,
        room: &str,
        line: &mut String,
    ) {
        match self.lookup(relay, room).await {
            Ok(f) if f.dm => {
                let _ = write!(line, " DM {room}");
            }
            Ok(f) => {
                let _ = write!(line, " id {room}");
                if let Some(name) = f.name {
                    let _ = write!(line, ", name {}", one_line(&name, 100));
                }
                if let Some(topic) = f.topic {
                    let _ = write!(line, ", topic {}", one_line(&topic, 300));
                }
            }
            Err(e) => {
                let _ = write!(line, " id {room}, error {}", one_line(&e, 200));
            }
        }
        if let Some(note) = settings.notes.get(&format!("{}:{room}", R::NAME)) {
            let _ = write!(line, ", note {note}");
        }
    }
}

fn one_line(text: &str, max: usize) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
        .collect()
}

/// The space a message belongs to: its listed parent, else its room (DISC-IN req. 55).
pub(crate) fn space(inbound: &Inbound) -> &str {
    inbound.parent.as_deref().unwrap_or(&inbound.room)
}

/// Text as stored: masked first, then cut (DISC-IN req. 56).
pub(crate) fn stored(text: &str) -> String {
    crate::sys::secrets::mask_all(text)
        .chars()
        .take(STORED_MAX)
        .collect()
}

/// Whether an owner message refers back in time (DISC-IN req. 57).
pub(crate) fn refers_back(text: &str) -> bool {
    let lower = text.to_lowercase();
    BACK.iter().any(|p| lower.contains(p))
}

fn cjk(c: char) -> bool {
    matches!(c, '\u{3040}'..='\u{30FF}' | '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}')
}

/// Topic words: lower-cased words without common ones, Korean particles stripped, Han and kana
/// runs split into character pairs.
pub(crate) fn tokens(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for word in text
        .to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || cjk(c)))
        .filter(|w| !w.is_empty())
    {
        let chars: Vec<char> = word.chars().collect();
        if chars.iter().any(|c| cjk(*c)) {
            if chars.len() == 1 {
                out.insert(word.to_string());
            }
            for pair in chars.windows(2) {
                out.insert(pair.iter().collect());
            }
            continue;
        }
        let mut w = word;
        if let Some(stripped) = PARTICLES
            .iter()
            .find_map(|p| w.strip_suffix(p).filter(|s| s.chars().count() >= 1))
        {
            w = stripped;
        }
        if w.chars().count() >= 2 && !COMMON.contains(&w) {
            out.insert(w.to_string());
        }
    }
    out
}

/// The rows to show for `query`: rows sharing rare words, best first, ties to the newer; the
/// message itself excluded; at most four, then in time order (DISC-IN req. 57-58). `rows` are newest
/// first.
pub(crate) fn rank<'a>(query: &str, exclude: &str, rows: &'a [RecallRow]) -> Vec<&'a RecallRow> {
    let wanted = tokens(query);
    if wanted.is_empty() {
        return Vec::new();
    }
    let row_tokens: Vec<HashSet<String>> = rows.iter().map(|r| tokens(&r.text)).collect();
    let mut df: HashMap<&str, usize> = HashMap::new();
    for set in &row_tokens {
        for t in set {
            if wanted.contains(t) {
                *df.entry(t.as_str()).or_default() += 1;
            }
        }
    }
    let mut scored: Vec<(f64, usize)> = row_tokens
        .iter()
        .enumerate()
        .filter(|(i, _)| rows.get(*i).is_some_and(|r| r.message != exclude))
        .filter_map(|(i, set)| {
            let score: f64 = set
                .iter()
                .filter_map(|t| df.get(t.as_str()))
                .map(|n| 1.0 / f64::from(u32::try_from(*n).unwrap_or(u32::MAX)))
                .sum();
            (score > 0.0).then_some((score, i))
        })
        .collect();
    // Newest rows come first in `rows`, so the lower index wins a tie.
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut picked: Vec<&RecallRow> = scored
        .into_iter()
        .take(SHOWN_ROWS)
        .filter_map(|(_, i)| rows.get(i))
        .collect();
    picked.sort_by_key(|r| r.at_ms);
    picked
}

/// The lookup line (DISC-IN req. 58): at most 3 KiB; the lowest-ranked rows go first when it is
/// longer. `picked` is in time order.
pub(crate) fn recall_line(picked: &[&RecallRow], query: &str) -> Option<String> {
    let order = rank_order(picked, query);
    let mut keep: Vec<usize> = (0..picked.len()).collect();
    loop {
        let mut line = "[earlier]".to_string();
        for i in &keep {
            let Some(r) = picked.get(*i) else { continue };
            let when = jiff::Timestamp::from_millisecond(r.at_ms)
                .map_or_default(|t| t.strftime("%Y-%m-%dT%H:%MZ").to_string());
            let text: String = r
                .text
                .replace("```", "")
                .replace("~~~", "")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(ROW_TEXT_MAX)
                .collect();
            let _ = write!(line, " | {when} {} in {}: {text}", r.who, r.conversation);
        }
        if keep.is_empty() {
            return None;
        }
        if line.len() <= LINE_MAX {
            return Some(line);
        }
        // Drop the lowest-ranked row still kept.
        let worst = order.iter().rev().find(|i| keep.contains(i)).copied();
        keep.retain(|i| Some(*i) != worst);
    }
}

/// Indices of `picked` from best to worst by the same score as [`rank`].
fn rank_order(picked: &[&RecallRow], query: &str) -> Vec<usize> {
    let wanted = tokens(query);
    let mut scored: Vec<(usize, usize)> = picked
        .iter()
        .enumerate()
        .map(|(i, r)| (tokens(&r.text).intersection(&wanted).count(), i))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    scored.into_iter().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(at_ms: i64, who: &str, message: &str, text: &str) -> RecallRow {
        RecallRow {
            at_ms,
            who: who.into(),
            conversation: "c1".into(),
            message: message.into(),
            text: text.into(),
        }
    }

    #[test]
    fn phrases_tokens_and_masking() {
        assert!(refers_back("What did you say LAST TIME about the deploy?"));
        assert!(refers_back("어제 말한 배포는?") && refers_back("前回の件"));
        assert!(!refers_back("deploy the build"));
        let t = tokens("The deploy of 서버를 前回の件");
        assert!(t.contains("deploy") && t.contains("서버") && t.contains("前回"));
        assert!(!t.contains("the"));
        let token = format!("xoxb-{}", "1234567890abcdef");
        let s = stored(&format!("password = hunter2 and the key {token}"));
        assert!(!s.contains("hunter2") && !s.contains(&token), "{s}");
        assert_eq!(stored(&"x".repeat(3000)).chars().count(), 2000);
    }

    #[test]
    fn ranking_prefers_rare_shared_words_then_newer() {
        let rows = vec![
            row(5, "owner", "m5", "the staging deploy failed again"),
            row(4, "lead", "m4", "staging is fine"),
            row(3, "owner", "m3", "lunch plans"),
            row(2, "lead", "m2", "the canary deploy rolled back"),
            row(1, "owner", "m1", "you said the canary deploy rolled back"),
        ];
        let picked = rank("what did you say about the canary last time", "m1", &rows);
        let ids: Vec<_> = picked.iter().map(|r| r.message.as_str()).collect();
        assert_eq!(ids, vec!["m2"], "the message itself is excluded");
        let picked = rank("staging deploy earlier", "x", &rows);
        let ids: Vec<_> = picked.iter().map(|r| r.message.as_str()).collect();
        assert_eq!(ids, vec!["m1", "m2", "m4", "m5"]);
        let line = recall_line(&picked, "staging deploy").unwrap();
        assert!(
            line.starts_with("[earlier] | 1970-01-01T00:00Z owner in c1:"),
            "{line}"
        );
        let long: Vec<RecallRow> = (0..4)
            .map(|i| row(i, "lead", "m", &format!("deploy {}", "w ".repeat(400))))
            .collect();
        let refs: Vec<&RecallRow> = long.iter().collect();
        let line = recall_line(&refs, "deploy").unwrap();
        assert!(line.len() <= LINE_MAX, "{}", line.len());
    }
}
