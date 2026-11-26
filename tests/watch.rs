//! Watched channels from the ops panel through the real hub, the fake Discord and the fake OMP in
//! its turn mode: the owner's picker writes `[discord] channels` and the running intake follows it
//! on the same Gateway connection; anyone else, another server or an invalid result changes
//! nothing; channels the picker cannot show are kept.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::time::Duration;

use serde_json::{Value, json};
use support::TestHub;
use support::discord::{FakeDiscord, GUILD, Seen};

const OWNER: &str = "400000000000000001";
const OTHER: &str = "400000000000000002";
const CH: &str = "600000000000000001";
const OPS: &str = "600000000000000002";
const NEW: &str = "600000000000000003";
const WAIT: Duration = Duration::from_secs(30);

/// A hub watching `channels` with the lead on the fake OMP and the ops room in `OPS`.
fn hub(fake: &FakeDiscord, channels: &[&str], discord_extra: &str) -> TestHub {
    let hub = TestHub::new("");
    let omp = hub.install_fake("omp");
    for f in ["rpc", "turns"] {
        hub.fixture("omp", f, Some(""));
    }
    let work = hub.root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let token = hub.private_file("token", "fake-token");
    for c in channels.iter().chain([&OPS, &NEW]) {
        fake.channel(c, 0, None);
    }
    hub.write_config(&format!(
        "[hub]\nlanguage = \"en\"\n[omp]\npath = \"{}\"\n[lead]\nmodel = \"fake/primary\"\n\
         cwd = \"{}\"\n{}guild = \"{GUILD}\"\n{discord_extra}\n\
         [destinations.cataly-ops]\nkind = \"discord\"\nchannel = \"{OPS}\"\n",
        omp.display(),
        work.display(),
        fake.config(&token, OWNER, channels)
    ));
    // The picker writes the file under setup's rules: an owner-only folder, as setup makes it.
    let dir = hub.config_path().parent().unwrap().to_path_buf();
    std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    hub
}

/// Waits until intake is connected and caught up and the lead is live; returns the panel's ID.
fn ready(hub: &TestHub, fake: &FakeDiscord) -> String {
    hub.until(WAIT, || {
        let s = hub.json(&["discord", "status", "--json"]);
        let lead = hub.json(&["lead", "status", "--json"]);
        (s["healthy"] == true && lead["phase"] == "live").then_some(())
    });
    fake.wait(WAIT, |f| {
        f.bot_messages(OPS)
            .first()
            .and_then(|m| m["id"].as_str().map(str::to_string))
    })
}

/// The interaction answers so far.
fn answers(fake: &FakeDiscord) -> Vec<Value> {
    fake.calls("POST", "/callback")
        .into_iter()
        .map(|s| s.body)
        .collect()
}

fn next_answer(fake: &FakeDiscord, before: usize) -> Value {
    fake.wait(WAIT, |f| answers(f).get(before).cloned())
}

fn press(fake: &FakeDiscord, user: &str, panel: &str) -> Value {
    let before = answers(fake).len();
    fake.press(user, OPS, panel, "ops:watch");
    next_answer(fake, before)
}

/// Submits the picker with `values` from `guild`.
fn pick(fake: &FakeDiscord, guild: &str, user: &str, panel: &str, values: &[&str]) -> Value {
    let before = answers(fake).len();
    let data = json!({"component_type": 8, "custom_id": "ops:watched", "values": values});
    fake.interact_in(guild, user, OPS, panel, 3, &data);
    next_answer(fake, before)
}

fn config_text(hub: &TestHub) -> String {
    std::fs::read_to_string(hub.config_path()).unwrap()
}

/// The bot's replies to message `id`.
fn replies_to(fake: &FakeDiscord, channel: &str, id: u64) -> Vec<Seen> {
    fake.creates()
        .into_iter()
        .filter(|c| {
            c.path == format!("/channels/{channel}/messages")
                && c.body["message_reference"]["message_id"]
                    .as_str()
                    .and_then(|m| m.parse::<u64>().ok())
                    == Some(id)
        })
        .collect()
}

fn answered(fake: &FakeDiscord, channel: &str, id: u64) {
    fake.wait(WAIT, |f| replies_to(f, channel, id).into_iter().next());
}

/// A message ID made now (Discord's epoch is 2015-01-01), with `n` as its increment.
fn fresh(n: u64) -> u64 {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    ((u64::try_from(ms).unwrap() - 1_420_070_400_000) << 22) + n
}

/// The owner picks two channels: the file and the running intake follow on the same connection,
/// and his message in the new channel reaches the lead and is answered there. Dropping the first
/// channel then stops intake from it at once and moves the home to a watched channel.
#[test]
fn owner_adds_a_channel_then_drops_one() {
    let fake = FakeDiscord::start();
    let mut hub = hub(&fake, &[CH], "");
    hub.start();
    let panel = ready(&hub, &fake);
    let picker = press(&fake, OWNER, &panel);
    assert_eq!(picker["type"], 4, "{picker}");
    assert_eq!(picker["data"]["flags"], 64);
    let select = &picker["data"]["components"][0]["components"][0];
    assert_eq!(select["type"], 8);
    assert_eq!(select["custom_id"], "ops:watched");
    assert_eq!(select["channel_types"], json!([0, 15]));
    assert_eq!(select["min_values"], 1);
    assert_eq!(select["max_values"], 25);
    assert_eq!(
        select["default_values"],
        json!([{"id": CH, "type": "channel"}])
    );

    let done = pick(&fake, GUILD, OWNER, &panel, &[CH, NEW]);
    assert_eq!(done["type"], 7, "{done}");
    assert_eq!(
        done["data"]["content"],
        format!("📡 Watching: <#{CH}> <#{NEW}>")
    );
    let text = config_text(&hub);
    assert!(
        text.contains(&format!("channels = [\"{CH}\", \"{NEW}\"]")),
        "{text}"
    );
    assert!(text.contains(&format!("home = \"{CH}\"")), "{text}");
    // A channel watched anew is read from the moment the pick was applied: IDs carry real time.
    let first = fresh(1);
    fake.say(FakeDiscord::message(
        NEW,
        first,
        OWNER,
        "hello from the new room",
    ));
    answered(&fake, NEW, first);

    let done = pick(&fake, GUILD, OWNER, &panel, &[NEW]);
    assert_eq!(done["data"]["content"], format!("📡 Watching: <#{NEW}>"));
    let text = config_text(&hub);
    assert!(text.contains(&format!("channels = [\"{NEW}\"]")), "{text}");
    assert!(text.contains(&format!("home = \"{NEW}\"")), "{text}");
    let (dropped, kept) = (fresh(2), fresh(3));
    fake.say(FakeDiscord::message(
        CH,
        dropped,
        OWNER,
        "no longer watched",
    ));
    fake.say(FakeDiscord::message(NEW, kept, OWNER, "still watched"));
    answered(&fake, NEW, kept);
    assert!(replies_to(&fake, CH, dropped).is_empty());
    // Only the two answered messages went through: the dropped channel's never entered the queue.
    hub.until(WAIT, || {
        let s = hub.json(&["discord", "status", "--json"]);
        (s["counts"]["queued"] == 0 && s["counts"]["owed_settled"] == 2).then_some(())
    });
    assert_eq!(fake.connections().len(), 1, "one Gateway connection");
    assert!(hub.stop().success());
}

/// Someone else's press or pick, or one from another server, is refused and changes nothing; an
/// invalid result is reported and the running config stays in force.
#[test]
fn refused_or_invalid_picks_change_nothing() {
    let fake = FakeDiscord::start();
    let mut hub = hub(&fake, &[CH], &format!("others = [\"{CH}\"]\n"));
    hub.start();
    let panel = ready(&hub, &fake);
    let before = config_text(&hub);
    let refusal = "Only the owner may change the watched channels.";
    let refused = press(&fake, OTHER, &panel);
    assert_eq!(refused["type"], 4);
    assert_eq!(refused["data"]["flags"], 64);
    assert_eq!(refused["data"]["content"], refusal);
    let refused = pick(&fake, GUILD, OTHER, &panel, &[NEW]);
    assert_eq!(refused["data"]["content"], refusal);
    let refused = pick(&fake, "500000000000000099", OWNER, &panel, &[NEW]);
    assert_eq!(refused["data"]["content"], refusal);
    assert_eq!(config_text(&hub), before);

    // `others` must stay among the watched channels: without CH the result is not a valid config.
    let invalid = pick(&fake, GUILD, OWNER, &panel, &[NEW]);
    assert_eq!(invalid["type"], 7, "{invalid}");
    let content = invalid["data"]["content"].as_str().unwrap();
    assert!(
        content.starts_with("⚠️ The watched channels were not changed")
            && content.contains("others"),
        "{content}"
    );
    assert_eq!(config_text(&hub), before);
    let status = hub.json(&["status", "--json"]);
    assert!(status["config_error"].is_null(), "{status}");
    fake.say(FakeDiscord::message(
        CH,
        800_000_000_000_000_201,
        OWNER,
        "still here",
    ));
    answered(&fake, CH, 800_000_000_000_000_201);
    assert!(hub.stop().success());
}

/// With 30 watched channels the picker shows the first 25 and says so; channels it could not show
/// stay watched after a pick.
#[test]
fn channels_past_the_picker_are_kept() {
    let fake = FakeDiscord::start();
    let many: Vec<String> = (0..30)
        .map(|i| (600_000_000_000_000_100u64 + i).to_string())
        .collect();
    let refs: Vec<&str> = many.iter().map(String::as_str).collect();
    let mut hub = hub(&fake, &refs, "");
    hub.start();
    let panel = ready(&hub, &fake);
    let picker = press(&fake, OWNER, &panel);
    let select = &picker["data"]["components"][0]["components"][0];
    assert_eq!(select["default_values"].as_array().unwrap().len(), 25);
    assert_eq!(select["default_values"][24]["id"], many[24]);
    assert!(
        picker["data"]["content"]
            .as_str()
            .unwrap()
            .contains("The other 5 watched channels do not fit in the list and stay watched."),
        "{picker}"
    );
    let done = pick(&fake, GUILD, OWNER, &panel, &refs[..3]);
    let content = done["data"]["content"].as_str().unwrap();
    assert!(
        content.contains("(5 channels the list could not show are kept)"),
        "{content}"
    );
    let kept: Vec<&str> = refs[..3].iter().chain(&refs[25..]).copied().collect();
    let list: Vec<String> = kept.iter().map(|c| format!("\"{c}\"")).collect();
    let text = config_text(&hub);
    assert!(
        text.contains(&format!("channels = [{}]", list.join(", "))),
        "{text}"
    );
    assert!(text.contains(&format!("home = \"{}\"", refs[0])), "{text}");
    fake.say(FakeDiscord::message(
        refs[29],
        800_000_000_000_000_301,
        OWNER,
        "a kept room",
    ));
    answered(&fake, refs[29], 800_000_000_000_000_301);
    assert!(hub.stop().success());
}

/// Idle resident memory of the hub with 1 and then 5 watched channels, Discord connected and the
/// lead attached on the fake OMP: `VmRSS` 10 s after intake and the lead are ready, as in
/// `docs/measurements.md`. `cargo test --release --test watch -- --ignored --nocapture idle_rss`.
#[test]
#[ignore = "measurement; run with --release --ignored --nocapture"]
fn idle_rss_by_watched_channels() {
    for n in [1u64, 5] {
        let fake = FakeDiscord::start();
        let channels: Vec<String> = (0..n)
            .map(|i| (600_000_000_000_000_100u64 + i).to_string())
            .collect();
        let refs: Vec<&str> = channels.iter().map(String::as_str).collect();
        let mut hub = hub(&fake, &refs, "");
        hub.start();
        ready(&hub, &fake);
        std::thread::sleep(Duration::from_secs(10));
        let pid = hub.json(&["status", "--json"])["pid"].as_u64().unwrap();
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let rss = status
            .lines()
            .find(|l| l.starts_with("VmRSS:"))
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .to_string();
        eprintln!("MEASURE idle_rss channels={n} vmrss_kb={rss}");
        assert!(hub.stop().success());
    }
}
