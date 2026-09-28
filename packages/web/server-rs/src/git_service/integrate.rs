//! Port of the integrate-worktree workflow from `server/lib/git/service.js`
//! (`computeIntegratePlan`, `getIntegrateConflictDetails`,
//! `isCherryPickInProgress`, `integrateWorktreeCommits`, `abortIntegrate`,
//! `continueIntegrate`) backing `/api/git/integrate/*`.
//!
//! 中文说明：integrate-worktree 工作流（把 worktree 分支上的提交搬移到目标
//! 分支）的移植，对应 `server/lib/git/service.js` 中的 `computeIntegratePlan`、
//! `getIntegrateConflictDetails`、`isCherryPickInProgress`、
//! `integrateWorktreeCommits`、`abortIntegrate`、`continueIntegrate`，
//! 支撑 `/api/git/integrate/*` 路由。核心思路：为目标分支创建临时 worktree，
//! 在其上按计划逐个 cherry-pick 提交；遇到冲突时返回可恢复的状态快照，
//! 由客户端决定 abort 还是 continue。

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::paths::{absolutize, normalize_directory_path, trim_git_lines};
use super::service::{GitService, ServiceResult};

/// 从 JSON 取分支名并校验：trim 后非空、不以 `-` 开头且不含 NUL，
/// 否则返回带字段名的 "… is required" / "Invalid …" 错误。
fn normalize_branch(value: Option<&Value>, field_name: &str) -> ServiceResult<String> {
    let branch = value
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if branch.is_empty() {
        return Err(format!("{} is required", field_name));
    }
    if branch.starts_with('-') || branch.contains('\0') {
        return Err(format!("Invalid {}", field_name));
    }
    Ok(branch)
}

/// 从 JSON 取 commit SHA 并校验：长度 4..=64 且全为十六进制字符，
/// 否则返回 "Invalid commit SHA"。
fn normalize_sha(value: Option<&Value>) -> ServiceResult<String> {
    let sha = value
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if !(sha.len() >= 4 && sha.len() <= 64 && sha.chars().all(|c| c.is_ascii_hexdigit())) {
        return Err("Invalid commit SHA".to_string());
    }
    Ok(sha)
}

/// 从 JSON 取路径字段：规范化（含 `~` 展开）、要求非空，并转为绝对路径。
fn normalize_path_field(value: Option<&Value>, field_name: &str) -> ServiceResult<PathBuf> {
    let target = normalize_directory_path(value.and_then(Value::as_str))
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| format!("{} is required", field_name))?;
    Ok(absolutize(Path::new(&target)))
}

/// integrate 流程关注的 worktree 条目：路径与其分支引用。
struct IntegrateWorktreeEntry {
    /// 工作树绝对路径（`worktree <path>` 行）。
    path: String,
    /// 完整分支引用（`branch <ref>` 行）；detached HEAD 时为 `None`。
    branch_ref: Option<String>,
}

/// 运行 `git worktree list --porcelain` 并解析出 (路径, 分支引用) 列表；
/// git 失败时把 "Failed to list git worktrees" 场景的错误透传为 Err。
async fn list_worktrees_for_integrate(
    service: &GitService,
    repo_root: &Path,
) -> ServiceResult<Vec<IntegrateWorktreeEntry>> {
    let out = service
        .run_or_throw(
            repo_root,
            &["worktree", "list", "--porcelain"],
            "Failed to list git worktrees",
        )
        .await?;
    let mut entries: Vec<IntegrateWorktreeEntry> = Vec::new();
    let mut current: Option<IntegrateWorktreeEntry> = None;
    for line in out.stdout.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some(IntegrateWorktreeEntry {
                path: path.trim().to_string(),
                branch_ref: None,
            });
            continue;
        }
        if let Some(entry) = current.as_mut()
            && let Some(branch) = line.strip_prefix("branch ")
        {
            entry.branch_ref = Some(branch.trim().to_string());
        }
    }
    if let Some(entry) = current.take() {
        entries.push(entry);
    }
    Ok(entries
        .into_iter()
        .filter(|entry| !entry.path.is_empty())
        .collect())
}

/// 确保目标分支以本地分支存在并返回本地分支名：`HEAD` 或已有本地分支直接
/// 返回原名；`remotes/<remote>/<name>` 形态或存在于 origin 远端时用
/// `branch --track` 建立跟踪分支；否则原样返回交由后续 git 调用暴露错误。
async fn ensure_local_integrate_branch(
    service: &GitService,
    repo_root: &Path,
    candidate: &str,
) -> ServiceResult<String> {
    if candidate == "HEAD" {
        return Ok("HEAD".to_string());
    }
    let has_local = service
        .run(
            repo_root,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{}", candidate),
            ],
        )
        .await;
    if has_local.success {
        return Ok(candidate.to_string());
    }

    if let Some(remote_ref) = candidate.strip_prefix("remotes/") {
        let mut parts = remote_ref.splitn(2, '/');
        let remote = parts.next().unwrap_or("origin");
        let name = parts.next().unwrap_or("");
        let remote = if remote.trim().is_empty() {
            "origin".to_string()
        } else {
            remote.to_string()
        };
        let name = if name.trim().is_empty() {
            return Err("branch is required".to_string());
        } else {
            name.to_string()
        };
        service
            .run_or_throw(
                repo_root,
                &["branch", "--track", &name, &format!("{}/{}", remote, name)],
                "Failed to track remote branch",
            )
            .await?;
        return Ok(name);
    }

    let remote_check = service
        .run(
            repo_root,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/remotes/origin/{}", candidate),
            ],
        )
        .await;
    if remote_check.success {
        service
            .run_or_throw(
                repo_root,
                &[
                    "branch",
                    "--track",
                    candidate,
                    &format!("origin/{}", candidate),
                ],
                "Failed to track remote branch",
            )
            .await?;
        return Ok(candidate.to_string());
    }

    Ok(candidate.to_string())
}

/// 对应 JS `computeIntegratePlan`：规范化 repoRoot 与两个分支名；任一分支
/// 为 `HEAD` 时直接返回空提交计划。否则先确保目标分支本地存在，再以
/// `git cherry <target> <source>` 取带 `+` 前缀（目标分支缺失）的提交，
/// 与 `rev-list --reverse <target>..<source>` 求交集，得到按序待搬移提交。
pub async fn compute_integrate_plan(service: &GitService, input: &Value) -> ServiceResult<Value> {
    let repo_root = normalize_path_field(input.get("repoRoot"), "repoRoot")?;
    let source_branch = normalize_branch(input.get("sourceBranch"), "sourceBranch")?;
    let target_branch_raw = normalize_branch(input.get("targetBranch"), "targetBranch")?;
    if source_branch == "HEAD" || target_branch_raw == "HEAD" {
        return Ok(json!({
            "repoRoot": repo_root.to_string_lossy(),
            "sourceBranch": source_branch,
            "targetBranch": target_branch_raw,
            "commits": [],
        }));
    }

    let target_branch =
        ensure_local_integrate_branch(service, &repo_root, &target_branch_raw).await?;
    let cherry = service
        .run_or_throw(
            &repo_root,
            &["cherry", &target_branch, &source_branch],
            "Failed to compute cherry commits",
        )
        .await?;
    let mut plus: Vec<String> = Vec::new();
    for line in trim_git_lines(&cherry.stdout) {
        if let Some(rest) = line.strip_prefix('+') {
            let sha: String = rest
                .trim()
                .chars()
                .take_while(|c| c.is_ascii_hexdigit())
                .collect();
            if sha.len() >= 7 && sha.len() <= 40 {
                plus.push(sha);
            }
        }
    }

    let rev_list = service
        .run_or_throw(
            &repo_root,
            &[
                "rev-list",
                "--reverse",
                &format!("{}..{}", target_branch, source_branch),
            ],
            "Failed to list commits",
        )
        .await?;
    let commits: Vec<String> = trim_git_lines(&rev_list.stdout)
        .into_iter()
        .filter(|sha| plus.contains(sha))
        .collect();

    Ok(json!({
        "repoRoot": repo_root.to_string_lossy(),
        "sourceBranch": source_branch,
        "targetBranch": target_branch,
        "commits": commits,
    }))
}

/// 在 `~/.config/ompchamber/tmp/oc-integrate-<pid><rand>` 下为目标分支创建
/// 临时 worktree；失败时清理已建目录并把 git 错误文本作为 Err 返回。
async fn create_integrate_temp_worktree(
    service: &GitService,
    repo_root: &Path,
    target_branch: &str,
) -> ServiceResult<PathBuf> {
    let tmp_parent = crate::config::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("ompchamber")
        .join("tmp");
    let _ = tokio::fs::create_dir_all(&tmp_parent).await;
    let unique = format!("oc-integrate-{}{}", std::process::id(), rand_suffix());
    let tmp_dir = tmp_parent.join(unique);
    let result = service
        .run(
            repo_root,
            &[
                "worktree",
                "add",
                "--force",
                &tmp_dir.to_string_lossy(),
                target_branch,
            ],
        )
        .await;
    if !result.success {
        let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
        let text = [result.stderr.trim(), result.message.trim()]
            .into_iter()
            .find(|chunk| !chunk.is_empty())
            .unwrap_or("")
            .to_string();
        return Err(if text.is_empty() {
            "Failed to create temp worktree".to_string()
        } else {
            text
        });
    }
    Ok(tmp_dir)
}

/// 生成 10 位十六进制随机后缀，保证临时 worktree 目录名唯一。
fn rand_suffix() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    (0..10)
        .map(|_| format!("{:x}", rng.random_range(0..16)))
        .collect()
}

/// 尽力清理临时 worktree：先 `worktree remove --force` 再 `worktree prune`；
/// 所有错误都被忽略（用于成功与失败两条收尾路径）。
async fn remove_integrate_temp_worktree(service: &GitService, repo_root: &Path, tmp_dir: &Path) {
    let _ = service
        .run(
            repo_root,
            &["worktree", "remove", "--force", &tmp_dir.to_string_lossy()],
        )
        .await;
    let _ = service.run(repo_root, &["worktree", "prune"]).await;
}

/// 临时 worktree 若配置了 upstream：先 `fetch` 再 `merge --ff-only` 快进到
/// 远端最新；快进失败返回错误，没有 upstream 时空操作。
async fn maybe_fast_forward_integrate_upstream(
    service: &GitService,
    tmp_dir: &Path,
) -> ServiceResult<()> {
    let upstream = service
        .run(
            tmp_dir,
            &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        )
        .await;
    let upstream_ref = upstream.stdout.trim().to_string();
    if upstream_ref.is_empty() {
        return Ok(());
    }
    let _ = service.run(tmp_dir, &["fetch"]).await;
    let ff = service
        .run(tmp_dir, &["merge", "--ff-only", &upstream_ref])
        .await;
    if !ff.success {
        let text = if !ff.stderr.trim().is_empty() {
            ff.stderr
        } else {
            ff.message
        };
        return Err(if text.trim().is_empty() {
            "Fast-forward failed".to_string()
        } else {
            text.trim().to_string()
        });
    }
    Ok(())
}

/// 对应 JS `getIntegrateConflictDetails`：在临时 worktree 上收集 porcelain
/// 状态、未合并文件列表、完整 diff，以及 `CHERRY_PICK_HEAD` 的补丁元信息
/// 与补丁正文（stdout 为空时回退取 stderr）。
pub async fn get_integrate_conflict_details(
    service: &GitService,
    tmp_dir: Option<&Value>,
) -> ServiceResult<Value> {
    let target = normalize_path_field(tmp_dir, "tempWorktreePath")?;

    let status = service.run(&target, &["status", "--porcelain"]).await;
    let unmerged = service
        .run(&target, &["diff", "--name-only", "--diff-filter=U"])
        .await;
    let diff = service.run(&target, &["diff", "--no-ext-diff"]).await;
    let meta = service
        .run(
            &target,
            &["show", "--no-patch", "--pretty=fuller", "CHERRY_PICK_HEAD"],
        )
        .await;
    let patch = service
        .run(&target, &["show", "--no-ext-diff", "CHERRY_PICK_HEAD"])
        .await;

    let pick = |result: &super::exec::GitCommandResult| -> String {
        if !result.stdout.is_empty() {
            result.stdout.clone()
        } else {
            result.stderr.clone()
        }
    };

    Ok(json!({
        "statusPorcelain": status.stdout,
        "unmergedFiles": trim_git_lines(&unmerged.stdout),
        "diff": if !diff.stdout.is_empty() { diff.stdout.clone() } else { diff.stderr.clone() },
        "currentPatchMeta": pick(&meta),
        "currentPatch": pick(&patch),
    }))
}

/// 对应 JS `isCherryPickInProgress`：以 `rev-parse --verify --quiet
/// CHERRY_PICK_HEAD` 是否成功判断 cherry-pick 是否进行中。
pub async fn is_cherry_pick_in_progress(
    service: &GitService,
    tmp_dir: Option<&Value>,
) -> ServiceResult<Value> {
    let target = normalize_path_field(tmp_dir, "tempWorktreePath")?;
    let head = service
        .run(
            &target,
            &["rev-parse", "--verify", "--quiet", "CHERRY_PICK_HEAD"],
        )
        .await;
    Ok(json!({ "inProgress": head.success }))
}

/// 找出检出目标分支、不在排除列表且 `status --porcelain` 干净的其它工作树
/// ——搬移完成后需要把它们的 HEAD 同步到目标分支新位置。
async fn compute_clean_integrate_worktrees_to_sync(
    service: &GitService,
    repo_root: &Path,
    target_branch: &str,
    exclude_paths: &[String],
) -> ServiceResult<Vec<String>> {
    let target_ref = format!("refs/heads/{}", target_branch);
    let entries = list_worktrees_for_integrate(service, repo_root).await?;
    let candidates: Vec<String> = entries
        .into_iter()
        .filter(|entry| entry.branch_ref.as_deref() == Some(target_ref.as_str()))
        .map(|entry| entry.path)
        .filter(|path| !path.is_empty() && !exclude_paths.contains(path))
        .collect();

    let mut clean = Vec::new();
    for candidate in candidates {
        let status = service.run(&candidate, &["status", "--porcelain"]).await;
        if status.stdout.trim().is_empty() {
            clean.push(candidate);
        }
    }
    Ok(clean)
}

/// 对每个待同步工作树执行 `reset --hard`（忽略错误），使其跟上目标分支。
async fn sync_clean_integrate_target_worktrees(service: &GitService, paths: &[String]) {
    for target in paths {
        let _ = service.run(target, &["reset", "--hard"]).await;
    }
}

/// 校验并规范化客户端回传的 integrate 计划：repoRoot、两个分支名与逐项
/// 校验过的 SHA 提交列表（缺失的 commits 视为空）。
async fn normalize_integrate_plan(
    plan: &Value,
) -> ServiceResult<(PathBuf, String, String, Vec<String>)> {
    let repo_root = normalize_path_field(plan.get("repoRoot"), "repoRoot")?;
    let source_branch = normalize_branch(plan.get("sourceBranch"), "sourceBranch")?;
    let target_branch = normalize_branch(plan.get("targetBranch"), "targetBranch")?;
    let commits: Vec<String> = match plan.get("commits").and_then(Value::as_array) {
        Some(items) => {
            let mut out = Vec::new();
            for item in items {
                out.push(normalize_sha(Some(item))?);
            }
            out
        }
        None => Vec::new(),
    };
    Ok((repo_root, source_branch, target_branch, commits))
}

/// 冲突暂停时可序列化的进行时状态：恢复（continue/abort）所需的全部上下文，
/// 字段名与 JS 版往返 JSON 保持一致。
struct IntegrateState {
    /// 主仓库根目录。
    repo_root: PathBuf,
    /// 临时 worktree 路径（cherry-pick 的执行地）。
    temp_worktree_path: PathBuf,
    /// 提交来源分支。
    source_branch: String,
    /// 搬移目标分支。
    target_branch: String,
    /// 完成后需要 `reset --hard` 同步的干净目标分支工作树列表。
    clean_target_worktrees: Vec<String>,
    /// 尚未 cherry-pick 的提交（含当前冲突的提交）。
    remaining_commits: Vec<String>,
    /// 正在 cherry-pick（发生冲突）的提交。
    current_commit: String,
}

/// 状态与客户端往返的 JSON 表示。
impl IntegrateState {
    /// 序列化为客户端持有的状态对象（键名与 JS 版一致）。
    fn to_json(&self) -> Value {
        json!({
            "repoRoot": self.repo_root.to_string_lossy(),
            "tempWorktreePath": self.temp_worktree_path.to_string_lossy(),
            "sourceBranch": self.source_branch,
            "targetBranch": self.target_branch,
            "cleanTargetWorktrees": self.clean_target_worktrees,
            "remainingCommits": self.remaining_commits,
            "currentCommit": self.current_commit,
        })
    }
}

/// 从客户端状态 JSON 反向校验并重建 IntegrateState；任一字段非法时返回
/// 对应的规范化错误。
fn normalize_integrate_state(state: &Value) -> ServiceResult<IntegrateState> {
    let clean_target_worktrees: Vec<String> =
        match state.get("cleanTargetWorktrees").and_then(Value::as_array) {
            Some(items) => {
                let mut out = Vec::new();
                for item in items {
                    out.push(
                        normalize_path_field(Some(item), "cleanTargetWorktree")?
                            .to_string_lossy()
                            .to_string(),
                    );
                }
                out
            }
            None => Vec::new(),
        };
    let remaining_commits: Vec<String> =
        match state.get("remainingCommits").and_then(Value::as_array) {
            Some(items) => {
                let mut out = Vec::new();
                for item in items {
                    out.push(normalize_sha(Some(item))?);
                }
                out
            }
            None => Vec::new(),
        };
    Ok(IntegrateState {
        repo_root: normalize_path_field(state.get("repoRoot"), "repoRoot")?,
        temp_worktree_path: normalize_path_field(
            state.get("tempWorktreePath"),
            "tempWorktreePath",
        )?,
        source_branch: normalize_branch(state.get("sourceBranch"), "sourceBranch")?,
        target_branch: normalize_branch(state.get("targetBranch"), "targetBranch")?,
        clean_target_worktrees,
        remaining_commits,
        current_commit: normalize_sha(state.get("currentCommit"))?,
    })
}

/// 逐个 cherry-pick 剩余提交：成功即弹出继续；出现未合并文件时打包冲突
/// 状态与详情返回 `{"kind":"conflict"}`；无冲突的失败把 stderr/message
/// 作为错误返回。全部成功后清理临时 worktree 并返回
/// `{"kind":"success","moved":N}`。
#[allow(clippy::too_many_arguments)]
async fn run_integrate_pick_loop(
    service: &GitService,
    repo_root: &Path,
    source_branch: &str,
    target_branch: &str,
    tmp_dir: &Path,
    mut remaining: Vec<String>,
    clean_target_worktrees: Vec<String>,
) -> ServiceResult<Value> {
    let moved = remaining.len();
    while !remaining.is_empty() {
        let sha = remaining[0].clone();
        let pick = service.run(tmp_dir, &["cherry-pick", &sha]).await;
        if pick.success {
            remaining.remove(0);
            continue;
        }

        let unmerged = service
            .run(tmp_dir, &["diff", "--name-only", "--diff-filter=U"])
            .await;
        let unmerged_files = trim_git_lines(&unmerged.stdout);
        if !unmerged_files.is_empty() {
            let details =
                get_integrate_conflict_details(service, Some(&json!(tmp_dir.to_string_lossy())))
                    .await?;
            let state = IntegrateState {
                repo_root: repo_root.to_path_buf(),
                temp_worktree_path: tmp_dir.to_path_buf(),
                source_branch: source_branch.to_string(),
                target_branch: target_branch.to_string(),
                clean_target_worktrees,
                remaining_commits: remaining.clone(),
                current_commit: sha,
            };
            return Ok(json!({ "kind": "conflict", "state": state.to_json(), "details": details }));
        }

        let text = if !pick.stderr.trim().is_empty() {
            pick.stderr
        } else {
            pick.message
        };
        return Err(if text.trim().is_empty() {
            "Cherry-pick failed".to_string()
        } else {
            text.trim().to_string()
        });
    }

    remove_integrate_temp_worktree(service, repo_root, tmp_dir).await;
    Ok(json!({ "kind": "success", "moved": moved }))
}

/// 对应 JS `integrateWorktreeCommits`：规范化计划后，空提交列表直接返回
/// noop；否则创建临时 worktree、快进 upstream、确认目标分支工作区干净，
/// 再进入 cherry-pick 循环；任何错误路径都会清理临时 worktree。
pub async fn integrate_worktree_commits(
    service: &GitService,
    input_plan: &Value,
) -> ServiceResult<Value> {
    let (repo_root, source_branch, target_branch, commits) =
        normalize_integrate_plan(input_plan).await?;
    if commits.is_empty() {
        return Ok(json!({ "kind": "noop", "reason": "No commits to move" }));
    }

    let tmp_dir = create_integrate_temp_worktree(service, &repo_root, &target_branch).await?;

    maybe_fast_forward_integrate_upstream(service, &tmp_dir).await?;

    let outcome = async {
        let clean = service.run(&tmp_dir, &["status", "--porcelain"]).await;
        if !clean.stdout.trim().is_empty() {
            return Err("Target branch has local changes; abort integration and retry".to_string());
        }

        let clean_target_worktrees = compute_clean_integrate_worktrees_to_sync(
            service,
            &repo_root,
            &target_branch,
            &[tmp_dir.to_string_lossy().to_string()],
        )
        .await
        .unwrap_or_default();

        run_integrate_pick_loop(
            service,
            &repo_root,
            &source_branch,
            &target_branch,
            &tmp_dir,
            commits,
            clean_target_worktrees,
        )
        .await
    }
    .await;

    match outcome {
        Ok(value) => Ok(value),
        Err(error) => {
            remove_integrate_temp_worktree(service, &repo_root, &tmp_dir).await;
            Err(error)
        }
    }
}

/// 对应 JS `abortIntegrate`：对临时 worktree 执行 `cherry-pick --abort` 并
/// 清理之；两者结果都被忽略，恒返回 success。
pub async fn abort_integrate(service: &GitService, state_input: &Value) -> ServiceResult<Value> {
    let state = normalize_integrate_state(state_input)?;
    let _ = service
        .run(&state.temp_worktree_path, &["cherry-pick", "--abort"])
        .await;
    remove_integrate_temp_worktree(service, &state.repo_root, &state.temp_worktree_path).await;
    Ok(json!({ "success": true }))
}

/// 对应 JS `continueIntegrate`：先 `cherry-pick --continue`；仍有未合并文件
/// 则打包新的冲突状态与详情返回，硬失败返回错误。成功后跳过刚完成的当前
/// 提交、继续 pick 剩余提交；全部完成时清理临时 worktree、同步干净的目标
/// 分支工作树，并返回搬移的提交数量。
pub async fn continue_integrate(service: &GitService, state_input: &Value) -> ServiceResult<Value> {
    let state = normalize_integrate_state(state_input)?;
    let cont = service
        .run(&state.temp_worktree_path, &["cherry-pick", "--continue"])
        .await;
    if !cont.success {
        let unmerged = service
            .run(
                &state.temp_worktree_path,
                &["diff", "--name-only", "--diff-filter=U"],
            )
            .await;
        if !trim_git_lines(&unmerged.stdout).is_empty() {
            let details = get_integrate_conflict_details(
                service,
                Some(&json!(state.temp_worktree_path.to_string_lossy())),
            )
            .await?;
            return Ok(json!({ "kind": "conflict", "state": state.to_json(), "details": details }));
        }
        let text = if !cont.stderr.trim().is_empty() {
            cont.stderr
        } else {
            cont.message
        };
        return Err(if text.trim().is_empty() {
            "Cherry-pick continue failed".to_string()
        } else {
            text.trim().to_string()
        });
    }

    let mut remaining = state.remaining_commits.clone();
    if !remaining.is_empty() && remaining[0] == state.current_commit {
        remaining.remove(0);
    }

    let mut still = remaining;
    while !still.is_empty() {
        let sha = still[0].clone();
        let pick = service
            .run(&state.temp_worktree_path, &["cherry-pick", &sha])
            .await;
        if pick.success {
            still.remove(0);
            continue;
        }
        let unmerged = service
            .run(
                &state.temp_worktree_path,
                &["diff", "--name-only", "--diff-filter=U"],
            )
            .await;
        if !trim_git_lines(&unmerged.stdout).is_empty() {
            let details = get_integrate_conflict_details(
                service,
                Some(&json!(state.temp_worktree_path.to_string_lossy())),
            )
            .await?;
            let mut next_state = state.clone();
            next_state.remaining_commits = still.clone();
            next_state.current_commit = sha;
            return Ok(
                json!({ "kind": "conflict", "state": next_state.to_json(), "details": details }),
            );
        }
        let text = if !pick.stderr.trim().is_empty() {
            pick.stderr
        } else {
            pick.message
        };
        return Err(if text.trim().is_empty() {
            "Cherry-pick failed".to_string()
        } else {
            text.trim().to_string()
        });
    }

    remove_integrate_temp_worktree(service, &state.repo_root, &state.temp_worktree_path).await;
    sync_clean_integrate_target_worktrees(service, &state.clean_target_worktrees).await;
    Ok(json!({ "kind": "success", "moved": state.remaining_commits.len() }))
}

/// 手写 Clone（与派生等价），逐字段克隆进行时状态。
impl Clone for IntegrateState {
    /// 逐字段深拷贝，保持与 JS 展开语义一致。
    fn clone(&self) -> Self {
        Self {
            repo_root: self.repo_root.clone(),
            temp_worktree_path: self.temp_worktree_path.clone(),
            source_branch: self.source_branch.clone(),
            target_branch: self.target_branch.clone(),
            clean_target_worktrees: self.clean_target_worktrees.clone(),
            remaining_commits: self.remaining_commits.clone(),
            current_commit: self.current_commit.clone(),
        }
    }
}
