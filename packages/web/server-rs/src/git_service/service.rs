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
//!
//! 中文说明：本模块是 git 服务的核心，移植自旧 JS server 的
//! `server/lib/git/service.js` 导出函数族，支撑 `/api/git/*` 路由。
//! 本文件承载：git 命令执行封装（对齐 simple-git 的 `git.raw` 失败
//! 语义）、按仓库根串行化的 index 变更队列、remote 存在性缓存
//! （30 秒 TTL）、worktree 引导状态表、status/diff 查询 API 以及一组
//! 纯解析辅助函数；提交、日志、分支、stash、push/pull 等其余操作在
//! 同模块的 service_ops.rs / worktrees.rs 等文件的 `impl GitService`
//! 块中扩展。

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

/// 服务层统一返回类型：成功值 T，或携带人类可读错误消息的 Err；
/// 错误消息字符串会直接透传给 `/api/git/*` 的客户端。
pub type ServiceResult<T> = Result<T, String>;

/// 附加在所有 diff 调用上的固定参数 `--no-ext-diff`：
/// 禁用 diff.external / gitattributes 配置的外部 diff 驱动，
/// 保证输出格式稳定可解析。
pub(crate) const NO_EXT_DIFF: &str = "--no-ext-diff";
/// remote 存在性缓存的存活时长（对应 JS 侧的 30 秒 TTL），
/// 用于抑制对 `git remote get-url` 的重复探测。
const REMOTE_EXISTENCE_CACHE_TTL: Duration = Duration::from_secs(30);
/// 二进制内容嗅探最多读取的字节数：只检查文件开头这些字节中是否含 NUL。
const BINARY_SNIFF_BYTES: usize = 8192;
/// get_status 为新增文件补充行数统计时最多处理的文件条数，超出即截断。
const MAX_NEW_FILE_STATS: usize = 200;
/// 参与新增文件行数统计的单文件大小上限（1 MiB），
/// 更大的文件跳过统计以避免整文件读取开销。
const MAX_NEW_FILE_STAT_SIZE: u64 = 1024 * 1024;

// ---------------------------------------------------------------------------
// Module state (JS module-scope maps)
// ---------------------------------------------------------------------------

/// worktree 引导（bootstrap）任务的进度快照，
/// 对应 JS 模块级 `worktreeBootstrapState` Map 中每个仓库的条目值。
#[derive(Debug, Clone)]
pub struct BootstrapState {
    /// 引导整体状态（由调用方定义的状态机值，如 running/done/error）。
    pub status: String,
    /// 当前引导阶段的人类可读描述。
    pub phase: String,
    /// 引导失败时的错误消息；尚未失败为 None。
    pub error: Option<String>,
    /// 本快照最近一次更新的 Unix 毫秒时间戳。
    pub updated_at_ms: u64,
}

/// git 服务主体：组合底层命令执行器与一组对应 JS 模块级可变状态的
/// 内部表（串行队列、缓存、引导状态），是 `/api/git/*` 的实现核心。
pub struct GitService {
    /// 底层 git 命令执行器（真实进程实现或测试注入），以 Arc 共享。
    runner: GitRunner,
    /// JS `gitIndexMutationQueues` — one serialized queue per repository root.
    /// 按仓库根目录键的 tokio 互斥锁表：同一仓库的 index 变更排队串行执行。
    pub(crate) queues: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
    /// JS `remoteExistenceCache` (`REMOTE_EXISTENCE_CACHE_TTL_MS` = 30s).
    /// remote 存在性缓存，键为 归一化目录 + NUL + remote 名，
    /// 值为 (是否存在, 探测时刻)。
    remote_existence_cache: Mutex<HashMap<String, (bool, Instant)>>,
    /// JS `worktreeBootstrapState`.
    /// 各仓库的 worktree 引导进度快照表。
    pub(crate) bootstrap_state: Mutex<HashMap<PathBuf, BootstrapState>>,
    /// JS `activeWorktreeBootstrapTasks` — a held lock marks an active task.
    /// 活跃引导任务表：条目持有的锁被占用即表示该仓库引导正在进行。
    pub(crate) active_bootstrap: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
}

/// Task body accepted by [`GitService::with_index_queue`]: receives the
/// service by shared reference for the queued duration.
/// 中文补充：任务体在持锁期间运行，生命周期绑定对服务的借用期，
/// 返回 `ServiceResult<T>`。
pub(crate) type QueuedTask<'a, T> = Pin<Box<dyn Future<Output = ServiceResult<T>> + Send + 'a>>;

/// 当前 Unix epoch 毫秒时间戳；系统时钟早于 epoch 时回退为 0。
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 默认实现：与 `GitService::new` 等价，使用真实 git 进程执行器。
impl Default for GitService {
    /// 委托给 `GitService::new`，二者完全等价。
    fn default() -> Self {
        Self::new()
    }
}

/// 主实现块：命令执行封装、按仓库的串行队列、仓库/文件上下文解析，
/// 以及 status/diff 查询等 API；其余操作见 service_ops.rs 等
/// 同模块文件中的其它 `impl GitService` 块。
impl GitService {
    /// 使用真实 git 进程执行器构造服务实例。
    pub fn new() -> Self {
        Self::with_runner(real_runner())
    }

    /// 使用注入的执行器构造服务，测试可借此替换为确定性的模拟实现。
    pub fn with_runner(runner: GitRunner) -> Self {
        Self {
            runner,
            queues: Mutex::new(HashMap::new()),
            remote_existence_cache: Mutex::new(HashMap::new()),
            bootstrap_state: Mutex::new(HashMap::new()),
            active_bootstrap: Mutex::new(HashMap::new()),
        }
    }

    /// 克隆返回底层执行器（Arc 共享，开销低）。
    pub(crate) fn runner(&self) -> GitRunner {
        Arc::clone(&self.runner)
    }

    /// 在指定目录执行 git 子命令（无附加环境变量），
    /// 原样返回执行结果，不在此处判定成败。
    pub(crate) async fn run(&self, cwd: impl AsRef<Path>, args: &[&str]) -> GitCommandResult {
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        (self.runner)(cwd.as_ref().to_path_buf(), argv, HashMap::new()).await
    }

    /// `run` 的带环境变量版本：env 会整体传给执行器
    /// （例如注入 GIT_SSH_COMMAND 指定私钥）。
    pub(crate) async fn run_with_env(
        &self,
        cwd: impl AsRef<Path>,
        args: &[&str],
        env: HashMap<String, String>,
    ) -> GitCommandResult {
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        (self.runner)(cwd.as_ref().to_path_buf(), argv, env).await
    }

    /// `run` 的参数变体：直接接受 `&[String]`，避免调用方先借用再转换。
    pub(crate) async fn run_strings(
        &self,
        cwd: impl AsRef<Path>,
        args: &[String],
    ) -> GitCommandResult {
        (self.runner)(cwd.as_ref().to_path_buf(), args.to_vec(), HashMap::new()).await
    }

    /// simple-git `git.raw` semantics: `Err` only when git exited non-zero
    /// with stderr output; the failure message is stdout+stderr.
    /// 中文补充：失败消息为 stdout 与 stderr 的直接拼接；
    /// 退出非零但 stderr 为空时仍按成功返回 stdout。
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

    /// `raw` 的带环境变量版本：失败判定与消息拼接语义完全一致。
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
    /// 中文补充：消息选择顺序为 命令输出 message → fallback_message
    /// → 兜底 "Git command failed"。
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
    /// 中文补充：队列键为目录所属仓库根；任务结束后若无其它等待者
    /// 则移除表项，防止队列 Map 无限增长。
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

    /// 计算串行队列键：目录归一化后解析所属仓库根，
    /// 解析失败时退回目录绝对路径；目录无效返回 None（调用方不排队直接执行）。
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

    /// 用 `rev-parse --show-toplevel` 解析仓库根并绝对化；
    /// 输出为空（不在仓库内）时返回 Err("Git directory is required")，
    /// 相对输出则相对 directory_path 归一化。
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

    /// 校验并归一化目录、解析仓库根，打包为 RepoContext；
    /// 是绝大多数公开 API 的公共前置步骤，目录无效时报错。
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
    /// 中文补充：命令失败时以空输出继续（解析结果即仓库根本身），
    /// 调用方按“文件不存在”处理；结果一律绝对化。
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
    /// 中文补充：候选顺序为 仓库根/路径、目录/路径，且必须位于仓库内；
    /// 工作区中须是普通文件或符号链接。
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
    /// 中文补充：探测命令为 `remote get-url <name>`（输出非空即存在），
    /// 结果按 归一化目录 + NUL + remote 名 缓存 REMOTE_EXISTENCE_CACHE_TTL。
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
    /// 中文补充：返回 `show-ref --verify <ref>` 是否执行成功，
    /// 用于校验具名引用/分支是否存在于本地。
    pub(crate) async fn git_ref_exists(&self, cwd: &Path, reference: &str) -> bool {
        self.run(cwd, &["show-ref", "--verify", reference])
            .await
            .success
    }

    /// JS `getConfig(key, scope)` → last configured value.
    /// 中文补充：底层命令为 `config --<scope> --null --get-all <key>`，
    /// 作用域取值如 local/global；键不存在返回 None。
    pub async fn get_config(&self, cwd: &Path, key: &str, scope: &str) -> Option<String> {
        // simple-git getConfig: `config --<scope> --null --get-all <key>`;
        // the last configured value wins.
        let mut args = vec!["config".to_string(), format!("--{}", scope)];
        args.extend(["--null", "--get-all", key].iter().map(|s| s.to_string()));
        let output = self.run_strings(cwd, &args).await;
        parse_config_null_value(&output.stdout)
    }

    /// 写入配置项（`config --<scope> <key> <value>`）；
    /// 失败时错误消息固定为 "Failed to set config"。
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

    /// 判定目录是否位于 git 仓库：目录串非空、路径存在
    /// 且 `rev-parse --git-dir` 执行成功三者缺一不可。
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

    /// 读取 HOME 目录下的 --global 配置（user.name、user.email、
    /// core.sshCommand），以 JSON 对象返回；缺失的键为 null。
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

    /// 读取指定 remote 的 URL（`remote get-url`）；
    /// 目录无效、命令失败或输出为空均返回 None。
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

    /// 读取实际生效的身份：每个键先查仓库 local 配置，缺失时回退
    /// HOME 下的 global 配置；返回字段与 get_global_identity 相同。
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

    /// 判定仓库是否配置了本地身份：
    /// user.name 或 user.email 任一存在于 --local 配置即返回 true。
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

    /// 把身份配置档写入仓库本地配置：始终设置 user.name/user.email；
    /// authType=ssh（默认）时写 core.sshCommand 并清除 credential.helper，
    /// authType=token 时反向（写 credential.helper=store、清 core.sshCommand）；
    /// signCommits 开启且给出 signingKey 时再配置 gpg.format=ssh、
    /// user.signingkey 与 commit.gpgsign。成功返回 true。
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

    /// 获取仓库状态概览（`/api/git/status` 的实现）。
    /// mode="light" 时跳过 diff 统计与 upstream 比较；完整模式额外做四件事：
    /// 合并 staged/working 两份 numstat 得到 diffStats；为状态码 ?/A 且
    /// 尚无插入统计的新文件读取内容计算行数（受 MAX_NEW_FILE_STATS 条数与
    /// MAX_NEW_FILE_STAT_SIZE 大小限制，含 NUL 字节的按二进制保留 0 计数）；
    /// 无上游跟踪时用基准引用估算 ahead；存在 upstream remote 时附带
    /// upstreamComparison。同时附带 mergeInProgress / rebaseInProgress。
    /// 目录不是仓库时返回 NOT_A_REPO_MESSAGE。
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

    /// 为没有上游跟踪的分支挑选“未发布提交数”的基准引用：
    /// 候选依次为 origin/HEAD 指向的分支、origin/main、origin/master、
    /// main、master，返回第一个本地可解析的引用；都不可用返回 None。
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

    /// 统计 HEAD 相对 refs/remotes/<remote>/<branch> 的领先/落后数量
    /// （`rev-list --left-right --count HEAD...ref`）；
    /// 远端引用本地不存在时返回 None。
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

    /// 检测 merge 进行中：MERGE_HEAD 可解析时读取其 7 位短 SHA 与
    /// MERGE_MSG 首行，返回 { head, message }；否则返回 Null。
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

    /// 检测 rebase 进行中：仓库内存在 rebase-merge 或 rebase-apply 目录时，
    /// 读取其 head-name/onto 文件并返回 { headName, onto }
    /// （分支名去掉 refs/heads/ 前缀，onto 取 7 位短 SHA）；否则返回 Null。
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

    /// 获取统一 diff 文本。staged 为真时附加 --cached；指定文件时先解析
    /// FileContext 并限定路径；context_lines 映射为 -U<n>（负数按 0 处理）。
    /// 对 diff 为空的未跟踪文件回退：符号链接手工合成 mode 120000 的
    /// 新文件 diff，其余文件用 `--no-index /dev/null <path>` 生成
    /// （该命令退出码 1 且有输出时，输出本身就是 diff）。
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

    /// 列出未被 ignore 规则排除的未跟踪文件
    /// （`ls-files --others --exclude-standard`）；命令失败时静默返回空列表。
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

    /// 批量生成未跟踪文件相对 /dev/null 的新文件 diff；
    /// 结果顺序与输入路径一一对应，单个文件失败时对应位置为空字符串。
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

    /// 获取 base...head 的三点 diff。base 的解析增强顺序：
    /// refs/remotes/origin/<base> 可用则改写为 origin/<base>；否则（base
    /// 不含通配/范围字符时）用 for-each-ref 在 refs/remotes/*/ 下模糊匹配；
    /// 随后校验两端引用本地可解析，再执行 `diff -U<n> base...head [-- path]`。
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

    /// 逐个校验引用能解析到 commit（`rev-parse --verify --quiet <ref>^{commit}`）；
    /// 任一失败返回 "Ref ... is not available locally. Fetch it before comparing."。
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

    /// 从分支 reflog 推断其创建来源作为 base：取最旧的
    /// "branch: Created from <src>" 条目（来源须为具名引用）；
    /// 来源等于分支自身、当前不可解析或 reflog 不可读时返回 { base: null }。
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

    /// 列出 base...head 的变更文件（`diff --name-status -z -C`，
    /// 开启重命名/复制检测）：解析 NUL 分隔的记录，R/C 状态会跳过
    /// 相似度得分与旧路径、取新路径，返回 [{ path, status }]，
    /// status 取首字母（缺省 M）。
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

    /// 获取单文件差异的两侧内容。图片文件以 data:<mime>;base64 形式返回
    /// HEAD 与（staged 时）index 中的版本；非图片先做二进制判定
    /// （内容嗅探 + git numstat），命中则返回 isBinary=true 且内容为空；
    /// 符号链接的 modified 为链接目标路径；普通文件直接读工作区内容，
    /// 文件不存在视为已删除（modified 为空）。两侧内容统一把 CRLF 规范为 LF。
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

    /// 判定指定文件的 diff 是否为二进制：先看 numstat 是否给出 `-` 计数；
    /// 未跟踪文件再跑 `--no-index --numstat`，并在其 stdout/stderr/message
    /// 中查找 `-` 计数或 "binary files"/"git binary patch" 字样。
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

    /// 执行 `status --porcelain -b -u --null`（附加 extra_args，
    /// 如 -uall）并把 stdout 解析为 StatusSummary；
    /// 执行失败时 stdout 为空，自然得到空 summary。
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

/// 一次仓库操作的定位信息：调用目录与仓库根。
pub(crate) struct RepoContext {
    /// 调用方传入的工作目录（已校验非空并归一化）。
    pub directory_path: PathBuf,
    /// `rev-parse --show-toplevel` 解析出的仓库根绝对路径。
    pub repo_root: PathBuf,
}

/// 解析后的仓库内文件定位信息。
pub(crate) struct FileContext {
    /// 文件在工作区中的绝对路径。
    pub absolute_path: PathBuf,
    /// 相对仓库根、传给 git 命令使用的路径（已转为 POSIX 分隔符）。
    pub repo_path: String,
    /// 是否为符号链接；符号链接的“内容”按链接目标路径处理。
    pub is_symbolic_link: bool,
}

/// porcelain 输出中单个文件的状态条目。
#[derive(Debug, Clone)]
pub struct StatusFile {
    /// 仓库相对路径；重命名记录取 NUL 分隔符前的新路径。
    pub path: String,
    /// index（暂存区）状态码，即 XY 对中的 X；空格表示该侧无变化。
    pub index: String,
    /// 工作区状态码，即 XY 对中的 Y；空格表示该侧无变化。
    pub working_dir: String,
}

/// `git status --porcelain -b -u --null` 的解析结果，
/// 字段语义对齐 simple-git 的 StatusSummary。
#[derive(Debug, Clone, Default)]
pub struct StatusSummary {
    /// 当前分支名；分支头未提供分支信息时为 None。
    pub current: Option<String>,
    /// 上游跟踪分支（分支头 "..." 之后的部分）；无跟踪关系时为 None。
    pub tracking: Option<String>,
    /// 领先上游的提交数；分支头未给出时为 0。
    pub ahead: i64,
    /// 落后上游的提交数；分支头未给出时为 0。
    pub behind: i64,
    /// 是否处于 detached HEAD（分支头包含 "(no branch)"）。
    pub detached: bool,
    /// 全部未被忽略文件的状态条目（含未跟踪与冲突文件）。
    pub files: Vec<StatusFile>,
    /// 处于冲突合并状态的文件路径列表（状态码 DD/AU/UD/UA/DU/AA/UU）。
    pub conflicted: Vec<String>,
}

/// 目录不是 git 仓库时返回给客户端的标准错误文本；
/// 刻意与 git 自身的 fatal 输出保持一致，客户端据此识别该场景。
pub(crate) const NOT_A_REPO_MESSAGE: &str =
    "fatal: not a git repository (or any of the parent directories): .git";

// ---------------------------------------------------------------------------
// Pure parsing helpers
// ---------------------------------------------------------------------------

/// 计算 target 相对 root 的路径字符串；target 不在 root 之下时
/// 退化为 target 自身的字符串（损失式转换）。
fn relative_path_string(root: &Path, target: &Path) -> String {
    let relative = target.strip_prefix(root).unwrap_or(target);
    relative.to_string_lossy().to_string()
}

/// 解析 `config --null --get-all` 的输出：按 NUL 分段，取最后一个非空段；
/// 没有任何值时返回 None（调用方据此映射为 JSON null）。
pub(crate) fn parse_config_null_value(stdout: &str) -> Option<String> {
    // `--null --get-all` emits value\0value\0…; the last configured value
    // wins. An empty final value is an empty string (caller maps to null).
    stdout
        .split('\0')
        .filter(|chunk| !chunk.is_empty())
        .next_back()
        .map(|value| value.to_string())
}

/// 解析 `rev-list --left-right --count` 的 "ahead behind" 两段输出为数值对；
/// 不足两段或任一段非数字时返回 None。
pub(crate) fn parse_ahead_behind_counts(value: &str) -> Option<(i64, i64)> {
    let mut parts = value.split_whitespace();
    let ahead = parts.next().and_then(|v| v.parse::<i64>().ok());
    let behind = parts.next().and_then(|v| v.parse::<i64>().ok());
    match (ahead, behind) {
        (Some(a), Some(b)) => Some((a, b)),
        _ => None,
    }
}

/// 把一份 numstat 输出按路径累加进统计 Map（同一路径再次出现时
/// insertions/deletions 与已有值相加），供 staged 与 working
/// 两份输出合并成完整 diffStats。
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

/// 解析 numstat 的单个计数字段：`-`（二进制标记）或解析失败时返回 0。
fn parse_numstat_count(raw: &str) -> i64 {
    if raw == "-" {
        0
    } else {
        raw.parse::<i64>().unwrap_or(0)
    }
}

/// 从 numstat 风格的输出判定二进制：首个非空行的增加数或删除数列
/// 为 `-` 即视为二进制；空输出返回 false。
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

/// 内容嗅探判定二进制：读取文件前 BINARY_SNIFF_BYTES 字节，
/// 其中出现 NUL 字节即判定为二进制；打开或读取失败按非二进制处理。
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
/// 中文补充：输出按 NUL 分段逐条解析；以 R 开头的分段会与其后一段
/// （旧路径）拼成一条记录后再交给 push_status_line。
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

/// 解析单条 porcelain 记录（XY 状态码 + 路径）：XY 前缀非法的行被丢弃；
/// `##` 行转交分支头解析；`!!`（被忽略文件）不记录；冲突状态码的路径
/// 额外计入 conflicted；重命名记录取 NUL 分隔符前的新路径。
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

/// 解析分支头（`## ` 之后的内容）：提取 ahead/behind 计数、
/// current/tracking 分支名，并处理 "No commits yet on <branch>"
/// 与 "(no branch)"（detached HEAD）两种特殊形态。
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

/// 提取 line 中 marker 之后紧随的连续 ASCII 数字并解析为整数；
/// marker 不存在或其后没有数字时返回 None。
fn extract_count_after(line: &str, marker: &str) -> Option<i64> {
    let pos = line.find(marker)?;
    let tail = &line[pos + marker.len()..];
    let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<i64>().ok()
}

/// JS `parseBranchCreationSource` — oldest "branch: Created from <src>" entry
/// whose source is a named ref (not bare/positional HEAD, not a raw hash).
/// 中文补充：reflog 输出最新条目在前，此处反向（从最旧条目）扫描，
/// 返回首个命中的来源；来源为空串时直接返回 None。
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

/// 解析日志查询的起点引用：`from` 为空返回 None；否则依次探测 `from`
/// 原值、`refs/remotes/origin/<from>`（命中则改写为 `origin/<from>`）；
/// 都未命中时原样返回 `from`，交给后续 git 命令自行报错。
/// `check_ref` 为异步的引用存在性探测回调。
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
/// 中文补充：由 epoch 毫秒手工换算得到（日期部分见 civil_from_days），
/// 不引入 chrono 依赖。
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

/// 把自 1970-01-01 起的天数换算为公历 (年, 月, 日)：
/// Howard Hinnant 的 civil_from_days 算法，纯整数运算并正确处理闰年。
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
