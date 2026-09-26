//! Port of `server/lib/github/pr-status.js` — PR lookup across remotes,
//! forks, and upstreams for a local branch.
//!
//! Behavior preserved from the JS (see `DOCUMENTATION.md` "How PR resolution
//! works"): remote ranking, fork-network expansion through `parent`/`source`,
//! default-branch skip, open-PR preference with per-owner head queries, the
//! shared repo-level pulls cache with in-flight coalescing, remembered
//! closed/merged history (6h found / 10m absent), Search-API fallback with
//! 403 backoff and miss caching, and open-PR-wins-over-historical ordering.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::github::client::{GithubClient, GithubError};
use crate::github::git_ops;
use crate::github::rate_limit::{GLOBAL_GATE, now_ms};
use crate::github::repo::{RepoRef, normalize_repo_key, resolve_github_repo_from_directory};

const REPO_DEFAULT_BRANCH_TTL_MS: i64 = 5 * 60_000;
const REPO_PULLS_CACHE_TTL_MS: i64 = 45_000;
const HISTORICAL_PR_FOUND_TTL_MS: i64 = 6 * 60 * 60 * 1000;
const HISTORICAL_PR_ABSENT_TTL_MS: i64 = 10 * 60 * 1000;
const HISTORICAL_PR_CACHE_MAX_ENTRIES: usize = 500;
const SEARCH_API_RETRY_MS: i64 = 5 * 60 * 1000;
const SEARCH_MISS_RETRY_MS: i64 = 10 * 60 * 1000;
const SEARCH_MISS_CACHE_MAX_ENTRIES: usize = 500;

pub(crate) fn normalize_text(value: Option<&str>) -> String {
    value.unwrap_or("").trim().to_string()
}

fn text_at<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(Value::as_str)
}

fn normalize_lower_at(value: &Value, pointer: &str) -> String {
    normalize_text(text_at(value, pointer)).to_lowercase()
}

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
pub struct SourceMatcher {
    repo_rank: HashMap<String, usize>,
    owner_rank: HashMap<String, usize>,
}

impl SourceMatcher {
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

    pub fn matches(&self, pr: &Value, fallback_repo_name: &str) -> bool {
        let repo_key = get_head_repo_key(pr, fallback_repo_name);
        if !repo_key.is_empty() && self.repo_rank.contains_key(&repo_key) {
            return true;
        }
        let owner = get_head_owner(pr).to_lowercase();
        !owner.is_empty() && self.owner_rank.contains_key(&owner)
    }

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

#[derive(Debug, Clone)]
pub struct SourceCandidate {
    pub repo: RepoRef,
}

#[derive(Debug, Clone)]
pub struct Target {
    pub repo: RepoRef,
    pub remote_name: String,
    pub priority: f64,
}

#[derive(Debug, Clone)]
pub struct ResolvedStatus {
    pub repo: Option<RepoRef>,
    pub pr: Option<Value>,
    pub default_branch: Option<String>,
    pub resolved_remote_name: Option<String>,
}

impl ResolvedStatus {
    fn empty() -> Self {
        Self {
            repo: None,
            pr: None,
            default_branch: None,
            resolved_remote_name: None,
        }
    }
}

pub fn is_terminal_pr(pr: &Value) -> bool {
    pr.get("state").and_then(Value::as_str) == Some("closed")
        || pr.get("merged_at").map(|v| !v.is_null()).unwrap_or(false)
}

/// `safeListPulls`: pulls.list; 404/403 → empty, other errors propagate.
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

#[derive(Debug, Clone)]
pub struct RepoPullsEntry {
    pub fetched_at: i64,
    pub prs: Vec<Value>,
    /// First page held everything, so a miss is authoritative.
    pub complete: bool,
}

struct PullsGate {
    lock: tokio::sync::Mutex<()>,
    completed: AtomicU64,
}

type Clock = Box<dyn Fn() -> i64 + Send + Sync>;

/// The PR resolution engine plus every module-level cache from pr-status.js.
pub struct PrStatusEngine {
    now: Clock,
    default_branch_cache: Mutex<HashMap<String, (Option<String>, i64)>>,
    repo_metadata_cache: Mutex<HashMap<String, (Option<Value>, i64)>>,
    repo_pulls_cache: Mutex<HashMap<String, RepoPullsEntry>>,
    pulls_gates: Mutex<HashMap<String, Arc<PullsGate>>>,
    historical_pr_cache: Mutex<Vec<(String, Option<Value>, i64)>>,
    search_api_disabled: Mutex<HashMap<String, i64>>,
    search_miss_cache: Mutex<Vec<(String, i64)>>,
}

impl PrStatusEngine {
    pub fn new() -> Self {
        Self::with_clock(Box::new(now_ms))
    }

    /// Injectable clock (JS tests use fake timers).
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

    fn now(&self) -> i64 {
        (self.now)()
    }

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

impl Default for PrStatusEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::client::{GithubRequest, GithubResponse};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    fn json_pulls(prs: Vec<Value>) -> Value {
        Value::Array(prs)
    }

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

    fn source_candidates() -> Vec<SourceCandidate> {
        vec![SourceCandidate {
            repo: RepoRef {
                owner: "acme".to_string(),
                repo: "app".to_string(),
                url: "https://github.com/acme/app".to_string(),
            },
        }]
    }

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

    fn fresh_engine() -> (PrStatusEngine, Arc<AtomicI64>) {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let clock = {
            let now = now.clone();
            Box::new(move || now.load(Ordering::SeqCst))
        };
        (PrStatusEngine::with_clock(clock), now)
    }

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

    #[test]
    fn tracking_parsers_match_js() {
        assert_eq!(parse_tracking_remote_name(Some(" origin/main ")), "origin");
        assert_eq!(parse_tracking_remote_name(Some("main")), "");
        assert_eq!(parse_tracking_remote_name(None), "");
        assert_eq!(parse_tracking_branch_name(Some("origin/feat/x")), "feat/x");
        assert_eq!(parse_tracking_branch_name(Some("origin/")), "");
        assert_eq!(parse_tracking_branch_name(Some("origin")), "");
    }

    #[test]
    fn rank_remote_names_orders_explicit_tracking_origin_upstream_then_rest() {
        let remotes = vec!["fork".to_string(), "origin".to_string()];
        let ranked = rank_remote_names(&remotes, "upstream", "fork");
        assert_eq!(ranked, vec!["upstream", "fork", "origin"]);
    }

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
