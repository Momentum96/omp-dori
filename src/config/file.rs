//! Line-preserving edits of the config file, shared by `cataly setup` and the hub: one setting's
//! line is replaced or added and everything else (comments, unknown keys, order) is kept. A write
//! snapshots the file (identity and bytes), validates the result as `cataly config check` does,
//! re-checks the snapshot just before an atomic replace (or a create that never overwrites), and
//! reads the file back, so a hand edit made meanwhile is never clobbered.

use std::fmt::Write as _;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::{FILE_MAX, describe, parse, sha256_hex};
use crate::sys::fs::{create_atomic, read_private, write_atomic};

/// Device, inode, mode, owner, group, size and the modification and change times.
type Identity = (u64, u64, u32, u32, u32, u64, i64, i64, i64, i64);

/// What identifies the file: any change of it means someone else wrote meanwhile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Snapshot {
    identity: Option<Identity>,
    pub(crate) bytes: Option<Vec<u8>>,
}

impl Snapshot {
    pub(crate) fn digest(&self) -> String {
        self.bytes
            .as_deref()
            .map_or_else(|| "none".to_string(), sha256_hex)
    }
}

pub(crate) fn snapshot(path: &Path) -> Result<Snapshot, String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Snapshot {
                identity: None,
                bytes: None,
            });
        }
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    // The same rule the hub applies when it reads the file: refuse, naming the fix.
    crate::sys::fs::check_private_file(path, &meta)
        .map_err(|e| format!("{e}; nothing is written to it"))?;
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > FILE_MAX {
        return Err(format!("{} is larger than 1 MiB", path.display()));
    }
    Ok(Snapshot {
        identity: Some((
            meta.dev(),
            meta.ino(),
            meta.mode(),
            meta.uid(),
            meta.gid(),
            meta.size(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec(),
        )),
        bytes: Some(bytes),
    })
}

/// A TOML basic string.
pub(crate) fn toml_string(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A TOML array of basic strings, such as `["1", "2"]`.
pub(crate) fn string_list<T: ToString>(items: &[T]) -> String {
    let quoted: Vec<String> = items.iter().map(|i| toml_string(&i.to_string())).collect();
    format!("[{}]", quoted.join(", "))
}

/// Sets `key` in `[section]` to `value`, replacing its line or adding it; everything else is kept.
pub(crate) fn set_key(text: &str, section: &str, key: &str, value: &str) -> String {
    set_rendered(text, section, key, &toml_string(value))
}

/// [`set_key`] with a value already written as TOML.
pub(crate) fn set_rendered(text: &str, section: &str, key: &str, rendered: &str) -> String {
    let line = format!("{key} = {rendered}");
    let header = format!("[{section}]");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let start = lines.iter().position(|l| l.trim() == header);
    let Some(start) = start else {
        let mut out = text.trim_end().to_string();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        let _ = write!(out, "{header}\n{line}\n");
        return out;
    };
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, l)| l.trim_start().starts_with('['))
        .map_or(lines.len(), |(i, _)| i);
    let existing = (start + 1..end).find(|i| {
        lines
            .get(*i)
            .and_then(|l| l.trim_start().strip_prefix(key))
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    });
    if let Some(i) = existing {
        if let Some(l) = lines.get_mut(i) {
            *l = line;
        }
    } else {
        let last = (start..end)
            .rev()
            .find(|i| lines.get(*i).is_some_and(|l| !l.trim().is_empty()))
            .unwrap_or(start);
        lines.insert(last + 1, line);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Edits the file as it is now: `change` gets its text (`missing` when there is no file) and
/// returns the new text, which must be a valid config; the write follows the snapshot, atomic-write
/// and read-back rules of [`publish`]. Returns the text written.
pub(crate) fn edit(
    path: &Path,
    missing: &str,
    change: impl FnOnce(&str) -> Result<String, String>,
) -> Result<String, String> {
    let before = snapshot(path)?;
    let text = match &before.bytes {
        Some(b) => {
            String::from_utf8(b.clone()).map_err(|_| format!("{} is not UTF-8", path.display()))?
        }
        None => missing.to_string(),
    };
    let text = change(&text)?;
    parse(&text)
        .map_err(|e| format!("the result would not be a valid config: {}", describe(&e)))?;
    publish(path, &before, text.as_bytes())?;
    Ok(text)
}

/// Re-checks the snapshot, writes atomically (never overwriting a file that appeared meanwhile), and
/// reads the result back.
pub(crate) fn publish(path: &Path, before: &Snapshot, bytes: &[u8]) -> Result<(), String> {
    let now = snapshot(path)?;
    if now != *before {
        return Err(format!(
            "{} changed meanwhile; your edit is kept, try again",
            path.display()
        ));
    }
    if let Some(dir) = path.parent() {
        crate::sys::fs::ensure_private_dir(dir).map_err(|e| e.to_string())?;
    }
    let written = if before.bytes.is_none() {
        create_atomic(path, bytes)
    } else {
        write_atomic(path, bytes)
    };
    written.map_err(|e| e.to_string())?;
    match read_private(path, FILE_MAX) {
        Ok(Some(back)) if back == bytes => Ok(()),
        _ => Err(format!("{} did not read back as written", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_key_replaces_adds_and_keeps_the_rest() {
        let text = "# mine\n[hub]\nlog_level = \"warn\" # loud\n\n[jobs.x]\nkind = \"once\"\n";
        let out = set_key(text, "hub", "log_level", "debug");
        assert_eq!(
            out,
            "# mine\n[hub]\nlog_level = \"debug\"\n\n[jobs.x]\nkind = \"once\"\n"
        );
        let out = set_key(&out, "omp", "path", "/opt/omp \"x\"");
        assert!(
            out.ends_with("[omp]\npath = \"/opt/omp \\\"x\\\"\"\n"),
            "{out}"
        );
        let out = set_key("[omp]\n# keep\n\n[hub]\n", "omp", "path", "omp");
        assert_eq!(out, "[omp]\n# keep\npath = \"omp\"\n\n[hub]\n");
    }

    #[test]
    fn snapshot_refuses_a_group_readable_file_like_the_hub() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("cataly-setup-perm-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        assert!(std::fs::write(&path, "[hub]\n").is_ok());
        assert!(std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).is_ok());
        let err = snapshot(&path).err().unwrap_or_default();
        assert!(err.contains("chmod go="), "{err}");
        assert!(read_private(&path, FILE_MAX).is_err());
        assert!(std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).is_ok());
        assert!(snapshot(&path).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
