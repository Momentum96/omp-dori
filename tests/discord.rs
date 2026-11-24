//! Job posts to a fake Discord REST server through the real hub (milestone 1 acceptance check 11):
//! no mentions, an enforced nonce, refusal by the outgoing text checks, and a retry after an
//! uncertain answer that creates no second message.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::time::Duration;

use support::TestHub;
use support::discord::FakeDiscord;

const CHANNEL: &str = "123456789012345678";
const TOKEN: &str = "test-token-not-real";

fn config(hub: &TestHub, discord: &FakeDiscord, jobs: &[(&str, &str)]) -> String {
    let omp = hub.install_fake("omp");
    let token = hub.private_file("discord-token", TOKEN);
    let mut text = format!(
        "[omp]\npath = \"{}\"\n\n[discord]\ntoken_file = \"{}\"\napi_base = \"{}\"\n\n[outgoing]\nbanned_characters = [\"\u{2014}\"]\n\n[destinations.chan]\nkind = \"discord\"\nchannel = \"{CHANNEL}\"\n\n",
        omp.display(),
        token.display(),
        discord.base()
    );
    for (name, says) in jobs {
        text.push_str(&support::probe(
            name,
            &format!(r#"["printf", "%s", "{says}"]"#),
            "destinations = [\"chan\"]\n",
        ));
    }
    text
}

/// Delivery rows of the hub's store: (state, detail).
fn deliveries(hub: &TestHub) -> Vec<(String, String)> {
    let db = rusqlite::Connection::open_with_flags(
        hub.state().join("cataly.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let mut stmt = db
        .prepare("SELECT state, COALESCE(detail, '') FROM event_deliveries ORDER BY event_id")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn posts_parse_no_mentions_carry_an_enforced_nonce_and_pass_the_text_checks() {
    let discord = FakeDiscord::start();
    let mut hub = TestHub::new("");
    hub.write_config(&config(
        &hub,
        &discord,
        &[
            ("good", "all fine @everyone"),
            ("unfilled", "left {unfilled}"),
            ("dash", "a \u{2014} b"),
        ],
    ));
    hub.start();
    let creates = discord.wait_creates(1, Duration::from_secs(15));
    // The refused posts never reach Discord; give them time to be decided.
    let rows = hub.until(Duration::from_secs(15), || {
        let rows = deliveries(&hub);
        (rows.len() == 3
            && rows
                .iter()
                .all(|(s, _)| s != "waiting" && s != "attempting"))
        .then_some(rows)
    });
    let creates_now = discord.creates();
    assert_eq!(creates_now.len(), 1, "{creates_now:?}");
    let post = &creates[0];
    assert_eq!(post.path, format!("/channels/{CHANNEL}/messages"));
    assert_eq!(
        post.authorization.as_deref(),
        Some("Bot test-token-not-real")
    );
    assert_eq!(
        post.body["allowed_mentions"],
        serde_json::json!({ "parse": [], "replied_user": false })
    );
    assert_eq!(post.body["enforce_nonce"], true);
    let nonce = post.body["nonce"].as_str().unwrap();
    assert!(!nonce.is_empty() && nonce.len() <= 25, "{nonce}");
    assert!(post.body["content"].as_str().unwrap().contains("all fine"));
    assert!(
        discord
            .seen()
            .iter()
            .all(|s| !s.body.to_string().contains("{unfilled}")
                && !s.body.to_string().contains('\u{2014}')),
        "refused text was never sent"
    );
    let sent = rows.iter().filter(|(s, _)| s == "sent").count();
    assert_eq!(sent, 1, "{rows:?}");
    assert!(
        rows.iter()
            .any(|(s, d)| s == "failed" && d.contains("unfilled template field {unfilled}")),
        "{rows:?}"
    );
    assert!(
        rows.iter()
            .any(|(s, d)| s == "failed" && d.contains("banned character U+2014")),
        "{rows:?}"
    );
    assert!(hub.stop().success());
}

/// A create answered 500 after Discord made the message is retried with the same nonce, and the
/// enforced nonce returns the existing message: exactly one message exists and the post is sent.
#[test]
fn a_retry_after_an_uncertain_answer_creates_no_second_message() {
    let discord = FakeDiscord::start();
    discord.script(&[500]);
    let mut hub = TestHub::new("");
    hub.write_config(&config(&hub, &discord, &[("ping", "pong")]));
    hub.start();
    let creates = discord.wait_creates(2, Duration::from_secs(20));
    assert_eq!(creates[0].body["nonce"], creates[1].body["nonce"]);
    let rows = hub.until(Duration::from_secs(15), || {
        let rows = deliveries(&hub);
        rows.iter().any(|(s, _)| s == "sent").then_some(rows)
    });
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(discord.messages(), 1);
    assert!(hub.stop().success());
}
