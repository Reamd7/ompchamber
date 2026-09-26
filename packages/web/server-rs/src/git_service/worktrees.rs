//! Worktree management — port of `getWorktrees` / `validateWorktreeCreate` /
//! `previewWorktreeCreate` / `createWorktree` / `removeWorktree` /
//! `getWorktreeBootstrapStatus` and the bootstrap pipeline
//! (`queueWorktreeBootstrap`, `runPostCheckoutHook`, `populateWorktree…`)
//! from `server/lib/git/service.js`.
//!
//! These are free functions over `Arc<GitService>` because the fast-create
//! path spawns a detached bootstrap task that must own its service handle.

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

const BOOTSTRAP_PENDING: &str = "pending";
const BOOTSTRAP_READY: &str = "ready";
const BOOTSTRAP_FAILED: &str = "failed";
const PHASE_DIRECTORY_CREATED: &str = "directory-created";
const PHASE_GIT_READY: &str = "git-ready";
const PHASE_SETUP_READY: &str = "setup-ready";
const GIT_NULL_REF: &str = "0000000000000000000000000000000000000000";

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn bootstrap_key(directory: &str) -> Option<PathBuf> {
    let normalized = normalize_directory_path(Some(directory))?;
    if normalized.trim().is_empty() {
        return None;
    }
    Some(absolutize(Path::new(&normalized)))
}

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

pub(crate) struct WorktreeProjectContext {
    pub project_id: String,
    pub sandbox: PathBuf,
    pub primary_worktree: PathBuf,
    pub worktree_root: PathBuf,
}

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

impl GitService {
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

struct WorktreeCandidate {
    name: String,
    directory: PathBuf,
    branch: String,
}

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

struct ExistingModeResolution {
    local_branch: String,
    checkout_ref: String,
    create_local_branch: bool,
    remote_ref: Option<super::paths::RemoteBranchRef>,
}

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

struct ExistingWorktreeSource {
    local_branch: String,
    checkout_ref: String,
    create_local_branch: bool,
    set_upstream: bool,
    upstream_remote: String,
    upstream_branch: String,
}

/// JS `resolveExistingWorktreeSource` (validate + create share it).
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
        use std::os::unix::fs::PermissionsExt;
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

async fn is_attached_git_worktree_directory(service: &GitService, directory: &str) -> bool {
    let result = service
        .run(directory, &["rev-parse", "--is-inside-work-tree"])
        .await;
    result.success && result.stdout.trim() == "true"
}

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
pub(crate) fn is_not_repo_result(result: &GitCommandResult) -> bool {
    let text = parse_git_error_text(&result.stderr, &result.stdout, &result.message);
    text.to_lowercase().contains("not a git repository")
}

impl GitService {
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

#[allow(dead_code)]
fn _unused(_: &HashSet<String>, _: &Map<String, Value>) {}
