//! Port of `server/lib/skills-catalog/install.js`
//! (`installSkillsFromRepository`): clone shallowly, sparse-checkout only
//! the requested skill directories, resolve conflicts (prompt / skipAll /
//! overwriteAll with per-skill decisions), and copy each skill without
//! symlinks and with traversal guards. Targets resolve to the user skill
//! dir or the project `.opencode`/`.agents` trees.
//!
//! 中文说明：本模块对应 `server/lib/skills-catalog/install.js` 的
//! `installSkillsFromRepository`：对目标仓库做浅克隆（`--depth 1`），用
//! sparse-checkout 只检出选中的技能目录，再按冲突策略（逐技能决策 /
//! `skipAll` / `overwriteAll`）处理已存在的同名技能并逐个拷贝。拷贝拒绝
//! 符号链接并做目录穿越防护；安装目标按 scope 解析到用户技能目录或项目
//! 内的 `.opencode` / `.agents` 目录树。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::skills_catalog::error::{CatalogError, ConflictEntry};
use crate::skills_catalog::git::{
    GitIdentity, GitResult, GitRunOptions, GitRunner, assert_git_available, resolve_runner,
};
use crate::skills_catalog::scan::{clone_error, mkd_temp, safe_rm, validate_skill_name};
use crate::skills_catalog::source::{SourceParseResult, parse_skill_repo_source};

/// A selection entry: JS `{ skillDir }`.
/// 一条待安装的技能选择：技能在源仓库内的目录（JS 侧 `{ skillDir }`），
/// 序列化为 camelCase 保持同形。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallSelection {
    /// 技能在仓库内的目录（POSIX 风格路径，如 `skills/alpha`）。
    pub skill_dir: String,
}

/// 一条安装成功的技能记录（JS `installed` 数组元素），携带安装位置信息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledSkill {
    /// 技能名（取自所选目录的 basename）。
    pub skill_name: String,
    /// 安装 scope：`user`（用户级）或 `project`（项目级）。
    pub scope: String,
    /// 目标目录树来源标签：`opencode` 或 `agents`。
    pub source: String,
}

/// 一条被跳过的技能记录（JS `skipped` 数组元素）：名称加上人类可读的
/// 跳过原因（非法名称、缺 SKILL.md、冲突跳过、拷贝失败等）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedSkill {
    /// 被跳过的技能名。
    pub skill_name: String,
    /// 跳过原因（与 JS 实现逐字一致的英文文案）。
    pub reason: String,
}

/// `installSkillsFromRepository` result: `{ ok, installed, skipped }` or
/// `{ ok: false, error }`.
/// 成功时 `ok: true` 且 `installed` / `skipped` 齐全；失败时 `ok: false`
/// 且仅带 `error`。None 字段序列化时省略，与 JS 返回对象形状一致。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InstallResult {
    /// 整体安装是否成功。
    pub ok: bool,
    /// 安装成功的技能列表（仅成功时存在）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed: Option<Vec<InstalledSkill>>,
    /// 被跳过的技能列表（仅成功时存在）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<Vec<SkippedSkill>>,
    /// 失败详情（仅失败时存在；conflicts 错误携带逐技能冲突列表）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<CatalogError>,
}

/// InstallResult 的构造辅助：成功打包列表，失败仅打包错误。
impl InstallResult {
    /// 构造成功结果：`ok: true` 并携带已安装与被跳过的技能列表。
    pub fn ok(installed: Vec<InstalledSkill>, skipped: Vec<SkippedSkill>) -> Self {
        InstallResult {
            ok: true,
            installed: Some(installed),
            skipped: Some(skipped),
            error: None,
        }
    }

    /// 构造失败结果：`ok: false` 并携带错误详情。
    pub fn err(error: CatalogError) -> Self {
        InstallResult {
            ok: false,
            installed: None,
            skipped: None,
            error: Some(error),
        }
    }
}

/// `installSkillsFromRepository({ source, subpath, defaultSubpath, identity,
/// scope, targetSource, workingDirectory, userSkillDir, selections,
/// conflictPolicy, conflictDecisions })`.
/// 安装参数全集：源仓库、scope 与目标目录树、可选 git 身份、选中目录、
/// 冲突策略与逐技能决策；全部可选以支持 `..Default::default()` 展开。
#[derive(Default, Clone)]
pub struct InstallParams {
    /// 仓库源字符串（HTTPS/SSH URL 或 `owner/repo` 简写）。
    pub source: Option<String>,
    /// 显式 subpath 覆盖，优先于源字符串内嵌路径。
    pub subpath: Option<String>,
    /// 源未自带 subpath 时使用的默认值。
    pub default_subpath: Option<String>,
    /// 可选 git 身份（携带 SSH key 时改用 SSH clone URL）。
    pub identity: Option<GitIdentity>,
    /// 安装 scope：`user` 或 `project`，其它值报 invalidSource。
    pub scope: Option<String>,
    /// 目标目录树：`opencode`（默认）或 `agents`。
    pub target_source: Option<String>,
    /// 项目安装的工作目录（scope 为 project 时必填）。
    pub working_directory: Option<String>,
    /// 用户技能根目录（scope 为 user 时必填；先做 legacy 路径迁移）。
    pub user_skill_dir: Option<String>,
    /// 待安装的技能目录选择列表（空列表报 no skills selected 错误）。
    pub selections: Option<Vec<InstallSelection>>,
    /// 冲突策略：`skipAll` / `overwriteAll`；无策略且目标已存在时返回
    /// conflicts 错误交由前端二次确认。
    pub conflict_policy: Option<String>,
    /// 逐技能冲突决策（技能名 → `skip` / `overwrite`），优先于全局策略。
    pub conflict_decisions: Option<HashMap<String, String>>,
    /// Test seam; `None` runs the real `git` binary.
    /// 测试注入点；`None` 时走真实 `git` 二进制。
    pub git_runner: Option<std::sync::Arc<dyn GitRunner>>,
}

/// `normalizeUserSkillDir`: migrate the legacy singular
/// `~/.config/opencode/skill` to `skills` unless only the legacy dir exists.
/// 传入值等于 legacy 单数目录时：仅当只有单数目录存在才沿用，否则返回
/// 复数 `skills` 目录（迁移语义）；其它值原样返回，空串视为未提供。
fn normalize_user_skill_dir(user_skill_dir: Option<&str>) -> Option<String> {
    let dir = user_skill_dir.filter(|dir| !dir.is_empty())?;
    let Some(home) = crate::config::home_dir() else {
        return Some(dir.to_string());
    };
    let legacy = home.join(".config").join("opencode").join("skill");
    let plural = home.join(".config").join("opencode").join("skills");
    // JS compares raw strings; compare the same way.
    if dir == legacy.to_string_lossy() {
        if legacy.exists() && !plural.exists() {
            return Some(legacy.to_string_lossy().to_string());
        }
        return Some(plural.to_string_lossy().to_string());
    }
    Some(dir.to_string())
}

/// install.js `toFsPath`: join `/`-separated parts (each trimmed) under the
/// repo dir.
/// 按 `/` 拆分、逐段 trim、丢弃空段后拼接到仓库根目录之下，得到仓库内
/// 相对目录的本地路径。
fn to_fs_path(repo_dir: &Path, repo_rel_posix_path: &str) -> PathBuf {
    let mut path = repo_dir.to_path_buf();
    for part in repo_rel_posix_path
        .split('/')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        path.push(part);
    }
    path
}

/// 递归创建目录（mkdir -p 语义）。
fn ensure_dir(dir_path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir_path)
}

/// 取 `/` 分隔路径的最后一段（无分隔符则原样返回），用作技能名。
fn posix_basename(path: &str) -> String {
    match path.rfind('/') {
        Some(index) => path[index + 1..].to_string(),
        None => path.to_string(),
    }
}

/// `getTargetSkillDir`.
/// 解析某个技能的安装目标目录：user+opencode → userSkillDir；
/// user+agents → `~/.agents/skills`；project → 工作目录下
/// `.opencode/skills` 或 `.agents/skills`。
fn get_target_skill_dir(
    scope: &str,
    target_source: Option<&str>,
    working_directory: Option<&str>,
    user_skill_dir: &str,
    skill_name: &str,
) -> PathBuf {
    let source = if target_source == Some("agents") {
        "agents"
    } else {
        "opencode"
    };

    if scope == "user" {
        if source == "agents" {
            return crate::config::home_dir()
                .unwrap_or_default()
                .join(".agents")
                .join("skills")
                .join(skill_name);
        }
        return Path::new(user_skill_dir).join(skill_name);
    }

    // JS throws when workingDirectory is missing; callers validated earlier.
    let working_directory = working_directory.unwrap_or("");
    if source == "agents" {
        return Path::new(working_directory)
            .join(".agents")
            .join("skills")
            .join(skill_name);
    }
    Path::new(working_directory)
        .join(".opencode")
        .join("skills")
        .join(skill_name)
}

/// 把目标目录树参数归一化为 `agents` 或默认 `opencode` 标签。
fn target_source_label(target_source: Option<&str>) -> &'static str {
    if target_source == Some("agents") {
        "agents"
    } else {
        "opencode"
    }
}

/// `copyDirectoryNoSymlinks`: recursive copy rejecting symlinks and guarding
/// containment under the realpath'd source root. Error strings mirror the JS
/// messages (io errors fall back to their std Display text).
/// 先 canonicalize 源目录得到真实根再递归拷贝；符号链接与逃逸出真实根
/// 的路径都报错。错误文案与 JS 一致（io 错误退化为 std Display 文本）。
fn copy_directory_no_symlinks(src_dir: &Path, dst_dir: &Path) -> Result<(), String> {
    let src_real = std::fs::canonicalize(src_dir).map_err(|error| error.to_string())?;
    let src_real = src_real.to_string_lossy().to_string();
    ensure_dir(dst_dir).map_err(|error| error.to_string())?;
    walk_copy(src_dir, dst_dir, &src_real)
}

/// 递归拷贝核心：逐项做符号链接检查与父目录真实路径的包含校验；目录
/// 递归、文件拷贝并（unix）保留权限位，其余类型跳过。
fn walk_copy(current_src: &Path, current_dst: &Path, src_real: &str) -> Result<(), String> {
    let entries = std::fs::read_dir(current_src).map_err(|error| error.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        let next_src = current_src.join(&name);
        let next_dst = current_dst.join(&name);

        let stat = std::fs::symlink_metadata(&next_src).map_err(|error| error.to_string())?;
        if stat.file_type().is_symlink() {
            return Err("Symlinks are not supported in skills".to_string());
        }

        // Guard against traversal: the real parent must stay under srcReal
        // (JS uses plain string startsWith).
        let parent = next_src.parent().unwrap_or_else(|| Path::new("."));
        let next_real_parent = std::fs::canonicalize(parent).map_err(|error| error.to_string())?;
        if !next_real_parent.to_string_lossy().starts_with(src_real) {
            return Err("Invalid source path traversal detected".to_string());
        }

        if stat.is_dir() {
            ensure_dir(&next_dst).map_err(|error| error.to_string())?;
            walk_copy(&next_src, &next_dst, src_real)?;
            continue;
        }

        if stat.is_file() {
            if let Some(dst_parent) = next_dst.parent() {
                ensure_dir(dst_parent).map_err(|error| error.to_string())?;
            }
            std::fs::copy(&next_src, &next_dst).map_err(|error| error.to_string())?;
            #[cfg(unix)]
            {
                use crate::os_compat::PermissionsExt;
                let mode = stat.permissions().mode() & 0o777;
                let _ = std::fs::set_permissions(&next_dst, std::fs::Permissions::from_mode(mode));
            }
            continue;
        }

        // Skip other types (sockets, devices, etc.)
    }
    Ok(())
}

/// 浅克隆仓库到临时目录：优先 `--filter=blob:none` 部分克隆，失败退回
/// 普通 `--depth 1 --no-checkout`；两次都失败时返回最后一次的 GitResult
/// 供上层做错误映射。
async fn clone_repo(
    git: &dyn GitRunner,
    clone_url: &str,
    temp_dir: &Path,
    identity: &Option<GitIdentity>,
) -> Result<(), GitResult> {
    let run = |args: Vec<String>| {
        git.run(
            &args,
            &GitRunOptions {
                timeout_ms: Some(90_000),
                identity: identity.clone(),
                ..GitRunOptions::default()
            },
        )
    };

    let preferred = vec![
        "clone".to_string(),
        "--depth".to_string(),
        "1".to_string(),
        "--filter=blob:none".to_string(),
        "--no-checkout".to_string(),
        clone_url.to_string(),
        temp_dir.to_string_lossy().to_string(),
    ];
    if run(preferred).await.ok {
        return Ok(());
    }

    let fallback = vec![
        "clone".to_string(),
        "--depth".to_string(),
        "1".to_string(),
        "--no-checkout".to_string(),
        clone_url.to_string(),
        temp_dir.to_string_lossy().to_string(),
    ];
    let fallback_result = run(fallback).await;
    if fallback_result.ok {
        return Ok(());
    }
    Err(fallback_result)
}

/// 单个技能的安装计划：源目录、派生技能名与名称是否合法可安装。
struct SkillPlan {
    /// 技能在仓库内的目录（用于 sparse-checkout 与定位源文件）。
    skill_dir_posix: String,
    /// 技能名（目录 basename）。
    skill_name: String,
    /// 名称是否通过 `validate_skill_name` 校验（不合法则直接跳过）。
    installable: bool,
}

/// `installSkillsFromRepository` port.
/// 校验（git 可用性、scope、目标树、project 工作目录、源解析、选中项）
/// → 预计算冲突（无决策且无自动策略时直接返回 conflicts 错误）→ 建临时
/// 目录执行安装；任何路径都会清理临时目录。
pub async fn install_skills_from_repository(params: InstallParams) -> InstallResult {
    let git = resolve_runner(params.git_runner.clone());
    let identity = params.identity.clone();

    if let Err(error) = assert_git_available(git.as_ref()).await {
        return InstallResult::err(error);
    }

    let Some(user_skill_dir) = normalize_user_skill_dir(params.user_skill_dir.as_deref()) else {
        return InstallResult::err(CatalogError::unknown("userSkillDir is required"));
    };

    let scope = params.scope.clone().unwrap_or_default();
    if scope != "user" && scope != "project" {
        return InstallResult::err(CatalogError::invalid_source("Invalid scope"));
    }

    if let Some(target_source) = &params.target_source
        && target_source != "opencode"
        && target_source != "agents"
    {
        return InstallResult::err(CatalogError::invalid_source("Invalid target source"));
    }

    let working_directory = params.working_directory.as_deref();
    if scope == "project" && working_directory.unwrap_or("").is_empty() {
        return InstallResult::err(CatalogError::invalid_source(
            "Project installs require a directory parameter",
        ));
    }

    let parsed = match parse_skill_repo_source(
        params.source.as_deref().unwrap_or(""),
        params.subpath.as_deref(),
    ) {
        SourceParseResult::Ok(parsed) => parsed,
        SourceParseResult::Err(error) => return InstallResult::err(error),
    };

    // JS computes effectiveSubpath and immediately drops it (`void`).
    let _effective_subpath = parsed.effective_subpath.clone().or_else(|| {
        params
            .default_subpath
            .as_deref()
            .map(str::trim)
            .filter(|subpath| !subpath.is_empty())
            .map(str::to_string)
    });

    let use_ssh = params
        .identity
        .as_ref()
        .and_then(|identity| identity.ssh_key.as_deref())
        .is_some_and(|key| !key.is_empty());
    let clone_url = if use_ssh {
        parsed.clone_url_ssh.clone()
    } else {
        parsed.clone_url_https.clone()
    };
    let requested_dirs: Vec<String> = params
        .selections
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|selection| selection.skill_dir.trim().to_string())
        .filter(|skill_dir| !skill_dir.is_empty())
        .collect();
    if requested_dirs.is_empty() {
        return InstallResult::err(CatalogError::invalid_source(
            "No skills selected for installation",
        ));
    }

    let skill_plans: Vec<SkillPlan> = requested_dirs
        .iter()
        .map(|skill_dir_posix| {
            let skill_name = posix_basename(skill_dir_posix);
            let installable = validate_skill_name(&skill_name);
            SkillPlan {
                skill_dir_posix: skill_dir_posix.clone(),
                skill_name,
                installable,
            }
        })
        .collect();

    // Validate names early and compute conflicts without mutating.
    let conflict_policy = params.conflict_policy.as_deref();
    let mut conflicts: Vec<ConflictEntry> = Vec::new();
    for plan in &skill_plans {
        if !plan.installable {
            continue;
        }
        let target_dir = get_target_skill_dir(
            &scope,
            params.target_source.as_deref(),
            working_directory,
            &user_skill_dir,
            &plan.skill_name,
        );
        if target_dir.exists() {
            let decision = params
                .conflict_decisions
                .as_ref()
                .and_then(|decisions| decisions.get(&plan.skill_name));
            let has_auto_policy =
                conflict_policy == Some("skipAll") || conflict_policy == Some("overwriteAll");
            if decision.is_none() && !has_auto_policy {
                conflicts.push(ConflictEntry {
                    skill_name: plan.skill_name.clone(),
                    scope: scope.clone(),
                    source: target_source_label(params.target_source.as_deref()).to_string(),
                });
            }
        }
    }

    if !conflicts.is_empty() {
        return InstallResult::err(CatalogError::conflicts(conflicts));
    }

    let temp_base = match mkd_temp("ompchamber-skills-install-") {
        Ok(dir) => dir,
        Err(error) => {
            return InstallResult::err(CatalogError::unknown(format!(
                "Failed to create temporary directory: {error}"
            )));
        }
    };

    let result = install_inner(
        git.as_ref(),
        &params,
        &scope,
        &clone_url,
        &identity,
        &temp_base,
        &skill_plans,
        &user_skill_dir,
    )
    .await;

    safe_rm(&temp_base);
    result
}

/// The body of the JS `try` block (temp cleanup runs on every path).
/// 克隆 → cone 模式 sparse-checkout 检出所选目录 → 逐技能按决策拷贝；
/// 单个技能失败只计入 skipped，克隆/检出失败则整体报错。
#[allow(clippy::too_many_arguments)]
async fn install_inner(
    git: &dyn GitRunner,
    params: &InstallParams,
    scope: &str,
    clone_url: &str,
    identity: &Option<GitIdentity>,
    temp_base: &Path,
    skill_plans: &[SkillPlan],
    user_skill_dir: &str,
) -> InstallResult {
    let cloned = clone_repo(git, clone_url, temp_base, identity).await;
    if let Err(failure) = cloned {
        return InstallResult::err(clone_error(&failure));
    }

    let temp_base_str = temp_base.to_string_lossy().to_string();
    let run_git = |args: Vec<String>, timeout_ms: u64| {
        git.run(
            &args,
            &GitRunOptions {
                timeout_ms: Some(timeout_ms),
                identity: identity.clone(),
                ..GitRunOptions::default()
            },
        )
    };

    // Selective checkout for only the requested skill dirs.
    let _ = run_git(
        vec![
            "-C".to_string(),
            temp_base_str.clone(),
            "sparse-checkout".to_string(),
            "init".to_string(),
            "--cone".to_string(),
        ],
        15_000,
    )
    .await;

    let mut set_args = vec![
        "-C".to_string(),
        temp_base_str.clone(),
        "sparse-checkout".to_string(),
        "set".to_string(),
    ];
    set_args.extend(skill_plans.iter().map(|plan| plan.skill_dir_posix.clone()));
    let set_result = run_git(set_args, 30_000).await;
    if !set_result.ok {
        let message = if !set_result.stderr.is_empty() {
            set_result.stderr.clone()
        } else if !set_result.message.is_empty() {
            set_result.message
        } else {
            "Failed to configure sparse checkout".to_string()
        };
        return InstallResult::err(CatalogError::unknown(message));
    }

    let checkout_result = run_git(
        vec![
            "-C".to_string(),
            temp_base_str.clone(),
            "checkout".to_string(),
            "--force".to_string(),
            "HEAD".to_string(),
        ],
        60_000,
    )
    .await;
    if !checkout_result.ok {
        let message = if !checkout_result.stderr.is_empty() {
            checkout_result.stderr.clone()
        } else if !checkout_result.message.is_empty() {
            checkout_result.message
        } else {
            "Failed to checkout repository".to_string()
        };
        return InstallResult::err(CatalogError::unknown(message));
    }

    let conflict_policy = params.conflict_policy.as_deref();
    let mut installed: Vec<InstalledSkill> = Vec::new();
    let mut skipped: Vec<SkippedSkill> = Vec::new();

    for plan in skill_plans {
        if !plan.installable {
            skipped.push(SkippedSkill {
                skill_name: plan.skill_name.clone(),
                reason: "Invalid skill name (directory basename)".to_string(),
            });
            continue;
        }

        let src_dir = to_fs_path(temp_base, &plan.skill_dir_posix);
        let skill_md_path = src_dir.join("SKILL.md");
        if !skill_md_path.exists() {
            skipped.push(SkippedSkill {
                skill_name: plan.skill_name.clone(),
                reason: "SKILL.md not found in selected directory".to_string(),
            });
            continue;
        }

        let target_dir = get_target_skill_dir(
            scope,
            params.target_source.as_deref(),
            params.working_directory.as_deref(),
            user_skill_dir,
            &plan.skill_name,
        );
        let exists = target_dir.exists();

        let mut decision = params
            .conflict_decisions
            .as_ref()
            .and_then(|decisions| decisions.get(&plan.skill_name).cloned());
        if decision.is_none() {
            if exists && conflict_policy == Some("skipAll") {
                decision = Some("skip".to_string());
            }
            if exists && conflict_policy == Some("overwriteAll") {
                decision = Some("overwrite".to_string());
            }
            if !exists {
                decision = Some("overwrite".to_string());
            }
        }

        if exists && decision.as_deref() == Some("skip") {
            skipped.push(SkippedSkill {
                skill_name: plan.skill_name.clone(),
                reason: "Already installed (skipped)".to_string(),
            });
            continue;
        }

        if exists && decision.as_deref() == Some("overwrite") {
            safe_rm(&target_dir);
        }

        // Ensure project parent directories exist.
        if let Some(parent) = target_dir.parent()
            && let Err(error) = ensure_dir(parent)
        {
            skipped.push(SkippedSkill {
                skill_name: plan.skill_name.clone(),
                reason: error.to_string(),
            });
            continue;
        }

        match copy_directory_no_symlinks(&src_dir, &target_dir) {
            Ok(()) => installed.push(InstalledSkill {
                skill_name: plan.skill_name.clone(),
                scope: scope.to_string(),
                source: target_source_label(params.target_source.as_deref()).to_string(),
            }),
            Err(reason) => {
                safe_rm(&target_dir);
                skipped.push(SkippedSkill {
                    skill_name: plan.skill_name.clone(),
                    reason,
                });
            }
        }
    }

    InstallResult::ok(installed, skipped)
}

/// install 流程测试：用 FakeGit 伪造克隆与 sparse-checkout，覆盖冲突
/// 策略、逐技能决策、符号链接拒绝、scope 解析、错误文案与序列化形状。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills_catalog::git::GitResult;
    use crate::skills_catalog::test_support::{
        EnvGuard, FakeGit, TEST_LOCK, copy_dir_recursive, unique_temp_dir, write_skill_md,
    };

    /// 把目录字符串列表包装成 InstallSelection 列表。
    fn selections(dirs: &[&str]) -> Vec<InstallSelection> {
        dirs.iter()
            .map(|dir| InstallSelection {
                skill_dir: dir.to_string(),
            })
            .collect()
    }

    /// A fake runner that "clones" a fixture repo into the target dir and
    /// otherwise succeeds; `mutate` can per-test behaviors.
    /// install 场景假 runner：`--version` 成功、`clone` 把 fixture 目录
    /// 整体拷到目标路径模拟克隆，其余子命令静默成功。
    fn install_git_fake(fixture: &Path) -> std::sync::Arc<FakeGit> {
        let fixture = fixture.to_path_buf();
        FakeGit::new(move |args| {
            if args.first().map(String::as_str) == Some("--version") {
                return GitResult::success("git version 2.0.0\n", "");
            }
            if args.first().map(String::as_str) == Some("clone") {
                if let Some(target) = args.last() {
                    copy_dir_recursive(&fixture, Path::new(target));
                }
                return GitResult::success("", "");
            }
            GitResult::success("", "")
        })
    }

    /// 每个测试的临时仓库 + 用户技能目录夹具（自带 alpha/beta 技能）。
    struct Fixture {
        /// 伪仓库根目录（含 skills/alpha 与 skills/beta）。
        repo: PathBuf,
        /// 用户技能根目录（作为 userSkillDir 传入）。
        skills_home: PathBuf,
    }

    /// Fixture 的构造、参数生成与清理辅助。
    impl Fixture {
        /// 建立夹具：写入 alpha（带 reference.md）与 beta 两个技能。
        fn new(prefix: &str) -> Self {
            let repo = unique_temp_dir(&format!("{prefix}-repo"));
            write_skill_md(
                &repo.join("skills/alpha"),
                "name: alpha\ndescription: Alpha skill\n",
                "alpha body\n",
            );
            std::fs::write(repo.join("skills/alpha/reference.md"), "ref").expect("write");
            write_skill_md(&repo.join("skills/beta"), "name: beta\n", "beta body\n");
            Fixture {
                repo,
                skills_home: unique_temp_dir(&format!("{prefix}-home")),
            }
        }

        /// 生成指向该夹具的默认 user-scope 安装参数。
        fn params(&self, dirs: &[&str]) -> InstallParams {
            InstallParams {
                source: Some("owner/repo".to_string()),
                scope: Some("user".to_string()),
                user_skill_dir: Some(self.skills_home.to_string_lossy().to_string()),
                selections: Some(selections(dirs)),
                git_runner: Some(install_git_fake(&self.repo)),
                ..Default::default()
            }
        }

        /// 删除夹具的全部临时目录。
        fn cleanup(&self) {
            std::fs::remove_dir_all(&self.repo).ok();
            std::fs::remove_dir_all(&self.skills_home).ok();
        }
    }

    /// install 的薄封装，让测试断言更短。
    async fn install(params: InstallParams) -> InstallResult {
        install_skills_from_repository(params).await
    }

    /// 行为契约：选中的技能被完整拷贝到用户技能目录（含附属文件），
    /// 结果列表携带名称/scope/source 且无跳过项。
    #[tokio::test]
    async fn installs_selected_skills_into_the_user_skill_dir() {
        let fixture = Fixture::new("install-user");
        let result = install(fixture.params(&["skills/alpha", "skills/beta"])).await;

        assert!(result.ok, "install failed: {:?}", result.error);
        let installed = result.installed.expect("installed");
        assert_eq!(installed.len(), 2);
        assert_eq!(installed[0].skill_name, "alpha");
        assert_eq!(installed[0].scope, "user");
        assert_eq!(installed[0].source, "opencode");

        let alpha_md = std::fs::read_to_string(fixture.skills_home.join("alpha/SKILL.md"))
            .expect("copied SKILL.md");
        assert!(alpha_md.contains("name: alpha"));
        assert!(alpha_md.ends_with("alpha body\n"));
        assert!(fixture.skills_home.join("alpha/reference.md").exists());
        assert!(fixture.skills_home.join("beta/SKILL.md").exists());
        assert_eq!(result.skipped, Some(Vec::new()));
        fixture.cleanup();
    }

    /// 行为契约：sparse-checkout 以 cone 模式 init，且 set 的目录恰好
    /// 等于选中的技能目录。
    #[tokio::test]
    async fn sparse_checkout_requests_exactly_the_selected_dirs() {
        let fixture = Fixture::new("install-sparse");
        let params = fixture.params(&["skills/alpha"]);
        let git = install_git_fake(&fixture.repo);
        let params = InstallParams {
            git_runner: Some(git.clone()),
            ..params
        };

        let result = install(params).await;
        assert!(result.ok, "install failed: {:?}", result.error);

        let sets: Vec<Vec<String>> = git
            .calls()
            .into_iter()
            .filter(|call| call.len() > 4 && call[2] == "sparse-checkout" && call[3] == "set")
            .collect();
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0][4], "skills/alpha");

        let inits: Vec<Vec<String>> = git
            .calls()
            .into_iter()
            .filter(|call| call.len() > 4 && call[2] == "sparse-checkout" && call[3] == "init")
            .collect();
        assert_eq!(inits.len(), 1, "cone-mode init: {:?}", inits);
        assert_eq!(inits[0][4], "--cone");
        fixture.cleanup();
    }

    /// 行为契约：目标已存在且无策略/决策时返回 conflicts 错误并列出
    /// 冲突项，已存在的技能文件保持原样。
    #[tokio::test]
    async fn existing_skills_without_policy_report_conflicts() {
        let fixture = Fixture::new("install-conflict");
        write_skill_md(&fixture.skills_home.join("alpha"), "", "existing\n");

        let result = install(fixture.params(&["skills/alpha", "skills/beta"])).await;

        assert!(!result.ok);
        let error = result.error.expect("error");
        assert_eq!(error.kind, "conflicts");
        assert_eq!(
            error.message,
            "Some skills already exist in the selected scope"
        );
        assert_eq!(
            error.conflicts,
            Some(vec![ConflictEntry {
                skill_name: "alpha".to_string(),
                scope: "user".to_string(),
                source: "opencode".to_string(),
            }])
        );
        // The pre-existing skill is untouched.
        assert_eq!(
            std::fs::read_to_string(fixture.skills_home.join("alpha/SKILL.md")).expect("read"),
            "---\n---\nexisting\n"
        );
        fixture.cleanup();
    }

    /// 行为契约：skipAll 策略保留已存在技能（记为 skipped），其余照常
    /// 安装。
    #[tokio::test]
    async fn skip_all_policy_keeps_existing_skills() {
        let fixture = Fixture::new("install-skipall");
        write_skill_md(&fixture.skills_home.join("alpha"), "", "existing\n");

        let result = install(InstallParams {
            conflict_policy: Some("skipAll".to_string()),
            ..fixture.params(&["skills/alpha", "skills/beta"])
        })
        .await;

        assert!(result.ok, "install failed: {:?}", result.error);
        assert_eq!(result.installed.expect("installed").len(), 1);
        let skipped = result.skipped.expect("skipped");
        assert_eq!(
            skipped,
            vec![SkippedSkill {
                skill_name: "alpha".to_string(),
                reason: "Already installed (skipped)".to_string(),
            }]
        );
        assert_eq!(
            std::fs::read_to_string(fixture.skills_home.join("alpha/SKILL.md")).expect("read"),
            "---\n---\nexisting\n"
        );
        fixture.cleanup();
    }

    /// 行为契约：overwriteAll 策略删除并替换已存在技能的内容。
    #[tokio::test]
    async fn overwrite_all_policy_replaces_existing_skills() {
        let fixture = Fixture::new("install-overwrite");
        write_skill_md(&fixture.skills_home.join("alpha"), "", "existing\n");

        let result = install(InstallParams {
            conflict_policy: Some("overwriteAll".to_string()),
            ..fixture.params(&["skills/alpha"])
        })
        .await;

        assert!(result.ok, "install failed: {:?}", result.error);
        assert_eq!(result.installed.expect("installed").len(), 1);
        let alpha =
            std::fs::read_to_string(fixture.skills_home.join("alpha/SKILL.md")).expect("read");
        assert!(alpha.contains("name: alpha"), "replaced content: {alpha}");
        fixture.cleanup();
    }

    /// 行为契约：逐技能决策（overwrite）优先于无策略状态，强制覆盖
    /// 安装且不报冲突。
    #[tokio::test]
    async fn per_skill_decisions_override_the_policy() {
        let fixture = Fixture::new("install-decisions");
        write_skill_md(&fixture.skills_home.join("alpha"), "", "existing\n");
        let mut decisions = HashMap::new();
        decisions.insert("alpha".to_string(), "overwrite".to_string());

        let result = install(InstallParams {
            conflict_decisions: Some(decisions),
            ..fixture.params(&["skills/alpha"])
        })
        .await;

        assert!(result.ok, "no conflict error: {:?}", result.error);
        let alpha =
            std::fs::read_to_string(fixture.skills_home.join("alpha/SKILL.md")).expect("read");
        assert!(
            alpha.contains("name: alpha"),
            "decision forced overwrite: {alpha}"
        );
        fixture.cleanup();
    }

    /// 行为契约：非法目录名与缺 SKILL.md 的目录都归入 skipped 并给出
    /// 对应原因文案，整体仍算成功。
    #[tokio::test]
    async fn invalid_names_and_missing_skill_md_are_skipped() {
        let fixture = Fixture::new("install-skips");
        let result = install(fixture.params(&["skills/Bad_Name", "skills/does-not-exist"])).await;

        assert!(result.ok, "install failed: {:?}", result.error);
        assert_eq!(result.installed.expect("installed").len(), 0);
        assert_eq!(
            result.skipped.expect("skipped"),
            vec![
                SkippedSkill {
                    skill_name: "Bad_Name".to_string(),
                    reason: "Invalid skill name (directory basename)".to_string(),
                },
                SkippedSkill {
                    skill_name: "does-not-exist".to_string(),
                    reason: "SKILL.md not found in selected directory".to_string(),
                },
            ]
        );
        fixture.cleanup();
    }

    /// 行为契约：源仓库中的符号链接导致该技能跳过且目标目录被清理。
    #[tokio::test]
    async fn symlinks_in_skill_sources_are_rejected() {
        let fixture = Fixture::new("install-symlink");
        crate::os_compat::symlink(
            std::path::Path::new("reference.md"),
            &fixture.repo.join("skills/alpha/link.md"),
        )
        .expect("symlink");

        let result = install(fixture.params(&["skills/alpha"])).await;

        assert!(result.ok, "install failed: {:?}", result.error);
        assert_eq!(
            result.skipped.expect("skipped"),
            vec![SkippedSkill {
                skill_name: "alpha".to_string(),
                reason: "Symlinks are not supported in skills".to_string(),
            }]
        );
        assert!(
            !fixture.skills_home.join("alpha").exists(),
            "target cleaned up"
        );
        fixture.cleanup();
    }

    /// 行为契约：project scope 分别落到工作目录的 `.opencode/skills`
    /// 与 agents 目标树的 `.agents/skills`。
    #[tokio::test]
    async fn project_scope_installs_into_working_directory_trees() {
        let fixture = Fixture::new("install-project");
        let project = unique_temp_dir("install-project-wd");

        let result = install(InstallParams {
            scope: Some("project".to_string()),
            working_directory: Some(project.to_string_lossy().to_string()),
            ..fixture.params(&["skills/alpha"])
        })
        .await;
        assert!(result.ok, "opencode project install: {:?}", result.error);
        assert!(project.join(".opencode/skills/alpha/SKILL.md").exists());
        let project2 = unique_temp_dir("install-project-wd2");
        let result = install(InstallParams {
            scope: Some("project".to_string()),
            working_directory: Some(project2.to_string_lossy().to_string()),
            target_source: Some("agents".to_string()),
            ..fixture.params(&["skills/beta"])
        })
        .await;
        assert!(result.ok, "agents project install: {:?}", result.error);
        assert!(project2.join(".agents/skills/beta/SKILL.md").exists());

        std::fs::remove_dir_all(&project).ok();
        std::fs::remove_dir_all(&project2).ok();
        fixture.cleanup();
    }

    /// 行为契约：各类参数校验失败返回与 JS 逐字一致的 kind 与 message。
    #[tokio::test]
    async fn validation_errors_match_the_js_messages() {
        let fixture = Fixture::new("install-validation");

        let cases: Vec<(InstallParams, &str, &str)> = vec![
            (
                InstallParams {
                    source: Some("owner/repo".to_string()),
                    scope: Some("user".to_string()),
                    ..Default::default()
                },
                "unknown",
                "userSkillDir is required",
            ),
            (
                InstallParams {
                    source: Some("owner/repo".to_string()),
                    scope: Some("bogus".to_string()),
                    user_skill_dir: Some("/tmp/x".to_string()),
                    ..Default::default()
                },
                "invalidSource",
                "Invalid scope",
            ),
            (
                InstallParams {
                    source: Some("owner/repo".to_string()),
                    scope: Some("user".to_string()),
                    target_source: Some("claude".to_string()),
                    user_skill_dir: Some("/tmp/x".to_string()),
                    ..Default::default()
                },
                "invalidSource",
                "Invalid target source",
            ),
            (
                InstallParams {
                    source: Some("owner/repo".to_string()),
                    scope: Some("project".to_string()),
                    user_skill_dir: Some("/tmp/x".to_string()),
                    ..Default::default()
                },
                "invalidSource",
                "Project installs require a directory parameter",
            ),
            (
                InstallParams {
                    scope: Some("user".to_string()),
                    user_skill_dir: Some("/tmp/x".to_string()),
                    selections: Some(Vec::new()),
                    ..Default::default()
                },
                "invalidSource",
                "Repository source is required",
            ),
            (
                InstallParams {
                    source: Some("owner/repo".to_string()),
                    scope: Some("user".to_string()),
                    user_skill_dir: Some("/tmp/x".to_string()),
                    selections: Some(Vec::new()),
                    ..Default::default()
                },
                "invalidSource",
                "No skills selected for installation",
            ),
        ];

        for (params, kind, message) in cases {
            let result = install(params).await;
            assert!(!result.ok);
            let error = result.error.expect("error");
            assert_eq!(error.kind, kind, "kind for {message}");
            assert_eq!(error.message, message);
        }
        fixture.cleanup();
    }

    /// 行为契约：克隆输出含 publickey 拒绝时映射为 authRequired 错误且
    /// sshOnly 为 true。
    #[tokio::test]
    async fn clone_auth_failure_maps_to_auth_required() {
        let fixture = Fixture::new("install-auth");
        let git = FakeGit::new(|args| {
            if args.first().map(String::as_str) == Some("--version") {
                return GitResult::success("git version 2.0.0\n", "");
            }
            GitResult {
                stderr: "Permission denied (publickey)".to_string(),
                message: "Command failed: git clone".to_string(),
                ..GitResult::default()
            }
        });

        let result = install(InstallParams {
            git_runner: Some(git),
            ..fixture.params(&["skills/alpha"])
        })
        .await;

        let error = result.error.expect("error");
        assert_eq!(error.kind, "authRequired");
        assert_eq!(error.ssh_only, Some(true));
        fixture.cleanup();
    }

    /// 行为契约：sparse-checkout set 失败时把 stderr 原文作为 unknown
    /// 错误 message 上报。
    #[tokio::test]
    async fn sparse_set_failure_reports_stderr_as_unknown() {
        let fixture = Fixture::new("install-sparse-fail");
        let git = FakeGit::new(|args| {
            if args.first().map(String::as_str) == Some("--version") {
                return GitResult::success("git version 2.0.0\n", "");
            }
            if args.first().map(String::as_str) == Some("clone") {
                return GitResult::success("", "");
            }
            if args.len() > 3 && args[2] == "sparse-checkout" && args[3] == "set" {
                return GitResult {
                    stderr: "sparse-checkout failed badly".to_string(),
                    message: "Command failed".to_string(),
                    ..GitResult::default()
                };
            }
            GitResult::success("", "")
        });

        let result = install(InstallParams {
            git_runner: Some(git),
            ..fixture.params(&["skills/alpha"])
        })
        .await;

        let error = result.error.expect("error");
        assert_eq!(error.kind, "unknown");
        assert_eq!(error.message, "sparse-checkout failed badly");
        fixture.cleanup();
    }

    /// 行为契约：legacy 单数 skill 目录在仅有单数目录时保留，否则迁移
    /// 到复数 skills 目录安装。
    #[tokio::test]
    async fn legacy_user_skill_dir_migrates_to_plural() {
        let _guard = TEST_LOCK.lock().await;
        let home = unique_temp_dir("install-home");
        let legacy = home.join(".config").join("opencode").join("skill");
        let plural = home.join(".config").join("opencode").join("skills");
        // Windows resolves home through USERPROFILE first (os.homedir parity) —
        // isolate both so the fixture home wins.
        let _env = EnvGuard::set("HOME", home.to_str().expect("utf8"));
        let _env_profile = EnvGuard::set("USERPROFILE", home.to_str().expect("utf8"));

        // Neither exists → plural.
        let fixture = Fixture::new("install-legacy-none");
        let result = install(InstallParams {
            user_skill_dir: Some(legacy.to_string_lossy().to_string()),
            ..fixture.params(&["skills/alpha"])
        })
        .await;
        assert!(result.ok, "install failed: {:?}", result.error);
        assert!(plural.join("alpha/SKILL.md").exists());
        assert!(!legacy.join("alpha").exists());
        fixture.cleanup();

        // Only legacy exists → keep legacy (fresh home: the plural dir must
        // not exist for this branch).
        {
            let home = unique_temp_dir("install-home-kept");
            let legacy = home.join(".config").join("opencode").join("skill");
            // Windows resolves home through USERPROFILE first (os.homedir parity) —
            // isolate both so the fixture home wins.
            let _env = EnvGuard::set("HOME", home.to_str().expect("utf8"));
            let _env_profile = EnvGuard::set("USERPROFILE", home.to_str().expect("utf8"));
            std::fs::create_dir_all(&legacy).expect("mkdir");

            let fixture = Fixture::new("install-legacy-kept");
            let result = install(InstallParams {
                user_skill_dir: Some(legacy.to_string_lossy().to_string()),
                ..fixture.params(&["skills/beta"])
            })
            .await;
            assert!(result.ok, "install failed: {:?}", result.error);
            assert!(legacy.join("beta/SKILL.md").exists());
            fixture.cleanup();
            std::fs::remove_dir_all(&home).ok();
        }

        std::fs::remove_dir_all(&home).ok();
    }

    /// 行为契约：normalize_user_skill_dir 对 None/空串返回 None，对
    /// 自定义路径原样保留。
    #[test]
    fn normalizes_paths_exactly_like_the_js_string_compare() {
        assert_eq!(normalize_user_skill_dir(None), None);
        assert_eq!(normalize_user_skill_dir(Some("")), None);
        assert_eq!(
            normalize_user_skill_dir(Some("/opt/custom-skills")),
            Some("/opt/custom-skills".to_string())
        );
    }

    /// 行为契约：成功与失败结果的 JSON 形状与 JS 返回对象完全一致
    /// （camelCase、省略 None 字段）。
    #[tokio::test]
    async fn install_result_serializes_to_the_js_shape() {
        let ok = InstallResult::ok(
            vec![InstalledSkill {
                skill_name: "alpha".to_string(),
                scope: "user".to_string(),
                source: "opencode".to_string(),
            }],
            vec![SkippedSkill {
                skill_name: "beta".to_string(),
                reason: "Already installed (skipped)".to_string(),
            }],
        );
        assert_eq!(
            serde_json::to_value(&ok).expect("serialize"),
            serde_json::json!({
                "ok": true,
                "installed": [{"skillName": "alpha", "scope": "user", "source": "opencode"}],
                "skipped": [{"skillName": "beta", "reason": "Already installed (skipped)"}],
            })
        );

        let failed = InstallResult::err(CatalogError::conflicts(vec![ConflictEntry {
            skill_name: "alpha".to_string(),
            scope: "user".to_string(),
            source: "opencode".to_string(),
        }]));
        assert_eq!(
            serde_json::to_value(&failed).expect("serialize"),
            serde_json::json!({
                "ok": false,
                "error": {
                    "kind": "conflicts",
                    "message": "Some skills already exist in the selected scope",
                    "conflicts": [{"skillName": "alpha", "scope": "user", "source": "opencode"}],
                },
            })
        );
    }
}
