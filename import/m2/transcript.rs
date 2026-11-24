//! OMP session-file reader: whether a saved session file may be resumed exactly, and the model OMP will restore
//! from it.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead as _, BufReader, Read as _, Seek as _, SeekFrom};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};
use serde_json::Value;

const HEADER_BYTES: u64 = 32 * 1024;
const LINE_LIMIT: u64 = 64 * 1024 * 1024;

pub(super) struct Transcript {
    pub(super) model: Option<String>,
}

/// The session ID in an OMP session file name `<stamp>Z_<uuid>.jsonl`.
pub(super) fn session_file_id(name: &str) -> Option<&str> {
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
        &stamp[..13],
        &stamp[14..16],
        &stamp[17..19],
        &stamp[20..23]
    );
    parse_iso(&iso).map(|_| id)
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
        && let Some(model) = entry.get("model").and_then(Value::as_str).filter(|m| !m.is_empty())
    {
        let role = entry.get("role").and_then(Value::as_str).unwrap_or("default");
        return Some(BranchEntry {
            parent,
            model: Some((role.to_owned(), model.to_owned())),
            assistant_model: None,
        });
    }
    if entry.get("type").and_then(Value::as_str) == Some("message")
        && let Some(message) = entry.get("message").filter(|m| m.is_object())
        && message.get("role").and_then(Value::as_str) == Some("assistant")
        && let (Some(provider), Some(model)) = (message.get("provider").and_then(Value::as_str), message.get("model").and_then(Value::as_str))
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
fn model_selector(selector: &str) -> bool {
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

/// The first `session` entry, skipping only leading `title` entries in the first 32 kibibytes.
fn header(file: &mut File) -> Result<Option<Value>> {
    let mut head = Vec::new();
    file.by_ref().take(HEADER_BYTES).read_to_end(&mut head)?;
    let mut start = 0;
    while start < head.len() {
        let Some(end) = head[start..].iter().position(|b| *b == b'\n') else {
            return Ok(None);
        };
        let entry: Value = serde_json::from_slice(&head[start..start + end])?;
        if !entry.is_object() {
            return Ok(None);
        }
        match entry.get("type").and_then(Value::as_str) {
            Some("session") => return Ok(Some(entry)),
            Some("title") => start += end + 1,
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

/// Verifies a saved session file for an exact resume. `Ok(None)` is "not verified".
pub(super) fn exact(
    path: &str,
    root: &Path,
    session: &str,
    cwd: &Path,
) -> Result<Option<Transcript>> {
    let given = Path::new(path);
    let name = given
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default();
    if !given.is_absolute() || path.chars().any(char::is_control) || session_file_id(name) != Some(session) {
        return Ok(None);
    }
    let file = real(given)?;
    let root = real(root)?;
    if file != given || !strictly_inside(&root, &file) {
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
    let Some(head) = header(&mut handle)? else {
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
        match header(&mut other)? {
            Some(candidate) if candidate.get("continuationOf").and_then(Value::as_str) != Some(session) => {}
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
            .take(LINE_LIMIT + 1)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        if u64::try_from(line.len()).unwrap_or(u64::MAX) > LINE_LIMIT {
            bail!("transcript line exceeds its limit");
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        let entry: Value = serde_json::from_slice(&line)?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
