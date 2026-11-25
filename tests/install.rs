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
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use support::github::FakeGithub;
use support::{BIN, TestHub, path_without_omp};

const SEED: [u8; 32] = [9; 32];
const V1: &str = env!("CARGO_PKG_VERSION");

fn target() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin-arm64"
    } else {
        "linux-x64"
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn pair(seed: [u8; 32]) -> ring::signature::Ed25519KeyPair {
    ring::signature::Ed25519KeyPair::from_seed_unchecked(&seed).unwrap()
}

fn key_hex() -> String {
    hex(ring::signature::KeyPair::public_key(&pair(SEED)).as_ref())
}

fn sha256(bytes: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

/// Rewrites SHA256SUMS for every other file in `dir` and signs it with `seed`.
fn sign_folder(dir: &Path, seed: [u8; 32]) {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with("SHA256SUMS"))
        .collect();
    names.sort();
    let sums = names.iter().fold(String::new(), |mut s, n| {
        use std::fmt::Write as _;
        let _ = writeln!(s, "{}  {n}", sha256(&std::fs::read(dir.join(n)).unwrap()));
        s
    });
    std::fs::write(dir.join("SHA256SUMS"), &sums).unwrap();
    let sig = hex(pair(seed).sign(sums.as_bytes()).as_ref());
    std::fs::write(dir.join("SHA256SUMS.sig"), sig).unwrap();
}

/// A folder of release assets for `binary`, which reports `version`.
fn release(dir: &Path, binary: &Path, version: &str) -> PathBuf {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    std::fs::copy(binary, dir.join(format!("cataly-{version}-{}", target()))).unwrap();
    let ext = Command::new(binary).arg("extension").output().unwrap();
    assert!(ext.status.success());
    std::fs::write(dir.join(format!("cataly-{version}.ts")), ext.stdout).unwrap();
    sign_folder(dir, SEED);
    dir.to_path_buf()
}

/// The next patch version after this crate's.
fn bump(version: &str, by: u64) -> String {
    let mut parts: Vec<u64> = version.split('.').map(|p| p.parse().unwrap()).collect();
    parts[2] += by;
    format!("{}.{}.{}", parts[0], parts[1], parts[2])
}

/// A build of this crate reporting `version`; `broken` makes its hub exit at once. Built once into
/// `target/install-variants` and reused.
fn variant(version: &str, broken: bool) -> PathBuf {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let out_dir = repo.join("target/install-variants");
    std::fs::create_dir_all(&out_dir).unwrap();
    let name = format!("cataly-{version}{}", if broken { "-broken" } else { "" });
    let out = out_dir.join(&name);
    let lock = std::fs::File::create(out_dir.join(".lock")).unwrap();
    lock.lock().unwrap();
    if out.exists() {
        return out;
    }
    let copy = out_dir.join(format!("src-{name}"));
    let _ = std::fs::remove_dir_all(&copy);
    copy_tree(repo, &copy);
    let manifest = copy.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest).unwrap();
    let text = text.replacen(
        &format!("\nversion = \"{V1}\"\n"),
        &format!("\nversion = \"{version}\"\n"),
        1,
    );
    std::fs::write(&manifest, text).unwrap();
    if broken {
        let hub = copy.join("src/hub/mod.rs");
        let text = std::fs::read_to_string(&hub).unwrap();
        let head = "pub(crate) fn run(paths: &Paths) -> ExitCode {";
        assert!(text.contains(head));
        let text = text.replacen(head, &format!("{head}\n    if !paths.socket.as_os_str().is_empty() {{ return ExitCode::from(1); }}"), 1);
        std::fs::write(&hub, text).unwrap();
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let built = Command::new(cargo)
        .args(["build", "--offline", "--bin", "cataly"])
        .current_dir(&copy)
        .env("CARGO_TARGET_DIR", out_dir.join("target"))
        // Without debug info a debug build stays under the 128 MiB binary bound (IR req. 9).
        .env("CARGO_PROFILE_DEV_DEBUG", "false")
        .status()
        .unwrap();
    assert!(built.success(), "the {name} build failed");
    std::fs::copy(out_dir.join("target/debug/cataly"), &out).unwrap();
    out
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().filter_map(Result::ok) {
        let name = entry.file_name();
        if name == "target" || name == ".git" {
            continue;
        }
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&path, &to.join(&name));
        } else if entry.file_type().unwrap().is_file() {
            std::fs::copy(&path, to.join(&name)).unwrap();
        }
    }
}

/// A sandbox home with the test release key.
fn sandbox(config: &str) -> TestHub {
    let mut hub = TestHub::new(config);
    hub.env
        .push(("CATALY_RELEASE_KEY".into(), PathBuf::from(key_hex())));
    hub
}

fn root(hub: &TestHub) -> PathBuf {
    hub.root.join("home/.local/share/cataly")
}

fn command_link(hub: &TestHub) -> PathBuf {
    hub.root.join("home/.local/bin/cataly")
}

/// `program args` with the sandbox's environment.
fn with_env(hub: &TestHub, program: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.env_clear()
        .env("PATH", path_without_omp())
        .env("HOME", hub.root.join("home"))
        .env("XDG_CONFIG_HOME", hub.root.join("config"))
        .env("XDG_STATE_HOME", hub.root.join("state"))
        .env("XDG_RUNTIME_DIR", hub.root.join("run"))
        .stdin(Stdio::null());
    for (k, v) in &hub.env {
        cmd.env(k, v);
    }
    cmd
}

fn run(hub: &TestHub, program: &Path, args: &[&str]) -> Output {
    with_env(hub, program).args(args).output().unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "status {:?}\nstdout {}\nstderr {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
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
    assert!(
        !hub.root.join("config/cataly/config.toml").exists()
            || std::fs::read(hub.config_path()).unwrap().is_empty()
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
    let foreign: &[u8] = if target() == "linux-x64" {
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

    hub.start();
    let before = snapshot(&hub);
    let out = run(
        &hub,
        &command_link(&hub),
        &["update", "--from", r2.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(6), "{}", text(&out));
    assert!(String::from_utf8_lossy(&out.stderr).contains("hub is running"));
    assert_eq!(snapshot(&hub), before, "held: nothing changed");
    hub.stop();

    let out = run(
        &hub,
        &command_link(&hub),
        &["update", "--from", r2.to_str().unwrap()],
    );
    assert!(out.status.success(), "{}", text(&out));
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

fn installed_hub(hub: &TestHub) -> std::process::Child {
    with_env(hub, &command_link(hub))
        .arg("hub")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn hub_version(hub: &TestHub) -> Option<String> {
    let reply = hub.try_request(r#"{"v":1,"id":1,"method":"status","params":{}}"#)?;
    let v: Value = serde_json::from_str(&reply).ok()?;
    v["ok"]["version"].as_str().map(str::to_string)
}

fn stop_hub(hub: &TestHub) {
    let _ = hub.try_request(r#"{"v":1,"id":2,"method":"hub.stop","params":{}}"#);
    hub.until(Duration::from_secs(30), || {
        hub_version(hub).is_none().then_some(())
    });
}

/// Check 4: `--restart` switches to a hub that answers with the new version, switches back when the
/// new hub does not report healthy within 30 s, and holds while a session is busy.
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
        &["update", "--restart", "--from", r2.to_str().unwrap()],
    );
    assert!(out.status.success(), "{}", text(&out));
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    eprintln!("measure: update --restart: {said}");
    assert_eq!(hub_version(&hub).as_deref(), Some(v2.as_str()));
    let _ = old.wait();

    // The broken build's hub never answers: after 30 s the switch goes back to v2.
    let out = run(
        &hub,
        &command_link(&hub),
        &["update", "--restart", "--from", r3.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(7), "{}", text(&out));
    assert_eq!(hub_version(&hub).as_deref(), Some(v2.as_str()));
    let (_, s) = status_install(&hub);
    assert_eq!(s["current"]["version"], v2.as_str());
    stop_hub(&hub);
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
    let out = run(&hub, BIN.as_ref(), &["rollback", "--restart"]);
    assert_eq!(out.status.code(), Some(6), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("dialog"),
        "{}",
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
    let latest = |g: &FakeGithub| {
        g.seen()
            .iter()
            .filter(|p| p.ends_with("/releases/latest"))
            .count()
    };

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
