//! Where Cataly keeps its files (DESIGN 11.2 and 8).

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

/// Longest socket path the platform accepts, in bytes without the terminating NUL.
#[cfg(target_os = "macos")]
const SOCKET_PATH_MAX: usize = 103;
#[cfg(not(target_os = "macos"))]
const SOCKET_PATH_MAX: usize = 107;

/// Every path the hub and the command line use.
#[derive(Debug, Clone)]
pub(crate) struct Paths {
    /// `$XDG_CONFIG_HOME/cataly/config.toml` or `~/.config/cataly/config.toml`.
    pub(crate) config_file: PathBuf,
    /// `$XDG_STATE_HOME/cataly` or `~/.local/state/cataly`.
    pub(crate) state_dir: PathBuf,
    /// `<state>/logs`: the hub log and the event log.
    pub(crate) logs_dir: PathBuf,
    /// `<state>/files`: job run output and spilled bodies.
    pub(crate) files_dir: PathBuf,
    /// `<state>/backup`: store copies taken before a migration.
    pub(crate) backup_dir: PathBuf,
    /// The directory holding the control socket.
    pub(crate) runtime_dir: PathBuf,
    /// `<runtime>/hub.sock`.
    pub(crate) socket: PathBuf,
}

/// The environment does not allow a usable layout.
#[derive(Debug)]
pub(crate) enum PathError {
    NoHome,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoHome => f.write_str("HOME is not set to an absolute path"),
        }
    }
}

impl Paths {
    /// Resolves the layout from the process environment.
    pub(crate) fn from_env() -> Result<Self, PathError> {
        Self::resolve(|key| std::env::var_os(key), super::own_uid())
    }

    /// Resolves the layout from an environment lookup; XDG values that are not absolute are ignored,
    /// as the XDG base directory rules require.
    pub(crate) fn resolve(
        get: impl Fn(&str) -> Option<OsString>,
        uid: u32,
    ) -> Result<Self, PathError> {
        let absolute = |key: &str| get(key).map(PathBuf::from).filter(|p| p.is_absolute());
        let home = absolute("HOME").ok_or(PathError::NoHome)?;
        let config_file = absolute("XDG_CONFIG_HOME")
            .unwrap_or_else(|| home.join(".config"))
            .join("cataly")
            .join("config.toml");
        let state_dir = absolute("XDG_STATE_HOME")
            .unwrap_or_else(|| home.join(".local").join("state"))
            .join("cataly");
        let runtime_dir = absolute("XDG_RUNTIME_DIR")
            .map(|dir| dir.join("cataly"))
            .filter(|dir| socket_fits(dir))
            .unwrap_or_else(|| PathBuf::from(format!("/tmp/cataly-{uid}")));
        Ok(Self::layout(config_file, state_dir, runtime_dir))
    }

    /// The same layout with the state directory at its real location (symlinks resolved; it must
    /// exist). The hub works from this, so every state path it records or hands to OMP is the one
    /// `canonicalize` reports: the resume check refuses a recorded transcript path that differs
    /// from its real location, and on macOS the temporary directories (`/tmp`, `/var/folders`) sit
    /// behind a symlink into `/private`.
    pub(crate) fn real_state(&self) -> std::io::Result<Self> {
        let state_dir = std::fs::canonicalize(&self.state_dir)?;
        Ok(Self::layout(
            self.config_file.clone(),
            state_dir,
            self.runtime_dir.clone(),
        ))
    }

    fn layout(config_file: PathBuf, state_dir: PathBuf, runtime_dir: PathBuf) -> Self {
        Self {
            config_file,
            logs_dir: state_dir.join("logs"),
            files_dir: state_dir.join("files"),
            backup_dir: state_dir.join("backup"),
            socket: runtime_dir.join("hub.sock"),
            runtime_dir,
            state_dir,
        }
    }

    /// `<state>/hub.lock`, the single-instance guard.
    pub(crate) fn lock_file(&self) -> PathBuf {
        self.state_dir.join("hub.lock")
    }

    /// `<state>/cataly.db`, the store.
    pub(crate) fn db_file(&self) -> PathBuf {
        self.state_dir.join("cataly.db")
    }

    /// `<state>/stall-exit`, written by the stall detector before it ends the process.
    pub(crate) fn stall_marker(&self) -> PathBuf {
        self.state_dir.join("stall-exit")
    }

    /// `<logs>/hub.log`.
    pub(crate) fn hub_log(&self) -> PathBuf {
        self.logs_dir.join("hub.log")
    }

    /// `<logs>/events.jsonl`, the owner-readable event log.
    pub(crate) fn event_log(&self) -> PathBuf {
        self.logs_dir.join("events.jsonl")
    }
}

fn socket_fits(dir: &Path) -> bool {
    dir.join("hub.sock").as_os_str().len() <= SOCKET_PATH_MAX
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn defaults_follow_home() {
        let p = Paths::resolve(env(&[("HOME", "/home/o")]), 1000).unwrap();
        assert_eq!(
            p.config_file,
            PathBuf::from("/home/o/.config/cataly/config.toml")
        );
        assert_eq!(p.state_dir, PathBuf::from("/home/o/.local/state/cataly"));
        assert_eq!(p.socket, PathBuf::from("/tmp/cataly-1000/hub.sock"));
        assert_eq!(
            p.db_file(),
            PathBuf::from("/home/o/.local/state/cataly/cataly.db")
        );
    }

    #[test]
    fn xdg_overrides_and_relative_values_ignored() {
        let p = Paths::resolve(
            env(&[
                ("HOME", "/home/o"),
                ("XDG_CONFIG_HOME", "/c"),
                ("XDG_STATE_HOME", "relative"),
                ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ]),
            1000,
        )
        .unwrap();
        assert_eq!(p.config_file, PathBuf::from("/c/cataly/config.toml"));
        assert_eq!(p.state_dir, PathBuf::from("/home/o/.local/state/cataly"));
        assert_eq!(p.socket, PathBuf::from("/run/user/1000/cataly/hub.sock"));
    }

    #[test]
    fn long_runtime_dir_falls_back_to_tmp() {
        let long = format!("/{}", "x".repeat(120));
        let p = Paths::resolve(env(&[("HOME", "/h"), ("XDG_RUNTIME_DIR", &long)]), 7).unwrap();
        assert_eq!(p.runtime_dir, PathBuf::from("/tmp/cataly-7"));
    }

    #[test]
    fn missing_home_is_refused() {
        assert!(Paths::resolve(env(&[]), 1).is_err());
    }

    #[test]
    fn real_state_resolves_a_symlinked_state_directory() {
        let scratch = crate::sys::fs::tests::scratch("paths");
        let real = std::fs::canonicalize(&*scratch).unwrap().join("real");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, scratch.join("link")).unwrap();
        let home = scratch.join("home");
        let state = scratch.join("link");
        let p = Paths::resolve(
            env(&[
                ("HOME", home.to_str().unwrap()),
                ("XDG_STATE_HOME", state.to_str().unwrap()),
            ]),
            1,
        )
        .unwrap();
        std::fs::create_dir(&p.state_dir).unwrap();
        let r = p.real_state().unwrap();
        assert_eq!(r.state_dir, real.join("cataly"));
        assert_eq!(r.logs_dir, real.join("cataly/logs"));
        assert_eq!(r.db_file(), real.join("cataly/cataly.db"));
        assert_eq!((r.config_file, r.socket), (p.config_file, p.socket));
    }
}
