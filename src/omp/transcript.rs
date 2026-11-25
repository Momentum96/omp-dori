//! OMP session-file reader (DESIGN 3.3, 7.1; PATCH-01 rules 8-10, HOST req. 9-10): whether a saved
//! session file may be resumed exactly and which model OMP will restore from it, and the bounded search
//! for a delivery marker in a user entry. Session files belong to OMP; the hub only reads them, with
//! no symlink followed, no FIFO opened, a 64 KiB header (1 MiB for a reopen check), the last 8 MiB and
//! 2 s for a search, and nothing over 64 MiB. Every read here blocks; callers use the async wrappers,
//! which run on the blocking pool.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{BufRead as _, BufReader, Read as _, Seek as _, SeekFrom};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Header search window for a delivery check.
const HEADER_BYTES: u64 = 64 * 1024;
/// Header search window when a transcript is reopened.
const REOPEN_HEADER_BYTES: u64 = 1024 * 1024;
/// Largest session file read at all.
const FILE_LIMIT: u64 = 64 * 1024 * 1024;
/// A delivery search reads at most this much from the end of the file.
const SEARCH_TAIL: u64 = 8 * 1024 * 1024;
/// And takes at most this long.
const SEARCH_BUDGET: Duration = Duration::from_secs(2);

/// A session file could not be read safely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TranscriptError {
    Io(String),
    /// Over the size limit, or a line over it.
    TooLarge,
    /// Not valid JSON lines.
    Malformed,
}

impl fmt::Display for TranscriptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "session file: {e}"),
            Self::TooLarge => f.write_str("session file exceeds 64 MiB"),
            Self::Malformed => f.write_str("session file is not valid JSON lines"),
        }
    }
}

impl std::error::Error for TranscriptError {}

impl From<std::io::Error> for TranscriptError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<rustix::io::Errno> for TranscriptError {
    fn from(e: rustix::io::Errno) -> Self {
        Self::Io(e.to_string())
    }
}

type Result<T, E = TranscriptError> = std::result::Result<T, E>;

/// What a verified saved session file says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Transcript {
    /// The model OMP restores on resume (`provider/id`), when the file names one.
    pub(crate) model: Option<String>,
}

/// The session ID in an OMP session file name `<stamp>Z_<uuid>.jsonl`.
pub(crate) fn session_file_id(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(".jsonl")?;
    let (stamp, id) = stem.split_once("Z_")?;
    let valid_id = id.len() == 36
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-');
    // 2026-01-02T03-04-05-678
    let bytes = stamp.as_bytes();
    let shape = bytes.len() == 23
        && bytes.iter().enumerate().all(|(at, b)| match at {
            4 | 7 | 13 | 16 | 19 => *b == b'-',
            10 => *b == b'T',
            _ => b.is_ascii_digit(),
        });
    if !valid_id || !shape {
        return None;
    }
    let iso = format!(
        "{}:{}:{}.{}Z",
        stamp.get(..13)?,
        stamp.get(14..16)?,
        stamp.get(17..19)?,
        stamp.get(20..23)?
    );
    iso.parse::<jiff::Timestamp>().ok().map(|_| id)
}

struct BranchEntry {
    parent: Option<String>,
    model: Option<(String, String)>,
    assistant_model: Option<String>,
}

fn branch_entry(entry: &Value) -> Option<BranchEntry> {
    let parent = match entry.get("parentId") {
        Some(Value::Null) => None,
        Some(Value::String(parent)) => Some(parent.clone()),
        _ => return None,
    };
    if entry.get("type").and_then(Value::as_str) == Some("model_change")
        && let Some(model) = entry
            .get("model")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
    {
        let role = entry
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("default");
        return Some(BranchEntry {
            parent,
            model: Some((role.to_owned(), model.to_owned())),
            assistant_model: None,
        });
    }
    if entry.get("type").and_then(Value::as_str) == Some("message")
        && let Some(message) = entry.get("message").filter(|m| m.is_object())
        && message.get("role").and_then(Value::as_str) == Some("assistant")
        && let (Some(provider), Some(model)) = (
            message.get("provider").and_then(Value::as_str),
            message.get("model").and_then(Value::as_str),
        )
        && model != "<synthetic>"
    {
        return Some(BranchEntry {
            parent,
            model: None,
            assistant_model: Some(format!("{provider}/{model}")),
        });
    }
    Some(BranchEntry {
        parent,
        model: None,
        assistant_model: None,
    })
}

/// `provider/id`, neither part empty, no whitespace or control characters.
pub(crate) fn model_selector(selector: &str) -> bool {
    let Some((provider, id)) = selector.split_once('/') else {
        return false;
    };
    let bad = |c: char| c.is_whitespace() || c.is_control();
    !provider.is_empty() && !id.is_empty() && !provider.chars().any(bad) && !id.chars().any(bad)
}

/// The active branch's last model role, never "fallback".
fn saved_model(entries: &HashMap<String, BranchEntry>, leaf: Option<&str>) -> Option<String> {
    let mut role: Option<&str> = None;
    let mut preferred: Option<&str> = None;
    let mut default_model: Option<&str> = None;
    let mut legacy: Option<&str> = None;
    let mut seen = HashSet::new();
    let mut id = leaf;
    while let Some(at) = id {
        let entry = entries.get(at)?;
        if !seen.insert(at) {
            return None;
        }
        if let Some((entry_role, selector)) = &entry.model {
            let current = *role.get_or_insert(entry_role.as_str());
            if current != "fallback" && entry_role == current {
                preferred = preferred.or(Some(selector.as_str()));
            }
            if entry_role == "default" {
                default_model = default_model.or(Some(selector.as_str()));
            }
        }
        legacy = legacy.or(entry.assistant_model.as_deref());
        id = entry.parent.as_deref();
    }
    preferred
        .or(default_model)
        .or(legacy)
        .filter(|selector| model_selector(selector))
        .map(str::to_owned)
}

/// Opens read-only without following a final symlink and without blocking on a FIFO.
fn read_only(path: &Path) -> Result<File> {
    use rustix::fs::{Mode, OFlags};
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    Ok(File::from(fd))
}

/// The first `session` entry, skipping only leading `title` entries in the first `limit` bytes.
fn header(file: &mut File, limit: u64) -> Result<Option<Value>> {
    let mut head = Vec::new();
    file.by_ref().take(limit).read_to_end(&mut head)?;
    let mut rest = head.as_slice();
    while !rest.is_empty() {
        let Some(end) = rest.iter().position(|b| *b == b'\n') else {
            return Ok(None);
        };
        let (line, tail) = rest.split_at(end);
        let entry: Value = serde_json::from_slice(line).map_err(|_| TranscriptError::Malformed)?;
        if !entry.is_object() {
            return Ok(None);
        }
        match entry.get("type").and_then(Value::as_str) {
            Some("session") => return Ok(Some(entry)),
            Some("title") => rest = tail.get(1..).unwrap_or_default(),
            _ => return Ok(None),
        }
    }
    Ok(None)
}

fn real(path: &Path) -> Result<PathBuf> {
    Ok(fs::canonicalize(path)?)
}

/// `file` lies below `root` (not equal to it) without `..` or root components.
fn strictly_inside(root: &Path, file: &Path) -> bool {
    file.strip_prefix(root).is_ok_and(|rel| {
        rel.components().next().is_some()
            && rel.components().all(|c| matches!(c, Component::Normal(_)))
    })
}

/// Verifies a saved session file for an exact resume (HOST req. 9-10). `Ok(None)` is "not verified".
pub(crate) fn exact(
    path: &Path,
    root: &Path,
    session: &str,
    cwd: &Path,
) -> Result<Option<Transcript>> {
    let name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default();
    if !path.is_absolute()
        || path.to_string_lossy().chars().any(char::is_control)
        || session_file_id(name) != Some(session)
    {
        return Ok(None);
    }
    let file = real(path)?;
    let root = real(root)?;
    if file != path || !strictly_inside(&root, &file) {
        return Ok(None);
    }
    let uid = rustix::process::getuid().as_raw();
    let parent = root.parent().map(fs::metadata).transpose()?;
    // OMP sometimes writes group-writable files beneath its mode-0700 agent directory.
    let write_mask =
        if parent.is_some_and(|p| p.is_dir() && p.uid() == uid && p.mode().trailing_zeros() >= 6) {
            0o002
        } else {
            0o022
        };
    let mut dir = file.parent().map(Path::to_path_buf);
    while let Some(at) = dir {
        let info = fs::metadata(&at)?;
        if !info.is_dir() || info.uid() != uid || info.mode() & write_mask != 0 {
            return Ok(None);
        }
        if at == root {
            break;
        }
        dir = at.parent().map(Path::to_path_buf);
    }
    let mut handle = read_only(&file)?;
    let info = handle.metadata()?;
    if !info.is_file() || info.uid() != uid || info.nlink() != 1 || info.mode() & write_mask != 0 {
        return Ok(None);
    }
    if info.len() > FILE_LIMIT {
        return Err(TranscriptError::TooLarge);
    }
    let Some(head) = header(&mut handle, REOPEN_HEADER_BYTES)? else {
        return Ok(None);
    };
    let header_cwd = head.get("cwd").and_then(Value::as_str).map(Path::new);
    if head.get("id").and_then(Value::as_str) != Some(session)
        || !header_cwd.is_some_and(Path::is_absolute)
        || header_cwd.map(real).transpose()?.as_deref() != Some(cwd)
    {
        return Ok(None);
    }
    // Only generated sibling headers can redirect OMP's resume target.
    let folder = file.parent().unwrap_or(root.as_path());
    for sibling in fs::read_dir(folder)? {
        let sibling = sibling?;
        let sibling_name = sibling.file_name();
        let Some(sibling_name) = sibling_name.to_str() else {
            continue;
        };
        if sibling_name == name || session_file_id(sibling_name).is_none() {
            continue;
        }
        let mut other = read_only(&sibling.path())?;
        let entry = other.metadata()?;
        if !entry.is_file() || entry.uid() != uid || entry.mode() & write_mask != 0 {
            return Ok(None);
        }
        match header(&mut other, HEADER_BYTES)? {
            Some(candidate)
                if candidate.get("continuationOf").and_then(Value::as_str) != Some(session) => {}
            _ => return Ok(None),
        }
    }
    handle.seek(SeekFrom::Start(0))?;
    read_branch(handle)
}

/// Reads every entry for the saved model; `None` when the transcript departed or holds a non-object line.
fn read_branch(handle: File) -> Result<Option<Transcript>> {
    let mut reader = BufReader::new(handle);
    let mut entries: HashMap<String, BranchEntry> = HashMap::new();
    let mut leaf: Option<String> = None;
    let mut last_self_selected: Option<String> = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader
            .by_ref()
            .take(FILE_LIMIT + 1)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        if u64::try_from(line.len()).unwrap_or(u64::MAX) > FILE_LIMIT {
            return Err(TranscriptError::TooLarge);
        }
        trim_newline(&mut line);
        let entry: Value = serde_json::from_slice(&line).map_err(|_| TranscriptError::Malformed)?;
        if !entry.is_object() {
            return Ok(None);
        }
        if entry.get("type").and_then(Value::as_str) == Some("custom_message")
            && entry.get("customType").and_then(Value::as_str) == Some("session-transfer")
            && entry
                .get("details")
                .is_some_and(|d| d.get("kind").and_then(Value::as_str) == Some("departed"))
        {
            return Ok(None);
        }
        let Some(id) = entry.get("id").and_then(Value::as_str).map(str::to_owned) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) == Some("session") {
            continue;
        }
        let Some(parsed) = branch_entry(&entry) else {
            continue;
        };
        entries.insert(id.clone(), parsed);
        leaf = match entry.get("activeLeafId") {
            Some(Value::Null) => None,
            Some(Value::String(active)) => Some(active.clone()),
            _ => Some(id.clone()),
        };
        if leaf.as_deref() == Some(id.as_str()) {
            last_self_selected = Some(id);
        }
    }
    let start = match &leaf {
        None => None,
        Some(at) if entries.contains_key(at) => Some(at.as_str()),
        Some(_) => last_self_selected.as_deref(),
    };
    Ok(Some(Transcript {
        model: saved_model(&entries, start),
    }))
}

fn trim_newline(line: &mut Vec<u8>) {
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
}

/// What a delivery search found (PATCH-01 rule 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Found {
    /// A user message entry carries the marker.
    Yes,
    /// The searched part of the file has no such entry.
    No,
    /// The file does not exist (yet).
    Missing,
    /// The search ran out of its 2 s budget.
    OutOfTime,
}

/// Looks for `marker` in the text of a user `message` entry within the last 8 MiB of `path`. A marker
/// quoted by the assistant or a tool does not count.
pub(crate) fn find_marker(path: &Path, marker: &str) -> Result<Found> {
    let started = Instant::now();
    let mut file = match read_only(path) {
        Ok(f) => f,
        Err(TranscriptError::Io(_)) if !path.exists() => return Ok(Found::Missing),
        Err(e) => return Err(e),
    };
    let info = file.metadata()?;
    if !info.is_file() {
        return Err(TranscriptError::Io("not a regular file".into()));
    }
    if info.len() > FILE_LIMIT {
        return Err(TranscriptError::TooLarge);
    }
    let start = info.len().saturating_sub(SEARCH_TAIL);
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut first = start > 0;
    loop {
        if started.elapsed() > SEARCH_BUDGET {
            return Ok(Found::OutOfTime);
        }
        line.clear();
        if reader
            .by_ref()
            .take(SEARCH_TAIL + 1)
            .read_until(b'\n', &mut line)?
            == 0
        {
            return Ok(Found::No);
        }
        // A tail that starts mid-file begins inside a line; that partial line is skipped.
        if std::mem::take(&mut first) {
            continue;
        }
        if !contains(&line, marker.as_bytes()) {
            continue;
        }
        trim_newline(&mut line);
        let Ok(entry) = serde_json::from_slice::<Value>(&line) else {
            // The last line may still be being written.
            continue;
        };
        if user_text_has(&entry, marker) {
            return Ok(Found::Yes);
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

fn user_text_has(entry: &Value, marker: &str) -> bool {
    if entry.get("type").and_then(Value::as_str) != Some("message") {
        return false;
    }
    let Some(message) = entry.get("message") else {
        return false;
    };
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    match message.get("content") {
        Some(Value::String(text)) => text.contains(marker),
        Some(Value::Array(parts)) => parts.iter().any(|p| {
            p.get("type").and_then(Value::as_str) == Some("text")
                && p.get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t.contains(marker))
        }),
        _ => false,
    }
}

/// Where the answer to one request stands in a session file (DISC-OUT req. 21-23).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reply {
    /// No user entry carries the tag in the searched part of the file.
    NotFound,
    /// The turn has not ended yet.
    Pending,
    /// The final assistant text of the turn the request started.
    Answer(String),
    /// Another input entered the transcript before an answer: the answer would belong to mixed work.
    Abandoned,
}

/// One message entry: role, visible text, whether it calls a tool, and the stop reason.
struct Said {
    user: bool,
    text: String,
    tool_call: bool,
    stop: Option<String>,
}

fn said(entry: &Value) -> Option<Said> {
    if entry.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let message = entry.get("message")?;
    let user = match message.get("role").and_then(Value::as_str)? {
        "user" => true,
        "assistant" => false,
        _ => return None,
    };
    let mut text = String::new();
    let mut tool_call = false;
    match message.get("content") {
        Some(Value::String(t)) => text.push_str(t),
        Some(Value::Array(parts)) => {
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    // Commentary is the provider's running narration, not the answer.
                    Some("text")
                        if part.get("phase").and_then(Value::as_str) != Some("commentary") =>
                    {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(t);
                        }
                    }
                    Some("toolCall" | "tool_use" | "toolUse") => tool_call = true,
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Some(Said {
        user,
        text,
        tool_call,
        stop: message
            .get("stopReason")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Reads the message entries of the last 8 MiB of `path` (2 s budget). `None` when the file is
/// missing or the budget ran out.
fn message_entries(path: &Path) -> Result<Option<Vec<Said>>> {
    let started = Instant::now();
    let mut file = match read_only(path) {
        Ok(f) => f,
        Err(TranscriptError::Io(_)) if !path.exists() => return Ok(None),
        Err(e) => return Err(e),
    };
    let info = file.metadata()?;
    if !info.is_file() {
        return Err(TranscriptError::Io("not a regular file".into()));
    }
    if info.len() > FILE_LIMIT {
        return Err(TranscriptError::TooLarge);
    }
    let start = info.len().saturating_sub(SEARCH_TAIL);
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut first = start > 0;
    let mut out = Vec::new();
    loop {
        if started.elapsed() > SEARCH_BUDGET {
            return Ok(None);
        }
        line.clear();
        if reader
            .by_ref()
            .take(SEARCH_TAIL + 1)
            .read_until(b'\n', &mut line)?
            == 0
        {
            return Ok(Some(out));
        }
        if std::mem::take(&mut first) {
            continue;
        }
        // An unfinished last line is skipped; it is read again next time.
        if line.last() != Some(&b'\n') {
            continue;
        }
        trim_newline(&mut line);
        if let Ok(entry) = serde_json::from_slice::<Value>(&line)
            && let Some(s) = said(&entry)
        {
            out.push(s);
        }
    }
}

/// The answer to each request tag. `prefix` starts every request tag, so a later request in the same
/// user entry (one batch) takes the answer; `settled` means the session is idle, so a final message
/// at the end of the file is the answer rather than a step of a turn still running.
pub(crate) fn replies(
    path: &Path,
    tags: &[String],
    prefix: &str,
    settled: bool,
) -> Result<Vec<Reply>> {
    let Some(entries) = message_entries(path)? else {
        return Ok(vec![Reply::Pending; tags.len()]);
    };
    Ok(tags
        .iter()
        .map(|tag| {
            let Some(at) = entries
                .iter()
                .rposition(|e| e.user && e.text.contains(tag.as_str()))
            else {
                return Reply::NotFound;
            };
            let later_request = entries.get(at).is_some_and(|e| {
                e.text
                    .find(tag.as_str())
                    .and_then(|i| e.text.get(i + tag.len()..))
                    .is_some_and(|rest| rest.contains(prefix))
            });
            if later_request {
                return Reply::Abandoned;
            }
            let mut answer: Option<&str> = None;
            for e in entries.iter().skip(at + 1) {
                if e.user {
                    return answer.map_or(Reply::Abandoned, |a| Reply::Answer(a.to_string()));
                }
                answer = (e.stop.as_deref() == Some("stop")
                    && !e.tool_call
                    && !e.text.trim().is_empty())
                .then_some(e.text.as_str());
            }
            match answer {
                Some(a) if settled => Reply::Answer(a.to_string()),
                _ => Reply::Pending,
            }
        })
        .collect())
}

/// [`replies`] on the blocking pool.
pub(crate) async fn read_replies(
    path: PathBuf,
    tags: Vec<String>,
    prefix: String,
    settled: bool,
) -> Result<Vec<Reply>> {
    tokio::task::spawn_blocking(move || replies(&path, &tags, &prefix, settled))
        .await
        .map_err(|e| TranscriptError::Io(format!("reply task: {e}")))?
}

/// [`find_marker`] on the blocking pool.
pub(crate) async fn search(path: PathBuf, marker: String) -> Result<Found> {
    tokio::task::spawn_blocking(move || find_marker(&path, &marker))
        .await
        .map_err(|e| TranscriptError::Io(format!("search task: {e}")))?
}

/// [`exact`] on the blocking pool.
pub(crate) async fn verify(
    path: PathBuf,
    root: PathBuf,
    session: String,
    cwd: PathBuf,
) -> Result<Option<Transcript>> {
    tokio::task::spawn_blocking(move || exact(&path, &root, &session, &cwd))
        .await
        .map_err(|e| TranscriptError::Io(format!("verify task: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_follow_the_turn_a_request_started() {
        let dir = crate::sys::fs::tests::scratch("replies");
        let path = dir.join("s.jsonl");
        let user = |t: &str| {
            format!(
                r#"{{"type":"message","message":{{"role":"user","content":[{{"type":"text","text":"{t}"}}]}}}}"#
            )
        };
        let said = |t: &str, stop: &str, tool: bool| {
            let tool = if tool {
                r#",{"type":"toolCall","id":"c","name":"bash","arguments":{}}"#
            } else {
                ""
            };
            format!(
                r#"{{"type":"message","message":{{"role":"assistant","content":[{{"type":"text","text":"{t}"}}{tool}],"stopReason":"{stop}"}}}}"#
            )
        };
        let lines = [
            r#"{"type":"session","id":"x"}"#.to_string(),
            user("[d a] one"),
            said("looking", "toolUse", true),
            said("first answer", "stop", false),
            user("[d b] two"),
            said("thinking", "toolUse", true),
            user("[d c] three [d e] five"),
            said("done", "stop", false),
        ];
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let tags: Vec<String> = ["[d a]", "[d b]", "[d c]", "[d e]", "[d z]"]
            .map(String::from)
            .to_vec();
        let got = replies(&path, &tags, "[d ", false).unwrap();
        assert_eq!(got[0], Reply::Answer("first answer".into()));
        assert_eq!(got[1], Reply::Abandoned, "steered before an answer");
        assert_eq!(
            got[2],
            Reply::Abandoned,
            "a later request in the same batch takes it"
        );
        assert_eq!(got[3], Reply::Pending, "the session still works");
        assert_eq!(got[4], Reply::NotFound);
        let got = replies(&path, &tags, "[d ", true).unwrap();
        assert_eq!(got[3], Reply::Answer("done".into()));
    }

    #[test]
    fn session_file_names() {
        let id = "0b9e7c1a-1111-4222-8333-444455556666";
        assert_eq!(
            session_file_id(&format!("2026-11-01T10-20-30-400Z_{id}.jsonl")),
            Some(id)
        );
        assert_eq!(
            session_file_id(&format!("2026-13-01T10-20-30-400Z_{id}.jsonl")),
            None
        );
        assert_eq!(session_file_id("x.jsonl"), None);
    }

    #[test]
    fn saved_model_prefers_the_branch_role() {
        let mut entries = HashMap::new();
        let entry = |parent: Option<&str>, model: Option<(&str, &str)>, assistant: Option<&str>| {
            BranchEntry {
                parent: parent.map(str::to_owned),
                model: model.map(|(r, s)| (r.to_owned(), s.to_owned())),
                assistant_model: assistant.map(str::to_owned),
            }
        };
        entries.insert(
            "a".to_owned(),
            entry(None, Some(("default", "p/one")), None),
        );
        entries.insert("b".to_owned(), entry(Some("a"), None, Some("p/legacy")));
        entries.insert(
            "c".to_owned(),
            entry(Some("b"), Some(("fallback", "p/fb")), None),
        );
        assert_eq!(saved_model(&entries, Some("c")).as_deref(), Some("p/one"));
        assert_eq!(saved_model(&entries, Some("b")).as_deref(), Some("p/one"));
        assert_eq!(saved_model(&entries, Some("x")), None);
        assert!(!model_selector("a b/c") && model_selector("a/b/c"));
    }

    #[test]
    fn markers_count_only_in_user_entries() {
        let dir = crate::sys::fs::tests::scratch("transcript");
        let path = dir.join("s.jsonl");
        let user = |text: &str| {
            serde_json::json!({"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": text}]}})
                .to_string()
        };
        let assistant = serde_json::json!({"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "[cataly:bbb] echoed"}]}});
        assert_eq!(find_marker(&path, "[cataly:aaa]").unwrap(), Found::Missing);
        fs::write(
            &path,
            format!(
                "{{\"type\":\"session\",\"id\":\"x\"}}\n{}\n{assistant}\n{{\"type\":\"message\",\"mess",
                user("[cataly:aaa] owner\nhello")
            ),
        )
        .unwrap();
        assert_eq!(find_marker(&path, "[cataly:aaa]").unwrap(), Found::Yes);
        assert_eq!(find_marker(&path, "[cataly:bbb]").unwrap(), Found::No);
        let string_content = serde_json::json!({"type": "message", "message": {"role": "user", "content": "[cataly:ccc] x"}});
        fs::write(&path, format!("{string_content}\n")).unwrap();
        assert_eq!(find_marker(&path, "[cataly:ccc]").unwrap(), Found::Yes);
    }
}
