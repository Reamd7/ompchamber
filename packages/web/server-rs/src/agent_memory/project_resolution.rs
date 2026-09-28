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
//!
//! 中文说明：会话目录到项目记忆归属的解析（对应 JS 端
//! project-resolution.js）。会话常运行在 worktree 中，其路径不是项目
//! 路径；若按会话目录本身建键，worktree 的记忆会归档到面板永不读取的
//! “项目”下。本模块保证同一仓库的所有 worktree 共享一份项目记忆。
//! 解析优先级：目录本身已是配置项目则直接采用（用户可显式把 worktree
//! 注册为项目，该选择优先于 git 拓扑）；否则解析到其主 worktree。

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use crate::projects::create_project_id_from_path;
use crate::settings::normalization::{path_relative, path_resolve};

/// `listProjectPaths` seam: returns configured project paths; may fail (an
/// unreadable project list must not lose the memory — the git-derived root
/// below still converges every worktree of the repository on one store).
/// `listProjectPaths` 接缝的类型：异步返回已配置项目路径；允许失败——
/// 项目清单读不出来时记忆也不能丢，下方 git 推导仍会把同一仓库的
/// worktree 收敛到同一存储。
pub type ListProjectPaths = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<String>>> + Send>> + Send + Sync,
>;

/// `resolvePrimaryWorktreeRoot` seam: returns the primary checkout root, or
/// `None` when git is unavailable / the directory is not a checkout (the JS
/// catch and the null return converge on the same fallback).
/// `resolvePrimaryWorktreeRoot` 接缝的类型：输入目录，返回其主
/// checkout 根；git 不可用或目录不是 checkout 时返回 `None`（JS 的
/// catch 与 null 返回最终落到同一回退路径）。
pub type ResolvePrimaryWorktreeRoot =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = Option<String>> + Send>> + Send + Sync>;

/// 把会话目录解析为项目记忆键的解析器：托管根、已配置项目、git 主
/// worktree 三个来源按序判定（见 [`MemoryProjectResolver::resolve`]）。
#[derive(Clone)]
pub struct MemoryProjectResolver {
    /// 已配置项目路径清单的读取接缝。
    list_project_paths: ListProjectPaths,
    /// 目录到 git 主 worktree 根的解析接缝。
    resolve_primary_worktree_root: ResolvePrimaryWorktreeRoot,
    /// 托管会话根（如 `<data_dir>/chats`）：其下所有会话目录共享根的存储。
    managed_roots: Vec<String>,
}

/// 解析器构造与主入口 `resolve`。
impl MemoryProjectResolver {
    /// 构造解析器；托管根先经 trim 与 `path.resolve` 归一化并丢弃空串，
    /// 保证后续包含性比较不受拼写差异影响。
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
    /// 把会话目录映射到所属项目的记忆键（project id）。空输入（None、
    /// 空串、纯空白）返回空串而非某个默认项目。判定顺序：命中托管根→
    /// 按托管根建键；目录是已配置项目→按自身建键；否则取 git 主
    /// worktree 根（拿不到时按目录本身），使同一仓库的所有 worktree
    /// 收敛到同一键。
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
/// JS `normalize` 的等价实现：trim；空串保持空串；否则 `path.resolve`
/// 归一化（消除相对段与尾部斜杠），避免同一目录的不同写法分裂存储。
fn normalize(value: Option<&str>) -> String {
    let trimmed = value.unwrap_or("").trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        path_resolve(trimmed).to_string_lossy().into_owned()
    }
}

/// JS containment check: `relative === '' || (!startsWith('..') && !isAbsolute)`.
/// JS 的包含性判定：candidate 相对 root 的路径为空串（同一目录），或既
/// 不以 `..` 开头也不是绝对路径（未越出 root）时为真。
fn is_within(root: &str, candidate: &str) -> bool {
    let relative = path_relative(root, candidate);
    relative.is_empty() || (!relative.starts_with("..") && !Path::new(&relative).is_absolute())
}

/// 用注入的假项目清单与假 git 解析验证解析优先级与各回退路径，
/// 对应 JS 端 project-resolution 测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用项目主目录路径。
    const PROJECT: &str = "/Users/x/projects/openchamber";

    /// 测试注入体：替换解析器的三个依赖接缝。
    struct Overrides {
        /// 假的已配置项目清单。
        list_project_paths: ListProjectPaths,
        /// 假的 git 主 worktree 解析。
        resolve_primary_worktree_root: ResolvePrimaryWorktreeRoot,
        /// 假的托管根列表。
        managed_project_roots: Vec<String>,
    }

    /// 默认注入：项目清单只含 PROJECT；git 解析把 WORKTREE 映射回
    /// PROJECT、其余目录原样返回；无托管根。
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

    /// 用给定注入体构造被测解析器。
    fn resolver_with(overrides: Overrides) -> MemoryProjectResolver {
        MemoryProjectResolver::new(
            overrides.list_project_paths,
            overrides.resolve_primary_worktree_root,
            overrides.managed_project_roots,
        )
    }

    /// 测试用 worktree 路径（不在项目路径之下）。
    const WORKTREE: &str = "/Users/x/.local/share/opencode/worktree/abc/jammy-koala";

    /// 验证 worktree 会话归属其主项目而非 worktree 自身——本模块修复的 bug。
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

    /// 验证项目主目录解析为它自身。
    #[tokio::test]
    async fn the_project_directory_resolves_to_itself() {
        let resolve = resolver_with(default_overrides());
        assert_eq!(
            resolve.resolve(Some(PROJECT)).await,
            create_project_id_from_path(PROJECT)
        );
    }

    /// 验证同一仓库的两个 worktree 解析到同一键，共享同一存储。
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

    /// 验证用户显式把 worktree 注册为项目时，该选择压过 git 拓扑。
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

    /// 验证不在任何仓库中的目录按自身路径建键。
    #[tokio::test]
    async fn a_directory_outside_any_repository_keys_by_itself() {
        let resolve = resolver_with(default_overrides());
        assert_eq!(
            resolve.resolve(Some("/tmp/loose")).await,
            create_project_id_from_path("/tmp/loose")
        );
    }

    /// 验证托管 chats 根下的各会话目录共享根的项目存储，
    /// 相邻目录（chats-other）不混入。
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

    /// 验证空目录、None 与纯空白都解析为空串，绝不落到默认项目。
    #[tokio::test]
    async fn no_directory_resolves_to_nothing_rather_than_to_some_default_project() {
        let resolve = resolver_with(default_overrides());
        assert_eq!(resolve.resolve(Some("")).await, "");
        assert_eq!(resolve.resolve(None).await, "");
        assert_eq!(resolve.resolve(Some("   ")).await, "");
    }

    /// 验证尾部斜杠与 `..` 相对段经归一化后仍指向同一项目键。
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

    /// 验证项目清单读取失败时，git 解析仍把 worktree 收敛到主项目。
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

    /// 验证 git 不可用（返回 None）时按目录自身建键，而非报错。
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
