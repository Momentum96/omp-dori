//! The quota watch (OJ-07, OJ-09; DESIGN 9.2). Each limit, identified by provider, account ordinal
//! and limit ID, has a band: the highest alert level its used share reaches. A rise warns once; a
//! fall from a warned band to below every level sends one recovery note. Unavailable usage data keeps
//! the last bands and counts as a soft failure of the tick.

use super::Tick;
use crate::config::owner::Quota;
use crate::jobs::event::{Emit, EventKind, Severity};
use crate::omp::usage::Limit;

/// The band of a used share: the highest level reached, 0 below all.
fn band(percent: f64, levels: &[u8]) -> u8 {
    levels
        .iter()
        .copied()
        .filter(|l| percent >= f64::from(*l))
        .max()
        .unwrap_or(0)
}

fn duration_text(ms: i64) -> String {
    if ms <= 0 {
        return "now".into();
    }
    let minutes = ms / 60_000;
    match (minutes / 1440, minutes % 1440 / 60, minutes % 60) {
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h{m}m"),
        (d, h, _) => format!("{d}d{h}h"),
    }
}

pub(super) async fn tick(t: &mut Tick<'_>, q: &Quota) -> Result<(), String> {
    // A tick at hub start can run before the first probe resolved the executable; the tick's own
    // deadline bounds this wait.
    t.ctx.omp.first_probe().await;
    let omp = t.ctx.omp.path();
    let read = t
        .ctx
        .omp
        .usage
        .read(omp.as_deref(), t.ctx.config().limits.cache, true)
        .await;
    match (read.error, read.snapshot) {
        (None, Some(snapshot)) => {
            for l in snapshot
                .usage
                .limits
                .iter()
                .filter(|l| q.providers.is_empty() || q.providers.contains(&l.provider))
            {
                bands(t, q, l);
            }
        }
        // Unavailable data keeps the last bands.
        (error, _) => t.soft_failure = Some(error.unwrap_or_else(|| "no usage data".into())),
    }
    Ok(())
}

fn bands(t: &mut Tick<'_>, q: &Quota, l: &Limit) {
    let Some(percent) = l.used_percent else {
        return;
    };
    let key = l.key();
    let now_band = band(percent, &q.levels);
    let before: u8 = t
        .memory
        .get(&key, "band")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    t.memory.transition(&key, "band", &now_band.to_string());
    if t.first {
        return;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let shown = percent.round().clamp(0.0, 1000.0) as u32;
    if now_band > before {
        let reset_in = l
            .reset_ms
            .map_or_else(|| "unknown".to_string(), |r| duration_text(r - t.now));
        let window = l.window_ms.map_or_else(|| l.label.clone(), duration_text);
        t.emit(
            Emit::new(EventKind::QuotaLevel, Severity::Warning, true, &key)
                .with("limit", &l.id)
                .with("provider", &l.provider)
                .with("account", l.account)
                .with("percent", shown)
                .with("window", window)
                .with("reset_in", reset_in),
        );
    } else if before > 0 && now_band == 0 {
        t.emit(
            Emit::new(EventKind::QuotaRecovered, Severity::Info, false, &key)
                .with("limit", &l.id)
                .with("provider", &l.provider)
                .with("account", l.account)
                .with("percent", shown),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_and_durations() {
        assert_eq!(band(69.9, &[70, 85, 95]), 0);
        assert_eq!(band(86.0, &[70, 85, 95]), 85);
        assert_eq!(band(100.0, &[70, 85, 95]), 95);
        assert_eq!(duration_text(90 * 60_000), "1h30m");
        assert_eq!(duration_text(0), "now");
    }
}
