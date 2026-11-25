//! Operating-system services: paths, owner-only files, the hub lock, peer credentials, bounded
//! subprocesses, host readers, service-manager notification, the stall detector, secret masking and
//! latency rings.

pub(crate) mod ancestry;
pub(crate) mod command;
pub(crate) mod fs;
pub(crate) mod host;
pub(crate) mod install;
pub(crate) mod latency;
pub(crate) mod lock;
pub(crate) mod notify;
pub(crate) mod paths;
pub(crate) mod peer;
pub(crate) mod release;
pub(crate) mod secrets;
pub(crate) mod service;
pub(crate) mod stall;

/// Debug builds only: crash points between durable steps, named by `CATALY_FAULT`, for the crash
/// tests. Release builds hold neither the points nor their names.
#[cfg(debug_assertions)]
pub(crate) mod fault {
    /// Whether `CATALY_FAULT` names this point.
    pub(crate) fn named(point: &str) -> bool {
        std::env::var("CATALY_FAULT").is_ok_and(|v| v == point)
    }

    /// Aborts the process, with no clean-up, when `CATALY_FAULT` names this point.
    pub(crate) fn point(point: &str) {
        if named(point) {
            std::process::abort();
        }
    }
}

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

/// Lower-case hexadecimal of `bytes`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}
