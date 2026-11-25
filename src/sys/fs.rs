//! Owner-only files and directories, and the one atomic owner-only file write (DESIGN 7.2, HOST-37).

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::ErrorClass;

/// A file-system operation failed or found a file that is not safely the owner's.
#[derive(Debug)]
pub(crate) enum FsError {
    Io { path: PathBuf, source: io::Error },
    NotPrivate { path: PathBuf, reason: &'static str },
    TooLarge { path: PathBuf, limit: u64 },
}

impl FsError {
    fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }

    pub(crate) fn class(&self) -> ErrorClass {
        match self {
            Self::Io { .. } => ErrorClass::Transient,
            Self::NotPrivate { .. } | Self::TooLarge { .. } => ErrorClass::Rejected,
        }
    }
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::NotPrivate { path, reason } => write!(f, "{}: refused, {reason}", path.display()),
            Self::TooLarge { path, limit } => {
                write!(f, "{}: refused, larger than {limit} bytes", path.display())
            }
        }
    }
}

impl std::error::Error for FsError {}

/// Creates `path` (and missing parents) and checks that the final directory is the owner's, not a
/// symlink and has mode 0700 or stricter. Every folder created here gets mode 0700 whatever the
/// umask says.
pub(crate) fn ensure_private_dir(path: &Path) -> Result<(), FsError> {
    create_private_dirs(path)?;
    check_private_dir(path)
}

/// [`ensure_private_dir`] for a folder that is Cataly's own by name (its config, secrets and state
/// folders): an existing one that is the owner's real directory but open to group or others (an
/// earlier run under a loose umask made it so) is tightened to 0700 instead of refused. Returns
/// whether it was tightened.
pub(crate) fn claim_private_dir(path: &Path) -> Result<bool, FsError> {
    create_private_dirs(path)?;
    let meta = fs::symlink_metadata(path).map_err(|e| FsError::io(path, e))?;
    let tighten = meta.is_dir()
        && !meta.file_type().is_symlink()
        && meta.uid() == super::own_uid()
        && meta.mode() & 0o077 != 0;
    if tighten {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|e| FsError::io(path, e))?;
    }
    check_private_dir(path)?;
    Ok(tighten)
}

/// Creates the missing folders of `path`, each with mode 0700 regardless of the umask.
fn create_private_dirs(path: &Path) -> Result<(), FsError> {
    let mut missing = Vec::new();
    let mut at = Some(path);
    while let Some(dir) = at.filter(|d| !d.as_os_str().is_empty()) {
        match fs::symlink_metadata(dir) {
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::NotFound => missing.push(dir),
            Err(e) => return Err(FsError::io(dir, e)),
        }
        at = dir.parent();
    }
    for dir in missing.into_iter().rev() {
        match fs::DirBuilder::new().mode(0o700).create(dir) {
            // The umask may have taken bits away: set the mode itself.
            Ok(()) => fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                .map_err(|e| FsError::io(dir, e))?,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(FsError::io(dir, e)),
        }
    }
    Ok(())
}

/// Checks that an existing directory is the owner's, not a symlink and has mode 0700 or stricter: the
/// hub's own, and what a client requires of the socket's directory before it connects (DESIGN 8).
pub(crate) fn check_private_dir(path: &Path) -> Result<(), FsError> {
    let meta = fs::symlink_metadata(path).map_err(|e| FsError::io(path, e))?;
    check_owner_only(path, &meta)?;
    if !meta.is_dir() {
        return Err(FsError::NotPrivate {
            path: path.to_path_buf(),
            reason: "not a directory",
        });
    }
    Ok(())
}

/// Checks that an existing file is a regular file of the owner with one link and mode 0600 or stricter.
pub(crate) fn check_private_file(path: &Path, meta: &fs::Metadata) -> Result<(), FsError> {
    check_owner_only(path, meta)?;
    if !meta.is_file() {
        return Err(FsError::NotPrivate {
            path: path.to_path_buf(),
            reason: "not a regular file",
        });
    }
    if meta.nlink() != 1 {
        return Err(FsError::NotPrivate {
            path: path.to_path_buf(),
            reason: "has more than one link",
        });
    }
    Ok(())
}

fn check_owner_only(path: &Path, meta: &fs::Metadata) -> Result<(), FsError> {
    let refuse = |reason| {
        Err(FsError::NotPrivate {
            path: path.to_path_buf(),
            reason,
        })
    };
    if meta.file_type().is_symlink() {
        return refuse("is a symlink");
    }
    if meta.uid() != super::own_uid() {
        return refuse("owned by another user");
    }
    if meta.mode() & 0o077 != 0 {
        return refuse("group or other users have access (chmod go= to fix)");
    }
    Ok(())
}

/// Reads an owner-only file of at most `limit` bytes. A missing file is `Ok(None)`.
pub(crate) fn read_private(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, FsError> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(nofollow())
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            // A symlink makes O_NOFOLLOW fail with ELOOP; report it as the refusal it is.
            if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(FsError::NotPrivate {
                    path: path.to_path_buf(),
                    reason: "is a symlink",
                });
            }
            return Err(FsError::io(path, e));
        }
    };
    let meta = file.metadata().map_err(|e| FsError::io(path, e))?;
    check_private_file(path, &meta)?;
    if meta.len() > limit {
        return Err(FsError::TooLarge {
            path: path.to_path_buf(),
            limit,
        });
    }
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| FsError::io(path, e))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(FsError::TooLarge {
            path: path.to_path_buf(),
            limit,
        });
    }
    Ok(Some(bytes))
}

/// Creates a new owner-only file; fails if it exists.
pub(crate) fn create_private(path: &Path) -> Result<File, FsError> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nofollow())
        .open(path)
        .map_err(|e| FsError::io(path, e))
}

/// Opens an owner-only file for appending, creating it with mode 0600 when missing.
pub(crate) fn open_append_private(path: &Path) -> Result<File, FsError> {
    let file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(nofollow())
        .open(path)
        .map_err(|e| FsError::io(path, e))?;
    let meta = file.metadata().map_err(|e| FsError::io(path, e))?;
    check_private_file(path, &meta)?;
    Ok(file)
}

/// Replaces `path` atomically with `bytes`: unique temporary file (0600) in the same directory, write,
/// fsync, rename over the target, fsync the directory.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), FsError> {
    publish(path, bytes, true)
}

/// Creates `path` atomically with `bytes` like [`write_atomic`], but fails when `path` exists: the
/// finished temporary file is hard-linked into place, which never overwrites.
pub(crate) fn create_atomic(path: &Path, bytes: &[u8]) -> Result<(), FsError> {
    publish(path, bytes, false)
}

fn publish(path: &Path, bytes: &[u8], replace: bool) -> Result<(), FsError> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let tmp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = create_private(&tmp)?;
        file.write_all(bytes).map_err(|e| FsError::io(&tmp, e))?;
        file.sync_all().map_err(|e| FsError::io(&tmp, e))?;
        if replace {
            fs::rename(&tmp, path).map_err(|e| FsError::io(path, e))?;
        } else {
            fs::hard_link(&tmp, path).map_err(|e| FsError::io(path, e))?;
            fs::remove_file(&tmp).map_err(|e| FsError::io(&tmp, e))?;
        }
        sync_dir(dir)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Flushes a directory's entries to disk.
pub(crate) fn sync_dir(dir: &Path) -> Result<(), FsError> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| FsError::io(dir, e))
}

/// Sets mode 0600 on a path this process just created (used for the socket).
pub(crate) fn set_owner_only(path: &Path) -> Result<(), FsError> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|e| FsError::io(path, e))
}

fn nofollow() -> i32 {
    rustix::fs::OFlags::NOFOLLOW.bits().cast_signed()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A unique, owner-only scratch directory under the system temp folder, removed with everything
    /// in it when the guard drops (also when a test fails: unwinding runs the drop).
    #[derive(Debug)]
    pub(crate) struct Scratch(PathBuf);

    impl std::ops::Deref for Scratch {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl AsRef<Path> for Scratch {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn scratch(tag: &str) -> Scratch {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cataly-unit-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        ensure_private_dir(&dir).unwrap();
        Scratch(dir)
    }

    /// Marks a test script executable by its owner only.
    pub(crate) fn make_executable(path: &Path) {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn atomic_create_never_overwrites() {
        let dir = scratch("fs-create");
        let path = dir.join("c.txt");
        create_atomic(&path, b"first").unwrap();
        assert!(create_atomic(&path, b"second").is_err());
        assert_eq!(read_private(&path, 16).unwrap().unwrap(), b"first");
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            1,
            "no temporary file left"
        );
    }

    #[test]
    fn atomic_write_is_owner_only_and_replaces() {
        let dir = scratch("fs");
        let path = dir.join("a.txt");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        let meta = fs::metadata(&path).unwrap();
        assert_eq!(meta.mode() & 0o777, 0o600);
        assert_eq!(read_private(&path, 16).unwrap().unwrap(), b"two");
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            1,
            "no temporary file left"
        );
    }

    #[test]
    fn loose_modes_symlinks_and_size_are_refused() {
        let dir = scratch("fs-refuse");
        let path = dir.join("loose");
        fs::write(&path, b"x").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            read_private(&path, 16),
            Err(FsError::NotPrivate { .. })
        ));
        let link = dir.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(matches!(
            read_private(&link, 16),
            Err(FsError::NotPrivate { .. })
        ));
        let big = dir.join("big");
        write_atomic(&big, &[0u8; 32]).unwrap();
        assert!(matches!(
            read_private(&big, 16),
            Err(FsError::TooLarge { .. })
        ));
        assert!(read_private(&dir.join("missing"), 16).unwrap().is_none());
        let open = dir.join("open");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ensure_private_dir(&open).is_err());
    }

    #[test]
    fn own_folders_are_created_and_tightened_to_0700() {
        let dir = scratch("fs-claim");
        let deep = dir.join("a/b/c");
        ensure_private_dir(&deep).unwrap();
        for d in [dir.join("a"), dir.join("a/b"), deep] {
            assert_eq!(fs::metadata(&d).unwrap().mode() & 0o777, 0o700, "{d:?}");
        }
        let loose = dir.join("loose");
        fs::create_dir(&loose).unwrap();
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(claim_private_dir(&loose).unwrap(), "tightened");
        assert_eq!(fs::metadata(&loose).unwrap().mode() & 0o777, 0o700);
        assert!(!claim_private_dir(&loose).unwrap(), "already private");
        let link = dir.join("link");
        std::os::unix::fs::symlink(&loose, &link).unwrap();
        assert!(
            claim_private_dir(&link).is_err(),
            "a symlink is never claimed"
        );
    }
}
