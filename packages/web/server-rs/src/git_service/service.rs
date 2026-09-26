//! Core git service — port of the exported functions in
//! `server/lib/git/service.js` that back the `/api/git/*` route family:
//! status/diff/commit/log/branch operations, stashes, push/pull/fetch,
//! merge/rebase, conflict details, remotes, and identity config.
//!
//! simple-git behaviour this port reproduces (verified against
//! simple-git@3.36.0 bundled in `packages/web/node_modules`):
//! - `git.raw` fails only when git exits non-zero *with* stderr; the failure
//!   message is stdout+stderr concatenated.
//! - `git.status()` runs `status --porcelain -b -u --null` (+ caller flags).
//! - `git.commit()` runs `-c core.abbrev=40 commit -m <msg> <files>` and the
//!   response shape is the parsed CommitSummary.
//! - `git.log()` runs a `--pretty=format:ò…` boundary format whose parsed
//!   fields are `hash,date,message,refs,body,author_name,author_email`.
//! - `git.branch()` runs `branch -v -a` (or `branch -v` for local-only).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde_json::{Map, Value, json};

use super::exec::{GitCommandResult, GitFailure, GitRunner, real_runner};
use super::paths::{
    absolutize, check_path_exists, is_image_file, is_inside_or_same_directory,
    normalize_directory_path, require_directory, to_git_path,
};

pub type ServiceResult<T> = Result<T, String>;

pub(crate) const NO_EXT_DIFF: &str = "--no-ext-diff";
const REMOTE_EXISTENCE_CACHE_TTL: Duration = Duration::from_secs(30);
const BINARY_SNIFF_BYTES: usize = 8192;
const MAX_NEW_FILE_STATS: usize = 200;
const MAX_NEW_FILE_STAT_SIZE: u64 = 1024 * 1024;

// ---------------------------------------------------------------------------
// Module state (JS module-scope maps)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BootstrapState {
    pub status: String,
    pub phase: String,
    pub error: Option<String>,
    pub updated_at_ms: u64,
}

pub struct GitService {
    runner: GitRunner,
    /// JS `gitIndexMutationQueues` — one serialized queue per repository root.
    pub(crate) queues: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
    /// JS `remoteExistenceCache` (`REMOTE_EXISTENCE_CACHE_TTL_MS` = 30s).
    remote_existence_cache: Mutex<HashMap<String, (bool, Instant)>>,
    /// JS `worktreeBootstrapState`.
    pub(crate) bootstrap_state: Mutex<HashMap<PathBuf, BootstrapState>>,
    /// JS `activeWorktreeBootstrapTasks` — a held lock marks an active task.
    pub(crate) active_bootstrap: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
}

/// Task body accepted by [`GitService::with_index_queue`]: receives the
/// service by shared reference for the queued duration.
pub(crate) type QueuedTask<'a, T> = Pin<Box<dyn Future<Output = ServiceResult<T>> + Send + 'a>>;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Default for GitService {
    fn default() -> Self {
        Self::new()
    }
}

impl GitService {
    pub fn new() -> Self {
        Self::with_runner(real_runner())
    }

    pub fn with_runner(runner: GitRunner) -> Self {
        Self {
            runner,
            queues: Mutex::new(HashMap::new()),
            remote_existence_cache: Mutex::new(HashMap::new()),
            bootstrap_state: Mutex::new(HashMap::new()),
            active_bootstrap: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn runner(&self) -> GitRunner {
        Arc::clone(&self.runner)
    }

    pub(crate) async fn run(&self, cwd: impl AsRef<Path>, args: &[&str]) -> GitCommandResult {
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        (self.runner)(cwd.as_ref().to_path_buf(), argv, HashMap::new()).await
    }

    pub(crate) async fn run_with_env(
        &self,
        cwd: impl AsRef<Path>,
        args: &[&str],
        env: HashMap<String, String>,
    ) -> GitCommandResult {
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        (self.runner)(cwd.as_ref().to_path_buf(), argv, env).await
    }

    pub(crate) async fn run_strings(
        &self,
        cwd: impl AsRef<Path>,
        args: &[String],
    ) -> GitCommandResult {
        (self.runner)(cwd.as_ref().to_path_buf(), args.to_vec(), HashMap::new()).await
    }

    /// simple-git `git.raw` semantics: `Err` only when git exited non-zero
    /// with stderr output; the failure message is stdout+stderr.
    pub(crate) async fn raw(
        &self,
        cwd: impl AsRef<Path>,
        args: &[String],
    ) -> Result<String, GitFailure> {
        let result = (self.runner)(cwd.as_ref().to_path_buf(), args.to_vec(), HashMap::new()).await;
        if result.success || result.stderr.trim().is_empty() {
            Ok(result.stdout)
        } else {
            let message = format!("{}{}", result.stdout, result.stderr);
            Err(GitFailure {
                exit_code: result.exit_code,
                stdout: result.stdout,
                stderr: result.stderr,
                message,
            })
        }
    }

    pub(crate) async fn raw_env(
        &self,
        cwd: impl AsRef<Path>,
        args: &[String],
        env: HashMap<String, String>,
    ) -> Result<String, GitFailure> {
        let result = (self.runner)(cwd.as_ref().to_path_buf(), args.to_vec(), env).await;
        if result.success || result.stderr.trim().is_empty() {
            Ok(result.stdout)
        } else {
            let message = format!("{}{}", result.stdout, result.stderr);
            Err(GitFailure {
                exit_code: result.exit_code,
                stdout: result.stdout,
                stderr: result.stderr,
                message,
            })
        }
    }

    /// `runGitCommandOrThrow` — failure message falls back per JS.
    pub(crate) async fn run_or_throw(
        &self,
        cwd: impl AsRef<Path>,
        args: &[&str],
        fallback_message: &str,
    ) -> ServiceResult<GitCommandResult> {
        let result = self.run(cwd, args).await;
        if !result.success {
            let message = if !result.message.trim().is_empty() {
                result.message.trim().to_string()
            } else if !fallback_message.is_empty() {
                fallback_message.to_string()
            } else {
                "Git command failed".to_string()
            };
            return Err(message);
        }
        Ok(result)
    }

    /// JS `withGitIndexMutationQueue` — serializes index mutations per repo.
    /// The task receives the service by reference for the queued duration.
    pub(crate) async fn with_index_queue<T>(
        &self,
        directory: Option<&str>,
        task: impl for<'a> FnOnce(&'a GitService) -> QueuedTask<'a, T>,
    ) -> ServiceResult<T>
    where
        T: Send,
    {
        let key = match directory {
            Some(dir) => self.queue_key(dir).await,
            None => None,
        };
        let Some(key) = key else {
            return task(self).await;
        };
        let lock = {
            let mut queues = self.queues.lock().unwrap_or_else(|e| e.into_inner());
            queues
                .entry(key.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;
        let result = task(self).await;
        let mut queues = self.queues.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = queues.get(&key)
            && Arc::ptr_eq(existing, &lock)
            && Arc::strong_count(&lock) <= 2
        {
            queues.remove(&key);
        }
        result
    }

    async fn queue_key(&self, directory: &str) -> Option<PathBuf> {
        let normalized = normalize_directory_path(Some(directory))?;
        if normalized.trim().is_empty() {
            return None;
        }
        match self.resolve_repository_root(&normalized).await {
            Ok(root) => Some(root),
            Err(_) => Some(absolutize(Path::new(&normalized))),
        }
    }

    // -- repository context --------------------------------------------------

    pub(crate) async fn resolve_repository_root(
        &self,
        directory_path: &str,
    ) -> ServiceResult<PathBuf> {
        let top_level = self
            .run(directory_path, &["rev-parse", "--show-toplevel"])
            .await
            .stdout
            .trim()
            .to_string();
        if top_level.is_empty() {
            // JS resolves against the raw (possibly empty) output and lets the
            // caller's path.resolve produce the directory itself.
            return Err("Git directory is required".to_string());
        }
        let top = PathBuf::from(&top_level);
        if top.is_absolute() {
            Ok(absolutize(&top))
        } else {
            Ok(absolutize(&Path::new(directory_path).join(top)))
        }
    }

    pub(crate) async fn repository_context(&self, directory: &str) -> ServiceResult<RepoContext> {
        let directory_path = require_directory(Some(directory))?;
        let repo_root = self.resolve_repository_root(&directory_path).await?;
        Ok(RepoContext {
            directory_path: PathBuf::from(&directory_path),
            repo_root,
        })
    }

    /// JS `resolveGitInternalPath` — `rev-parse --git-path <name>` resolved
    /// against the repository root.
    pub(crate) async fn resolve_git_internal_path(
        &self,
        repo_root: &Path,
        git_path: &str,
    ) -> ServiceResult<PathBuf> {
        let resolved = self
            .raw(
                repo_root,
                &["rev-parse".into(), "--git-path".into(), git_path.into()],
            )
            .await
            .unwrap_or_default()
            .trim()
            .to_string();
        Ok(absolutize(&repo_root.join(resolved)))
    }

    /// JS `resolveGitFileContext` — resolve a caller-supplied file path to an
    /// in-repo path that exists in the worktree, the index, or HEAD.
    pub(crate) async fn resolve_git_file_context(
        &self,
        directory_path: &Path,
        repo_root: &Path,
        file_path: &str,
    ) -> ServiceResult<FileContext> {
        let mut candidates: Vec<PathBuf> = vec![
            absolutize(&repo_root.join(file_path)),
            absolutize(&directory_path.join(file_path)),
        ];
        candidates.dedup();

        for absolute_path in candidates {
            if !is_inside_or_same_directory(repo_root, &absolute_path) {
                continue;
            }
            let repo_path = to_git_path(&relative_path_string(repo_root, &absolute_path));
            let metadata = tokio::fs::symlink_metadata(&absolute_path).await.ok();
            let is_symbolic_link = metadata
                .as_ref()
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false);
            let exists_in_worktree = metadata
                .as_ref()
                .map(|m| m.is_file() || is_symbolic_link)
                .unwrap_or(false);
            let exists_in_index = self
                .raw(
                    repo_root,
                    &["cat-file".into(), "-e".into(), format!(":{}", repo_path)],
                )
                .await
                .is_ok();
            let exists_in_head = self
                .raw(
                    repo_root,
                    &[
                        "cat-file".into(),
                        "-e".into(),
                        format!("HEAD:{}", repo_path),
                    ],
                )
                .await
                .is_ok();
            if exists_in_worktree || exists_in_index || exists_in_head {
                return Ok(FileContext {
                    absolute_path,
                    repo_path,
                    is_symbolic_link,
                });
            }
        }

        Err("Invalid file path".to_string())
    }

    /// JS `hasRemote` with the module-level 30s existence cache.
    pub(crate) async fn has_remote(&self, cwd: &Path, directory: &str, remote_name: &str) -> bool {
        let remote = remote_name.trim();
        if remote.is_empty() {
            return false;
        }
        let key = format!(
            "{}\0{}",
            absolutize(&PathBuf::from(
                normalize_directory_path(Some(directory)).unwrap_or_default()
            ))
            .to_string_lossy(),
            remote
        );
        if let Some((exists, checked_at)) = self
            .remote_existence_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            && checked_at.elapsed() < REMOTE_EXISTENCE_CACHE_TTL
        {
            return *exists;
        }
        let exists = self
            .raw(
                cwd,
                &["remote".into(), "get-url".into(), remote.to_string()],
            )
            .await
            .map(|out| !out.trim().is_empty())
            .unwrap_or(false);
        self.remote_existence_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, (exists, Instant::now()));
        exists
    }

    /// JS `gitRefExists` — deliberately without `--quiet`.
    pub(crate) async fn git_ref_exists(&self, cwd: &Path, reference: &str) -> bool {
        self.run(cwd, &["show-ref", "--verify", reference])
            .await
            .success
    }

    /// JS `getConfig(key, scope)` → last configured value.
    pub async fn get_config(&self, cwd: &Path, key: &str, scope: &str) -> Option<String> {
        let mut args = vec!["config".to_string(), format!("--{}", scope)];
        args.extend(
            ["--null", "--show-origin", "--get-all", key]
                .iter()
                .map(|s| s.to_string()),
        );
        let output = self.run_strings(cwd, &args).await;
        parse_config_null_value(&output.stdout)
    }

    pub async fn add_config(
        &self,
        cwd: &Path,
        key: &str,
        value: &str,
        scope: &str,
    ) -> ServiceResult<()> {
        self.run_or_throw(
            cwd,
            &["config", &format!("--{}", scope), key, value],
            "Failed to set config",
        )
        .await
        .map(|_| ())
    }

    // -----------------------------------------------------------------------
    // Repository / identity queries
    // -----------------------------------------------------------------------

    pub async fn is_git_repository(&self, directory: &str) -> bool {
        let Some(directory_path) =
            normalize_directory_path(Some(directory)).filter(|v| !v.trim().is_empty())
        else {
            return false;
        };
        if !check_path_exists(&directory_path).await {
            return false;
        }
        self.run(&directory_path, &["rev-parse", "--git-dir"])
            .await
            .success
    }

    pub async fn get_global_identity(&self) -> Value {
        let home = crate::config::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let user_name = self.get_config(&home, "user.name", "global").await;
        let user_email = self.get_config(&home, "user.email", "global").await;
        let ssh_command = self.get_config(&home, "core.sshCommand", "global").await;
        json!({
            "userName": user_name,
            "userEmail": user_email,
            "sshCommand": ssh_command,
        })
    }

    pub async fn get_remote_url(&self, directory: &str, remote_name: &str) -> Option<String> {
        let directory_path = require_directory(Some(directory)).ok()?;
        let result = self
            .run(&directory_path, &["remote", "get-url", remote_name])
            .await;
        let trimmed = result.stdout.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    }

    pub async fn get_current_identity(&self, directory: &str) -> Value {
        let home = crate::config::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let directory_path = normalize_directory_path(Some(directory)).unwrap_or_default();
        let cwd = if directory_path.trim().is_empty() {
            home.clone()
        } else {
            PathBuf::from(directory_path)
        };
        let pick = |local: Option<String>, global: Option<String>| local.or(global);
        let user_name = pick(
            self.get_config(&cwd, "user.name", "local").await,
            self.get_config(&home, "user.name", "global").await,
        );
        let user_email = pick(
            self.get_config(&cwd, "user.email", "local").await,
            self.get_config(&home, "user.email", "global").await,
        );
        let ssh_command = pick(
            self.get_config(&cwd, "core.sshCommand", "local").await,
            self.get_config(&home, "core.sshCommand", "global").await,
        );
        json!({
            "userName": user_name,
            "userEmail": user_email,
            "sshCommand": ssh_command,
        })
    }

    pub async fn has_local_identity(&self, directory: &str) -> bool {
        let Ok(directory_path) = require_directory(Some(directory)) else {
            return false;
        };
        let cwd = PathBuf::from(directory_path);
        let name = self.get_config(&cwd, "user.name", "local").await;
        if name.is_some() {
            return true;
        }
        self.get_config(&cwd, "user.email", "local").await.is_some()
    }

    pub async fn set_local_identity(
        &self,
        directory: &str,
        profile: &Value,
    ) -> ServiceResult<bool> {
        let directory_path = require_directory(Some(directory))?;
        let cwd = PathBuf::from(directory_path);
        let text = |field: &str| {
            profile
                .get(field)
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_default()
        };
        let auth_type = {
            let value = text("authType");
            if value.is_empty() {
                "ssh".to_string()
            } else {
                value
            }
        };

        self.add_config(&cwd, "user.name", &text("userName"), "local")
            .await?;
        self.add_config(&cwd, "user.email", &text("userEmail"), "local")
            .await?;

        let ssh_key = text("sshKey");
        if auth_type == "ssh" && !ssh_key.is_empty() {
            let ssh_command = super::paths::build_ssh_command(&ssh_key)?;
            self.run_or_throw(
                &cwd,
                &["config", "--local", "core.sshCommand", &ssh_command],
                "Failed to set config",
            )
            .await?;
            let _ = self
                .raw(
                    &cwd,
                    &[
                        "config".into(),
                        "--local".into(),
                        "--unset".into(),
                        "credential.helper".into(),
                    ],
                )
                .await;
        } else if auth_type == "token" {
            let host = text("host");
            if !host.is_empty() {
                self.add_config(&cwd, "credential.helper", "store", "local")
                    .await?;
                let _ = self
                    .raw(
                        &cwd,
                        &[
                            "config".into(),
                            "--local".into(),
                            "--unset".into(),
                            "core.sshCommand".into(),
                        ],
                    )
                    .await;
            }
        }

        let sign_commits = profile.get("signCommits") == Some(&Value::Bool(true));
        let signing_key = text("signingKey");
        if sign_commits && !signing_key.trim().is_empty() {
            self.add_config(&cwd, "gpg.format", "ssh", "local").await?;
            self.add_config(&cwd, "user.signingkey", signing_key.trim(), "local")
                .await?;
            self.add_config(&cwd, "commit.gpgsign", "true", "local")
                .await?;
        }

        Ok(true)
    }

    // -----------------------------------------------------------------------
    // Status
    // -----------------------------------------------------------------------

    pub async fn get_status(&self, directory: &str, mode: Option<&str>) -> ServiceResult<Value> {
        let light_mode = mode == Some("light");
        let normalized = require_directory(Some(directory))?;
        if !self.is_git_repository(&normalized).await {
            return Err(NOT_A_REPO_MESSAGE.to_string());
        }
        let context = self.repository_context(&normalized).await?;
        let status = self.parse_status(&context.repo_root, &["-uall"]).await;

        let (staged_stats_raw, working_stats_raw) = if light_mode {
            (String::new(), String::new())
        } else {
            let staged = self
                .raw(
                    &context.repo_root,
                    &["diff".into(), "--cached".into(), "--numstat".into()],
                )
                .await
                .unwrap_or_default();
            let working = self
                .raw(&context.repo_root, &["diff".into(), "--numstat".into()])
                .await
                .unwrap_or_default();
            (staged, working)
        };

        let mut diff_stats: serde_json::Map<String, Value> = serde_json::Map::new();
        accumulate_numstat(&staged_stats_raw, &mut diff_stats);
        accumulate_numstat(&working_stats_raw, &mut diff_stats);

        let mut new_file_stats: Vec<(String, i64, i64)> = Vec::new();
        if !light_mode {
            for file in &status.files {
                if new_file_stats.len() >= MAX_NEW_FILE_STATS {
                    break;
                }
                let working = file.working_dir.trim();
                let index_status = file.index.trim();
                let status_code = if working.is_empty() {
                    index_status
                } else {
                    working
                };
                if status_code != "?" && status_code != "A" {
                    continue;
                }
                let existing = diff_stats.get(&file.path);
                if existing
                    .and_then(|v| v.get("insertions"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    > 0
                {
                    continue;
                }
                let absolute = context.repo_root.join(&file.path);
                match tokio::fs::metadata(&absolute).await {
                    Ok(meta) if meta.is_file() && meta.len() <= MAX_NEW_FILE_STAT_SIZE => {
                        match tokio::fs::read(&absolute).await {
                            Ok(buffer) => {
                                if buffer.contains(&0) {
                                    new_file_stats.push((
                                        file.path.clone(),
                                        existing
                                            .and_then(|v| v.get("insertions"))
                                            .and_then(Value::as_i64)
                                            .unwrap_or(0),
                                        existing
                                            .and_then(|v| v.get("deletions"))
                                            .and_then(Value::as_i64)
                                            .unwrap_or(0),
                                    ));
                                } else {
                                    let text =
                                        String::from_utf8_lossy(&buffer).replace("\r\n", "\n");
                                    let line_count = if text.is_empty() {
                                        0
                                    } else if text.ends_with('\n') {
                                        text.trim_end_matches('\n').split('\n').count() as i64
                                    } else {
                                        text.split('\n').count() as i64
                                    };
                                    new_file_stats.push((file.path.clone(), line_count, 0));
                                }
                            }
                            Err(_) => continue,
                        }
                    }
                    _ => continue,
                }
            }
        }

        for (path, insertions, deletions) in new_file_stats {
            diff_stats.insert(
                path,
                json!({ "insertions": insertions, "deletions": deletions }),
            );
        }

        let mut tracking = status.tracking.clone();
        let mut ahead = status.ahead;
        let mut behind = status.behind;
        let mut upstream_comparison = Value::Null;

        if !light_mode
            && tracking.is_none()
            && let Some(current) = &status.current
            && let Some(base_ref) = self
                .select_base_ref_for_unpublished(&context.repo_root, current)
                .await
        {
            let count_raw = self
                .raw(
                    &context.repo_root,
                    &[
                        "rev-list".into(),
                        "--count".into(),
                        format!("{}..HEAD", base_ref),
                    ],
                )
                .await
                .unwrap_or_default()
                .trim()
                .to_string();
            if let Ok(count) = count_raw.parse::<i64>() {
                ahead = count;
                behind = 0;
            }
        }

        let tracking_is_upstream = tracking
            .as_deref()
            .map(|t| t.starts_with("upstream/"))
            .unwrap_or(false);
        if !light_mode
            && status.current.is_some()
            && (!tracking_is_upstream || tracking.is_none())
            && self
                .has_remote(
                    &context.repo_root,
                    &context.directory_path.to_string_lossy(),
                    "upstream",
                )
                .await
            && let Some(current) = &status.current
            && let Some(comparison) = self
                .remote_branch_comparison(&context.repo_root, "upstream", current)
                .await
        {
            upstream_comparison = json!({
                "remote": comparison.0,
                "branch": comparison.1,
                "ahead": comparison.2,
                "behind": comparison.3,
            });
        }

        let _ = &mut tracking;

        let merge_in_progress = self.detect_merge_in_progress(&context).await;
        let rebase_in_progress = self.detect_rebase_in_progress(&context).await;

        let mut payload = Map::new();
        payload.insert(
            "current".into(),
            status
                .current
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        payload.insert(
            "tracking".into(),
            tracking.take().map(Value::String).unwrap_or(Value::Null),
        );
        payload.insert("ahead".into(), json!(ahead));
        payload.insert("behind".into(), json!(behind));
        if !upstream_comparison.is_null() {
            payload.insert("upstreamComparison".into(), upstream_comparison);
        }
        payload.insert(
            "files".into(),
            Value::Array(
                status
                    .files
                    .iter()
                    .map(|f| {
                        json!({
                            "path": f.path,
                            "index": f.index,
                            "working_dir": f.working_dir,
                        })
                    })
                    .collect(),
            ),
        );
        payload.insert("isClean".into(), Value::Bool(status.files.is_empty()));
        if !light_mode {
            payload.insert("diffStats".into(), Value::Object(diff_stats));
        }
        payload.insert("mergeInProgress".into(), merge_in_progress);
        payload.insert("rebaseInProgress".into(), rebase_in_progress);
        Ok(Value::Object(payload))
    }

    async fn select_base_ref_for_unpublished(
        &self,
        repo_root: &Path,
        _current: &str,
    ) -> Option<String> {
        let mut candidates: Vec<String> = Vec::new();
        let origin_head = self
            .raw(
                repo_root,
                &[
                    "symbolic-ref".into(),
                    "-q".into(),
                    "refs/remotes/origin/HEAD".into(),
                ],
            )
            .await
            .unwrap_or_default()
            .trim()
            .to_string();
        if !origin_head.is_empty() {
            candidates.push(origin_head.trim_start_matches("refs/remotes/").to_string());
        }
        candidates.extend(
            ["origin/main", "origin/master", "main", "master"]
                .iter()
                .map(|s| s.to_string()),
        );
        for reference in candidates {
            let exists = self
                .raw(
                    repo_root,
                    &["rev-parse".into(), "--verify".into(), reference.clone()],
                )
                .await
                .map(|out| !out.trim().is_empty())
                .unwrap_or(false);
            if exists {
                return Some(reference);
            }
        }
        None
    }

    async fn remote_branch_comparison(
        &self,
        repo_root: &Path,
        remote_name: &str,
        branch: &str,
    ) -> Option<(String, String, i64, i64)> {
        let remote_ref = format!("refs/remotes/{}/{}", remote_name.trim(), branch.trim());
        let exists = self
            .raw(
                repo_root,
                &["rev-parse".into(), "--verify".into(), remote_ref.clone()],
            )
            .await
            .map(|out| !out.trim().is_empty())
            .unwrap_or(false);
        if !exists {
            return None;
        }
        let counts_raw = self
            .raw(
                repo_root,
                &[
                    "rev-list".into(),
                    "--left-right".into(),
                    "--count".into(),
                    format!("HEAD...{}", remote_ref),
                ],
            )
            .await
            .unwrap_or_default();
        parse_ahead_behind_counts(&counts_raw)
            .map(|(ahead, behind)| (remote_name.to_string(), branch.to_string(), ahead, behind))
    }

    async fn detect_merge_in_progress(&self, context: &RepoContext) -> Value {
        let merge_head_exists = self
            .raw(
                &context.repo_root,
                &[
                    "rev-parse".into(),
                    "--verify".into(),
                    "--quiet".into(),
                    "MERGE_HEAD".into(),
                ],
            )
            .await
            .is_ok();
        if !merge_head_exists {
            return Value::Null;
        }
        let merge_head = self
            .raw(
                &context.repo_root,
                &["rev-parse".into(), "MERGE_HEAD".into()],
            )
            .await
            .unwrap_or_default();
        let head_sha: String = merge_head.trim().chars().take(7).collect();
        if head_sha.is_empty() {
            return Value::Null;
        }
        let merge_msg = match self
            .resolve_git_internal_path(&context.repo_root, "MERGE_MSG")
            .await
        {
            Ok(path) => tokio::fs::read_to_string(path).await.unwrap_or_default(),
            Err(_) => String::new(),
        };
        let message = merge_msg.lines().next().unwrap_or("").to_string();
        json!({ "head": head_sha, "message": message })
    }

    async fn detect_rebase_in_progress(&self, context: &RepoContext) -> Value {
        let rebase_merge = self
            .resolve_git_internal_path(&context.repo_root, "rebase-merge")
            .await
            .ok();
        let rebase_apply = self
            .resolve_git_internal_path(&context.repo_root, "rebase-apply")
            .await
            .ok();
        let merge_path = match rebase_merge {
            Some(path) if check_path_exists(&path).await => Some(path),
            _ => None,
        };
        let apply_path = match rebase_apply {
            Some(path) if check_path_exists(&path).await => Some(path),
            _ => None,
        };
        let Some(rebase_path) = merge_path.or(apply_path) else {
            return Value::Null;
        };
        let head_name = tokio::fs::read_to_string(rebase_path.join("head-name"))
            .await
            .unwrap_or_default();
        let onto = tokio::fs::read_to_string(rebase_path.join("onto"))
            .await
            .unwrap_or_default();
        let head_name_trimmed = head_name.trim().replace("refs/heads/", "");
        let onto_trimmed: String = onto.trim().chars().take(7).collect();
        if head_name_trimmed.is_empty() && onto_trimmed.is_empty() {
            return Value::Null;
        }
        json!({ "headName": head_name_trimmed, "onto": onto_trimmed })
    }

    // -----------------------------------------------------------------------
    // Diffs
    // -----------------------------------------------------------------------

    pub async fn get_diff(
        &self,
        directory: &str,
        file_path: Option<&str>,
        staged: bool,
        context_lines: Option<i64>,
    ) -> ServiceResult<String> {
        let context = self.repository_context(directory).await?;
        let mut args: Vec<String> = vec!["diff".into(), "--no-color".into(), NO_EXT_DIFF.into()];
        let file_context = match file_path {
            Some(path) if !path.is_empty() => Some(
                self.resolve_git_file_context(&context.directory_path, &context.repo_root, path)
                    .await?,
            ),
            _ => None,
        };

        if let Some(lines) = context_lines {
            args.push(format!("-U{}", lines.max(0)));
        }
        if staged {
            args.push("--cached".into());
        }
        if let Some(fc) = &file_context {
            args.push("--".into());
            args.push(fc.repo_path.clone());
        }

        let diff = self
            .raw(&context.repo_root, &args)
            .await
            .map_err(|f| f.message.trim().to_string())?;
        if !diff.trim().is_empty() {
            return Ok(diff);
        }
        if staged {
            return Ok(diff);
        }
        let Some(fc) = file_context else {
            return Ok(diff);
        };

        let tracked = self
            .raw(
                &context.repo_root,
                &[
                    "ls-files".into(),
                    "--error-unmatch".into(),
                    "--".into(),
                    fc.repo_path.clone(),
                ],
            )
            .await;
        if tracked.is_ok() {
            return Ok(diff);
        }
        if fc.is_symbolic_link {
            let target = tokio::fs::read_link(&fc.absolute_path)
                .await
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .to_string();
            return Ok([
                format!("diff --git a/{} b/{}", fc.repo_path, fc.repo_path),
                "new file mode 120000".to_string(),
                "--- /dev/null".to_string(),
                format!("+++ b/{}", fc.repo_path),
                "@@ -0,0 +1 @@".to_string(),
                format!("+{}", target),
                "\\ No newline at end of file".to_string(),
                String::new(),
            ]
            .join("\n"));
        }

        let mut no_index_args: Vec<String> =
            vec!["diff".into(), "--no-color".into(), NO_EXT_DIFF.into()];
        if let Some(lines) = context_lines {
            no_index_args.push(format!("-U{}", lines.max(0)));
        }
        no_index_args.extend([
            "--no-index".into(),
            "--".into(),
            "/dev/null".into(),
            fc.repo_path.clone(),
        ]);
        match self.raw(&context.repo_root, &no_index_args).await {
            Ok(output) => Ok(output),
            Err(failure) if failure.exit_code == 1 && !failure.message.is_empty() => {
                Ok(failure.message)
            }
            Err(failure) => Err(failure.stderr.trim().to_string()),
        }
    }

    pub async fn list_untracked_paths(&self, directory: &str) -> ServiceResult<Vec<String>> {
        let context = self.repository_context(directory).await?;
        let result = self
            .run(
                &context.repo_root,
                &["ls-files", "--others", "--exclude-standard"],
            )
            .await;
        if !result.success {
            return Ok(Vec::new());
        }
        Ok(result
            .stdout
            .split('\n')
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect())
    }

    pub async fn get_untracked_diffs(
        &self,
        directory: &str,
        file_paths: &[String],
        context_lines: i64,
    ) -> ServiceResult<Vec<String>> {
        let paths: Vec<String> = file_paths.to_vec();
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let context = self.repository_context(directory).await?;
        let mut results: Vec<String> = vec![String::new(); paths.len()];
        for (index, path) in paths.iter().enumerate() {
            let outcome: ServiceResult<String> = async {
                let fc = self
                    .resolve_git_file_context(&context.directory_path, &context.repo_root, path)
                    .await?;
                let mut args: Vec<String> =
                    vec!["diff".into(), "--no-color".into(), NO_EXT_DIFF.into()];
                args.push(format!("-U{}", context_lines.max(0)));
                args.extend([
                    "--no-index".into(),
                    "--".into(),
                    "/dev/null".into(),
                    fc.repo_path,
                ]);
                match self.raw(&context.repo_root, &args).await {
                    Ok(output) => Ok(output),
                    Err(failure) if failure.exit_code == 1 && !failure.message.is_empty() => {
                        Ok(failure.message)
                    }
                    Err(_) => Ok(String::new()),
                }
            }
            .await;
            results[index] = outcome.unwrap_or_default();
        }
        Ok(results)
    }

    pub async fn get_range_diff(
        &self,
        directory: &str,
        base: &str,
        head: &str,
        file_path: Option<&str>,
        context_lines: i64,
    ) -> ServiceResult<String> {
        let context = self.repository_context(directory).await?;
        let base_ref = base.trim().to_string();
        let head_ref = head.trim().to_string();
        if base_ref.is_empty() || head_ref.is_empty() {
            return Err("base and head are required".to_string());
        }

        let mut resolved_base = base_ref.clone();
        let origin_candidate = format!("refs/remotes/origin/{}", base_ref);
        let verified = self
            .raw(
                &context.repo_root,
                &["rev-parse".into(), "--verify".into(), origin_candidate],
            )
            .await
            .map(|out| !out.trim().is_empty())
            .unwrap_or(false);
        if verified {
            resolved_base = format!("origin/{}", base_ref);
        }

        if resolved_base == base_ref && !base_ref.chars().any(|c| "*?[]^~:\\".contains(c)) {
            let resolves_locally = self
                .raw(
                    &context.repo_root,
                    &[
                        "rev-parse".into(),
                        "--verify".into(),
                        format!("refs/heads/{}", base_ref),
                    ],
                )
                .await
                .map(|out| !out.trim().is_empty())
                .unwrap_or(false);
            if !resolves_locally {
                let remote_match = self
                    .raw(
                        &context.repo_root,
                        &[
                            "for-each-ref".into(),
                            "--count=1".into(),
                            "--format=%(refname:short)".into(),
                            format!("refs/remotes/*/{}", base_ref),
                        ],
                    )
                    .await
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if !remote_match.is_empty() {
                    resolved_base = remote_match;
                }
            }
        }

        self.assert_range_refs_resolve(&context.repo_root, &[&resolved_base, &head_ref])
            .await?;

        let mut args: Vec<String> = vec!["diff".into(), "--no-color".into(), NO_EXT_DIFF.into()];
        args.push(format!("-U{}", context_lines.max(0)));
        args.push(format!("{}...{}", resolved_base, head_ref));
        if let Some(file_path) = file_path.filter(|p| !p.is_empty()) {
            let fc = self
                .resolve_git_file_context(&context.directory_path, &context.repo_root, file_path)
                .await?;
            args.push("--".into());
            args.push(fc.repo_path);
        }
        self.raw(&context.repo_root, &args)
            .await
            .map_err(|f| f.message.trim().to_string())
    }

    async fn assert_range_refs_resolve(
        &self,
        repo_root: &Path,
        refs: &[&str],
    ) -> ServiceResult<()> {
        for reference in refs {
            let resolves = self
                .raw(
                    repo_root,
                    &[
                        "rev-parse".into(),
                        "--verify".into(),
                        "--quiet".into(),
                        format!("{}^{{commit}}", reference),
                    ],
                )
                .await
                .map(|out| !out.trim().is_empty())
                .unwrap_or(false);
            if !resolves {
                return Err(format!(
                    "Ref \"{}\" is not available locally. Fetch it before comparing.",
                    reference
                ));
            }
        }
        Ok(())
    }

    pub async fn get_branch_base(&self, directory: &str, branch: &str) -> ServiceResult<Value> {
        let branch_name = branch.trim().to_string();
        if branch_name.is_empty() {
            return Err("branch is required".to_string());
        }
        let context = self.repository_context(directory).await?;
        let reflog = match self
            .raw(
                &context.repo_root,
                &[
                    "reflog".into(),
                    "show".into(),
                    "--format=%gs".into(),
                    branch_name.clone(),
                ],
            )
            .await
        {
            Ok(out) => out,
            Err(_) => return Ok(json!({ "base": null })),
        };
        let Some(source) = parse_branch_creation_source(&reflog) else {
            return Ok(json!({ "base": null }));
        };
        if source == branch_name {
            return Ok(json!({ "base": null }));
        }
        let resolves = self
            .raw(
                &context.repo_root,
                &[
                    "rev-parse".into(),
                    "--verify".into(),
                    "--quiet".into(),
                    source.clone(),
                ],
            )
            .await
            .map(|out| !out.trim().is_empty())
            .unwrap_or(false);
        if !resolves {
            return Ok(json!({ "base": null }));
        }
        Ok(json!({ "base": source }))
    }

    pub async fn get_range_files(
        &self,
        directory: &str,
        base: &str,
        head: &str,
    ) -> ServiceResult<Vec<Value>> {
        let context = self.repository_context(directory).await?;
        let base_ref = base.trim().to_string();
        let head_ref = head.trim().to_string();
        if base_ref.is_empty() || head_ref.is_empty() {
            return Err("base and head are required".to_string());
        }

        let mut resolved_base = base_ref.clone();
        let origin_candidate = format!("refs/remotes/origin/{}", base_ref);
        let verified = self
            .raw(
                &context.repo_root,
                &["rev-parse".into(), "--verify".into(), origin_candidate],
            )
            .await
            .map(|out| !out.trim().is_empty())
            .unwrap_or(false);
        if verified {
            resolved_base = format!("origin/{}", base_ref);
        }

        self.assert_range_refs_resolve(&context.repo_root, &[&resolved_base, &head_ref])
            .await?;

        let raw = self
            .raw(
                &context.repo_root,
                &[
                    "diff".into(),
                    "--name-status".into(),
                    "-z".into(),
                    "-C".into(),
                    format!("{}...{}", resolved_base, head_ref),
                ],
            )
            .await
            .map_err(|f| f.message.trim().to_string())?;
        let tokens: Vec<&str> = raw.split('\0').collect();
        let mut files = Vec::new();
        let mut index = 0;
        while index < tokens.len() {
            let status = tokens[index].trim().to_string();
            if status.is_empty() {
                index += 1;
                continue;
            }
            let is_rename_or_copy = status.starts_with('R') || status.starts_with('C');
            let path = if is_rename_or_copy {
                index += 2;
                tokens
                    .get(index)
                    .map(|t| t.trim().to_string())
                    .unwrap_or_default()
            } else {
                index += 1;
                tokens
                    .get(index)
                    .map(|t| t.trim().to_string())
                    .unwrap_or_default()
            };
            index += 1;
            if !path.is_empty() {
                files.push(json!({ "path": path, "status": status.chars().next().unwrap_or('M') }));
            }
        }
        Ok(files)
    }

    pub async fn get_file_diff(
        &self,
        directory: &str,
        file_path: &str,
        staged: bool,
    ) -> ServiceResult<Value> {
        if directory.trim().is_empty() || file_path.trim().is_empty() {
            return Err("directory and path are required for getFileDiff".to_string());
        }
        let context = self.repository_context(directory).await?;
        let is_image = is_image_file(file_path);
        let mime = if is_image {
            super::paths::image_mime_type(file_path)
        } else {
            ""
        };
        let fc = self
            .resolve_git_file_context(&context.directory_path, &context.repo_root, file_path)
            .await?;

        if !is_image && !fc.is_symbolic_link {
            let sniffed = looks_binary_by_sniff(&fc.absolute_path).await;
            let by_git = self
                .is_binary_diff(&context.repo_root, &fc.repo_path, staged)
                .await;
            if sniffed || by_git {
                return Ok(json!({
                    "original": "",
                    "modified": "",
                    "path": file_path,
                    "isBinary": true,
                }));
            }
        }

        let mut original = String::new();
        if is_image {
            let result = self
                .run(
                    &context.repo_root,
                    &["show", &format!("HEAD:{}", fc.repo_path)],
                )
                .await;
            if result.success && !result.stdout_bytes.is_empty() {
                original = format!(
                    "data:{};base64,{}",
                    mime,
                    base64::engine::general_purpose::STANDARD.encode(&result.stdout_bytes)
                );
            }
        } else {
            original = self
                .raw(
                    &context.repo_root,
                    &["show".into(), format!("HEAD:{}", fc.repo_path)],
                )
                .await
                .unwrap_or_default();
        }

        let mut modified = String::new();
        if staged {
            if is_image {
                let result = self
                    .run(&context.repo_root, &["show", &format!(":{}", fc.repo_path)])
                    .await;
                if result.success && !result.stdout_bytes.is_empty() {
                    modified = format!(
                        "data:{};base64,{}",
                        mime,
                        base64::engine::general_purpose::STANDARD.encode(&result.stdout_bytes)
                    );
                }
            } else {
                modified = self
                    .raw(
                        &context.repo_root,
                        &["show".into(), format!(":{}", fc.repo_path)],
                    )
                    .await
                    .unwrap_or_default();
            }
        } else if fc.is_symbolic_link {
            modified = tokio::fs::read_link(&fc.absolute_path)
                .await
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .to_string();
        } else {
            let stat = tokio::fs::metadata(&fc.absolute_path).await;
            match stat {
                Ok(stat) if !stat.is_file() => {
                    return Ok(json!({
                        "original": original.replace("\r\n", "\n"),
                        "modified": "",
                        "path": file_path,
                        "isBinary": false,
                    }));
                }
                Ok(_meta) => {
                    if is_image {
                        let buffer = tokio::fs::read(&fc.absolute_path)
                            .await
                            .map_err(|e| e.to_string())?;
                        modified = format!(
                            "data:{};base64,{}",
                            mime,
                            base64::engine::general_purpose::STANDARD.encode(&buffer)
                        );
                    } else {
                        modified = tokio::fs::read_to_string(&fc.absolute_path)
                            .await
                            .map_err(|e| e.to_string())?;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    modified = String::new();
                }
                Err(error) => return Err(error.to_string()),
            }
        }

        Ok(json!({
            "original": original.replace("\r\n", "\n"),
            "modified": modified.replace("\r\n", "\n"),
            "path": file_path,
            "isBinary": false,
        }))
    }

    async fn is_binary_diff(&self, repo_root: &Path, repo_path: &str, staged: bool) -> bool {
        let mut args: Vec<String> = vec!["diff".into(), "--numstat".into()];
        if staged {
            args.push("--cached".into());
        }
        args.extend(["--".into(), repo_path.to_string()]);
        let result = self.run_strings(repo_root, &args).await;
        if parse_is_binary_from_numstat(&result.stdout) {
            return true;
        }
        if !staged {
            let tracked = self
                .run(repo_root, &["ls-files", "--error-unmatch", "--", repo_path])
                .await
                .success;
            if !tracked {
                let no_index = self
                    .run(
                        repo_root,
                        &[
                            "diff",
                            "--no-index",
                            "--numstat",
                            "--",
                            "/dev/null",
                            repo_path,
                        ],
                    )
                    .await;
                let combined_text = format!(
                    "{}\n{}\n{}",
                    no_index.stdout, no_index.stderr, no_index.message
                )
                .to_lowercase();
                if parse_is_binary_from_numstat(&no_index.stdout)
                    || parse_is_binary_from_numstat(&no_index.stderr)
                    || parse_is_binary_from_numstat(&no_index.message)
                {
                    return true;
                }
                if combined_text.contains("binary files")
                    || combined_text.contains("git binary patch")
                {
                    return true;
                }
            }
        }
        false
    }

    // -----------------------------------------------------------------------
    // Status parsing (simple-git StatusSummary port)
    // -----------------------------------------------------------------------

    pub(crate) async fn parse_status(&self, cwd: &Path, extra_args: &[&str]) -> StatusSummary {
        let mut argv: Vec<String> = vec![
            "status".into(),
            "--porcelain".into(),
            "-b".into(),
            "-u".into(),
            "--null".into(),
        ];
        argv.extend(extra_args.iter().map(|s| s.to_string()));
        let output = self.run_strings(cwd, &argv).await;
        parse_status_summary(&output.stdout)
    }
}

// ---------------------------------------------------------------------------
// Shared value types
// ---------------------------------------------------------------------------

pub(crate) struct RepoContext {
    pub directory_path: PathBuf,
    pub repo_root: PathBuf,
}

pub(crate) struct FileContext {
    pub absolute_path: PathBuf,
    pub repo_path: String,
    pub is_symbolic_link: bool,
}

#[derive(Debug, Clone)]
pub struct StatusFile {
    pub path: String,
    pub index: String,
    pub working_dir: String,
}

#[derive(Debug, Clone, Default)]
pub struct StatusSummary {
    pub current: Option<String>,
    pub tracking: Option<String>,
    pub ahead: i64,
    pub behind: i64,
    pub detached: bool,
    pub files: Vec<StatusFile>,
    pub conflicted: Vec<String>,
}

pub(crate) const NOT_A_REPO_MESSAGE: &str =
    "fatal: not a git repository (or any of the parent directories): .git";

// ---------------------------------------------------------------------------
// Pure parsing helpers
// ---------------------------------------------------------------------------

fn relative_path_string(root: &Path, target: &Path) -> String {
    let relative = target.strip_prefix(root).unwrap_or(target);
    relative.to_string_lossy().to_string()
}

pub(crate) fn parse_config_null_value(stdout: &str) -> Option<String> {
    let mut value: Option<String> = None;
    for chunk in stdout.split('\0') {
        if chunk.trim().is_empty() {
            continue;
        }
        match chunk.split_once('\n') {
            Some((_, rest)) => value = Some(rest.to_string()),
            None => continue,
        }
    }
    value
}

pub(crate) fn parse_ahead_behind_counts(value: &str) -> Option<(i64, i64)> {
    let mut parts = value.split_whitespace();
    let ahead = parts.next().and_then(|v| v.parse::<i64>().ok());
    let behind = parts.next().and_then(|v| v.parse::<i64>().ok());
    match (ahead, behind) {
        (Some(a), Some(b)) => Some((a, b)),
        _ => None,
    }
}

fn accumulate_numstat(raw: &str, map: &mut serde_json::Map<String, Value>) {
    for line in raw.split('\n').map(str::trim).filter(|l| !l.is_empty()) {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 3 {
            continue;
        }
        let path = parts[2..].join("\t");
        if path.is_empty() {
            continue;
        }
        let insertions = parse_numstat_count(parts[0]);
        let deletions = parse_numstat_count(parts[1]);
        let existing = map.get(&path);
        let prior_insertions = existing
            .and_then(|v| v.get("insertions"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let prior_deletions = existing
            .and_then(|v| v.get("deletions"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        map.insert(path.clone(), json!({ "insertions": prior_insertions + insertions, "deletions": prior_deletions + deletions }));
    }
}

fn parse_numstat_count(raw: &str) -> i64 {
    if raw == "-" {
        0
    } else {
        raw.parse::<i64>().unwrap_or(0)
    }
}

pub(crate) fn parse_is_binary_from_numstat(raw: &str) -> bool {
    let text = raw.trim();
    if text.is_empty() {
        return false;
    }
    let first_line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let mut parts = first_line.split('\t');
    let added = parts.next().unwrap_or("");
    let deleted = parts.next().unwrap_or("");
    added == "-" || deleted == "-"
}

async fn looks_binary_by_sniff(absolute_path: &Path) -> bool {
    use tokio::io::AsyncReadExt;
    let Ok(mut handle) = tokio::fs::File::open(absolute_path).await else {
        return false;
    };
    let mut buffer = vec![0u8; BINARY_SNIFF_BYTES];
    match handle.read(&mut buffer).await {
        Ok(bytes_read) if bytes_read > 0 => buffer[..bytes_read].contains(&0),
        _ => false,
    }
}

/// Port of simple-git's `parseStatusSummary` for
/// `git status --porcelain -b -u --null [-uall]`.
pub(crate) fn parse_status_summary(stdout: &str) -> StatusSummary {
    let mut summary = StatusSummary::default();
    let chunks: Vec<&str> = stdout.split('\0').collect();
    let mut i = 0;
    while i < chunks.len() {
        let mut line = chunks[i].trim().to_string();
        i += 1;
        if line.is_empty() {
            continue;
        }
        if line.starts_with('R') && i < chunks.len() {
            line.push('\0');
            line.push_str(chunks[i]);
            i += 1;
        }
        push_status_line(&mut summary, &line);
    }
    summary
}

fn push_status_line(summary: &mut StatusSummary, line: &str) {
    let bytes = line.as_bytes();
    let (index, working_dir, path) = if bytes.len() > 2 && bytes[2] == b' ' {
        (
            line.get(0..1).unwrap_or(" ").to_string(),
            line.get(1..2).unwrap_or(" ").to_string(),
            line.get(3..).unwrap_or("").to_string(),
        )
    } else if bytes.len() > 1 && bytes[1] == b' ' {
        (
            " ".to_string(),
            line.get(0..1).unwrap_or(" ").to_string(),
            line.get(2..).unwrap_or("").to_string(),
        )
    } else {
        return;
    };

    let raw = format!("{}{}", index, working_dir);
    if raw == "##" {
        parse_status_branch_header(summary, &path);
        return;
    }
    if raw == "!!" {
        return;
    }

    if matches!(raw.as_str(), "DD" | "AU" | "UD" | "UA" | "DU" | "AA" | "UU") {
        summary.conflicted.push(path.clone());
    }

    let mut entry_path = path;
    if (index == "R" || working_dir == "R")
        && let Some((to, from)) = entry_path.split_once('\0')
    {
        let _ = from;
        entry_path = to.to_string();
    }
    summary.files.push(StatusFile {
        path: entry_path,
        index,
        working_dir,
    });
}

fn parse_status_branch_header(summary: &mut StatusSummary, rest: &str) {
    // `rest` is everything after "## " (already sliced by the caller).
    let line = rest.trim_start_matches(' ');
    summary.ahead = extract_count_after(line, "ahead ").unwrap_or(0);
    summary.behind = extract_count_after(line, "behind ").unwrap_or(0);

    // current: up to "..." or whitespace.
    let current_end = line
        .find("...")
        .or_else(|| line.find(' '))
        .unwrap_or(line.len());
    let current_candidate = &line[..current_end];
    if !current_candidate.is_empty() {
        summary.current = Some(current_candidate.to_string());
    }

    // tracking: after "..." up to whitespace.
    if let Some(pos) = line.find("...") {
        let tail = &line[pos + 3..];
        let tracking_end = tail.find(' ').unwrap_or(tail.len());
        let tracking = &tail[..tracking_end];
        if !tracking.is_empty() {
            summary.tracking = Some(tracking.to_string());
        }
    }

    // "No commits yet on <branch>" / "(no branch)" handling.
    if let Some(on_pos) = line.find(" on ") {
        let tail = &line[on_pos + 4..];
        let end = tail.find("...").unwrap_or(tail.len());
        let branch = &tail[..end];
        if !branch.is_empty() {
            summary.current = Some(branch.to_string());
        }
    }
    if line.contains("(no branch)") {
        summary.detached = true;
    }
}

fn extract_count_after(line: &str, marker: &str) -> Option<i64> {
    let pos = line.find(marker)?;
    let tail = &line[pos + marker.len()..];
    let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<i64>().ok()
}

/// JS `parseBranchCreationSource` — oldest "branch: Created from <src>" entry
/// whose source is a named ref (not bare/positional HEAD, not a raw hash).
pub fn parse_branch_creation_source(reflog_text: &str) -> Option<String> {
    let lines: Vec<&str> = reflog_text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    for line in lines.iter().rev() {
        let Some(rest) = line.strip_prefix("branch: Created from ") else {
            continue;
        };
        let source = rest.trim();
        if source.is_empty() {
            return None;
        }
        let is_head = source == "HEAD" || source.starts_with("HEAD@");
        let is_hash = source.len() >= 7
            && source.len() <= 40
            && source.chars().all(|c| c.is_ascii_hexdigit());
        if is_head || is_hash {
            return None;
        }
        return Some(source.to_string());
    }
    None
}

pub fn resolve_base_ref_for_log(
    from: Option<&str>,
    check_ref: impl Fn(String) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + 'static,
) -> Pin<Box<dyn Future<Output = Option<String>> + Send>> {
    let from = from
        .map(|value| value.trim().to_string())
        .filter(|v| !v.is_empty());
    Box::pin(async move {
        let Some(from) = from else { return None };
        if check_ref(from.clone()).await {
            return Some(from);
        }
        let origin_ref = format!("refs/remotes/origin/{}", from);
        if check_ref(origin_ref).await {
            return Some(format!("origin/{}", from));
        }
        Some(from)
    })
}

/// JS `new Date().toISOString()` — UTC ISO-8601 with millisecond precision.
pub fn iso_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let millis = now.as_millis() as i64;
    let (seconds, ms_part) = (millis / 1000, millis % 1000);
    let days = seconds.div_euclid(86_400);
    let secs_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year,
        month,
        day,
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
        ms_part
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
