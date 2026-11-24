//! One prompt turn over rpc-ui: acceptance, `agent_start`, final assistant text from `message_end`,
//! and the `prompt_result` settle rules.

use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use tokio::time::timeout;

use super::rpc_client::{Kind, Response, Rpc};

const MAX_REPLY_BYTES: usize = 64 * 1024;
const TURN_DEADLINE: Duration = Duration::from_mins(5);

#[derive(Serialize)]
struct Prompt<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    id: &'a str,
    message: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Admission {
    agent_invoked: Option<bool>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event<'a> {
    AgentStart,
    #[serde(rename_all = "camelCase")]
    PromptResult {
        id: Option<&'a str>,
        agent_invoked: bool,
        status: PromptStatus,
        session_settled: bool,
    },
    MessageEnd {
        message: Message<'a>,
    },
    ExtensionUiRequest {
        method: &'a str,
    },
    #[serde(other)]
    Other,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PromptStatus {
    Completed,
    Aborted,
    Error,
}

#[derive(Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
enum Message<'a> {
    #[serde(rename_all = "camelCase")]
    Assistant {
        stop_reason: &'a str,
        content: Vec<Content>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Content {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}

pub enum TurnOutcome {
    Completed(String),
    Failed(&'static str),
}

impl Rpc {
    pub(crate) async fn run_turn(&mut self, message: &str) -> Result<TurnOutcome> {
        timeout(TURN_DEADLINE, self.prompt(message))
            .await
            .context("OMP turn deadline elapsed; execution may have started")?
    }

    async fn prompt(&mut self, message: &str) -> Result<TurnOutcome> {
        let id = self.allocate_id()?;
        self.write(&Prompt {
            kind: "prompt",
            id: &id,
            message,
        })
        .await?;
        let mut accepted = false;
        let mut started = false;
        let mut answer = None;
        loop {
            let frame = self.frames.read().await?;
            match frame.kind {
                Kind::Response => {
                    let response: Response<'_> = serde_json::from_slice(&frame.bytes)?;
                    if response.id.as_deref() != Some(&id) {
                        continue;
                    }
                    ensure!(
                        response.command == "prompt",
                        "OMP prompt response command mismatch"
                    );
                    if !response.success && !accepted {
                        ensure!(
                            !started,
                            "OMP reported rejection after starting an agent turn"
                        );
                        return Ok(TurnOutcome::Failed("OMP declined the request"));
                    }
                    if accepted && !response.success {
                        continue;
                    }
                    ensure!(!accepted, "duplicate OMP prompt acceptance");
                    let admission: Option<Admission> = response
                        .data
                        .map(|data| serde_json::from_str(data.get()))
                        .transpose()?;
                    if admission.and_then(|ack| ack.agent_invoked) == Some(false) {
                        return Ok(TurnOutcome::Failed(
                            "OMP handled the input without starting an agent turn",
                        ));
                    }
                    accepted = true;
                }
                Kind::Event => match serde_json::from_slice(&frame.bytes)? {
                    Event::AgentStart => {
                        started = true;
                        answer = None;
                    }
                    Event::MessageEnd {
                        message:
                            Message::Assistant {
                                stop_reason,
                                content,
                            },
                    } if started => {
                        answer = if stop_reason == "stop" {
                            public_text(content)
                        } else {
                            None
                        };
                    }
                    Event::PromptResult {
                        id: Some(result_id),
                        agent_invoked,
                        status,
                        session_settled,
                    } if result_id == id => {
                        return settled_result(
                            accepted,
                            session_settled,
                            agent_invoked,
                            status,
                            answer,
                        );
                    }
                    Event::ExtensionUiRequest {
                        method: "confirm" | "select" | "input" | "editor" | "formDialog" | "form",
                    } => {
                        bail!(
                            "OMP requested interactive input; Cataly did not approve or answer it"
                        );
                    }
                    Event::PromptResult { .. }
                    | Event::MessageEnd { .. }
                    | Event::ExtensionUiRequest { .. }
                    | Event::Other => {}
                },
                Kind::RpcFrameError | Kind::Ready | Kind::RpcChunk => {
                    bail!("OMP prompt observation stream became untrustworthy")
                }
            }
        }
    }
}

fn settled_result(
    accepted: bool,
    session_settled: bool,
    agent_invoked: bool,
    status: PromptStatus,
    answer: Option<String>,
) -> Result<TurnOutcome> {
    ensure!(
        accepted,
        "OMP reported prompt completion before accepting the request"
    );
    ensure!(
        session_settled,
        "OMP has outstanding work after this turn; result is not final"
    );
    Ok(match status {
        PromptStatus::Completed if agent_invoked => answer.map_or(
            TurnOutcome::Failed("OMP finished without a complete public text reply"),
            TurnOutcome::Completed,
        ),
        PromptStatus::Completed => TurnOutcome::Failed("OMP did not start an agent turn"),
        PromptStatus::Aborted => TurnOutcome::Failed("OMP aborted the turn"),
        PromptStatus::Error => TurnOutcome::Failed("OMP reported a turn error"),
    })
}

fn public_text(parts: Vec<Content>) -> Option<String> {
    let mut result = String::new();
    for part in parts {
        match part {
            Content::Text { text } => {
                if text.trim().is_empty() {
                    continue;
                }
                let separator = usize::from(!result.is_empty());
                if result
                    .len()
                    .checked_add(text.len())?
                    .checked_add(separator)?
                    > MAX_REPLY_BYTES
                {
                    return None;
                }
                if separator != 0 {
                    result.push('\n');
                }
                result.push_str(&text);
            }
            Content::Other => {}
        }
    }
    (!result.is_empty()).then_some(result)
}
