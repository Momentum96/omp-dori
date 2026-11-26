//! Remote workers (docs/design/workers.md): another machine, or another OS user, that runs lanes for
//! this hub over OpenSSH. The hub keeps the relays, the lead, the store and every job; a worker runs
//! the same binary in worker mode (`cataly worker serve`), started by its sshd's forced command for
//! each hub connection.
//!
//! - [`mux`]: frames, channels, credit and heartbeat over the one SSH stream.
//! - [`serve`]: the worker side: the forced-command entry and the worker protocol's methods.
//! - [`link`]: the hub side: the `ssh` child, the channel-0 client, and the hub's worker table.
//! - [`enrol`]: keys, the `authorized_keys` line, host-key pins and `tailscale ip`.

pub(crate) mod enrol;
pub(crate) mod link;
pub(crate) mod mux;
pub(crate) mod serve;

use serde::{Deserialize, Serialize};

/// Worker protocol major (design 2.2, 2.8): fields are only added within a major.
pub(crate) const PROTOCOL: u32 = 1;
/// The command the hub asks for. It does not exist: a server that applies the forced command
/// ignores it, one that runs it instead fails with "not found" (design 2.2).
pub(crate) const MARKER: &str = "cataly-worker-v1";
/// Longest channel-0 line: a `read` or `write` carries up to 8 MiB as base64.
pub(crate) const LINE_MAX: usize = 12 * 1024 * 1024;
/// Largest file `read` returns and `write` accepts.
pub(crate) const FILE_MAX: u64 = 8 * 1024 * 1024;
/// Remote lanes one worker runs at once, at most.
pub(crate) const LANES_MAX: u32 = 8;

/// Worker protocol methods (design 2.2).
pub(crate) mod method {
    pub(crate) const HELLO: &str = "hello";
    pub(crate) const STATUS: &str = "status";
    pub(crate) const EXEC: &str = "exec";
    pub(crate) const SPAWN: &str = "spawn";
    pub(crate) const SIGNAL: &str = "signal";
    pub(crate) const READ: &str = "read";
    pub(crate) const STAT: &str = "stat";
    pub(crate) const WRITE: &str = "write";
    pub(crate) const FORGET: &str = "forget";
    /// A delivery marker in a session file, searched by the same parser on the worker: the hub
    /// confirms every delivery this way, and moving an 8 MiB tail each time would not be light.
    pub(crate) const FIND: &str = "find";
    /// The resume check of a saved session file, by the same parser on the worker.
    pub(crate) const VERIFY: &str = "verify";
}

/// Where per-machine work runs (design 2.1): here, or on a worker over its stream. Parsers stay
/// where their callers are; the place only runs bounded commands and moves bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Place {
    Local,
    Worker(String),
}

impl Place {
    pub(crate) fn of(worker: Option<&str>) -> Self {
        worker.map_or(Self::Local, |w| Self::Worker(w.to_string()))
    }

    /// `hub`, or the worker's name: where a check ran.
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Local => "hub",
            Self::Worker(w) => w,
        }
    }

    /// One bounded command with captured output, as [`crate::sys::command::run`] runs it here.
    pub(crate) async fn run(
        &self,
        spec: &crate::sys::command::Spec<'_>,
    ) -> Result<crate::sys::command::Finished, String> {
        match self {
            Self::Local => crate::sys::command::run(spec)
                .await
                .map_err(|e| e.to_string()),
            Self::Worker(name) => {
                let link = link::connect(name).await?;
                let cwd = spec.cwd.map(|c| c.display().to_string());
                link.exec(spec.argv, cwd.as_deref(), spec.env, spec.timeout)
                    .await
            }
        }
    }

    /// A file's bytes, at most `max`.
    pub(crate) async fn read(&self, path: &std::path::Path, max: u64) -> Result<Vec<u8>, String> {
        match self {
            Self::Local => {
                use std::io::Read as _;
                let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
                let mut bytes = Vec::new();
                file.take(max)
                    .read_to_end(&mut bytes)
                    .map_err(|e| e.to_string())?;
                Ok(bytes)
            }
            Self::Worker(name) => {
                link::connect(name)
                    .await?
                    .read(&path.display().to_string(), max)
                    .await
            }
        }
    }
}

/// The prefix of a path on a worker as the hub records it: `worker:<name>:/abs/path`.
const REMOTE: &str = "worker:";

/// A worker's path as the hub records it.
pub(crate) fn remote_path(worker: &str, path: &str) -> String {
    format!("{REMOTE}{worker}:{path}")
}

/// The worker and its path, when `path` is a worker's.
pub(crate) fn split_remote(path: &std::path::Path) -> Option<(&str, &str)> {
    let text = path.to_str()?.strip_prefix(REMOTE)?;
    let (worker, rest) = text.split_once(':')?;
    rest.starts_with('/').then_some((worker, rest))
}

/// The `hello` answer: what the worker is and whether it can run lanes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Hello {
    /// Worker protocol major.
    pub(crate) protocol: u32,
    /// The bridge protocol of the worker's own `cataly`.
    pub(crate) bridge: u64,
    /// The worker's product version.
    pub(crate) version: String,
    pub(crate) os: String,
    pub(crate) arch: String,
    /// `<state>/worker/<hub id>` on the worker.
    pub(crate) root: String,
    /// `omp --version`'s first line, or why it failed.
    pub(crate) omp: Probed,
    /// `git --version`'s first line, or why it failed.
    pub(crate) git: Probed,
    /// Why OMP would not load the worker's bridge, if it would not.
    pub(crate) bridge_link: Option<String>,
    /// The bridge socket `CATALY_SOCKET` names for the worker's sessions.
    pub(crate) socket: String,
}

/// A tool's version line, or why it could not be read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Probed {
    pub(crate) version: Option<String>,
    pub(crate) error: Option<String>,
}

impl Probed {
    /// The version, or the failure as text.
    pub(crate) fn text(&self) -> String {
        match (&self.version, &self.error) {
            (Some(v), _) => v.clone(),
            (None, Some(e)) => format!("unavailable: {e}"),
            (None, None) => "not reported".into(),
        }
    }
}

impl Hello {
    /// Why this worker cannot work with this hub; `None` when it is compatible (design 2.8).
    pub(crate) fn incompatible(&self, name: &str) -> Option<String> {
        if self.protocol != PROTOCOL {
            return Some(format!(
                "incompatible: the cataly on {name} speaks worker protocol {} where this hub speaks \
                 {PROTOCOL}; run cataly update on {name}",
                self.protocol
            ));
        }
        if self.bridge != crate::hub::bridge::PROTOCOL {
            return Some(format!(
                "incompatible: the cataly on {name} speaks bridge protocol {} where this hub speaks \
                 {}; run cataly update on {name}",
                self.bridge,
                crate::hub::bridge::PROTOCOL
            ));
        }
        None
    }
}
