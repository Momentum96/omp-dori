//! Local delivery into a session, and the idempotent remote send/receipt/interrupt/model/close commands, each
//! confirmed by readback.
//!
//! Admission receipts live exactly as long as this process-bound native lane. They never correlate a model reply.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::broker::{Broker, Op, command, raw};
use super::dialogs::{
    AgentStatus, DeliverOptions, Delivery, Expected, GoalStatus, ModelSelector, NativeState,
    RemoteCommand, RemoteInput,
};
use super::http::Reply;
use super::rpc::{Rpc, RpcError};

/// Never evict an earlier ID into a potentially duplicated submission.
const MAX_RECEIPTS: usize = 512;
const HOLDS: &str = "Native identity or actionable dialog holds the operation";

/// The outcome of a local delivery.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub(super) enum DeliverOutcome {
    Delivered { started: bool },
    NotReady { status: AgentStatus },
    Ambiguous { detail: &'static str, failure: () },
}

/// Local delivery: held while the goal is paused or blocked; slash-leading text is wrapped so OMP never expands it.
pub(super) async fn deliver_local(
    rpc: &Rpc,
    state: &NativeState,
    text: Result<String, &'static str>,
    options: &DeliverOptions,
) -> Op<DeliverOutcome> {
    let held = matches!(state.goal.status, GoalStatus::Paused | GoalStatus::Blocked);
    let status = if held {
        AgentStatus::Blocked
    } else if state.settled {
        AgentStatus::Idle
    } else {
        AgentStatus::Working
    };
    if held || !options.when_admits(status) {
        return Ok(DeliverOutcome::NotReady { status });
    }
    let text = text.map_err(str::to_owned)?;
    if text.trim().is_empty() {
        return Ok(DeliverOutcome::NotReady { status });
    }
    // Preserve the local lane's established behavior; remote exact input never wraps slash-leading text.
    let message = if text.trim_start().starts_with('/') {
        format!("[CATALY message]\n{text}")
    } else {
        text
    };
    // Follow-up queues cannot drain an idle OMP transcript without an assistant/tool-result tail, in particular a
    // freshly opened lead. Start a native prompt when idle and queue it if a turn races with admission.
    let request = if state.settled {
        command(
            "prompt",
            vec![
                ("message", Value::String(message)),
                ("streamingBehavior", Value::from("followUp")),
            ],
        )
    } else {
        command("follow_up", vec![("message", Value::String(message))])
    };
    Ok(match rpc.request(request).await {
        Ok(_) => DeliverOutcome::Delivered {
            started: state.settled,
        },
        Err(_) => DeliverOutcome::Ambiguous {
            detail: "OMP did not confirm the submitted input",
            failure: (),
        },
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum ReceiptStatus {
    Pending,
    Admitted,
    Refused,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Admission {
    id: String,
    kind: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Receipt {
    id: String,
    status: ReceiptStatus,
    reply_attributed: bool,
    #[serde(rename = "receipt", skip_serializing_if = "Option::is_none")]
    admission: Option<Admission>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

impl Receipt {
    fn of(id: &str, status: ReceiptStatus, reason: Option<String>) -> Self {
        Self {
            id: id.to_owned(),
            status,
            reply_attributed: false,
            admission: (status == ReceiptStatus::Admitted).then(|| Admission {
                id: id.to_owned(),
                kind: "native-submission",
            }),
            reason,
        }
    }
}

struct Submission {
    digest: String,
    receipt: Receipt,
}

#[derive(Serialize)]
struct Interrupt {
    status: &'static str,
    foreground: bool,
    background: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

#[derive(Serialize)]
struct ModelChange {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

impl ModelChange {
    const fn refused(reason: &'static str) -> Self {
        Self {
            status: "refused",
            model: None,
            reason: Some(reason),
        }
    }

    const fn changed(model: String) -> Self {
        Self {
            status: "changed",
            model: Some(model),
            reason: None,
        }
    }
}

#[derive(Serialize)]
struct Close {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

/// The remote submission ledger keyed by request ID.
#[derive(Default)]
pub(super) struct Remote {
    submissions: Mutex<HashMap<String, Submission>>,
}

impl Remote {
    fn ledger(&self) -> std::sync::MutexGuard<'_, HashMap<String, Submission>> {
        self.submissions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn settle(&self, id: &str, receipt: Receipt) {
        if let Some(entry) = self.ledger().get_mut(id) {
            entry.receipt = receipt;
        }
    }

    pub(super) async fn handle(
        &self,
        broker: &Broker,
        rpc: &Rpc,
        expected_session_id: Option<String>,
        command: RemoteCommand,
    ) -> Reply {
        let bound = broker.remote_session();
        let session = match (expected_session_id, bound) {
            (Some(session), Some(bound)) if session == bound => session,
            _ => return Err("Native session identity mismatch".to_owned()),
        };
        match command {
            RemoteCommand::Send(input) => {
                let input = input.map_err(str::to_owned)?;
                raw(&self.send(broker, rpc, &session, input).await?)
            }
            RemoteCommand::Receipt(id) => {
                let id = id.map_err(str::to_owned)?;
                // An unknown ID has no receipt.
                self.ledger()
                    .get(&id)
                    .map_or(Ok(None), |entry| raw(&entry.receipt))
            }
            RemoteCommand::Interrupt => raw(&interrupt(broker, rpc, &session).await?),
            RemoteCommand::Model(selector) => match selector {
                Err(reason) => raw(&ModelChange::refused(reason)),
                Ok(selector) => raw(&model(broker, rpc, &session, selector).await?),
            },
            RemoteCommand::Close(force) => match force {
                Err(reason) => raw(&Close {
                    status: "refused",
                    reason: Some(reason),
                }),
                Ok(_) => raw(&close(broker, rpc, &session).await?),
            },
        }
    }

    async fn send(
        &self,
        broker: &Broker,
        rpc: &Rpc,
        session: &str,
        input: RemoteInput,
    ) -> Op<Receipt> {
        let RemoteInput { id, text, delivery } = input;
        let mut hash = Sha256::new();
        hash.update(delivery.as_str());
        hash.update([0]);
        hash.update(&text);
        let digest = hex(&hash.finalize());
        {
            let mut ledger = self.ledger();
            if let Some(prior) = ledger.get(&id) {
                if prior.digest != digest {
                    return Err("Native request ID has different input".to_owned());
                }
                return Ok(prior.receipt.clone());
            }
            if ledger.len() >= MAX_RECEIPTS {
                return Err("Native submission receipt storage is full".to_owned());
            }
            ledger.insert(
                id.clone(),
                Submission {
                    digest,
                    receipt: Receipt::of(&id, ReceiptStatus::Pending, None),
                },
            );
        }
        let admitted = broker
            .exclusive(async {
                let state = broker.check(&Expected::Session(session.to_owned())).await?;
                if matches!(state.goal.status, GoalStatus::Paused | GoalStatus::Blocked) {
                    return Err("The owner must resume the native goal locally".to_owned());
                }
                let steer = delivery == Delivery::Steer;
                if steer && !state.foreground {
                    return Err("No foreground turn to steer".to_owned());
                }
                if !steer && !state.settled {
                    return Err("Native lane is busy; choose explicit steer".to_owned());
                }
                // OMP's RPC has no raw flag for steer/follow_up. It will expand a slash-leading template or
                // reject an extension command, so refuse this form rather than changing the payload.
                if text.trim_start().starts_with('/') {
                    return Err(
                        "Native headless RPC cannot submit slash-leading text literally".to_owned(),
                    );
                }
                let kind = if steer { "steer" } else { "follow_up" };
                let receipt = match rpc
                    .request(command(kind, vec![("message", Value::from(text.as_str()))]))
                    .await
                {
                    Ok(_) => Receipt::of(&id, ReceiptStatus::Admitted, None),
                    Err(RpcError::Refused(_)) => Receipt::of(
                        &id,
                        ReceiptStatus::Refused,
                        Some("OMP did not acknowledge admission".to_owned()),
                    ),
                    Err(RpcError::Failed(_)) => Receipt::of(
                        &id,
                        ReceiptStatus::Pending,
                        Some("Admission unconfirmed; do not submit a new ID".to_owned()),
                    ),
                };
                self.settle(&id, receipt);
                Ok(())
            })
            .await;
        if let Err(reason) = admitted {
            self.settle(&id, Receipt::of(&id, ReceiptStatus::Refused, Some(reason)));
        }
        Ok(self.ledger().get(&id).map_or_else(
            || Receipt::of(&id, ReceiptStatus::Pending, None),
            |entry| entry.receipt.clone(),
        ))
    }
}

async fn interrupt(broker: &Broker, rpc: &Rpc, session: &str) -> Op<Interrupt> {
    let expected = Expected::Session(session.to_owned());
    broker
        .exclusive(async {
            let state = broker.check(&expected).await.ok();
            if !state.as_ref().is_some_and(|state| state.foreground) {
                return Ok(Interrupt {
                    status: "refused",
                    foreground: false,
                    background: "unverified",
                    reason: Some(if state.is_some() {
                        "No foreground turn to interrupt"
                    } else {
                        HOLDS
                    }),
                });
            }
            rpc.request(command("abort", Vec::new()))
                .await
                .map_err(|error| error.to_string())?;
            let after = broker.check(&expected).await.ok();
            if after.is_none_or(|after| after.foreground) {
                return Err("Native abort requested; foreground readback unconfirmed".to_owned());
            }
            Ok(Interrupt {
                status: "interrupted",
                foreground: true,
                background: "unverified",
                reason: None,
            })
        })
        .await
}

async fn model(
    broker: &Broker,
    rpc: &Rpc,
    session: &str,
    selector: ModelSelector,
) -> Op<ModelChange> {
    let expected = Expected::Session(session.to_owned());
    let ModelSelector {
        model,
        provider,
        model_id,
    } = selector;
    broker
        .exclusive(async {
            let Some(state) = broker
                .check(&expected)
                .await
                .ok()
                .filter(|state| state.settled)
            else {
                return Ok(ModelChange::refused(
                    "Native model changes require a settled, unblocked lane",
                ));
            };
            if state.model.as_deref() == Some(model.as_str()) {
                return Ok(ModelChange::changed(model));
            }
            let request = command(
                "set_model",
                vec![
                    ("provider", Value::String(provider)),
                    ("modelId", Value::String(model_id)),
                ],
            );
            match rpc.request(request).await {
                Ok(_) => {}
                Err(RpcError::Refused(_)) => {
                    return Ok(ModelChange::refused("OMP rejected the model selector"));
                }
                Err(RpcError::Failed(_)) => {
                    return Err(
                        "Native model request unconfirmed; inspect the current model".to_owned(),
                    );
                }
            }
            if broker.check(&expected).await?.model.as_deref() != Some(model.as_str()) {
                return Err("Native model readback did not match".to_owned());
            }
            Ok(ModelChange::changed(model))
        })
        .await
}

async fn close(broker: &Broker, rpc: &Rpc, session: &str) -> Op<Close> {
    broker
        .exclusive(async {
            let state = broker
                .check(&Expected::Session(session.to_owned()))
                .await
                .ok();
            // RPC disconnect cancels dialogs. While a turn or background work can present one, a previous state read
            // cannot authorize forced EOF: the next approval frame may be in transit.
            if !state.as_ref().is_some_and(|state| state.settled) {
                return Ok(Close {
                    status: "refused",
                    reason: Some(if state.is_some() {
                        "Native lane must settle before close, even with force"
                    } else {
                        HOLDS
                    }),
                });
            }
            Ok(match broker.shutdown(rpc, session).await {
                Ok(()) => Close {
                    status: "closed",
                    reason: None,
                },
                Err(_) => Close {
                    status: "close-unconfirmed",
                    reason: Some("OMP exit did not confirm after graceful RPC disconnect"),
                },
            })
        })
        .await
}
