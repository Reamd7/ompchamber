//! Port of `server/lib/small-model/catalog.js` backed by a minimal version of
//! `server/lib/opencode/models-metadata.js` (the opencode module itself is
//! not ported yet — this implements the layers small-model needs):
//!
//! 1. In-memory copy, fresh within the TTL (10 minutes).
//! 2. On-disk cache in the OMPChamber data dir (`models-dev.catalog.json`,
//!    same `{version, etag, fetchedAt, data}` shape the JS writes) — seeds
//!    the in-memory copy on boot and serves as the stale fallback when the
//!    network is unreachable.
//! 3. Network: conditional GET with `If-None-Match`.
//!
//! Known gap vs the JS: no proxy fallback (env HTTPS_PROXY/HTTP_PROXY reach
//! reqwest automatically; the macOS system-proxy `scutil` detection and the
//! hand-rolled CONNECT tunnel are not ported).
//!
//! 中文说明：本文件是 `server/lib/small-model/catalog.js` 的移植，并内置一份精简版
//! models 元数据层（完整 opencode 模块尚未移植，这里只实现 small-model 需要的部分）。
//! 三级缓存策略：
//! 1. 内存副本：TTL（默认 10 分钟）内直接命中；
//! 2. 磁盘缓存：OMPChamber 数据目录下的 `models-dev.catalog.json`，与 JS 写出的
//!    `{version, etag, fetchedAt, data}` 结构一致——启动时播种内存副本，网络不可达
//!    时作为过期兜底；
//! 3. 网络：带 `If-None-Match` 的条件 GET 协商刷新。
//! 与 JS 版的已知差距：无 proxy 兜底（HTTPS_PROXY/HTTP_PROXY 由 reqwest 自动识别；
//! macOS 的 `scutil` 系统代理探测与手写 CONNECT 隧道未移植）。

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::small_model::http::{Fetch, FetchRequest, now_ms};

/// models.dev 目录 API 的线上地址；缓存刷新与测试默认都指向它。
pub const MODELS_DEV_API_URL: &str = "https://models.dev/api.json";
/// 内存目录副本的默认保鲜期（10 分钟）；过期后下一次读取触发条件刷新。
const DEFAULT_TTL_MS: u64 = 10 * 60 * 1000;
/// models.dev/api.json is a ~4MB catalog; cold fetches routinely exceed 8s.
/// Every failure path falls back to cached data, so a patient timeout is safe.
///
/// 中文补充：约 4MB 的目录冷拉取常超 8 秒，而所有失败路径都会回退缓存，
/// 因此 20 秒的宽松超时是安全的。
const DEFAULT_TIMEOUT_MS: u64 = 20_000;
/// 磁盘缓存格式版本号；不匹配（旧格式或异源文件）时整份缓存作废、返回 None。
const DISK_CACHE_VERSION: u64 = 1;

/// 一份完整的目录快照：元数据本体、协商缓存用的 ETag，以及两份时钟——毫秒墙钟
/// 时间戳用于落盘诊断，tokio 单调时钟用于进程内 TTL 判断。
#[derive(Clone)]
struct CatalogEntry {
    /// models.dev api.json 解析出的元数据；Arc 让读端零拷贝共享。
    metadata: Arc<Value>,
    /// 上次成功响应的 ETag，作为下次条件请求的 If-None-Match 发回。
    etag: Option<String>,
    /// 拉取时刻的毫秒墙钟时间戳，随磁盘缓存一起持久化。
    fetched_at_ms: u64,
    /// 拉取时刻的 tokio 单调时钟采样；TTL 判断只依赖它，不受系统时间回拨影响。
    fetched_at: tokio::time::Instant,
}

/// Mutex 保护的缓存可变状态：内存快照、磁盘播种标记、在飞请求与代际计数。
struct CatalogState {
    /// 当前内存快照；None 表示尚无任何可用副本。
    memory: Option<CatalogEntry>,
    /// 磁盘缓存是否已在首次读取时播种进内存（整个进程只做一次）。
    disk_loaded: bool,
    /// 正在进行的网络刷新（Shared future）；并发调用共享同一次请求，避免惊群。
    inflight: Option<Shared<BoxFuture<'static, Result<CatalogEntry, String>>>>,
    /// 代际计数：每次发起新请求自增；请求完成后仅当代际仍匹配才清空 inflight。
    generation: u64,
}

/// The shared in-process models.dev cache (one per service; the JS module
/// global is equivalent because the server builds one service).
///
/// 中文补充：进程内共享的 models.dev 缓存（每个 service 一份；JS 用模块级全局变量，
/// 服务端只构建一个 service，二者等价）。
pub struct CatalogCache {
    /// 可注入的出站 HTTP 通道（生产为 reqwest 实现，测试换 fake）。
    fetch: Fetch,
    /// 目录 API 地址，构造时固定。
    url: String,
    /// 磁盘缓存文件路径（通常在 OMPChamber 数据目录下）。
    cache_path: PathBuf,
    /// 内存副本保鲜期；过期后下一次读取触发条件刷新。
    ttl: Duration,
    /// 单次网络请求的超时上限（毫秒）。
    timeout_ms: u64,
    /// 可变状态，由 tokio Mutex 保护。
    state: tokio::sync::Mutex<CatalogState>,
}

/// 目录读取结果：成功时是 Arc 共享的元数据（零拷贝），失败时是已格式化的错误消息。
pub type CatalogResult = Result<Arc<Value>, String>;

/// 缓存的读取入口与并发去重控制。
impl CatalogCache {
    /// 以默认 TTL（10 分钟）与默认超时构造缓存。
    pub fn new(fetch: Fetch, cache_path: PathBuf) -> Arc<Self> {
        Self::with_ttl(fetch, cache_path, Duration::from_millis(DEFAULT_TTL_MS))
    }

    /// Test seam: a shortened TTL makes revalidation observable.
    ///
    /// 中文补充：测试缝——缩短 TTL 让"过期后重新发起条件请求"变得可观测。
    pub fn with_ttl(fetch: Fetch, cache_path: PathBuf, ttl: Duration) -> Arc<Self> {
        Arc::new(Self {
            fetch,
            url: MODELS_DEV_API_URL.to_string(),
            cache_path,
            ttl,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            state: tokio::sync::Mutex::new(CatalogState {
                memory: None,
                disk_loaded: false,
                inflight: None,
                generation: 0,
            }),
        })
    }

    /// `getModelsMetadata().metadata`: fresh in-memory copy first, then a
    /// conditional network refresh (ETag), seeding from and persisting to the
    /// on-disk cache. On total network failure any cached copy is served
    /// stale; the error only propagates when nothing has ever been cached.
    ///
    /// 中文补充：先吃新鲜内存副本，过期则发起 ETag 条件网络刷新，磁盘缓存负责
    /// 播种与持久化；网络彻底失败时返回过期缓存，只有从未缓存过才把错误上抛。
    pub async fn get_model_catalog(&self) -> CatalogResult {
        let (shared, generation) = {
            let mut state = self.state.lock().await;
            if !state.disk_loaded {
                state.disk_loaded = true;
                if state.memory.is_none() {
                    state.memory = load_disk_cache(&self.cache_path);
                }
            }
            if let Some(memory) = &state.memory
                && memory.fetched_at.elapsed() < self.ttl
            {
                return Ok(Arc::clone(&memory.metadata));
            }
            if let Some(existing) = &state.inflight {
                (existing.clone(), state.generation)
            } else {
                let fetch = Arc::clone(&self.fetch);
                let url = self.url.clone();
                let timeout_ms = self.timeout_ms;
                let cache_path = self.cache_path.clone();
                let etag = state.memory.as_ref().and_then(|entry| entry.etag.clone());
                let metadata_seed = state
                    .memory
                    .as_ref()
                    .map(|entry| Arc::clone(&entry.metadata));
                let shared: Shared<BoxFuture<'static, Result<CatalogEntry, String>>> =
                    tokio::spawn(async move {
                        let entry =
                            fetch_catalog(fetch, &url, timeout_ms, etag, metadata_seed).await?;
                        persist_disk_cache(&cache_path, &entry);
                        Ok(entry)
                    })
                    .map(|joined| joined.unwrap_or_else(|error| Err(error.to_string())))
                    .boxed()
                    .shared();
                state.inflight = Some(shared.clone());
                state.generation += 1;
                (shared, state.generation)
            }
        };
        let result = shared.await;
        let mut state = self.state.lock().await;
        if state.generation == generation {
            state.inflight = None;
        }
        match result {
            Ok(entry) => {
                let metadata = Arc::clone(&entry.metadata);
                state.memory = Some(entry);
                Ok(metadata)
            }
            Err(error) => match state.memory.take() {
                // Stale-but-cached beats a hard failure; a later call retries.
                Some(entry) => {
                    let metadata = Arc::clone(&entry.metadata);
                    state.memory = Some(entry);
                    Ok(metadata)
                }
                None => Err(error),
            },
        }
    }
}

/// `parseCatalog` + conditional fetch (direct only — proxy fallback is a
/// documented gap). A 304 keeps the cached metadata with a refreshed clock.
///
/// 中文补充：仅直连（proxy 兜底是已知差距）；304 时沿用旧元数据但刷新时钟。
async fn fetch_catalog(
    fetch: Fetch,
    url: &str,
    timeout_ms: u64,
    etag: Option<String>,
    metadata_seed: Option<Arc<Value>>,
) -> Result<CatalogEntry, String> {
    let mut headers = vec![("Accept".to_string(), "application/json".to_string())];
    if let Some(etag) = &etag {
        headers.push(("If-None-Match".to_string(), etag.clone()));
    }
    let response = fetch(FetchRequest {
        method: "GET".to_string(),
        url: url.to_string(),
        headers,
        body: None,
        timeout_ms,
    })
    .await?;
    if response.status == 304 {
        let metadata = metadata_seed
            .ok_or_else(|| "models.dev replied 304 without a cached copy".to_string())?;
        return Ok(CatalogEntry {
            metadata,
            etag,
            fetched_at_ms: now_ms(),
            fetched_at: tokio::time::Instant::now(),
        });
    }
    if !(200..300).contains(&response.status) {
        return Err(format!(
            "models.dev responded with status {}",
            response.status
        ));
    }
    let metadata: Value = serde_json::from_slice(&response.body)
        .map_err(|_| "models.dev returned invalid JSON".to_string())?;
    if metadata.as_object().is_none() {
        return Err("models.dev returned an unexpected payload".to_string());
    }
    Ok(CatalogEntry {
        metadata: Arc::new(metadata),
        etag: response.header("etag").map(str::to_string),
        fetched_at_ms: now_ms(),
        fetched_at: tokio::time::Instant::now(),
    })
}

/// 读取并校验磁盘缓存文件；版本不符、字段缺失、内容非对象或 IO 失败一律返回 None，
/// 调用方将其视为"没有缓存"。播种出的副本时钟被人为拨到过期，首次读取必触发再验证。
fn load_disk_cache(cache_path: &std::path::Path) -> Option<CatalogEntry> {
    let parsed: Value = serde_json::from_str(&std::fs::read_to_string(cache_path).ok()?).ok()?;
    if parsed.get("version").and_then(Value::as_u64) != Some(DISK_CACHE_VERSION) {
        return None;
    }
    let data = parsed.get("data")?;
    data.as_object()?;
    let fetched_at_ms = parsed.get("fetchedAt").and_then(Value::as_u64)?;
    Some(CatalogEntry {
        metadata: Arc::new(data.clone()),
        etag: parsed
            .get("etag")
            .and_then(Value::as_str)
            .map(str::to_string),
        fetched_at_ms,
        // Disk seeds are never fresh: the first read always revalidates.
        fetched_at: tokio::time::Instant::now()
            - Duration::from_secs(DEFAULT_TTL_MS.max(1) / 1000 + 60),
    })
}

/// Best-effort atomic persistence (tmp + rename); the in-memory copy still
/// serves this process when it fails.
///
/// 中文补充：尽力而为的原子落盘（先写 tmp 再 rename）；失败时静默放弃，
/// 本进程仍有内存副本可用。
fn persist_disk_cache(cache_path: &std::path::Path, entry: &CatalogEntry) {
    let payload = serde_json::json!({
        "version": DISK_CACHE_VERSION,
        "etag": entry.etag,
        "fetchedAt": entry.fetched_at_ms,
        "data": *entry.metadata,
    });
    let Ok(payload) = serde_json::to_string(&payload) else {
        return;
    };
    let temp = cache_path.with_extension(format!("json.{}.tmp", std::process::id()));
    let Ok(parent) = std::path::Path::new(cache_path)
        .parent()
        .map(PathBuf::from)
        .ok_or(())
    else {
        return;
    };
    if std::fs::create_dir_all(&parent).is_err() {
        return;
    }
    if std::fs::write(&temp, payload).is_err() {
        return;
    }
    if std::fs::rename(&temp, cache_path).is_err() {
        let _ = std::fs::remove_file(&temp);
    }
}
/// `getCatalogProvider` (re-exported from resolve.js semantics).
///
/// 中文补充：目录顶层按 provider id 取对象型条目（语义与 resolve.js 的同名函数一致）。
pub fn get_catalog_provider<'a>(catalog: &'a Value, provider_id: &str) -> Option<&'a Value> {
    let entry = catalog.get(provider_id)?;
    entry.as_object().map(|_| entry)
}

/// 目录缓存：TTL 命中、磁盘持久化、网络失败降级与 provider 查找的行为测试。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::small_model::http::FetchResponse;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 按标签创建唯一的临时目录，返回其中的 models-dev.catalog.json 路径。
    fn temp_cache_path(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sm-catalog-{label}-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("models-dev.catalog.json")
    }

    /// 构造 fake Fetch：固定返回给定状态码/响应体/ETag，并用原子计数器记录调用次数。
    fn fetch_returning(
        status: u16,
        body: &str,
        etag: Option<&str>,
        calls: Arc<AtomicUsize>,
    ) -> Fetch {
        let body = body.to_string();
        let etag = etag.map(str::to_string);
        Arc::new(move |_request| {
            let calls = Arc::clone(&calls);
            let body = body.clone();
            let etag = etag.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(FetchResponse {
                    status,
                    headers: etag
                        .map(|etag| vec![("etag".to_string(), etag)])
                        .unwrap_or_default(),
                    body: body.clone().into_bytes(),
                })
            })
        })
    }

    /// 验证 TTL 窗口内只发起一次网络请求，且成功结果连同 ETag 一并持久化到磁盘。
    #[tokio::test]
    async fn fetches_once_then_serves_within_ttl_and_persists_to_disk() {
        let cache_path = temp_cache_path("fresh");
        let calls = Arc::new(AtomicUsize::new(0));
        let cache = CatalogCache::new(
            fetch_returning(
                200,
                r#"{"openai":{"api":"https://api.openai.com/v1"}}"#,
                Some("v1"),
                Arc::clone(&calls),
            ),
            cache_path.clone(),
        );

        let catalog = cache.get_model_catalog().await.unwrap();
        assert_eq!(catalog["openai"]["api"], json!("https://api.openai.com/v1"));
        cache.get_model_catalog().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "TTL window: one fetch");
        assert!(cache_path.exists(), "disk cache persisted");
        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&cache_path).unwrap()).unwrap();
        assert_eq!(on_disk["version"], json!(DISK_CACHE_VERSION));
        assert_eq!(on_disk["etag"], json!("v1"));
        assert_eq!(
            on_disk["data"]["openai"]["api"],
            json!("https://api.openai.com/v1")
        );
        std::fs::remove_dir_all(cache_path.parent().unwrap()).ok();
    }

    /// 验证网络失败时仍返回过期缓存；只有从未缓存过的情况下错误才上抛。
    #[tokio::test]
    async fn serves_stale_cache_when_the_network_fails() {
        let cache_path = temp_cache_path("stale");
        let ok_calls = Arc::new(AtomicUsize::new(0));
        let cache_ok = CatalogCache::new(
            fetch_returning(
                200,
                r#"{"anthropic":{"api":"x"}}"#,
                None,
                Arc::clone(&ok_calls),
            ),
            cache_path.clone(),
        );
        cache_ok.get_model_catalog().await.unwrap();

        // Same disk cache, dead network: the stale copy still answers.
        let failing: crate::small_model::Fetch = Arc::new(|_request: FetchRequest| {
            Box::pin(async { Err::<FetchResponse, _>("network down".to_string()) })
        });
        let cache_fail = CatalogCache::new(std::sync::Arc::clone(&failing), cache_path.clone());
        let catalog = cache_fail
            .get_model_catalog()
            .await
            .expect("stale copy serves");
        assert_eq!(catalog["anthropic"]["api"], json!("x"));

        // Dead network and nothing cached: the error propagates.
        let empty_dir = std::env::temp_dir().join(format!("sm-catalog-empty-{}", now_ms()));
        std::fs::create_dir_all(&empty_dir).unwrap();
        let cache_empty =
            CatalogCache::new(std::sync::Arc::clone(&failing), empty_dir.join("none.json"));
        assert!(cache_empty.get_model_catalog().await.is_err());
        std::fs::remove_dir_all(cache_path.parent().unwrap()).ok();
        std::fs::remove_dir_all(&empty_dir).ok();
    }

    /// 验证 provider 查找只接受对象型条目；标量条目与缺失键都返回 None。
    #[test]
    fn catalog_provider_lookup_requires_an_object_entry() {
        let catalog = json!({
            "openai": { "api": "https://api.openai.com/v1" },
            "broken": 7
        });
        assert!(get_catalog_provider(&catalog, "openai").is_some());
        assert!(get_catalog_provider(&catalog, "broken").is_none());
        assert!(get_catalog_provider(&catalog, "missing").is_none());
    }
}
