//! Port of `server/lib/opencode/models-metadata.js` — models.dev catalog
//! access with persistent on-disk caching, ETag conditional revalidation,
//! and automatic proxy retry.
//!
//! Layers (outermost first), exactly like the JS:
//! 1. In-memory copy, fresh within `ttl_ms` (shared by every consumer).
//! 2. On-disk cache (`models-dev.catalog.json` in the OMPChamber data dir) —
//!    survives restarts, seeds the in-memory copy on boot, and serves as the
//!    stale fallback when the network (and proxy) are unreachable.
//! 3. Network: conditional GET with `If-None-Match` (models.dev/Cloudflare
//!    revalidates via ETag; 304 keeps the cached body). Direct first; on a
//!    NETWORK error (blocked/reset/timeout — not an HTTP status) retry via a
//!    detected proxy: env `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY` first, then
//!    the macOS system proxy (`scutil --proxy`).
//!
//! JS divergences kept deliberate:
//! - The JS hand-rolls an HTTP CONNECT tunnel over `node:net`+`node:tls`
//!   because Node's `fetch` cannot take a proxy; reqwest speaks CONNECT
//!   natively, so the proxy retry uses a per-proxy reqwest client (plain-TCP
//!   CONNECT, like the JS tunnel).
//! - Concurrent callers serialize on a fetch mutex and then re-read the
//!   memory cache instead of sharing one in-flight future (same wire result,
//!   one network request either way; noted in PORT-MANIFEST.md).
//!
//! 中文说明：本模块是 `server/lib/opencode/models-metadata.js` 的 Rust 移植，
//! 为 UI 提供 models.dev 模型目录的读取与缓存。读取顺序与 JS 版一致：
//! 内存缓存（TTL 内新鲜）→ 磁盘缓存（断网兜底、重启预热）→ 网络
//! （先直连条件 GET；仅在网络级错误时按序尝试检测到的 proxy）。
//! 所有网络与 proxy 依赖都经 [`CatalogIo`] 注入，便于测试替换。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;

use super::http::{HttpFetch, HttpRequest, default_fetch};

/// models.dev 目录 API 的固定地址（与 JS 版硬编码的 URL 相同）。
pub(crate) const MODELS_DEV_API_URL: &str = "https://models.dev/api.json";
/// 默认内存缓存有效期：10 分钟（对应 JS 模块默认 ttlMs）。
pub(crate) const DEFAULT_TTL_MS: u64 = 10 * 60 * 1000;
/// 默认网络请求超时：20 秒（对应 JS 模块默认 timeoutMs）。
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 20_000;
/// 磁盘缓存文件格式版本号；版本不匹配的旧文件按不存在处理，避免读到不兼容结构。
const DISK_CACHE_VERSION: u64 = 1;
/// macOS `scutil --proxy` 检测结果的进程内缓存时长（60 秒）。
const SCUTIL_TTL_MS: u64 = 60_000;
/// `scutil --proxy` 子进程执行超时；超时按“无系统代理”处理。
const SCUTIL_TIMEOUT: Duration = Duration::from_secs(3);

/// proxy 目标端点，对应 JS 的 `{ host, port }` 形状。
/// `httpsGetViaProxy` target: `{ host, port }` (JS shape).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ProxyEndpoint {
    /// proxy 主机名或 IP 地址。
    pub host: String,
    /// proxy 端口号。
    pub port: u16,
}

/// 经 proxy CONNECT 隧道执行 GET 的响应，对应 JS 的 `{ status, etag, body }`。
/// JS `{ status, etag, body }` from the CONNECT-tunnel GET.
#[derive(Debug, Clone)]
pub(crate) struct ProxyResponse {
    /// HTTP 状态码（304 表示内容未变）。
    pub status: u16,
    /// 响应携带的 ETag（可能缺席）。
    pub etag: Option<String>,
    /// 响应正文文本。
    pub body: String,
}

/// 经指定 proxy 执行 GET 的可注入闭包签名：
/// (url, proxy 端点, 超时毫秒, 可选 ETag) → 响应或错误字符串。
pub(crate) type ProxyGet = Arc<
    dyn Fn(
            String,
            ProxyEndpoint,
            u64,
            Option<String>,
        ) -> BoxFuture<'static, Result<ProxyResponse, String>>
        + Send
        + Sync,
>;
/// 异步返回当前可用 proxy 候选列表（按优先级排序）的可注入闭包。
pub(crate) type ProxyCandidates =
    Arc<dyn Fn() -> BoxFuture<'static, Vec<ProxyEndpoint>> + Send + Sync>;

/// 可注入的 IO 接缝集合，对应 JS 的 options 参数
/// （fetchImpl / proxyGet / proxyCandidates）：测试注入 fake，生产注入真实实现。
/// Injectable seams mirroring the JS `options` (`fetchImpl`, `proxyGet`,
/// `proxyCandidates`).
pub(crate) struct CatalogIo {
    /// 直连 HTTP fetch 实现。
    pub fetch: HttpFetch,
    /// 经 proxy 的 GET 实现。
    pub proxy_get: ProxyGet,
    /// proxy 候选探测实现。
    pub proxy_candidates: ProxyCandidates,
}

/// 一次目录拉取失败。是否为源站 HTTP 回答、是否为超时，决定后续分流（见各字段）。
/// A fetch failure. `http_status` presence marks an origin answer (4xx/5xx),
/// which must NOT trigger the proxy retry; `timeout` drives the route's
/// 504-vs-502 mapping (`TimeoutError`/`AbortError` in the JS).
#[derive(Debug, Clone)]
pub(crate) struct FetchFailure {
    /// 人类可读的错误消息。
    pub message: String,
    /// 标记这是源站的 HTTP 错误回答（4xx/5xx），不得触发 proxy 重试。
    /// Marks an origin HTTP answer (4xx/5xx) which must NOT trigger the
    /// proxy retry. Only constructed (and matched) on the early-return path
    /// in `fetch_catalog`; the field documents the JS's `error.httpStatus`
    /// check even though no later reader needs it.
    #[allow(dead_code)]
    pub http_status: Option<u16>,
    /// 是否为超时类错误（对应 JS 的 TimeoutError/AbortError，路由据此映射 504）。
    /// JS `error.name === 'TimeoutError' | 'AbortError'` for the route's
    /// 504 mapping. Like the JS, the fetch rotation's surviving error is the
    /// proxy attempt's, so this never observes a timeout in practice.
    #[allow(dead_code)]
    pub timeout: bool,
}

/// 一次成功拉取的结果：304 未修改，或携带新 ETag 的新鲜载荷。
enum FetchOutcome {
    /// 源站返回 304：缓存正文仍然有效，仅时间戳推进。
    NotModified,
    /// 拉到新载荷及其 ETag（可能缺席）。
    Fresh {
        /// 新的目录 JSON（对象或数组）。
        metadata: Value,
        /// 新载荷的 ETag，用于下一次条件请求。
        etag: Option<String>,
    },
}

/// 一份缓存条目：目录正文、配对 ETag 与最近抓取时间（Unix 毫秒）。
struct CacheEntry {
    /// 目录 JSON 正文。
    metadata: Value,
    /// 与正文配对的 ETag（可能缺席）。
    etag: Option<String>,
    /// 最近一次成功抓取的 Unix 毫秒时间戳，TTL 判断依据。
    fetched_at: u64,
}

/// 内存缓存状态，由 `state` 同步互斥锁保护。
struct MemoryState {
    /// 当前缓存条目；None 表示内存中尚无缓存。
    entry: Option<CacheEntry>,
    /// 磁盘缓存是否已在本实例内加载过（仅一次性加载）。
    disk_loaded: bool,
}

/// `getModelsMetadata` 的结果，对应 JS 的 `{ metadata, fromCache, stale? }`。
/// `getModelsMetadata` result: `{ metadata, fromCache, stale? }`.
#[derive(Debug, Clone)]
pub(crate) struct ModelsMetadataResult {
    /// 目录 JSON 正文。
    pub metadata: Value,
    /// 是否来自缓存（内存或磁盘）而非本次网络请求。
    pub from_cache: bool,
    /// 是否为网络彻底失败后回退的过期缓存。
    pub stale: bool,
}

/// 目录缓存实例：JS 模块级状态（memoryCache/diskLoaded/inflight）的归宿，每个数据目录一个实例。
/// Module-level JS state (`memoryCache`, `diskLoaded`, `inflight`) owned by
/// one cache instance per data directory.
pub(crate) struct ModelsMetadataCache {
    /// 注入的 fetch 与 proxy 接缝。
    io: CatalogIo,
    /// 磁盘缓存文件路径。
    cache_path: PathBuf,
    /// 内存缓存状态（同步互斥锁保护）。
    state: Mutex<MemoryState>,
    /// 对应 JS 的 inflight：并发调用共享同一次刷新。
    /// JS `inflight`: concurrent callers share one refresh.
    fetch_lock: AsyncMutex<()>,
    /// 可注入的时钟（Unix 毫秒），测试可固定时间。
    now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
}

/// `ModelsMetadataCache` 的实现：内存/磁盘双层缓存加条件网络刷新，逐一对应 JS 模块函数。
impl ModelsMetadataCache {
    /// 以显式注入的 IO、缓存路径与时钟构造实例（测试入口）。
    pub(crate) fn new(
        io: CatalogIo,
        cache_path: PathBuf,
        now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        Self {
            io,
            cache_path,
            state: Mutex::new(MemoryState {
                entry: None,
                disk_loaded: false,
            }),
            fetch_lock: AsyncMutex::new(()),
            now_ms,
        }
    }

    /// 生产构造器：真实 HTTP fetch、reqwest CONNECT proxy 与系统时钟。
    /// Production cache for a data directory.
    pub(crate) fn production(cache_path: PathBuf) -> Self {
        Self::new(
            CatalogIo {
                fetch: default_fetch(),
                proxy_get: Arc::new(|url, proxy, timeout_ms, etag| {
                    Box::pin(default_proxy_get(url, proxy, timeout_ms, etag))
                }),
                proxy_candidates: Arc::new(|| Box::pin(detect_proxy_candidates())),
            },
            cache_path,
            Arc::new(system_now_ms),
        )
    }

    /// 读取目录（对应 JS `getModelsMetadata`）：TTL 内直接命中内存缓存；
    /// 否则持锁刷新——304 保留原正文仅推进时间戳，网络彻底失败时回退到
    /// 任意已有缓存（标记 stale），无任何缓存则返回错误。
    /// JS `getModelsMetadata({ url, ttlMs, timeoutMs, cachePath })` with the
    /// module's default TTL/timeout and this cache's fixed disk path.
    pub(crate) async fn get(
        &self,
        url: &str,
        ttl_ms: u64,
        timeout_ms: u64,
    ) -> Result<ModelsMetadataResult, FetchFailure> {
        self.load_disk_cache();
        if let Some(result) = self.fresh_result(ttl_ms) {
            return Ok(result);
        }

        let _guard = self.fetch_lock.lock().await;
        // Another caller may have refreshed while we waited for the lock.
        if let Some(result) = self.fresh_result(ttl_ms) {
            return Ok(result);
        }

        let (etag, cached_metadata) = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state
                .entry
                .as_ref()
                .map(|entry| (entry.etag.clone(), Some(entry.metadata.clone())))
                .unwrap_or((None, None))
        };
        let fetched_at = (self.now_ms)();
        match fetch_catalog(url, timeout_ms, etag.as_deref(), &self.io).await {
            Ok(FetchOutcome::Fresh { metadata, etag }) => {
                let entry = CacheEntry {
                    metadata,
                    etag,
                    fetched_at,
                };
                let result = ModelsMetadataResult {
                    metadata: entry.metadata.clone(),
                    from_cache: false,
                    stale: false,
                };
                self.persist_disk_cache(&entry);
                self.state.lock().unwrap_or_else(|e| e.into_inner()).entry = Some(entry);
                Ok(result)
            }
            Ok(FetchOutcome::NotModified) => {
                // 304 keeps the cached body; only the timestamp moves.
                let entry = CacheEntry {
                    metadata: cached_metadata.unwrap_or(Value::Null),
                    etag,
                    fetched_at,
                };
                let result = ModelsMetadataResult {
                    metadata: entry.metadata.clone(),
                    from_cache: false,
                    stale: false,
                };
                self.persist_disk_cache(&entry);
                self.state.lock().unwrap_or_else(|e| e.into_inner()).entry = Some(entry);
                Ok(result)
            }
            Err(error) => {
                // Total network failure: serve any cached copy stale.
                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(entry) = &state.entry {
                    return Ok(ModelsMetadataResult {
                        metadata: entry.metadata.clone(),
                        from_cache: true,
                        stale: true,
                    });
                }
                Err(error)
            }
        }
    }

    /// 内存缓存仍在 TTL 内时返回命中结果（from_cache 且非 stale），否则 None。
    fn fresh_result(&self, ttl_ms: u64) -> Option<ModelsMetadataResult> {
        let now = (self.now_ms)();
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let entry = state.entry.as_ref()?;
        if now.saturating_sub(entry.fetched_at) < ttl_ms {
            Some(ModelsMetadataResult {
                metadata: entry.metadata.clone(),
                from_cache: true,
                stale: false,
            })
        } else {
            None
        }
    }

    /// 惰性加载磁盘缓存（每实例一次，对应 JS `loadDiskCache`）：文件缺失或损坏一律视为空缓存，不报错。
    /// JS `loadDiskCache` (once per process; missing/corrupt reads as empty).
    fn load_disk_cache(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.disk_loaded {
            return;
        }
        state.disk_loaded = true;
        let Ok(raw) = std::fs::read_to_string(&self.cache_path) else {
            return;
        };
        let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
            return;
        };
        let version = parsed.get("version").and_then(Value::as_u64);
        let data = parsed.get("data");
        let fetched_at = parsed.get("fetchedAt").and_then(Value::as_f64);
        if version == Some(DISK_CACHE_VERSION)
            && matches!(data, Some(Value::Object(_)))
            && let Some(fetched_at) = fetched_at
        {
            state.entry = Some(CacheEntry {
                metadata: data.cloned().unwrap_or(Value::Null),
                etag: parsed
                    .get("etag")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                fetched_at: fetched_at as u64,
            });
        }
    }

    /// 将条目写回磁盘（对应 JS `persistDiskCache`）：先写临时文件再原子 rename；失败仅清理临时文件，不影响调用方。
    /// JS `persistDiskCache` — atomic temp+rename, best-effort.
    fn persist_disk_cache(&self, entry: &CacheEntry) {
        let payload = serde_json::json!({
            "version": DISK_CACHE_VERSION,
            "etag": entry.etag,
            "fetchedAt": entry.fetched_at,
            "data": entry.metadata,
        });
        let temp = self.cache_path.with_file_name(format!(
            "{}.{}.tmp",
            self.cache_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            std::process::id()
        ));
        let write = (|| -> std::io::Result<()> {
            if let Some(parent) = self.cache_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&temp, payload.to_string())?;
            std::fs::rename(&temp, &self.cache_path)?;
            Ok(())
        })();
        if write.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
    }
}

/// 当前 Unix 毫秒时间戳（时钟早于 epoch 时返回 0）。
pub(crate) fn system_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Fetch orchestration (fetchCatalog)
// ---------------------------------------------------------------------------

/// 解析 models.dev 响应（对应 JS `parseCatalog`）：必须是 JSON 对象或数组，否则视为“意外载荷”。
/// JS `parseCatalog`: must decode to an object (or array — JS `typeof []` is
/// `"object"`); anything else is "an unexpected payload".
fn parse_catalog(body_text: &str) -> Result<Value, String> {
    let metadata: Value = serde_json::from_str(body_text)
        .map_err(|err| format!("Unexpected end of JSON input: {err}"))?;
    match metadata {
        Value::Object(_) | Value::Array(_) => Ok(metadata),
        _ => Err("models.dev returned an unexpected payload".to_string()),
    }
}

/// 拉取目录（对应 JS `fetchCatalog`）：先直连条件 GET；仅当直连出现
/// 网络级错误或载荷解析失败时才按序尝试 proxy 候选。源站的 HTTP 状态
/// 错误是真实回答，直接返回、不触发 proxy 重试。
/// JS `fetchCatalog`: direct first, then via each detected proxy on network
/// errors. HTTP status errors from the origin do NOT trigger the proxy
/// retry — the origin answered.
async fn fetch_catalog(
    url: &str,
    timeout_ms: u64,
    etag: Option<&str>,
    io: &CatalogIo,
) -> Result<FetchOutcome, FetchFailure> {
    // Attempt 1: direct conditional GET.
    let mut request = HttpRequest::get(url)
        .header("Accept", "application/json")
        .timeout(timeout_ms);
    if let Some(etag) = etag {
        request = request.header("If-None-Match", etag);
    }
    // (A direct parse or network failure falls through to the proxy attempt
    // below, like the JS `lastError` rotation.)

    match (io.fetch)(request).await {
        Ok(response) if response.status == 304 => return Ok(FetchOutcome::NotModified),
        Ok(response) if !response.ok() => {
            // An origin HTTP status error is a real answer; no proxy retry.
            return Err(FetchFailure {
                message: format!("models.dev responded with status {}", response.status),
                http_status: Some(response.status),
                timeout: false,
            });
        }
        Ok(response) => match parse_catalog(&response.text()) {
            Ok(metadata) => {
                return Ok(FetchOutcome::Fresh {
                    metadata,
                    etag: response.header("etag").map(str::to_string),
                });
            }
            Err(_) => { /* fall through to the proxy attempt */ }
        },
        Err(_) => { /* fall through to the proxy attempt */ }
    }

    // Attempt 2: proxy candidates, in order.
    let candidates = (io.proxy_candidates)().await;
    let mut proxy_error = String::from("no proxy configured");
    for proxy in candidates {
        match (io.proxy_get)(url.to_string(), proxy, timeout_ms, etag.map(str::to_string)).await {
            Ok(response) if response.status == 304 => return Ok(FetchOutcome::NotModified),
            Ok(response) if (200..300).contains(&response.status) => {
                match parse_catalog(&response.body) {
                    Ok(metadata) => {
                        return Ok(FetchOutcome::Fresh {
                            metadata,
                            etag: response.etag,
                        });
                    }
                    Err(message) => proxy_error = message,
                }
            }
            Ok(response) => {
                proxy_error = format!(
                    "models.dev responded with status {} via proxy",
                    response.status
                );
            }
            Err(error) => proxy_error = error,
        }
    }
    // The proxy attempt's error wins, like the JS loop.
    Err(FetchFailure {
        message: proxy_error,
        http_status: None,
        timeout: false,
    })
}

// ---------------------------------------------------------------------------
// Proxy detection (env first, then macOS system proxy)
// ---------------------------------------------------------------------------

/// 解析 proxy URL（对应 JS `httpProxyFromUrl`）：只接受 http/https scheme（socks 需要另一种隧道），缺省端口取 80/443。
/// JS `httpProxyFromUrl`: http/https proxies only (`socks://` needs a
/// different tunnel).
pub(crate) fn http_proxy_from_url(raw: &str) -> Option<ProxyEndpoint> {
    let url = url::Url::parse(raw).ok()?;
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host = url.host_str()?.to_string();
    if host.is_empty() {
        return None;
    }
    let port = url
        .port()
        .unwrap_or(if scheme == "https" { 443 } else { 80 });
    Some(ProxyEndpoint { host, port })
}

/// 从显式值列表解析 env proxy（对应 JS `detectEnvProxies`）；env 读取留在调用点，保证测试封闭。
/// JS `detectEnvProxies` over an explicit value list (env read stays at the
/// call site so tests stay hermetic).
pub(crate) fn env_proxies_from(values: &[Option<String>]) -> Vec<ProxyEndpoint> {
    values
        .iter()
        .flatten()
        .filter_map(|raw| http_proxy_from_url(raw))
        .collect()
}

/// 读取 HTTPS_PROXY/HTTP_PROXY/ALL_PROXY（含小写变体）并解析为 proxy 列表。
fn detect_env_proxies() -> Vec<ProxyEndpoint> {
    env_proxies_from(&[
        std::env::var("HTTPS_PROXY").ok(),
        std::env::var("https_proxy").ok(),
        std::env::var("HTTP_PROXY").ok(),
        std::env::var("http_proxy").ok(),
        std::env::var("ALL_PROXY").ok(),
        std::env::var("all_proxy").ok(),
    ])
}

/// 解析 `scutil --proxy` 的字典输出：仅提取已启用（Enable=1）的 HTTP/HTTPS 代理键值对。
/// JS `detectScutilProxies` output parser: `Key : value` dictionary lines.
pub(crate) fn parse_scutil_output(stdout: &str) -> Vec<ProxyEndpoint> {
    let read = |key: &str| -> Option<String> {
        for line in stdout.lines() {
            let trimmed = line.trim_start();
            let Some(rest) = trimmed.strip_prefix(key) else {
                continue;
            };
            let rest = rest.trim_start();
            let Some(value_part) = rest.strip_prefix(':') else {
                continue;
            };
            let value = value_part.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
        None
    };
    let mut proxies = Vec::new();
    if read("HTTPSEnable").as_deref() == Some("1")
        && let (Some(host), Some(port)) = (read("HTTPSProxy"), read("HTTPSPort"))
        && let Ok(port) = port.parse::<u16>()
    {
        proxies.push(ProxyEndpoint { host, port });
    }
    if read("HTTPEnable").as_deref() == Some("1")
        && let (Some(host), Some(port)) = (read("HTTPProxy"), read("HTTPPort"))
        && let Ok(port) = port.parse::<u16>()
    {
        proxies.push(ProxyEndpoint { host, port });
    }
    proxies
}

/// 执行 `scutil --proxy` 探测 macOS 系统 proxy（结果缓存 60 秒）；超时或失败按无系统代理处理。
async fn detect_scutil_proxies() -> Vec<ProxyEndpoint> {
    // 进程内 scutil 结果缓存：(上次刷新毫秒, proxy 列表)。
    static CACHE: LazyLock<Mutex<(u64, Vec<ProxyEndpoint>)>> =
        LazyLock::new(|| Mutex::new((0, Vec::new())));
    let now = system_now_ms();
    {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if now.saturating_sub(cache.0) < SCUTIL_TTL_MS {
            return cache.1.clone();
        }
    }
    let output = tokio::time::timeout(
        SCUTIL_TIMEOUT,
        tokio::process::Command::new("scutil")
            .arg("--proxy")
            .output(),
    )
    .await;
    let proxies = match output {
        Ok(Ok(output)) if output.status.success() => {
            parse_scutil_output(&String::from_utf8_lossy(&output.stdout))
        }
        _ => return Vec::new(),
    };
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = (now, proxies.clone());
    proxies
}

/// 汇总 proxy 候选（对应 JS `detectProxyCandidates`）：env 优先，macOS 追加 scutil 系统代理，再按 host+port 去重；任何一步都不抛错。
/// JS `detectProxyCandidates`: env proxies + (darwin) system proxies, env
/// first, deduped. Never throws.
pub(crate) async fn detect_proxy_candidates() -> Vec<ProxyEndpoint> {
    let mut all = detect_env_proxies();
    if cfg!(target_os = "macos") {
        // scutil unavailable/timeout: env proxies (if any) still apply.
        all.extend(detect_scutil_proxies().await);
    }
    let mut seen = std::collections::HashSet::new();
    all.retain(|proxy| seen.insert((proxy.host.clone(), proxy.port)));
    all
}

// ---------------------------------------------------------------------------
// Proxy GET (reqwest CONNECT — the JS hand-rolled tunnel's replacement)
// ---------------------------------------------------------------------------

/// 按 (host, port) 缓存的 reqwest client 池：每个 proxy 复用一个配置好 CONNECT 的 client。
static PROXY_CLIENTS: LazyLock<Mutex<HashMap<(String, u16), reqwest::Client>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 取（或创建并缓存）指定 proxy 的 reqwest client。
fn proxy_client(proxy: &ProxyEndpoint) -> reqwest::Client {
    let key = (proxy.host.clone(), proxy.port);
    let mut cache = PROXY_CLIENTS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(client) = cache.get(&key) {
        return client.clone();
    }
    let proxy_url = format!("http://{}:{}", proxy.host, proxy.port);
    let mut builder = reqwest::Client::builder();
    if let Ok(proxy) = reqwest::Proxy::all(&proxy_url) {
        builder = builder.proxy(proxy);
    }
    let client = builder.build().unwrap_or_default();
    cache.insert(key, client.clone());
    client
}

/// 生产环境的经 proxy GET（对应 JS `httpsGetViaProxy`）：reqwest CONNECT 隧道 + identity 编码 + Connection: close，与 JS 手写隧道行为一致。
/// Production `httpsGetViaProxy` equivalent: identity-encoded HTTP/1.1 GET
/// through the proxy's CONNECT tunnel with `Connection: close` framing.
async fn default_proxy_get(
    url: String,
    proxy: ProxyEndpoint,
    timeout_ms: u64,
    etag: Option<String>,
) -> Result<ProxyResponse, String> {
    let client = proxy_client(&proxy);
    let mut request = client
        .get(&url)
        .header("Accept", "application/json")
        .header("Accept-Encoding", "identity")
        .header("Connection", "close")
        .timeout(Duration::from_millis(timeout_ms));
    if let Some(etag) = &etag {
        request = request.header("If-None-Match", etag);
    }
    let response = request.send().await.map_err(|err| err.to_string())?;
    let status = response.status().as_u16();
    let etag = response
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = response.text().await.map_err(|err| err.to_string())?;
    Ok(ProxyResponse { status, etag, body })
}

// ---------------------------------------------------------------------------
// Disk cache path helper (JS `cacheFilePath`)
// ---------------------------------------------------------------------------

/// 磁盘缓存文件路径（对应 JS `cacheFilePath`）：<数据目录>/models-dev.catalog.json。
/// JS `cacheFilePath()`: `<OMPCHAMBER_DATA_DIR>/models-dev.catalog.json`.
pub(crate) fn cache_file_path(data_dir: &Path) -> PathBuf {
    data_dir.join("models-dev.catalog.json")
}
