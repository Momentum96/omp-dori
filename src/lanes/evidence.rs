//! Reports and evidence (EVR-01 to EVR-16): report input rules, file evidence with a stability
//! check, tool evidence from the bridge's facts, display cleaning and the thread text.

use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use crate::config::lanes::KINDS;

const TEXT_MAX: usize = 8000;
const SUMMARY_MAX: usize = 500;
const FILE_MAX: u64 = 1 << 30;
const ITEMS_MAX: usize = 64;
/// Tools that organise the lane rather than change anything (EVR req. 12).
const ORGANISING: [&str; 11] = [
    "cataly_lane",
    "cataly_lead",
    "ask",
    "todo",
    "todo_write",
    "goal",
    "think",
    "thinking",
    "task",
    "send_message",
    "notify",
];

/// The report tool's input (EVR req. 2).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ReportInput {
    pub(crate) kind: String,
    pub(crate) text: String,
    pub(crate) summary: Option<String>,
    pub(crate) files: Vec<String>,
    pub(crate) calls: Vec<String>,
}

/// Checks kind and sizes; nothing is stored when this fails.
pub(crate) fn check_report(r: &ReportInput) -> Result<(), String> {
    if !KINDS.contains(&r.kind.as_str()) {
        return Err(format!("kind must be one of {}", KINDS.join(", ")));
    }
    let text = r.text.trim();
    if text.is_empty() || text.chars().count() > TEXT_MAX {
        return Err("text must hold 1 to 8000 characters".into());
    }
    if let Some(s) = &r.summary
        && (s.trim().is_empty() || s.chars().count() > SUMMARY_MAX)
    {
        return Err("summary must hold 1 to 500 characters".into());
    }
    if r.files.len() + r.calls.len() > ITEMS_MAX {
        return Err("at most 64 evidence items".into());
    }
    Ok(())
}

/// One file as evidence: (identity, SHA-256), or why it is refused (EVR req. 9).
pub(crate) fn file_item(cwd: &Path, path: &str) -> Result<(String, String), String> {
    let joined = cwd.join(path);
    let real = std::fs::canonicalize(&joined).map_err(|e| match e.raw_os_error() {
        Some(40 | 62) => format!("{path}: symlink loop"),
        _ if e.kind() == std::io::ErrorKind::NotFound => format!("{path}: missing"),
        _ => format!("{path}: unreadable ({e})"),
    })?;
    let before = std::fs::metadata(&real).map_err(|e| format!("{path}: unreadable ({e})"))?;
    if !before.is_file() {
        return Err(format!("{path}: not a regular file"));
    }
    if before.len() > FILE_MAX {
        return Err(format!("{path}: larger than 1 GiB"));
    }
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut file = std::fs::File::open(&real).map_err(|e| format!("{path}: unreadable ({e})"))?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = std::io::Read::read(&mut file, &mut buf)
            .map_err(|e| format!("{path}: unreadable ({e})"))?;
        if n == 0 {
            break;
        }
        ctx.update(buf.get(..n).unwrap_or_default());
    }
    let after = std::fs::metadata(&real).map_err(|e| format!("{path}: unreadable ({e})"))?;
    let stamp = |m: &std::fs::Metadata| (m.dev(), m.ino(), m.len(), m.mtime(), m.mtime_nsec());
    if stamp(&before) != stamp(&after) {
        return Err(format!(
            "{path}: file was being modified; retry after it settles"
        ));
    }
    Ok((
        format!("file:{}", real.display()),
        crate::sys::hex(ctx.finish().as_ref()),
    ))
}

/// Whether a shell command the bridge saw runs the Cataly CLI (a word `cataly`, alone or ending a
/// path) or names a Cataly tool (refused in the shell, DESIGN 12.2 A13), between blanks or shell
/// separators. Such a call organises the lane (EVR req. 12).
fn runs_cataly(command: &str) -> bool {
    command
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')'))
        .any(|w| matches!(w, "cataly" | "cataly_lane" | "cataly_lead") || w.ends_with("/cataly"))
}

/// Tool evidence from the bridge's facts (EVR req. 10-13, EXT req. 21-23).
pub(crate) fn tool_items(
    calls: &[String],
    facts: Option<&Value>,
) -> Result<Vec<(String, String)>, String> {
    if calls.is_empty() {
        return Ok(Vec::new());
    }
    let facts = facts
        .and_then(Value::as_array)
        .ok_or("no tool facts from the bridge; nothing was recorded")?;
    let mut out = Vec::new();
    for call in calls {
        let fact = facts
            .iter()
            .find(|f| f.get("call").and_then(Value::as_str) == Some(call.as_str()))
            .ok_or(format!("call {call}: not seen by the bridge"))?;
        let flag = |k: &str| fact.get(k).and_then(Value::as_bool) == Some(true);
        let tool = fact.get("tool").and_then(Value::as_str).unwrap_or_default();
        if !flag("seen") {
            return Err(format!("call {call}: not seen starting in this run"));
        }
        if flag("ambiguous") {
            return Err(format!("call {call}: its ID started twice"));
        }
        if flag("error") {
            return Err(format!("call {call}: ended with an error"));
        }
        if ORGANISING.contains(&tool)
            || fact
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(runs_cataly)
        {
            return Err(format!(
                "call {call}: {tool} organises the lane and changes nothing"
            ));
        }
        if flag("running") {
            return Err(format!("call {call}: its background job has not completed"));
        }
        let fp = fact
            .get("fingerprint")
            .and_then(Value::as_str)
            .filter(|f| !f.is_empty())
            .ok_or(format!("call {call}: no result yet"))?;
        out.push((format!("tool:{fp}"), fp.to_string()));
    }
    Ok(out)
}

/// Display form of stored text: escapes and control characters dropped, line breaks folded into
/// single spaces (EVR req. 6, COORD req. 65).
pub(crate) fn display(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut space = false;
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // ESC [ ... final byte
            if chars.peek() == Some(&'[') {
                chars.next();
                for n in chars.by_ref() {
                    if ('@'..='~').contains(&n) {
                        break;
                    }
                }
            }
            continue;
        }
        let blank = c.is_whitespace()
            || c.is_control()
            || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
        if blank {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(c);
    }
    out
}

/// First `max` characters with "(truncated)" when cut (EVR req. 17).
pub(crate) fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut s: String = text.chars().take(max).collect();
    s.push_str(" (truncated)");
    s
}

/// Thread text (EVR req. 20, WS req. 15): home as `~`, other absolute paths removed, links reduced to
/// origin and path, backticks as plain quotes. Punctuation around a word is kept around its
/// replacement, so "/etc/hosts," becomes "[path removed],".
pub(crate) fn thread_text(text: &str, home: Option<&str>) -> String {
    let flat = display(text);
    let mut out = Vec::new();
    for word in flat.split(' ') {
        let word = word.replace('`', "'");
        let lead = word.len() - word.trim_start_matches(['(', '"', '\'', '[']).len();
        let rest = word.get(lead..).unwrap_or_default();
        let core = rest.trim_end_matches([',', '.', ';', ':', '!', '?', ')', ']', '"', '\'']);
        let trail = rest.get(core.len()..).unwrap_or_default();
        let cleaned = if core.starts_with("http://") || core.starts_with("https://") {
            let cut = core.find(['?', '#']).unwrap_or(core.len());
            core.get(..cut).unwrap_or(core).to_string()
        } else if let Some(h) = home.filter(|h| !h.is_empty() && core.starts_with(*h)) {
            format!("~{}", core.get(h.len()..).unwrap_or_default())
        } else if core.starts_with('/') && core.len() > 1 {
            "[path removed]".to_string()
        } else {
            core.to_string()
        };
        out.push(format!(
            "{}{cleaned}{trail}",
            word.get(..lead).unwrap_or_default()
        ));
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reports_files_and_tools() {
        let bad = ReportInput {
            kind: "progress".into(),
            text: "  ".into(),
            ..ReportInput::default()
        };
        assert!(check_report(&bad).is_err());
        let ok = ReportInput {
            kind: "finished".into(),
            text: "done".into(),
            ..ReportInput::default()
        };
        assert!(check_report(&ok).is_ok());
        let dir = crate::sys::fs::tests::scratch("evidence");
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let (ident, hash) = file_item(&dir, "a.txt").unwrap();
        assert!(ident.starts_with("file:/") && hash.len() == 64);
        assert!(file_item(&dir, "missing").unwrap_err().contains("missing"));
        assert!(file_item(&dir, ".").unwrap_err().contains("regular"));
        let facts = json!([
            { "call": "c1", "seen": true, "tool": "bash", "fingerprint": "f1" },
            { "call": "c2", "seen": true, "tool": "cataly_lane", "fingerprint": "f2" },
            { "call": "c3", "seen": true, "tool": "bash", "error": true, "fingerprint": "f3" },
            { "call": "c4", "seen": false },
            { "call": "c5", "seen": true, "tool": "bash", "command": "cd x && ~/bin/cataly lane list", "fingerprint": "f5" }
        ]);
        assert_eq!(
            tool_items(&["c1".into()], Some(&facts)).unwrap(),
            [("tool:f1".to_string(), "f1".to_string())]
        );
        for c in ["c2", "c3", "c4", "c5", "c9"] {
            assert!(tool_items(&[c.into()], Some(&facts)).is_err(), "{c}");
        }
    }

    #[test]
    fn display_and_thread_text_clean() {
        assert_eq!(display("a\u{1b}[31mb\n\nc\u{7}"), "ab c");
        assert_eq!(
            thread_text(
                "see /home/me/x.rs and /etc/passwd at https://h.io/p?q=1 `x`",
                Some("/home/me")
            ),
            "see ~/x.rs and [path removed] at https://h.io/p 'x'"
        );
        assert_eq!(
            thread_text(
                "(/etc/hosts), /home/me/a.txt. see https://h.io/p?q=1.",
                Some("/home/me")
            ),
            "([path removed]), ~/a.txt. see https://h.io/p."
        );
        assert_eq!(cut("abcdef", 3), "abc (truncated)");
    }
}
