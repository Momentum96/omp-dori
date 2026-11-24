use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::value::RawValue;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

use super::rpc_frames::{Frames, Kind, LOGICAL_LIMIT, PHYSICAL_LIMIT};

const EXIT_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Serialize)]
#[serde(tag = "type")]
pub enum Request<'a> {
    #[serde(rename = "negotiate_protocol", rename_all = "camelCase")]
    Negotiate { protocol_version: u32 },
    #[serde(rename = "get_state")]
    GetState,
    #[serde(rename = "fork")]
    Fork,
    #[serde(rename = "set_host_tools")]
    SetHostTools { tools: &'a [serde_json::Value] },
}

impl Request<'_> {
    const fn name(self) -> &'static str {
        match self {
            Self::Negotiate { .. } => "negotiate_protocol",
            Self::GetState => "get_state",
            Self::Fork => "fork",
            Self::SetHostTools { .. } => "set_host_tools",
        }
    }
}

#[derive(Serialize)]
struct CommandFrame<'a> {
    id: &'a str,
    #[serde(flatten)]
    request: Request<'a>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Negotiated {
    protocol_version: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Ready {
    protocol_version: u32,
    #[serde(default)]
    supported_protocol_versions: Vec<u32>,
    max_frame_bytes: Option<usize>,
    max_reassembled_frame_bytes: Option<usize>,
}

#[derive(Deserialize)]
struct Response<'a> {
    id: Option<String>,
    command: String,
    success: bool,
    #[serde(borrow)]
    data: Option<&'a RawValue>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Registration {
    tool_names: Vec<String>,
}

pub struct Rpc {
    child: Child,
    stdin: ChildStdin,
    frames: Frames<ChildStdout>,
    next_id: u32,
}

impl Rpc {
    pub(crate) fn spawn(mut command: Command) -> Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().context("OMP RPC stdin is unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("OMP RPC stdout is unavailable")?;
        Ok(Self {
            child,
            stdin,
            frames: Frames::new(stdout),
            next_id: 0,
        })
    }

    pub(crate) async fn negotiate(&mut self) -> Result<u32> {
        let frame = self.frames.read().await?;
        ensure!(
            frame.kind == Kind::Ready,
            "OMP RPC did not begin with a ready frame"
        );
        let ready: Ready =
            serde_json::from_slice(&frame.bytes).context("invalid OMP RPC ready frame")?;
        ensure!(
            ready.protocol_version == 1,
            "unsupported OMP startup RPC protocol {}",
            ready.protocol_version
        );
        self.frames.set_limits(
            ready.max_frame_bytes.unwrap_or(PHYSICAL_LIMIT),
            ready.max_reassembled_frame_bytes.unwrap_or(LOGICAL_LIMIT),
        )?;
        if !ready.supported_protocol_versions.contains(&2) {
            return Ok(1);
        }
        let result: Negotiated = self
            .call(Request::Negotiate {
                protocol_version: 2,
            })
            .await?
            .context("OMP declined RPC protocol 2")?;
        ensure!(
            result.protocol_version == 2,
            "OMP did not confirm RPC protocol 2"
        );
        self.frames.enable_chunking();
        Ok(2)
    }

    pub(crate) async fn call<T: DeserializeOwned>(
        &mut self,
        request: Request<'_>,
    ) -> Result<Option<T>> {
        let id = self.allocate_id()?;
        self.write(&CommandFrame { id: &id, request }).await?;
        loop {
            let frame = self.frames.read().await?;
            match frame.kind {
                Kind::Response => {
                    let response: Response<'_> =
                        serde_json::from_slice(&frame.bytes).context("invalid OMP RPC response")?;
                    if response.id.as_deref() == Some(&id) {
                        ensure!(
                            response.command == request.name(),
                            "OMP RPC response command mismatch"
                        );
                        if !response.success {
                            return Ok(None);
                        }
                        let data = response.data.context("OMP RPC response was missing data")?;
                        return serde_json::from_str(data.get()).map(Some).with_context(|| {
                            format!("invalid OMP {} response data", request.name())
                        });
                    }
                }
                Kind::RpcFrameError => bail!("OMP reported a lost or overflowing RPC frame"),
                Kind::Ready | Kind::RpcChunk => bail!("unexpected OMP RPC transport frame"),
                Kind::Event => {}
            }
        }
    }

    fn allocate_id(&mut self) -> Result<String> {
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("OMP RPC request IDs exhausted")?;
        Ok(format!("cataly-{}", self.next_id))
    }

    async fn write(&mut self, frame: &(impl Serialize + Sync)) -> Result<()> {
        let mut bytes = serde_json::to_vec(frame)?;
        bytes.push(b'\n');
        ensure!(
            bytes.len() <= self.frames.physical_limit(),
            "Cataly RPC command exceeded the OMP wire limit"
        );
        self.stdin
            .write_all(&bytes)
            .await
            .context("writing OMP RPC request")
    }

    pub(crate) async fn probe_host_tools(&mut self) -> Result<bool> {
        let Some(registration) = self
            .call::<Registration>(Request::SetHostTools { tools: &[] })
            .await?
        else {
            return Ok(false);
        };
        ensure!(
            registration.tool_names.is_empty(),
            "OMP registered unexpected host tools"
        );
        Ok(true)
    }

    pub(crate) async fn close(self) -> Result<()> {
        let Self {
            mut child,
            stdin,
            mut frames,
            ..
        } = self;
        // EOF is OMP's public graceful shutdown command. Drain stdout while waiting so
        // a full pipe cannot prevent journal disposal or child exit.
        drop(stdin);
        let result = timeout(EXIT_DEADLINE, async {
            let mut sink = tokio::io::sink();
            tokio::try_join!(child.wait(), tokio::io::copy(&mut frames.reader, &mut sink))
        })
        .await;
        let Ok(result) = result else {
            child
                .kill()
                .await
                .context("terminating OMP after shutdown deadline")?;
            bail!("OMP did not exit cleanly before the shutdown deadline");
        };
        let (status, _) = result.context("waiting for OMP shutdown")?;
        ensure!(status.success(), "OMP exited with {status}");
        Ok(())
    }
}
