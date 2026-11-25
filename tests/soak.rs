//! Soak runs of the release hub against the fakes, in compressed time (ignored by default):
//! `cargo test --release --test soak -- --ignored --nocapture --test-threads=1`. Every few
//! iterations the hub's resident memory (total, anonymous, file-backed), open descriptors, threads
//! and live tokio tasks are sampled; the report gives each one's slope after the warm-up. Memory
//! that levels off is caches filling; growth with every iteration is retained memory.
//! `CATALY_SOAK_ITER` sets the iterations (default 2000). For bisection, `CATALY_SOAK_SKIP` leaves
//! parts out (comma list of `discord`, `send`, `restarts`, `lanes`, `jobs`, `reconnects`), and the
//! allocator's `MALLOC_*` and `GLIBC_TUNABLES` variables and `LD_PRELOAD` (a heap sampler, see
//! `docs/measurements.md`) are passed to the hub.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::print_stdout
)]

mod support;

use std::collections::HashSet;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::TestHub;
use support::discord::{FakeDiscord, Gw};

const OWNER: &str = "400000000000000001";
const CH: &str = "600000000000000001";
const OPS: &str = "600000000000000002";
const WAIT: Duration = Duration::from_secs(30);
/// The first Discord message ID of a run; iteration `i` sends `FIRST + 1000 * i`. Above every ID
/// the fake gives the bot's own posts (900…001 upward), and spaced so the bot's posts of one
/// iteration fit between: later messages have higher IDs, as on Discord.
const FIRST: u64 = 950_000_000_000_000_000;

/// One reading of the hub process.
#[derive(Debug, Clone, Copy)]
struct Sample {
    iteration: usize,
    rss_kib: u64,
    anon_kib: u64,
    file_kib: u64,
    fds: u64,
    threads: u64,
    tasks: u64,
}

fn status_field(text: &str, name: &str) -> u64 {
    text.lines()
        .find_map(|l| l.strip_prefix(name))
        .and_then(|rest| rest.trim_start_matches(':').split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

fn sample(hub: &TestHub, iteration: usize) -> Sample {
    let pid = hub.pid();
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let fds = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap()
        .count() as u64;
    // A hub built before `tasks` was reported reads 0.
    let tasks = hub.json(&["stats", "--json"])["tasks"]
        .as_u64()
        .unwrap_or(0);
    Sample {
        iteration,
        rss_kib: status_field(&status, "VmRSS"),
        anon_kib: status_field(&status, "RssAnon"),
        file_kib: status_field(&status, "RssFile"),
        fds,
        threads: status_field(&status, "Threads"),
        tasks,
    }
}

/// Least-squares slope of `value` per 1000 iterations over the samples after the warm-up (the first
/// quarter of the run).
fn slope(samples: &[Sample], value: impl Fn(&Sample) -> u64) -> f64 {
    let warm = samples.len() / 4;
    let tail = &samples[warm..];
    let n = tail.len() as f64;
    let mx = tail.iter().map(|s| s.iteration as f64).sum::<f64>() / n;
    let my = tail.iter().map(|s| value(s) as f64).sum::<f64>() / n;
    let (mut num, mut den) = (0.0, 0.0);
    for s in tail {
        let dx = s.iteration as f64 - mx;
        num += dx * (value(s) as f64 - my);
        den += dx * dx;
    }
    if den == 0.0 { 0.0 } else { num / den * 1000.0 }
}

fn report(name: &str, samples: &[Sample]) {
    println!("SOAK {name}: samples (iteration, rss KiB, anon KiB, file KiB, fds, threads, tasks):");
    for s in samples {
        println!(
            "  {} {} {} {} {} {} {}",
            s.iteration, s.rss_kib, s.anon_kib, s.file_kib, s.fds, s.threads, s.tasks
        );
    }
    println!(
        "SOAK {name}: slope per 1000 iterations after warm-up: rss {:.1} KiB, anon {:.1} KiB, \
         file {:.1} KiB, fds {:.2}, threads {:.2}, tasks {:.2}",
        slope(samples, |s| s.rss_kib),
        slope(samples, |s| s.anon_kib),
        slope(samples, |s| s.file_kib),
        slope(samples, |s| s.fds),
        slope(samples, |s| s.threads),
        slope(samples, |s| s.tasks),
    );
}

/// Descriptors, threads and tasks are counts that must not drift: the last sample may exceed the
/// first sample after the warm-up only by what in-flight work holds at one moment.
fn assert_counts_level(samples: &[Sample]) {
    let warm = samples[samples.len() / 4];
    let last = samples[samples.len() - 1];
    assert!(
        last.fds <= warm.fds + 8,
        "descriptors grew: {warm:?} -> {last:?}"
    );
    assert!(
        last.threads <= warm.threads + 8,
        "threads grew: {warm:?} -> {last:?}"
    );
    assert!(
        last.tasks <= warm.tasks + 16,
        "tasks grew: {warm:?} -> {last:?}"
    );
}

fn iterations() -> usize {
    std::env::var("CATALY_SOAK_ITER")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000)
}

/// Whether `CATALY_SOAK_SKIP` leaves this part out.
fn skip(part: &str) -> bool {
    std::env::var("CATALY_SOAK_SKIP").is_ok_and(|v| v.split(',').any(|p| p.trim() == part))
}

/// The hub with the fake OMP (turns and the ask form) as lead and lanes, intake and the ops room on
/// the fake Discord, and every job kind: owner jobs on the fake `gh` and OMP, a probe and a cron
/// job.
fn soak_hub(fake: &FakeDiscord) -> TestHub {
    let mut hub = TestHub::new("");
    for (key, value) in std::env::vars_os() {
        let key = key.to_string_lossy().into_owned();
        if key.starts_with("MALLOC_") || key == "GLIBC_TUNABLES" || key == "LD_PRELOAD" {
            hub.env.push((key, PathBuf::from(value)));
        }
    }
    let omp = hub.install_fake("omp");
    let gh = hub.install_fake("gh");
    for f in ["rpc", "turns", "ask"] {
        hub.fixture("omp", f, Some(""));
    }
    hub.fixture("gh", "pr-list.json", Some("[]"));
    let work = hub.root.join("work");
    let tree = hub.root.join("tree");
    for dir in [&work, &tree] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let git = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&tree)
        .env("HOME", &hub.root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(git.success());
    let watched = hub.private_file("watched", "x");
    let token = hub.private_file("token", "fake-token");
    fake.channel(CH, 0, None);
    fake.channel(OPS, 0, None);
    let probe = support::probe(
        "ping",
        r#"["printf", "pong"]"#,
        "destinations = [\"cataly-ops\"]\n",
    );
    hub.write_config(&format!(
        "[omp]\npath = \"{omp}\"\n[lead]\nmodel = \"fake/primary\"\ncwd = \"{work}\"\n\
         {discord}form_default_after = \"2s\"\n\
         [lanes]\nmodel = \"fake/primary\"\ncwd = \"{work}\"\nobjection = \"2s\"\n\
         [destinations.cataly-ops]\nkind = \"discord\"\nchannel = \"{OPS}\"\n\n{probe}\
         [jobs.tick]\nkind = \"cron\"\ncron = \"* * * * *\"\nzone = \"UTC\"\nargv = [\"true\"]\ncwd = \"/\"\n\
         [jobs.gh]\nkind = \"github\"\ngh = \"{gh}\"\nnotifications = false\nlogin = \"me\"\n\
         repos = [{{ repo = \"o/r\", authors = [\"me\"] }}]\n\
         [jobs.pr]\nkind = \"pr\"\ngh = \"{gh}\"\nlogin = \"me\"\n\
         [jobs.q]\nkind = \"quota\"\n\
         [jobs.bk]\nkind = \"backup\"\ndir = \"{tree}\"\nname = \"Backup\"\nemail = \"b@example.org\"\n\
         [jobs.h]\nkind = \"health\"\nfiles = [{{ path = \"{watched}\", max_age = \"1h\" }}]\n\
         [jobs.mail]\nkind = \"items\"\nargv = [\"echo\", \"one\"]\n\
         [jobs.host]\nkind = \"pressure\"\n",
        omp = omp.display(),
        work = work.display(),
        discord = fake.config(&token, OWNER, &[CH]),
        gh = gh.display(),
        tree = tree.display(),
        watched = watched.display(),
    ));
    hub
}

fn ready(hub: &TestHub) {
    hub.until(WAIT, || {
        let s = hub.json(&["discord", "status", "--json"]);
        let lead = hub.json(&["lead", "status", "--json"]);
        (s["healthy"] == true && lead["phase"] == "live").then_some(())
    });
}

fn reference(body: &Value) -> Option<&str> {
    body["message_reference"]["message_id"].as_str()
}

/// Waits for the hub's reply to message `id`; returns its text.
fn reply(fake: &FakeDiscord, id: u64) -> String {
    let id = id.to_string();
    fake.wait(WAIT, |f| {
        f.creates()
            .into_iter()
            .find(|c| reference(&c.body) == Some(id.as_str()))
            .map(|c| c.body["content"].as_str().unwrap_or_default().to_string())
    })
}

/// A question card not seen before; returns its message and its first button.
fn new_card(fake: &FakeDiscord, seen: &mut HashSet<String>) -> (String, String) {
    let card = fake.wait(WAIT, |f| {
        f.bot_messages(CH).into_iter().rev().find(|m| {
            m["components"].as_array().is_some_and(|c| !c.is_empty())
                && !seen.contains(m["id"].as_str().unwrap())
        })
    });
    let id = card["id"].as_str().unwrap().to_string();
    seen.insert(id.clone());
    let button = card["components"][0]["components"][0]["custom_id"]
        .as_str()
        .unwrap()
        .to_string();
    (id, button)
}

fn lane(hub: &TestHub, op: &str, params: &Value) -> Value {
    let out = hub.cli(&["lane", op, "--json", "--params", &params.to_string()]);
    assert!(out.status.success(), "lane {op}: {out:?}");
    serde_json::from_slice(&out.stdout).unwrap()
}

fn record(hub: &TestHub, key: &str) -> Value {
    lane(hub, "list", &json!({ "all": true }))
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["key"] == key)
        .cloned()
        .unwrap_or(Value::Null)
}

/// Lanes on the fake OMP, no relays.
fn lane_hub() -> TestHub {
    let hub = TestHub::new("");
    let omp = hub.install_fake("omp");
    for f in ["rpc", "turns"] {
        hub.fixture("omp", f, Some(""));
    }
    let work = hub.root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    hub.write_config(&format!(
        "[omp]\npath = \"{}\"\n[lanes]\nmodel = \"fake/primary\"\ncwd = \"{}\"\nobjection = \"2s\"\n",
        omp.display(),
        work.display()
    ));
    hub
}

/// Closed lanes leave no per-lane state in the hub: no spend reader, no turn lock; their bridge
/// links stay within the bound.
#[test]
fn closed_lanes_leave_no_per_lane_state() {
    let mut hub = lane_hub();
    hub.start();
    hub.until(WAIT, || {
        hub.json(&["status", "--json"])["omp"]
            .is_object()
            .then_some(())
    });
    for n in 0..3 {
        lane_cycle(&hub, &format!("c-{n}"));
    }
    let retained = hub.json(&["stats", "--json"])["retained"].clone();
    assert_eq!(retained["lane_spend"], 0, "{retained}");
    assert!(retained["lane_turns"].as_u64().unwrap() <= 1, "{retained}");
    assert!(
        retained["bridge_links"].as_u64().unwrap() <= 3,
        "{retained}"
    );
    assert!(hub.stop().success());
}

fn omp_data(hub: &TestHub) -> PathBuf {
    hub.root.join("bin/omp-data")
}

/// The launch token the fake OMP of this session recorded.
fn token(hub: &TestHub, session: &str) -> String {
    let text = std::fs::read_to_string(omp_data(hub).join("launch")).unwrap();
    text.lines()
        .rfind(|l| l.split(' ').nth(1) == Some(session))
        .and_then(|l| l.split(' ').next())
        .unwrap()
        .to_string()
}

/// A lane opened, its bridge connected and disconnected, the lane closed.
fn lane_cycle(hub: &TestHub, key: &str) {
    let opened = lane(
        hub,
        "open",
        &json!({ "key": key, "title": format!("Soak {key}"), "brief": "Do the work.",
                 "checks": [{ "kind": "command", "argv": ["true"] }] }),
    );
    assert_eq!(opened["outcome"], "started", "{opened}");
    let session = record(hub, key)["session_id"].as_str().unwrap().to_string();
    let stream = UnixStream::connect(hub.socket()).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    let hello = json!({ "v": 1, "id": 1, "method": "bridge.hello",
        "params": { "ext": 1, "token": token(hub, &session), "session": session, "agent": "main" } });
    (&stream)
        .write_all(format!("{hello}\n").as_bytes())
        .unwrap();
    let mut answer = String::new();
    BufReader::new(&stream).read_line(&mut answer).unwrap();
    assert!(answer.contains("\"ok\""), "{answer}");
    drop(stream);
    let closed = lane(hub, "close", &json!({ "key": key, "note": "done" }));
    assert!(!closed.is_null());
    hub.until(WAIT, || {
        (record(hub, key)["state"] == "closed").then_some(())
    });
}

/// Owner messages and replies, question cards answered and expired, Gateway reconnects, lead
/// restarts, lane cycles with bridge connections, job ticks of every kind and ops panel edits.
#[test]
#[ignore = "soak; run with --release --ignored --nocapture"]
fn hub_soak() {
    let total = iterations();
    let every = (total / 40).max(1);
    let fake = FakeDiscord::start();
    let mut hub = soak_hub(&fake);
    hub.start();
    ready(&hub);
    let began = Instant::now();
    let mut samples = vec![sample(&hub, 0)];
    let mut cards = HashSet::new();
    let (mut asked, mut expired, mut reconnects, mut restarts, mut lanes, mut ticks) =
        (0, 0, 0, 0, 0, 0);
    let (mut connected, mut discord) = (Instant::now(), 0);
    for i in 0..total {
        let id = FIRST + 1000 * i as u64;
        // Every fifth iteration is an owner message on Discord, each with its request ID (the
        // lead's receipts no longer refuse the 513th within a week, decision 20); the others are
        // `cataly lead send` (no request ID), each a full turn of the lead.
        let on_discord = !skip("discord");
        if on_discord && i % 100 == 50 {
            fake.say(FakeDiscord::message(CH, id, OWNER, &format!("ASK {i}")));
            let (card, button) = new_card(&fake, &mut cards);
            if i % 200 == 50 {
                fake.press(OWNER, CH, &card, &button);
                assert_eq!(reply(&fake, id), "picked Red");
                asked += 1;
            } else {
                assert_eq!(reply(&fake, id), "picked Blue");
                expired += 1;
            }
            discord += 1;
        } else if on_discord && i % 5 == 0 {
            fake.say(FakeDiscord::message(CH, id, OWNER, &format!("message {i}")));
            assert_eq!(reply(&fake, id), "ok");
            discord += 1;
        } else if !skip("send") {
            let out = hub.cli(&["lead", "send", &format!("note {i}"), "--quiet"]);
            assert!(out.status.success(), "{out:?}");
        }
        // The Gateway's reconnect backoff resets after 60 s connected; reconnects come after that,
        // so each one waits the first backoff step only.
        if !skip("reconnects") && i % 10 == 5 && connected.elapsed() > Duration::from_secs(61) {
            // A requested reconnect (op 7, resumed) or an invalid session (op 9, identified
            // again), each with its catch-up read; the next message goes after that.
            let catch_ups = || {
                hub.json(&["stats", "--json"])["discord_catch_up_latency"]["count"]
                    .as_u64()
                    .unwrap_or(0)
            };
            let before = catch_ups();
            fake.gw(if reconnects % 2 == 0 {
                Gw::Op7
            } else {
                Gw::Op9(false)
            });
            hub.until(WAIT, || (catch_ups() > before).then_some(()));
            ready(&hub);
            connected = Instant::now();
            reconnects += 1;
        }
        if !skip("restarts") && i % 100 == 60 {
            assert!(hub.cli(&["lead", "stop"]).status.success());
            assert!(hub.cli(&["lead", "start"]).status.success());
            ready(&hub);
            restarts += 1;
        }
        if !skip("lanes") && i % 50 == 40 {
            lane_cycle(&hub, &format!("s-{i}"));
            lanes += 1;
        }
        if !skip("jobs") && i % 20 == 15 {
            for job in ["ping", "tick", "gh", "pr", "q", "bk", "h", "mail", "host"] {
                // A live tick of the same job (the cron job runs every minute) refuses a dry one.
                let out = hub.cli(&["job", "run", job, "--json"]);
                let busy = String::from_utf8_lossy(&out.stderr).contains("is running");
                assert!(out.status.success() || busy, "{out:?}");
                ticks += usize::from(out.status.success());
            }
        }
        if (i + 1) % every == 0 {
            samples.push(sample(&hub, i + 1));
        }
    }
    let panel_edits = fake
        .calls("PATCH", &format!("/channels/{OPS}/messages"))
        .len();
    println!(
        "SOAK hub_soak: {total} iterations in {:.0} s: {discord} owner messages on Discord and \
         {} by `lead send`; {asked} cards answered, {expired} cards expired, \
         {reconnects} Gateway reconnects, {restarts} lead restarts, {lanes} lane cycles with a \
         bridge connection, {ticks} job ticks, {panel_edits} panel edits",
        began.elapsed().as_secs_f64(),
        total - discord
    );
    report("hub_soak", &samples);
    println!(
        "SOAK hub_soak: retained {}",
        hub.json(&["stats", "--json"])["retained"]
    );
    assert_counts_level(&samples);
    assert!(hub.stop().success());
}
