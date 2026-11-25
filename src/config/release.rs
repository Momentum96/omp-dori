//! `[update]` (IR-21, IR-22: the daily release notice) and `[usage]` (COORD-50: opt-in usage counts
//! with no default collector).

use std::path::PathBuf;
use std::time::Duration;

use super::{ConfigError, Section};
use crate::relay::http::Base;

/// `[update]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpdateSettings {
    /// The daily check for a newer stable release; off means no request is ever made (IR req. 38).
    pub(crate) check: bool,
    /// Loopback-only test override of GitHub's API.
    pub(crate) api_base: Option<Base>,
    /// The first check after the hub starts (IR req. 36: 10 minutes).
    pub(crate) first_after: Duration,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            check: true,
            api_base: None,
            first_after: Duration::from_mins(10),
        }
    }
}

impl UpdateSettings {
    /// Where release metadata comes from.
    pub(crate) fn api(&self) -> Result<Base, String> {
        match &self.api_base {
            Some(b) => Ok(b.clone()),
            None => Base::parse(crate::sys::release::GITHUB_API),
        }
    }
}

/// `[usage]`: off unless the owner opts in and names a collector (COORD req. 69, DESIGN 12.2 A26).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UsageSettings {
    pub(crate) enabled: bool,
    /// The OTLP/HTTP logs endpoint, such as `https://collector.example/v1/logs`.
    pub(crate) endpoint: Option<Base>,
    /// An owner-only file holding the collector's key, sent as a bearer token.
    pub(crate) key_file: Option<PathBuf>,
}

pub(super) fn parse(
    errors: &mut Vec<ConfigError>,
    root: &mut Section<'_>,
    update: &mut UpdateSettings,
    usage: &mut UsageSettings,
) {
    if let Some(mut u) = root.table(errors, "update") {
        update.check = u.bool(errors, "check").unwrap_or(true);
        if let Some(base) = u.string(errors, "api_base") {
            match Base::parse(base) {
                Ok(b) if b.is_loopback() => update.api_base = Some(b),
                Ok(_) => {
                    errors.push(u.error("api_base", "the test override must point at localhost"));
                }
                Err(e) => errors.push(u.error("api_base", &e)),
            }
        }
        if let Some(d) = u.duration(
            errors,
            "first_after",
            Duration::from_secs(1),
            Duration::from_hours(24),
        ) {
            update.first_after = d;
        }
        u.finish(errors);
    }
    if let Some(mut u) = root.table(errors, "usage") {
        usage.enabled = u.bool(errors, "enabled").unwrap_or(false);
        if let Some(url) = u.string(errors, "endpoint") {
            match Base::parse(url) {
                Ok(b) => usage.endpoint = Some(b),
                Err(e) => errors.push(u.error("endpoint", &e)),
            }
        }
        if let Some(file) = u.string(errors, "key_file") {
            if std::path::Path::new(file).is_absolute() {
                usage.key_file = Some(PathBuf::from(file));
            } else {
                errors.push(u.error("key_file", "must be an absolute path"));
            }
        }
        if usage.enabled && usage.endpoint.is_none() {
            errors.push(u.error(
                "endpoint",
                "usage counts have no default collector: name one, or leave them off",
            ));
        }
        u.finish(errors);
    }
}

#[cfg(test)]
mod tests {
    use crate::config;

    #[test]
    fn update_and_usage_settings() {
        let c = config::parse("").unwrap();
        assert!(c.update.check && !c.usage.enabled);
        assert_eq!(c.update.first_after.as_secs(), 600);
        let c = config::parse(
            "[update]\ncheck = false\nfirst_after = \"2s\"\napi_base = \"http://127.0.0.1:9\"\n\
             [usage]\nenabled = true\nendpoint = \"https://otel.example/v1/logs\"\n",
        )
        .unwrap();
        assert!(!c.update.check && c.usage.enabled);
        let errors = config::parse("[usage]\nenabled = true\n").unwrap_err();
        assert!(
            errors.iter().any(|e| e.path == "usage.endpoint"),
            "{errors:?}"
        );
        assert!(config::parse("[update]\napi_base = \"https://api.github.com\"\n").is_err());
    }
}
