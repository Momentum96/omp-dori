//! Cron expressions with an IANA time zone (WS req. 52): five fields, names for months and weekdays,
//! lists, ranges and steps; day-of-month and day-of-week combine with OR when both are restricted. A
//! local time that does not exist (DST gap) does not fire; a repeated local time (DST fold) fires once,
//! at its first occurrence.

use jiff::Timestamp;
use jiff::civil::{Date, DateTime};
use jiff::tz::{AmbiguousOffset, TimeZone};

/// Days searched for the next occurrence; eight years always include a 29 February.
const SEARCH_DAYS: u32 = 366 * 8;

/// A parsed five-field cron expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cron {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    days_star: bool,
    weekdays_star: bool,
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

struct Field {
    name: &'static str,
    min: u32,
    max: u32,
    names: &'static [&'static str],
    names_from: u32,
}

const FIELDS: [Field; 5] = [
    Field {
        name: "minute",
        min: 0,
        max: 59,
        names: &[],
        names_from: 0,
    },
    Field {
        name: "hour",
        min: 0,
        max: 23,
        names: &[],
        names_from: 0,
    },
    Field {
        name: "day of month",
        min: 1,
        max: 31,
        names: &[],
        names_from: 0,
    },
    Field {
        name: "month",
        min: 1,
        max: 12,
        names: &MONTHS,
        names_from: 1,
    },
    // 7 is accepted as Sunday and folded onto 0 below.
    Field {
        name: "day of week",
        min: 0,
        max: 7,
        names: &WEEKDAYS,
        names_from: 0,
    },
];

/// Parses an expression; the error names the offending field.
pub(crate) fn parse(expr: &str) -> Result<Cron, String> {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    let [minute, hour, day, month, weekday] = parts.as_slice() else {
        return Err(format!(
            "expected 5 fields (minute hour day month weekday), found {}",
            parts.len()
        ));
    };
    let [f_min, f_hour, f_day, f_month, f_week] = &FIELDS;
    let weekdays_raw = parse_field(weekday, f_week)?;
    let weekdays = (weekdays_raw & 0x7f) | u64::from(weekdays_raw & (1 << 7) != 0);
    Ok(Cron {
        minutes: parse_field(minute, f_min)?,
        hours: u32::try_from(parse_field(hour, f_hour)?)
            .map_err(|_| "hour out of range".to_string())?,
        days: u32::try_from(parse_field(day, f_day)?)
            .map_err(|_| "day out of range".to_string())?,
        months: u16::try_from(parse_field(month, f_month)?)
            .map_err(|_| "month out of range".to_string())?,
        weekdays: u8::try_from(weekdays).map_err(|_| "weekday out of range".to_string())?,
        days_star: day.starts_with('*'),
        weekdays_star: weekday.starts_with('*'),
    })
}

fn parse_field(text: &str, field: &Field) -> Result<u64, String> {
    if text.contains('?') {
        return Err(format!("{}: '?' is not supported, use '*'", field.name));
    }
    let mut bits = 0u64;
    for item in text.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step
                    .parse()
                    .ok()
                    .filter(|s| *s >= 1)
                    .ok_or_else(|| format!("{}: bad step in '{item}'", field.name))?;
                (range, step)
            }
            None => (item, 1),
        };
        let (lo, hi) = if range == "*" {
            (field.min, field.max)
        } else if let Some((a, b)) = range.split_once('-') {
            (value(a, field)?, value(b, field)?)
        } else {
            let v = value(range, field)?;
            // `a/n` means from a to the end of the field.
            (v, if item.contains('/') { field.max } else { v })
        };
        if lo > hi {
            return Err(format!("{}: range '{range}' runs backwards", field.name));
        }
        let mut v = lo;
        while v <= hi {
            bits |= 1u64 << v;
            v += step;
        }
    }
    Ok(bits)
}

fn value(text: &str, field: &Field) -> Result<u32, String> {
    let lower = text.to_ascii_lowercase();
    let v = match field.names.iter().position(|n| *n == lower) {
        Some(i) => u32::try_from(i).map_err(|_| "name index".to_string())? + field.names_from,
        None => text
            .parse::<u32>()
            .map_err(|_| format!("{}: '{text}' is not a value", field.name))?,
    };
    if v < field.min || v > field.max {
        return Err(format!(
            "{}: {v} is outside {}-{}",
            field.name, field.min, field.max
        ));
    }
    Ok(v)
}

fn has(bits: u64, v: i8) -> bool {
    u32::try_from(v).is_ok_and(|v| v < 64 && bits & (1u64 << v) != 0)
}

impl Cron {
    fn day_matches(&self, date: Date) -> bool {
        if !has(u64::from(self.months), date.month()) {
            return false;
        }
        let dom = has(u64::from(self.days), date.day());
        let dow = has(
            u64::from(self.weekdays),
            date.weekday().to_sunday_zero_offset(),
        );
        match (self.days_star, self.weekdays_star) {
            (false, false) => dom || dow,
            (true, false) => dow,
            (false, true) => dom,
            (true, true) => true,
        }
    }

    /// The first occurrence strictly after `after`, in `tz`; `None` if none exists within eight years.
    pub(crate) fn next_after(&self, tz: &TimeZone, after: Timestamp) -> Option<Timestamp> {
        let start = after.to_zoned(tz.clone()).datetime();
        let mut date = start.date();
        for day in 0..SEARCH_DAYS {
            if self.day_matches(date)
                && let Some(ts) = self.first_in_day(tz, date, (day == 0).then_some(start), after)
            {
                return Some(ts);
            }
            date = date.tomorrow().ok()?;
        }
        None
    }

    fn first_in_day(
        &self,
        tz: &TimeZone,
        date: Date,
        not_before: Option<DateTime>,
        after: Timestamp,
    ) -> Option<Timestamp> {
        for hour in 0..24i8 {
            if !has(u64::from(self.hours), hour) {
                continue;
            }
            for minute in 0..60i8 {
                if !has(self.minutes, minute) {
                    continue;
                }
                let dt = date.at(hour, minute, 0, 0);
                if not_before.is_some_and(|s| dt <= s) {
                    continue;
                }
                let offset = match tz.to_ambiguous_timestamp(dt).offset() {
                    AmbiguousOffset::Unambiguous { offset } => offset,
                    AmbiguousOffset::Gap { .. } => continue,
                    AmbiguousOffset::Fold { before, .. } => before,
                };
                if let Ok(ts) = offset.to_timestamp(dt)
                    && ts > after
                {
                    return Some(ts);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn next_n(cron: &Cron, tz: &TimeZone, mut after: Timestamp, n: usize) -> Vec<String> {
        let mut out = Vec::new();
        for _ in 0..n {
            after = cron.next_after(tz, after).unwrap();
            out.push(after.to_string());
        }
        out
    }

    #[test]
    fn parses_and_refuses() {
        assert!(parse("*/15 9-17 * * mon-fri").is_ok());
        assert!(parse("0 0 1 JAN,jul 7").is_ok());
        assert!(parse("5/10 * * * *").is_ok());
        assert!(parse("* * ? * *").unwrap_err().contains("'?'"));
        assert!(parse("60 * * * *").unwrap_err().contains("minute"));
        assert!(parse("* * * *").unwrap_err().contains("5 fields"));
        assert!(parse("* * * * 1-0").is_err());
        assert!(parse("*/0 * * * *").is_err());
        assert!(parse("@daily").is_err());
    }

    #[test]
    fn utc_steps_and_weekday_seven() {
        let utc = TimeZone::UTC;
        let c = parse("*/20 10 * * *").unwrap();
        assert_eq!(
            next_n(&c, &utc, ts("2026-01-01T10:05:00Z"), 3),
            [
                "2026-01-01T10:20:00Z",
                "2026-01-01T10:40:00Z",
                "2026-01-02T10:00:00Z"
            ]
        );
        let sunday = parse("0 12 * * 7").unwrap();
        assert_eq!(
            next_n(&sunday, &utc, ts("2026-11-25T00:00:00Z"), 1),
            ["2026-11-29T12:00:00Z"]
        );
    }

    #[test]
    fn day_of_month_or_day_of_week() {
        // The 13th or any Friday.
        let c = parse("0 0 13 * fri").unwrap();
        let got = next_n(&c, &TimeZone::UTC, ts("2026-11-01T00:00:00Z"), 4);
        assert_eq!(
            got,
            [
                "2026-11-06T00:00:00Z",
                "2026-11-13T00:00:00Z",
                "2026-11-20T00:00:00Z",
                "2026-11-27T00:00:00Z"
            ]
        );
        // With a star day-of-month only the weekday decides.
        let c = parse("0 0 * * fri").unwrap();
        assert_eq!(
            next_n(&c, &TimeZone::UTC, ts("2026-11-12T00:00:00Z"), 1),
            ["2026-11-13T00:00:00Z"]
        );
    }

    #[test]
    fn leap_day_is_found() {
        let c = parse("0 0 29 2 *").unwrap();
        assert_eq!(
            next_n(&c, &TimeZone::UTC, ts("2026-03-01T00:00:00Z"), 1),
            ["2028-02-29T00:00:00Z"]
        );
        let never = parse("0 0 31 2 *").unwrap();
        assert!(
            never
                .next_after(&TimeZone::UTC, ts("2026-01-01T00:00:00Z"))
                .is_none()
        );
    }

    #[test]
    fn dst_gap_does_not_fire() {
        // Berlin skips 02:00-03:00 on 2026-03-29.
        let tz = TimeZone::get("Europe/Berlin").unwrap();
        let c = parse("30 2 * * *").unwrap();
        let got = next_n(&c, &tz, ts("2026-03-28T12:00:00Z"), 2);
        assert_eq!(got, ["2026-03-30T00:30:00Z", "2026-03-31T00:30:00Z"]);
    }

    #[test]
    fn dst_repeat_fires_once() {
        // Berlin repeats 02:00-03:00 on 2026-10-25 (CEST 00:00-01:00 UTC, then CET 01:00-02:00 UTC).
        let tz = TimeZone::get("Europe/Berlin").unwrap();
        let c = parse("30 2 * * *").unwrap();
        let got = next_n(&c, &tz, ts("2026-10-24T12:00:00Z"), 2);
        assert_eq!(got, ["2026-10-25T00:30:00Z", "2026-10-26T01:30:00Z"]);
        // Starting inside the second pass does not fire the repeated time again.
        assert_eq!(
            next_n(&c, &tz, ts("2026-10-25T01:10:00Z"), 1),
            ["2026-10-26T01:30:00Z"]
        );
        // Each repeated local minute fires once, in its first pass, so the next minute after 02:59 CEST
        // is 03:00 CET.
        let every = parse("* * * * *").unwrap();
        assert_eq!(
            next_n(&every, &tz, ts("2026-10-25T00:59:00Z"), 1),
            ["2026-10-25T02:00:00Z"]
        );
    }
}
