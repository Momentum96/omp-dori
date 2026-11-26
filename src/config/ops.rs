//! `[ops]`: the status panel and alerts in the ops room (`#cataly-ops` after setup). The panel is
//! one Discord message kept up to date; alerts are separate posts for what is worth a ping.

use std::collections::BTreeMap;
use std::time::Duration;

use super::{ConfigError, Destination, Section};

/// The destination setup creates for the ops room, used when `[ops] destination` is not set.
pub(crate) const DEFAULT_DESTINATION: &str = "cataly-ops";

/// The alert kinds, as named in `[ops.alerts]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum AlertKind {
    /// The host over the alert level for at least five minutes.
    Pressure,
    /// A provider limit reaching an alert level (`quota.level`).
    Quota,
    /// A job failing twice in a row.
    Jobs,
    /// A refused backup.
    Backup,
    /// Pull requests and notifications from the `github` and `pr` jobs.
    Github,
    /// New items from an `items` job (mail watch).
    Mail,
}

impl AlertKind {
    pub(crate) const ALL: [Self; 6] = [
        Self::Pressure,
        Self::Quota,
        Self::Jobs,
        Self::Backup,
        Self::Github,
        Self::Mail,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Pressure => "pressure",
            Self::Quota => "quota",
            Self::Jobs => "jobs",
            Self::Backup => "backup",
            Self::Github => "github",
            Self::Mail => "mail",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpsSettings {
    /// The `[destinations]` name of the ops room; `None` when there is no panel.
    pub(crate) destination: Option<String>,
    /// How long the mute button silences alerts.
    pub(crate) mute: Duration,
    /// Alert kinds turned off.
    pub(crate) off: Vec<AlertKind>,
}

impl Default for OpsSettings {
    fn default() -> Self {
        Self {
            destination: None,
            mute: Duration::from_hours(1),
            off: Vec::new(),
        }
    }
}

impl OpsSettings {
    pub(crate) fn alerts(&self, kind: AlertKind) -> bool {
        !self.off.contains(&kind)
    }
}

/// Reads `[ops]` once `[destinations]` is known. Without `destination` the panel goes to the
/// `cataly-ops` destination when that is a Discord destination; `destination = ""` turns it off.
pub(super) fn parse(
    errors: &mut Vec<ConfigError>,
    root: &mut Section<'_>,
    destinations: &BTreeMap<String, Destination>,
    ops: &mut OpsSettings,
) {
    let discord = |name: &str| matches!(destinations.get(name), Some(Destination::Discord { .. }));
    ops.destination = discord(DEFAULT_DESTINATION).then(|| DEFAULT_DESTINATION.to_string());
    let Some(mut o) = root.table(errors, "ops") else {
        return;
    };
    match o.string(errors, "destination") {
        None => {}
        Some("") => ops.destination = None,
        Some(name) if discord(name) => ops.destination = Some(name.to_string()),
        Some(name) => errors.push(o.error(
            "destination",
            &format!("'{name}' is not a Discord destination in [destinations]"),
        )),
    }
    if let Some(d) = o.duration(
        errors,
        "mute",
        Duration::from_mins(1),
        Duration::from_hours(24),
    ) {
        ops.mute = d;
    }
    if let Some(mut a) = o.table(errors, "alerts") {
        for kind in AlertKind::ALL {
            if a.bool(errors, kind.name()) == Some(false) {
                ops.off.push(kind);
            }
        }
        a.finish(errors);
    }
    o.finish(errors);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    #[test]
    fn ops_settings() {
        let room =
            "[destinations.cataly-ops]\nkind = \"discord\"\nchannel = \"100000000000000012\"\n";
        let c = config::parse(room).unwrap();
        assert_eq!(c.ops.destination.as_deref(), Some("cataly-ops"));
        assert_eq!(c.ops.mute, Duration::from_hours(1));
        assert!(AlertKind::ALL.iter().all(|k| c.ops.alerts(*k)));
        assert_eq!(config::parse("").unwrap().ops.destination, None);
        let c = config::parse(&format!(
            "{room}[ops]\ndestination = \"\"\nmute = \"30m\"\n[ops.alerts]\ngithub = false\n"
        ))
        .unwrap();
        assert_eq!(c.ops.destination, None);
        assert_eq!(c.ops.mute, Duration::from_mins(30));
        assert!(!c.ops.alerts(AlertKind::Github) && c.ops.alerts(AlertKind::Mail));
        let errors = config::parse("[ops]\ndestination = \"nowhere\"\n").unwrap_err();
        assert!(errors.iter().any(|e| e.path == "ops.destination"));
        let errors = config::parse("[ops.alerts]\nweather = true\n").unwrap_err();
        assert!(errors.iter().any(|e| e.path == "ops.alerts.weather"));
    }
}
