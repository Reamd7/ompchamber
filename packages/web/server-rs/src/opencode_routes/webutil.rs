//! Small JS-semantics helpers shared by the opencode route ports: path
//! resolution, query-string reading with express "first value wins"
//! semantics, JS truthiness/`String()` coercions, and wall-clock millis.
//!
//! 中文说明：opencode 路由移植共享的 JS 语义工具集——Node
//! `path.resolve` 风格的词法路径解析、express "首个值优先" 的 query
//! 读取、JS 真值与 `String()` 强转、`Object.keys` 对齐，以及引擎
//! URL 拼接。它们让 Rust 端逐字节复刻 JS 行为，供各路由模块复用。

use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// `path.resolve(...)` — lexical absolutization (no symlink resolution):
/// make absolute against the CWD, then normalize `.`/`..` components.
/// 中文：对应 Node `path.resolve(input)`：先 trim，绝对路径原样、相对
/// 路径拼到当前工作目录，再词法归一化（消解 `.`/`..`，不解析符号链接、
/// 不触碰文件系统）。
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
/// 中文：Unix epoch 起的墙钟毫秒数，等价于 JS `Date.now()`，供
/// TTL/过期判断使用。
pub(crate) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Express `req.query.<key>` with an array value: the JS handlers take the
/// first entry. `form_urlencoded` over the raw query, first occurrence wins.
/// 中文：读取 query 参数的首个取值（express 数组 query 取第一项的
/// 语义）；直接用 `form_urlencoded` 解析原始 query 串，遇到重复键时
/// 首次出现者胜出。
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
/// 中文：按名称读取请求头的原始字符串值（对应 `req.get(name)`），
/// 非 ASCII 值会被丢弃（`to_str` 失败返回 `None`）。
pub(crate) fn header_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// JS truthiness for a JSON value: `false`, `0`, `""`, and `null` are falsy.
/// 中文：JSON 值的 JS 真值判定：`null`、`false`、`0`、`""` 为假，
/// 其余（含空数组/空对象）为真。
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
/// 中文：JSON 值的 JS `String()` 强转结果，逐例对齐 JS 语义
/// （数字十进制、数组逗号连接并把 null/undefined 变空串、对象恒为
/// "[object Object]"），供配置归一化保持与 JS 相同的输出。
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
/// 中文：对齐 JS `Object.keys` 在本模块遇到的数据形态上的行为：
/// 对象列出键、数组与字符串列出下标、其余返回空。
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
/// 中文：拼接 OpenCode 引擎 URL（`network-runtime.js` 的
/// `buildOpenCodeUrl`，API 前缀为空）。引擎端口未知时镜像 JS 抛错行为
/// 返回 `Err`。
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
