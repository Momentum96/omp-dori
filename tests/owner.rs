//! Owner jobs end to end through the real hub, against fake `gh` and fake `omp` (milestone 1
//! acceptance checks 7, 8, 9, 10 and 12). Ticks are dry runs (`cataly job run`), which use the same
//! rules as live ticks on their own memory.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::process::Command;
use std::time::Duration;

use serde_json::{Value, json};
use support::TestHub;

/// The head commit of PR `number` in the fixtures.
fn head(number: u64) -> String {
    format!("{number:040x}")
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// A hub whose OMP is the fake one; `jobs` is appended to the config.
fn hub_with_fake_omp(jobs: impl Fn(&TestHub) -> String) -> TestHub {
    let hub = TestHub::new("");
    let omp = hub.install_fake("omp");
    let config = format!("[omp]\npath = \"{}\"\n\n{}", omp.display(), jobs(&hub));
    hub.write_config(&config);
    hub
}

fn gh_json(hub: &TestHub, file: &str, value: &Value) {
    hub.fixture("gh", &format!("{file}.json"), Some(&value.to_string()));
}

fn pr_row(number: u64, author: &str) -> Value {
    json!({
        "number": number,
        "title": format!("Change {number}"),
        "state": "OPEN",
        "url": format!("https://github.com/o/r/pull/{number}"),
        "author": { "login": author }
    })
}

/// Fixtures of one open PR with no activity.
fn quiet_pr(hub: &TestHub, number: u64) {
    let base = format!("repos_o_r_pulls_{number}");
    let sha = head(number);
    gh_json(
        hub,
        &base,
        &json!({ "number": number, "state": "open", "merged": false, "title": format!("Change {number}"),
                 "head": { "sha": sha } }),
    );
    for list in ["comments", "reviews", "commits"] {
        gh_json(hub, &format!("{base}_{list}"), &json!([]));
    }
    gh_json(
        hub,
        &format!("repos_o_r_issues_{number}_comments"),
        &json!([]),
    );
    gh_json(
        hub,
        &format!("repos_o_r_commits_{sha}_check-runs"),
        &json!({ "check_runs": [] }),
    );
    gh_json(
        hub,
        &format!("repos_o_r_commits_{sha}_status"),
        &json!({ "statuses": [] }),
    );
}

fn comment(id: u64, login: &str, body: &str) -> Value {
    json!({ "id": id, "user": { "login": login }, "body": body,
            "html_url": format!("https://github.com/o/r/pull/1#c{id}") })
}

/// Check 7: baseline reports nothing old; a new PR, a review by someone else and a failed check
/// each produce one event; own and bot comments do not.
#[test]
fn github_watch_reports_each_new_thing_once() {
    let mut hub = TestHub::new("");
    let gh = hub.install_fake("gh");
    let omp = hub.install_fake("omp");
    hub.write_config(&format!(
        "[omp]\npath = \"{}\"\n\n[jobs.gh]\nkind = \"github\"\nenabled = false\ngh = \"{}\"\nnotifications = false\nlogin = \"me\"\nbots = [\"ci-bot[bot]\"]\nrepos = [{{ repo = \"o/r\", authors = [\"me\"] }}]\n",
        omp.display(),
        gh.display()
    ));
    // Before the baseline: one PR with an old comment by someone else and an old failed check.
    gh_json(&hub, "pr-list", &json!([pr_row(1, "me")]));
    quiet_pr(&hub, 1);
    gh_json(
        &hub,
        "repos_o_r_issues_1_comments",
        &json!([comment(10, "alice", "old remark")]),
    );
    gh_json(
        &hub,
        &format!("repos_o_r_commits_{}_check-runs", head(1)),
        &json!({ "check_runs": [{ "id": 77, "name": "lint", "conclusion": "failure" }] }),
    );
    hub.start();
    assert_eq!(
        hub.tick("gh"),
        Vec::<String>::new(),
        "baseline: nothing old"
    );
    assert_eq!(hub.tick("gh"), Vec::<String>::new(), "nothing new");

    // A new PR appears.
    gh_json(&hub, "pr-list", &json!([pr_row(2, "me"), pr_row(1, "me")]));
    quiet_pr(&hub, 2);
    assert_eq!(hub.tick("gh"), ["github.pr"]);
    assert_eq!(hub.tick("gh"), Vec::<String>::new());

    // Own and bot comments are silent; a review by someone else is one event.
    gh_json(
        &hub,
        "repos_o_r_issues_1_comments",
        &json!([
            comment(10, "alice", "old remark"),
            comment(11, "me", "thanks"),
            comment(12, "ci-bot[bot]", "coverage 91%"),
        ]),
    );
    assert_eq!(hub.tick("gh"), Vec::<String>::new(), "own and bot comments");
    gh_json(
        &hub,
        "repos_o_r_pulls_1_reviews",
        &json!([{ "id": 20, "user": { "login": "bob" }, "state": "CHANGES_REQUESTED",
                  "body": "please rename", "html_url": "https://github.com/o/r/pull/1#r20" }]),
    );
    let events = hub.run_job("gh");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["kind"], "pr.feedback");
    assert!(events[0]["text"].as_str().unwrap().contains("bob"));
    assert_eq!(
        hub.tick("gh"),
        Vec::<String>::new(),
        "a review is reported once"
    );

    // A new failed check run on the head.
    gh_json(
        &hub,
        &format!("repos_o_r_commits_{}_check-runs", head(1)),
        &json!({ "check_runs": [
            { "id": 77, "name": "lint", "conclusion": "failure" },
            { "id": 78, "name": "tests", "conclusion": "failure" }
        ] }),
    );
    assert_eq!(hub.tick("gh"), ["pr.check_failed"]);
    assert_eq!(hub.tick("gh"), Vec::<String>::new());
    assert!(hub.stop().success());
}

fn usage(percent: u32, resets_at_ms: i64) -> String {
    json!({
        "generatedAt": now_ms(),
        "reports": [{ "provider": "acme", "fetchedAt": now_ms(),
            "limits": [{ "id": "five", "label": "5h", "resetsAt": resets_at_ms,
                         "amount": { "used": percent, "limit": 100, "unit": "percent" } }] }],
        "accountsWithoutUsage": [], "disabledCredentials": [], "capacity": {}
    })
    .to_string()
}

/// Check 8: a band rise warns once, the gate closes at 85 % and opens at the reported reset time
/// even while usage data stays unavailable.
#[test]
fn quota_warns_once_and_the_gate_opens_at_the_reset_time() {
    let mut hub =
        hub_with_fake_omp(|_| "[jobs.q]\nkind = \"quota\"\nenabled = false\n".to_string());
    let later = now_ms() + 3_600_000;
    hub.fixture("omp", "usage.json", Some(&usage(50, later)));
    hub.start();
    assert_eq!(hub.tick("q"), Vec::<String>::new(), "baseline");
    hub.fixture("omp", "usage.json", Some(&usage(72, later)));
    assert_eq!(hub.tick("q"), ["quota.level"]);
    assert_eq!(
        hub.tick("q"),
        Vec::<String>::new(),
        "same band: warned once"
    );

    let reset = now_ms() + 4_000;
    hub.fixture("omp", "usage.json", Some(&usage(86, reset)));
    let mut kinds = hub.tick("q");
    kinds.sort();
    assert_eq!(kinds, ["quota.hold", "quota.level"]);

    // Data becomes unavailable: the gate stays closed until the reported reset time, no longer.
    hub.fixture("omp", "usage.json", None);
    let kinds = hub.tick("q");
    assert!(!kinds.contains(&"quota.released".to_string()), "{kinds:?}");
    let wait = u64::try_from(reset - now_ms() + 300).unwrap_or(0);
    std::thread::sleep(Duration::from_millis(wait));
    let kinds = hub.tick("q");
    assert!(kinds.contains(&"quota.released".to_string()), "{kinds:?}");
    assert!(
        !hub.tick("q").contains(&"quota.released".to_string()),
        "released once"
    );
    assert!(hub.stop().success());
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Check 9: backup refuses a GitHub token and a 60 MiB file, names files and rule but never the
/// secret, and alerts again only when the file set changes. (That it never force-pushes is the
/// backup module's unit test against a remote that moved on.)
#[test]
fn backup_refuses_secrets_and_big_files_and_alerts_on_change_only() {
    let mut hub = TestHub::new("");
    let work = hub.root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    let omp = hub.install_fake("omp");
    hub.write_config(&format!(
        "[omp]\npath = \"{}\"\n\n[jobs.bk]\nkind = \"backup\"\nenabled = false\ndir = \"{}\"\nname = \"Backup\"\nemail = \"b@example.org\"\n",
        omp.display(),
        work.display()
    ));
    let token = format!("ghp_{}", "Zx9".repeat(12));
    std::fs::write(work.join("notes.md"), "plain\n").unwrap();
    std::fs::write(work.join("env.sh"), format!("export T={token}\n")).unwrap();
    hub.start();
    let events = hub.run_job("bk");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["kind"], "backup.refused");
    let text = events[0].to_string();
    assert!(
        text.contains("env.sh") && text.contains("GitHub token"),
        "{text}"
    );
    assert!(!text.contains(&token), "the secret itself is never shown");
    assert_eq!(
        hub.tick("bk"),
        Vec::<String>::new(),
        "same files: no repeat"
    );

    let big = std::fs::File::create(work.join("big.bin")).unwrap();
    big.set_len(60 * 1024 * 1024).unwrap();
    let events = hub.run_job("bk");
    assert_eq!(events.len(), 1, "{events:?}");
    let text = events[0].to_string();
    assert!(
        text.contains("big.bin") && text.contains("50 MiB"),
        "{text}"
    );
    assert!(hub.stop().success());
}

fn git_out(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap()
}

/// A hub with fake `omp` and `gh` (first on PATH) and a private config folder, as setup needs.
fn backup_sandbox() -> TestHub {
    let mut hub = TestHub::new("");
    let omp = hub.install_fake("omp");
    hub.install_fake("gh");
    hub.write_config(&format!("[omp]\npath = \"{}\"\n", omp.display()));
    std::fs::set_permissions(
        hub.root.join("config/cataly"),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let mut path = std::ffi::OsString::from(hub.root.join("bin"));
    path.push(":");
    path.push(support::path_without_omp());
    hub.env.push(("PATH".into(), path.into()));
    hub
}

/// Decision 16: `setup --backup` reads the gh login, refuses a public repository, only offers an
/// absent one and creates it private after `--create`, makes the state folder a git tree with its
/// ignore list and remote, and adds the job. Restore: snapshot, change, restore from a local bare
/// repository; the store opens (with its migrations) and the config is the one backed up.
#[test]
fn home_backup_setup_and_restore() {
    let mut hub = backup_sandbox();
    gh_json(
        &hub,
        "user",
        &json!({ "login": "owner", "name": "Owner", "id": 7 }),
    );
    let calls =
        || std::fs::read_to_string(hub.root.join("bin/gh-data/calls.log")).unwrap_or_default();
    let state = hub.state();

    gh_json(
        &hub,
        "repos_owner_cataly-home",
        &json!({ "private": false }),
    );
    let out = hub.cli(&["setup", "--backup", "--json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("public"),
        "{out:?}"
    );
    hub.fixture("gh", "repos_owner_cataly-home.json", None);

    let preview = hub.json(&["setup", "--backup", "--json"]);
    assert_eq!(preview["backup"]["state"], "absent", "{preview}");
    assert_eq!(preview["backup"]["repository"], "owner/cataly-home");
    let digest = preview["before"].as_str().unwrap().to_string();
    let out = hub.cli(&["setup", "--backup", "--apply", "--expect", &digest]);
    assert!(!out.status.success(), "an absent repository needs --create");
    assert!(!calls().contains("repo create"));
    assert!(!state.join(".git").exists());

    let out = hub.cli(&[
        "setup", "--backup", "--apply", "--create", "--expect", &digest,
    ]);
    assert!(out.status.success(), "{out:?}");
    assert!(calls().contains("repo create owner/cataly-home --private"));
    let ignore = std::fs::read_to_string(state.join(".gitignore")).unwrap();
    for line in [
        "*token*",
        "*.db",
        "!/store/cataly.db",
        "*-wal",
        "*-shm",
        "*-journal",
    ] {
        assert!(ignore.lines().any(|l| l == line), "{line} in {ignore}");
    }
    let remote = git_out(&state, &["remote", "get-url", "origin"]);
    assert_eq!(
        String::from_utf8_lossy(&remote.stdout).trim(),
        "https://github.com/owner/cataly-home.git"
    );
    let config = std::fs::read_to_string(hub.config_path()).unwrap();
    assert!(
        config.contains("[jobs.home]") && config.contains("include_cataly = true"),
        "{config}"
    );
    let again = hub.json(&["setup", "--backup", "--json"]);
    assert_eq!(again["backup"]["state"], "private");
    assert_eq!(again["action"], "keep", "applying twice changes nothing");

    // Snapshot: the first tick pushes to a local bare repository.
    let bare = hub.root.join("bare.git");
    git(&hub.root, &["init", "-q", "--bare", bare.to_str().unwrap()]);
    git(
        &state,
        &["remote", "set-url", "origin", bare.to_str().unwrap()],
    );
    hub.start();
    hub.until(Duration::from_secs(30), || {
        git_out(&bare, &["rev-parse", "--verify", "-q", "main"])
            .status
            .success()
            .then_some(())
    });
    let tree = git_out(&bare, &["ls-tree", "-r", "--name-only", "main"]);
    let tree = String::from_utf8_lossy(&tree.stdout).to_string();
    assert!(tree.lines().any(|l| l == "store/cataly.db"), "{tree}");
    assert!(tree.lines().any(|l| l == "config/cataly.toml"), "{tree}");
    assert!(
        !tree
            .lines()
            .any(|l| l == "cataly.db" || l.ends_with("-wal")),
        "{tree}"
    );
    let events = hub.events().len();
    assert!(events > 0);
    assert!(hub.stop().success());
    restore_and_open(&mut hub, &bare, &config, events);
}

/// Change (the config is edited, the live store is lost), then restore from the bare repository:
/// the config is the one backed up and the store opens with the events of the snapshot.
fn restore_and_open(hub: &mut TestHub, bare: &std::path::Path, config: &str, events: usize) {
    let state = hub.state();
    hub.write_config("# changed\n");
    for f in ["cataly.db", "cataly.db-wal", "cataly.db-shm"] {
        let _ = std::fs::remove_file(state.join(f));
    }
    let copy = hub.root.join("restore");
    git(
        &hub.root,
        &[
            "clone",
            "-q",
            bare.to_str().unwrap(),
            copy.to_str().unwrap(),
        ],
    );
    std::fs::copy(copy.join("store/cataly.db"), state.join("cataly.db")).unwrap();
    std::fs::set_permissions(
        state.join("cataly.db"),
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    let restored = std::fs::read_to_string(copy.join("config/cataly.toml")).unwrap();
    assert_eq!(restored, config);
    hub.write_config(&restored);
    hub.start();
    let after = hub.events();
    assert!(
        !after.is_empty() && after.len() <= events + 4,
        "{} vs {events}",
        after.len()
    );
    assert!(hub.stop().success());
}

/// Check 10 (health part): a problem counts after two bad ticks, is reported once, and its
/// clearing is reported once; `cataly health` shows it meanwhile.
#[test]
fn health_reports_after_two_bad_ticks_and_clears_once() {
    let note = std::cell::RefCell::new(None);
    let mut hub = hub_with_fake_omp(|hub| {
        let path = hub.root.join("note.md");
        *note.borrow_mut() = Some(path.clone());
        format!(
            "[health]\nignore = [\"omp\"]\n\n[jobs.h]\nkind = \"health\"\nenabled = false\nfiles = [{{ path = \"{}\", max_age = \"1h\" }}]\n",
            path.display()
        )
    });
    let note = note.into_inner().unwrap();
    hub.start();
    assert_eq!(
        hub.tick("h"),
        Vec::<String>::new(),
        "one bad tick is not enough"
    );
    let events = hub.run_job("h");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["kind"], "health.problem");
    assert!(events[0]["text"].as_str().unwrap().contains("note.md"));
    assert_eq!(hub.tick("h"), Vec::<String>::new(), "reported once");
    let report = hub.cli(&["health", "--json"]);
    assert_eq!(report.status.code(), Some(1), "warnings remain");
    let report: Value = serde_json::from_slice(&report.stdout).unwrap();
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"].as_str().unwrap().starts_with("file."))
        .cloned()
        .unwrap();
    assert_eq!(check["level"], "warn");

    std::fs::write(&note, "fresh").unwrap();
    assert_eq!(hub.tick("h"), ["health.cleared"]);
    assert_eq!(hub.tick("h"), Vec::<String>::new(), "cleared once");
    assert!(hub.cli(&["health"]).status.success());
    assert!(hub.stop().success());
}

/// Check 12 (fake part): an OMP older than the minimum is named with the minimum in status and
/// health, and no session start is possible.
#[test]
fn old_omp_is_refused_and_health_names_both_versions() {
    let mut hub = hub_with_fake_omp(|_| String::new());
    hub.fixture("omp", "version", Some("19.0.0"));
    hub.start();
    let status = hub.until(Duration::from_secs(10), || {
        let s = hub.json(&["status", "--json"]);
        s["omp"].is_object().then_some(s)
    });
    assert_eq!(status["omp"]["version"], "19.0.0");
    assert_eq!(status["omp"]["meets_minimum"], false);
    let refused = status["sessions_refused"].as_str().unwrap();
    assert!(
        refused.contains("19.0.0") && refused.contains("19.1.1"),
        "{refused}"
    );
    let health = hub.cli(&["health"]);
    assert_eq!(health.status.code(), Some(1));
    let text = String::from_utf8_lossy(&health.stdout);
    let line = text.lines().find(|l| l.contains("omp")).unwrap();
    assert!(
        line.starts_with("WARN") && line.contains("19.0.0") && line.contains("19.1.1"),
        "{text}"
    );
    assert!(hub.stop().success());
}
