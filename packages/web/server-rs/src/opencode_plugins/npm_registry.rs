//! Port of `server/lib/opencode/npm-registry.js` — npm package metadata with
//! a 1h TTL cache and in-flight deduplication.
//!
//! Gaps vs the JS (noted for PORT-MANIFEST): duplicate concurrent lookups for
//! the same name wait on a per-name mutex and then hit the cache instead of
//! sharing one in-flight request (same wire result, one request either way),
//! and the registry error text for transport failures is reqwest's rather
//! than Node's (`This operation was aborted`).
//!
//! 中文概述：npm registry 元数据客户端——1 小时 TTL 的进程内缓存，
//! 辅以按包名串行化的在途请求去重；成功结果与 404 会被缓存，
//! 网络/HTTP 错误不缓存以便立即重试。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

/// 缓存条目存活时间：1 小时（与 JS 版一致）。
const NPM_CACHE_TTL: Duration = Duration::from_secs(3_600);
/// 单次 registry 请求超时：5 秒。
const NPM_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// npm registry 固定 base URL。
const NPM_REGISTRY_BASE: &str = "https://registry.npmjs.org";

/// JS `NpmLookupResult`.
///
/// 中文：一次元数据查询的结果值——成功载荷，或区分网络层与 HTTP 层的错误。
#[derive(Debug, Clone)]
pub(crate) enum NpmInfo {
    /// 查询成功：dist-tags、版本列表与 latest 摘要。
    Ok {
        /// `dist-tags.latest` 的值；registry 未提供该 tag 时为 `None`。
        latest: Option<String>,
        /// 全部已发布版本号（字母序，仅用于成员判断）。
        versions: Vec<String>,
        /// `dist-tags` 映射：tag 名到版本号。
        dist_tags: HashMap<String, String>,
    },
    /// 查询失败：区分传输层故障与 registry 返回的 HTTP 错误。
    Err {
        /// `u16` HTTP status or the `network` sentinel.
        /// 中文：`true` 表示请求发送或响应解析失败的网络层故障，非 HTTP 状态错误。
        network: bool,
        /// 失败时的 HTTP 状态码；网络层故障无状态码，为 `None`。
        status: Option<u16>,
        /// 面向调用方的错误描述文本。
        error: String,
    },
}

/// 查询结果的便捷判定与字段访问（对应 JS 结果对象的同名字段）。
impl NpmInfo {
    /// 是否为成功结果。
    pub(crate) fn ok(&self) -> bool {
        matches!(self, NpmInfo::Ok { .. })
    }

    /// 错误描述文本；成功时返回空串。
    pub(crate) fn error_text(&self) -> &str {
        match self {
            NpmInfo::Err { error, .. } => error,
            NpmInfo::Ok { .. } => "",
        }
    }

    /// 失败时的 HTTP 状态码；成功或网络故障时为 `None`。
    pub(crate) fn status_code(&self) -> Option<u16> {
        match self {
            NpmInfo::Err { status, .. } => *status,
            NpmInfo::Ok { .. } => None,
        }
    }

    /// 是否为网络层故障（可与 registry 明确拒绝区分以决定重试策略）。
    pub(crate) fn is_network(&self) -> bool {
        matches!(self, NpmInfo::Err { network: true, .. })
    }

    /// `latest` dist-tag 的版本号；失败时为 `None`。
    pub(crate) fn latest(&self) -> Option<&str> {
        match self {
            NpmInfo::Ok { latest, .. } => latest.as_deref(),
            NpmInfo::Err { .. } => None,
        }
    }

    /// 全部版本号切片；失败时返回空切片。
    pub(crate) fn versions(&self) -> &[String] {
        match self {
            NpmInfo::Ok { versions, .. } => versions,
            NpmInfo::Err { .. } => &[],
        }
    }
}
/// 缓存条目：写入时间戳与查询结果，用于 TTL 新鲜度判定与命中克隆返回。
struct CacheEntry {
    /// 条目写入时间点，与 [`NPM_CACHE_TTL`] 比较判定是否过期。
    fetched_at: Instant,
    /// 被缓存的查询结果（成功或 404）。
    payload: NpmInfo,
}

/// 进程级结果缓存：包名到最近一次成功/404 结果的映射。
static CACHE: std::sync::LazyLock<Mutex<HashMap<String, CacheEntry>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// 按包名的 tokio 互斥锁表：同名包的并发查询排队串行，避免重复请求。
static INFLIGHT: std::sync::LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// 请求 User-Agent：编译期读取仓库根 package.json 的版本号拼成
/// `ompchamber-server/<版本>`，读取或解析失败时退化为 `ompchamber-server/dev`。
static USER_AGENT: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let package_json =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../package.json");
    match std::fs::read_to_string(&package_json)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|pkg| {
            pkg.get("version")
                .and_then(Value::as_str)
                .map(str::to_string)
        }) {
        Some(version) => format!("ompchamber-server/{version}"),
        None => "ompchamber-server/dev".to_string(),
    }
});

/// `encodeURIComponent(name).replace(/^%40/, '@')`
///
/// 中文：对包名做 URI 编码后把开头的 `%40` 还原为 `@`，
/// 使 scoped 包名编码为 `@scope%2Ffoo`（registry 路由要求的前缀保留）。
fn encode_name(name: &str) -> String {
    let encoded = super::http_util::encode_uri_component(name);
    encoded
        .strip_prefix("%40")
        .map(|rest| format!("@{rest}"))
        .unwrap_or(encoded)
}

/// 解析 registry 文档的 `dist-tags` 字段为 tag 到版本号的映射；
/// 字段缺失或非对象返回空映射，非字符串值被跳过。
fn parse_dist_tags(value: &Value) -> HashMap<String, String> {
    let Value::Object(map) = value else {
        return HashMap::new();
    };
    map.iter()
        .filter_map(|(tag, value)| value.as_str().map(|v| (tag.clone(), v.to_string())))
        .collect()
}

/// 解析 registry 文档的 `versions` 字段为版本号列表（取对象键）；
/// 非对象输入返回空表。键序与 JS 的插入序不同，但仅做成员判断。
fn parse_versions(value: &Value) -> Vec<String> {
    let Value::Object(map) = value else {
        return Vec::new();
    };
    // JS `Object.keys` yields insertion order; serde_json maps iterate
    // alphabetically. Membership checks are order-insensitive.
    map.keys().cloned().collect()
}

/// `lookupNpmPackage` — one direct registry fetch.
///
/// 中文：绕过缓存直接请求一次 registry。请求发送失败或 JSON 解析失败
/// 记为网络错误；404 记为“包不存在”；其余非 2xx 携带状态码返回。
/// 超时取 [`NPM_FETCH_TIMEOUT`]，User-Agent 取 [`USER_AGENT`]。
pub(crate) async fn lookup_npm_package(http: &reqwest::Client, name: &str) -> NpmInfo {
    let url = format!("{NPM_REGISTRY_BASE}/{}", encode_name(name));
    let request = http
        .get(&url)
        .header("user-agent", USER_AGENT.as_str())
        .header("accept", "application/json")
        .timeout(NPM_FETCH_TIMEOUT);
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            return NpmInfo::Err {
                network: true,
                status: None,
                error: error.to_string(),
            };
        }
    };
    let status = response.status().as_u16();
    if response.status().is_success() {
        let data = match response.json::<Value>().await {
            Ok(data) => data,
            Err(error) => {
                return NpmInfo::Err {
                    network: true,
                    status: None,
                    error: error.to_string(),
                };
            }
        };
        let dist_tags = parse_dist_tags(data.get("dist-tags").unwrap_or(&Value::Null));
        let latest = dist_tags.get("latest").cloned();
        return NpmInfo::Ok {
            latest,
            versions: parse_versions(data.get("versions").unwrap_or(&Value::Null)),
            dist_tags,
        };
    }
    if status == 404 {
        return NpmInfo::Err {
            network: false,
            status: Some(404),
            error: "Package not found".to_string(),
        };
    }
    NpmInfo::Err {
        network: false,
        status: Some(status),
        error: format!("Registry returned {status}"),
    }
}

/// `getNpmInfo(name, { forceRefresh })` — TTL cache + per-name serialization.
///
/// 中文：带 TTL 缓存的查询入口——除非 `force_refresh`，先查 [`CACHE`]
/// 命中且未过期即返回；未命中则获取该包名的在途锁串行执行，拿到锁后
/// 再复查一次缓存（排队期间他人可能已刷新），仍过期才真正请求并回写。
/// 仅成功结果与 404 回写缓存。
pub(crate) async fn get_npm_info(
    http: &reqwest::Client,
    name: &str,
    force_refresh: bool,
) -> NpmInfo {
    if !force_refresh
        && let Some(entry) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(name)
        && Instant::now().duration_since(entry.fetched_at) < NPM_CACHE_TTL
    {
        return entry.payload.clone();
    }

    let lock = {
        let mut guards = INFLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        guards.entry(name.to_string()).or_default().clone()
    };
    let _serial = lock.lock().await;

    // Another waiter may have refreshed the cache while we queued.
    if !force_refresh
        && let Some(entry) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(name)
        && Instant::now().duration_since(entry.fetched_at) < NPM_CACHE_TTL
    {
        return entry.payload.clone();
    }

    let result = lookup_npm_package(http, name).await;
    if result.ok() || result.status_code() == Some(404) {
        CACHE.lock().unwrap_or_else(|e| e.into_inner()).insert(
            name.to_string(),
            CacheEntry {
                fetched_at: Instant::now(),
                payload: result.clone(),
            },
        );
    }
    result
}

/// registry 客户端纯函数单测：包名编码与文档字段解析（不发真实请求）。
#[cfg(test)]
mod tests {
    use super::*;

    /// 验证包名编码与 JS encodeURIComponent 语义一致，且 scoped 前缀 `@` 保留。
    #[test]
    fn encodes_scoped_names_like_the_js() {
        assert_eq!(encode_name("foo"), "foo");
        assert_eq!(encode_name("@scope/foo"), "@scope%2Ffoo");
        assert_eq!(encode_name("a/b"), "a%2Fb");
    }

    /// 验证 dist-tags 与 versions 字段解析：tag 映射、latest 提取与版本列表。
    #[test]
    fn parses_registry_payloads() {
        let payload = serde_json::json!({
            "dist-tags": { "latest": "2.0.0", "next": "3.0.0" },
            "versions": { "1.0.0": {}, "2.0.0": {} }
        });
        let dist_tags = parse_dist_tags(payload.get("dist-tags").unwrap());
        assert_eq!(dist_tags.get("latest").map(String::as_str), Some("2.0.0"));
        let versions = parse_versions(payload.get("versions").unwrap());
        assert_eq!(versions.len(), 2);
        assert!(versions.contains(&"1.0.0".to_string()));
    }
}
