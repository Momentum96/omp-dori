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
use support::discord::{FakeDiscord, GUILD, Gw, Seen};

const OWNER: &str = "400000000000000001";
const OTHER: &str = "400000000000000002";
const CH: &str = "600000000000000001";
const OPS: &str = "600000000000000002";
const NEW: &str = "600000000000000003";
const THIRD: &str = "600000000000000004";
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
    let mut hub = hub(&fake, &[CH], &format!("others = [\"{CH}\"]\n"));
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
    // CH talked with other people: it leaves `others` in the same write instead of refusing.
    assert!(text.contains("others = []"), "{text}");
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

/// Someone else's press or pick, or one from another server, is refused and changes nothing; so
/// is a pick of a channel the bot cannot read, naming the channel and the missing permission; an
/// invalid result (the file as it is at submit time) is reported and the running config stays.
#[test]
fn refused_or_invalid_picks_change_nothing() {
    let fake = FakeDiscord::start();
    let mut hub = hub(&fake, &[CH], "");
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

    // A new channel the bot cannot read refuses the whole pick before anything is written.
    let unchanged = "⚠️ The watched channels were not changed; the running config stays:";
    for (code, need) in [(50013, "Read Message History"), (50001, "View Channel")] {
        fake.deny(NEW, code);
        let refused = pick(&fake, GUILD, OWNER, &panel, &[CH, NEW]);
        assert_eq!(refused["type"], 7, "{refused}");
        assert_eq!(
            refused["data"]["content"],
            format!("{unchanged} the bot cannot read <#{NEW}>: it needs {need} there.")
        );
        assert_eq!(config_text(&hub), before);
    }
    fake.deny(NEW, 0);
    let checks = fake.calls("GET", &format!("/channels/{NEW}/threads/archived/public"));
    assert_eq!(checks.len(), 2, "one check per pick");

    // A hand edit made while the picker was open is what the pick is based on: here it makes the
    // file invalid, so nothing is written and the running config stays.
    let edited = before.replace(
        &format!("guild = \"{GUILD}\""),
        &format!("guild = \"{GUILD}\"\nfollow_threads = \"maybe\""),
    );
    std::fs::write(hub.config_path(), &edited).unwrap();
    let invalid = pick(&fake, GUILD, OWNER, &panel, &[CH, NEW]);
    let content = invalid["data"]["content"].as_str().unwrap();
    assert!(
        content.starts_with(unchanged) && content.contains("follow_threads"),
        "{content}"
    );
    assert_eq!(config_text(&hub), edited);
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

/// A watched channel the bot cannot read, at start or later, leaves intake alone with one notice;
/// the other channels are read as before.
#[test]
fn an_unreadable_channel_drops_out_alone() {
    let fake = FakeDiscord::start();
    let mut hub = hub(&fake, &[CH, NEW, THIRD], "");
    fake.deny(THIRD, 50001);
    hub.start();
    ready(&hub, &fake);
    // Later: NEW loses Read Message History; the catch-up read after a reconnect meets it.
    fake.deny(NEW, 50013);
    fake.gw(Gw::Op7);
    fake.wait_connections(2, WAIT);
    let notices = |room: &str| {
        hub.events()
            .into_iter()
            .filter(|e| e["kind"] == "relay.held" && e.to_string().contains(room))
            .count()
    };
    hub.until(WAIT, || (notices(NEW) == 1).then_some(()));
    assert_eq!(notices(THIRD), 1);
    let s = hub.until(WAIT, || {
        let s = hub.json(&["discord", "status", "--json"]);
        (s["healthy"] == true).then_some(s)
    });
    assert!(s["health"]["reason"].is_null(), "{s}");
    // Message IDs carry real time, as Discord's do: each is newer than the bot's replies before it,
    // which a catch-up read takes as the room's position.
    let rest = fresh(401);
    fake.say(FakeDiscord::message(CH, rest, OWNER, "the rest goes on"));
    answered(&fake, CH, rest);
    let unread = fresh(402);
    fake.say(FakeDiscord::message(NEW, unread, OWNER, "not read"));
    let connected = |hub: &TestHub| hub.json(&["discord", "status", "--json"])["health"].clone();
    let before = connected(&hub)["connected_ms"].clone();
    fake.gw(Gw::Op7);
    fake.wait_connections(3, WAIT);
    // The live message comes after the catch-up read of the new connection, as on a slow host.
    hub.until(WAIT, || {
        let h = connected(&hub);
        (h["connected_ms"] != before && h["caught_up"] == true).then_some(())
    });
    let after = fresh(403);
    fake.say(FakeDiscord::message(
        CH,
        after,
        OWNER,
        "after the second reconnect",
    ));
    answered(&fake, CH, after);
    assert!(replies_to(&fake, NEW, unread).is_empty());
    assert_eq!((notices(NEW), notices(THIRD)), (1, 1), "one notice each");
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

/// A 0.2.3 file with a quota job's obsolete `hold`: the hub removes that line at start, keeping the
/// original as `config.toml.before-0.2.4`; the owner's pick then writes through the same writer onto
/// the cleaned file, so the line stays gone, the backup keeps the original and the pick applies.
#[test]
fn a_pick_after_the_hold_clean_up_keeps_both() {
    let fake = FakeDiscord::start();
    let mut hub = hub(&fake, &[CH], "");
    let hold = "\n[jobs.q]\nkind = \"quota\"\nhold = 85\nenabled = false\n";
    let old = format!("{}{hold}", config_text(&hub));
    hub.write_config(&old);
    hub.start();
    let panel = ready(&hub, &fake);
    let backup = hub.config_path().with_extension("toml.before-0.2.4");
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), old);
    let done = pick(&fake, GUILD, OWNER, &panel, &[CH, NEW]);
    assert_eq!(
        done["data"]["content"],
        format!("📡 Watching: <#{CH}> <#{NEW}>"),
        "{done}"
    );
    let text = config_text(&hub);
    assert!(
        text.contains(&format!("channels = [\"{CH}\", \"{NEW}\"]")),
        "{text}"
    );
    assert!(!text.contains("hold = 85"), "{text}");
    assert!(
        text.contains("[jobs.q]\nkind = \"quota\"\nenabled = false\n"),
        "{text}"
    );
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), old);
    assert!(hub.stop().success());
}

/// A pick while the lead sleeps applies on the running intake without waking it: the panel
/// interaction is not a message for the lead. The owner's first message in the channel picked then
/// wakes it on the same session and is answered there.
#[test]
fn a_pick_while_the_lead_sleeps_leaves_it_asleep_until_a_message_in_the_new_channel() {
    let fake = FakeDiscord::start();
    let mut hub = hub(&fake, &[CH], "");
    hub.env
        .push(("CATALY_IDLE_SLEEP".into(), std::path::PathBuf::from("5s")));
    hub.start();
    let panel = ready(&hub, &fake);
    let lead = |hub: &TestHub| hub.json(&["lead", "status", "--json"]);
    let slept = hub.until(WAIT, || {
        let v = lead(&hub);
        (v["phase"] == "asleep").then_some(v)
    });
    let session = slept["asleep"]["session_id"].clone();
    assert!(session.is_string(), "{slept}");
    let pids = hub.root.join("bin/omp-data/pids");
    let starts = std::fs::read_to_string(&pids).unwrap().lines().count();

    let done = pick(&fake, GUILD, OWNER, &panel, &[CH, NEW]);
    assert_eq!(
        done["data"]["content"],
        format!("📡 Watching: <#{CH}> <#{NEW}>"),
        "{done}"
    );
    assert!(
        config_text(&hub).contains(&format!("channels = [\"{CH}\", \"{NEW}\"]")),
        "{}",
        config_text(&hub)
    );
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        lead(&hub)["phase"],
        "asleep",
        "the pick does not wake the lead"
    );
    assert_eq!(
        std::fs::read_to_string(&pids).unwrap().lines().count(),
        starts
    );

    let id = fresh(5);
    fake.say(FakeDiscord::message(
        NEW,
        id,
        OWNER,
        "wake up in the new room",
    ));
    answered(&fake, NEW, id);
    let woke = lead(&hub);
    assert_eq!(woke["session"]["session_id"], session, "{woke}");
    assert_eq!(fake.connections().len(), 1, "one Gateway connection");
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
