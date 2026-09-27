//! Port of `server/lib/github/pr-status.js` — PR lookup across remotes,
//! forks, and upstreams for a local branch.
//!
//! Behavior preserved from the JS (see `DOCUMENTATION.md` "How PR resolution
//! works"): remote ranking, fork-network expansion through `parent`/`source`,
//! default-branch skip, open-PR preference with per-owner head queries, the
//! shared repo-level pulls cache with in-flight coalescing, remembered
//! closed/merged history (6h found / 10m absent), Search-API fallback with
//! 403 backoff and miss caching, and open-PR-wins-over-historical ordering.
//!
//! 中文概述：为本地分支解析 GitHub PR 状态的引擎。跨 remote（显式指定、跟踪
//! remote、origin/upstream 及其余）、fork 网络与上游仓库查找与分支关联的
//! open PR；找不到时回退到已关闭/已合并的历史 PR。缓存与降级策略（列表 TTL、
//! 在途合并、Search 403 退避、未命中记忆）均与 JS 版 pr-status.js 对齐。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::github::client::{GithubClient, GithubError};
use crate::github::git_ops;
use crate::github::rate_limit::{GLOBAL_GATE, now_ms};
use crate::github::repo::{RepoRef, normalize_repo_key, resolve_github_repo_from_directory};

/// 仓库默认分支/元数据缓存的 TTL（5 分钟）。
const REPO_DEFAULT_BRANCH_TTL_MS: i64 = 5 * 60_000;
/// 仓库级 PR 列表缓存的 TTL（45 秒）。
const REPO_PULLS_CACHE_TTL_MS: i64 = 45_000;
/// 历史 PR“命中”记录的缓存 TTL（6 小时）。
const HISTORICAL_PR_FOUND_TTL_MS: i64 = 6 * 60 * 60 * 1000;
/// 历史 PR“不存在”记录的缓存 TTL（10 分钟）。
const HISTORICAL_PR_ABSENT_TTL_MS: i64 = 10 * 60 * 1000;
/// 历史 PR 缓存的最大条目数（超出后淘汰最旧记录）。
const HISTORICAL_PR_CACHE_MAX_ENTRIES: usize = 500;
/// Search API 因 403 被禁用后的重试间隔（5 分钟）。
const SEARCH_API_RETRY_MS: i64 = 5 * 60 * 1000;
/// Search 未命中缓存的 TTL（10 分钟内不重查同一分支）。
const SEARCH_MISS_RETRY_MS: i64 = 10 * 60 * 1000;
/// Search 未命中缓存的最大条目数。
const SEARCH_MISS_CACHE_MAX_ENTRIES: usize = 500;

/// 对可选字符串做 trim 归一化；`None` 视为空字符串（JS 版 normalizeText）。
pub(crate) fn normalize_text(value: Option<&str>) -> String {
    value.unwrap_or("").trim().to_string()
}

/// 按 JSON Pointer 读取字符串字段；类型不符或缺失返回 `None`。
fn text_at<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(Value::as_str)
}

/// 读取 JSON Pointer 处的字符串并归一化为小写（供大小写不敏感比较）。
fn normalize_lower_at(value: &Value, pointer: &str) -> String {
    normalize_text(text_at(value, pointer)).to_lowercase()
}

/// 从 `origin/main` 形式的 upstream 引用解析 remote 名；
/// 无 `/` 或前缀为空时返回空串。
pub fn parse_tracking_remote_name(tracking_branch: Option<&str>) -> String {
    let normalized = normalize_text(tracking_branch);
    if normalized.is_empty() {
        return String::new();
    }
    match normalized.find('/') {
        Some(index) if index > 0 => normalized[..index].trim().to_string(),
        _ => String::new(),
    }
}

/// 从 `origin/feat/x` 解析分支名（首个 `/` 之后的全部内容）；
/// 无分隔符或尾随 `/` 时返回空串。
pub fn parse_tracking_branch_name(tracking_branch: Option<&str>) -> String {
    let normalized = normalize_text(tracking_branch);
    if normalized.is_empty() {
        return String::new();
    }
    match normalized.find('/') {
        Some(index) if index > 0 && index < normalized.len() - 1 => {
            normalized[index + 1..].trim().to_string()
        }
        _ => String::new(),
    }
}

/// `pushUnique` with the lowercase key function.
/// 以小写 key 去重地追加元素：空串忽略，保留原大小写，重复跳过。
fn push_unique_lower(collection: &mut Vec<String>, value: &str) {
    let normalized = normalize_text(Some(value));
    if normalized.is_empty() {
        return;
    }
    let key = normalized.to_lowercase();
    if collection.iter().any(|item| item.to_lowercase() == key) {
        return;
    }
    collection.push(normalized);
}

/// 生成 remote 查找优先级：显式 remote > 跟踪 remote > origin > upstream > 其余按传入顺序。
/// 去重大小写不敏感，保留首次出现的写法。
pub fn rank_remote_names(remote_names: &[String], explicit: &str, tracking: &str) -> Vec<String> {
    let mut ranked = Vec::new();
    push_unique_lower(&mut ranked, explicit);
    if !tracking.is_empty() {
        push_unique_lower(&mut ranked, tracking);
    }
    push_unique_lower(&mut ranked, "origin");
    push_unique_lower(&mut ranked, "upstream");
    for name in remote_names {
        push_unique_lower(&mut ranked, name);
    }
    ranked
}

/// 提取 PR head 的 owner：优先 `head.repo.owner.login`，其次 `head.user.login`，
/// 最后从 `head.label`（`owner:branch`）截取；全部缺失时返回空串。
pub fn get_head_owner(pr: &Value) -> String {
    let repo_owner = normalize_text(text_at(pr, "/head/repo/owner/login"));
    if !repo_owner.is_empty() {
        return repo_owner;
    }
    let user_owner = normalize_text(text_at(pr, "/head/user/login"));
    if !user_owner.is_empty() {
        return user_owner;
    }
    let head_label = normalize_text(text_at(pr, "/head/label"));
    match head_label.find(':') {
        Some(index) if index > 0 => head_label[..index].trim().to_string(),
        _ => String::new(),
    }
}

/// 生成 PR head 仓库的归一化 key `owner/repo`：优先 head.repo 字段，
/// 缺失时用 `head.label` 的 owner 拼上 fallback 仓库名；无法确定返回空串。
pub fn get_head_repo_key(pr: &Value, fallback_repo_name: &str) -> String {
    let repo_owner = normalize_text(text_at(pr, "/head/repo/owner/login"));
    let repo_name = normalize_text(text_at(pr, "/head/repo/name"));
    if !repo_owner.is_empty() && !repo_name.is_empty() {
        return normalize_repo_key(&repo_owner, &repo_name);
    }
    let head_label = normalize_text(text_at(pr, "/head/label"));
    if let Some(index) = head_label.find(':')
        && index > 0
    {
        let label_owner = head_label[..index].trim().to_string();
        if !label_owner.is_empty() && !fallback_repo_name.is_empty() {
            return normalize_repo_key(&label_owner, fallback_repo_name);
        }
    }
    String::new()
}

/// `buildSourceMatcher`: rank PR head repos/owners by candidate order.
/// 按候选顺序对 PR head 仓库/owner 建立排名表，供匹配与排序使用。
pub struct SourceMatcher {
/// 候选 `owner/repo` key → 候选序号。
    repo_rank: HashMap<String, usize>,
/// 候选 owner（小写）→ 候选序号。
    owner_rank: HashMap<String, usize>,
}

/// 构建与查询 head 匹配排名表。
impl SourceMatcher {
/// 由 source 候选构建匹配器：同一 key 只记录最先出现的序号。
    pub fn build(source_candidates: &[SourceCandidate]) -> Self {
        let mut repo_rank = HashMap::new();
        let mut owner_rank = HashMap::new();
        for (index, candidate) in source_candidates.iter().enumerate() {
            let repo_key = normalize_repo_key(&candidate.repo.owner, &candidate.repo.repo);
            if !repo_key.is_empty() {
                repo_rank.entry(repo_key).or_insert(index);
            }
            let owner = candidate.repo.owner.trim().to_lowercase();
            if !owner.is_empty() {
                owner_rank.entry(owner).or_insert(index);
            }
        }
        Self {
            repo_rank,
            owner_rank,
        }
    }

/// PR 的 head 仓库或 owner 命中任一候选即视为匹配。
    pub fn matches(&self, pr: &Value, fallback_repo_name: &str) -> bool {
        let repo_key = get_head_repo_key(pr, fallback_repo_name);
        if !repo_key.is_empty() && self.repo_rank.contains_key(&repo_key) {
            return true;
        }
        let owner = get_head_owner(pr).to_lowercase();
        !owner.is_empty() && self.owner_rank.contains_key(&owner)
    }

/// 比较两个 PR 与候选的贴近程度：先比 head 仓库排名，再比 owner 排名；
/// 未命中的按 `usize::MAX` 排在最后。
    pub fn compare(
        &self,
        left: &Value,
        right: &Value,
        fallback_repo_name: &str,
    ) -> std::cmp::Ordering {
        let left_repo = self
            .repo_rank
            .get(&get_head_repo_key(left, fallback_repo_name))
            .copied()
            .unwrap_or(usize::MAX);
        let right_repo = self
            .repo_rank
            .get(&get_head_repo_key(right, fallback_repo_name))
            .copied()
            .unwrap_or(usize::MAX);
        if left_repo != right_repo {
            return left_repo.cmp(&right_repo);
        }
        let left_owner = self
            .owner_rank
            .get(&get_head_owner(left).to_lowercase())
            .copied()
            .unwrap_or(usize::MAX);
        let right_owner = self
            .owner_rank
            .get(&get_head_owner(right).to_lowercase())
            .copied()
            .unwrap_or(usize::MAX);
        left_owner.cmp(&right_owner)
    }
}

/// 分支可能的来源仓库（fork 网络中的 head 候选）。
#[derive(Debug, Clone)]
pub struct SourceCandidate {
/// 来源仓库。
    pub repo: RepoRef,
}

/// 一个待查询的 PR 归属候选：仓库、对应 remote 名与优先级。
#[derive(Debug, Clone)]
pub struct Target {
/// 目标仓库。
    pub repo: RepoRef,
/// 该仓库对应的本地 remote 名。
    pub remote_name: String,
/// 查询优先级（越小越先查；fork 网络扩展时按 parent/source 递增）。
    pub priority: f64,
}

/// 分支 PR 状态解析结果：open 或历史 PR、归属仓库、默认分支与命中的 remote。
#[derive(Debug, Clone)]
pub struct ResolvedStatus {
/// 命中的仓库（完全无候选时为 `None`）。
    pub repo: Option<RepoRef>,
/// 命中的 PR（open 优先，否则为已关闭/已合并的历史 PR）。
    pub pr: Option<Value>,
/// 命中仓库的默认分支名。
    pub default_branch: Option<String>,
/// 命中候选对应的 remote 名；经 Search 回退命中的为 `None`。
    pub resolved_remote_name: Option<String>,
}

/// 空结果的构造。
impl ResolvedStatus {
/// 全字段为 `None` 的空状态（目录不存在、无任何候选等场景）。
    fn empty() -> Self {
        Self {
            repo: None,
            pr: None,
            default_branch: None,
            resolved_remote_name: None,
        }
    }
}

/// PR 是否已终止：state 为 closed，或 `merged_at` 非空（已合并）。
pub fn is_terminal_pr(pr: &Value) -> bool {
    pr.get("state").and_then(Value::as_str) == Some("closed")
        || pr.get("merged_at").map(|v| !v.is_null()).unwrap_or(false)
}

/// `safeListPulls`: pulls.list; 404/403 → empty, other errors propagate.
/// 调用 pulls.list：404（仓库不存在）与 403（无权限/限流）吞掉并返回空列表，
/// 其余错误向上传播；限流错误同时上报全局 gate。
async fn safe_list_pulls(
    engine: &PrStatusEngine,
    client: &GithubClient,
    owner: &str,
    repo: &str,
    state: &str,
    head: Option<&str>,
    per_page: u32,
) -> Result<Vec<Value>, GithubError> {
    match client
        .pulls_list(owner, repo, state, head, per_page, None)
        .await
    {
        Ok((data, _)) => Ok(data.as_array().cloned().unwrap_or_default()),
        Err(error) => {
            GLOBAL_GATE.note_if_rate_limit_error(&error, engine.now());
            if error.status == Some(404) || error.status == Some(403) {
                return Ok(Vec::new());
            }
            Err(error)
        }
    }
}

/// 仓库级 PR 列表缓存条目。
#[derive(Debug, Clone)]
pub struct RepoPullsEntry {
/// 拉取时间戳（毫秒），用于 TTL 判断。
    pub fetched_at: i64,
/// 缓存的 PR JSON 数组。
    pub prs: Vec<Value>,
    /// First page held everything, so a miss is authoritative.
/// 为 true 表示第一页已包含全部结果，列表未命中即可下权威结论。
    pub complete: bool,
}

/// 同一 repo+state 的在途合并门闩：等待者通过 completed 计数判断能否共享结果。
struct PullsGate {
/// 串行化实际 GitHub 调用的异步锁。
    lock: tokio::sync::Mutex<()>,
/// 已完成的拉取次数，供等待者检测“等锁期间已有新结果”。
    completed: AtomicU64,
}

/// 可注入的时钟函数（返回毫秒时间戳）；测试用假时钟推进 TTL 边界。
type Clock = Box<dyn Fn() -> i64 + Send + Sync>;

/// The PR resolution engine plus every module-level cache from pr-status.js.
/// PR 解析引擎：集中持有 pr-status.js 的全部模块级缓存——默认分支、仓库元数据、
/// PR 列表（含在途合并门闩）、历史 PR 记忆、Search 禁用与未命中记录。
pub struct PrStatusEngine {
/// 注入的时钟。
    now: Clock,
/// repo key → (默认分支, 拉取时间)。
    default_branch_cache: Mutex<HashMap<String, (Option<String>, i64)>>,
/// repo key → (仓库元数据, 拉取时间)；元数据为 `None` 表示 403/404 已缓存。
    repo_metadata_cache: Mutex<HashMap<String, (Option<Value>, i64)>>,
/// `owner/repo::state` → PR 列表条目。
    repo_pulls_cache: Mutex<HashMap<String, RepoPullsEntry>>,
/// `owner/repo::state` → 在途合并门闩。
    pulls_gates: Mutex<HashMap<String, Arc<PullsGate>>>,
/// 历史 PR 记忆表 (key, PR, 时间)，FIFO 容量淘汰。
    historical_pr_cache: Mutex<Vec<(String, Option<Value>, i64)>>,
/// repo 集合 key → Search API 被 403 禁用至的时间点。
    search_api_disabled: Mutex<HashMap<String, i64>>,
/// Search 未命中记录 (key, 时间)，避免反复搜索同一分支。
    search_miss_cache: Mutex<Vec<(String, i64)>>,
}

/// 解析入口与各缓存的生命周期管理。
impl PrStatusEngine {
/// 使用系统时钟构造引擎。
    pub fn new() -> Self {
        Self::with_clock(Box::new(now_ms))
    }

    /// Injectable clock (JS tests use fake timers).
/// 以自定义时钟构造（测试注入假时间以穿越 TTL 边界）。
    pub fn with_clock(now: Clock) -> Self {
        Self {
            now,
            default_branch_cache: Mutex::new(HashMap::new()),
            repo_metadata_cache: Mutex::new(HashMap::new()),
            repo_pulls_cache: Mutex::new(HashMap::new()),
            pulls_gates: Mutex::new(HashMap::new()),
            historical_pr_cache: Mutex::new(Vec::new()),
            search_api_disabled: Mutex::new(HashMap::new()),
            search_miss_cache: Mutex::new(Vec::new()),
        }
    }

/// 当前注入时钟的毫秒时间戳。
    fn now(&self) -> i64 {
        (self.now)()
    }

/// 读取未过期的默认分支缓存；未命中或已过期返回 `None`。
    fn get_repo_default_branch_cached(&self, repo_key: &str) -> Option<Option<String>> {
        let (branch, fetched_at) = self
            .default_branch_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(repo_key)
            .cloned()?;
        if self.now() - fetched_at < REPO_DEFAULT_BRANCH_TTL_MS {
            return Some(branch);
        }
        None
    }

    /// `getRepoDefaultBranch`: metadata-backed, all errors → null.
/// 查询仓库默认分支：先查专用缓存，再复用新鲜的元数据缓存，
/// 最后才请求 repos API；任何错误（含限流）都归一化为 `None`。
    pub async fn get_repo_default_branch(
        &self,
        client: &GithubClient,
        repo: &RepoRef,
    ) -> Option<String> {
        let repo_key = normalize_repo_key(&repo.owner, &repo.repo);
        if repo_key.is_empty() {
            return None;
        }
        if let Some(cached) = self.get_repo_default_branch_cached(&repo_key) {
            return cached;
        }
        // Reuse fresh full metadata (expandRepoNetwork fetched it already).
        if let Some((data, fetched_at)) = self
            .repo_metadata_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&repo_key)
            .cloned()
            && self.now() - fetched_at < REPO_DEFAULT_BRANCH_TTL_MS
        {
            let branch = data
                .as_ref()
                .and_then(|d| d.get("default_branch"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|b| !b.trim().is_empty());
            self.default_branch_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(repo_key, (branch.clone(), self.now()));
            return branch;
        }
        match client.repos_get(&repo.owner, &repo.repo).await {
            Ok(data) => {
                let branch = data
                    .get("default_branch")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .filter(|b| !b.trim().is_empty());
                self.default_branch_cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(repo_key, (branch.clone(), self.now()));
                branch
            }
            Err(error) => {
                GLOBAL_GATE.note_if_rate_limit_error(&error, self.now());
                None
            }
        }
    }

    /// `getRepoMetadata`: 403/404 → cached null; rate-limit noted; other
    /// errors propagate.
/// 读取仓库元数据并写入 TTL 缓存：403/404 缓存为 `None`，
/// 限流错误上报 gate，其余错误向上传播。
    pub async fn get_repo_metadata(
        &self,
        client: &GithubClient,
        repo: &RepoRef,
    ) -> Result<Option<Value>, GithubError> {
        let repo_key = normalize_repo_key(&repo.owner, &repo.repo);
        if repo_key.is_empty() {
            return Ok(None);
        }
        if let Some((data, fetched_at)) = self
            .repo_metadata_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&repo_key)
            .cloned()
            && self.now() - fetched_at < REPO_DEFAULT_BRANCH_TTL_MS
        {
            return Ok(data);
        }
        match client.repos_get(&repo.owner, &repo.repo).await {
            Ok(data) => {
                self.repo_metadata_cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(repo_key, (Some(data.clone()), self.now()));
                Ok(Some(data))
            }
            Err(error) => {
                GLOBAL_GATE.note_if_rate_limit_error(&error, self.now());
                if error.status == Some(403) || error.status == Some(404) {
                    self.repo_metadata_cache
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(repo_key, (None, self.now()));
                    return Ok(None);
                }
                Err(error)
            }
        }
    }

    /// `resolveRemoteCandidates`: resolve each ranked remote, dedup by repo.
/// 并发解析各 remote 指向的 GitHub 仓库，按输入顺序以 repo key 去重保留首个。
    pub async fn resolve_remote_candidates(
        &self,
        directory: &str,
        ranked_remote_names: &[String],
    ) -> Vec<(String, RepoRef)> {
        let resolved =
            futures::future::join_all(ranked_remote_names.iter().map(|remote_name| async move {
                let (repo, _) = resolve_github_repo_from_directory(directory, remote_name).await;
                (remote_name.clone(), repo)
            }))
            .await;

        let mut results = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for (remote_name, repo) in resolved {
            let Some(repo) = repo else { continue };
            let key = normalize_repo_key(&repo.owner, &repo.repo);
            if key.is_empty() || seen.contains(&key) {
                continue;
            }
            seen.insert(key);
            results.push((remote_name, repo));
        }
        results
    }

    /// `expandRepoNetwork`: candidate repos plus parent/source upstreams,
    /// deduped, priority-ordered.
/// 候选网络扩展：并发取元数据后补上 `parent`(+0.1)/`source`(+0.2) 上游，
/// 去重并按 priority 升序排序（越小越优先）。
    pub async fn expand_repo_network(
        &self,
        client: &GithubClient,
        candidates: &[Target],
    ) -> Result<Vec<Target>, GithubError> {
        let metadatas = futures::future::join_all(candidates.iter().map(|candidate| async move {
            let metadata = self.get_repo_metadata(client, &candidate.repo).await;
            (candidate, metadata)
        }))
        .await;

        let mut expanded: Vec<Target> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let push = |repo: RepoRef,
                    remote_name: &str,
                    priority: f64,
                    expanded: &mut Vec<Target>,
                    seen: &mut HashSet<String>| {
            let key = normalize_repo_key(&repo.owner, &repo.repo);
            if key.is_empty() || seen.contains(&key) {
                return;
            }
            seen.insert(key);
            expanded.push(Target {
                repo,
                remote_name: remote_name.to_string(),
                priority,
            });
        };

        for (candidate, metadata) in metadatas {
            let Ok(metadata) = metadata else { continue };
            let Some(metadata) = metadata else { continue };
            push(
                candidate.repo.clone(),
                &candidate.remote_name,
                candidate.priority,
                &mut expanded,
                &mut seen,
            );
            for (field, bump) in [("parent", 0.1), ("source", 0.2)] {
                if let Some(entry) = metadata.get(field) {
                    let login = entry.pointer("/owner/login").and_then(Value::as_str);
                    let name = entry.get("name").and_then(Value::as_str);
                    if let (Some(login), Some(name)) = (login, name)
                        && !login.is_empty()
                        && !name.is_empty()
                    {
                        let url = entry
                            .get("html_url")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("https://github.com/{login}/{name}"));
                        push(
                            RepoRef {
                                owner: login.to_string(),
                                repo: name.to_string(),
                                url,
                            },
                            &candidate.remote_name,
                            candidate.priority + bump,
                            &mut expanded,
                            &mut seen,
                        );
                    }
                }
            }
        }

        expanded.sort_by(|left, right| {
            left.priority
                .partial_cmp(&right.priority)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(expanded)
    }

    /// `getRepoPulls`: shared per-repo+state pull list with TTL and in-flight
    /// coalescing (concurrent branch resolutions share one GitHub call).
/// 读取（或拉取）`owner/repo::state` 的 PR 列表：TTL 缓存 + 在途合并，
/// 首页不足 100 条时标记 complete（此时未命中即权威）。
    pub async fn get_repo_pulls(
        &self,
        client: &GithubClient,
        repo: &RepoRef,
        state: &str,
        force: bool,
    ) -> Result<RepoPullsEntry, GithubError> {
        let key = format!("{}/{}::{state}", repo.owner.trim(), repo.repo.trim());
        let snapshot = |force: bool| -> Option<RepoPullsEntry> {
            let cache = self
                .repo_pulls_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = cache.get(&key)
                && (force || self.now() - entry.fetched_at < REPO_PULLS_CACHE_TTL_MS)
            {
                return Some(entry.clone());
            }
            None
        };
        if let Some(entry) = snapshot(force) {
            return Ok(entry);
        }

        let gate = {
            let mut gates = self.pulls_gates.lock().unwrap_or_else(|e| e.into_inner());
            gates
                .entry(key.clone())
                .or_insert_with(|| {
                    Arc::new(PullsGate {
                        lock: tokio::sync::Mutex::new(()),
                        completed: AtomicU64::new(0),
                    })
                })
                .clone()
        };
        let before = gate.completed.load(AtomicOrdering::Relaxed);
        let _guard = gate.lock.lock().await;
        // A fetch completed while we were waiting: share its result (this is
        // the JS in-flight-promise behavior, also for `force`).
        if gate.completed.load(AtomicOrdering::Relaxed) != before
            && let Some(entry) = snapshot(false)
        {
            return Ok(entry);
        }

        let prs = safe_list_pulls(self, client, &repo.owner, &repo.repo, state, None, 100).await?;
        let entry = RepoPullsEntry {
            fetched_at: self.now(),
            complete: prs.len() < 100,
            prs,
        };
        self.repo_pulls_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, entry.clone());
        gate.completed.fetch_add(1, AtomicOrdering::Relaxed);
        Ok(entry)
    }

/// 记录某 repo+branch 的历史 PR 结论（含“不存在”）：同 key 覆盖，超容量淘汰最旧。
    fn remember_historical_pr(&self, key: &str, pr: Option<Value>) {
        let mut cache = self
            .historical_pr_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        cache.retain(|(k, _, _)| k != key);
        cache.push((key.to_string(), pr, self.now()));
        if cache.len() > HISTORICAL_PR_CACHE_MAX_ENTRIES {
            cache.remove(0);
        }
    }

/// 查询未过期的历史 PR 缓存：命中记录 6 小时有效，“不存在”记录 10 分钟有效；
/// 未命中或已过期返回 `None`（外层会重新查询）。
    fn historical_pr_fresh(&self, key: &str) -> Option<Option<Value>> {
        let cache = self
            .historical_pr_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (_, pr, fetched_at) = cache.iter().find(|(k, _, _)| k == key)?;
        let ttl = match pr {
            Some(_) => HISTORICAL_PR_FOUND_TTL_MS,
            None => HISTORICAL_PR_ABSENT_TTL_MS,
        };
        if self.now() - fetched_at < ttl {
            Some(pr.clone())
        } else {
            None
        }
    }

    /// `invalidateRepoPullsCache`: drop the repo's pull lists, remembered
    /// search misses, and remembered history.
/// 清空指定仓库的 PR 列表缓存、Search 未命中记录与历史 PR 记忆，
/// 用于 PR 创建/关闭等变更后强制下次轮询重新拉取。
    pub fn invalidate_repo_pulls_cache(&self, owner: &str, repo: &str) {
        let prefix = format!("{}/{}::", owner.trim(), repo.trim());
        self.repo_pulls_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|key, _| !key.starts_with(&prefix));
        let repo_name_lower = repo.trim().to_lowercase();
        self.search_miss_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(key, _)| {
                let repo_part = key.split("::").next().unwrap_or("");
                !repo_part.split(',').any(|name| name == repo_name_lower)
            });
        let historical_prefix = format!("{}::", normalize_repo_key(owner, repo));
        self.historical_pr_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(key, _, _)| !key.starts_with(&historical_prefix));
    }

/// 记录一次 Search 未命中（repo 集合 + 分支）；同 key 覆盖，超容量淘汰最旧。
    fn remember_search_miss(&self, key: &str) {
        let mut cache = self
            .search_miss_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        cache.retain(|(k, _)| k != key);
        cache.push((key.to_string(), self.now()));
        if cache.len() > SEARCH_MISS_CACHE_MAX_ENTRIES {
            cache.remove(0);
        }
    }

    /// `parseRepoFromApiUrl`: `…/repos/{owner}/{repo}` → parts.
/// 从 `https://api.github.com/repos/{owner}/{repo}` 解析出 owner/repo；
/// URL 非法或路径形状不符返回 `None`。
    fn parse_repo_from_api_url(value: &str) -> Option<(String, String)> {
        let normalized = value.trim();
        if normalized.is_empty() {
            return None;
        }
        let url = url::Url::parse(normalized).ok()?;
        let parts: Vec<&str> = url
            .path()
            .trim_start_matches('/')
            .split('/')
            .filter(|p| !p.is_empty())
            .collect();
        if parts.len() < 2 || parts[0] != "repos" {
            return None;
        }
        let owner = parts[1];
        let repo = parts.get(2)?;
        if owner.is_empty() || repo.is_empty() {
            return None;
        }
        Some((owner.to_string(), repo.to_string()))
    }

    /// `searchFallbackPr`: Search-API fallback for a branch with no PR found
    /// in any repo list (only when coverage was not authoritative).
/// Search API 回退：按 `is:pr state:open head:{branch}` 搜索并逐个用
/// pulls.get 验证 head ref，要求结果落在候选仓库名集合内。
/// 403 时把该 repo 集合禁用 5 分钟；无结果时记录未命中 10 分钟。
    pub async fn search_fallback_pr(
        &self,
        client: &GithubClient,
        branch: &str,
        repo_names: &[String],
    ) -> Result<Option<(RepoRef, Value)>, GithubError> {
        let mut sorted_names = repo_names.to_vec();
        sorted_names.sort();
        let repo_key = sorted_names.join(",").to_lowercase();

        if let Some(disabled_at) = self
            .search_api_disabled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&repo_key)
            .copied()
            && self.now() - disabled_at < SEARCH_API_RETRY_MS
        {
            return Ok(None);
        }

        let miss_key = format!("{repo_key}::{}", branch.trim());
        if let Some(missed_at) = self
            .search_miss_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|(k, _)| *k == miss_key)
            .map(|(_, at)| *at)
            && self.now() - missed_at < SEARCH_MISS_RETRY_MS
        {
            return Ok(None);
        }

        let normalized_repo_names: HashSet<String> = repo_names
            .iter()
            .map(|name| name.trim().to_lowercase())
            .filter(|name| !name.is_empty())
            .collect();

        let response = match client
            .search_issues(&format!("is:pr state:open head:{branch}"), 20, 1)
            .await
        {
            Ok(response) => {
                self.search_api_disabled
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&repo_key);
                response
            }
            Err(error) => {
                GLOBAL_GATE.note_if_rate_limit_error(&error, self.now());
                if error.status == Some(403) {
                    self.search_api_disabled
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(repo_key, self.now());
                    return Ok(None);
                }
                if error.status == Some(404) {
                    self.remember_search_miss(&miss_key);
                    return Ok(None);
                }
                return Err(error);
            }
        };

        let items = response
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for item in items {
            let Some(repository_url) = item.get("repository_url").and_then(Value::as_str) else {
                continue;
            };
            let Some((owner, repo)) = Self::parse_repo_from_api_url(repository_url) else {
                continue;
            };
            if !normalized_repo_names.is_empty()
                && !normalized_repo_names.contains(&repo.trim().to_lowercase())
            {
                continue;
            }
            let number = item
                .get("number")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            match client.pulls_get(&owner, &repo, &number.to_string()).await {
                Ok(pr) => {
                    if normalize_text(pr.pointer("/head/ref").and_then(Value::as_str)) != branch {
                        continue;
                    }
                    return Ok(Some((
                        RepoRef {
                            url: format!("https://github.com/{owner}/{repo}"),
                            owner,
                            repo,
                        },
                        pr,
                    )));
                }
                Err(error) => {
                    if error.status == Some(403) || error.status == Some(404) {
                        continue;
                    }
                    return Err(error);
                }
            }
        }

        self.remember_search_miss(&miss_key);
        Ok(None)
    }

    /// `findBranchPrCandidates`: the open/historical pair for one target.
/// 对单个目标仓库解析 (open PR, 历史 PR)：先查共享 open 列表，未权威时再按
/// source owner 逐个精确查询 `owner:branch`；open 一旦命中立即返回，
/// 历史记录仅在 include_history（主关联）时查询并写入记忆缓存。
    pub async fn find_branch_pr_candidates(
        &self,
        client: &GithubClient,
        target: &Target,
        branch: &str,
        source_candidates: &[SourceCandidate],
        force: bool,
        coverage: Option<&mut bool>,
        include_history: bool,
    ) -> Result<(Option<Value>, Option<Value>), GithubError> {
        let matcher = SourceMatcher::build(source_candidates);
        let mut source_owners: Vec<String> = Vec::new();
        for candidate in source_candidates {
            push_unique_lower(&mut source_owners, &candidate.repo.owner);
        }

        let pick_preferred = |prs: &[Value]| -> Option<Value> {
            let mut matches: Vec<Value> = prs
                .iter()
                .filter(|pr| {
                    normalize_text(pr.pointer("/head/ref").and_then(Value::as_str)) == branch
                })
                .filter(|pr| matcher.matches(pr, &target.repo.repo))
                .cloned()
                .collect();
            matches.sort_by(|left, right| matcher.compare(left, right, &target.repo.repo));
            matches.into_iter().next()
        };

        // Shared repo-level open list.
        let mut open_list_was_complete = false;
        match self
            .get_repo_pulls(client, &target.repo, "open", force)
            .await
        {
            Ok(list_entry) => {
                if let Some(found) = pick_preferred(&list_entry.prs) {
                    return Ok((Some(found), None));
                }
                open_list_was_complete = list_entry.complete;
            }
            Err(_) => {
                // fall through to the precise per-head queries
            }
        }

        if !open_list_was_complete && let Some(coverage) = coverage {
            *coverage = false;
        }

        if open_list_was_complete && !include_history {
            return Ok((None, None));
        }

        let historical_key = format!(
            "{}::{branch}",
            normalize_repo_key(&target.repo.owner, &target.repo.repo)
        );
        if include_history
            && !force
            && open_list_was_complete
            && let Some(cached) = self.historical_pr_fresh(&historical_key)
        {
            return Ok((None, cached));
        }

        // One query per source owner; `state: all` answers both when history
        // is requested.
        let mut historical: Option<Value> = None;
        for owner in &source_owners {
            let direct_candidates = safe_list_pulls(
                self,
                client,
                &target.repo.owner,
                &target.repo.repo,
                if include_history { "all" } else { "open" },
                Some(&format!("{owner}:{branch}")),
                100,
            )
            .await?;
            let open_matches: Vec<Value> = direct_candidates
                .iter()
                .filter(|pr| !is_terminal_pr(pr))
                .cloned()
                .collect();
            if let Some(open_match) = pick_preferred(&open_matches) {
                return Ok((Some(open_match), None));
            }
            if include_history && historical.is_none() {
                let mut past: Vec<Value> = direct_candidates
                    .iter()
                    .filter(|pr| {
                        normalize_text(pr.pointer("/head/ref").and_then(Value::as_str)) == branch
                    })
                    .filter(|pr| matcher.matches(pr, &target.repo.repo))
                    .filter(|pr| is_terminal_pr(pr))
                    .cloned()
                    .collect();
                past.sort_by(|left, right| {
                    let left_number = left.get("number").and_then(Value::as_i64).unwrap_or(0);
                    let right_number = right.get("number").and_then(Value::as_i64).unwrap_or(0);
                    right_number.cmp(&left_number)
                });
                historical = past.into_iter().next();
            }
        }

        if include_history {
            self.remember_historical_pr(&historical_key, historical.clone());
        }
        Ok((None, historical))
    }

    /// `resolveGitHubPrStatus`.
/// 完整解析流程：目录存在性检查 → remote 排序 → 候选解析 → fork 网络扩展 →
/// 逐候选逐分支查询（跳过默认分支）→ 覆盖不权威时 Search 回退 →
/// open 优先、历史次之，最后兜底返回首个候选仓库。
    pub async fn resolve_github_pr_status(
        &self,
        client: &GithubClient,
        directory: &str,
        branch: &str,
        remote_name: &str,
        force: bool,
    ) -> Result<ResolvedStatus, GithubError> {
        // Deleted worktrees still get polled by sidebar sessions — bail before
        // touching git or GitHub.
        if directory.is_empty() || !tokio::fs::metadata(directory).await.is_ok() {
            return Ok(ResolvedStatus::empty());
        }

        let normalized_branch = branch.trim().to_string();
        let normalized_remote_name = if remote_name.trim().is_empty() {
            "origin".to_string()
        } else {
            remote_name.trim().to_string()
        };

        let (tracking, remote_names) = futures::future::join(
            git_ops::get_tracking_branch(directory),
            git_ops::get_remote_names(directory),
        )
        .await;

        let tracking_remote_name = parse_tracking_remote_name(tracking.as_deref());
        let tracking_branch_name = parse_tracking_branch_name(tracking.as_deref());
        let mut branch_candidates: Vec<String> = Vec::new();
        push_unique_lower(&mut branch_candidates, &normalized_branch);
        push_unique_lower(&mut branch_candidates, &tracking_branch_name);
        let ranked_remote_names = rank_remote_names(
            &remote_names,
            &normalized_remote_name,
            &tracking_remote_name,
        );

        let resolved_remote_targets = self
            .resolve_remote_candidates(directory, &ranked_remote_names)
            .await;
        let candidates: Vec<Target> = resolved_remote_targets
            .into_iter()
            .enumerate()
            .map(|(index, (remote_name, repo))| Target {
                repo,
                remote_name,
                priority: index as f64,
            })
            .collect();
        let resolved_targets = self.expand_repo_network(client, &candidates).await?;
        if resolved_targets.is_empty() {
            return Ok(ResolvedStatus::empty());
        }

        // Only the ranked-first remote's network can be the source of the
        // branch's PRs.
        let primary_remote_name = resolved_targets[0].remote_name.clone();
        let source_candidates: Vec<SourceCandidate> = resolved_targets
            .iter()
            .filter(|target| target.remote_name == primary_remote_name)
            .map(|target| SourceCandidate {
                repo: target.repo.clone(),
            })
            .collect();
        let mut coverage_authoritative = true;

        let mut fallback_repo = resolved_targets[0].repo.clone();
        let mut fallback_remote_name = resolved_targets[0].remote_name.clone();
        let mut fallback_default_branch =
            self.get_repo_default_branch(client, &fallback_repo).await;

        let mut historical_match: Option<ResolvedStatus> = None;

        for (target_index, target) in resolved_targets.iter().enumerate() {
            let default_branch = self.get_repo_default_branch(client, &target.repo).await;
            if fallback_repo.owner.is_empty() && fallback_repo.repo.is_empty() {
                fallback_repo = target.repo.clone();
                fallback_remote_name = target.remote_name.clone();
                fallback_default_branch = default_branch.clone();
            }

            let has_cross_repo_source = source_candidates.iter().any(|candidate| {
                normalize_repo_key(&candidate.repo.owner, &candidate.repo.repo)
                    != normalize_repo_key(&target.repo.owner, &target.repo.repo)
            });
            for (candidate_index, candidate_branch) in branch_candidates.iter().enumerate() {
                if default_branch.as_deref() == Some(candidate_branch.as_str())
                    && !has_cross_repo_source
                {
                    continue;
                }
                let is_primary_association = target_index == 0 && candidate_index == 0;

                let (open, historical) = self
                    .find_branch_pr_candidates(
                        client,
                        target,
                        candidate_branch,
                        &source_candidates,
                        force,
                        Some(&mut coverage_authoritative),
                        is_primary_association,
                    )
                    .await?;
                if let Some(open) = open {
                    return Ok(ResolvedStatus {
                        repo: Some(target.repo.clone()),
                        pr: Some(open),
                        default_branch: default_branch.clone(),
                        resolved_remote_name: Some(target.remote_name.clone()),
                    });
                }
                if let Some(historical) = historical
                    && historical_match.is_none()
                {
                    historical_match = Some(ResolvedStatus {
                        repo: Some(target.repo.clone()),
                        pr: Some(historical),
                        default_branch: default_branch.clone(),
                        resolved_remote_name: Some(target.remote_name.clone()),
                    });
                }
            }
        }

        for candidate_branch in &branch_candidates {
            if coverage_authoritative {
                break;
            }
            let repo_names: Vec<String> = resolved_targets
                .iter()
                .map(|target| target.repo.repo.clone())
                .collect();
            if let Some((repo, pr)) = self
                .search_fallback_pr(client, candidate_branch, &repo_names)
                .await?
            {
                let default_branch = self.get_repo_default_branch(client, &repo).await;
                return Ok(ResolvedStatus {
                    repo: Some(repo),
                    pr: Some(pr),
                    default_branch,
                    resolved_remote_name: None,
                });
            }
        }

        if let Some(historical_match) = historical_match {
            return Ok(historical_match);
        }

        Ok(ResolvedStatus {
            repo: Some(fallback_repo),
            pr: None,
            default_branch: fallback_default_branch,
            resolved_remote_name: Some(fallback_remote_name),
        })
    }
}

/// Default 转发到 `new`。
impl Default for PrStatusEngine {
/// 与 `PrStatusEngine::new` 等价。
    fn default() -> Self {
        Self::new()
    }
}

/// PR 状态解析引擎的单元测试（fake transport + 可推进的假时钟）。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::client::{GithubRequest, GithubResponse};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

/// 把 PR 数组包成 pulls.list 的 JSON 响应体。
    fn json_pulls(prs: Vec<Value>) -> Value {
        Value::Array(prs)
    }

/// 构造 200 + JSON content-type 的响应。
    fn json_ok(body: Value) -> GithubResponse {
        GithubResponse {
            status: 200,
            headers: vec![(
                "content-type".to_string(),
                "application/json; charset=utf-8".to_string(),
            )],
            body: serde_json::to_vec(&body).unwrap(),
        }
    }

/// 造一条 open 状态、head 为 `acme:feature` 的 PR。
    fn open_pr() -> Value {
        serde_json::json!({
            "number": 15,
            "state": "open",
            "head": {
                "ref": "feature",
                "label": "acme:feature",
                "user": { "login": "acme" },
                "repo": { "owner": { "login": "acme" }, "name": "app" },
            },
        })
    }

/// 造一条已合并（state=closed 且 merged_at 非空）的 PR，编号可指定。
    fn merged_pr(number: i64) -> Value {
        serde_json::json!({
            "number": number,
            "state": "closed",
            "merged_at": "2026-01-01T00:00:00Z",
            "head": {
                "ref": "feature",
                "label": "acme:feature",
                "user": { "login": "acme" },
                "repo": { "owner": { "login": "acme" }, "name": "app" },
            },
        })
    }

    /// Fake transport answering pulls.list by (state, head presence).
/// 构造按 (state, 是否带 head 参数) 应答 pulls.list 的 fake 客户端；
/// 返回值附带请求计数器供断言调用次数。
    fn pulls_client(
        state_to_prs: impl Fn(&str, bool) -> Vec<Value> + Send + Sync + 'static,
    ) -> (GithubClient, Arc<AtomicI64>) {
        let calls = Arc::new(AtomicI64::new(0));
        let calls_for_transport = calls.clone();
        let responder = Arc::new(state_to_prs);
        let client = GithubClient::with_transport(Arc::new(move |req: GithubRequest| {
            let calls = calls_for_transport.clone();
            let responder = responder.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let url = reqwest::Url::parse(&req.url).unwrap();
                let query: HashMap<String, String> = url
                    .query_pairs()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                let path = url.path().to_string();
                let state = query.get("state").cloned().unwrap_or_default();
                let head = query.get("head").cloned();
                if path.ends_with("/pulls") {
                    let prs = responder(&state, head.is_some());
                    return Ok(json_ok(json_pulls(prs)));
                }
                Err(GithubError {
                    status: Some(500),
                    message: format!("unexpected url {}", req.url),
                    headers: Vec::new(),
                    data: None,
                })
            })
        }));
        (client, calls)
    }

/// 标准查询目标：acme/app @ origin。
    fn target() -> Target {
        Target {
            repo: RepoRef {
                owner: "acme".to_string(),
                repo: "app".to_string(),
                url: "https://github.com/acme/app".to_string(),
            },
            remote_name: "origin".to_string(),
            priority: 0.0,
        }
    }

/// 标准来源候选：acme/app。
    fn source_candidates() -> Vec<SourceCandidate> {
        vec![SourceCandidate {
            repo: RepoRef {
                owner: "acme".to_string(),
                repo: "app".to_string(),
                url: "https://github.com/acme/app".to_string(),
            },
        }]
    }

/// 以标准 target/候选调用 find_branch_pr_candidates 的测试捷径。
    async fn call(
        engine: &PrStatusEngine,
        client: &GithubClient,
        force: bool,
        include_history: bool,
    ) -> (Option<Value>, Option<Value>) {
        engine
            .find_branch_pr_candidates(
                client,
                &target(),
                "feature",
                &source_candidates(),
                force,
                None,
                include_history,
            )
            .await
            .unwrap()
    }

/// 构造带可控假时钟（AtomicI64）的新引擎；测试推进时间即可穿越 TTL。
    fn fresh_engine() -> (PrStatusEngine, Arc<AtomicI64>) {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let clock = {
            let now = now.clone();
            Box::new(move || now.load(Ordering::SeqCst))
        };
        (PrStatusEngine::with_clock(clock), now)
    }

/// 验证：open PR 命中即返回，不再额外查询历史。
    #[tokio::test]
    async fn open_pr_wins_and_no_history_lookup_is_spent() {
        let (engine, _) = fresh_engine();
        let (client, calls) = pulls_client(|state, _| {
            if state == "open" {
                vec![open_pr()]
            } else {
                vec![merged_pr(12)]
            }
        });
        let (open, historical) = call(&engine, &client, true, true).await;
        assert_eq!(open.unwrap()["number"], 15);
        assert!(historical.is_none());
        // Every call asked for open state.
        let observed = calls.load(Ordering::SeqCst);
        assert!(observed >= 1);
    }

/// 验证：共享 open 列表（100 条封顶）漏掉目标时，按 head 的精确查询仍能找回 open PR。
    #[tokio::test]
    async fn open_pr_still_wins_when_shared_open_list_missed_it() {
        let (engine, _) = fresh_engine();
        let (client, _) = pulls_client(|_state, has_head| {
            if has_head {
                vec![merged_pr(12), open_pr()]
            } else {
                // 100 other open PRs: the shared list is incomplete.
                (0..100)
                    .map(|i| serde_json::json!({ "number": i, "state": "open", "head": { "ref": "other" } }))
                    .collect()
            }
        });
        let (open, historical) = call(&engine, &client, true, true).await;
        assert_eq!(open.unwrap()["number"], 15);
        assert!(historical.is_none());
    }

/// 验证：无 open PR 时返回该分支最近一条已合并的历史 PR。
    #[tokio::test]
    async fn returns_branch_history_when_no_open_pr_exists() {
        let (engine, _) = fresh_engine();
        let (client, _) = pulls_client(|_state, has_head| {
            if has_head {
                vec![merged_pr(7), merged_pr(12)]
            } else {
                Vec::new()
            }
        });
        let (open, historical) = call(&engine, &client, true, true).await;
        assert!(open.is_none());
        // The newest past PR for the head is the relevant record.
        assert_eq!(historical.unwrap()["number"], 12);
    }

/// 验证：分支从未有过 PR 时 open 与历史均为空，且历史查询使用 state=all。
    #[tokio::test]
    async fn no_history_for_branch_that_never_had_a_pr() {
        let (engine, _) = fresh_engine();
        let (client, calls) = pulls_client(|_, _| Vec::new());
        let (open, historical) = call(&engine, &client, true, true).await;
        assert!(open.is_none());
        assert!(historical.is_none());
        // History-enabled per-head queries use state=all.
        let transport_state_all_seen = calls.load(Ordering::SeqCst) >= 2;
        assert!(transport_state_all_seen);
    }

/// 验证：次级关联（不查历史）在共享 open 列表已权威时只花一次调用。
    #[tokio::test]
    async fn spends_no_call_on_history_for_secondary_target() {
        let (engine, _) = fresh_engine();
        let (client, calls) = pulls_client(|_state, has_head| {
            if has_head {
                vec![merged_pr(12)]
            } else {
                Vec::new()
            }
        });
        let (open, historical) = call(&engine, &client, true, false).await;
        assert!(open.is_none());
        assert!(historical.is_none());
        // The complete open list already answered the only question.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

/// 验证：轮询复用历史缓存，不重复发起 GitHub 查询。
    #[tokio::test]
    async fn reuses_cached_history_instead_of_requerying_every_poll() {
        let (engine, _) = fresh_engine();
        let (client, calls) = pulls_client(|_state, has_head| {
            if has_head {
                vec![merged_pr(12)]
            } else {
                Vec::new()
            }
        });
        call(&engine, &client, true, true).await;
        let calls_after_first = calls.load(Ordering::SeqCst);

        let (open, historical) = call(&engine, &client, false, true).await;
        assert!(open.is_none());
        assert_eq!(historical.unwrap()["number"], 12);
        assert_eq!(calls.load(Ordering::SeqCst), calls_after_first);
    }

/// 验证：命中记录的 6 小时 TTL 长于“不存在”的 10 分钟，短暂过期后仍可从缓存读到。
    #[tokio::test]
    async fn found_record_outlives_the_shorter_no_history_window() {
        let (engine, now) = fresh_engine();
        let (client, calls) = pulls_client(|_state, has_head| {
            if has_head {
                vec![merged_pr(12)]
            } else {
                Vec::new()
            }
        });
        call(&engine, &client, true, true).await;
        let calls_after_first = calls.load(Ordering::SeqCst);

        // Past the absent-window expiry, far short of the found-record one.
        now.store(1_000_000 + 30 * 60 * 1000, Ordering::SeqCst);
        let (open, historical) = call(&engine, &client, false, true).await;
        assert!(open.is_none());
        assert_eq!(historical.unwrap()["number"], 12);
        // Only the shared open list was re-fetched (TTL had expired).
        assert_eq!(calls.load(Ordering::SeqCst), calls_after_first + 1);
    }

/// 验证：“不存在”记录的 10 分钟窗口过期后会重新查询该分支。
    #[tokio::test]
    async fn requeries_branch_with_no_history_once_shorter_window_passes() {
        let (engine, now) = fresh_engine();
        let (client, calls) = pulls_client(|_, _| Vec::new());
        call(&engine, &client, true, true).await;
        let calls_after_first = calls.load(Ordering::SeqCst);

        now.store(1_000_000 + 30 * 60 * 1000, Ordering::SeqCst);
        call(&engine, &client, false, true).await;
        assert!(calls.load(Ordering::SeqCst) > calls_after_first + 1);
    }

/// 验证：tracking 引用的 remote/分支名解析与 JS 版边界行为一致。
    #[test]
    fn tracking_parsers_match_js() {
        assert_eq!(parse_tracking_remote_name(Some(" origin/main ")), "origin");
        assert_eq!(parse_tracking_remote_name(Some("main")), "");
        assert_eq!(parse_tracking_remote_name(None), "");
        assert_eq!(parse_tracking_branch_name(Some("origin/feat/x")), "feat/x");
        assert_eq!(parse_tracking_branch_name(Some("origin/")), "");
        assert_eq!(parse_tracking_branch_name(Some("origin")), "");
    }

/// 验证：remote 排序为显式 > 跟踪 > origin > upstream > 其余。
    #[test]
    fn rank_remote_names_orders_explicit_tracking_origin_upstream_then_rest() {
        let remotes = vec!["fork".to_string(), "origin".to_string()];
        let ranked = rank_remote_names(&remotes, "upstream", "fork");
        assert_eq!(ranked, vec!["upstream", "fork", "origin"]);
    }

/// 验证：API URL 的 repos 路径解析成功与非法形状的拒绝。
    #[test]
    fn parse_repo_from_api_url_shapes() {
        assert_eq!(
            PrStatusEngine::parse_repo_from_api_url("https://api.github.com/repos/acme/app"),
            Some(("acme".to_string(), "app".to_string()))
        );
        assert_eq!(
            PrStatusEngine::parse_repo_from_api_url("https://api.github.com/users/acme"),
            None
        );
        assert_eq!(PrStatusEngine::parse_repo_from_api_url(""), None);
    }
}
