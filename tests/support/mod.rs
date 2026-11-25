//! Helpers that run the real `cataly` binary against a temporary home, state directory and socket.
//! Nothing here touches the user's real Cataly state.

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

pub mod discord;
pub mod model;
pub mod slack;
pub mod telegram;

pub const BIN: &str = env!("CARGO_BIN_EXE_cataly");

/// A hub sandbox: short root under /tmp (socket paths have a 103-byte limit on macOS).
pub struct TestHub {
    pub root: PathBuf,
    pub child: Option<Child>,
    /// Extra environment for the hub and CLI processes.
    pub env: Vec<(String, PathBuf)>,
}

impl TestHub {
    pub fn new(config: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/cy-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["home", "config/cataly", "state", "run"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let hub = Self {
            root,
            child: None,
            env: Vec::new(),
        };
        hub.write_config(config);
        hub
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join("config/cataly/config.toml")
    }

    pub fn socket(&self) -> PathBuf {
        self.root.join("run/cataly/hub.sock")
    }

    pub fn state(&self) -> PathBuf {
        self.root.join("state/cataly")
    }

    /// Writes the config file with mode 0600, replacing it.
    pub fn write_config(&self, text: &str) {
        let path = self.config_path();
        let _ = std::fs::remove_file(&path);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .env_clear()
            .env("PATH", path_without_omp())
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"));
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        cmd
    }

    /// Starts the hub process after linking the bridge into the sandbox's OMP agent directory
    /// (DESIGN 12.2 C19: sessions start only with the link in place).
    pub fn spawn_hub(&self) -> Child {
        let linked = self.command(&["bridge", "install"]).output().unwrap();
        assert!(
            linked.status.success(),
            "{}",
            String::from_utf8_lossy(&linked.stderr)
        );
        self.command(&["hub"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// Starts the hub and waits until `status` answers; returns the time that took.
    pub fn start(&mut self) -> Duration {
        let started = Instant::now();
        self.child = Some(self.spawn_hub());
        self.wait_ready();
        started.elapsed()
    }

    pub fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self
                .try_request(r#"{"v":1,"id":1,"method":"status"}"#)
                .is_some_and(|r| r.contains("\"ok\""))
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("hub did not answer within 10 s");
    }

    pub fn pid(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }

    pub fn cli(&self, args: &[&str]) -> Output {
        self.command(args).stdin(Stdio::null()).output().unwrap()
    }

    /// Runs the CLI and parses its `--json` stdout.
    pub fn json(&self, args: &[&str]) -> serde_json::Value {
        let out = self.cli(args);
        assert!(
            out.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    pub fn try_request(&self, line: &str) -> Option<String> {
        let mut stream = UnixStream::connect(self.socket()).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        stream.write_all(line.as_bytes()).ok()?;
        stream.write_all(b"\n").ok()?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).ok()?;
        Some(reply)
    }

    /// One raw request line, one parsed reply.
    pub fn request(&self, line: &str) -> serde_json::Value {
        serde_json::from_str(&self.try_request(line).expect("no reply")).unwrap()
    }

    pub fn signal(&self, sig: &str) {
        let status = Command::new("kill")
            .args([sig, &self.pid().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// Sends TERM and waits for the exit status.
    pub fn stop(&mut self) -> std::process::ExitStatus {
        self.signal("-TERM");
        self.wait_exit(Duration::from_secs(20))
    }

    /// Kills the hub with SIGKILL: no clean-up runs.
    pub fn crash(&mut self) {
        self.signal("-KILL");
        self.wait_exit(Duration::from_secs(5));
    }

    pub fn wait_exit(&mut self, limit: Duration) -> std::process::ExitStatus {
        let child = self.child.as_mut().unwrap();
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                self.child = None;
                return status;
            }
            assert!(Instant::now() < deadline, "hub did not exit in time");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Polls `f` every 20 ms until it returns `Some`, for at most `limit`.
    #[allow(clippy::unused_self)]
    pub fn until<T>(&self, limit: Duration, mut f: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(
                Instant::now() < deadline,
                "condition not met within {limit:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Every stored event, oldest first (`events --json` prints one JSON object per line).
    pub fn events(&self) -> Vec<serde_json::Value> {
        let out = self.cli(&["events", "--after", "0", "--limit", "1000", "--json"]);
        assert!(
            out.status.success(),
            "events failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    pub fn event_kinds(&self) -> Vec<String> {
        self.events()
            .iter()
            .map(|e| e["kind"].as_str().unwrap().to_string())
            .collect()
    }

    pub fn job(&self, name: &str) -> serde_json::Value {
        let list = self.json(&["job", "list", "--json"]);
        list["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|j| j["name"] == name)
            .cloned()
            .expect("job listed")
    }

    /// Copies `tests/support/fake-<name>` to `<root>/bin/<name>` with its empty data directory
    /// `<root>/bin/<name>-data`; returns the executable's path.
    pub fn install_fake(&self, name: &str) -> PathBuf {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/support")
            .join(format!("fake-{name}"));
        let bin = self.root.join("bin");
        std::fs::create_dir_all(bin.join(format!("{name}-data"))).unwrap();
        let target = bin.join(name);
        std::fs::copy(&source, &target).unwrap();
        std::fs::set_permissions(&target, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        target
    }

    /// Writes (or with `None` removes) a fixture file of an installed fake.
    pub fn fixture(&self, fake: &str, file: &str, text: Option<&str>) {
        let path = self
            .root
            .join("bin")
            .join(format!("{fake}-data"))
            .join(file);
        match text {
            Some(t) => std::fs::write(path, t).unwrap(),
            None => {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    /// Writes an owner-only file (a token file, for example).
    pub fn private_file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.root.join(name);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        f.write_all(text.as_bytes()).unwrap();
        path
    }

    /// One dry tick of a job: the reply's events.
    pub fn run_job(&self, name: &str) -> Vec<serde_json::Value> {
        self.json(&["job", "run", name, "--json"])["events"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// One dry tick of a job: the kinds of its events.
    pub fn tick(&self, name: &str) -> Vec<String> {
        self.run_job(name)
            .iter()
            .map(|e| e["kind"].as_str().unwrap().to_string())
            .collect()
    }
}

/// Stops a still-running hub with TERM first so it closes its OMP children (a KILL would orphan them
/// with the sandbox as their home), then KILL after 10 s; then removes the whole sandbox, including
/// any OMP agent directory under it. Runs on test failure too.
impl Drop for TestHub {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .status();
            let deadline = Instant::now() + Duration::from_secs(10);
            while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The test's PATH without any directory holding an `omp`: a hub whose config names no OMP must not
/// find and run the user's installed one (it would probe it, unpack its native module into the
/// sandbox home and fetch model lists from the network). Contract tests name stock OMP by its full
/// path in `[omp] path`.
pub fn path_without_omp() -> std::ffi::OsString {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths(std::env::split_paths(&path).filter(|dir| !dir.join("omp").exists()))
        .unwrap_or_default()
}

/// A probe job section.
pub fn probe(name: &str, argv: &str, extra: &str) -> String {
    format!(
        "[jobs.{name}]\nkind = \"probe\"\nevery = \"1h\"\nargv = {argv}\ncwd = \"/\"\n{extra}\n"
    )
}

pub fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}
