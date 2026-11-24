//! Transition memory and once-per-change emission (WS req. 4, OJ req. 3-6), shared by every job kind.
//! A tick loads its job's memory, asks [`Memory::transition`] for each condition, and commits the
//! collected writes only after its events are in the stream.

use std::collections::HashMap;

use crate::store::MemoryWrite;

/// The stored conditions of one job (live or dry), plus the changes a tick makes to them.
#[derive(Debug, Clone)]
pub(crate) struct Memory {
    job: String,
    dry: bool,
    entries: HashMap<(String, String), String>,
    writes: Vec<MemoryWrite>,
}

impl Memory {
    pub(crate) fn new(job: &str, dry: bool, rows: Vec<(String, String, String)>) -> Self {
        let entries = rows.into_iter().map(|(s, c, v)| ((s, c), v)).collect();
        Self {
            job: job.to_string(),
            dry,
            entries,
            writes: Vec::new(),
        }
    }

    pub(crate) fn get(&self, subject: &str, condition: &str) -> Option<&str> {
        self.entries
            .get(&(subject.to_string(), condition.to_string()))
            .map(String::as_str)
    }

    /// Stores `value`; returns `true` when it differs from what was stored (a transition, including the
    /// first value), `false` when it is the same.
    pub(crate) fn transition(&mut self, subject: &str, condition: &str, value: &str) -> bool {
        let key = (subject.to_string(), condition.to_string());
        if self.entries.get(&key).is_some_and(|v| v == value) {
            return false;
        }
        self.entries.insert(key, value.to_string());
        self.record(subject, condition, Some(value.to_string()));
        true
    }

    /// Every stored `(subject, value)` with this condition, sorted by subject.
    pub(crate) fn with_condition(&self, condition: &str) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .entries
            .iter()
            .filter(|((_, c), _)| c == condition)
            .map(|((s, _), v)| (s.clone(), v.clone()))
            .collect();
        out.sort();
        out
    }

    /// Forgets a condition; returns what was stored.
    pub(crate) fn clear(&mut self, subject: &str, condition: &str) -> Option<String> {
        let previous = self
            .entries
            .remove(&(subject.to_string(), condition.to_string()));
        if previous.is_some() {
            self.record(subject, condition, None);
        }
        previous
    }

    fn record(&mut self, subject: &str, condition: &str, value: Option<String>) {
        self.writes
            .retain(|w| !(w.subject == subject && w.condition == condition));
        self.writes.push(MemoryWrite {
            job: self.job.clone(),
            subject: subject.to_string(),
            condition: condition.to_string(),
            dry: self.dry,
            value,
        });
    }

    /// The changes to commit with the tick's state.
    pub(crate) fn into_writes(self) -> Vec<MemoryWrite> {
        self.writes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_only_on_change_and_collects_one_write_per_key() {
        let mut m = Memory::new("j", false, vec![("s".into(), "c".into(), "ok".into())]);
        assert!(!m.transition("s", "c", "ok"));
        assert!(m.transition("s", "c", "bad"));
        assert!(!m.transition("s", "c", "bad"));
        assert!(m.transition("s", "c", "worse"));
        assert!(m.transition("t", "c", "new"));
        assert_eq!(m.clear("t", "c").as_deref(), Some("new"));
        assert_eq!(m.clear("t", "c"), None);
        assert_eq!(m.get("s", "c"), Some("worse"));
        let writes = m.into_writes();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].value.as_deref(), Some("worse"));
        assert_eq!(writes[1].value, None);
    }
}
