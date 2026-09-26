//! Port of `server/lib/walkthrough/pull-request.js`.
//!
//! Raw unified diff for a pull request. GitHub already returns the
//! merge-base diff for a PR, so this matches the three-dot semantics used for
//! local branch reviews: work merged in from the base branch is not part of
//! it.
//!
//! JS precedent calls the shared GitHub octokit helper
//! (`getOctokitOrNull`) plus `resolveGitHubRepoFromDirectory`. The ported
//! `src/github` module keeps those internals private, so the minimal honest
//! equivalents live here: the stored OAuth token from `<data_dir>/github-auth.json`
//! (the gh-CLI credential fallback is a noted gap), remote resolution through
//! the ported git service, and the diff request over the shared rustls HTTP
//! client. The HTTP call is a seam for tests, mirroring the octokit mock in
//! `pull-request.test.js`.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};

use super::error::WalkthroughError;
use super::small_model::BoxFuture;
use super::sources::PrDiffFn;

const GITHUB_API_BASE: &str = "https://api.github.com";

/// One issued request, for seam-observing tests.
#[derive(Debug, Clone, PartialEq)]
pub struct PrDiffHttpRequest {
    pub url: String,
    pub accept: String,
    pub authorization: Option<String>,
}

pub type PrHttpFn = Arc<
    dyn Fn(PrDiffHttpRequest) -> BoxFuture<Result<(u16, String), WalkthroughError>> + Send + Sync,
>;

pub type RemoteUrlFn = Arc<dyn Fn(String) -> BoxFuture<Option<String>> + Send + Sync>;

/// The PR-diff differ's collaborators, all injectable.
pub struct PullRequestDiffer {
    data_dir: PathBuf,
    remote_url: RemoteUrlFn,
    http: PrHttpFn,
}

impl PullRequestDiffer {
    pub fn new(data_dir: PathBuf, remote_url: RemoteUrlFn, http: PrHttpFn) -> Self {
        Self {
            data_dir,
            remote_url,
            http,
        }
    }

    /// `getOctokitOrNull`, minimal honest form: the stored GitHub OAuth entry
    /// in `<data_dir>/github-auth.json`. Gap (noted): the JS resolution also
    /// falls back to the `gh` CLI credential and honors its settings
    /// preference; those live in the private `src/github` port.
    fn stored_token(&self) -> Option<String> {
        let raw = std::fs::read_to_string(self.data_dir.join("github-auth.json")).ok()?;
        let value: Value = serde_json::from_str(&raw).ok()?;
        let token = value
            .get("accessToken")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        (!token.is_empty()).then_some(token)
    }

    /// `getPullRequestDiff`.
    pub async fn diff(
        &self,
        directory: &str,
        number: i64,
    ) -> Result<(String, Value), WalkthroughError> {
        let Some(token) = self.stored_token() else {
            return Err(WalkthroughError::with_code(
                "Connect a GitHub account to review pull requests",
                401,
                "github-not-connected",
            ));
        };

        // The resolver returns `{ repo, remoteUrl }`, not the repo itself.
        // Reading `.owner` off the wrapper made this check fail for every
        // repository in the JS — the check stays on the repo itself.
        let remote_url = (self.remote_url)(directory.to_string()).await;
        let Some(repo) = remote_url.as_deref().and_then(parse_github_remote_url) else {
            return Err(WalkthroughError::with_code(
                "This directory has no GitHub remote",
                400,
                "no-github-remote",
            ));
        };

        let (status, body) = (self.http)(PrDiffHttpRequest {
            url: format!("{GITHUB_API_BASE}/repos/{repo}/pulls/{number}"),
            accept: "application/vnd.github.v3.diff".to_string(),
            authorization: Some(format!("Bearer {token}")),
        })
        .await?;

        if !(200..300).contains(&status) {
            // The JS octokit rejection carried no `statusCode`, so routes
            // answered 500 with the transport message.
            return Err(WalkthroughError::internal(format!(
                "GitHub request failed with {status}"
            )));
        }

        let patch = body;
        if patch.trim().is_empty() {
            return Err(WalkthroughError::with_code(
                format!("Pull request #{number} has no diff"),
                404,
                "empty-diff",
            ));
        }

        Ok((
            patch,
            json!({ "owner": repo.owner, "repo": repo.repo, "number": number }),
        ))
    }

    /// Adapt into the sources seam shape.
    pub fn into_pr_diff_fn(self: Arc<Self>) -> PrDiffFn {
        Arc::new(move |directory: String, number: i64| {
            let differ = Arc::clone(&self);
            Box::pin(async move { differ.diff(&directory, number).await })
        })
    }
}

/// The parsed `owner/repo` pair of a GitHub remote.
#[derive(Debug, Clone, PartialEq)]
pub struct RepoRef {
    pub owner: String,
    pub repo: String,
}

impl std::fmt::Display for RepoRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.repo)
    }
}

/// Mirrors `parseGitHubRemoteUrl` (and the ported `github::repo` copy):
/// `git@github.com:`, `ssh://git@github.com/`, and `https://github.com/`
/// shapes with an optional `.git` suffix.
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
        })
    };

    if let Some(rest) = value.strip_prefix("git@github.com:") {
        return from_owner_repo(rest);
    }
    if let Some(rest) = value.strip_prefix("ssh://git@github.com/") {
        return from_owner_repo(rest);
    }

    let url = url::Url::parse(value).ok()?;
    if url.host_str() != Some("github.com") {
        return None;
    }
    let path = url.path().trim_matches('/');
    from_owner_repo(path)
}

/// Production wiring: git remote resolution through the ported git service
/// and the shared rustls client. The 8s timeout mirrors the github module's
/// octokit-parity default.
pub fn pull_request_differ(
    data_dir: PathBuf,
    git: Arc<crate::git_service::service::GitService>,
) -> Arc<PullRequestDiffer> {
    let remote_url: RemoteUrlFn = Arc::new(move |directory: String| {
        let git = Arc::clone(&git);
        Box::pin(async move { git.get_remote_url(&directory, "origin").await })
    });
    let http: PrHttpFn = Arc::new(|request: PrDiffHttpRequest| {
        Box::pin(async move {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(8))
                .build()
                .map_err(|error| WalkthroughError::internal(error.to_string()))?;
            let mut builder = client
                .get(&request.url)
                .header("accept", &request.accept)
                .header("user-agent", "octokit-rest.js/rust-port");
            if let Some(authorization) = &request.authorization {
                builder = builder.header("authorization", authorization);
            }
            let response = builder
                .send()
                .await
                .map_err(|error| WalkthroughError::internal(error.to_string()))?;
            let status = response.status().as_u16();
            let body = response
                .text()
                .await
                .map_err(|error| WalkthroughError::internal(error.to_string()))?;
            Ok((status, body))
        })
    });
    Arc::new(PullRequestDiffer::new(data_dir, remote_url, http))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    fn temp_data_dir() -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("walkthrough-pr-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn differ(
        data_dir: &PathBuf,
        remote: Option<&'static str>,
        response: (u16, &'static str),
    ) -> (Arc<PullRequestDiffer>, Arc<Mutex<Vec<PrDiffHttpRequest>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_for_http = Arc::clone(&seen);
        let differ = Arc::new(PullRequestDiffer::new(
            data_dir.clone(),
            Arc::new(move |_directory| Box::pin(async move { remote.map(|url| url.to_string()) })),
            Arc::new(move |request: PrDiffHttpRequest| {
                let seen = Arc::clone(&seen_for_http);
                let status = response.0;
                let body = response.1.to_string();
                Box::pin(async move {
                    seen.lock().unwrap().push(request.clone());
                    Ok((status, body))
                })
            }),
        ));
        (differ, seen)
    }

    const PATCH: &str = "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1,1 +1,2 @@\n+const added = true;\n";

    #[tokio::test]
    async fn requests_the_diff_for_the_resolved_repository() {
        let data_dir = temp_data_dir();
        std::fs::write(
            data_dir.join("github-auth.json"),
            json!({ "accessToken": "gho_test" }).to_string(),
        )
        .unwrap();

        let (differ, seen) = differ(
            &data_dir,
            Some("git@github.com:openchamber/openchamber.git"),
            (200, PATCH),
        );
        let result = differ.diff("/repo", 2122).await.unwrap();

        assert_eq!(result.0, PATCH);
        assert_eq!(
            result.1,
            json!({ "owner": "openchamber", "repo": "openchamber", "number": 2122 })
        );
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].url,
            "https://api.github.com/repos/openchamber/openchamber/pulls/2122"
        );
        assert_eq!(requests[0].accept, "application/vnd.github.v3.diff");
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("Bearer gho_test")
        );

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[tokio::test]
    async fn reports_a_missing_github_remote_only_when_there_really_is_none() {
        let data_dir = temp_data_dir();
        std::fs::write(
            data_dir.join("github-auth.json"),
            json!({ "accessToken": "gho_test" }).to_string(),
        )
        .unwrap();

        let (differ, seen) = differ(&data_dir, None, (200, PATCH));
        let error = differ.diff("/repo", 2122).await.unwrap_err();

        assert_eq!(error.status, 400);
        assert_eq!(error.code.as_deref(), Some("no-github-remote"));
        assert!(seen.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[tokio::test]
    async fn asks_the_user_to_connect_github_before_anything_else() {
        let data_dir = temp_data_dir();
        let (differ, seen) = differ(
            &data_dir,
            Some("git@github.com:openchamber/openchamber.git"),
            (200, PATCH),
        );
        // No github-auth.json: no stored token.
        let error = differ.diff("/repo", 2122).await.unwrap_err();

        assert_eq!(error.status, 401);
        assert_eq!(error.code.as_deref(), Some("github-not-connected"));
        assert!(seen.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[tokio::test]
    async fn treats_an_empty_diff_as_a_missing_pull_request_rather_than_an_empty_review() {
        let data_dir = temp_data_dir();
        std::fs::write(
            data_dir.join("github-auth.json"),
            json!({ "accessToken": "gho_test" }).to_string(),
        )
        .unwrap();
        let (differ, _) = differ(
            &data_dir,
            Some("https://github.com/ompchamber/ompchamber"),
            (200, "   "),
        );
        let error = differ.diff("/repo", 2122).await.unwrap_err();

        assert_eq!(error.status, 404);
        assert_eq!(error.code.as_deref(), Some("empty-diff"));
        assert_eq!(error.message, "Pull request #2122 has no diff");
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[tokio::test]
    async fn surfaces_provider_failures_as_internal_errors() {
        let data_dir = temp_data_dir();
        std::fs::write(
            data_dir.join("github-auth.json"),
            json!({ "accessToken": "gho_test" }).to_string(),
        )
        .unwrap();
        let (differ, _) = differ(
            &data_dir,
            Some("git@github.com:openchamber/openchamber.git"),
            (401, "Bad credentials"),
        );
        let error = differ.diff("/repo", 2122).await.unwrap_err();
        assert_eq!(error.status, 500);
        assert_eq!(error.message, "GitHub request failed with 401");
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn parses_the_supported_remote_shapes() {
        assert_eq!(
            parse_github_remote_url("git@github.com:owner/name.git").map(|r| r.to_string()),
            Some("owner/name".to_string())
        );
        assert_eq!(
            parse_github_remote_url("ssh://git@github.com/owner/name").map(|r| r.to_string()),
            Some("owner/name".to_string())
        );
        assert_eq!(
            parse_github_remote_url("https://github.com/owner/name.git").map(|r| r.to_string()),
            Some("owner/name".to_string())
        );
        assert_eq!(
            parse_github_remote_url("https://gitlab.com/owner/name"),
            None
        );
        assert_eq!(parse_github_remote_url(""), None);
    }
}
