//! The terminal runtime against stock OMP (milestone 6): the Tern stand-in runs the block's command
//! line, stock OMP's TUI, in a pseudo-terminal with the loopback model; the bridge in that OMP says
//! hello with the single-use token and the hub controls the session through it. Runs only with
//! `CATALY_CONTRACT_OMP=1` (`CATALY_CONTRACT_OMP_PATH` selects the executable).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::TestHub;
use support::discord::FakeDiscord;
use support::model::{FakeModel, models_yml};

const WAIT: Duration = Duration::from_secs(120);
const OWNER: &str = "400000000000000001";
const CH: &str = "600000000000000001";
const FLAGS: &str = "extra_flags = [\"--no-skills\", \"--no-rules\", \"--no-context-files\"]\n";

fn enabled() -> bool {
    let on = std::env::var("CATALY_CONTRACT_OMP").as_deref() == Ok("1");
    if !on {
        eprintln!("skipped: set CATALY_CONTRACT_OMP=1 to run the OMP contract suite");
    }
    on
}

fn installed_omp() -> PathBuf {
    if let Some(path) = std::env::var_os("CATALY_CONTRACT_OMP_PATH") {
        return PathBuf::from(path);
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|d| d.join("omp"))
        .find(|p| p.is_file())
        .expect("omp on PATH")
}

/// A hub whose sessions run stock OMP on the fake model in an isolated agent directory, on
/// `runtime`, with the Tern stand-in; `omp_settings` is OMP's own `config.yml`, `extra` more config
/// given the path of an owner-only token file.
fn sandbox(
    model: &FakeModel,
    runtime: &str,
    omp_settings: &str,
    extra: &dyn Fn(&Path) -> String,
) -> TestHub {
    let mut hub = TestHub::new("");
    let (agent, work) = (hub.root.join("agent"), hub.root.join("work"));
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(
        agent.join("models.yml"),
        models_yml(model.port, &["primary"]),
    )
    .unwrap();
    // An owner's OMP has finished its first-run setup; the sandbox's would show the wizard over the
    // TUI's own dialogs.
    let settings = format!("startup:\n  setupWizard: false\n{omp_settings}");
    std::fs::write(agent.join("config.yml"), settings).unwrap();
    let tern = hub.install_fake("tern");
    hub.env.push(("PI_CODING_AGENT_DIR".into(), agent));
    let extra = extra(&hub.private_file("token", "fake-token"));
    hub.write_config(&format!(
        "[omp]\npath = \"{}\"\n[sessions]\nruntime = \"{runtime}\"\ntern = \"{}\"\n{FLAGS}\
         [lead]\nmodel = \"fake/primary\"\ncwd = \"{}\"\n\
         [lanes]\nmodel = \"fake/primary\"\ncwd = \"{}\"\n{extra}",
        installed_omp().display(),
        tern.display(),
        work.display(),
        work.display()
    ));
    hub
}

/// The lead live and idle with nothing pending (and, in tern, its bridge connected).
fn settled(hub: &TestHub) -> Value {
    hub.until(WAIT, || {
        let v = hub.json(&["lead", "status", "--json"]);
        let linked = v["session"]["runtime"] == "headless" || v["bridge"]["connected"] == true;
        (v["phase"] == "live"
            && linked
            && v["session"]["status"] == "idle"
            && v["sent"] == 0
            && v["waiting"] == 0)
            .then_some(v)
    })
}

/// `n` wakes through `cataly lead send`: for each, the CLI start to the model request that carries
/// the wake (from the request's arrival time, not the next request of any kind) and to the CLI's
/// exit.
fn wakes(hub: &TestHub, model: &FakeModel, n: usize) -> Vec<(Duration, Duration)> {
    (0..n)
        .map(|i| {
            settled(hub);
            let before = model.arrivals().len();
            let text = format!("wake {i}");
            let sent = Instant::now();
            let out = hub.cli(&["lead", "send", &text]);
            let cli = sent.elapsed();
            assert!(out.status.success(), "{out:?}");
            let arrived = hub.until(WAIT, || {
                let bodies = model.bodies();
                let at = (before..bodies.len()).find(|&k| bodies[k].to_string().contains(&text))?;
                model.arrivals().get(at).copied()
            });
            (arrived.saturating_duration_since(sent), cli)
        })
        .collect()
}

/// p50 and max in milliseconds.
fn spread(mut v: Vec<Duration>) -> String {
    v.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    format!(
        "p50={:.1} max={:.1} n={}",
        ms(v[v.len() / 2]),
        ms(v[v.len() - 1]),
        v.len()
    )
}

fn report(label: &str, hub: &TestHub, runs: &[(Duration, Duration)]) {
    let stats: Value = hub.json(&["stats", "--json"]);
    eprintln!(
        "MEASURE {label} cli_to_model_ms {} | cli_exit_ms {} | hub_send_to_written {} | written_to_agent_start {}",
        spread(runs.iter().map(|r| r.0).collect()),
        spread(runs.iter().map(|r| r.1).collect()),
        stats["wake_latency"],
        stats["agent_start_latency"]
    );
}

/// Resident memory (KiB) and CPU time (ms, user + system) of a process, from `/proc` (10 ms
/// resolution).
#[cfg(target_os = "linux")]
fn usage(pid: u64) -> (u64, u64) {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let rss = status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .unwrap();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let fields: Vec<&str> = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .collect();
    let ticks: u64 = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
    (rss, ticks * 10)
}

/// Resident memory (KiB) and CPU time (ms, user + system) of a process, from `ps`: macOS has no
/// `/proc`, and its `time` column reads `[h:]m:s.cc` (10 ms resolution).
#[cfg(not(target_os = "linux"))]
fn usage(pid: u64) -> (u64, u64) {
    let out = Command::new("ps")
        .args(["-o", "rss=,time=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    let mut words = text.split_whitespace();
    let rss = words.next().unwrap().parse().unwrap();
    let time = words.next().unwrap();
    let (whole, frac) = time.split_once('.').unwrap_or((time, "0"));
    let secs = whole
        .split(':')
        .fold(0u64, |acc, p| acc * 60 + p.parse::<u64>().unwrap());
    let frac_ms: u64 = format!("{frac:0<3}")[..3].parse().unwrap();
    (rss, secs * 1000 + frac_ms)
}

/// The block's screen output so far (the stand-in keeps one file per block), without terminal
/// control sequences and white space, since the TUI positions words with cursor moves.
fn screen(hub: &TestHub) -> String {
    let raw: String = std::fs::read_dir(hub.root.join("bin/tern-data"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".out"))
        .map(|e| String::from_utf8_lossy(&std::fs::read(e.path()).unwrap()).into_owned())
        .collect();
    let mut text = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        match c {
            // CSI ends at its final byte, OSC at BEL or ESC \, a charset choice after one more
            // character, any other escape after its one character.
            '\u{1b}' => match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    for c in chars.by_ref() {
                        if c == '\u{7}' || c == '\u{1b}' {
                            break;
                        }
                    }
                }
                Some('(' | ')') => {
                    chars.next();
                }
                _ => {}
            },
            c if c.is_whitespace() || c.is_control() => {}
            c => text.push(c),
        }
    }
    text
}

fn kill(pid: u64) {
    let _ = Command::new("kill").arg(pid.to_string()).status();
}

/// Checks 1 and 2 with stock OMP's TUI, the tern wake by stage, and the bridge's own cost: the same
/// session and workload (ten turns, then 30 s idle) with the bridge active through the hub and with
/// OMP's extensions off in the same pseudo-terminal stand-in.
#[test]
fn tern_lead_in_stock_omp() {
    if !enabled() {
        return;
    }
    let model = FakeModel::start();
    let mut hub = sandbox(&model, "tern", "", &|_| String::new());
    hub.start();
    // Check 1: the block runs OMP; its bridge presents the token and the hub binds the lead.
    let live = settled(&hub);
    let calls = std::fs::read_to_string(hub.root.join("bin/tern-data/calls.log")).unwrap();
    assert!(
        calls.contains("CATALY_LAUNCH=") && !calls.contains("rpc-ui"),
        "{calls}"
    );
    // Check 2 and the wake by stage.
    let runs = wakes(&hub, &model, 10);
    assert!(
        model
            .bodies()
            .iter()
            .any(|b| b.to_string().contains("wake 9"))
    );
    report("tern", &hub, &runs);

    // The bridge's cost: the same session resumed in the stand-in, the same typed workload, with
    // the bridge active (hub-hosted tern lead) and with OMP's extensions off.
    let file = PathBuf::from(live["session"]["session_file"].as_str().unwrap());
    // The last wake's turn may still run, and `lead stop` refuses a working lead.
    settled(&hub);
    let out = hub.cli(&["lead", "stop"]);
    assert!(out.status.success(), "{out:?}");
    let input = typing(&hub);
    assert!(hub.cli(&["lead", "start"]).status.success());
    let mut input = input.join().unwrap();
    let live = settled(&hub);
    assert_eq!(live["session"]["session_file"], file.display().to_string());
    let with = workload(live["session"]["pid"].as_u64().unwrap(), || {
        typed_turns(&hub, &model, &mut input);
    });
    assert!(hub.cli(&["lead", "stop"]).status.success());
    drop(input);
    let argv = omp_argv(&calls, &file);
    let without = without_bridge(&hub, &model, &argv);
    eprintln!(
        "MEASURE bridge_cost with: start_rss_kib={} after_turns_rss_kib={} idle_rss_kib={} turns_cpu_ms={} idle_30s_cpu_ms={} | without: start_rss_kib={} after_turns_rss_kib={} idle_rss_kib={} turns_cpu_ms={} idle_30s_cpu_ms={}",
        with.0,
        with.1,
        with.2,
        with.3,
        with.4,
        without.0,
        without.1,
        without.2,
        without.3,
        without.4
    );
    assert!(hub.stop().success());
}

/// RSS 5 s after start, after the turns and after 30 s idle (KiB); CPU of the turns and of the idle
/// (ms).
fn workload(pid: u64, turns: impl FnOnce()) -> (u64, u64, u64, u64, u64) {
    std::thread::sleep(Duration::from_secs(5));
    let (rss0, cpu0) = usage(pid);
    turns();
    let (rss1, cpu1) = usage(pid);
    std::thread::sleep(Duration::from_secs(30));
    let (rss2, cpu2) = usage(pid);
    (rss0, rss1, rss2, cpu1 - cpu0, cpu2 - cpu1)
}

/// A fresh named pipe the next block reads as its terminal input; the writer end opens once the
/// block starts.
fn typing(hub: &TestHub) -> std::thread::JoinHandle<std::fs::File> {
    let fifo = hub.root.join("bin/tern-data/input");
    let _ = std::fs::remove_file(&fifo);
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    std::thread::spawn(move || std::fs::OpenOptions::new().write(true).open(fifo).unwrap())
}

/// Ten prompts typed at the terminal, each answered by the model before the next.
fn typed_turns(hub: &TestHub, model: &FakeModel, input: &mut std::fs::File) {
    for i in 0..10 {
        let before = model.arrivals().len();
        input.write_all(format!("typed {i}\r").as_bytes()).unwrap();
        input.flush().unwrap();
        hub.until(WAIT, || (model.arrivals().len() > before).then_some(()));
        // The answer is one chunk; give the TUI time to end the turn.
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// The command line Tern ran for the first block (`/usr/bin/env` with the probe's variables, then
/// OMP), without the launch token and socket, resuming `file` with extensions off.
fn omp_argv(calls: &str, file: &Path) -> Vec<String> {
    let line = calls.lines().next().unwrap();
    let words: Vec<&str> = line.split_whitespace().collect();
    let at = words.iter().position(|w| *w == "/usr/bin/env").unwrap();
    let mut argv: Vec<String> = words[at..]
        .iter()
        .filter(|w| !w.starts_with("CATALY_"))
        .map(|w| {
            if w.starts_with("--session-dir=") {
                format!("--resume={}", file.display())
            } else {
                (*w).to_string()
            }
        })
        .collect();
    argv.push("--no-extensions".into());
    argv
}

/// `argv` in a block of the stand-in, as the hub opens one, with the typed workload.
fn without_bridge(hub: &TestHub, model: &FakeModel, argv: &[String]) -> (u64, u64, u64, u64, u64) {
    let input = typing(hub);
    let work = hub.root.join("work");
    let out = Command::new(hub.root.join("bin/tern"))
        .args([
            "new",
            "tab",
            "--json",
            "--cwd",
            &work.display().to_string(),
            "--",
        ])
        .args(argv)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", hub.root.join("home"))
        .env("XDG_CONFIG_HOME", hub.root.join("config"))
        .env("XDG_STATE_HOME", hub.root.join("state"))
        .env("XDG_RUNTIME_DIR", hub.root.join("run"))
        .env("PI_CODING_AGENT_DIR", hub.root.join("agent"))
        .output()
        .unwrap();
    let block: Value = serde_json::from_slice(&out.stdout).unwrap();
    let mut input = input.join().unwrap();
    let pidfile = hub.root.join(format!(
        "bin/tern-data/block-{}.pid",
        block["id"].as_str().unwrap()
    ));
    let pty: u64 = std::fs::read_to_string(pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let pid: u64 = hub.until(WAIT, || {
        let out = Command::new("ps")
            .args(["-o", "pid=", "--ppid", &pty.to_string()])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    });
    let result = workload(pid, || typed_turns(hub, model, &mut input));
    kill(pid);
    result
}

/// The same wakes on the headless runtime, for comparison with the tern figures.
#[test]
fn headless_wake_for_comparison() {
    if !enabled() {
        return;
    }
    let model = FakeModel::start();
    let mut hub = sandbox(&model, "headless", "", &|_| String::new());
    hub.start();
    let runs = wakes(&hub, &model, 10);
    report("headless", &hub, &runs);
    assert!(hub.stop().success());
}

/// Check 4 with stock OMP: an approval in the tern lead blocks it and the hub never answers it; an
/// `ask` form is mirrored to Discord, the owner's press answers it, and the terminal's own dialog
/// is withdrawn (the tool call ends with the Discord answer and the turn goes on).
#[test]
fn tern_dialogs_in_stock_omp() {
    if !enabled() {
        return;
    }
    let model = FakeModel::start();
    let fake = FakeDiscord::start();
    fake.channel(CH, 0, None);
    let mut hub = sandbox(
        &model,
        "tern",
        "tools:\n  approvalMode: always-ask\n",
        &|token| {
            format!(
                "{}form_default_after = \"10m\"\n",
                fake.config(token, OWNER, &[CH])
            )
        },
    );
    hub.start();
    settled(&hub);

    // An approval: blocked, refused to the hub, never answered.
    fake.say(FakeDiscord::message(
        CH,
        800_000_000_000_000_301,
        OWNER,
        "TOOL please",
    ));
    let blocked = hub.until(WAIT, || {
        let v = hub.json(&["lead", "status", "--json"]);
        (v["session"]["status"] == "blocked").then_some(v)
    });
    let dialog = blocked["session"]["dialogs"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let asked = model.arrivals().len();
    let out = hub.cli(&["lead", "answer", &dialog, r#"{"value":"Allow"}"#]);
    assert!(!out.status.success());
    assert!(
        format!("{out:?}").contains("person at the terminal"),
        "{out:?}"
    );
    std::thread::sleep(Duration::from_secs(3));
    let still = hub.json(&["lead", "status", "--json"]);
    assert_eq!(still["session"]["status"], "blocked", "{still}");
    assert_eq!(
        model.arrivals().len(),
        asked,
        "the tool ran without an answer"
    );
    assert!(hub.cli(&["lead", "interrupt"]).status.success());
    settled(&hub);

    // A mirrored form answered from Discord.
    let source = 800_000_000_000_000_302u64;
    fake.say(FakeDiscord::message(CH, source, OWNER, "ASK me something"));
    let q = fake.wait(WAIT, |f| {
        f.bot_messages(CH)
            .into_iter()
            .find(|m| m["components"].as_array().is_some_and(|c| !c.is_empty()))
    });
    assert!(
        q["content"].as_str().unwrap().contains("Pick a color"),
        "{q}"
    );
    // The terminal shows its own dialog for the same form.
    let shown = Instant::now();
    while !screen(&hub).contains("Pickacolor") {
        let text = screen(&hub);
        let tail: String = text
            .chars()
            .skip(text.chars().count().saturating_sub(600))
            .collect();
        assert!(shown.elapsed() < WAIT, "no local dialog: ...{tail}");
        std::thread::sleep(Duration::from_millis(100));
    }
    let blue = q["components"][0]["components"][1]["custom_id"]
        .as_str()
        .unwrap();
    fake.press(OWNER, CH, q["id"].as_str().unwrap(), blue);
    let answer = fake.wait(WAIT, |f| {
        f.creates().into_iter().find(|c| {
            c.body["message_reference"]["message_id"].as_str() == Some(&source.to_string())
        })
    });
    assert_eq!(answer.body["content"], "ok");
    assert!(
        model
            .bodies()
            .iter()
            .any(|b| b.to_string().contains("The user answered (at Discord)")),
        "the tool call did not end with the Discord answer"
    );
    let lead = settled(&hub);
    assert!(
        lead["session"]["dialogs"].as_array().unwrap().is_empty(),
        "{lead}"
    );
    let pid = lead["session"]["pid"].as_u64().unwrap();
    assert!(hub.stop().success());
    kill(pid);
}

/// Check 5 with stock OMP: a tern lane whose OMP is stopped past 45 s is marked lost; when the
/// same process continues, its bridge reconnects with the same session and the lane is restored.
#[test]
fn tern_lane_lost_and_restored_in_stock_omp() {
    if !enabled() {
        return;
    }
    let model = FakeModel::start();
    let mut hub = sandbox(&model, "tern", "", &|_| String::new());
    hub.start();
    settled(&hub);
    let params = json!({ "key": "far-1", "title": "Work far-1", "brief": "Do the work.",
        "checks": [{ "kind": "command", "argv": ["true"] }] });
    let out = hub.cli(&["lane", "open", "--json", "--params", &params.to_string()]);
    assert!(out.status.success(), "{out:?}");
    let record = || {
        let all = hub.json(&["lane", "list", "--json", "--params", r#"{"all":true}"#]);
        all.as_array()
            .unwrap()
            .iter()
            .find(|l| l["key"] == "far-1")
            .cloned()
            .unwrap()
    };
    // The lane's goal is set in stock OMP's goal mode, so the start ends running, not held.
    let rec = hub.until(WAIT, || {
        let r = record();
        (r["live"] == true
            && r["pid"].is_u64()
            && matches!(r["state"].as_str(), Some("running" | "held")))
        .then_some(r)
    });
    assert_eq!(rec["state"], "running", "{rec}");
    let (pid, session) = (rec["pid"].as_u64().unwrap(), rec["session_id"].clone());
    let stop = Command::new("kill")
        .args(["-STOP", &pid.to_string()])
        .status()
        .unwrap();
    assert!(stop.success());
    let stopped = Instant::now();
    let lost = hub.until(Duration::from_secs(90), || {
        let r = record();
        r["lost"].is_string().then_some(r)
    });
    let after = stopped.elapsed();
    assert!(
        after >= Duration::from_secs(30),
        "lost after {after:?}: {lost}"
    );
    // The event is recorded by the hub's event task after the lane record changed.
    hub.until(WAIT, || {
        hub.events()
            .iter()
            .any(|e| e["kind"] == "lane.lost" && e.to_string().contains("far-1"))
            .then_some(())
    });
    let cont = Command::new("kill")
        .args(["-CONT", &pid.to_string()])
        .status()
        .unwrap();
    assert!(cont.success());
    let back = hub.until(WAIT, || {
        let r = record();
        (r["lost"].is_null() && r["live"] == true).then_some(r)
    });
    assert_eq!(
        (back["pid"].as_u64(), &back["session_id"]),
        (Some(pid), &session)
    );
    eprintln!(
        "MEASURE tern_lane_lost_after_stop_s={:.1}",
        after.as_secs_f64()
    );
    assert!(hub.stop().success());
    kill(pid);
}

/// LANE req. 44 in a terminal session with stock OMP, which refuses a goal change while a turn
/// runs: holding a working tern lane interrupts the turn, waits for it to end, then pauses the goal
/// and confirms it; continuing resumes it.
#[test]
fn tern_lane_held_while_working_in_stock_omp() {
    if !enabled() {
        return;
    }
    let model = FakeModel::start();
    let mut hub = sandbox(&model, "tern", "", &|_| String::new());
    hub.start();
    settled(&hub);
    let brief = "Run this first.\nCALL bash {\"command\":\"sleep 60\"}";
    let params = json!({ "key": "busy-1", "title": "Work busy-1", "brief": brief,
        "checks": [{ "kind": "command", "argv": ["true"] }] });
    let lane = |op: &str, p: Value| hub.cli(&["lane", op, "--json", "--params", &p.to_string()]);
    let out = lane("open", params);
    assert!(out.status.success(), "{out:?}");
    let read = || {
        let out = lane("read", json!({ "key": "busy-1", "count": 1 }));
        serde_json::from_slice::<Value>(&out.stdout).unwrap_or(Value::Null)
    };
    let state = || {
        let all = hub.json(&["lane", "list", "--json", "--params", r#"{"all":true}"#]);
        all.as_array()
            .unwrap()
            .iter()
            .find(|l| l["key"] == "busy-1")
            .cloned()
            .unwrap()
    };
    // The kickoff turn runs the long `bash` call under an active goal.
    let deadline = Instant::now() + WAIT;
    loop {
        let asked = model
            .bodies()
            .iter()
            .any(|b| b.to_string().contains("[lane busy-1]"));
        let r = read();
        if asked && r["status"] == "working" && r["goal"]["status"] == "active" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "kickoff not seen working under an active goal: asked {asked}, read {r}, lane {}",
            state()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(state()["state"], "running");
    let asked = Instant::now();
    let out = lane(
        "pause",
        json!({ "key": "busy-1", "reason": "owner said wait" }),
    );
    let took = asked.elapsed();
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("held"),
        "{out:?}"
    );
    assert!(took < Duration::from_secs(20), "held after {took:?}");
    let held = state();
    assert_eq!(held["state"], "held", "{held}");
    let r = read();
    assert_eq!(r["goal"]["status"], "paused", "{r}");
    assert_ne!(r["status"], "working", "{r}");
    // No goal continuation starts while the lane is held.
    std::thread::sleep(Duration::from_secs(3));
    let r = read();
    assert_eq!(
        (&r["goal"]["status"], &r["status"]),
        (&json!("paused"), &json!("idle")),
        "{r}"
    );
    let out = lane("resume", json!({ "key": "busy-1" }));
    assert!(out.status.success(), "{out:?}");
    assert_eq!(state()["state"], "running");
    assert_eq!(read()["goal"]["status"], "active");
    eprintln!(
        "MEASURE tern_lane_hold_while_working_s={:.1}",
        took.as_secs_f64()
    );
    let pid = state()["pid"].as_u64().unwrap();
    assert!(hub.stop().success());
    kill(pid);
}
