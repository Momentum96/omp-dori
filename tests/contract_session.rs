//! Contract suite, rpc cases of PATCH-12 (milestone 2): stock OMP in rpc-ui mode against a loopback
//! fake model provider, in an isolated agent directory, home and session directory. Runs only with
//! `CATALY_CONTRACT_OMP=1` (`CATALY_CONTRACT_OMP_PATH` selects the executable). Each case has 30 s.
//!
//! Cases: prompt with `prompt_result` and `session_settled` and the delivery marker in the session
//! file; follow-up; steer; abort; an approval dialog answered Deny; resume identity; lease. Then the
//! hub end to end: the lead starts, a message is written and confirmed from the session file, the
//! lead stops cleanly and resumes its session after a hub restart (acceptance check 12), and the
//! fallback chain moves a usage-limited lead to the backup model (acceptance check 9).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::TestHub;
use support::model::{FakeModel, models_yml};

const CASE: Duration = Duration::from_secs(30);

fn enabled() -> bool {
    let on = std::env::var("CATALY_CONTRACT_OMP").as_deref() == Ok("1");
    if !on {
        eprintln!("skipped: set CATALY_CONTRACT_OMP=1 to run the OMP contract suite");
    }
    on
}

fn installed_omp() -> PathBuf {
    if let Some(path) = std::env::var_os("CATALY_CONTRACT_OMP_PATH") {
        return PathBuf::from(path);
    }
    let path = std::env::var_os("PATH").expect("PATH");
    std::env::split_paths(&path)
        .map(|dir| dir.join("omp"))
        .find(|p| p.is_file())
        .expect("omp on PATH")
}

/// An isolated OMP profile: agent directory with the fake provider, home, cwd and session directory.
struct Sandbox {
    root: PathBuf,
    model: FakeModel,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("cataly-omp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["agent", "home", "work", "sessions"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let model = FakeModel::start();
        std::fs::write(
            root.join("agent/models.yml"),
            models_yml(model.port, &["primary", "backup", "limited"]),
        )
        .unwrap();
        Self { root, model }
    }

    fn omp(&self, extra: &[String]) -> Rpc {
        let mut cmd = Command::new(installed_omp());
        cmd.args([
            "--mode",
            "rpc-ui",
            "--approval-mode",
            "always-ask",
            "--no-extensions",
            "--no-skills",
            "--no-rules",
            "--no-context-files",
            "--model=fake/primary",
        ])
        .args(extra)
        .current_dir(self.root.join("work"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", self.root.join("home"))
        .env("PI_CODING_AGENT_DIR", self.root.join("agent"))
        .env("PI_NOTIFICATIONS", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
        let mut child = cmd.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && tx.send(v).is_err()
                {
                    return;
                }
            }
        });
        let mut rpc = Rpc {
            child,
            stdin: Some(stdin),
            rx,
            next: 0,
            frames: Vec::new(),
        };
        rpc.until(|f| f["type"] == "ready");
        rpc
    }

    fn fresh(&self) -> Rpc {
        self.omp(&[format!(
            "--session-dir={}",
            self.root.join("sessions").display()
        )])
    }
}

struct Rpc {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: mpsc::Receiver<Value>,
    next: u32,
    /// Every frame read so far.
    frames: Vec<Value>,
}

impl Rpc {
    fn send(&mut self, mut command: Value) -> String {
        self.next += 1;
        let id = format!("t{}", self.next);
        if command.get("type") != Some(&json!("extension_ui_response")) {
            command["id"] = json!(id);
        }
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{command}").unwrap();
        stdin.flush().unwrap();
        id
    }

    fn until(&mut self, f: impl Fn(&Value) -> bool) -> Value {
        if let Some(v) = self.frames.iter().find(|v| f(v)) {
            return v.clone();
        }
        let deadline = Instant::now() + CASE;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let frame = self
                .rx
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("frame not seen within 30 s; saw {:#?}", self.frames));
            self.frames.push(frame.clone());
            if f(&frame) {
                return frame;
            }
        }
    }

    fn call(&mut self, command: Value) -> Value {
        let id = self.send(command);
        self.until(|f| f["type"] == "response" && f["id"] == id.as_str())
    }

    fn state(&mut self) -> Value {
        let reply = self.call(json!({"type": "get_state"}));
        assert_eq!(reply["success"], true, "{reply}");
        reply["data"].clone()
    }

    fn result_of(&mut self, id: &str) -> Value {
        self.until(|f| f["type"] == "prompt_result" && f["id"] == id)
    }

    fn close(mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                assert!(status.success(), "OMP exit {status}");
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        panic!("OMP did not exit within 15 s of end of input");
    }
}

fn user_entry_with(file: &Path, marker: &str) -> bool {
    let text = std::fs::read_to_string(file).unwrap_or_default();
    text.lines().any(|line| {
        let v: Value = serde_json::from_str(line).unwrap_or(Value::Null);
        v["type"] == "message" && v["message"]["role"] == "user" && line.contains(marker)
    })
}

fn wait_entry(file: &Path, marker: &str) {
    let deadline = Instant::now() + CASE;
    while !user_entry_with(file, marker) {
        assert!(
            Instant::now() < deadline,
            "{marker} not in {}",
            file.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn rpc_contract_against_stock_omp() {
    if !enabled() {
        return;
    }
    let sandbox = Sandbox::new("rpc");
    let mut omp = sandbox.fresh();
    let state = omp.state();
    let (id, file) = (
        state["sessionId"].as_str().unwrap().to_string(),
        state["sessionFile"].as_str().unwrap().to_string(),
    );
    assert!(Path::new(&file).is_absolute(), "{state}");

    // prompt: admitted, a turn, its prompt_result, session_settled, the marker in a user entry.
    let p = omp.send(json!({"type": "prompt", "message": "[cataly:c1] from owner\nhello", "streamingBehavior": "followUp"}));
    let ack = omp.until(|f| f["type"] == "response" && f["id"] == p.as_str());
    assert_eq!(
        (ack["success"].clone(), ack["command"].clone()),
        (json!(true), json!("prompt"))
    );
    omp.until(|f| f["type"] == "agent_start");
    let result = omp.result_of(&p);
    assert_eq!(result["status"], "completed", "{result}");
    assert_eq!(result["agentInvoked"], true);
    omp.until(|f| f["type"] == "session_settled");
    wait_entry(Path::new(&file), "[cataly:c1]");
    assert_eq!(omp.state()["isSettled"], true);

    // follow_up while streaming: queued, then answered after the running turn.
    let slow =
        omp.send(json!({"type": "prompt", "message": "SLOW one", "streamingBehavior": "followUp"}));
    omp.until(|f| f["type"] == "agent_start");
    let follow = omp.call(json!({"type": "follow_up", "message": "[cataly:c2] second"}));
    assert_eq!(follow["success"], true, "{follow}");
    assert_eq!(omp.result_of(&slow)["status"], "completed");
    wait_entry(Path::new(&file), "[cataly:c2]");

    // steer: admitted into a running turn.
    let slow =
        omp.send(json!({"type": "prompt", "message": "SLOW two", "streamingBehavior": "followUp"}));
    std::thread::sleep(Duration::from_millis(700));
    let steer = omp.call(json!({"type": "steer", "message": "[cataly:c3] steer now"}));
    assert_eq!(steer["success"], true, "{steer}");
    omp.result_of(&slow);
    wait_entry(Path::new(&file), "[cataly:c3]");
    omp.until(|f| f["type"] == "session_settled");

    // abort: the turn ends aborted and the session reads back idle.
    let slow = omp
        .send(json!({"type": "prompt", "message": "SLOW three", "streamingBehavior": "followUp"}));
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(omp.state()["isStreaming"], true);
    assert_eq!(omp.call(json!({"type": "abort"}))["success"], true);
    assert_eq!(omp.result_of(&slow)["status"], "aborted");
    assert_eq!(omp.state()["isStreaming"], false);

    // A tool call under always-ask opens an approval select; Deny answers it once.
    let tool = omp
        .send(json!({"type": "prompt", "message": "TOOL please", "streamingBehavior": "followUp"}));
    let dialog = omp.until(|f| f["type"] == "extension_ui_request" && f["method"] == "select");
    let options: Vec<&str> = dialog["options"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(options.contains(&"Deny"), "{dialog}");
    omp.send(json!({"type": "extension_ui_response", "id": dialog["id"], "value": "Deny"}));
    assert_eq!(omp.result_of(&tool)["status"], "completed");

    // Lease: a live process holds its session.
    let lease = Command::new(installed_omp())
        .args(["session", "lease", "--json", "--", &id])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", sandbox.root.join("home"))
        .env("PI_CODING_AGENT_DIR", sandbox.root.join("agent"))
        .output()
        .unwrap();
    let lease: Value = serde_json::from_slice(&lease.stdout).unwrap();
    assert!(lease["held"].to_string().contains(&id), "{lease}");
    let rpc_lease = omp.call(json!({"type": "get_session_lease", "sessionIds": [id]}));
    assert_eq!(rpc_lease["success"], true, "{rpc_lease}");
    omp.close();

    // Resume identity: the same ID and file after --resume.
    let mut resumed = sandbox.omp(&[format!("--resume={file}")]);
    let again = resumed.state();
    assert_eq!(again["sessionId"], id.as_str(), "{again}");
    assert_eq!(
        std::fs::canonicalize(again["sessionFile"].as_str().unwrap()).unwrap(),
        std::fs::canonicalize(&file).unwrap()
    );
    resumed.close();
    assert_ne!(sandbox.model.seen().len(), 0);
    let _ = std::fs::remove_dir_all(&sandbox.root);
}

fn lead_hub(sandbox: &Sandbox, model: &str, extra: &str) -> TestHub {
    let mut hub = TestHub::new("");
    hub.env
        .push(("PI_CODING_AGENT_DIR".into(), sandbox.root.join("agent")));
    hub.write_config(&format!(
        "[omp]\npath = \"{}\"\n[sessions]\nextra_flags = [\"--no-skills\", \"--no-rules\", \"--no-context-files\"]\n\
         [lead]\nmodel = \"{model}\"\ncwd = \"{}\"\n{extra}",
        installed_omp().display(),
        sandbox.root.join("work").display()
    ));
    hub
}

fn wait_lead(hub: &TestHub, pred: impl Fn(&Value) -> bool) -> Value {
    hub.until(Duration::from_secs(60), || {
        let v = hub.json(&["lead", "status", "--json"]);
        pred(&v).then_some(v)
    })
}

/// Acceptance check 12 with stock OMP, and the measurements of slice 2a.
#[test]
fn lead_end_to_end_and_resume() {
    if !enabled() {
        return;
    }
    let sandbox = Sandbox::new("lead");
    let mut hub = lead_hub(&sandbox, "fake/primary", "");
    hub.start();
    let live = wait_lead(&hub, |v| v["phase"] == "live");
    let session = live["session"]["session_id"].as_str().unwrap().to_string();
    let file = live["session"]["session_file"]
        .as_str()
        .unwrap()
        .to_string();
    let rss = hub.json(&["stats", "--json"])["rss_bytes"]
        .as_u64()
        .unwrap();
    for n in 0..5 {
        let out = hub.cli(&["lead", "send", &format!("hello {n}"), "--json"]);
        assert!(out.status.success(), "{out:?}");
        let receipt: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(
            ["sent", "delivered", "waiting"].contains(&receipt["state"].as_str().unwrap()),
            "{receipt}"
        );
        wait_lead(&hub, |v| v["sent"] == 0 && v["waiting"] == 0);
    }
    let stats = hub.json(&["stats", "--json"]);
    eprintln!(
        "MEASURE rss_lead_attached={rss} wake={} confirm={} agent_start={}",
        stats["wake_latency"], stats["confirm_latency"], stats["agent_start_latency"]
    );
    assert!(
        stats["confirm_latency"]["count"].as_u64().unwrap() >= 5,
        "{stats}"
    );
    // The overlay pins the safety settings (check 9: modelFallback on, no auto-redeem).
    let overlay: Value = serde_json::from_str(
        &std::fs::read_to_string(hub.state().join("omp-overlay.yml")).unwrap(),
    )
    .unwrap();
    assert_eq!(overlay["retry"]["modelFallback"], true);
    assert_eq!(overlay["codexResets"]["autoRedeem"], "no");

    // A clean stop (hub shutdown) and a restart resume the same session and file.
    assert!(hub.stop().success());
    hub.start();
    let again = wait_lead(&hub, |v| v["phase"] == "live");
    assert_eq!(again["session"]["session_id"], session.as_str(), "{again}");
    assert_eq!(again["session"]["session_file"], file.as_str());

    // An explicit stop, then a second start refuses nothing and resumes again.
    let stopped = hub.json(&["lead", "stop", "--json"]);
    assert_eq!(stopped["phase"], "stopped", "{stopped}");
    let started = hub.json(&["lead", "start", "--json"]);
    assert_eq!(
        started["session"]["session_id"],
        session.as_str(),
        "{started}"
    );
    let refused = hub.cli(&["lead", "start"]);
    assert!(!refused.status.success());
    assert!(hub.stop().success());
    let _ = std::fs::remove_dir_all(&sandbox.root);
}

/// Acceptance check 9 with stock OMP: a usage-limited model falls back along the owner's chain.
#[test]
fn lead_falls_back_along_the_owner_chain() {
    if !enabled() {
        return;
    }
    let sandbox = Sandbox::new("fallback");
    let mut hub = lead_hub(
        &sandbox,
        "fake/limited",
        "[fallback]\non = \"usage-limit\"\n[fallback.chains]\ndefault = [\"fake/backup\"]\n",
    );
    hub.start();
    wait_lead(&hub, |v| v["phase"] == "live");
    let overlay: Value = serde_json::from_str(
        &std::fs::read_to_string(hub.state().join("omp-overlay.yml")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        overlay["retry"]["fallbackChains"]["default"][0],
        "fake/backup"
    );
    assert!(hub.cli(&["lead", "send", "hello"]).status.success());
    let kinds = hub.until(Duration::from_secs(60), || {
        let kinds = hub.event_kinds();
        kinds
            .iter()
            .any(|k| k == "session.fallback" || k == "session.quota_blocked")
            .then_some(kinds)
    });
    eprintln!(
        "fallback events: {kinds:?}; models asked: {:?}",
        sandbox.model.seen()
    );
    assert!(kinds.iter().any(|k| k == "session.fallback"), "{kinds:?}");
    // OMP reports one `retry_fallback_applied` per fallen-back request; the hub turns each into one
    // quiet event, never a reset or another account.
    assert!(sandbox.model.seen().iter().any(|m| m == "backup"));
    assert!(hub.stop().success());
    let _ = std::fs::remove_dir_all(&sandbox.root);
}
