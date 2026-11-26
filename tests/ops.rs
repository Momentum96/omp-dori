//! The ops room through the real hub and a fake Discord: the panel is one embed message created
//! with its banner and thumbnail and pinned; its buttons answer the owner only; events routed to
//! the room are never posted as plain lines.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::time::Duration;

use serde_json::Value;
use support::TestHub;
use support::discord::{FakeDiscord, Seen};

const OWNER: &str = "400000000000000001";
const OTHER: &str = "400000000000000002";
const CH: &str = "600000000000000001";
const OPS: &str = "600000000000000002";
const WAIT: Duration = Duration::from_secs(30);

fn hub(fake: &FakeDiscord, language: &str) -> TestHub {
    let hub = TestHub::new("");
    let omp = hub.install_fake("omp");
    let token = hub.private_file("token", "fake-token");
    fake.channel(CH, 0, None);
    fake.channel(OPS, 0, None);
    let probe = support::probe(
        "ping",
        r#"["printf", "pong"]"#,
        "destinations = [\"cataly-ops\"]\n",
    );
    hub.write_config(&format!(
        "[hub]\nlanguage = \"{language}\"\n[omp]\npath = \"{}\"\n{}\n\
         [destinations.cataly-ops]\nkind = \"discord\"\nchannel = \"{OPS}\"\n\n{probe}",
        omp.display(),
        fake.config(&token, OWNER, &[CH])
    ));
    hub
}

fn panel(fake: &FakeDiscord) -> Seen {
    fake.wait(WAIT, |f| {
        f.creates()
            .into_iter()
            .find(|c| c.path == format!("/channels/{OPS}/messages"))
    })
}

/// The interaction answers so far.
fn answers(fake: &FakeDiscord) -> Vec<Value> {
    fake.calls("POST", "/callback")
        .into_iter()
        .map(|s| s.body)
        .collect()
}

fn press(fake: &FakeDiscord, user: &str, message: &str, id: &str) -> Value {
    let before = answers(fake).len();
    fake.press(user, OPS, message, id);
    fake.wait(WAIT, |f| answers(f).get(before).cloned())
}

#[test]
fn the_panel_is_one_pinned_embed_with_owner_only_buttons() {
    let fake = FakeDiscord::start();
    let mut hub = hub(&fake, "ko");
    hub.start();
    let created = panel(&fake);
    let names: Vec<&str> = created.files.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["panel-banner.jpg", "panel-thumb.jpg"]);
    assert!(created.files.iter().all(|(_, size)| *size > 1000));
    let embed = &created.body["embeds"][0];
    assert_eq!(embed["title"], "Cataly 상황판");
    assert_eq!(embed["image"]["url"], "attachment://panel-banner.jpg");
    assert_eq!(embed["thumbnail"]["url"], "attachment://panel-thumb.jpg");
    assert!(embed["timestamp"].is_string(), "the update time");
    assert_eq!(
        created.body["attachments"][1]["filename"],
        "panel-thumb.jpg"
    );
    assert_eq!(created.body["enforce_nonce"], true);
    let message = fake
        .bot_messages(OPS)
        .first()
        .and_then(|m| m["id"].as_str().map(str::to_string))
        .unwrap();
    fake.wait(WAIT, |f| {
        f.calls("PUT", &format!("/channels/{OPS}/messages/pins/{message}"))
            .into_iter()
            .next()
    });
    fake.wait_connections(1, WAIT);
    hub.until(WAIT, || {
        (hub.json(&["discord", "status", "--json"])["healthy"] == true).then_some(())
    });
    // Someone else's press gets the ephemeral refusal and changes nothing.
    let refused = press(&fake, OTHER, &message, "ops:mute");
    assert_eq!(refused["type"], 4);
    assert_eq!(refused["data"]["flags"], 64);
    assert_eq!(
        refused["data"]["content"],
        "이 버튼은 주인만 누를 수 있어요."
    );
    // The owner's presses: the usage table, a refresh too soon, the mute.
    let usage = press(&fake, OWNER, &message, "ops:usage");
    assert_eq!(usage["data"]["flags"], 64);
    assert_eq!(usage["data"]["embeds"][0]["title"], "📊 사용량 자세히");
    let soon = press(&fake, OWNER, &message, "ops:refresh");
    assert!(
        soon["data"]["content"]
            .as_str()
            .unwrap()
            .starts_with("⏳ 방금 갱신했어요."),
        "{soon}"
    );
    let mute = press(&fake, OWNER, &message, "ops:mute");
    assert!(
        mute["data"]["content"]
            .as_str()
            .unwrap()
            .starts_with("🔕 1시간 동안 알림을 끌게요"),
        "{mute}"
    );
    // The probe's events went to the panel, not into the room as lines.
    hub.until(WAIT, || {
        hub.events()
            .iter()
            .any(|e| e["kind"] == "probe.fired")
            .then_some(())
    });
    let room: Vec<Seen> = fake
        .creates()
        .into_iter()
        .filter(|c| c.path == format!("/channels/{OPS}/messages"))
        .collect();
    assert!(room.iter().all(|c| c.body["content"].is_null()), "{room:?}");
    assert_eq!(room.len(), 1, "one panel, no alert for a probe");
    assert!(hub.stop().success());
}

#[test]
fn the_panel_speaks_english_when_asked() {
    let fake = FakeDiscord::start();
    let mut hub = hub(&fake, "en");
    hub.start();
    let created = panel(&fake);
    let embed = &created.body["embeds"][0];
    assert_eq!(embed["title"], "Cataly status");
    let labels: Vec<&str> = created.body["components"][0]["components"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["label"].as_str().unwrap())
        .collect();
    assert_eq!(
        labels,
        [
            "Refresh",
            "Usage details",
            "Mute 1 hour",
            "Watched channels"
        ]
    );
    assert!(hub.stop().success());
}
