//! `cataly setup discord` end to end against the fake Discord, as a script runs it (no terminal):
//! a missing or refused token, the Message Content intent off, the bot not yet in a server, two
//! servers, channels found and reused, channels created, and a second run that changes nothing.

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
use support::discord::{APP, APP_OWNER, FakeDiscord, GUILD};

const TOKEN: &str = "setup-token-not-real";
const LEAD: &str = "500000000000000010";
const OPS: &str = "500000000000000011";
/// The invite permissions without and with Manage Channels.
const PERMISSIONS: &str = "326417583168";
const WITH_MANAGE: &str = "326417583184";

/// A sandbox whose config points at the fake, with `token` in the token file (none when `None`).
fn sandbox(token: Option<&str>) -> (TestHub, FakeDiscord) {
    let fake = FakeDiscord::start();
    fake.set_token(TOKEN);
    let hub = TestHub::new("");
    let file = match token {
        Some(t) => hub.private_file("discord-token", t),
        None => hub.root.join("discord-token"),
    };
    std::fs::set_permissions(
        hub.config_path().parent().unwrap(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    hub.write_config(&format!(
        "# mine\n[discord]\ntoken_file = \"{}\"\napi_base = \"{}\"\n",
        file.display(),
        fake.base()
    ));
    (hub, fake)
}

/// Runs `cataly setup discord --json` with `args`: success, the report, stderr.
fn setup(hub: &TestHub, args: &[&str]) -> (bool, Value, String) {
    let mut all = vec!["setup", "discord", "--json"];
    all.extend_from_slice(args);
    let out = hub.cli(&all);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !stdout.contains(TOKEN) && !stderr.contains(TOKEN),
        "token leaked"
    );
    let report = serde_json::from_str(&stdout).unwrap_or(Value::Null);
    (out.status.success(), report, stderr)
}

fn stage(report: &Value) -> &str {
    report["discord"]["stage"].as_str().unwrap_or_default()
}

fn creates(fake: &FakeDiscord) -> Vec<String> {
    fake.calls("POST", "/channels")
        .into_iter()
        .filter(|s| s.path.starts_with("/guilds/"))
        .map(|s| s.body["name"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn a_missing_or_refused_token_stops_at_the_token_step() {
    let (hub, fake) = sandbox(None);
    let out = hub.cli(&["setup", "discord"]);
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("New Application"), "{text}");
    assert!(text.contains("Reset Token"), "{text}");
    assert!(fake.seen().is_empty(), "no token, no request");

    let (hub, fake) = sandbox(Some("not-the-token"));
    let (ok, report, stderr) = setup(&hub, &[]);
    assert!(!ok);
    assert_eq!(stage(&report), "token");
    assert!(stderr.contains("refused the token"), "{stderr}");
    let paths: Vec<String> = fake.seen().into_iter().map(|s| s.path).collect();
    assert_eq!(
        paths,
        ["/users/@me"],
        "nothing else is asked with a bad token"
    );
}

#[test]
fn the_message_content_intent_is_named_then_switched_on_when_asked() {
    let (hub, fake) = sandbox(Some(TOKEN));
    fake.set_app_flags(1 << 23);
    let (ok, report, stderr) = setup(&hub, &[]);
    assert!(!ok);
    assert_eq!(stage(&report), "intent");
    assert!(
        stderr.contains("Privileged Gateway Intents, Message Content Intent"),
        "{stderr}"
    );
    assert!(fake.calls("PATCH", "/applications/@me").is_empty());
    assert_eq!(fake.app_flags(), 1 << 23);

    let (ok, report, _) = setup(&hub, &["--enable-intent"]);
    assert!(!ok, "the bot is in no server yet");
    assert_eq!(report["discord"]["message_content"], "switched on");
    assert_eq!(fake.app_flags(), (1 << 23) | (1 << 19));
    assert_eq!(stage(&report), "guild");
}

#[test]
fn the_invite_link_is_printed_and_the_join_is_awaited() {
    let (hub, fake) = sandbox(Some(TOKEN));
    let (ok, report, stderr) = setup(&hub, &[]);
    assert!(!ok);
    assert_eq!(stage(&report), "guild");
    let invite = report["discord"]["invite"].as_str().unwrap();
    assert_eq!(
        invite,
        format!(
            "https://discord.com/oauth2/authorize?client_id={APP}&scope=bot&permissions={PERMISSIONS}"
        )
    );
    assert!(stderr.contains("no server yet"), "{stderr}");
    let (_, report, _) = setup(&hub, &["--create-channels"]);
    assert!(
        report["discord"]["invite"]
            .as_str()
            .unwrap()
            .ends_with(&format!("permissions={WITH_MANAGE}"))
    );

    // The bot joins while setup waits; it goes on to the channels, which it may not create.
    std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_secs(1));
            fake.join(GUILD, "home");
        });
        let (ok, report, stderr) = setup(&hub, &["--wait", "20s"]);
        assert!(!ok);
        assert_eq!(stage(&report), "channels", "{stderr}");
        assert_eq!(report["discord"]["guild"]["id"], GUILD);
        assert!(stderr.contains("--create-channels"), "{stderr}");
    });
    assert_eq!(creates(&fake), Vec::<String>::new());
}

#[test]
fn several_servers_need_a_choice() {
    let (hub, fake) = sandbox(Some(TOKEN));
    fake.join(GUILD, "home");
    fake.join("500000000000000099", "work");
    let (ok, report, stderr) = setup(&hub, &[]);
    assert!(!ok);
    assert_eq!(stage(&report), "guild");
    assert!(stderr.contains("--guild"), "{stderr}");
    assert_eq!(report["discord"]["guilds"].as_array().unwrap().len(), 2);
    let (_, report, _) = setup(&hub, &["--guild", GUILD]);
    assert_eq!(report["discord"]["guild"]["name"], "home");
    assert_eq!(stage(&report), "channels");
}

#[test]
fn existing_channels_are_reused_and_a_second_run_changes_nothing() {
    let (hub, fake) = sandbox(Some(TOKEN));
    fake.join(GUILD, "home");
    fake.named(LEAD, "cataly", 0, GUILD);
    fake.named(OPS, "cataly-ops", 0, GUILD);
    fake.named("500000000000000012", "cataly", 2, GUILD);
    fake.named("500000000000000013", "general", 0, GUILD);

    // No owner yet: a script must name him.
    let (ok, report, stderr) = setup(&hub, &["--create-channels"]);
    assert!(!ok);
    assert_eq!(stage(&report), "owner");
    assert!(stderr.contains(APP_OWNER), "{stderr}");

    let (ok, preview, stderr) = setup(&hub, &["--owner", "app", "--create-channels"]);
    assert!(ok, "{stderr}");
    assert_eq!(creates(&fake), Vec::<String>::new(), "both channels exist");
    let d = &preview["discord"];
    assert_eq!(d["stage"], "ready");
    assert_eq!(d["owner"]["id"], APP_OWNER);
    assert_eq!(d["channels"][0]["id"], LEAD);
    assert_eq!(d["channels"][0]["state"], "existing");
    assert_eq!(d["channels"][1]["id"], OPS);
    assert_eq!(preview["action"], "update");
    assert_eq!(preview["valid"], true);
    let before = std::fs::read_to_string(hub.config_path()).unwrap();

    let digest = preview["before"].as_str().unwrap();
    let (ok, _, stderr) = setup(&hub, &["--owner", "app", "--apply", "--expect", "0000"]);
    assert!(
        !ok && stderr.contains("changed since its snapshot"),
        "{stderr}"
    );
    assert_eq!(std::fs::read_to_string(hub.config_path()).unwrap(), before);
    let (ok, done, stderr) = setup(&hub, &["--owner", "app", "--apply", "--expect", digest]);
    assert!(ok, "{stderr}");
    assert_eq!(done["applied"], true);
    let text = std::fs::read_to_string(hub.config_path()).unwrap();
    assert!(text.starts_with("# mine\n"), "{text}");
    for line in [
        format!("guild = \"{GUILD}\""),
        format!("owner = \"{APP_OWNER}\""),
        format!("channels = [\"{LEAD}\"]"),
        "[destinations.cataly-ops]\nkind = \"discord\"".to_string(),
        format!("channel = \"{OPS}\""),
    ] {
        assert!(text.contains(&line), "{line} missing:\n{text}");
    }
    let check = hub.cli(&["config", "check"]);
    assert!(check.status.success(), "{check:?}");

    // Owner, server and channels now come from the config and Discord: nothing changes.
    let (ok, again, stderr) = setup(&hub, &[]);
    assert!(ok, "{stderr}");
    assert_eq!(again["action"], "keep");
    assert_eq!(again["discord"]["owner"]["source"], "config");
    let (ok, again, _) = setup(&hub, &["--apply"]);
    assert!(ok);
    assert_eq!(again["action"], "keep");
    assert_eq!(std::fs::read_to_string(hub.config_path()).unwrap(), text);
    assert_eq!(creates(&fake), Vec::<String>::new());
    assert!(fake.calls("PATCH", "/applications/@me").is_empty());
}

#[test]
fn missing_channels_are_created_once_after_confirmation() {
    let (hub, fake) = sandbox(Some(TOKEN));
    fake.join(GUILD, "home");
    fake.named(LEAD, "cataly", 0, GUILD);
    let owner = "100000000000000001";
    let (ok, report, _) = setup(&hub, &["--owner", owner]);
    assert!(!ok, "no confirmation, no channel");
    assert_eq!(stage(&report), "channels");
    assert_eq!(creates(&fake), Vec::<String>::new());

    let (ok, preview, stderr) = setup(&hub, &["--owner", owner, "--create-channels"]);
    assert!(ok, "{stderr}");
    assert_eq!(creates(&fake), ["cataly-ops"], "only the missing one");
    let made = &preview["discord"]["channels"][1];
    assert_eq!(made["state"], "created");
    let ops = made["id"].as_str().unwrap().to_string();
    let body = &fake.calls("POST", "/guilds/")[0].body;
    assert_eq!(body["type"], 0);

    let digest = preview["before"].as_str().unwrap();
    let (ok, _, stderr) = setup(&hub, &["--owner", owner, "--apply", "--expect", digest]);
    assert!(ok, "{stderr}");
    let text = std::fs::read_to_string(hub.config_path()).unwrap();
    assert!(text.contains(&format!("channel = \"{ops}\"")), "{text}");

    let (ok, again, _) = setup(&hub, &["--create-channels"]);
    assert!(ok);
    assert_eq!(again["action"], "keep");
    assert_eq!(again["discord"]["channels"][1]["state"], "existing");
    assert_eq!(creates(&fake).len(), 1, "a second run creates nothing");
}

#[test]
fn the_lead_model_and_remote_approvals_are_written_when_asked() {
    let (hub, fake) = sandbox(Some(TOKEN));
    fake.join(GUILD, "home");
    fake.named(LEAD, "cataly", 0, GUILD);
    fake.named(OPS, "cataly-ops", 0, GUILD);
    let (ok, report, stderr) = setup(&hub, &["--owner", "app", "--lead-model", "opus"]);
    assert!(!ok && stderr.contains("--lead-model"), "{stderr} {report}");

    let flags = [
        "--owner",
        "app",
        "--lead-model",
        "anthropic/claude-opus-5-5",
        "--remote-approvals",
    ];
    let mut apply = flags.to_vec();
    apply.push("--apply");
    let (ok, done, stderr) = setup(&hub, &apply);
    assert!(ok, "{stderr}");
    assert_eq!(done["applied"], true);
    let text = std::fs::read_to_string(hub.config_path()).unwrap();
    assert!(text.contains("remote_approvals = true\n"), "{text}");
    assert!(
        text.contains("[lead]\nmodel = \"anthropic/claude-opus-5-5\"\n"),
        "{text}"
    );
    let check = hub.cli(&["config", "check"]);
    assert!(check.status.success(), "{check:?}");

    // Set already: the same flags change nothing, and without them nothing is asked or removed.
    let (ok, again, _) = setup(&hub, &apply);
    assert!(ok);
    assert_eq!(again["action"], "keep");
    let (ok, again, _) = setup(&hub, &[]);
    assert!(ok);
    assert_eq!(again["action"], "keep");
    assert_eq!(std::fs::read_to_string(hub.config_path()).unwrap(), text);
}
