//! Port of `server/lib/skills-catalog/install.js`
//! (`installSkillsFromRepository`): clone shallowly, sparse-checkout only
//! the requested skill directories, resolve conflicts (prompt / skipAll /
//! overwriteAll with per-skill decisions), and copy each skill without
//! symlinks and with traversal guards. Targets resolve to the user skill
//! dir or the project `.opencode`/`.agents` trees.

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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallSelection {
    pub skill_dir: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledSkill {
    pub skill_name: String,
    pub scope: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedSkill {
    pub skill_name: String,
    pub reason: String,
}

/// `installSkillsFromRepository` result: `{ ok, installed, skipped }` or
/// `{ ok: false, error }`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InstallResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed: Option<Vec<InstalledSkill>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<Vec<SkippedSkill>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<CatalogError>,
}

impl InstallResult {
    pub fn ok(installed: Vec<InstalledSkill>, skipped: Vec<SkippedSkill>) -> Self {
        InstallResult {
            ok: true,
            installed: Some(installed),
            skipped: Some(skipped),
            error: None,
        }
    }

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
#[derive(Default, Clone)]
pub struct InstallParams {
    pub source: Option<String>,
    pub subpath: Option<String>,
    pub default_subpath: Option<String>,
    pub identity: Option<GitIdentity>,
    pub scope: Option<String>,
    pub target_source: Option<String>,
    pub working_directory: Option<String>,
    pub user_skill_dir: Option<String>,
    pub selections: Option<Vec<InstallSelection>>,
    pub conflict_policy: Option<String>,
    pub conflict_decisions: Option<HashMap<String, String>>,
    /// Test seam; `None` runs the real `git` binary.
    pub git_runner: Option<std::sync::Arc<dyn GitRunner>>,
}

/// `normalizeUserSkillDir`: migrate the legacy singular
/// `~/.config/opencode/skill` to `skills` unless only the legacy dir exists.
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

fn ensure_dir(dir_path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir_path)
}

fn posix_basename(path: &str) -> String {
    match path.rfind('/') {
        Some(index) => path[index + 1..].to_string(),
        None => path.to_string(),
    }
}

/// `getTargetSkillDir`.
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
fn copy_directory_no_symlinks(src_dir: &Path, dst_dir: &Path) -> Result<(), String> {
    let src_real = std::fs::canonicalize(src_dir).map_err(|error| error.to_string())?;
    let src_real = src_real.to_string_lossy().to_string();
    ensure_dir(dst_dir).map_err(|error| error.to_string())?;
    walk_copy(src_dir, dst_dir, &src_real)
}

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

struct SkillPlan {
    skill_dir_posix: String,
    skill_name: String,
    installable: bool,
}

/// `installSkillsFromRepository` port.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills_catalog::git::GitResult;
    use crate::skills_catalog::test_support::{
        EnvGuard, FakeGit, TEST_LOCK, copy_dir_recursive, unique_temp_dir, write_skill_md,
    };

    fn selections(dirs: &[&str]) -> Vec<InstallSelection> {
        dirs.iter()
            .map(|dir| InstallSelection {
                skill_dir: dir.to_string(),
            })
            .collect()
    }

    /// A fake runner that "clones" a fixture repo into the target dir and
    /// otherwise succeeds; `mutate` can per-test behaviors.
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

    struct Fixture {
        repo: PathBuf,
        skills_home: PathBuf,
    }

    impl Fixture {
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

        fn cleanup(&self) {
            std::fs::remove_dir_all(&self.repo).ok();
            std::fs::remove_dir_all(&self.skills_home).ok();
        }
    }

    async fn install(params: InstallParams) -> InstallResult {
        install_skills_from_repository(params).await
    }

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

    #[tokio::test]
    async fn symlinks_in_skill_sources_are_rejected() {
        let fixture = Fixture::new("install-symlink");
        #[cfg(unix)]
        std::os::unix::fs::symlink("reference.md", fixture.repo.join("skills/alpha/link.md"))
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

    #[tokio::test]
    async fn legacy_user_skill_dir_migrates_to_plural() {
        let _guard = TEST_LOCK.lock().await;
        let home = unique_temp_dir("install-home");
        let legacy = home.join(".config/opencode/skill");
        let plural = home.join(".config/opencode/skills");
        let _env = EnvGuard::set("HOME", home.to_str().expect("utf8"));

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
            let legacy = home.join(".config/opencode/skill");
            let _env = EnvGuard::set("HOME", home.to_str().expect("utf8"));
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

    #[test]
    fn normalizes_paths_exactly_like_the_js_string_compare() {
        assert_eq!(normalize_user_skill_dir(None), None);
        assert_eq!(normalize_user_skill_dir(Some("")), None);
        assert_eq!(
            normalize_user_skill_dir(Some("/opt/custom-skills")),
            Some("/opt/custom-skills".to_string())
        );
    }

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
