//! The probe (DESIGN 3.4; HOST-03, HOST-35, PATCH-09, PATCH-13): `omp --version` (5 s, 4 KiB), then
//! one ephemeral rpc-ui child with no session, extensions, skills, rules, context files or tools,
//! offline: ready frame, protocol negotiation, `get_state` (no transcript), `goal get`,
//! `set_ask_dialog`, `get_session_lease`, `set_host_tools []`, end of input; 20 s for the exchange.
//! Then the lease command line (10 s). The result is a feature set; nothing falls back silently.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::timeout;

use super::rpc::{self, RESPONSE_TIMEOUT, Rpc, RpcError};
use super::{MINIMUM, TESTED, Version, child_env, usage};
use crate::sys::command::{self, KILL_GRACE, Output, Spec};
use crate::sys::now_ms;

const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
const VERSION_CAP: u64 = 4096;
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(20);
const READY_TIMEOUT: Duration = Duration::from_secs(15);
/// After end of input: 15 s, then TERM, 5 s, KILL (DESIGN 4.2).
const CLOSE_WAIT: Duration = Duration::from_secs(15);
const STDERR_CAP: u64 = 1024 * 1024;
/// A session ID no OMP holds, for the lease probes.
const PROBE_SESSION: &str = "cataly-probe-0";

/// Flags of the ephemeral probe child (DESIGN 3.2).
const PROBE_FLAGS: [&str; 11] = [
    "--mode",
    "rpc-ui",
    "--no-session",
    "--no-extensions",
    "--no-skills",
    "--no-rules",
    "--no-context-files",
    "--no-tools",
    "--offline",
    "--approval-mode",
    "always-ask",
];

/// What the probe found, as health and `status` show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Report {
    /// The executable probed; `None` when the configured name was not found.
    pub(crate) path: Option<String>,
    /// The version OMP printed, like `19.1.1`.
    pub(crate) version: Option<String>,
    pub(crate) minimum: String,
    /// Newest release the contract suite passed on.
    pub(crate) tested: String,
    pub(crate) meets_minimum: bool,
    /// A newer major than the tested release: allowed, reported as untested.
    pub(crate) untested: bool,
    /// Protocol in use after negotiation.
    pub(crate) protocol: Option<u64>,
    /// Versions the ready frame offered.
    pub(crate) offered: Vec<u64>,
    pub(crate) max_frame_bytes: Option<u64>,
    pub(crate) max_reassembled_frame_bytes: Option<u64>,
    /// Confirmed features: `v2_chunks`, `goals`, `ask_dialog`, `lease_rpc`, `lease_cli`,
    /// `host_tools`, `bash_exclude_from_context`.
    pub(crate) features: Vec<String>,
    /// Why the probe failed, if it did.
    pub(crate) error: Option<String>,
    pub(crate) at_ms: i64,
}

impl Report {
    fn new(path: Option<&Path>) -> Self {
        Self {
            path: path.map(|p| p.display().to_string()),
            version: None,
            minimum: MINIMUM.to_string(),
            tested: TESTED.to_string(),
            meets_minimum: false,
            untested: false,
            protocol: None,
            offered: Vec::new(),
            max_frame_bytes: None,
            max_reassembled_frame_bytes: None,
            features: Vec::new(),
            error: None,
            at_ms: now_ms(),
        }
    }

    /// Whether a session may start with this OMP, and why not (PATCH-09).
    pub(crate) fn sessions(&self) -> Result<(), String> {
        match (&self.version, &self.error) {
            (Some(v), _) if !self.meets_minimum => Err(format!(
                "OMP {v} is below the minimum {}; no session can start",
                self.minimum
            )),
            (_, Some(e)) => Err(format!("OMP probe failed: {e}")),
            (None, None) => Err("OMP has not been probed yet".into()),
            (Some(_), None) => Ok(()),
        }
    }

    /// One line for `status` and health.
    pub(crate) fn summary(&self) -> String {
        let path = self.path.as_deref().unwrap_or("(not found)");
        let Some(version) = &self.version else {
            return format!("{path}: {}", self.error.as_deref().unwrap_or("not probed"));
        };
        let mut line = format!("{path} {version}");
        if !self.meets_minimum {
            let _ = write!(line, " (below the minimum {})", self.minimum);
        } else if self.untested {
            let _ = write!(line, " (untested above {})", self.tested);
        }
        if let Some(p) = self.protocol {
            let _ = write!(
                line,
                ", protocol {p} (offered {}), frames {} / {} bytes",
                self.offered
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("/"),
                self.max_frame_bytes
                    .map_or_else(|| "-".into(), |n| n.to_string()),
                self.max_reassembled_frame_bytes
                    .map_or_else(|| "-".into(), |n| n.to_string()),
            );
        }
        if !self.features.is_empty() {
            let _ = write!(line, ", features {}", self.features.join(" "));
        }
        if let Some(e) = &self.error {
            let _ = write!(line, "; probe failed: {e}");
        }
        line
    }
}

/// Runs the whole probe against `path`; `stderr_log` receives the rpc child's stderr.
pub(crate) async fn probe(path: Option<&Path>, stderr_log: Option<&Path>) -> Report {
    let mut report = Report::new(path);
    let Some(path) = path else {
        report.error = Some("the configured OMP executable was not found".into());
        return report;
    };
    let version = match version(path).await {
        Ok(v) => v,
        Err(e) => {
            report.error = Some(e);
            return report;
        }
    };
    report.version = Some(version.to_string());
    report.meets_minimum = version >= MINIMUM;
    report.untested = version.0 > TESTED.0;
    if !report.meets_minimum {
        return report;
    }
    if let Err(e) = exchange(path, stderr_log, &mut report).await {
        report.error = Some(e);
        return report;
    }
    match usage::lease(path, &[PROBE_SESSION]).await {
        Ok(lease) if lease.held.is_empty() => report.features.push("lease_cli".into()),
        Ok(_) => report.error = Some("omp session lease reports an unknown session as held".into()),
        Err(e) => report.error = Some(e),
    }
    report
}

/// `omp --version` → `omp/X.Y.Z`.
pub(crate) async fn version(path: &Path) -> Result<Version, String> {
    let argv = [path.display().to_string(), "--version".to_string()];
    let env = child_env();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let spec = Spec {
        argv: &argv,
        cwd: None,
        env: &env,
        timeout: VERSION_TIMEOUT,
        kill_grace: KILL_GRACE,
        output_cap: VERSION_CAP,
        output: Output::Capture,
    };
    let done = command::run(&spec).await.map_err(|e| e.to_string())?;
    if done.timed_out {
        return Err("omp --version did not answer within 5 s".into());
    }
    if done.truncated {
        return Err("omp --version printed more than 4 KiB".into());
    }
    if !done.success() {
        return Err(format!("omp --version failed ({:?})", done.exit));
    }
    let text = String::from_utf8_lossy(&done.stdout);
    Version::parse_output(&text).ok_or_else(|| {
        format!(
            "the configured executable did not identify itself as OMP: '{}'",
            crate::jobs::event::one_line(text.trim(), 80)
        )
    })
}

async fn exchange(
    path: &Path,
    stderr_log: Option<&Path>,
    report: &mut Report,
) -> Result<(), String> {
    let mut argv = vec![path.display().to_string()];
    argv.extend(PROBE_FLAGS.iter().map(|f| (*f).to_string()));
    let env = child_env();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let cwd = std::env::temp_dir();
    let mut child = command::attach(&argv, Some(&cwd), &env, stderr_log, STDERR_CAP)
        .map_err(|e| e.to_string())?;
    let (Some(stdin), Some(stdout)) = (child.take_stdin(), child.take_stdout()) else {
        child.close(CLOSE_WAIT, KILL_GRACE).await;
        return Err("OMP's pipes are not available".into());
    };
    let result = timeout(EXCHANGE_TIMEOUT, async {
        let rpc = Rpc::connect(stdout, stdin, READY_TIMEOUT, None)
            .await
            .map_err(|e| e.to_string())?;
        let outcome = steps(&rpc, report).await;
        rpc.end_input().await;
        crate::log::debug(
            "omp probe exchange done",
            &[("events_dropped", &rpc.dropped_events())],
        );
        outcome
    })
    .await
    .unwrap_or_else(|_| Err("the rpc exchange took longer than 20 s".into()));
    let (exit, _) = child.close(CLOSE_WAIT, KILL_GRACE).await;
    if result.is_ok() && exit != command::Exit::Code(0) {
        return Err(format!(
            "OMP did not exit cleanly after end of input ({exit:?})"
        ));
    }
    result
}

async fn steps(rpc: &Rpc, report: &mut Report) -> Result<(), String> {
    let ready = rpc.ready();
    report.offered.clone_from(&ready.supported);
    report.max_frame_bytes = ready.max_frame_bytes;
    report.max_reassembled_frame_bytes = ready.max_reassembled_frame_bytes;
    report.protocol = Some(rpc.protocol());
    if rpc.protocol() == 2 {
        report.features.push("v2_chunks".into());
    }
    if ready.bash_exclude_from_context {
        report.features.push("bash_exclude_from_context".into());
    }
    let call = |kind: &'static str, fields: Vec<(&'static str, Value)>| async move {
        rpc.call(rpc::command(kind, &fields), RESPONSE_TIMEOUT)
            .await
            .map_err(|e: RpcError| format!("{kind}: {e}"))
    };
    let state = call("get_state", vec![]).await?;
    let data = state.data.unwrap_or(Value::Null);
    if !state.success
        || data
            .get("sessionId")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        return Err("get_state did not report a session ID".into());
    }
    if data.get("sessionFile").is_some_and(|f| !f.is_null()) {
        return Err("OMP opened a transcript in a probe with no session".into());
    }
    if call("goal", vec![("op", json!("get"))]).await?.success {
        report.features.push("goals".into());
    }
    if call("set_ask_dialog", vec![("enabled", json!(true))])
        .await?
        .success
    {
        report.features.push("ask_dialog".into());
    }
    let lease = call(
        "get_session_lease",
        vec![("sessionIds", json!([PROBE_SESSION]))],
    )
    .await?;
    if lease.success
        && lease
            .data
            .as_ref()
            .is_some_and(|d| d.get("held").is_some_and(Value::is_array))
    {
        report.features.push("lease_rpc".into());
    }
    let tools = call("set_host_tools", vec![("tools", json!([]))]).await?;
    if tools.success {
        let registered = tools
            .data
            .as_ref()
            .and_then(|d| d.get("toolNames"))
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if registered > 0 {
            return Err("OMP registered host tools for an empty list".into());
        }
        report.features.push("host_tools".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_need_a_good_probe() {
        let mut r = Report::new(Some(Path::new("/x/omp")));
        assert!(r.sessions().unwrap_err().contains("not been probed"));
        r.version = Some("19.0.0".into());
        let refused = r.sessions().unwrap_err();
        assert!(
            refused.contains("19.0.0") && refused.contains("19.1.1"),
            "{refused}"
        );
        assert!(r.summary().contains("below the minimum 19.1.1"));
        r.meets_minimum = true;
        r.version = Some("20.0.0".into());
        r.untested = true;
        assert!(r.sessions().is_ok());
        assert!(r.summary().contains("untested above 19.1.1"));
        r.error = Some("x".into());
        assert!(r.sessions().is_err());
    }

    #[tokio::test]
    async fn missing_executable_is_reported() {
        let r = probe(None, None).await;
        assert!(r.error.unwrap().contains("not found"));
        let r = probe(Some(Path::new("/nonexistent/omp")), None).await;
        assert!(r.version.is_none() && r.error.is_some());
    }
}
