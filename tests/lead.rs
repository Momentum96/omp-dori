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

/// Just after an OMP update the first probe can outlast its bound under load. A failed probe is
/// tried again by itself after 5 s, with no change to the executable; status, health and the
/// sessions refusal name the retry, and the lead starts as soon as a probe works.
#[test]
fn a_failed_probe_is_retried_and_the_lead_starts() {
    let mut hub = TestHub::new("");
    let omp = hub.install_fake("omp");
    hub.fixture("omp", "rpc", Some(""));
    hub.fixture("omp", "slow-start", Some(""));
    let work = hub.root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    hub.write_config(&format!(
        "[omp]\npath = \"{}\"\n[lead]\nmodel = \"fake/primary\"\ncwd = \"{}\"\n",
        omp.display(),
        work.display()
    ));
    hub.start();
    let status = hub.until(Duration::from_secs(40), || {
        let s = hub.json(&["status", "--json"]);
        s["omp"]["retry"].is_object().then_some(s)
    });
    let report = &status["omp"];
    assert!(report["error"].is_string(), "{status}");
    assert_eq!(report["retry"]["attempt"], 2, "{status}");
    assert_eq!(report["retry"]["after_s"], 5, "{status}");
    let refused = status["sessions_refused"].as_str().unwrap();
    assert!(
        refused.starts_with("OMP probe failed: ")
            && refused.ends_with("; probing again in 5 s (attempt 2)"),
        "{refused}"
    );
    let health = String::from_utf8(hub.cli(&["health"]).stdout).unwrap();
    assert!(
        health
            .lines()
            .any(|l| l.contains("omp") && l.contains("probing again")),
        "{health}"
    );

    let lead = live(&hub, &Value::Null);
    assert!(lead["session"]["session_id"].is_string(), "{lead}");
    let status = hub.json(&["status", "--json"]);
    assert!(
        status["omp"]["error"].is_null() && status["omp"]["retry"].is_null(),
        "{status}"
    );
    assert!(hub.root.join("bin/omp-data/slow-start.used").exists());
    assert!(hub.stop().success());
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

/// The hub works from the real location of its state directory. On macOS `/tmp` and `/var/folders`
/// sit behind a symlink into `/private`; a transcript path recorded through that symlink differs
/// from its real location, the exact-resume check refused it, and the lead started a fresh session
/// after every hub restart. Here the state directory is reached through a symlink on any system.
#[test]
fn lead_resumes_through_a_symlinked_state_directory() {
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
    let link = hub.root.join("state-link");
    std::os::unix::fs::symlink(hub.root.join("state"), &link).unwrap();
    hub.env.push(("XDG_STATE_HOME".into(), link));
    hub.start();
    let first = live(&hub, &Value::Null);
    hub.crash();
    hub.start();
    let second = live(&hub, &first["session"]["pid"]);
    assert_eq!(
        second["session"]["session_id"], first["session"]["session_id"],
        "{second}"
    );
    assert_eq!(
        second["session"]["session_file"],
        first["session"]["session_file"]
    );
    assert!(hub.stop().success());
}

/// The usage OMP reports: one `fake` limit at `percent`, resetting in 2100.
fn usage(percent: u32) -> String {
    serde_json::json!({
        "generatedAt": 1, "reports": [{ "provider": "fake", "fetchedAt": 1,
            "limits": [{ "id": "five", "label": "5h", "resetsAt": 4_102_444_800_000_i64,
                         "amount": { "used": percent, "limit": 100, "unit": "percent" } }] }],
        "accountsWithoutUsage": [], "disabledCredentials": [], "capacity": {}
    })
    .to_string()
}

/// OJ-08 holds new sessions only. With the gate held on the lead's provider, a hub restart (as
/// after `cataly update`) resumes the lead's existing session, and so does `lead start`; a fresh
/// lead waits, and `lead start --fresh` and `lead status` name the provider, the limit, its reset
/// time and `cataly quota lift`.
#[test]
fn the_quota_gate_holds_a_fresh_lead_but_never_a_resume() {
    let mut hub = TestHub::new("");
    let omp = hub.install_fake("omp");
    hub.fixture("omp", "rpc", Some(""));
    hub.fixture("omp", "usage.json", Some(&usage(10)));
    let work = hub.root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let config = format!(
        "[omp]\npath = \"{}\"\n[lead]\nmodel = \"fake/primary\"\ncwd = \"{}\"\n\
         [jobs.q]\nkind = \"quota\"\nenabled = false\n",
        omp.display(),
        work.display()
    );
    hub.write_config(&config);
    hub.start();
    let first = live(&hub, &Value::Null);
    let session = first["session"]["session_id"].clone();
    assert!(session.is_string(), "{first}");
    assert!(hub.stop().success());

    // The first live quota tick, at this start, closes the gate on `fake` and stores it.
    hub.fixture("omp", "usage.json", Some(&usage(90)));
    hub.write_config(&config.replace("enabled = false\n", ""));
    hub.start();
    hub.until(Duration::from_secs(20), || {
        (hub.json(&["quota", "--json"])["providers"] == serde_json::json!(["fake"])).then_some(())
    });
    let second = live(&hub, &first["session"]["pid"]);
    assert_eq!(second["session"]["session_id"], session, "{second}");
    assert!(hub.stop().success());

    // The next hub restores the held gate before the lead starts: the lead still resumes.
    hub.start();
    assert_eq!(
        hub.json(&["quota", "--json"])["providers"],
        serde_json::json!(["fake"])
    );
    let third = live(&hub, &second["session"]["pid"]);
    assert_eq!(third["session"]["session_id"], session, "{third}");

    // A fresh lead is a new session: held, and said plainly. The stop is recorded as the child
    // exits, so the refusal is asked again until it is the last word on the stopped lead.
    let stopped = hub.cli(&["lead", "stop"]);
    assert!(stopped.status.success(), "{stopped:?}");
    let (said, status) = hub.until(Duration::from_secs(20), || {
        let fresh = hub.cli(&["lead", "start", "--fresh"]);
        assert!(!fresh.status.success(), "{fresh:?}");
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&fresh.stdout),
            String::from_utf8_lossy(&fresh.stderr)
        );
        let status = hub.json(&["lead", "status", "--json"]);
        (status["quota"]["provider"] == "fake").then_some((said, status))
    });
    for part in [
        "quota gate holds new sessions on fake",
        "fake#",
        "until 2100-01-01T00:00:00Z",
        "`cataly quota lift`",
    ] {
        assert!(said.contains(part), "{part}: {said}");
    }
    assert_eq!(
        status["phase"], "stopped",
        "the stopped lead stays stopped: {status}"
    );
    assert!(
        status["reason"]
            .as_str()
            .is_some_and(|r| r.contains("`cataly quota lift`")),
        "{status}"
    );

    // `lead start` without `--fresh` resumes the existing session through the held gate.
    let resumed = hub.cli(&["lead", "start"]);
    assert!(resumed.status.success(), "{resumed:?}");
    let fourth = live(&hub, &third["session"]["pid"]);
    assert_eq!(fourth["session"]["session_id"], session, "{fourth}");
    assert!(hub.json(&["lead", "status", "--json"])["quota"].is_null());
    assert!(hub.stop().success());
}

/// DESIGN 8: a process inside the lead's session (a child of its OMP) has the lead's role on the
/// control socket, even without its launch token, and is refused every owner-only method; it reads
/// status, and finds the hub through `CATALY_SOCKET` alone. A plain shell is the owner. A caller
/// that carries a launch token but descends from no session fails closed.
#[test]
fn a_session_child_is_refused_owner_methods_and_a_plain_shell_is_the_owner() {
    let mut hub = TestHub::new("");
    let omp = hub.install_fake("omp");
    hub.fixture("omp", "rpc", Some(""));
    let bin = support::BIN;
    hub.fixture(
        "omp",
        "child",
        Some(&format!(
            "{bin} quota lift; echo \"lift=$?\"\n\
             {bin} job enable x --ack; echo \"enable=$?\"\n\
             {bin} setup --yes --backup off; echo \"setup=$?\"\n\
             {bin} status --json > /dev/null; echo \"status=$?\"\n\
             env -u CATALY_LAUNCH {bin} voice restart; echo \"untokened=$?\"\n\
             echo done\n"
        )),
    );
    let work = hub.root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    hub.write_config(&format!(
        "[omp]\npath = \"{}\"\n[lead]\nmodel = \"fake/primary\"\ncwd = \"{}\"\n",
        omp.display(),
        work.display()
    ));
    hub.start();
    live(&hub, &Value::Null);
    let out = hub.root.join("bin/omp-data/child.out");
    let text = hub.until(Duration::from_secs(20), || {
        std::fs::read_to_string(&out)
            .ok()
            .filter(|t| t.contains("done"))
    });
    for refused in ["lift=1", "enable=1", "setup=1", "untokened=1"] {
        assert!(text.contains(refused), "{refused}: {text}");
    }
    assert!(text.contains("status=0"), "{text}");
    assert!(text.contains("owner's alone"), "{text}");
    assert!(text.contains("only the owner may run this"), "{text}");

    // The test process is a plain shell of the owner.
    let lifted = hub.cli(&["quota", "lift"]);
    assert!(
        lifted.status.success(),
        "{}",
        String::from_utf8_lossy(&lifted.stderr)
    );
    let me = hub.request(r#"{"v":1,"id":1,"method":"whoami"}"#);
    assert_eq!(me["ok"]["role"], "owner", "{me}");
    // A launch token outside every session: reads only.
    let tokened = hub.request(r#"{"v":1,"id":2,"method":"quota.lift","launch":"t"}"#);
    assert!(
        tokened["err"]["message"]
            .as_str()
            .unwrap()
            .contains("unplaced"),
        "{tokened}"
    );
    let read = hub.request(r#"{"v":1,"id":3,"method":"status","launch":"t"}"#);
    assert!(read["ok"].is_object(), "{read}");
    assert!(hub.stop().success());
}
