//! Line-preserving edits of the config file, shared by `cataly setup` and the hub: one setting's
//! line is replaced or added and everything else (comments, unknown keys, order) is kept. A write
//! snapshots the file (identity and bytes), validates the result as `cataly config check` does,
//! re-checks the snapshot just before an atomic replace (or a create that never overwrites), and
//! reads the file back, so a hand edit made meanwhile is never clobbered.

use std::fmt::Write as _;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

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
    if let Some(dir) = path.parent() {
        crate::sys::fs::ensure_private_dir(dir).map_err(|e| e.to_string())?;
    }
    publish_in_place(path, before, bytes)
}

/// [`publish`] leaving the folder as it is, for the hub's start-up clean-up.
fn publish_in_place(path: &Path, before: &Snapshot, bytes: &[u8]) -> Result<(), String> {
    let now = snapshot(path)?;
    if now != *before {
        return Err(format!(
            "{} changed meanwhile; your edit is kept, try again",
            path.display()
        ));
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

/// Removes `[section]` with its lines, up to the next header; every other byte is kept. Comment
/// lines just above the next header stay with that header.
pub(crate) fn remove_section(text: &str, section: &str) -> String {
    let header = format!("[{section}]");
    let mut inside = false;
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let bare = line.split('#').next().unwrap_or_default().trim();
        if bare.starts_with('[') {
            inside = bare == header;
        }
        if !inside {
            out.push_str(line);
        }
    }
    let trimmed = out.trim_end();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

/// Removes `key`'s line from `[section]`, keeping every other byte as it is; `None` when the section
/// has no such line. The counterpart of [`set_rendered`] for a key that went away.
pub(crate) fn remove_key(text: &str, section: &str, key: &str) -> Option<String> {
    let header = format!("[{section}]");
    let (mut inside, mut removed) = (false, false);
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let bare = line.split('#').next().unwrap_or_default().trim();
        if bare.starts_with('[') {
            inside = bare == header;
        } else if inside
            && !removed
            && line
                .trim_start()
                .strip_prefix(key)
                .is_some_and(|rest| rest.trim_start().starts_with('='))
        {
            removed = true;
            continue;
        }
        out.push_str(line);
    }
    removed.then_some(out)
}

/// The copy of the config file kept before the hub removed obsolete lines from it.
pub(crate) fn before_copy(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".before-0.2.4");
    PathBuf::from(name)
}

/// What the hub's start-up clean-up of the config file did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Cleanup {
    /// Nothing to remove, or no file that parses (the start-up check reports that).
    Nothing,
    /// The lines of these settings are gone; the original is kept at `backup`.
    Removed { keys: Vec<String>, backup: PathBuf },
    /// The lines stay, ignored; why the file was not changed.
    Kept { keys: Vec<String>, why: String },
}

/// Removes the obsolete `hold = …` line from every `kind = "quota"` job section of the config file,
/// keeping every other byte and the file's 0600 mode, and writes it atomically. The original is
/// kept next to it first (an older copy there is left as it is). A read-only file, or one that
/// cannot be written, keeps its lines.
pub(crate) fn remove_obsolete_hold(path: &Path) -> Cleanup {
    let Ok(before) = snapshot(path) else {
        return Cleanup::Nothing;
    };
    let Some(text) = before
        .bytes
        .as_deref()
        .and_then(|b| std::str::from_utf8(b).ok())
    else {
        return Cleanup::Nothing;
    };
    let Ok(parsed) = parse(text) else {
        return Cleanup::Nothing;
    };
    let keys = parsed.obsolete_hold();
    if keys.is_empty() {
        return Cleanup::Nothing;
    }
    let kept = |why: String| Cleanup::Kept {
        keys: keys.clone(),
        why,
    };
    if before.identity.is_some_and(|i| i.2 & 0o200 == 0) {
        return kept(format!("{} is read-only", path.display()));
    }
    let mut out = text.to_string();
    for key in &keys {
        let section = key.strip_suffix(".hold").unwrap_or(key);
        match remove_key(&out, section, "hold") {
            Some(next) => out = next,
            None => return kept(format!("{key} is not a line of its own under [{section}]")),
        }
    }
    if !parse(&out).is_ok_and(|c| c.obsolete_hold().is_empty()) {
        return kept("the file without those lines would not read as expected".into());
    }
    let backup = before_copy(path);
    if !backup.exists()
        && let Err(e) = create_atomic(&backup, text.as_bytes())
    {
        return kept(e.to_string());
    }
    match publish_in_place(path, &before, out.as_bytes()) {
        Ok(()) => Cleanup::Removed { keys, backup },
        Err(why) => Cleanup::Kept { keys, why },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    /// A 0.2.3 config with `hold` and comments: the clean-up removes that one line, keeps every other
    /// byte and the mode, keeps the original next to it, and a second run changes nothing.
    #[test]
    fn the_obsolete_hold_line_goes_and_nothing_else_changes() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = crate::sys::fs::tests::scratch("hold-clean");
        let path = dir.join("config.toml");
        let old = "# mine\r\n[hub]\nlog_level = \"warn\" # loud\n\n\
                   # The quota job\n[jobs.quota] # alerts\nkind = \"quota\"\nlevels = [70, 85, 95]\n\
                   hold = 85 # the gate\nevery = \"5m\"\n\n[jobs.other]\nkind = \"quota\"\n  hold=90\n\n\
                   [omp]\npath = \"omp\"\n";
        crate::sys::fs::create_atomic(&path, old.as_bytes()).unwrap();
        let Cleanup::Removed { keys, backup } = remove_obsolete_hold(&path) else {
            panic!("not removed");
        };
        assert_eq!(keys, ["jobs.other.hold", "jobs.quota.hold"]);
        let new = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            new,
            old.replace("hold = 85 # the gate\n", "")
                .replace("  hold=90\n", "")
        );
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), old);
        assert_eq!(backup, dir.join("config.toml.before-0.2.4"));
        for p in [&path, &backup] {
            let mode = std::fs::metadata(p).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{}", p.display());
        }
        assert_eq!(
            config::parse(&new).unwrap().obsolete_hold(),
            Vec::<String>::new()
        );
        assert_eq!(remove_obsolete_hold(&path), Cleanup::Nothing);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            new,
            "twice: no change"
        );
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), old);
    }

    /// A read-only config keeps its line, ignored, and still loads with one notice.
    #[test]
    fn a_read_only_config_keeps_the_line_and_still_loads() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = crate::sys::fs::tests::scratch("hold-ro");
        let path = dir.join("config.toml");
        let old = "[jobs.q]\nkind = \"quota\"\nhold = 85\n";
        crate::sys::fs::create_atomic(&path, old.as_bytes()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        let Cleanup::Kept { keys, why } = remove_obsolete_hold(&path) else {
            panic!("changed a read-only file");
        };
        assert_eq!(keys, ["jobs.q.hold"]);
        assert!(why.contains("read-only"), "{why}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
        assert!(!dir.join("config.toml.before-0.2.4").exists());
        let loaded = config::load(&path).unwrap();
        assert_eq!(loaded.obsolete_hold(), ["jobs.q.hold"]);
        assert!(
            config::obsolete_notice("jobs.q.hold")
                .starts_with("jobs.q.hold no longer does anything")
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn remove_key_touches_its_section_only() {
        let text = "[a]\nhold = 1\n[b]\nholder = 2\nhold = 3\n";
        assert_eq!(
            remove_key(text, "b", "hold").as_deref(),
            Some("[a]\nhold = 1\n[b]\nholder = 2\n")
        );
        assert_eq!(remove_key(text, "c", "hold"), None);
        assert_eq!(remove_key("[b]\nholder = 2\n", "b", "hold"), None);
    }

    /// The hold clean-up and a watch-picker write share the one writer: a pick after the clean-up
    /// keeps the line gone and the backup as the original, and a pick whose snapshot predates the
    /// clean-up is refused instead of bringing the `hold` line back.
    #[test]
    fn the_hold_clean_up_and_a_channel_pick_do_not_undo_each_other() {
        let dir = crate::sys::fs::tests::scratch("hold-pick");
        let path = dir.join("config.toml");
        let head = "[discord]\nguild = \"100000000000000001\"\nowner = \"100000000000000002\"\n";
        let rest = "# mine\n\n[jobs.q]\nkind = \"quota\"\n";
        let (one, two) = ("100000000000000003", "100000000000000004");
        let old = format!("{head}channels = [\"{one}\"]\n{rest}hold = 85\n");
        crate::sys::fs::create_atomic(&path, old.as_bytes()).unwrap();
        let stale = snapshot(&path).unwrap();
        let Cleanup::Removed { backup, .. } = remove_obsolete_hold(&path) else {
            panic!("not removed");
        };
        let picked = set_rendered(&old, "discord", "channels", &string_list(&[one, two]));
        let refused = publish(&path, &stale, picked.as_bytes()).unwrap_err();
        assert!(refused.contains("changed meanwhile"), "{refused}");
        let written = edit(&path, "", |text| {
            Ok(set_rendered(
                text,
                "discord",
                "channels",
                &string_list(&[one, two]),
            ))
        })
        .unwrap();
        assert_eq!(
            written,
            format!("{head}channels = [\"{one}\", \"{two}\"]\n{rest}")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), written);
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), old);
        assert_eq!(remove_obsolete_hold(&path), Cleanup::Nothing);
    }

    #[test]
    fn remove_section_takes_out_one_table() {
        let text = "[hub]\nlog_level = \"info\"\n\n[workers.a]\ndestination = \"d@a\"\n\n[workers.b]\ndestination = \"d@b\"\n";
        assert_eq!(
            remove_section(text, "workers.a"),
            "[hub]\nlog_level = \"info\"\n\n[workers.b]\ndestination = \"d@b\"\n"
        );
        assert_eq!(
            remove_section(text, "workers.b"),
            "[hub]\nlog_level = \"info\"\n\n[workers.a]\ndestination = \"d@a\"\n"
        );
        assert_eq!(remove_section(text, "workers.c"), text);
        assert_eq!(remove_section("[workers.a]\nx = 1\n", "workers.a"), "");
    }

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
