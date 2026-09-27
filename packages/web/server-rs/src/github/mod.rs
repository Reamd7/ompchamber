//! Port of `server/lib/github/routes.js` (`registerGitHubRoutes`) — the
//! `/api/github/*` Express routes on axum.
//!
//! Octokit is replaced by [`client::GithubClient`]; every route preserves the
//! JS response shapes, status codes, conditional JSON keys, and the route-level
//! caches (90s PR-status cache, 30s PR-context cache, memoized auth login).
//!
//! 中文说明：本模块把旧 JS server 的 `server/lib/github/routes.js`
//! （`registerGitHubRoutes`，即全部 `/api/github/*` Express 路由）移植到
//! axum。Octokit 被替换为 [`client::GithubClient`]；每个路由都保持与
//! JS 版一致的响应结构、状态码、条件性 JSON 键以及路由级缓存
//! （pr/status 90 秒缓存、pulls/context 30 秒缓存、认证登录名 memo 化）。

/// GitHub OAuth 凭据的持久化存储（token、scope、多账号与 gh CLI 开关）。
mod auth;
/// GitHub REST/GraphQL 客户端封装（替代 Octokit），含错误类型与响应头工具。
mod client;
/// GitHub OAuth device flow（设备码换 token）的表单提交实现。
mod device_flow;
/// 读取本机 gh CLI 凭据（token 缓存 + `gh auth token` 兜底）的适配层。
mod gh_cli;
/// 目录级 git 操作（tracking branch、remote 名称列表等异步封装）。
mod git_ops;
/// PR status 解析引擎：从 git 目录/remote 解析仓库并查找分支对应的 PR。
mod pr_status;
/// GitHub API 全局速率限制门（429/retry-after 探测与冷却期降级）。
mod rate_limit;
/// 仓库解析：目录 remote URL → `RepoRef`，以及 fork/upstream 网络缓存。
mod repo;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{Map, Value, json};

use crate::context::RouterContext;
use auth::{AuthStore, AuthUser, GH_CLI_ACCOUNT_ID};
use client::{GithubClient, GithubError};
use device_flow::FormPoster;
use gh_cli::{GhCliCredential, TokenFetcher};
use pr_status::PrStatusEngine;
use rate_limit::{GLOBAL_GATE, RateLimitGate};
use repo::{NetworkRepo, RepoNetworkCache, RepoRef};

/// pr/status 路由级缓存的 TTL（毫秒），与 JS 版的 90 秒一致。
const PR_STATUS_CACHE_TTL_MS: i64 = 90_000;
/// pr/status 缓存的最大条目数；超出且写入新键时淘汰最旧条目。
const PR_STATUS_CACHE_MAX_ENTRIES: usize = 200;
/// pr/status 解析阶段的超时（毫秒），对应 JS 的 `withTimeout` 12 秒。
const PR_STATUS_RESOLVE_TIMEOUT_MS: u64 = 12_000;
/// pulls/context 路由级缓存的 TTL（毫秒），与 JS 版的 30 秒一致。
const PR_CONTEXT_CACHE_TTL_MS: i64 = 30_000;
/// pulls/context 缓存（Vec + FIFO 淘汰）的最大条目数。
const PR_CONTEXT_CACHE_MAX_ENTRIES: usize = 50;

/// 由 token 构造 [`GithubClient`] 的工厂；生产环境用 [`GithubClient::new`]，
/// 测试用它注入 fake transport。
type TransportFactory = Arc<dyn Fn(&str) -> GithubClient + Send + Sync>;

/// Route-level error mirroring the JS catch blocks.
/// 中文说明：GitHub API 错误或解析超时，二者决定不同的降级路径
/// （清除凭据 / 回退缓存 / 503）。
enum HandlerError {
    /// GitHub API 请求失败（携带状态码、响应头与可选响应体）。
    Github(GithubError),
    /// `withTimeout` rejection (`error.code === 'ETIMEDOUT'`).
    /// 中文说明：超时时优先回退到任何已缓存数据，否则返回 503。
    TimedOut,
}

/// 让 `?` 运算符把 [`GithubError`] 自动包装成 [`HandlerError::Github`]。
impl From<GithubError> for HandlerError {
    fn from(error: GithubError) -> Self {
        HandlerError::Github(error)
    }
}

/// 401/403 视为 token 失效或权限不足，调用方据此清除本地存储的凭据。
fn is_github_auth_invalid(error: &GithubError) -> bool {
    error.status == Some(401) || error.status == Some(403)
}

/// 403/404 视为资源不可见（私有仓库无权限或不存在），调用方据此返回
/// "已连接但无仓库/无 PR" 的降级载荷。
fn is_github_resource_unavailable(error: &GithubError) -> bool {
    error.status == Some(403) || error.status == Some(404)
}

/// pr/status 缓存条目：完整响应体与写入时刻。
struct PrStatusCacheEntry {
    /// 缓存的完整 JSON 响应体。
    data: Value,
    /// 写入时的毫秒时间戳，用于 TTL 判断与最旧条目淘汰。
    fetched_at: i64,
}

/// pulls/context 缓存条目：响应体、是否含 check 明细与写入时刻。
struct PrContextCacheEntry {
    /// 缓存的完整 JSON 响应体。
    data: Value,
    /// 生成该条目时是否请求了 checkDetails：含明细的缓存可服务
    /// 不带明细的请求，反之不可复用。
    include_check_details: bool,
    /// 写入时的毫秒时间戳，用于 TTL 判断。
    fetched_at: i64,
}

/// memo 化的认证登录名状态：未解析/上次失败（Idle）或已解析（可能为空）。
enum ResolvedAuthLogin {
    /// No attempt yet (or the last attempt failed — JS resets the memoized
    /// promise on failure so the next call retries).
    /// 中文说明：失败后回退到该状态，下次调用会重新发起请求。
    Idle,
    /// 已成功解析；`None` 表示响应中没有非空 `login`。
    Resolved(Option<String>),
}

/// 全部 `/api/github/*` 路由共享的状态：凭据存储、客户端工厂、
/// 各类缓存（pr/status、pulls/context、登录名 memo）与限流门。
pub struct GithubState {
    /// 路由上下文（数据目录等；路由实现暂未直接使用，保留供扩展）。
    #[allow(dead_code)]
    ctx: RouterContext,
    /// GitHub 凭据持久化存储（token、账号列表、gh CLI 开关）。
    auth: AuthStore,
    /// PR status 解析引擎（自带 repo→pulls 缓存）。
    engine: Arc<PrStatusEngine>,
    /// fork/upstream 仓库网络解析缓存。
    repo_network: Arc<RepoNetworkCache>,
    /// 全局 GitHub 速率限制门（生产环境共享 `GLOBAL_GATE`）。
    gate: Arc<RateLimitGate>,
    /// 本机 gh CLI 凭据读取器（带内部 token 缓存）。
    gh_cli: Arc<GhCliCredential>,
    /// 由 token 构造客户端的工厂（测试注入 fake transport 的接缝）。
    transport_factory: TransportFactory,
    /// device flow 的表单提交函数（测试可注入 fake）。
    form_poster: FormPoster,
    /// pr/status 路由级缓存：`directory::branch::remote` → 条目。
    pr_status_cache: Mutex<HashMap<String, PrStatusCacheEntry>>,
    /// pulls/context 路由级缓存（Vec 存储，超限 FIFO 淘汰）。
    pr_context_cache: Mutex<Vec<(String, PrContextCacheEntry)>>,
    /// memo 化的认证登录名（成功缓存、失败重试）。
    resolved_auth_login: Mutex<ResolvedAuthLogin>,
}

/// [`GithubState`] 的构造（生产/测试接缝）、token 选择与缓存读写。
impl GithubState {
    /// 生产构造：真实 transport 工厂、全局限流门与 gh CLI 全局实例。
    fn from_ctx(ctx: RouterContext) -> Arc<Self> {
        Arc::new(Self {
            auth: AuthStore::new(ctx.config.data_dir.clone()),
            ctx,
            engine: Arc::new(PrStatusEngine::new()),
            repo_network: Arc::new(RepoNetworkCache::new()),
            gate: Arc::clone(&GLOBAL_GATE),
            gh_cli: GhCliCredential::global(),
            transport_factory: Arc::new(GithubClient::new),
            form_poster: device_flow::default_form_poster(),
            pr_status_cache: Mutex::new(HashMap::new()),
            pr_context_cache: Mutex::new(Vec::new()),
            resolved_auth_login: Mutex::new(ResolvedAuthLogin::Idle),
        })
    }

    /// Test seams: fake transport factory, form poster, gh fetcher, gate.
    /// 中文说明：用临时数据目录 + 注入组件组装状态，不依赖真实
    /// 配置文件与网络。
    #[allow(clippy::too_many_arguments)]
    fn with_seams(
        data_dir: PathBuf,
        transport_factory: TransportFactory,
        form_poster: FormPoster,
        gh_fetcher: TokenFetcher,
        gate: Arc<RateLimitGate>,
        engine: Arc<PrStatusEngine>,
    ) -> Arc<Self> {
        let auth = AuthStore::new(data_dir.clone());
        let ctx = RouterContext {
            config: Arc::new(crate::config::ServerConfig {
                port: 0,
                host: None,
                lan: false,
                ui_password: None,
                api_only: false,
                data_dir: data_dir.clone(),
                dist_dir: data_dir,
                tunnel: Default::default(),
                engine: crate::config::EngineConfig::External {
                    base_url: "http://127.0.0.1:1".to_string(),
                },
            }),
            engine: crate::engine::EngineState::external("http://127.0.0.1:1".to_string(), None),
            hub: crate::hub::EventHub::new(),
        };
        Arc::new(Self {
            auth,
            ctx,
            engine,
            repo_network: Arc::new(RepoNetworkCache::new()),
            gate,
            gh_cli: Arc::new(GhCliCredential::with_fetcher(gh_fetcher)),
            transport_factory,
            form_poster,
            pr_status_cache: Mutex::new(HashMap::new()),
            pr_context_cache: Mutex::new(Vec::new()),
            resolved_auth_login: Mutex::new(ResolvedAuthLogin::Idle),
        })
    }

    /// `getOctokitOrNull`.
    /// 中文说明：gh CLI 激活时优先其 token、否则优先自有 token，
    /// 另一方作为兜底；两者皆无时返回 `None`（未连接）。
    fn get_octokit_or_null(&self) -> Option<GithubClient> {
        let gh_token = if !self.auth.is_gh_cli_disabled() {
            self.gh_cli.token()
        } else {
            None
        };
        let own_token = self.auth.get_github_auth().map(|entry| entry.access_token);
        let token = if self.auth.is_gh_cli_active() {
            gh_token.or(own_token)
        } else {
            own_token.or(gh_token)
        };
        token.map(|token| (self.transport_factory)(&token))
    }

    /// 写入 pr/status 缓存；达到上限且是新键时先淘汰 `fetched_at` 最旧的条目。
    fn set_pr_status_cache(&self, key: &str, data: Value, fetched_at: i64) {
        let mut cache = self
            .pr_status_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if cache.len() >= PR_STATUS_CACHE_MAX_ENTRIES
            && !cache.contains_key(key)
            && let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, entry)| entry.fetched_at)
                .map(|(k, _)| k.clone())
        {
            cache.remove(&oldest);
        }
        cache.insert(key.to_string(), PrStatusCacheEntry { data, fetched_at });
    }

    /// 读取 TTL 内的 pr/status 缓存；命中返回响应体克隆。
    fn cached_pr_status(&self, key: &str, now: i64) -> Option<Value> {
        let cache = self
            .pr_status_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        cache
            .get(key)
            .filter(|e| now - e.fetched_at < PR_STATUS_CACHE_TTL_MS)
            .map(|e| e.data.clone())
    }

    /// 读取 pr/status 缓存但不校验 TTL——限流/超时时的降级数据源。
    fn any_cached_pr_status(&self, key: &str) -> Option<Value> {
        self.pr_status_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .map(|e| e.data.clone())
    }

    /// The route's `res.json` override: stamp `fetchedAt` and cache
    /// `connected: true` payloads before sending.
    /// 中文说明：`connected:true` 的载荷补上 `fetchedAt` 并写入缓存，
    /// 断连载荷不缓存。
    fn send_pr_status(&self, cache_key: &str, mut data: Value) -> Response {
        if data.get("connected") == Some(&Value::Bool(true)) {
            if data.get("fetchedAt").and_then(Value::as_i64).is_none()
                && let Some(obj) = data.as_object_mut()
            {
                obj.insert("fetchedAt".to_string(), json!(rate_limit::now_ms()));
            }
            let fetched_at = data
                .get("fetchedAt")
                .and_then(Value::as_i64)
                .unwrap_or_else(rate_limit::now_ms);
            self.set_pr_status_cache(cache_key, data.clone(), fetched_at);
        }
        Json(data).into_response()
    }

    /// The pulls/context `res.json` override: cache payloads that carry `pr`.
    /// 中文说明：带非空 `pr` 的载荷补上 `fetchedAt` 并写入 FIFO 缓存。
    fn send_pr_context(
        &self,
        cache_key: &str,
        include_check_details: bool,
        mut data: Value,
    ) -> Response {
        if data.get("pr").map(|v| !v.is_null()).unwrap_or(false) {
            if data.get("fetchedAt").and_then(Value::as_i64).is_none()
                && let Some(obj) = data.as_object_mut()
            {
                obj.insert("fetchedAt".to_string(), json!(rate_limit::now_ms()));
            }
            let fetched_at = data
                .get("fetchedAt")
                .and_then(Value::as_i64)
                .unwrap_or_else(rate_limit::now_ms);
            let mut cache = self
                .pr_context_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            cache.retain(|(k, _)| k != cache_key);
            cache.push((
                cache_key.to_string(),
                PrContextCacheEntry {
                    data: data.clone(),
                    include_check_details,
                    fetched_at,
                },
            ));
            if cache.len() > PR_CONTEXT_CACHE_MAX_ENTRIES {
                cache.remove(0);
            }
        }
        Json(data).into_response()
    }

    /// `invalidatePrContextCache`.
    /// 中文说明：缓存键是 `[directory, number]` 的 JSON 序列化，
    /// `number` 为 `None` 时清空该目录下全部条目。
    fn invalidate_pr_context_cache(&self, directory: &str, number: Option<i64>) {
        self.pr_context_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(key, _)| {
                let parsed: Value = match serde_json::from_str(key) {
                    Ok(value) => value,
                    Err(_) => return false,
                };
                let cached_directory = parsed.get(0).and_then(Value::as_str);
                let cached_number = parsed.get(1).and_then(Value::as_i64);
                cached_directory == Some(directory)
                    && number.is_none_or(|n| cached_number == Some(n))
            });
    }

    /// `getGitHubUserSummary`: authenticated user + verified email fallback.
    /// 中文说明：profile 未公开 email 时，从邮箱列表里依次尝试
    /// "已验证且 primary" → "已验证" 的回退。
    async fn get_github_user_summary(client: &GithubClient) -> Result<Value, GithubError> {
        let me = client.users_get_authenticated().await?;

        let mut email = me.get("email").and_then(Value::as_str).map(str::to_string);
        if email.is_none()
            && let Ok(emails) = client.users_list_emails().await
        {
            let list = emails.as_array().cloned().unwrap_or_default();
            let pick = |verified_only: bool| {
                list.iter().find(|e| {
                    let verified = e.get("verified").and_then(Value::as_bool) == Some(true);
                    (verified || !verified_only)
                        && e.get("email").map(|v| v.is_string()).unwrap_or(false)
                        && (!verified_only
                            || e.get("primary").and_then(Value::as_bool) == Some(true))
                })
            };
            email = pick(true)
                .or_else(|| pick(false))
                .and_then(|e| e.get("email").and_then(Value::as_str).map(str::to_string));
        }

        Ok(json!({
            "login": me.get("login").cloned().unwrap_or(Value::Null),
            "id": me.get("id").cloned().unwrap_or(Value::Null),
            "avatarUrl": me.get("avatar_url").cloned().unwrap_or(Value::Null),
            "name": me.get("name").filter(|v| v.is_string()).cloned().unwrap_or(Value::Null),
            "email": email,
        }))
    }

    /// Memoized `octokit.rest.users.getAuthenticated()` login (JS
    /// `resolvedAuthLoginPromise`): cached on success, retried after failure.
    /// 中文说明：并发下可能重复请求，但成功后所有调用共享缓存结果。
    async fn resolve_auth_login(&self, client: &GithubClient) -> Option<String> {
        {
            let memo = self
                .resolved_auth_login
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let ResolvedAuthLogin::Resolved(login) = &*memo {
                return login.clone();
            }
        }
        match client.users_get_authenticated().await {
            Ok(response) => {
                let login = response
                    .get("login")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .filter(|l| !l.is_empty());
                *self
                    .resolved_auth_login
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) =
                    ResolvedAuthLogin::Resolved(login.clone());
                login
            }
            Err(_) => {
                *self
                    .resolved_auth_login
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = ResolvedAuthLogin::Idle;
                None
            }
        }
    }

    /// `getRequestedRepo`.
    /// 中文说明：`owner`/`repo` 任一为空（或缺失）即视为未指定。
    fn requested_repo(query: &HashMap<String, String>) -> Option<RepoRef> {
        let owner = query
            .get("owner")
            .map(String::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let repo = query
            .get("repo")
            .map(String::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if owner.is_empty() || repo.is_empty() {
            return None;
        }
        Some(RepoRef {
            url: format!("https://github.com/{owner}/{repo}"),
            owner,
            repo,
        })
    }

    /// `resolveRepoForRequest`.
    /// 中文说明：显式仓库与目录仓库不一致时，需出现在 fork 网络内
    /// 才被接受，否则返回 `None`（调用方回退为无仓库载荷）。
    async fn resolve_repo_for_request(
        &self,
        client: &GithubClient,
        directory: &str,
        requested_repo: Option<RepoRef>,
    ) -> Option<RepoRef> {
        let (repo, _) = repo::resolve_github_repo_from_directory(directory, "origin").await;
        let Some(requested) = requested_repo else {
            return repo;
        };
        if let Some(repo) = repo
            && repo.owner == requested.owner
            && repo.repo == requested.repo
        {
            return Some(requested);
        }
        let network = self
            .repo_network
            .resolve_repo_network(client, directory, "origin")
            .await
            .unwrap_or(None);
        let allowed = network
            .unwrap_or_default()
            .iter()
            .any(|item| item.owner == requested.owner && item.repo == requested.repo);
        if allowed { Some(requested) } else { None }
    }
}

/// JSON helper: insert only when `Some` (JS `undefined` keys are omitted).
/// 中文说明：仅在 `Some` 时插入键值，模拟 JS 对象里 `undefined` 键被省略的行为。
fn set_opt(map: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        map.insert(key.to_string(), value);
    }
}

/// 构造 `{"error": message}` 形式的 JSON 错误响应（带指定状态码）。
fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// 记录 error 日志并返回 500；message 为空时使用 fallback 文案。
fn internal_error(message: String, fallback: &str) -> Response {
    tracing::error!("{message}");
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        if message.is_empty() {
            fallback.to_string()
        } else {
            message
        },
    )
}

// ============================================================================
// Aggregation helpers (dedupeCheckRuns / summarizeCheckRuns /
// summarizeCombinedStatuses)
// ============================================================================

/// GitHub's UI shows only the latest run per (app, name); mirror that.
/// 中文说明：按 `app id::name` 分组，保留 `started_at` 最新（相同时
/// id 更大）的那条，供汇总计数不重复。
fn dedupe_check_runs(check_runs: &[Value]) -> Vec<Value> {
    // (key, started_at_epoch, id, run)
    let mut by_name: Vec<(String, f64, i64, Value)> = Vec::new();
    for run in check_runs {
        let app_id = run
            .pointer("/app/id")
            .and_then(Value::as_i64)
            .map(|v| v.to_string())
            .or_else(|| {
                run.pointer("/app/slug")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default();
        let name = run.get("name").and_then(Value::as_str).unwrap_or("");
        let key = format!("{app_id}::{name}");
        let started_at = parse_iso_ms(run.get("started_at").and_then(Value::as_str).unwrap_or(""));
        let id = run.get("id").and_then(Value::as_i64).unwrap_or(0);
        match by_name.iter_mut().find(|(k, _, _, _)| *k == key) {
            None => by_name.push((key, started_at, id, run.clone())),
            Some(entry) => {
                if started_at > entry.1 || (started_at == entry.1 && id > entry.2) {
                    *entry = (key, started_at, id, run.clone());
                }
            }
        }
    }
    by_name.into_iter().map(|(_, _, _, run)| run).collect()
}

/// `Date.parse` for ISO-8601 shapes GitHub returns (all UTC `Z` forms);
/// 0.0 when unparseable (matching `Date.parse(...) || 0`).
/// 中文说明：支持 `YYYY-MM-DD`、`[T ]HH:MM[:SS[.fff]][Z|±HH:MM]` 等
/// GitHub 返回的形态；无法解析时按 JS `Date.parse(...) || 0` 语义返回 0.0。
fn parse_iso_ms(value: &str) -> f64 {
    let value = value.trim();
    if value.is_empty() {
        return 0.0;
    }
    // YYYY-MM-DD[T ]HH:MM[:SS[.SSS]][Z|±HH:MM]
    let bytes = value.as_bytes();
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return f64::NAN;
    }
    let year: i64 = match value[0..4].parse() {
        Ok(v) => v,
        Err(_) => return f64::NAN,
    };
    let month: i64 = match value[5..7].parse() {
        Ok(v) => v,
        Err(_) => return f64::NAN,
    };
    let day: i64 = match value[8..10].parse() {
        Ok(v) => v,
        Err(_) => return f64::NAN,
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return f64::NAN;
    }
    let (hour, minute, second, millis, offset_seconds) = if bytes.len() == 10 {
        (0, 0, 0, 0.0, 0)
    } else {
        let rest = &value[10..];
        let rest = rest
            .strip_prefix('T')
            .or_else(|| rest.strip_prefix(' '))
            .unwrap_or(rest);
        if rest.len() < 5 || rest.as_bytes()[2] != b':' {
            return f64::NAN;
        }
        let hour: i64 = match rest[0..2].parse() {
            Ok(v) => v,
            Err(_) => return f64::NAN,
        };
        let minute: i64 = match rest[3..5].parse() {
            Ok(v) => v,
            Err(_) => return f64::NAN,
        };
        let mut second = 0;
        let mut millis = 0.0;
        let mut idx = 5;
        let tail = &rest[idx..];
        if let Some(t) = tail.strip_prefix(':')
            && t.len() >= 2
        {
            second = t[0..2].parse().unwrap_or(0);
            idx += 3;
            if let Some(frac) = rest[idx..].strip_prefix('.') {
                let digits: String = frac.chars().take_while(|c| c.is_ascii_digit()).collect();
                if !digits.is_empty() {
                    millis = format!("0.{digits}").parse().unwrap_or(0.0);
                    idx += 1 + digits.len();
                }
            }
        }
        let zone = &rest[idx..];
        let offset_seconds = match zone {
            "" | "Z" | "z" => 0,
            z if z.starts_with('+') || z.starts_with('-') => {
                let sign = if z.starts_with('+') { 1 } else { -1 };
                let zh: i64 = z[1..3].parse().unwrap_or(0);
                let zm: i64 = if z.len() >= 6 {
                    z[4..6].parse().unwrap_or(0)
                } else {
                    0
                };
                sign * (zh * 3600 + zm * 60)
            }
            _ => 0,
        };
        (hour, minute, second, millis, offset_seconds)
    };

    // days from civil (Howard Hinnant's algorithm), UTC-normalized.
    let yy = if month <= 2 { year - 1 } else { year };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset_seconds;
    seconds as f64 * 1000.0 + millis
}

/// 把 check-run 列表汇总为计数对象：success/failure/pending（细分
/// inProgress/queued）、最早的 startedAt（仅未完成 run 参与）与聚合
/// state（failure > pending > success > unknown）。
fn summarize_check_runs(check_runs: &[Value]) -> Value {
    let mut success = 0i64;
    let mut failure = 0i64;
    let mut pending = 0i64;
    let mut in_progress = 0i64;
    let mut queued = 0i64;
    let mut started_at: Option<String> = None;
    for run in check_runs {
        let status = run.get("status").and_then(Value::as_str).unwrap_or("");
        let conclusion = run.get("conclusion").and_then(Value::as_str);
        if status == "in_progress" {
            pending += 1;
            in_progress += 1;
            if let Some(run_started_at) = run.get("started_at").and_then(Value::as_str) {
                let better = match &started_at {
                    None => true,
                    Some(current) => run_started_at < current.as_str(),
                };
                if better {
                    started_at = Some(run_started_at.to_string());
                }
            }
            continue;
        }
        if status == "queued" {
            pending += 1;
            queued += 1;
            continue;
        }
        let Some(conclusion) = conclusion else {
            pending += 1;
            continue;
        };
        match conclusion {
            "success" | "neutral" | "skipped" => success += 1,
            _ => failure += 1,
        }
    }
    let total = success + failure + pending;
    let state = if failure > 0 {
        "failure"
    } else if pending > 0 {
        "pending"
    } else if total > 0 {
        "success"
    } else {
        "unknown"
    };
    let mut summary = json!({
        "state": state,
        "total": total,
        "success": success,
        "failure": failure,
        "pending": pending,
        "inProgress": in_progress,
        "queued": queued,
    });
    if let Some(started_at) = started_at
        && let Some(obj) = summary.as_object_mut()
    {
        obj.insert("startedAt".to_string(), json!(started_at));
    }
    summary
}

/// 把 legacy commit status 列表汇总为与 check-runs 相同的形状；
/// error 计入 failure，inProgress 复用 pending、queued 恒为 0。
fn summarize_combined_statuses(statuses: &[Value]) -> Value {
    let mut success = 0i64;
    let mut failure = 0i64;
    let mut pending = 0i64;
    for status in statuses {
        match status.get("state").and_then(Value::as_str).unwrap_or("") {
            "success" => success += 1,
            "failure" | "error" => failure += 1,
            "pending" => pending += 1,
            _ => {}
        }
    }
    let total = success + failure + pending;
    let state = if failure > 0 {
        "failure"
    } else if pending > 0 {
        "pending"
    } else if total > 0 {
        "success"
    } else {
        "unknown"
    };
    json!({
        "state": state,
        "total": total,
        "success": success,
        "failure": failure,
        "pending": pending,
        "inProgress": pending,
        "queued": 0,
    })
}

// ============================================================================
// Routes
// ============================================================================

/// 注册全部 `/api/github/*` 路由并注入共享的 [`GithubState`]。
pub fn router(ctx: RouterContext) -> Router {
    let state = GithubState::from_ctx(ctx);
    Router::new()
        .route("/api/github/auth/status", get(auth_status))
        .route("/api/github/auth/gh-cli", post(auth_gh_cli))
        .route("/api/github/auth/start", post(auth_start))
        .route("/api/github/auth/complete", post(auth_complete))
        .route("/api/github/auth/activate", post(auth_activate))
        .route("/api/github/auth", delete(auth_delete))
        .route("/api/github/me", get(me))
        .route("/api/github/pr/status", get(pr_status_route))
        .route("/api/github/pr/create", post(pr_create))
        .route("/api/github/pr/update", post(pr_update))
        .route("/api/github/pr/merge", post(pr_merge))
        .route("/api/github/pr/ready", post(pr_ready))
        .route("/api/github/repo/upstream", get(repo_upstream))
        .route("/api/github/repo/branches", get(repo_branches))
        .route("/api/github/issues/list", get(issues_list))
        .route("/api/github/issues/get", get(issues_get))
        .route("/api/github/issues/comments", get(issues_comments))
        .route("/api/github/pulls/list", get(pulls_list))
        .route("/api/github/pulls/context", get(pulls_context))
        .with_state(state)
}

/// GET /api/github/auth/status
/// 中文说明：汇总存储账号与 gh CLI 账号、当前用户与 token 有效性。
async fn auth_status(State(state): State<Arc<GithubState>>) -> Response {
    match auth_status_inner(&state).await {
        Ok(response) => response,
        Err(message) => internal_error(message, "Failed to get GitHub auth status"),
    }
}

/// auth/status 主体：拼装账号列表（含 gh CLI 虚拟账号），自有 token
/// 失效时清除存储并返回未连接。
async fn auth_status_inner(state: &GithubState) -> Result<Response, String> {
    let auth = state.auth.get_github_auth();
    let mut accounts: Vec<Value> = state
        .auth
        .get_github_auth_accounts()
        .into_iter()
        .map(|account| account.to_json())
        .collect();
    let gh_cli_disabled = state.auth.is_gh_cli_disabled();
    let gh_cli_active = state.auth.is_gh_cli_active();
    let gh_token = state.gh_cli.token();
    let using_own_token = auth
        .as_ref()
        .map(|a| !a.access_token.is_empty())
        .unwrap_or(false);

    let mut gh_cli_user: Option<Value> = None;
    if gh_token.is_some() && !gh_cli_disabled {
        gh_cli_user = GithubState::get_github_user_summary(&(state.transport_factory)(
            gh_token.as_deref().unwrap_or(""),
        ))
        .await
        .ok();
    }
    if gh_cli_active && gh_cli_user.is_none() {
        state.auth.set_gh_cli_active(false);
    }

    let gh_cli_current = gh_token.is_some()
        && !gh_cli_disabled
        && gh_cli_user.is_some()
        && (gh_cli_active || !using_own_token);
    if let Some(user) = gh_cli_user.clone() {
        for account in accounts.iter_mut() {
            if gh_cli_current && let Some(map) = account.as_object_mut() {
                map.insert("current".to_string(), Value::Bool(false));
            }
        }
        accounts.push(json!({
            "id": GH_CLI_ACCOUNT_ID,
            "user": user,
            "current": gh_cli_current,
            "source": "gh-cli",
        }));
    }

    let build_gh_cli = |active_user: Option<Value>| -> Value {
        let mut obj = json!({
            "available": gh_token.is_some(),
            "disabled": gh_cli_disabled,
            "active": gh_cli_current,
        });
        if let Some(map) = obj.as_object_mut() {
            let user = active_user.or_else(|| gh_cli_user.clone());
            if !gh_cli_disabled && user.is_some() {
                map.insert("user".to_string(), user.unwrap());
            }
        }
        obj
    };

    let octokit = state.get_octokit_or_null();
    let Some(octokit) = octokit else {
        return Ok(Json(json!({
            "connected": false,
            "accounts": accounts,
            "ghCli": build_gh_cli(None),
        }))
        .into_response());
    };

    let mut user: Option<Value> = None;
    match GithubState::get_github_user_summary(&octokit).await {
        Ok(summary) => user = Some(summary),
        Err(error) => {
            if is_github_auth_invalid(&error) {
                if using_own_token {
                    state.auth.clear_github_auth();
                }
                let accounts: Vec<Value> = state
                    .auth
                    .get_github_auth_accounts()
                    .into_iter()
                    .map(|a| a.to_json())
                    .collect();
                return Ok(Json(json!({
                    "connected": false,
                    "accounts": accounts,
                    "ghCli": build_gh_cli(None),
                }))
                .into_response());
            }
        }
    }

    let fallback = if using_own_token {
        auth.as_ref()
            .and_then(|a| a.user.as_ref())
            .map(AuthUser::to_json_null_filled)
    } else {
        None
    };
    let merged_user = user.or(fallback).unwrap_or(Value::Null);
    let mut payload = json!({
        "connected": true,
        "user": merged_user,
        "accounts": accounts,
        "ghCli": build_gh_cli(if gh_cli_current { Some(merged_user.clone()) } else { None }),
    });
    if let Some(map) = payload.as_object_mut() {
        // `scope: ghCliCurrent ? undefined : auth?.scope` — omitted when the
        // gh CLI is current or there is no stored auth; empty string included.
        if !gh_cli_current && let Some(auth) = auth.as_ref() {
            map.insert("scope".to_string(), json!(auth.scope));
        }
    }
    Ok(Json(payload).into_response())
}

/// POST /api/github/auth/gh-cli
/// 中文说明：disabled 标志持久化，gh CLI token 缓存立即清空。
async fn auth_gh_cli(State(state): State<Arc<GithubState>>, Json(body): Json<Value>) -> Response {
    let disabled = body
        .get("disabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    state.auth.set_gh_cli_disabled(disabled);
    state.gh_cli.clear();
    Json(json!({ "disabled": state.auth.is_gh_cli_disabled() })).into_response()
}

/// POST /api/github/auth/start
/// 中文说明：client id 未配置时直接 400。
async fn auth_start(State(state): State<Arc<GithubState>>) -> Response {
    let client_id = state.auth.get_github_client_id();
    if client_id.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "GitHub OAuth client not configured. Set OMPCHAMBER_GITHUB_CLIENT_ID.",
        );
    }
    let scope = state.auth.get_github_scopes();
    match device_flow::start_device_flow(&state.form_poster, &client_id, &scope).await {
        Ok(payload) => {
            let get_str = |key: &str| payload.get(key).and_then(Value::as_str).map(|v| json!(v));
            Json(json!({
                "deviceCode": get_str("device_code").unwrap_or(Value::Null),
                "userCode": get_str("user_code").unwrap_or(Value::Null),
                "verificationUri": get_str("verification_uri").unwrap_or(Value::Null),
                "verificationUriComplete": get_str("verification_uri_complete").unwrap_or(Value::Null),
                "expiresIn": payload.get("expires_in").cloned().unwrap_or(Value::Null),
                "interval": payload.get("interval").cloned().unwrap_or(Value::Null),
                "scope": scope,
            }))
            .into_response()
        }
        Err(error) => internal_error(error.message, "Failed to start GitHub device flow"),
    }
}

/// POST /api/github/auth/complete
/// 中文说明：deviceCode 缺失 400；GitHub 返回 error 时透传
/// `connected:false` + 错误码；成功则拉取用户并持久化 token。
async fn auth_complete(State(state): State<Arc<GithubState>>, Json(body): Json<Value>) -> Response {
    let client_id = state.auth.get_github_client_id();
    if client_id.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "GitHub OAuth client not configured. Set OMPCHAMBER_GITHUB_CLIENT_ID.",
        );
    }

    let device_code = body
        .get("deviceCode")
        .and_then(Value::as_str)
        .or_else(|| body.get("device_code").and_then(Value::as_str))
        .unwrap_or("");
    if device_code.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "deviceCode is required");
    }

    let payload = match device_flow::exchange_device_code(
        &state.form_poster,
        &client_id,
        device_code,
    )
    .await
    {
        Ok(payload) => payload,
        Err(error) => {
            return internal_error(error.message, "Failed to complete GitHub device flow");
        }
    };

    if let Some(error_code) = payload.get("error").and_then(Value::as_str) {
        return Json(json!({
            "connected": false,
            "status": error_code,
            "error": payload
                .get("error_description")
                .and_then(Value::as_str)
                .unwrap_or(error_code),
        }))
        .into_response();
    }

    let Some(access_token) = payload.get("access_token").and_then(Value::as_str) else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Missing access_token from GitHub",
        );
    };

    let octokit = (state.transport_factory)(access_token);
    let user = match GithubState::get_github_user_summary(&octokit).await {
        Ok(user) => user,
        Err(error) => {
            return internal_error(error.message, "Failed to complete GitHub device flow");
        }
    };

    let scope = payload.get("scope").and_then(Value::as_str).unwrap_or("");
    let token_type = payload
        .get("token_type")
        .and_then(Value::as_str)
        .unwrap_or("bearer");
    let user_model = auth_user_from_summary(&user);
    let _ = state
        .auth
        .set_github_auth(access_token, scope, token_type, user_model, None);
    let accounts: Vec<Value> = state
        .auth
        .get_github_auth_accounts()
        .into_iter()
        .map(|a| a.to_json())
        .collect();

    Json(json!({
        "connected": true,
        "user": user,
        "scope": scope,
        "accounts": accounts,
    }))
    .into_response()
}

/// Map the user-summary JSON back into the stored `AuthUser` shape.
/// 中文说明：非对象输入返回 `None`；字段缺失即为 `None`。
fn auth_user_from_summary(user: &Value) -> Option<AuthUser> {
    if !user.is_object() {
        return None;
    }
    let field = |key: &str| user.get(key).and_then(Value::as_str).map(str::to_string);
    Some(AuthUser {
        login: field("login"),
        avatar_url: field("avatarUrl"),
        id: user.get("id").and_then(Value::as_i64),
        name: field("name"),
        email: field("email"),
    })
}

/// POST /api/github/auth/activate
/// 中文说明：gh CLI 账号激活后其余存储账号全部置为非当前。
async fn auth_activate(State(state): State<Arc<GithubState>>, Json(body): Json<Value>) -> Response {
    let account_id = body
        .get("accountId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if account_id.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "accountId is required");
    }

    if account_id == GH_CLI_ACCOUNT_ID {
        let gh_token = if !state.auth.is_gh_cli_disabled() {
            state.gh_cli.token()
        } else {
            None
        };
        let Some(gh_token) = gh_token else {
            return error_response(StatusCode::NOT_FOUND, "GitHub CLI account not found");
        };
        let octokit = (state.transport_factory)(&gh_token);
        let user = match GithubState::get_github_user_summary(&octokit).await {
            Ok(user) => user,
            Err(error) => {
                return internal_error(error.message, "Failed to activate GitHub account");
            }
        };
        state.auth.set_gh_cli_active(true);
        let accounts: Vec<Value> = state
            .auth
            .get_github_auth_accounts()
            .into_iter()
            .map(|mut account| {
                account.current = false;
                account.to_json()
            })
            .chain(std::iter::once(json!({
                "id": GH_CLI_ACCOUNT_ID,
                "user": user,
                "current": true,
                "source": "gh-cli",
            })))
            .collect();
        return Json(json!({
            "connected": true,
            "user": user,
            "accounts": accounts,
            "ghCli": { "available": true, "disabled": false, "active": true, "user": user },
        }))
        .into_response();
    }

    if !state.auth.activate_github_auth(&account_id) {
        return error_response(StatusCode::NOT_FOUND, "GitHub account not found");
    }

    let auth = state.auth.get_github_auth();
    let mut accounts: Vec<Value> = state
        .auth
        .get_github_auth_accounts()
        .into_iter()
        .map(|a| a.to_json())
        .collect();
    let Some(auth) = auth else {
        return Json(json!({ "connected": false, "accounts": accounts })).into_response();
    };
    if auth.access_token.is_empty() {
        return Json(json!({ "connected": false, "accounts": accounts })).into_response();
    }

    let gh_cli_disabled = state.auth.is_gh_cli_disabled();
    let gh_token = if !gh_cli_disabled {
        state.gh_cli.token()
    } else {
        None
    };
    let mut gh_cli_user: Option<Value> = None;
    if let Some(gh_token) = gh_token.as_deref()
        && let Ok(user) =
            GithubState::get_github_user_summary(&(state.transport_factory)(gh_token)).await
    {
        gh_cli_user = Some(user.clone());
        accounts.push(json!({
            "id": GH_CLI_ACCOUNT_ID,
            "user": user,
            "current": false,
            "source": "gh-cli",
        }));
    }

    let octokit = state.get_octokit_or_null();
    let Some(octokit) = octokit else {
        let mut gh_cli = json!({
            "available": gh_token.is_some(),
            "disabled": gh_cli_disabled,
            "active": false,
        });
        if let (Some(map), Some(user)) = (gh_cli.as_object_mut(), gh_cli_user.clone()) {
            map.insert("user".to_string(), user);
        }
        return Json(json!({ "connected": false, "accounts": accounts, "ghCli": gh_cli }))
            .into_response();
    };

    let mut user = auth
        .user
        .as_ref()
        .map(|u| u.to_json_null_filled())
        .unwrap_or(Value::Null);
    match GithubState::get_github_user_summary(&octokit).await {
        Ok(summary) => user = summary,
        Err(error) => {
            if is_github_auth_invalid(&error) {
                state.auth.clear_github_auth();
                let accounts: Vec<Value> = state
                    .auth
                    .get_github_auth_accounts()
                    .into_iter()
                    .map(|a| a.to_json())
                    .collect();
                return Json(json!({ "connected": false, "accounts": accounts })).into_response();
            }
        }
    }

    let mut gh_cli = json!({
        "available": gh_token.is_some(),
        "disabled": gh_cli_disabled,
        "active": false,
    });
    if let (Some(map), Some(user)) = (gh_cli.as_object_mut(), gh_cli_user.clone()) {
        map.insert("user".to_string(), user);
    }
    Json(json!({
        "connected": true,
        "user": user,
        "scope": auth.scope,
        "accounts": accounts,
        "ghCli": gh_cli,
    }))
    .into_response()
}

/// DELETE /api/github/auth
/// 中文说明：返回 `success:true` 与是否确实移除了凭据。
async fn auth_delete(State(state): State<Arc<GithubState>>) -> Response {
    let removed = state.auth.clear_github_auth();
    Json(json!({ "success": true, "removed": removed })).into_response()
}

/// GET /api/github/me
/// 中文说明：401/403 视为 token 失效——清除凭据后返回 401。
async fn me(State(state): State<Arc<GithubState>>) -> Response {
    let Some(octokit) = state.get_octokit_or_null() else {
        return error_response(StatusCode::UNAUTHORIZED, "GitHub not connected");
    };
    match GithubState::get_github_user_summary(&octokit).await {
        Ok(user) => Json(user).into_response(),
        Err(error) if is_github_auth_invalid(&error) => {
            state.auth.clear_github_auth();
            error_response(StatusCode::UNAUTHORIZED, "GitHub token expired or revoked")
        }
        Err(error) => internal_error(error.message, "Failed to fetch GitHub user"),
    }
}

/// GET /api/github/pr/status
/// 中文说明：directory+branch 必填；限流/超时时回退任何已缓存数据，
/// 403/404 返回"已连接但无仓库"的降级载荷。
async fn pr_status_route(
    State(state): State<Arc<GithubState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let directory = query
        .get("directory")
        .map(String::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let branch = query
        .get("branch")
        .map(String::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    // JS: `req.query.remote` present-but-empty stays '' (cache key uses the
    // raw value; the resolver normalizes to origin itself).
    let remote = match query.get("remote") {
        Some(value) => value.trim().to_string(),
        None => "origin".to_string(),
    };
    let force = matches!(
        query.get("force").map(String::as_str),
        Some("true") | Some("1")
    );
    if directory.is_empty() || branch.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "directory and branch are required");
    }

    let now = rate_limit::now_ms();
    let cache_key = format!("{directory}::{branch}::{remote}");
    if !force && let Some(cached) = state.cached_pr_status(&cache_key, now) {
        return Json(cached).into_response();
    }

    if state.gate.is_rate_limited(rate_limit::now_ms()) {
        if let Some(cached) = state.any_cached_pr_status(&cache_key) {
            return Json(cached).into_response();
        }
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "GitHub rate limited");
    }

    match pr_status_inner(&state, &directory, &branch, &remote, force, &cache_key).await {
        Ok(response) => response,
        Err(HandlerError::Github(error)) => {
            if error.status == Some(401) {
                state.auth.clear_github_auth();
                return state.send_pr_status(&cache_key, json!({ "connected": false }));
            }
            let was_rate_limited = state
                .gate
                .note_if_rate_limit_error(&error, rate_limit::now_ms());
            if was_rate_limited {
                if let Some(cached) = state.any_cached_pr_status(&cache_key) {
                    return Json(cached).into_response();
                }
                return error_response(StatusCode::SERVICE_UNAVAILABLE, "GitHub rate limited");
            }
            if is_github_resource_unavailable(&error) {
                return state.send_pr_status(
                    &cache_key,
                    json!({
                        "connected": true,
                        "repo": null,
                        "branch": branch,
                        "pr": null,
                        "checks": null,
                        "canMerge": false,
                        "defaultBranch": null,
                        "resolvedRemoteName": null,
                    }),
                );
            }
            internal_error(error.message, "Failed to load GitHub PR status")
        }
        Err(HandlerError::TimedOut) => {
            if let Some(cached) = state.any_cached_pr_status(&cache_key) {
                return Json(cached).into_response();
            }
            error_response(StatusCode::SERVICE_UNAVAILABLE, "GitHub request timed out")
        }
    }
}

/// pr/status 主体：解析仓库与 PR、汇总 checks（check-runs 优先、
/// legacy statuses 兜底）、判定协作权限得出 canMerge，组装完整载荷。
async fn pr_status_inner(
    state: &GithubState,
    directory: &str,
    branch: &str,
    remote: &str,
    force: bool,
    cache_key: &str,
) -> Result<Response, HandlerError> {
    let Some(octokit) = state.get_octokit_or_null() else {
        return Ok(state.send_pr_status(cache_key, json!({ "connected": false })));
    };

    let resolved_status = match tokio::time::timeout(
        std::time::Duration::from_millis(PR_STATUS_RESOLVE_TIMEOUT_MS),
        state
            .engine
            .resolve_github_pr_status(&octokit, directory, branch, remote, force),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => return Err(HandlerError::TimedOut),
    };

    let Some(search_repo) = resolved_status.repo.clone() else {
        return Ok(state.send_pr_status(
            cache_key,
            json!({
                "connected": true,
                "repo": null,
                "branch": branch,
                "pr": null,
                "checks": null,
                "canMerge": false,
                "defaultBranch": null,
                "resolvedRemoteName": null,
            }),
        ));
    };
    let Some(first) = resolved_status.pr.clone() else {
        return Ok(state.send_pr_status(
            cache_key,
            json!({
                "connected": true,
                "repo": search_repo.to_json(),
                "branch": branch,
                "pr": null,
                "checks": null,
                "canMerge": false,
                "defaultBranch": resolved_status.default_branch.clone(),
                "resolvedRemoteName": resolved_status.resolved_remote_name.clone(),
            }),
        ));
    };

    let number = first
        .get("number")
        .and_then(Value::as_i64)
        .unwrap_or_default()
        .to_string();
    let pr_data = octokit
        .pulls_get(&search_repo.owner, &search_repo.repo, &number)
        .await?;
    if pr_data.is_null() {
        return Ok(state.send_pr_status(
            cache_key,
            json!({
                "connected": true,
                "repo": search_repo.to_json(),
                "branch": branch,
                "pr": null,
                "checks": null,
                "canMerge": false,
            }),
        ));
    }

    let is_merged = pr_data
        .get("merged")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || pr_data
            .get("merged_at")
            .map(|v| !v.is_null())
            .unwrap_or(false);
    let pr_state = if is_merged {
        "merged"
    } else if pr_data.get("state").and_then(Value::as_str) == Some("closed") {
        "closed"
    } else {
        "open"
    };
    let is_historical = pr_state != "open";

    // Checks summary: prefer check-runs (Actions), fallback to statuses.
    let mut checks: Option<Value> = None;
    let sha = pr_data
        .pointer("/head/sha")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(sha) = sha.as_deref().filter(|_| !is_historical) {
        if let Ok(runs) = octokit
            .checks_list_for_ref(&search_repo.owner, &search_repo.repo, sha)
            .await
        {
            let check_runs = dedupe_check_runs(
                runs.get("check_runs")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                    .as_slice(),
            );
            if !check_runs.is_empty() {
                checks = Some(summarize_check_runs(&check_runs));
            }
        }
        if checks.is_none()
            && let Ok(combined) = octokit
                .repos_combined_status(&search_repo.owner, &search_repo.repo, sha)
                .await
        {
            checks = Some(summarize_combined_statuses(
                combined
                    .get("statuses")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                    .as_slice(),
            ));
        }
    }

    // Permission check (best-effort).
    let mut can_merge = false;
    if !is_historical {
        let mut username = state
            .auth
            .get_github_auth()
            .and_then(|auth| auth.user.and_then(|u| u.login));
        if username.is_none() {
            username = state.resolve_auth_login(&octokit).await;
        }
        if let Some(username) = username
            && let Ok(perm) = octokit
                .repos_collaborator_permission(&search_repo.owner, &search_repo.repo, &username)
                .await
        {
            let level = perm.get("permission").and_then(Value::as_str).unwrap_or("");
            can_merge = matches!(level, "admin" | "maintain" | "write");
        }
    }

    Ok(state.send_pr_status(
        cache_key,
        json!({
            "connected": true,
            "repo": search_repo.to_json(),
            "branch": branch,
            "pr": {
                "number": pr_data.get("number").cloned().unwrap_or(Value::Null),
                "title": pr_data.get("title").cloned().unwrap_or(Value::Null),
                "body": pr_data.get("body").and_then(Value::as_str).unwrap_or(""),
                "url": pr_data.get("html_url").cloned().unwrap_or(Value::Null),
                "state": pr_state,
                "draft": pr_data.get("draft").and_then(Value::as_bool).unwrap_or(false),
                "base": pr_data.pointer("/base/ref").cloned().unwrap_or(Value::Null),
                "head": pr_data.pointer("/head/ref").cloned().unwrap_or(Value::Null),
                "headSha": pr_data.pointer("/head/sha").cloned().unwrap_or(Value::Null),
                "mergeable": pr_data.get("mergeable").cloned().unwrap_or(Value::Null),
                "mergeableState": pr_data.get("mergeable_state").cloned().unwrap_or(Value::Null),
            },
            "checks": checks,
            "canMerge": can_merge,
            "defaultBranch": resolved_status.default_branch.clone(),
            "resolvedRemoteName": resolved_status.resolved_remote_name.clone(),
        }),
    ))
}

/// POST /api/github/pr/create
/// 中文说明：参数校验 400、未连接 401、head 校验失败 400、其余 500。
async fn pr_create(State(state): State<Arc<GithubState>>, Json(body): Json<Value>) -> Response {
    match pr_create_inner(&state, &body).await {
        Ok(response) => response,
        Err(PrCreateError::Validation(message)) => error_response(StatusCode::BAD_REQUEST, message),
        Err(PrCreateError::Unauthorized) => {
            error_response(StatusCode::UNAUTHORIZED, "GitHub not connected")
        }
        Err(PrCreateError::HeadAccess(message)) => error_response(StatusCode::BAD_REQUEST, message),
        Err(PrCreateError::Internal(message)) => {
            internal_error(message, "Failed to create GitHub PR")
        }
    }
}

/// pr/create 的内部错误分类，决定路由层映射到的 HTTP 状态码。
enum PrCreateError {
    /// 参数校验失败（缺必填字段、仓库无法解析等）→ 400。
    Validation(String),
    /// 未连接 GitHub（无可用 token）→ 401。
    Unauthorized,
    /// head 分支校验失败（常见于 fork PR 无源仓库写权限）→ 400。
    HeadAccess(String),
    /// 其它内部错误 → 500。
    Internal(String),
}

/// pr/create's `normalizeBranchRef`.
/// 归一化分支引用：依次剥离 `refs/heads/`、`heads/`、`remotes/` 前缀，
/// 再剥离已知的 remote 名前缀（剥离后为空则保留原值）。
fn normalize_branch_ref(value: &str, remote_names: &std::collections::HashSet<String>) -> String {
    let mut normalized = value.trim().to_string();
    if let Some(stripped) = normalized.strip_prefix("refs/heads/") {
        normalized = stripped.to_string();
    }
    if let Some(stripped) = normalized.strip_prefix("heads/") {
        normalized = stripped.to_string();
    }
    if let Some(stripped) = normalized.strip_prefix("remotes/") {
        normalized = stripped.to_string();
    }
    if let Some(index) = normalized.find('/')
        && index > 0
    {
        let maybe_remote = &normalized[..index];
        if remote_names.contains(maybe_remote) {
            let without_prefix = normalized[index + 1..].trim().to_string();
            if !without_prefix.is_empty() {
                normalized = without_prefix;
            }
        }
    }
    normalized
}

/// pr/create 主体：解析目标仓库与 head 来源 remote、处理 fork 的
/// `owner:branch` 形式、调用 pulls/create，并失效相关缓存。
async fn pr_create_inner(state: &GithubState, body: &Value) -> Result<Response, PrCreateError> {
    let text = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let directory = text("directory");
    let title = text("title");
    let head = text("head");
    let requested_base = text("base");
    let body_text = body.get("body").and_then(Value::as_str).map(str::to_string);
    let draft = body.get("draft").and_then(Value::as_bool);
    // JS: present-but-empty string stays '' (git lookup then fails like JS).
    let remote = match body.get("remote").and_then(Value::as_str) {
        Some(value) => value.trim().to_string(),
        None => "origin".to_string(),
    };
    let head_remote = text("headRemote");
    let target_repo = body
        .get("targetRepo")
        .filter(|v| v.is_object())
        .and_then(|v| {
            let owner = v.get("owner").and_then(Value::as_str)?.trim();
            let repo = v.get("repo").and_then(Value::as_str)?.trim();
            if owner.is_empty() || repo.is_empty() {
                return None;
            }
            Some(RepoRef {
                owner: owner.to_string(),
                repo: repo.to_string(),
                url: format!("https://github.com/{owner}/{repo}"),
            })
        });

    if directory.is_empty() || title.is_empty() || head.is_empty() || requested_base.is_empty() {
        return Err(PrCreateError::Validation(
            "directory, title, head, base are required".to_string(),
        ));
    }

    let octokit = state
        .get_octokit_or_null()
        .ok_or(PrCreateError::Unauthorized)?;

    let repo = if let Some(target) = target_repo {
        target
    } else {
        let (resolved, _) = repo::resolve_github_repo_from_directory(&directory, &remote).await;
        match resolved {
            Some(repo) => repo,
            None => {
                return Err(PrCreateError::Validation(
                    "Unable to resolve GitHub repo from git remote".to_string(),
                ));
            }
        }
    };

    // Source remote for the head branch: explicit → tracking → origin for
    // non-origin targets.
    let mut source_remote = head_remote.clone();
    if source_remote.is_empty()
        && let Some(tracking) = git_ops::get_tracking_branch(&directory).await
        && let Some(tracking_remote) = tracking.split('/').next()
        && !tracking_remote.is_empty()
    {
        source_remote = tracking_remote.to_string();
    }
    if source_remote.is_empty() && remote != "origin" {
        source_remote = "origin".to_string();
    }

    let mut remote_names: std::collections::HashSet<String> =
        std::collections::HashSet::from([remote.clone()]);
    for name in git_ops::get_remote_names(&directory).await {
        remote_names.insert(name);
    }
    if !source_remote.is_empty() {
        remote_names.insert(source_remote.clone());
    }

    let base = normalize_branch_ref(&requested_base, &remote_names);
    if base.is_empty() {
        return Err(PrCreateError::Validation(
            "Invalid base branch name".to_string(),
        ));
    }

    // Fork workflows: resolve the head repo for cross-repo `owner:branch`.
    let mut head_ref = head.clone();
    let mut head_repo: Option<RepoRef> = None;
    if !source_remote.is_empty() {
        let (resolved, _) =
            repo::resolve_github_repo_from_directory(&directory, &source_remote).await;
        let Some(resolved_repo) = resolved else {
            return Err(PrCreateError::Validation(format!(
                "Cannot resolve GitHub repo for remote \"{source_remote}\". Check that the remote URL is a valid GitHub repository."
            )));
        };
        if resolved_repo.owner != repo.owner || resolved_repo.repo != repo.repo {
            head_ref = format!("{}:{head}", resolved_repo.owner);
        }
        head_repo = Some(resolved_repo);
    }

    if head_ref.contains(':') {
        let head_owner = head_ref.split(':').next().unwrap_or("");
        let head_repo_name = head_repo
            .as_ref()
            .map(|r| r.repo.clone())
            .unwrap_or_else(|| repo.repo.clone());
        if !head_repo_name.is_empty()
            && let Err(error) = octokit
                .repos_get_branch(head_owner, &head_repo_name, &head)
                .await
            && error.status == Some(404)
        {
            return Err(PrCreateError::Validation(format!(
                "Branch \"{head}\" not found on {head_owner}/{head_repo_name}. Please push your branch first: git push {} {head}",
                if source_remote.is_empty() {
                    "origin"
                } else {
                    source_remote.as_str()
                }
            )));
        }
        // Other errors: continue and let the create attempt handle it.
    }

    let mut payload = json!({ "title": title, "head": head_ref, "base": base });
    if let Some(body_text) = body_text {
        payload["body"] = json!(body_text);
    }
    if let Some(draft) = draft {
        payload["draft"] = json!(draft);
    }

    let created = match octokit
        .pulls_create(&repo.owner, &repo.repo, &payload)
        .await
    {
        Ok(created) => created,
        Err(error) => {
            let message = error.message.clone();
            // Head validation error (common with fork PRs).
            let is_head_validation_error = message.contains("Validation Failed")
                && message.contains("\"field\":\"head\"")
                && message.contains("\"code\":\"invalid\"");
            if is_head_validation_error {
                return Err(PrCreateError::HeadAccess(
                    "Unable to create PR: You must have write access to the source repository. Make sure you have pushed your branch to a repository you own (your fork), and that the branch exists on the remote.".to_string(),
                ));
            }
            return Err(PrCreateError::Internal(message));
        }
    };
    if created.is_null() {
        return Err(PrCreateError::Internal("Failed to create PR".to_string()));
    }

    // Invalidate caches.
    let head_branch = match head.split(':').nth(1) {
        Some(branch) if !branch.is_empty() => branch.to_string(),
        _ => head.clone(),
    };
    state
        .pr_status_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&format!("{directory}::{head_branch}::{remote}"));
    state
        .engine
        .invalidate_repo_pulls_cache(&repo.owner, &repo.repo);

    let create_state = if created.get("state").and_then(Value::as_str) == Some("closed") {
        "closed"
    } else {
        "open"
    };
    let created_summary = json!({
        "number": created.get("number").cloned().unwrap_or(Value::Null),
        "title": created.get("title").cloned().unwrap_or(Value::Null),
        "body": created.get("body").and_then(Value::as_str).unwrap_or(""),
        "url": created.get("html_url").cloned().unwrap_or(Value::Null),
        "state": create_state,
        "draft": created.get("draft").and_then(Value::as_bool).unwrap_or(false),
        "base": created.pointer("/base/ref").cloned().unwrap_or(Value::Null),
        "head": created.pointer("/head/ref").cloned().unwrap_or(Value::Null),
        "headSha": created.pointer("/head/sha").cloned().unwrap_or(Value::Null),
        "mergeable": created.get("mergeable").cloned().unwrap_or(Value::Null),
        "mergeableState": created.get("mergeable_state").cloned().unwrap_or(Value::Null),
    });
    Ok(Json(created_summary).into_response())
}

/// The shared PR summary shape (`pr/create`, `pr/update` responses).
/// 中文说明：把 PR 响应裁剪为 create/update 共用的摘要形状，
/// 状态归一为 merged/closed/open。
fn pr_summary_json(pr: &Value) -> Value {
    let state = if pr.get("merged_at").map(|v| !v.is_null()).unwrap_or(false) {
        "merged"
    } else if pr.get("state").and_then(Value::as_str) == Some("closed") {
        "closed"
    } else {
        "open"
    };
    json!({
        "number": pr.get("number").cloned().unwrap_or(Value::Null),
        "title": pr.get("title").cloned().unwrap_or(Value::Null),
        "body": pr.get("body").and_then(Value::as_str).unwrap_or(""),
        "url": pr.get("html_url").cloned().unwrap_or(Value::Null),
        "state": state,
        "draft": pr.get("draft").and_then(Value::as_bool).unwrap_or(false),
        "base": pr.pointer("/base/ref").cloned().unwrap_or(Value::Null),
        "head": pr.pointer("/head/ref").cloned().unwrap_or(Value::Null),
        "headSha": pr.pointer("/head/sha").cloned().unwrap_or(Value::Null),
        "mergeable": pr.get("mergeable").cloned().unwrap_or(Value::Null),
        "mergeableState": pr.get("mergeable_state").cloned().unwrap_or(Value::Null),
    })
}

/// POST /api/github/pr/update
/// 更新 PR 的标题（可选正文）；401/403/404/422 分别映射为
/// 未连接/无权限/不存在/校验错误响应。
async fn pr_update(State(state): State<Arc<GithubState>>, Json(body): Json<Value>) -> Response {
    let directory = body
        .get("directory")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let number = body.get("number").and_then(Value::as_i64);
    let title = body
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let body_text = body.get("body").and_then(Value::as_str).map(str::to_string);
    if directory.is_empty() || number.is_none() || title.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "directory, number, title are required",
        );
    }
    let number = number.unwrap_or_default();

    let Some(octokit) = state.get_octokit_or_null() else {
        return error_response(StatusCode::UNAUTHORIZED, "GitHub not connected");
    };

    let (repo, _) = repo::resolve_github_repo_from_directory(&directory, "origin").await;
    let Some(repo) = repo else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "Unable to resolve GitHub repo from git remote",
        );
    };

    let mut payload = json!({ "title": title });
    if let Some(body_text) = body_text {
        payload["body"] = json!(body_text);
    }

    let updated = match octokit
        .pulls_update(&repo.owner, &repo.repo, &number.to_string(), &payload)
        .await
    {
        Ok(updated) => updated,
        Err(error) => {
            return match error.status {
                Some(401) => error_response(StatusCode::UNAUTHORIZED, "GitHub not connected"),
                Some(403) => {
                    error_response(StatusCode::FORBIDDEN, "Not authorized to edit this PR")
                }
                Some(404) => {
                    error_response(StatusCode::NOT_FOUND, "PR not found in this repository")
                }
                Some(422) => {
                    let api_message = error
                        .data
                        .as_ref()
                        .and_then(|d| d.get("message"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let first_error = error
                        .data
                        .as_ref()
                        .and_then(|d| d.get("errors"))
                        .and_then(Value::as_array)
                        .and_then(|errors| errors.first())
                        .and_then(|first| {
                            first
                                .get("message")
                                .or_else(|| first.get("code"))
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        });
                    let message: Vec<String> =
                        [api_message, first_error].into_iter().flatten().collect();
                    let joined = if message.is_empty() {
                        "Invalid PR update payload".to_string()
                    } else {
                        message.join(" · ")
                    };
                    error_response(StatusCode::UNPROCESSABLE_ENTITY, joined)
                }
                _ => internal_error(error.message, "Failed to update GitHub PR"),
            };
        }
    };
    if updated.is_null() {
        return internal_error(String::new(), "Failed to update PR");
    }

    state.invalidate_pr_context_cache(&directory, Some(number));
    Json(pr_summary_json(&updated)).into_response()
}

/// POST /api/github/pr/merge
/// 按 method（merge/squash/rebase）合并 PR；405/409 冲突时返回
/// 200 + `merged:false` 载荷而非错误。
async fn pr_merge(State(state): State<Arc<GithubState>>, Json(body): Json<Value>) -> Response {
    let directory = body
        .get("directory")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let number = body.get("number").and_then(Value::as_i64);
    let method = body
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("merge")
        .to_string();
    if directory.is_empty() || number.is_none() {
        return error_response(StatusCode::BAD_REQUEST, "directory and number are required");
    }
    let number = number.unwrap_or_default();

    let Some(octokit) = state.get_octokit_or_null() else {
        return error_response(StatusCode::UNAUTHORIZED, "GitHub not connected");
    };
    let (repo, _) = repo::resolve_github_repo_from_directory(&directory, "origin").await;
    let Some(repo) = repo else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "Unable to resolve GitHub repo from git remote",
        );
    };

    match octokit.pulls_merge(&repo.owner, &repo.repo, &number.to_string(), &method).await {
        Ok(result) => {
            state.invalidate_pr_context_cache(&directory, Some(number));
            state.engine.invalidate_repo_pulls_cache(&repo.owner, &repo.repo);
            Json(json!({
                "merged": result.get("merged").and_then(Value::as_bool).unwrap_or(false),
                "message": result.get("message").cloned().unwrap_or(Value::Null),
            }))
            .into_response()
        }
        Err(error) => match error.status {
            Some(403) => error_response(StatusCode::FORBIDDEN, "Not authorized to merge this PR"),
            Some(405) | Some(409) => Json(json!({
                "merged": false,
                "message": if error.message.is_empty() { "PR not mergeable" } else { error.message.as_str() },
            }))
            .into_response(),
            _ => internal_error(error.message, "Failed to merge GitHub PR"),
        },
    }
}

/// POST /api/github/pr/ready
/// 通过 GraphQL `markPullRequestReadyForReview` mutation 把 draft PR
/// 标记为 ready for review；非 draft 直接幂等返回 ready:true。
async fn pr_ready(State(state): State<Arc<GithubState>>, Json(body): Json<Value>) -> Response {
    let directory = body
        .get("directory")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let number = body.get("number").and_then(Value::as_i64);
    if directory.is_empty() || number.is_none() {
        return error_response(StatusCode::BAD_REQUEST, "directory and number are required");
    }
    let number = number.unwrap_or_default();

    let Some(octokit) = state.get_octokit_or_null() else {
        return error_response(StatusCode::UNAUTHORIZED, "GitHub not connected");
    };
    let (repo, _) = repo::resolve_github_repo_from_directory(&directory, "origin").await;
    let Some(repo) = repo else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "Unable to resolve GitHub repo from git remote",
        );
    };

    let pr = match octokit
        .pulls_get(&repo.owner, &repo.repo, &number.to_string())
        .await
    {
        Ok(pr) => pr,
        Err(error) => return internal_error(error.message, "Failed to mark PR ready"),
    };
    let node_id = pr
        .get("node_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(node_id) = node_id.filter(|id| !id.is_empty()) else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to resolve PR node id",
        );
    };

    if pr.get("draft").and_then(Value::as_bool) == Some(false) {
        return Json(json!({ "ready": true })).into_response();
    }

    // markPullRequestReadyForReview 的 GraphQL mutation（按 Node ID 调用）。
    const MARK_READY_MUTATION: &str = "mutation($pullRequestId: ID!) {\n  markPullRequestReadyForReview(input: { pullRequestId: $pullRequestId }) {\n    pullRequest {\n      id\n      isDraft\n    }\n  }\n}";
    if let Err(error) = octokit
        .graphql(MARK_READY_MUTATION, json!({ "pullRequestId": node_id }))
        .await
    {
        if error.status == Some(403) {
            return error_response(StatusCode::FORBIDDEN, "Not authorized to mark PR ready");
        }
        return internal_error(error.message, "Failed to mark PR ready");
    }

    state.invalidate_pr_context_cache(&directory, Some(number));
    state
        .engine
        .invalidate_repo_pulls_cache(&repo.owner, &repo.repo);
    Json(json!({ "ready": true })).into_response()
}

/// GET /api/github/repo/upstream
/// 检测目录 remote 是否为 fork，并返回 upstream 仓库的默认分支、
/// head SHA 与本地指向它的 remote 名。
async fn repo_upstream(
    State(state): State<Arc<GithubState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let directory = query
        .get("directory")
        .map(String::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if directory.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "directory is required");
    }

    let Some(octokit) = state.get_octokit_or_null() else {
        return Json(json!({ "connected": false, "isFork": false, "upstream": null }))
            .into_response();
    };

    let network = match state
        .repo_network
        .resolve_repo_network(&octokit, &directory, "origin")
        .await
    {
        Ok(network) => network,
        Err(error) => return internal_error(error.message, "Failed to detect upstream repo"),
    };

    let upstream: Option<&NetworkRepo> = match &network {
        None => None,
        Some(entries) if entries.len() <= 1 => None,
        Some(entries) => entries.iter().find(|r| r.source == "upstream"),
    };
    if upstream.is_none() {
        return Json(json!({ "connected": true, "isFork": false, "upstream": null }))
            .into_response();
    }
    let upstream = upstream.unwrap();

    let mut default_branch = "main".to_string();
    let mut default_branch_sha: Option<String> = None;
    if let Ok(metadata) = octokit.repos_get(&upstream.owner, &upstream.repo).await {
        if let Some(branch) = metadata.get("default_branch").and_then(Value::as_str)
            && !branch.is_empty()
        {
            default_branch = branch.to_string();
        }
        if let Ok(reference) = octokit
            .git_get_ref(
                &upstream.owner,
                &upstream.repo,
                &format!("heads/{default_branch}"),
            )
            .await
        {
            default_branch_sha = reference
                .pointer("/object/sha")
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|sha| !sha.is_empty());
        }
    }

    // Find a configured remote pointing at the upstream repo.
    let mut upstream_remote_name: Option<String> = None;
    for name in git_ops::get_remote_names(&directory).await {
        let (resolved, _) = repo::resolve_github_repo_from_directory(&directory, &name).await;
        if let Some(resolved) = resolved
            && resolved.owner == upstream.owner
            && resolved.repo == upstream.repo
        {
            upstream_remote_name = Some(name);
            break;
        }
    }

    Json(json!({
        "connected": true,
        "isFork": true,
        "upstream": {
            "owner": upstream.owner,
            "repo": upstream.repo,
            "url": upstream.url,
            "defaultBranch": default_branch,
            "defaultBranchSha": default_branch_sha,
            "remoteName": upstream_remote_name,
        },
    }))
    .into_response()
}

/// GET /api/github/repo/branches
/// 分页拉取仓库全部分支名（每页 100 条，最多 500 页的防御上限），
/// 返回 `{ "branches": [name...] }`。
async fn repo_branches(
    State(state): State<Arc<GithubState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let owner = query
        .get("owner")
        .map(String::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let repo = query
        .get("repo")
        .map(String::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if owner.is_empty() || repo.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "owner and repo are required");
    }

    let Some(octokit) = state.get_octokit_or_null() else {
        return Json(json!({ "branches": [] })).into_response();
    };

    let mut branches: Vec<Value> = Vec::new();
    let mut page = 1u32;
    // Guard: GitHub caps at a few hundred pages; avoid spinning forever on a
    // misbehaving transport (the JS loop would never terminate there either).
    while page <= 500 {
        match octokit.repos_list_branches(&owner, &repo, page).await {
            Ok(data) => {
                let names = data.as_array().cloned().unwrap_or_default();
                if names.is_empty() {
                    break;
                }
                let count = names.len();
                for branch in names {
                    if let Some(name) = branch.get("name").and_then(Value::as_str) {
                        branches.push(json!(name));
                    }
                }
                if count < 100 {
                    break;
                }
                page += 1;
            }
            Err(error) => return internal_error(error.message, "Failed to fetch repo branches"),
        }
    }

    Json(json!({ "branches": branches })).into_response()
}

/// 取查询参数原始值，缺失返回空字符串。
fn query_string(query: &HashMap<String, String>, key: &str) -> String {
    query.get(key).cloned().unwrap_or_default()
}

/// 取查询参数并去除首尾空白，缺失返回空字符串。
fn query_trimmed(query: &HashMap<String, String>, key: &str) -> String {
    query_string(query, key).trim().to_string()
}

/// JS `Number(req.query.number)`: NaN/0 falsy → validation error later.
/// 中文说明：空白、非数值、NaN 及 0 都视为 `None`，与 JS 的 falsy 语义一致。
fn query_number(query: &HashMap<String, String>, key: &str) -> Option<f64> {
    let raw = query_string(query, key);
    if raw.trim().is_empty() {
        return None;
    }
    let parsed = raw.trim().parse::<f64>().ok()?;
    if !parsed.is_finite() || parsed == 0.0 {
        return None;
    }
    Some(parsed)
}

/// Number rendered the way JS would interpolate it (integers without `.0`).
/// 中文说明：整数值去掉 `.0` 后缀，避免路径中出现 `15.0` 这类 JS 不会产生的形式。
fn number_string(number: f64) -> String {
    if number.fract() == 0.0 {
        format!("{}", number as i64)
    } else {
        format!("{number}")
    }
}

/// GET /api/github/issues/list
/// 列出 fork 网络内各仓库的开放 issue（过滤 PR）；带 `query` 时改走
/// 搜索 API 并按仓库限定符限定范围。
async fn issues_list(
    State(state): State<Arc<GithubState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let directory = query_trimmed(&query, "directory");
    let page = query
        .get("page")
        .and_then(|p| p.parse::<f64>().ok())
        .unwrap_or(1.0);
    let search_query = query_trimmed(&query, "query");
    if directory.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "directory is required");
    }

    let Some(octokit) = state.get_octokit_or_null() else {
        return Json(json!({ "connected": false })).into_response();
    };

    let network = match state
        .repo_network
        .resolve_repo_network(&octokit, &directory, "origin")
        .await
    {
        Ok(network) => network,
        Err(error) => return internal_error(error.message, "Failed to list GitHub issues"),
    };
    let (repo, _) = repo::resolve_github_repo_from_directory(&directory, "origin").await;
    let Some(repo) = repo else {
        return Json(json!({ "connected": true, "repo": null, "issues": [] })).into_response();
    };

    let effective_page = if page.is_finite() && page > 0.0 {
        page as u32
    } else {
        1
    };
    let repos_to_query: Vec<(String, String, &'static str)> = network
        .unwrap_or_default()
        .into_iter()
        .map(|entry| (entry.owner, entry.repo, entry.source))
        .collect::<Vec<_>>()
        .if_empty_push((repo.owner.clone(), repo.repo.clone(), "origin"));

    let map_issue_summary = |item: &Value, repo_ref: &(String, String, &'static str)| -> Value {
        json!({
            "number": item.get("number").cloned().unwrap_or(Value::Null),
            "title": item.get("title").cloned().unwrap_or(Value::Null),
            "url": item.get("html_url").cloned().unwrap_or(Value::Null),
            "state": if item.get("state").and_then(Value::as_str) == Some("closed") { "closed" } else { "open" },
            "author": item.get("user").map(|user| json!({
                "login": user.get("login").cloned().unwrap_or(Value::Null),
                "id": user.get("id").cloned().unwrap_or(Value::Null),
                "avatarUrl": user.get("avatar_url").cloned().unwrap_or(Value::Null),
            })).unwrap_or(Value::Null),
            "labels": map_labels(item),
            "sourceRepo": { "owner": repo_ref.0, "repo": repo_ref.1, "source": repo_ref.2 },
        })
    };

    if !search_query.is_empty() {
        let repo_qualifiers = repos_to_query
            .iter()
            .map(|(owner, repo, _)| format!("repo:{owner}/{repo}"))
            .collect::<Vec<_>>()
            .join(" ");
        let q = format!("{repo_qualifiers} {search_query} type:issue state:open");
        match octokit.search_issues(&q, 50, effective_page).await {
            Ok(result) => {
                let total_count = result
                    .get("total_count")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let items = result
                    .get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let issues: Vec<Value> = items
                    .iter()
                    .filter(|item| {
                        item.get("pull_request")
                            .map(|v| v.is_null())
                            .unwrap_or(true)
                    })
                    .map(|item| {
                        let repo_full_name = item
                            .get("repository_url")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .replace("https://api.github.com/repos/", "");
                        let matched = repos_to_query
                            .iter()
                            .find(|(owner, repo, _)| format!("{owner}/{repo}") == repo_full_name);
                        map_issue_summary(item, matched.unwrap_or(&repos_to_query[0]))
                    })
                    .collect();
                let fetched_count = ((effective_page as i64 - 1) * 50) + items.len() as i64;
                let has_more = fetched_count < total_count;
                return Json(json!({
                    "connected": true,
                    "repo": repo.to_json(),
                    "issues": issues,
                    "page": effective_page,
                    "hasMore": has_more,
                }))
                .into_response();
            }
            Err(error) => {
                tracing::error!("Failed to search GitHub issues: {error:?}");
                return Json(json!({
                    "connected": true,
                    "repo": repo.to_json(),
                    "issues": [],
                    "page": effective_page,
                    "hasMore": false,
                }))
                .into_response();
            }
        }
    }

    let results = futures::future::join_all(repos_to_query.iter().map(|repo_ref| async {
        match octokit
            .issues_list_for_repo(&repo_ref.0, &repo_ref.1, "open", 50, effective_page)
            .await
        {
            Ok((list, headers)) => {
                let link = client::header_value(&headers, "link").unwrap_or("");
                let has_more = link.contains("rel=\"next\"");
                let issues: Vec<Value> = list
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .filter(|item| {
                        item.get("pull_request")
                            .map(|v| v.is_null())
                            .unwrap_or(true)
                    })
                    .map(|item| map_issue_summary(item, repo_ref))
                    .collect();
                (issues, has_more)
            }
            Err(error) => {
                tracing::warn!(
                    "Failed to list issues for {}/{}: {}",
                    repo_ref.0,
                    repo_ref.1,
                    error.message
                );
                (Vec::new(), false)
            }
        }
    }))
    .await;

    let all_issues: Vec<Value> = results
        .iter()
        .flat_map(|(issues, _)| issues.iter().cloned())
        .collect();
    let any_has_more = results.iter().any(|(_, has_more)| *has_more);

    Json(json!({
        "connected": true,
        "repo": repo.to_json(),
        "issues": all_issues,
        "page": effective_page,
        "hasMore": any_has_more,
    }))
    .into_response()
}

/// 辅助 trait：仓库列表为空时回退压入一个默认仓库（复刻 JS 的
/// `if (!repos.length)` 逻辑）。
trait IfEmptyPush {
    /// `self` 为空时压入 fallback 元组并返回，否则原样返回。
    fn if_empty_push(self, fallback: (String, String, &'static str)) -> Self;
}

/// 对 `(owner, repo, source)` 三元组向量的 [`IfEmptyPush`] 实现。
impl IfEmptyPush for Vec<(String, String, &'static str)> {
    /// 空列表时压入 fallback（通常是目录 origin 仓库）。
    fn if_empty_push(mut self, fallback: (String, String, &'static str)) -> Self {
        if self.is_empty() {
            self.push(fallback);
        }
        self
    }
}

/// 把 issue/PR 的 labels 映射为 `{name, color?}` 数组；
/// 跳过纯字符串标签与空名标签。
fn map_labels(item: &Value) -> Value {
    let Some(labels) = item.get("labels").and_then(Value::as_array) else {
        return json!([]);
    };
    Value::Array(
        labels
            .iter()
            .filter_map(|label| {
                if label.is_string() {
                    return None;
                }
                let name = label.get("name").and_then(Value::as_str).unwrap_or("");
                if name.is_empty() {
                    return None;
                }
                let mut obj = Map::new();
                obj.insert("name".to_string(), json!(name));
                if let Some(color) = label.get("color").and_then(Value::as_str) {
                    obj.insert("color".to_string(), json!(color));
                }
                Some(Value::Object(obj))
            })
            .collect(),
    )
}

/// GET /api/github/issues/get
/// 获取单个 issue 详情（显式 `owner/repo` 需在网络内校验通过）；
/// 对 PR 返回 400 "Not a GitHub issue"。
async fn issues_get(
    State(state): State<Arc<GithubState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let directory = query_trimmed(&query, "directory");
    let number = query_number(&query, "number");
    if directory.is_empty() || number.is_none() {
        return error_response(StatusCode::BAD_REQUEST, "directory and number are required");
    }
    let number_string = number_string(number.unwrap_or_default());

    let Some(octokit) = state.get_octokit_or_null() else {
        return Json(json!({ "connected": false })).into_response();
    };

    let repo = state
        .resolve_repo_for_request(&octokit, &directory, GithubState::requested_repo(&query))
        .await;
    let Some(repo) = repo else {
        return Json(json!({ "connected": true, "repo": null, "issue": null })).into_response();
    };

    match octokit
        .issues_get(&repo.owner, &repo.repo, &number_string)
        .await
    {
        Ok(issue) => {
            if issue.is_null()
                || issue
                    .get("pull_request")
                    .map(|v| !v.is_null())
                    .unwrap_or(false)
            {
                return error_response(StatusCode::BAD_REQUEST, "Not a GitHub issue");
            }
            Json(json!({
                "connected": true,
                "repo": repo.to_json(),
                "issue": {
                    "number": issue.get("number").cloned().unwrap_or(Value::Null),
                    "title": issue.get("title").cloned().unwrap_or(Value::Null),
                    "url": issue.get("html_url").cloned().unwrap_or(Value::Null),
                    "state": if issue.get("state").and_then(Value::as_str) == Some("closed") { "closed" } else { "open" },
                    "body": issue.get("body").and_then(Value::as_str).unwrap_or(""),
                    "createdAt": issue.get("created_at").cloned().unwrap_or(Value::Null),
                    "updatedAt": issue.get("updated_at").cloned().unwrap_or(Value::Null),
                    "author": issue.get("user").map(|user| json!({
                        "login": user.get("login").cloned().unwrap_or(Value::Null),
                        "id": user.get("id").cloned().unwrap_or(Value::Null),
                        "avatarUrl": user.get("avatar_url").cloned().unwrap_or(Value::Null),
                    })).unwrap_or(Value::Null),
                    "assignees": issue.get("assignees").and_then(Value::as_array).map(|users| {
                        Value::Array(users.iter().filter(|u| !u.is_null()).map(|u| json!({
                            "login": u.get("login").cloned().unwrap_or(Value::Null),
                            "id": u.get("id").cloned().unwrap_or(Value::Null),
                            "avatarUrl": u.get("avatar_url").cloned().unwrap_or(Value::Null),
                        })).collect())
                    }).unwrap_or(json!([])),
                    "labels": map_labels(&issue),
                },
            }))
            .into_response()
        }
        Err(error) => internal_error(error.message, "Failed to fetch GitHub issue"),
    }
}

/// GET /api/github/issues/comments
/// 列出指定 issue 的评论，映射为前端使用的精简形状。
async fn issues_comments(
    State(state): State<Arc<GithubState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let directory = query_trimmed(&query, "directory");
    let number = query_number(&query, "number");
    if directory.is_empty() || number.is_none() {
        return error_response(StatusCode::BAD_REQUEST, "directory and number are required");
    }
    let number_string = number_string(number.unwrap_or_default());

    let Some(octokit) = state.get_octokit_or_null() else {
        return Json(json!({ "connected": false })).into_response();
    };

    let repo = state
        .resolve_repo_for_request(&octokit, &directory, GithubState::requested_repo(&query))
        .await;
    let Some(repo) = repo else {
        return Json(json!({ "connected": true, "repo": null, "comments": [] })).into_response();
    };

    match octokit
        .issues_list_comments(&repo.owner, &repo.repo, &number_string)
        .await
    {
        Ok(result) => {
            let comments = map_issue_comments(&result);
            Json(json!({ "connected": true, "repo": repo.to_json(), "comments": comments }))
                .into_response()
        }
        Err(error) => internal_error(error.message, "Failed to fetch GitHub issue comments"),
    }
}

/// 把评论数组映射为 `{id,url,body,createdAt,updatedAt,author}` 列表，
/// 非 JSON 数组输入返回空数组。
fn map_issue_comments(result: &Value) -> Value {
    Value::Array(
        result
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|comment| {
                json!({
                    "id": comment.get("id").cloned().unwrap_or(Value::Null),
                    "url": comment.get("html_url").cloned().unwrap_or(Value::Null),
                    "body": comment.get("body").and_then(Value::as_str).unwrap_or(""),
                    "createdAt": comment.get("created_at").cloned().unwrap_or(Value::Null),
                    "updatedAt": comment.get("updated_at").cloned().unwrap_or(Value::Null),
                    "author": comment.get("user").map(|user| json!({
                        "login": user.get("login").cloned().unwrap_or(Value::Null),
                        "id": user.get("id").cloned().unwrap_or(Value::Null),
                        "avatarUrl": user.get("avatar_url").cloned().unwrap_or(Value::Null),
                    })).unwrap_or(Value::Null),
                })
            })
            .collect(),
    )
}

/// GET /api/github/pulls/list
/// 列出 fork 网络内各仓库的开放 PR；带 `query` 时走搜索 API，
/// 再按编号逐个拉取完整 PR 详情。
async fn pulls_list(
    State(state): State<Arc<GithubState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let directory = query_trimmed(&query, "directory");
    let page = query
        .get("page")
        .and_then(|p| p.parse::<f64>().ok())
        .unwrap_or(1.0);
    let search_query = query_trimmed(&query, "query");
    if directory.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "directory is required");
    }

    let Some(octokit) = state.get_octokit_or_null() else {
        return Json(json!({ "connected": false })).into_response();
    };

    let network = match state
        .repo_network
        .resolve_repo_network(&octokit, &directory, "origin")
        .await
    {
        Ok(network) => network,
        Err(error) if error.status == Some(401) => {
            state.auth.clear_github_auth();
            return Json(json!({ "connected": false })).into_response();
        }
        Err(error) => return internal_error(error.message, "Failed to list GitHub pull requests"),
    };
    let (repo, _) = repo::resolve_github_repo_from_directory(&directory, "origin").await;
    let Some(repo) = repo else {
        return Json(json!({ "connected": true, "repo": null, "prs": [] })).into_response();
    };
    let effective_page = if page.is_finite() && page > 0.0 {
        page as u32
    } else {
        1
    };
    let repos_to_query: Vec<(String, String, &'static str)> = network
        .unwrap_or_default()
        .into_iter()
        .map(|entry| (entry.owner, entry.repo, entry.source))
        .collect::<Vec<_>>()
        .if_empty_push((repo.owner.clone(), repo.repo.clone(), "origin"));

    let map_pr_summary = |pr: &Value, repo_ref: &(String, String, &'static str)| -> Value {
        let merged_state = if pr.get("merged_at").map(|v| !v.is_null()).unwrap_or(false) {
            "merged"
        } else if pr.get("state").and_then(Value::as_str) == Some("closed") {
            "closed"
        } else {
            "open"
        };
        let head_repo = pr.pointer("/head/repo").map(|head_repo| {
            let obj = json!({
                "owner": head_repo.pointer("/owner/login").cloned().unwrap_or(Value::Null),
                "repo": head_repo.get("name").cloned().unwrap_or(Value::Null),
                "url": head_repo.get("html_url").cloned().unwrap_or(Value::Null),
                "cloneUrl": head_repo.get("clone_url").cloned().unwrap_or(Value::Null),
                "sshUrl": head_repo.get("ssh_url").cloned().unwrap_or(Value::Null),
            });
            // headRepo only counts when owner+repo+url are all present.
            let complete = obj.get("owner").map(|v| !v.is_null()).unwrap_or(false)
                && obj.get("repo").map(|v| !v.is_null()).unwrap_or(false)
                && obj.get("url").map(|v| !v.is_null()).unwrap_or(false);
            if complete { obj } else { Value::Null }
        });
        json!({
            "number": pr.get("number").cloned().unwrap_or(Value::Null),
            "title": pr.get("title").cloned().unwrap_or(Value::Null),
            "url": pr.get("html_url").cloned().unwrap_or(Value::Null),
            "state": merged_state,
            "draft": pr.get("draft").and_then(Value::as_bool).unwrap_or(false),
            "base": pr.pointer("/base/ref").cloned().unwrap_or(Value::Null),
            "head": pr.pointer("/head/ref").cloned().unwrap_or(Value::Null),
            "headSha": pr.pointer("/head/sha").cloned().unwrap_or(Value::Null),
            "mergeable": pr.get("mergeable").cloned().unwrap_or(Value::Null),
            "mergeableState": pr.get("mergeable_state").cloned().unwrap_or(Value::Null),
            "author": pr.get("user").map(|user| json!({
                "login": user.get("login").cloned().unwrap_or(Value::Null),
                "id": user.get("id").cloned().unwrap_or(Value::Null),
                "avatarUrl": user.get("avatar_url").cloned().unwrap_or(Value::Null),
            })).unwrap_or(Value::Null),
            "headLabel": pr.pointer("/head/label").cloned().unwrap_or(Value::Null),
            "headRepo": head_repo.unwrap_or(Value::Null),
            "sourceRepo": { "owner": repo_ref.0, "repo": repo_ref.1, "source": repo_ref.2 },
        })
    };

    if !search_query.is_empty() {
        let repo_qualifiers = repos_to_query
            .iter()
            .map(|(owner, repo, _)| format!("repo:{owner}/{repo}"))
            .collect::<Vec<_>>()
            .join(" ");
        let q = format!("{repo_qualifiers} {search_query} type:pr state:open");
        match octokit.search_issues(&q, 50, effective_page).await {
            Ok(result) => {
                let total_count = result
                    .get("total_count")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let items = result
                    .get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let find_repo = |item: &Value| -> (String, String, &'static str) {
                    let repository_url = item
                        .get("repository_url")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    // /repos/{owner}/{repo}$ regex
                    if let Some(position) = repository_url.rfind("/repos/") {
                        let rest = &repository_url[position + "/repos/".len()..];
                        let mut parts = rest.split('/');
                        if let (Some(owner), Some(repo)) = (parts.next(), parts.next())
                            && let Some(matched) = repos_to_query
                                .iter()
                                .find(|(o, r, _)| o == owner && r == repo)
                        {
                            return matched.clone();
                        }
                    }
                    repos_to_query[0].clone()
                };
                let pr_refs: Vec<(f64, (String, String, &'static str))> = items
                    .iter()
                    .map(|item| {
                        (
                            item.get("number")
                                .and_then(Value::as_f64)
                                .unwrap_or_default(),
                            find_repo(item),
                        )
                    })
                    .filter(|(number, _)| number.is_finite() && *number > 0.0)
                    .collect();
                let fetched =
                    futures::future::join_all(pr_refs.iter().map(|(number, repo_ref)| async {
                        match octokit
                            .pulls_get(&repo_ref.0, &repo_ref.1, &number_string(*number))
                            .await
                        {
                            Ok(pr) => Some(map_pr_summary(&pr, repo_ref)),
                            Err(_) => None,
                        }
                    }))
                    .await;
                let prs: Vec<Value> = fetched.into_iter().flatten().collect();
                let fetched_count = ((effective_page as i64 - 1) * 50) + items.len() as i64;
                let has_more = fetched_count < total_count;
                return Json(json!({
                    "connected": true,
                    "repo": repo.to_json(),
                    "prs": prs,
                    "page": effective_page,
                    "hasMore": has_more,
                }))
                .into_response();
            }
            Err(error) => {
                tracing::error!("Failed to search GitHub PRs: {error:?}");
                return Json(json!({
                    "connected": true,
                    "repo": repo.to_json(),
                    "prs": [],
                    "page": effective_page,
                    "hasMore": false,
                }))
                .into_response();
            }
        }
    }

    let results = futures::future::join_all(repos_to_query.iter().map(|repo_ref| async {
        match octokit
            .pulls_list(
                &repo_ref.0,
                &repo_ref.1,
                "open",
                None,
                50,
                Some(effective_page),
            )
            .await
        {
            Ok((list, headers)) => {
                let link = client::header_value(&headers, "link").unwrap_or("");
                let has_more = link.contains("rel=\"next\"");
                let prs: Vec<Value> = list
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|pr| map_pr_summary(pr, repo_ref))
                    .collect();
                (prs, has_more)
            }
            Err(error) => {
                tracing::warn!(
                    "Failed to list PRs for {}/{}: {}",
                    repo_ref.0,
                    repo_ref.1,
                    error.message
                );
                (Vec::new(), false)
            }
        }
    }))
    .await;

    let all_prs: Vec<Value> = results
        .iter()
        .flat_map(|(prs, _)| prs.iter().cloned())
        .collect();
    let any_has_more = results.iter().any(|(_, has_more)| *has_more);

    Json(json!({
        "connected": true,
        "repo": repo.to_json(),
        "prs": all_prs,
        "page": effective_page,
        "hasMore": any_has_more,
    }))
    .into_response()
}

/// GET /api/github/pulls/context
/// 获取 PR 的完整上下文（详情、两类评论、文件、checks 摘要，
/// 可选 diff 与 check 明细）；带 30 秒路由级缓存。
async fn pulls_context(
    State(state): State<Arc<GithubState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let directory = query_trimmed(&query, "directory");
    let number = query_number(&query, "number");
    let include_diff = matches!(
        query.get("diff").map(String::as_str),
        Some("1") | Some("true")
    );
    let include_check_details = matches!(
        query.get("checkDetails").map(String::as_str),
        Some("1") | Some("true")
    );
    if directory.is_empty() || number.is_none() {
        return error_response(StatusCode::BAD_REQUEST, "directory and number are required");
    }
    let number_string = number_string(number.unwrap_or_default());

    let Some(octokit) = state.get_octokit_or_null() else {
        return Json(json!({ "connected": false })).into_response();
    };

    let requested_repo = GithubState::requested_repo(&query);
    let context_cache_key = serde_json::to_string(&json!([
        directory,
        number_string,
        include_diff,
        requested_repo
            .as_ref()
            .map(|r| format!("{}/{}", r.owner, r.repo))
            .unwrap_or_else(|| "null".to_string()),
    ]))
    .unwrap_or_default();

    {
        let cache = state
            .pr_context_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some((_, entry)) = cache.iter().find(|(k, _)| *k == context_cache_key)
            && rate_limit::now_ms() - entry.fetched_at < PR_CONTEXT_CACHE_TTL_MS
            && (entry.include_check_details || !include_check_details)
        {
            return Json(entry.data.clone()).into_response();
        }
    }

    match pulls_context_inner(
        &state,
        &octokit,
        &directory,
        &number_string,
        requested_repo,
        include_diff,
        include_check_details,
        &context_cache_key,
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            if error.status == Some(401) {
                state.auth.clear_github_auth();
                return Json(json!({ "connected": false })).into_response();
            }
            internal_error(error.message, "Failed to load GitHub PR context")
        }
    }
}

/// 把 PR 响应裁剪为 pulls/context 使用的详细形状（含 headRepo 完整性
/// 校验、body 与创建/更新时间）。
fn pr_context_pr_json(pr_data: &Value) -> Value {
    let merged_state = if pr_data
        .get("merged")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        "merged"
    } else if pr_data.get("state").and_then(Value::as_str) == Some("closed") {
        "closed"
    } else {
        "open"
    };
    json!({
        "number": pr_data.get("number").cloned().unwrap_or(Value::Null),
        "title": pr_data.get("title").cloned().unwrap_or(Value::Null),
        "url": pr_data.get("html_url").cloned().unwrap_or(Value::Null),
        "state": merged_state,
        "draft": pr_data.get("draft").and_then(Value::as_bool).unwrap_or(false),
        "base": pr_data.pointer("/base/ref").cloned().unwrap_or(Value::Null),
        "head": pr_data.pointer("/head/ref").cloned().unwrap_or(Value::Null),
        "headSha": pr_data.pointer("/head/sha").cloned().unwrap_or(Value::Null),
        "mergeable": pr_data.get("mergeable").cloned().unwrap_or(Value::Null),
        "mergeableState": pr_data.get("mergeable_state").cloned().unwrap_or(Value::Null),
        "author": pr_data.get("user").map(|user| json!({
            "login": user.get("login").cloned().unwrap_or(Value::Null),
            "id": user.get("id").cloned().unwrap_or(Value::Null),
            "avatarUrl": user.get("avatar_url").cloned().unwrap_or(Value::Null),
        })).unwrap_or(Value::Null),
        "headLabel": pr_data.pointer("/head/label").cloned().unwrap_or(Value::Null),
        "headRepo": pr_data.pointer("/head/repo").map(|head_repo| {
            let owner = head_repo.pointer("/owner/login").cloned().unwrap_or(Value::Null);
            let repo = head_repo.get("name").cloned().unwrap_or(Value::Null);
            let url = head_repo.get("html_url").cloned().unwrap_or(Value::Null);
            if owner.is_null() || repo.is_null() || url.is_null() {
                Value::Null
            } else {
                json!({
                    "owner": owner,
                    "repo": repo,
                    "url": url,
                    "cloneUrl": head_repo.get("clone_url").cloned().unwrap_or(Value::Null),
                    "sshUrl": head_repo.get("ssh_url").cloned().unwrap_or(Value::Null),
                })
            }
        }).unwrap_or(Value::Null),
        "body": pr_data.get("body").and_then(Value::as_str).unwrap_or(""),
        "createdAt": pr_data.get("created_at").cloned().unwrap_or(Value::Null),
        "updatedAt": pr_data.get("updated_at").cloned().unwrap_or(Value::Null),
    })
}

/// pulls/context 主体：解析目标仓库后拉取 PR、issue/review 评论与文件，
/// 汇总 checks（可选 Actions job 步骤与失败 run 的 annotations），
/// 可选附上 diff，最后经缓存覆写返回。
#[allow(clippy::too_many_arguments)]
async fn pulls_context_inner(
    state: &GithubState,
    octokit: &GithubClient,
    directory: &str,
    number_string: &str,
    requested_repo: Option<RepoRef>,
    include_diff: bool,
    include_check_details: bool,
    context_cache_key: &str,
) -> Result<Response, GithubError> {
    let repo = state
        .resolve_repo_for_request(octokit, directory, requested_repo)
        .await;
    let Some(repo) = repo else {
        return Ok(state.send_pr_context(
            context_cache_key,
            include_check_details,
            json!({ "connected": true, "repo": null, "pr": null }),
        ));
    };

    let pr_data = octokit
        .pulls_get(&repo.owner, &repo.repo, number_string)
        .await?;
    if pr_data.is_null() {
        return Ok(error_response(StatusCode::NOT_FOUND, "PR not found"));
    }

    let pr = pr_context_pr_json(&pr_data);

    let issue_comments_resp = octokit
        .issues_list_comments(&repo.owner, &repo.repo, number_string)
        .await?;
    let issue_comments = map_issue_comments(&issue_comments_resp);

    let review_comments_resp = octokit
        .pulls_list_review_comments(&repo.owner, &repo.repo, number_string)
        .await?;
    let review_comments = Value::Array(
        review_comments_resp
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|comment| {
                json!({
                    "id": comment.get("id").cloned().unwrap_or(Value::Null),
                    "url": comment.get("html_url").cloned().unwrap_or(Value::Null),
                    "body": comment.get("body").and_then(Value::as_str).unwrap_or(""),
                    "createdAt": comment.get("created_at").cloned().unwrap_or(Value::Null),
                    "updatedAt": comment.get("updated_at").cloned().unwrap_or(Value::Null),
                    "path": comment.get("path").cloned().unwrap_or(Value::Null),
                    "line": comment.get("line").cloned().unwrap_or(Value::Null),
                    "position": comment.get("position").cloned().unwrap_or(Value::Null),
                    "author": comment.get("user").map(|user| json!({
                        "login": user.get("login").cloned().unwrap_or(Value::Null),
                        "id": user.get("id").cloned().unwrap_or(Value::Null),
                        "avatarUrl": user.get("avatar_url").cloned().unwrap_or(Value::Null),
                    })).unwrap_or(Value::Null),
                })
            })
            .collect(),
    );

    let files_resp = octokit
        .pulls_list_files(&repo.owner, &repo.repo, number_string)
        .await?;
    let files = Value::Array(
        files_resp
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|file| {
                json!({
                    "filename": file.get("filename").cloned().unwrap_or(Value::Null),
                    "status": file.get("status").cloned().unwrap_or(Value::Null),
                    "additions": file.get("additions").cloned().unwrap_or(Value::Null),
                    "deletions": file.get("deletions").cloned().unwrap_or(Value::Null),
                    "changes": file.get("changes").cloned().unwrap_or(Value::Null),
                    "patch": file.get("patch").cloned().unwrap_or(Value::Null),
                })
            })
            .collect(),
    );

    // checks summary (same logic as the status endpoint), optionally with
    // per-run jobs and annotations.
    let mut checks: Option<Value> = None;
    let mut check_runs_out: Option<Vec<Value>> = None;
    let sha = pr_data
        .pointer("/head/sha")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(sha) = sha.as_deref() {
        if let Ok(runs) = octokit
            .checks_list_for_ref(&repo.owner, &repo.repo, sha)
            .await
        {
            let check_runs = dedupe_check_runs(
                runs.get("check_runs")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                    .as_slice(),
            );
            if !check_runs.is_empty() {
                let mut parsed_jobs: HashMap<i64, Vec<Value>> = HashMap::new();
                let mut parsed_annotations: HashMap<i64, Vec<Value>> = HashMap::new();

                if include_check_details {
                    // Prefetch actions jobs per runId.
                    let mut run_ids: Vec<i64> = Vec::new();
                    let mut job_ids: HashMap<String, (i64, Option<i64>)> = HashMap::new();
                    for run in &check_runs {
                        let details = run.get("details_url").and_then(Value::as_str).unwrap_or("");
                        if let Some((run_id, job_id)) = parse_actions_details_url(details) {
                            if !run_ids.contains(&run_id) {
                                run_ids.push(run_id);
                            }
                            job_ids.insert(details.to_string(), (run_id, job_id));
                        }
                    }
                    for run_id in run_ids {
                        match octokit
                            .actions_list_jobs(&repo.owner, &repo.repo, run_id)
                            .await
                        {
                            Ok(jobs_resp) => {
                                parsed_jobs.insert(
                                    run_id,
                                    jobs_resp
                                        .get("jobs")
                                        .and_then(Value::as_array)
                                        .cloned()
                                        .unwrap_or_default(),
                                );
                            }
                            Err(_) => {
                                parsed_jobs.insert(run_id, Vec::new());
                            }
                        }
                    }

                    for run in &check_runs {
                        let run_id = run.get("id").and_then(Value::as_i64).unwrap_or_default();
                        let conclusion = run
                            .get("conclusion")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_lowercase();
                        let should_load = run_id > 0
                            && !conclusion.is_empty()
                            && !matches!(conclusion.as_str(), "success" | "neutral" | "skipped");
                        if !should_load {
                            continue;
                        }
                        let mut annotations: Vec<Value> = Vec::new();
                        for page in 1..=3u32 {
                            match octokit
                                .checks_list_annotations(&repo.owner, &repo.repo, run_id, page)
                                .await
                            {
                                Ok(chunk_resp) => {
                                    let chunk = chunk_resp.as_array().cloned().unwrap_or_default();
                                    let len = chunk.len();
                                    annotations.extend(chunk);
                                    if len < 50 {
                                        break;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        if !annotations.is_empty() {
                            parsed_annotations.insert(run_id, annotations);
                        }
                    }
                }

                let check_runs_json: Vec<Value> = check_runs
                    .iter()
                    .map(|run| {
                        let details_url = run
                            .get("details_url")
                            .and_then(Value::as_str)
                            .filter(|v| !v.is_empty())
                            .map(|v| json!(v));
                        let mut entry = json!({
                            "id": run.get("id").cloned().unwrap_or(Value::Null),
                            "name": run.get("name").cloned().unwrap_or(Value::Null),
                            "status": run.get("status").cloned().unwrap_or(Value::Null),
                            "conclusion": run.get("conclusion").cloned().unwrap_or(Value::Null),
                            "detailsUrl": details_url.clone().unwrap_or(Value::Null),
                        });
                        let obj = entry.as_object_mut().unwrap();
                        set_opt(obj, "startedAt", run.get("started_at").filter(|v| !v.is_null() && v.as_str() != Some("")).cloned().map(|v| json!(v)));
                        set_opt(obj, "completedAt", run.get("completed_at").filter(|v| !v.is_null() && v.as_str() != Some("")).cloned().map(|v| json!(v)));
                        if let Some(app) = run.get("app").filter(|v| !v.is_null()) {
                            let mut app_obj = Map::new();
                            set_opt(&mut app_obj, "name", app.get("name").filter(|v| !v.is_null() && v.as_str() != Some("")).cloned().map(|v| json!(v)));
                            set_opt(&mut app_obj, "slug", app.get("slug").filter(|v| !v.is_null() && v.as_str() != Some("")).cloned().map(|v| json!(v)));
                            obj.insert("app".to_string(), Value::Object(app_obj));
                        }
                        if let Some(output) = run.get("output").filter(|v| !v.is_null()) {
                            let mut output_obj = Map::new();
                            set_opt(&mut output_obj, "title", output.get("title").filter(|v| v.as_str().is_some_and(|s| !s.is_empty())).cloned().map(|v| json!(v)));
                            set_opt(&mut output_obj, "summary", output.get("summary").filter(|v| v.as_str().is_some_and(|s| !s.is_empty())).cloned().map(|v| json!(v)));
                            set_opt(&mut output_obj, "text", output.get("text").filter(|v| v.as_str().is_some_and(|s| !s.is_empty())).cloned().map(|v| json!(v)));
                            obj.insert("output".to_string(), Value::Object(output_obj));
                        }
                        if include_check_details
                            && let Some(details) =
                                run.get("details_url").and_then(Value::as_str).map(str::to_string)
                                && let Some((run_id, job_id)) = parse_actions_details_url(&details) {
                                    let jobs = parsed_jobs.get(&run_id).cloned().unwrap_or_default();
                                    let matched = job_id
                                        .and_then(|id| jobs.iter().find(|j| j.get("id").and_then(Value::as_i64) == Some(id)))
                                        .or_else(|| {
                                            jobs.iter().find(|j| {
                                                j.get("name").and_then(Value::as_str)
                                                    == run.get("name").and_then(Value::as_str)
                                            })
                                        });
                                    if let Some(picked) = matched {
                                        obj.insert("job".to_string(), json!({
                                            "runId": run_id,
                                            "jobId": picked.get("id").cloned().unwrap_or(Value::Null),
                                            "url": picked.get("html_url").cloned().unwrap_or(Value::Null),
                                            "name": picked.get("name").cloned().unwrap_or(Value::Null),
                                            "workflowName": picked.get("workflow_name").cloned().unwrap_or(Value::Null),
                                            "conclusion": picked.get("conclusion").cloned().unwrap_or(Value::Null),
                                            "steps": picked.get("steps").and_then(Value::as_array).map(|steps| {
                                                Value::Array(steps.iter().map(|step| {
                                                    json!({
                                                        "name": step.get("name").cloned().unwrap_or(Value::Null),
                                                        "status": step.get("status").cloned().unwrap_or(Value::Null),
                                                        "conclusion": step.get("conclusion").cloned().unwrap_or(Value::Null),
                                                        "number": step.get("number").cloned().unwrap_or(Value::Null),
                                                        "startedAt": step.get("started_at").cloned().unwrap_or(Value::Null),
                                                        "completedAt": step.get("completed_at").cloned().unwrap_or(Value::Null),
                                                    })
                                                }).collect())
                                            }).unwrap_or(Value::Null),
                                        }));
                                    } else {
                                        let mut job = Map::new();
                                        job.insert("runId".to_string(), json!(run_id));
                                        if let Some(job_id) = job_id {
                                            job.insert("jobId".to_string(), json!(job_id));
                                        }
                                        job.insert("url".to_string(), json!(details));
                                        obj.insert("job".to_string(), Value::Object(job));
                                    }
                                }
                        if let Some(run_id) = run.get("id").and_then(Value::as_i64)
                            && let Some(annotations) = parsed_annotations.get(&run_id) {
                                let mapped: Vec<Value> = annotations
                                    .iter()
                                    .map(|annotation| {
                                        let mut obj = Map::new();
                                        set_opt(&mut obj, "path", annotation.get("path").filter(|v| v.as_str().is_some_and(|s| !s.is_empty())).cloned().map(|v| json!(v)));
                                        if let Some(start_line) = annotation.get("start_line").and_then(Value::as_i64) {
                                            obj.insert("startLine".to_string(), json!(start_line));
                                        }
                                        if let Some(end_line) = annotation.get("end_line").and_then(Value::as_i64) {
                                            obj.insert("endLine".to_string(), json!(end_line));
                                        }
                                        set_opt(&mut obj, "level", annotation.get("annotation_level").filter(|v| v.as_str().is_some_and(|s| !s.is_empty())).cloned().map(|v| json!(v)));
                                        obj.insert("message".to_string(), json!(annotation.get("message").and_then(Value::as_str).unwrap_or("")));
                                        set_opt(&mut obj, "title", annotation.get("title").filter(|v| v.as_str().is_some_and(|s| !s.is_empty())).cloned().map(|v| json!(v)));
                                        set_opt(&mut obj, "rawDetails", annotation.get("raw_details").filter(|v| v.as_str().is_some_and(|s| !s.is_empty())).cloned().map(|v| json!(v)));
                                        Value::Object(obj)
                                    })
                                    .filter(|annotation| {
                                        annotation.get("message").and_then(Value::as_str).is_some_and(|m| !m.is_empty())
                                    })
                                    .collect();
                                obj.insert("annotations".to_string(), Value::Array(mapped));
                            }
                        entry
                    })
                    .collect();
                check_runs_out = Some(check_runs_json);
                checks = Some(summarize_check_runs(&check_runs));
            }
        }
        if checks.is_none()
            && let Ok(combined) = octokit
                .repos_combined_status(&repo.owner, &repo.repo, sha)
                .await
        {
            checks = Some(summarize_combined_statuses(
                combined
                    .get("statuses")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                    .as_slice(),
            ));
        }
    }

    let diff: Option<String> = if include_diff {
        match octokit
            .pulls_get_diff(&repo.owner, &repo.repo, number_string)
            .await
        {
            Ok(diff) => diff.filter(|d| !d.is_empty()),
            Err(error) => return Err(error),
        }
    } else {
        None
    };

    let mut payload = json!({
        "connected": true,
        "repo": repo.to_json(),
        "pr": pr,
        "issueComments": issue_comments,
        "reviewComments": review_comments,
        "files": files,
        "checks": checks,
    });
    if let Some(map) = payload.as_object_mut() {
        if let Some(diff) = diff {
            map.insert("diff".to_string(), json!(diff));
        }
        if let Some(check_runs) = check_runs_out {
            map.insert("checkRuns".to_string(), Value::Array(check_runs));
        }
    }

    Ok(state.send_pr_context(context_cache_key, include_check_details, payload))
}

/// `/actions/runs/(\d+)(?:/job/(\d+))?` from a check-run `details_url`.
/// 中文说明：仅识别 GitHub Actions 的 URL 形态，非数字或非正 id 返回 `None`。
fn parse_actions_details_url(details_url: &str) -> Option<(i64, Option<i64>)> {
    let position = details_url.find("/actions/runs/")?;
    let rest = &details_url[position + "/actions/runs/".len()..];
    let mut parts = rest.split('/');
    let run_id: i64 = parts.next()?.parse().ok()?;
    if run_id <= 0 {
        return None;
    }
    let job_id = match (parts.next(), parts.next()) {
        (Some("job"), Some(job)) => job.parse::<i64>().ok().filter(|id| *id > 0),
        _ => None,
    };
    Some((run_id, job_id))
}
/// 单元测试：聚合函数的变换规则、各路由的响应形状与状态码映射、
/// 缓存复用与限流降级；全部通过注入 fake transport / form poster /
/// gate 完成，不访问真实 GitHub。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::client::{GithubRequest, GithubResponse};
    use axum::body::Body;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
    use tower::ServiceExt;

    // ---- fixtures & helpers -------------------------------------------------

    /// 创建唯一的临时目录（tag + 进程号 + 时间戳 + 计数器），先清理再新建。
    fn temp_dir(tag: &str) -> PathBuf {
        // 并发计数器：保证并发测试下临时目录名不冲突。
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-ghroutes-{tag}-{}-{}-{unique}",
            std::process::id(),
            rate_limit::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 构造 status 200、JSON 内容的 transport 成功结果。
    fn json_ok(body: Value) -> Result<GithubResponse, GithubError> {
        Ok(GithubResponse {
            status: 200,
            headers: vec![(
                "content-type".to_string(),
                "application/json; charset=utf-8".to_string(),
            )],
            body: serde_json::to_vec(&body).unwrap(),
        })
    }

    /// 构造指定状态码的 [`GithubError`]，message 取响应体的 `message` 字段。
    fn json_error(status: u16, body: Value) -> Result<GithubResponse, GithubError> {
        let response = GithubResponse {
            status,
            headers: vec![(
                "content-type".to_string(),
                "application/json; charset=utf-8".to_string(),
            )],
            body: serde_json::to_vec(&body).unwrap(),
        };
        Err(GithubError {
            status: Some(status),
            message: body
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_string(),
            headers: response.headers.clone(),
            data: Some(body),
        })
    }

    /// A fake transport answering from a route table keyed by
    /// `(method, path)` with optional query matching.
    /// fake transport 的应答函数类型：按请求返回成功响应或错误。
    type Responder =
        Box<dyn Fn(&GithubRequest) -> Result<GithubResponse, GithubError> + Send + Sync>;

    /// 用应答函数构造同步应答的 [`GithubClient`]。
    fn client_with(responder: Responder) -> GithubClient {
        GithubClient::with_transport(Arc::new(move |req: GithubRequest| {
            let result = responder(&req);
            Box::pin(async move { result })
        }))
    }

    /// 同 [`client_with`]，但把每次 transport 调用计入 `calls` 计数器。
    fn counting_client(calls: Arc<AtomicI64>, responder: Responder) -> GithubClient {
        GithubClient::with_transport(Arc::new(move |req: GithubRequest| {
            calls.fetch_add(1, Ordering::SeqCst);
            let result = responder(&req);
            Box::pin(async move { result })
        }))
    }

    /// 组装带 fake transport 的测试状态（真实临时目录 + 独立限流门）。
    fn make_state(tag: &str, responder: Responder) -> (Arc<GithubState>, PathBuf) {
        let dir = temp_dir(tag);
        let gate = Arc::new(RateLimitGate::new());
        let client = client_with(responder);
        let state = GithubState::with_seams(
            dir.clone(),
            Arc::new(move |_token: &str| client.clone()),
            device_flow::default_form_poster(),
            Arc::new(|| None),
            gate,
            Arc::new(PrStatusEngine::new()),
        );
        (state, dir)
    }

    /// 读取响应体并解析为 JSON（解析失败时得 `null`）。
    async fn body_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    /// 初始化带指定 origin remote 的临时 git 仓库目录。
    fn git_dir(tag: &str, remote_url: &str) -> PathBuf {
        let dir = temp_dir(tag);
        let output = std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&dir)
            .output()
            .expect("git init");
        assert!(output.status.success(), "git init failed");
        let output = std::process::Command::new("git")
            .args(["remote", "add", "origin", remote_url])
            .current_dir(&dir)
            .output()
            .expect("git remote add");
        assert!(output.status.success(), "git remote add failed");
        dir
    }

    // ---- aggregation transforms on fixture payloads -------------------------

    /// 构造单个 check-run 的 fixture JSON。
    fn check_run(
        id: i64,
        name: &str,
        status: &str,
        conclusion: Option<&str>,
        started_at: &str,
    ) -> Value {
        json!({
            "id": id,
            "name": name,
            "status": status,
            "conclusion": conclusion,
            "started_at": started_at,
            "app": { "id": 1, "slug": "github-actions", "name": "GitHub Actions" },
        })
    }

    /// 验证汇总把 in_progress/queued 计入 pending 并记录最早的 startedAt。
    #[test]
    fn summarize_check_runs_counts_pending_split_and_started_at() {
        let runs = vec![
            check_run(
                1,
                "build",
                "completed",
                Some("success"),
                "2026-01-01T00:00:00Z",
            ),
            check_run(2, "test", "in_progress", None, "2026-01-01T00:05:00Z"),
            check_run(3, "lint", "queued", None, ""),
            check_run(4, "audit", "completed", None, "2026-01-01T00:06:00Z"),
            check_run(
                5,
                "deploy",
                "completed",
                Some("failure"),
                "2026-01-01T00:01:00Z",
            ),
            check_run(
                6,
                "skipme",
                "completed",
                Some("skipped"),
                "2026-01-01T00:02:00Z",
            ),
        ];
        let summary = summarize_check_runs(&runs);
        assert_eq!(summary["state"], "failure"); // any failure dominates
        assert_eq!(summary["success"], 2); // success + skipped
        assert_eq!(summary["failure"], 1);
        assert_eq!(summary["pending"], 3); // in_progress + queued + unconcluded
        assert_eq!(summary["inProgress"], 1);
        assert_eq!(summary["queued"], 1);
        assert_eq!(summary["total"], 6);
        // Earliest in-progress start time wins.
        assert_eq!(summary["startedAt"], "2026-01-01T00:05:00Z");
    }

    /// 验证无失败但有未完成 run 时聚合 state 为 pending。
    #[test]
    fn summarize_check_runs_pending_state_without_failures() {
        let runs = vec![check_run(
            1,
            "build",
            "in_progress",
            None,
            "2026-02-01T00:00:00Z",
        )];
        let summary = summarize_check_runs(&runs);
        assert_eq!(summary["state"], "pending");
        assert_eq!(summary["total"], 1);
    }

    /// 验证按 (app, name) 去重时保留 started_at 最新的 run。
    #[test]
    fn dedupe_check_runs_keeps_latest_run_per_app_and_name() {
        let runs = vec![
            check_run(
                11,
                "build",
                "completed",
                Some("failure"),
                "2026-01-01T00:00:00Z",
            ),
            check_run(12, "build", "in_progress", None, "2026-01-01T00:10:00Z"),
            check_run(
                20,
                "test",
                "completed",
                Some("success"),
                "2026-01-01T00:00:00Z",
            ),
        ];
        let deduped = dedupe_check_runs(&runs);
        assert_eq!(deduped.len(), 2);
        let build = deduped.iter().find(|r| r["name"] == "build").unwrap();
        assert_eq!(build["id"], 12); // newer started_at wins
    }

    /// 验证 started_at 相同时以更大的 run id 决胜。
    #[test]
    fn dedupe_check_runs_breaks_ties_by_run_id() {
        let runs = vec![
            check_run(5, "build", "queued", None, "2026-01-01T00:00:00Z"),
            check_run(9, "build", "queued", None, "2026-01-01T00:00:00Z"),
        ];
        let deduped = dedupe_check_runs(&runs);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0]["id"], 9);
    }

    /// 验证 legacy commit status 的 error 计入 failure。
    #[test]
    fn summarize_combined_statuses_maps_error_to_failure() {
        let statuses = json!([
            { "state": "success" },
            { "state": "error" },
            { "state": "pending" },
            { "state": "weird" },
        ]);
        let summary = summarize_combined_statuses(statuses.as_array().unwrap());
        assert_eq!(summary["state"], "failure");
        assert_eq!(summary["success"], 1);
        assert_eq!(summary["failure"], 1);
        assert_eq!(summary["pending"], 1);
        assert_eq!(summary["inProgress"], 1);
        assert_eq!(summary["queued"], 0);
        assert_eq!(summary["total"], 3);
    }

    /// 验证 ISO-8601 各形态（纯日期、毫秒、时区偏移）解析为正确毫秒时间戳。
    #[test]
    fn parse_iso_ms_handles_github_shapes() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00Z"), 0.0);
        assert_eq!(parse_iso_ms("1970-01-02T00:00:00Z"), 86_400_000.0);
        assert_eq!(parse_iso_ms("2026-01-01T00:00:00.500Z") > 1.7e12, true);
        assert_eq!(parse_iso_ms(""), 0.0);
        assert!(parse_iso_ms("not-a-date").is_nan());
    }

    /// 验证 details_url 的 run/job id 解析及非法输入返回 `None`。
    #[test]
    fn parse_actions_details_url_shapes() {
        assert_eq!(
            parse_actions_details_url("https://github.com/acme/app/actions/runs/12345"),
            Some((12345, None))
        );
        assert_eq!(
            parse_actions_details_url("https://github.com/acme/app/actions/runs/12345/job/678"),
            Some((12345, Some(678)))
        );
        assert_eq!(parse_actions_details_url("https://example.com/nope"), None);
    }

    /// 验证 refs/heads、heads、remotes 及已知 remote 名前缀的剥离规则。
    #[test]
    fn normalize_branch_ref_strips_prefixes_and_remote_names() {
        let names: std::collections::HashSet<String> = ["origin", "upstream"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(normalize_branch_ref("refs/heads/main", &names), "main");
        assert_eq!(normalize_branch_ref("heads/feat/x", &names), "feat/x");
        assert_eq!(normalize_branch_ref("remotes/origin/feat", &names), "feat");
        assert_eq!(normalize_branch_ref("origin/feat", &names), "feat");
        assert_eq!(normalize_branch_ref("gsxdsm/feat", &names), "gsxdsm/feat");
        // A remote prefix with nothing after it keeps the original shape.
        assert_eq!(normalize_branch_ref("origin/", &names), "origin/");
    }

    // ---- route shapes via oneshot -------------------------------------------

    /// 构建与生产 router 相同路由表的测试 Router（oneshot 驱动）。
    fn app_for(state: Arc<GithubState>) -> Router {
        Router::new()
            .route("/api/github/auth/status", get(auth_status))
            .route("/api/github/auth/gh-cli", post(auth_gh_cli))
            .route("/api/github/auth/start", post(auth_start))
            .route("/api/github/auth/complete", post(auth_complete))
            .route("/api/github/auth/activate", post(auth_activate))
            .route("/api/github/auth", delete(auth_delete))
            .route("/api/github/me", get(me))
            .route("/api/github/pr/status", get(pr_status_route))
            .route("/api/github/pr/create", post(pr_create))
            .route("/api/github/pr/update", post(pr_update))
            .route("/api/github/pr/merge", post(pr_merge))
            .route("/api/github/pr/ready", post(pr_ready))
            .route("/api/github/repo/upstream", get(repo_upstream))
            .route("/api/github/repo/branches", get(repo_branches))
            .route("/api/github/issues/list", get(issues_list))
            .route("/api/github/issues/get", get(issues_get))
            .route("/api/github/issues/comments", get(issues_comments))
            .route("/api/github/pulls/list", get(pulls_list))
            .route("/api/github/pulls/context", get(pulls_context))
            .with_state(state)
    }

    /// 把应答函数包装成忽略 token 的 [`TransportFactory`]。
    fn fake_transport(responder: Responder) -> TransportFactory {
        let client = client_with(responder);
        Arc::new(move |_token: &str| client.clone())
    }

    /// 新建独立限流门，避免测试间相互影响。
    fn fresh_gate() -> Arc<RateLimitGate> {
        Arc::new(RateLimitGate::new())
    }

    /// 验证未连接 GitHub 时 /me 返回 401 与固定错误文案。
    #[tokio::test]
    async fn me_returns_401_when_not_connected() {
        let (state, _dir) = make_state(
            "me404",
            Box::new(|req| {
                Err(GithubError {
                    status: None,
                    message: format!("unexpected {req:?}"),
                    headers: vec![],
                    data: None,
                })
            }),
        );
        let response = app_for(state)
            .oneshot(RequestBuilder::get("/api/github/me").build())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = body_json(response).await;
        assert_eq!(body["error"], "GitHub not connected");
    }

    /// Minimal request helper (avoids importing http::Request builders directly).
    /// 构造测试用 HTTP 请求的最小助手（GET/POST/DELETE + JSON body）。
    struct RequestBuilder {
        /// HTTP 方法。
        method: axum::http::Method,
        /// 请求 URI（含查询串）。
        uri: String,
        /// 可选的 JSON 请求体。
        body: Option<Value>,
    }

    /// [`RequestBuilder`] 的便捷构造与 `http::Request` 转换。
    impl RequestBuilder {
        /// 构造无 body 的 GET 请求。
        fn get(uri: impl Into<String>) -> Self {
            Self {
                method: axum::http::Method::GET,
                uri: uri.into(),
                body: None,
            }
        }
        /// 构造带 JSON body 的 POST 请求。
        fn post(uri: impl Into<String>, body: Value) -> Self {
            Self {
                method: axum::http::Method::POST,
                uri: uri.into(),
                body: Some(body),
            }
        }
        /// 构造无 body 的 DELETE 请求。
        fn delete(uri: impl Into<String>) -> Self {
            Self {
                method: axum::http::Method::DELETE,
                uri: uri.into(),
                body: None,
            }
        }
        /// 转成 `http::Request`；带 body 时自动设置 JSON content-type。
        fn build(self) -> axum::http::Request<Body> {
            let mut builder = axum::http::Request::builder()
                .method(self.method)
                .uri(self.uri);
            let body = match self.body {
                Some(json) => {
                    builder = builder.header("content-type", "application/json");
                    Body::from(serde_json::to_vec(&json).unwrap())
                }
                None => Body::empty(),
            };
            builder.body(body).unwrap()
        }
    }

    /// 验证存有自有 token 时 auth/status 返回 connected:true、账号与 scope 信息。
    #[tokio::test]
    async fn auth_status_reflects_connected_own_token_account() {
        let dir = temp_dir("authstatus");
        let auth = AuthStore::new(dir.clone());
        auth.set_github_auth(
            "token1234567890",
            "repo",
            "bearer",
            Some(AuthUser {
                login: Some("octocat".into()),
                avatar_url: Some("https://avatars.test/octocat.png".into()),
                id: Some(583231),
                name: Some("The Octocat".into()),
                email: Some("octocat@github.com".into()),
            }),
            None,
        )
        .unwrap();
        let transport = fake_transport(Box::new(|req| {
            assert!(req.url.starts_with("https://api.github.com/"));
            match req.url.as_str() {
                "https://api.github.com/user" => json_ok(json!({
                    "login": "octocat",
                    "id": 583231,
                    "avatar_url": "https://avatars.test/octocat.png",
                    "name": "The Octocat",
                    "email": null,
                })),
                "https://api.github.com/user/emails?per_page=100" => json_ok(
                    json!([{ "email": "octo@example.com", "primary": true, "verified": true }]),
                ),
                other => Err(GithubError {
                    status: Some(500),
                    message: format!("unexpected {other}"),
                    headers: vec![],
                    data: None,
                }),
            }
        }));
        let state = GithubState::with_seams(
            dir,
            transport,
            device_flow::default_form_poster(),
            Arc::new(|| None),
            fresh_gate(),
            Arc::new(PrStatusEngine::new()),
        );
        let response = app_for(state)
            .oneshot(RequestBuilder::get("/api/github/auth/status").build())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["connected"], true);
        assert_eq!(body["user"]["login"], "octocat");
        assert_eq!(body["user"]["email"], "octo@example.com"); // verified email fallback
        assert_eq!(body["scope"], "repo");
        assert_eq!(body["accounts"][0]["id"], "octocat");
        assert_eq!(body["accounts"][0]["current"], true);
        assert_eq!(body["ghCli"]["available"], false);
        assert!(body["ghCli"].get("user").is_none() || body["ghCli"]["user"].is_null());
    }

    /// 验证无任何凭据时 auth/status 返回 connected:false 与空账号列表。
    #[tokio::test]
    async fn auth_status_disconnected_without_tokens() {
        let (state, _dir) = make_state("disconnected", Box::new(|_| json_ok(json!({}))));
        let response = app_for(state)
            .oneshot(RequestBuilder::get("/api/github/auth/status").build())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["connected"], false);
        assert_eq!(body["accounts"].as_array().map(Vec::len), Some(0));
    }

    /// 验证 gh-cli 开关写入后回读生效，并同步清空 token 缓存。
    #[tokio::test]
    async fn auth_gh_cli_toggles_disabled_flag() {
        let (state, _dir) = make_state("ghclitoggle", Box::new(|_| json_ok(json!({}))));
        let response = app_for(state.clone())
            .oneshot(
                RequestBuilder::post("/api/github/auth/gh-cli", json!({ "disabled": true }))
                    .build(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["disabled"], true);
    }

    /// 验证 auth/start 返回 device flow 的驼峰化字段（deviceCode 等）。
    #[tokio::test]
    async fn auth_start_returns_device_flow_fields() {
        let dir = temp_dir("authstart");
        let poster: FormPoster = Arc::new(|url, _body| {
            assert_eq!(url, "https://github.com/login/device/code");
            let payload = json!({
                "device_code": "dc-1",
                "user_code": "WDJB-MJHT",
                "verification_uri": "https://github.com/login/device",
                "verification_uri_complete": "https://github.com/login/device#wdjb",
                "expires_in": 900,
                "interval": 5,
            });
            Box::pin(async move { Ok(payload) })
        });
        let state = GithubState::with_seams(
            dir,
            fake_transport(Box::new(|_| json_ok(json!({})))),
            poster,
            Arc::new(|| None),
            fresh_gate(),
            Arc::new(PrStatusEngine::new()),
        );
        let response = app_for(state)
            .oneshot(RequestBuilder::post("/api/github/auth/start", json!({})).build())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["deviceCode"], "dc-1");
        assert_eq!(body["userCode"], "WDJB-MJHT");
        assert_eq!(body["verificationUri"], "https://github.com/login/device");
        assert_eq!(body["expiresIn"], 900);
        assert_eq!(body["interval"], 5);
        assert_eq!(body["scope"], "repo read:org workflow read:user user:email");
    }

    /// 验证 device code 尚未完成授权时 auth/complete 返回
    /// connected:false 与 `authorization_pending` 状态码。
    #[tokio::test]
    async fn auth_complete_reports_pending_authorization() {
        let dir = temp_dir("authpending");
        let poster: FormPoster = Arc::new(|url, _| {
            assert_eq!(url, "https://github.com/login/oauth/access_token");
            let payload = json!({ "error": "authorization_pending" });
            Box::pin(async move { Ok(payload) })
        });
        let state = GithubState::with_seams(
            dir,
            fake_transport(Box::new(|_| json_ok(json!({})))),
            poster,
            Arc::new(|| None),
            fresh_gate(),
            Arc::new(PrStatusEngine::new()),
        );
        let response = app_for(state)
            .oneshot(
                RequestBuilder::post("/api/github/auth/complete", json!({ "deviceCode": "dc-1" }))
                    .build(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["connected"], false);
        assert_eq!(body["status"], "authorization_pending");
        assert_eq!(body["error"], "authorization_pending");
    }

    /// 验证 auth/complete 缺少 deviceCode 时返回 400。
    #[tokio::test]
    async fn auth_complete_requires_device_code() {
        let (state, _dir) = make_state("complete400", Box::new(|_| json_ok(json!({}))));
        let response = app_for(state)
            .oneshot(RequestBuilder::post("/api/github/auth/complete", json!({})).build())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(body["error"], "deviceCode is required");
    }

    /// 验证 DELETE /auth 清除存储凭据并返回 success:true / removed:true。
    #[tokio::test]
    async fn auth_delete_clears_the_account() {
        let dir = temp_dir("authdelete");
        let auth = AuthStore::new(dir.clone());
        auth.set_github_auth("token1234567890", "repo", "bearer", None, None)
            .unwrap();
        let state = GithubState::with_seams(
            dir,
            fake_transport(Box::new(|_| json_ok(json!({})))),
            device_flow::default_form_poster(),
            Arc::new(|| None),
            fresh_gate(),
            Arc::new(PrStatusEngine::new()),
        );
        let response = app_for(state)
            .oneshot(RequestBuilder::delete("/api/github/auth").build())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["success"], true);
        assert_eq!(body["removed"], true);
    }

    /// 开放 PR 的最小 fixture JSON（含 head 分支、sha 与 head 仓库信息）。
    const PR_FIXTURE: &str = r#"{
        "number": 15,
        "title": "Add feature",
        "body": "The body",
        "html_url": "https://github.com/acme/app/pull/15",
        "state": "open",
        "draft": false,
        "merged": false,
        "merged_at": null,
        "mergeable": true,
        "mergeable_state": "clean",
        "base": { "ref": "main" },
        "head": {
            "ref": "feature",
            "sha": "abc123def456",
            "label": "acme:feature",
            "repo": {
                "owner": { "login": "acme" },
                "name": "app",
                "html_url": "https://github.com/acme/app",
                "clone_url": "https://github.com/acme/app.git",
                "ssh_url": "git@github.com:acme/app.git"
            }
        }
    }"#;

    /// 把 [`PR_FIXTURE`] 解析成 `Value`，供各 pr/status 场景复用。
    fn open_pr_fixture() -> Value {
        serde_json::from_str(PR_FIXTURE).unwrap()
    }

    /// 构造 pr/status 场景的 fake transport：预置仓库元数据、PR 列表/详情、
    /// check-runs 与协作权限的路由表，并把每次调用计入 `calls`。
    fn pr_status_transport(dir: PathBuf, calls: Arc<AtomicI64>) -> (TransportFactory, PathBuf) {
        let auth = AuthStore::new(dir.clone());
        auth.set_github_auth(
            "token1234567890",
            "repo",
            "bearer",
            Some(AuthUser {
                login: Some("octocat".into()),
                avatar_url: None,
                id: Some(1),
                name: None,
                email: None,
            }),
            None,
        )
        .unwrap();
        let transport = {
            let calls = calls.clone();
            let responder: Responder = Box::new(move |req: &GithubRequest| {
                calls.fetch_add(1, Ordering::SeqCst);
                let url = reqwest::Url::parse(&req.url).unwrap();
                let path = url.path().to_string();
                let query: HashMap<String, String> = url
                    .query_pairs()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                match path.as_str() {
                    "/repos/acme/app" => json_ok(json!({ "default_branch": "main" })),
                    "/repos/acme/app/pulls" => {
                        if query.get("head").is_some() {
                            json_ok(json!([open_pr_fixture()]))
                        } else {
                            json_ok(json!([open_pr_fixture()]))
                        }
                    }
                    "/repos/acme/app/pulls/15" => json_ok(open_pr_fixture()),
                    "/repos/acme/app/commits/abc123def456/check-runs" => json_ok(json!({
                        "check_runs": [
                            { "id": 1, "name": "build", "status": "completed", "conclusion": "success", "started_at": "2026-01-01T00:00:00Z" },
                            { "id": 2, "name": "test", "status": "in_progress", "conclusion": null, "started_at": "2026-01-01T00:05:00Z" }
                        ]
                    })),
                    "/repos/acme/app/collaborators/octocat/permission" => {
                        json_ok(json!({ "permission": "write" }))
                    }
                    "/user" => json_ok(json!({ "login": "octocat", "id": 1 })),
                    other => Err(GithubError {
                        status: Some(500),
                        message: format!("unexpected path {other}"),
                        headers: vec![],
                        data: None,
                    }),
                }
            });
            let client = client_with(responder);
            Arc::new(move |_token: &str| client.clone()) as TransportFactory
        };
        (transport, dir)
    }

    /// 用给定 transport 与独立限流门组装 pr/status 测试状态。
    fn pr_status_state(transport: TransportFactory, dir: PathBuf) -> Arc<GithubState> {
        GithubState::with_seams(
            dir,
            transport,
            device_flow::default_form_poster(),
            Arc::new(|| None),
            fresh_gate(),
            Arc::new(PrStatusEngine::new()),
        )
    }

    /// 验证 pr/status 的完整响应形状，以及 TTL 内二次请求命中缓存
    /// （transport 调用计数不增长）。
    #[tokio::test]
    async fn pr_status_full_shape_and_cache_reuse() {
        let dir = temp_dir("prstatus");
        let calls = Arc::new(AtomicI64::new(0));
        let (transport, dir) = pr_status_transport(dir, calls.clone());
        let state = pr_status_state(transport, dir);
        let git = git_dir("prstatus", "git@github.com:acme/app.git");

        let uri = format!(
            "/api/github/pr/status?directory={}&branch=feature",
            urlencoding_encode(&git.to_string_lossy())
        );
        let response = app_for(state.clone())
            .oneshot(RequestBuilder::get(uri).build())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["connected"], true);
        assert_eq!(body["repo"]["owner"], "acme");
        assert_eq!(body["repo"]["repo"], "app");
        assert_eq!(body["branch"], "feature");
        assert_eq!(body["pr"]["number"], 15);
        assert_eq!(body["pr"]["title"], "Add feature");
        assert_eq!(body["pr"]["body"], "The body");
        assert_eq!(body["pr"]["url"], "https://github.com/acme/app/pull/15");
        assert_eq!(body["pr"]["state"], "open");
        assert_eq!(body["pr"]["draft"], false);
        assert_eq!(body["pr"]["base"], "main");
        assert_eq!(body["pr"]["head"], "feature");
        assert_eq!(body["pr"]["headSha"], "abc123def456");
        assert_eq!(body["pr"]["mergeable"], true);
        assert_eq!(body["pr"]["mergeableState"], "clean");
        assert_eq!(body["checks"]["state"], "pending");
        assert_eq!(body["checks"]["success"], 1);
        assert_eq!(body["checks"]["pending"], 1);
        assert_eq!(body["canMerge"], true);
        assert_eq!(body["defaultBranch"], "main");
        assert_eq!(body["resolvedRemoteName"], "origin");
        assert!(body["fetchedAt"].is_i64());

        // Second request is served from the 90s cache: no new GitHub calls.
        let calls_after_first = calls.load(Ordering::SeqCst);
        assert!(calls_after_first > 0);
        let uri2 = format!(
            "/api/github/pr/status?directory={}&branch=feature",
            urlencoding_encode(&git.to_string_lossy())
        );
        let response = app_for(state)
            .oneshot(RequestBuilder::get(uri2).build())
            .await
            .unwrap();
        let body2 = body_json(response).await;
        assert_eq!(body2["pr"]["number"], 15);
        assert_eq!(calls.load(Ordering::SeqCst), calls_after_first);
    }

    /// 与 JS `encodeURIComponent` 等价的百分号编码（额外保留 `/`），
    /// 用于把目录路径拼进测试查询串。
    fn urlencoding_encode(value: &str) -> String {
        let mut out = String::new();
        for byte in value.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                    out.push(byte as char)
                }
                _ => out.push_str(&format!("%{byte:02X}")),
            }
        }
        out
    }

    /// 验证 pr/status 缺少 directory 或 branch 时返回 400。
    #[tokio::test]
    async fn pr_status_requires_directory_and_branch() {
        let (state, _dir) = make_state("pr400", Box::new(|_| json_ok(json!({}))));
        let response = app_for(state)
            .oneshot(RequestBuilder::get("/api/github/pr/status?directory=/tmp").build())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(body["error"], "directory and branch are required");
    }

    /// 验证无任何 token 时 pr/status 返回 `connected:false` 而不报错。
    #[tokio::test]
    async fn pr_status_disconnected_without_token() {
        let (state, _dir) = make_state("prdisc", Box::new(|_| json_ok(json!({}))));
        let git = git_dir("prdisc", "git@github.com:acme/app.git");
        let uri = format!(
            "/api/github/pr/status?directory={}&branch=feature",
            urlencoding_encode(&git.to_string_lossy())
        );
        let response = app_for(state)
            .oneshot(RequestBuilder::get(uri).build())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["connected"], false);
    }

    /// 验证限流冷却期内且无缓存时 pr/status 返回 503 与固定文案。
    #[tokio::test]
    async fn pr_status_rate_limited_serves_503_without_cache() {
        let dir = temp_dir("prrl");
        let auth = AuthStore::new(dir.clone());
        auth.set_github_auth("token1234567890", "repo", "bearer", None, None)
            .unwrap();
        let gate = Arc::new(RateLimitGate::new());
        // Cooldown that is still active.
        let error = GithubError {
            status: Some(429),
            message: "rate limited".into(),
            headers: vec![("retry-after".to_string(), "60".to_string())],
            data: None,
        };
        gate.note_rate_limit(&error, rate_limit::now_ms());
        let state = GithubState::with_seams(
            dir,
            fake_transport(Box::new(|_| json_ok(json!({})))),
            device_flow::default_form_poster(),
            Arc::new(|| None),
            gate,
            Arc::new(PrStatusEngine::new()),
        );
        let response = app_for(state)
            .oneshot(RequestBuilder::get("/api/github/pr/status?directory=/tmp&branch=x").build())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(response).await;
        assert_eq!(body["error"], "GitHub rate limited");
    }

    /// 验证 pr/update 把 422 响应的 message 与 errors[0] 用 `·` 拼接后返回。
    #[tokio::test]
    async fn pr_update_maps_422_to_joined_message() {
        let dir = temp_dir("pr422");
        let auth = AuthStore::new(dir.clone());
        auth.set_github_auth("token1234567890", "repo", "bearer", None, None)
            .unwrap();
        let git = git_dir("pr422", "git@github.com:acme/app.git");
        let transport = fake_transport(Box::new(|req| {
            if req
                .url
                .starts_with("https://api.github.com/repos/acme/app/pulls/7")
            {
                json_error(
                    422,
                    json!({
                        "message": "Validation Failed",
                        "errors": [{ "message": "title is too long", "code": "too_long" }]
                    }),
                )
            } else {
                json_ok(json!({}))
            }
        }));
        let state = GithubState::with_seams(
            dir,
            transport,
            device_flow::default_form_poster(),
            Arc::new(|| None),
            fresh_gate(),
            Arc::new(PrStatusEngine::new()),
        );
        let response = app_for(state)
            .oneshot(
                RequestBuilder::post(
                    "/api/github/pr/update",
                    json!({ "directory": git.to_string_lossy(), "number": 7, "title": "new" }),
                )
                .build(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = body_json(response).await;
        assert_eq!(body["error"], "Validation Failed · title is too long");
    }

    /// 验证 pr/merge 的 409 冲突映射为 200 + `merged:false` 载荷而非错误。
    #[tokio::test]
    async fn pr_merge_conflict_maps_to_merged_false() {
        let dir = temp_dir("prmerge409");
        let auth = AuthStore::new(dir.clone());
        auth.set_github_auth("token1234567890", "repo", "bearer", None, None)
            .unwrap();
        let git = git_dir("prmerge409", "git@github.com:acme/app.git");
        let transport = fake_transport(Box::new(|req| {
            if req.url.ends_with("/merge") {
                json_error(409, json!({ "message": "Pull Request is not mergeable" }))
            } else {
                json_ok(json!({}))
            }
        }));
        let state = GithubState::with_seams(
            dir,
            transport,
            device_flow::default_form_poster(),
            Arc::new(|| None),
            fresh_gate(),
            Arc::new(PrStatusEngine::new()),
        );
        let response = app_for(state)
            .oneshot(
                RequestBuilder::post(
                    "/api/github/pr/merge",
                    json!({ "directory": git.to_string_lossy(), "number": 7 }),
                )
                .build(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["merged"], false);
        assert_eq!(body["message"], "Pull Request is not mergeable");
    }

    /// 验证 pr/create 缺少必填字段（directory/title/head/base）时返回 400。
    #[tokio::test]
    async fn pr_create_validates_required_fields() {
        let (state, _dir) = make_state("prcreate400", Box::new(|_| json_ok(json!({}))));
        let response = app_for(state)
            .oneshot(
                RequestBuilder::post(
                    "/api/github/pr/create",
                    json!({ "directory": "/tmp", "title": "t" }),
                )
                .build(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(body["error"], "directory, title, head, base are required");
    }

    /// 验证 repo/branches 按页拉取分支名，直到出现不满页（<100）时停止。
    #[tokio::test]
    async fn repo_branches_paginates_until_short_page() {
        let dir = temp_dir("branches");
        let auth = AuthStore::new(dir.clone());
        auth.set_github_auth("token1234567890", "repo", "bearer", None, None)
            .unwrap();
        let pages = Arc::new(AtomicUsize::new(0));
        let pages_for_transport = pages.clone();
        let transport = fake_transport(Box::new(move |req| {
            let page = reqwest::Url::parse(&req.url)
                .ok()
                .and_then(|url| {
                    url.query_pairs()
                        .find(|(k, _)| k == "page")
                        .and_then(|(_, v)| v.parse::<u32>().ok())
                })
                .unwrap_or(1);
            pages_for_transport.fetch_add(1, Ordering::SeqCst);
            if page == 1 {
                json_ok(Value::Array(
                    (0..100)
                        .map(|i| json!({ "name": format!("b{i}") }))
                        .collect(),
                ))
            } else {
                json_ok(json!([{ "name": "last" }]))
            }
        }));
        let state = GithubState::with_seams(
            dir,
            transport,
            device_flow::default_form_poster(),
            Arc::new(|| None),
            fresh_gate(),
            Arc::new(PrStatusEngine::new()),
        );
        let response = app_for(state)
            .oneshot(RequestBuilder::get("/api/github/repo/branches?owner=acme&repo=app").build())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        let branches = body["branches"].as_array().unwrap();
        assert_eq!(branches.len(), 101);
        assert_eq!(branches[100], "last");
        assert_eq!(pages.load(Ordering::SeqCst), 2);
    }

    /// 验证 pulls/context 的完整载荷形状（评论/文件/checks 摘要），
    /// 以及 TTL 内二次请求命中缓存（transport 调用数不增长）。
    #[tokio::test]
    async fn pulls_context_caches_within_ttl() {
        let dir = temp_dir("ctxcache");
        let auth = AuthStore::new(dir.clone());
        auth.set_github_auth("token1234567890", "repo", "bearer", None, None)
            .unwrap();
        let git = git_dir("ctxcache", "git@github.com:acme/app.git");
        let calls = Arc::new(AtomicI64::new(0));
        let calls_for_transport = calls.clone();
        let transport = fake_transport(Box::new(move |req| {
            calls_for_transport.fetch_add(1, Ordering::SeqCst);
            match req.url.as_str() {
                "https://api.github.com/repos/acme/app" => {
                    json_ok(json!({ "default_branch": "main" }))
                }
                "https://api.github.com/repos/acme/app/pulls/15" => json_ok(open_pr_fixture()),
                "https://api.github.com/repos/acme/app/issues/15/comments?per_page=100" => {
                    json_ok(json!([{
                        "id": 1,
                        "html_url": "https://github.com/acme/app/pull/15#issuecomment-1",
                        "body": "looks good",
                        "created_at": "2026-01-01T00:00:00Z",
                        "updated_at": "2026-01-01T00:00:00Z",
                        "user": { "login": "octocat", "id": 1, "avatar_url": "https://avatars.test/a.png" }
                    }]))
                }
                "https://api.github.com/repos/acme/app/pulls/15/comments?per_page=100" => {
                    json_ok(json!([]))
                }
                "https://api.github.com/repos/acme/app/pulls/15/files?per_page=100" => {
                    json_ok(json!([{
                        "filename": "src/lib.rs", "status": "modified",
                        "additions": 2, "deletions": 1, "changes": 3,
                        "patch": "@@ -1,2 +1,3 @@"
                    }]))
                }
                "https://api.github.com/repos/acme/app/commits/abc123def456/check-runs?per_page=100" => {
                    json_ok(json!({ "check_runs": [] }))
                }
                "https://api.github.com/repos/acme/app/commits/abc123def456/status" => {
                    json_ok(json!({ "statuses": [] }))
                }
                other => Err(GithubError {
                    status: Some(500),
                    message: format!("unexpected {other}"),
                    headers: vec![],
                    data: None,
                }),
            }
        }));
        let state = GithubState::with_seams(
            dir,
            transport,
            device_flow::default_form_poster(),
            Arc::new(|| None),
            fresh_gate(),
            Arc::new(PrStatusEngine::new()),
        );
        let uri = format!(
            "/api/github/pulls/context?directory={}&number=15",
            urlencoding_encode(&git.to_string_lossy())
        );
        let response = app_for(state.clone())
            .oneshot(RequestBuilder::get(&uri).build())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["connected"], true);
        assert_eq!(body["pr"]["number"], 15);
        assert_eq!(body["pr"]["headRepo"]["owner"], "acme");
        assert_eq!(body["issueComments"][0]["body"], "looks good");
        assert_eq!(body["files"][0]["filename"], "src/lib.rs");
        assert_eq!(body["checks"]["state"], "unknown");
        assert!(body.get("diff").is_none());
        assert!(body.get("checkRuns").is_none());

        let calls_after_first = calls.load(Ordering::SeqCst);
        let response = app_for(state)
            .oneshot(RequestBuilder::get(&uri).build())
            .await
            .unwrap();
        let body2 = body_json(response).await;
        assert_eq!(body2["pr"]["number"], 15);
        assert_eq!(calls.load(Ordering::SeqCst), calls_after_first);
    }

    /// 验证 `diff=1` 时 pulls/context 响应携带 `diff` 字段。
    #[tokio::test]
    async fn pulls_context_includes_diff_when_requested() {
        let dir = temp_dir("ctxdiff");
        let auth = AuthStore::new(dir.clone());
        auth.set_github_auth("token1234567890", "repo", "bearer", None, None)
            .unwrap();
        let git = git_dir("ctxdiff", "git@github.com:acme/app.git");
        let transport = fake_transport(Box::new(move |req| {
            let is_diff = req.headers.iter().any(|(k, v)| {
                k.eq_ignore_ascii_case("accept") && v == "application/vnd.github.v3.diff"
            });
            match req.url.as_str() {
                "https://api.github.com/repos/acme/app" => {
                    json_ok(json!({ "default_branch": "main" }))
                }
                "https://api.github.com/repos/acme/app/pulls/15" => {
                    if is_diff {
                        Ok(GithubResponse {
                            status: 200,
                            headers: vec![(
                                "content-type".to_string(),
                                "application/vnd.github.v3.diff; charset=utf-8".to_string(),
                            )],
                            body: b"diff --git a/x b/x".to_vec(),
                        })
                    } else {
                        json_ok(open_pr_fixture())
                    }
                }
                "https://api.github.com/repos/acme/app/issues/15/comments?per_page=100" => {
                    json_ok(json!([]))
                }
                "https://api.github.com/repos/acme/app/pulls/15/comments?per_page=100" => {
                    json_ok(json!([]))
                }
                "https://api.github.com/repos/acme/app/pulls/15/files?per_page=100" => {
                    json_ok(json!([]))
                }
                "https://api.github.com/repos/acme/app/commits/abc123def456/check-runs?per_page=100" => {
                    json_ok(json!({ "check_runs": [] }))
                }
                "https://api.github.com/repos/acme/app/commits/abc123def456/status" => {
                    json_ok(json!({ "statuses": [] }))
                }
                other => Err(GithubError {
                    status: Some(500),
                    message: format!("unexpected {other}"),
                    headers: vec![],
                    data: None,
                }),
            }
        }));
        let state = GithubState::with_seams(
            dir,
            transport,
            device_flow::default_form_poster(),
            Arc::new(|| None),
            fresh_gate(),
            Arc::new(PrStatusEngine::new()),
        );
        let uri = format!(
            "/api/github/pulls/context?directory={}&number=15&diff=1",
            urlencoding_encode(&git.to_string_lossy())
        );
        let response = app_for(state)
            .oneshot(RequestBuilder::get(&uri).build())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["diff"], "diff --git a/x b/x");
    }

    /// 验证 auth/activate 对未知 accountId 返回 404。
    #[tokio::test]
    async fn auth_activate_unknown_account_is_404() {
        let (state, _dir) = make_state("activate404", Box::new(|_| json_ok(json!({}))));
        let response = app_for(state)
            .oneshot(
                RequestBuilder::post(
                    "/api/github/auth/activate",
                    json!({ "accountId": "nobody" }),
                )
                .build(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = body_json(response).await;
        assert_eq!(body["error"], "GitHub account not found");
    }

    /// 验证 issues/get 对 PR（带 pull_request 字段）返回 400 "Not a GitHub issue"。
    #[tokio::test]
    async fn issues_get_rejects_pull_requests() {
        let dir = temp_dir("issuepr");
        let auth = AuthStore::new(dir.clone());
        auth.set_github_auth("token1234567890", "repo", "bearer", None, None)
            .unwrap();
        let git = git_dir("issuepr", "git@github.com:acme/app.git");
        let transport = fake_transport(Box::new(move |req| match req.url.as_str() {
            "https://api.github.com/repos/acme/app" => json_ok(json!({ "default_branch": "main" })),
            "https://api.github.com/repos/acme/app/issues/9" => {
                json_ok(json!({ "number": 9, "title": "t", "pull_request": { "url": "x" } }))
            }
            other => Err(GithubError {
                status: Some(500),
                message: format!("unexpected {other}"),
                headers: vec![],
                data: None,
            }),
        }));
        let state = GithubState::with_seams(
            dir,
            transport,
            device_flow::default_form_poster(),
            Arc::new(|| None),
            fresh_gate(),
            Arc::new(PrStatusEngine::new()),
        );
        let uri = format!(
            "/api/github/issues/get?directory={}&number=9",
            urlencoding_encode(&git.to_string_lossy())
        );
        let response = app_for(state)
            .oneshot(RequestBuilder::get(&uri).build())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(body["error"], "Not a GitHub issue");
    }

    /// 验证 counting_client 对每次 API 调用递增计数器。
    #[tokio::test]
    async fn counting_client_counts_transport_calls() {
        let calls = Arc::new(AtomicI64::new(0));
        let client = counting_client(calls.clone(), Box::new(|_| json_ok(json!({}))));
        client.users_get_authenticated().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
