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
/// Seconds per day as an exact `f64`, used to convert a duration to days without an `i64 -> f64`
/// cast (`86_400` is far below 2^53, so the value is represented exactly).
const SECS_PER_DAY_F64: f64 = 86_400.0;

/// Parses a friendly/ISO duration into a fixed [`SignedDuration`] (days = 24h, weeks = 7d).
///
/// Accepts the formats understood by [`jiff::Span`], such as `"7d"`, `"2 weeks"`, `"36h"`, and
/// ISO-8601 `"P7D"`. Calendar units (years, months) are rejected because they have no fixed length.
///
/// # Errors
///
/// Returns [`CoreError::Config`] when `s` cannot be parsed as a [`jiff::Span`] (malformed or empty
/// input), when it uses years or months (which have no fixed length), or when it resolves to a
/// negative span (a cooldown window must be non-negative).
///
/// # Examples
///
/// ```
/// use cooldown_core::duration::parse_duration;
/// use jiff::SignedDuration;
///
/// assert_eq!(parse_duration("7d").unwrap(), SignedDuration::from_hours(24 * 7));
/// assert_eq!(parse_duration("36h").unwrap(), SignedDuration::from_hours(36));
/// assert_eq!(parse_duration("P7D").unwrap(), SignedDuration::from_hours(24 * 7));
/// assert!(parse_duration("1mo").is_err());
/// ```
pub fn parse_duration(s: &str) -> Result<SignedDuration, CoreError> {
    let trimmed = s.trim();
    let span = Span::from_str(trimmed)
        .map_err(|e| CoreError::Config(format!("invalid duration {trimmed:?}: {e}")))?;
    if span.get_years() != 0 || span.get_months() != 0 {
        return Err(CoreError::Config(format!(
            "duration {trimmed:?} uses years/months, which have no fixed length; use days, weeks, hours, minutes, or seconds"
        )));
    }
    let secs = i64::from(span.get_weeks()) * 7 * SECS_PER_DAY
        + i64::from(span.get_days()) * SECS_PER_DAY
        + i64::from(span.get_hours()) * 3_600
        + span.get_minutes() * 60
        + span.get_seconds();
    if secs < 0 {
        return Err(CoreError::Config(format!(
            "duration {trimmed:?} is negative; a window must be non-negative"
        )));
    }
    Ok(SignedDuration::from_secs(secs))
}

/// Parses a freeze cutoff: an RFC3339 instant, or a bare civil date (`2026-06-01`) anchored at
/// 00:00:00 UTC.
///
/// An RFC3339 timestamp is used verbatim; a bare `YYYY-MM-DD` date is anchored at midnight UTC.
///
/// # Errors
///
/// Returns [`CoreError::Config`] when `s` is neither a valid RFC3339 instant nor a valid
/// `YYYY-MM-DD` civil date (malformed or empty input), or when the parsed date cannot be resolved
/// to a UTC instant.
///
/// # Examples
///
/// ```
/// use cooldown_core::duration::parse_freeze;
///
/// // A bare date is anchored at midnight UTC.
/// let date = parse_freeze("2026-06-01").unwrap();
/// assert_eq!(date.to_string(), "2026-06-01T00:00:00Z");
///
/// // An RFC3339 instant is preserved.
/// let instant = parse_freeze("2026-06-01T12:30:00Z").unwrap();
/// assert_eq!(instant.to_string(), "2026-06-01T12:30:00Z");
///
/// assert!(parse_freeze("not-a-date").is_err());
/// ```
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

/// Renders a [`SignedDuration`] as a float number of days for display (the JSON `minAgeDays`).
///
/// # Examples
///
/// ```
/// use cooldown_core::duration::duration_as_days;
/// use jiff::SignedDuration;
///
/// assert!((duration_as_days(SignedDuration::from_hours(36)) - 1.5).abs() < 1e-9);
/// ```
#[must_use]
pub fn duration_as_days(d: SignedDuration) -> f64 {
    d.as_secs_f64() / SECS_PER_DAY_F64
}

/// The signed gap `a - b` as a fixed [`SignedDuration`]. (`Timestamp - Timestamp` yields a
/// calendar `Span` in jiff; we want a flat instant difference for the no-tolerance comparison.)
#[must_use]
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
        assert!((duration_as_days(SignedDuration::from_hours(36)) - 1.5).abs() < 1e-9);
    }
}
