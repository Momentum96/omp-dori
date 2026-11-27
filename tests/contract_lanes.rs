//! Contract suite, lane cases of slice 3a: stock OMP lanes started by the hub against a loopback fake
//! model provider, in an isolated agent directory and home. Runs only with `CATALY_CONTRACT_OMP=1`
//! (`CATALY_CONTRACT_OMP_PATH` selects the executable).
//!
//! Cases: one open-report-claim-close cycle, the report sent by the lane's own model through the
//! bridge's lane tool; and acceptance check 10, a hub restart with three lanes (two with active
//! goals resumed, one paused stays paused) and the recap. Both print the slice-3a measurements.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use support::model::{FakeModel, models_yml};
use support::release::{
    V1, bump, command_link, hub_version, installed_hub, release, run, stop_hub, text,
    trust_test_key, variant,
};
use support::{BIN, TestHub};

const WAIT: Duration = Duration::from_secs(90);

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

/// The isolated OMP profile (agent directory with the fake provider, work directory); removed with
/// the hub's own sandbox when the test ends.
struct Lanes {
    hub: TestHub,
    model: FakeModel,
}

impl Lanes {
    fn new() -> Self {
        Self::with_config("")
    }

    /// The sandbox with `extra` appended to the config (`{work}` names the work directory).
    fn with_config(extra: &str) -> Self {
        let hub = TestHub::new("");
        let agent = hub.root.join("agent");
        let work = hub.root.join("work");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        let model = FakeModel::start();
        std::fs::write(
            agent.join("models.yml"),
            models_yml(model.port, &["primary"]),
        )
        .unwrap();
        let mut hub = hub;
        hub.env.push(("PI_CODING_AGENT_DIR".into(), agent));
        let extra = extra.replace("{work}", &work.display().to_string());
        hub.write_config(&format!(
            "[omp]\npath = \"{}\"\n[sessions]\nextra_flags = [\"--no-skills\", \"--no-rules\", \
             \"--no-context-files\"]\n[lanes]\nmodel = \"fake/primary\"\ncwd = \"{}\"\nobjection = \"2s\"\n\
             [lanes.shell]\ndeny = [\"echo cataly-denied.*\"]\n{extra}",
            installed_omp().display(),
            work.display()
        ));
        // The bridge linked into the sandbox's agent directory, as an install does.
        hub.link_bridge();
        Self { hub, model }
    }

    fn start(&mut self) {
        self.hub.start();
        self.hub.until(WAIT, || {
            self.hub.json(&["status", "--json"])["omp"]
                .is_object()
                .then_some(())
        });
    }

    fn lane(&self, op: &str, params: &Value) -> Result<Value, String> {
        let out = self
            .hub
            .cli(&["lane", op, "--json", "--params", &params.to_string()]);
        if out.status.success() {
            Ok(serde_json::from_slice(&out.stdout).unwrap())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).into_owned())
        }
    }

    fn open(&self, key: &str) -> Value {
        let p = json!({
            "key": key,
            "title": format!("Work {key}"),
            "brief": "Do the work.",
            "checks": [{ "kind": "command", "argv": ["true"] }],
        });
        self.lane("open", &p).unwrap()
    }

    fn record(&self, key: &str) -> Value {
        let all = self.lane("list", &json!({ "all": true })).unwrap();
        all.as_array()
            .unwrap()
            .iter()
            .find(|l| l["key"] == key)
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// The goal status the lane's live session reads back.
    fn goal(&self, key: &str) -> Option<String> {
        let read = self.lane("read", &json!({ "key": key, "count": 1 })).ok()?;
        read["goal"]["status"].as_str().map(str::to_lowercase)
    }

    fn rss(&self) -> u64 {
        self.hub.json(&["stats", "--json"])["rss_bytes"]
            .as_u64()
            .unwrap()
    }
}

/// One lane through its whole life in stock OMP: open, a report from the lane's model through the
/// lane tool, a claim, the objection window and the automatic close.
#[test]
fn open_report_claim_close_in_stock_omp() {
    if !enabled() {
        return;
    }
    let mut l = Lanes::new();
    l.start();
    let opened = l.open("cycle-1");
    assert!(opened.to_string().contains("started"), "{opened}");
    let rec = l.hub.until(WAIT, || {
        let r = l.record("cycle-1");
        (r["live"] == true && r["state"] == "running").then_some(r)
    });
    let report = json!({ "op": "report", "kind": "progress", "text": "built the first part" });
    let sent = l.lane(
        "send",
        &json!({ "key": "cycle-1", "text": format!("Report now.\nCALL cataly_lane {report}") }),
    );
    assert!(sent.is_ok(), "{sent:?}");
    // Cataly's own tools carry an allow approval policy, so OMP runs the lane tool without asking.
    let reports = l.hub.until(WAIT, || {
        let r = l.lane("reports", &json!({ "key": "cycle-1" })).ok()?;
        r.as_array().is_some_and(|a| !a.is_empty()).then_some(r)
    });
    assert!(
        reports.to_string().contains("built the first part"),
        "{reports}"
    );
    let read = l.lane("read", &json!({ "key": "cycle-1" })).unwrap();
    assert!(
        read["dialogs"].as_array().is_none_or(Vec::is_empty),
        "{read}"
    );
    l.lane(
        "claim",
        &json!({ "key": "cycle-1", "evidence": "report 1" }),
    )
    .unwrap();
    let closed = l.hub.until(WAIT, || {
        let r = l.record("cycle-1");
        (r["state"] == "closed").then_some(r)
    });
    assert_eq!(closed["outcome"], "completed", "{closed}");
    assert!(
        closed["receipt"].to_string().contains("pass command true"),
        "{closed}"
    );
    assert_eq!(closed["session_id"], rec["session_id"]);
    l.model.assert_eval_not_offered();
    assert!(l.hub.stop().success());
}

/// Acceptance check 10 with stock OMP, and the lane start time and RSS per lane.
#[test]
fn hub_restart_brings_three_stock_lanes_back() {
    if !enabled() {
        return;
    }
    let mut l = Lanes::new();
    l.start();
    let keys = ["r-1", "r-2", "r-3"];
    let base = l.rss();
    for key in keys {
        l.open(key);
    }
    let before: Vec<Value> = l.hub.until(WAIT, || {
        let r: Vec<Value> = keys.iter().map(|k| l.record(k)).collect();
        r.iter()
            .all(|x| x["live"] == true && x["state"] == "running")
            .then_some(r)
    });
    let with = l.rss();
    let stats = l.hub.json(&["stats", "--json"]);
    eprintln!(
        "MEASURE lane_start={} hub_rss_base={base} hub_rss_three_lanes={with}",
        stats["lane_start_latency"]
    );
    for key in &keys[..2] {
        l.hub.until(WAIT, || {
            (l.goal(key).as_deref() == Some("active")).then_some(())
        });
    }
    l.lane(
        "pause",
        &json!({ "key": "r-3", "reason": "owner said wait" }),
    )
    .unwrap();
    assert_eq!(l.goal("r-3").as_deref(), Some("paused"));
    assert!(l.hub.stop().success());
    l.start();
    let after: Vec<Value> = l.hub.until(WAIT, || {
        let r: Vec<Value> = keys.iter().map(|k| l.record(k)).collect();
        r.iter().all(|x| x["live"] == true).then_some(r)
    });
    for (b, a) in before.iter().zip(&after) {
        assert_eq!(a["session_id"], b["session_id"], "{a}");
        assert_eq!(a["session_file"], b["session_file"], "{a}");
    }
    for key in &keys[..2] {
        l.hub.until(WAIT, || {
            (l.goal(key).as_deref() == Some("active")).then_some(())
        });
    }
    assert_eq!(l.goal("r-3").as_deref(), Some("paused"));
    assert_eq!(l.record("r-3")["state"], "held");
    let recap = l.hub.until(WAIT, || {
        let r: Vec<Value> = l
            .hub
            .events()
            .into_iter()
            .filter(|e| e["kind"] == "lane.recap")
            .collect();
        (!r.is_empty()).then_some(r)
    });
    let text = recap[0]["text"].as_str().unwrap();
    assert!(text.contains("r-3: restarted; stays held"), "{text}");
    assert!(text.contains("r-1: restarted; goal active"), "{text}");
    assert!(l.model.seen().iter().all(|m| m == "primary"));
    assert!(l.hub.stop().success());
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

/// A plain `cataly update` with live sessions in stock OMP: the new hub resumes the lead and both
/// lanes on their exact session files, an active goal stays active and a paused one paused, the
/// next prompt of each session carries its earlier turn, and the CLI says which sessions came back.
#[test]
fn update_resumes_the_lead_and_lanes_with_their_history() {
    if !enabled() {
        return;
    }
    let mut l = Lanes::with_config("[lead]\nmodel = \"fake/primary\"\ncwd = \"{work}\"\n");
    trust_test_key(&mut l.hub);
    let hub = &l.hub;
    let v2 = bump(V1, 1);
    let r1 = release(&hub.root.join("r1"), &variant(V1, false), V1);
    let r2 = release(&hub.root.join("r2"), &variant(&v2, false), &v2);
    // The installed bridge replaces the sandbox's own link.
    std::fs::remove_file(hub.root.join("agent/extensions/cataly.ts")).unwrap();
    let out = run(
        hub,
        BIN.as_ref(),
        &["install", "--from", r1.to_str().unwrap()],
    );
    assert!(out.status.success(), "{}", text(&out));

    let mut old = installed_hub(hub);
    hub.until(WAIT, || {
        (hub_version(hub).as_deref() == Some(V1)).then_some(())
    });
    hub.until(WAIT, || {
        hub.json(&["status", "--json"])["omp"]
            .is_object()
            .then_some(())
    });
    let lead = |h: &TestHub| h.json(&["lead", "status", "--json"]);
    let settled =
        |v: &Value| v["sent"] == 0 && v["waiting"] == 0 && v["session"]["status"] == "idle";
    hub.until(WAIT, || (lead(hub)["phase"] == "live").then_some(()));
    assert!(
        hub.cli(&["lead", "send", "remember PELICAN-1"])
            .status
            .success()
    );
    let lead_before = hub.until(WAIT, || {
        let v = lead(hub);
        (settled(&v) && body_with(&l.model, "PELICAN-1").is_some()).then_some(v)
    });
    for key in ["u-1", "u-2"] {
        l.open(key);
        hub.until(WAIT, || {
            (l.goal(key).as_deref() == Some("active")).then_some(())
        });
    }
    l.lane(
        "pause",
        &json!({ "key": "u-2", "reason": "owner said wait" }),
    )
    .unwrap();
    let lanes_before: Vec<Value> = ["u-1", "u-2"].iter().map(|k| l.record(k)).collect();

    let out = run(
        hub,
        &command_link(hub),
        &["update", "--from", r2.to_str().unwrap()],
    );
    assert!(out.status.success(), "{}", text(&out));
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    eprintln!("cataly update: {said}");
    assert!(
        said.contains("lead를 이어서 열었습니다. lane 2개를 이어서 열었습니다: u-1, u-2."),
        "{said}"
    );
    let _ = old.wait();
    assert_eq!(hub_version(hub).as_deref(), Some(v2.as_str()));

    let lead_after = hub.until(WAIT, || {
        let v = lead(hub);
        (v["phase"] == "live").then_some(v)
    });
    for field in ["session_id", "session_file"] {
        assert_eq!(
            lead_after["session"][field], lead_before["session"][field],
            "{lead_after}"
        );
    }
    for before in &lanes_before {
        let key = before["key"].as_str().unwrap();
        let after = l.record(key);
        assert_eq!(after["session_id"], before["session_id"], "{after}");
        assert_eq!(after["session_file"], before["session_file"], "{after}");
        assert_eq!(after["live"], true, "{after}");
    }
    hub.until(WAIT, || {
        (l.goal("u-1").as_deref() == Some("active")).then_some(())
    });
    assert_eq!(l.goal("u-2").as_deref(), Some("paused"));

    // The conversations came back: each next prompt carries the earlier turn.
    assert!(
        hub.cli(&["lead", "send", "and now PELICAN-2"])
            .status
            .success()
    );
    let next = hub.until(WAIT, || body_with(&l.model, "PELICAN-2"));
    assert!(next.contains("PELICAN-1"), "the lead's history: {next}");
    l.lane(
        "send",
        &json!({ "key": "u-1", "text": "next step HERON-2" }),
    )
    .unwrap();
    let next = hub.until(WAIT, || body_with(&l.model, "HERON-2"));
    assert!(next.contains("Work u-1"), "the lane's history: {next}");
    stop_hub(hub);
}

/// The text OMP returned to the model for each tool call it made, by tool name, over every request.
fn tool_results(model: &FakeModel) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for body in model.bodies() {
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        for (call, result) in messages.iter().zip(messages.iter().skip(1)) {
            let name = call["tool_calls"][0]["function"]["name"].as_str();
            if let (Some(name), "tool") = (name, result["role"].as_str().unwrap_or_default()) {
                out.push((name.to_string(), result["content"].to_string()));
            }
        }
    }
    out
}

/// Safety 4 in stock OMP: a lane's subagent (the `task` tool) is held by the lane's rules. Its
/// bridge connects as `sub` with the lane session as parent, so the hub's shell rule blocks its
/// `bash`; and the overlay leaves `eval` unavailable in the subagent.
#[test]
fn a_lane_subagent_is_held_by_shell_rules_and_eval_deny_in_stock_omp() {
    if !enabled() {
        return;
    }
    let mut l = Lanes::new();
    l.start();
    l.open("sub-1");
    l.hub.until(WAIT, || {
        let r = l.record("sub-1");
        (r["live"] == true && r["state"] == "running").then_some(())
    });
    let calls = [
        (
            "bash",
            json!({ "command": "echo cataly-denied-in-subagent" }),
        ),
        (
            "eval",
            json!({ "language": "js", "code": "console.log('EVAL' + '-RAN')" }),
        ),
    ];
    for (name, args) in &calls {
        let task = json!({ "context": "Contract case.", "tasks": [{
            "name": format!("Try{name}"), "solutionSpace": "given",
            "task": format!("Run this.\nCALL {name} {args}"),
        }]});
        let text = format!("Spawn a worker.\nCALL task {task}");
        let sent = l.lane("send", &json!({ "key": "sub-1", "text": text }));
        assert!(sent.is_ok(), "{sent:?}");
        let deadline = std::time::Instant::now() + WAIT;
        let result = loop {
            let all = tool_results(&l.model);
            if let Some((_, text)) = all.iter().find(|(tool, _)| tool == name) {
                break text.clone();
            }
            // The lane asks before it spawns (an `exec` tool); the owner allows every dialog, so a
            // subagent call that should be refused can only be refused by the rule under test.
            let read = l.lane("read", &json!({ "key": "sub-1", "count": 1 }));
            for dialog in read
                .iter()
                .flat_map(|r| r["dialogs"].as_array().cloned().unwrap_or_default())
            {
                let id = dialog["id"].as_str().unwrap_or_default();
                let answer =
                    json!({ "key": "sub-1", "dialog": id, "answer": { "value": "Approve" } });
                let _ = l.lane("answer", &answer);
            }
            if std::time::Instant::now() > deadline {
                let last: Vec<Value> = l
                    .model
                    .bodies()
                    .iter()
                    .map(|b| {
                        b["messages"]
                            .as_array()
                            .and_then(|m| m.last())
                            .cloned()
                            .unwrap_or_default()
                    })
                    .collect();
                panic!(
                    "no {name} result within {WAIT:?}; tool results {all:?}; lane {read:?}; models {:?}; last messages {last:#?}",
                    l.model.seen()
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        eprintln!("subagent {name} result: {result}");
        assert!(!result.contains("EVAL-RAN"), "{result}");
        if *name == "bash" {
            assert!(result.contains("Blocked by shell rule"), "{result}");
        } else {
            assert!(
                result.contains("blocked by user policy") || result.contains("not found"),
                "{result}"
            );
        }
    }
    l.model.assert_eval_not_offered();
    assert!(l.hub.stop().success());
}
