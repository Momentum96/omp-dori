//! The lead through the real hub with the fake OMP in its rpc-ui mode (`tests/support/fake-omp`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::Value;
use support::TestHub;

fn kill(pid: &Value) {
    let status = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Waits until the lead is live on a running process other than `not`.
fn live(hub: &TestHub, not: &Value) -> Value {
    hub.until(Duration::from_secs(20), || {
        let v = hub.json(&["lead", "status", "--json"]);
        let pid = &v["session"]["pid"];
        (v["phase"] == "live" && pid.is_u64() && pid != not).then_some(v)
    })
}

/// DESIGN 2.2 supervision for the lead: when its OMP exits by itself the hub resumes the same session
/// after 1 s, then 2 s; the owner is alerted at the first exit only and the lead gets one resume
/// note; `lead stop` cancels a planned restart.
#[test]
fn lead_restarts_on_its_session_after_omp_exits() {
    let mut hub = TestHub::new("");
    let omp = hub.install_fake("omp");
    hub.fixture("omp", "rpc", Some(""));
    let work = hub.root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    hub.write_config(&format!(
        "[omp]\npath = \"{}\"\n[lead]\nmodel = \"fake/primary\"\ncwd = \"{}\"\n",
        omp.display(),
        work.display()
    ));
    hub.start();
    let first = live(&hub, &Value::Null);
    let session = first["session"]["session_id"].clone();
    let file = first["session"]["session_file"].clone();
    assert!(session.is_string(), "{first}");

    kill(&first["session"]["pid"]);
    let exited = Instant::now();
    let second = live(&hub, &first["session"]["pid"]);
    assert!(
        exited.elapsed() >= Duration::from_secs(1),
        "the first retry waits 1 s"
    );
    assert_eq!(second["session"]["session_id"], session, "{second}");
    assert_eq!(second["session"]["session_file"], file);

    kill(&second["session"]["pid"]);
    let exited = Instant::now();
    let third = live(&hub, &second["session"]["pid"]);
    assert!(
        exited.elapsed() >= Duration::from_secs(2),
        "the second retry waits 2 s"
    );
    assert_eq!(third["session"]["session_id"], session, "{third}");

    // Events are recorded after the phase they report turns live: wait for the third start's record.
    let resumed = |events: &[Value]| {
        events
            .iter()
            .filter(|e| {
                e["kind"] == "lead.started" && e["text"].as_str().unwrap().contains("resumed")
            })
            .count()
    };
    let events = hub.until(Duration::from_secs(10), || {
        let events = hub.events();
        (resumed(&events) == 2).then_some(events)
    });
    let stopped: Vec<&Value> = events
        .iter()
        .filter(|e| e["kind"] == "session.stopped")
        .collect();
    assert_eq!(stopped.len(), 2, "{events:?}");
    assert_eq!(
        (stopped[0]["urgent"].clone(), stopped[1]["urgent"].clone()),
        (Value::Bool(true), Value::Bool(false)),
        "the owner is alerted once per series"
    );
    assert!(
        stopped[0]["text"]
            .as_str()
            .unwrap()
            .contains("restarting in 1 s")
    );
    assert!(
        stopped[1]["text"]
            .as_str()
            .unwrap()
            .contains("restarting in 2 s")
    );
    let inbox = hub.json(&["lead", "inbox", "--json"]);
    let notes: Vec<&Value> = inbox
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["preview"].as_str().unwrap().contains("resume note"))
        .collect();
    assert_eq!(notes.len(), 1, "the lead is told once: {inbox}");

    // A third exit plans a restart in 4 s; `lead stop` cancels it.
    kill(&third["session"]["pid"]);
    hub.until(Duration::from_secs(10), || {
        let v = hub.json(&["lead", "status", "--json"]);
        v["reason"]
            .as_str()
            .is_some_and(|r| r.contains("restarting in 4 s"))
            .then_some(())
    });
    let stopped = hub.json(&["lead", "stop", "--json"]);
    assert_eq!(stopped["reason"], "the automatic restart was cancelled");
    assert!(hub.stop().success());
    let pids = std::fs::read_to_string(hub.root.join("bin/omp-data/pids")).unwrap();
    // The probe, then three lead processes; nothing started after the cancel.
    assert_eq!(pids.lines().count(), 4, "{pids}");
}

/// Decision 15 writes early only what the caller knows was not written before (a relay message
/// read live, or one its transcript lacks). A `lead send` with a request ID commits first: the
/// crash point after an early write never fires, and a retry with the same ID after a crash finds
/// its receipt, so the lead gets the message once.
#[test]
fn request_id_sends_commit_before_the_write() {
    let mut hub = TestHub::new("");
    let omp = hub.install_fake("omp");
    hub.fixture("omp", "rpc", Some(""));
    hub.fixture("omp", "turns", Some(""));
    let work = hub.root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    hub.write_config(&format!(
        "[omp]\npath = \"{}\"\n[lead]\nmodel = \"fake/primary\"\ncwd = \"{}\"\n",
        omp.display(),
        work.display()
    ));
    hub.env.push((
        "CATALY_FAULT".into(),
        std::path::PathBuf::from("wake_after_write"),
    ));
    hub.start();
    let lead = live(&hub, &Value::Null);
    let file = std::path::PathBuf::from(lead["session"]["session_file"].as_str().unwrap());
    let send = |hub: &TestHub| {
        hub.json(&[
            "lead",
            "send",
            "hello once",
            "--request-id",
            "cli-1",
            "--json",
        ])
    };
    let first = send(&hub);
    assert!(first["inbox_id"].is_i64(), "{first}");
    assert!(
        hub.cli(&["status"]).status.success(),
        "the hub is still up: the write came after the commit"
    );
    hub.crash();
    hub.env.clear();
    hub.start();
    live(&hub, &lead["session"]["pid"]);
    let again = send(&hub);
    assert_eq!(again["inbox_id"], first["inbox_id"], "{again}");
    std::thread::sleep(Duration::from_secs(1));
    let entries = std::fs::read_to_string(&file)
        .unwrap()
        .matches("hello once")
        .count();
    assert_eq!(entries, 1, "written once");
    assert!(hub.stop().success());
}
