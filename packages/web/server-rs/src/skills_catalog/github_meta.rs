//! Port of `server/lib/skills-catalog/github-meta.js`: best-effort GitHub
//! repository metadata (stars, last push) for catalog enrichment. Failures
//! resolve to `null`, never throw, are deduplicated in-flight, and are
//! cached — 3h on success, 5min for failures — in memory and on disk
//! (`skills-github-meta.json` in the data dir). The HTTP fetch is a
//! 1500ms-budget transport seam (JS mocks `globalThis.fetch` in tests).
//!
//! 中文说明：尽力获取 GitHub 仓库元数据（star 数与最近推送时间）用于
//! 目录增强：失败一律解析为 `null` 绝不抛错；同仓库并发查询在途去重；
//! 成功缓存 3 小时、失败缓存 5 分钟，同时落盘到数据目录的
//! `skills-github-meta.json`。HTTP 抓取是 1500ms 预算的 transport seam，
//! 测试可注入假实现替代。

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use crate::skills_catalog::disk_cache::{now_millis, read_disk_cache, write_disk_cache};

/// GitHub REST API 根地址。
const GITHUB_API_BASE: &str = "https://api.github.com";
/// 成功元数据的缓存 TTL（3 小时）。
const CACHE_TTL_MS: u64 = 3 * 60 * 60 * 1000;
/// 失败结果的短缓存 TTL（5 分钟），避免反复命中被限流的 API。
const FAILURE_CACHE_TTL_MS: u64 = 5 * 60 * 1000;
/// 单次 HTTP 抓取的总预算（1500ms），超时按失败处理。
const FETCH_TIMEOUT_MS: u64 = 1_500;
/// 磁盘缓存文件名（位于数据目录内）。
const DISK_CACHE_FILE: &str = "skills-github-meta.json";
/// 磁盘写入防抖延迟（1s），合并短时间内的多次缓存更新。
const DISK_WRITE_DELAY_MS: u64 = 1_000;

/// `{ stars, repoUpdatedAt }` — both `null` when unknown.
/// 字段未知时为 `None`（序列化为 `null`），对应 JS 侧可空语义。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RepoMeta {
    /// 仓库 star 数（`stargazers_count`）。
    pub stars: Option<i64>,
    /// 仓库最近推送时间（`pushed_at`，ISO 8601 字符串）。
    pub repo_updated_at: Option<String>,
}

/// Transport seam for the `fetch()` call against `api.github.com`.
/// Returns `(status, body)` on the wire or an error message (timeouts,
/// network failures — the JS catch path).
/// 把 `fetch()` 抽象成 trait，测试注入假实现即可离线脚本化响应。
pub trait MetaTransport: Send + Sync {
    /// 抓取指定 URL，返回 `(HTTP 状态码, 响应体字节)` 或失败消息。
    fn fetch(&self, url: &str) -> BoxFuture<'static, Result<(u16, Vec<u8>), String>>;
}

/// Default transport: reqwest (rustls) with the 1500ms total budget.
/// 默认 transport：reqwest（rustls），整体超时即 1500ms 预算。
#[derive(Debug, Clone)]
pub struct ReqwestMetaTransport {
    /// 复用的 reqwest 客户端（已带超时配置）。
    client: reqwest::Client,
}

/// 默认 transport 的构造。
impl Default for ReqwestMetaTransport {
    /// 构造带 1500ms 超时的客户端；builder 失败退回客户端默认值。
    fn default() -> Self {
        ReqwestMetaTransport {
            client: reqwest::Client::builder()
                .timeout(Duration::from_millis(FETCH_TIMEOUT_MS))
                .build()
                .unwrap_or_default(),
        }
    }
}

/// MetaTransport 的 reqwest 实现。
impl MetaTransport for ReqwestMetaTransport {
    /// GET 指定 URL（带 GitHub JSON accept 头）并收集状态码与响应体。
    fn fetch(&self, url: &str) -> BoxFuture<'static, Result<(u16, Vec<u8>), String>> {
        let client = self.client.clone();
        let url = url.to_string();
        Box::pin(async move {
            let response = client
                .get(&url)
                .header("accept", "application/vnd.github+json")
                .send()
                .await
                .map_err(|error| error.to_string())?;
            let status = response.status().as_u16();
            let body = response.bytes().await.map_err(|error| error.to_string())?;
            Ok((status, body.to_vec()))
        })
    }
}

/// 进程级默认 transport 单例，`fetch_github_repo_metas` 直接使用。
static DEFAULT_TRANSPORT: LazyLock<Arc<dyn MetaTransport>> =
    LazyLock::new(|| Arc::new(ReqwestMetaTransport::default()));

/// 单个仓库的缓存条目（value 为全 None 的默认值时代表失败缓存）。
#[derive(Debug, Clone)]
struct MetaEntry {
    /// 条目到期时间（Unix 毫秒）。
    expires_at: u64,
    /// 缓存的元数据。
    value: RepoMeta,
}

/// 模块级共享状态：内存缓存、在途请求表、磁盘加载闩锁与防抖写任务。
struct MetaState {
    /// repo → 缓存条目。
    meta_cache: HashMap<String, MetaEntry>,
    /// repo → 在途共享 future（并发去重）。
    in_flight: HashMap<String, Shared<BoxFuture<'static, Option<RepoMeta>>>>,
    /// 磁盘缓存是否已一次性导入。
    disk_loaded: bool,
    /// 挂起中的防抖磁盘写任务句柄。
    disk_write_timer: Option<tokio::task::JoinHandle<()>>,
}

/// 全局状态互斥锁（惰性初始化）。
static STATE: LazyLock<Mutex<MetaState>> = LazyLock::new(|| {
    Mutex::new(MetaState {
        meta_cache: HashMap::new(),
        in_flight: HashMap::new(),
        disk_loaded: false,
        disk_write_timer: None,
    })
});

/// 锁定全局状态；锁中毒时恢复内部数据继续使用。
fn lock_state() -> MutexGuard<'static, MetaState> {
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `parseMeta(payload)`: `{ stars, repoUpdatedAt }` for object payloads,
/// `None` otherwise (`stars` only when `stargazers_count` is a finite
/// number, `repoUpdatedAt` only for non-empty strings).
/// 仅接受 JSON 对象：`stars` 取有限数字的 `stargazers_count`，
/// `repo_updated_at` 取非空字符串的 `pushed_at`；非对象载荷返回 None
/// （即 JS 的 `parseMeta() === null`）。
fn parse_meta(payload: &Value) -> Option<RepoMeta> {
    if !payload.is_object() {
        return None;
    }
    let stars = payload
        .get("stargazers_count")
        .and_then(Value::as_f64)
        .filter(|stars| stars.is_finite())
        .map(|stars| stars as i64);
    let repo_updated_at = payload
        .get("pushed_at")
        .and_then(Value::as_str)
        .filter(|pushed_at| !pushed_at.is_empty())
        .map(str::to_string);
    Some(RepoMeta {
        stars,
        repo_updated_at,
    })
}

/// 首次调用时把磁盘上未过期的合法条目导入内存并置位闩锁，此后不再
/// 读盘；损坏或过期条目直接忽略。
fn load_disk_entries(state: &mut MetaState) {
    if state.disk_loaded {
        return;
    }
    state.disk_loaded = true;
    let Some(Value::Object(persisted)) = read_disk_cache(DISK_CACHE_FILE) else {
        return;
    };
    let now = now_millis();
    for (repo, entry) in persisted {
        let Some(fields) = entry.as_object() else {
            continue;
        };
        let valid = fields
            .get("expiresAt")
            .and_then(Value::as_u64)
            .is_some_and(|expires_at| expires_at > now)
            && fields.get("value").is_some_and(Value::is_object);
        if valid {
            let expires_at = fields
                .get("expiresAt")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let value = fields
                .get("value")
                .and_then(|value| serde_json::from_value::<RepoMeta>(value.clone()).ok())
                .unwrap_or_default();
            state
                .meta_cache
                .insert(repo, MetaEntry { expires_at, value });
        }
    }
}

/// 已有防抖任务则返回；否则在当前 runtime 排一个 1s 后执行的任务：
/// 清句柄、收集未过期条目并整体写入磁盘文件。无 runtime 时跳过。
fn schedule_disk_write(state: &mut MetaState) {
    if state.disk_write_timer.is_some() {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let task = handle.spawn(async move {
        tokio::time::sleep(Duration::from_millis(DISK_WRITE_DELAY_MS)).await;
        let mut state = lock_state();
        state.disk_write_timer = None;
        let now = now_millis();
        let mut persisted = serde_json::Map::new();
        for (repo, entry) in &state.meta_cache {
            if entry.expires_at > now {
                persisted.insert(
                    repo.clone(),
                    serde_json::json!({
                        "expiresAt": entry.expires_at,
                        "value": entry.value,
                    }),
                );
            }
        }
        write_disk_cache(DISK_CACHE_FILE, &Value::Object(persisted));
    });
    state.disk_write_timer = Some(task);
}

/// 写入内存缓存（到期 = now + ttl）并调度防抖磁盘写入。
fn cache_repo_meta(repo: &str, value: RepoMeta, ttl_ms: u64) {
    let mut state = lock_state();
    state.meta_cache.insert(
        repo.to_string(),
        MetaEntry {
            expires_at: now_millis() + ttl_ms,
            value,
        },
    );
    schedule_disk_write(&mut state);
}

/// `fetchRepoMeta`: cached lookup, in-flight dedup, transport fetch, failure
/// caching. `None` mirrors the JS `null` returns.
/// 先查缓存（触发磁盘导入），未过期命中直接返回；否则取/建在途
/// shared future 并 await：transport 失败、非 2xx、JSON 解析失败均按
/// 失败短缓存返回 None，成功则按 3h TTL 缓存后返回 Some。
async fn fetch_repo_meta(
    transport: &Arc<dyn MetaTransport>,
    normalized_repo: &str,
) -> Option<RepoMeta> {
    {
        let mut state = lock_state();
        load_disk_entries(&mut state);
        if let Some(entry) = state.meta_cache.get(normalized_repo)
            && now_millis() < entry.expires_at
        {
            return Some(entry.value.clone());
        }
    }

    let run: Shared<BoxFuture<'static, Option<RepoMeta>>> = {
        let mut state = lock_state();
        if let Some(existing) = state.in_flight.get(normalized_repo) {
            existing.clone()
        } else {
            let transport = Arc::clone(transport);
            let repo = normalized_repo.to_string();
            let future = async move {
                let url = format!("{GITHUB_API_BASE}/repos/{repo}");
                let outcome = match transport.fetch(&url).await {
                    Err(_) => {
                        cache_repo_meta(&repo, RepoMeta::default(), FAILURE_CACHE_TTL_MS);
                        None
                    }
                    Ok((status, body)) => {
                        if !(200..300).contains(&status) {
                            // Cache failures briefly so repeated catalog loads
                            // do not re-hit a rate-limited API.
                            cache_repo_meta(&repo, RepoMeta::default(), FAILURE_CACHE_TTL_MS);
                            None
                        } else {
                            match serde_json::from_slice::<Value>(&body) {
                                // response.json() throwing is the JS catch
                                // path: failure cache + null.
                                Err(_) => {
                                    cache_repo_meta(
                                        &repo,
                                        RepoMeta::default(),
                                        FAILURE_CACHE_TTL_MS,
                                    );
                                    None
                                }
                                Ok(payload) => match parse_meta(&payload) {
                                    Some(meta) => {
                                        cache_repo_meta(&repo, meta.clone(), CACHE_TTL_MS);
                                        Some(meta)
                                    }
                                    // parseMeta() === null: returned, not cached.
                                    None => None,
                                },
                            }
                        }
                    }
                };
                lock_state().in_flight.remove(&repo);
                outcome
            }
            .boxed()
            .shared();
            state
                .in_flight
                .insert(normalized_repo.to_string(), future.clone());
            future
        }
    };

    run.await
}

/// `fetchGitHubRepoMetas(normalizedRepos)`: metadata per repo (empty/null
/// entries filtered, repos deduplicated), lookups run concurrently.
/// 并发获取多个仓库的元数据（使用默认 transport）。
pub async fn fetch_github_repo_metas(repos: &[String]) -> HashMap<String, Option<RepoMeta>> {
    fetch_github_repo_metas_with(Arc::clone(&DEFAULT_TRANSPORT), repos).await
}

/// Transport-injectable variant (the seam the JS tests reach by mocking
/// `globalThis.fetch`).
/// 先按非空去重仓库列表并发发起 `fetch_repo_meta`，再汇总为
/// repo → Option<RepoMeta> 映射（None 对应 JS 的 null 条目）。
pub async fn fetch_github_repo_metas_with(
    transport: Arc<dyn MetaTransport>,
    repos: &[String],
) -> HashMap<String, Option<RepoMeta>> {
    let mut seen = HashSet::new();
    let mut unique: Vec<String> = Vec::new();
    for repo in repos {
        if !repo.is_empty() && seen.insert(repo.clone()) {
            unique.push(repo.clone());
        }
    }

    let lookups = unique.into_iter().map(|repo| {
        let transport = Arc::clone(&transport);
        async move {
            let meta = fetch_repo_meta(&transport, &repo).await;
            (repo, meta)
        }
    });
    futures::future::join_all(lookups)
        .await
        .into_iter()
        .collect()
}

/// `clearGitHubMetaCache()`: test-only in-memory cache reset (JS also latches
/// the disk loader so no disk state is re-imported afterwards).
/// 只清内存缓存与在途表；同时闩死磁盘加载，避免清空后又从磁盘捞回。
pub fn clear_github_meta_cache() {
    let mut state = lock_state();
    state.meta_cache.clear();
    state.in_flight.clear();
    state.disk_loaded = true;
}

/// Test-only full reset (also clears the disk latch and pending writes).
/// 完全复位：终止挂起的防抖写任务、清缓存与在途表、复位磁盘闩锁。
#[cfg(test)]
pub(crate) fn reset_for_tests() {
    let mut state = lock_state();
    if let Some(timer) = state.disk_write_timer.take() {
        timer.abort();
    }
    state.meta_cache.clear();
    state.in_flight.clear();
    state.disk_loaded = false;
}

/// github-meta 测试：ScriptedTransport 脚本化响应，覆盖成功/失败解析、
/// 失败短缓存、去重过滤、磁盘持久化与跨“进程”复用。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills_catalog::test_support::{EnvGuard, TEST_LOCK, unique_temp_dir};
    use serde_json::json;

    /// 按脚本出响应的假 transport：记录调用 URL，响应从尾部弹出。
    struct ScriptedTransport {
        /// 待消费的脚本化响应栈（LIFO）。
        responses: Mutex<Vec<Result<(u16, Vec<u8>), String>>>,
        /// 已收到的 fetch URL 记录。
        calls: Mutex<Vec<String>>,
    }

    /// ScriptedTransport 的构造与断言辅助。
    impl ScriptedTransport {
        /// 生成 200 + JSON 体的成功响应。
        fn ok(payload: Value) -> Result<(u16, Vec<u8>), String> {
            Ok((200, serde_json::to_vec(&payload).expect("serialize")))
        }

        /// 生成指定状态码 + 文本体的响应。
        fn status(status: u16, body: &str) -> Result<(u16, Vec<u8>), String> {
            Ok((status, body.as_bytes().to_vec()))
        }

        /// 用响应栈构造共享的假 transport。
        fn new(responses: Vec<Result<(u16, Vec<u8>), String>>) -> Arc<Self> {
            Arc::new(ScriptedTransport {
                responses: Mutex::new(responses),
                calls: Mutex::new(Vec::new()),
            })
        }

        /// 收到的 fetch 调用次数（断言缓存命中用）。
        fn call_count(&self) -> usize {
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len()
        }
    }

    /// MetaTransport 的脚本化实现。
    impl MetaTransport for ScriptedTransport {
        /// 记录 URL、弹出栈顶响应返回；栈空时兜底返回 500。
        fn fetch(&self, url: &str) -> BoxFuture<'static, Result<(u16, Vec<u8>), String>> {
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(url.to_string());
            let response = self
                .responses
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .pop()
                .unwrap_or_else(|| Self::status(500, "no scripted response"));
            Box::pin(async move { response })
        }
    }

    // Responses are popped from the end (LIFO); reverse for FIFO scripting.
    /// 便捷构造：把 FIFO 语义的响应列表反转后交给 ScriptedTransport。
    fn scripted(responses: Vec<Result<(u16, Vec<u8>), String>>) -> Arc<ScriptedTransport> {
        let mut responses = responses;
        responses.reverse();
        ScriptedTransport::new(responses)
    }

    /// 行为契约：2xx JSON 响应解析出 stars 与 repoUpdatedAt。
    #[tokio::test]
    async fn returns_stars_and_pushed_at_from_the_github_api() {
        let _guard = TEST_LOCK.lock().await;
        reset_for_tests();

        let transport = scripted(vec![ScriptedTransport::ok(json!({
            "stargazers_count": 42,
            "pushed_at": "2026-08-01T00:00:00Z",
        }))]);

        let metas =
            fetch_github_repo_metas_with(transport, &["anthropics/skills".to_string()]).await;

        assert_eq!(
            metas.get("anthropics/skills"),
            Some(&Some(RepoMeta {
                stars: Some(42),
                repo_updated_at: Some("2026-08-01T00:00:00Z".to_string()),
            }))
        );
        clear_github_meta_cache();
    }

    /// 行为契约：403 等失败查找解析为 null 且不抛错。
    #[tokio::test]
    async fn resolves_failed_lookups_to_null_without_throwing() {
        let _guard = TEST_LOCK.lock().await;
        reset_for_tests();

        let transport = scripted(vec![ScriptedTransport::status(403, "rate limited")]);

        let metas =
            fetch_github_repo_metas_with(transport, &["anthropics/skills".to_string()]).await;

        assert_eq!(metas.get("anthropics/skills"), Some(&None));
        clear_github_meta_cache();
    }

    /// 行为契约：失败被短缓存，第二次查询不再打 transport。
    #[tokio::test]
    async fn caches_failed_lookups_briefly_to_avoid_repeat_hits() {
        let _guard = TEST_LOCK.lock().await;
        reset_for_tests();

        let transport = scripted(vec![ScriptedTransport::status(403, "rate limited")]);

        fetch_github_repo_metas_with(transport.clone(), &["anthropics/skills".to_string()]).await;
        let second =
            fetch_github_repo_metas_with(transport.clone(), &["anthropics/skills".to_string()])
                .await;

        assert_eq!(transport.call_count(), 1, "failure should be cached");
        assert_eq!(
            second.get("anthropics/skills"),
            Some(&Some(RepoMeta::default()))
        );
        clear_github_meta_cache();
    }

    /// 行为契约：仓库列表去重并过滤空串，每个仓库只查一次。
    #[tokio::test]
    async fn deduplicates_repositories_and_filters_empty() {
        let _guard = TEST_LOCK.lock().await;
        reset_for_tests();

        let transport = scripted(vec![ScriptedTransport::ok(json!({
            "stargazers_count": 1,
            "pushed_at": null,
        }))]);

        let metas = fetch_github_repo_metas_with(
            transport,
            &["a/b".to_string(), "a/b".to_string(), String::new()],
        )
        .await;

        assert_eq!(metas.len(), 1, "one unique non-empty repo: {metas:?}");
        assert_eq!(
            metas.get("a/b"),
            Some(&Some(RepoMeta {
                stars: Some(1),
                repo_updated_at: None,
            }))
        );
        clear_github_meta_cache();
    }

    /// 行为契约：transport 错误进入失败缓存（默认空 RepoMeta）。
    #[tokio::test]
    async fn transport_errors_are_failure_cached() {
        let _guard = TEST_LOCK.lock().await;
        reset_for_tests();

        let transport = scripted(vec![Err("network down".to_string())]);

        let metas = fetch_github_repo_metas_with(transport, &["a/b".to_string()]).await;
        assert_eq!(metas.get("a/b"), Some(&None));
        assert_eq!(
            lock_state().meta_cache.get("a/b").map(|entry| &entry.value),
            Some(&RepoMeta::default()),
            "failure entry cached"
        );
        clear_github_meta_cache();
    }

    /// 行为契约：非对象 JSON 载荷返回 null 且不写入缓存。
    #[tokio::test]
    async fn non_object_json_payloads_return_null_without_caching() {
        let _guard = TEST_LOCK.lock().await;
        reset_for_tests();

        let transport = scripted(vec![ScriptedTransport::ok(json!([1, 2, 3]))]);

        let metas = fetch_github_repo_metas_with(transport, &["a/b".to_string()]).await;
        assert_eq!(metas.get("a/b"), Some(&None));
        assert!(
            lock_state().meta_cache.get("a/b").is_none(),
            "parseMeta() null must not be cached"
        );
        clear_github_meta_cache();
    }

    /// 行为契约：条目防抖落盘后，新的“进程”（清内存、保磁盘）直接
    /// 复用磁盘条目而不再发起请求。
    #[tokio::test]
    async fn persists_entries_to_disk_and_reloads_them() {
        let _guard = TEST_LOCK.lock().await;
        let dir = unique_temp_dir("github-meta-test");
        let _env = EnvGuard::set("OMPCHAMBER_DATA_DIR", dir.to_str().expect("utf8"));
        reset_for_tests();

        let transport = scripted(vec![ScriptedTransport::ok(json!({
            "stargazers_count": 7,
            "pushed_at": "2026-01-01T00:00:00Z",
        }))]);
        fetch_github_repo_metas_with(transport, &["a/b".to_string()]).await;
        tokio::time::sleep(Duration::from_millis(1_200)).await;

        let raw = std::fs::read_to_string(dir.join("skills-github-meta.json")).expect("disk file");
        let on_disk: Value = serde_json::from_str(&raw).expect("json");
        assert_eq!(
            on_disk.pointer("/a~1b/value/stars"),
            Some(&json!(7)),
            "slash-escaped repo key with stars: {raw}"
        );

        // A later "process" (fresh memory, same disk) reuses the entry
        // without hitting the transport.
        reset_for_tests();
        let transport = scripted(vec![ScriptedTransport::status(500, "should not be called")]);
        let metas = fetch_github_repo_metas_with(transport, &["a/b".to_string()]).await;
        assert_eq!(
            metas.get("a/b"),
            Some(&Some(RepoMeta {
                stars: Some(7),
                repo_updated_at: Some("2026-01-01T00:00:00Z".to_string()),
            }))
        );
        clear_github_meta_cache();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 行为契约：parse_meta 对缺失、非数字与非对象载荷宽容返回 None。
    #[test]
    fn parse_meta_tolerates_missing_and_non_numeric_fields() {
        assert_eq!(
            parse_meta(&json!({ "stargazers_count": "many", "pushed_at": "" })),
            Some(RepoMeta {
                stars: None,
                repo_updated_at: None
            })
        );
        assert_eq!(parse_meta(&json!("scalar")), None);
        assert_eq!(parse_meta(&Value::Null), None);
    }
}
