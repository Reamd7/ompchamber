//! Port of `server/lib/opencode/npm-registry.js` — npm package metadata
//! lookups with a 1h TTL cache and in-flight request deduplication.
//!
//! JS source map: `lookupNpmPackage` → [`lookup_npm_package`],
//! `getNpmInfo` → [`get_npm_info`], `clearCache` → [`clear_cache`]. The JS
//! module is a library (no routes of its own); its consumers in the JS tree
//! are the plugin/npm status routes, which the `opencode_plugins` module
//! ports with its own copy of this client. This copy serves the
//! `opencode_meta` module surface.
//!
//! Divergence vs the JS (noted for PORT-MANIFEST): duplicate concurrent
//! lookups for the same name wait on a per-name mutex and then hit the cache
//! instead of sharing one in-flight request (same wire result, one request
//! either way), and the network error text is reqwest's rather than Node's.
//!
//! 中文说明：npm 包元数据查询模块（移植自 `server/lib/opencode/npm-registry.js`）。
//! 提供 1 小时 TTL 的进程内缓存与按包名串行化的在途请求去重，向上层
//! 返回统一的 [`NpmInfo`]：成功载荷或带 HTTP 状态码 / `network` 标记的
//! 错误，函数本身绝不向上抛出 IO 异常。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use serde_json::{Map, Value};

use super::http::{HttpFetch, HttpRequest, default_fetch};

/// npm 元数据缓存有效期（JS：1 小时 = 3_600_000 毫秒）。
const NPM_CACHE_TTL_MS: u64 = 3_600_000;
/// 单次 registry 请求的超时时间（JS：5 秒），等价于 fetch 的 AbortSignal.timeout。
const NPM_FETCH_TIMEOUT_MS: u64 = 5_000;
/// npm registry 根地址；测试通过 [`NpmIo::registry_base`] 替换为本地 canned 响应地址。
pub(crate) const NPM_REGISTRY_BASE: &str = "https://registry.npmjs.org";

/// JS `NpmLookupResult` — success payload or lookup error.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NpmInfo {
/// 查询成功：最新版本号、全部版本号列表与 dist-tags 映射。
    Ok {
        /// `distTags.latest ?? null`.
        latest: Option<String>,
/// 全部已发布版本号（registry `versions` 对象的键）。
        versions: Vec<String>,
/// 仅保留字符串值的 dist-tags 映射（数组值条目被丢弃，同 JS `parseDistTags`）。
        dist_tags: Map<String, Value>,
    },
/// 查询失败：HTTP 状态码或 network 标记，附错误文本。
    Err {
        /// JS `status`: numeric HTTP status or the literal `'network'`.
        status: NpmStatus,
/// 错误描述文本（registry 差异信息或底层传输错误 message）。
        error: String,
    },
}

/// JS 结果里的 `status` 字段：数字 HTTP 状态码或字面量 `'network'`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NpmStatus {
/// registry 返回的数字 HTTP 状态码（如 404、500）。
    Code(u16),
/// 网络/传输层失败（超时、连接错误、响应体 JSON 解析失败）。
    Network,
}

/// `NpmInfo` 辅助方法：JS wire 形状序列化与 `cacheResult` 缓存资格判定。
impl NpmInfo {
    /// JS wire shape (`{ ok, latest, versions, distTags }` /
    /// `{ ok, status, error }`).
    pub(crate) fn to_value(&self) -> Value {
        match self {
            NpmInfo::Ok {
                latest,
                versions,
                dist_tags,
            } => serde_json::json!({
                "ok": true,
                "latest": latest,
                "versions": versions,
                "distTags": Value::Object(dist_tags.clone()),
            }),
            NpmInfo::Err { status, error } => match status {
                NpmStatus::Code(code) => serde_json::json!({
                    "ok": false,
                    "status": code,
                    "error": error,
                }),
                NpmStatus::Network => serde_json::json!({
                    "ok": false,
                    "status": "network",
                    "error": error,
                }),
            },
        }
    }

    /// JS `cacheResult`: only definitive answers (success or 404) are cached.
    fn is_definitive(&self) -> bool {
        match self {
            NpmInfo::Ok { .. } => true,
            NpmInfo::Err {
                status: NpmStatus::Code(404),
                ..
            } => true,
            NpmInfo::Err { .. } => false,
        }
    }
}

/// Injectable transport + registry base (tests point at canned responses).
pub(crate) struct NpmIo {
/// 出站 HTTP 传输（见 [`super::http`]），生产为 reqwest 直连，测试注入 canned 响应。
    pub fetch: HttpFetch,
/// registry 根地址，请求 URL 拼装为 `{base}/{encode_name(name)}`。
    pub registry_base: String,
}

/// 生产默认值：reqwest 直连传输 + 官方 npm registry 地址。
impl Default for NpmIo {
    /// 构造生产默认值：reqwest 直连传输 + 官方 registry 地址。
    fn default() -> Self {
        Self {
            fetch: default_fetch(),
            registry_base: NPM_REGISTRY_BASE.to_string(),
        }
    }
}

/// 单条缓存记录：抓取时刻（毫秒）与结果载荷。
struct CacheEntry {
/// 抓取时刻（UNIX epoch 毫秒），用于 TTL 新鲜度判定。
    fetched_at: u64,
/// 抓取到的结果（仅成功或 404 等确定性答案会入缓存）。
    payload: NpmInfo,
}

/// JS module state: `_cache`, `_inFlight`, `_userAgent`.
static CACHE: LazyLock<Mutex<HashMap<String, CacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// 按包名的在途请求互斥锁表（JS `_inFlight`）：并发同名查询先排队，
/// 拿到锁后回读缓存，避免重复出网。
static INFLIGHT: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// registry 请求的 User-Agent（形如 `ompchamber-server/<version>`；JS `_userAgent`）。
/// 首次使用时读取 web 包 package.json 的 version，读取或解析失败退化为 `/dev`。
static USER_AGENT: LazyLock<String> = LazyLock::new(|| {
    let pkg_path = web_package_root().join("package.json");
    match std::fs::read_to_string(&pkg_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
    {
        Some(pkg) if pkg.get("version").and_then(Value::as_str).is_some() => {
            format!(
                "ompchamber-server/{}",
                pkg["version"].as_str().unwrap_or_default()
            )
        }
        _ => "ompchamber-server/dev".to_string(),
    }
});

/// JS `_getPackageJsonPath()` resolves to the web package's package.json.
fn web_package_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// JS `encodeName`: `encodeURIComponent(name).replace(/^%40/, '@')`.
pub(crate) fn encode_name(name: &str) -> String {
    let mut encoded = String::with_capacity(name.len());
    for &byte in name.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    match encoded.strip_prefix("%40") {
        Some(rest) => format!("@{rest}"),
        None => encoded,
    }
}

/// JS `parseDistTags`: string-valued entries of an object (arrays dropped).
fn parse_dist_tags(value: Option<&Value>) -> Map<String, Value> {
    let Some(map) = value.and_then(Value::as_object) else {
        return Map::new();
    };
    map.iter()
        .filter(|(_, value)| value.is_string())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// JS `parseVersions`: keys of an object.
fn parse_versions(value: Option<&Value>) -> Vec<String> {
    let Some(map) = value.and_then(Value::as_object) else {
        return Vec::new();
    };
    map.keys().cloned().collect()
}

/// JS `lookupNpmPackage` — one direct registry fetch (never fails; errors
/// come back as `NpmInfo::Err`).
pub(crate) async fn lookup_npm_package(io: &NpmIo, name: &str) -> NpmInfo {
    let url = format!("{}/{}", io.registry_base, encode_name(name));
    let request = HttpRequest::get(url)
        .header("User-Agent", USER_AGENT.as_str())
        .header("Accept", "application/json")
        .timeout(NPM_FETCH_TIMEOUT_MS);
    match (io.fetch)(request).await {
        Ok(response) if response.ok() => match serde_json::from_slice::<Value>(&response.body) {
            Ok(data) => {
                let dist_tags = parse_dist_tags(data.get("dist-tags"));
                let latest = dist_tags
                    .get("latest")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                NpmInfo::Ok {
                    latest,
                    versions: parse_versions(data.get("versions")),
                    dist_tags,
                }
            }
            // JS: a body parse failure lands in the catch → 'network'.
            Err(error) => NpmInfo::Err {
                status: NpmStatus::Network,
                error: error.to_string(),
            },
        },
        Ok(response) if response.status == 404 => NpmInfo::Err {
            status: NpmStatus::Code(404),
            error: "Package not found".to_string(),
        },
        Ok(response) => NpmInfo::Err {
            status: NpmStatus::Code(response.status),
            error: format!("Registry returned {}", response.status),
        },
        Err(error) => NpmInfo::Err {
            status: NpmStatus::Network,
            error: error.message(),
        },
    }
}

/// JS `getNpmInfo(name, { forceRefresh })` — TTL cache + per-name request
/// serialization.
pub(crate) async fn get_npm_info(io: &NpmIo, name: &str, force_refresh: bool) -> NpmInfo {
    let now = now_ms();
    if !force_refresh {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache.get(name)
            && now.saturating_sub(entry.fetched_at) < NPM_CACHE_TTL_MS
        {
            return entry.payload.clone();
        }
    }

    let guard = {
        let mut inflight = INFLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(inflight.entry(name.to_string()).or_default())
    };
    let _serial = guard.lock().await;
    // Another caller may have finished the lookup while we waited.
    if !force_refresh {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache.get(name)
            && now_ms().saturating_sub(entry.fetched_at) < NPM_CACHE_TTL_MS
        {
            return entry.payload.clone();
        }
    }

    let result = lookup_npm_package(io, name).await;
    if result.is_definitive() {
        CACHE.lock().unwrap_or_else(|e| e.into_inner()).insert(
            name.to_string(),
            CacheEntry {
                fetched_at: now_ms(),
                payload: result.clone(),
            },
        );
    }
    result
}

/// JS `clearCache` (test seam).
pub(crate) fn clear_cache() {
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).clear();
    INFLIGHT.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// 测试专用通道（JS 测试同款手段）：绕过网络直接写入带指定时间戳的
/// 缓存条目，用于验证 TTL 过期与强制刷新逻辑。
#[cfg(test)]
pub(crate) mod testing {
    /// Force a cache entry with an explicit `fetchedAt` (TTL expiry tests).
    pub(crate) fn set_cache_entry(name: &str, fetched_at: u64, payload: super::NpmInfo) {
        super::CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                name.to_string(),
                super::CacheEntry {
                    fetched_at,
                    payload,
                },
            );
    }
}

/// 当前 UNIX epoch 毫秒时间戳；系统时钟早于 epoch 时返回 0。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
