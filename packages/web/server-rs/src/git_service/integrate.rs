//! Port of the integrate-worktree workflow from `server/lib/git/service.js`
//! (`computeIntegratePlan`, `getIntegrateConflictDetails`,
//! `isCherryPickInProgress`, `integrateWorktreeCommits`, `abortIntegrate`,
//! `continueIntegrate`) backing `/api/git/integrate/*`.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::paths::{absolutize, normalize_directory_path, trim_git_lines};
use super::service::{GitService, ServiceResult};

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

fn normalize_path_field(value: Option<&Value>, field_name: &str) -> ServiceResult<PathBuf> {
    let target = normalize_directory_path(value.and_then(Value::as_str))
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| format!("{} is required", field_name))?;
    Ok(absolutize(Path::new(&target)))
}

struct IntegrateWorktreeEntry {
    path: String,
    branch_ref: Option<String>,
}

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

fn rand_suffix() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    (0..10)
        .map(|_| format!("{:x}", rng.random_range(0..16)))
        .collect()
}

async fn remove_integrate_temp_worktree(service: &GitService, repo_root: &Path, tmp_dir: &Path) {
    let _ = service
        .run(
            repo_root,
            &["worktree", "remove", "--force", &tmp_dir.to_string_lossy()],
        )
        .await;
    let _ = service.run(repo_root, &["worktree", "prune"]).await;
}

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

async fn sync_clean_integrate_target_worktrees(service: &GitService, paths: &[String]) {
    for target in paths {
        let _ = service.run(target, &["reset", "--hard"]).await;
    }
}

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

struct IntegrateState {
    repo_root: PathBuf,
    temp_worktree_path: PathBuf,
    source_branch: String,
    target_branch: String,
    clean_target_worktrees: Vec<String>,
    remaining_commits: Vec<String>,
    current_commit: String,
}

impl IntegrateState {
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

pub async fn abort_integrate(service: &GitService, state_input: &Value) -> ServiceResult<Value> {
    let state = normalize_integrate_state(state_input)?;
    let _ = service
        .run(&state.temp_worktree_path, &["cherry-pick", "--abort"])
        .await;
    remove_integrate_temp_worktree(service, &state.repo_root, &state.temp_worktree_path).await;
    Ok(json!({ "success": true }))
}

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

impl Clone for IntegrateState {
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
