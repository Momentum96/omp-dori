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

fn restart_with_slow_lead(persistent_failure: bool) -> (TestHub, Value) {
    let mut hub = TestHub::new("");
    let omp = hub.install_fake("omp");
    hub.fixture("omp", "rpc", Some(""));
    hub.fixture("omp", "turns", Some(""));
    let work = hub.root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let config = format!(
        "[omp]\npath = \"{}\"\n[lead]\nmodel = \"fake/primary\"\ncwd = \"{}\"\n",
        omp.display(),
        work.display()
    );
    hub.write_config(&config);
    hub.start();
    let first = live(&hub, &Value::Null);
    assert!(hub.stop().success());
    hub.fixture("omp", "slow-resume", Some(""));
    if persistent_failure {
        hub.fixture("omp", "fail-resume", Some(""));
        let id = first["session"]["session_id"].as_str().unwrap();
        hub.write_config(&format!("{config}session_id = \"{id}\"\n"));
    }
    hub.start();
    (hub, first)
}

#[test]
fn hub_restart_recovers_from_a_lead_ready_timeout_on_the_same_transcript() {
    let (mut hub, first) = restart_with_slow_lead(false);
    assert!(hub.cli(&["lead", "send", "after-restart"]).status.success());
    let resumed = live(&hub, &first["session"]["pid"]);
    assert_eq!(
        resumed["session"]["session_id"],
        first["session"]["session_id"]
    );
    let file = resumed["session"]["session_file"].as_str().unwrap();
    assert_eq!(file, first["session"]["session_file"].as_str().unwrap());
    let pids = std::fs::read_to_string(hub.root.join("bin/omp-data/resume-pids")).unwrap();
    assert_eq!(pids.lines().count(), 2, "one failed start and one retry");
    let failed = pids.lines().next().unwrap();
    let child = Command::new("kill").args(["-0", failed]).output().unwrap();
    assert!(
        !child.status.success(),
        "failed OMP {failed} is still running"
    );
    hub.until(Duration::from_secs(5), || {
        std::fs::read_to_string(file)
            .unwrap()
            .contains("after-restart")
            .then_some(())
    });
    assert!(hub.stop().success());
    assert_eq!(
        std::fs::read_to_string(file)
            .unwrap()
            .matches("after-restart")
            .count(),
        1
    );
}

#[test]
fn hub_shutdown_cancels_a_lead_ready_retry_and_next_hub_can_resume() {
    let (mut hub, first) = restart_with_slow_lead(true);
    hub.until(Duration::from_secs(25), || {
        let lead = hub.json(&["lead", "status", "--json"]);
        let reason = lead["reason"].as_str()?;
        (reason.contains("Code(7)") && reason.contains("restarting in 2 s")).then_some(())
    });
    assert!(hub.stop().success());
    let pids = hub.root.join("bin/omp-data/resume-pids");
    assert_eq!(std::fs::read_to_string(&pids).unwrap().lines().count(), 2);
    hub.fixture("omp", "fail-resume", None);
    hub.start();
    let resumed = live(&hub, &first["session"]["pid"]);
    assert_eq!(
        resumed["session"]["session_id"],
        first["session"]["session_id"]
    );
    assert_eq!(
        resumed["session"]["session_file"],
        first["session"]["session_file"]
    );
    assert!(hub.stop().success());
    assert_eq!(std::fs::read_to_string(pids).unwrap().lines().count(), 3);
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
            "{bin} config reload; echo \"reload=$?\"\n\
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
    for refused in ["reload=1", "enable=1", "setup=1", "untokened=1"] {
        assert!(text.contains(refused), "{refused}: {text}");
    }
    assert!(text.contains("status=0"), "{text}");
    assert!(text.contains("owner's alone"), "{text}");
    assert!(text.contains("only the owner may run this"), "{text}");

    // The test process is a plain shell of the owner.
    let reloaded = hub.cli(&["config", "reload"]);
    assert!(
        reloaded.status.success(),
        "{}",
        String::from_utf8_lossy(&reloaded.stderr)
    );
    let me = hub.request(r#"{"v":1,"id":1,"method":"whoami"}"#);
    assert_eq!(me["ok"]["role"], "owner", "{me}");
    // A launch token outside every session: reads only.
    let tokened = hub.request(r#"{"v":1,"id":2,"method":"config.reload","launch":"t"}"#);
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

/// A lead hub on the fake OMP running turns, with fixtures `extra` and the idle sleep shortened to
/// `sleep` (`CATALY_IDLE_SLEEP`, debug builds only); `lead` adds `[lead]` lines, `config` sections.
fn sleepy_hub(fixtures: &[&str], sleep: &str, lead: &str, config: &str) -> TestHub {
    let mut hub = TestHub::new("");
    let omp = hub.install_fake("omp");
    for f in ["rpc", "turns"].iter().chain(fixtures) {
        hub.fixture("omp", f, Some(""));
    }
    let work = hub.root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    hub.env
        .push(("CATALY_IDLE_SLEEP".into(), std::path::PathBuf::from(sleep)));
    hub.write_config(&format!(
        "[omp]\npath = \"{}\"\n[lead]\nmodel = \"fake/primary\"\ncwd = \"{}\"\n{lead}{config}",
        omp.display(),
        work.display()
    ));
    hub
}

fn lead_view(hub: &TestHub) -> Value {
    hub.json(&["lead", "status", "--json"])
}

fn asleep(hub: &TestHub) -> Value {
    hub.until(Duration::from_secs(20), || {
        let v = lead_view(hub);
        (v["phase"] == "asleep").then_some(v)
    })
}

fn running(pid: &Value) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn omp_starts(hub: &TestHub) -> usize {
    std::fs::read_to_string(hub.root.join("bin/omp-data/pids"))
        .unwrap()
        .lines()
        .count()
}

/// The lead stays live and on the same process for `secs` seconds (several idle sleeps).
fn stays_awake(hub: &TestHub, pid: &Value, secs: u64) {
    let until = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < until {
        let v = lead_view(hub);
        assert_eq!(v["phase"], "live", "{v}");
        assert_eq!(&v["session"]["pid"], pid, "{v}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The owner's lines carrying `marker` in a session file.
fn user_entries(file: &Value, marker: &str) -> usize {
    std::fs::read_to_string(file.as_str().unwrap())
        .unwrap()
        .lines()
        .filter(|l| l.contains("\"role\":\"user\"") && l.contains(marker))
        .count()
}

/// An idle lead sleeps: its OMP ends and the session is kept, shown in `lead status`, `status` and
/// `health`. A hub restart (the update path) keeps it asleep without starting OMP; an owner message
/// then wakes it on the same session and is delivered exactly once, and it sleeps again.
#[test]
fn an_idle_lead_sleeps_stays_asleep_over_a_restart_and_wakes_once_on_its_session() {
    let mut hub = sleepy_hub(&[], "2s", "", "");
    hub.start();
    let first = live(&hub, &Value::Null);
    let (session, file) = (
        first["session"]["session_id"].clone(),
        first["session"]["session_file"].clone(),
    );
    let slept = asleep(&hub);
    assert_eq!(slept["asleep"]["session_id"], session, "{slept}");
    assert!(slept["session"].is_null(), "{slept}");
    hub.until(Duration::from_secs(10), || {
        (!running(&first["session"]["pid"])).then_some(())
    });
    let status = hub.json(&["status", "--json"]);
    assert!(
        status["lead"].as_str().unwrap().contains("asleep since"),
        "{status}"
    );
    let health: Value = serde_json::from_slice(&hub.cli(&["health", "--json"]).stdout).unwrap();
    let line = health["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "lead")
        .cloned()
        .unwrap();
    assert_eq!(line["level"], "ok", "{line}");
    assert!(line["detail"].as_str().unwrap().starts_with("asleep since"));
    let starts = omp_starts(&hub);

    // The orderly stop and start an update makes: the new hub keeps the lead asleep.
    assert!(hub.stop().success());
    hub.start();
    let resumed = hub.until(Duration::from_secs(20), || {
        let r = hub.json(&["status", "--json"])["resumed"].clone();
        r.is_object().then_some(r)
    });
    assert_eq!(resumed["lead_start"], "not_running", "{resumed}");
    assert!(resumed["lead_asleep_ms"].is_i64(), "{resumed}");
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(lead_view(&hub)["phase"], "asleep");
    // The new hub's OMP probe is the only start.
    assert_eq!(
        omp_starts(&hub),
        starts + 1,
        "no OMP started for a sleeping lead"
    );

    let sent = Instant::now();
    let out = hub.cli(&["lead", "send", "WAKE-1", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let receipt: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(receipt["state"], "waiting", "{receipt}");
    let woke = live(&hub, &first["session"]["pid"]);
    assert_eq!(woke["session"]["session_id"], session, "{woke}");
    assert_eq!(woke["session"]["session_file"], file, "{woke}");
    hub.until(Duration::from_secs(20), || {
        (user_entries(&file, "WAKE-1") > 0).then_some(())
    });
    eprintln!(
        "MEASURE fake_omp send_to_entry_ms={}",
        sent.elapsed().as_millis()
    );
    let again = asleep(&hub);
    assert_eq!(again["asleep"]["session_id"], session, "{again}");
    assert_eq!(user_entries(&file, "WAKE-1"), 1, "delivered exactly once");
    assert_eq!(
        (again["waiting"].clone(), again["sent"].clone()),
        (Value::from(0), Value::from(0))
    );
    assert!(
        !hub.events().iter().any(|e| e["kind"] == "lead.refused"),
        "the resume was never refused"
    );
    assert!(hub.stop().success());
}

/// An open question card keeps the lead awake; once it is answered the lead sleeps.
#[test]
fn an_open_card_keeps_the_lead_awake() {
    let mut hub = sleepy_hub(&["ask"], "1s", "", "");
    hub.start();
    let first = live(&hub, &Value::Null);
    assert!(hub.cli(&["lead", "send", "ASK pick one"]).status.success());
    hub.until(Duration::from_secs(10), || {
        let v = lead_view(&hub);
        v["session"]["dialogs"]
            .as_array()
            .is_some_and(|d| !d.is_empty())
            .then_some(())
    });
    stays_awake(&hub, &first["session"]["pid"], 4);
    let answered = hub.cli(&["lead", "answer", "d1", r#"{"data":{"color":"Blue"}}"#]);
    assert!(answered.status.success(), "{answered:?}");
    asleep(&hub);
    assert!(hub.stop().success());
}

/// A delivery in doubt (written, never found in the session file) keeps the lead awake.
#[test]
fn a_delivery_in_doubt_keeps_the_lead_awake() {
    let mut hub = sleepy_hub(&["noentry"], "1s", "", "");
    hub.start();
    let first = live(&hub, &Value::Null);
    assert!(hub.cli(&["lead", "send", "LOST-1"]).status.success());
    hub.until(Duration::from_secs(90), || {
        let v = lead_view(&hub);
        (v["in_doubt"].as_u64() > Some(0)).then_some(())
    });
    stays_awake(&hub, &first["session"]["pid"], 4);
    assert!(hub.stop().success());
}

/// An open lane keeps the lead awake, with the lead's inbox empty.
#[test]
fn an_open_lane_keeps_the_lead_awake() {
    let work = "/tmp";
    let mut hub = sleepy_hub(
        &[],
        "1s",
        "",
        &format!("[lanes]\nmodel = \"fake/primary\"\ncwd = \"{work}\"\n"),
    );
    hub.start();
    let first = live(&hub, &Value::Null);
    let params = serde_json::json!({
        "key": "awake-1",
        "title": "Work",
        "brief": "Do the work.",
        "checks": [{ "kind": "command", "argv": ["true"] }],
    })
    .to_string();
    let opened = hub.cli(&["lane", "open", "--json", "--params", &params]);
    assert!(opened.status.success(), "{opened:?}");
    // The lane's start note (quiet: it waits for the lead's next turn, and alone would not keep
    // the lead awake) shows the lane started.
    hub.until(Duration::from_secs(20), || {
        (lead_view(&hub)["notes"].as_u64() > Some(0)).then_some(())
    });
    stays_awake(&hub, &first["session"]["pid"], 4);
    assert!(hub.stop().success());
}

/// Quiet notes wait for the lead's next turn, also through a sleep: a lead with only quiet notes
/// pending sleeps, and the notes go with its first turn after the wake.
#[test]
fn quiet_notes_wait_through_a_sleep_and_go_with_the_first_turn() {
    let mut hub = sleepy_hub(&[], "2s", "", "");
    hub.start();
    let first = live(&hub, &Value::Null);
    // An exit by itself brings the lead back on its session with a quiet resume note.
    kill(&first["session"]["pid"]);
    let second = live(&hub, &first["session"]["pid"]);
    let file = second["session"]["session_file"].clone();
    let slept = asleep(&hub);
    assert_eq!(slept["notes"], 1, "{slept}");
    assert_eq!(user_entries(&file, "resume note"), 0);
    assert!(hub.cli(&["lead", "send", "WAKE-Q"]).status.success());
    let woke = live(&hub, &second["session"]["pid"]);
    assert_eq!(
        woke["session"]["session_id"],
        first["session"]["session_id"]
    );
    hub.until(Duration::from_secs(20), || {
        (user_entries(&file, "WAKE-Q") > 0).then_some(())
    });
    let text = std::fs::read_to_string(file.as_str().unwrap()).unwrap();
    let turn = text
        .lines()
        .find(|l| l.contains("\"role\":\"user\"") && l.contains("WAKE-Q"))
        .unwrap();
    assert!(
        turn.contains("resume note"),
        "one turn carries both: {turn}"
    );
    assert_eq!(user_entries(&file, "resume note"), 1);
    assert!(hub.stop().success());
}

/// `idle_sleep = "0s"` turns sleep off.
#[test]
fn zero_idle_sleep_never_sleeps() {
    let mut hub = sleepy_hub(&[], "1s", "idle_sleep = \"0s\"\n", "");
    hub.start();
    let first = live(&hub, &Value::Null);
    stays_awake(&hub, &first["session"]["pid"], 5);
    assert!(hub.stop().success());
}
