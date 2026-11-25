//! The bridge registry (DESIGN 8 "Bridge link", EXT req. 2-5): single-use launch tokens issued to
//! the hub's own OMP children, the reconnect key of each accepted connection, and what the hub knows
//! about each session's bridge. Roles are assigned here, never claimed by the bridge.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::Serialize;

use super::Role;
use crate::sys::now_ms;

/// A launch token is good for one hello within this time of the launch.
const TOKEN_LIFE_MS: i64 = 10 * 60 * 1000;
/// Tokens kept at most (a hub starts few sessions).
const TOKENS_MAX: usize = 64;
/// Tool answers kept per hub for repeated call IDs (EXT req. 18).
const ANSWERS_MAX: usize = 256;
/// Session processes kept at most, oldest dropped first (a hub starts few sessions).
const PIDS_MAX: usize = 256;
/// Bridge links kept at most. Every session start (a lead restart, each lane) gets a new handle, so
/// links of ended sessions are dropped, the longest disconnected first, beyond this many.
const LINKS_MAX: usize = 64;

/// What the hub shows about one session's bridge.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub(crate) struct LinkView {
    pub(crate) connected: bool,
    pub(crate) session: Option<String>,
    pub(crate) since_ms: Option<i64>,
    pub(crate) last_seq: u64,
    pub(crate) dropped: u64,
}

struct Issued {
    handle: String,
    role: Role,
    at_ms: i64,
    life_ms: i64,
}

struct Link {
    key: String,
    role: Role,
    view: LinkView,
}

#[derive(Default)]
struct Inner {
    tokens: HashMap<String, Issued>,
    links: HashMap<String, Link>,
    answers: VecDeque<(String, String)>,
    /// The control port of each terminal-runtime session (DESIGN 4.3).
    ports: HashMap<String, Arc<super::tern::Port>>,
    /// The OMP process of each session: the hub's own child, or the peer of its accepted bridge.
    pids: VecDeque<(u32, String, Role)>,
}

/// Launch tokens and accepted bridge connections of this hub run.
#[derive(Default)]
pub(crate) struct Links(Mutex<Inner>);

impl std::fmt::Debug for Links {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Links")
    }
}

impl Links {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records the token handed to a session's OMP child before it is spawned.
    pub(crate) fn issue(&self, token: &str, handle: &str, role: Role) {
        self.issue_for(token, handle, role, TOKEN_LIFE_MS);
    }

    /// A token good for `life_ms`: a terminal block's token is visible in the process list, so it
    /// lives 60 s (DESIGN 4.3).
    pub(crate) fn issue_for(&self, token: &str, handle: &str, role: Role, life_ms: i64) {
        let now = now_ms();
        let mut inner = self.lock();
        inner.tokens.retain(|_, t| now - t.at_ms < t.life_ms);
        if inner.tokens.len() >= TOKENS_MAX {
            return;
        }
        inner.tokens.insert(
            token.to_string(),
            Issued {
                handle: handle.to_string(),
                role,
                at_ms: now,
                life_ms,
            },
        );
    }

    /// Uses a token: the session it was issued for, once (EXT req. 5).
    pub(crate) fn redeem(&self, token: &str) -> Option<(String, Role)> {
        let issued = self.lock().tokens.remove(token)?;
        (now_ms() - issued.at_ms < issued.life_ms).then_some((issued.handle, issued.role))
    }

    /// Registers the control port of a terminal session under its handle.
    pub(crate) fn add_port(&self, handle: &str, port: Arc<super::tern::Port>) {
        self.lock().ports.insert(handle.to_string(), port);
    }

    pub(crate) fn port(&self, handle: &str) -> Option<Arc<super::tern::Port>> {
        self.lock().ports.get(handle).cloned()
    }

    pub(crate) fn remove_port(&self, handle: &str) {
        self.lock().ports.remove(handle);
    }

    /// The session a reconnect key belongs to.
    pub(crate) fn by_key(&self, key: &str) -> Option<(String, Role)> {
        self.lock()
            .links
            .iter()
            .find(|(_, l)| !key.is_empty() && l.key == key)
            .map(|(h, l)| (h.clone(), l.role))
    }

    /// An accepted connection; replaces an earlier one of the same session.
    pub(crate) fn connect(&self, handle: &str, role: Role, key: &str, session: &str) {
        let mut inner = self.lock();
        inner.links.insert(
            handle.to_string(),
            Link {
                key: key.to_string(),
                role,
                view: LinkView {
                    connected: true,
                    session: Some(session.to_string()).filter(|s| !s.is_empty()),
                    since_ms: Some(now_ms()),
                    last_seq: 0,
                    dropped: 0,
                },
            },
        );
        while inner.links.len() > LINKS_MAX {
            let Some(oldest) = inner
                .links
                .iter()
                .filter(|(_, l)| !l.view.connected)
                .min_by_key(|(_, l)| l.view.since_ms)
                .map(|(h, _)| h.clone())
            else {
                break;
            };
            inner.links.remove(&oldest);
        }
    }

    /// How many links are kept (the hub's `stats`).
    pub(crate) fn kept(&self) -> usize {
        self.lock().links.len()
    }

    /// The connection with `key` closed; a newer connection of the session is left alone.
    pub(crate) fn disconnect(&self, handle: &str, key: &str) {
        if let Some(link) = self.lock().links.get_mut(handle)
            && link.key == key
        {
            link.view.connected = false;
            link.view.since_ms = Some(now_ms());
        }
    }

    /// Notes an event: its number, a new session ID, events the bridge had to drop.
    pub(crate) fn observe(&self, handle: &str, seq: u64, session: Option<&str>, dropped: u64) {
        if let Some(link) = self.lock().links.get_mut(handle) {
            link.view.last_seq = link.view.last_seq.max(seq);
            link.view.dropped += dropped;
            if let Some(s) = session.filter(|s| !s.is_empty()) {
                link.view.session = Some(s.to_string());
            }
        }
    }

    pub(crate) fn view(&self, handle: &str) -> LinkView {
        self.lock()
            .links
            .get(handle)
            .map_or_default(|l| l.view.clone())
    }

    /// The answer already given to a tool call ID, so a retry does nothing twice (EXT req. 18).
    pub(crate) fn answered(&self, call: &str) -> Option<String> {
        self.lock()
            .answers
            .iter()
            .find(|(c, _)| c == call)
            .map(|(_, a)| a.clone())
    }

    pub(crate) fn remember(&self, call: &str, answer: &str) {
        let mut inner = self.lock();
        if inner.answers.len() >= ANSWERS_MAX {
            inner.answers.pop_front();
        }
        inner
            .answers
            .push_back((call.to_string(), answer.to_string()));
    }

    /// Records the OMP process of a session, so a control connection from it or from one of its
    /// descendants gets that session's role (DESIGN 8).
    pub(crate) fn adopt(&self, pid: u32, handle: &str, role: Role) {
        let mut inner = self.lock();
        inner.pids.retain(|(p, _, _)| *p != pid);
        if inner.pids.len() >= PIDS_MAX {
            inner.pids.pop_front();
        }
        inner.pids.push_back((pid, handle.to_string(), role));
    }

    /// The session whose OMP process comes first in `chain` (a process, then its ancestors).
    pub(crate) fn place(&self, chain: &[u32]) -> Option<(String, Role)> {
        let inner = self.lock();
        chain.iter().find_map(|pid| {
            inner
                .pids
                .iter()
                .find(|(p, _, _)| p == pid)
                .map(|(_, handle, role)| (handle.clone(), *role))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_single_use_and_keys_find_their_session() {
        let links = Links::default();
        links.issue("t1", "h1", Role::Lead);
        assert_eq!(links.redeem("t1"), Some(("h1".into(), Role::Lead)));
        assert_eq!(links.redeem("t1"), None);
        assert_eq!(links.redeem("nope"), None);
        links.connect("h1", Role::Lead, "k1", "s1");
        assert_eq!(links.by_key("k1"), Some(("h1".into(), Role::Lead)));
        assert_eq!(links.by_key(""), None);
        links.observe("h1", 7, Some("s2"), 3);
        let v = links.view("h1");
        assert!(v.connected && v.last_seq == 7 && v.dropped == 3);
        assert_eq!(v.session.as_deref(), Some("s2"));
        links.disconnect("h1", "old");
        assert!(links.view("h1").connected);
        links.disconnect("h1", "k1");
        assert!(!links.view("h1").connected);
        links.remember("c1", "done");
        assert_eq!(links.answered("c1").as_deref(), Some("done"));
        links.adopt(40, "h1", Role::Lead);
        links.adopt(50, "h2", Role::Lane);
        assert_eq!(
            links.place(&[60, 50, 40, 1]),
            Some(("h2".into(), Role::Lane))
        );
        assert_eq!(links.place(&[41, 40]), Some(("h1".into(), Role::Lead)));
        assert_eq!(links.place(&[41, 1]), None);
    }

    /// Every session start brings a new handle; ended sessions' links do not pile up, and a
    /// connected link is never dropped.
    #[test]
    fn links_of_ended_sessions_are_bounded() {
        let links = Links::default();
        links.connect("lead", Role::Lead, "k-lead", "s-lead");
        for n in 0..1000 {
            let (handle, key) = (format!("h{n}"), format!("k{n}"));
            links.connect(&handle, Role::Lane, &key, "s");
            links.disconnect(&handle, &key);
        }
        assert!(links.kept() <= LINKS_MAX, "{} links kept", links.kept());
        assert!(links.view("lead").connected);
        assert_eq!(links.by_key("k-lead"), Some(("lead".into(), Role::Lead)));
    }
}
