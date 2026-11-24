//! The hub log: JSON lines with time, level, message and key-values, written to a rotating owner-only
//! file or to stderr. Values pass through secret masking, so a token never reaches the log.

use std::fmt::{self, Write as _};
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{LazyLock, Mutex, PoisonError};

use crate::sys::{fs, secrets};

/// Size at which the hub log file is rotated to `<name>.1`.
const ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// Log levels, most severe first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

impl Level {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "error" => Some(Self::Error),
            "warn" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
        }
    }
}

/// One key-value pair of a log line.
pub(crate) type Kv<'a> = (&'a str, &'a dyn fmt::Display);

enum Target {
    Stderr,
    File {
        path: PathBuf,
        file: File,
        size: u64,
    },
}

static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);
static TARGET: LazyLock<Mutex<Target>> = LazyLock::new(|| Mutex::new(Target::Stderr));

/// Sends the log to an owner-only file instead of stderr.
pub(crate) fn to_file(path: PathBuf) -> Result<(), fs::FsError> {
    let file = fs::open_append_private(&path)?;
    let size = file.metadata().map_or(0, |m| m.len());
    *TARGET.lock().unwrap_or_else(PoisonError::into_inner) = Target::File { path, file, size };
    Ok(())
}

pub(crate) fn set_level(level: Level) {
    LEVEL.store(level as u8, Ordering::Relaxed);
}

fn enabled(level: Level) -> bool {
    level as u8 <= LEVEL.load(Ordering::Relaxed)
}

pub(crate) fn error(msg: &str, kv: &[Kv<'_>]) {
    write(Level::Error, msg, kv);
}
pub(crate) fn warn(msg: &str, kv: &[Kv<'_>]) {
    write(Level::Warn, msg, kv);
}
pub(crate) fn info(msg: &str, kv: &[Kv<'_>]) {
    write(Level::Info, msg, kv);
}
pub(crate) fn debug(msg: &str, kv: &[Kv<'_>]) {
    write(Level::Debug, msg, kv);
}

fn write(level: Level, msg: &str, kv: &[Kv<'_>]) {
    if !enabled(level) {
        return;
    }
    let line = format_line(&jiff::Timestamp::now().to_string(), level, msg, kv);
    let mut target = TARGET.lock().unwrap_or_else(PoisonError::into_inner);
    match &mut *target {
        Target::Stderr => {
            let _ = std::io::stderr().lock().write_all(line.as_bytes());
        }
        Target::File { path, file, size } => {
            if *size >= ROTATE_BYTES {
                let mut old = path.clone().into_os_string();
                old.push(".1");
                if std::fs::rename(&*path, &old).is_ok()
                    && let Ok(fresh) = fs::open_append_private(path)
                {
                    *file = fresh;
                    *size = 0;
                }
            }
            if file.write_all(line.as_bytes()).is_ok() {
                *size = size.saturating_add(u64::try_from(line.len()).unwrap_or(0));
            }
        }
    }
}

fn format_line(time: &str, level: Level, msg: &str, kv: &[Kv<'_>]) -> String {
    let mut line = String::with_capacity(96 + msg.len());
    let mut value = String::new();
    line.push_str("{\"t\":");
    push_json(&mut line, time);
    line.push_str(",\"level\":");
    push_json(&mut line, level.as_str());
    line.push_str(",\"msg\":");
    push_json(&mut line, &secrets::mask(msg));
    for (key, val) in kv {
        value.clear();
        let _ = write!(value, "{val}");
        line.push(',');
        push_json(&mut line, key);
        line.push(':');
        push_json(&mut line, &secrets::mask(&value));
    }
    line.push_str("}\n");
    line
}

/// Appends `s` as a JSON string literal.
pub(crate) fn push_json(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_json_with_masked_values() {
        let token = format!("ghp_{}", "x".repeat(36));
        let line = format_line(
            "T",
            Level::Warn,
            "task \"x\" ended",
            &[("error", &token), ("n", &3)],
        );
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["level"], "warn");
        assert_eq!(v["msg"], "task \"x\" ended");
        assert_eq!(v["error"], "[masked]");
        assert_eq!(v["n"], "3");
        assert!(line.ends_with("}\n"));
        let mut s = String::new();
        push_json(&mut s, "a\u{1}b");
        assert_eq!(s, "\"a\\u0001b\"");
    }

    #[test]
    fn levels_filter() {
        assert_eq!(Level::parse("debug"), Some(Level::Debug));
        assert_eq!(Level::parse("loud"), None);
        assert!(Level::Error < Level::Debug);
        assert!(enabled(Level::Error));
    }
}
