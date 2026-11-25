//! Parsers for `omp usage --json` and `omp session lease --json` (DESIGN 3.3), the one
//! account-limits reader with its cache (WS req. 37-39, OJ req. 20), and the read of OMP's default
//! model (`omp config get modelRoles --json`) that setup offers for the lead.
//!
//! Verified against omp 19.1.1 (an isolated profile with no accounts, and an extension usage provider):
//! top level `generatedAt`, `reports`, `accountsWithoutUsage`, `disabledCredentials`, `capacity`;
//! per report `provider`, `fetchedAt`, `limits`; per limit `id`, `label`, `scope.provider`,
//! `amount.used`, `amount.limit`, `amount.unit`. Not verified: where a limit carries its reset time
//! and window length. Those are read from the candidate names below and treated as unknown when
//! absent; an unknown shape is "unavailable", never zero use.

use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde_json::Value;

use crate::sys::command::{self, KILL_GRACE, Output, Spec};
use crate::sys::now_ms;

/// `omp usage --json`: 15 s, 1 MiB (DESIGN 9.4).
const USAGE_TIMEOUT: Duration = Duration::from_secs(15);
/// `omp session lease --json`: 10 s, 1 MiB.
const LEASE_TIMEOUT: Duration = Duration::from_secs(10);
const OUTPUT_CAP: u64 = 1024 * 1024;
/// `omp config get modelRoles --json`: 10 s.
const CONFIG_TIMEOUT: Duration = Duration::from_secs(10);

/// Provider names shown as they are; anything else shows as "unknown" (WS req. 37).
const KNOWN_PROVIDERS: [&str; 12] = [
    "anthropic",
    "openai",
    "openai-codex",
    "google",
    "google-gemini-cli",
    "google-antigravity",
    "github-copilot",
    "xai",
    "zai",
    "kimi-code",
    "minimax",
    "cursor",
];

/// One usage limit of one account.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Limit {
    /// Provider ID as OMP reports it.
    pub(crate) provider: String,
    /// 1-based ordinal of the account among this provider's reports; never the account's name.
    pub(crate) account: usize,
    /// The limit's own identity.
    pub(crate) id: String,
    pub(crate) label: String,
    /// Used share in per cent, when the amount allows computing it.
    pub(crate) used_percent: Option<f64>,
    /// Reset instant, epoch ms (unverified field names).
    pub(crate) reset_ms: Option<i64>,
    /// Window length in ms (unverified field names; falls back to the label, like "5h" or "7d").
    pub(crate) window_ms: Option<i64>,
    pub(crate) fetched_ms: Option<i64>,
}

impl Limit {
    /// Stable identity: provider, account ordinal and limit ID (OJ req. 20).
    pub(crate) fn key(&self) -> String {
        format!("{}#{}/{}", self.provider, self.account, self.id)
    }

    pub(crate) fn provider_shown(&self) -> &str {
        if KNOWN_PROVIDERS.contains(&self.provider.as_str()) {
            &self.provider
        } else {
            "unknown"
        }
    }
}

/// A parsed usage answer.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Usage {
    pub(crate) generated_ms: Option<i64>,
    pub(crate) limits: Vec<Limit>,
    /// Accounts OMP lists without usage data (count only; names are never kept).
    pub(crate) accounts_without_data: usize,
    pub(crate) disabled_credentials: usize,
}

fn instant_ms(v: &Value) -> Option<i64> {
    if let Some(n) = v.as_f64().filter(|n| n.is_finite() && *n > 0.0) {
        // Values below 10^11 are seconds (until the year 5138).
        #[allow(clippy::cast_possible_truncation)]
        let ms = if n < 1e11 { n * 1000.0 } else { n } as i64;
        return Some(ms);
    }
    v.as_str()
        .and_then(|s| s.parse::<jiff::Timestamp>().ok())
        .map(jiff::Timestamp::as_millisecond)
}

fn first<'a>(v: &'a Value, paths: &[&[&str]]) -> Option<&'a Value> {
    paths.iter().find_map(|path| {
        path.iter()
            .try_fold(v, |at, key| at.get(key))
            .filter(|found| !found.is_null())
    })
}

/// Window length from a label like `5h`, `7d`, `weekly` or `daily`.
fn label_window_ms(label: &str) -> Option<i64> {
    let l = label.trim().to_ascii_lowercase();
    match l.as_str() {
        "weekly" | "week" => return Some(7 * 86_400_000),
        "daily" | "day" => return Some(86_400_000),
        _ => {}
    }
    crate::config::parse_duration(&l)
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .filter(|ms| *ms > 0)
}

/// Parses `omp usage --json` output.
pub(crate) fn parse_usage(bytes: &[u8]) -> Result<Usage, String> {
    let root: Value =
        serde_json::from_slice(bytes).map_err(|_| "usage output is not JSON".to_string())?;
    let reports = root
        .get("reports")
        .and_then(Value::as_array)
        .ok_or("usage output has no reports list")?;
    let count = |key: &str| root.get(key).and_then(Value::as_array).map_or(0, Vec::len);
    let mut usage = Usage {
        generated_ms: root.get("generatedAt").and_then(instant_ms),
        limits: Vec::new(),
        accounts_without_data: count("accountsWithoutUsage"),
        disabled_credentials: count("disabledCredentials"),
    };
    let mut ordinals: Vec<(String, usize)> = Vec::new();
    for report in reports {
        let provider = report
            .get("provider")
            .and_then(Value::as_str)
            .ok_or("a usage report has no provider")?
            .to_string();
        let account = if let Some((_, n)) = ordinals.iter_mut().find(|(p, _)| *p == provider) {
            *n += 1;
            *n
        } else {
            ordinals.push((provider.clone(), 1));
            1
        };
        let fetched_ms = report.get("fetchedAt").and_then(instant_ms);
        let limits = report
            .get("limits")
            .and_then(Value::as_array)
            .ok_or("a usage report has no limits list")?;
        for limit in limits {
            let label = limit
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let id = limit
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| (!label.is_empty()).then(|| label.clone()))
                .ok_or("a usage limit has neither id nor label")?;
            let used = first(limit, &[&["amount", "used"]]).and_then(Value::as_f64);
            let max = first(limit, &[&["amount", "limit"]]).and_then(Value::as_f64);
            let used_percent = match (used, max) {
                (Some(u), Some(m)) if m > 0.0 && u.is_finite() && m.is_finite() => {
                    Some(u / m * 100.0)
                }
                _ => first(limit, &[&["amount", "usedFraction"], &["usedFraction"]])
                    .and_then(Value::as_f64)
                    .filter(|f| f.is_finite())
                    .map(|f| f * 100.0),
            };
            let reset_ms = first(
                limit,
                &[
                    &["resetsAt"],
                    &["resetAt"],
                    &["window", "resetsAt"],
                    &["window", "resetAt"],
                    &["amount", "resetsAt"],
                ],
            )
            .and_then(instant_ms);
            let window_ms = first(
                limit,
                &[&["window", "durationMs"], &["windowMs"], &["durationMs"]],
            )
            .and_then(Value::as_i64)
            .filter(|ms| *ms > 0)
            .or_else(|| label_window_ms(&label));
            usage.limits.push(Limit {
                provider: provider.clone(),
                account,
                id,
                label,
                used_percent,
                reset_ms,
                window_ms,
                fetched_ms,
            });
        }
    }
    Ok(usage)
}

/// `omp session lease --json` output: sessions a running OMP owns, and those it could not probe.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Lease {
    pub(crate) held: Vec<String>,
    pub(crate) unknown: Vec<String>,
}

/// Parses `{held, unknown}` (verified against omp 19.1.1).
pub(crate) fn parse_lease(bytes: &[u8]) -> Result<Lease, String> {
    let root: Value =
        serde_json::from_slice(bytes).map_err(|_| "lease output is not JSON".to_string())?;
    let list = |key: &str| -> Result<Vec<String>, String> {
        root.get(key)
            .and_then(Value::as_array)
            .ok_or_else(|| format!("lease output has no {key} list"))?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("lease {key} entry is not a string"))
            })
            .collect()
    };
    Ok(Lease {
        held: list("held")?,
        unknown: list("unknown")?,
    })
}

/// Runs one bounded OMP read command and returns its stdout.
async fn read_command(omp: &Path, words: &[&str], limit: Duration) -> Result<Vec<u8>, String> {
    let mut argv = vec![omp.display().to_string()];
    argv.extend(words.iter().map(|a| (*a).to_string()));
    let env = super::child_env();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let spec = Spec {
        argv: &argv,
        cwd: None,
        env: &env,
        timeout: limit,
        kill_grace: KILL_GRACE,
        output_cap: OUTPUT_CAP,
        output: Output::Capture,
    };
    let done = command::run(&spec).await.map_err(|e| e.to_string())?;
    if done.timed_out {
        return Err(format!("omp {} timed out", words.join(" ")));
    }
    if done.truncated {
        return Err(format!("omp {} wrote more than 1 MiB", words.join(" ")));
    }
    if !done.success() {
        return Err(format!("omp {} failed ({:?})", words.join(" "), done.exit));
    }
    Ok(done.stdout)
}

/// `omp session lease --json -- <ids>`.
pub(crate) async fn lease(omp: &Path, ids: &[&str]) -> Result<Lease, String> {
    let mut args = vec!["session", "lease", "--json", "--"];
    args.extend_from_slice(ids);
    parse_lease(&read_command(omp, &args, LEASE_TIMEOUT).await?)
}

/// The model OMP starts with when none is named: `modelRoles.default` of `omp config get
/// modelRoles --json` (verified against omp 19.1.1: `{"key": "modelRoles", "value": {"default":
/// "provider/id:thinking", ...}}`), without its thinking suffix. `None` when OMP names none.
pub(crate) fn parse_default_model(bytes: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(bytes).ok()?;
    let model = v.get("value")?.get("default")?.as_str()?.trim();
    let model = match model.rsplit_once(':') {
        Some((m, level)) if crate::config::session::THINKING.contains(&level) => m,
        _ => model,
    };
    super::transcript::model_selector(model).then(|| model.to_string())
}

/// [`parse_default_model`] of OMP's answer; `None` when OMP cannot say.
pub(crate) async fn default_model(omp: &Path) -> Option<String> {
    let words = ["config", "get", "modelRoles", "--json"];
    let bytes = read_command(omp, &words, CONFIG_TIMEOUT).await.ok()?;
    parse_default_model(&bytes)
}

/// A usage answer with its age.
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    pub(crate) usage: Usage,
    pub(crate) fetched_ms: i64,
}

/// The one account-limits reader: runs only the read (`omp usage --json`), never another usage
/// subcommand, and keeps the last good answer for the cache period.
#[derive(Default)]
pub(crate) struct Reader {
    last: Mutex<Option<Snapshot>>,
}

/// What a read returned.
#[derive(Debug, Clone)]
pub(crate) struct Read {
    /// The newest good answer, if any.
    pub(crate) snapshot: Option<Snapshot>,
    /// Why this read could not refresh it.
    pub(crate) error: Option<String>,
    /// The answer is older than the cache period, dated in the future, or the fetch failed.
    pub(crate) stale: bool,
}

impl Reader {
    /// Returns the cached answer when younger than `cache` (unless `force`), else fetches.
    pub(crate) async fn read(&self, omp: Option<&Path>, cache: Duration, force: bool) -> Read {
        let now = now_ms();
        let cache_ms = i64::try_from(cache.as_millis()).unwrap_or(i64::MAX);
        let cached = self
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let fresh = |s: &Snapshot| s.fetched_ms <= now && now - s.fetched_ms < cache_ms;
        if !force && let Some(s) = cached.as_ref().filter(|s| fresh(s)) {
            return Read {
                snapshot: Some(s.clone()),
                error: None,
                stale: false,
            };
        }
        let fetched = match omp {
            Some(omp) => read_command(omp, &["usage", "--json"], USAGE_TIMEOUT)
                .await
                .and_then(|bytes| parse_usage(&bytes)),
            None => Err("the OMP executable was not found".to_string()),
        };
        match fetched {
            Ok(usage) => {
                let snapshot = Snapshot {
                    usage,
                    fetched_ms: now_ms(),
                };
                *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(snapshot.clone());
                Read {
                    snapshot: Some(snapshot),
                    error: None,
                    stale: false,
                }
            }
            Err(error) => Read {
                stale: true,
                snapshot: cached,
                error: Some(error),
            },
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The exact output omp 19.1.1 printed for one extension usage provider.
    pub(crate) const VERIFIED: &str = r#"{
      "generatedAt": 1795554930728,
      "reports": [{"provider": "cyfake", "fetchedAt": 1795554930696,
        "limits": [{"id": "five", "label": "5h", "scope": {"provider": "cyfake"},
                    "amount": {"used": 86, "limit": 100, "unit": "percent"}}]}],
      "accountsWithoutUsage": [], "disabledCredentials": [],
      "capacity": {"cyfake": [{"window": "5h", "accounts": 1, "usedAccounts": 0.86, "remainingAccounts": 0.14}]}
    }"#;

    #[test]
    fn parses_the_verified_shape() {
        let u = parse_usage(VERIFIED.as_bytes()).unwrap();
        assert_eq!(u.generated_ms, Some(1_795_554_930_728));
        assert_eq!(u.limits.len(), 1);
        let l = &u.limits[0];
        assert_eq!(l.key(), "cyfake#1/five");
        assert_eq!(l.used_percent, Some(86.0));
        assert_eq!(l.window_ms, Some(5 * 3_600_000));
        assert_eq!(l.reset_ms, None, "no reset field in the verified shape");
        assert_eq!(l.provider_shown(), "unknown");
        let empty = parse_usage(br#"{"generatedAt":1,"reports":[],"accountsWithoutUsage":[],"disabledCredentials":[],"capacity":{}}"#).unwrap();
        assert!(empty.limits.is_empty(), "no reports, no limits");
    }

    #[test]
    fn two_accounts_and_shared_window_names_stay_apart() {
        let text = r#"{"reports":[
          {"provider":"anthropic","limits":[{"id":"a","label":"5h","amount":{"used":10,"limit":100},"resetsAt":1700000000}]},
          {"provider":"anthropic","limits":[{"id":"b","label":"5h","amount":{"used":1,"limit":4},"window":{"resetsAt":"2026-11-25T10:00:00Z","durationMs":3600000}}]}]}"#;
        let u = parse_usage(text.as_bytes()).unwrap();
        assert_eq!(u.limits[0].key(), "anthropic#1/a");
        assert_eq!(u.limits[1].key(), "anthropic#2/b");
        assert_eq!(u.limits[0].reset_ms, Some(1_700_000_000_000));
        assert_eq!(u.limits[1].used_percent, Some(25.0));
        assert_eq!(u.limits[1].window_ms, Some(3_600_000));
        assert!(u.limits[1].reset_ms.is_some());
    }

    #[test]
    fn malformed_usage_and_lease_are_errors() {
        assert!(parse_usage(b"nope").is_err());
        assert!(parse_usage(br#"{"reports":{}}"#).is_err());
        assert!(parse_usage(br#"{"reports":[{"limits":[]}]}"#).is_err());
        let lease = parse_lease(br#"{"held":["a"],"unknown":[]}"#).unwrap();
        assert_eq!(lease.held, ["a"]);
        assert!(parse_lease(br#"{"held":[1],"unknown":[]}"#).is_err());
        assert!(parse_lease(br#"{"held":[]}"#).is_err());
    }

    #[test]
    fn the_default_model_drops_its_thinking_level() {
        let verified = br#"{"key": "modelRoles", "value": {"smol": "x/y:low",
            "default": "anthropic/claude-opus-5-5:high"}, "type": "record", "description": ""}"#;
        assert_eq!(
            parse_default_model(verified).as_deref(),
            Some("anthropic/claude-opus-5-5")
        );
        let plain = br#"{"value": {"default": "openai/gpt-5.2"}}"#;
        assert_eq!(
            parse_default_model(plain).as_deref(),
            Some("openai/gpt-5.2")
        );
        assert_eq!(parse_default_model(br#"{"value": {"smol": "x/y"}}"#), None);
        assert_eq!(
            parse_default_model(br#"{"value": {"default": "opus"}}"#),
            None
        );
        assert_eq!(parse_default_model(b"Unknown setting"), None);
    }
}
