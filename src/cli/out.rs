//! The CLI's only writers to stdout and stderr. A closed pipe is not an error worth reporting.

use std::io::Write;

/// Writes text to stdout as it is.
pub(crate) fn text(s: &str) {
    let _ = std::io::stdout().lock().write_all(s.as_bytes());
}

/// Writes a question without a line end and flushes it, so it shows before the answer is typed.
pub(crate) fn prompt(s: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

/// Writes one line to stdout.
pub(crate) fn line(s: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(s.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

/// Writes one JSON value as a line to stdout.
pub(crate) fn json(value: &serde_json::Value) {
    line(&serde_json::to_string(value).unwrap_or_default());
}

/// Writes one line to stderr.
pub(crate) fn error_line(s: &str) {
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(s.as_bytes());
    let _ = err.write_all(b"\n");
}
