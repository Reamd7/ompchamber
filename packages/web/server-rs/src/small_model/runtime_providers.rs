//! Port of `server/lib/small-model/runtime-providers.js`: provider state
//! that exists only inside the running OpenCode process (`GET /provider`).
//!
//! One snapshot per 30s (shared in-flight request); answers `null` — never an
//! empty provider list — when OpenCode is unreachable, so a momentary outage
//! cannot retract providers. The JS module is wired from `server/index.js`
//! with the OpenCode connection and reset on OpenCode restart; the Rust port
//! derives the connection from [`crate::engine::EngineState`] on every fetch
//! (a restart moves the base URL underneath it, which the next TTL refresh
//! picks up) and exposes [`RuntimeProviders::reset`] for explicit rewiring.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use serde_json::Value;

use crate::engine::EngineState;
use crate::small_model::http::FetchResponse;

const SNAPSHOT_TTL_MS: u64 = 30_000;
const SNAPSHOT_TIMEOUT_MS: u64 = 5_000;

/// opencode zen hands out this sentinel instead of a key when the user has no
/// zen login, and trims its catalog to the free models. Those run on
/// OpenCode's own subsidised infrastructure and are meant to be reached
/// through OpenCode, not by us — the sentinel is never accepted as a
/// credential.
pub const ZEN_ANONYMOUS_API_KEY: &str = "public";

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeProvider {
    pub id: String,
    pub source: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    /// True only for the zen-without-login case: a provider that is present
    /// and usable through OpenCode, but that we must not call ourselves.
    pub anonymous_zen: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ProviderSnapshot {
    pub providers: HashMap<String, RuntimeProvider>,
    pub connected: Vec<String>,
}

fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn record(value: &Value) -> &serde_json::Map<String, Value> {
    static EMPTY: std::sync::LazyLock<serde_json::Map<String, Value>> =
        std::sync::LazyLock::new(Default::default);
    value.as_object().unwrap_or(&EMPTY)
}

fn endpoint(value: &Value) -> Option<String> {
    text(value).map(|raw| raw.trim_end_matches('/').to_string())
}

/// `parseProviderListing`: everything the `/provider` payload claims is
/// checked here.
pub fn parse_provider_listing(payload: &Value) -> ProviderSnapshot {
    let mut snapshot = ProviderSnapshot::default();
    if payload.as_object().is_none() {
        return snapshot;
    }

    for raw in payload
        .get("all")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let raw = record(raw);
        let Some(id) = text(raw.get("id").unwrap_or(&Value::Null)) else {
            continue;
        };
        let options = record(raw.get("options").unwrap_or(&Value::Null));
        // JS `Object.values(models)[0]` — the first model in payload order.
        // serde_json maps are key-sorted, so "first" is the alphabetically
        // first model rather than the payload's first (same fallback URL in
        // practice; noted in the port report).
        let first_model_api = raw
            .get("models")
            .and_then(Value::as_object)
            .and_then(|models| models.values().next())
            .and_then(|model| model.get("api"))
            .map(record);
        let declared_key = text(options.get("apiKey").unwrap_or(&Value::Null));
        let anonymous_zen = declared_key.as_deref() == Some(ZEN_ANONYMOUS_API_KEY);
        let api_key = if anonymous_zen {
            None
        } else {
            declared_key.or_else(|| text(raw.get("key").unwrap_or(&Value::Null)))
        };
        let base_url = endpoint(options.get("baseURL").unwrap_or(&Value::Null)).or_else(|| {
            first_model_api.and_then(|api| endpoint(api.get("url").unwrap_or(&Value::Null)))
        });
        snapshot.providers.insert(
            id.clone(),
            RuntimeProvider {
                id,
                source: text(raw.get("source").unwrap_or(&Value::Null)),
                api_key,
                base_url,
                anonymous_zen,
            },
        );
    }

    // Providers OpenCode considers usable right now. A provider can be
    // present in `all` (it is in the catalog) without any credential behind it.
    for raw in payload
        .get("connected")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        if let Some(id) = text(raw)
            && !snapshot.connected.contains(&id)
        {
            snapshot.connected.push(id);
        }
    }

    snapshot
}

/// Snapshot fetch outcome: `Ok(None)` models the JS unwired connection
/// (`if (!connection) return null`).
pub type ProviderFetchFuture = BoxFuture<'static, Result<Option<Value>, String>>;
pub type ProviderFetch = Arc<dyn Fn() -> ProviderFetchFuture + Send + Sync>;

/// Production fetch against the managed engine (`buildOpenCodeUrl` +
/// `getOpenCodeAuthHeaders` + `AbortSignal.timeout`).
pub fn engine_provider_fetch(engine: Arc<EngineState>) -> ProviderFetch {
    let fetch = crate::small_model::http::reqwest_fetch();
    Arc::new(move || {
        let engine = Arc::clone(&engine);
        let fetch = Arc::clone(&fetch);
        Box::pin(async move {
            let Some(base_url) = engine.base_url() else {
                return Ok(None);
            };
            let url = format!("{}/provider", base_url.trim_end_matches('/'));
            let mut headers = vec![("Accept".to_string(), "application/json".to_string())];
            if let Some(auth) = engine.auth_header() {
                headers.push(("Authorization".to_string(), auth));
            }
            let request = crate::small_model::http::FetchRequest {
                method: "GET".to_string(),
                url,
                headers,
                body: None,
                timeout_ms: SNAPSHOT_TIMEOUT_MS,
            };
            let response: FetchResponse =
                tokio::time::timeout(Duration::from_millis(SNAPSHOT_TIMEOUT_MS), fetch(request))
                    .await
                    .map_err(|_| "OpenCode provider listing timed out".to_string())??;
            if !(200..300).contains(&response.status) {
                return Err(format!(
                    "OpenCode provider listing failed with {}",
                    response.status
                ));
            }
            serde_json::from_slice(&response.body)
                .map(Some)
                .map_err(|_| "OpenCode provider listing returned invalid JSON".to_string())
        })
    })
}

struct SnapshotState {
    snapshot: Option<ProviderSnapshot>,
    /// TTL reference on the tokio clock (virtual under paused tests, wall
    /// clock in production — the same source `tokio::time` deadlines use).
    snapshot_at: Option<tokio::time::Instant>,
    inflight: Option<Shared<BoxFuture<'static, Result<Option<Value>, String>>>>,
    /// Ties the inflight clear to the flight that created it (JS: the
    /// `.finally` belongs to the one stored promise, not to each awaiter).
    generation: u64,
}

impl SnapshotState {
    fn new() -> Self {
        Self {
            snapshot: None,
            snapshot_at: None,
            inflight: None,
            generation: 0,
        }
    }

    fn fresh(&self, ttl: Duration) -> bool {
        self.snapshot_at
            .is_some_and(|at| tokio::time::Instant::now().duration_since(at) < ttl)
    }
}

/// The cached runtime-provider view.
pub struct RuntimeProviders {
    fetch: Option<ProviderFetch>,
    state: tokio::sync::Mutex<SnapshotState>,
    ttl: std::time::Duration,
}

impl RuntimeProviders {
    /// Unwired (`configureOpenCodeRuntimeProviders(null)`): every lookup
    /// answers "nothing known" and resolution stays file-based.
    pub fn unwired() -> Arc<Self> {
        Arc::new(Self {
            fetch: None,
            state: tokio::sync::Mutex::new(SnapshotState::new()),
            ttl: std::time::Duration::from_millis(SNAPSHOT_TTL_MS),
        })
    }

    pub fn new(fetch: ProviderFetch) -> Arc<Self> {
        Self::with_ttl(fetch, std::time::Duration::from_millis(SNAPSHOT_TTL_MS))
    }

    /// Test seam: a shortened TTL makes refresh behavior observable without
    /// waiting out the production window.
    pub fn with_ttl(fetch: ProviderFetch, ttl: std::time::Duration) -> Arc<Self> {
        Arc::new(Self {
            fetch: Some(fetch),
            state: tokio::sync::Mutex::new(SnapshotState::new()),
            ttl,
        })
    }

    pub fn from_engine(engine: Arc<EngineState>) -> Arc<Self> {
        Self::new(engine_provider_fetch(engine))
    }

    /// `resetOpenCodeRuntimeProviders`: drops every cached answer.
    pub async fn reset(&self) {
        let mut state = self.state.lock().await;
        state.snapshot = None;
        state.snapshot_at = None;
        state.inflight = None;
    }

    /// `getRuntimeProviderSnapshot`: the current snapshot, or `None` when
    /// OpenCode is unwired or cannot be reached and nothing was cached.
    pub async fn snapshot(&self) -> Option<ProviderSnapshot> {
        let Some(fetch) = self.fetch.clone() else {
            return None;
        };
        let (shared, generation) = {
            let mut state = self.state.lock().await;
            if state.fresh(self.ttl) {
                return state.snapshot.clone();
            }
            if let Some(existing) = &state.inflight {
                (existing.clone(), state.generation)
            } else {
                let shared: Shared<BoxFuture<'static, Result<Option<Value>, String>>> =
                    fetch().shared();
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
            Ok(Some(payload)) => {
                let snapshot = parse_provider_listing(&payload);
                state.snapshot = Some(snapshot.clone());
                state.snapshot_at = Some(tokio::time::Instant::now());
                Some(snapshot)
            }
            _ => {
                // Keep serving the previous snapshot when there is one: a
                // momentarily unreachable OpenCode should not retract
                // providers that were resolving a second ago.
                state.snapshot.clone()
            }
        }
    }
    /// `getRuntimeProvider`: runtime credential and endpoint for one
    /// provider, or `None` when OpenCode knows nothing about it.
    pub async fn provider(&self, provider_id: &str) -> Option<RuntimeProvider> {
        self.snapshot().await?.providers.get(provider_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn payload() -> Value {
        json!({
            "all": [
                {
                    "id": "llmapi",
                    "source": "config",
                    "options": { "apiKey": "plugin-key", "baseURL": "https://api.llmapi.ai/v1/" },
                    "models": { "claude-opus-4-8": { "api": { "id": "claude-opus-4-8", "url": "", "npm": "@ai-sdk/anthropic" } } }
                },
                {
                    "id": "opencode",
                    "source": "custom",
                    "options": { "apiKey": ZEN_ANONYMOUS_API_KEY },
                    "models": { "free-model": { "api": { "id": "free-model", "url": "https://opencode.ai/zen/v1", "npm": "@ai-sdk/openai-compatible" } } }
                },
                {
                    "id": "zai-coding-plan",
                    "source": "api",
                    "key": "auth-json-key",
                    "options": {},
                    "models": { "glm-5": { "api": { "id": "glm-5", "url": "https://api.z.ai/api/coding/paas/v4", "npm": "@ai-sdk/openai-compatible" } } }
                }
            ],
            "connected": ["llmapi", "opencode", "zai-coding-plan"]
        })
    }

    #[test]
    fn parses_credential_endpoint_and_zen_sentinel() {
        let snapshot = parse_provider_listing(&payload());
        assert_eq!(
            snapshot.providers["llmapi"],
            RuntimeProvider {
                id: "llmapi".to_string(),
                source: Some("config".to_string()),
                api_key: Some("plugin-key".to_string()),
                base_url: Some("https://api.llmapi.ai/v1".to_string()),
                anonymous_zen: false,
            }
        );
        let zen = &snapshot.providers["opencode"];
        assert_eq!(zen.api_key, None);
        assert!(zen.anonymous_zen);
        assert_eq!(zen.base_url.as_deref(), Some("https://opencode.ai/zen/v1"));
        assert_eq!(
            snapshot.connected,
            vec!["llmapi", "opencode", "zai-coding-plan"]
        );
    }

    #[test]
    fn falls_back_to_the_model_endpoint_without_a_provider_base_url() {
        let snapshot = parse_provider_listing(&payload());
        assert_eq!(
            snapshot.providers["zai-coding-plan"].base_url.as_deref(),
            Some("https://api.z.ai/api/coding/paas/v4")
        );
        assert_eq!(
            snapshot.providers["zai-coding-plan"].api_key.as_deref(),
            Some("auth-json-key")
        );
    }

    #[tokio::test]
    async fn serves_one_snapshot_to_concurrent_callers_instead_of_refetching() {
        let calls = Arc::new(AtomicUsize::new(0));
        let failing = Arc::new(AtomicBool::new(false));
        let payload = payload();
        let calls_for_fetch = Arc::clone(&calls);
        let failing_for_fetch = Arc::clone(&failing);
        let providers = RuntimeProviders::with_ttl(
            Arc::new(move || {
                let calls = Arc::clone(&calls_for_fetch);
                let failing = Arc::clone(&failing_for_fetch);
                let payload = payload.clone();
                Box::pin(async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if failing.load(Ordering::SeqCst) {
                        return Err("connection refused".to_string());
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Ok(Some(payload))
                })
            }),
            // Zero TTL: every read past the in-flight window refetches, so
            // the failure refresh below genuinely runs.
            Duration::ZERO,
        );

        // Cold, concurrent callers share one in-flight fetch.
        let joined = futures::future::join_all([
            providers.snapshot(),
            providers.snapshot(),
            providers.snapshot(),
        ])
        .await;
        assert!(joined.iter().all(Option::is_some));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Failure refresh (TTL zero): the previous snapshot stands.
        failing.store(true, Ordering::SeqCst);
        let refreshed = providers.snapshot().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(
            refreshed
                .expect("previous snapshot survives")
                .providers
                .contains_key("llmapi")
        );
    }

    #[tokio::test]
    async fn serves_the_cached_snapshot_within_the_ttl_window() {
        let calls = Arc::new(AtomicUsize::new(0));
        let payload = payload();
        let calls_for_fetch = Arc::clone(&calls);
        let providers = RuntimeProviders::new(Arc::new(move || {
            let calls = Arc::clone(&calls_for_fetch);
            let payload = payload.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Some(payload))
            })
        }));
        providers.snapshot().await.unwrap();
        providers.snapshot().await.unwrap();
        providers.snapshot().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "TTL window: one fetch");
    }

    #[tokio::test]
    async fn answers_unknown_rather_than_no_providers_when_unreachable() {
        let providers = RuntimeProviders::new(Arc::new(|| {
            Box::pin(async { Err("connection refused".to_string()) })
        }));
        assert!(providers.snapshot().await.is_none());
    }

    #[tokio::test]
    async fn stays_on_file_based_resolution_until_configured() {
        let providers = RuntimeProviders::unwired();
        assert!(providers.provider("llmapi").await.is_none());
    }
}
