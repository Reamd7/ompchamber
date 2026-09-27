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
//! 中文说明：本模块移植自 `server/lib/small-model/runtime-providers.js`，
//! 管理只存在于运行中 OpenCode 进程内的 provider 状态（来自
//! `GET /provider`）。每 30s 一份快照（在飞请求共享）；OpenCode 不可达
//! 时回答 null——绝不是空 provider 列表——避免瞬时故障收回 provider。
//! JS 侧由 server/index.js 接线、在 OpenCode 重启时重置；Rust 移植在
//! 每次抓取时从 EngineState 重新推导连接（重启换掉的 base URL 由下一
//! 次 TTL 刷新接住），并提供 RuntimeProviders::reset 供显式重接。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use serde_json::Value;

use crate::engine::EngineState;
use crate::small_model::http::FetchResponse;

/// 快照缓存 TTL：30s 内重复读取直接复用，过期后单飞刷新。
const SNAPSHOT_TTL_MS: u64 = 30_000;
/// /provider 抓取超时：fetch 自带超时 + 外层 tokio 超时双重保险。
const SNAPSHOT_TIMEOUT_MS: u64 = 5_000;

/// opencode zen hands out this sentinel instead of a key when the user has no
/// zen login, and trims its catalog to the free models. Those run on
/// OpenCode's own subsidised infrastructure and are meant to be reached
/// through OpenCode, not by us — the sentinel is never accepted as a
/// credential.
/// 中文说明：用户没有 zen 登录时 opencode zen 发放的哨兵值（而非真
/// key），并把 catalog 裁剪到免费模型——那些跑在 OpenCode 自行补贴的
/// 基础设施上、应经 OpenCode 而非由我们直连；哨兵永远不被当作凭据。
pub const ZEN_ANONYMOUS_API_KEY: &str = "public";

/// 单个运行时 provider 的可用信息：OpenCode 进程内解析出的凭据与端点。
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeProvider {
    /// provider id（与 catalog/auth.json 的键一致）。
    pub id: String,
    /// OpenCode 侧的来源标记（config/custom/api 等）。
    pub source: Option<String>,
    /// 可用凭据；zen 匿名哨兵会被置为 None。
    pub api_key: Option<String>,
    /// 请求端点：优先 options.baseURL，缺失时回退首个模型的 api.url。
    pub base_url: Option<String>,
    /// True only for the zen-without-login case: a provider that is present
    /// and usable through OpenCode, but that we must not call ourselves.
    /// 中文说明：仅 zen-无登录 场景为 true——该 provider 经 OpenCode
    /// 存在且可用，但我们绝不能自己直连。
    pub anonymous_zen: bool,
}

/// 一次 /provider 抓取的解析结果：全部已知 provider 与当前已连接集合。
#[derive(Debug, Clone, Default)]
pub struct ProviderSnapshot {
    /// 按 id 索引的 provider 详情表。
    pub providers: HashMap<String, RuntimeProvider>,
    /// OpenCode 认为当前可用的 provider id（去重保序）。
    pub connected: Vec<String>,
}

/// 读取字符串字段：trim 后非空才返回 Some。
fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// 把 Value 当对象读；非对象时返回进程级共享的空表（避免每次分配）。
fn record(value: &Value) -> &serde_json::Map<String, Value> {
    static EMPTY: std::sync::LazyLock<serde_json::Map<String, Value>> =
        std::sync::LazyLock::new(Default::default);
    value.as_object().unwrap_or(&EMPTY)
}

/// 读取端点字段并去掉尾部斜杠（统一后续路径拼接的形态）。
fn endpoint(value: &Value) -> Option<String> {
    text(value).map(|raw| raw.trim_end_matches('/').to_string())
}

/// `parseProviderListing`: everything the `/provider` payload claims is
/// checked here.
/// 中文说明：parseProviderListing——/provider 载荷声称的一切都在这里
/// 逐条校验：all 列表检查 id/options/key/models，connected 数组去重
/// 保序收录；载荷不是对象时返回空快照。
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
/// 中文说明：Ok(None) 建模 JS 的"连接未接线"（if (!connection) return
/// null）；Err 为人类可读的失败原因。
pub type ProviderFetchFuture = BoxFuture<'static, Result<Option<Value>, String>>;
/// 快照抓取接缝：每次调用发起一次 /provider 请求（生产为引擎 fetch，
/// 测试注入桩）。
pub type ProviderFetch = Arc<dyn Fn() -> ProviderFetchFuture + Send + Sync>;

/// Production fetch against the managed engine (`buildOpenCodeUrl` +
/// `getOpenCodeAuthHeaders` + `AbortSignal.timeout`).
/// 中文说明：面向受管引擎的生产抓取——buildOpenCodeUrl +
/// getOpenCodeAuthHeaders + AbortSignal.timeout 的等价实现；引擎未
/// 运行（无 base URL）时返回 Ok(None)。
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

/// 快照缓存的内部状态（受 RuntimeProviders 的 mutex 保护）。
struct SnapshotState {
    /// 最近一次成功解析的快照；None 表示从未成功过。
    snapshot: Option<ProviderSnapshot>,
    /// TTL reference on the tokio clock (virtual under paused tests, wall
    /// clock in production — the same source `tokio::time` deadlines use).
    /// 中文说明：TTL 基准取 tokio 时钟——暂停测试下为虚拟时间，与
    /// tokio::time 的 deadline 同源。
    snapshot_at: Option<tokio::time::Instant>,
    /// 在飞的共享抓取 future；None 表示当前没有请求。
    inflight: Option<Shared<BoxFuture<'static, Result<Option<Value>, String>>>>,
    /// Ties the inflight clear to the flight that created it (JS: the
    /// `.finally` belongs to the one stored promise, not to each awaiter).
    /// 中文说明：把"清空在飞请求"绑定到创建它的那次 flight（JS 的
    /// .finally 属于被存储的那个 promise，而非每个 await 者）。
    generation: u64,
}

/// 状态的初始化与新鲜度判定。
impl SnapshotState {
    /// 构造全空状态（无快照、无在飞请求）。
    fn new() -> Self {
        Self {
            snapshot: None,
            snapshot_at: None,
            inflight: None,
            generation: 0,
        }
    }

    /// 已有快照且仍在 TTL 内时为 true（可直接复用缓存）。
    fn fresh(&self, ttl: Duration) -> bool {
        self.snapshot_at
            .is_some_and(|at| tokio::time::Instant::now().duration_since(at) < ttl)
    }
}

/// The cached runtime-provider view.
/// 中文说明：带缓存的运行时 provider 视图。
pub struct RuntimeProviders {
    /// 抓取接缝；None 表示未接线（一切查询回答"不知道"）。
    fetch: Option<ProviderFetch>,
    /// 快照缓存状态（含在飞请求与代号）。
    state: tokio::sync::Mutex<SnapshotState>,
    /// 快照新鲜期；测试可缩短以观察刷新行为。
    ttl: std::time::Duration,
}

/// 构造（未接线/生产 TTL/自定义 TTL）与查询入口。
impl RuntimeProviders {
    /// Unwired (`configureOpenCodeRuntimeProviders(null)`): every lookup
    /// answers "nothing known" and resolution stays file-based.
    /// 中文说明：未接线形态（configureOpenCodeRuntimeProviders(null)）
    /// ——每次查询都回答"不知道"，解析保持纯文件态。
    pub fn unwired() -> Arc<Self> {
        Arc::new(Self {
            fetch: None,
            state: tokio::sync::Mutex::new(SnapshotState::new()),
            ttl: std::time::Duration::from_millis(SNAPSHOT_TTL_MS),
        })
    }

    /// 以生产 TTL（30s）构造已接线的视图。
    pub fn new(fetch: ProviderFetch) -> Arc<Self> {
        Self::with_ttl(fetch, std::time::Duration::from_millis(SNAPSHOT_TTL_MS))
    }

    /// Test seam: a shortened TTL makes refresh behavior observable without
    /// waiting out the production window.
    /// 中文说明：测试接缝——缩短 TTL 让刷新行为可观察，无需等待
    /// 生产窗口。
    pub fn with_ttl(fetch: ProviderFetch, ttl: std::time::Duration) -> Arc<Self> {
        Arc::new(Self {
            fetch: Some(fetch),
            state: tokio::sync::Mutex::new(SnapshotState::new()),
            ttl,
        })
    }

    /// 用引擎派生的生产抓取器构造（service.rs 的 production 装配使用）。
    pub fn from_engine(engine: Arc<EngineState>) -> Arc<Self> {
        Self::new(engine_provider_fetch(engine))
    }

    /// `resetOpenCodeRuntimeProviders`: drops every cached answer.
    /// 中文说明：resetOpenCodeRuntimeProviders——丢弃全部缓存答案
    /// （快照时间戳与在飞请求）。
    pub async fn reset(&self) {
        let mut state = self.state.lock().await;
        state.snapshot = None;
        state.snapshot_at = None;
        state.inflight = None;
    }

    /// `getRuntimeProviderSnapshot`: the current snapshot, or `None` when
    /// OpenCode is unwired or cannot be reached and nothing was cached.
    /// 中文说明：getRuntimeProviderSnapshot——返回当前快照；OpenCode
    /// 未接线或不可达且无缓存时为 None。TTL 内直接复用；过期则单飞
    /// 刷新，失败时保留旧快照（瞬时不可达不应收回 provider）。
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
    /// 中文说明：getRuntimeProvider——单个 provider 的运行时凭据与
    /// 端点；OpenCode 对其一无所知时为 None。
    pub async fn provider(&self, provider_id: &str) -> Option<RuntimeProvider> {
        self.snapshot().await?.providers.get(provider_id).cloned()
    }
}

/// runtime-providers 的行为测试：载荷解析、单飞共享、TTL 缓存与
/// 不可达时的"未知而非空列表"语义。
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// 共享的 /provider 样例载荷：插件 key、zen 哨兵与仅模型端点三种形态。
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

    /// 解析应提取凭据、端点（去尾斜杠）与 zen 匿名哨兵标记，并保序收录 connected。
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

    /// provider 无 options.baseURL 时应回退到首个模型的 api.url，并采用 auth.json 风格的顶层 key。
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

    /// 并发调用应共享同一次在飞抓取；TTL 归零后的失败刷新不得抹掉旧快照。
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

    /// TTL 窗口内的重复读取只发起一次抓取。
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

    /// OpenCode 不可达时应回答 None（未知）而非空 provider 列表。
    #[tokio::test]
    async fn answers_unknown_rather_than_no_providers_when_unreachable() {
        let providers = RuntimeProviders::new(Arc::new(|| {
            Box::pin(async { Err("connection refused".to_string()) })
        }));
        assert!(providers.snapshot().await.is_none());
    }

    /// 未接线视图对任何 provider 都回答 None，解析保持文件态。
    #[tokio::test]
    async fn stays_on_file_based_resolution_until_configured() {
        let providers = RuntimeProviders::unwired();
        assert!(providers.provider("llmapi").await.is_none());
    }
}
