//! The hosted OMP launch (DESIGN 4.2; HOST req. 17-22): the command line built from structured
//! config, the child environment allow-list and the settings overlay.
//!
//! The overlays (one for the lead, one for lanes) are written once per hub run, owner-only. They pin
//! `ask.timeout: 0` (questions never time out), goal mode on with continuation in rpc mode for lanes
//! and off for the lead, the saved-reset auto-redeem key to `no` (OJ-13), and OMP's own
//! model fallback on with `cooldown-expiry` revert (decision 7, OJ-10/11). `retry.fallbackOn` and
//! `retry.fallbackChains` appear only when the owner set them in Cataly's `[fallback]` section; Cataly
//! never invents a chain. JSON is valid YAML, so the overlay is written as JSON.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::config::session::FallbackSettings;
use crate::sys::fs::{FsError, write_atomic};

/// Fresh session in a known directory, or an exact resume of a session file (HOST req. 9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    Fresh(PathBuf),
    Resume(PathBuf),
}

/// One hosted OMP launch.
#[derive(Debug, Clone)]
pub(crate) struct Launch<'a> {
    pub(crate) exe: &'a Path,
    /// `provider/id`.
    pub(crate) model: &'a str,
    pub(crate) thinking: Option<&'a str>,
    pub(crate) target: &'a Target,
    pub(crate) overlay: &'a Path,
    /// Role instructions file (`--append-system-prompt`).
    pub(crate) instructions: Option<&'a Path>,
    /// Owner-approved extra flags, already validated by the config.
    pub(crate) extra: &'a [String],
    /// `--max-cost` (a lane's cost ceiling, WS-18).
    pub(crate) max_cost: Option<f64>,
    /// `--tools` (a look-only lane, OPC-46).
    pub(crate) tools: Option<&'a str>,
}

impl Launch<'_> {
    /// The argument vector: the executable, the owner's extra flags, then the flags the hub always
    /// sets, so a later hub flag wins over anything before it.
    pub(crate) fn argv(&self) -> Vec<String> {
        self.build(true)
    }

    /// The terminal runtime's argument vector: the same flags without the rpc-ui mode and the
    /// approval mode, so OMP runs its TUI with the owner's own approvals (HOST req. 93).
    pub(crate) fn tui_argv(&self) -> Vec<String> {
        self.build(false)
    }

    fn build(&self, rpc: bool) -> Vec<String> {
        let mut argv = vec![self.exe.display().to_string()];
        argv.extend(self.extra.iter().cloned());
        if rpc {
            argv.extend(["--mode", "rpc-ui", "--approval-mode", "always-ask"].map(String::from));
        }
        argv.extend([
            format!("--config={}", self.overlay.display()),
            format!("--model={}", self.model),
        ]);
        if let Some(level) = self.thinking {
            argv.push(format!("--thinking={level}"));
        }
        match self.target {
            Target::Fresh(dir) => argv.push(format!("--session-dir={}", dir.display())),
            Target::Resume(file) => argv.push(format!("--resume={}", file.display())),
        }
        if let Some(file) = self.instructions {
            argv.push(format!("--append-system-prompt={}", file.display()));
        }
        if let Some(cost) = self.max_cost {
            argv.push(format!("--max-cost={cost}"));
        }
        if let Some(tools) = self.tools {
            argv.push(format!("--tools={tools}"));
        }
        argv
    }
}

/// The child environment beyond the runner's pass-through list (HOST req. 21-22): the probe's
/// variables (notifications off, profile overrides), the owner's listed provider credentials that the
/// hub itself has, the hub socket and the single-use launch token. Never a relay token or a Cataly
/// secret; nothing secret goes into argv.
pub(crate) fn session_env(
    credentials: &[String],
    socket: &Path,
    token: &str,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = super::child_env()
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    for name in credentials {
        if let Ok(value) = std::env::var(name) {
            env.push((name.clone(), value));
        }
    }
    env.push(("CATALY_SOCKET".into(), socket.display().to_string()));
    env.push(("CATALY_LAUNCH".into(), token.to_string()));
    env
}

/// The overlay file of a session kind in the state directory. Lanes run OMP goals (decision 5);
/// the lead holds no standing goal (OPC-36, DESIGN 9.1), so its overlay turns goal mode off and OMP
/// itself refuses a goal and hides the goal tool there.
pub(crate) fn overlay_path(state: &Path, lane: bool) -> PathBuf {
    state.join(if lane {
        "omp-overlay-lane.yml"
    } else {
        "omp-overlay.yml"
    })
}

/// The overlay's settings for a lane (`lane`) or the lead.
pub(crate) fn overlay(fallback: &FallbackSettings, lane: bool) -> Value {
    let mut retry = Map::new();
    retry.insert("modelFallback".into(), Value::Bool(true));
    retry.insert("fallbackRevertPolicy".into(), json!("cooldown-expiry"));
    if let Some(on) = &fallback.on {
        retry.insert("fallbackOn".into(), json!(on));
    }
    if !fallback.chains.is_empty() {
        retry.insert("fallbackChains".into(), json!(fallback.chains));
    }
    json!({
        "ask": { "timeout": 0 },
        "goal": { "enabled": lane, "continuationModes": ["interactive", "rpc"] },
        "codexResets": { "autoRedeem": "no" },
        "retry": retry,
    })
}

/// Writes the overlay file of a session kind (mode 0600, atomically) and returns its path.
pub(crate) fn write_overlay(
    state: &Path,
    fallback: &FallbackSettings,
    lane: bool,
) -> Result<PathBuf, FsError> {
    let path = overlay_path(state, lane);
    let mut text = overlay(fallback, lane).to_string();
    text.push('\n');
    write_atomic(&path, text.as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_puts_hub_flags_last() {
        let target = Target::Resume(PathBuf::from("/s/a.jsonl"));
        let extra = vec!["--no-skills".to_string()];
        let launch = Launch {
            exe: Path::new("/bin/omp"),
            model: "p/m",
            thinking: Some("low"),
            target: &target,
            overlay: Path::new("/o.yml"),
            instructions: Some(Path::new("/lead.md")),
            extra: &extra,
            max_cost: None,
            tools: None,
        };
        assert_eq!(
            launch.argv(),
            [
                "/bin/omp",
                "--no-skills",
                "--mode",
                "rpc-ui",
                "--approval-mode",
                "always-ask",
                "--config=/o.yml",
                "--model=p/m",
                "--thinking=low",
                "--resume=/s/a.jsonl",
                "--append-system-prompt=/lead.md"
            ]
        );
    }

    #[test]
    fn overlay_pins_safety_and_carries_only_owner_chains() {
        let plain = overlay(&FallbackSettings::default(), false);
        assert_eq!(plain["goal"]["enabled"], false, "the lead holds no goal");
        assert_eq!(
            overlay(&FallbackSettings::default(), true)["goal"]["enabled"],
            true
        );
        assert_eq!(plain["ask"]["timeout"], 0);
        assert_eq!(plain["codexResets"]["autoRedeem"], "no");
        assert_eq!(plain["retry"]["modelFallback"], true);
        assert_eq!(plain["retry"]["fallbackRevertPolicy"], "cooldown-expiry");
        assert!(plain["retry"].get("fallbackChains").is_none());
        assert!(plain["retry"].get("fallbackOn").is_none());
        let mut chains = std::collections::BTreeMap::new();
        chains.insert("default".to_string(), vec!["p/backup".to_string()]);
        let owner = overlay(
            &FallbackSettings {
                on: Some("usage-limit".into()),
                chains,
            },
            true,
        );
        assert_eq!(owner["retry"]["fallbackChains"]["default"][0], "p/backup");
        assert_eq!(owner["retry"]["fallbackOn"], "usage-limit");
        let env = session_env(
            &["CATALY_NOT_SET_ANYWHERE".into()],
            Path::new("/s.sock"),
            "t",
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "PI_NOTIFICATIONS" && v == "off")
        );
        assert!(env.iter().any(|(k, _)| k == "CATALY_LAUNCH"));
        assert!(!env.iter().any(|(k, _)| k == "CATALY_NOT_SET_ANYWHERE"));
    }
}
