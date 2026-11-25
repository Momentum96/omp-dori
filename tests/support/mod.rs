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
pub mod github;
pub mod model;
pub mod slack;
pub mod telegram;

pub const BIN: &str = env!("CARGO_BIN_EXE_cataly");

/// The test admission floors `TestHub::write_config` adds.
pub const ADMISSION: &str = "\n[jobs.test-admission]\nkind = \"pressure\"\nenabled = false\n\
    min_free_memory = 0\nmin_free_disk_gb = 0\nmax_sessions = 0\n";

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

    /// Writes the config file with mode 0600, replacing it. Unless the text names its own pressure
    /// job, a disabled one is added whose floors (memory, disk, sessions) are 0: session-start
    /// admission reads its floors from the pressure job, and without one the product defaults
    /// (50 GB free disk among them) would make tests depend on the host. The default refusal is
    /// `pressure::tests::the_default_rule_refuses_a_small_disk`.
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
        if !text.contains("kind = \"pressure\"") {
            f.write_all(ADMISSION.as_bytes()).unwrap();
        }
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut cmd = self.sandboxed(BIN);
        cmd.args(args);
        cmd
    }

    /// `program` with the sandbox's environment.
    fn sandboxed(&self, program: &str) -> Command {
        let mut cmd = Command::new(program);
        cmd.env_clear()
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

    /// Links this binary's bridge into the sandbox's OMP agent directory, as an install does: the
    /// file `cataly extension` prints, under `<root>/bridge`, linked from `extensions/cataly.ts`.
    /// Returns the link.
    pub fn link_bridge(&self) -> PathBuf {
        let printed = self.command(&["extension"]).output().unwrap();
        assert!(printed.status.success(), "{printed:?}");
        let file = self.root.join("bridge/cataly.ts");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, &printed.stdout).unwrap();
        let agent = self
            .env
            .iter()
            .find(|(k, _)| k == "PI_CODING_AGENT_DIR")
            .map_or_else(|| self.root.join("home/.omp/agent"), |(_, v)| v.clone());
        let link = agent.join("extensions/cataly.ts");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        // An installed link (`cataly install` made it) is the real thing; it stays.
        if std::fs::symlink_metadata(&link).is_err() {
            std::os::unix::fs::symlink(&file, &link).unwrap();
        }
        link
    }

    /// Starts the hub process after linking the bridge into the sandbox's OMP agent directory
    /// (DESIGN 12.2 C19: sessions start only with the link in place). A killed test runs no `Drop`,
    /// so the hub is started by a shell that leaves a poller behind and execs it (same PID): the
    /// poller sends the hub TERM once this test process is gone, and ends with the hub.
    pub fn spawn_hub(&self) -> Child {
        self.link_bridge();
        let watch = "t=$PPID h=$$; (while kill -0 $t && kill -0 $h; do sleep 1; done; kill -TERM $h) \
                     >/dev/null 2>&1 & exec \"$0\" \"$@\"";
        self.sandboxed("/bin/sh")
            .args(["-c", watch, BIN, "hub"])
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

    /// Copies `tests/support/fake-<name>` to `<root>/bin/<name>` with its data directory
    /// `<root>/bin/<name>-data`, which holds only `owner`: this test process's PID, which work a fake
    /// leaves running (the Tern stand-in's blocks) must not outlive. Returns the executable's path.
    pub fn install_fake(&self, name: &str) -> PathBuf {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/support")
            .join(format!("fake-{name}"));
        let bin = self.root.join("bin");
        let data = bin.join(format!("{name}-data"));
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("owner"), std::process::id().to_string()).unwrap();
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

    /// The CLI with `args`, started by a shell that sets `umask` first.
    pub fn umask_command(&self, umask: &str, args: &[&str]) -> Command {
        let mut cmd = self.sandboxed("/bin/sh");
        cmd.arg("-c")
            .arg(format!("umask {umask}; exec \"$0\" \"$@\""))
            .arg(BIN)
            .args(args);
        cmd
    }

    /// Runs the CLI with `args` and `umask 0002` on a pseudo-terminal, as the owner does. Each step
    /// waits for its text in the output since the previous step, then runs its action, whose
    /// answer (if any) is typed. Returns the exit status and everything the terminal showed.
    pub fn pty(
        &self,
        args: &[&str],
        steps: Vec<Step<'_>>,
        limit: Duration,
    ) -> (Option<i32>, String) {
        use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
        use std::io::Read as _;
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let name = ptsname(&master, Vec::new()).unwrap();
        let slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name.to_str().unwrap())
            .unwrap();
        let mut cmd = self.umask_command("0002", args);
        cmd.stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave);
        let mut child = cmd.spawn().unwrap();
        // The terminal reads end once the child and every copy of its side are gone.
        drop(cmd);
        let mut writer = std::fs::File::from(master);
        let mut reader = writer.try_clone().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        let mut steps = steps.into_iter();
        let mut next = steps.next();
        let mut shown = String::new();
        let mut from = 0;
        let deadline = Instant::now() + limit;
        loop {
            let done = child.try_wait().unwrap();
            while let Ok(chunk) = rx.recv_timeout(Duration::from_millis(30)) {
                shown.push_str(&String::from_utf8_lossy(&chunk));
            }
            while let Some((expect, act)) = next.as_mut() {
                let Some(at) = shown[from..].find(*expect) else {
                    break;
                };
                from += at + expect.len();
                if let Some(answer) = act() {
                    writer.write_all(answer.as_bytes()).unwrap();
                }
                next = steps.next();
            }
            if let Some(status) = done {
                return (status.code(), shown);
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("no exit within {limit:?}:\n{shown}");
            }
        }
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

    /// The process groups the Tern stand-in recorded for its blocks: each wrapper's and its
    /// program's.
    fn block_groups(&self) -> Vec<u32> {
        let Ok(entries) = std::fs::read_dir(self.root.join("bin/tern-data")) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                let block = p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("block-"));
                block && p.extension().is_some_and(|x| x == "pid" || x == "omp")
            })
            .filter_map(|p| std::fs::read_to_string(p).ok()?.trim().parse().ok())
            .collect()
    }

    /// Every process whose command line names this sandbox, as `PID command line`.
    fn sandbox_processes(&self) -> Vec<(u32, String)> {
        let mark = format!("{}/", self.root.display());
        let Ok(out) = Command::new("ps").args(["-A", "-o", "pid=,args="]).output() else {
            eprintln!("ps did not run: processes of {mark} not checked");
            return Vec::new();
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.contains(&mark))
            .filter_map(|l| {
                let (pid, args) = l.trim_start().split_once(' ')?;
                Some((pid.parse().ok()?, args.trim().to_string()))
            })
            .collect()
    }

    /// Ends the Tern stand-in's blocks (TERM to each recorded process group, KILL after 5 s); then
    /// whatever still runs with this sandbox on its command line 2 s later is killed and returned.
    fn end_blocks(&self) -> Vec<String> {
        let groups = self.block_groups();
        let signal = |sig: &str| {
            for g in &groups {
                let _ = Command::new("kill")
                    .args([sig, "--", &format!("-{g}"), &g.to_string()])
                    .stderr(Stdio::null())
                    .status();
            }
        };
        let alive = |g: &u32| {
            Command::new("kill")
                .args(["-0", "--", &format!("-{g}")])
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        signal("-TERM");
        let deadline = Instant::now() + Duration::from_secs(5);
        while groups.iter().any(alive) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        signal("-KILL");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let left = self.sandbox_processes();
            if left.is_empty() || Instant::now() >= deadline {
                for (pid, _) in &left {
                    let _ = Command::new("kill")
                        .args(["-KILL", &pid.to_string()])
                        .status();
                }
                return left.into_iter().map(|(p, a)| format!("{p} {a}")).collect();
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Stops a still-running hub with TERM first so it closes its OMP children (a KILL would orphan them
/// with the sandbox as their home), then KILL after 10 s; ends the Tern stand-in's blocks, which
/// outlive a hub as Tern's do; then removes the whole sandbox, including any OMP agent directory
/// under it. Runs on test failure too, and then first prints the end of the hub log, so a failure on
/// CI shows what the hub did. A process still running with the sandbox on its command line fails the
/// test (it is killed first).
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
        let left = self.end_blocks();
        if std::thread::panicking() {
            let log =
                std::fs::read_to_string(self.state().join("logs/hub.log")).unwrap_or_default();
            let lines: Vec<&str> = log.lines().collect();
            let tail = &lines[lines.len().saturating_sub(200)..];
            eprintln!(
                "---- hub.log (last {} lines)\n{}",
                tail.len(),
                tail.join("\n")
            );
        }
        let _ = std::fs::remove_dir_all(&self.root);
        if !left.is_empty() {
            let text = format!("left running after the hub was dropped (killed): {left:?}");
            if std::thread::panicking() {
                eprintln!("{text}");
            } else {
                panic!("{text}");
            }
        }
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

/// One pseudo-terminal step: the text to wait for, and what to do then (the answer to type).
pub type Step<'a> = (&'a str, Box<dyn FnMut() -> Option<String> + 'a>);
/// A probe job section.
pub fn probe(name: &str, argv: &str, extra: &str) -> String {
    format!(
        "[jobs.{name}]\nkind = \"probe\"\nevery = \"1h\"\nargv = {argv}\ncwd = \"/\"\n{extra}\n"
    )
}

pub fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}
