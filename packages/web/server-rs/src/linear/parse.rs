//! Port of `server/lib/linear/parse.js` — untyped JSON value helpers that
//! mirror the JS coercions (`isPlainObject`, `readTrimmedString`,
//! `readFiniteNumber`, `readEnv`, `isString`).

use serde_json::Value;

/// JS `isPlainObject`.
pub fn is_plain_object(value: &Value) -> bool {
    value.is_object()
}

/// `isPlainObject(x) ? x : null` — a plain-object reference or nothing.
pub fn as_plain_object(value: &Value) -> Option<&Value> {
    is_plain_object(value).then_some(value)
}

/// JS `isString` (JSON strings only).
pub fn is_string(value: &Value) -> bool {
    value.is_string()
}

/// JS `readTrimmedString`: strings trim to a non-empty value or `''`.
pub fn read_trimmed_string(value: &Value) -> String {
    match value {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                String::new()
            } else {
                trimmed.to_string()
            }
        }
        _ => String::new(),
    }
}

/// JS `readFiniteNumber`: JSON numbers are always finite; anything else is
/// `null`.
pub fn read_finite_number(value: &Value) -> Option<f64> {
    value.as_f64()
}

/// JS truthiness for parsed JSON values (`0`, `''`, `null`, `false` are falsy).
pub fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|v| v != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(_) => true,
    }
}

/// JS `readEnv`: trimmed env value, `''` when unset or whitespace.
pub fn read_env_from(
    map: Option<&std::collections::HashMap<String, String>>,
    name: &str,
) -> String {
    match map {
        Some(overrides) => overrides
            .get(name)
            .map(|v| v.trim().to_string())
            .unwrap_or_default(),
        None => std::env::var(name)
            .map(|v| v.trim().to_string())
            .unwrap_or_default(),
    }
}

/// Serialize an f64 the way `JSON.stringify` renders a JS number: integral
/// values print without a decimal point.
pub fn num(value: f64) -> Value {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9.0e15 {
        Value::from(value as i64)
    } else {
        Value::from(value)
    }
}

/// JS `Number.isFinite(x) ? x : null` with JS `||` fallback semantics (0 and
/// NaN are falsy).
pub fn truthy_num(value: Option<f64>) -> Option<f64> {
    value.filter(|v| *v != 0.0 && !v.is_nan())
}
