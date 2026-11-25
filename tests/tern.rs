//! The terminal runtime through the real hub (milestone 6): the Tern stand-in
//! (`tests/support/fake-tern`) runs the block's command line in a pseudo-terminal, the fake OMP in
//! its TUI mode records what it was given, and this test plays the OMP side of the bridge.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::TestHub;

/// The OMP side of one bridge connection.
struct Bridge {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next: u64,
}

impl Bridge {
    fn connect(socket: &Path) -> Self {
        let writer = UnixStream::connect(socket).unwrap();
        writer
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let reader = BufReader::new(writer.try_clone().unwrap());
        Self {
            reader,
            writer,
            next: 0,
        }
    }

    fn send(&mut self, method: &str, params: &Value) -> u64 {
        self.next += 1;
        let line = json!({ "v": 1, "id": self.next, "method": method, "params": params });
        writeln!(self.writer, "{line}").unwrap();
        self.next
    }

    fn line(&mut self) -> Value {
        let mut line = String::new();
        assert!(
            self.reader.read_line(&mut line).unwrap() > 0,
            "bridge closed"
        );
        serde_json::from_str(&line).unwrap()
    }

    fn request(&mut self, method: &str, params: &Value) -> Value {
        let id = self.send(method, params);
        loop {
            let msg = self.line();
            if msg["id"] == id {
                return msg["ok"].clone();
            }
        }
    }

    /// The next hub command; `state` readbacks are answered from `state` on the way.
    fn command(&mut self, state: &Value) -> Value {
        loop {
            let cmd = self.line()["item"]["cmd"].clone();
            if cmd["op"] == "state" {
                self.reply(&cmd, state.clone());
            } else if !cmd.is_null() {
                return cmd;
            }
        }
    }

    fn reply(&mut self, cmd: &Value, mut fields: Value) {
        fields["n"] = cmd["n"].clone();
        self.send("bridge.reply", &fields);
    }

    fn event(&mut self, session: &str, kind: &str, fields: &Value) {
        let mut params = json!({ "session": session, "kind": kind });
        params
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        self.send("bridge.event", &params);
    }
}

fn hello(token: &str, session: &str, file: &Path, pid: u64) -> Value {
    json!({ "ext": 1, "token": token, "session": session, "file": file, "pid": pid, "agent": "main",
        "idle": true, "model": "fake/primary" })
}

/// Appends the custom message OMP writes for a delivery to a session file.
fn write_entry(file: &Path, text: &str, id: &str) {
    let entry = json!({ "type": "custom_message", "customType": "cataly-delivery", "content": text,
        "display": true, "details": { "delivery": id } });
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(file)
        .unwrap();
    writeln!(f, "{entry}").unwrap();
}

fn tern_hub(tern: Option<&Path>) -> (TestHub, PathBuf) {
    let mut hub = TestHub::new("");
    let omp = hub.install_fake("omp");
    // The probe's health check needs the fake's rpc-ui peer; the block runs its TUI mode.
    hub.fixture("omp", "rpc", Some(""));
    hub.fixture("omp", "tui", Some(""));
    let tern = tern.map_or_else(|| hub.install_fake("tern"), Path::to_path_buf);
    let work = hub.root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    hub.write_config(&format!(
        "[omp]\npath = \"{}\"\n[sessions]\nruntime = \"tern\"\ntern = \"{}\"\n\
         [lead]\nmodel = \"fake/primary\"\ncwd = \"{}\"\n",
        omp.display(),
        tern.display(),
        work.display()
    ));
    hub.start();
    (hub, work)
}

/// The first block the fake OMP recorded: what the bridge in it would present.
fn block_of(hub: &TestHub) -> Value {
    let tui = hub.root.join("bin/omp-data/tui.jsonl");
    let started = Instant::now();
    loop {
        let text = std::fs::read_to_string(&tui).unwrap_or_default();
        if let Some(Ok(v)) = text.lines().next().map(serde_json::from_str) {
            return v;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "no block: {}",
            hub.json(&["lead", "status", "--json"])
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Check 3: a delivery found in another session's file is never resent; the owner is told once.
fn misdelivery(hub: &TestHub, bridge: &mut Bridge, (file1, file2): (&Path, &Path), idle: &Value) {
    let mut sending = hub
        .command(&["lead", "send", "hello three"])
        .spawn()
        .unwrap();
    let cmd = bridge.command(idle);
    let lost_id = cmd["id"].as_str().unwrap().to_string();
    write_entry(file2, cmd["text"].as_str().unwrap(), &lost_id);
    bridge.reply(&cmd, json!({ "sent": true }));
    bridge.event("s2", "session_switch", &json!({ "file": file2 }));
    bridge.event("s2", "agent_end", &json!({ "will_continue": false }));
    let count = || {
        hub.events()
            .iter()
            .filter(|e| e.to_string().contains("misdelivered"))
            .count()
    };
    hub.until(Duration::from_secs(30), || (count() > 0).then_some(()));
    bridge.event("s2", "agent_end", &json!({ "will_continue": false }));
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(count(), 1);
    assert!(sending.wait().unwrap().success());
    let text = std::fs::read_to_string(file1).unwrap();
    assert!(!text.contains(&lost_id), "resent into the target");
}

/// Acceptance checks 1, 2 and 3 and the tern wake latency.
#[test]
fn tern_lead_is_opened_in_a_block_and_controlled_through_the_bridge() {
    let (hub, work) = tern_hub(None);
    let block = block_of(&hub);
    // Check 1: Tern ran OMP's TUI in a terminal with the token, without rpc flags.
    assert_eq!(block["tty"], "yes", "{block}");
    let args = block["args"].as_str().unwrap();
    assert!(
        args.contains("--model=fake/primary") && !args.contains("rpc-ui"),
        "{args}"
    );
    let calls = std::fs::read_to_string(hub.root.join("bin/tern-data/calls.log")).unwrap();
    assert!(calls.starts_with("new tab --json --cwd"), "{calls}");
    let token = block["token"].as_str().unwrap().to_string();
    let pid = block["pid"].as_u64().unwrap();
    let (file1, file2) = (work.join("s1.jsonl"), work.join("s2.jsonl"));
    std::fs::write(&file1, "").unwrap();
    let mut bridge = Bridge::connect(&hub.socket());
    let welcome = bridge.request("bridge.hello", &hello(&token, "s1", &file1, pid));
    assert_eq!(
        (welcome["role"].as_str(), welcome["tern"].as_bool()),
        (Some("lead"), Some(true))
    );
    let key = welcome["key"].as_str().unwrap().to_string();
    // A nested OMP inherits the used token and is told none.
    let mut nested = Bridge::connect(&hub.socket());
    let refused = nested.request("bridge.hello", &hello(&token, "s9", &file1, pid + 1));
    assert_eq!(refused["role"], "none", "{refused}");
    let live = hub.until(Duration::from_secs(20), || {
        let v = hub.json(&["lead", "status", "--json"]);
        (v["phase"] == "live").then_some(v)
    });
    assert_eq!(live["session"]["session_id"], "s1", "{live}");

    // Check 2: a wake goes through the bridge and is confirmed from the session file.
    let idle = json!({ "idle": true, "session": "s1", "file": file1 });
    let asked = Instant::now();
    let send = |text: &str| hub.command(&["lead", "send", text]).spawn().unwrap();
    let mut sending = send("hello one");
    let cmd = bridge.command(&idle);
    let at_bridge = asked.elapsed();
    assert_eq!(
        (cmd["op"].as_str(), cmd["mode"].as_str()),
        (Some("deliver"), Some("wake"))
    );
    let id = cmd["id"].as_str().unwrap().to_string();
    write_entry(&file1, cmd["text"].as_str().unwrap(), &id);
    bridge.reply(&cmd, json!({ "sent": true }));
    bridge.event("s1", "agent_start", &json!({}));
    let wake = asked.elapsed();
    bridge.event("s1", "agent_end", &json!({ "will_continue": false }));
    hub.until(Duration::from_secs(20), || {
        let v = hub.json(&["lead", "status", "--json"]);
        (v["sent"] == 0 && v["waiting"] == 0).then_some(())
    });
    eprintln!(
        "MEASURE tern_wake_cli_to_bridge_ms={} tern_wake_cli_to_agent_start_ms={}",
        at_bridge.as_millis(),
        wake.as_millis()
    );

    // During a user's session switch the delivery is refused "not current" and kept pending.
    assert!(sending.wait().unwrap().success());
    let mut sending = send("hello two");
    let cmd = bridge.command(&idle);
    bridge.reply(&cmd, json!({ "refused": "not current" }));
    hub.until(Duration::from_secs(20), || {
        let inbox = hub.json(&["lead", "inbox", "--json"]).to_string();
        inbox.contains("hello two").then_some(())
    });
    bridge.event("s1", "agent_end", &json!({ "will_continue": false }));
    let again = bridge.command(&idle);
    assert!(
        again["text"].as_str().unwrap().contains("hello two"),
        "{again}"
    );
    write_entry(
        &file1,
        again["text"].as_str().unwrap(),
        again["id"].as_str().unwrap(),
    );
    bridge.reply(&again, json!({ "sent": true }));
    bridge.event("s1", "agent_end", &json!({ "will_continue": false }));
    hub.until(Duration::from_secs(20), || {
        let v = hub.json(&["lead", "status", "--json"]);
        (v["sent"] == 0 && v["waiting"] == 0).then_some(())
    });

    assert!(sending.wait().unwrap().success());
    misdelivery(&hub, &mut bridge, (&file1, &file2), &idle);

    // The same process reconnects with its key; another process with that key is refused.
    drop(bridge);
    let mut other = Bridge::connect(&hub.socket());
    let refused = other.request(
        "bridge.hello",
        &json!({ "ext": 1, "key": key, "session": "s2", "file": file2, "pid": pid + 7, "agent": "main" }),
    );
    assert_eq!(refused["role"], "none", "{refused}");
    let mut back = Bridge::connect(&hub.socket());
    let restored = back.request(
        "bridge.hello",
        &json!({ "ext": 1, "key": key, "session": "s2", "file": file2, "pid": pid, "agent": "main",
            "idle": true, "model": "fake/primary" }),
    );
    assert_eq!(restored["role"], "lead", "{restored}");
    let _ = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status();
}

/// Check 6: with `tern` configured and its executable missing, starts are refused with a hint.
#[test]
fn a_missing_tern_executable_refuses_starts_with_an_install_hint() {
    let (hub, _work) = tern_hub(Some(Path::new("/nonexistent/tern")));
    let text = hub.until(Duration::from_secs(20), || {
        let out = hub.cli(&["lead", "start"]);
        assert!(!out.status.success());
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (!text.contains("not been probed")).then_some(text)
    });
    assert!(text.contains("install Tern"), "{text}");
}
