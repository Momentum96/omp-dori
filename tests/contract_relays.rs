//! Contract suite, Telegram and Slack end to end against stock OMP (milestone 4 check 5 and its
//! measurements): the hub with intake on the fake Telegram Bot API, the fake Slack and the fake
//! Discord (`tests/support/`), and the lead on stock OMP with the loopback fake model, in an isolated
//! agent directory. Runs only with `CATALY_CONTRACT_OMP=1` (`CATALY_CONTRACT_OMP_PATH` selects the
//! executable).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;
use support::TestHub;
use support::discord::FakeDiscord;
use support::model::{FakeModel, models_yml};
use support::slack::{self as fake_slack, FakeSlack};
use support::telegram::{self as fake_tg, FakeTelegram};

const DISCORD_OWNER: &str = "400000000000000001";
const CH: &str = "600000000000000001";
const DM: &str = "D0OWNER";
const RELAYS: [&str; 3] = ["discord", "telegram", "slack"];
const WAIT: Duration = Duration::from_secs(120);

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

/// A hub whose lead runs stock OMP on the fake model, with all three relays on their fakes.
fn hub(tg: &FakeTelegram, slack: &FakeSlack, discord: &FakeDiscord, model: &FakeModel) -> TestHub {
    let mut hub = TestHub::new("");
    let agent = hub.root.join("agent");
    let work = hub.root.join("work");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(
        agent.join("models.yml"),
        models_yml(model.port, &["primary"]),
    )
    .unwrap();
    hub.env.push(("PI_CODING_AGENT_DIR".into(), agent));
    let tg_token = hub.private_file("telegram-token", fake_tg::TOKEN);
    let slack_tokens = hub.private_file("slack-tokens", &FakeSlack::tokens());
    let discord_token = hub.private_file("token", "fake-token");
    discord.channel(CH, 0, None);
    hub.write_config(&format!(
        "[omp]\npath = \"{}\"\n[sessions]\nextra_flags = [\"--no-skills\", \"--no-rules\", \"--no-context-files\"]\n\
         [lead]\nmodel = \"fake/primary\"\ncwd = \"{}\"\n{}{}{}",
        installed_omp().display(),
        work.display(),
        tg.config(&tg_token),
        slack.config(&slack_tokens),
        discord.config(&discord_token, DISCORD_OWNER, &[CH]),
    ));
    hub
}

/// Waits until the lead is idle with nothing owed and every relay is healthy with no answer waiting.
fn settled(hub: &TestHub) {
    let deadline = Instant::now() + WAIT;
    loop {
        let lead = hub.json(&["lead", "status", "--json"]);
        let relays: Vec<Value> = RELAYS
            .iter()
            .map(|r| hub.json(&[r, "status", "--json"]))
            .collect();
        let quiet = relays
            .iter()
            .all(|s| s["healthy"] == true && s["counts"]["answers_waiting"] == 0);
        if lead["phase"] == "live"
            && lead["session"]["status"] == "idle"
            && lead["sent"] == 0
            && lead["waiting"] == 0
            && quiet
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "not settled within {WAIT:?}: lead {lead}\nrelays {relays:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn tg_replies_to(tg: &FakeTelegram, message: i64) -> Vec<fake_tg::Seen> {
    tg.calls("sendMessage")
        .into_iter()
        .filter(|s| s.body["reply_parameters"]["message_id"] == message)
        .collect()
}

fn slack_posts_in(slack: &FakeSlack, thread: &str) -> Vec<fake_slack::Seen> {
    slack
        .calls("chat.postMessage")
        .into_iter()
        .filter(|s| s.body["channel"] == DM && s.body["thread_ts"] == thread)
        .collect()
}

/// Check 5 with stock OMP: a Telegram and a Slack request each reach the lead and get its answer in
/// place with a seen mark. Then 20 more requests per relay for the intake latency, Telegram receive
/// lag, and hub RSS with three relays connected, idle.
#[test]
fn telegram_and_slack_requests_against_stock_omp() {
    if !enabled() {
        return;
    }
    let tg = FakeTelegram::start();
    let slack = FakeSlack::start();
    let discord = FakeDiscord::start();
    let model = FakeModel::start();
    let mut hub = hub(&tg, &slack, &discord, &model);
    hub.start();
    settled(&hub);
    tg.push(
        "message",
        FakeTelegram::message(11, FakeTelegram::dm(), 7, "hello from telegram"),
    );
    let answer = tg.wait(WAIT, |f| tg_replies_to(f, 11).pop());
    assert_eq!(answer.body["chat_id"], 7);
    assert_eq!(answer.body["text"], "ok");
    let marks = tg.calls("setMessageReaction");
    assert_eq!(marks.len(), 1, "one seen mark");
    assert_eq!(marks[0].body["message_id"], 11);
    settled(&hub);
    let ts = slack.say(DM, fake_slack::OWNER, "hello from slack", None);
    let posted = slack.wait(WAIT, |f| slack_posts_in(f, &ts).pop());
    assert_eq!(posted.body["text"], "ok");
    let reactions = slack.calls("reactions.add");
    assert_eq!(reactions.len(), 1, "one seen mark");
    assert_eq!(reactions[0].body["timestamp"], ts.as_str());
    settled(&hub);
    let rounds = 20;
    for n in 0..rounds {
        let id = 100 + n;
        tg.push(
            "message",
            FakeTelegram::message(id, FakeTelegram::dm(), 7, &format!("telegram {n}")),
        );
        tg.wait(WAIT, |f| tg_replies_to(f, id).pop());
        settled(&hub);
        let ts = slack.say(DM, fake_slack::OWNER, &format!("slack {n}"), None);
        slack.wait(WAIT, |f| slack_posts_in(f, &ts).pop());
        settled(&hub);
    }
    for relay in ["telegram", "slack"] {
        let s = hub.json(&[relay, "status", "--json"]);
        assert_eq!(s["counts"]["owed_settled"], rounds + 1, "{relay}: {s}");
    }
    assert_eq!(
        tg.calls("setMessageReaction").len(),
        usize::try_from(rounds + 1).unwrap()
    );
    assert_eq!(
        slack.calls("reactions.add").len(),
        usize::try_from(rounds + 1).unwrap()
    );
    std::thread::sleep(Duration::from_secs(10));
    let stats = hub.json(&["stats", "--json"]);
    eprintln!(
        "MEASURE telegram_wake={} slack_wake={} telegram_receive_lag={} rss_three_relays_idle={}",
        stats["telegram_wake_latency"],
        stats["slack_wake_latency"],
        stats["telegram_receive_lag"],
        stats["rss_bytes"]
    );
    for key in ["telegram_wake_latency", "slack_wake_latency"] {
        assert!(
            stats[key]["count"].as_i64().unwrap_or(0) > rounds,
            "{key}: {stats}"
        );
    }
    assert!(hub.stop().success());
}
