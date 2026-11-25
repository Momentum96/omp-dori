//! The terminal runtime against stock OMP (milestone 6): the Tern stand-in runs the block's command
//! line, stock OMP's TUI, in a pseudo-terminal with the loopback model; the bridge in that OMP says
//! hello with the single-use token and the hub controls the lead through it. Runs only with
//! `CATALY_CONTRACT_OMP=1` (`CATALY_CONTRACT_OMP_PATH` selects the executable).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::Value;
use support::TestHub;
use support::model::{FakeModel, models_yml};

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

/// Resident memory (KiB) and CPU time (s) of a process, from `ps`.
fn usage(pid: u64) -> (u64, f64) {
    let out = Command::new("ps")
        .args(["-o", "rss=,time=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.split_whitespace();
    let rss = parts.next().unwrap().parse().unwrap();
    let cpu = parts
        .next()
        .unwrap()
        .split([':', '-'])
        .fold(0.0, |acc, p| acc * 60.0 + p.parse::<f64>().unwrap_or(0.0));
    (rss, cpu)
}

/// Checks 1 and 2 with stock OMP's TUI, the tern wake latency and the bridge's cost.
#[test]
fn tern_lead_in_stock_omp() {
    if !enabled() {
        return;
    }
    let root = std::env::temp_dir().join(format!("cataly-omp-tern-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for dir in ["agent", "work"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    let model = FakeModel::start();
    std::fs::write(
        root.join("agent/models.yml"),
        models_yml(model.port, &["primary"]),
    )
    .unwrap();
    let mut hub = TestHub::new("");
    let tern = hub.install_fake("tern");
    hub.env
        .push(("PI_CODING_AGENT_DIR".into(), root.join("agent")));
    hub.write_config(&format!(
        "[omp]\npath = \"{}\"\n[sessions]\nruntime = \"tern\"\ntern = \"{}\"\n\
         extra_flags = [\"--no-skills\", \"--no-rules\", \"--no-context-files\"]\n\
         [lead]\nmodel = \"fake/primary\"\ncwd = \"{}\"\n",
        installed_omp().display(),
        tern.display(),
        root.join("work").display()
    ));
    hub.start();
    // Check 1: the block runs OMP; its bridge presents the token and the hub binds the lead.
    let live = hub.until(Duration::from_secs(60), || {
        let v = hub.json(&["lead", "status", "--json"]);
        (v["phase"] == "live" && v["bridge"]["connected"] == true).then_some(v)
    });
    let pid = live["session"]["pid"].as_u64().unwrap();
    let calls = std::fs::read_to_string(hub.root.join("bin/tern-data/calls.log")).unwrap();
    assert!(
        calls.contains("CATALY_LAUNCH=") && !calls.contains("rpc-ui"),
        "{calls}"
    );
    // Check 2: a wake through the bridge reaches the model and is confirmed from the file.
    let sent = Instant::now();
    assert!(hub.cli(&["lead", "send", "tern check"]).status.success());
    hub.until(Duration::from_secs(60), || {
        model
            .bodies()
            .iter()
            .any(|b| b.to_string().contains("tern check"))
            .then_some(())
    });
    let to_model = sent.elapsed();
    hub.until(Duration::from_secs(60), || {
        let v = hub.json(&["lead", "status", "--json"]);
        (v["sent"] == 0 && v["waiting"] == 0).then_some(())
    });
    let stats: Value = hub.json(&["stats", "--json"]);
    // The bridge's cost in the active session: OMP's RSS and CPU over 30 s connected and idle.
    let (rss0, cpu0) = usage(pid);
    std::thread::sleep(Duration::from_secs(30));
    let (rss1, cpu1) = usage(pid);
    eprintln!(
        "MEASURE tern_cli_to_model_ms={} agent_start_latency={} omp_tui_rss_kib_before={rss0} \
         omp_tui_rss_kib_after={rss1} omp_tui_cpu_s_idle_30s={:.2}",
        to_model.as_millis(),
        stats["agent_start_latency"],
        cpu1 - cpu0
    );
    assert!(hub.stop().success());
    let _ = Command::new("kill").arg(pid.to_string()).status();
    let _ = std::fs::remove_dir_all(&root);
}
