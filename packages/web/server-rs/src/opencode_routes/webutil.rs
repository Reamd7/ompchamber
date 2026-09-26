//! Small JS-semantics helpers shared by the opencode route ports: path
//! resolution, query-string reading with express "first value wins"
//! semantics, JS truthiness/`String()` coercions, and wall-clock millis.

use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// `path.resolve(...)` — lexical absolutization (no symlink resolution):
/// make absolute against the CWD, then normalize `.`/`..` components.
pub(crate) fn resolve_path(input: &str) -> PathBuf {
    let trimmed = input.trim();
    let joined = if Path::new(trimmed).is_absolute() {
        PathBuf::from(trimmed)
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(trimmed)
    };
    let mut output = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // `..` at a root boundary stays at the root (JS path.resolve).
                if !output.pop() {
                    output.push(component.as_os_str());
                }
            }
            other => output.push(other.as_os_str()),
        }
    }
    output
}

/// Wall-clock milliseconds since the Unix epoch (`Date.now()`).
pub(crate) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Express `req.query.<key>` with an array value: the JS handlers take the
/// first entry. `form_urlencoded` over the raw query, first occurrence wins.
pub(crate) fn first_query_value(uri: &axum::http::Uri, key: &str) -> Option<String> {
    let query = uri.query()?;
    for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if name == key {
            return Some(value.into_owned());
        }
    }
    None
}

/// Header lookup returning the raw string (`req.get(...)`).
pub(crate) fn header_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// JS truthiness for a JSON value: `false`, `0`, `""`, and `null` are falsy.
pub(crate) fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|n| n != 0.0).unwrap_or(true),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// JS `String(value)` for JSON values (`String(123) === "123"`,
/// `String([1, null]) === "1,"`, `String({}) === "[object Object]"`).
pub(crate) fn js_to_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::Null | Value::Bool(false) => String::new(),
                other => js_to_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// `Object.keys(value)` parity for the shapes that reach the `fields`
/// computations: objects list keys, arrays/strings list indices, anything
/// else has none.
pub(crate) fn object_keys(value: &Value) -> Vec<String> {
    match value {
        Value::Object(map) => map.keys().cloned().collect(),
        Value::Array(items) => (0..items.len()).map(|i| i.to_string()).collect(),
        Value::String(text) => (0..text.chars().count()).map(|i| i.to_string()).collect(),
        _ => Vec::new(),
    }
}

/// Engine URL (`network-runtime.js` `buildOpenCodeUrl` with the default empty
/// API prefix). Mirrors the JS throw when the engine port is unknown.
pub(crate) fn engine_url(
    ctx: &crate::context::RouterContext,
    path: &str,
) -> Result<String, String> {
    let base = ctx
        .engine
        .base_url()
        .ok_or_else(|| "OpenCode port is not available".to_string())?;
    let normalized_path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    Ok(format!("{}{}", base.trim_end_matches('/'), normalized_path))
}
