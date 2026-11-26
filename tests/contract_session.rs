//! Contract suite, rpc cases of PATCH-12 (milestone 2): stock OMP in rpc-ui mode against a loopback
//! fake model provider, in an isolated agent directory, home and session directory. Runs only with
//! `CATALY_CONTRACT_OMP=1` (`CATALY_CONTRACT_OMP_PATH` selects the executable). Each case has 30 s.
//!
//! Cases: prompt with `prompt_result` and `session_settled` and the delivery marker in the session
//! file; follow-up; steer; abort; an approval dialog answered Deny; resume identity; lease. Then the
//! hub end to end: the lead starts, a message is written and confirmed from the session file, the
//! lead stops cleanly and resumes its session after a hub restart (acceptance check 12), also in the
//! folder its session recorded after the default folder changed, and the
//! fallback chain moves a usage-limited lead to the backup model (acceptance check 9). The bridge
//! case: inert in an ordinary session (check 11), its start-up cost, and an accepted lead. The lead's
//! tool and read-only commands run without asking, its tool named in `bash` is refused without a
//! dialog, a write still asks, and the CLI and `eval` stay refused. An idle lead sleeps and an owner
//! message wakes it on the same session (debug builds, with the measurements of the lead's sleep).

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

/// The whole profile (agent directory, sessions, home) goes with the sandbox, also when a test fails.
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
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
        self.omp_with(false, extra, &[])
    }

    /// OMP in rpc-ui mode; `discover` lets it load the agent directory's extensions (the bridge
    /// link), else only `-e` paths in `extra` load.
    fn omp_with(&self, discover: bool, extra: &[String], env: &[(&str, PathBuf)]) -> Rpc {
        let mut cmd = Command::new(installed_omp());
        cmd.args([
            "--mode",
            "rpc-ui",
            "--approval-mode",
            "always-ask",
            "--no-skills",
            "--no-rules",
            "--no-context-files",
            "--model=fake/primary",
        ])
        .args((!discover).then_some("--no-extensions"))
        .args(extra)
        .current_dir(self.root.join("work"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", self.root.join("home"))
        .env("PI_CODING_AGENT_DIR", self.root.join("agent"))
        .env("PI_NOTIFICATIONS", "off")
        .envs(env.iter().map(|(k, v)| (*k, v)))
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

/// A test that fails mid-conversation must not leave OMP running in the sandbox.
impl Drop for Rpc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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

    /// Ends input and waits for OMP's own exit. This test's budget mirrors the hub's 15 s close
    /// wait; it is stretched by `CATALY_TEST_PATIENCE` on the slower runners, where an OMP that
    /// shares the host with the other contract tests can take longer to exit.
    fn close(mut self) {
        drop(self.stdin.take());
        let wait = support::patience(Duration::from_secs(15));
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                assert!(status.success(), "OMP exit {status}");
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        panic!("OMP did not exit within {wait:?} of end of input");
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
}

fn lead_hub(sandbox: &Sandbox, model: &str, extra: &str) -> TestHub {
    let mut hub = TestHub::new("");
    hub.env
        .push(("PI_CODING_AGENT_DIR".into(), sandbox.root.join("agent")));
    hub.write_config(&lead_config(model, Some(&sandbox.root.join("work")), extra));
    hub
}

/// The lead's config with stock OMP; without `cwd` the lead's default folder is the hub's home.
fn lead_config(model: &str, cwd: Option<&Path>, extra: &str) -> String {
    let cwd = cwd.map_or_else(String::new, |c| format!("cwd = \"{}\"\n", c.display()));
    format!(
        "[omp]\npath = \"{}\"\n[sessions]\nextra_flags = [\"--no-skills\", \"--no-rules\", \"--no-context-files\"]\n\
         [lead]\nmodel = \"{model}\"\n{cwd}{extra}",
        installed_omp().display(),
    )
}

fn wait_lead(hub: &TestHub, pred: impl Fn(&Value) -> bool) -> Value {
    hub.until(Duration::from_secs(60), || {
        let v = hub.json(&["lead", "status", "--json"]);
        pred(&v).then_some(v)
    })
}

/// Every saved-reset auto-redeem key of OMP 19.1.1.
const RESET_KEYS: [&str; 5] = [
    "codexResets.autoRedeem5h",
    "codexResets.autoRedeemWeekly",
    "claudeResets.autoRedeem5h",
    "claudeResets.autoRedeemWeekly",
    "claudeResets.autoRedeemFable",
];

/// A test extension that writes each key's effective value and the layer that supplied it, read
/// through OMP's public settings registry, to `CATALY_PROBE_OUT` when it loads.
const SETTINGS_PROBE: &str = r#"import { lookup } from "@oh-my-pi/pi-coding-agent/config/registry";
import { writeFileSync } from "node:fs";
export default function (pi: any) {
  const out: Record<string, unknown> = {};
  for (const key of JSON.parse(process.env.CATALY_PROBE_KEYS ?? "[]")) {
    const handle = lookup(key);
    out[key] = handle ? [handle.get(pi.pi.settings), handle.provenance(pi.pi.settings)] : null;
  }
  writeFileSync(process.env.CATALY_PROBE_OUT ?? "", JSON.stringify(out));
}
"#;

/// The reset keys as a running stock OMP session sees them with `overlay`, in a profile whose
/// global config says `yes` to every one of them (the owner's worst case).
fn effective_settings(sandbox: &Sandbox, overlay: &Path) -> Value {
    let agent = sandbox.root.join("probe-agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::copy(
        sandbox.root.join("agent/models.yml"),
        agent.join("models.yml"),
    )
    .unwrap();
    let mut global = String::new();
    for section in ["codexResets", "claudeResets"] {
        global.push_str(section);
        global.push_str(":\n");
        for key in RESET_KEYS
            .iter()
            .filter_map(|k| k.strip_prefix(section)?.strip_prefix('.'))
        {
            global.push_str("  ");
            global.push_str(key);
            global.push_str(": yes\n");
        }
    }
    std::fs::write(agent.join("config.yml"), global).unwrap();
    let probe = sandbox.root.join("settings-probe.ts");
    std::fs::write(&probe, SETTINGS_PROBE).unwrap();
    let out = sandbox.root.join("settings-probe.json");
    let _ = std::fs::remove_file(&out);
    let keys = PathBuf::from(serde_json::to_string(&RESET_KEYS).unwrap());
    let omp = sandbox.omp_with(
        false,
        &[
            format!("--config={}", overlay.display()),
            "-e".into(),
            probe.display().to_string(),
            "--no-session".into(),
        ],
        &[
            ("PI_CODING_AGENT_DIR", agent),
            ("CATALY_PROBE_OUT", out.clone()),
            ("CATALY_PROBE_KEYS", keys),
        ],
    );
    omp.close();
    serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap()
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
    // Wakes of an idle lead: each message waits until the previous turn ended and settled.
    let wakes = 40;
    for n in 0..wakes {
        wait_lead(&hub, |v| {
            v["sent"] == 0 && v["waiting"] == 0 && v["session"]["status"] == "idle"
        });
        let out = hub.cli(&["lead", "send", &format!("hello {n}"), "--json"]);
        assert!(out.status.success(), "{out:?}");
        let receipt: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(
            ["sent", "delivered"].contains(&receipt["state"].as_str().unwrap()),
            "{receipt}"
        );
    }
    wait_lead(&hub, |v| v["sent"] == 0 && v["waiting"] == 0);
    let stats = hub.json(&["stats", "--json"]);
    eprintln!(
        "MEASURE rss_lead_attached={rss} wake={} confirm={} agent_start={} store_commit={}",
        stats["wake_latency"],
        stats["confirm_latency"],
        stats["agent_start_latency"],
        stats["store_commit_latency"]
    );
    assert_eq!(stats["wake_latency"]["count"], wakes, "{stats}");
    assert_eq!(stats["confirm_latency"]["count"], wakes, "{stats}");
    // The overlay pins the safety settings (check 9: modelFallback on, no auto-redeem).
    let overlay: Value = serde_json::from_str(
        &std::fs::read_to_string(hub.state().join("omp-overlay.yml")).unwrap(),
    )
    .unwrap();
    assert_eq!(overlay["retry"]["modelFallback"], true);
    let effective = effective_settings(&sandbox, &hub.state().join("omp-overlay.yml"));
    for key in RESET_KEYS {
        assert_eq!(
            effective[key],
            json!(["no", "overlay"]),
            "{key} over a global yes: {effective}"
        );
    }
    assert_eq!(
        overlay["goal"]["enabled"], false,
        "the lead holds no standing goal (OPC-36)"
    );

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
}

/// The newest request body the fake model received whose text contains `marker`, as JSON text.
fn body_with(model: &FakeModel, marker: &str) -> Option<String> {
    model
        .bodies()
        .iter()
        .rev()
        .map(Value::to_string)
        .find(|b| b.contains(marker))
}

/// The owner's 0.1.1 update with stock OMP: a lead started in another folder than today's default,
/// with OMP's per-session folder holding a subagent transcript next to its session file, resumes the
/// same session in its recorded folder after a hub restart, its next request carries the earlier
/// turn, and the hub reports it resumed.
#[test]
fn lead_resumes_in_its_recorded_folder_after_the_default_changed() {
    if !enabled() {
        return;
    }
    let sandbox = Sandbox::new("lead-folder");
    let mut hub = lead_hub(&sandbox, "fake/primary", "");
    hub.start();
    let live = wait_lead(&hub, |v| v["phase"] == "live");
    assert!(
        hub.cli(&["lead", "send", "remember OSPREY-1"])
            .status
            .success()
    );
    wait_lead(&hub, |v| {
        v["sent"] == 0
            && v["waiting"] == 0
            && v["session"]["status"] == "idle"
            && body_with(&sandbox.model, "OSPREY-1").is_some()
    });
    let file = PathBuf::from(live["session"]["session_file"].as_str().unwrap());
    assert!(hub.stop().success());

    // OMP's per-session folder with a subagent transcript, and a new default folder (the home).
    let artifacts = file.with_extension("");
    std::fs::create_dir_all(&artifacts).unwrap();
    std::fs::write(
        artifacts.join("Scout.jsonl"),
        json!({"type": "session", "version": 3, "id": "sub", "cwd": sandbox.root.join("work")})
            .to_string()
            + "\n",
    )
    .unwrap();
    hub.write_config(&lead_config("fake/primary", None, ""));
    hub.start();
    let again = wait_lead(&hub, |v| v["phase"] == "live");
    for field in ["session_id", "session_file"] {
        assert_eq!(again["session"][field], live["session"][field], "{again}");
    }
    assert_eq!(
        again["session"]["cwd"].as_str().map(PathBuf::from),
        Some(std::fs::canonicalize(sandbox.root.join("work")).unwrap()),
        "{again}"
    );
    let resumed = hub.until(Duration::from_secs(60), || {
        let r = hub.json(&["status", "--json"])["resumed"].clone();
        r.is_object().then_some(r)
    });
    assert_eq!(resumed["lead_start"], "resumed", "{resumed}");
    assert_eq!(resumed["lead"], true, "{resumed}");

    assert!(
        hub.cli(&["lead", "send", "and now OSPREY-2"])
            .status
            .success()
    );
    let next = hub.until(Duration::from_secs(60), || {
        body_with(&sandbox.model, "OSPREY-2")
    });
    assert!(next.contains("OSPREY-1"), "the lead's history: {next}");
    assert!(hub.stop().success());
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
    // A second turn runs on the fallback model; the one move stays one quiet event.
    let settled =
        |v: &Value| v["sent"] == 0 && v["waiting"] == 0 && v["session"]["status"] == "idle";
    wait_lead(&hub, settled);
    assert!(hub.cli(&["lead", "send", "hello again"]).status.success());
    let idle = wait_lead(&hub, settled);
    assert_eq!(idle["session"]["model"], "fake/backup", "{idle}");
    let fallbacks: Vec<Value> = hub
        .events()
        .into_iter()
        .filter(|e| e["kind"] == "session.fallback")
        .collect();
    assert_eq!(fallbacks.len(), 1, "{fallbacks:?}");
    assert_eq!(fallbacks[0]["urgent"], false);
    assert!(sandbox.model.seen().iter().any(|m| m == "backup"));
    // A reported fallback is never an unexpected model (DESIGN 6.1).
    let kinds = hub.event_kinds();
    assert!(
        !kinds.iter().any(|k| k == "session.model_unexpected"),
        "{kinds:?}"
    );
    assert!(hub.stop().success());
}

/// Loads the installed bridge through a counting proxy of OMP's extension API and records what it
/// registered, the entries it wrote and its own load cost (import plus factory) inside OMP.
const PROBE: &str = r#"import { writeFileSync } from "node:fs";
export default async function probe(pi: any) {
	const seen = { on: [] as string[], tools: [] as string[], entries: [] as string[], loadMs: 0 };
	const dump = () => writeFileSync(process.env.CATALY_PROBE_OUT!, JSON.stringify(seen));
	const proxy = new Proxy(pi, {
		get(t: any, k: string | symbol) {
			if (k === "on") return (n: string, h: any) => (seen.on.push(n), t.on(n, h));
			if (k === "registerTool") return (d: any) => (seen.tools.push(d.name), t.registerTool(d));
			if (k === "appendEntry") return (c: string, d: any) => (seen.entries.push(c), t.appendEntry(c, d));
			const v = t[k];
			return typeof v === "function" ? v.bind(t) : v;
		},
	});
	const t0 = performance.now();
	const bridge = await import(process.env.CATALY_PROBE_BRIDGE!);
	bridge.default(proxy);
	seen.loadMs = performance.now() - t0;
	dump();
	for (const e of ["session_start", "agent_end", "session_shutdown"]) pi.on(e, dump);
}
"#;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// One prompt through an ordinary (non-Cataly) session; returns once OMP has exited.
fn ordinary_turn(mut omp: Rpc) {
    let p =
        omp.send(json!({"type": "prompt", "message": "hello", "streamingBehavior": "followUp"}));
    assert_eq!(omp.result_of(&p)["status"], "completed");
    omp.close();
}

/// OMP runs in which the bridge's load cost is measured; the bound applies to the fastest.
const LOAD_RUNS: usize = 7;

/// Acceptance check 11 and the bridge contract case (PATCH-12 "bridge load and inert behaviour"):
/// in an ordinary OMP session the installed bridge registers only the session start and switch
/// handlers, writes nothing and never connects; its own load cost inside OMP is measured, and OMP's
/// start-up with and without the link.
#[test]
fn bridge_in_stock_omp() {
    if !enabled() {
        return;
    }
    let sandbox = Sandbox::new("bridge");
    let mut linker = TestHub::new("");
    linker
        .env
        .push(("PI_CODING_AGENT_DIR".into(), sandbox.root.join("agent")));
    let link = linker.link_bridge();
    let target = std::fs::read_link(&link).unwrap();
    // A listener where the bridge would connect: any accept means it was not inert.
    let socket = sandbox.root.join("hub.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let probe = sandbox.root.join("probe.ts");
    std::fs::write(&probe, PROBE).unwrap();
    let out = sandbox.root.join("probe.json");
    let env = [
        ("CATALY_SOCKET", socket.clone()),
        ("CATALY_PROBE_OUT", out.clone()),
        ("CATALY_PROBE_BRIDGE", target.clone()),
    ];

    // Inert, observed through the probe (the link is set aside so the bridge loads once per run).
    // The load is measured in several OMP runs and the 5 ms bound applies to the fastest: one run
    // under host load can stall for longer while the bridge is fine (build notes, "Timing bounds
    // under load").
    let parked = sandbox.root.join("cataly.ts.parked");
    std::fs::rename(&link, &parked).unwrap();
    let sessions = format!("--session-dir={}", sandbox.root.join("sessions").display());
    let mut loads = Vec::with_capacity(LOAD_RUNS);
    for _ in 0..LOAD_RUNS {
        ordinary_turn(sandbox.omp_with(
            false,
            &["-e".into(), probe.display().to_string(), sessions.clone()],
            &env,
        ));
        let seen: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(
            seen["on"],
            json!(["session_start", "session_switch"]),
            "{seen}"
        );
        assert_eq!(seen["tools"], json!([]), "{seen}");
        assert_eq!(seen["entries"], json!([]), "{seen}");
        loads.push(seen["loadMs"].as_f64().unwrap());
    }
    let load_ms = loads.iter().copied().fold(f64::INFINITY, f64::min);

    // Inert through the real link, discovered by OMP itself.
    std::fs::rename(&parked, &link).unwrap();
    ordinary_turn(sandbox.omp_with(true, std::slice::from_ref(&sessions), &env));
    assert!(
        listener.accept().is_err(),
        "an ordinary session connected to the hub socket"
    );
    for body in sandbox.model.bodies() {
        let text = body.to_string();
        assert!(
            !text.contains("Cataly: your role"),
            "guidance in an ordinary session"
        );
        assert!(
            !text.contains("\"name\":\"cataly_lead\""),
            "hub tool in an ordinary session"
        );
    }
    for entry in std::fs::read_dir(sandbox.root.join("sessions")).unwrap() {
        let text = std::fs::read_to_string(entry.unwrap().path()).unwrap_or_default();
        assert!(
            !text.contains("\"customType\":\"cataly\""),
            "marker written"
        );
    }

    let [with, empty, none] = startup_medians(&sandbox, &link, &parked, &sessions, &env);
    eprintln!(
        "MEASURE bridge_load_in_omp_ms={load_ms:.2} (fastest of {loads:.2?}) omp_ready_ms \
         link={with:.1} empty_extension={empty:.1} no_extension={none:.1}; link minus empty={:.1}",
        with - empty
    );
    assert!(
        load_ms < 5.0,
        "bridge import and factory took {load_ms:.2} ms at best ({loads:.2?})"
    );
    assert!(
        listener.accept().is_err(),
        "a start-up run connected to the hub socket"
    );
    drop(linker);
}

/// OMP start-up (spawn to `ready`) medians, interleaved: with the bridge link; with only an empty
/// extension (OMP's own cost of loading any extension); with no extension at all.
fn startup_medians(
    sandbox: &Sandbox,
    link: &Path,
    parked: &Path,
    sessions: &str,
    env: &[(&str, PathBuf)],
) -> [f64; 3] {
    let noop = sandbox.root.join("agent/extensions/noop.ts");
    let noop_parked = sandbox.root.join("noop.ts.parked");
    std::fs::write(
        &noop_parked,
        "export default function noop(_pi: unknown) {}\n",
    )
    .unwrap();
    let mut runs: [Vec<f64>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    std::fs::rename(link, parked).unwrap();
    for n in 0..30 {
        let variant = n % 3;
        match variant {
            0 => std::fs::rename(parked, link).unwrap(),
            1 => {
                std::fs::rename(link, parked).unwrap();
                std::fs::rename(&noop_parked, &noop).unwrap();
            }
            _ => std::fs::rename(&noop, &noop_parked).unwrap(),
        }
        let started = Instant::now();
        let omp = sandbox.omp_with(true, &[sessions.to_string()], env);
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        omp.close();
        runs[variant].push(ms);
    }
    std::fs::rename(parked, link).unwrap();
    runs.map(median)
}

/// The bridge contract case for an accepted lead: it connects, the hub's tool is offered and the
/// guidance is in the system prompt; the session gets the Cataly marker.
#[test]
fn bridge_accepted_lead_in_stock_omp() {
    if !enabled() {
        return;
    }
    let lead_box = Sandbox::new("bridgelead");
    let mut hub = lead_hub(&lead_box, "fake/primary", "");
    hub.start();
    let live = wait_lead(&hub, |v| {
        v["phase"] == "live" && v["bridge"]["connected"] == true
    });
    assert!(
        live["bridge"]["session"] == live["session"]["session_id"],
        "{live}"
    );
    assert!(hub.cli(&["lead", "send", "bridge check"]).status.success());
    let last = hub.until(Duration::from_secs(60), || {
        let bodies = lead_box.model.bodies();
        let last = bodies.last()?.to_string();
        last.contains("bridge check").then_some(last)
    });
    wait_lead(&hub, |v| v["sent"] == 0 && v["waiting"] == 0);
    assert!(last.contains("Cataly: your role"), "no guidance: {last}");
    // The bridge registers the tool `essential`, so OMP offers it as a top-level tool even with its
    // default `tools.commands`, never as a command mounted in `bash` (DESIGN 12.2 A13).
    assert!(
        last.contains("\"name\":\"cataly_lead\""),
        "not a top-level tool: {last}"
    );
    let file = live["session"]["session_file"].as_str().unwrap();
    let text = std::fs::read_to_string(file).unwrap();
    assert!(
        text.contains("\"customType\":\"cataly\""),
        "no session marker"
    );
    assert!(hub.stop().success());
}

/// Resident memory of one process in bytes (Linux `/proc`); `None` elsewhere or once it is gone.
fn rss_of(pid: u64) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let kb = status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(kb * 1024)
}

/// The lead's sleep with stock OMP and its bridge: an idle lead sleeps (its OMP ends) and an owner
/// message resumes the same session. Measures the lead OMP's RSS awake, the hub's RSS awake and
/// asleep, and per wake the time from `lead send` to the lead live again, to the bridge of the same
/// session accepted by the hub, and to the model request carrying the message.
#[test]
fn lead_sleeps_and_wakes_against_stock_omp() {
    if !enabled() {
        return;
    }
    let sandbox = Sandbox::new("sleep");
    let mut hub = lead_hub(&sandbox, "fake/primary", "");
    hub.env
        .push(("CATALY_IDLE_SLEEP".into(), PathBuf::from("4s")));
    hub.start();
    let first = wait_lead(&hub, |v| {
        v["phase"] == "live" && v["bridge"]["connected"] == true
    });
    let session = first["session"]["session_id"].clone();
    for n in 0..5 {
        let marker = format!("SLEEP-{n}");
        let awake = wait_lead(&hub, |v| {
            v["phase"] == "live" && v["sent"] == 0 && v["waiting"] == 0
        });
        assert!(
            hub.cli(&["lead", "send", &format!("hello {marker}")])
                .status
                .success()
        );
        hub.until(Duration::from_secs(60), || {
            body_with(&sandbox.model, &marker)
        });
        wait_lead(&hub, |v| {
            v["sent"] == 0 && v["waiting"] == 0 && v["session"]["status"] == "idle"
        });
        let pid = awake["session"]["pid"].as_u64().unwrap();
        let omp_awake = rss_of(pid).unwrap_or(0);
        let hub_awake = hub.json(&["stats", "--json"])["rss_bytes"].clone();
        let slept = wait_lead(&hub, |v| v["phase"] == "asleep");
        assert_eq!(slept["asleep"]["session_id"], session, "{slept}");
        hub.until(Duration::from_secs(30), || {
            rss_of(pid).is_none().then_some(())
        });
        std::thread::sleep(Duration::from_secs(1));
        let hub_asleep = hub.json(&["stats", "--json"])["rss_bytes"].clone();
        eprintln!(
            "MEASURE cycle={n} omp_awake_rss={omp_awake} hub_awake_rss={hub_awake} hub_asleep_rss={hub_asleep}"
        );

        let wake_marker = format!("WAKE-{n}");
        let sent = Instant::now();
        assert!(
            hub.cli(&["lead", "send", &format!("wake {wake_marker}")])
                .status
                .success()
        );
        let (mut live, mut bridge, mut model) = (None, None, None);
        loop {
            let v = hub.json(&["lead", "status", "--json"]);
            if live.is_none() && v["phase"] == "live" {
                assert_eq!(v["session"]["session_id"], session, "{v}");
                live = Some(sent.elapsed());
            }
            if bridge.is_none()
                && v["bridge"]["connected"] == true
                && v["bridge"]["session"] == session
            {
                bridge = Some(sent.elapsed());
            }
            if model.is_none() && body_with(&sandbox.model, &wake_marker).is_some() {
                model = Some(sent.elapsed());
            }
            if live.is_some() && bridge.is_some() && model.is_some() {
                break;
            }
            assert!(sent.elapsed() < Duration::from_secs(60), "no wake: {v}");
        }
        let ms = |d: Option<Duration>| d.map_or(0.0, |d| d.as_secs_f64() * 1000.0);
        eprintln!(
            "MEASURE wake={n} send_to_live_ms={:.0} send_to_bridge_ms={:.0} send_to_model_ms={:.0}",
            ms(live),
            ms(bridge),
            ms(model)
        );
    }
    assert!(hub.stop().success());
}

/// The lead's tool name as the hub registers it.
const LEAD_TOOL: &str = "cataly_lead";

/// Every tool result the fake model was sent: call ID, tool name and content, in order.
fn tool_results(model: &FakeModel) -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = Vec::new();
    for body in model.bodies() {
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        for (call, result) in messages.iter().zip(messages.iter().skip(1)) {
            let call = &call["tool_calls"][0];
            let id = result["tool_call_id"].as_str().unwrap_or_default();
            if result["role"] == "tool" && !out.iter().any(|(seen, ..)| seen == id) {
                let name = call["function"]["name"].as_str().unwrap_or_default();
                out.push((id.into(), name.into(), result["content"].to_string()));
            }
        }
    }
    out
}

/// The lead's open approval dialogs.
fn dialogs(hub: &TestHub) -> Vec<Value> {
    let v = hub.json(&["lead", "status", "--json"]);
    v["session"]["dialogs"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// Sends the owner message that makes the fake model call `tool` with `args` once and returns that
/// call's result once the turn has settled. Nobody answers dialogs here, so a call that asked could
/// never return; one that opens a dialog fails at once.
fn lead_calls(hub: &TestHub, model: &FakeModel, tool: &str, args: &Value) -> String {
    let before = tool_results(model).len();
    let text = format!("Run this.\nCALL {tool} {args}");
    assert!(hub.cli(&["lead", "send", &text]).status.success());
    let result = hub.until(Duration::from_secs(60), || {
        let asked = dialogs(hub);
        assert!(asked.is_empty(), "{tool} {args} asked: {asked:?}");
        let all = tool_results(model);
        all.get(before..)?
            .iter()
            .find(|(_, name, _)| name == tool)
            .map(|(.., content)| content.clone())
    });
    wait_lead(hub, |v| v["sent"] == 0 && v["waiting"] == 0);
    eprintln!("lead {tool} {args}: {result}");
    result
}

/// Like [`lead_calls`] for a `bash` line that must ask: returns the approval dialog OMP opened
/// and the call's result after the dialog is answered Deny.
fn lead_asks(hub: &TestHub, model: &FakeModel, command: &str) -> (Value, String) {
    let before = tool_results(model).len();
    let text = format!("Run this.\nCALL bash {}", json!({ "command": command }));
    assert!(hub.cli(&["lead", "send", &text]).status.success());
    let dialog = hub.until(Duration::from_secs(60), || dialogs(hub).first().cloned());
    let id = dialog["id"].as_str().unwrap();
    let deny = hub.cli(&["lead", "answer", id, r#"{"value":"Deny"}"#]);
    assert!(deny.status.success(), "{deny:?}");
    let result = hub.until(Duration::from_secs(60), || {
        let all = tool_results(model);
        all.get(before..)?
            .iter()
            .find(|(_, name, _)| name == "bash")
            .map(|(.., content)| content.clone())
    });
    wait_lead(hub, |v| v["sent"] == 0 && v["waiting"] == 0);
    eprintln!("lead bash {command}: asked {dialog}, then {result}");
    (dialog, result)
}

/// The owner's lines from the first runs of 0.1.1, where every one asked for approval: the bridge
/// registers the lead's tool `essential`, so stock OMP 19.1.1 offers it only as a direct call that
/// runs without asking (its `policy: allow`), and the hub's shell veto refuses a `bash` line that
/// names it, before OMP's approval, with a pointer to the direct call. Approvals follow the owner's
/// own OMP settings (decision 19, DESIGN 12.2 A13): under OMP's default a command runs without a
/// dialog; with the owner's `tools.approvalMode: always-ask` a write asks and the dialog reaches
/// the hub. Rule 0 still refuses the CLI's `lead send` in the lead's shell; OMP's user deny still
/// refuses `eval`.
#[test]
fn lead_follows_the_owners_omp_approval_settings_in_stock_omp() {
    if !enabled() {
        return;
    }
    let lead_box = Sandbox::new("leadtool");
    let work = lead_box.root.join("work");
    std::fs::write(work.join("notes.txt"), "READ-ME-OK\n").unwrap();
    let mut hub = lead_hub(&lead_box, "fake/primary", "");
    hub.start();
    wait_lead(&hub, |v| {
        v["phase"] == "live" && v["bridge"]["connected"] == true
    });
    let model = &lead_box.model;
    // The bash form, as the owner's lead wrote it: refused by the hub without a dialog.
    for line in [
        format!("{LEAD_TOOL} onboarding.start 2>&1; echo ---; {LEAD_TOOL} requests 2>&1"),
        format!("{LEAD_TOOL} question.ask --help 2>&1 | head -30"),
        format!("{LEAD_TOOL} --help 2>&1 | head -60"),
        format!(
            "{LEAD_TOOL} request.close --id 1 --text \"done\" 2>&1; {LEAD_TOOL} request.next --id 2 --text \"next\" 2>&1"
        ),
    ] {
        let shell = lead_calls(&hub, model, "bash", &json!({ "command": line }));
        assert!(
            shell.contains("cataly_lead is a tool, not a shell command"),
            "{shell}"
        );
        assert!(!shell.contains("Onboarding runs"), "{shell}");
    }
    // The direct call is offered and runs without asking.
    assert!(
        model
            .bodies()
            .last()
            .is_some_and(|b| b.to_string().contains("\"name\":\"cataly_lead\""))
    );
    let start = json!({ "op": "onboarding.start" });
    let direct = lead_calls(&hub, model, LEAD_TOOL, &start);
    assert!(direct.contains("Onboarding runs"), "{direct}");
    let direct = lead_calls(&hub, model, LEAD_TOOL, &json!({ "op": "requests" }));
    assert!(
        direct.contains("request-log") || direct.contains("next:"),
        "{direct}"
    );
    // OMP's default (no approval setting of the owner's): commands run without a dialog.
    for (command, shows) in [("cat notes.txt", "READ-ME-OK"), ("ls -la", "notes.txt")] {
        let read = lead_calls(&hub, model, "bash", &json!({ "command": command }));
        assert!(read.contains(shows), "{command}: {read}");
    }
    let wrote = lead_calls(
        &hub,
        model,
        "bash",
        &json!({ "command": "echo made > made.txt" }),
    );
    assert!(work.join("made.txt").exists(), "{wrote}");
    // The CLI stays refused in the lead's shell, and the refusal names the tool to use.
    let cli = json!({ "command": "cataly lead send hi" });
    let refused = lead_calls(&hub, model, "bash", &cli);
    assert!(
        refused.contains("rule 0") && refused.contains(LEAD_TOOL),
        "{refused}"
    );
    let cell = json!({ "language": "js", "code": "console.log('EVAL' + '-RAN')" });
    let eval = lead_calls(&hub, model, "eval", &cell);
    assert!(
        eval.contains("blocked by user policy") && !eval.contains("EVAL-RAN"),
        "{eval}"
    );
    // The owner's own OMP setting applies: a write asks, the dialog reaches the hub, Deny holds.
    assert!(hub.stop().success());
    std::fs::write(
        lead_box.root.join("agent/config.yml"),
        "tools:\n  approvalMode: always-ask\n",
    )
    .unwrap();
    hub.start();
    wait_lead(&hub, |v| {
        v["phase"] == "live" && v["bridge"]["connected"] == true
    });
    let (dialog, denied) = lead_asks(&hub, model, "echo again > again.txt");
    assert!(
        dialog["title"]
            .as_str()
            .is_some_and(|t| t.contains("echo again > again.txt")),
        "{dialog}"
    );
    assert!(!work.join("again.txt").exists(), "{denied}");
    assert!(hub.stop().success());
}
