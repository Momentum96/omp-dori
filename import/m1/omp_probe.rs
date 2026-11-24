use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

use super::rpc_client::{Request, Rpc};

const CONTROL_DEADLINE: Duration = Duration::from_secs(20);

pub enum Target<'a> {
    Ephemeral,
    Fresh(&'a Path),
    Saved { file: &'a Path, id: &'a str },
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionState {
    session_id: String,
    session_file: Option<PathBuf>,
    is_streaming: bool,
    is_compacting: bool,
    queued_message_count: usize,
    is_settled: Option<bool>,
    #[serde(skip_serializing)]
    message_count: Option<usize>,
}

pub struct Observation {
    pub(crate) state: SessionState,
    pub(crate) protocol: u32,
    pub(crate) host_tools: bool,
}

pub struct Connection {
    pub(crate) rpc: Rpc,
    pub(crate) observation: Observation,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Identity {
    pub(crate) id: String,
    pub(crate) file: PathBuf,
}

impl SessionState {
    pub(crate) async fn identity(&self) -> Result<Identity> {
        let observed = self
            .session_file
            .as_deref()
            .filter(|file| file.is_absolute())
            .context("OMP did not return an absolute session file")?;
        let file = tokio::fs::canonicalize(observed).await?;
        ensure!(
            tokio::fs::metadata(&file).await?.is_file(),
            "OMP session is not a regular file"
        );
        Ok(Identity {
            id: self.session_id.clone(),
            file,
        })
    }

    pub(crate) async fn verify(&self, expected: &Identity) -> Result<()> {
        ensure!(
            self.session_id == expected.id,
            "OMP resumed a different session ID"
        );
        ensure!(
            self.identity().await?.file == expected.file,
            "OMP resumed a different session file"
        );
        Ok(())
    }
}

pub struct Executable<'a> {
    path: PathBuf,
    cwd: &'a Path,
    model: Option<&'a str>,
}

impl<'a> Executable<'a> {
    pub(crate) fn new(path: &Path, cwd: &'a Path, model: Option<&'a str>) -> Result<Self> {
        let path = if path.is_relative() && path.as_os_str() != path.file_name().unwrap_or_default()
        {
            std::env::current_dir()?.join(path)
        } else {
            path.to_owned()
        };
        Ok(Self { path, cwd, model })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.path);
        command.current_dir(self.cwd).kill_on_drop(true);
        command
    }

    pub(crate) fn in_dir<'b>(&'b self, cwd: &'b Path) -> Executable<'b> {
        Executable {
            path: self.path.clone(),
            cwd,
            model: self.model,
        }
    }

    pub(crate) async fn version(&self) -> Result<String> {
        let mut child = self
            .command()
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting {} --version", self.path.display()))?;
        let mut bytes = Vec::new();
        let mut output = child
            .stdout
            .take()
            .context("OMP version stdout is unavailable")?
            .take(4097);
        timeout(CONTROL_DEADLINE, async {
            output.read_to_end(&mut bytes).await?;
            ensure!(bytes.len() <= 4096, "OMP version output exceeded 4 KiB");
            let status = child.wait().await?;
            ensure!(status.success(), "OMP version probe exited with {status}");
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("OMP version probe timed out")??;
        let version = std::str::from_utf8(&bytes)?.trim();
        let version = version
            .strip_prefix("omp/")
            .or_else(|| version.strip_prefix("omp "))
            .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_graphic()))
            .context("configured executable did not identify itself as OMP")?;
        Ok(version.to_owned())
    }

    pub(crate) async fn inspect(&self, target: Target<'_>) -> Result<Observation> {
        let Connection { rpc, observation } = self.connect(target).await?;
        rpc.close().await?;
        Ok(observation)
    }

    pub(crate) async fn connect(&self, target: Target<'_>) -> Result<Connection> {
        let mut command = self.command();
        command.args([
            "--mode",
            "rpc-ui",
            "--no-extensions",
            "--no-skills",
            "--no-rules",
            "--no-context-files",
            "--no-tools",
            "--offline",
        ]);
        if let Some(model) = self.model {
            command.arg(format!("--model={model}"));
        }
        match target {
            Target::Ephemeral => {
                command.arg("--no-session");
            }
            Target::Fresh(dir) => {
                command.arg("--session-dir").arg(dir);
            }
            Target::Saved { file, .. } => {
                let mut arg = std::ffi::OsString::from("--resume=");
                arg.push(file);
                command.arg(arg);
            }
        }
        let mut rpc = Rpc::spawn(command)
            .with_context(|| format!("starting {} in RPC mode", self.path.display()))?;
        let observation = match timeout(CONTROL_DEADLINE, observe(&mut rpc, target)).await {
            Ok(result) => result,
            Err(elapsed) => Err(elapsed).context("OMP readiness/state deadline elapsed"),
        };
        match observation {
            Ok(observation) => Ok(Connection { rpc, observation }),
            Err(error) => match rpc.close().await {
                Ok(()) => Err(error),
                Err(exit_error) => Err(error.context(format!("OMP shutdown: {exit_error:#}"))),
            },
        }
    }
}

pub async fn session_state(rpc: &mut Rpc) -> Result<SessionState> {
    let state: SessionState = timeout(CONTROL_DEADLINE, rpc.call(Request::GetState))
        .await
        .context("OMP state deadline elapsed")??
        .context("OMP declined get_state")?;
    ensure!(
        !state.session_id.trim().is_empty(),
        "OMP returned an empty session ID"
    );
    ensure!(
        !state.is_streaming
            && !state.is_compacting
            && state.queued_message_count == 0
            && state.is_settled != Some(false),
        "OMP reported active or queued work during a lifecycle check"
    );
    Ok(state)
}

async fn observe(rpc: &mut Rpc, target: Target<'_>) -> Result<Observation> {
    let protocol = rpc.negotiate().await?;
    let mut state = session_state(rpc).await?;
    let host_tools = match target {
        Target::Ephemeral => {
            ensure!(
                state.session_file.is_none(),
                "OMP opened a persisted session during doctor"
            );
            // An empty tool list probes the capability without exposing any actions.
            rpc.probe_host_tools().await?
        }
        Target::Saved { file, id } => {
            state
                .verify(&Identity {
                    id: id.to_owned(),
                    file: file.to_owned(),
                })
                .await?;
            false
        }
        Target::Fresh(dir) => {
            #[derive(Deserialize)]
            struct Fork {
                cancelled: bool,
            }
            ensure!(
                state.message_count == Some(0),
                "OMP did not start an empty session"
            );
            let fork: Fork = rpc
                .call(Request::Fork)
                .await?
                .context("OMP declined native session creation")?;
            ensure!(!fork.cancelled, "OMP cancelled native session creation");
            let before = state.session_id;
            state = session_state(rpc).await?;
            let identity = state.identity().await?;
            ensure!(
                identity.id != before && identity.file.starts_with(dir),
                "OMP did not create a new session under the assigned root"
            );
            false
        }
    };
    Ok(Observation {
        state,
        protocol,
        host_tools,
    })
}
