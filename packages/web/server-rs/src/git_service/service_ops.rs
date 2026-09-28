//! Remaining `service.js` exports — staging/revert/hunks, commit, branch
//! operations, remotes, push/pull/fetch, merge/rebase/continue, conflict
//! details, stashes, log, commit-file diffs, worktree-type probes, and the
//! primary-root/toplevel resolvers. Split from `service.rs` only for file
//! size; same `impl GitService`.
//!
//! 本模块承载旧 `service.js` 其余导出的 Rust 实现：暂存/还原与 hunk 应用、
//! commit、分支操作、远端与 push/pull/fetch、merge/rebase/continue、冲突详情、
//! stash、log、提交文件 diff、worktree 类型探测，以及主 worktree 根目录/顶层
//! 目录解析。仅为控制文件体积才从 `service.rs` 拆出，仍是同一个
//! `impl GitService` 的延续实现。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::exec::{GitCommandResult, is_index_lock_error};
use super::paths::{
    absolutize, canonical_path, check_path_exists, clean_branch_name, is_valid_commit_hash,
    normalize_directory_path, require_directory, to_git_path, validate_repository_file_paths,
};
use super::service::{GitService, NO_EXT_DIFF, ServiceResult, iso_now};

// ---------------------------------------------------------------------------
// Status-file helper (shared by cherry-pick/revert/merge/rebase conflict paths)
// ---------------------------------------------------------------------------

/// 读取当前 status 并返回冲突文件路径列表；cherry-pick/revert/merge/rebase
/// 的冲突路径共用此辅助。
async fn conflict_files(service: &GitService, repo_root: &Path) -> Vec<String> {
    let status = service.parse_status(repo_root, &[]).await;
    status.conflicted
}

/// 判断错误文本是否表示 cherry-pick 类冲突：含 "conflict" 或
/// "patch does not apply"（大小写不敏感）。
fn is_conflict_message(text: &str) -> bool {
    let lowered = text.to_lowercase();
    lowered.contains("conflict") || lowered.contains("patch does not apply")
}

/// 判断错误文本是否表示 revert 冲突：含 "conflict" 或 "revert failed"
/// （大小写不敏感）。
fn is_revert_conflict_message(text: &str) -> bool {
    let lowered = text.to_lowercase();
    lowered.contains("conflict") || lowered.contains("revert failed")
}

/// 判断错误文本是否表示 rebase 冲突：含 "conflict"、"could not apply" 或
/// "merge conflict"（大小写不敏感）。
fn is_rebase_conflict_message(text: &str) -> bool {
    let lowered = text.to_lowercase();
    lowered.contains("conflict")
        || lowered.contains("could not apply")
        || lowered.contains("merge conflict")
}

/// 判断 `rebase --continue`/`merge --continue` 的失败输出是否表示仍有冲突
/// 待解决：含 "conflict"、"needs merge"、"unmerged" 或 "fix conflicts"
/// （大小写不敏感）。
fn is_continue_conflict_message(text: &str) -> bool {
    let lowered = text.to_lowercase();
    lowered.contains("conflict")
        || lowered.contains("needs merge")
        || lowered.contains("unmerged")
        || lowered.contains("fix conflicts")
}

/// 按优先级拼接 git 失败信息：stderr、stdout、message 三段去掉空白后以换行
/// 连接（空段被跳过），是全模块统一的错误文本合成器。
fn failure_text(stdout: &str, stderr: &str, message: &str) -> String {
    [stderr.trim(), stdout.trim(), message.trim()]
        .iter()
        .filter(|chunk| !chunk.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

/// [`GitService`] 的核心操作 impl 块（自 `service.rs` 拆出的延续部分）：暂存与
/// hunk、commit、分支与远端操作、push/pull/fetch、merge/rebase、stash、log、
/// 冲突处理及 worktree 解析等。
impl GitService {
    // -----------------------------------------------------------------------
    // Staging / unstaging (JS stageFiles / unstageFiles)
    // -----------------------------------------------------------------------

    /// 暂存指定文件（对应 JS 版 `stageFiles`），转调 [`stage_unfiles`]（stage=true）。
    pub async fn stage_files(&self, directory: &str, paths: &[Value]) -> ServiceResult<()> {
        self.stage_unfiles(directory, paths, true).await
    }

    /// 取消暂存指定文件（对应 JS 版 `unstageFiles`），转调 [`stage_unfiles`]（stage=false）。
    pub async fn unstage_files(&self, directory: &str, paths: &[Value]) -> ServiceResult<()> {
        self.stage_unfiles(directory, paths, false).await
    }

    /// 暂存/取消暂存的共用实现，整体运行于 index 队列内以避免 `index.lock` 竞争：
    /// 先做目录与路径非空校验、路径规范化及仓库越界校验，再逐个把文件解析为
    /// repo 相对路径并二次校验。暂存走 `git add --`（整批失败且为 pathspec 错误时
    /// 逐文件重试，跳过已不存在的项）；取消暂存先 `git restore --staged --`，
    /// 失败时回退 `git reset HEAD --`。
    async fn stage_unfiles(
        &self,
        directory: &str,
        paths: &[Value],
        stage: bool,
    ) -> ServiceResult<()> {
        let normalized_paths = super::paths::normalize_file_path_list(paths);
        if directory.trim().is_empty() || normalized_paths.is_empty() {
            let what = if stage { "stageFile" } else { "unstageFile" };
            return Err(format!("directory and path are required for {}", what));
        }
        let directory_path = require_directory(Some(directory))?;
        validate_repository_file_paths(&directory_path, &normalized_paths)?;
        let directory = directory.to_string();
        let queue_directory = directory.clone();

        self.with_index_queue(Some(&queue_directory), |service| {
            Box::pin(async move {
                let context = service.repository_context(&directory).await?;
                let mut repo_paths: Vec<String> = Vec::new();
                for file_path in &normalized_paths {
                    let fc = service
                        .resolve_git_file_context(
                            &context.directory_path,
                            &context.repo_root,
                            file_path,
                        )
                        .await?;
                    if !repo_paths.contains(&fc.repo_path) {
                        repo_paths.push(fc.repo_path);
                    }
                }
                validate_repository_file_paths(&context.repo_root, &repo_paths)?;

                if stage {
                    let mut args: Vec<String> = vec!["add".into(), "--".into()];
                    args.extend(repo_paths.iter().cloned());
                    if let Err(failure) = service.raw(&context.repo_root, &args).await {
                        let text = failure_text(&failure.stdout, &failure.stderr, &failure.message);
                        let is_pathspec_error =
                            text.contains("pathspec") && text.contains("did not match any files");
                        if !is_pathspec_error {
                            return Err(text.trim().to_string());
                        }
                        for repo_path in &repo_paths {
                            let per_path = service
                                .raw(
                                    &context.repo_root,
                                    &["add".into(), "--".into(), repo_path.clone()],
                                )
                                .await;
                            if let Err(per_failure) = per_path {
                                let per_text = failure_text(
                                    &per_failure.stdout,
                                    &per_failure.stderr,
                                    &per_failure.message,
                                );
                                let per_pathspec = per_text.contains("pathspec")
                                    && per_text.contains("did not match any files");
                                if !per_pathspec {
                                    return Err(per_text.trim().to_string());
                                }
                            }
                        }
                    }
                } else {
                    let mut args: Vec<String> =
                        vec!["restore".into(), "--staged".into(), "--".into()];
                    args.extend(repo_paths.iter().cloned());
                    if service.raw(&context.repo_root, &args).await.is_err() {
                        let mut fallback: Vec<String> =
                            vec!["reset".into(), "HEAD".into(), "--".into()];
                        fallback.extend(repo_paths.iter().cloned());
                        service
                            .raw(&context.repo_root, &fallback)
                            .await
                            .map_err(|f| {
                                failure_text(&f.stdout, &f.stderr, &f.message)
                                    .trim()
                                    .to_string()
                            })?;
                    }
                }
                Ok(())
            })
        })
        .await
    }

    // -----------------------------------------------------------------------
    // revertFile / applyHunk
    // -----------------------------------------------------------------------

    /// 还原文件：`scope == "working"` 只还原工作区，其余（视为 "all"）先取消暂存。
    /// 未跟踪文件走 `git clean -f -d --`（失败再直接删除文件/目录，均视为成功）；
    /// 已跟踪文件在 all 模式先 `restore --staged`（失败回退 `reset HEAD --`），
    /// 最后 `restore --`（失败回退 `checkout --`，仍失败才报错）。
    /// 整体运行于 index 队列内。
    pub async fn revert_file(
        &self,
        directory: &str,
        file_path: &str,
        scope: Option<&str>,
    ) -> ServiceResult<()> {
        let scope = if scope == Some("working") {
            "working"
        } else {
            "all"
        };
        let directory = directory.to_string();
        let file_path = file_path.to_string();
        let queue_directory = directory.clone();
        self.with_index_queue(Some(&queue_directory), |service| {
            Box::pin(async move {
                let directory_path = require_directory(Some(&directory))?;
                let repo_root = service.resolve_repository_root(&directory_path).await?;
                let fc = service
                    .resolve_git_file_context(
                        &PathBuf::from(directory_path),
                        &repo_root,
                        &file_path,
                    )
                    .await?;

                let tracked = service
                    .raw(
                        &repo_root,
                        &[
                            "ls-files".into(),
                            "--error-unmatch".into(),
                            "--".into(),
                            fc.repo_path.clone(),
                        ],
                    )
                    .await
                    .is_ok();

                if !tracked {
                    let clean = service
                        .raw(
                            &repo_root,
                            &[
                                "clean".into(),
                                "-f".into(),
                                "-d".into(),
                                "--".into(),
                                fc.repo_path.clone(),
                            ],
                        )
                        .await;
                    if clean.is_err() {
                        match tokio::fs::remove_dir_all(&fc.absolute_path).await {
                            Ok(()) => return Ok(()),
                            Err(_not_found)
                                if _not_found.kind() == std::io::ErrorKind::NotFound =>
                            {
                                return Ok(());
                            }
                            Err(_error) => {
                                let _ = tokio::fs::remove_file(&fc.absolute_path).await;
                                return Ok(());
                            }
                        }
                    } else {
                        return Ok(());
                    }
                }

                if scope == "all"
                    && service
                        .raw(
                            &repo_root,
                            &[
                                "restore".into(),
                                "--staged".into(),
                                "--".into(),
                                fc.repo_path.clone(),
                            ],
                        )
                        .await
                        .is_err()
                {
                    let _ = service
                        .raw(
                            &repo_root,
                            &[
                                "reset".into(),
                                "HEAD".into(),
                                "--".into(),
                                fc.repo_path.clone(),
                            ],
                        )
                        .await;
                }

                if service
                    .raw(
                        &repo_root,
                        &["restore".into(), "--".into(), fc.repo_path.clone()],
                    )
                    .await
                    .is_err()
                {
                    service
                        .raw(
                            &repo_root,
                            &["checkout".into(), "--".into(), fc.repo_path.clone()],
                        )
                        .await
                        .map_err(|f| {
                            failure_text(&f.stdout, &f.stderr, &f.message)
                                .trim()
                                .to_string()
                        })?;
                }
                Ok(())
            })
        })
        .await
    }

    /// 以补丁形式应用单个 hunk，整体运行于 index 队列内。action 映射 flags：
    /// stage → `--cached`、unstage → `--cached --reverse`、discard → `--reverse`，
    /// 非法值直接报 "Invalid hunk action"。patch 必填且须包含 `@@ ` hunk 头；
    /// 补丁推断出的目标路径（`extract_patch_target_path`）必须与请求文件一致；
    /// 补丁先写入临时文件，`git apply --check` 预检失败提示 hunk 已不适用，
    /// 预检通过才真正 apply，最后清理临时文件。
    pub async fn apply_hunk(
        &self,
        directory: &str,
        file_path: &str,
        patch: &str,
        action: &str,
    ) -> ServiceResult<()> {
        let flags: &[&str] = match action {
            "stage" => &["--cached"],
            "unstage" => &["--cached", "--reverse"],
            "discard" => &["--reverse"],
            _ => return Err("Invalid hunk action".to_string()),
        };
        if patch.trim().is_empty() {
            return Err("patch is required to apply a hunk".to_string());
        }
        if !patch
            .lines()
            .any(|line| line.starts_with("@@") && line.len() > 2 && line[2..].starts_with(' '))
        {
            return Err("patch does not contain a hunk header".to_string());
        }

        let directory = directory.to_string();
        let queue_directory = directory.clone();
        self.with_index_queue(Some(&queue_directory), |service| {
            let flags: Vec<String> = flags.iter().map(|s| s.to_string()).collect();
            let patch = patch.to_string();
            let file_path = file_path.to_string();
            Box::pin(async move {
                let context = service.repository_context(&directory).await?;
                let fc = service
                    .resolve_git_file_context(
                        &context.directory_path,
                        &context.repo_root,
                        &file_path,
                    )
                    .await?;
                validate_repository_file_paths(
                    &context.repo_root,
                    std::slice::from_ref(&fc.repo_path),
                )?;

                let target_path = extract_patch_target_path(&patch);
                if let Some(target) = target_path
                    && target != fc.repo_path
                    && target != file_path
                {
                    return Err("patch target path does not match the requested file".to_string());
                }

                let tmp_path = std::env::temp_dir().join(format!(
                    "ompchamber-hunk-{}-{}.patch",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0),
                    hex_random_suffix()
                ));
                tokio::fs::write(&tmp_path, &patch)
                    .await
                    .map_err(|e| e.to_string())?;

                let result = async {
                    let mut check_args: Vec<String> = vec!["apply".into()];
                    check_args.extend(flags.iter().cloned());
                    check_args.extend(["--check".into(), tmp_path.to_string_lossy().to_string()]);
                    if let Err(failure) = service.raw(&context.repo_root, &check_args).await {
                        let text = failure_text(&failure.stdout, &failure.stderr, &failure.message)
                            .trim()
                            .to_string();
                        return Err(if text.is_empty() {
                            "Hunk no longer applies — refresh and try again.".to_string()
                        } else {
                            format!("Hunk no longer applies — refresh and try again.\n{}", text)
                        });
                    }

                    let mut apply_args: Vec<String> = vec!["apply".into()];
                    apply_args.extend(flags.iter().cloned());
                    apply_args.push(tmp_path.to_string_lossy().to_string());
                    service
                        .raw(&context.repo_root, &apply_args)
                        .await
                        .map_err(|f| {
                            failure_text(&f.stdout, &f.stderr, &f.message)
                                .trim()
                                .to_string()
                        })?;
                    Ok(())
                }
                .await;

                let _ = tokio::fs::remove_file(&tmp_path).await;
                result
            })
        })
        .await
    }

    // -----------------------------------------------------------------------
    // commit
    // -----------------------------------------------------------------------

    /// 提交变更，整体运行于 index 队列内。`options.addAll == true` 时执行 `add .`；
    /// 否则按 `options.files` 解析 repo 路径，仅保留 status 中实际存在的文件
    /// （全部被过滤时报错提示刷新 git status）。提供 `options.stageFiles` 时改为
    /// “仅提交所选暂存”：把未选中的已暂存文件临时 `restore --staged` 撤出索引，
    /// 提交后重新 `add` 恢复；未提供时只为工作区仍有改动的文件补 `add`。
    /// 提交由 `commit_once` 完成；遇到 pathspec 错误时回退为不带文件列表的
    /// 整索引提交。成功返回 commit 哈希、分支名与 diffstat 汇总。
    pub async fn commit(
        &self,
        directory: &str,
        message: &str,
        options: &Value,
    ) -> ServiceResult<Value> {
        let directory = directory.to_string();
        let queue_directory = directory.clone();
        self.with_index_queue(Some(&queue_directory), |service| {
            let message = message.to_string();
            let options = options.clone();
            Box::pin(async move {
                let context = service.repository_context(&directory).await?;
                let string_list = |value: Option<&Value>| -> Vec<String> {
                    value
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(|v| v.as_str())
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default()
                };
                let requested_files = string_list(options.get("files"));
                let requested_stage_files = options.get("stageFiles").and_then(Value::as_array).map(|_| {
                    string_list(options.get("stageFiles"))
                });
                let mut files_to_commit: Vec<String> = Vec::new();
                let mut commit_from_index_only = false;
                let mut temporarily_unstaged_files: Vec<String> = Vec::new();

                let add_all = options.get("addAll") == Some(&Value::Bool(true));
                if add_all {
                    service.run(&context.repo_root, &["add", "."]).await;
                } else if !requested_files.is_empty() {
                    for file_path in &requested_files {
                        let fc = service
                            .resolve_git_file_context(&context.directory_path, &context.repo_root, file_path)
                            .await?;
                        if !files_to_commit.contains(&fc.repo_path) {
                            files_to_commit.push(fc.repo_path);
                        }
                    }
                    let mut stage_files_to_commit: Option<Vec<String>> = None;
                    if let Some(stage) = &requested_stage_files {
                        let mut out: Vec<String> = Vec::new();
                        for file_path in stage {
                            if let Ok(fc) = service
                                .resolve_git_file_context(&context.directory_path, &context.repo_root, file_path)
                                .await
                                && !out.contains(&fc.repo_path) {
                                    out.push(fc.repo_path);
                                }
                        }
                        stage_files_to_commit = Some(out);
                    }

                    let status = service.parse_status(&context.repo_root, &[]).await;
                    let status_paths: HashSet<String> = status.files.iter().map(|f| f.path.clone()).collect();
                    files_to_commit.retain(|path| status_paths.contains(path));

                    if files_to_commit.is_empty() {
                        return Err(
                            "No selected files are available to commit. Refresh git status and try again."
                                .to_string(),
                        );
                    }

                    if let Some(stage_list) = &requested_stage_files {
                        commit_from_index_only = true;
                        let selected: HashSet<&String> = files_to_commit.iter().collect();
                        temporarily_unstaged_files = status
                            .files
                            .iter()
                            .filter(|file| {
                                let index_status = file.index.trim();
                                !index_status.is_empty()
                                    && index_status != "?"
                                    && !selected.contains(&file.path)
                            })
                            .map(|file| file.path.clone())
                            .collect();
                        if !temporarily_unstaged_files.is_empty() {
                            let mut args: Vec<String> = vec!["restore".into(), "--staged".into(), "--".into()];
                            args.extend(temporarily_unstaged_files.iter().cloned());
                            let _ = service.raw(&context.repo_root, &args).await;
                        }
                        let _ = stage_list;
                    }

                    let files_needing_add: Vec<String> = if requested_stage_files.is_some() {
                        stage_files_to_commit
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|path| status_paths.contains(path.as_str()))
                            .collect()
                    } else {
                        files_to_commit
                            .iter()
                            .filter(|path| {
                                status.files.iter().any(|file| {
                                    file.path == **path
                                        && !(file.index != " " && file.working_dir == " ")
                                })
                            })
                            .cloned()
                            .collect()
                    };

                    if !files_needing_add.is_empty() {
                        let mut args: Vec<String> = vec!["add".into(), "--".into()];
                        args.extend(files_needing_add.iter().cloned());
                        let _ = service.raw(&context.repo_root, &args).await;
                    }
                }

                let commit_args: Option<Vec<String>> =
                    if !commit_from_index_only && !add_all && !files_to_commit.is_empty() {
                        Some(files_to_commit.clone())
                    } else {
                        None
                    };

                let (commit_sha, commit_branch, summary_value) =
                    match service.commit_once(&context.repo_root, &message, commit_args.clone()).await {
                    Ok(summary) => summary,
                    Err(text) => {
                        let is_pathspec_error =
                            text.contains("pathspec") && text.contains("did not match any files");
                        if !is_pathspec_error || commit_args.is_none() {
                            if !temporarily_unstaged_files.is_empty() {
                                let mut args: Vec<String> = vec!["add".into(), "--".into()];
                                args.extend(temporarily_unstaged_files.iter().cloned());
                                let _ = service.raw(&context.repo_root, &args).await;
                            }
                            return Err(text);
                        }
                        service.commit_once(&context.repo_root, &message, None).await?
                    }
                };

                if !temporarily_unstaged_files.is_empty() {
                    let mut args: Vec<String> = vec!["add".into(), "--".into()];
                    args.extend(temporarily_unstaged_files.iter().cloned());
                    let _ = service.raw(&context.repo_root, &args).await;
                }

                Ok(json!({
                    "success": true,
                    "commit": commit_sha,
                    "branch": commit_branch,
                    "summary": summary_value,
                }))
            })
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Branch operations
    // -----------------------------------------------------------------------

    /// 列出本地与活跃远端分支：`git branch -v -a` 经 `parse_branch_summary` 解析；
    /// 远端分支经 `filter_active_remote_branches` 过滤（剔除远端已删除的分支、
    /// 补齐远端现存但本地引用缺失的分支），另附 `remote_default_branches` 得到的
    /// 各 remote 默认分支。返回 all（本地分支 + `remotes/<remote>/<branch>`）、
    /// current、branches 条目映射与 defaultBranches。
    pub async fn get_branches(&self, directory: &str) -> ServiceResult<Value> {
        let context = self.repository_context(directory).await?;
        let output = self
            .raw(
                &context.repo_root,
                &["branch".into(), "-v".into(), "-a".into()],
            )
            .await
            .map_err(|f| {
                failure_text(&f.stdout, &f.stderr, &f.message)
                    .trim()
                    .to_string()
            })?;
        let summary = parse_branch_summary(&output);

        let remote_branches: Vec<String> = summary
            .all
            .iter()
            .filter(|b| b.starts_with("remotes/"))
            .cloned()
            .collect();
        let active_remote_branches = self
            .filter_active_remote_branches(&context.repo_root, remote_branches)
            .await;
        let default_branches = self.remote_default_branches(&context.repo_root).await;

        let mut filtered_all: Vec<String> = summary
            .all
            .iter()
            .filter(|branch| !branch.starts_with("remotes/"))
            .cloned()
            .collect();
        filtered_all.extend(active_remote_branches);

        Ok(json!({
            "all": filtered_all,
            "current": summary.current,
            "branches": summary.branches,
            "defaultBranches": Value::Object(default_branches),
        }))
    }

    /// 执行 `git remote` 列出全部远端名（非空行）；命令失败时输出为空，
    /// 等价于返回空列表。
    async fn remote_names(&self, repo_root: &Path) -> Vec<String> {
        let output = self.run(repo_root, &["remote"]).await;
        output
            .stdout
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    }

    /// 过滤出远端仍然存在的分支：对每个 remote 执行 `ls-remote --heads` 得到远端分支
    /// 集合（命令失败的 remote 记为不可达）。输入的 `remotes/<remote>/<branch>` 中，
    /// 不可达 remote 的分支原样保留（无法验证），可达 remote 仅保留远端仍存在的分支；
    /// 最后补上 ls-remote 发现但输入未列出的分支（以 `remotes/<remote>/<branch>`
    /// 形式去重追加）。
    async fn filter_active_remote_branches(
        &self,
        repo_root: &Path,
        remote_branches: Vec<String>,
    ) -> Vec<String> {
        let remotes = self.remote_names(repo_root).await;
        let mut branches_by_remote: HashMap<String, HashSet<String>> = HashMap::new();
        let mut unreachable_remotes: HashSet<String> = HashSet::new();

        for remote in &remotes {
            let ls_remote = self
                .raw(
                    repo_root,
                    &["ls-remote".into(), "--heads".into(), remote.clone()],
                )
                .await;
            match ls_remote {
                Ok(out) => {
                    let mut set = HashSet::new();
                    for line in out.lines() {
                        if let Some((_, reference)) = line.split_once('\t')
                            && let Some(branch) = reference.strip_prefix("refs/heads/")
                        {
                            set.insert(branch.to_string());
                        }
                    }
                    branches_by_remote.insert(remote.clone(), set);
                }
                Err(_) => {
                    unreachable_remotes.insert(remote.clone());
                }
            }
        }

        let mut active: Vec<String> = Vec::new();
        for remote_branch in &remote_branches {
            let Some(rest) = remote_branch.strip_prefix("remotes/") else {
                continue;
            };
            let Some((remote_name, branch_name)) = rest.split_once('/') else {
                continue;
            };
            if unreachable_remotes.contains(remote_name) {
                active.push(remote_branch.clone());
                continue;
            }
            if branches_by_remote
                .get(remote_name)
                .map(|set| set.contains(branch_name))
                .unwrap_or(false)
            {
                active.push(remote_branch.clone());
            }
        }

        let mut seen: HashSet<String> = active.iter().cloned().collect();
        for (remote_name, branches) in &branches_by_remote {
            for branch_name in branches {
                let qualified = format!("remotes/{}/{}", remote_name, branch_name);
                if seen.insert(qualified.clone()) {
                    active.push(qualified);
                }
            }
        }
        active
    }

    /// 解析各 remote 的默认分支：先从 `for-each-ref --format='%(refname) %(symref)'
    /// refs/remotes` 的 `refs/remotes/<remote>/HEAD` symref 提取；对没有 HEAD symref
    /// 的 remote，退回 `ls-remote --symref <remote> HEAD` 并经 `extract_symref_head`
    /// 解析（失败忽略）。返回 remote 到默认分支名的映射。
    async fn remote_default_branches(&self, repo_root: &Path) -> Map<String, Value> {
        let mut defaults = Map::new();
        let refs = self
            .raw(
                repo_root,
                &[
                    "for-each-ref".into(),
                    "--format=%(refname) %(symref)".into(),
                    "refs/remotes".into(),
                ],
            )
            .await
            .unwrap_or_default();
        for line in refs.lines() {
            let mut parts = line.trim().splitn(2, ' ');
            let reference = parts.next().unwrap_or("");
            let symbolic_ref = parts.next().unwrap_or("");
            let Some(rest) = reference.strip_prefix("refs/remotes/") else {
                continue;
            };
            let Some(remote_name) = rest.strip_suffix("/HEAD") else {
                continue;
            };
            let prefix = format!("refs/remotes/{}/", remote_name);
            if symbolic_ref.starts_with(&prefix) {
                defaults.insert(
                    remote_name.to_string(),
                    Value::String(symbolic_ref[prefix.len()..].to_string()),
                );
            }
        }

        let remotes = self.remote_names(repo_root).await;
        let missing: Vec<String> = remotes
            .into_iter()
            .filter(|name| !defaults.contains_key(name))
            .collect();
        if missing.is_empty() {
            return defaults;
        }
        for remote in missing {
            let output = self
                .raw(
                    repo_root,
                    &[
                        "ls-remote".into(),
                        "--symref".into(),
                        remote.clone(),
                        "HEAD".into(),
                    ],
                )
                .await
                .unwrap_or_default();
            if let Some(branch) = extract_symref_head(&output) {
                defaults.insert(remote, Value::String(branch));
            }
        }
        defaults
    }

    /// 统计各分支未推送的提交数：branch_names 去重去空后最多取前 5 个；仅统计
    /// `branch -v` 中真实存在的本地分支，解析其 `@{upstream}`（无 upstream 跳过），
    /// 用 `rev-list --count <upstream>..<branch>` 计数，只保留 count > 0 的项。
    /// 返回 `{ "counts": { "<branch>": n } }`。
    pub async fn get_unpushed_branch_counts(
        &self,
        directory: &str,
        branch_names: &[Value],
    ) -> ServiceResult<Value> {
        let context = self.repository_context(directory).await?;
        let mut requested: Vec<String> = Vec::new();
        for name in branch_names {
            let Some(name) = name.as_str().filter(|n| !n.is_empty()) else {
                continue;
            };
            if !requested.iter().any(|n| n == name) {
                requested.push(name.to_string());
            }
        }
        requested.truncate(5);
        if requested.is_empty() {
            return Ok(json!({ "counts": {} }));
        }

        let local_output = self
            .raw(&context.repo_root, &["branch".into(), "-v".into()])
            .await
            .unwrap_or_default();
        let local: HashSet<String> = parse_branch_summary(&local_output)
            .all
            .into_iter()
            .collect();

        let mut counts = Map::new();
        for branch in requested {
            if !local.contains(&branch) {
                continue;
            }
            let upstream = self
                .raw(
                    &context.repo_root,
                    &[
                        "rev-parse".into(),
                        "--abbrev-ref".into(),
                        "--symbolic-full-name".into(),
                        format!("{}@{{upstream}}", branch),
                    ],
                )
                .await
                .map(|out| out.trim().to_string())
                .unwrap_or_default();
            if upstream.is_empty() {
                continue;
            }
            let count = self
                .raw(
                    &context.repo_root,
                    &[
                        "rev-list".into(),
                        "--count".into(),
                        format!("{}..{}", upstream, branch),
                    ],
                )
                .await
                .ok()
                .and_then(|out| out.trim().parse::<i64>().ok())
                .unwrap_or(0);
            if count > 0 {
                counts.insert(branch, json!(count));
            }
        }
        Ok(json!({ "counts": counts }))
    }

    /// 创建并切换到新分支：`git checkout -b <branch_name> <start_point>`，
    /// 起点缺省为 HEAD。命令结果不检查（与 JS 版行为一致），始终返回 success 与分支名。
    pub async fn create_branch(
        &self,
        directory: &str,
        branch_name: &str,
        start_point: Option<&str>,
    ) -> ServiceResult<Value> {
        let context = self.repository_context(directory).await?;
        let start = start_point.filter(|s| !s.is_empty()).unwrap_or("HEAD");
        self.run(&context.repo_root, &["checkout", "-b", branch_name, start])
            .await;
        Ok(json!({ "success": true, "branch": branch_name }))
    }

    /// 切换分支：分支名去空白后为空直接报 "Branch name is required"；目标经
    /// `resolve_branch_checkout_target` 解析——远端分支首次检出时用
    /// `checkout -b <branch> --track <remote_ref>` 建立跟踪分支，
    /// 否则直接 `checkout <branch>`。
    pub async fn checkout_branch(
        &self,
        directory: &str,
        branch_name: &str,
    ) -> ServiceResult<Value> {
        let context = self.repository_context(directory).await?;
        let requested = branch_name.trim().to_string();
        if requested.is_empty() {
            return Err("Branch name is required".to_string());
        }

        let target = self
            .resolve_branch_checkout_target(&context.repo_root, &requested)
            .await?;
        if let Some(remote_ref) = &target.remote_ref {
            self.run(
                &context.repo_root,
                &["checkout", "-b", &target.branch, "--track", remote_ref],
            )
            .await;
        } else {
            self.run(&context.repo_root, &["checkout", &target.branch])
                .await;
        }
        Ok(json!({ "success": true, "branch": target.branch }))
    }

    /// 解析 checkout 请求对应的实际目标。请求名已是本地分支（`refs/heads/<name>` 存在）
    /// 时按原样返回；否则识别 `remotes/<remote>/<branch>` 或 `<remote>/<branch>` 形式：
    /// 远端跟踪引用不存在时先 `fetch <remote> <branch>`（fetch 失败或之后引用仍不存在
    /// 则报错），本地同名分支已存在时返回纯本地目标，否则返回带 `remote_ref` 的跟踪
    /// 目标（跳过 `<remote>/HEAD`）。无法识别为远端引用时按原样返回，交由 git 报错。
    async fn resolve_branch_checkout_target(
        &self,
        repo_root: &Path,
        requested: &str,
    ) -> ServiceResult<CheckoutTarget> {
        let as_requested = CheckoutTarget {
            branch: requested.to_string(),
            remote_ref: None,
        };
        if self
            .git_ref_exists(repo_root, &format!("refs/heads/{}", requested))
            .await
        {
            return Ok(as_requested);
        }
        let remote_ref_name = requested.strip_prefix("remotes/").unwrap_or(requested);
        let remotes = self.remote_names(repo_root).await;
        let Some(remote) = remotes
            .iter()
            .find(|name| !name.is_empty() && remote_ref_name.starts_with(&format!("{}/", name)))
        else {
            return Ok(as_requested);
        };
        let local_branch = remote_ref_name[remote.len() + 1..].to_string();
        if local_branch.is_empty() || local_branch == "HEAD" {
            return Ok(as_requested);
        }

        if !self
            .git_ref_exists(repo_root, &format!("refs/remotes/{}", remote_ref_name))
            .await
        {
            let fetch = self.run(repo_root, &["fetch", remote, &local_branch]).await;
            if !fetch.success {
                let text = failure_text(&fetch.stdout, &fetch.stderr, &fetch.message);
                return Err(format!(
                    "Failed to fetch {} from {}: {}",
                    local_branch,
                    remote,
                    text.trim()
                ));
            }
            if !self
                .git_ref_exists(repo_root, &format!("refs/remotes/{}", remote_ref_name))
                .await
            {
                return Err(format!(
                    "Branch {} no longer exists on remote {}",
                    local_branch, remote
                ));
            }
        }

        let local_exists = self
            .git_ref_exists(repo_root, &format!("refs/heads/{}", local_branch))
            .await;
        Ok(CheckoutTarget {
            branch: local_branch,
            remote_ref: if local_exists {
                None
            } else {
                Some(remote_ref_name.to_string())
            },
        })
    }

    /// 以 detached HEAD 检出指定提交：先用 `is_valid_commit_hash` 校验哈希格式，
    /// 再执行 `git checkout <hash>`；失败返回合成错误文本。
    pub async fn checkout_commit(&self, directory: &str, hash: &str) -> ServiceResult<Value> {
        if !is_valid_commit_hash(hash) {
            return Err("Invalid commit hash".to_string());
        }
        let context = self.repository_context(directory).await?;
        let result = self.run(&context.repo_root, &["checkout", hash]).await;
        if !result.success {
            return Err(
                failure_text(&result.stdout, &result.stderr, &result.message)
                    .trim()
                    .to_string(),
            );
        }
        Ok(json!({ "success": true }))
    }

    /// cherry-pick 指定提交（哈希先校验格式）。成功返回 `{ success, conflict: false }`；
    /// 失败文本命中 [`is_conflict_message`] 特征时返回 `conflict: true` 并附
    /// `conflict_files` 冲突文件列表；其余失败按合成文本报错。
    pub async fn cherry_pick(&self, directory: &str, hash: &str) -> ServiceResult<Value> {
        if !is_valid_commit_hash(hash) {
            return Err("Invalid commit hash".to_string());
        }
        let context = self.repository_context(directory).await?;
        let result = self.run(&context.repo_root, &["cherry-pick", hash]).await;
        if result.success {
            return Ok(json!({ "success": true, "conflict": false }));
        }
        let text = failure_text(&result.stdout, &result.stderr, &result.message);
        if is_conflict_message(&text) {
            return Ok(json!({
                "success": false,
                "conflict": true,
                "conflictFiles": conflict_files(self, &context.repo_root).await,
            }));
        }
        Err(text.trim().to_string())
    }

    /// 以 `git revert --no-commit <hash>` 反转指定提交（不自动生成提交，由调用方
    /// 自行 commit）。失败文本命中 [`is_revert_conflict_message`] 特征时返回
    /// `conflict: true` 与冲突文件列表；其余失败按合成文本报错。
    pub async fn revert_commit(&self, directory: &str, hash: &str) -> ServiceResult<Value> {
        if !is_valid_commit_hash(hash) {
            return Err("Invalid commit hash".to_string());
        }
        let context = self.repository_context(directory).await?;
        let result = self
            .run(&context.repo_root, &["revert", "--no-commit", hash])
            .await;
        if result.success {
            return Ok(json!({ "success": true, "conflict": false }));
        }
        let text = failure_text(&result.stdout, &result.stderr, &result.message);
        if is_revert_conflict_message(&text) {
            return Ok(json!({
                "success": false,
                "conflict": true,
                "conflictFiles": conflict_files(self, &context.repo_root).await,
            }));
        }
        Err(text.trim().to_string())
    }

    /// 以指定模式重置到某提交：mode 直接映射为 `--<mode>`。`hard` 且未传 force 时
    /// 先查 status，工作区存在未提交变更则拒绝执行并提示先 stash/commit 或使用 force；
    /// 哈希先经 `is_valid_commit_hash` 校验。失败返回合成错误文本。
    pub async fn reset_to_commit(
        &self,
        directory: &str,
        hash: &str,
        mode: &str,
        force: bool,
    ) -> ServiceResult<Value> {
        if !is_valid_commit_hash(hash) {
            return Err("Invalid commit hash".to_string());
        }
        let context = self.repository_context(directory).await?;
        if mode == "hard" && !force {
            let status = self.parse_status(&context.repo_root, &[]).await;
            if !status.files.is_empty() {
                return Err(
                    "Cannot hard reset: uncommitted changes in working tree. Stash or commit first, or use force."
                        .to_string(),
                );
            }
        }
        let result = self
            .run(&context.repo_root, &["reset", &format!("--{}", mode), hash])
            .await;
        if !result.success {
            return Err(
                failure_text(&result.stdout, &result.stderr, &result.message)
                    .trim()
                    .to_string(),
            );
        }
        Ok(json!({ "success": true }))
    }

    /// 删除本地分支：force 为 true 用 `-D`（强制删除），否则 `-d`（git 会拒绝删除
    /// 未合并分支）；branch 自动剥离可选的 `refs/heads/` 前缀。失败返回合成错误文本。
    pub async fn delete_branch(
        &self,
        directory: &str,
        branch: &str,
        force: bool,
    ) -> ServiceResult<Value> {
        let context = self.repository_context(directory).await?;
        let branch_name = branch.strip_prefix("refs/heads/").unwrap_or(branch);
        let flag = if force { "-D" } else { "-d" };
        let result = self
            .run(&context.repo_root, &["branch", flag, branch_name])
            .await;
        if !result.success {
            return Err(
                failure_text(&result.stdout, &result.stderr, &result.message)
                    .trim()
                    .to_string(),
            );
        }
        Ok(json!({ "success": true }))
    }

    /// 重命名本地分支：先读取旧分支的 `branch.<name>.remote`/`.merge` 本地配置，
    /// `git branch -m` 成功后，若旧分支配置过 upstream，则为新分支名恢复
    /// `--set-upstream-to=<remote>/<branch>`（merge 引用指向旧名时同步替换为新名，
    /// 恢复失败被忽略）。改名失败返回合成错误文本。
    pub async fn rename_branch(
        &self,
        directory: &str,
        old_name: &str,
        new_name: &str,
    ) -> ServiceResult<Value> {
        let context = self.repository_context(directory).await?;
        let normalized_old = clean_branch_name(old_name.trim());
        let normalized_new = clean_branch_name(new_name.trim());

        let previous_remote = self
            .get_config(
                &context.repo_root,
                &format!("branch.{}.remote", normalized_old),
                "local",
            )
            .await
            .unwrap_or_default();
        let previous_merge = self
            .get_config(
                &context.repo_root,
                &format!("branch.{}.merge", normalized_old),
                "local",
            )
            .await
            .unwrap_or_default();

        let result = self
            .run(&context.repo_root, &["branch", "-m", old_name, new_name])
            .await;
        if !result.success {
            return Err(
                failure_text(&result.stdout, &result.stderr, &result.message)
                    .trim()
                    .to_string(),
            );
        }

        if !previous_remote.is_empty() && !previous_merge.is_empty() && !normalized_new.is_empty() {
            let previous_merge_branch = clean_branch_name(&previous_merge);
            let next_merge_branch = if previous_merge_branch == normalized_old {
                normalized_new.clone()
            } else {
                previous_merge_branch
            };
            let full = format!("{}/{}", previous_remote, next_merge_branch);
            if !previous_remote.trim().is_empty() && !next_merge_branch.trim().is_empty() {
                let _ = self
                    .run_or_throw(
                        &context.repo_root,
                        &[
                            "branch",
                            &format!("--set-upstream-to={}", full),
                            &normalized_new,
                        ],
                        &format!("Failed to set upstream to {}", full),
                    )
                    .await;
            }
        }

        Ok(json!({ "success": true, "branch": new_name }))
    }

    // -----------------------------------------------------------------------
    // Remotes / push / pull / fetch
    // -----------------------------------------------------------------------

    /// 列出远端配置：解析 `git remote -v` 的 `name\turl (fetch|push)` 行，按首次出现
    /// 顺序聚合为 name/fetchUrl/pushUrl 条目数组。错误文本含 "not a git repository"
    /// 时静默返回空数组，其余错误照常返回。
    pub async fn get_remotes(&self, directory: &str) -> ServiceResult<Value> {
        let context = self.repository_context(directory).await?;
        let output = self
            .raw(&context.repo_root, &["remote".into(), "-v".into()])
            .await
            .map_err(|f| {
                let text = failure_text(&f.stdout, &f.stderr, &f.message);
                if text.to_lowercase().contains("not a git repository") {
                    String::new()
                } else {
                    text.trim().to_string()
                }
            });
        let output = match output {
            Ok(out) => out,
            Err(empty) if empty.is_empty() => return Ok(json!([])),
            Err(other) => return Err(other),
        };

        // name\turl (fetch|push)
        let mut order: Vec<String> = Vec::new();
        let mut refs: HashMap<String, (String, String)> = HashMap::new();
        for line in output.lines().filter(|l| !l.trim().is_empty()) {
            let (name, rest) = match line.split_once('\t') {
                Some(pair) => pair,
                None => continue,
            };
            let rest = rest.trim();
            let (url, purpose) = match rest.rsplit_once('(') {
                Some((url, tail)) => (url.trim(), tail.trim_end_matches(')').to_string()),
                None => (rest, String::new()),
            };
            let entry = refs
                .entry(name.to_string())
                .or_insert_with(|| (String::new(), String::new()));
            if purpose == "fetch" && !url.is_empty() {
                entry.0 = url.to_string();
            } else if purpose == "push" && !url.is_empty() {
                entry.1 = url.to_string();
            }
            if !order.iter().any(|n| n == name) {
                order.push(name.to_string());
            }
        }
        let remotes: Vec<Value> = order
            .into_iter()
            .map(|name| {
                let (fetch_url, push_url) = refs.get(&name).cloned().unwrap_or_default();
                json!({ "name": name, "fetchUrl": fetch_url, "pushUrl": push_url })
            })
            .collect();
        Ok(Value::Array(remotes))
    }

    /// 移除远端：名称去空白后必填，且禁止移除 origin；执行 `git remote remove <name>`，
    /// 失败返回合成错误文本。
    pub async fn remove_remote(&self, directory: &str, remote_name: &str) -> ServiceResult<Value> {
        let remote = remote_name.trim();
        if remote.is_empty() {
            return Err("remote is required to remove a remote".to_string());
        }
        if remote == "origin" {
            return Err("Cannot remove origin remote".to_string());
        }
        let context = self.repository_context(directory).await?;
        let result = self
            .run(&context.repo_root, &["remote", "remove", remote])
            .await;
        if !result.success {
            return Err(
                failure_text(&result.stdout, &result.stderr, &result.message)
                    .trim()
                    .to_string(),
            );
        }
        Ok(json!({ "success": true }))
    }

    /// 拉取并合并远端更新：remote/branch 取自 options；只提供 remote 时用当前分支
    /// （status.current）补全 branch；`options.rebase == true` 时附加 `--rebase`。
    /// 命令失败返回合成错误文本，成功返回经 `parse_pull_summary` 解析的汇总 JSON。
    pub async fn pull(&self, directory: &str, options: &Value) -> ServiceResult<Value> {
        let context = self.repository_context(directory).await?;
        let remote = options
            .get("remote")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let mut branch = options
            .get("branch")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let with_rebase = options.get("rebase") == Some(&Value::Bool(true));

        if !remote.is_empty() && branch.is_empty() {
            let status = self.parse_status(&context.repo_root, &[]).await;
            branch = status.current.unwrap_or_default().trim().to_string();
        }

        let mut argv: Vec<String> = vec!["pull".into()];
        if with_rebase {
            argv.push("--rebase".into());
        }
        if !remote.is_empty() && !branch.is_empty() {
            argv.insert(1, remote.clone());
            argv.insert(2, branch.clone());
        }
        let result = self.run_strings(&context.repo_root, &argv).await;
        if !result.success {
            return Err(
                failure_text(&result.stdout, &result.stderr, &result.message)
                    .trim()
                    .to_string(),
            );
        }
        let summary = parse_pull_summary(&result.stdout, &result.stderr);
        Ok(summary)
    }

    /// 推送到远端，带多层 upstream 兜底：remote/branch 均缺省时先做无参 push，
    /// 失败且特征像缺 upstream（[`looks_like_missing_upstream`]）时取当前分支并回退到
    /// origin（或首个 remote），附带 `--set-upstream` 重试；只提供 remote 且当前分支
    /// 无跟踪时同样直接带 `--set-upstream` 推当前分支；常规路径按 remote[:branch] 推送，
    /// 失败且像缺 upstream 时用有效分支名带 `--set-upstream` 再试一次。
    /// 错误统一经 [`first_push_error`] 规整后返回。
    pub async fn push(&self, directory: &str, options: &Value) -> ServiceResult<Value> {
        let context = self.repository_context(directory).await?;
        let remote = options
            .get("remote")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let branch = options
            .get("branch")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let raw_options = options.get("options");

        let push_argv =
            |remote: Option<&str>, branch: Option<&str>, extra: &[String]| -> Vec<String> {
                // simple-git pushTask: ["push", ...customArgs, remote?, branch?,
                // "--verbose", "--porcelain"] with "-v" removed.
                let mut argv = vec!["push".to_string()];
                argv.extend(extra.iter().cloned());
                if let Some(branch) = branch {
                    argv.insert(1, branch.to_string());
                }
                if let Some(remote) = remote {
                    argv.insert(1, remote.to_string());
                }
                argv.retain(|arg| arg != "-v");
                argv.push("--verbose".into());
                argv.push("--porcelain".into());
                argv
            };

        let extra_flags = build_raw_git_options(raw_options);
        let upstream_extra = |extra: &[String]| -> Vec<String> {
            let mut out = extra.to_vec();
            if !out.iter().any(|arg| arg == "--set-upstream") {
                out.push("--set-upstream".into());
            }
            out
        };

        if remote.is_empty() && branch.is_empty() {
            match self
                .push_once(
                    &context.repo_root,
                    push_argv(None, None, &extra_flags),
                    directory,
                )
                .await
            {
                Ok(value) => {
                    return Ok(json!({
                        "success": true,
                        "pushed": value.get("pushed").cloned().unwrap_or(json!([])),
                        "repo": directory,
                        "ref": Value::Null,
                    }));
                }
                Err(text) => {
                    if !looks_like_missing_upstream(&text) {
                        return Err(first_push_error(&text));
                    }
                    let status = self.parse_status(&context.repo_root, &[]).await;
                    let current = status.current.clone().unwrap_or_default();
                    let remotes = self.remote_names(&context.repo_root).await;
                    let fallback_remote = remotes
                        .iter()
                        .find(|name| *name == "origin")
                        .or_else(|| remotes.first())
                        .cloned()
                        .unwrap_or_default();
                    if current.is_empty() || fallback_remote.is_empty() {
                        return Err(first_push_error(&text));
                    }
                    return match self
                        .push_once(
                            &context.repo_root,
                            push_argv(
                                Some(&fallback_remote),
                                Some(&current),
                                &upstream_extra(&extra_flags),
                            ),
                            directory,
                        )
                        .await
                    {
                        Ok(value) => Ok(value),
                        Err(fallback_text) => Err(first_push_error(&fallback_text)),
                    };
                }
            }
        }

        let remote_name = if remote.is_empty() {
            "origin".to_string()
        } else {
            remote.clone()
        };

        if branch.is_empty() {
            let status = self.parse_status(&context.repo_root, &[]).await;
            if let Some(current) = &status.current
                && status.tracking.as_deref().unwrap_or("").is_empty()
            {
                return match self
                    .push_once(
                        &context.repo_root,
                        push_argv(
                            Some(&remote_name),
                            Some(current),
                            &upstream_extra(&extra_flags),
                        ),
                        directory,
                    )
                    .await
                {
                    Ok(value) => Ok(value),
                    Err(text) => Err(first_push_error(&text)),
                };
            }
        }

        match self
            .push_once(
                &context.repo_root,
                push_argv(
                    Some(&remote_name),
                    if branch.is_empty() {
                        None
                    } else {
                        Some(&branch)
                    },
                    &extra_flags,
                ),
                directory,
            )
            .await
        {
            Ok(value) => Ok(value),
            Err(text) => {
                if !looks_like_missing_upstream(&text) {
                    return Err(first_push_error(&text));
                }
                let status = self.parse_status(&context.repo_root, &[]).await;
                let effective_branch = if branch.is_empty() {
                    status.current.clone().unwrap_or_default()
                } else {
                    branch.clone()
                };
                if effective_branch.is_empty() {
                    return Err(first_push_error(&text));
                }
                match self
                    .push_once(
                        &context.repo_root,
                        push_argv(
                            Some(&remote_name),
                            Some(&effective_branch),
                            &upstream_extra(&extra_flags),
                        ),
                        directory,
                    )
                    .await
                {
                    Ok(value) => Ok(value),
                    Err(fallback_text) => Err(first_push_error(&fallback_text)),
                }
            }
        }
    }

    /// 删除远端分支：执行 `git push <remote> :<branch> --verbose --porcelain`。
    /// `branch` 必填（自动剥离可选的 `refs/heads/` 前缀），`remote` 缺省为 origin；
    /// 命令失败返回合成错误文本，成功返回 `{ success: true }`。
    pub async fn delete_remote_branch(
        &self,
        directory: &str,
        options: &Value,
    ) -> ServiceResult<Value> {
        let branch = options
            .get("branch")
            .and_then(Value::as_str)
            .filter(|b| !b.is_empty())
            .ok_or_else(|| "branch is required to delete remote branch".to_string())?;
        let remote = options
            .get("remote")
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
            .unwrap_or("origin");
        let context = self.repository_context(directory).await?;
        let target_branch = branch.strip_prefix("refs/heads/").unwrap_or(branch);
        let argv = vec![
            "push".to_string(),
            remote.to_string(),
            format!(":{}", target_branch),
            "--verbose".to_string(),
            "--porcelain".to_string(),
        ];
        let result = self.run_strings(&context.repo_root, &argv).await;
        if !result.success {
            return Err(
                failure_text(&result.stdout, &result.stderr, &result.message)
                    .trim()
                    .to_string(),
            );
        }
        Ok(json!({ "success": true }))
    }

    /// 拉取远端更新。`remote`/`branch` 取自 options（其 `options` 字段经
    /// `build_raw_git_options` 展开为透传参数）：仅提供 remote 时 fetch 该 remote；
    /// remote 与 branch 齐全时 fetch 指定分支；两者都缺省时 fetch origin。
    /// 命令失败返回合成错误文本。
    pub async fn fetch(&self, directory: &str, options: &Value) -> ServiceResult<Value> {
        let context = self.repository_context(directory).await?;
        let remote = options
            .get("remote")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let branch = options
            .get("branch")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let extra = build_raw_git_options(options.get("options"));

        let argv: Vec<String> = if !remote.is_empty() && branch.is_empty() {
            let mut argv = vec!["fetch".to_string()];
            argv.extend(extra);
            argv.push(remote.clone());
            argv
        } else {
            let mut argv = vec!["fetch".to_string()];
            argv.extend(extra);
            if !remote.is_empty() && !branch.is_empty() {
                argv.push(if remote.is_empty() {
                    "origin".to_string()
                } else {
                    remote.clone()
                });
                argv.push(branch.clone());
            } else if remote.is_empty() && branch.is_empty() {
                argv.push("origin".to_string());
            }
            argv
        };

        let result = self.run_strings(&context.repo_root, &argv).await;
        if !result.success {
            return Err(
                failure_text(&result.stdout, &result.stderr, &result.message)
                    .trim()
                    .to_string(),
            );
        }
        Ok(json!({ "success": true }))
    }

    // -----------------------------------------------------------------------
    // Merge / rebase
    // -----------------------------------------------------------------------

    /// 变基到 `options.onto` 指定的基点（必填）。成功返回 `{ success, conflict: false }`；
    /// 失败文本命中 [`is_rebase_conflict_message`] 特征时返回 `conflict: true` 并附
    /// `conflict_files` 冲突文件列表；其余失败按合成文本报错。
    pub async fn rebase(&self, directory: &str, options: &Value) -> ServiceResult<Value> {
        let context = self.repository_context(directory).await?;
        let onto = options
            .get("onto")
            .and_then(Value::as_str)
            .filter(|o| !o.is_empty())
            .ok_or_else(|| "onto parameter is required for rebase".to_string())?
            .to_string();
        let result = self.run(&context.repo_root, &["rebase", &onto]).await;
        if result.success {
            return Ok(json!({ "success": true, "conflict": false }));
        }
        let text = failure_text(&result.stdout, &result.stderr, &result.message);
        if is_rebase_conflict_message(&text) {
            return Ok(json!({
                "success": false,
                "conflict": true,
                "conflictFiles": conflict_files(self, &context.repo_root).await,
            }));
        }
        Err(text.trim().to_string())
    }

    /// 执行 `git rebase --abort` 放弃进行中的变基；失败返回合成错误文本。
    pub async fn abort_rebase(&self, directory: &str) -> ServiceResult<Value> {
        let context = self.repository_context(directory).await?;
        let result = self.run(&context.repo_root, &["rebase", "--abort"]).await;
        if !result.success {
            return Err(
                failure_text(&result.stdout, &result.stderr, &result.message)
                    .trim()
                    .to_string(),
            );
        }
        Ok(json!({ "success": true }))
    }

    /// 合并 `options.branch` 指定的分支（必填）。成功返回 `{ success, conflict: false }`；
    /// 失败文本（大小写不敏感）含 "conflict" 或 "automatic merge failed" 时返回
    /// `conflict: true` 并附 `conflict_files` 冲突文件列表；其余失败按合成文本报错。
    pub async fn merge(&self, directory: &str, options: &Value) -> ServiceResult<Value> {
        let context = self.repository_context(directory).await?;
        let branch = options
            .get("branch")
            .and_then(Value::as_str)
            .filter(|b| !b.is_empty())
            .ok_or_else(|| "branch parameter is required for merge".to_string())?
            .to_string();
        let result = self.run(&context.repo_root, &["merge", &branch]).await;
        if result.success {
            return Ok(json!({ "success": true, "conflict": false }));
        }
        let text = failure_text(&result.stdout, &result.stderr, &result.message);
        let lowered = text.to_lowercase();
        if lowered.contains("conflict") || lowered.contains("automatic merge failed") {
            return Ok(json!({
                "success": false,
                "conflict": true,
                "conflictFiles": conflict_files(self, &context.repo_root).await,
            }));
        }
        Err(text.trim().to_string())
    }

    /// 执行 `git merge --abort` 放弃进行中的合并；失败返回合成错误文本。
    pub async fn abort_merge(&self, directory: &str) -> ServiceResult<Value> {
        let context = self.repository_context(directory).await?;
        let result = self.run(&context.repo_root, &["merge", "--abort"]).await;
        if !result.success {
            return Err(
                failure_text(&result.stdout, &result.stderr, &result.message)
                    .trim()
                    .to_string(),
            );
        }
        Ok(json!({ "success": true }))
    }

    /// 继续变基：以 `GIT_EDITOR=true` 执行 `rebase --continue`，避免交互式编辑器阻塞。
    /// 失败文本命中 [`is_continue_conflict_message`] 时返回 `conflict: true` 与冲突文件
    /// 列表；输出含 "nothing to commit"/"no changes"（空提交场景）时自动补一次
    /// `rebase --skip` 并按成功返回；其余失败按合成文本报错。
    pub async fn continue_rebase(&self, directory: &str) -> ServiceResult<Value> {
        let context = self.repository_context(directory).await?;
        let mut env = HashMap::new();
        env.insert("GIT_EDITOR".to_string(), "true".to_string());
        let result = self
            .run_with_env(&context.repo_root, &["rebase", "--continue"], env)
            .await;
        if result.success {
            return Ok(json!({ "success": true, "conflict": false }));
        }
        let text = failure_text(&result.stdout, &result.stderr, &result.message);
        if is_continue_conflict_message(&text) {
            return Ok(json!({
                "success": false,
                "conflict": true,
                "conflictFiles": conflict_files(self, &context.repo_root).await,
            }));
        }
        let lowered = text.to_lowercase();
        if lowered.contains("nothing to commit") || lowered.contains("no changes") {
            let mut env = HashMap::new();
            env.insert("GIT_EDITOR".to_string(), "true".to_string());
            let skip = self
                .run_with_env(&context.repo_root, &["rebase", "--skip"], env)
                .await;
            let _ = skip;
            return Ok(json!({ "success": true, "conflict": false }));
        }
        Err(text.trim().to_string())
    }

    /// 继续合并：先查 status，仍存在冲突文件时直接返回 `conflict: true` 与冲突列表；
    /// 否则以 `GIT_EDITOR=true` 执行 `git -c core.abbrev=40 commit --no-edit` 完成
    /// 合并提交。失败文本命中 [`is_continue_conflict_message`] 时同样返回冲突，
    /// 含 "nothing to commit"/"no changes added" 时视为合并已完成返回成功；
    /// 其余失败按合成文本报错。
    pub async fn continue_merge(&self, directory: &str) -> ServiceResult<Value> {
        let context = self.repository_context(directory).await?;
        let status = self.parse_status(&context.repo_root, &[]).await;
        if !status.conflicted.is_empty() {
            return Ok(json!({
                "success": false,
                "conflict": true,
                "conflictFiles": status.conflicted,
            }));
        }
        let mut env = HashMap::new();
        env.insert("GIT_EDITOR".to_string(), "true".to_string());
        let argv = vec![
            "-c".to_string(),
            "core.abbrev=40".to_string(),
            "commit".to_string(),
            "--no-edit".to_string(),
        ];
        let result = self
            .run_strings_with_env(&context.repo_root, &argv, env)
            .await;
        if result.success {
            return Ok(json!({ "success": true, "conflict": false }));
        }
        let text = failure_text(&result.stdout, &result.stderr, &result.message);
        if is_continue_conflict_message(&text) {
            return Ok(json!({
                "success": false,
                "conflict": true,
                "conflictFiles": conflict_files(self, &context.repo_root).await,
            }));
        }
        let lowered = text.to_lowercase();
        if lowered.contains("nothing to commit") || lowered.contains("no changes added") {
            return Ok(json!({ "success": true, "conflict": false }));
        }
        Err(text.trim().to_string())
    }

    /// 汇总当前冲突现场的详情：`status --porcelain` 原文、`diff --name-only
    /// --diff-filter=U` 的未合并文件列表、`diff NO_EXT_DIFF` 的冲突 diff，以及
    /// headInfo 与 operation——存在 MERGE_HEAD 时判定为 merge 并附 MERGE_HEAD 哈希与
    /// MERGE_MSG 文件内容；否则存在 REBASE_HEAD 时判定为 rebase 并附 REBASE_HEAD 哈希。
    /// 各探测命令失败时均按空值兜底，不产生错误。
    pub async fn get_conflict_details(&self, directory: &str) -> ServiceResult<Value> {
        let context = self.repository_context(directory).await?;
        let status_porcelain = self
            .raw(&context.repo_root, &["status".into(), "--porcelain".into()])
            .await
            .unwrap_or_default();
        let unmerged = self
            .raw(
                &context.repo_root,
                &[
                    "diff".into(),
                    "--name-only".into(),
                    "--diff-filter=U".into(),
                ],
            )
            .await
            .unwrap_or_default();
        let diff = self
            .raw(&context.repo_root, &["diff".into(), NO_EXT_DIFF.into()])
            .await
            .unwrap_or_default();

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

        let mut operation = "merge";
        let mut head_info = String::new();
        if merge_head_exists {
            let merge_head = self
                .raw(
                    &context.repo_root,
                    &["rev-parse".into(), "MERGE_HEAD".into()],
                )
                .await
                .unwrap_or_default();
            let merge_msg = match self
                .resolve_git_internal_path(&context.repo_root, "MERGE_MSG")
                .await
            {
                Ok(path) => tokio::fs::read_to_string(path).await.unwrap_or_default(),
                Err(_) => String::new(),
            };
            head_info = format!("MERGE_HEAD: {}\n{}", merge_head.trim(), merge_msg);
        } else {
            let rebase_head_exists = self
                .raw(
                    &context.repo_root,
                    &[
                        "rev-parse".into(),
                        "--verify".into(),
                        "--quiet".into(),
                        "REBASE_HEAD".into(),
                    ],
                )
                .await
                .is_ok();
            if rebase_head_exists {
                operation = "rebase";
                let rebase_head = self
                    .raw(
                        &context.repo_root,
                        &["rev-parse".into(), "REBASE_HEAD".into()],
                    )
                    .await
                    .unwrap_or_default();
                head_info = format!("REBASE_HEAD: {}", rebase_head.trim());
            }
        }

        Ok(json!({
            "statusPorcelain": status_porcelain.trim(),
            "unmergedFiles": super::paths::trim_git_lines(&unmerged),
            "diff": diff.trim(),
            "headInfo": head_info.trim(),
            "operation": operation,
        }))
    }

    // -----------------------------------------------------------------------
    // Stashes
    // -----------------------------------------------------------------------

    /// 列出全部 stash：执行 `git stash list --format=%gd␟%gs␟%cr␟%H`
    /// （以 U+001F 单元分隔符分列），解析为 ref/message/relativeTime/hash 条目数组；
    /// 命令失败时返回经 `failure_text` 合成的错误文本。
    pub async fn list_stashes(&self, directory: &str) -> ServiceResult<Vec<Value>> {
        let context = self.repository_context(directory).await?;
        let output = self
            .raw(
                &context.repo_root,
                &[
                    "stash".into(),
                    "list".into(),
                    "--format=%gd\u{1f}%gs\u{1f}%cr\u{1f}%H".into(),
                ],
            )
            .await
            .map_err(|f| {
                failure_text(&f.stdout, &f.stderr, &f.message)
                    .trim()
                    .to_string()
            })?;
        let mut stashes = Vec::new();
        for line in output.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let mut parts = line.splitn(4, '\u{1f}');
            let reference = parts.next().unwrap_or("").to_string();
            if reference.is_empty() {
                continue;
            }
            stashes.push(json!({
                "ref": reference,
                "message": parts.next().unwrap_or(""),
                "relativeTime": parts.next().unwrap_or(""),
                "hash": parts.next().unwrap_or(""),
            }));
        }
        Ok(stashes)
    }

    /// 统计各 stash 涉及的文件数：`refs` 为字符串数组，先去重去空白，再对每个引用执行
    /// `git stash show --name-only` 统计非空行数（命令失败按 0 计），
    /// 返回 `{ "counts": { "<ref>": n } }`。
    pub async fn count_stash_files(
        &self,
        directory: &str,
        refs: Option<&Value>,
    ) -> ServiceResult<Value> {
        let context = self.repository_context(directory).await?;
        let mut unique_refs: Vec<String> = Vec::new();
        if let Some(items) = refs.and_then(Value::as_array) {
            for value in items {
                let text = value.as_str().unwrap_or("").trim().to_string();
                if !text.is_empty() && !unique_refs.contains(&text) {
                    unique_refs.push(text);
                }
            }
        }
        let mut counts = Map::new();
        for reference in unique_refs {
            let names = self
                .raw(
                    &context.repo_root,
                    &[
                        "stash".into(),
                        "show".into(),
                        "--name-only".into(),
                        reference.clone(),
                    ],
                )
                .await;
            let count = match names {
                Ok(out) => out
                    .lines()
                    .map(|l| l.trim())
                    .filter(|l| !l.is_empty())
                    .count(),
                Err(_) => 0,
            };
            counts.insert(reference, json!(count));
        }
        Ok(json!({ "counts": counts }))
    }

    /// 创建 stash：执行 `git stash push --include-untracked -m <message>`。
    /// message 取 options.message（去空白），缺省为 "OMPChamber stash <ISO 时间>"；
    /// 输出包含 "no local changes" 时 created=false，表示没有可入栈的变更。
    pub async fn stash_push(&self, directory: &str, options: &Value) -> ServiceResult<Value> {
        let context = self.repository_context(directory).await?;
        let message = options
            .get("message")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("OMPChamber stash {}", iso_now()));
        let output = self
            .raw(
                &context.repo_root,
                &[
                    "stash".into(),
                    "push".into(),
                    "--include-untracked".into(),
                    "-m".into(),
                    message.clone(),
                ],
            )
            .await
            .map_err(|f| {
                failure_text(&f.stdout, &f.stderr, &f.message)
                    .trim()
                    .to_string()
            })?;
        let created = !output.to_lowercase().contains("no local changes");
        Ok(json!({
            "success": true,
            "created": created,
            "message": message,
            "output": output.trim(),
        }))
    }

    /// 应用 stash（引用取自 `stash_ref`，默认 `stash@{0}`）：先尝试保留暂存区结构的
    /// `stash apply --index`，失败时退回不带 `--index` 的普通 apply；
    /// 两次都失败才向上返回合成错误文本。
    pub async fn stash_apply(&self, directory: &str, options: &Value) -> ServiceResult<Value> {
        let context = self.repository_context(directory).await?;
        let reference = stash_ref(options);
        let with_index = self
            .raw(
                &context.repo_root,
                &[
                    "stash".into(),
                    "apply".into(),
                    "--index".into(),
                    reference.clone(),
                ],
            )
            .await;
        if with_index.is_err() {
            self.raw(
                &context.repo_root,
                &["stash".into(), "apply".into(), reference.clone()],
            )
            .await
            .map_err(|f| {
                failure_text(&f.stdout, &f.stderr, &f.message)
                    .trim()
                    .to_string()
            })?;
        }
        Ok(json!({ "success": true, "ref": reference }))
    }

    /// 删除指定 stash 引用（`git stash drop <ref>`），失败时返回合成错误文本。
    pub async fn stash_drop(&self, directory: &str, options: &Value) -> ServiceResult<Value> {
        let context = self.repository_context(directory).await?;
        let reference = stash_ref(options);
        self.raw(
            &context.repo_root,
            &["stash".into(), "drop".into(), reference.clone()],
        )
        .await
        .map_err(|f| {
            failure_text(&f.stdout, &f.stderr, &f.message)
                .trim()
                .to_string()
        })?;
        Ok(json!({ "success": true, "ref": reference }))
    }

    /// 弹出 stash：依次执行 `stash_apply` 与 `stash_drop`，任一步失败即中止并向上
    /// 透传其错误；成功返回 `{ success, ref }`。
    pub async fn stash_pop(&self, directory: &str, options: &Value) -> ServiceResult<Value> {
        let reference = stash_ref(options);
        self.stash_apply(directory, options).await?;
        self.stash_drop(directory, options).await?;
        Ok(json!({ "success": true, "ref": reference }))
    }

    // -----------------------------------------------------------------------
    // Log / commit inspection
    // -----------------------------------------------------------------------

    /// 获取提交历史。`options.all` 为真时走全引用通道：`log --all --topo-order
    /// --shortstat` 加自定义分隔 format，由 `parse_raw_log_entries` 解析出带
    /// refs/作者字段的条目。常规通道先用 `resolve_base_ref_for_log_with` 归一化
    /// from 引用，再执行 simple-git 风格的 `\u{f2}` 边界 format log（单文件历史时
    /// 附加 `--follow` 与 `:(top)<path>`，并使用 `from...to` 范围），得到主列表；
    /// 随后跑第二条 `--shortstat` log 由 `parse_log_stats` 补充各提交的
    /// diffstat 与 parents，合并为 all/latest/total。max_count 缺省或非正时取 50。
    pub async fn get_log(&self, directory: &str, options: &LogOptions) -> ServiceResult<Value> {
        let context = self.repository_context(directory).await?;
        let max_count = if options.max_count.unwrap_or(0) > 0 {
            options.max_count.unwrap()
        } else {
            50
        };

        if options.all {
            let mut argv: Vec<String> = vec![
                "log".into(),
                format!("--max-count={}", max_count),
                "--all".into(),
                "--topo-order".into(),
                "--date=iso".into(),
                "--pretty=format:%x1e%H%x1f%P%x1f%an%x1f%ae%x1f%ad%x1f%s%x1f%D".into(),
                "--shortstat".into(),
            ];
            let raw_log = self.raw(&context.repo_root, &argv).await.map_err(|f| {
                failure_text(&f.stdout, &f.stderr, &f.message)
                    .trim()
                    .to_string()
            })?;
            argv.clear();
            let entries = parse_raw_log_entries(&raw_log, true);
            let latest = entries.first().cloned();
            return Ok(json!({ "all": entries, "latest": latest, "total": entries.len() }));
        }

        let file_path = match options.file.as_deref().filter(|f| !f.is_empty()) {
            Some(file) => {
                let fc = self
                    .resolve_git_file_context(&context.directory_path, &context.repo_root, file)
                    .await?;
                Some(fc.repo_path)
            }
            None => None,
        };

        // simple-git `git.log({...})` — the boundary format is:
        // `òòòòòò %H òò %aI òò %s òò %D òò %b òò %aN òò %aE òò`
        let resolved_from = self
            .resolve_base_ref_for_log_with(&context.repo_root, options.from.as_deref())
            .await;

        let mut log_argv: Vec<String> = vec![format!(
            "--pretty=format:\u{f2}\u{f2}\u{f2}\u{f2}\u{f2}\u{f2} %H \u{f2} %aI \u{f2} %s \u{f2} %D \u{f2} %b \u{f2} %aN \u{f2} %aE \u{f2}\u{f2}"
        )];
        log_argv.push(format!("--max-count={}", max_count));
        if let Some(file) = &file_path {
            log_argv.push("--follow".into());
            log_argv.push(format!(":(top){}", file));
        }
        if let Some(from) = &resolved_from {
            log_argv.push(format!(
                "{}...{}",
                from,
                options.to.as_deref().unwrap_or("")
            ));
        }
        let mut base_argv = vec!["log".to_string()];
        base_argv.extend(log_argv);
        let base_output = self
            .raw(&context.repo_root, &base_argv)
            .await
            .map_err(|f| {
                failure_text(&f.stdout, &f.stderr, &f.message)
                    .trim()
                    .to_string()
            })?;
        let base_entries = parse_boundary_log(&base_output);
        let base_total = base_entries.len();

        let mut stats_argv: Vec<String> = vec![
            "log".into(),
            format!("--max-count={}", max_count),
            "--date=iso".into(),
            "--pretty=format:%x1e%H%x1f%P%x1f%an%x1f%ae%x1f%ad%x1f%s".into(),
            "--shortstat".into(),
        ];
        if let Some(from) = &resolved_from {
            if let Some(to) = options.to.as_deref().filter(|t| !t.is_empty()) {
                stats_argv.push(format!("{}..{}", from, to));
            } else {
                stats_argv.push(format!("{}..HEAD", from));
            }
        } else if let Some(to) = options.to.as_deref().filter(|t| !t.is_empty()) {
            stats_argv.push(to.to_string());
        }
        if let Some(file) = &file_path {
            stats_argv.push("--".into());
            stats_argv.push(file.clone());
        }
        let stats_output = self
            .raw(&context.repo_root, &stats_argv)
            .await
            .unwrap_or_default();
        let stats = parse_log_stats(&stats_output);

        let merged: Vec<Value> = base_entries
            .iter()
            .map(|entry| {
                let stat = stats
                    .get(entry.hash_str().as_str())
                    .cloned()
                    .unwrap_or_default();
                json!({
                    "hash": entry.hash,
                    "date": entry.date,
                    "message": entry.message,
                    "refs": entry.refs.clone().unwrap_or_default(),
                    "body": entry.body.clone().unwrap_or_default(),
                    "author_name": entry.author_name,
                    "author_email": entry.author_email,
                    "filesChanged": stat.files_changed,
                    "insertions": stat.insertions,
                    "deletions": stat.deletions,
                    "parents": stat.parents,
                })
            })
            .collect();
        let latest = merged.first().cloned().unwrap_or(Value::Null);
        Ok(json!({ "all": merged, "latest": latest, "total": base_total }))
    }

    /// 获取单个提交的变更文件列表：先 `git show --numstat` 取
    /// path/insertions/deletions（"-" 列表示二进制，计数按 0），再
    /// `git show --name-status` 取 A/M/D/R/C 状态（R/C 取制表符分隔的最后一个路径），
    /// 状态按 `numstat_destination_path` 归一化后的路径匹配；匹配不到时
    /// 含 " => " 的路径记为 R，否则记为 M。
    pub async fn get_commit_files(
        &self,
        directory: &str,
        commit_hash: &str,
    ) -> ServiceResult<Value> {
        let context = self.repository_context(directory).await?;
        let numstat = self
            .raw(
                &context.repo_root,
                &[
                    "show".into(),
                    "--numstat".into(),
                    "--format=".into(),
                    commit_hash.to_string(),
                ],
            )
            .await
            .map_err(|f| {
                failure_text(&f.stdout, &f.stderr, &f.message)
                    .trim()
                    .to_string()
            })?;

        let mut files: Vec<(String, i64, i64, bool)> = Vec::new();
        for line in numstat.lines().filter(|l| !l.trim().is_empty()) {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 3 {
                continue;
            }
            let file_path = parts[2..].join("\t");
            if file_path.is_empty() {
                continue;
            }
            let insertions = if parts[0] == "-" {
                0
            } else {
                parts[0].parse::<i64>().unwrap_or(0)
            };
            let deletions = if parts[1] == "-" {
                0
            } else {
                parts[1].parse::<i64>().unwrap_or(0)
            };
            let is_binary = parts[0] == "-" && parts[1] == "-";
            let change_type = if file_path.contains(" => ") { "R" } else { "M" };
            files.push((file_path, insertions, deletions, is_binary));
            let _ = change_type;
        }

        let name_status = self
            .raw(
                &context.repo_root,
                &[
                    "show".into(),
                    "--name-status".into(),
                    "--format=".into(),
                    commit_hash.to_string(),
                ],
            )
            .await
            .unwrap_or_default();
        let mut status_map: HashMap<String, String> = HashMap::new();
        for line in name_status.lines().filter(|l| !l.trim().is_empty()) {
            let parts: Vec<&str> = line.splitn(2, '\t').collect();
            if parts.len() != 2 {
                continue;
            }
            let status = parts[0];
            let first = status.chars().next().unwrap_or('M');
            if !matches!(first, 'A' | 'M' | 'D' | 'R' | 'C') {
                continue;
            }
            let path_part = parts[1];
            let path = if (first == 'R' || first == 'C') && path_part.contains('\t') {
                path_part.split('\t').next_back().unwrap_or(path_part)
            } else {
                path_part
            };
            status_map.insert(path.to_string(), first.to_string());
        }

        let out: Vec<Value> = files
            .into_iter()
            .map(|(path, insertions, deletions, is_binary)| {
                let base_path = numstat_destination_path(&path);
                let change_type = status_map
                    .get(&base_path)
                    .or_else(|| status_map.get(&path))
                    .cloned()
                    .unwrap_or_else(|| {
                        if path.contains(" => ") {
                            "R".to_string()
                        } else {
                            "M".to_string()
                        }
                    });
                json!({
                    "path": path,
                    "insertions": insertions,
                    "deletions": deletions,
                    "isBinary": is_binary,
                    "changeType": change_type,
                })
            })
            .collect();
        Ok(json!({ "files": out }))
    }

    /// 读取指定提交中某文件的改前/改后内容。directory/hash/file_path 任一为空直接报
    /// 参数错误；is_binary 时短路返回空内容。先构造相对 repo 根与相对 directory 的两个
    /// 候选路径（过滤 `..`/绝对路径并去重），用 `git show <hash>^:<path>` 与
    /// `<hash>:<path>` 成对探测；任一侧成功即采用该候选，全部失败时经
    /// `resolve_git_commit_file_path` 重解析路径再试；两次读取都失败则返回带 stderr
    /// 的错误，单侧失败时该侧内容返回空串。
    pub async fn get_commit_file_diff(
        &self,
        directory: &str,
        hash: &str,
        file_path: &str,
        is_binary: bool,
    ) -> ServiceResult<Value> {
        if directory.trim().is_empty() || hash.trim().is_empty() || file_path.trim().is_empty() {
            return Err("directory, hash, and path are required for getCommitFileDiff".to_string());
        }
        if is_binary {
            return Ok(json!({ "original": "", "modified": "", "isBinary": true }));
        }

        let context = self.repository_context(directory).await?;
        let repo_root = &context.repo_root;
        let repo_relative = to_git_path(&relative(
            repo_root,
            &absolutize(&repo_root.join(file_path)),
        ));
        let directory_relative = to_git_path(&relative(
            repo_root,
            &absolutize(&context.directory_path.join(file_path)),
        ));
        let mut candidates: Vec<String> = vec![repo_relative, directory_relative];
        candidates.retain(|candidate| {
            !candidate.is_empty() && !candidate.starts_with("..") && !candidate.starts_with('/')
        });
        candidates.dedup();

        let mut original_result: Option<GitCommandResult> = None;
        let mut modified_result: Option<GitCommandResult> = None;

        for candidate in &candidates {
            let original = self
                .run(repo_root, &["show", &format!("{}^:{}", hash, candidate)])
                .await;
            let modified = self
                .run(repo_root, &["show", &format!("{}:{}", hash, candidate)])
                .await;
            if original.success || modified.success {
                original_result = Some(original);
                modified_result = Some(modified);
                break;
            }
        }

        if original_result.is_none() || modified_result.is_none() {
            let resolved = self
                .resolve_git_commit_file_path(repo_root, hash, &candidates)
                .await?;
            original_result = Some(
                self.run(repo_root, &["show", &format!("{}^:{}", hash, resolved)])
                    .await,
            );
            modified_result = Some(
                self.run(repo_root, &["show", &format!("{}:{}", hash, resolved)])
                    .await,
            );
        }

        let original_result = original_result.unwrap();
        let modified_result = modified_result.unwrap();
        if !original_result.success && !modified_result.success {
            let stderr = if !original_result.stderr.is_empty() {
                original_result.stderr.clone()
            } else {
                modified_result.stderr.clone()
            };
            return Err(format!(
                "Failed to read file content at commit {}: {}",
                hash, stderr
            ));
        }

        Ok(json!({
            "original": if original_result.success { original_result.stdout } else { String::new() },
            "modified": if modified_result.success { modified_result.stdout } else { String::new() },
            "isBinary": false,
        }))
    }

    /// 用 `git ls-tree --name-only` 依次验证候选路径在提交本身或其父提交中是否存在，
    /// 返回第一个命中的候选路径；全部未命中时返回 "Invalid file path" 错误。
    /// 供 `get_commit_file_diff` 在直接 `show` 失败时做路径重解析。
    async fn resolve_git_commit_file_path(
        &self,
        repo_root: &Path,
        hash: &str,
        candidates: &[String],
    ) -> ServiceResult<String> {
        for candidate in candidates {
            let original = self
                .run(
                    repo_root,
                    &[
                        "ls-tree",
                        "--name-only",
                        &format!("{}^", hash),
                        "--",
                        candidate,
                    ],
                )
                .await;
            let modified = self
                .run(
                    repo_root,
                    &["ls-tree", "--name-only", hash, "--", candidate],
                )
                .await;
            if (original.success && !original.stdout_trim().is_empty())
                || (modified.success && !modified.stdout_trim().is_empty())
            {
                return Ok(candidate.clone());
            }
        }
        Err("Invalid file path".to_string())
    }

    /// 批量获取提交摘要。`shas` 为 JSON 字符串数组，仅接受 4–64 位十六进制哈希，
    /// 含非法值时直接报 "Invalid commit SHA"；空列表短路返回空结果。
    /// 通过 `git show -s --format=%H\t%h\t%s` 解析出 sha/short/subject；
    /// 首选 `run_or_throw` 通道，失败时退回 `raw` 通道以相同参数重试一次。
    pub async fn get_commit_summaries(
        &self,
        directory: &str,
        shas: Option<&Value>,
    ) -> ServiceResult<Value> {
        let commits: Vec<String> = shas
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if commits.is_empty() {
            return Ok(json!({ "commits": [] }));
        }
        if commits.iter().any(|sha| {
            !(sha.len() >= 4 && sha.len() <= 64 && sha.chars().all(|c| c.is_ascii_hexdigit()))
        }) {
            return Err("Invalid commit SHA".to_string());
        }
        let mut argv: Vec<String> = vec![
            "show".into(),
            "-s".into(),
            "--format=%H\u{9}%h\u{9}%s".into(),
        ];
        argv.extend(commits.iter().cloned());
        argv.push("--".into());
        let result = self
            .run_or_throw(&directory, &[], "Failed to get commit summaries")
            .await;
        let result = match result {
            Ok(result) => result,
            Err(_) => {
                let argv_ref: Vec<&str> = argv.iter().map(String::as_str).collect();
                let _ = argv_ref;
                let output = self
                    .raw(&directory, &{
                        let mut a: Vec<String> = vec![
                            "show".into(),
                            "-s".into(),
                            "--format=%H\u{9}%h\u{9}%s".into(),
                        ];
                        a.extend(commits.iter().cloned());
                        a.push("--".into());
                        a
                    })
                    .await
                    .map_err(|_| "Failed to get commit summaries".to_string())?;
                GitCommandResult::ok(output)
            }
        };
        let parsed: Vec<Value> = result
            .stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| {
                let mut parts = line.splitn(3, '\t');
                let sha = parts.next().unwrap_or("");
                let short = parts.next().unwrap_or("");
                let subject = parts.next().unwrap_or("");
                if sha.is_empty() || short.is_empty() {
                    return None;
                }
                Some(json!({ "sha": sha, "short": short, "subject": subject }))
            })
            .collect();
        Ok(json!({ "commits": parsed }))
    }

    // -----------------------------------------------------------------------
    // Worktree type probes + primary-root/toplevel resolvers
    // -----------------------------------------------------------------------

    /// 判断目录是否为 linked worktree：比较 `rev-parse --git-dir` 与
    /// `--git-common-dir` 的输出，二者不同即为 linked worktree；
    /// 目录非法或任一命令失败时一律按 false 处理。
    pub async fn is_linked_worktree(&self, directory: &str) -> bool {
        let Ok(directory_path) = require_directory(Some(directory)) else {
            return false;
        };
        let cwd = PathBuf::from(directory_path);
        let git_dir = self.run(&cwd, &["rev-parse", "--git-dir"]).await;
        if !git_dir.success {
            return false;
        }
        let git_common_dir = self.run(&cwd, &["rev-parse", "--git-common-dir"]).await;
        if !git_common_dir.success {
            return false;
        }
        git_dir.stdout.trim() != git_common_dir.stdout.trim()
    }

    /// 校验 directory 是否是位于 worktree_root 之内的 git 仓库。任一路径为空或
    /// directory 不是 git 仓库时返回全空的无效对象（valid=false）；有效时返回
    /// canonicalize 后的 resolvedCwd/resolvedWorktreeRoot，以及 insideWorktreeRoot
    /// （二者相等或 cwd 以 `<root>/` 为前缀）。
    pub async fn validate_worktree_directory(&self, directory: &str, worktree_root: &str) -> Value {
        let invalid = json!({
            "valid": false,
            "insideWorktreeRoot": false,
            "resolvedWorktreeRoot": null,
            "resolvedCwd": null,
        });
        let directory_path = normalize_directory_path(Some(directory)).filter(|v| !v.is_empty());
        let root_path = normalize_directory_path(Some(worktree_root)).filter(|v| !v.is_empty());
        let (Some(directory_path), Some(root_path)) = (directory_path, root_path) else {
            return invalid;
        };
        if !self.is_git_repository(&directory_path).await {
            return invalid;
        }
        let resolved_cwd = canonical_path(&directory_path).await;
        let resolved_root = canonical_path(&root_path).await;
        let cwd_text = resolved_cwd.to_string_lossy().to_string();
        let root_text = resolved_root.to_string_lossy().to_string();
        let inside = cwd_text == root_text || cwd_text.starts_with(&format!("{}/", root_text));
        json!({
            "valid": true,
            "insideWorktreeRoot": inside,
            "resolvedWorktreeRoot": root_text,
            "resolvedCwd": cwd_text,
        })
    }

    /// 汇总 worktree 当前状态 JSON：worktreeRoot（解析失败时状态标记为 invalid）、cwd、
    /// 分支与 headState（symbolic-ref 命中为 branch；为空时回退 rev-parse HEAD，
    /// 仍为空记 unborn，否则 detached 且分支显示为 7 位短哈希）、worktreeStatus，以及
    /// attentionReason——按 merge（存在 MERGE_HEAD 且在分支上）、rebase（rebase-merge/
    /// rebase-apply 目录存在）、cherry-pick/revert（有冲突文件且 CHERRY_PICK_HEAD/
    /// REVERT_HEAD 存在）的优先级判定，无异常时为 null。
    pub async fn canonicalize_worktree_state(&self, directory: &str) -> Value {
        let empty = || {
            json!({
                "worktreeRoot": null,
                "cwd": null,
                "branch": null,
                "headState": "detached",
                "worktreeStatus": "not-a-repo",
                "legacy": false,
                "degraded": false,
                "attentionReason": null,
            })
        };
        let Some(directory_path) =
            normalize_directory_path(Some(directory)).filter(|v| !v.is_empty())
        else {
            return empty();
        };
        if !self.is_git_repository(&directory_path).await {
            return empty();
        }

        let cwd = canonical_path(&directory_path).await;
        let cwd_path = PathBuf::from(&directory_path);
        let repo_root = self
            .resolve_repository_root(&directory_path)
            .await
            .unwrap_or_else(|_| cwd_path.clone());

        let mut worktree_root = Value::Null;
        let mut worktree_status = "ready";
        match self.resolve_worktree_project_context(&directory_path).await {
            Ok(context) => {
                worktree_root = Value::String(
                    canonical_path(context.worktree_root)
                        .await
                        .to_string_lossy()
                        .to_string(),
                );
            }
            Err(_) => worktree_status = "invalid",
        }

        let head_state;
        let branch;
        let symbolic = self
            .raw(
                &cwd_path,
                &["symbolic-ref".into(), "-q".into(), "HEAD".into()],
            )
            .await
            .unwrap_or_default();
        if !symbolic.trim().is_empty() {
            head_state = "branch";
            branch = Value::String(clean_branch_name(symbolic.trim()));
        } else {
            let head = self
                .raw(&cwd_path, &["rev-parse".into(), "HEAD".into()])
                .await
                .unwrap_or_default();
            if head.trim().is_empty() {
                head_state = "unborn";
                branch = Value::Null;
            } else {
                head_state = "detached";
                branch = Value::String(head.trim().chars().take(7).collect());
            }
        }

        let mut attention_reason = Value::Null;
        let status = self.parse_status(&cwd_path, &["-uall"]).await;
        let merge_head = self
            .raw(
                &cwd_path,
                &["rev-parse".into(), "--verify".into(), "MERGE_HEAD".into()],
            )
            .await
            .is_ok();
        if status.current.is_some() && merge_head {
            attention_reason = Value::String("merge".to_string());
        } else {
            let rebase_merge = self
                .resolve_git_internal_path(&repo_root, "rebase-merge")
                .await
                .ok();
            let rebase_apply = self
                .resolve_git_internal_path(&repo_root, "rebase-apply")
                .await
                .ok();
            let rebase_merge_exists = match rebase_merge {
                Some(path) => check_path_exists(path).await,
                None => false,
            };
            let rebase_apply_exists = match rebase_apply {
                Some(path) => check_path_exists(path).await,
                None => false,
            };
            if rebase_merge_exists || rebase_apply_exists {
                attention_reason = Value::String("rebase".to_string());
            } else if !status.conflicted.is_empty() {
                let cherry_pick = self
                    .resolve_git_internal_path(&repo_root, "CHERRY_PICK_HEAD")
                    .await
                    .ok();
                let revert_head = self
                    .resolve_git_internal_path(&repo_root, "REVERT_HEAD")
                    .await
                    .ok();
                let cherry_pick_exists = match cherry_pick {
                    Some(path) => check_path_exists(path).await,
                    None => false,
                };
                let revert_exists = match revert_head {
                    Some(path) => check_path_exists(path).await,
                    None => false,
                };
                if cherry_pick_exists {
                    attention_reason = Value::String("cherry-pick".to_string());
                } else if revert_exists {
                    attention_reason = Value::String("revert".to_string());
                }
            }
        }

        json!({
            "worktreeRoot": worktree_root,
            "cwd": cwd.to_string_lossy().to_string(),
            "branch": branch,
            "headState": head_state,
            "worktreeStatus": worktree_status,
            "legacy": false,
            "degraded": false,
            "attentionReason": attention_reason,
        })
    }

    /// 解析主 worktree 根目录：先取 `rev-parse --absolute-git-dir`（反斜杠归一为 `/`），
    /// 交给 [`derive_primary_worktree_root_from_git_dir`] 反推；失败再用
    /// `--git-common-dir`（相对路径基于 directory 绝对化）重试；两者都失败时
    /// 原样返回传入的 directory。
    pub async fn resolve_primary_worktree_root(&self, directory: &str) -> Value {
        let result = self
            .run(
                directory,
                &["rev-parse", "--absolute-git-dir", "--git-common-dir"],
            )
            .await;
        if !result.success {
            return json!({ "root": directory });
        }
        let lines: Vec<&str> = result
            .stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        let absolute_git_dir = lines.first().copied().unwrap_or("").replace('\\', "/");
        if let Some(root) = derive_primary_worktree_root_from_git_dir(&absolute_git_dir) {
            return json!({ "root": root });
        }
        let raw_common_dir = lines.get(1).copied().unwrap_or("").replace('\\', "/");
        if !raw_common_dir.is_empty() {
            let common_dir = if raw_common_dir.starts_with('/') {
                raw_common_dir.clone()
            } else {
                absolutize(&Path::new(directory).join(&raw_common_dir))
                    .to_string_lossy()
                    .replace('\\', "/")
            };
            if let Some(root) = derive_primary_worktree_root_from_git_dir(&common_dir) {
                return json!({ "root": root });
            }
        }
        json!({ "root": directory })
    }

    /// 运行 `git rev-parse --show-toplevel` 获取当前 worktree 的顶层目录
    /// （路径分隔符归一为 `/`）；命令失败或输出为空时回退为传入的 directory。
    pub async fn resolve_worktree_top_level(&self, directory: &str) -> Value {
        let result = self.run(directory, &["rev-parse", "--show-toplevel"]).await;
        if !result.success {
            return json!({ "root": directory });
        }
        let root = result.stdout.trim().replace('\\', "/");
        if root.is_empty() {
            json!({ "root": directory })
        } else {
            json!({ "root": root })
        }
    }

    /// 通过 `repository_context` 解析仓库根目录，并返回其字符串形式。
    pub async fn get_repository_root(&self, directory: &str) -> ServiceResult<String> {
        let context = self.repository_context(directory).await?;
        Ok(context.repo_root.to_string_lossy().to_string())
    }

    // -----------------------------------------------------------------------
    // Long-path support (issue #2746)
    // -----------------------------------------------------------------------

    /// 确保 worktree 启用 `core.longpaths`（issue #2746 的 Windows 长路径支持）：
    /// 已为 true 则直接返回；否则写入 `core.longpaths=true`，写入失败也被忽略
    /// （尽力而为，不阻塞后续填充流程）。
    pub async fn ensure_worktree_longpaths(&self, directory: &str) -> ServiceResult<()> {
        let current = self
            .run(directory, &["config", "--get", "core.longpaths"])
            .await;
        if current.stdout.trim().to_lowercase() == "true" {
            return Ok(());
        }
        let _ = self
            .run(directory, &["config", "core.longpaths", "true"])
            .await;
        Ok(())
    }

    /// 带锁恢复的 worktree 填充：执行 reset 检出命令，遇到 `index.lock` 冲突时按
    /// 250ms → 750ms 退避重试；重试前用 `rev-parse --git-path index.lock` 定位锁文件，
    /// 并以 `file_identity` 记录其指纹，仅当锁文件在两次探测间确实未变化（陈旧锁）时
    /// 才删除锁文件做最后一次尝试。非锁错误或最终仍失败时返回经
    /// [`format_worktree_populate_error`] 规整的错误消息。
    pub async fn populate_worktree_with_lock_recovery(&self, directory: &str) -> ServiceResult<()> {
        self.ensure_worktree_longpaths(directory).await?;

        let reset_args = ["-c", "core.longpaths=true", "reset", "--hard"];
        let result = self.run(directory, &reset_args).await;
        if result.success {
            return Ok(());
        }
        if !is_index_lock_error(&result) {
            return Err(format_worktree_populate_error(&result.message));
        }

        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let result = self.run(directory, &reset_args).await;
        if result.success {
            return Ok(());
        }
        if !is_index_lock_error(&result) {
            return Err(format_worktree_populate_error(&result.message));
        }

        let lock_path = self
            .run(directory, &["rev-parse", "--git-path", "index.lock"])
            .await;
        let lock_path: Option<PathBuf> = if lock_path.success && !lock_path.stdout_trim().is_empty()
        {
            let value = lock_path.stdout_trim().to_string();
            Some(if value.starts_with('/') {
                PathBuf::from(value)
            } else {
                absolutize(&Path::new(directory).join(value))
            })
        } else {
            None
        };
        let identity = match &lock_path {
            Some(path) => file_identity(path).await.ok(),
            None => None,
        };
        tokio::time::sleep(std::time::Duration::from_millis(750)).await;

        let result = self.run(directory, &reset_args).await;
        if result.success {
            return Ok(());
        }
        let lock_unchanged = match (&lock_path, &identity) {
            (Some(path), Some(id)) => file_identity(path).await.ok().as_ref() == Some(id),
            _ => false,
        };
        if !is_index_lock_error(&result) || !lock_unchanged {
            return Err(format_worktree_populate_error(&result.message));
        }

        if let Some(path) = &lock_path {
            let _ = tokio::fs::remove_file(path).await;
        }
        let final_result = self.run(directory, &reset_args).await;
        if !final_result.success {
            let message = if final_result.message.trim().is_empty() {
                "Failed to populate worktree".to_string()
            } else {
                final_result.message.trim().to_string()
            };
            return Err(format_worktree_populate_error(&message));
        }
        Ok(())
    }

    /// 以额外的环境变量执行任意 git 子命令（字符串参数形式），直接转发给注入的
    /// runner 闭包并原样返回 `GitCommandResult`；供需要向子进程注入 env
    /// （如 worktree 场景）的调用方使用。
    pub(crate) async fn run_strings_with_env(
        &self,
        cwd: impl AsRef<Path>,
        args: &[String],
        env: HashMap<String, String>,
    ) -> GitCommandResult {
        (self.runner())(cwd.as_ref().to_path_buf(), args.to_vec(), env).await
    }
}

// ---------------------------------------------------------------------------
// Log options + pure parsing helpers
// ---------------------------------------------------------------------------

/// [`GitService`] 的单次命令封装（commit/push）与引用探测辅助；自 `service.rs`
/// 拆分出的 impl 块，仅为本文件内部方法服务。
impl GitService {
    /// `rev-parse --verify <ref>` resolves to a commit-ish (quiet).
    /// 运行 `git rev-parse --verify <ref>` 静默探测引用是否解析到某个 commit-ish；
    /// 命令失败或输出为空都按“不可解析”（false）处理，不产生错误。
    async fn ref_resolves(&self, repo_root: &Path, reference: &str) -> bool {
        self.raw(
            repo_root,
            &["rev-parse".into(), "--verify".into(), reference.to_string()],
        )
        .await
        .map(|out| !out.trim().is_empty())
        .unwrap_or(false)
    }

    /// One `git -c core.abbrev=40 commit -m <msg> [files]` invocation.
    /// 执行一次 `git -c core.abbrev=40 commit -m <msg> [files...]`；成功时解析 stdout
    /// 得到（提交哈希、分支名、changes/insertions/deletions 汇总 JSON）三元组，
    /// 失败时返回经 `failure_text` 合成的错误消息字符串。
    async fn commit_once(
        &self,
        repo_root: &Path,
        message: &str,
        files: Option<Vec<String>>,
    ) -> ServiceResult<(String, String, Value)> {
        let mut argv: Vec<String> = vec![
            "-c".into(),
            "core.abbrev=40".into(),
            "commit".into(),
            "-m".into(),
            message.to_string(),
        ];
        if let Some(files) = files {
            argv.extend(files);
        }
        let result = self.run_strings(repo_root, &argv).await;
        if result.success {
            let parsed = parse_commit_summary(&result.stdout);
            Ok((parsed.commit, parsed.branch, parsed.value))
        } else {
            Err(
                failure_text(&result.stdout, &result.stderr, &result.message)
                    .trim()
                    .to_string(),
            )
        }
    }

    /// One `git push --verbose --porcelain …` invocation, parsed.
    /// 执行一次 `git push --verbose --porcelain ...` 并用 `parse_push_result` 解析输出；
    /// 成功返回推送结果 JSON 对象，失败返回合成后的错误消息字符串。
    async fn push_once(
        &self,
        repo_root: &Path,
        argv: Vec<String>,
        directory: &str,
    ) -> ServiceResult<Value> {
        let result = self.run_strings(repo_root, &argv).await;
        if result.success {
            Ok(parse_push_result(&result.stdout, &result.stderr, directory))
        } else {
            Err(
                failure_text(&result.stdout, &result.stderr, &result.message)
                    .trim()
                    .to_string(),
            )
        }
    }
}

/// log 查询的公共选项集合，字段与 JS 版 log API 的参数一一对应。
#[derive(Debug, Default, Clone)]
pub struct LogOptions {
    /// 限制返回的提交条数（映射为 `--max-count`）；`None` 表示不限制。
    pub max_count: Option<i64>,
    /// log 起点引用（分支名或 commit）；使用前会经
    /// `resolve_base_ref_for_log_with` 归一化为可解析的 ref。
    pub from: Option<String>,
    /// log 终点引用；`None` 表示默认到 HEAD。
    pub to: Option<String>,
    /// 仅跟踪指定文件的提交历史；`None` 表示全仓库范围。
    pub file: Option<String>,
    /// 是否附加 `--all` 以覆盖所有引用的提交历史。
    pub all: bool,
}

/// checkout 目标描述：目标分支名加上可选的远端跟踪引用。
struct CheckoutTarget {
    /// 目标分支名（本地分支名，不含 remote 前缀）。
    branch: String,
    /// 对应的远端跟踪引用（如 `origin/main`）；仅在远端分支首次检出等场景存在。
    remote_ref: Option<String>,
}

/// 从 stash 操作的 options JSON 中读取 `ref` 字段并去除空白；字段缺失或为空时
/// 回退到 `stash@{0}`（最新一条 stash）。
fn stash_ref(options: &Value) -> String {
    options
        .get("ref")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "stash@{0}".to_string())
}

/// 将 JS 侧透传的 raw 配置展开为 git 命令行参数数组：数组形式逐项透传（跳过空白项）；
/// 对象形式按键值生成参数——布尔 `true`/`null` 只生成开关项、`false` 整体跳过，
/// 字符串值原样追加，其它类型序列化为 JSON 文本后追加。
fn build_raw_git_options(raw: Option<&Value>) -> Vec<String> {
    let mut out = Vec::new();
    match raw {
        Some(Value::Array(items)) => {
            for item in items {
                let text = item.as_str().unwrap_or("").trim();
                if !text.is_empty() {
                    out.push(text.to_string());
                }
            }
        }
        Some(Value::Object(map)) => {
            for (key, value) in map {
                let option = key.trim();
                if option.is_empty() || value == &Value::Bool(false) {
                    continue;
                }
                if value == &Value::Bool(true) || value.is_null() {
                    out.push(option.to_string());
                } else {
                    out.push(option.to_string());
                    out.push(match value {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    });
                }
            }
        }
        _ => {}
    }
    out
}

/// 依据错误文本特征判断 push 失败是否因当前分支缺少 upstream 配置
/// （"has no upstream"/"no upstream"/"set-upstream"，或同时出现 upstream、push 与 -u），
/// 用于决定是否应自动附带 `--set-upstream` 重试。
fn looks_like_missing_upstream(text: &str) -> bool {
    let message = text.to_lowercase();
    message.contains("has no upstream")
        || message.contains("no upstream")
        || message.contains("set-upstream")
        || message.contains("set upstream")
        || (message.contains("upstream") && message.contains("push") && message.contains("-u"))
}

/// 提取 push 的错误文本：非空则返回 trim 后的原文，否则回退为通用消息
/// "Failed to push to remote"。
fn first_push_error(text: &str) -> String {
    for chunk in [text.trim()].iter() {
        if !chunk.is_empty() {
            return (*chunk).to_string();
        }
    }
    "Failed to push to remote".to_string()
}

/// 生成文件的稳定性指纹 `dev:ino:len:mtime`（设备号、inode、长度、修改时间），
/// 用于判断 `index.lock` 在两次探测之间是否被真正写入过；IO 失败时返回 Err。
async fn file_identity(path: &Path) -> std::io::Result<String> {
    let meta = tokio::fs::metadata(path).await?;
use crate::os_compat::MetadataExt;
    Ok(format!(
        "{}:{}:{}:{}",
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mtime()
    ))
}

/// 规整 worktree 填充失败的错误消息：空消息回退为通用文案；当文本（大小写不敏感、
/// 统一 "file name too long" 写法后）命中路径超长特征时，追加 Windows 长路径指引
/// （core.longpaths 已启用 / 建议 LongPathsEnabled 或缩短绝对路径）。
pub fn format_worktree_populate_error(message: &str) -> String {
    let text = if message.trim().is_empty() {
        "Failed to populate worktree".to_string()
    } else {
        message.trim().to_string()
    };
    let lowered = text
        .to_lowercase()
        .replace("file name too long", "filename too long");
    if !lowered.contains("filename too long") {
        return text;
    }
    [
        text.as_str(),
        "The worktree checkout path exceeds this system's path-length limit.",
        "OMPChamber enables Git `core.longpaths` for worktree population; if this still fails on Windows, enable OS long paths (LongPathsEnabled) or open the repository from a shorter absolute path.",
    ]
    .join("\n")
}

/// 从 `.git` 目录路径反推主 worktree 根目录：先剥离 `/.git` 后缀；若是 linked
/// worktree（路径含 `/.git/worktrees/`）则截取该标记之前的部分。无法识别时返回 `None`。
fn derive_primary_worktree_root_from_git_dir(git_dir: &str) -> Option<String> {
    let normalized = git_dir.trim();
    if normalized.is_empty() {
        return None;
    }
    if let Some(root) = normalized.strip_suffix("/.git") {
        return if root.is_empty() {
            None
        } else {
            Some(root.to_string())
        };
    }
    if let Some(marker_index) = normalized.find("/.git/worktrees/")
        && marker_index > 0
    {
        return Some(normalized[..marker_index].to_string());
    }
    None
}

/// 计算 target 相对 root 的路径字符串；target 不在 root 之下时原样返回其完整路径字符串。
fn relative(root: &Path, target: &Path) -> String {
    target
        .strip_prefix(root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| target.to_string_lossy().to_string())
}

/// 生成 8 位十六进制随机后缀，用于临时分支名/文件名去重。
fn hex_random_suffix() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    (0..8)
        .map(|_| format!("{:x}", rng.random_range(0..16)))
        .collect()
}

/// `extractPatchTargetPath` — first `---`/`+++` target that is a real path.
/// 扫描补丁文本中首个 `---`/`+++` 头部行，取其路径 token 并归一化
/// （跳过 `/dev/null`、剥离 `a/`/`b/` 前缀）；用于从补丁内容推断目标文件路径，
/// 找不到有效路径时返回 `None`。
pub(crate) fn extract_patch_target_path(patch: &str) -> Option<String> {
    for line in patch.lines() {
        let trimmed = line.trim_start();
        let value = if let Some(rest) = trimmed.strip_prefix("---") {
            rest.trim_start()
        } else if let Some(rest) = trimmed.strip_prefix("+++") {
            rest.trim_start()
        } else {
            continue;
        };
        if let Some(path) = normalize_patch_target_path(parse_patch_path_token(value)) {
            return Some(path);
        }
    }
    None
}

/// 解析 diff 头部行中的路径 token：空串与 `/dev/null` 视为无路径；带引号形式按
/// JSON 转义规则解码（解码失败时退化为去引号文本）；裸路径截取首个制表符之前的部分。
fn parse_patch_path_token(value: &str) -> Option<String> {
    if value.is_empty() || value == "/dev/null" {
        return None;
    }
    if let Some(quoted) = value.strip_prefix('"') {
        let mut token = String::from("\"");
        let mut escaped = false;
        for ch in quoted.chars() {
            token.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                break;
            }
        }
        if let Ok(parsed) = serde_json::from_str::<String>(&token) {
            return Some(parsed);
        }
        return Some(token.trim_matches('"').to_string());
    }
    let first_tab = value.find('\t').unwrap_or(value.len());
    let token = &value[..first_tab];
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// 过滤并归一化补丁目标路径：`None`、空串与 `/dev/null` 一律返回 `None`，
/// 其余剥离 `a/`/`b/` diff 前缀后返回。
fn normalize_patch_target_path(value: Option<String>) -> Option<String> {
    let value = value?;
    if value.is_empty() || value == "/dev/null" {
        return None;
    }
    let stripped = value
        .strip_prefix("a/")
        .or_else(|| value.strip_prefix("b/"))
        .unwrap_or(&value);
    Some(stripped.to_string())
}

// -- commit / pull / push / branch parsers ----------------------------------

/// `git commit` 输出的解析结果。
#[derive(Debug, Clone)]
struct CommitSummaryParsed {
    /// 新提交的哈希。
    commit: String,
    /// 提交所在分支名。
    branch: String,
    /// changes/insertions/deletions 汇总统计的 JSON 对象。
    value: Value,
}

/// 解析 `git commit` 的 stdout：从 `[branch (root-commit) hash] subject` 汇总行提取
/// 分支名与提交哈希（跳过可选的 "(root-commit)" token），并汇总
/// "N files changed / insertions / deletions" 的 diffstat 统计。
fn parse_commit_summary(stdout: &str) -> CommitSummaryParsed {
    let mut commit = String::new();
    let mut branch = String::new();
    let mut changes = 0i64;
    let mut insertions = 0i64;
    let mut deletions = 0i64;

    for line in stdout.lines() {
        // `[branch (maybe root-commit) hash] subject`
        if line.starts_with('[')
            && commit.is_empty()
            && let Some(end) = line.find(']')
        {
            let inner = &line[1..end];
            let mut parts = inner.split_whitespace();
            if let Some(first) = parts.next() {
                branch = first.to_string();
                // Skip an optional "(root-commit)" token.
                let mut token = parts.next();
                if token.is_some_and(|t| t.starts_with('(')) {
                    token = parts.next();
                }
                if let Some(hash) = token {
                    commit = hash.to_string();
                }
            }
        }
        if let Some(count) = count_before(line, " file", " files changed") {
            changes = count;
        }
        if let Some((count, '+')) = insertion_deletion_count(line, "insertion") {
            insertions = count;
        }
        if let Some((count, '-')) = insertion_deletion_count(line, "deletion") {
            deletions = count;
        }
    }

    CommitSummaryParsed {
        commit,
        branch,
        value: json!({
            "changes": changes,
            "insertions": insertions,
            "deletions": deletions,
        }),
    }
}

/// 在行内定位复数/单数标记（如 " files changed" / " file"），返回标记之前的行尾数字；
/// 找不到标记或行尾无完整数字时返回 `None`。
fn count_before(line: &str, singular_marker: &str, plural_marker: &str) -> Option<i64> {
    let marker_pos = line
        .find(plural_marker)
        .or_else(|| line.find(singular_marker))?;
    trailing_digits(&line[..marker_pos])
}

/// 提取字符串末尾连续的 ASCII 数字并解析为 i64；末尾没有数字时返回 `None`。
fn trailing_digits(head: &str) -> Option<i64> {
    let trimmed = head.trim_end();
    let start = trimmed
        .rfind(|c: char| !c.is_ascii_digit())
        .map(|index| index + 1)
        .unwrap_or(0);
    trimmed[start..].parse::<i64>().ok()
}

/// 在行内查找 insertion/deletion 计数词，并依据其后的 `(+)`/`(-)` 方向标记返回
/// （数量, 方向字符）；缺少方向标记时返回 `None`。
fn insertion_deletion_count(line: &str, word: &str) -> Option<(i64, char)> {
    let pos = line.find(word)?;
    let tail = &line[pos..];
    let direction = if tail.contains("(+)") {
        '+'
    } else if tail.contains("(-)") {
        '-'
    } else {
        return None;
    };
    trailing_digits(&line[..pos]).map(|count| (count, direction))
}

/// 解析 `git pull` 的 stdout/stderr 为与 JS 版对齐的 JSON 结果：逐文件解析
/// `file | 5 +++--` 统计行得到 insertions/deletions，识别 `create mode`/`delete mode`
/// 行归类 created/deleted 列表，并汇总 changes/insertions/deletions 总量；
/// 重复出现的文件名只记录一次。
fn parse_pull_summary(stdout: &str, stderr: &str) -> Value {
    let mut files: Vec<Value> = Vec::new();
    let mut insertions = Map::new();
    let mut deletions = Map::new();
    let mut created: Vec<Value> = Vec::new();
    let mut deleted: Vec<Value> = Vec::new();
    let mut changes = 0i64;
    let mut summary_insertions = 0i64;
    let mut summary_deletions = 0i64;

    for line in stdout.lines().chain(stderr.lines()) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // ` file | 5 +++--`
        if let Some((head, tail)) = trimmed.split_once('|') {
            let file = head.trim();
            let bar = tail.trim();
            let plus = bar.matches('+').count();
            let minus = bar.matches('-').count();
            if !file.is_empty()
                && (plus > 0 || minus > 0 || bar.chars().next().is_some_and(|c| c.is_ascii_digit()))
            {
                if !files.iter().any(|v| v.as_str() == Some(file)) {
                    files.push(Value::String(file.to_string()));
                }
                if plus > 0 {
                    insertions.insert(file.to_string(), json!(plus));
                }
                if minus > 0 {
                    deletions.insert(file.to_string(), json!(minus));
                }
                continue;
            }
        }
        if let Some(rest) = trimmed.strip_prefix("create mode ") {
            if let Some((_mode, file)) = rest.trim().split_once(' ') {
                files.push(Value::String(file.to_string()));
                created.push(Value::String(file.to_string()));
            }
        } else if let Some(rest) = trimmed.strip_prefix("delete mode ")
            && let Some((_mode, file)) = rest.trim().split_once(' ')
        {
            files.push(Value::String(file.to_string()));
            deleted.push(Value::String(file.to_string()));
        }
        if let Some(count) = count_before(trimmed, " file", " files changed") {
            changes = count;
            if let Some((count, '+')) = insertion_deletion_count(trimmed, "insertion") {
                summary_insertions = count;
            }
            if let Some((count, '-')) = insertion_deletion_count(trimmed, "deletion") {
                summary_deletions = count;
            }
        }
    }

    json!({
        "success": true,
        "summary": {
            "changes": changes,
            "insertions": summary_insertions,
            "deletions": summary_deletions,
        },
        "files": files,
        "insertions": insertions,
        "deletions": deletions,
        "created": created,
        "deleted": deleted,
    })
}

/// 解析 `git push` 的 stdout/stderr，组装与 JS 版 simple-git 对齐的 JSON 结果：
/// `pushed` 数组（每个引用的 new/deleted/tag/alreadyUpdated 标记及 local/remote 引用名）、
/// 目标仓库地址（`Pushing to ...` 行）以及被更新的本地跟踪 ref。
/// `_directory` 仅用于保持与 JS 版签名一致，解析本身不使用。
/// 始终返回 `success: true`；推送失败由调用方依据 git 退出码另行判断。
fn parse_push_result(stdout: &str, stderr: &str, _directory: &str) -> Value {
    let mut pushed: Vec<Value> = Vec::new();
    let mut repo = Value::Null;
    let mut reference = Value::Null;

    for line in stdout.lines().chain(stderr.lines()) {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Pushing to ") {
            repo = Value::String(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("updating local tracking ref '") {
            if let Some(local) = rest.strip_suffix('\'') {
                reference = json!({ "local": local });
            }
            continue;
        }
        // `=[new branch]  main:main` / `*[new branch] local:remote [status]`
        let mut chars = trimmed.chars();
        if matches!(chars.next(), Some('=' | '*' | '-')) && trimmed.starts_with(['=', '*', '-']) {
            let body = &trimmed[1..];
            let body = body.trim_start();
            if let Some((local_remote, status)) = split_push_line(body) {
                let deleted = status.contains("deleted");
                let tag = status.contains("tag") || local_remote.0.starts_with("refs/tags");
                let already_updated = !status.contains("new");
                pushed.push(json!({
                    "deleted": deleted,
                    "tag": tag,
                    "branch": !tag,
                    "new": !already_updated,
                    "alreadyUpdated": already_updated,
                    "local": local_remote.0,
                    "remote": local_remote.1,
                }));
            }
        }
    }

    json!({
        "success": true,
        "pushed": pushed,
        "repo": repo,
        "ref": reference,
    })
}

/// 拆解 push 汇总行 `local:remote [status]`，返回 `((local, remote), status)`。
/// 行内缺少 `[...]` 状态括号或 `local:remote` 冒号时返回 `None`。
fn split_push_line(body: &str) -> Option<((String, String), String)> {
    // local:remote [status]
    let bracket = body.find("[")?;
    let close = body.rfind("]")?;
    let head = body[..bracket].trim();
    let status = body[bracket + 1..close].to_string();
    let (local, remote) = head.split_once(':')?;
    Some(((local.to_string(), remote.to_string()), status))
}

/// `git branch -v` 输出的解析结果，形状与 JS 版 simple-git 的 branch 汇总对齐。
#[derive(Debug, Default)]
struct BranchSummaryParsed {
    /// 全部分支名列表，按输出顺序排列（含 detached HEAD 行解析出的名字）。
    all: Vec<String>,
    /// 当前检出的分支名；detached HEAD 时为解析出的提交名，可能为空串。
    current: String,
    /// 分支名到分支条目 JSON 对象（current/linkedWorkTree/name/commit/label）的映射。
    branches: Map<String, Value>,
}

/// 逐行解析 `git branch -v` 输出。先剥离 `* `/`+ ` 前缀标记（当前分支 / linked worktree），
/// 再分别处理 `(HEAD detached at/from ...)` 行与 `name hash label` 常规行；
/// `remotes/origin/HEAD -> origin/main` 这类 symref 行被跳过，与 simple-git 行为一致。
fn parse_branch_summary(stdout: &str) -> BranchSummaryParsed {
    let mut parsed = BranchSummaryParsed::default();
    for line in stdout.lines() {
        // simple-git trims each row before parsing; `branch -v` pads
        // non-current branches with leading spaces.
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut rest = trimmed;
        let mut marker = "";
        for prefix in ["* ", "+ "] {
            if let Some(after) = rest.strip_prefix(prefix) {
                marker = &prefix[..1];
                rest = after;
                break;
            }
        }
        if rest.starts_with("(HEAD detached at ") || rest.starts_with("(HEAD detached from ") {
            // `* (HEAD detached at abc1234) label`
            let end = rest.find(')').unwrap_or(rest.len());
            let inner = &rest[1..end];
            let name = inner
                .trim_start_matches("HEAD detached at ")
                .trim_start_matches("HEAD detached from ")
                .split_whitespace()
                .next_back()
                .unwrap_or("")
                .to_string();
            let label = rest[end + 1..].trim().to_string();
            let commit = rest[end + 1..]
                .split_whitespace()
                .next()
                .filter(|token| token.chars().all(|c| c.is_ascii_alphanumeric()))
                .unwrap_or("")
                .to_string();
            if marker == "*" {
                parsed.current = name.clone();
            }
            let entry_name = name.clone();
            parsed.all.push(name.clone());
            parsed.branches.insert(
                name,
                json!({
                    "current": marker == "*",
                    "linkedWorkTree": marker == "+",
                    "name": entry_name,
                    "commit": commit,
                    "label": label,
                }),
            );
            continue;
        }
        // `name hash label` — simple-git splits on /\s+/ (padded columns).
        let mut parts = rest.split_whitespace();
        let name = parts.next().unwrap_or("").to_string();
        let commit = parts.next().unwrap_or("").to_string();
        let label = parts.collect::<Vec<_>>().join(" ");
        if name.is_empty() {
            continue;
        }
        // `remotes/origin/HEAD -> origin/main` symref row: simple-git skips it.
        if commit == "->" {
            continue;
        }
        if marker == "*" {
            parsed.current = name.clone();
        }
        let entry_name = name.clone();
        parsed.all.push(name.clone());
        parsed.branches.insert(
            name,
            json!({
                "current": marker == "*",
                "linkedWorkTree": marker == "+",
                "name": entry_name,
                "commit": commit,
                "label": label,
            }),
        );
    }
    parsed
}

/// 从 `git symbolic-ref` 风格输出中提取当前分支名：匹配 `ref: refs/heads/<branch>` 行
/// 并去除行尾残留的 `HEAD` 字样；没有有效分支名时返回 `None`。
fn extract_symref_head(output: &str) -> Option<String> {
    // `ref: refs/heads/main\tHEAD`
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("ref: refs/heads/") {
            let branch = rest.trim().trim_end_matches("HEAD").trim();
            if !branch.is_empty() {
                return Some(branch.to_string());
            }
        }
    }
    None
}

// -- log parsers --------------------------------------------------------------

/// 边界 log（`--boundary`）中的单条提交记录；各字段由 `\u{f2}` 分隔符拆出，
/// 供 log 解析的边界回退路径使用。
#[derive(Debug, Clone)]
struct BoundaryLogEntry {
    /// 提交哈希（短哈希或全长哈希，取决于调用方的 git 格式参数）。
    hash: String,
    /// 与 `hash` 相同内容的副本，保留以对齐 JS 版的字段命名。
    hash_string: String,
    /// 提交日期字符串（按调用方 `--date=` 格式原样保留）。
    date: String,
    /// 提交消息首行（subject）。
    message: String,
    /// 装饰引用（如 `refs/heads/main`、tag 名），无则为 `None`。
    refs: Option<String>,
    /// 提交正文（body），无则为 `None`。
    body: Option<String>,
    /// 作者姓名。
    author_name: String,
    /// 作者邮箱。
    author_email: String,
}

/// 为 [`BoundaryLogEntry`] 提供的对齐 JS 版访问器的辅助方法。
impl BoundaryLogEntry {
    /// 返回提交哈希的克隆；保留为独立方法以对齐 JS 版的 `hash` 访问器。
    fn hash_str(&self) -> String {
        self.hash.clone()
    }
}

/// 解析以 `\u{f2}\u{f2}\u{f2}\u{f2}\u{f2}\u{f2} ` 分隔的边界 log 输出。每条记录先截断到
/// ` \u{f2}\u{f2}` 提交边界，再按 `" \u{f2} "` 拆出 hash/date/message/refs/body/author_name/
/// author_email 七个字段；hash 为空的记录直接丢弃。
fn parse_boundary_log(stdout: &str) -> Vec<BoundaryLogEntry> {
    let trimmed = stdout.trim();
    let start_boundary = "\u{f2}\u{f2}\u{f2}\u{f2}\u{f2}\u{f2} ";
    let commit_boundary = " \u{f2}\u{f2}";
    let splitter = " \u{f2} ";
    let mut entries = Vec::new();
    for record in trimmed.split(start_boundary) {
        let record = record.trim();
        if record.is_empty() {
            continue;
        }
        let head = match record.split_once(commit_boundary) {
            Some((head, _)) => head,
            None => record,
        };
        let fields: Vec<&str> = head.split(splitter).collect();
        let field = |index: usize| {
            fields
                .get(index)
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        };
        let hash = field(0);
        if hash.is_empty() {
            continue;
        }
        entries.push(BoundaryLogEntry {
            hash_string: hash.clone(),
            hash,
            date: field(1),
            message: field(2),
            refs: Some(field(3)).filter(|v| !v.is_empty()),
            body: Some(field(4)).filter(|v| !v.is_empty()),
            author_name: field(5),
            author_email: field(6),
        });
    }
    entries
}

/// 单个提交的 diffstat 聚合值。
#[derive(Debug, Clone, Default)]
struct LogStat {
    /// 变更文件数。
    files_changed: i64,
    /// 新增行数。
    insertions: i64,
    /// 删除行数。
    deletions: i64,
    /// 父提交哈希列表，用于识别合并提交。
    parents: Vec<String>,
}

/// 解析自定义分隔的 `log --stat` 输出：记录以 `\u{1e}` 分块，首行用 `\u{1f}` 拆出 hash 与
/// 父提交列表，其余行提取 "N files changed / insertions / deletions" 汇总，
/// 返回 hash 到 [`LogStat`] 的映射。
fn parse_log_stats(stdout: &str) -> HashMap<String, LogStat> {
    let mut stats: HashMap<String, LogStat> = HashMap::new();
    for record in stdout.split('\u{1e}') {
        let record = record.trim();
        if record.is_empty() {
            continue;
        }
        let mut lines = record.lines();
        let header = lines.next().unwrap_or("");
        let mut fields = header.split('\u{1f}');
        let hash = fields.next().unwrap_or("").trim().to_string();
        let parents = fields
            .next()
            .unwrap_or("")
            .trim()
            .split(' ')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        if hash.is_empty() {
            continue;
        }
        let mut stat = LogStat {
            parents,
            ..Default::default()
        };
        for line in lines {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(count) = count_before(trimmed, " file", " files changed") {
                stat.files_changed = count;
            }
            if let Some((count, '+')) = insertion_deletion_count(trimmed, "insertion") {
                stat.insertions = count;
            }
            if let Some((count, '-')) = insertion_deletion_count(trimmed, "deletion") {
                stat.deletions = count;
            }
        }
        stats.insert(hash, stat);
    }
    stats
}

/// 解析自定义分隔的原始 log 输出为 JSON 条目数组。首行以 `\u{1f}` 拆出
/// hash/parents/author_name/author_email/date/message/refs 字段，其余行提取 diffstat；
/// `with_refs` 为 true 时额外输出 refs/body 与作者字段，以对齐 JS 版的两种 log 形状。
fn parse_raw_log_entries(stdout: &str, with_refs: bool) -> Vec<Value> {
    let mut entries = Vec::new();
    for record in stdout.split('\u{1e}') {
        let record = record.trim();
        if record.is_empty() {
            continue;
        }
        let mut lines = record.lines().filter(|l| !l.trim().is_empty());
        let header = lines.next().unwrap_or("");
        let fields: Vec<&str> = header.split('\u{1f}').collect();
        let field = |index: usize| {
            fields
                .get(index)
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        };
        let hash = field(0);
        if hash.is_empty() {
            continue;
        }
        let parents: Vec<String> = field(1)
            .split(' ')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
        let mut stat = LogStat {
            parents,
            ..Default::default()
        };
        for line in lines {
            let trimmed = line.trim();
            if let Some(count) = count_before(trimmed, " file", " files changed") {
                stat.files_changed = count;
            }
            if let Some((count, '+')) = insertion_deletion_count(trimmed, "insertion") {
                stat.insertions = count;
            }
            if let Some((count, '-')) = insertion_deletion_count(trimmed, "deletion") {
                stat.deletions = count;
            }
        }
        let mut entry = Map::new();
        entry.insert("hash".into(), Value::String(hash));
        entry.insert("date".into(), Value::String(field(4)));
        entry.insert("message".into(), Value::String(field(5)));
        if with_refs {
            entry.insert("refs".into(), Value::String(field(6)));
            entry.insert("body".into(), Value::String(String::new()));
            entry.insert("author_name".into(), Value::String(field(2)));
            entry.insert("author_email".into(), Value::String(field(3)));
        }
        entry.insert("filesChanged".into(), json!(stat.files_changed));
        entry.insert("insertions".into(), json!(stat.insertions));
        entry.insert("deletions".into(), json!(stat.deletions));
        entry.insert("parents".into(), json!(stat.parents));
        entries.push(Value::Object(entry));
    }
    entries
}

/// [`GitService`] 的 log 引用解析辅助方法；自 `service.rs` 拆出后的延续 impl 块。
impl GitService {
    /// `resolveBaseRefForLog` bound to a repository.
    /// 将用户给定的 log 起点归一化为可解析引用：先按原样探测是否为有效 ref，
    /// 失败则退回 `refs/remotes/origin/<name>`（返回 `origin/<name>` 形式）；
    /// 两者都无法解析时仍原样返回，交由后续 git 命令自行报错。
    /// `from` 为 `None` 或全空白时返回 `None`，表示未指定起点。
    pub(crate) async fn resolve_base_ref_for_log_with(
        &self,
        repo_root: &Path,
        from: Option<&str>,
    ) -> Option<String> {
        let normalized = from.map(str::trim).filter(|v| !v.is_empty())?;
        if self.ref_resolves(repo_root, normalized).await {
            return Some(normalized.to_string());
        }
        let origin_ref = format!("refs/remotes/origin/{}", normalized);
        if self.ref_resolves(repo_root, &origin_ref).await {
            return Some(format!("origin/{}", normalized));
        }
        Some(normalized.to_string())
    }
}

/// 把 `git log --numstat` 输出中的重命名路径归一化为重命名后的目标路径：
/// 支持 `old => new` 与 `prefix{old => new}suffix` 两种形式（后者会压缩拼接产生的 `//`）；
/// 不含 ` => ` 的路径原样返回。
fn numstat_destination_path(file_path: &str) -> String {
    if !file_path.contains(" => ") {
        return file_path.to_string();
    }
    // `prefix{old => new}suffix`
    if let (Some(brace_open), Some(brace_close)) = (file_path.find('{'), file_path.rfind('}')) {
        let inner = &file_path[brace_open + 1..brace_close];
        if let Some((_, destination)) = inner.split_once(" => ") {
            let prefix = &file_path[..brace_open];
            let suffix = &file_path[brace_close + 1..];
            let mut joined = format!("{}{}{}", prefix, destination, suffix);
            while joined.contains("//") {
                joined = joined.replace("//", "/");
            }
            return joined;
        }
    }
    file_path
        .rsplit(" => ")
        .next()
        .map(str::trim)
        .unwrap_or(file_path)
        .to_string()
}

// Silence unused warnings for values kept for parity with the JS shapes.
