//! Deterministic UTC calendar helpers for the sandbox `Date` value.
//!
//! Upstream uses the host `Date` implementation. The Rust port implements the
//! subset that CodeMode programs use, with a deliberate deviation: components
//! and non-UTC getters are interpreted as UTC so execution does not depend on
//! the host timezone. String coercion and boundary serialization both use the
//! ISO form, matching upstream `SandboxDate`.

/// Maximum absolute time value accepted by JavaScript `Date`.
const MAX_TIME_MS: f64 = 8.64e15;

const MS_PER_DAY: f64 = 86_400_000.0;

/// Calendar components of a valid date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateParts {
    /// Calendar year.
    pub year: i64,
    /// Month, zero-based.
    pub month: u32,
    /// Day of month, one-based.
    pub day: u32,
    /// Day of week, zero for Sunday.
    pub weekday: u32,
    /// Hours, zero-based.
    pub hours: u32,
    /// Minutes, zero-based.
    pub minutes: u32,
    /// Seconds, zero-based.
    pub seconds: u32,
    /// Milliseconds, zero-based.
    pub milliseconds: u32,
}

/// Current wall-clock time as Unix milliseconds.
pub fn now_ms() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as f64,
        Err(error) => -(error.duration().as_millis() as f64),
    }
}

/// Applies the JavaScript TimeClip algorithm: truncate toward zero and reject
/// values outside the supported range.
pub fn clip(time_ms: f64) -> f64 {
    if !time_ms.is_finite() {
        return f64::NAN;
    }
    let time_ms = time_ms.trunc();
    if time_ms.abs() > MAX_TIME_MS {
        f64::NAN
    } else {
        time_ms
    }
}

/// Returns the calendar components of `time_ms`, or `None` for an invalid date.
pub fn parts(time_ms: f64) -> Option<DateParts> {
    if !time_ms.is_finite() || time_ms.abs() > MAX_TIME_MS {
        return None;
    }
    let time_ms = time_ms.trunc();
    let days = (time_ms / MS_PER_DAY).floor() as i64;
    let within_day = time_ms - days as f64 * MS_PER_DAY;
    let (year, month, day) = civil_from_days(days);
    let hours = (within_day / 3_600_000.0).floor() as u32;
    let minutes = ((within_day / 60_000.0).floor() as u32) % 60;
    let seconds = ((within_day / 1_000.0).floor() as u32) % 60;
    let milliseconds = (within_day as u32) % 1_000;
    Some(DateParts {
        year,
        month: month - 1,
        day,
        weekday: (days + 4).rem_euclid(7) as u32,
        hours,
        minutes,
        seconds,
        milliseconds,
    })
}

/// Formats a time value as an ISO 8601 string, or `None` for an invalid date.
pub fn iso_string(time_ms: f64) -> Option<String> {
    let parts = parts(time_ms)?;
    let year = format_year(parts.year);
    Some(format!(
        "{year}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        parts.month + 1,
        parts.day,
        parts.hours,
        parts.minutes,
        parts.seconds,
        parts.milliseconds
    ))
}

fn format_year(year: i64) -> String {
    if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else if year < 0 {
        format!("-{:06}", -year)
    } else {
        format!("+{year:06}")
    }
}

/// Parses an ISO 8601 date or date-time string, returning `NaN` when invalid.
///
/// Accepted forms:
///
/// - `YYYY`, `YYYY-MM`, `YYYY-MM-DD` (UTC midnight).
/// - `YYYY-MM-DDTHH:mm[:ss[.fff]]` with `Z`, `±HH:MM`, `±HHMM`, or no zone.
/// - The date and time may be separated by `T` or a space.
///
/// A missing zone is treated as UTC. Native JavaScript treats it as local time;
/// CodeMode chooses determinism over host timezone dependence.
pub fn parse_iso(text: &str) -> f64 {
    let text = text.trim();
    if text.is_empty() {
        return f64::NAN;
    }

    let (date_text, time_text) = match text.find(['T', ' ']) {
        Some(index) => (&text[..index], Some(&text[index + 1..])),
        None => (text, None),
    };

    let Some((year, month, day)) = parse_date_part(date_text) else {
        return f64::NAN;
    };

    let (hours, minutes, seconds, milliseconds, offset_minutes) = match time_text {
        None => (0, 0, 0, 0, 0),
        Some(time_text) => {
            let Some(parsed) = parse_time_part(time_text) else {
                return f64::NAN;
            };
            parsed
        }
    };

    let days = days_from_civil(year, month, day);
    let local_ms = days as f64 * MS_PER_DAY
        + hours as f64 * 3_600_000.0
        + minutes as f64 * 60_000.0
        + seconds as f64 * 1_000.0
        + milliseconds as f64;
    clip(local_ms - offset_minutes as f64 * 60_000.0)
}

fn parse_date_part(text: &str) -> Option<(i64, u32, u32)> {
    let mut pieces = text.split('-');
    let year = pieces.next()?.parse::<i64>().ok()?;
    let month = match pieces.next() {
        Some(piece) => piece.parse::<u32>().ok()?,
        None => 1,
    };
    let day = match pieces.next() {
        Some(piece) => piece.parse::<u32>().ok()?,
        None => 1,
    };
    if pieces.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some((year, month, day))
}

fn parse_time_part(text: &str) -> Option<(u32, u32, u32, u32, i32)> {
    let (time_text, offset_minutes) = split_offset(text)?;
    let mut pieces = time_text.split(':');
    let hours = pieces.next()?.parse::<u32>().ok()?;
    let minutes = pieces.next()?.parse::<u32>().ok()?;
    let (seconds, milliseconds) = match pieces.next() {
        Some(piece) => parse_seconds(piece)?,
        None => (0, 0),
    };
    if pieces.next().is_some() || hours > 23 || minutes > 59 || seconds > 59 {
        return None;
    }
    Some((hours, minutes, seconds, milliseconds, offset_minutes))
}

fn parse_seconds(text: &str) -> Option<(u32, u32)> {
    let (seconds, fraction) = match text.find('.') {
        Some(index) => (&text[..index], Some(&text[index + 1..])),
        None => (text, None),
    };
    let seconds = seconds.parse::<u32>().ok()?;
    let milliseconds = match fraction {
        Some(fraction) => {
            if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let mut digits = fraction[..fraction.len().min(3)].to_owned();
            while digits.len() < 3 {
                digits.push('0');
            }
            digits.parse::<u32>().ok()?
        }
        None => 0,
    };
    Some((seconds, milliseconds))
}

fn split_offset(text: &str) -> Option<(&str, i32)> {
    if let Some(time) = text.strip_suffix('Z').or_else(|| text.strip_suffix('z')) {
        return Some((time, 0));
    }
    let offset_index = text
        .char_indices()
        .skip(1)
        .find(|(_, character)| *character == '+' || *character == '-')
        .map(|(index, _)| index);
    let Some(index) = offset_index else {
        return Some((text, 0));
    };
    let (time, offset) = text.split_at(index);
    let sign = if offset.starts_with('-') { -1 } else { 1 };
    let offset = &offset[1..];
    let (hours, minutes) = match offset.find(':') {
        Some(colon) => (&offset[..colon], &offset[colon + 1..]),
        None if offset.len() == 4 => (&offset[..2], &offset[2..]),
        None => (offset, "0"),
    };
    let hours = hours.parse::<i32>().ok()?;
    let minutes = minutes.parse::<i32>().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some((time, sign * (hours * 60 + minutes)))
}

/// Builds a time value from `Date` component arguments.
///
/// `values[0]` is the year, `values[1]` the zero-based month, `values[2]` the
/// day (default 1), then hours, minutes, seconds, and milliseconds. Years 0-99
/// map to 1900-1999, matching JavaScript. Out-of-range components roll over.
pub fn from_components(values: &[f64]) -> f64 {
    if values.is_empty() || values.iter().any(|value| !value.is_finite()) {
        return f64::NAN;
    }
    let year = to_integer(values[0]);
    let year = if (0..=99).contains(&year) {
        year + 1900
    } else {
        year
    };
    let month = component(values.get(1), 0.0);
    let day = component(values.get(2), 1.0);
    let hours = component(values.get(3), 0.0);
    let minutes = component(values.get(4), 0.0);
    let seconds = component(values.get(5), 0.0);
    let milliseconds = component(values.get(6), 0.0);

    let total_months = year
        .checked_mul(12)
        .and_then(|value| value.checked_add(month))
        .unwrap_or(i64::MAX / 2);
    let normalized_year = total_months.div_euclid(12);
    let normalized_month = total_months.rem_euclid(12) as u32 + 1;
    let days = days_from_civil(normalized_year, normalized_month, 1)
        .checked_add(day)
        .and_then(|value| value.checked_sub(1))
        .unwrap_or(i64::MAX / 2);

    let time_ms = days as f64 * MS_PER_DAY
        + hours as f64 * 3_600_000.0
        + minutes as f64 * 60_000.0
        + seconds as f64 * 1_000.0
        + milliseconds as f64;
    clip(time_ms)
}

fn component(value: Option<&f64>, default: f64) -> i64 {
    match value {
        Some(value) => to_integer(*value),
        None => default as i64,
    }
}

fn to_integer(value: f64) -> i64 {
    if !value.is_finite() {
        return 0;
    }
    value.trunc().clamp(i64::MIN as f64, i64::MAX as f64) as i64
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = if month > 2 { month - 3 } else { month + 9 } as i64;
    let day_of_year = (153 * month_prime + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::{clip, from_components, iso_string, parse_iso, parts};

    #[test]
    fn formats_epoch_and_round_trips() {
        assert_eq!(iso_string(0.0).as_deref(), Some("1970-01-01T00:00:00.000Z"));
        assert_eq!(
            iso_string(-1.0).as_deref(),
            Some("1969-12-31T23:59:59.999Z")
        );
        assert_eq!(
            iso_string(1_704_164_645_000.0).as_deref(),
            Some("2024-01-02T03:04:05.000Z")
        );
    }

    #[test]
    fn parses_iso_dates_and_offsets() {
        assert_eq!(parse_iso("2024-01-02T03:04:05.000Z"), 1_704_164_645_000.0);
        assert_eq!(parse_iso("2024-01-02"), 1_704_153_600_000.0);
        assert_eq!(parse_iso("2024-01-02T03:04:05+09:00"), 1_704_132_245_000.0);
        assert_eq!(parse_iso("2024-01-02 03:04:05Z"), 1_704_164_645_000.0);
        assert!(parse_iso("garbage").is_nan());
        assert!(parse_iso("2024-13-01").is_nan());
    }

    #[test]
    fn builds_component_dates_with_rollover() {
        assert_eq!(from_components(&[2024.0, 0.0, 2.0]), 1_704_153_600_000.0);
        assert_eq!(from_components(&[2024.0, 12.0, 1.0]), 1_735_689_600_000.0);
        assert!(from_components(&[f64::NAN, 0.0]).is_nan());
        assert!(from_components(&[2024.0, f64::NAN]).is_nan());
    }

    #[test]
    fn exposes_calendar_parts() {
        let date = parts(1_704_164_645_009.0).expect("valid date");
        assert_eq!(date.year, 2024);
        assert_eq!(date.month, 0);
        assert_eq!(date.day, 2);
        assert_eq!(date.weekday, 2);
        assert_eq!(date.hours, 3);
        assert_eq!(date.minutes, 4);
        assert_eq!(date.seconds, 5);
        assert_eq!(date.milliseconds, 9);
        assert!(parts(0.0).is_some());
        assert!(parts(9.0e15).is_none());
    }

    #[test]
    fn clips_fractional_and_out_of_range_values() {
        assert_eq!(clip(1.9), 1.0);
        assert_eq!(clip(-1.9), -1.0);
        assert!(clip(9.0e15).is_nan());
        assert!(clip(f64::INFINITY).is_nan());
    }
}
