//! Hand-rolled cron expression parser matching `cron-parser@4.9.0` semantics
//! for the subset this server accepts.
//!
//! Ported from `node_modules/cron-parser/lib/expression.js`:
//! - 1–6 whitespace-separated fields; leading fields default from
//!   `['0','*','*','*','*','*']` (second, minute, hour, dom, month, dow).
//!   More than 6 fields is `Invalid cron expression` (no year field).
//! - `@yearly/@monthly/@weekly/@daily/@hourly` predefined aliases.
//! - `*` and `?` wildcards, lists `,`, ranges `-`, steps `/` (bare `n/step`
//!   means `n-max/step`), 3-letter month/day-of-week aliases, dow `7` → `0`.
//! - Day-of-month `L` (last day of month), day-of-week `nL` (last weekday of
//!   month) and `#` (nth weekday of month).
//! - Vixie day semantics: when both dom and dow are restricted, a day matches
//!   when EITHER matches; the dom "wildcard" test is length ≥ the non-leap
//!   days-in-month table entry for the month (cron-parser quirk preserved).
//! - Single-month + explicit dom larger than the (non-leap) month length is
//!   `Invalid explicit day of month definition`.
//!
//! Differences from cron-parser (noted in PORT-MANIFEST.md): timezone math is
//! DST-free — the zone's offset at the evaluation instant is held fixed, and
//! cron-parser's DST hour shifting is not reproduced. Search is bounded to
//! ~8 years of days instead of the 10 000-step loop limit; expressions that
//! can never match return `None` (the JS caller maps that to `null`).

use super::timeutil::{Civil, MS_PER_SECOND, days_from_civil, days_in_month, weekday_from_days};

/// cron-parser's non-leap days-in-month table (Feb = 29).
const DAYS_IN_MONTH_TABLE: [u32; 12] = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

const MONTH_ALIASES: [(&str, u32); 12] = [
    ("jan", 1),
    ("feb", 2),
    ("mar", 3),
    ("apr", 4),
    ("may", 5),
    ("jun", 6),
    ("jul", 7),
    ("aug", 8),
    ("sep", 9),
    ("oct", 10),
    ("nov", 11),
    ("dec", 12),
];

const DOW_ALIASES: [(&str, u32); 7] = [
    ("sun", 0),
    ("mon", 1),
    ("tue", 2),
    ("wed", 3),
    ("thu", 4),
    ("fri", 5),
    ("sat", 6),
];

#[derive(Debug, Clone, PartialEq)]
pub struct CronError(pub String);

impl std::fmt::Display for CronError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CronError {}

/// A single field's values. `L` markers (dom/dow) are tracked separately.
#[derive(Debug, Clone, Default)]
struct FieldValues {
    values: Vec<u32>,
    /// Plain `L` marker (dom: last day of month).
    has_l: bool,
    /// `nL` marker: last weekday `n` of the month (day-of-week only).
    l_weekday: Option<u32>,
}

impl FieldValues {
    fn contains(&self, v: u32) -> bool {
        self.values.binary_search(&v).is_ok()
    }
}

/// Parsed day-of-week `#` occurrence (1-5), when present.
#[derive(Debug, Clone)]
pub struct CronExpr {
    seconds: Vec<u32>,
    minutes: Vec<u32>,
    hours: Vec<u32>,
    dom: FieldValues,
    months: Vec<u32>,
    dow: FieldValues,
    /// `dow#n` nth-day-of-month occurrence.
    nth_day_of_week: Option<u32>,
}

struct Constraints {
    min: i64,
    max: i64,
    allow_l: bool,
}

const SECOND_C: Constraints = Constraints {
    min: 0,
    max: 59,
    allow_l: false,
};
const MINUTE_C: Constraints = Constraints {
    min: 0,
    max: 59,
    allow_l: false,
};
const HOUR_C: Constraints = Constraints {
    min: 0,
    max: 23,
    allow_l: false,
};
const DOM_C: Constraints = Constraints {
    min: 1,
    max: 31,
    allow_l: true,
};
const MONTH_C: Constraints = Constraints {
    min: 1,
    max: 12,
    allow_l: false,
};
const DOW_C: Constraints = Constraints {
    min: 0,
    max: 7,
    allow_l: true,
};

fn replace_aliases(value: &str, aliases: &[(&str, u32)]) -> Result<String, CronError> {
    // cron-parser replaces every 3-letter run (`/[a-z]{3}/gi`); unknown
    // 3-letter runs are errors. Other characters (including a lone `L`)
    // pass through untouched.
    let mut out = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                i += 1;
            }
            let mut run_start = start;
            while i - run_start >= 3 {
                let tri = &value[run_start..run_start + 3];
                let lower = tri.to_ascii_lowercase();
                if let Some((_, n)) = aliases.iter().find(|(name, _)| *name == lower) {
                    out.push_str(&n.to_string());
                } else {
                    return Err(CronError(format!(
                        "Validation error, cannot resolve alias \"{lower}\""
                    )));
                }
                run_start += 3;
            }
            out.push_str(&value[run_start..i]);
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    Ok(out)
}

fn valid_chars(field: &str, value: &str) -> Result<(), CronError> {
    let ok = match field {
        "dayOfMonth" => value.chars().all(|c| {
            c == ','
                || c == '?'
                || c == '*'
                || c.is_ascii_digit()
                || c == 'L'
                || c == '/'
                || c == '-'
        }),
        "dayOfWeek" => value.chars().all(|c| {
            c == ','
                || c == '?'
                || c == '*'
                || c.is_ascii_digit()
                || c == 'L'
                || c == '#'
                || c == '/'
                || c == '-'
        }),
        _ => value
            .chars()
            .all(|c| c == ',' || c == '*' || c.is_ascii_digit() || c == '/' || c == '-'),
    };
    if ok {
        Ok(())
    } else {
        Err(CronError(format!("Invalid characters, got value: {value}")))
    }
}

/// Parse one field into sorted values. Mirrors `_parseField`.
fn parse_field(field: &str, raw: &str, c: &Constraints) -> Result<FieldValues, CronError> {
    let mut value = raw.to_string();
    if field == "month" {
        value = replace_aliases(&value, &MONTH_ALIASES)?;
    } else if field == "dayOfWeek" {
        value = replace_aliases(&value, &DOW_ALIASES)?;
    }
    valid_chars(field, &value)?;

    if value.contains('*') {
        value = value.replace('*', &format!("{}-{}", c.min, c.max));
    } else if value.contains('?') {
        value = value.replace('?', &format!("{}-{}", c.min, c.max));
    }

    // parseSequence
    let atoms: Vec<&str> = value.split(',').collect();
    if atoms.iter().any(|a| a.is_empty()) {
        return Err(CronError("Invalid list value format".to_string()));
    }

    let mut stack: Vec<Item> = Vec::new();
    for atom in atoms {
        parse_repeat(atom, c, field, &mut stack)?;
    }

    // Scalars then strings (cron-parser sorts numbers first, 'L' last).
    let mut numbers: Vec<u32> = stack
        .iter()
        .filter_map(|i| match i {
            Item::Num(n) => Some(*n),
            _ => None,
        })
        .collect();
    numbers.sort_unstable();
    numbers.dedup();
    let has_l = stack.iter().any(|i| matches!(i, Item::L));
    let l_weekday = stack.iter().find_map(|i| match i {
        Item::LastWeekday(n) => Some(*n),
        _ => None,
    });
    if numbers.is_empty() && !has_l && l_weekday.is_none() {
        return Err(CronError(format!(
            "Validation error, Field {field} contains no values"
        )));
    }
    Ok(FieldValues {
        values: numbers,
        has_l,
        l_weekday,
    })
}

enum Item {
    Num(u32),
    L,
    /// `nL`: weekday n restricted to its last occurrence in the month.
    LastWeekday(u32),
}

/// `parseRepeat`: `n/step` widens to `n-max/step`.
fn parse_repeat(
    val: &str,
    c: &Constraints,
    field: &str,
    stack: &mut Vec<Item>,
) -> Result<(), CronError> {
    let parts: Vec<&str> = val.split('/').collect();
    if parts.len() > 2 {
        return Err(CronError(format!("Invalid repeat: {val}")));
    }
    if parts.len() == 2 {
        let head = parts[0];
        let widened = match head.parse::<i64>() {
            Ok(n) => format!("{}-{}", n, c.max),
            Err(_) => head.to_string(),
        };
        return parse_range(&widened, parts[1], c, field, stack);
    }
    parse_range(val, "1", c, field, stack)
}

/// `parseRange`: `a-b/step` expansion with constraint checks.
fn parse_range(
    val: &str,
    repeat: &str,
    c: &Constraints,
    field: &str,
    stack: &mut Vec<Item>,
) -> Result<(), CronError> {
    let parts: Vec<&str> = val.split('-').collect();
    if parts.len() == 1 {
        // Scalar.
        let s = parts[0];
        if s == "L" {
            if !c.allow_l {
                return Err(CronError(format!(
                    "Constraint error, got value {s} expected range {}-{}",
                    c.min, c.max
                )));
            }
            stack.push(Item::L);
            return Ok(());
        }
        // `5L` (day-of-week last-weekday marker): numeric prefix + L marker.
        if let Some(prefix) = s.strip_suffix('L')
            && c.allow_l
            && !prefix.is_empty()
        {
            let n: i64 = prefix.parse().map_err(|_| {
                CronError(format!(
                    "Constraint error, got value {s} expected range {}-{}",
                    c.min, c.max
                ))
            })?;
            if n < c.min || n > c.max {
                return Err(CronError(format!(
                    "Constraint error, got value {s} expected range {}-{}",
                    c.min, c.max
                )));
            }
            stack.push(Item::LastWeekday((n % 7) as u32));
            return Ok(());
        }
        let n: i64 = s.parse().map_err(|_| {
            CronError(format!(
                "Constraint error, got value {s} expected range {}-{}",
                c.min, c.max
            ))
        })?;
        if n < c.min || n > c.max {
            return Err(CronError(format!(
                "Constraint error, got value {s} expected range {}-{}",
                c.min, c.max
            )));
        }
        let mut m = n;
        if field == "dayOfWeek" {
            m = n % 7;
        }
        stack.push(Item::Num(m as u32));
        return Ok(());
    }

    if parts.len() > 2 {
        return Err(CronError(format!("Invalid range: {val}")));
    }
    if parts[0].is_empty() && parts[1].is_empty() {
        return Err(CronError(format!("Invalid range: {val}")));
    }
    if parts[0].is_empty() || parts[1].is_empty() {
        return Err(CronError(format!("Invalid range: {val}")));
    }

    let min: i64 = parts[0].parse().map_err(|_| {
        CronError(format!(
            "Constraint error, got range {}-{} expected range {}-{}",
            parts[0], parts[1], c.min, c.max
        ))
    })?;
    let max: i64 = parts[1].parse().map_err(|_| {
        CronError(format!(
            "Constraint error, got range {}-{} expected range {}-{}",
            parts[0], parts[1], c.min, c.max
        ))
    })?;
    if min < c.min || max > c.max {
        return Err(CronError(format!(
            "Constraint error, got range {}-{} expected range {}-{}",
            parts[0], parts[1], c.min, c.max
        )));
    }
    if min > max {
        return Err(CronError(format!("Invalid range: {val}")));
    }
    let step: i64 = repeat.parse().map_err(|_| {
        CronError(format!(
            "Constraint error, cannot repeat at every {repeat} time."
        ))
    })?;
    if step <= 0 {
        return Err(CronError(format!(
            "Constraint error, cannot repeat at every {repeat} time."
        )));
    }

    // dow: a range whose max is 7 also emits 0 (so `*` = 0-7 keeps 8 values
    // — cron-parser's wildcard test relies on that length).
    if field == "dayOfWeek" && max % 7 == 0 {
        stack.push(Item::Num(0));
    }
    let mut index = min;
    while index <= max {
        let value = index as u32;
        // cron-parser skips values already on the stack (the pre-pushed 0).
        if !stack
            .iter()
            .any(|item| matches!(item, Item::Num(n) if *n == value))
        {
            stack.push(Item::Num(value));
        }
        index += step;
    }
    Ok(())
}

/// Parse `dow#n` (nth weekday) — extracted before field parsing like the JS.
fn parse_nth_day(value: &str) -> Result<(String, Option<u32>), CronError> {
    let parts: Vec<&str> = value.split('#').collect();
    if parts.len() == 1 {
        return Ok((value.to_string(), None));
    }
    if value.contains(',') {
        return Err(CronError(
            "Constraint error, invalid dayOfWeek `#` and `,` special characters are incompatible"
                .to_string(),
        ));
    }
    if value.contains('/') {
        return Err(CronError(
            "Constraint error, invalid dayOfWeek `#` and `/` special characters are incompatible"
                .to_string(),
        ));
    }
    if value.contains('-') {
        return Err(CronError(
            "Constraint error, invalid dayOfWeek `#` and `-` special characters are incompatible"
                .to_string(),
        ));
    }
    let nth: i64 = parts[parts.len() - 1].parse().map_err(|_| {
        CronError("Constraint error, invalid dayOfWeek occurrence number (#)".to_string())
    })?;
    if parts.len() > 2 || !(1..=5).contains(&nth) {
        return Err(CronError(
            "Constraint error, invalid dayOfWeek occurrence number (#)".to_string(),
        ));
    }
    Ok((parts[0].to_string(), Some(nth as u32)))
}

impl CronExpr {
    /// Parse a cron expression (cron-parser `parseExpression`).
    pub fn parse(expression: &str) -> Result<Self, CronError> {
        let expr = match expression {
            "@yearly" => "0 0 1 1 *",
            "@monthly" => "0 0 1 * *",
            "@weekly" => "0 0 * * 0",
            "@daily" => "0 0 * * *",
            "@hourly" => "0 * * * *",
            other => other,
        };

        let atoms: Vec<&str> = expr.split_whitespace().collect();
        if atoms.len() > 6 {
            return Err(CronError("Invalid cron expression".to_string()));
        }

        // Field defaults ['0','*','*','*','*','*'] (second minute hour dom
        // month dow); provided atoms right-align onto the tail fields.
        let defaults = ["0", "*", "*", "*", "*", "*"];
        let start = 6usize.saturating_sub(atoms.len());
        let mut fields: Vec<String> = Vec::with_capacity(6);
        for i in 0..6 {
            let value = if i < start {
                defaults[i].to_string()
            } else {
                atoms[i - start].to_string()
            };
            fields.push(value);
        }

        let mut nth_day_of_week = None;
        let mut dow_raw = fields[5].clone();
        if dow_raw.contains('#') {
            let (base, nth) = parse_nth_day(&dow_raw)?;
            dow_raw = base;
            nth_day_of_week = nth;
        }

        let seconds = parse_field("second", &fields[0], &SECOND_C)?;
        let minutes = parse_field("minute", &fields[1], &MINUTE_C)?;
        let hours = parse_field("hour", &fields[2], &HOUR_C)?;
        let dom = parse_field("dayOfMonth", &fields[3], &DOM_C)?;
        let months = parse_field("month", &fields[4], &MONTH_C)?;
        let dow = parse_field("dayOfWeek", &dow_raw, &DOW_C)?;

        // `_handleMaxDaysInMonth`: single month + explicit dom beyond the
        // (non-leap) month length is invalid; other out-of-month days are
        // filtered out ('L' survives).
        let mut dom = dom;
        if months.values.len() == 1 {
            let days = DAYS_IN_MONTH_TABLE[(months.values[0] - 1) as usize];
            if let Some(first) = dom.values.first()
                && *first > days
            {
                return Err(CronError(
                    "Invalid explicit day of month definition".to_string(),
                ));
            }
            dom.values.retain(|d| *d <= days);
        }

        Ok(CronExpr {
            seconds: seconds.values,
            minutes: minutes.values,
            hours: hours.values,
            dom,
            months: months.values,
            dow,
            nth_day_of_week,
        })
    }

    /// cron-parser `next()`: the first matching instant strictly after
    /// `current_utc_ms`, computed under a fixed zone offset (seconds).
    /// `None` mirrors the JS throw paths (`computeNextRunAt` maps to `null`).
    pub fn next_after(&self, current_utc_ms: i64, offset_seconds: i64) -> Option<i64> {
        // Matches sit on whole seconds; start strictly after `current`.
        let start_sec = current_utc_ms.div_euclid(MS_PER_SECOND) + 1;
        let start_ms = start_sec * MS_PER_SECOND;
        let start = civil_from_utc(start_ms, offset_seconds);

        // Bound the search: ~8 years covers any leap-year window
        // (cron-parser instead fails after its 10 000-step loop limit).
        let max_days = 8 * 366 + 1;

        let mut day_civil = Civil {
            year: start.year,
            month: start.month,
            day: start.day,
            hour: 0,
            minute: 0,
            second: 0,
        };

        for _ in 0..max_days {
            let month_ok = self.months.binary_search(&day_civil.month).is_ok();
            if !month_ok {
                // Advance to the 1st of the next month.
                let (ny, nm) = if day_civil.month == 12 {
                    (day_civil.year + 1, 1)
                } else {
                    (day_civil.year, day_civil.month + 1)
                };
                day_civil = Civil {
                    year: ny,
                    month: nm,
                    day: 1,
                    hour: 0,
                    minute: 0,
                    second: 0,
                };
                continue;
            }
            if !self.day_matches(&day_civil) {
                day_civil = next_day(&day_civil);
                continue;
            }

            // Find the smallest allowed (h, m, s) at or after the start time
            // on this day.
            let at_start = if day_civil.day == start.day
                && day_civil.month == start.month
                && day_civil.year == start.year
            {
                Some((start.hour, start.minute, start.second))
            } else {
                None
            };
            if let Some(instant) = first_time_at_or_after(
                &day_civil,
                &self.hours,
                &self.minutes,
                &self.seconds,
                at_start,
                offset_seconds,
            ) {
                return Some(instant);
            }
            day_civil = next_day(&day_civil);
        }
        None
    }

    /// Vixie dom/dow semantics (see cron-parser `_findSchedule`).
    fn day_matches(&self, day: &Civil) -> bool {
        let dom_match = self.dom.contains(day.day)
            || (self.dom.has_l && day.day == days_in_month(day.year, day.month));
        let weekday = day.weekday();
        let dow_match = self.dow.contains(weekday)
            || (self.dow.l_weekday == Some(weekday) && self.is_last_weekday_of_month(day, weekday));
        let dom_wildcard =
            self.dom.values.len() >= DAYS_IN_MONTH_TABLE[(day.month - 1) as usize] as usize;
        let dow_wildcard = self.dow.values.len() >= 8
            || (self.dow.values.len() == 7
                && !self.dow.has_l
                && !self.dow.has_l
                && self.dow_full_range());

        if !dom_match && (!dow_match || dow_wildcard) {
            return false;
        }
        if !dom_wildcard && dow_wildcard && !dom_match {
            return false;
        }
        if dom_wildcard && !dow_wildcard && !dow_match {
            return false;
        }
        if let Some(nth) = self.nth_day_of_week
            && !is_nth_day_match(day, nth)
        {
            return false;
        }
        true
    }

    fn dow_full_range(&self) -> bool {
        // `*` in dow expands to 0-7 (8 values incl. 7→0). An explicit 0-6 is
        // 7 values and counts as restricted in cron-parser.
        false
    }

    /// `nL` day-of-week: is `day` the last occurrence of weekday `n` this
    /// month (cron-parser `isLastWeekdayOfMonth`)?
    fn is_last_weekday_of_month(&self, day: &Civil, weekday: u32) -> bool {
        let last_day = days_in_month(day.year, day.month);
        let last_date = last_day
            - ((weekday_from_days(days_from_civil(day.year, day.month, last_day)) + 7 - weekday)
                % 7);
        day.day == last_date
    }
}

fn is_nth_day_match(day: &Civil, nth: u32) -> bool {
    if nth >= 6 {
        return false;
    }
    if nth == 1 && day.day < 8 {
        return true;
    }
    let offset = if !day.day.is_multiple_of(7) { 1 } else { 0 };
    let adjusted = day.day - day.day % 7;
    let occurrence = adjusted / 7 + offset;
    occurrence == nth
}

fn next_day(day: &Civil) -> Civil {
    let last = days_in_month(day.year, day.month);
    if day.day < last {
        Civil {
            day: day.day + 1,
            ..*day
        }
    } else if day.month < 12 {
        Civil {
            year: day.year,
            month: day.month + 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
        }
    } else {
        Civil {
            year: day.year + 1,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
        }
    }
}

fn civil_from_utc(utc_ms: i64, offset_seconds: i64) -> Civil {
    super::timeutil::civil_from_utc_ms(utc_ms, offset_seconds)
}

fn first_time_at_or_after(
    day: &Civil,
    hours: &[u32],
    minutes: &[u32],
    seconds: &[u32],
    at_start: Option<(u32, u32, u32)>,
    offset_seconds: i64,
) -> Option<i64> {
    let (sh, sm, ss) = at_start.unwrap_or((0, 0, 0));
    for &h in hours {
        if h < sh {
            continue;
        }
        for &m in minutes {
            if h == sh && m < sm {
                continue;
            }
            for &s in seconds {
                if h == sh && m == sm && s < ss {
                    continue;
                }
                let candidate = Civil {
                    hour: h,
                    minute: m,
                    second: s,
                    ..*day
                };
                return Some(super::timeutil::utc_ms_from_civil(
                    &candidate,
                    offset_seconds,
                ));
            }
        }
    }
    None
}

/// Convenience for validation (`validateCronExpression`): parse + compute one
/// next occurrence from `now_ms` under the zone's fixed offset.
pub fn validate(expression: &str, timezone: &str, now_ms: i64) -> bool {
    let offset = super::timeutil::offset_or_utc(timezone, now_ms);
    match CronExpr::parse(expression) {
        Ok(expr) => expr.next_after(now_ms, offset).is_some(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::super::timeutil::{is_leap_year, utc_ms_from_civil};
    use super::*;

    fn utc(y: i64, m: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
        utc_ms_from_civil(
            &Civil {
                year: y,
                month: m,
                day: d,
                hour: h,
                minute: mi,
                second: s,
            },
            0,
        )
    }

    fn next(expr: &str, now: i64) -> Option<i64> {
        CronExpr::parse(expr).ok()?.next_after(now, 0)
    }

    #[test]
    fn parses_five_field_expressions() {
        // Every 5 minutes from 14:03.
        let now = utc(2026, 1, 1, 14, 3, 0);
        assert_eq!(next("*/5 * * * *", now), Some(utc(2026, 1, 1, 14, 5, 0)));
        // Daily at 09:00.
        assert_eq!(
            next("0 9 * * *", utc(2026, 1, 1, 8, 0, 0)),
            Some(utc(2026, 1, 1, 9, 0, 0))
        );
        // Exactly-on boundary: strictly after.
        assert_eq!(
            next("0 9 * * *", utc(2026, 1, 1, 9, 0, 0)),
            Some(utc(2026, 1, 2, 9, 0, 0))
        );
        // Sub-second `now` still cannot re-match the current second: the next
        // occurrence is tomorrow 09:00 (cron-parser increments out of it).
        assert_eq!(
            next("0 9 * * *", utc(2026, 1, 1, 9, 0, 0) + 500),
            Some(utc(2026, 1, 2, 9, 0, 0))
        );
        // Weekly Monday 09:00 (2026-01-01 is a Thursday).
        assert_eq!(
            next("0 9 * * 1", utc(2026, 1, 1, 10, 0, 0)),
            Some(utc(2026, 1, 5, 9, 0, 0))
        );
        assert_eq!(
            next("0 8 * * 1", utc(2026, 1, 1, 10, 0, 0)),
            Some(utc(2026, 1, 5, 8, 0, 0))
        );
        // Half-hourly alias-free step.
        assert_eq!(
            next("*/30 * * * *", utc(2026, 1, 1, 9, 0, 0)),
            Some(utc(2026, 1, 1, 9, 30, 0))
        );
    }

    #[test]
    fn supports_names_ranges_lists_and_steps() {
        let now = utc(2026, 1, 1, 0, 0, 0);
        assert_eq!(next("0 9 * JAN *", now), Some(utc(2026, 1, 1, 9, 0, 0)));
        assert_eq!(next("0 9 * feb *", now), Some(utc(2026, 2, 1, 9, 0, 0)));
        assert_eq!(next("0 9 * * mon-fri", now), Some(utc(2026, 1, 1, 9, 0, 0))); // Thursday
        assert_eq!(next("0 9 * * sun,sat", now), Some(utc(2026, 1, 3, 9, 0, 0)));
        assert_eq!(
            next("30 8-10 * * *", utc(2026, 1, 1, 8, 0, 0)),
            Some(utc(2026, 1, 1, 8, 30, 0))
        );
        assert_eq!(next("0 9 15 * *", now), Some(utc(2026, 1, 15, 9, 0, 0)));
        // Bare number + step widens to N-max.
        assert_eq!(
            next("20/15 * * * *", utc(2026, 1, 1, 0, 0, 0)),
            Some(utc(2026, 1, 1, 0, 20, 0))
        );
        assert_eq!(
            next("20/15 * * * *", utc(2026, 1, 1, 0, 35, 0)),
            Some(utc(2026, 1, 1, 0, 50, 0))
        );
        // dow 7 == 0 (Sunday).
        assert_eq!(next("0 9 * * 7", now), Some(utc(2026, 1, 4, 9, 0, 0)));
    }

    #[test]
    fn supports_optional_seconds_field() {
        let now = utc(2026, 1, 1, 9, 0, 0);
        assert_eq!(next("30 * * * * *", now), Some(utc(2026, 1, 1, 9, 0, 30)));
        assert_eq!(next("0 5 * * * *", now), Some(utc(2026, 1, 1, 9, 5, 0)));
        assert_eq!(next("*/15 * * * * *", now), Some(utc(2026, 1, 1, 9, 0, 15)));
    }

    #[test]
    fn handles_leap_day_expressions() {
        let now = utc(2026, 1, 1, 0, 0, 0);
        // Feb 29 next occurs in 2028.
        assert_eq!(next("0 0 29 2 *", now), Some(utc(2028, 2, 29, 0, 0, 0)));
    }

    #[test]
    fn vixie_day_semantics_when_both_dom_and_dow_restricted() {
        // "30 4 1,15 * 5": 04:30 on the 1st and 15th plus every Friday.
        // 2026-01-01 matches the dom half of the OR, so it fires on the 1st.
        let now = utc(2026, 1, 1, 0, 0, 0);
        assert_eq!(next("30 4 1,15 * 5", now), Some(utc(2026, 1, 1, 4, 30, 0)));
        // Restricted dom + wildcard dow → dom only.
        assert_eq!(next("0 0 1 * *", now), Some(utc(2026, 2, 1, 0, 0, 0)));
    }

    #[test]
    fn supports_l_and_nth_markers() {
        let now = utc(2026, 1, 1, 0, 0, 0);
        // L in dom: last day of January.
        assert_eq!(next("0 0 L * *", now), Some(utc(2026, 1, 31, 0, 0, 0)));
        // 5L: last Friday of January 2026 is the 30th.
        assert_eq!(next("0 0 * * 5L", now), Some(utc(2026, 1, 30, 0, 0, 0)));
        // Second Friday of January 2026: the 9th.
        assert_eq!(next("0 0 * * 5#2", now), Some(utc(2026, 1, 9, 0, 0, 0)));
    }

    #[test]
    fn rejects_invalid_expressions_with_cron_parser_messages() {
        assert_eq!(
            CronExpr::parse("not a cron expression at all with too many fields")
                .unwrap_err()
                .0,
            "Invalid cron expression"
        );
        // cron-parser treats "" as [""]: the falsy atom falls back to field
        // defaults, so the empty expression parses as "0 * * * * *".
        assert!(CronExpr::parse("").is_ok());
        assert_eq!(
            CronExpr::parse("0 9 * * 8").unwrap_err().0,
            "Constraint error, got value 8 expected range 0-7"
        );
        assert_eq!(
            CronExpr::parse("61 * * * *").unwrap_err().0,
            "Constraint error, got value 61 expected range 0-59"
        );
        assert_eq!(
            CronExpr::parse("0 25 * * *").unwrap_err().0,
            "Constraint error, got value 25 expected range 0-23"
        );
        assert_eq!(
            CronExpr::parse("* * * foo *").unwrap_err().0,
            "Validation error, cannot resolve alias \"foo\""
        );
        assert_eq!(
            CronExpr::parse("* * * * 5#9").unwrap_err().0,
            "Constraint error, invalid dayOfWeek occurrence number (#)"
        );
        // Single month + dom beyond the month length.
        assert_eq!(
            CronExpr::parse("0 0 31 4 *").unwrap_err().0,
            "Invalid explicit day of month definition"
        );
        // Stray single letters fail the character whitelist (only 3-letter
        // runs reach alias resolution in cron-parser).
        assert_eq!(
            CronExpr::parse("0 9 * * x").unwrap_err().0,
            "Invalid characters, got value: x"
        );
    }

    #[test]
    fn never_matching_expressions_return_none() {
        // 30 February can never happen (filtered out of the dom set).
        let now = utc(2026, 1, 1, 0, 0, 0);
        assert_eq!(next("0 0 30 2 *", now), None);
        assert!(validate("0 0 30 2 *", "UTC", now) == false);
        assert!(validate("*/5 * * * *", "UTC", now));
    }

    #[test]
    fn computes_under_a_fixed_zone_offset() {
        // 09:00 UTC == 11:00 at Kyiv's +2 offset: cron in Kyiv zone at 09:00
        // local fires at 07:00 UTC.
        let now = utc(2026, 1, 1, 6, 0, 0);
        let expr = CronExpr::parse("0 9 * * *").expect("valid");
        assert_eq!(
            expr.next_after(now, 2 * 3600),
            Some(utc(2026, 1, 1, 7, 0, 0))
        );
    }

    #[test]
    fn predefined_aliases_expand() {
        let now = utc(2026, 1, 1, 12, 0, 0);
        assert_eq!(next("@daily", now), Some(utc(2026, 1, 2, 0, 0, 0)));
        assert_eq!(next("@hourly", now), Some(utc(2026, 1, 1, 13, 0, 0)));
    }

    #[test]
    fn is_leap_year_table_agrees_with_helper() {
        assert!(is_leap_year(2028));
        assert!(!is_leap_year(2026));
        // Regression guard for the day-matching path across a leap February.
        let now = utc(2028, 2, 28, 23, 0, 0);
        assert_eq!(next("0 0 29 2 *", now), Some(utc(2028, 2, 29, 0, 0, 0)));
    }
}
