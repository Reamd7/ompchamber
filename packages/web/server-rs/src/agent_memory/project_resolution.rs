//! Port of `server/lib/agent-memory/project-resolution.js`.
//!
//! Which project's memory a session directory belongs to. A session often
//! runs in a worktree, whose path is not the project's path; keying memory
//! by the session directory filed a worktree's memories under a project the
//! panel never reads. Every worktree of a repository shares one project
//! memory, which is also what the user means by "this project".
//!
//! A directory that is itself a configured project is taken as-is; anything
//! else resolves to its primary worktree. The configured check comes first
//! because a user may register a worktree as a project in its own right, and
//! that choice has to win over the git topology.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use crate::projects::create_project_id_from_path;
use crate::settings::normalization::{path_relative, path_resolve};

/// `listProjectPaths` seam: returns configured project paths; may fail (an
/// unreadable project list must not lose the memory — the git-derived root
/// below still converges every worktree of the repository on one store).
pub type ListProjectPaths = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<String>>> + Send>> + Send + Sync,
>;

/// `resolvePrimaryWorktreeRoot` seam: returns the primary checkout root, or
/// `None` when git is unavailable / the directory is not a checkout (the JS
/// catch and the null return converge on the same fallback).
pub type ResolvePrimaryWorktreeRoot =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = Option<String>> + Send>> + Send + Sync>;

#[derive(Clone)]
pub struct MemoryProjectResolver {
    list_project_paths: ListProjectPaths,
    resolve_primary_worktree_root: ResolvePrimaryWorktreeRoot,
    managed_roots: Vec<String>,
}

impl MemoryProjectResolver {
    pub fn new(
        list_project_paths: ListProjectPaths,
        resolve_primary_worktree_root: ResolvePrimaryWorktreeRoot,
        managed_project_roots: Vec<String>,
    ) -> Self {
        MemoryProjectResolver {
            list_project_paths,
            resolve_primary_worktree_root,
            managed_roots: managed_project_roots
                .iter()
                .map(|root| normalize(Some(root.as_str())))
                .filter(|root| !root.is_empty())
                .collect(),
        }
    }

    /// Maps a session directory onto the project whose memory it belongs to.
    /// No directory resolves to `""` rather than to some default project.
    pub async fn resolve(&self, directory: Option<&str>) -> String {
        let resolved = normalize(directory);
        if resolved.is_empty() {
            return String::new();
        }

        if let Some(managed_root) = self
            .managed_roots
            .iter()
            .find(|root| is_within(root, &resolved))
        {
            return create_project_id_from_path(managed_root);
        }

        let configured: Vec<String> = match (self.list_project_paths)().await {
            Ok(paths) => paths
                .iter()
                .map(|path| normalize(Some(path)))
                .filter(|path| !path.is_empty())
                .collect(),
            // An unreadable project list must not lose the memory.
            Err(_) => Vec::new(),
        };
        if configured.contains(&resolved) {
            return create_project_id_from_path(&resolved);
        }

        let primary_root = match (self.resolve_primary_worktree_root)(resolved.clone()).await {
            Some(root) => normalize(Some(&root)),
            None => String::new(),
        };

        create_project_id_from_path(if primary_root.is_empty() {
            &resolved
        } else {
            &primary_root
        })
    }
}

/// JS `normalize`: trim, empty stays empty, otherwise `path.resolve`.
fn normalize(value: Option<&str>) -> String {
    let trimmed = value.unwrap_or("").trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        path_resolve(trimmed).to_string_lossy().into_owned()
    }
}

/// JS containment check: `relative === '' || (!startsWith('..') && !isAbsolute)`.
fn is_within(root: &str, candidate: &str) -> bool {
    let relative = path_relative(root, candidate);
    relative.is_empty() || (!relative.starts_with("..") && !Path::new(&relative).is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROJECT: &str = "/Users/x/projects/openchamber";

    struct Overrides {
        list_project_paths: ListProjectPaths,
        resolve_primary_worktree_root: ResolvePrimaryWorktreeRoot,
        managed_project_roots: Vec<String>,
    }

    fn default_overrides() -> Overrides {
        Overrides {
            list_project_paths: Arc::new(|| Box::pin(async { Ok(vec![PROJECT.to_string()]) })),
            resolve_primary_worktree_root: Arc::new(|directory: String| {
                Box::pin(async move {
                    // The resolver hands us platform-normalized directories
                    // (`\Users\...` on Windows), so compare in that form —
                    // path.resolve is the identity on POSIX.
                    let normalize = |value: &str| {
                        crate::settings::normalization::path_resolve(value)
                            .to_string_lossy()
                            .into_owned()
                    };
                    Some(if directory == normalize(WORKTREE) {
                        normalize(PROJECT)
                    } else {
                        directory
                    })
                })
            }),
            managed_project_roots: Vec::new(),
        }
    }

    fn resolver_with(overrides: Overrides) -> MemoryProjectResolver {
        MemoryProjectResolver::new(
            overrides.list_project_paths,
            overrides.resolve_primary_worktree_root,
            overrides.managed_project_roots,
        )
    }

    const WORKTREE: &str = "/Users/x/.local/share/opencode/worktree/abc/jammy-koala";

    #[tokio::test]
    async fn a_worktree_resolves_to_the_project_it_belongs_to() {
        let resolve = resolver_with(default_overrides());
        // The bug this exists for: keyed by its own path, a worktree wrote
        // memory into a project the panel never reads.
        assert_eq!(
            resolve.resolve(Some(WORKTREE)).await,
            create_project_id_from_path(PROJECT)
        );
    }

    #[tokio::test]
    async fn the_project_directory_resolves_to_itself() {
        let resolve = resolver_with(default_overrides());
        assert_eq!(
            resolve.resolve(Some(PROJECT)).await,
            create_project_id_from_path(PROJECT)
        );
    }

    #[tokio::test]
    async fn every_worktree_of_one_repository_shares_a_store() {
        let second = "/Users/x/.local/share/opencode/worktree/abc/other";
        let resolve = resolver_with(Overrides {
            resolve_primary_worktree_root: Arc::new(|_| {
                Box::pin(async { Some(PROJECT.to_string()) })
            }),
            ..default_overrides()
        });

        assert_eq!(
            resolve.resolve(Some(WORKTREE)).await,
            resolve.resolve(Some(second)).await
        );
    }

    #[tokio::test]
    async fn a_worktree_registered_as_a_project_keeps_its_own_store() {
        // The user's explicit choice wins over the git topology.
        let resolve = resolver_with(Overrides {
            list_project_paths: Arc::new(|| {
                Box::pin(async { Ok(vec![PROJECT.to_string(), WORKTREE.to_string()]) })
            }),
            ..default_overrides()
        });

        assert_eq!(
            resolve.resolve(Some(WORKTREE)).await,
            create_project_id_from_path(WORKTREE)
        );
    }

    #[tokio::test]
    async fn a_directory_outside_any_repository_keys_by_itself() {
        let resolve = resolver_with(default_overrides());
        assert_eq!(
            resolve.resolve(Some("/tmp/loose")).await,
            create_project_id_from_path("/tmp/loose")
        );
    }

    #[tokio::test]
    async fn managed_chat_session_directories_share_the_chats_root_store() {
        let chats_root = "/Users/x/.config/ompchamber/chats";
        let resolve = resolver_with(Overrides {
            managed_project_roots: vec![chats_root.to_string()],
            ..default_overrides()
        });

        assert_eq!(
            resolve
                .resolve(Some(&format!("{chats_root}/2026-08-21/session-a")))
                .await,
            create_project_id_from_path(chats_root)
        );
        assert_eq!(
            resolve
                .resolve(Some(&format!("{chats_root}/2026-08-21/session-b")))
                .await,
            create_project_id_from_path(chats_root)
        );
        assert_ne!(
            resolve
                .resolve(Some("/Users/x/.config/ompchamber/chats-other/session-a"))
                .await,
            create_project_id_from_path(chats_root)
        );
    }

    #[tokio::test]
    async fn no_directory_resolves_to_nothing_rather_than_to_some_default_project() {
        let resolve = resolver_with(default_overrides());
        assert_eq!(resolve.resolve(Some("")).await, "");
        assert_eq!(resolve.resolve(None).await, "");
        assert_eq!(resolve.resolve(Some("   ")).await, "");
    }

    #[tokio::test]
    async fn trailing_slashes_and_relative_segments_do_not_fork_the_store() {
        let resolve = resolver_with(default_overrides());
        assert_eq!(
            resolve.resolve(Some(&format!("{PROJECT}/"))).await,
            create_project_id_from_path(PROJECT)
        );
        assert_eq!(
            resolve
                .resolve(Some(&format!("{PROJECT}/packages/..")))
                .await,
            create_project_id_from_path(PROJECT)
        );
    }

    #[tokio::test]
    async fn an_unreadable_project_list_still_converges_worktrees_on_the_repository() {
        let resolve = resolver_with(Overrides {
            list_project_paths: Arc::new(|| {
                Box::pin(async { Err(anyhow::anyhow!("settings unreadable")) })
            }),
            ..default_overrides()
        });

        assert_eq!(
            resolve.resolve(Some(WORKTREE)).await,
            create_project_id_from_path(PROJECT)
        );
    }

    #[tokio::test]
    async fn git_being_unavailable_falls_back_to_the_directory_instead_of_failing() {
        let resolve = resolver_with(Overrides {
            resolve_primary_worktree_root: Arc::new(|_| Box::pin(async { None })),
            ..default_overrides()
        });

        assert_eq!(
            resolve.resolve(Some(WORKTREE)).await,
            create_project_id_from_path(WORKTREE)
        );
    }
}
