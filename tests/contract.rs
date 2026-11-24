//! Contract test against the installed stock OMP (PATCH-12 probe cases; milestone 1 acceptance check
//! 12). It runs only with `CATALY_CONTRACT_OMP=1`; `CATALY_CONTRACT_OMP_PATH` selects the executable,
//! else `omp` on `PATH`. OMP runs in an isolated home and agent directory whose `models.yml` names
//! one loopback model, because OMP will not start without a model; the probe sends no prompt, so
//! that model is never called. Only `--version`, an rpc-ui exchange and `session lease` run.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::path::PathBuf;
use std::time::Duration;

use support::TestHub;

fn installed_omp() -> PathBuf {
    if let Some(path) = std::env::var_os("CATALY_CONTRACT_OMP_PATH") {
        return PathBuf::from(path);
    }
    let path = std::env::var_os("PATH").expect("PATH");
    std::env::split_paths(&path)
        .map(|dir| dir.join("omp"))
        .find(|p| p.is_file())
        .expect("omp on PATH")
}

/// One loopback model on a port nothing serves: OMP needs a model to start, the probe never calls it.
const MODELS: &str = "providers:
  cataly-loopback:
    baseUrl: http://127.0.0.1:9/v1
    api: openai-completions
    auth: none
    models:
      - id: probe-model
        name: Probe model (never called)
        contextWindow: 8192
        maxTokens: 1024
";

#[test]
fn probe_against_the_installed_omp() {
    if std::env::var("CATALY_CONTRACT_OMP").as_deref() != Ok("1") {
        eprintln!("skipped: set CATALY_CONTRACT_OMP=1 to probe the installed OMP");
        return;
    }
    let omp = installed_omp();
    let mut hub = TestHub::new("");
    let agent_dir = hub.root.join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(agent_dir.join("models.yml"), MODELS).unwrap();
    hub.env.push(("PI_CODING_AGENT_DIR".into(), agent_dir));
    hub.write_config(&format!("[omp]\npath = \"{}\"\n", omp.display()));
    hub.start();
    let status = hub.until(Duration::from_secs(60), || {
        let s = hub.json(&["status", "--json"]);
        s["omp"].is_object().then_some(s)
    });
    let report = &status["omp"];
    eprintln!("omp probe report: {report}");
    assert!(report["error"].is_null(), "probe failed: {report}");
    assert_eq!(report["meets_minimum"], true, "{report}");
    let version = report["version"].as_str().unwrap();
    assert!(!version.is_empty(), "{report}");
    let protocol = report["protocol"].as_u64().unwrap();
    assert!(protocol == 1 || protocol == 2, "{report}");
    let offered: Vec<u64> = report["offered"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(serde_json::Value::as_u64)
        .collect();
    assert!(offered.contains(&protocol), "{report}");
    assert!(
        report["max_frame_bytes"].as_u64().is_some_and(|n| n > 0),
        "{report}"
    );
    let features: Vec<&str> = report["features"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert!(features.contains(&"lease_cli"), "{report}");
    assert!(status["sessions_refused"].is_null(), "{status}");
    let health = String::from_utf8(hub.cli(&["health"]).stdout).unwrap();
    let line = health.lines().find(|l| l.contains(" omp ")).unwrap();
    assert!(line.contains(version), "{health}");
    assert!(hub.stop().success());
}
