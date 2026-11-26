//! Remote workers (docs/design/workers.md), phase 1: enrolment in two steps, the pinned host key, the
//! forced command and its marker, `worker list`, `doctor` and `remove`, and worker mode itself. The
//! worker is a second Cataly home on this machine; `tests/support/fake-ssh` plays its sshd and
//! honours the `authorized_keys` line exactly as sshd applies a forced command, so the real
//! `cataly worker serve` speaks the real stream to the real hub.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use support::{BIN, TestHub, path_without_omp};

/// A worker: its own home, config, state and runtime folders, a host key, and a fake OMP.
struct WorkerBox {
    root: PathBuf,
    host: String,
    port: u16,
}

impl WorkerBox {
    /// Sets up a worker the fake `ssh` reaches as `<host>:<port>`.
    fn new(hub: &TestHub, host: &str, port: u16) -> Self {
        let root = hub.root.join(format!("w-{host}"));
        for dir in ["home", "config", "state", "run", "bin/omp-data"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let omp = root.join("bin/omp");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake-omp"),
            &omp,
        )
        .unwrap();
        std::fs::set_permissions(&omp, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        let w = Self {
            root,
            host: host.into(),
            port,
        };
        w.new_host_key();
        let path = std::env::join_paths(
            std::iter::once(w.root.join("bin")).chain(std::env::split_paths(&path_without_omp())),
        )
        .unwrap();
        let env = format!(
            "HOME={}\nXDG_CONFIG_HOME={}\nXDG_STATE_HOME={}\nXDG_RUNTIME_DIR={}\nPATH={}\n",
            w.root.join("home").display(),
            w.root.join("config").display(),
            w.root.join("state").display(),
            w.root.join("run").display(),
            path.to_string_lossy()
        );
        std::fs::write(w.data_file(""), env).unwrap();
        // Installing cataly on the worker links its bridge into OMP's extension folder.
        let printed = w.cmd(&["extension"]).output().unwrap();
        let file = w.root.join("bridge/cataly.ts");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, &printed.stdout).unwrap();
        let link = w.root.join("home/.omp/agent/extensions/cataly.ts");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&file, &link).unwrap();
        w
    }

    /// `ssh-data/hosts/<host>_<port><suffix>` in the hub's fake `ssh` data.
    fn data_file(&self, suffix: &str) -> PathBuf {
        let dir = self.root.parent().unwrap().join("bin/ssh-data/hosts");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{}_{}{suffix}", self.host, self.port))
    }

    /// A fresh host key, as after reinstalling the worker's sshd.
    fn new_host_key(&self) {
        let key = self.root.join("hostkey");
        let _ = std::fs::remove_file(&key);
        let _ = std::fs::remove_file(self.root.join("hostkey.pub"));
        let made = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .status()
            .unwrap();
        assert!(made.success());
        std::fs::copy(self.root.join("hostkey.pub"), self.data_file(".pub")).unwrap();
    }

    /// The CLI as the worker's user.
    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        let env = std::fs::read_to_string(self.data_file("")).unwrap_or_default();
        cmd.env_clear()
            .env("HOME", self.root.join("home"))
            .env("PATH", path_without_omp());
        for line in env.lines() {
            if let Some((k, v)) = line.split_once('=') {
                cmd.env(k, v);
            }
        }
        cmd.args(args).stdin(Stdio::null());
        cmd
    }

    fn authorized_keys(&self) -> PathBuf {
        self.root.join("home/.ssh/authorized_keys")
    }

    /// `worker init` with the hub's key line; returns the fingerprint it printed.
    fn init(&self, hub_key: &str, extra: &[&str]) -> String {
        let key_file = self.root.join("hostkey.pub");
        let mut args = vec![
            "worker",
            "init",
            "--hub-key",
            hub_key,
            "--host-key-file",
            key_file.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        let out = self.cmd(&args).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "{out:?}");
        assert!(
            text.contains("Lanes from that hub run tools with"),
            "{text}"
        );
        after(&text, "--host-key ")
            .split_whitespace()
            .next()
            .unwrap()
            .to_string()
    }
}

/// The text after `marker`.
fn after<'a>(text: &'a str, marker: &str) -> &'a str {
    let at = text
        .find(marker)
        .unwrap_or_else(|| panic!("no {marker} in {text}"));
    &text[at + marker.len()..]
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A hub with the fake `ssh`, `ssh-keyscan` and `tailscale` first on its `PATH`.
fn hub() -> TestHub {
    let mut hub = TestHub::new("");
    for fake in ["ssh", "ssh-keyscan", "tailscale"] {
        hub.install_fake(fake);
    }
    std::fs::create_dir_all(hub.root.join("bin/ssh-data/hosts")).unwrap();
    let path = std::env::join_paths(
        std::iter::once(hub.root.join("bin")).chain(std::env::split_paths(&path_without_omp())),
    )
    .unwrap();
    hub.env.push(("PATH".into(), PathBuf::from(path)));
    hub
}

/// `worker add` without a host key: the `worker init` line's key.
fn first_add(hub: &TestHub, dest: &str, extra: &[&str]) -> String {
    let mut args = vec!["worker", "add", dest];
    args.extend_from_slice(extra);
    let out = hub.cli(&args);
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    after(&text, "--hub-key '")
        .split('\'')
        .next()
        .unwrap()
        .to_string()
}

fn list(hub: &TestHub) -> Vec<Value> {
    hub.json(&["worker", "list", "--json"])["workers"]
        .as_array()
        .unwrap()
        .clone()
}

/// Enrolment in two steps, the pin, the forced command, list and doctor over the real stream, a
/// changed host key refused, a missing forced command refused, and removal.
#[test]
#[allow(clippy::too_many_lines)]
fn enrol_check_refuse_and_remove() {
    let mut hub = hub();
    let w = WorkerBox::new(&hub, "box", 22);
    let key = first_add(&hub, "dev@box", &[]);
    assert!(
        key.starts_with("ssh-ed25519 ") && key.contains(" cataly-hub-"),
        "{key}"
    );
    let secret = hub.root.join("config/cataly/secrets/worker-box");
    assert_eq!(
        std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // Asking again keeps the same key.
    assert_eq!(first_add(&hub, "dev@box", &[]), key);
    let fingerprint = w.init(&key, &[]);
    assert!(fingerprint.starts_with("SHA256:"), "{fingerprint}");
    let hub_id = after(&key, "cataly-hub-").to_string();
    let line = std::fs::read_to_string(w.authorized_keys()).unwrap();
    assert_eq!(
        line,
        format!("restrict,command=\"{BIN} worker serve --hub {hub_id}\" {key}\n")
    );
    // Running init twice changes nothing.
    w.init(&key, &[]);
    assert_eq!(std::fs::read_to_string(w.authorized_keys()).unwrap(), line);

    // A fingerprint that is not the scanned one pins nothing.
    let wrong = hub.cli(&["worker", "add", "dev@box", "--host-key", "SHA256:not-it"]);
    assert!(!wrong.status.success());
    assert!(stderr(&wrong).contains("nothing was pinned"), "{wrong:?}");
    assert!(!hub.root.join("config/cataly/known_hosts").exists());

    let added = hub.cli(&[
        "worker",
        "add",
        "dev@box",
        "--host-key",
        &fingerprint,
        "--lanes",
        "2",
        "--repo",
        "git@github.com:vntrevx/cataly-core.git",
    ]);
    assert!(added.status.success(), "{added:?}");
    let text = stdout(&added);
    assert!(text.starts_with("box enrolled · "), "{text}");
    assert!(
        text.contains(&format!(
            "cataly {} (compatible)",
            env!("CARGO_PKG_VERSION")
        )),
        "{text}"
    );
    assert!(
        text.contains("omp/19.1.1") && text.contains("up to 2 lanes"),
        "{text}"
    );
    let config = std::fs::read_to_string(hub.config_path()).unwrap();
    assert!(
        config.contains("[workers.box]\ndestination = \"dev@box\"\nport = 22\n"),
        "{config}"
    );
    assert!(
        config.contains("repos = [\"github.com/vntrevx/cataly-core\"]"),
        "{config}"
    );
    let pins = std::fs::read_to_string(hub.root.join("config/cataly/known_hosts")).unwrap();
    assert!(pins.starts_with("box ssh-ed25519 "), "{pins}");

    hub.start();
    let workers = list(&hub);
    assert_eq!(workers.len(), 1);
    assert_eq!(workers[0]["state"], "idle");
    assert_eq!(workers[0]["lanes_max"], 2);
    // Doctor connects; the git access check of the repository would need the network, so the
    // repository is dropped from the config first.
    let without_repo = config.replace("repos = [\"github.com/vntrevx/cataly-core\"]", "repos = []");
    hub.write_config(&without_repo);
    assert!(hub.cli(&["config", "reload"]).status.success());
    let doctor = hub.cli(&["worker", "doctor", "box"]);
    let report = stdout(&doctor);
    assert!(doctor.status.success(), "{doctor:?}");
    for check in [
        "connection",
        "protocols",
        "omp",
        "bridge link",
        "git",
        "disk",
    ] {
        assert!(
            report.contains(&format!("ok   {check}")),
            "{check}: {report}"
        );
    }
    let workers = list(&hub);
    assert_eq!(workers[0]["state"], "online");
    assert_eq!(workers[0]["version"], env!("CARGO_PKG_VERSION"));
    let status = stdout(&hub.cli(&["status"]));
    assert!(
        status.contains("worker   box") && status.contains("online"),
        "{status}"
    );
    // The worker's side: one serve, its bridge socket, its root.
    assert!(
        w.root.join("run/cataly").read_dir().unwrap().any(|e| {
            e.unwrap().file_name().to_string_lossy() == format!("worker-{hub_id}.sock")
        })
    );
    assert!(
        w.root
            .join(format!("state/cataly/worker/{hub_id}/serve.lock"))
            .exists()
    );

    // A changed host key is refused and never accepted on its own.
    hub.stop();
    w.new_host_key();
    hub.start();
    let doctor = hub.cli(&["worker", "doctor", "box"]);
    assert!(!doctor.status.success());
    assert!(stdout(&doctor).contains("host key changed"), "{doctor:?}");
    let workers = list(&hub);
    assert_eq!(workers[0]["state"], "refused");
    assert_eq!(
        std::fs::read_to_string(hub.root.join("config/cataly/known_hosts")).unwrap(),
        pins
    );
    // Pin the new key again, from a line without the forced command: the marker runs and fails.
    let new_fp = w.init(&key, &[]);
    let bare = line.replace(
        &format!("restrict,command=\"{BIN} worker serve --hub {hub_id}\" "),
        "",
    );
    std::fs::write(w.authorized_keys(), &bare).unwrap();
    let refused = hub.cli(&["worker", "add", "dev@box", "--host-key", &new_fp]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("no forced command"),
        "{refused:?}"
    );
    w.init(&key, &[]);
    let readded = hub.cli(&["worker", "add", "dev@box", "--host-key", &new_fp]);
    assert!(readded.status.success(), "{readded:?}");
    let doctor = hub.cli(&["worker", "doctor", "box"]);
    assert!(doctor.status.success(), "{doctor:?}");

    // Remove: the worker forgets the line, the hub its key, pin and section.
    let removed = hub.cli(&["worker", "remove", "box"]);
    assert!(removed.status.success(), "{removed:?}");
    assert_eq!(std::fs::read_to_string(w.authorized_keys()).unwrap(), "");
    assert!(!secret.exists());
    assert!(
        !std::fs::read_to_string(hub.root.join("config/cataly/known_hosts"))
            .unwrap()
            .contains("box ")
    );
    assert!(
        !std::fs::read_to_string(hub.config_path())
            .unwrap()
            .contains("[workers.box]")
    );
    assert_eq!(list(&hub).len(), 0);
}

/// `--tailscale` resolves the machine on the tailnet, limits the key to the hub's tailnet
/// addresses and pins the tailnet IP; an address off the tailnet is refused.
#[test]
fn tailscale_addresses_and_refusals() {
    let hub = hub();
    hub.fixture("tailscale", "self", Some("100.64.1.2\nfd7a:115c:a1e0::2\n"));
    hub.fixture("tailscale", "linux-box", Some("100.101.102.103\n"));
    hub.fixture("tailscale", "lan-box", Some("192.168.1.9\n"));
    let w = WorkerBox::new(&hub, "100.101.102.103", 2222);
    let out = hub.cli(&["worker", "add", "dev@linux-box:2222", "--tailscale"]);
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    assert!(
        text.contains("--from 100.64.1.2,fd7a:115c:a1e0::2"),
        "{text}"
    );
    let key = after(&text, "--hub-key '").split('\'').next().unwrap();
    let fp = w.init(key, &["--from", "100.64.1.2,fd7a:115c:a1e0::2"]);
    let line = std::fs::read_to_string(w.authorized_keys()).unwrap();
    assert!(
        line.starts_with("restrict,from=\"100.64.1.2,fd7a:115c:a1e0::2\",command="),
        "{line}"
    );
    let added = hub.cli(&[
        "worker",
        "add",
        "dev@linux-box:2222",
        "--tailscale",
        "--host-key",
        &fp,
    ]);
    assert!(added.status.success(), "{added:?}");
    let config = std::fs::read_to_string(hub.config_path()).unwrap();
    assert!(
        config.contains("[workers.linux-box]\ndestination = \"dev@100.101.102.103\"\nport = 2222\nname = \"linux-box\"\ntailscale = true"),
        "{config}"
    );
    let pins = std::fs::read_to_string(hub.root.join("config/cataly/known_hosts")).unwrap();
    assert!(
        pins.starts_with("[100.101.102.103]:2222 ssh-ed25519 "),
        "{pins}"
    );
    for (dest, why) in [
        ("dev@lan-box", "lan-box is not on the tailnet"),
        ("dev@unknown-box", "unknown-box is not on the tailnet"),
        ("dev@10.0.0.5", "10.0.0.5 is not on the tailnet"),
    ] {
        let out = hub.cli(&["worker", "add", dest, "--tailscale"]);
        assert!(!out.status.success(), "{dest}");
        assert!(stderr(&out).contains(why), "{dest}: {out:?}");
    }
}

/// An unreachable worker is offline, not refused; worker mode refuses to run outside sshd.
#[test]
fn offline_workers_and_serve_refusals() {
    let mut hub = hub();
    hub.write_config("[workers.gone]\ndestination = \"dev@gone\"\n");
    hub.start();
    hub.fixture("ssh", "down", Some(""));
    let doctor = hub.cli(&["worker", "doctor", "gone"]);
    assert!(!doctor.status.success());
    assert!(
        stdout(&doctor).contains("unreachable: ssh: connect to host gone"),
        "{doctor:?}"
    );
    let workers = list(&hub);
    assert_eq!(workers[0]["state"], "offline");
    assert_eq!(
        hub.cli(&["worker", "doctor", "nobody"]).status.code(),
        Some(1)
    );

    let serve = hub.cli(&["worker", "serve", "--hub", "3f2a9c"]);
    assert!(!serve.status.success());
    assert!(
        stderr(&serve).contains("runs only Cataly worker mode"),
        "{serve:?}"
    );
    let mut wrong = hub.command(&["worker", "serve", "--hub", "3f2a9c"]);
    wrong
        .env("SSH_ORIGINAL_COMMAND", "sh -i")
        .stdin(Stdio::null());
    let out = wrong.output().unwrap();
    assert!(!out.status.success() && stderr(&out).contains("runs only Cataly worker mode"));
}
