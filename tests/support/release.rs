//! Signed release folders, builds of this crate reporting other versions, and the installed hub,
//! for the install tests and the contract case that updates a hub with live sessions.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::Value;

use super::{TestHub, path_without_omp};

pub const SEED: [u8; 32] = [9; 32];
pub const V1: &str = env!("CARGO_PKG_VERSION");

pub fn target() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin-arm64"
    } else {
        "linux-x64"
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn pair(seed: [u8; 32]) -> ring::signature::Ed25519KeyPair {
    ring::signature::Ed25519KeyPair::from_seed_unchecked(&seed).unwrap()
}

pub fn key_hex() -> String {
    hex(ring::signature::KeyPair::public_key(&pair(SEED)).as_ref())
}

pub fn sha256(bytes: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

/// The test release key handed to the debug binary.
pub fn trust_test_key(hub: &mut TestHub) {
    hub.env
        .push(("CATALY_RELEASE_KEY".into(), PathBuf::from(key_hex())));
}

/// Rewrites SHA256SUMS for every other file in `dir` and signs it with `seed`.
pub fn sign_folder(dir: &Path, seed: [u8; 32]) {
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
pub fn release(dir: &Path, binary: &Path, version: &str) -> PathBuf {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    std::fs::copy(binary, dir.join(format!("cataly-{version}-{}", target()))).unwrap();
    let ext = Command::new(binary).arg("extension").output().unwrap();
    assert!(ext.status.success());
    std::fs::write(dir.join(format!("cataly-{version}.ts")), ext.stdout).unwrap();
    sign_folder(dir, SEED);
    dir.to_path_buf()
}

/// The patch version `by` after `version`.
pub fn bump(version: &str, by: u64) -> String {
    let mut parts: Vec<u64> = version.split('.').map(|p| p.parse().unwrap()).collect();
    parts[2] += by;
    format!("{}.{}.{}", parts[0], parts[1], parts[2])
}

/// A build of this crate reporting `version`; `broken` makes its hub exit at once. Built once into
/// `target/install-variants` and reused.
pub fn variant(version: &str, broken: bool) -> PathBuf {
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

pub fn copy_tree(from: &Path, to: &Path) {
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

/// The program folder.
pub fn root(hub: &TestHub) -> PathBuf {
    hub.root.join("home/.local/share/cataly")
}

pub fn command_link(hub: &TestHub) -> PathBuf {
    hub.root.join("home/.local/bin/cataly")
}

/// `program args` with the sandbox's environment.
pub fn with_env(hub: &TestHub, program: &Path) -> Command {
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

pub fn run(hub: &TestHub, program: &Path, args: &[&str]) -> Output {
    with_env(hub, program).args(args).output().unwrap()
}

pub fn text(out: &Output) -> String {
    format!(
        "status {:?}\nstdout {}\nstderr {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The installed command's hub, started by hand.
pub fn installed_hub(hub: &TestHub) -> std::process::Child {
    with_env(hub, &command_link(hub))
        .arg("hub")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

pub fn hub_version(hub: &TestHub) -> Option<String> {
    let reply = hub.try_request(r#"{"v":1,"id":1,"method":"status","params":{}}"#)?;
    let v: Value = serde_json::from_str(&reply).ok()?;
    v["ok"]["version"].as_str().map(str::to_string)
}

pub fn stop_hub(hub: &TestHub) {
    let _ = hub.try_request(r#"{"v":1,"id":2,"method":"hub.stop","params":{}}"#);
    hub.until(Duration::from_secs(30), || {
        hub_version(hub).is_none().then_some(())
    });
}
