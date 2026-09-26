//! Port of `server/lib/github/repo/index.js` + `repo/fork-detection.js`:
//! remote URL parsing, directory→repo resolution, and the repo network
//! (origin + parent/source upstream) with its own 5-minute metadata cache.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use serde_json::Value;

use crate::github::client::{GithubClient, GithubError};
use crate::github::git_ops;
use crate::github::rate_limit::now_ms;

/// `REPO_METADATA_TTL_MS` / `REPO_METADATA_CACHE_MAX_ENTRIES`.
const REPO_METADATA_TTL_MS: i64 = 5 * 60_000;
const REPO_METADATA_CACHE_MAX_ENTRIES: usize = 200;

#[derive(Debug, Clone, PartialEq)]
pub struct RepoRef {
    pub owner: String,
    pub repo: String,
    pub url: String,
}

impl RepoRef {
    pub fn to_json(&self) -> Value {
        serde_json::json!({ "owner": self.owner, "repo": self.repo, "url": self.url })
    }
}

/// `parseGitHubRemoteUrl`: `git@github.com:`, `ssh://git@github.com/`, and
/// `https://github.com/` shapes (with optional `.git` suffix).
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
#[derive(Debug, Clone)]
pub struct NetworkRepo {
    pub owner: String,
    pub repo: String,
    pub url: String,
    /// `'origin'` or `'upstream'`.
    pub source: &'static str,
}

/// fork-detection.js's module-local metadata cache.
#[derive(Default)]
pub struct RepoNetworkCache {
    entries: Mutex<HashMap<String, (Option<Value>, i64)>>,
}

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

impl RepoNetworkCache {
    pub fn new() -> Self {
        Self::default()
    }

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

pub fn normalize_repo_key(owner: &str, repo: &str) -> String {
    let owner = owner.trim().to_lowercase();
    let repo = repo.trim().to_lowercase();
    if owner.is_empty() || repo.is_empty() {
        return String::new();
    }
    format!("{owner}/{repo}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ssh_colon_urls() {
        let parsed = parse_github_remote_url("git@github.com:acme/app.git").unwrap();
        assert_eq!(parsed.owner, "acme");
        assert_eq!(parsed.repo, "app");
        assert_eq!(parsed.url, "https://github.com/acme/app");
    }

    #[test]
    fn parses_ssh_scheme_urls() {
        let parsed = parse_github_remote_url("ssh://git@github.com/acme/app.git").unwrap();
        assert_eq!(parsed.owner, "acme");
        assert_eq!(parsed.repo, "app");
    }

    #[test]
    fn parses_https_urls_with_and_without_git_suffix() {
        let parsed = parse_github_remote_url("https://github.com/acme/app").unwrap();
        assert_eq!(parsed.repo, "app");
        let parsed = parse_github_remote_url("https://github.com/acme/app.git").unwrap();
        assert_eq!(parsed.repo, "app");
        assert_eq!(parsed.url, "https://github.com/acme/app");
    }

    #[test]
    fn rejects_non_github_and_malformed_urls() {
        assert!(parse_github_remote_url("https://gitlab.com/acme/app").is_none());
        assert!(parse_github_remote_url("git@gitlab.com:acme/app").is_none());
        assert!(parse_github_remote_url("").is_none());
        assert!(parse_github_remote_url("not a url").is_none());
        assert!(parse_github_remote_url("https://github.com/onlyowner").is_none());
        assert!(parse_github_remote_url("git@github.com:onlyowner").is_none());
    }

    #[test]
    fn trims_input_and_tolerates_extra_path_segments_like_js_split() {
        // JS `cleaned.split('/')` takes the first two parts and ignores extras.
        let parsed = parse_github_remote_url("  https://github.com/acme/app/extra  ").unwrap();
        assert_eq!(parsed.owner, "acme");
        assert_eq!(parsed.repo, "app");
    }

    #[test]
    fn normalize_repo_key_lowercases_and_requires_both_parts() {
        assert_eq!(normalize_repo_key(" AcMe ", "App"), "acme/app");
        assert_eq!(normalize_repo_key("", "app"), "");
        assert_eq!(normalize_repo_key("acme", ""), "");
    }
}
