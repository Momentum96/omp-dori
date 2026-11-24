//! Operating-system services: paths, owner-only files, the hub lock, peer credentials, bounded
//! subprocesses, host readers, service-manager notification, the stall detector, secret masking and
//! latency rings.

pub(crate) mod command;
pub(crate) mod fs;
pub(crate) mod host;
pub(crate) mod latency;
pub(crate) mod lock;
pub(crate) mod notify;
pub(crate) mod paths;
pub(crate) mod peer;
pub(crate) mod secrets;
pub(crate) mod stall;

use std::fmt;

/// The one decision class every error carries (DESIGN 9.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorClass {
    /// Retry with backoff.
    Transient,
    /// Tell the caller; nothing happened.
    Rejected,
    /// The effect may or may not have happened; never retried.
    InDoubt,
    /// Stop the task.
    Fatal,
}

impl ErrorClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::Rejected => "rejected",
            Self::InDoubt => "in_doubt",
            Self::Fatal => "fatal",
        }
    }
}

impl fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The real user ID of this process.
pub(crate) fn own_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// Milliseconds since the Unix epoch, from the wall clock.
pub(crate) fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}
