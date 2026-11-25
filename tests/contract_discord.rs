//! Contract suite, Discord end to end against stock OMP (milestone 2 checks 1, 2 and 7, and the wake
//! measurement of DESIGN 2.3): the hub with intake on the fake Discord (`tests/support/discord.rs`)
//! and the lead on stock OMP with the loopback fake model, in an isolated agent directory. Runs only
//! with `CATALY_CONTRACT_OMP=1` (`CATALY_CONTRACT_OMP_PATH` selects the executable).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;
use support::TestHub;
use support::discord::{FakeDiscord, Gw};
use support::model::{FakeModel, models_yml};

const OWNER: &str = "400000000000000001";
const OTHER: &str = "400000000000000002";
const CH: &str = "600000000000000001";
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

/// A hub whose lead runs stock OMP on the fake model and whose intake uses the fake Discord.
fn hub(fake: &FakeDiscord, model: &FakeModel, discord_extra: &str) -> TestHub {
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
    let token = hub.private_file("token", "fake-token");
    fake.channel(CH, 0, None);
    hub.write_config(&format!(
        "[omp]\npath = \"{}\"\n[sessions]\nextra_flags = [\"--no-skills\", \"--no-rules\", \"--no-context-files\"]\n\
         [lead]\nmodel = \"fake/primary\"\ncwd = \"{}\"\n{}{discord_extra}",
        installed_omp().display(),
        work.display(),
        fake.config(&token, OWNER, &[CH])
    ));
    hub
}

fn settled(hub: &TestHub) -> Value {
    hub.until(WAIT, || {
        let lead = hub.json(&["lead", "status", "--json"]);
        let discord = hub.json(&["discord", "status", "--json"]);
        (lead["phase"] == "live"
            && lead["session"]["status"] == "idle"
            && lead["sent"] == 0
            && lead["waiting"] == 0
            && discord["healthy"] == true
            && discord["counts"]["answers_waiting"] == 0)
            .then_some(discord)
    })
}

fn reference(c: &support::discord::Seen) -> Option<String> {
    c.body["message_reference"]["message_id"]
        .as_str()
        .map(str::to_string)
}

/// Checks 1 and 2 with stock OMP, the end-to-end wake latency, RSS with Discord connected and the
/// lead attached, and reconnect to catch-up complete.
#[test]
fn discord_requests_against_stock_omp() {
    if !enabled() {
        return;
    }
    let fake = FakeDiscord::start();
    let model = FakeModel::start();
    let mut hub = hub(&fake, &model, "");
    hub.start();
    settled(&hub);
    let mut id = 800_000_000_000_000_100u64;
    let wakes = 40;
    for n in 0..wakes {
        id += 1;
        fake.say(FakeDiscord::message(CH, id, OWNER, &format!("hello {n}")));
        let created = fake.wait_creates(n + 1, WAIT);
        assert_eq!(reference(&created[n]), Some(id.to_string()));
        assert_eq!(created[n].body["content"], "ok");
        settled(&hub);
    }
    let s = settled(&hub);
    assert_eq!(s["counts"]["owed_settled"], wakes, "{s}");
    assert_eq!(fake.calls("PUT", "/reactions/").len(), wakes);
    // Check 2: a second message while the lead works is a follow-up answered after the first. The
    // lead works once the model has the slow request: the session status reads `unknown` during a
    // turn (no fresh readback, HOST req. 36), so it cannot tell.
    let (slow, next) = (id + 1, id + 2);
    let asked = model.seen().len();
    fake.say(FakeDiscord::message(CH, slow, OWNER, "SLOW please"));
    hub.until(WAIT, || (model.seen().len() > asked).then_some(()));
    fake.say(FakeDiscord::message(CH, next, OWNER, "and then this"));
    let created = fake.wait_creates(wakes + 2, WAIT);
    assert_eq!(reference(&created[wakes]), Some(slow.to_string()));
    assert_eq!(reference(&created[wakes + 1]), Some(next.to_string()));
    settled(&hub);
    // Reconnects: each op 7 is resumed and caught up.
    for n in 0..5 {
        fake.gw(Gw::Op7);
        fake.wait_connections(n + 2, WAIT);
        settled(&hub);
    }
    let stats = hub.json(&["stats", "--json"]);
    eprintln!(
        "MEASURE discord_wake={} catch_up={} rss_discord_lead={} hub_wake={} agent_start={}",
        stats["discord_wake_latency"],
        stats["discord_catch_up_latency"],
        stats["rss_bytes"],
        stats["wake_latency"],
        stats["agent_start_latency"]
    );
    assert!(
        stats["discord_wake_latency"]["count"].as_u64().unwrap() >= u64::try_from(wakes).unwrap()
    );
    assert!(hub.stop().success());
}

/// Check 7 with stock OMP: the `ask` tool's question (structured questions over rpc-ui) is posted
/// with buttons; a press by someone else is refused; the owner's press answers OMP, and the turn's
/// final answer is posted.
#[test]
fn ask_tool_answered_from_discord() {
    if !enabled() {
        return;
    }
    let fake = FakeDiscord::start();
    let model = FakeModel::start();
    let mut hub = hub(&fake, &model, "form_default_after = \"10m\"\n");
    hub.start();
    settled(&hub);
    let source = 800_000_000_000_000_201u64;
    fake.say(FakeDiscord::message(CH, source, OWNER, "ASK me something"));
    let q = fake.wait(WAIT, |f| {
        f.bot_messages(CH)
            .into_iter()
            .find(|m| m["components"].as_array().is_some_and(|c| !c.is_empty()))
    });
    assert!(
        q["content"].as_str().unwrap().contains("Pick a color"),
        "{q}"
    );
    let qid = q["id"].as_str().unwrap();
    let blue = q["components"][0]["components"][1]["custom_id"]
        .as_str()
        .unwrap();
    fake.press(OTHER, CH, qid, blue);
    let refusal = fake.wait(WAIT, |f| f.calls("POST", "/callback").into_iter().next());
    assert_eq!(refusal.body["type"], 4);
    fake.press(OWNER, CH, qid, blue);
    let answer = fake.wait(WAIT, |f| {
        f.creates()
            .into_iter()
            .find(|c| reference(c) == Some(source.to_string()))
    });
    assert_eq!(answer.body["content"], "ok");
    let lead = hub.json(&["lead", "status", "--json"]);
    assert!(
        lead["session"]["dialogs"].as_array().unwrap().is_empty(),
        "{lead}"
    );
    assert!(hub.stop().success());
}
