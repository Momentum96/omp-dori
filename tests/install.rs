//! Milestone 7 acceptance checks 1-4 and 7 (DESIGN 11; IR req. 13-40): install through
//! `install.sh` from a local folder, reinstall, update, rollback, status repair, every refusal with
//! nothing changed, the restart option (switch, switch back, hold while busy), and the daily release
//! notice against a fake GitHub. Everything runs in a temporary home and program folder; releases
//! are signed with a test key handed to the debug binary through `CATALY_RELEASE_KEY`. Higher
//! versions are real builds of a copy of this crate with the manifest version raised.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant};

use serde_json::Value;
use support::discord::FakeDiscord;
use support::github::FakeGithub;
use support::release::{
    SEED, V1, bump, command_link, copy_tree, hub_version, installed_hub, release, root, run,
    sha256, sign_folder, stop_hub, target, text, trust_test_key, variant, with_env,
};
use support::{ADMISSION, BIN, TestHub};

/// A sandbox home with the test release key.
fn sandbox(config: &str) -> TestHub {
    let mut hub = TestHub::new(config);
    trust_test_key(&mut hub);
    hub
}

/// Every entry under the program folder and both links, with type, mode, size and link target;
/// the lock and staging folders are bookkeeping and left out.
fn snapshot(hub: &TestHub) -> Vec<String> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.filter_map(Result::ok) {
            let path = e.path();
            let rel = path.strip_prefix(base).unwrap().display().to_string();
            if rel == "lock" || rel.starts_with("staging") {
                continue;
            }
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let link = std::fs::read_link(&path).map_or_default(|l| l.display().to_string());
            let content = if meta.is_file() {
                sha256(&std::fs::read(&path).unwrap())
            } else {
                String::new()
            };
            out.push(format!(
                "{rel} {:o} {} {link} {content}",
                meta.mode(),
                meta.len()
            ));
            if meta.is_dir() {
                walk(&path, base, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(&root(hub), &root(hub), &mut out);
    for link in [
        command_link(hub),
        hub.root.join("home/.omp/agent/extensions/cataly.ts"),
    ] {
        out.push(format!(
            "{} {:?}",
            link.display(),
            std::fs::read_link(&link).ok()
        ));
    }
    out.sort();
    out
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o7777
}

fn status_install(hub: &TestHub) -> (Output, Value) {
    let out = run(hub, BIN.as_ref(), &["status", "--install", "--json"]);
    let v = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    (out, v)
}

fn install_sh(hub: &TestHub, folder: &Path) -> Output {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh");
    with_env(hub, Path::new("sh"))
        .arg(script)
        .arg(folder)
        .output()
        .unwrap()
}

/// DESIGN 11.2 layout and modes after a first install; returns the program folder and extension link.
fn assert_layout(hub: &TestHub) -> (PathBuf, PathBuf) {
    let program = root(hub);
    let vdir = program.join("versions").join(V1);
    assert_eq!(mode(&program), 0o700);
    assert_eq!(mode(&vdir), 0o555);
    assert_eq!(mode(&vdir.join("cataly")), 0o555);
    assert_eq!(mode(&vdir.join("cataly.ts")), 0o444);
    let current = std::fs::read_link(program.join("current")).unwrap();
    assert_eq!(current, Path::new("versions").join(V1));
    let command = std::fs::read_link(command_link(hub)).unwrap();
    assert_eq!(command, program.join("current/cataly"));
    let ext = hub.root.join("home/.omp/agent/extensions/cataly.ts");
    let target = std::fs::read_link(&ext).unwrap();
    assert_eq!(target, program.join("current/cataly.ts"));
    (program, ext)
}

/// Check 1 and the refusals of check 3: install through install.sh, layout and modes, both links,
/// a reinstall that changes nothing, and each refused candidate leaving everything as it was.
#[test]
fn install_reinstall_and_refusals() {
    let hub = sandbox("");
    let r1 = release(&hub.root.join("r1"), &variant(V1, false), V1);
    let started = Instant::now();
    let out = install_sh(&hub, &r1);
    let took = started.elapsed();
    assert!(out.status.success(), "{}", text(&out));
    eprintln!(
        "measure: install through install.sh {} ms",
        took.as_millis()
    );
    let (program, ext) = assert_layout(&hub);
    assert_eq!(mode(ext.parent().unwrap()), 0o700);
    let version = run(&hub, &command_link(&hub), &["version", "--json"]);
    let v: Value = serde_json::from_slice(&version.stdout).unwrap();
    assert_eq!(v["version"], V1);
    assert_eq!(v["target"], target());
    // Install leaves the config as the sandbox wrote it: empty but for `TestHub`'s admission floors.
    assert!(
        !hub.root.join("config/cataly/config.toml").exists()
            || std::fs::read_to_string(hub.config_path()).unwrap() == ADMISSION
    );
    assert!(
        !hub.root.join("state/cataly").exists(),
        "install creates no state"
    );
    let (out, s) = status_install(&hub);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(s["current"]["version"], V1);
    assert_eq!(
        s["current"]["binary_sha256"],
        sha256(&std::fs::read(r1.join(format!("cataly-{V1}-{}", target()))).unwrap())
    );

    let before = snapshot(&hub);
    let out = install_sh(&hub, &r1);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("nothing changed"),
        "{}",
        text(&out)
    );
    assert_eq!(snapshot(&hub), before, "a reinstall changes nothing");

    let refused = |folder: &Path, why: &str| {
        let out = run(
            &hub,
            BIN.as_ref(),
            &["install", "--from", folder.to_str().unwrap()],
        );
        assert_eq!(out.status.code(), Some(6), "{why}: {}", text(&out));
        assert_eq!(snapshot(&hub), before, "{why}: nothing changed");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let bin_name = format!("cataly-{V1}-{}", target());
    let ext_name = format!("cataly-{V1}.ts");
    let copy = |tag: &str| {
        let dir = hub.root.join(tag);
        let _ = std::fs::remove_dir_all(&dir);
        support_copy(&r1, &dir);
        dir
    };
    let d = copy("tampered-bin");
    append(&d.join(&bin_name), b"x");
    assert!(refused(&d, "tampered binary").contains("does not match"));
    let d = copy("tampered-ext");
    append(&d.join(&ext_name), b"// x\n");
    assert!(refused(&d, "tampered extension").contains("does not match"));
    let d = copy("missing-line");
    std::fs::remove_file(d.join(&ext_name)).unwrap();
    sign_folder(&d, SEED);
    std::fs::copy(r1.join(&ext_name), d.join(&ext_name)).unwrap();
    assert!(refused(&d, "missing checksum line").contains("no line"));
    let d = copy("bad-signature");
    sign_folder(&d, [3; 32]);
    assert!(refused(&d, "bad signature").contains("does not match"));
    let d = copy("wrong-target");
    let foreign: &[u8] = if target().starts_with("linux-") {
        &[0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0, 0, 1, 0, 0, 0, 0]
    } else {
        &[
            0x7f, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0x3e, 0,
        ]
    };
    std::fs::write(d.join(&bin_name), foreign).unwrap();
    sign_folder(&d, SEED);
    assert!(refused(&d, "wrong-target binary").contains("not an executable"));

    // Uninstall removes the program folder and both links only.
    let out = run(&hub, BIN.as_ref(), &["uninstall"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!program.exists() && !command_link(&hub).exists() && !ext.exists());
    assert!(
        hub.root.join("home/.omp/agent/extensions").is_dir(),
        "OMP's folder stays"
    );
}

/// Check 3, the link collision: a foreign file at the command link path of a fresh home is refused
/// and nothing is created.
#[test]
fn foreign_link_is_refused() {
    let fresh = sandbox("");
    let r1 = release(&fresh.root.join("r1"), &variant(V1, false), V1);
    std::fs::create_dir_all(fresh.root.join("home/.local/bin")).unwrap();
    std::fs::write(command_link(&fresh), "mine").unwrap();
    let out = run(
        &fresh,
        BIN.as_ref(),
        &["install", "--from", r1.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(6), "{}", text(&out));
    assert!(String::from_utf8_lossy(&out.stderr).contains("move it away"));
    assert!(!root(&fresh).exists(), "nothing created");
    assert_eq!(std::fs::read(command_link(&fresh)).unwrap(), b"mine");
}

fn support_copy(from: &Path, to: &Path) {
    copy_tree(from, to);
}

fn append(path: &Path, bytes: &[u8]) {
    let mut data = std::fs::read(path).unwrap();
    data.extend_from_slice(bytes);
    std::fs::write(path, data).unwrap();
}

/// Check 2 and the running-hub refusal of check 3: update to a higher build (held while a hub
/// answers), rollback, status clean after each, and an interrupted switch repaired by status.
#[test]
fn update_rollback_and_repair() {
    let v2 = bump(V1, 1);
    let mut hub = sandbox("");
    let r1 = release(&hub.root.join("r1"), &variant(V1, false), V1);
    let r2 = release(&hub.root.join("r2"), &variant(&v2, false), &v2);
    let out = run(
        &hub,
        BIN.as_ref(),
        &["install", "--from", r1.to_str().unwrap()],
    );
    assert!(out.status.success(), "{}", text(&out));

    // --no-restart switches the files and leaves the running hub on its version.
    hub.start();
    let out = run(
        &hub,
        &command_link(&hub),
        &["update", "--no-restart", "--from", r2.to_str().unwrap()],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("stays on"),
        "{}",
        text(&out)
    );
    assert_eq!(
        hub_version(&hub).as_deref(),
        Some(V1),
        "the hub was not touched"
    );
    hub.stop();
    let (out, s) = status_install(&hub);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(s["current"]["version"], v2.as_str());
    assert_eq!(s["previous"]["version"], V1);
    let version = run(&hub, &command_link(&hub), &["version", "--json"]);
    assert!(String::from_utf8_lossy(&version.stdout).contains(&v2));

    let out = run(
        &hub,
        &command_link(&hub),
        &["rollback", "--expect", "9.9.9"],
    );
    assert_eq!(out.status.code(), Some(6), "{}", text(&out));
    let out = run(&hub, &command_link(&hub), &["rollback", "--expect", V1]);
    assert!(out.status.success(), "{}", text(&out));
    let (out, s) = status_install(&hub);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(s["current"]["version"], V1);
    assert_eq!(s["previous"]["version"], v2.as_str());
    assert_eq!(
        std::fs::read_to_string(root(&hub).join("silenced")).unwrap(),
        v2
    );
    // An older candidate is refused outside rollback.
    let out = run(
        &hub,
        BIN.as_ref(),
        &["update", "--from", r2.to_str().unwrap()],
    );
    assert!(
        out.status.success(),
        "updating to the newer one again works: {}",
        text(&out)
    );
    let out = run(
        &hub,
        BIN.as_ref(),
        &["install", "--from", r1.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(6), "{}", text(&out));
    assert!(String::from_utf8_lossy(&out.stderr).contains("only `cataly rollback`"));

    // Killed between rename and record: the link names the other version, records do not.
    let current = root(&hub).join("current");
    let tmp = root(&hub).join("current.tmp");
    std::os::unix::fs::symlink(Path::new("versions").join(V1), &tmp).unwrap();
    std::fs::rename(&tmp, &current).unwrap();
    let (out, s) = status_install(&hub);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(s["repaired"].as_array().unwrap().len(), 2, "{s}");
    assert_eq!(s["current"]["version"], V1);
    assert_eq!(s["previous"]["version"], v2.as_str());
    let (out, s) = status_install(&hub);
    assert!(
        out.status.success() && s["repaired"].as_array().unwrap().is_empty(),
        "{s}"
    );
}

/// Check 4: a plain `cataly update` restarts a running hub on the new version, switches back when
/// the new hub does not report healthy within 30 s, and holds while a session is busy.
#[test]
fn restart_switches_back_and_holds_while_busy() {
    let (v2, v3) = (bump(V1, 1), bump(V1, 2));
    let hub = sandbox("");
    let r1 = release(&hub.root.join("r1"), &variant(V1, false), V1);
    let r2 = release(&hub.root.join("r2"), &variant(&v2, false), &v2);
    let r3 = release(&hub.root.join("r3"), &variant(&v3, true), &v3);
    let out = run(
        &hub,
        BIN.as_ref(),
        &["install", "--from", r1.to_str().unwrap()],
    );
    assert!(out.status.success(), "{}", text(&out));

    let mut old = installed_hub(&hub);
    hub.until(Duration::from_secs(20), || {
        (hub_version(&hub).as_deref() == Some(V1)).then_some(())
    });
    let out = run(
        &hub,
        &command_link(&hub),
        &["update", "--from", r2.to_str().unwrap()],
    );
    assert!(out.status.success(), "{}", text(&out));
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    eprintln!("measure: update: {said}");
    assert_eq!(hub_version(&hub).as_deref(), Some(v2.as_str()));
    let _ = old.wait();

    // The broken build's hub never answers: after 30 s the switch goes back to v2.
    let out = run(
        &hub,
        &command_link(&hub),
        &["update", "--from", r3.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(7), "{}", text(&out));
    assert_eq!(hub_version(&hub).as_deref(), Some(v2.as_str()));
    let (_, s) = status_install(&hub);
    assert_eq!(s["current"]["version"], v2.as_str());
    stop_hub(&hub);
}

/// The owner's plain `cataly update` while the hub runs: the new hub thanks the owner once in his
/// Discord home room, a rollback says so, and an ordinary restart says nothing.
#[test]
fn the_updated_hub_says_so_once_in_the_home_room() {
    const OWNER: &str = "400000000000000001";
    const CH: &str = "600000000000000001";
    let v2 = bump(V1, 1);
    let fake = FakeDiscord::start();
    fake.channel(CH, 0, None);
    let hub = sandbox("");
    let token = hub.private_file("discord-token", "fake-token");
    hub.write_config(&fake.config(&token, OWNER, &[CH]));
    let r1 = release(&hub.root.join("r1"), &variant(V1, false), V1);
    let r2 = release(&hub.root.join("r2"), &variant(&v2, false), &v2);
    let out = run(
        &hub,
        BIN.as_ref(),
        &["install", "--from", r1.to_str().unwrap()],
    );
    assert!(out.status.success(), "{}", text(&out));
    let posts = |f: &FakeDiscord| -> Vec<String> {
        f.bot_messages(CH)
            .iter()
            .map(|m| m["content"].as_str().unwrap_or_default().to_string())
            .collect()
    };

    let mut first = installed_hub(&hub);
    hub.until(Duration::from_secs(20), || {
        (hub_version(&hub).as_deref() == Some(V1)).then_some(())
    });
    let out = run(
        &hub,
        &command_link(&hub),
        &["update", "--from", r2.to_str().unwrap()],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(hub_version(&hub).as_deref(), Some(v2.as_str()));
    let _ = first.wait();
    let updated = fake.wait(Duration::from_secs(90), |f| {
        let p = posts(f);
        (!p.is_empty()).then_some(p)
    });
    assert_eq!(updated.len(), 1, "{updated:?}");
    assert!(
        updated[0].starts_with(&format!(
            "{v2} 버전으로 업데이트했습니다. 보스, 감사합니다!\n{V1} -> {v2}"
        )),
        "{updated:?}"
    );

    let out = run(&hub, &command_link(&hub), &["rollback"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(hub_version(&hub).as_deref(), Some(V1));
    let after = fake.wait(Duration::from_secs(90), |f| {
        let p = posts(f);
        (p.len() >= 2).then_some(p)
    });
    assert!(
        after[1].starts_with(&format!("{V1} 버전으로 되돌렸습니다.\n{v2} -> {V1}")),
        "{after:?}"
    );

    // An ordinary restart of the same version says nothing.
    stop_hub(&hub);
    let mut again = installed_hub(&hub);
    hub.until(Duration::from_secs(20), || {
        (hub_version(&hub).as_deref() == Some(V1)).then_some(())
    });
    std::thread::sleep(Duration::from_secs(5));
    assert_eq!(posts(&fake).len(), 2, "{:?}", posts(&fake));
    stop_hub(&hub);
    let _ = again.wait();
}

/// Check 4, the hold: a lead waiting on a dialog refuses the orderly stop; after 60 s the update is
/// held, nothing changed and the hub still runs.
#[test]
fn restart_holds_while_a_session_is_busy() {
    let v2 = bump(V1, 1);
    let mut hub = sandbox("");
    let omp = hub.install_fake("omp");
    for f in ["rpc", "turns", "ask"] {
        hub.fixture("omp", f, Some(""));
    }
    let work = hub.root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    hub.write_config(&format!(
        "[omp]\npath = \"{}\"\n[lead]\nmodel = \"fake/primary\"\ncwd = \"{}\"\n",
        omp.display(),
        work.display()
    ));
    let r1 = release(&hub.root.join("r1"), &variant(V1, false), V1);
    let r2 = release(&hub.root.join("r2"), &variant(&v2, false), &v2);
    let out = run(
        &hub,
        BIN.as_ref(),
        &["install", "--from", r1.to_str().unwrap()],
    );
    assert!(out.status.success(), "{}", text(&out));
    let out = run(
        &hub,
        &command_link(&hub),
        &["update", "--from", r2.to_str().unwrap()],
    );
    assert!(out.status.success(), "{}", text(&out));
    // Back on V1 (its extension matches the test hub), with v2 kept as the previous version.
    let out = run(&hub, &command_link(&hub), &["rollback"]);
    assert!(out.status.success(), "{}", text(&out));
    std::fs::remove_file(root(&hub).join("silenced")).unwrap();
    hub.start();
    hub.until(Duration::from_secs(20), || {
        (hub.json(&["lead", "status", "--json"])["phase"] == "live").then_some(())
    });
    let sent = hub.cli(&["lead", "send", "please ASK me"]);
    assert!(sent.status.success(), "{sent:?}");
    hub.until(Duration::from_secs(20), || {
        let v = hub.json(&["lead", "status", "--json"]);
        v["session"]["dialogs"]
            .as_array()
            .is_some_and(|d| !d.is_empty())
            .then_some(())
    });
    let before = snapshot(&hub);
    let started = Instant::now();
    // This build, not the installed variant (variants are cached builds of an earlier tree).
    let out = run(&hub, BIN.as_ref(), &["rollback"]);
    assert_eq!(out.status.code(), Some(6), "{}", text(&out));
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        said.contains("dialog") && said.contains("작업이 끝나면 다시 실행해주세요"),
        "{}",
        text(&out)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.contains("기다리는 중"))
            .count(),
        1,
        "one waiting line: {}",
        text(&out)
    );
    assert!(started.elapsed() >= Duration::from_mins(1));
    assert_eq!(snapshot(&hub), before, "held: nothing changed");
    assert!(
        !root(&hub).join("silenced").exists(),
        "a held rollback does not silence the version"
    );
    assert_eq!(
        hub_version(&hub).as_deref(),
        Some(V1),
        "the hub was not stopped"
    );
}

/// Check 7: one notice per newer stable release, never for a version rolled back from, no request
/// with the check off; `cataly check` and `cataly update` read the same release and download its
/// assets through GitHub's redirect.
#[test]
fn update_notice_check_and_download() {
    let v2 = bump(V1, 1);
    let github = FakeGithub::start();
    let config = format!(
        "[update]\napi_base = \"{}\"\nfirst_after = \"1s\"\n",
        github.base()
    );
    let mut hub = sandbox(&config);
    let r2 = release(&hub.root.join("r2"), &variant(&v2, false), &v2);
    github.publish(&format!("v{v2}"), r2);
    let latest = |g: &FakeGithub| g.seen().iter().filter(|p| p.ends_with("/releases")).count();

    hub.start();
    let notices = |h: &TestHub| {
        h.events()
            .into_iter()
            .filter(|e| e["kind"] == "update.available")
            .count()
    };
    hub.until(Duration::from_secs(20), || {
        (notices(&hub) == 1).then_some(())
    });
    let event = hub
        .events()
        .into_iter()
        .find(|e| e["kind"] == "update.available")
        .unwrap();
    assert!(
        event["text"].as_str().unwrap().contains("cataly update"),
        "{event}"
    );
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(notices(&hub), 1);
    assert_eq!(latest(&github), 1, "one check a day");
    hub.stop();
    hub.start();
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(notices(&hub), 1, "announced once, also after a restart");
    assert_eq!(latest(&github), 1);
    hub.stop();

    // `cataly check` and an update from GitHub through the redirect; the sandbox's bridge link
    // made for the hub above is not Cataly's install link, so it goes first.
    std::fs::remove_file(hub.root.join("home/.omp/agent/extensions/cataly.ts")).unwrap();
    let r1 = release(&hub.root.join("r1"), &variant(V1, false), V1);
    let out = run(
        &hub,
        BIN.as_ref(),
        &["install", "--from", r1.to_str().unwrap()],
    );
    assert!(out.status.success(), "{}", text(&out));
    let out = run(&hub, BIN.as_ref(), &["check", "--json"]);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    assert_eq!(v["verdict"], "newer", "{}", text(&out));
    let out = run(&hub, &command_link(&hub), &["update"]);
    assert!(out.status.success(), "{}", text(&out));
    let (_, s) = status_install(&hub);
    assert_eq!(s["current"]["version"], v2.as_str());
    assert!(
        s["current"]["source"]
            .as_str()
            .unwrap()
            .contains("/releases/tag/v"),
        "{s}"
    );

    // Rolled back from: never announced.
    let mut silenced = sandbox(&config);
    let data = root(&silenced);
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("silenced"), &v2).unwrap();
    std::fs::set_permissions(
        data.join("silenced"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let seen = latest(&github);
    silenced.start();
    silenced.until(Duration::from_secs(20), || {
        (latest(&github) > seen).then_some(())
    });
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(notices(&silenced), 0);
    silenced.stop();

    // Off: no request at all.
    let mut off = sandbox(&format!(
        "[update]\ncheck = false\napi_base = \"{}\"\nfirst_after = \"1s\"\n",
        github.base()
    ));
    let seen = github.seen().len();
    off.start();
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(github.seen().len(), seen, "no request while off");
    assert_eq!(notices(&off), 0);
    off.stop();
}

/// `install.sh` with no tag reads the release list, as GitHub prints it, through a fake `curl`, and
/// installs the newest stable Cataly release: newer prereleases, drafts and releases of other
/// software are skipped, text in a release body is never taken for a tag, and a newest release
/// without this host's binary is refused by name.
#[test]
fn install_sh_picks_the_newest_stable_cataly_release() {
    let hub = sandbox("");
    // A debug build without debug info, so it stays under install.sh's 128 MiB bound.
    let r1 = release(&hub.root.join("r1"), &variant(V1, false), V1);
    let fake = hub.root.join("curl-bin");
    let log = hub.root.join("curl.log");
    std::fs::create_dir_all(&fake).unwrap();
    let curl = format!(
        "#!/bin/sh\nout=''\nprev=''\nurl=''\n\
         for a in \"$@\"; do [ \"$prev\" = -o ] && out=$a; prev=$a; url=$a; done\n\
         echo \"$url\" >> '{log}'\n\
         case \"$url\" in\n\
         'https://api.github.com/repos/vntrevx/Cataly/releases?per_page=30') cp '{list}' \"$out\" ;;\n\
         https://github.com/vntrevx/Cataly/releases/download/v{V1}/*) cp '{r1}'/\"${{url##*/}}\" \"$out\" ;;\n\
         *) exit 22 ;;\nesac\n",
        log = log.display(),
        list = hub.root.join("releases.json").display(),
        r1 = r1.display(),
    );
    std::fs::write(fake.join("curl"), curl).unwrap();
    std::fs::set_permissions(fake.join("curl"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let names = |v: &str, binary: bool| -> Value {
        let mut names = vec![
            "SHA256SUMS".to_string(),
            "SHA256SUMS.sig".into(),
            format!("cataly-{v}.ts"),
            "install.sh".into(),
        ];
        if binary {
            names.push(format!("cataly-{v}-{}", target()));
        }
        names
            .into_iter()
            .map(|n| serde_json::json!({ "name": n, "state": "uploaded", "uploader": { "login": "o" } }))
            .collect()
    };
    let entry = |tag: &str, prerelease: bool, draft: bool, assets: Value| {
        serde_json::json!({
            "url": "https://api.github.com/x", "author": { "login": "o", "id": 1 },
            "tag_name": tag, "name": tag, "draft": draft, "prerelease": prerelease,
            "assets": assets,
            "body": "notes, with {braces} and \"tag_name\": \"v100.0.0\", \"name\": \"SHA256SUMS\"",
        })
    };
    let install = |list: Value| {
        std::fs::write(
            hub.root.join("releases.json"),
            serde_json::to_string_pretty(&list).unwrap(),
        )
        .unwrap();
        let mut path = std::ffi::OsString::from(&fake);
        path.push(":");
        path.push(support::path_without_omp());
        with_env(&hub, Path::new("sh"))
            .env("PATH", path)
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh"))
            .output()
            .unwrap()
    };

    let out = install(serde_json::json!([
        entry("v99.0.0-rc.1", false, false, names("99.0.0-rc.1", true)),
        entry("v98.0.0", true, false, names("98.0.0", true)),
        entry("v97.0.0", false, true, names("97.0.0", true)),
        entry(
            "v96.0.0",
            false,
            false,
            serde_json::json!([{ "name": "other-tool.tar.gz" }])
        ),
        entry(&format!("v{V1}"), false, false, names(V1, true)),
        entry("v0.0.1", false, false, names("0.0.1", true)),
    ]));
    assert!(out.status.success(), "{}", text(&out));
    let (_, s) = status_install(&hub);
    assert_eq!(s["current"]["version"], V1, "{s}");
    let fetched = std::fs::read_to_string(&log).unwrap();
    assert!(
        fetched
            .lines()
            .skip(1)
            .all(|u| u.contains(&format!("/download/v{V1}/"))),
        "{fetched}"
    );

    let out = install(serde_json::json!([
        entry("v50.0.0", false, false, names("50.0.0", false)),
        entry(&format!("v{V1}"), false, false, names(V1, true)),
    ]));
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        said.contains(&format!(
            "release v50.0.0 has no cataly-50.0.0-{}",
            target()
        )),
        "{said}"
    );
}
