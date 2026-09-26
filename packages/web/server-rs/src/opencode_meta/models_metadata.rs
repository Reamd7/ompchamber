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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;

use super::http::{HttpFetch, HttpRequest, default_fetch};

pub(crate) const MODELS_DEV_API_URL: &str = "https://models.dev/api.json";
pub(crate) const DEFAULT_TTL_MS: u64 = 10 * 60 * 1000;
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 20_000;
const DISK_CACHE_VERSION: u64 = 1;
const SCUTIL_TTL_MS: u64 = 60_000;
const SCUTIL_TIMEOUT: Duration = Duration::from_secs(3);

/// `httpsGetViaProxy` target: `{ host, port }` (JS shape).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ProxyEndpoint {
    pub host: String,
    pub port: u16,
}

/// JS `{ status, etag, body }` from the CONNECT-tunnel GET.
#[derive(Debug, Clone)]
pub(crate) struct ProxyResponse {
    pub status: u16,
    pub etag: Option<String>,
    pub body: String,
}

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
pub(crate) type ProxyCandidates =
    Arc<dyn Fn() -> BoxFuture<'static, Vec<ProxyEndpoint>> + Send + Sync>;

/// Injectable seams mirroring the JS `options` (`fetchImpl`, `proxyGet`,
/// `proxyCandidates`).
pub(crate) struct CatalogIo {
    pub fetch: HttpFetch,
    pub proxy_get: ProxyGet,
    pub proxy_candidates: ProxyCandidates,
}

/// A fetch failure. `http_status` presence marks an origin answer (4xx/5xx),
/// which must NOT trigger the proxy retry; `timeout` drives the route's
/// 504-vs-502 mapping (`TimeoutError`/`AbortError` in the JS).
#[derive(Debug, Clone)]
pub(crate) struct FetchFailure {
    pub message: String,
    /// Marks an origin HTTP answer (4xx/5xx) which must NOT trigger the
    /// proxy retry. Only constructed (and matched) on the early-return path
    /// in `fetch_catalog`; the field documents the JS's `error.httpStatus`
    /// check even though no later reader needs it.
    #[allow(dead_code)]
    pub http_status: Option<u16>,
    /// JS `error.name === 'TimeoutError' | 'AbortError'` for the route's
    /// 504 mapping. Like the JS, the fetch rotation's surviving error is the
    /// proxy attempt's, so this never observes a timeout in practice.
    #[allow(dead_code)]
    pub timeout: bool,
}

enum FetchOutcome {
    NotModified,
    Fresh {
        metadata: Value,
        etag: Option<String>,
    },
}

struct CacheEntry {
    metadata: Value,
    etag: Option<String>,
    fetched_at: u64,
}

struct MemoryState {
    entry: Option<CacheEntry>,
    disk_loaded: bool,
}

/// `getModelsMetadata` result: `{ metadata, fromCache, stale? }`.
#[derive(Debug, Clone)]
pub(crate) struct ModelsMetadataResult {
    pub metadata: Value,
    pub from_cache: bool,
    pub stale: bool,
}

/// Module-level JS state (`memoryCache`, `diskLoaded`, `inflight`) owned by
/// one cache instance per data directory.
pub(crate) struct ModelsMetadataCache {
    io: CatalogIo,
    cache_path: PathBuf,
    state: Mutex<MemoryState>,
    /// JS `inflight`: concurrent callers share one refresh.
    fetch_lock: AsyncMutex<()>,
    now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl ModelsMetadataCache {
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

pub(crate) fn system_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Fetch orchestration (fetchCatalog)
// ---------------------------------------------------------------------------

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

/// JS `detectEnvProxies` over an explicit value list (env read stays at the
/// call site so tests stay hermetic).
pub(crate) fn env_proxies_from(values: &[Option<String>]) -> Vec<ProxyEndpoint> {
    values
        .iter()
        .flatten()
        .filter_map(|raw| http_proxy_from_url(raw))
        .collect()
}

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

async fn detect_scutil_proxies() -> Vec<ProxyEndpoint> {
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

static PROXY_CLIENTS: LazyLock<Mutex<HashMap<(String, u16), reqwest::Client>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

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

/// JS `cacheFilePath()`: `<OMPCHAMBER_DATA_DIR>/models-dev.catalog.json`.
pub(crate) fn cache_file_path(data_dir: &Path) -> PathBuf {
    data_dir.join("models-dev.catalog.json")
}
