//! Port of `server/lib/quota/utils/` — shared value coercion, formatters, and
//! transformers every provider builds its result shape from
//! (`auth.js`, `transformers.js`, `formatters.js`, `index.js`).
//!
//! Porting notes:
//! - JS `Number(value)` string parsing (empty → 0, whitespace-trimmed) is
//!   mirrored by [`to_number`]; non-finite results collapse to `None`.
//! - JSON numbers keep JS semantics: integer-valued floats serialize without
//!   a trailing `.0` via [`num`].
//! - `Date.parse` is ported as an ISO-8601 subset ([`parse_iso_ms`]);
//!   timestamps without a timezone are read as UTC (JS would use the server's
//!   local zone — see the module gap notes in PORT-MANIFEST).
//! - `toLocaleTimeString`/`toLocaleString` labels ([`format_reset_time`]) are
//!   locale-dependent in JS; the port emits deterministic en-US/UTC labels so
//!   the two `resetAtFormatted`/`resetAfterFormatted` fields stay populated.
//!
//! 中文说明：本文件汇集各 quota provider 共用的数值转换、格式化与结果
//! 组装工具，逐项对齐 JS 版 utils 的行为——JS Number/JSON 序列化语义、
//! ISO-8601 子集解析、`toFixed` 舍入、统一 usage window 与 provider
//! result 信封，以及凭据条目的读取/归一化辅助函数。

use std::path::Path;

use serde_json::{Map, Value, json};

use crate::quota::deps::QuotaDeps;

/// JS `asObject` — arrays count as objects too (callers only read fields).
/// 中文说明：仅当值为 JSON 对象或数组时返回 `Some`（调用方只按对象读取字段）。
pub fn as_object_value(value: Option<&Value>) -> Option<&Value> {
    match value? {
        Value::Object(_) | Value::Array(_) => value,
        _ => None,
    }
}

/// JS `nonEmptyString` (xai.js) — trimmed non-empty string.
/// 中文说明：trim 后非空才返回 `Some`，空串与其它类型一律 `None`。
pub fn non_empty_string_value(value: &Value) -> Option<String> {
    let trimmed = value.as_str()?.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// JS-style property access: missing keys and JSON `null` both read as
/// absent, matching the `value?.key` / `if (value)` patterns the JS uses
/// (a `null` field is falsy and every coerce maps it to null).
/// 中文说明：模拟 JS 的 `value?.key` + 真值判断——键缺失或值为 `null` 都视为不存在。
pub fn field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value
        .as_object()
        .and_then(|map| map.get(key))
        .filter(|value| !value.is_null())
}

/// `asNonEmptyString`: non-empty trimmed string.
/// 中文说明：与 [`non_empty_string_value`] 行为一致的别名实现，供各 provider 选用。
pub fn as_non_empty_string(value: &Value) -> Option<String> {
    let text = value.as_str()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// JS `Number(string)` for the string branch of `toNumber`: trimmed, empty → 0,
/// invalid → None (JS NaN filtered by `Number.isFinite` upstream).
/// 中文说明：trim 后空串按 JS `Number("")` 返回 0；解析失败或得到非有限值返回 `None`。
fn js_number_string(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Some(0.0);
    }
    trimmed.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// `toNumber`: finite numbers pass through; numeric strings parse; everything
/// else is null.
/// 中文说明：数字须有限才透传；字符串交给 [`js_number_string`]；其余类型一律 `None`。
pub fn to_number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(n) => n.as_f64().filter(|v| v.is_finite()),
        Value::String(s) => js_number_string(s),
        _ => None,
    }
}

/// Serialize a float the way `JSON.stringify` would: integer-valued floats
/// lose the fractional part.
/// 中文说明：整数值且绝对值小于 2^53 的浮点数序列化为整数，其余保留小数，与 `JSON.stringify` 一致。
pub fn num(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

/// Same canonicalization as [`num`] but as a display string
/// (JS template interpolation of a number).
/// 中文说明：与 [`num`] 相同的整数化规则，但输出展示字符串（等价 JS 模板插值数字）。
pub fn num_str(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

// ============== date helpers ==============

/// Days from 1970-01-01 for a civil date (Howard Hinnant's algorithm).
/// 中文说明：公历日期折算为距 1970-01-01 的天数（Howard Hinnant 算法，纯天运算无时区）。
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = ((month as i64) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`]: (year, month, day).
/// 中文说明：[`days_from_civil`] 的逆运算，返回 `(年, 月, 日)` 三元组。
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// 0 = Sunday … 6 = Saturday for a days-from-epoch value.
/// 中文说明：1970-01-01 是周四（偏移 +4），据此把天数折算为 0=周日 … 6=周六。
fn weekday_from_days(days: i64) -> usize {
    (((days % 7) + 7 + 4) % 7) as usize
}

/// 月份缩写表，供 en-US 风格的 `Mon D` 标签使用。
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
/// 星期缩写表（索引 0=Sun … 6=Sat）。
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// 把 24 小时制的时/分格式化为 `h:MM AM/PM`（0 点显示 12 AM、12 点显示 12 PM）。
fn hour_minute_ampm(hour: u32, minute: u32) -> String {
    let display_hour = match hour % 12 {
        0 => 12,
        h => h,
    };
    let meridiem = if hour < 12 { "AM" } else { "PM" };
    format!("{display_hour}:{minute:02} {meridiem}")
}

/// `Date.parse` subset: `YYYY-MM-DD[THH:MM[:SS[.frac]]][Z|±HH[:]MM]]`.
/// Fractional seconds may have any digit count. A missing timezone reads as
/// UTC (JS would apply the server's local offset).
///
/// 中文说明：支持任意位数的小数秒；无时区时按 UTC 解析（JS `Date.parse`
/// 会套用本地时区，这是移植时的已知偏差）。解析失败返回 `None`。
pub fn parse_iso_ms(text: &str) -> Option<i64> {
    let text = text.trim();
    let bytes = text.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    let year: i64 = text.get(0..4)?.parse().ok()?;
    if !bytes[4].is_ascii_digit() && bytes[4] != b'-' {
        return None;
    }
    if bytes[4] != b'-' {
        return None;
    }
    let month: u32 = text.get(5..7)?.parse().ok()?;
    if bytes[7] != b'-' {
        return None;
    }
    let day: u32 = text.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut total_ms = days_from_civil(year, month, day) * 86_400_000;
    if bytes.len() == 10 {
        return Some(total_ms);
    }
    let mut index = match bytes[10] {
        b'T' | b't' | b' ' => 11,
        _ => return None,
    };
    if bytes.len() < index + 5 || bytes[index + 2] != b':' {
        return None;
    }
    let hour: u32 = text.get(index..index + 2)?.parse().ok()?;
    let minute: u32 = text.get(index + 3..index + 5)?.parse().ok()?;
    index += 5;
    let mut second = 0u32;
    let mut millis = 0i64;
    if bytes.get(index) == Some(&b':') {
        if bytes.len() < index + 3 {
            return None;
        }
        second = text.get(index + 1..index + 3)?.parse().ok()?;
        index += 3;
        if matches!(bytes.get(index), Some(b'.') | Some(b',')) {
            index += 1;
            let start = index;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            if index == start {
                return None;
            }
            let mut fraction = String::new();
            for c in text[start..index].chars().take(3) {
                fraction.push(c);
            }
            while fraction.len() < 3 {
                fraction.push('0');
            }
            millis = fraction.parse().ok()?;
        }
    }
    let offset_ms: i64 = match bytes.get(index) {
        None | Some(&b'Z') | Some(&b'z') => 0,
        Some(&b'+') | Some(&b'-') => {
            let sign: i64 = if bytes[index] == b'-' { -1 } else { 1 };
            if bytes.len() < index + 3 {
                return None;
            }
            let offset_hour: i64 = text.get(index + 1..index + 3)?.parse().ok()?;
            index += 3;
            let offset_minute: i64 = if bytes.get(index) == Some(&b':') {
                index += 1;
                if bytes.len() < index + 2 {
                    return None;
                }
                text.get(index..index + 2)?.parse().ok()?
            } else if bytes.len() >= index + 2 {
                text.get(index..index + 2)?.parse().ok()?
            } else {
                0
            };
            sign * (offset_hour * 3_600_000 + offset_minute * 60_000)
        }
        _ => return None,
    };
    if hour > 24 || minute > 59 || second > 60 {
        return None;
    }
    total_ms += hour as i64 * 3_600_000 + minute as i64 * 60_000 + second as i64 * 1000 + millis;
    Some(total_ms - offset_ms)
}

/// `toTimestamp`: falsy values are null; numbers below 1e12 are seconds;
/// strings parse as dates.
/// 中文说明：0、空串与其它类型视为无时间戳；数值小于 1e12 认为是秒并 ×1000，
/// 字符串走 [`parse_iso_ms`]。
pub fn to_timestamp(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(n) => {
            let raw = n.as_f64()?;
            if raw == 0.0 {
                return None;
            }
            Some(if raw < 1_000_000_000_000.0 {
                (raw * 1000.0) as i64
            } else {
                raw as i64
            })
        }
        Value::String(s) if !s.is_empty() => parse_iso_ms(s),
        _ => None,
    }
}

/// `normalizeTimestamp`: numbers only; seconds below 1e12 are scaled.
/// 中文说明：仅接受数值输入；秒（<1e12）换算为毫秒，毫秒原样返回，否则 `None`。
pub fn normalize_timestamp(value: &Value) -> Option<i64> {
    let raw = value.as_f64()?;
    Some(if raw < 1_000_000_000_000.0 {
        (raw * 1000.0) as i64
    } else {
        raw as i64
    })
}

/// JS `new Date(value).getTime()` for number/string inputs (numbers are
/// already milliseconds — no scaling).
/// 中文说明：数值已是毫秒不做缩放；字符串按 [`parse_iso_ms`] 解析；其它类型 `None`。
pub fn date_to_ms(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => Some(n.as_f64()? as i64),
        Value::String(s) if !s.is_empty() => parse_iso_ms(s),
        _ => None,
    }
}

// ============== formatters.js ==============

/// Round a fixed-precision decimal string at `decimals` with JS `toFixed`
/// semantics (ties pick the larger integer; the sign always mirrors the
/// input, so small negatives keep a `-0.00` shape like JS).
///
/// 中文说明：在十进制字符串上按位数舍入并补齐小数位，正负号跟随输入，
/// 因此 `-0.125` 舍两位得 `-0.12`（与 JS 的进位方向一致）。
fn round_decimal_string(text: &str, decimals: usize) -> String {
    let negative = text.starts_with('-');
    let body = text.trim_start_matches(['-', '+']);
    let (int_part, fraction) = match body.split_once('.') {
        Some((int, frac)) => (int, frac),
        None => (body, ""),
    };
    if fraction.len() <= decimals {
        let mut out = text.to_string();
        if decimals > 0 && !out.contains('.') {
            out.push_str(&".0".repeat(decimals));
        }
        return out;
    }
    let kept: Vec<u8> = fraction.as_bytes()[..decimals].to_vec();
    let rest = &fraction.as_bytes()[decimals..];
    let round_up = match rest[0] {
        // Exact tie: JS toFixed picks the larger n (toward +infinity).
        b'5' if rest[1..].iter().all(|&b| b == b'0') => !negative,
        // Anything at or past the half (including 5 with a non-zero tail).
        c if c >= b'5' => true,
        _ => false,
    };
    let mut digits: Vec<u8> = int_part.as_bytes().to_vec();
    digits.extend(kept);
    if round_up {
        let mut i = digits.len();
        loop {
            if i == 0 {
                digits.insert(0, b'1');
                break;
            }
            i -= 1;
            if digits[i] == b'9' {
                digits[i] = b'0';
            } else {
                digits[i] += 1;
                break;
            }
        }
    }
    let int_len = int_part.len().max(digits.len().saturating_sub(decimals));
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(std::str::from_utf8(&digits[..int_len]).unwrap_or("0"));
    if decimals > 0 {
        out.push('.');
        out.push_str(std::str::from_utf8(&digits[int_len..]).unwrap_or("0"));
    }
    out
}

/// JS `value.toFixed(digits)` for finite floats.
/// 中文说明：先以高精度格式化取回十进制字符串，再交给 [`round_decimal_string`] 舍入，
/// 规避二进制浮点本身的表示偏差。
pub fn js_to_fixed(value: f64, digits: usize) -> String {
    let hint = format!("{:.*}", digits + 8, value);
    round_decimal_string(&hint, digits)
}

/// `formatMoney`: fixed 2-decimal money label; null for non-numbers.
/// 中文说明：非有限值或 `None` 返回 `None`，否则输出固定两位小数的金额字符串。
pub fn format_money(value: Option<f64>) -> Option<String> {
    let value = value?;
    if !value.is_finite() {
        return None;
    }
    Some(js_to_fixed(value, 2))
}

/// `formatResetTime`: en-US/UTC label — time-only when the reset is today,
/// otherwise `Mon D, Wkd, h:MM AM/PM`.
///
/// 中文说明：以 UTC 日期判定"今天"——重置时刻与 now 同一天时只给时刻，
/// 否则输出 `Mon D, Wkd, h:MM AM/PM`，保证字段确定性。
pub fn format_reset_time(timestamp_ms: i64, now_ms: u64) -> Option<String> {
    let days = timestamp_ms.div_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    let time_ms = timestamp_ms.rem_euclid(86_400_000);
    let hour = (time_ms / 3_600_000) as u32;
    let minute = ((time_ms % 3_600_000) / 60_000) as u32;

    let now_days = (now_ms as i64).div_euclid(86_400_000);
    let (now_year, now_month, now_day) = civil_from_days(now_days);

    if (year, month, day) == (now_year, now_month, now_day) {
        return Some(hour_minute_ampm(hour, minute));
    }
    let weekday = WEEKDAYS[weekday_from_days(days)];
    Some(format!(
        "{} {}, {}, {}",
        MONTHS[(month - 1) as usize],
        day,
        weekday,
        hour_minute_ampm(hour, minute)
    ))
}

/// 判断 resetAt 是否携带可解析的时间戳：非空字符串或数字为真；
/// `null`、空串与其它类型为假（对齐 JS 的真值判断）。
fn has_reset_timestamp(reset_at: Option<&Value>) -> bool {
    match reset_at {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(_)) => true,
        Some(_) => false,
    }
}

/// The reset instant in epoch ms when JS would parse one
/// (`new Date(resetAt).getTime()`), for the derived fields.
///
/// 中文说明：用 [`date_to_ms`] 把 resetAt 折算为 epoch 毫秒，供派生字段计算。
fn reset_instant(reset_at: Option<&Value>) -> Option<i64> {
    date_to_ms(reset_at?)
}

/// `calculateResetAfterSeconds`.
/// 中文说明：距重置的整秒数，向下取整且不为负（已过期返回 0）；
/// resetAt 不可解析时为 `None`。
pub fn calculate_reset_after_seconds(reset_at: Option<&Value>, now_ms: u64) -> Option<i64> {
    if !has_reset_timestamp(reset_at) {
        return None;
    }
    let instant = reset_instant(reset_at)?;
    let delta = (instant - now_ms as i64).div_euclid(1000);
    Some(delta.max(0))
}

/// `toUsageWindow` — the unified window shape every provider emits.
///
/// 中文说明：所有 provider 共用的窗口构造器——`usedPercent`（须有限）、
/// `remainingPercent`（= 100 - used，下限 0）、`windowSeconds`、
/// `resetAfterSeconds`、`resetAt` 原样回填、格式化时间双字段及可选
/// `valueLabel`；缺失信息一律写 `null` 保持键存在。
#[allow(clippy::too_many_arguments)]
pub fn to_usage_window(
    now_ms: u64,
    used_percent: Option<f64>,
    window_seconds: Option<f64>,
    reset_at: Option<&Value>,
    value_label: Option<&str>,
) -> Value {
    let has_finite_used = used_percent.filter(|v| v.is_finite());
    let remaining_percent = has_finite_used.map(|v| (100.0 - v).max(0.0));
    let reset_after_seconds = calculate_reset_after_seconds(reset_at, now_ms);
    let reset_formatted = if has_reset_timestamp(reset_at) {
        reset_instant(reset_at).and_then(|ms| format_reset_time(ms, now_ms))
    } else {
        None
    };

    let mut map = Map::new();
    match has_finite_used {
        Some(used) => {
            map.insert("usedPercent".into(), num(used));
        }
        None => {
            map.insert("usedPercent".into(), Value::Null);
        }
    }
    if let (Some(_), Some(remaining)) = (has_finite_used, remaining_percent) {
        map.insert("remainingPercent".into(), num(remaining));
    } else {
        map.insert("remainingPercent".into(), Value::Null);
    }
    match window_seconds {
        Some(seconds) => {
            map.insert("windowSeconds".into(), num(seconds));
        }
        None => {
            map.insert("windowSeconds".into(), Value::Null);
        }
    }
    match reset_after_seconds {
        Some(seconds) => {
            map.insert("resetAfterSeconds".into(), json!(seconds));
        }
        None => {
            map.insert("resetAfterSeconds".into(), Value::Null);
        }
    }
    map.insert("resetAt".into(), reset_at.cloned().unwrap_or(Value::Null));
    match &reset_formatted {
        Some(formatted) => {
            map.insert("resetAtFormatted".into(), json!(formatted));
            map.insert("resetAfterFormatted".into(), json!(formatted));
        }
        None => {
            map.insert("resetAtFormatted".into(), Value::Null);
            map.insert("resetAfterFormatted".into(), Value::Null);
        }
    }
    if let Some(label) = value_label
        && !label.is_empty()
    {
        map.insert("valueLabel".into(), json!(label));
    }
    Value::Object(map)
}

/// `buildResult` — the unified provider result envelope, key order preserved.
///
/// 中文说明：统一结果信封——`providerId/providerName/ok/configured/
/// usage/fetchedAt` 恒有，`error`/`planLabel` 仅在非空时写入。
#[allow(clippy::too_many_arguments)]
pub fn build_result(
    provider_id: &str,
    provider_name: &str,
    ok: bool,
    configured: bool,
    usage: Option<Value>,
    error: Option<&str>,
    plan_label: Option<&str>,
    now_ms: u64,
) -> Value {
    let mut map = Map::new();
    map.insert("providerId".into(), json!(provider_id));
    map.insert("providerName".into(), json!(provider_name));
    map.insert("ok".into(), json!(ok));
    map.insert("configured".into(), json!(configured));
    map.insert("usage".into(), usage.unwrap_or(Value::Null));
    if let Some(error) = error.filter(|e| !e.is_empty()) {
        map.insert("error".into(), json!(error));
    }
    if let Some(plan) = plan_label.filter(|p| !p.is_empty()) {
        map.insert("planLabel".into(), json!(plan));
    }
    map.insert("fetchedAt".into(), json!(now_ms));
    Value::Object(map)
}

/// Convenience: usage payload `{ windows, models? }`.
/// 中文说明：`{ windows, models? }` 的便捷组装；models 为空时整个键省略。
pub fn usage_payload(windows: Map<String, Value>, models: Option<Map<String, Value>>) -> Value {
    let mut map = Map::new();
    map.insert("windows".into(), Value::Object(windows));
    if let Some(models) = models.filter(|m| !m.is_empty()) {
        map.insert("models".into(), Value::Object(models));
    }
    Value::Object(map)
}

// ============== transformers.js ==============

/// z.ai limit 的 unit 代码 → 秒数映射：3=小时（3600）、6=周（604800）。
const ZAI_TOKEN_WINDOW_SECONDS: &[(f64, f64)] = &[(3.0, 3_600.0), (6.0, 604_800.0)];

/// `resolveWindowSeconds` — z.ai limit unit × number as window seconds.
/// 中文说明：读取 limit 的 `number` 与 `unit`，二者缺一、number 为 0 或
/// unit 不在已知映射中都返回 `None`。
pub fn resolve_window_seconds(limit: &Value) -> Option<f64> {
    let number = to_number(field(limit, "number"))?;
    if number == 0.0 {
        return None;
    }
    let unit = to_number(field(limit, "unit"))?;
    let unit_seconds = ZAI_TOKEN_WINDOW_SECONDS
        .iter()
        .find(|(known, _)| *known == unit)
        .map(|(_, seconds)| *seconds)?;
    Some(unit_seconds * number)
}

/// `resolveWindowLabel` — `'tokens'` fallback, weekly/`Nd`/`Nh`/`Ns` labels.
/// 中文说明：无秒数回退 `tokens`；整周输出 `weekly`（7 天）或 `Nd`，
/// 整小时输出 `Nh`，其余输出 `Ns`。
pub fn resolve_window_label(window_seconds: Option<f64>) -> String {
    let Some(seconds) = window_seconds else {
        return "tokens".to_string();
    };
    if seconds != 0.0 && seconds % 86_400.0 == 0.0 {
        let days = seconds / 86_400.0;
        return if days == 7.0 {
            "weekly".to_string()
        } else {
            format!("{}d", num_str(days))
        };
    }
    if seconds != 0.0 && seconds % 3_600.0 == 0.0 {
        return format!("{}h", num_str(seconds / 3_600.0));
    }
    format!("{}s", num_str(seconds))
}

/// `durationToLabel` — Kimi `window.duration` + `timeUnit` → label.
/// 中文说明：Kimi 的 duration+timeUnit → `Nm`/`Nh`/`Nd` 标签；
/// 缺失、为 0 或未知单位时回退 `limit`。
pub fn duration_to_label(duration: Option<f64>, unit: Option<&str>) -> String {
    let (Some(duration), Some(unit)) = (duration, unit) else {
        return "limit".to_string();
    };
    if duration == 0.0 {
        return "limit".to_string();
    }
    match unit {
        "TIME_UNIT_MINUTE" => format!("{}m", num_str(duration)),
        "TIME_UNIT_HOUR" => format!("{}h", num_str(duration)),
        "TIME_UNIT_DAY" => format!("{}d", num_str(duration)),
        _ => "limit".to_string(),
    }
}

/// `durationToSeconds` — Kimi `window.duration` + `timeUnit` → seconds.
/// 中文说明：Kimi 的 duration+timeUnit 折算为秒（分/时/日三档）；
/// duration 为 0 或未知单位返回 `None`。
pub fn duration_to_seconds(duration: Option<f64>, unit: Option<&str>) -> Option<f64> {
    let (duration, unit) = (duration?, unit?);
    if duration == 0.0 {
        return None;
    }
    match unit {
        "TIME_UNIT_MINUTE" => Some(duration * 60.0),
        "TIME_UNIT_HOUR" => Some(duration * 3_600.0),
        "TIME_UNIT_DAY" => Some(duration * 86_400.0),
        _ => None,
    }
}

// ============== auth.js ==============

/// `getAuthEntry` — first alias present in the auth file.
/// 中文说明：按给定顺序在 auth 对象里取第一个存在的条目（不递归）。
pub fn get_auth_entry<'a>(auth: &'a Value, aliases: &[&str]) -> Option<&'a Value> {
    aliases.iter().find_map(|alias| field(auth, alias))
}

/// `normalizeAuthEntry` — string entries become `{ token }`; objects pass
/// through; anything else is null.
/// 中文说明：字符串条目包装为 `{ token }`；对象/数组原样克隆；
/// `null` 与其它类型归为 `None`。
pub fn normalize_auth_entry(entry: Option<&Value>) -> Option<Value> {
    match entry? {
        Value::Null => None,
        Value::String(token) => Some(json!({ "token": token })),
        value @ (Value::Object(_) | Value::Array(_)) => Some(value.clone()),
        _ => None,
    }
}

/// `readJsonFile` — missing/empty/unparseable files read as null.
/// 中文说明：文件缺失、内容为空白或 JSON 解析失败都返回 `None`。
pub fn read_json_file(path: &Path) -> Option<Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str(trimmed).ok()
}

/// OpenCode config locations (`ANTIGRAVITY_ACCOUNTS_PATHS` precedent):
/// `~/.config/opencode` and `~/.local/share/opencode`.
/// 中文说明：`~/.config/opencode`；home 不可得时为 `None`。
pub fn opencode_config_dir(deps: &QuotaDeps) -> Option<std::path::PathBuf> {
    (deps.home_dir)().map(|home| home.join(".config").join("opencode"))
}

/// OpenCode 数据目录 `~/.local/share/opencode`；home 不可得时为 `None`。
pub fn opencode_data_dir(deps: &QuotaDeps) -> Option<std::path::PathBuf> {
    (deps.home_dir)().map(|home| home.join(".local").join("share").join("opencode"))
}

/// Antigravity accounts candidates in JS resolution order.
/// 中文说明：按 JS 解析顺序依次给出 config 目录与 data 目录下的
/// `antigravity-accounts.json` 两个候选路径。
pub fn antigravity_accounts_paths(deps: &QuotaDeps) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    if let Some(config) = opencode_config_dir(deps) {
        paths.push(config.join("antigravity-accounts.json"));
    }
    if let Some(data) = opencode_data_dir(deps) {
        paths.push(data.join("antigravity-accounts.json"));
    }
    paths
}

/// Insert-or-replace helper preserving insertion order (BTreeMap-free).
/// 中文说明：向 windows map 插入或覆盖一个键；serde_json 的 Map 保持插入序。
pub fn set_window(windows: &mut Map<String, Value>, key: &str, value: Value) {
    windows.insert(key.to_string(), value);
}

/// Clone a JSON object map out of a value (`{ ...obj }` spread on objects).
/// 中文说明：值是对象时克隆其 map，否则返回空 map（JS 对对象做 `{ ...obj }` 展开的等价写法）。
pub fn object_clone(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

/// utils 的行为单元测试（对齐 JS 语义的边界样例）。
#[cfg(test)]
mod tests {
    use super::*;

    /// 验证 `to_number`：数字/数字字符串（含首尾空白、空串→0）可解析，
    /// 非法串、布尔与 null 为 None。
    #[test]
    fn to_number_parses_numbers_and_numeric_strings() {
        assert_eq!(to_number(Some(&json!(3))), Some(3.0));
        assert_eq!(to_number(Some(&json!("12.5"))), Some(12.5));
        assert_eq!(to_number(Some(&json!("  7 "))), Some(7.0));
        assert_eq!(to_number(Some(&json!(""))), Some(0.0));
        assert_eq!(to_number(Some(&json!("12abc"))), None);
        assert_eq!(to_number(Some(&json!(true))), None);
        assert_eq!(to_number(Some(&Value::Null)), None);
    }

    /// 验证 `num` 把整数值浮点序列化为不带小数部分的整数。
    #[test]
    fn num_serializes_integer_valued_floats_without_fraction() {
        assert_eq!(num(36.0).to_string(), "36");
        assert_eq!(num(24.5).to_string(), "24.5");
        assert_eq!(num(100.0).to_string(), "100");
    }

    /// 验证 `format_money` 固定两位小数，NaN/None 返回 None。
    #[test]
    fn format_money_matches_to_fixed_two() {
        assert_eq!(format_money(Some(7.54)), Some("7.54".to_string()));
        assert_eq!(format_money(Some(12.3456)), Some("12.35".to_string()));
        assert_eq!(format_money(Some(0.0)), Some("0.00".to_string()));
        assert_eq!(format_money(None), None);
        assert_eq!(format_money(Some(f64::NAN)), None);
    }

    /// 验证 `js_to_fixed` 的负数舍入方向与向上进位（9.999 → 10.00）。
    #[test]
    fn js_to_fixed_handles_negative_and_carry() {
        assert_eq!(js_to_fixed(-1.005, 2), "-1.00");
        assert_eq!(js_to_fixed(9.999, 2), "10.00");
        assert_eq!(js_to_fixed(2674.8724080324173, 0), "2675");
    }

    /// 验证窗口标签规则：无值 → tokens、一周 → weekly、整天 → Nd、整小时 → Nh、其余 → Ns。
    #[test]
    fn resolve_window_labels_durations() {
        assert_eq!(resolve_window_label(None), "tokens");
        assert_eq!(resolve_window_label(Some(604_800.0)), "weekly");
        assert_eq!(resolve_window_label(Some(18_000.0)), "5h");
        assert_eq!(resolve_window_label(Some(86_400.0 * 2.0)), "2d");
        assert_eq!(resolve_window_label(Some(90.0)), "90s");
    }

    /// 验证 z.ai unit 代码 3/6 分别换算为小时/周窗口，未知 unit 或缺 number 为 None。
    #[test]
    fn resolve_window_seconds_maps_zai_units() {
        let limit = json!({ "unit": 3, "number": 5 });
        assert_eq!(resolve_window_seconds(&limit), Some(18_000.0));
        let weekly = json!({ "unit": 6, "number": 1 });
        assert_eq!(resolve_window_seconds(&weekly), Some(604_800.0));
        assert_eq!(
            resolve_window_seconds(&json!({ "unit": 9, "number": 1 })),
            None
        );
        assert_eq!(resolve_window_seconds(&json!({ "unit": 3 })), None);
    }

    /// 验证 `to_timestamp`：秒自动 ×1000、毫秒原样、ISO 字符串解析；0/空串/非法输入为 None。
    #[test]
    fn to_timestamp_scales_seconds_and_parses_iso() {
        assert_eq!(
            to_timestamp(Some(&json!(1_784_491_827))),
            Some(1_784_491_827_000)
        );
        assert_eq!(
            to_timestamp(Some(&json!(1_784_491_827_000i64))),
            Some(1_784_491_827_000)
        );
        assert_eq!(
            to_timestamp(Some(&json!("2026-08-12T12:00:00.000Z"))),
            Some(1_786_536_000_000)
        );
        assert_eq!(
            to_timestamp(Some(&json!("2026-08-14T19:10:00.313090+00:00"))),
            Some(1_786_734_600_313)
        );
        assert_eq!(to_timestamp(Some(&json!(0))), None);
        assert_eq!(to_timestamp(Some(&json!(""))), None);
        assert_eq!(to_timestamp(Some(&json!("not-a-date"))), None);
    }

    /// 验证 `parse_iso_ms` 支持纯日期、任意位数小数秒与时区偏移，垃圾输入返回 None。
    #[test]
    fn parse_iso_ms_supports_date_only_and_fraction_digits() {
        assert_eq!(parse_iso_ms("2026-08-12"), Some(1_786_492_800_000));
        assert_eq!(
            parse_iso_ms("2026-08-04T06:21:48.514003Z"),
            Some(1_785_824_508_514)
        );
        assert_eq!(
            parse_iso_ms("2026-08-04T06:21:48+02:00"),
            Some(1_785_817_308_000)
        );
        assert_eq!(parse_iso_ms("garbage"), None);
    }

    /// 验证 usage window 派生字段：remaining = 100-used（钳 0）、resetAfterSeconds
    /// 与格式化时间正确生成，缺失输入写 null。
    #[test]
    fn to_usage_window_derives_remaining_and_reset_fields() {
        let now = 1_000_000_000_u64;
        let window = to_usage_window(
            now,
            Some(60.0),
            Some(18_000.0),
            Some(&json!(now + 60_000)),
            None,
        );
        assert_eq!(window["remainingPercent"], json!(40));
        assert_eq!(window["resetAfterSeconds"], json!(60));
        assert!(window["resetAtFormatted"].is_string());

        let clamped = to_usage_window(now, Some(110.0), None, None, None);
        assert_eq!(clamped["remainingPercent"], json!(0));

        let missing = to_usage_window(now, None, None, None, None);
        assert_eq!(missing["remainingPercent"], Value::Null);
        assert_eq!(missing["usedPercent"], Value::Null);
        assert_eq!(missing["windowSeconds"], Value::Null);
    }

    /// 验证 `build_result` 仅在 error/planLabel 非空时写入对应键，usage 缺省为 null。
    #[test]
    fn build_result_includes_optional_keys_only_when_truthy() {
        let result = build_result(
            "codex",
            "Codex",
            false,
            false,
            None,
            Some("Unsupported provider"),
            None,
            42,
        );
        assert_eq!(result["providerId"], json!("codex"));
        assert_eq!(result["usage"], Value::Null);
        assert_eq!(result["error"], json!("Unsupported provider"));
        assert!(result.get("planLabel").is_none());
        assert_eq!(result["fetchedAt"], json!(42));

        let ok_result = build_result(
            "zai-coding-plan",
            "z.ai",
            true,
            true,
            Some(json!({"windows": {}})),
            None,
            Some("pro"),
            7,
        );
        assert_eq!(ok_result["planLabel"], json!("pro"));
        assert!(ok_result.get("error").is_none());
    }

    /// 验证 `normalize_auth_entry`：字符串 token 包装为 { token }，对象透传，null/数字为 None。
    #[test]
    fn normalize_auth_entry_wraps_string_tokens() {
        assert_eq!(
            normalize_auth_entry(Some(&json!("tok"))),
            Some(json!({ "token": "tok" }))
        );
        let entry = json!({ "access": "a" });
        assert_eq!(normalize_auth_entry(Some(&entry)), Some(entry));
        assert_eq!(normalize_auth_entry(Some(&Value::Null)), None);
        assert_eq!(normalize_auth_entry(Some(&json!(42))), None);
    }

    /// 验证 `get_auth_entry` 按别名顺序优先命中第一个存在的条目。
    #[test]
    fn get_auth_entry_prefers_first_alias() {
        let auth = json!({ "kimi": { "key": "k" }, "kimi-for-coding": { "key": "kf" } });
        let entry = get_auth_entry(&auth, &["kimi-for-coding", "kimi"]).unwrap();
        assert_eq!(entry["key"], json!("kf"));
        assert!(get_auth_entry(&auth, &["missing"]).is_none());
    }

    /// 验证 `format_reset_time`：重置时刻在"今天"只显示时刻，跨天输出 `Mon D, Wkd, h:MM AM/PM`。
    #[test]
    fn format_reset_time_switches_between_today_and_future() {
        let now = 1_786_492_800_000_u64; // 2026-08-12T00:00:00Z
        assert_eq!(
            format_reset_time((now + 3_600_000) as i64, now).as_deref(),
            Some("1:00 AM")
        );
        let later = format_reset_time(1_786_579_200_000, now).unwrap(); // 2026-08-13T00:00:00Z
        assert_eq!(later, "Aug 13, Thu, 12:00 AM");
    }

    /// 验证 Kimi duration/timeUnit 的标签与秒数换算，未知单位分别回退 limit / None。
    #[test]
    fn duration_helpers_map_kimi_units() {
        assert_eq!(
            duration_to_label(Some(300.0), Some("TIME_UNIT_MINUTE")),
            "300m"
        );
        assert_eq!(
            duration_to_seconds(Some(300.0), Some("TIME_UNIT_MINUTE")),
            Some(18_000.0)
        );
        assert_eq!(duration_to_label(None, None), "limit");
        assert_eq!(duration_to_seconds(Some(5.0), Some("TIME_UNIT_WEEK")), None);
    }

    /// 验证 `read_json_file` 把缺失、空白与损坏 JSON 文件都读作 None，合法文件正常解析。
    #[test]
    fn read_json_file_treats_missing_and_broken_files_as_null() {
        let dir =
            std::env::temp_dir().join(format!("ompchamber-quota-utils-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.json");
        assert!(read_json_file(&path).is_none());
        std::fs::write(&path, "  ").unwrap();
        assert!(read_json_file(&path).is_none());
        std::fs::write(&path, "{\"a\":1}").unwrap();
        assert_eq!(read_json_file(&path), Some(json!({"a": 1})));
        std::fs::write(&path, "{broken").unwrap();
        assert!(read_json_file(&path).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 验证 `round_decimal_string` 的十进制舍入语义（含 0.125 的 JS 进位方向与负号处理）。
    #[test]
    fn round_decimal_string_keeps_precision_hints() {
        assert_eq!(round_decimal_string("12.345678901234", 2), "12.35");
        assert_eq!(round_decimal_string("0.12500000000", 2), "0.13");
        assert_eq!(round_decimal_string("-0.12500000000", 2), "-0.12");
    }
}
