//! Hub process tests: single instance, stale socket, protocol versions, config reload, CLI verbs.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::time::Duration;

use support::{TestHub, exists, probe};

#[test]
fn second_hub_names_the_first_and_stale_socket_is_replaced() {
    let mut hub = TestHub::new("");
    hub.start();
    let first = hub.pid();
    let second = hub.cli(&["hub"]);
    assert_eq!(second.status.code(), Some(4));
    let err = String::from_utf8_lossy(&second.stderr);
    assert!(
        err.contains(&format!("already running (pid {first}")),
        "{err}"
    );
    assert_eq!(
        hub.json(&["status", "--json"])["pid"],
        first,
        "the first hub keeps serving"
    );

    // A killed hub leaves its socket file behind; the next start replaces it.
    hub.crash();
    assert!(exists(&hub.socket()));
    hub.start();
    assert_ne!(hub.pid(), first);
    assert_eq!(hub.json(&["status", "--json"])["pid"], hub.pid());
    assert!(hub.stop().success());
    assert!(!exists(&hub.socket()), "a clean stop removes the socket");
}

#[test]
fn protocol_version_mismatch_and_unknown_method() {
    let mut hub = TestHub::new("");
    hub.start();
    let reply = hub.request(r#"{"v":2,"id":5,"method":"status"}"#);
    assert_eq!(reply["id"], 5);
    assert_eq!(reply["err"]["code"], "version_mismatch");
    assert!(
        reply["err"]["message"]
            .as_str()
            .unwrap()
            .contains("restart the hub")
    );
    let reply = hub.request(r#"{"v":1,"id":6,"method":"launch.rockets","extra":1}"#);
    assert_eq!(reply["err"]["code"], "unknown_method");
    let reply = hub.request("this is not json");
    assert_eq!(reply["err"]["code"], "bad_request");
    let reply = hub.request(r#"{"v":1,"id":7,"method":"status","future_field":true}"#);
    assert_eq!(reply["id"], 7, "unknown fields are ignored");
    assert!(reply["ok"]["pid"].is_number());
    // An oversized line gets an error reply and the connection is closed.
    let big = format!(
        "{{\"v\":1,\"id\":8,\"method\":\"status\",\"pad\":\"{}\"}}\n",
        "x".repeat(1_100_000)
    );
    let stream = std::os::unix::net::UnixStream::connect(hub.socket()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let sender = std::thread::spawn(move || {
        // The hub may close before the whole line is written.
        let _ = std::io::Write::write_all(&mut writer, big.as_bytes());
    });
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).unwrap();
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["err"]["code"], "bad_request");
    sender.join().unwrap();
    hub.stop();
}

#[test]
fn status_stats_and_usage_errors() {
    let mut hub = TestHub::new("");
    let not_running = hub.cli(&["status"]);
    assert_eq!(not_running.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&not_running.stderr).contains("hub is not running"));
    hub.start();
    let status = hub.json(&["status", "--json"]);
    assert_eq!(status["protocol"], 1);
    assert!(status["config_error"].is_null());
    let text = String::from_utf8(hub.cli(&["status"]).stdout).unwrap();
    // The one configured job is `TestHub`'s disabled admission-floor job.
    assert!(
        text.starts_with("hub ") && text.contains("jobs     1 configured"),
        "{text}"
    );
    assert!(text.contains("\nlead     no lead configured"), "{text}");
    assert!(status["lead"].is_null());
    for _ in 0..5 {
        hub.cli(&["status"]);
    }
    let figures = hub.json(&["stats", "--json"]);
    assert!(figures["rss_bytes"].as_u64().unwrap() > 0);
    assert!(figures["control_latency"]["count"].as_u64().unwrap() >= 5);
    assert!(figures["store_commit_latency"]["count"].as_u64().unwrap() >= 1);
    let usage = hub.cli(&["job", "run"]);
    assert_eq!(usage.status.code(), Some(2));
    assert_eq!(String::from_utf8_lossy(&usage.stderr).lines().count(), 1);
    let help = hub.cli(&["--help"]);
    assert!(
        help.status.success() && String::from_utf8_lossy(&help.stdout).contains("config check")
    );
    hub.stop();
}

#[test]
fn invalid_config_is_refused_with_its_path_and_the_previous_stays() {
    let good = probe("watch", r#"["echo", "up"]"#, "");
    let mut hub = TestHub::new(&good);
    hub.start();
    let cases = [
        (
            format!("{good}colour = \"red\"\n"),
            "jobs.watch.colour: unknown setting",
        ),
        (
            good.replace("every = \"1h\"", "every = \"5s\""),
            "jobs.watch.every: must be between 30s and 86400s",
        ),
        (
            format!("{good}destinations = [\"pager\"]\n"),
            "jobs.watch.destinations[0]: unknown destination 'pager'",
        ),
        (
            format!("{good}templates = {{ \"probe.fired\" = \"{{job}} {{colour}}\" }}\n"),
            "jobs.watch.templates.probe.fired: unknown field '{colour}'",
        ),
    ];
    for (text, path_error) in &cases {
        hub.write_config(text);
        let check = hub.cli(&["config", "check"]);
        assert_eq!(check.status.code(), Some(1));
        assert!(
            String::from_utf8_lossy(&check.stderr).contains(path_error),
            "check: {path_error}"
        );
        let reload = hub.cli(&["config", "reload"]);
        assert_eq!(reload.status.code(), Some(1));
        let err = String::from_utf8_lossy(&reload.stderr);
        assert!(
            err.contains(path_error) && err.contains("previous one stays in force"),
            "{err}"
        );
        let status = hub.json(&["status", "--json"]);
        assert!(
            status["config_error"]
                .as_str()
                .unwrap()
                .contains(path_error)
        );
        // `watch` and `TestHub`'s admission-floor job.
        assert_eq!(status["jobs"], 2, "the previous config stays in force");
        assert_eq!(hub.job("watch")["trigger"], "probe every 3600s");
    }
    // Four distinct errors, each reported once; the fix is reported once.
    hub.write_config(&good);
    assert!(hub.cli(&["config", "reload"]).status.success());
    assert!(hub.json(&["status", "--json"])["config_error"].is_null());
    let kinds = hub.event_kinds();
    assert_eq!(kinds.iter().filter(|k| *k == "config.invalid").count(), 4);
    assert_eq!(kinds.iter().filter(|k| *k == "config.valid").count(), 1);
    // A hub refuses to start on an invalid file.
    hub.stop();
    hub.write_config("[hub]\nlog_level = \"loud\"\n");
    let refused = hub.cli(&["hub"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("hub.log_level"));
}

#[test]
fn config_check_reads_a_given_file() {
    let hub = TestHub::new("");
    let file = hub.root.join("candidate.toml");
    std::fs::write(&file, "[jobs.x]\nkind = \"cron\"\ncron = \"0 9 * * mon\"\nzone = \"Europe/Berlin\"\nargv = [\"true\"]\ncwd = \"/\"\n").unwrap();
    std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let v = hub.json(&[
        "config",
        "check",
        "--file",
        file.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(
        (v["valid"].as_bool(), v["jobs"].as_u64()),
        (Some(true), Some(1))
    );
    std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    let loose = hub.cli(&["config", "check", "--file", file.to_str().unwrap()]);
    assert_eq!(loose.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&loose.stderr).contains("group or other users have access"));
}

#[test]
fn events_since_and_follow() {
    let mut hub = TestHub::new(&probe("p", r#"["echo", "hello"]"#, ""));
    hub.start();
    // The probe runs at creation and fires once (a new job starts primed).
    hub.until(Duration::from_secs(10), || {
        hub.event_kinds()
            .contains(&"probe.fired".to_string())
            .then_some(())
    });
    let since = String::from_utf8(hub.cli(&["events", "--since", "1h"]).stdout).unwrap();
    assert!(
        since.contains("hub 0.1.0 started") && since.contains("p: condition met"),
        "{since}"
    );
    let fired = hub
        .events()
        .into_iter()
        .find(|e| e["kind"] == "probe.fired")
        .unwrap();
    assert_eq!(fired["body"], "hello\n");
    assert_eq!(fired["urgent"], true);

    let mut follower = hub
        .command(&["events", "--follow", "--json"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(follower.stdout.take().unwrap()).lines();
    std::thread::sleep(Duration::from_millis(300));
    assert!(hub.cli(&["job", "run", "p"]).status.success());
    let line = lines.next().unwrap().unwrap();
    let event: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(
        (event["kind"].as_str(), event["dry"].as_bool()),
        (Some("probe.fired"), Some(true))
    );
    follower.kill().unwrap();
    let _ = follower.wait();

    // The owner-readable event log holds every event once.
    let log = std::fs::read_to_string(hub.state().join("logs/events.jsonl")).unwrap();
    let ids: Vec<i64> = log
        .lines()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["id"]
                .as_i64()
                .unwrap()
        })
        .collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]) && !ids.is_empty());
    hub.stop();
}

#[test]
fn owner_only_files_and_directories() {
    use std::os::unix::fs::PermissionsExt;
    let mut hub = TestHub::new("");
    hub.start();
    let mode = |p: std::path::PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(hub.state()), 0o700);
    assert_eq!(mode(hub.state().join("cataly.db")), 0o600);
    assert_eq!(mode(hub.state().join("hub.lock")), 0o600);
    assert_eq!(mode(hub.state().join("logs/hub.log")), 0o600);
    assert_eq!(mode(hub.socket()), 0o600);
    assert_eq!(mode(hub.root.join("run/cataly")), 0o700);
    hub.stop();
}

/// `cataly setup --fallback-on --fallback-chain`: reported without a write, validated, then applied
/// with `--yes` against the report's hash; the hub's config parser takes the result (decision 7).
/// The config folder, made under the test's umask, is tightened rather than refused.
#[test]
fn setup_sets_the_fallback_chain() {
    let hub = TestHub::new("[hub]\nlog_level = \"info\"\n[telegram]\nowner = 7\n");
    let before = std::fs::read_to_string(hub.config_path()).unwrap();
    for bad in [
        ["--fallback-on", "sometimes"],
        ["--fallback-chain", "no-provider"],
    ] {
        let out = hub.cli(&["setup", bad[0], bad[1]]);
        assert!(!out.status.success(), "{bad:?} refused");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(bad[0]),
            "{bad:?}: the error names the flag"
        );
    }
    let args = [
        "setup",
        "--fallback-on",
        "usage-limit",
        "--fallback-chain",
        "fake/backup, other/model",
        "--backup",
        "off",
        "--json",
    ];
    let preview = hub.json(&args);
    assert_eq!(preview["action"], "update");
    assert_eq!(preview["valid"], true);
    assert_eq!(std::fs::read_to_string(hub.config_path()).unwrap(), before);
    let chain = preview["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["setting"] == "fallback.chains.default")
        .unwrap()
        .clone();
    assert_eq!(chain["value"], "fake/backup,other/model");
    assert_eq!(
        chain["source"],
        "this file, when the hub starts (no hub is running)"
    );
    let expect = preview["before"].as_str().unwrap().to_string();
    let mut apply = args.to_vec();
    apply.extend(["--yes", "--expect", &expect]);
    assert_eq!(hub.json(&apply)["applied"], true);
    let text = std::fs::read_to_string(hub.config_path()).unwrap();
    assert!(
        text.contains("[fallback]\non = \"usage-limit\"\n"),
        "{text}"
    );
    assert!(
        text.contains("[fallback.chains]\ndefault = [\"fake/backup\", \"other/model\"]\n"),
        "{text}"
    );
    let check = hub.cli(&["config", "check"]);
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
}

/// `cataly setup --lead-model --lead-cwd` writes `[lead]` (review 5): a bad value names its flag, and
/// the applied file passes `config check`.
#[test]
fn setup_sets_the_lead() {
    let hub = TestHub::new("[hub]\nlog_level = \"info\"\n[telegram]\nowner = 7\n");
    for bad in [["--lead-model", "opus"], ["--lead-cwd", "work"]] {
        let out = hub.cli(&["setup", bad[0], bad[1]]);
        assert!(!out.status.success(), "{bad:?} refused");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(bad[0]),
            "{bad:?}: the error names the flag"
        );
    }
    let preview = hub.json(&["setup", "--json", "--backup", "off"]);
    let lead = |report: &serde_json::Value| {
        report["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["setting"] == "lead.model")
            .unwrap()
            .clone()
    };
    assert_eq!(lead(&preview)["value"], "-");
    assert!(
        lead(&preview)["note"].as_str().unwrap().contains("no lead"),
        "{preview}"
    );
    let args = [
        "setup",
        "--lead-model",
        "anthropic/claude-opus-5-5",
        "--lead-cwd",
        "/srv/work",
        "--backup",
        "off",
        "--json",
    ];
    let preview = hub.json(&args);
    assert_eq!(preview["valid"], true);
    let expect = preview["before"].as_str().unwrap().to_string();
    let mut apply = args.to_vec();
    apply.extend(["--yes", "--expect", &expect]);
    let applied = hub.json(&apply);
    assert_eq!(applied["applied"], true);
    assert_eq!(lead(&applied)["value"], "anthropic/claude-opus-5-5");
    let text = std::fs::read_to_string(hub.config_path()).unwrap();
    assert!(
        text.contains("[lead]\nmodel = \"anthropic/claude-opus-5-5\"\ncwd = \"/srv/work\"\n"),
        "{text}"
    );
    let check = hub.cli(&["config", "check"]);
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
}
