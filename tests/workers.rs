//! Remote workers (docs/design/workers.md). Phase 1: enrolment in two steps, the pinned host key, the
//! forced command and its marker, `worker list`, `doctor` and `remove`, and worker mode itself.
//! Phase 2: lanes on a worker, opened from an owner's thread message, running in the worker's own
//! worktree with their `command` check there, surviving a dropped connection on the same session,
//! queued while their worker is offline, and bridge channels kept to their own worker's lanes. The
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

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use support::discord::FakeDiscord;
use support::{BIN, TestHub, path_without_omp};

const WAIT: Duration = Duration::from_secs(60);
const OWNER: &str = "400000000000000001";
const CH: &str = "600000000000000001";
const THREAD: &str = "600000000000000009";

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

// Phase 2: lanes on workers.

impl WorkerBox {
    /// The worker's fake OMP runs rpc-ui sessions with turns, as a logged-in OMP would.
    fn runs_lanes(&self) {
        for f in ["rpc", "turns"] {
            std::fs::write(self.root.join("bin/omp-data").join(f), "").unwrap();
        }
    }

    /// The bridge socket `serve` opens for the hub's sessions.
    fn socket(&self, hub_id: &str) -> PathBuf {
        self.root.join(format!("run/cataly/worker-{hub_id}.sock"))
    }

    /// The launch token the worker's fake OMP recorded for `session`, and how many starts did.
    fn tokens(&self, session: &Value) -> Vec<String> {
        let text =
            std::fs::read_to_string(self.root.join("bin/omp-data/launch")).unwrap_or_default();
        text.lines()
            .filter(|l| l.split(' ').nth(1) == session.as_str())
            .filter_map(|l| l.split(' ').next())
            .map(str::to_string)
            .collect()
    }

    /// The PID of the `serve` that holds this hub's connection now.
    fn serve_pid(&self, hub_id: &str) -> Option<i32> {
        let lock = self
            .root
            .join(format!("state/cataly/worker/{hub_id}/serve.lock"));
        std::fs::read_to_string(lock).ok()?.trim().parse().ok()
    }
}

/// Enrols `w` as `dest` in both steps; returns the hub ID of the key's line.
fn enrol(hub: &TestHub, w: &WorkerBox, dest: &str, extra: &[&str]) -> String {
    let key = first_add(hub, dest, extra);
    let fingerprint = w.init(&key, &[]);
    let mut args = vec!["worker", "add", dest, "--host-key", &fingerprint];
    args.extend_from_slice(extra);
    let added = hub.cli(&args);
    assert!(added.status.success(), "{added:?}");
    w.runs_lanes();
    after(&key, "cataly-hub-").to_string()
}

/// A hub that runs lanes (the fake OMP here too, for its lead), with a repository whose origin is
/// a bare repository on this machine, its commit pushed. Returns the checkout.
fn lane_hub(extra: &str) -> (TestHub, PathBuf) {
    let hub = hub();
    let omp = hub.install_fake("omp");
    for f in ["rpc", "turns"] {
        hub.fixture("omp", f, Some(""));
    }
    let work = hub.root.join("work");
    let repo = work.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let origin = work.join("origin.git");
    git(&work, &["init", "-q", "--bare", origin.to_str().unwrap()]);
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("a.txt"), "1").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-qm", "one"]);
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&repo, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    hub.write_config(&format!(
        "[omp]\npath = \"{}\"\n[lanes]\nmodel = \"fake/primary\"\ncwd = \"{}\"\nobjection = \"2s\"\n{extra}",
        omp.display(),
        work.display()
    ));
    (hub, repo)
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// Starts the hub and waits until the OMP probe finished (starts need it).
fn start(hub: &mut TestHub) {
    hub.start();
    hub.until(WAIT, || {
        hub.json(&["status", "--json"])["omp"]
            .is_object()
            .then_some(())
    });
}

/// One lane operation as the owner; `Err` is the refusal.
fn lane(hub: &TestHub, op: &str, params: &Value) -> Result<Value, String> {
    let out = hub.cli(&["lane", op, "--json", "--params", &params.to_string()]);
    if out.status.success() {
        Ok(serde_json::from_slice(&out.stdout).unwrap())
    } else {
        Err(stderr(&out))
    }
}

fn record(hub: &TestHub, key: &str) -> Value {
    lane(hub, "list", &json!({ "all": true }))
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["key"] == key)
        .cloned()
        .unwrap_or(Value::Null)
}

/// The session file a worker lane recorded (`worker:<name>:/path`), read on this machine.
fn remote_text(rec: &Value) -> String {
    let file = rec["session_file"].as_str().unwrap_or_default();
    let path = file.splitn(3, ':').nth(2).unwrap_or_default();
    std::fs::read_to_string(path).unwrap_or_default()
}

/// A test-side bridge connection on a socket.
struct Bridge {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next: u64,
}

impl Bridge {
    fn connect(socket: &Path, params: Value) -> (Self, Value) {
        let writer = UnixStream::connect(socket).unwrap();
        writer
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let reader = BufReader::new(writer.try_clone().unwrap());
        let mut b = Self {
            reader,
            writer,
            next: 1,
        };
        let answer = b.call("bridge.hello", params);
        (b, answer)
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let mut line = json!({ "v": 1, "id": id, "method": method });
        line["params"] = params;
        self.writer
            .write_all(format!("{line}\n").as_bytes())
            .unwrap();
        loop {
            let mut reply = String::new();
            self.reader.read_line(&mut reply).unwrap();
            let v: Value = serde_json::from_str(&reply).unwrap();
            if v["id"] == id {
                return v["ok"].clone();
            }
        }
    }

    fn tool(&mut self, call: &str, tool: &str, session: &Value, input: Value) -> Value {
        let mut params = json!({ "call": call, "tool": tool, "session": session });
        params["input"] = input;
        self.call("bridge.tool", params)
    }
}

/// The lead's bridge with its launch token, and its session.
fn lead_bridge(hub: &TestHub) -> (Bridge, Value) {
    let session = hub.until(WAIT, || {
        let v = hub.json(&["lead", "status", "--json"]);
        (v["phase"] == "live").then(|| v["session"]["session_id"].clone())
    });
    let launch = std::fs::read_to_string(hub.root.join("bin/omp-data/launch")).unwrap();
    let token = launch
        .lines()
        .rfind(|l| l.split(' ').nth(1) == session.as_str())
        .and_then(|l| l.split(' ').next())
        .unwrap()
        .to_string();
    let hello = json!({ "ext": 1, "token": token, "session": session, "agent": "main" });
    let (bridge, answer) = Bridge::connect(&hub.socket(), hello);
    assert_eq!(answer["role"], "lead", "{answer}");
    (bridge, session)
}

fn lead_tool(b: &mut Bridge, session: &Value, call: &str, input: Value) -> (String, bool) {
    let v = b.tool(call, "cataly_lead", session, input);
    (
        v["text"].as_str().unwrap_or_default().to_string(),
        v["error"] == true,
    )
}

/// A worker lane opened by the lead from the owner's message: its thread with the kickoff saying
/// where it runs, its session in the worker's own worktree at the pushed commit, a report through
/// the worker's bridge socket reaching the thread, an owner reply in the thread reaching the lane,
/// its `command` check run on the worker, and the close removing the worker's worktree.
#[test]
#[allow(clippy::too_many_lines)]
fn a_worker_lane_runs_from_a_thread_and_checks_there() {
    let fake = FakeDiscord::start();
    let (mut hub, repo) = lane_hub("");
    let token = hub.private_file("token", "fake-token");
    fake.channel(CH, 0, None);
    let config = std::fs::read_to_string(hub.config_path()).unwrap();
    hub.write_config(&format!(
        "{config}[lead]\nmodel = \"fake/primary\"\ncwd = \"{}\"\n{}",
        repo.display(),
        fake.config(&token, OWNER, &[CH])
    ));
    let w = WorkerBox::new(&hub, "box", 22);
    let hub_id = enrol(&hub, &w, "dev@box", &[]);
    // An uncommitted edit here stays behind; the open says so.
    std::fs::write(repo.join("a.txt"), "edited here").unwrap();
    start(&mut hub);
    hub.until(WAIT, || {
        (hub.json(&["discord", "status", "--json"])["healthy"] == true).then_some(())
    });
    let (mut b, s) = lead_bridge(&hub);
    let (listed, _) = lead_tool(&mut b, &s, "w0", json!({ "op": "lane.workers" }));
    assert!(listed.starts_with("- box: "), "{listed}");
    let asked = 800_000_000_000_000_101u64;
    fake.say(FakeDiscord::message(
        CH,
        asked,
        OWNER,
        "run the tests on box",
    ));
    let open = json!({
        "op": "lane.open", "key": "box-test", "title": "Test on box",
        "brief": "Run the tests.",
        "checks": [{ "kind": "command", "argv": ["sh", "-c", "pwd -P | grep -q /cataly/worker/"] }],
        "thread_from": format!("discord:{CH}/{asked}"),
        "kickoff": "Testing on box; done when the check passes.",
        "cwd": repo, "worker": "box",
    });
    let (text, err) = lead_tool(&mut b, &s, "l1", open);
    assert!(!err && text.contains("started"), "{text}");
    assert!(
        text.contains("uncommitted edits are not on the worker"),
        "{text}"
    );
    let rec = record(&hub, "box-test");
    assert_eq!(rec["worker"], "box", "{rec}");
    let root = w.root.join(format!("state/cataly/worker/{hub_id}"));
    let cwd = rec["cwd"].as_str().unwrap();
    assert!(
        Path::new(cwd).starts_with(root.join("lanes")),
        "the lane works in the worker's worktree: {rec}"
    );
    assert!(
        rec["session_file"]
            .as_str()
            .unwrap()
            .starts_with("worker:box:"),
        "{rec}"
    );
    let header: Value = serde_json::from_str(remote_text(&rec).lines().next().unwrap()).unwrap();
    assert_eq!(header["cwd"], cwd, "OMP runs in the worktree");
    assert_eq!(
        std::fs::read_to_string(Path::new(cwd).join("a.txt")).unwrap(),
        "1",
        "the worktree holds the pushed commit, not the edit"
    );
    // The thread under the owner's message, its kickoff naming the machine and the commit.
    let thread = asked.to_string();
    let kickoff = fake.wait(WAIT, |f| f.bot_messages(&thread).first().cloned());
    let first = kickoff["content"].as_str().unwrap();
    assert!(first.starts_with("Testing on box"), "{first}");
    assert!(first.contains("box (Linux"), "{first}");

    // A report through the worker's bridge socket reaches the thread.
    let tokens = w.tokens(&rec["session_id"]);
    let hello =
        json!({ "ext": 1, "token": tokens[0], "session": rec["session_id"], "agent": "main" });
    let (mut lb, welcome) = Bridge::connect(&w.socket(&hub_id), hello);
    assert_eq!(welcome["role"], "lane", "{welcome}");
    lb.tool(
        "r1",
        "cataly_lane",
        &rec["session_id"],
        json!({ "op": "report", "kind": "stuck", "text": "the box needs a fixture" }),
    );
    fake.wait(WAIT, |f| {
        f.bot_messages(&thread)
            .iter()
            .any(|m| m["content"] == "Lane update: the box needs a fixture")
            .then_some(())
    });
    // An owner message in the thread reaches the lane on the worker.
    fake.say(FakeDiscord::message(
        &thread,
        800_000_000_000_000_102,
        OWNER,
        "also check logout",
    ));
    hub.until(WAIT, || {
        remote_text(&rec)
            .contains("also check logout")
            .then_some(())
    });
    // The close runs the command check on the worker and removes its worktree there.
    let mut n = 0;
    let closed = hub.until(WAIT, || {
        n += 1;
        let close = json!({ "op": "lane.close", "key": "box-test", "note": "tested" });
        let (text, err) = lead_tool(&mut b, &s, &format!("c{n}"), close);
        (!err).then_some(text)
    });
    assert!(closed.contains("closed"), "{closed}");
    let rec = record(&hub, "box-test");
    let receipt = rec["receipt"].to_string();
    assert!(
        receipt.contains("pass command sh (box): exit 0"),
        "{receipt}"
    );
    assert!(!Path::new(cwd).exists(), "{receipt}");
    drop((b, lb));
    hub.stop();
}

/// The worker's `serve` dies mid-lane (as when the machine or its network drops): the lane is lost,
/// its thread hears it in the owner's language, `worker.offline` and `worker.online` come once each,
/// and the lane comes back by itself on the same session file and takes messages.
#[test]
#[allow(clippy::too_many_lines)]
fn a_dropped_worker_lane_resumes_on_its_session() {
    let fake = FakeDiscord::start();
    let (mut hub, repo) = lane_hub("");
    let token = hub.private_file("token", "fake-token");
    fake.channel(CH, 0, None);
    fake.thread(THREAD, CH);
    let config = std::fs::read_to_string(hub.config_path()).unwrap();
    hub.write_config(&format!("{config}{}", fake.config(&token, OWNER, &[CH])));
    let w = WorkerBox::new(&hub, "box", 22);
    let hub_id = enrol(&hub, &w, "dev@box", &[]);
    start(&mut hub);
    let open = json!({
        "key": "drop", "title": "Survive a drop", "brief": "Work.",
        "checks": [{ "kind": "command", "argv": ["true"] }],
        "cwd": repo, "worker": "box", "thread": format!("discord:{THREAD}"),
    });
    let opened = lane(&hub, "open", &open).unwrap();
    assert_eq!(opened["outcome"], "started", "{opened}");
    let rec = record(&hub, "drop");
    let pid = w.serve_pid(&hub_id).unwrap();
    rustix_kill(pid);
    // Back on the same session: a second start of that session on the worker, the lane live.
    hub.until(WAIT, || {
        (w.tokens(&rec["session_id"]).len() >= 2).then_some(())
    });
    let back = hub.until(WAIT, || {
        let r = record(&hub, "drop");
        (r["live"] == true && r["lost"].is_null()).then_some(r)
    });
    assert_eq!(back["session_id"], rec["session_id"]);
    assert_eq!(back["session_file"], rec["session_file"]);
    assert_ne!(w.serve_pid(&hub_id), Some(pid), "a new serve took over");
    let lost = hub
        .events()
        .into_iter()
        .any(|e| e["kind"] == "lane.lost" && e.to_string().contains("box"));
    assert!(lost, "{:?}", hub.event_kinds());
    let count = |kind: &str| hub.event_kinds().iter().filter(|k| *k == kind).count();
    assert_eq!(count("worker.offline"), 1);
    assert_eq!(count("worker.online"), 1);
    let notes = fake.wait(WAIT, |f| {
        let texts: Vec<String> = f
            .bot_messages(THREAD)
            .iter()
            .filter_map(|m| m["content"].as_str().map(str::to_string))
            .collect();
        texts
            .iter()
            .any(|t| t == "다시 연결돼서 이어서 해요")
            .then_some(texts)
    });
    assert!(
        notes.contains(&"작업 PC box 연결이 끊겼어요. 다시 연결되면 이어서 해요".to_string()),
        "{notes:?}"
    );
    lane(
        &hub,
        "send",
        &json!({ "key": "drop", "text": "after the drop" }),
    )
    .unwrap();
    hub.until(WAIT, || {
        remote_text(&back).contains("after the drop").then_some(())
    });
    hub.stop();
}

/// A message the worker's OMP took but whose entry was not in the session file when the worker
/// dropped ends in doubt once the lane is back on that file: never written again, the lead and the
/// owner told, and the lane's messages fenced until the lead releases it (DESIGN 9.3).
#[test]
fn a_delivery_in_flight_at_a_drop_ends_in_doubt() {
    let (mut hub, repo) = lane_hub("");
    let w = WorkerBox::new(&hub, "box", 22);
    let hub_id = enrol(&hub, &w, "dev@box", &[]);
    start(&mut hub);
    let open = json!({
        "key": "flight", "title": "In flight", "brief": "Work.",
        "checks": [{ "kind": "command", "argv": ["true"] }],
        "cwd": repo, "worker": "box",
    });
    assert_eq!(lane(&hub, "open", &open).unwrap()["outcome"], "started");
    let rec = record(&hub, "flight");
    hub.until(WAIT, || {
        remote_text(&rec)
            .contains("Lane instructions")
            .then_some(())
    });
    // The worker's OMP takes the message, and the drop comes before its entry is written.
    let data = w.root.join("bin/omp-data");
    std::fs::write(data.join("noentry"), "").unwrap();
    lane(
        &hub,
        "send",
        &json!({ "key": "flight", "text": "in flight" }),
    )
    .unwrap();
    let written = || {
        std::fs::read_to_string(data.join("prompts"))
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains("in flight"))
            .count()
    };
    hub.until(WAIT, || (written() == 1).then_some(()));
    rustix_kill(w.serve_pid(&hub_id).unwrap());
    std::fs::remove_file(data.join("noentry")).unwrap();
    // Back on the same session, the file without the entry: in doubt, told once, not resent.
    let doubt = hub.until(WAIT, || {
        hub.events()
            .into_iter()
            .find(|e| e["kind"] == "delivery.in_doubt" && e.to_string().contains("flight"))
    });
    assert!(doubt.to_string().contains("not resent"), "{doubt}");
    let back = record(&hub, "flight");
    assert_eq!(back["session_file"], rec["session_file"]);
    assert!(back["live"] == true && back["lost"].is_null(), "{back}");
    assert!(back["doubt"].is_string(), "{back}");
    let told = || {
        hub.events()
            .into_iter()
            .filter(|e| e["kind"] == "lane.alert" && e.to_string().contains("delivery in doubt"))
            .count()
    };
    hub.until(WAIT, || (told() > 0).then_some(()));
    let fenced = lane(&hub, "send", &json!({ "key": "flight", "text": "next" })).unwrap_err();
    assert!(fenced.contains("in doubt"), "{fenced}");
    // The lead releases the fence; the next message goes, and the one in doubt never again.
    lane(
        &hub,
        "release",
        &json!({ "key": "flight", "note": "not in the session; told the lane myself" }),
    )
    .unwrap();
    lane(
        &hub,
        "send",
        &json!({ "key": "flight", "text": "after release" }),
    )
    .unwrap();
    hub.until(WAIT, || {
        remote_text(&back).contains("after release").then_some(())
    });
    assert_eq!(written(), 1, "the message in doubt was written again");
    assert!(!remote_text(&back).contains("in flight"));
    assert_eq!(told(), 1, "the lead hears it once: {:?}", hub.event_kinds());
    hub.stop();
}

/// SIGKILL, as a crash or a reboot ends the process.
fn rustix_kill(pid: i32) {
    let killed = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
}

/// A lane for a worker that is offline is queued, and starts by itself once the worker answers.
#[test]
fn a_lane_for_an_offline_worker_waits_and_starts() {
    let (mut hub, repo) = lane_hub("");
    let w = WorkerBox::new(&hub, "box", 22);
    enrol(&hub, &w, "dev@box", &[]);
    start(&mut hub);
    hub.fixture("ssh", "down", Some(""));
    let open = json!({
        "key": "later", "title": "Wait for box", "brief": "Work.",
        "checks": [{ "kind": "command", "argv": ["true"] }],
        "cwd": repo, "worker": "box",
    });
    let queued = lane(&hub, "open", &open).unwrap();
    let outcome = queued["outcome"].as_str().unwrap();
    assert!(outcome.starts_with("queued: box is offline"), "{queued}");
    assert!(record(&hub, "later").is_null());
    let listed = lane(&hub, "queued", &json!({})).unwrap();
    assert_eq!(listed["queued"][0]["key"], "later", "{listed}");
    // The worker comes back; the owner's doctor reaches it, and the lane starts there.
    std::fs::remove_file(hub.root.join("bin/ssh-data/down")).unwrap();
    let doctor = hub.cli(&["worker", "doctor", "box"]);
    assert!(doctor.status.success(), "{doctor:?}");
    let rec = hub.until(WAIT, || {
        let r = record(&hub, "later");
        (r["live"] == true && r["state"] == "running").then_some(r)
    });
    assert_eq!(rec["worker"], "box");
    assert!(remote_text(&rec).contains("Lane instructions"), "{rec}");
    hub.stop();
}

/// Bridge channels stay with their worker: a lane's reconnect key works through its own worker's
/// socket only, never another worker's nor this machine's, and no worker can say hello as the lead.
#[test]
fn a_worker_bridge_reaches_only_its_own_lanes() {
    let (mut hub, repo) = lane_hub("");
    let config = std::fs::read_to_string(hub.config_path()).unwrap();
    hub.write_config(&format!(
        "{config}[lead]\nmodel = \"fake/primary\"\ncwd = \"{}\"\n",
        repo.display()
    ));
    let w = WorkerBox::new(&hub, "box", 22);
    let other = WorkerBox::new(&hub, "other", 22);
    let hub_id = enrol(&hub, &w, "dev@box", &[]);
    enrol(&hub, &other, "dev@other", &[]);
    start(&mut hub);
    let open = json!({
        "key": "mine", "title": "Mine", "brief": "Work.",
        "checks": [{ "kind": "command", "argv": ["true"] }],
        "cwd": repo, "worker": "box",
    });
    assert_eq!(lane(&hub, "open", &open).unwrap()["outcome"], "started");
    let rec = record(&hub, "mine");
    let token = w.tokens(&rec["session_id"])[0].clone();
    let hello = json!({ "ext": 1, "token": token, "session": rec["session_id"], "agent": "main" });
    let (_own, welcome) = Bridge::connect(&w.socket(&hub_id), hello);
    assert_eq!(welcome["role"], "lane", "{welcome}");
    let key = welcome["key"].clone();
    // The other worker is connected too, its socket open.
    assert!(hub.cli(&["worker", "doctor", "other"]).status.success());
    let again = json!({ "ext": 1, "key": key, "session": rec["session_id"], "agent": "main" });
    let (_x, through_other) = Bridge::connect(&other.socket(&hub_id), again.clone());
    assert_eq!(through_other["role"], "none", "{through_other}");
    assert!(
        through_other["reason"]
            .as_str()
            .unwrap()
            .contains("not a lane that runs on other"),
        "{through_other}"
    );
    let (_y, through_hub) = Bridge::connect(&hub.socket(), again);
    assert_eq!(through_hub["role"], "none", "{through_hub}");
    // The lead's session identity gets nothing through a worker.
    let lead = hub.until(WAIT, || {
        let v = hub.json(&["lead", "status", "--json"]);
        (v["phase"] == "live").then(|| v["session"]["session_id"].clone())
    });
    let marker = json!({ "ext": 1, "marker": true, "session": lead, "agent": "main" });
    let (_z, as_lead) = Bridge::connect(&w.socket(&hub_id), marker);
    assert_eq!(as_lead["role"], "none", "{as_lead}");
    // A worker channel speaks only the bridge.
    let mut raw = UnixStream::connect(w.socket(&hub_id)).unwrap();
    raw.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    raw.write_all(b"{\"v\":1,\"id\":1,\"method\":\"lane.list\",\"params\":{}}\n")
        .unwrap();
    let mut reply = String::new();
    BufReader::new(raw).read_line(&mut reply).unwrap();
    assert!(reply.contains("carries only the bridge"), "{reply}");
    hub.stop();
}

/// A worker that failed is asked again only after a minute, by itself: still down at the first
/// retry, up at the next, and the queued lane starts then. Its state changes are told once each.
#[test]
fn an_offline_worker_is_tried_again_each_minute() {
    let (mut hub, repo) = lane_hub("");
    let w = WorkerBox::new(&hub, "box", 22);
    enrol(&hub, &w, "dev@box", &[]);
    start(&mut hub);
    hub.fixture("ssh", "down", Some(""));
    let open = json!({
        "key": "retry", "title": "Retry box", "brief": "Work.",
        "checks": [{ "kind": "command", "argv": ["true"] }],
        "cwd": repo, "worker": "box",
    });
    let queued = lane(&hub, "open", &open).unwrap();
    assert!(
        queued["outcome"].as_str().unwrap().starts_with("queued: "),
        "{queued}"
    );
    let log = hub.root.join("bin/ssh-data/log");
    let tries = || {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter(|l| *l == "dev@box:22")
            .count()
    };
    let first = tries();
    let failed_at = std::time::Instant::now();
    hub.until(Duration::from_secs(120), || (tries() > first).then_some(()));
    let waited = failed_at.elapsed();
    assert!(
        waited >= Duration::from_secs(55),
        "the retry waited a minute: {waited:?}"
    );
    // Still down at the first retry; up for the next.
    std::fs::remove_file(hub.root.join("bin/ssh-data/down")).unwrap();
    let rec = hub.until(Duration::from_secs(120), || {
        let r = record(&hub, "retry");
        (r["live"] == true).then_some(r)
    });
    assert_eq!(rec["worker"], "box");
    let kinds = hub.event_kinds();
    let count = |kind: &str| kinds.iter().filter(|k| *k == kind).count();
    assert_eq!(count("worker.offline"), 1, "{kinds:?}");
    assert_eq!(count("worker.online"), 1, "{kinds:?}");
    hub.stop();
}
