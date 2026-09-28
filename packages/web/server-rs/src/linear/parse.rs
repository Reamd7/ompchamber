//! Port of `server/lib/linear/parse.js` — untyped JSON value helpers that
//! mirror the JS coercions (`isPlainObject`, `readTrimmedString`,
//! `readFiniteNumber`, `readEnv`, `isString`).
//! 本模块是 `server/lib/linear/parse.js` 的 Rust 移植：一组无类型 JSON
//! 值辅助函数，逐一复刻 JS 的隐式转换语义（isPlainObject、
//! readTrimmedString、readFiniteNumber、readEnv、isString 等）。

use serde_json::Value;

/// 对应 JS isPlainObject：值是否为 JSON 对象。
/// JS `isPlainObject`.
pub fn is_plain_object(value: &Value) -> bool {
    value.is_object()
}

/// 对应 JS isPlainObject(x) ? x : null：是对象则返回引用，否则 None。
/// `isPlainObject(x) ? x : null` — a plain-object reference or nothing.
pub fn as_plain_object(value: &Value) -> Option<&Value> {
    is_plain_object(value).then_some(value)
}

/// 对应 JS isString：值是否为 JSON 字符串。
/// JS `isString` (JSON strings only).
pub fn is_string(value: &Value) -> bool {
    value.is_string()
}

/// 对应 JS readTrimmedString：字符串 trim 后非空则返回 trim 结果，
/// 其余情况（含非字符串与全空白）返回空串。
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

/// 对应 JS readFiniteNumber：JSON 数字恒为有限值，直接 as_f64；
/// 其它类型返回 None。
/// JS `readFiniteNumber`: JSON numbers are always finite; anything else is
/// `null`.
pub fn read_finite_number(value: &Value) -> Option<f64> {
    value.as_f64()
}

/// 对应 JS 真值表：0、空串、null、false 为假，非空数组/对象为真。
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

/// 对应 JS readEnv：优先读覆盖表（测试注入），否则读进程环境变量；
/// 未设置或全空白时返回空串。
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

/// 按 JSON.stringify 渲染 JS 数字的规则序列化 f64：范围内的整数值
/// 序列化为整数（不带小数点）。
/// Serialize an f64 the way `JSON.stringify` renders a JS number: integral
/// values print without a decimal point.
pub fn num(value: f64) -> Value {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9.0e15 {
        Value::from(value as i64)
    } else {
        Value::from(value)
    }
}

/// 对应 JS Number.isFinite(x) ? x : null 加上 JS || 回退语义：
/// 0 与 NaN 视为假，返回 None。
/// JS `Number.isFinite(x) ? x : null` with JS `||` fallback semantics (0 and
/// NaN are falsy).
pub fn truthy_num(value: Option<f64>) -> Option<f64> {
    value.filter(|v| *v != 0.0 && !v.is_nan())
}
