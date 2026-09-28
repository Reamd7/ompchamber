//! Worktree management — port of `getWorktrees` / `validateWorktreeCreate` /
//! `previewWorktreeCreate` / `createWorktree` / `removeWorktree` /
//! `getWorktreeBootstrapStatus` and the bootstrap pipeline
//! (`queueWorktreeBootstrap`, `runPostCheckoutHook`, `populateWorktree…`)
//! from `server/lib/git/service.js`.
//!
//! These are free functions over `Arc<GitService>` because the fast-create
//! path spawns a detached bootstrap task that must own its service handle.
//!
//! 中文说明:worktree(git 工作树)管理模块,完整移植自旧 JS server 的
//! `server/lib/git/service.js`。覆盖 worktree 的列出、创建校验、预览、创建
//! (含后台引导流水线)、删除与引导状态查询。bootstrap 状态以内存表
//! (per-directory)维护,并用 per-directory 锁把后台任务与删除操作串行化,
//! 保证 remove 不会与进行中的引导竞争。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Map, Value, json};

use super::exec::{GitCommandResult, parse_git_error_text};
use super::paths::{
    OPENCODE_WORKTREE_ATTEMPTS, absolutize, canonical_path, check_path_exists, clean_branch_name,
    generate_opencode_random_name, is_inside_or_same_directory, normalize_directory_path,
    opencode_data_path, parse_remote_branch_ref, parse_worktree_porcelain, slug_worktree_name,
};
use super::service::{GitService, ServiceResult};

/// bootstrap 状态值 "pending":后台引导任务仍在进行,配合 phase 表示当前阶段。
const BOOTSTRAP_PENDING: &str = "pending";
/// bootstrap 状态值 "ready":引导全部完成,worktree 可正常使用。
const BOOTSTRAP_READY: &str = "ready";
/// bootstrap 状态值 "failed":引导中途失败,状态里的 error 字段携带原因文本。
const BOOTSTRAP_FAILED: &str = "failed";
/// bootstrap 阶段 "directory-created":目录已创建,checkout/安装尚未开始,
/// 或失败后回退到的初始阶段。
const PHASE_DIRECTORY_CREATED: &str = "directory-created";
/// bootstrap 阶段 "git-ready":checkout 与 post-checkout hook 已完成,
/// 启动脚本执行中。
const PHASE_GIT_READY: &str = "git-ready";
/// bootstrap 阶段 "setup-ready":启动脚本执行完毕,引导流程整体结束。
const PHASE_SETUP_READY: &str = "setup-ready";
/// 全零 SHA(null ref):按 git 约定作为 post-checkout hook 的第一个参数
/// (非 clone 场景下的 previous HEAD)。
const GIT_NULL_REF: &str = "0000000000000000000000000000000000000000";

/// 当前 Unix 时间戳(毫秒),用于 bootstrap 状态的 updatedAt 字段。
/// 系统时钟早于 epoch 等异常情况下回退为 0,保证不 panic。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 把目录字符串规范化为 bootstrap 状态表的键:先 normalize 再 absolutize 成
/// 绝对路径,确保同一 worktree 的不同写法命中同一条状态记录;
/// 输入为空或规范化后为空则返回 None(调用方据此跳过落表)。
fn bootstrap_key(directory: &str) -> Option<PathBuf> {
    let normalized = normalize_directory_path(Some(directory))?;
    if normalized.trim().is_empty() {
        return None;
    }
    Some(absolutize(Path::new(&normalized)))
}

/// 写入指定 worktree 目录的 bootstrap 状态,并返回等价的 JSON 快照
/// (即 API 响应中的 bootstrapStatus 对象)。副作用:更新 service.bootstrap_state
/// 内存表;error 文本去除首尾空白,为空则序列化为 null。目录键非法(None)时
/// 只返回快照、不落表。
pub(crate) fn set_bootstrap_state(
    service: &GitService,
    directory: &str,
    status: &str,
    phase: &str,
    error: Option<String>,
) -> Value {
    let key = bootstrap_key(directory);
    let state = json!({
        "status": status,
        "phase": phase,
        "error": error.filter(|e| !e.trim().is_empty()).map(|e| e.trim().to_string()),
        "updatedAt": now_ms(),
    });
    if let Some(key) = key {
        service
            .bootstrap_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                key,
                super::service::BootstrapState {
                    status: status.to_string(),
                    phase: phase.to_string(),
                    error: state
                        .get("error")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    updated_at_ms: now_ms(),
                },
            );
    }
    state
}

/// 删除目录的 bootstrap 状态记录;remove 成功或清理孤儿目录后调用,
/// 键不存在时静默通过。
fn clear_bootstrap_state(service: &GitService, directory: &str) {
    if let Some(key) = bootstrap_key(directory) {
        service
            .bootstrap_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
    }
}

/// JS `trackWorktreeBootstrapTask` / `waitForActiveWorktreeBootstrap`: the
/// background task holds a per-directory lock for its duration; removal waits
/// on that lock so it never races live bootstrap work.
/// 中文说明:后台任务在整个执行期间持有该目录的 per-directory 锁;
/// 删除操作会先等待同一把锁,因此 remove 永远不会与仍在运行的 bootstrap
/// 任务竞争。任务结束后若无他人共享锁,顺带从 active 表清理该条目;
/// 目录键非法时退化为普通 spawn(无串行化)。
fn track_bootstrap_task<F, Fut>(service: &Arc<GitService>, directory: &str, task: F)
where
    F: FnOnce(Arc<GitService>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    if let Some(key) = bootstrap_key(directory) {
        let lock = {
            let mut active = service
                .active_bootstrap
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            active
                .entry(key)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let service = Arc::clone(service);
        let task_service = Arc::clone(&service);
        tokio::spawn(async move {
            let _guard = lock.lock().await;
            task(task_service).await;
            let mut active = service
                .active_bootstrap
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            active.retain(|_, entry| Arc::strong_count(entry) > 1);
        });
    } else {
        let service = Arc::clone(service);
        let task_service = Arc::clone(&service);
        tokio::spawn(async move {
            task(task_service).await;
        });
    }
}

/// 等待该目录上正在运行的 bootstrap 任务(若有)结束:获取并持有同一把
/// per-directory 锁直至任务释放。没有活动任务或目录键非法时立即返回;
/// remove_worktree 在动手删除前调用以避免竞态。
pub(crate) async fn wait_for_active_bootstrap(service: &GitService, directory: &str) {
    let Some(key) = bootstrap_key(directory) else {
        return;
    };
    let lock = {
        let active = service
            .active_bootstrap
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match active.get(&key) {
            Some(lock) => Arc::clone(lock),
            None => return,
        }
    };
    let _guard = lock.lock().await;
}

// ---------------------------------------------------------------------------
// Project context
// ---------------------------------------------------------------------------

/// worktree 项目上下文:由任意仓库子目录推导出的项目标识与路径锚点,
/// 是创建/删除/预览 worktree 的公共前置数据。
pub(crate) struct WorktreeProjectContext {
    /// OpenCode 项目 ID(默认取仓库最早根提交的 SHA),用于按项目隔离 worktree 目录。
    pub project_id: String,
    /// 请求目录所在 git 仓库顶层(sandbox)的绝对路径。
    pub sandbox: PathBuf,
    /// 主 worktree 绝对路径(git common dir 的父目录),git 元数据命令都在这里执行。
    pub primary_worktree: PathBuf,
    /// 本项目受管 worktree 的父目录:<opencode data>/worktree/<project_id>。
    pub worktree_root: PathBuf,
}

/// 读取或生成并持久化 OpenCode 项目 ID:优先复用 .git/opencode 文件中的已有
/// 内容;缺失时用 `rev-list --max-parents=0 --all` 取排序后第一个根提交 SHA
/// 并写回文件(写失败被忽略,下次重算)。取不到根提交则返回 Err。
async fn ensure_opencode_project_id(
    service: &GitService,
    primary_worktree: &Path,
) -> ServiceResult<String> {
    let id_file = primary_worktree.join(".git").join("opencode");
    if let Ok(existing) = tokio::fs::read_to_string(&id_file).await {
        let trimmed = existing.trim().to_string();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
    }

    let roots_result = service
        .run_or_throw(
            primary_worktree,
            &["rev-list", "--max-parents=0", "--all"],
            "Failed to resolve repository roots",
        )
        .await?;
    let mut roots: Vec<String> = roots_result
        .stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    roots.sort();

    let project_id = roots.into_iter().next().unwrap_or_default();
    if project_id.is_empty() {
        return Err("Failed to derive OpenCode project ID".to_string());
    }
    let _ = tokio::fs::create_dir_all(primary_worktree.join(".git")).await;
    let _ = tokio::fs::write(&id_file, &project_id).await;
    Ok(project_id)
}

/// GitService 上与 worktree 管理相关的扩展方法(项目上下文解析)。
impl GitService {
    /// 把任意目录解析为 WorktreeProjectContext:`rev-parse --show-toplevel` 求
    /// sandbox,`rev-parse --git-common-dir` 求主 worktree,再确保 project ID 存在
    /// 并拼出 worktree 根目录。目录为空或任一 git 命令失败都返回 Err
    /// (调用方一般转成 validation_failed 错误码)。
    pub(crate) async fn resolve_worktree_project_context(
        &self,
        directory: &str,
    ) -> ServiceResult<WorktreeProjectContext> {
        let directory_path = normalize_directory_path(Some(directory))
            .filter(|v| !v.is_empty())
            .ok_or_else(|| "Directory is required".to_string())?;

        let top_result = self
            .run_or_throw(
                &directory_path,
                &["rev-parse", "--show-toplevel"],
                "Failed to resolve git top-level directory",
            )
            .await?;
        let sandbox = absolutize(&Path::new(&directory_path).join(top_result.stdout.trim()));

        let common_result = self
            .run_or_throw(
                &sandbox,
                &["rev-parse", "--git-common-dir"],
                "Failed to resolve git common directory",
            )
            .await?;
        let common_dir = absolutize(&sandbox.join(common_result.stdout.trim()));
        let primary_worktree = common_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(common_dir.clone());
        let project_id = ensure_opencode_project_id(self, &primary_worktree).await?;
        let worktree_root = opencode_data_path().join("worktree").join(&project_id);

        Ok(WorktreeProjectContext {
            project_id,
            sandbox,
            primary_worktree,
            worktree_root,
        })
    }
}

// ---------------------------------------------------------------------------
// Listing / removal / creation
// ---------------------------------------------------------------------------

/// JS getWorktrees:列出仓库全部 worktree 并映射为 { head, name, branch, path }
/// (name 取路径最后一段)。与 JS 一致地吞掉所有失败:目录缺失、非仓库、
/// git 报错一律返回空数组,因此 routes 层的列表接口恒 200。
pub async fn get_worktrees(service: &GitService, directory: &str) -> Vec<Value> {
    let Some(directory_path) = normalize_directory_path(Some(directory)).filter(|v| !v.is_empty())
    else {
        return Vec::new();
    };
    if !check_path_exists(&directory_path).await {
        return Vec::new();
    }
    let Ok(repo_root) = service.resolve_repository_root(&directory_path).await else {
        return Vec::new();
    };
    let Ok(result) = service
        .run_or_throw(
            &repo_root,
            &["worktree", "list", "--porcelain"],
            "Failed to list git worktrees",
        )
        .await
    else {
        return Vec::new();
    };
    parse_worktree_porcelain(&result.stdout)
        .into_iter()
        .map(|entry| {
            json!({
                "head": entry.head,
                "name": Path::new(&entry.worktree)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                "branch": entry.branch,
                "path": entry.worktree,
            })
        })
        .collect()
}

/// 运行 `worktree list --porcelain` 并解析为结构化条目;与 get_worktrees
/// 不同,失败时把错误经 ServiceResult 透传给调用方。
async fn list_worktree_entries(
    service: &GitService,
    directory: &str,
) -> ServiceResult<Vec<super::paths::WorktreePorcelainEntry>> {
    let result = service
        .run_or_throw(
            directory,
            &["worktree", "list", "--porcelain"],
            "Failed to list git worktrees",
        )
        .await?;
    Ok(parse_worktree_porcelain(&result.stdout))
}

/// 在 porcelain 列表中查找已检出指定本地分支的 worktree:按完整 ref
/// (refs/heads/<name>)或清洗后的分支名双向比较。返回 Some 表示分支已被占用
/// (git 不允许同一分支同时检出在两个 worktree)。
async fn find_branch_in_use(
    service: &GitService,
    primary_worktree: &Path,
    local_branch_name: &str,
) -> Option<super::paths::WorktreePorcelainEntry> {
    if local_branch_name.is_empty() {
        return None;
    }
    let entries = list_worktree_entries(service, &primary_worktree.to_string_lossy())
        .await
        .ok()?;
    let target_ref = format!("refs/heads/{}", local_branch_name);
    let target_clean = clean_branch_name(&target_ref);
    entries.into_iter().find(|entry| {
        let entry_ref = entry.branch_ref.trim();
        let entry_clean = if entry_ref.is_empty() {
            clean_branch_name(&entry.branch)
        } else {
            clean_branch_name(entry_ref)
        };
        entry_ref == target_ref || entry_clean == target_clean
    })
}

/// 生成候选 worktree 名列表(共 OPENCODE_WORKTREE_ATTEMPTS 个):base 名先
/// slug 化,第一个用原名,其余追加随机后缀;base 为空时全部用随机名。
/// 供 resolve_candidate_directory 逐个探测。
fn resolve_worktree_name_candidates(base_name: &str) -> Vec<String> {
    let normalized_base = slug_worktree_name(base_name);
    let mut candidates = Vec::with_capacity(OPENCODE_WORKTREE_ATTEMPTS);
    if normalized_base.is_empty() {
        for _ in 0..OPENCODE_WORKTREE_ATTEMPTS {
            candidates.push(generate_opencode_random_name());
        }
        return candidates;
    }
    for index in 0..OPENCODE_WORKTREE_ATTEMPTS {
        if index == 0 {
            candidates.push(normalized_base.clone());
        } else {
            candidates.push(format!(
                "{}-{}",
                normalized_base,
                generate_opencode_random_name()
            ));
        }
    }
    candidates
}

/// 选定的 worktree 候选:目录名、磁盘路径与将要创建/检出的本地分支。
struct WorktreeCandidate {
    /// worktree 目录名(未显式指定分支时也是 ompchamber/<name> 分支名的来源)。
    name: String,
    /// worktree 在 worktree_root 下的完整路径。
    directory: PathBuf,
    /// 该 worktree 使用的本地分支名(existing 模式来自解析,new 模式由本模块生成)。
    branch: String,
}

/// 依次探测候选名,选出可用的 (name, directory, branch):目录已存在则跳过;
/// 未显式指定分支时还要求 `ompchamber/<name>` 分支不存在(show-ref 校验),
/// 避免与既有分支撞名;显式分支名直接采用。全部候选冲突时返回 Err。
async fn resolve_candidate_directory(
    service: &GitService,
    worktree_root: &Path,
    preferred_name: &str,
    explicit_branch_name: &str,
    primary_worktree: &Path,
) -> ServiceResult<WorktreeCandidate> {
    let candidates = resolve_worktree_name_candidates(preferred_name);
    for name in candidates {
        let directory = worktree_root.join(&name);
        if check_path_exists(&directory).await {
            continue;
        }
        if !explicit_branch_name.is_empty() {
            return Ok(WorktreeCandidate {
                name,
                directory,
                branch: explicit_branch_name.to_string(),
            });
        }
        let branch = format!("ompchamber/{}", name);
        let branch_ref = format!("refs/heads/{}", branch);
        let branch_exists = service
            .run(
                primary_worktree,
                &["show-ref", "--verify", "--quiet", &branch_ref],
            )
            .await;
        if branch_exists.success {
            continue;
        }
        return Ok(WorktreeCandidate {
            name,
            directory,
            branch,
        });
    }
    Err("Failed to generate a unique worktree name".to_string())
}

/// existing 模式分支解析的中间结果。
struct ExistingModeResolution {
    /// 最终使用的本地分支名。
    local_branch: String,
    /// worktree add 的检出起点:已存在的本地分支名,或远端分支的 remote-tracking ref。
    checkout_ref: String,
    /// 是否需要用 -b 新建本地分支(检出远端 ref 时为 true)。
    create_local_branch: bool,
    /// 来源为远端分支时的解析结果,供 upstream 推断使用。
    remote_ref: Option<super::paths::RemoteBranchRef>,
}

/// existing 模式的分支解析:同名本地分支存在则直接检出;否则把输入解析成
/// remote/branch(必要时先 fetch 再复查),本地分支名优先取用户指定的
/// preferred_branch_name,缺省用远端分支名。本地与远端都找不到时返回 Err。
async fn resolve_branch_for_existing_mode(
    service: &GitService,
    primary_worktree: &Path,
    existing_branch: &str,
    preferred_branch_name: &str,
) -> ServiceResult<ExistingModeResolution> {
    let requested = existing_branch.trim();
    if requested.is_empty() {
        return Err("existingBranch is required in existing mode".to_string());
    }

    let normalized_local = clean_branch_name(requested);
    let local_ref = format!("refs/heads/{}", normalized_local);
    let local_exists = service
        .run(
            primary_worktree,
            &["show-ref", "--verify", "--quiet", &local_ref],
        )
        .await;
    if local_exists.success {
        let checkout_ref = normalized_local.clone();
        return Ok(ExistingModeResolution {
            local_branch: normalized_local,
            checkout_ref,
            create_local_branch: false,
            remote_ref: None,
        });
    }

    let Some(remote_ref) = parse_remote_branch_ref(requested) else {
        return Err(format!("Branch not found: {}", requested));
    };

    let remote_exists = service
        .run(
            primary_worktree,
            &["show-ref", "--verify", "--quiet", &remote_ref.full_ref],
        )
        .await;
    if !remote_exists.success {
        let _ = fetch_remote_branch_ref(
            service,
            primary_worktree,
            &remote_ref.remote,
            &remote_ref.branch,
        )
        .await;
        let recheck = service
            .run(
                primary_worktree,
                &["show-ref", "--verify", "--quiet", &remote_ref.full_ref],
            )
            .await;
        if !recheck.success {
            return Err(format!("Remote branch not found: {}", requested));
        }
    }

    let local_branch = clean_branch_name(
        if preferred_branch_name.trim().is_empty() {
            &remote_ref.branch
        } else {
            preferred_branch_name
        }
        .trim(),
    );
    if local_branch.is_empty() {
        return Err("Failed to resolve local branch name for existing branch worktree".to_string());
    }

    Ok(ExistingModeResolution {
        local_branch,
        checkout_ref: remote_ref.remote_ref.clone(),
        create_local_branch: true,
        remote_ref: Some(remote_ref),
    })
}

/// 确保名为 remote_name 的 remote 存在且指向 remote_url:不存在则
/// `remote add`,URL 不一致则 `remote set-url`;name 或 url 为空时静默跳过
/// (视为未配置)。
async fn ensure_remote_with_url(
    service: &GitService,
    primary_worktree: &Path,
    remote_name: &str,
    remote_url: &str,
) -> ServiceResult<()> {
    let name = remote_name.trim();
    let url = remote_url.trim();
    if name.is_empty() || url.is_empty() {
        return Ok(());
    }
    let get_url = service
        .run(primary_worktree, &["remote", "get-url", name])
        .await;
    if get_url.success {
        if get_url.stdout.trim() != url {
            service
                .run_or_throw(
                    primary_worktree,
                    &["remote", "set-url", name, url],
                    "Failed to update git remote URL",
                )
                .await?;
        }
        return Ok(());
    }
    service
        .run_or_throw(
            primary_worktree,
            &["remote", "add", name, url],
            "Failed to add git remote",
        )
        .await?;
    Ok(())
}

/// 用强制 refspec `+refs/heads/<b>:refs/remotes/<r>/<b>` 从指定 remote fetch
/// 单个分支以更新本地 remote-tracking ref;remote 或 branch 为空时直接成功
/// 返回,不执行任何命令。
async fn fetch_remote_branch_ref(
    service: &GitService,
    primary_worktree: &Path,
    remote_name: &str,
    branch_name: &str,
) -> ServiceResult<()> {
    let remote = remote_name.trim();
    let branch = branch_name.trim();
    if remote.is_empty() || branch.is_empty() {
        return Ok(());
    }
    let refspec = format!("+refs/heads/{}:refs/remotes/{}/{}", branch, remote, branch);
    service
        .run_or_throw(
            primary_worktree,
            &["fetch", remote, &refspec],
            &format!("Failed to fetch {}/{}", remote, branch),
        )
        .await
        .map(|_| ())
}

/// 判断输入值是否应按远端分支处理:显式 refs/remotes/ 或 remotes/ 前缀视为是;
/// 存在同名本地分支则返回 None(优先本地);其余按 remote 分支解析返回 Some。
async fn resolve_remote_branch_ref(
    service: &GitService,
    primary_worktree: &Path,
    value: &str,
) -> Option<super::paths::RemoteBranchRef> {
    let raw = value.trim();
    let parsed = parse_remote_branch_ref(raw)?;
    if raw.starts_with("refs/remotes/") || raw.starts_with("remotes/") {
        return Some(parsed);
    }
    let local_ref = format!("refs/heads/{}", raw);
    let local_exists = service
        .run(
            primary_worktree,
            &["show-ref", "--verify", "--quiet", &local_ref],
        )
        .await;
    if local_exists.success {
        return None;
    }
    Some(parsed)
}

/// existing 模式下(validate 与 create 共享)解析出的 worktree 来源全量信息。
struct ExistingWorktreeSource {
    /// 最终使用的本地分支名。
    local_branch: String,
    /// worktree add 的检出起点(本地分支名或远端 ref)。
    checkout_ref: String,
    /// 是否需要 -b 新建本地分支。
    create_local_branch: bool,
    /// 是否为该本地分支配置 upstream 跟踪。
    set_upstream: bool,
    /// upstream 的 remote 名(显式指定优先,否则由来源推断)。
    upstream_remote: String,
    /// upstream 的分支名(显式指定优先,否则由来源推断)。
    upstream_branch: String,
}

/// JS `resolveExistingWorktreeSource` (validate + create share it).
/// 中文说明:解析 existing 模式输入。走 provisioned remote 路径(ensureRemoteName/
/// ensureRemoteUrl 与解析出的 remote 一致)时,validate 意图只做 ls-remote 探测,
/// create 意图才实际 add remote 并 fetch;其余情况走通用分支解析并推断 upstream。
/// intent 取 "validate" 或 "create",决定是否产生真实的 git 副作用。
async fn resolve_existing_worktree_source(
    service: &GitService,
    primary_worktree: &Path,
    input: &Value,
    intent: &str,
) -> ServiceResult<ExistingWorktreeSource> {
    let text = |field: &str| {
        input
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let preferred_branch_name = clean_branch_name(&text("branchName"));
    let ensure_remote_name = text("ensureRemoteName");
    let ensure_remote_url = text("ensureRemoteUrl");
    let requested_existing_branch = text("existingBranch");
    let want_upstream = input.get("setUpstream") == Some(&Value::Bool(true));
    let explicit_upstream_remote = text("upstreamRemote");
    let explicit_upstream_branch = text("upstreamBranch");
    let parsed_existing_remote =
        resolve_remote_branch_ref(service, primary_worktree, &requested_existing_branch).await;

    if let Some(parsed) = &parsed_existing_remote
        && !ensure_remote_name.is_empty()
        && !ensure_remote_url.is_empty()
        && parsed.remote == ensure_remote_name
    {
        if intent == "validate" {
            let ls_remote = service
                .run(
                    primary_worktree,
                    &[
                        "ls-remote",
                        "--heads",
                        &ensure_remote_url,
                        &format!("refs/heads/{}", parsed.branch),
                    ],
                )
                .await;
            if !ls_remote.success {
                return Err(format!(
                    "Unable to reach remote {} ({}). Check network access and credentials for that repository.",
                    ensure_remote_name, ensure_remote_url
                ));
            }
            if ls_remote.stdout.trim().is_empty() {
                return Err(format!("Remote branch not found: {}", parsed.remote_ref));
            }
        } else {
            ensure_remote_with_url(
                service,
                primary_worktree,
                &ensure_remote_name,
                &ensure_remote_url,
            )
            .await?;
            if let Err(error) =
                fetch_remote_branch_ref(service, primary_worktree, &parsed.remote, &parsed.branch)
                    .await
            {
                return Err(format!(
                    "Unable to fetch {}/{} from {}. {}",
                    parsed.remote, parsed.branch, ensure_remote_url, error
                ));
            }
        }

        let local_branch = clean_branch_name(if preferred_branch_name.is_empty() {
            &parsed.branch
        } else {
            &preferred_branch_name
        });
        return Ok(ExistingWorktreeSource {
            local_branch,
            checkout_ref: parsed.remote_ref.clone(),
            create_local_branch: true,
            set_upstream: want_upstream,
            upstream_remote: if explicit_upstream_remote.is_empty() {
                parsed.remote.clone()
            } else {
                explicit_upstream_remote
            },
            upstream_branch: if explicit_upstream_branch.is_empty() {
                parsed.branch.clone()
            } else {
                explicit_upstream_branch
            },
        });
    }

    if requested_existing_branch.is_empty() {
        return Err("existingBranch is required in existing mode".to_string());
    }

    let resolved = resolve_branch_for_existing_mode(
        service,
        primary_worktree,
        &requested_existing_branch,
        &preferred_branch_name,
    )
    .await?;
    let (upstream_remote, upstream_branch, has_upstream) =
        if let Some(remote_ref) = &resolved.remote_ref {
            (
                if explicit_upstream_remote.is_empty() {
                    remote_ref.remote.clone()
                } else {
                    explicit_upstream_remote.clone()
                },
                if explicit_upstream_branch.is_empty() {
                    remote_ref.branch.clone()
                } else {
                    explicit_upstream_branch.clone()
                },
                true,
            )
        } else if !explicit_upstream_remote.is_empty() && !explicit_upstream_branch.is_empty() {
            (explicit_upstream_remote, explicit_upstream_branch, true)
        } else {
            (String::new(), String::new(), false)
        };

    Ok(ExistingWorktreeSource {
        local_branch: resolved.local_branch,
        checkout_ref: resolved.checkout_ref,
        create_local_branch: resolved.create_local_branch,
        set_upstream: want_upstream && has_upstream,
        upstream_remote,
        upstream_branch,
    })
}

/// 用 ls-remote 探测远端分支,返回 (remote 可达, 分支存在)。remote_url 非空时
/// 直接探测该 URL(不依赖本地 remote 配置);命令失败视为不可达,返回
/// (false, false)。
async fn check_remote_branch_exists(
    service: &GitService,
    primary_worktree: &Path,
    remote_name: &str,
    branch_name: &str,
    remote_url: &str,
) -> (bool, bool) {
    let remote = remote_name.trim();
    let branch = branch_name.trim();
    if remote.is_empty() || branch.is_empty() {
        return (false, false);
    }
    let target = if remote_url.trim().is_empty() {
        remote
    } else {
        remote_url.trim()
    };
    let ls_remote = service
        .run(
            primary_worktree,
            &[
                "ls-remote",
                "--heads",
                target,
                &format!("refs/heads/{}", branch),
            ],
        )
        .await;
    if !ls_remote.success {
        return (false, false);
    }
    (true, !ls_remote.stdout.trim().is_empty())
}

// ---------------------------------------------------------------------------
// validateWorktreeCreate / previewWorktreeCreate
// ---------------------------------------------------------------------------

/// JS validateWorktreeCreate:只校验、不产生 git 副作用,始终返回
/// { ok, errors: [{code, message}...], resolved: { mode, localBranch } }。
/// new 模式检查分支重名(branch_exists)、startRef 可达性(remote_unreachable /
/// start_ref_not_found);existing 模式复用 resolve_existing_worktree_source
/// (branch_not_found)。末尾统一追加 branch_in_use、invalid_remote_config
/// (ensureRemote 两字段必须成对)以及 setUpstream 时的 upstream_incomplete /
/// remote_not_found。项目上下文解析失败直接返回 ok:false + validation_failed。
pub async fn validate_worktree_create(
    service: &GitService,
    directory: &str,
    input: &Value,
) -> Value {
    let mode = if input.get("mode").and_then(Value::as_str) == Some("existing") {
        "existing"
    } else {
        "new"
    };
    let text = |field: &str| {
        input
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let mut errors: Vec<Value> = Vec::new();
    let mut local_branch = String::new();
    let mut inferred_upstream: Option<(String, String)> = None;

    let context = match service.resolve_worktree_project_context(directory).await {
        Ok(context) => context,
        Err(error) => {
            return json!({
                "ok": false,
                "errors": [{ "code": "validation_failed", "message": error }],
            });
        }
    };
    let preferred_branch_name = clean_branch_name(&text("branchName"));
    let start_ref_raw = text("startRef");
    let start_ref = if start_ref_raw.is_empty() {
        "HEAD".to_string()
    } else {
        start_ref_raw.clone()
    };
    let ensure_remote_name = text("ensureRemoteName");
    let ensure_remote_url = text("ensureRemoteUrl");

    let outcome = async {
        if mode == "existing" {
            match resolve_existing_worktree_source(
                service,
                &context.primary_worktree,
                input,
                "validate",
            )
            .await
            {
                Ok(resolved) => {
                    local_branch = resolved.local_branch;
                    if !resolved.upstream_remote.is_empty() || !resolved.upstream_branch.is_empty()
                    {
                        inferred_upstream =
                            Some((resolved.upstream_remote, resolved.upstream_branch));
                    }
                }
                Err(error) => {
                    errors.push(json!({ "code": "branch_not_found", "message": error }));
                }
            }
        } else {
            if !preferred_branch_name.is_empty() {
                let exists = service
                    .run(
                        &context.primary_worktree,
                        &[
                            "show-ref",
                            "--verify",
                            "--quiet",
                            &format!("refs/heads/{}", preferred_branch_name),
                        ],
                    )
                    .await;
                if exists.success {
                    errors.push(json!({
                        "code": "branch_exists",
                        "message": format!("Branch already exists: {}", preferred_branch_name),
                    }));
                }
                local_branch = preferred_branch_name.clone();
            }

            let parsed_remote_ref =
                resolve_remote_branch_ref(service, &context.primary_worktree, &start_ref).await;
            if !start_ref.is_empty() && start_ref != "HEAD" {
                let using_provisioned_remote = parsed_remote_ref.as_ref().is_some_and(|parsed| {
                    !ensure_remote_name.is_empty()
                        && !ensure_remote_url.is_empty()
                        && ensure_remote_name == parsed.remote
                });
                if let Some(parsed) = &parsed_remote_ref {
                    let url = if using_provisioned_remote {
                        ensure_remote_url.as_str()
                    } else {
                        ""
                    };
                    let (success, found) = check_remote_branch_exists(
                        service,
                        &context.primary_worktree,
                        &parsed.remote,
                        &parsed.branch,
                        url,
                    )
                    .await;
                    if !success {
                        let named = if using_provisioned_remote {
                            &ensure_remote_name
                        } else {
                            &parsed.remote
                        };
                        errors.push(json!({
                            "code": "remote_unreachable",
                            "message": format!("Unable to query remote {}", named),
                        }));
                    } else if !found {
                        errors.push(json!({
                            "code": "start_ref_not_found",
                            "message": format!("Remote branch not found: {}", parsed.remote_ref),
                        }));
                    }
                } else {
                    let start_ref_exists = service
                        .run(
                            &context.primary_worktree,
                            &["rev-parse", "--verify", "--quiet", &start_ref],
                        )
                        .await;
                    if !start_ref_exists.success {
                        errors.push(json!({
                            "code": "start_ref_not_found",
                            "message": format!("Start ref not found: {}", start_ref),
                        }));
                    }
                }
            }

            if let Some(parsed) = &parsed_remote_ref {
                inferred_upstream = Some((parsed.remote.clone(), parsed.branch.clone()));
            }
        }
        Ok::<(), ()>(())
    };
    let _ = outcome.await;

    if !local_branch.is_empty()
        && let Some(in_use) =
            find_branch_in_use(service, &context.primary_worktree, &local_branch).await
    {
        errors.push(json!({
            "code": "branch_in_use",
            "message": format!("Branch is already checked out in {}", in_use.worktree),
        }));
    }

    if (!ensure_remote_name.is_empty()) != (!ensure_remote_url.is_empty()) {
        errors.push(json!({
            "code": "invalid_remote_config",
            "message": "Both ensureRemoteName and ensureRemoteUrl are required together",
        }));
    }

    if input.get("setUpstream") == Some(&Value::Bool(true)) {
        let upstream_remote = if text("upstreamRemote").is_empty() {
            inferred_upstream
                .as_ref()
                .map(|(remote, _)| remote.clone())
                .unwrap_or_default()
        } else {
            text("upstreamRemote")
        };
        let upstream_branch = if text("upstreamBranch").is_empty() {
            inferred_upstream
                .as_ref()
                .map(|(_, branch)| branch.clone())
                .unwrap_or_default()
        } else {
            text("upstreamBranch")
        };
        if upstream_remote.trim().is_empty() || upstream_branch.trim().is_empty() {
            errors.push(json!({
                "code": "upstream_incomplete",
                "message": "upstreamRemote and upstreamBranch are required when setUpstream is true",
            }));
        } else {
            let remote_exists = service
                .run(
                    &context.primary_worktree,
                    &["remote", "get-url", &upstream_remote],
                )
                .await;
            if !remote_exists.success && ensure_remote_name != upstream_remote {
                errors.push(json!({
                    "code": "remote_not_found",
                    "message": format!("Remote not found: {}", upstream_remote),
                }));
            }
        }
    }

    json!({
        "ok": errors.is_empty(),
        "errors": errors,
        "resolved": { "mode": mode, "localBranch": if local_branch.is_empty() { Value::Null } else { Value::String(local_branch) } },
    })
}

/// JS previewWorktreeCreate:预览将要创建的 worktree。确保 worktree_root 存在后
/// 解析候选目录,返回 { name, branch, path };不创建分支、不建 worktree,
/// 无任何 git 写副作用。existing 模式下 branch 直接取用户输入的 branchName。
pub async fn preview_worktree_create(
    service: &GitService,
    directory: &str,
    input: &Value,
) -> ServiceResult<Value> {
    let mode = if input.get("mode").and_then(Value::as_str) == Some("existing") {
        "existing"
    } else {
        "new"
    };
    let context = service.resolve_worktree_project_context(directory).await?;
    let _ = tokio::fs::create_dir_all(&context.worktree_root).await;

    let preferred_name = {
        let value = input
            .get("worktreeName")
            .or_else(|| input.get("name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        value.to_string()
    };
    let preferred_branch_name = clean_branch_name(
        input
            .get("branchName")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or(""),
    );

    let candidate = resolve_candidate_directory(
        service,
        &context.worktree_root,
        &preferred_name,
        if mode == "new" && !preferred_branch_name.is_empty() {
            &preferred_branch_name
        } else {
            ""
        },
        &context.primary_worktree,
    )
    .await?;

    Ok(json!({
        "name": candidate.name,
        "branch": if mode == "new" { Value::String(candidate.branch) } else { Value::String(preferred_branch_name) },
        "path": candidate.directory.to_string_lossy(),
    }))
}

// ---------------------------------------------------------------------------
// createWorktree / removeWorktree / bootstrap status
// ---------------------------------------------------------------------------

/// 同步创建路径的核心:解析分支来源(existing/new)并检查分支占用;必要时
/// ensure remote 并 fetch 起点;拼装并执行 `worktree add --no-checkout`
/// (需新建本地分支时附 -b);写入 pending/directory-created 状态,把后续
/// checkout/hook/启动脚本交给 queue_worktree_bootstrap;最后 rev-parse HEAD
/// 作为返回的 head。返回 { head, name, branch, path, directoryCreated,
/// bootstrapStatus }。
async fn attach_git_worktree_to_candidate(
    service: &Arc<GitService>,
    context: &WorktreeProjectContext,
    candidate: &WorktreeCandidate,
    input: &Value,
) -> ServiceResult<Value> {
    let mode = if input.get("mode").and_then(Value::as_str) == Some("existing") {
        "existing"
    } else {
        "new"
    };
    let start_ref_raw = input
        .get("startRef")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    let start_ref = if start_ref_raw.is_empty() {
        "HEAD".to_string()
    } else {
        start_ref_raw.to_string()
    };
    let ensure_remote_name = input
        .get("ensureRemoteName")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let ensure_remote_url = input
        .get("ensureRemoteUrl")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();

    let local_branch: String;
    let mut set_upstream = input.get("setUpstream") == Some(&Value::Bool(true));
    let mut upstream_remote = String::new();
    let mut upstream_branch = String::new();
    let mut worktree_add_args: Vec<String> =
        vec!["worktree".into(), "add".into(), "--no-checkout".into()];

    if mode == "existing" {
        let resolved =
            resolve_existing_worktree_source(service, &context.primary_worktree, input, "create")
                .await?;
        local_branch = resolved.local_branch.clone();
        set_upstream = resolved.set_upstream;

        if let Some(in_use) =
            find_branch_in_use(service, &context.primary_worktree, &local_branch).await
        {
            return Err(format!(
                "Branch is already checked out in {}",
                in_use.worktree
            ));
        }

        if resolved.create_local_branch {
            worktree_add_args.push("-b".into());
            worktree_add_args.push(local_branch.clone());
        }
        worktree_add_args.push(candidate.directory.to_string_lossy().to_string());
        worktree_add_args.push(resolved.checkout_ref.clone());
        upstream_remote = resolved.upstream_remote;
        upstream_branch = resolved.upstream_branch;
    } else {
        local_branch = candidate.branch.clone();
        if local_branch.is_empty() {
            return Err("Failed to resolve branch name for new worktree".to_string());
        }

        let branch_exists = service
            .run(
                &context.primary_worktree,
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{}", local_branch),
                ],
            )
            .await;
        if branch_exists.success {
            return Err(format!("Branch already exists: {}", local_branch));
        }
        if let Some(in_use) =
            find_branch_in_use(service, &context.primary_worktree, &local_branch).await
        {
            return Err(format!(
                "Branch is already checked out in {}",
                in_use.worktree
            ));
        }

        worktree_add_args.push("-b".into());
        worktree_add_args.push(local_branch.clone());
        worktree_add_args.push(candidate.directory.to_string_lossy().to_string());
        if !start_ref.is_empty() && start_ref != "HEAD" {
            worktree_add_args.push(start_ref.clone());
        }

        if let Some(parsed) =
            resolve_remote_branch_ref(service, &context.primary_worktree, &start_ref).await
        {
            upstream_remote = parsed.remote.clone();
            upstream_branch = parsed.branch.clone();
        }
    }

    if !ensure_remote_name.is_empty() && !ensure_remote_url.is_empty() {
        ensure_remote_with_url(
            service,
            &context.primary_worktree,
            &ensure_remote_name,
            &ensure_remote_url,
        )
        .await?;
    }

    if mode == "new"
        && let Some(parsed) =
            resolve_remote_branch_ref(service, &context.primary_worktree, &start_ref).await
    {
        fetch_remote_branch_ref(
            service,
            &context.primary_worktree,
            &parsed.remote,
            &parsed.branch,
        )
        .await?;
    }

    {
        let argv: Vec<&str> = worktree_add_args.iter().map(String::as_str).collect();
        service
            .run_or_throw(
                &context.primary_worktree,
                &argv,
                "Failed to create git worktree",
            )
            .await?;
    }

    if set_upstream {
        if upstream_remote.is_empty() {
            upstream_remote = input
                .get("upstreamRemote")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("")
                .to_string();
        }
        if upstream_branch.is_empty() {
            upstream_branch = input
                .get("upstreamBranch")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("")
                .to_string();
        }
    } else {
        upstream_remote.clear();
        upstream_branch.clear();
    }

    let bootstrap_status = set_bootstrap_state(
        service,
        &candidate.directory.to_string_lossy(),
        BOOTSTRAP_PENDING,
        PHASE_DIRECTORY_CREATED,
        None,
    );

    queue_worktree_bootstrap(
        service,
        &candidate.directory.to_string_lossy(),
        &context.project_id,
        &context.primary_worktree.to_string_lossy(),
        &local_branch,
        set_upstream,
        &upstream_remote,
        &upstream_branch,
        &ensure_remote_name,
        &ensure_remote_url,
        input
            .get("startCommand")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );

    let head_result = service
        .run(&candidate.directory, &["rev-parse", "HEAD"])
        .await;
    let head = head_result.stdout.trim().to_string();

    Ok(json!({
        "head": head,
        "name": candidate.name,
        "branch": local_branch,
        "path": candidate.directory.to_string_lossy(),
        "directoryCreated": true,
        "bootstrapStatus": bootstrap_status,
    }))
}

/// 组装并启动后台 bootstrap 任务(经 track_bootstrap_task 按 per-directory 锁
/// 串行化):populate(checkout + 项目 setup,带锁恢复)→ post-checkout hook →
/// 可选的 upstream 配置 → git-ready → 项目/自定义启动脚本 → ready。任一步
/// 失败则置 failed/directory-created 并记录错误文本;所有参数按值捕获进
/// detached 任务,与调用方栈帧解耦。
fn queue_worktree_bootstrap(
    service: &Arc<GitService>,
    directory: &str,
    project_id: &str,
    primary_worktree: &str,
    local_branch: &str,
    set_upstream: bool,
    upstream_remote: &str,
    upstream_branch: &str,
    ensure_remote_name: &str,
    ensure_remote_url: &str,
    start_command: &str,
) {
    let directory = directory.to_string();
    let project_id = project_id.to_string();
    let primary_worktree = primary_worktree.to_string();
    let local_branch = local_branch.to_string();
    let upstream_remote = upstream_remote.to_string();
    let upstream_branch = upstream_branch.to_string();
    let ensure_remote_name = ensure_remote_name.to_string();
    let ensure_remote_url = ensure_remote_url.to_string();
    let start_command = start_command.to_string();
    let set_upstream = set_upstream;

    let directory_for_key = directory.clone();
    track_bootstrap_task(service, &directory_for_key, move |service| async move {
        let outcome = async {
            service
                .populate_worktree_with_lock_recovery(&directory)
                .await?;
            run_post_checkout_hook(&service, &directory).await;
            if set_upstream {
                let _ = apply_upstream_configuration(
                    &service,
                    Path::new(&primary_worktree),
                    Path::new(&directory),
                    &local_branch,
                    &upstream_remote,
                    &upstream_branch,
                    &ensure_remote_name,
                    &ensure_remote_url,
                )
                .await;
            }
            set_bootstrap_state(
                &service,
                &directory,
                BOOTSTRAP_PENDING,
                PHASE_GIT_READY,
                None,
            );
            run_worktree_start_scripts(&service, &directory, &project_id, &start_command).await;
            set_bootstrap_state(
                &service,
                &directory,
                BOOTSTRAP_READY,
                PHASE_SETUP_READY,
                None,
            );
            Ok::<(), String>(())
        }
        .await;
        if let Err(error) = outcome {
            set_bootstrap_state(
                &service,
                &directory,
                BOOTSTRAP_FAILED,
                PHASE_DIRECTORY_CREATED,
                Some(error),
            );
        }
    });
}

/// 若 worktree 配置了 post-checkout hook(存在、是普通文件,且在 Unix 上带
/// 执行位),则按 git 原生约定以参数 (GIT_NULL_REF, HEAD, 1) 运行它,并注入
/// GIT_DIR / GIT_WORK_TREE 环境变量以复刻 git 自身的 hook 环境。hook 失败只记
/// warn,不影响 bootstrap 主流程。
async fn run_post_checkout_hook(service: &GitService, directory: &str) {
    let result = service
        .run(directory, &["rev-parse", "--git-path", "hooks"])
        .await;
    if !result.success {
        return;
    }
    let hook_directory = normalize_directory_path(Some(result.stdout.trim())).unwrap_or_default();
    if hook_directory.is_empty() {
        return;
    }
    let hook_path = Path::new(&hook_directory).join("post-checkout");
    let Ok(metadata) = tokio::fs::metadata(&hook_path).await else {
        return;
    };
    if !metadata.is_file() {
        return;
    }
    #[cfg(unix)]
    {
use crate::os_compat::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return;
        }
    }

    let head_result = service.run(directory, &["rev-parse", "HEAD"]).await;
    let git_dir_result = service
        .run(directory, &["rev-parse", "--absolute-git-dir"])
        .await;
    if !head_result.success || !git_dir_result.success {
        return;
    }
    let head = head_result.stdout.trim().to_string();
    let git_dir = git_dir_result.stdout.trim().to_string();
    if head.is_empty() || git_dir.is_empty() {
        return;
    }

    // GIT_DIR/GIT_WORK_TREE mirror git's own hook environment.
    let mut env = std::collections::HashMap::new();
    env.insert("GIT_DIR".to_string(), git_dir);
    env.insert(
        "GIT_WORK_TREE".to_string(),
        absolutize(Path::new(directory))
            .to_string_lossy()
            .to_string(),
    );
    let argv = [GIT_NULL_REF.to_string(), head, "1".to_string()];
    let result = service.run_strings_with_env(directory, &argv, env).await;
    if !result.success {
        tracing::warn!(
            "[GitService] post-checkout hook failed in worktree {}: {}",
            directory,
            result.message
        );
    }
}

/// 为新 worktree 的本地分支配置 upstream:先确保 ensure remote 存在;fetch 上游
/// 分支成功才执行 `branch --set-upstream-to=<remote>/<branch>`。fetch 失败时
/// 选择保持未跟踪并返回 Ok,而不是为从未 fetch 到的 ref 写跟踪配置。
async fn apply_upstream_configuration(
    service: &GitService,
    primary_worktree: &Path,
    worktree_directory: &Path,
    local_branch: &str,
    upstream_remote: &str,
    upstream_branch: &str,
    ensure_remote_name: &str,
    ensure_remote_url: &str,
) -> ServiceResult<()> {
    if !ensure_remote_name.trim().is_empty() && !ensure_remote_url.trim().is_empty() {
        ensure_remote_with_url(
            service,
            primary_worktree,
            ensure_remote_name.trim(),
            ensure_remote_url.trim(),
        )
        .await?;
    }
    let remote = upstream_remote.trim();
    let branch = upstream_branch.trim();
    if remote.is_empty() || branch.is_empty() || local_branch.is_empty() {
        return Ok(());
    }
    if fetch_remote_branch_ref(service, primary_worktree, remote, branch)
        .await
        .is_err()
    {
        // Leave tracking unset rather than writing branch.* config for a ref
        // that was never fetched.
        return Ok(());
    }
    let full = format!("{}/{}", remote, branch);
    service
        .run_or_throw(
            worktree_directory,
            &[
                "branch",
                &format!("--set-upstream-to={}", full),
                local_branch,
            ],
            &format!("Failed to set upstream to {}", full),
        )
        .await?;
    Ok(())
}

/// 执行单个启动命令:以 `bash -lc` 在 worktree 目录、干净环境 + git 环境变量下
/// 运行(对齐 JS 实现;JS 在 Windows 上用 cmd /c,这里统一走 bash)。
/// 空命令直接返回成功结果,不 spawn 进程。
async fn run_worktree_start_command(
    service: &GitService,
    directory: &str,
    command: &str,
) -> GitCommandResult {
    let text = command.trim();
    if text.is_empty() {
        return GitCommandResult::ok(String::new());
    }
    // JS runs `bash -lc <command>` (cmd /c on Windows) with the git env.
    let argv = vec!["-lc".to_string(), text.to_string()];
    service.run_bash_with_env(directory, argv).await
}

/// 从 <opencode data>/storage/project/<id>.json 读取 commands.start 字段;
/// 文件缺失、JSON 解析失败或字段为空都返回空字符串(视为无项目启动命令)。
async fn load_project_start_command(project_id: &str) -> String {
    let storage_path = opencode_data_path()
        .join("storage")
        .join("project")
        .join(format!("{}.json", project_id));
    let Ok(raw) = tokio::fs::read_to_string(storage_path).await else {
        return String::new();
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
        return String::new();
    };
    parsed
        .get("commands")
        .and_then(|commands| commands.get("start"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// 依次执行项目级 start 命令与请求附带的 startCommand:项目级命令失败时直接
/// 返回(跳过 extra);两者失败都只记 warn,bootstrap 仍会标记 ready,
/// 不阻塞创建流程。
async fn run_worktree_start_scripts(
    service: &GitService,
    directory: &str,
    project_id: &str,
    start_command: &str,
) {
    let project_start = load_project_start_command(project_id).await;
    if !project_start.is_empty() {
        let result = run_worktree_start_command(service, directory, &project_start).await;
        if !result.success {
            tracing::warn!(
                "Worktree project start command failed: {}",
                if !result.message.is_empty() {
                    result.message
                } else {
                    result.stderr
                }
            );
            return;
        }
    }
    let extra = start_command.trim();
    if extra.is_empty() {
        return;
    }
    let result = run_worktree_start_command(service, directory, extra).await;
    if !result.success {
        tracing::warn!(
            "Worktree start command failed: {}",
            if !result.message.is_empty() {
                result.message
            } else {
                result.stderr
            }
        );
    }
}

/// 判断目录当前是否仍挂载为 git worktree:`rev-parse --is-inside-work-tree`
/// 成功且输出 true 才算;cleanup 用它避免误删仍被 git 管理的目录。
async fn is_attached_git_worktree_directory(service: &GitService, directory: &str) -> bool {
    let result = service
        .run(directory, &["rev-parse", "--is-inside-work-tree"])
        .await;
    result.success && result.stdout.trim() == "true"
}

/// 快速创建失败后的兜底清理:仅当候选目录位于受管 worktree_root 之内(且不是
/// 根本身)、未挂到 git、且是空目录时,删除这个空目录;其余情况保持原样,
/// 绝不递归删除非空内容。
async fn cleanup_failed_fast_worktree_create(
    service: &GitService,
    context: &WorktreeProjectContext,
    candidate: &WorktreeCandidate,
) {
    let candidate_directory = absolutize(&candidate.directory);
    let worktree_root = absolutize(&context.worktree_root);
    let inside = is_inside_or_same_directory(&worktree_root, &candidate_directory)
        && candidate_directory != worktree_root;
    if !inside {
        return;
    }
    if is_attached_git_worktree_directory(service, &candidate_directory.to_string_lossy()).await {
        return;
    }
    if let Ok(mut entries) = tokio::fs::read_dir(&candidate_directory).await {
        let empty = !entries
            .next_entry()
            .await
            .is_ok_and(|entry| entry.is_some());
        if empty {
            let _ = tokio::fs::remove_dir(&candidate_directory).await;
        }
    }
}

/// JS createWorktree 入口。returnAfterDirectoryCreated=true(快速路径)时先
/// validate,失败即返回错误消息(多条用换行拼接);成功则创建空目录、写入
/// pending/directory-created,把真正的 attach 交给后台任务(失败时置 failed 并
/// 调 cleanup_failed_fast_worktree_create 清理),立即返回 head 为空串的占位
/// 结果。否则同步调用 attach_git_worktree_to_candidate 完成全部步骤。
pub async fn create_worktree(
    service: &Arc<GitService>,
    directory: &str,
    input: &Value,
) -> ServiceResult<Value> {
    let mode = if input.get("mode").and_then(Value::as_str) == Some("existing") {
        "existing"
    } else {
        "new"
    };
    let context = service.resolve_worktree_project_context(directory).await?;

    let return_after_directory_created =
        input.get("returnAfterDirectoryCreated") == Some(&Value::Bool(true));
    if return_after_directory_created {
        let validation = validate_worktree_create(service, directory, input).await;
        if validation.get("ok") != Some(&Value::Bool(true)) {
            let message = validation
                .get("errors")
                .and_then(Value::as_array)
                .map(|errors| {
                    errors
                        .iter()
                        .filter_map(|error| error.get("message").and_then(Value::as_str))
                        .filter(|m| !m.is_empty())
                        .map(|m| m.to_string())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "Failed to validate worktree creation".to_string());
            return Err(message);
        }
    }

    let _ = tokio::fs::create_dir_all(&context.worktree_root).await;

    let preferred_name = {
        let value = input
            .get("worktreeName")
            .or_else(|| input.get("name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        value.to_string()
    };
    let preferred_branch_name = clean_branch_name(
        input
            .get("branchName")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or(""),
    );

    let candidate = resolve_candidate_directory(
        service,
        &context.worktree_root,
        &preferred_name,
        if mode == "new" && !preferred_branch_name.is_empty() {
            &preferred_branch_name
        } else {
            ""
        },
        &context.primary_worktree,
    )
    .await?;

    if return_after_directory_created {
        let _ = tokio::fs::create_dir(&candidate.directory).await;
        let bootstrap_status = set_bootstrap_state(
            service,
            &candidate.directory.to_string_lossy(),
            BOOTSTRAP_PENDING,
            PHASE_DIRECTORY_CREATED,
            None,
        );
        let local_branch = if mode == "existing" {
            let fallback = input
                .get("branchName")
                .or_else(|| input.get("existingBranch"))
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            let cleaned = clean_branch_name(fallback);
            if cleaned.is_empty() {
                candidate.branch.clone()
            } else {
                cleaned
            }
        } else {
            candidate.branch.clone()
        };

        let directory_text = candidate.directory.to_string_lossy().to_string();
        let project_directory = directory.to_string();
        let input = input.clone();
        let background_candidate = WorktreeCandidate {
            name: candidate.name.clone(),
            directory: candidate.directory.clone(),
            branch: candidate.branch.clone(),
        };
        track_bootstrap_task(service, &directory_text, move |service| async move {
            // attachGitWorktreeToCandidate + cleanup on failure.
            let context = service
                .resolve_worktree_project_context(&project_directory)
                .await;
            match context {
                Ok(context) => {
                    if let Err(error) = attach_git_worktree_to_candidate(
                        &service,
                        &context,
                        &background_candidate,
                        &input,
                    )
                    .await
                    {
                        set_bootstrap_state(
                            &service,
                            &background_candidate.directory.to_string_lossy(),
                            BOOTSTRAP_FAILED,
                            PHASE_DIRECTORY_CREATED,
                            Some(error.clone()),
                        );
                        let _ = cleanup_failed_fast_worktree_create(
                            &service,
                            &context,
                            &background_candidate,
                        )
                        .await;
                        tracing::warn!("Background worktree creation failed: {}", error);
                    }
                }
                Err(error) => {
                    set_bootstrap_state(
                        &service,
                        &background_candidate.directory.to_string_lossy(),
                        BOOTSTRAP_FAILED,
                        PHASE_DIRECTORY_CREATED,
                        Some(error.clone()),
                    );
                    tracing::warn!("Background worktree creation failed: {}", error);
                }
            }
        });

        return Ok(json!({
            "head": "",
            "name": candidate.name,
            "branch": local_branch,
            "path": candidate.directory.to_string_lossy(),
            "directoryCreated": true,
            "bootstrapStatus": bootstrap_status,
        }));
    }

    attach_git_worktree_to_candidate(service, &context, &candidate, input).await
}

/// 查询目录的 bootstrap 状态快照 { status, phase, error, updatedAt };没有记录
/// (从未引导、或成功后被清理)时返回 ready/setup-ready 的缺省值。
/// 目录为空则返回 Err。
pub async fn get_worktree_bootstrap_status(
    service: &GitService,
    directory: &str,
) -> ServiceResult<Value> {
    let Some(key) = bootstrap_key(directory) else {
        return Err("Worktree directory is required".to_string());
    };
    if let Some(state) = service
        .bootstrap_state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
    {
        return Ok(json!({
            "status": state.status,
            "phase": state.phase,
            "error": state.error,
            "updatedAt": state.updated_at_ms,
        }));
    }
    Ok(json!({
        "status": BOOTSTRAP_READY,
        "phase": PHASE_SETUP_READY,
        "error": null,
        "updatedAt": now_ms(),
    }))
}

/// 删除 worktree:先 wait_for_active_bootstrap 等待进行中的引导;拒绝删除主
/// worktree;在 porcelain 列表中按 canonical path 匹配目标,命中则
/// `worktree remove --force`,并按 deleteLocalBranch 决定是否删除本地分支。
/// 未命中但目标位于受管 worktree_root 内(孤儿目录)时直接删除磁盘目录。
/// 最后清理 bootstrap 状态;成功恒返回 true。
pub async fn remove_worktree(
    service: &GitService,
    directory: &str,
    input: &Value,
) -> ServiceResult<bool> {
    let target_directory = normalize_directory_path(Some(
        input.get("directory").and_then(Value::as_str).unwrap_or(""),
    ))
    .filter(|v| !v.trim().is_empty())
    .ok_or_else(|| "Worktree directory is required".to_string())?;

    wait_for_active_bootstrap(service, &target_directory).await;

    let context = service.resolve_worktree_project_context(directory).await?;
    let delete_local_branch = input.get("deleteLocalBranch") == Some(&Value::Bool(true));

    let target_canonical = canonical_path(&target_directory).await;
    let primary_canonical = canonical_path(&context.primary_worktree).await;
    if target_canonical == primary_canonical {
        return Err("Cannot remove the primary workspace".to_string());
    }
    let worktree_root_canonical = canonical_path(&context.worktree_root).await;

    let entries =
        list_worktree_entries(service, &context.primary_worktree.to_string_lossy()).await?;
    let mut matched: Option<super::paths::WorktreePorcelainEntry> = None;
    for entry in &entries {
        if entry.worktree.is_empty() {
            continue;
        }
        if canonical_path(&entry.worktree).await == target_canonical {
            matched = Some(entry.clone());
            break;
        }
    }

    let Some(matched) = matched else {
        let is_managed_orphan = target_canonical != worktree_root_canonical
            && is_inside_or_same_directory(&worktree_root_canonical, &target_canonical);
        if is_managed_orphan && check_path_exists(&target_directory).await {
            let _ = tokio::fs::remove_dir_all(&target_directory).await;
        }
        clear_bootstrap_state(service, &target_directory);
        return Ok(true);
    };

    service
        .run_or_throw(
            &context.primary_worktree,
            &["worktree", "remove", "--force", &matched.worktree],
            "Failed to remove git worktree",
        )
        .await?;

    if delete_local_branch {
        let branch_name = clean_branch_name(if matched.branch_ref.trim().is_empty() {
            &matched.branch
        } else {
            matched.branch_ref.trim()
        });
        if !branch_name.is_empty() {
            service
                .run_or_throw(
                    &context.primary_worktree,
                    &["branch", "-D", &branch_name],
                    &format!("Failed to delete local branch {}", branch_name),
                )
                .await?;
        }
    }

    clear_bootstrap_state(service, &matched.worktree);
    Ok(true)
}

// Re-exported for the routes layer: JS getWorktrees swallows non-repo errors.
/// 判断 git 命令结果是否为 "not a git repository" 类错误(大小写不敏感);
/// routes 层用它把这类失败转成 200 的软非仓库负载,对齐 JS 行为。
pub(crate) fn is_not_repo_result(result: &GitCommandResult) -> bool {
    let text = parse_git_error_text(&result.stderr, &result.stdout, &result.message);
    text.to_lowercase().contains("not a git repository")
}

/// GitService 上与启动命令执行相关的扩展(供 worktree bootstrap 使用)。
impl GitService {
    /// 在指定目录中以 bash 运行 argv:env_clear 后仅注入 build_git_env 生成的 git
    /// 环境变量,stdin 关闭并捕获 stdout/stderr。返回 GitCommandResult;spawn 失败
    /// 映射为 success=false、message 带 "spawn bash" 前缀,不向上抛错。
    pub(crate) async fn run_bash_with_env(
        &self,
        directory: &str,
        argv: Vec<String>,
    ) -> GitCommandResult {
        let env = super::exec::build_git_env(&std::collections::HashMap::new());
        let mut command = tokio::process::Command::new("bash");
        command
            .args(&argv)
            .env_clear()
            .envs(&env)
            .current_dir(directory)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        match command.output().await {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let code = output.status.code().unwrap_or(1);
                GitCommandResult {
                    success: code == 0,
                    exit_code: code,
                    message: if code == 0 {
                        String::new()
                    } else {
                        stderr.trim().to_string()
                    },
                    stdout,
                    stderr,
                    stdout_bytes: Vec::new(),
                }
            }
            Err(error) => GitCommandResult {
                success: false,
                exit_code: 1,
                stdout: String::new(),
                stderr: String::new(),
                message: format!("spawn bash: {}", error),
                stdout_bytes: Vec::new(),
            },
        }
    }
}

/// 占位函数:消耗当前未被其它代码使用的 HashSet / Map 导入,避免 unused
/// import 警告;无任何运行时行为。
#[allow(dead_code)]
fn _unused(_: &HashSet<String>, _: &Map<String, Value>) {}
