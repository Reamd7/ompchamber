//! Port of `server/lib/github/repo/index.js` + `repo/fork-detection.js`:
//! remote URL parsing, directory→repo resolution, and the repo network
//! (origin + parent/source upstream) with its own 5-minute metadata cache.
//!
//! 中文说明：GitHub 仓库识别与 fork 网络解析，包含三块能力：remote
//! URL 到 owner/repo 的解析（parse_github_remote_url）、从本地目录解析
//! 仓库（resolve_github_repo_from_directory）、解析仓库网络（origin 加
//! parent/source 上游，RepoNetworkCache::resolve_repo_network）。仓库
//! 元数据带 5 分钟 TTL 缓存，上限 200 条，满了按最早抓取时间淘汰。

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use serde_json::Value;

use crate::github::client::{GithubClient, GithubError};
use crate::github::git_ops;
use crate::github::rate_limit::now_ms;

/// `REPO_METADATA_TTL_MS` / `REPO_METADATA_CACHE_MAX_ENTRIES`.
/// 仓库元数据缓存的有效期：5 分钟（毫秒）。
const REPO_METADATA_TTL_MS: i64 = 5 * 60_000;
/// 元数据缓存的条目上限：达到上限后按最早的 fetchedAt 淘汰。
const REPO_METADATA_CACHE_MAX_ENTRIES: usize = 200;

/// 从 remote URL 解析出的 GitHub 仓库引用：owner/repo 加规范化 web URL。
#[derive(Debug, Clone, PartialEq)]
pub struct RepoRef {
    /// 仓库所有者（用户或组织登录名）。
    pub owner: String,
    /// 仓库名（已去掉 .git 后缀）。
    pub repo: String,
    /// 规范化的 https://github.com/<owner>/<repo> 页面地址。
    pub url: String,
}

/// RepoRef 的 JSON 序列化辅助。
impl RepoRef {
    /// 序列化为 {owner, repo, url} 的 JSON 对象，供 API 响应直接复用。
    pub fn to_json(&self) -> Value {
        serde_json::json!({ "owner": self.owner, "repo": self.repo, "url": self.url })
    }
}

/// `parseGitHubRemoteUrl`: `git@github.com:`, `ssh://git@github.com/`, and
/// `https://github.com/` shapes (with optional `.git` suffix).
///
/// 支持 git@github.com:<owner>/<repo>、ssh://git@github.com/<owner>/<repo>、
/// https://github.com/<owner>/<repo> 三种形态，.git 后缀可有可无。输入
/// 先 trim；非 github.com 域名、路径不足两段或无法解析时返回 None；与
/// JS 版一致，多余路径段被忽略（只取前两段）。
pub fn parse_github_remote_url(raw: &str) -> Option<RepoRef> {
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }

    let from_owner_repo = |rest: &str| -> Option<RepoRef> {
        let cleaned = rest.strip_suffix(".git").unwrap_or(rest);
        let mut parts = cleaned.split('/');
        let owner = parts.next().unwrap_or("");
        let repo = parts.next().unwrap_or("");
        if owner.is_empty() || repo.is_empty() {
            return None;
        }
        Some(RepoRef {
            owner: owner.to_string(),
            repo: repo.to_string(),
            url: format!("https://github.com/{owner}/{repo}"),
        })
    };

    if let Some(rest) = value.strip_prefix("git@github.com:") {
        return from_owner_repo(rest);
    }
    if let Some(rest) = value.strip_prefix("ssh://git@github.com/") {
        return from_owner_repo(rest);
    }

    let url = match url::Url::parse(value) {
        Ok(url) => url,
        Err(_) => return None,
    };
    if url.host_str() != Some("github.com") {
        return None;
    }
    let path = url.path().trim_matches('/');
    from_owner_repo(path)
}

/// `resolveGitHubRepoFromDirectory(directory, remoteName)`: `{ repo, remoteUrl }`.
///
/// 对应 JS 版 resolveGitHubRepoFromDirectory：经 git_ops 读取指定
/// remote 的 URL，再解析出 RepoRef，返回 (repo, remoteUrl) 二元组。
/// 读取 remote 失败时两者均为 None；URL 存在但不是可识别的 GitHub
/// 仓库时 repo 为 None 而 remoteUrl 仍有值。
pub async fn resolve_github_repo_from_directory(
    directory: &str,
    remote_name: &str,
) -> (Option<RepoRef>, Option<String>) {
    let remote_url = git_ops::get_remote_url(directory, remote_name).await;
    match remote_url {
        None => (None, None),
        Some(url) => (parse_github_remote_url(&url), Some(url)),
    }
}

/// One entry of `resolveRepoNetwork`'s result.
///
/// 仓库网络中的单个仓库：owner/repo/url 加它在网络中的角色（source）。
#[derive(Debug, Clone)]
pub struct NetworkRepo {
    /// 仓库所有者。
    pub owner: String,
    /// 仓库名。
    pub repo: String,
    /// 仓库页面 URL。
    pub url: String,
    /// `'origin'` or `'upstream'`.
    /// 'origin'（本地 remote 指向的仓库）或 'upstream'（fork 的上游）。
    pub source: &'static str,
}

/// fork-detection.js's module-local metadata cache.
///
/// fork-detection.js 的模块级元数据缓存：key 为小写 owner/repo，值为
/// (元数据 JSON；403/404 负缓存为 None, 抓取时刻的 Unix 毫秒)。
#[derive(Default)]
pub struct RepoNetworkCache {
    /// key → (repo 元数据，None 表示已知的 403/404；抓取时刻)。
    entries: Mutex<HashMap<String, (Option<Value>, i64)>>,
}

/// 把 fork 网络的一个上游仓库（parent 或 source 元数据）追加进结果：
/// 要求 owner.login 与 name 均存在且非空，按 normalize_repo_key 去重
/// （同一仓库只保留首个），url 优先取元数据的 html_url、缺失时回退拼
/// 接 github.com 地址，source 固定为 upstream。
fn push_upstream(entry: &Value, result: &mut Vec<NetworkRepo>, seen: &mut HashSet<String>) {
    let login = entry.pointer("/owner/login").and_then(Value::as_str);
    let name = entry.get("name").and_then(Value::as_str);
    let (login, name) = match (login, name) {
        (Some(login), Some(name)) if !login.is_empty() && !name.is_empty() => (login, name),
        _ => return,
    };
    let key = normalize_repo_key(login, name);
    if seen.contains(&key) {
        return;
    }
    seen.insert(key);
    result.push(NetworkRepo {
        owner: login.to_string(),
        repo: name.to_string(),
        url: entry
            .get("html_url")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("https://github.com/{login}/{name}")),
        source: "upstream",
    });
}

/// 元数据缓存的读写与仓库网络解析入口。
impl RepoNetworkCache {
    /// 构造空缓存。
    pub fn new() -> Self {
        Self::default()
    }

    /// 写入一条缓存（data 为 None 表示已知 403/404 的负缓存）；条目数
    /// 已达上限且 key 不存在时，先淘汰 fetchedAt 最早的一条再插入。
    fn set(&self, key: String, data: Option<Value>) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if entries.len() >= REPO_METADATA_CACHE_MAX_ENTRIES && !entries.contains_key(&key) {
            // Evict the oldest entry (lowest fetchedAt).
            if let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, (_, fetched_at))| *fetched_at)
                .map(|(k, _)| k.clone())
            {
                entries.remove(&oldest);
            }
        }
        entries.insert(key, (data, now_ms()));
    }

    /// `getRepoMetadata`: cached within TTL, else `repos.get`; 403/404 cache
    /// as null, other errors propagate.
    ///
    /// 对应 JS 版 getRepoMetadata：TTL 内命中缓存直接返回（包括负缓存
    /// None）；否则调用 repos_get 拉取。归一化 key 为空串时直接返回
    /// None；403/404 将 None 写入缓存并返回 Ok(None)，其余错误原样上抛。
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
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&repo_key)
            .cloned()
            && now_ms() - fetched_at < REPO_METADATA_TTL_MS
        {
            return Ok(data);
        }
        match client.repos_get(&repo.owner, &repo.repo).await {
            Ok(data) => {
                self.set(repo_key, Some(data.clone()));
                Ok(Some(data))
            }
            Err(error) => {
                if error.status == Some(403) || error.status == Some(404) {
                    self.set(repo_key, None);
                    return Ok(None);
                }
                Err(error)
            }
        }
    }

    /// `resolveRepoNetwork(octokit, directory, remoteName)`: origin first,
    /// then the parent/source upstream repo. `None` when the remote does not
    /// resolve or the repo is not a fork.
    ///
    /// 对应 JS 版 resolveRepoNetwork：先解析出 origin 仓库，再取元数据
    /// 并依次追加 parent 与 source 上游（去重）。返回 Ok(None) 表示
    /// remote 解析失败或仓库不是 fork（结果只有 origin）；元数据为
    /// None（403/404）时退化为仅含 origin 的列表。
    pub async fn resolve_repo_network(
        &self,
        client: &GithubClient,
        directory: &str,
        remote_name: &str,
    ) -> Result<Option<Vec<NetworkRepo>>, GithubError> {
        let (repo, _) = resolve_github_repo_from_directory(directory, remote_name).await;
        let Some(repo) = repo else {
            return Ok(None);
        };

        let metadata = self.get_repo_metadata(client, &repo).await?;
        let Some(metadata) = metadata else {
            return Ok(Some(vec![NetworkRepo {
                owner: repo.owner,
                repo: repo.repo,
                url: repo.url,
                source: "origin",
            }]));
        };

        let mut result = vec![NetworkRepo {
            owner: repo.owner.clone(),
            repo: repo.repo.clone(),
            url: repo.url.clone(),
            source: "origin",
        }];
        let mut seen_keys = HashSet::from([normalize_repo_key(&repo.owner, &repo.repo)]);

        if let Some(parent) = metadata.get("parent") {
            push_upstream(parent, &mut result, &mut seen_keys);
        }
        if let Some(source) = metadata.get("source") {
            push_upstream(source, &mut result, &mut seen_keys);
        }

        if result.len() == 1 {
            return Ok(None);
        }
        Ok(Some(result))
    }
}

/// 仓库归一化缓存 key：owner/repo 分别 trim + 转小写后以 / 拼接；任一
/// 部分为空返回空串（调用方以此跳过缓存与上游追加）。
pub fn normalize_repo_key(owner: &str, repo: &str) -> String {
    let owner = owner.trim().to_lowercase();
    let repo = repo.trim().to_lowercase();
    if owner.is_empty() || repo.is_empty() {
        return String::new();
    }
    format!("{owner}/{repo}")
}

/// 验证 remote URL 解析规则与缓存 key 的归一化行为。
#[cfg(test)]
mod tests {
    use super::*;

    /// 验证 git@github.com:owner/repo.git（scp 风格）URL 的解析。
    #[test]
    fn parses_ssh_colon_urls() {
        let parsed = parse_github_remote_url("git@github.com:acme/app.git").unwrap();
        assert_eq!(parsed.owner, "acme");
        assert_eq!(parsed.repo, "app");
        assert_eq!(parsed.url, "https://github.com/acme/app");
    }

    /// 验证 ssh:// scheme URL 的解析。
    #[test]
    fn parses_ssh_scheme_urls() {
        let parsed = parse_github_remote_url("ssh://git@github.com/acme/app.git").unwrap();
        assert_eq!(parsed.owner, "acme");
        assert_eq!(parsed.repo, "app");
    }

    /// 验证 https URL 带/不带 .git 后缀解析出同一 owner/repo/URL。
    #[test]
    fn parses_https_urls_with_and_without_git_suffix() {
        let parsed = parse_github_remote_url("https://github.com/acme/app").unwrap();
        assert_eq!(parsed.repo, "app");
        let parsed = parse_github_remote_url("https://github.com/acme/app.git").unwrap();
        assert_eq!(parsed.repo, "app");
        assert_eq!(parsed.url, "https://github.com/acme/app");
    }

    /// 验证非 GitHub 域名、空串、乱码与缺 owner/repo 段的输入返回 None。
    #[test]
    fn rejects_non_github_and_malformed_urls() {
        assert!(parse_github_remote_url("https://gitlab.com/acme/app").is_none());
        assert!(parse_github_remote_url("git@gitlab.com:acme/app").is_none());
        assert!(parse_github_remote_url("").is_none());
        assert!(parse_github_remote_url("not a url").is_none());
        assert!(parse_github_remote_url("https://github.com/onlyowner").is_none());
        assert!(parse_github_remote_url("git@github.com:onlyowner").is_none());
    }

    /// 验证输入先 trim 且多余路径段被忽略（对齐 JS 的 split('/') 行为）。
    #[test]
    fn trims_input_and_tolerates_extra_path_segments_like_js_split() {
        // JS `cleaned.split('/')` takes the first two parts and ignores extras.
        let parsed = parse_github_remote_url("  https://github.com/acme/app/extra  ").unwrap();
        assert_eq!(parsed.owner, "acme");
        assert_eq!(parsed.repo, "app");
    }

    /// 验证缓存 key 小写归一化，且缺任一部分时返回空串。
    #[test]
    fn normalize_repo_key_lowercases_and_requires_both_parts() {
        assert_eq!(normalize_repo_key(" AcMe ", "App"), "acme/app");
        assert_eq!(normalize_repo_key("", "app"), "");
        assert_eq!(normalize_repo_key("acme", ""), "");
    }
}
