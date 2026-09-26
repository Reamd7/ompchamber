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

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::small_model::http::{Fetch, FetchRequest, now_ms};

pub const MODELS_DEV_API_URL: &str = "https://models.dev/api.json";
const DEFAULT_TTL_MS: u64 = 10 * 60 * 1000;
/// models.dev/api.json is a ~4MB catalog; cold fetches routinely exceed 8s.
/// Every failure path falls back to cached data, so a patient timeout is safe.
const DEFAULT_TIMEOUT_MS: u64 = 20_000;
const DISK_CACHE_VERSION: u64 = 1;

#[derive(Clone)]
struct CatalogEntry {
    metadata: Arc<Value>,
    etag: Option<String>,
    fetched_at_ms: u64,
    fetched_at: tokio::time::Instant,
}

struct CatalogState {
    memory: Option<CatalogEntry>,
    disk_loaded: bool,
    inflight: Option<Shared<BoxFuture<'static, Result<CatalogEntry, String>>>>,
    generation: u64,
}

/// The shared in-process models.dev cache (one per service; the JS module
/// global is equivalent because the server builds one service).
pub struct CatalogCache {
    fetch: Fetch,
    url: String,
    cache_path: PathBuf,
    ttl: Duration,
    timeout_ms: u64,
    state: tokio::sync::Mutex<CatalogState>,
}

pub type CatalogResult = Result<Arc<Value>, String>;

impl CatalogCache {
    pub fn new(fetch: Fetch, cache_path: PathBuf) -> Arc<Self> {
        Self::with_ttl(fetch, cache_path, Duration::from_millis(DEFAULT_TTL_MS))
    }

    /// Test seam: a shortened TTL makes revalidation observable.
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
pub fn get_catalog_provider<'a>(catalog: &'a Value, provider_id: &str) -> Option<&'a Value> {
    let entry = catalog.get(provider_id)?;
    entry.as_object().map(|_| entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::small_model::http::FetchResponse;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_cache_path(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sm-catalog-{label}-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("models-dev.catalog.json")
    }

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
