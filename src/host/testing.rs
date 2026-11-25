//! An in-process fake of OMP's rpc-ui side for unit tests: answers `get_state` from a mutable state,
//! runs a prompt as one complete turn (and appends its user entry to a session file), and lets a
//! test push any frame or end the stream.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::struct_excessive_bools,
    clippy::needless_pass_by_value
)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::mpsc;

use super::headless::{Session, Settings};
use super::{Control, Role, Snapshot, StartError, StartSpec, Started};
use crate::omp::launch::Target;
use crate::omp::rpc::Rpc;

/// What the fake reports and does.
#[derive(Debug, Clone)]
pub(crate) struct FakeState {
    pub(crate) session_id: String,
    pub(crate) file: PathBuf,
    pub(crate) streaming: bool,
    pub(crate) model: (String, String),
    pub(crate) extra_usage: bool,
    /// Answer prompts at all (false: the line is read and nothing comes back).
    pub(crate) answer_prompts: bool,
    /// Append the prompt's user entry to the session file.
    pub(crate) write_entries: bool,
}

pub(crate) struct Fake {
    state: Arc<Mutex<FakeState>>,
    seen: Arc<Mutex<Vec<Value>>>,
    push: mpsc::UnboundedSender<Option<Value>>,
    hub: Mutex<Option<(ReadHalf<DuplexStream>, WriteHalf<DuplexStream>)>>,
    /// The session-file directory `start` made; removed when the fake drops.
    dir: Option<crate::sys::fs::tests::Scratch>,
}

pub(crate) fn spec(handle: &str, expected: Option<(&str, PathBuf)>) -> StartSpec {
    StartSpec {
        handle: handle.into(),
        role: Role::Lead,
        cwd: std::env::temp_dir(),
        model: "fake/primary".into(),
        thinking: None,
        target: expected.as_ref().map_or_else(
            || Target::Fresh(std::env::temp_dir()),
            |(_, file)| Target::Resume(file.clone()),
        ),
        expected_id: expected.map(|(id, _)| id.to_string()),
        instructions: None,
    }
}

impl Fake {
    pub(crate) fn start() -> Self {
        let dir = crate::sys::fs::tests::scratch("fake-omp");
        let mut fake = Self::with(FakeState {
            session_id: "0b9e7c1a-1111-4222-8333-444455556666".into(),
            file: dir.join("2026-11-25T01-02-03-004Z_0b9e7c1a-1111-4222-8333-444455556666.jsonl"),
            streaming: false,
            model: ("fake".into(), "primary".into()),
            extra_usage: false,
            answer_prompts: true,
            write_entries: true,
        });
        fake.dir = Some(dir);
        fake
    }

    pub(crate) fn with(state: FakeState) -> Self {
        let (hub_side, fake_side) = tokio::io::duplex(1 << 20);
        let state = Arc::new(Mutex::new(state));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (push, pushed) = mpsc::unbounded_channel();
        tokio::spawn(run(
            fake_side,
            Arc::clone(&state),
            Arc::clone(&seen),
            pushed,
        ));
        Self {
            state,
            seen,
            push,
            hub: Mutex::new(Some(tokio::io::split(hub_side))),
            dir: None,
        }
    }

    pub(crate) async fn bind(
        &self,
        spec: StartSpec,
    ) -> Result<(Arc<Session>, Started), StartError> {
        let (reader, writer) = self.hub.lock().unwrap().take().expect("bound once");
        let (tx, rx) = mpsc::channel(4096);
        let rpc = Rpc::connect(reader, writer, Duration::from_secs(5), Some(tx))
            .await
            .unwrap();
        Session::bind(rpc, rx, None, spec, Settings::default()).await
    }

    pub(crate) fn session_id(&self) -> String {
        self.state.lock().unwrap().session_id.clone()
    }

    pub(crate) fn file(&self) -> PathBuf {
        self.state.lock().unwrap().file.clone()
    }

    pub(crate) fn set(&self, f: impl FnOnce(&mut FakeState)) {
        f(&mut self.state.lock().unwrap());
    }

    pub(crate) fn push(&self, frame: Value) {
        self.push.send(Some(frame)).unwrap();
    }

    /// Ends OMP's output, as an exited child would.
    pub(crate) fn exit(&self) {
        let _ = self.push.send(None);
    }

    pub(crate) fn seen(&self, kind: &str) -> Vec<Value> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["type"] == kind)
            .cloned()
            .collect()
    }

    /// Waits for the `n`th frame of a kind (1-based) and returns it.
    pub(crate) async fn wait_seen(&self, kind: &str, n: usize) -> Value {
        for _ in 0..300 {
            if let Some(v) = self.seen(kind).get(n - 1) {
                return v.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("no {kind} #{n}");
    }

    pub(crate) async fn wait_for(
        &self,
        session: &Session,
        f: impl Fn(&Snapshot) -> bool,
    ) -> Arc<Snapshot> {
        for _ in 0..300 {
            let s = session.snapshot();
            if f(&s) {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition not reached: {:?}", session.snapshot());
    }
}

fn response(id: &Value, command: &str, data: Value) -> Value {
    json!({"type": "response", "id": id, "command": command, "success": true, "data": data})
}

async fn run(
    side: DuplexStream,
    state: Arc<Mutex<FakeState>>,
    seen: Arc<Mutex<Vec<Value>>>,
    mut pushed: mpsc::UnboundedReceiver<Option<Value>>,
) {
    let (reader, mut writer) = tokio::io::split(side);
    let mut lines = BufReader::new(reader).lines();
    let mut out =
        vec![json!({"type": "ready", "protocolVersion": 1, "supportedProtocolVersions": [1]})];
    loop {
        for frame in out.drain(..) {
            let mut line = frame.to_string();
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() {
                return;
            }
        }
        tokio::select! {
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { return };
                let Ok(command) = serde_json::from_str::<Value>(&line) else { continue };
                seen.lock().unwrap().push(command.clone());
                out = answer(&command, &state);
            }
            frame = pushed.recv() => match frame {
                Some(Some(frame)) => out.push(frame),
                _ => return,
            },
        }
    }
}

fn answer(command: &Value, state: &Arc<Mutex<FakeState>>) -> Vec<Value> {
    let id = &command["id"];
    let kind = command["type"].as_str().unwrap_or_default();
    let mut st = state.lock().unwrap();
    match kind {
        "get_state" => {
            let mut data = json!({
                "sessionId": st.session_id, "sessionFile": st.file.display().to_string(),
                "isStreaming": st.streaming, "isCompacting": false, "queuedMessageCount": 0,
                "isSettled": !st.streaming, "goal": null,
                "model": {"provider": st.model.0, "id": st.model.1},
            });
            if st.extra_usage {
                data["usageLimit"] = json!({"stage": "wrap_up", "extraUsage": true});
            }
            vec![response(id, kind, data)]
        }
        "prompt" if !st.answer_prompts => Vec::new(),
        "prompt" => {
            let text = command["message"].as_str().unwrap_or_default().to_string();
            if st.write_entries {
                use std::io::Write as _;
                let entry = json!({"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": text}]}});
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&st.file)
                    .unwrap();
                writeln!(f, "{entry}").unwrap();
            }
            vec![
                response(id, kind, json!({})),
                json!({"type": "agent_start"}),
                json!({"type": "message_end", "message": {"role": "user", "content": [{"type": "text", "text": text}]}}),
                json!({"type": "message_end", "message": {"role": "assistant", "content": [{"type": "text", "text": "ok"}]}}),
                json!({"type": "agent_end", "isTerminal": true}),
                json!({"type": "prompt_result", "id": id, "agentInvoked": true, "status": "completed", "sessionSettled": true}),
                json!({"type": "session_settled"}),
            ]
        }
        "abort" => {
            st.streaming = false;
            vec![response(id, kind, json!({}))]
        }
        "set_model" => {
            st.model = (
                command["provider"].as_str().unwrap_or_default().into(),
                command["modelId"].as_str().unwrap_or_default().into(),
            );
            vec![response(id, kind, json!({}))]
        }
        "extension_ui_response" => Vec::new(),
        _ => vec![response(id, kind, json!({}))],
    }
}
