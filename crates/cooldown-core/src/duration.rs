//! Duration and freeze-date parsing.
//!
//! Windows normalise to a fixed [`SignedDuration`] with **days = 24h** (and weeks = 7d) so the
//! cooldown boundary stays a pure UTC-instant comparison with no tolerance, matching the no-clock-
//! skew rule. `jiff::SignedDuration` itself refuses calendar units, so we parse into a `jiff::Span`
//! (which accepts `"7d"`, `"2 weeks"`, `"36h"`, ISO-8601 `"P7D"`) and fold it down field-by-field.
//! Years and months are rejected as ambiguous — there is no fixed number of days in them.

use crate::error::CoreError;
use jiff::{SignedDuration, Span, Timestamp, civil, tz::TimeZone};
use std::str::FromStr;

const SECS_PER_DAY: i64 = 86_400;

/// Parse a friendly/ISO duration into a fixed [`SignedDuration`] (days = 24h, weeks = 7d).
pub fn parse_duration(s: &str) -> Result<SignedDuration, CoreError> {
    let trimmed = s.trim();
    let span = Span::from_str(trimmed)
        .map_err(|e| CoreError::Config(format!("invalid duration {trimmed:?}: {e}")))?;
    if span.get_years() != 0 || span.get_months() != 0 {
        return Err(CoreError::Config(format!(
            "duration {trimmed:?} uses years/months, which have no fixed length; use days, weeks, hours, minutes, or seconds"
        )));
    }
    let secs = span.get_weeks() as i64 * 7 * SECS_PER_DAY
        + span.get_days() as i64 * SECS_PER_DAY
        + span.get_hours() as i64 * 3_600
        + span.get_minutes() * 60
        + span.get_seconds();
    if secs < 0 {
        return Err(CoreError::Config(format!(
            "duration {trimmed:?} is negative; a window must be non-negative"
        )));
    }
    Ok(SignedDuration::from_secs(secs))
}

/// Parse a freeze cutoff: an RFC3339 instant, or a bare civil date (`2026-06-01`) anchored at
/// 00:00:00 UTC.
pub fn parse_freeze(s: &str) -> Result<Timestamp, CoreError> {
    let trimmed = s.trim();
    if let Ok(ts) = Timestamp::from_str(trimmed) {
        return Ok(ts);
    }
    let date = civil::Date::from_str(trimmed).map_err(|e| {
        CoreError::Config(format!(
            "invalid freeze cutoff {trimmed:?}: expected an RFC3339 instant or a YYYY-MM-DD date ({e})"
        ))
    })?;
    date.at(0, 0, 0, 0)
        .to_zoned(TimeZone::UTC)
        .map(|z| z.timestamp())
        .map_err(|e| CoreError::Config(format!("invalid freeze date {trimmed:?}: {e}")))
}

/// Render a [`SignedDuration`] as a float number of days for display (the JSON `minAgeDays`).
pub fn duration_as_days(d: SignedDuration) -> f64 {
    d.as_secs_f64() / SECS_PER_DAY as f64
}

/// The signed gap `a - b` as a fixed [`SignedDuration`]. (`Timestamp - Timestamp` yields a
/// calendar `Span` in jiff; we want a flat instant difference for the no-tolerance comparison.)
pub fn since(a: Timestamp, b: Timestamp) -> SignedDuration {
    let secs = a.as_second() - b.as_second();
    let nanos = a.subsec_nanosecond() - b.subsec_nanosecond();
    SignedDuration::new(secs, nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_friendly_and_iso() {
        assert_eq!(
            parse_duration("7d").unwrap(),
            SignedDuration::from_hours(24 * 7)
        );
        assert_eq!(
            parse_duration("2 weeks").unwrap(),
            SignedDuration::from_hours(24 * 14)
        );
        assert_eq!(
            parse_duration("36h").unwrap(),
            SignedDuration::from_hours(36)
        );
        assert_eq!(
            parse_duration("P7D").unwrap(),
            SignedDuration::from_hours(24 * 7)
        );
        assert_eq!(parse_duration("0d").unwrap(), SignedDuration::ZERO);
    }

    #[test]
    fn rejects_calendar_ambiguity() {
        assert!(parse_duration("1mo").is_err());
        assert!(parse_duration("P1Y").is_err());
    }

    #[test]
    fn freeze_accepts_date_and_instant() {
        let d = parse_freeze("2026-06-01").unwrap();
        assert_eq!(d.to_string(), "2026-06-01T00:00:00Z");
        let i = parse_freeze("2026-06-01T12:30:00Z").unwrap();
        assert_eq!(i.to_string(), "2026-06-01T12:30:00Z");
    }

    #[test]
    fn days_display() {
        assert_eq!(duration_as_days(SignedDuration::from_hours(36)), 1.5);
    }
}
