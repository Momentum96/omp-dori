//! Job engine tests through the real hub: condition rules, job verbs, destinations, broken-off runs
//! after a crash, and child termination on a graceful stop.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::time::Duration;

use support::{TestHub, probe};

/// `job run` is one dry tick; the condition rules apply to dry state the same way as live.
#[test]
fn condition_job_fires_on_change_to_success() {
    let mut hub = TestHub::new("");
    let flag = hub.root.join("flag");
    let argv = format!(r#"["sh", "-c", "cat {}"]"#, flag.display());
    hub.write_config(&probe("site", &argv, "enabled = false\n"));
    hub.start();
    let tick = |hub: &TestHub| -> Vec<String> {
        let v = hub.json(&["job", "run", "site", "--json"]);
        v["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["kind"].as_str().unwrap().to_string())
            .collect()
    };
    assert!(tick(&hub).is_empty(), "failure: nothing, the job is primed");
    std::fs::write(&flag, "up").unwrap();
    assert_eq!(tick(&hub), ["probe.fired"], "change to success fires");
    assert!(tick(&hub).is_empty(), "still succeeding: no repeat");
    std::fs::remove_file(&flag).unwrap();
    assert_eq!(tick(&hub), Vec::<String>::new());
    std::fs::write(&flag, "up").unwrap();
    assert_eq!(tick(&hub), ["probe.fired"]);
    assert_eq!(hub.job("site")["state"], "disabled");
    hub.stop();
}

#[test]
fn every_success_with_identical_output_skipped() {
    let mut hub = TestHub::new("");
    let file = hub.root.join("out");
    std::fs::write(&file, "a").unwrap();
    let argv = format!(r#"["cat", "{}"]"#, file.display());
    hub.write_config(&probe(
        "feed",
        &argv,
        "enabled = false\nfire = \"every\"\nskip_identical = true\n",
    ));
    hub.start();
    let fired = |hub: &TestHub| {
        hub.json(&["job", "run", "feed", "--json"])["events"]
            .as_array()
            .unwrap()
            .len()
    };
    assert_eq!(fired(&hub), 1);
    assert_eq!(fired(&hub), 0, "identical output is skipped");
    std::fs::write(&file, "b").unwrap();
    assert_eq!(fired(&hub), 1);
    hub.stop();
}

#[test]
fn job_verbs_and_command_destination() {
    let mut hub = TestHub::new("");
    let sink = hub.root.join("delivered");
    let config = format!(
        "[destinations.note]\nkind = \"command\"\nargv = [\"sh\", \"-c\", \"printf %s \\\"$CATALY_EVENT\\\" >> {}\"]\n{}",
        sink.display(),
        probe(
            "ping",
            r#"["echo", "pong"]"#,
            "destinations = [\"note\"]\ntemplates = { \"probe.fired\" = \"{job} answered\" }\n"
        )
    );
    hub.write_config(&config);
    hub.start();
    // The live first probe fires and is delivered to the command destination.
    let delivered = hub.until(Duration::from_secs(10), || {
        std::fs::read_to_string(&sink)
            .ok()
            .filter(|s| !s.is_empty())
    });
    let event: serde_json::Value = serde_json::from_str(&delivered).unwrap();
    assert_eq!(
        (event["kind"].as_str(), event["text"].as_str()),
        (Some("probe.fired"), Some("ping answered"))
    );
    let job = hub.job("ping");
    assert_eq!(job["kind"], "probe");
    assert_eq!(job["last_run"]["outcome"], "fired");
    assert!(job["next_due_ms"].as_i64().unwrap() > 0);

    // A dry run is never delivered.
    let dry = hub.json(&["job", "run", "ping", "--json"]);
    assert!(
        dry["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["dry"] == true)
    );
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        std::fs::read_to_string(&sink)
            .unwrap()
            .matches("probe.fired")
            .count(),
        1
    );

    assert!(hub.cli(&["job", "disable", "ping"]).status.success());
    assert_eq!(hub.job("ping")["state"], "on hold");
    assert!(hub.cli(&["job", "enable", "ping"]).status.success());
    assert_eq!(hub.job("ping")["state"], "waiting for trigger");
    let missing = hub.cli(&["job", "disable", "nope"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("no job named 'nope'"));
    let text = String::from_utf8(hub.cli(&["job", "list"]).stdout).unwrap();
    assert!(text.starts_with("ping"), "{text}");
    hub.stop();
}

#[test]
fn hub_crash_mid_run_breaks_the_run_off_and_holds_the_job() {
    let mut hub = TestHub::new(&probe("slow", r#"["sleep", "30"]"#, ""));
    hub.start();
    // Running and durably recorded (the run record precedes the child).
    hub.until(Duration::from_secs(10), || {
        let job = hub.job("slow");
        (job["state"] == "running"
            && job["last_run"]["id"].is_number()
            && job["last_run"]["outcome"].is_null())
        .then_some(())
    });
    hub.crash();
    hub.start();
    let job = hub.job("slow");
    assert_eq!(
        (job["state"].as_str(), job["needs_ack"].as_bool()),
        (Some("on hold"), Some(true))
    );
    assert_eq!(job["last_run"]["outcome"], "broken off");
    assert!(hub.event_kinds().contains(&"run.broken".to_string()));
    let refused = hub.cli(&["job", "enable", "slow"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--ack"));
    assert!(
        hub.cli(&["job", "enable", "slow", "--ack"])
            .status
            .success()
    );
    assert_eq!(hub.job("slow")["state"], "waiting for trigger");
    hub.stop();
}

#[test]
fn graceful_stop_terminates_running_children() {
    let mut hub = TestHub::new("");
    let pidfile = hub.root.join("child.pid");
    let argv = format!(
        r#"["sh", "-c", "echo $$ > {}; exec sleep 60"]"#,
        pidfile.display()
    );
    hub.write_config(&probe("slow", &argv, ""));
    hub.start();
    let pid = hub.until(Duration::from_secs(10), || {
        std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
    });
    let started = std::time::Instant::now();
    assert!(hub.stop().success());
    assert!(started.elapsed() < Duration::from_secs(10));
    let alive = std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .unwrap()
        .success();
    assert!(!alive, "the job child got TERM");
    hub.start();
    let job = hub.job("slow");
    assert_eq!(
        (job["state"].as_str(), job["needs_ack"].as_bool()),
        (Some("on hold"), Some(true))
    );
    hub.stop();
}

#[test]
fn timed_job_runs_and_late_one_shot_is_skipped() {
    let at = jiff::Timestamp::now() + jiff::SignedDuration::from_secs(2);
    let config = format!(
        "[jobs.soon]\nkind = \"once\"\nat = \"{}\"\nargv = [\"echo\", \"ran\"]\ncwd = \"/\"\n\
         [jobs.missed]\nkind = \"once\"\nat = \"2020-01-01T00:00:00Z\"\nargv = [\"true\"]\ncwd = \"/\"\n",
        at.strftime("%Y-%m-%dT%H:%M:%SZ")
    );
    let mut hub = TestHub::new(&config);
    hub.start();
    hub.until(Duration::from_secs(15), || {
        hub.event_kinds()
            .contains(&"run.done".to_string())
            .then_some(())
    });
    let done = hub
        .events()
        .into_iter()
        .find(|e| e["kind"] == "run.done")
        .unwrap();
    assert_eq!(done["text"], "soon: succeeded, 4 bytes of output");
    assert_eq!(done["body"], "ran\n");
    // The run's registry entry clears just after its event is appended.
    hub.until(Duration::from_secs(5), || {
        (hub.job("soon")["state"] == "consumed").then_some(())
    });
    let missed = hub.job("missed");
    assert_eq!(
        (missed["state"].as_str(), missed["skipped"].as_i64()),
        (Some("consumed"), Some(1))
    );
    let refused = hub.cli(&["job", "enable", "soon"]);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("already consumed"));
    hub.stop();
}
