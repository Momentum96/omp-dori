//! The OMP boundary (DESIGN 3): the rpc-ui transport, the probe with its minimum version, and the
//! usage and lease readers. This milestone starts no session; the probe result is cached here, shown
//! by health and `status`, and re-taken when the configured executable changes on disk.

pub(crate) mod probe;
pub(crate) mod rpc;
pub(crate) mod usage;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use tokio::sync::watch;

use crate::config::Config;
use crate::log;

/// Oldest OMP the contract suite passed on (DESIGN 3.4).
pub(crate) const MINIMUM: Version = Version(19, 1, 1);
/// Newest OMP the contract suite passed on; a newer major is allowed and reported as untested.
pub(crate) const TESTED: Version = Version(19, 1, 1);
/// How often the executable's identity is compared with the probed one.
const RECHECK: Duration = Duration::from_secs(60);

/// An OMP release number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Version(pub(crate) u64, pub(crate) u64, pub(crate) u64);

impl Version {
    /// Parses `omp/19.1.1` (or `omp 19.1.1`); a pre-release suffix is ignored.
    pub(crate) fn parse_output(text: &str) -> Option<Self> {
        let rest = text
            .trim()
            .strip_prefix("omp/")
            .or_else(|| text.trim().strip_prefix("omp "))?;
        let core = rest.split(['-', '+', ' ']).next()?;
        let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
        let version = Self(parts.next()??, parts.next()??, parts.next()??);
        parts.next().is_none().then_some(version)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Environment for OMP children beyond the runner's pass-through list: notifications off, and the
/// documented profile overrides when the hub itself was given them (isolated tests, contract suite).
pub(crate) fn child_env() -> Vec<(&'static str, String)> {
    let mut env = vec![("PI_NOTIFICATIONS", "off".to_string())];
    for key in ["PI_CODING_AGENT_DIR", "PI_CONFIG_DIR"] {
        if let Ok(value) = std::env::var(key) {
            env.push((key, value));
        }
    }
    env
}

/// Resolves the configured executable: an absolute path as it is, a bare name on `PATH`.
pub(crate) fn resolve(setting: &str) -> Option<PathBuf> {
    let candidate = Path::new(setting);
    if candidate.is_absolute() {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(setting))
        .find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// What identifies the executable on disk.
type Identity = (PathBuf, u64, Option<SystemTime>);

fn identity(path: &Path) -> Option<Identity> {
    let meta = std::fs::metadata(path).ok()?;
    Some((path.to_path_buf(), meta.len(), meta.modified().ok()))
}

/// The hub's view of OMP: the last probe report and the one usage reader.
pub(crate) struct Omp {
    report: watch::Sender<Option<Arc<probe::Report>>>,
    path: Mutex<Option<PathBuf>>,
    pub(crate) usage: usage::Reader,
}

impl Omp {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            report: watch::channel(None).0,
            path: Mutex::new(None),
            usage: usage::Reader::default(),
        })
    }

    /// The latest probe report, if a probe finished.
    pub(crate) fn report(&self) -> Option<Arc<probe::Report>> {
        self.report.borrow().clone()
    }

    /// The executable resolved at the last check.
    pub(crate) fn path(&self) -> Option<PathBuf> {
        self.path
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Probes at start, then re-probes when the configured setting or the executable's path, size
    /// or modification time changes; one metadata read per minute otherwise. Session starts never
    /// spawn a probe.
    pub(crate) async fn watch(
        self: Arc<Self>,
        mut config: watch::Receiver<Arc<Config>>,
        stderr_log: PathBuf,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), String> {
        let mut probed: Option<(String, Option<Identity>)> = None;
        loop {
            let setting = config.borrow_and_update().omp.path.clone();
            let resolved = match &probed {
                // A bare name is resolved on PATH once; afterwards only its file is watched.
                Some((s, Some((p, _, _)))) if *s == setting => Some(p.clone()),
                _ => resolve(&setting),
            };
            let now = resolved.as_deref().and_then(identity);
            if probed.as_ref() != Some(&(setting.clone(), now.clone())) {
                self.path
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone_from(&resolved);
                let report = probe::probe(resolved.as_deref(), Some(&stderr_log)).await;
                log::info("omp probed", &[("result", &report.summary())]);
                self.report.send_replace(Some(Arc::new(report)));
                probed = Some((setting, now));
            }
            tokio::select! {
                () = tokio::time::sleep(RECHECK) => {}
                changed = config.changed() => if changed.is_err() {
                    return Ok(());
                },
                _ = shutdown.changed() => {}
            }
            if *shutdown.borrow() {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_and_compare() {
        assert_eq!(
            Version::parse_output("omp/19.1.1\n"),
            Some(Version(19, 1, 1))
        );
        assert_eq!(
            Version::parse_output("omp 20.0.3-beta.1"),
            Some(Version(20, 0, 3))
        );
        assert_eq!(Version::parse_output("omp/19.1"), None);
        assert_eq!(Version::parse_output("node 19.1.1"), None);
        assert_eq!(Version::parse_output("omp/19.1.1.4"), None);
        assert!(Version(19, 0, 0) < MINIMUM && Version(19, 2, 0) > MINIMUM);
        assert_eq!(MINIMUM.to_string(), "19.1.1");
    }

    #[test]
    fn resolves_absolute_paths_and_names_on_path() {
        assert!(resolve("/nonexistent/omp").is_none());
        assert!(resolve("sh").is_some());
        assert!(resolve("cataly-no-such-tool").is_none());
    }
}
