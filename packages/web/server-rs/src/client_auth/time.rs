//! Wall-clock access and ISO-8601 helpers for the client-auth runtimes.
//!
//! JS precedent: `remote-clients.js` / `pairing.js` / `tunnel-auth.js` read
//! `Date.now()` and persist `new Date().toISOString()` strings. The clock is
//! injectable (same pattern as `scheduled_tasks::Clock`) so expiry and TTL
//! behavior is deterministic under test.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Wall clock in unix ms (JS `Date.now()`).
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    })
}

/// `new Date(ms).toISOString()` — UTC ISO-8601 with millisecond precision.
pub fn iso_utc_from_unix_millis(unix_millis: i64) -> String {
    let secs = unix_millis.div_euclid(1000);
    let millis = unix_millis.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// `Date.parse(value)` for the timestamp shapes these stores actually hold
/// (and accept): `YYYY-MM-DD`, optionally followed by `T`/space and
/// `HH:MM[:SS[.fff]]` with a `Z` / `±HH:MM` / `±HHMM` / `±HH` offset.
/// Returns `None` for anything else (JS yields `NaN`, callers test
/// `Number.isFinite`). Other `Date.parse` formats are a noted gap.
pub fn parse_iso_ms(value: &str) -> Option<i64> {
    let value = value.trim();
    if value.len() < 10 {
        return None;
    }
    let bytes = value.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    if !bytes
        .iter()
        .take(10)
        .enumerate()
        .filter(|(i, _)| *i != 4 && *i != 7)
        .all(|(_, b)| b.is_ascii_digit())
    {
        return None;
    }
    let year: i64 = value[0..4].parse().ok()?;
    let month: u32 = value[5..7].parse().ok()?;
    let day: u32 = value[8..10].parse().ok()?;
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    let mut secs = days_from_civil(year, month, day) * 86_400;
    let mut millis = 0i64;
    let mut offset_ms: i64 = 0;

    let mut rest = &value[10..];
    if rest.starts_with(['T', 't', ' ']) {
        rest = &rest[1..];
        // HH:MM[:SS[.fraction]]
        if rest.len() < 5 || !is_digits(&rest[0..2]) || rest.as_bytes()[2] != b':' {
            return None;
        }
        let hour: i64 = rest[0..2].parse().ok()?;
        let minute: i64 = rest[3..5].parse().ok()?;
        if hour > 23 || minute > 59 {
            return None;
        }
        secs += hour * 3600 + minute * 60;
        rest = &rest[5..];
        if rest.starts_with(':') {
            rest = &rest[1..];
            let sec_len = rest.chars().take_while(char::is_ascii_digit).count();
            if sec_len != 2 {
                return None;
            }
            let second: i64 = rest[0..2].parse().ok()?;
            if second > 59 {
                return None;
            }
            secs += second;
            rest = &rest[2..];
            if let Some(frac) = rest.strip_prefix('.') {
                let frac_len = frac.chars().take_while(char::is_ascii_digit).count();
                if frac_len == 0 {
                    return None;
                }
                // JS truncates sub-millisecond digits.
                let ms_digits: String = frac.chars().take(3).collect();
                millis = format!("{ms_digits:0<3}").parse().ok()?;
                rest = &rest[1 + frac_len..];
            }
        }
        let offset = rest;
        if offset == "Z" || offset == "z" {
            // UTC
        } else if let Some(sign) = offset.chars().next()
            && matches!(sign, '+' | '-')
        {
            let body = &offset[1..];
            let (oh, om): (i64, i64) = if body.len() == 5 && body.as_bytes()[2] == b':' {
                (body[0..2].parse().ok()?, body[3..5].parse().ok()?)
            } else if body.len() == 4 && is_digits(body) {
                (body[0..2].parse().ok()?, body[2..4].parse().ok()?)
            } else if body.len() == 2 && is_digits(body) {
                (body.parse().ok()?, 0)
            } else {
                return None;
            };
            if oh > 23 || om > 59 {
                return None;
            }
            offset_ms = (oh * 3600 + om * 60) * 1000;
            if sign == '-' {
                offset_ms = -offset_ms;
            }
        } else if !offset.is_empty() {
            return None;
        }
    } else if !rest.is_empty() {
        return None;
    }

    Some(secs * 1000 + millis - offset_ms)
}

/// `new Date(ms).toUTCString()` — RFC 7231 IMF-fixdate
/// (`Www, dd Mmm yyyy HH:MM:SS GMT`).
pub fn http_date_from_unix_millis(unix_millis: i64) -> String {
    let secs = unix_millis.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    // 1970-01-01 was a Thursday.
    let weekday = WEEKDAYS[(days + 4).rem_euclid(7) as usize];
    format!(
        "{weekday}, {day:02} {} {year:04} {:02}:{:02}:{:02} GMT",
        MONTHS[(month - 1) as usize],
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Howard Hinnant's `civil_from_days` (same algorithm family as the other
/// ported modules).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_roundtrip_matches_js_shapes() {
        assert_eq!(iso_utc_from_unix_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            iso_utc_from_unix_millis(1_790_380_800_000),
            "2026-09-26T00:00:00.000Z"
        );
        assert_eq!(iso_utc_from_unix_millis(-1), "1969-12-31T23:59:59.999Z");
    }

    #[test]
    fn parse_iso_ms_handles_dateparse_shapes() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_ms("1970-01-02T00:00:00Z"), Some(86_400_000));
        assert_eq!(parse_iso_ms("1970-01-01"), Some(0));
        assert_eq!(
            parse_iso_ms("2026-01-01T00:00:00.500Z"),
            Some(1_767_225_600_500)
        );
        // Space separator and no seconds.
        assert_eq!(
            parse_iso_ms("1970-01-01 01:30"),
            parse_iso_ms("1970-01-01T01:30:00")
        );
        // JS truncates sub-millisecond digits.
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00.1234Z"), Some(123));
        // Numeric offsets.
        assert_eq!(
            parse_iso_ms("1970-01-01T01:00:00+01:00"),
            parse_iso_ms("1970-01-01T00:00:00Z")
        );
        assert_eq!(
            parse_iso_ms("1970-01-01T01:00:00-0100"),
            parse_iso_ms("1970-01-01T02:00:00Z")
        );
        assert_eq!(
            parse_iso_ms("1970-01-01T01:00:00+02"),
            parse_iso_ms("1970-01-01T01:00:00+0200")
        );
        // Whitespace and invalid shapes.
        assert_eq!(parse_iso_ms("  1970-01-01T00:00:00Z "), Some(0));
        assert_eq!(parse_iso_ms(""), None);
        assert_eq!(parse_iso_ms("not-a-date"), None);
        assert_eq!(parse_iso_ms("2026-02-30"), None);
        assert_eq!(parse_iso_ms("2026-13-01"), None);
    }

    #[test]
    fn http_date_matches_toutcstring() {
        assert_eq!(
            http_date_from_unix_millis(0),
            "Thu, 01 Jan 1970 00:00:00 GMT"
        );
        assert_eq!(
            http_date_from_unix_millis(1_790_380_800_000),
            "Sat, 26 Sep 2026 00:00:00 GMT"
        );
    }
}
