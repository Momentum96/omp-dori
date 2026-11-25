//! The usage ledger and caps (VOICE-19, VOICE req. 81-82). The host stores one row per day in the
//! configured zone; this module holds the arithmetic and the cap rule, so both sides agree.

use std::time::Duration;

use crate::settings::Prices;

/// One day's usage.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Day {
    /// `YYYY-MM-DD` in the configured zone.
    pub day: String,
    /// Billed speech seconds.
    pub speech_seconds: f64,
    /// Sessions started.
    pub sessions: u32,
    /// Estimated backend and summary spending in USD.
    pub spend_usd: f64,
}

impl Day {
    /// A stored row is sound only with finite, non-negative values. Anything else is corruption:
    /// voice stops and the row is never rewritten (VOICE req. 82).
    #[must_use]
    pub fn sound(&self) -> bool {
        self.speech_seconds.is_finite()
            && self.speech_seconds >= 0.0
            && self.spend_usd.is_finite()
            && self.spend_usd >= 0.0
    }
}

/// The three caps.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Caps {
    /// Speech per day.
    pub daily: Duration,
    /// Zero means off.
    pub total: Duration,
    /// Estimated spending per day in USD.
    pub spending_usd: f64,
}

/// Which cap is reached, if any. `total_seconds` is the sum over every stored day.
#[must_use]
pub fn reached(today: &Day, total_seconds: f64, caps: &Caps) -> Option<&'static str> {
    if today.speech_seconds >= caps.daily.as_secs_f64() {
        return Some("daily speech cap");
    }
    if !caps.total.is_zero() && total_seconds >= caps.total.as_secs_f64() {
        return Some("total speech cap");
    }
    if today.spend_usd >= caps.spending_usd {
        return Some("spending cap");
    }
    None
}

/// Token counts of one backend or summary response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Spend {
    /// All input tokens.
    pub input: u64,
    /// Input tokens read from the cache.
    pub cached: u64,
    /// Input tokens written to the cache.
    pub written: u64,
    /// Output tokens.
    pub output: u64,
}

/// Cached plus written tokens above the input count: the usage report is not trustworthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidUsage;

impl Spend {
    /// Reads a Responses `usage` object; missing counts are zero.
    #[must_use]
    pub fn from_usage(usage: &serde_json::Value) -> Self {
        let n = |v: Option<&serde_json::Value>| v.and_then(serde_json::Value::as_u64).unwrap_or(0);
        let details = usage.get("input_tokens_details");
        Self {
            input: n(usage.get("input_tokens")),
            cached: n(details.and_then(|d| d.get("cached_tokens"))),
            written: n(details.and_then(|d| {
                d.get("cache_write_tokens")
                    .or_else(|| d.get("cache_creation_tokens"))
            })),
            output: n(usage.get("output_tokens")),
        }
    }

    /// USD: (uncached input x input + cached x cached + written x write + output x output) / 1e6.
    ///
    /// # Errors
    /// Cached plus written tokens exceed the input count.
    pub fn cost(&self, p: &Prices) -> Result<f64, InvalidUsage> {
        let reused = self.cached.checked_add(self.written).ok_or(InvalidUsage)?;
        if reused > self.input {
            return Err(InvalidUsage);
        }
        #[allow(clippy::cast_precision_loss)]
        let f = |n: u64| n as f64;
        Ok((f(self.input - reused) * p.input
            + f(self.cached) * p.cached_input
            + f(self.written) * p.cache_write
            + f(self.output) * p.output)
            / 1_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spending_formula_and_invalid_usage() {
        let p = Prices {
            input: 1.0,
            cached_input: 0.1,
            cache_write: 1.25,
            output: 4.0,
        };
        let s = Spend {
            input: 1_000_000,
            cached: 200_000,
            written: 100_000,
            output: 500_000,
        };
        let c = s.cost(&p).unwrap();
        assert!((c - (0.7 + 0.02 + 0.125 + 2.0)).abs() < 1e-9);
        let bad = Spend {
            input: 10,
            cached: 8,
            written: 3,
            output: 0,
        };
        assert_eq!(bad.cost(&p), Err(InvalidUsage));
        let u = serde_json::json!({"input_tokens": 10, "input_tokens_details": {"cached_tokens": 4},
            "output_tokens": 7});
        assert_eq!(
            Spend::from_usage(&u),
            Spend {
                input: 10,
                cached: 4,
                written: 0,
                output: 7
            }
        );
    }

    #[test]
    fn caps() {
        let caps = Caps {
            daily: Duration::from_mins(30),
            total: Duration::ZERO,
            spending_usd: 3.0,
        };
        let mut d = Day::default();
        assert_eq!(reached(&d, 1e9, &caps), None);
        d.spend_usd = 3.0;
        assert_eq!(reached(&d, 0.0, &caps), Some("spending cap"));
        d.speech_seconds = 1800.0;
        assert_eq!(reached(&d, 0.0, &caps), Some("daily speech cap"));
        let caps = Caps {
            total: Duration::from_mins(10),
            ..caps
        };
        assert_eq!(
            reached(&Day::default(), 600.0, &caps),
            Some("total speech cap")
        );
        assert!(
            !Day {
                spend_usd: f64::NAN,
                ..Day::default()
            }
            .sound()
        );
    }
}
