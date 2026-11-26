//! The hub's single-instance guard: an exclusive OS lock on `<state>/hub.lock` (DESIGN 2.1).

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use super::fs::{FsError, check_private_file};

/// Held for the hub's lifetime; the OS releases the lock when the process ends.
#[derive(Debug)]
pub(crate) struct HubLock {
    _file: File,
}

/// Result of trying to become the one hub.
#[derive(Debug)]
pub(crate) enum LockOutcome {
    Acquired(HubLock),
    /// Another process holds the lock; `pid` is what it wrote into the file, when readable.
    Held {
        pid: Option<u32>,
    },
}

/// Tries to take the lock without waiting. On success the file holds this process's PID.
pub(crate) fn acquire(path: &Path) -> Result<LockOutcome, FsError> {
    let io = |source| FsError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)
        .map_err(io)?;
    let meta = file.metadata().map_err(io)?;
    check_private_file(path, &meta)?;
    match file.try_lock() {
        Ok(()) => {
            file.set_len(0).map_err(io)?;
            file.seek(SeekFrom::Start(0)).map_err(io)?;
            file.write_all(format!("{}\n", std::process::id()).as_bytes())
                .map_err(io)?;
            file.sync_all().map_err(io)?;
            Ok(LockOutcome::Acquired(HubLock { _file: file }))
        }
        Err(TryLockError::WouldBlock) => {
            let mut text = String::new();
            let pid = (&file)
                .take(32)
                .read_to_string(&mut text)
                .ok()
                .and_then(|_| text.trim().parse().ok());
            Ok(LockOutcome::Held { pid })
        }
        Err(TryLockError::Error(e)) => Err(io(e)),
    }
}

/// Whether a process holds the lock at `path` now. The probe takes a shared lock for an instant and
/// changes nothing; a missing file is a `NotFound` error.
pub(crate) fn is_held(path: &Path) -> std::io::Result<bool> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)?;
    match file.try_lock_shared() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::fs::tests::scratch;

    #[test]
    fn second_acquire_reports_the_holder() {
        let dir = scratch("lock");
        let path = dir.join("hub.lock");
        assert_eq!(
            is_held(&path).map_err(|e| e.kind()),
            Err(std::io::ErrorKind::NotFound)
        );
        let first = acquire(&path).unwrap();
        assert!(matches!(first, LockOutcome::Acquired(_)));
        assert!(is_held(&path).unwrap());
        match acquire(&path).unwrap() {
            LockOutcome::Held { pid } => assert_eq!(pid, Some(std::process::id())),
            LockOutcome::Acquired(_) => panic!("lock taken twice"),
        }
        drop(first);
        // Another test thread that forks a child at this moment holds a copy of the lock's
        // descriptor until the child's exec closes it, so the lock may stay taken for a few
        // milliseconds after the drop.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match acquire(&path).unwrap() {
                LockOutcome::Acquired(_) => break,
                LockOutcome::Held { .. } if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                LockOutcome::Held { .. } => panic!("the lock stayed held after its drop"),
            }
        }
    }
}
