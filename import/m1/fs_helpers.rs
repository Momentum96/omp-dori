//! Bounded reads, owner-only directories, temp-then-rename writes and removal.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

/// `path` with `suffix` appended to its last component.
pub fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}

/// Reads a whole file, failing above `limit` bytes.
pub fn read_limited(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(std::io::Error::other(format!(
            "{} exceeds its size limit",
            path.display()
        )));
    }
    Ok(bytes)
}

/// Creates `path` and its missing parents with mode 0700.
pub fn private_dirs(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("creating {}", path.display()))
}

/// Writes `text` to a fresh owner-only temporary sibling and renames it over `path`.
pub fn replace_file(path: &Path, temporary: &Path, text: &str) -> Result<()> {
    if let Err(error) = fs::remove_file(temporary)
        && error.kind() != ErrorKind::NotFound
    {
        return Err(error.into());
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(temporary)
        .with_context(|| format!("creating {}", temporary.display()))?;
    file.write_all(text.as_bytes())?;
    drop(file);
    fs::rename(temporary, path).with_context(|| format!("replacing {}", path.display()))
}

/// Removes a file or a directory tree; a missing path is not an error.
pub fn remove_any(path: &Path) -> Result<()> {
    let result = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) => Err(error),
    };
    match result {
        Err(error) if error.kind() != ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}
