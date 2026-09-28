//! Nested git repository discovery — port of routes.js `findGitDirectories`
//! (depth- and visit-capped readdir walk; `.git` directory, file, or symlink
//! marks a repository boundary; junk directories and symlinks are never
//! descended into).
//!
//! 中文说明：嵌套 git 仓库发现——`/api/fs/git-dirs` 路由的底层实现。
//! 以深度上限 3、访问目录数上限 100 做受控 BFS：`.git`（目录、文件或
//! symlink，worktree/file 均算）标记仓库边界并停止下钻；跳过名单目录
//! 与 symlink 目录保证遍历安全且有界。

use std::path::{Path, PathBuf};

/// 遍历的最大深度（根为 0；深度 ≥ 3 的目录不再下钻）。
pub const GIT_DIRS_MAX_DEPTH: usize = 3;
/// 最多访问的目录总数（防大仓库失控）。
pub const GIT_DIRS_MAX_DIRS: usize = 100;
/// 永不下钻的目录名清单（构建产物与依赖目录）。
const GIT_DIRS_SKIP_LIST: [&str; 6] = ["node_modules", "dist", "build", ".venv", "target", ".next"];

/// Walks `root_path` and returns every nested git repository path. The root
/// itself, when it is a repo, yields no results; unreadable subtrees are
/// silently skipped while an unreadable root still errors.
///
/// 中文说明：遍历 `root_path` 返回全部嵌套仓库路径。根自身是仓库时
/// 不产出任何结果且立即停止；子目录不可读则静默跳过，根不可读才
/// 向上返回错误。
pub fn find_git_directories(root_path: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut results: Vec<PathBuf> = Vec::new();
    let mut visited: usize = 0;
    walk(
        root_path,
        root_path,
        0,
        GIT_DIRS_MAX_DEPTH,
        GIT_DIRS_MAX_DIRS,
        &mut visited,
        &mut results,
    )?;
    Ok(results)
}

/// 递归遍历实现：读取一层目录、识别仓库边界、按字典序下钻子目录。
///
/// `visited` 在进入目录体时计数（读取失败的目录不计），达到
/// `max_dirs` 后不再展开新目录。
fn walk(
    root_path: &Path,
    dir: &Path,
    depth: usize,
    max_depth: usize,
    max_dirs: usize,
    visited: &mut usize,
    results: &mut Vec<PathBuf>,
) -> std::io::Result<()> {
    if *visited >= max_dirs {
        return Ok(());
    }

    let dirents = match std::fs::read_dir(dir) {
        Ok(dirents) => dirents,
        // Unreadable subtree — skip it unless it is the root itself, which
        // the route maps to 403/404/500 through the shared error handling.
        Err(_) if dir != root_path => return Ok(()),
        Err(error) => return Err(error),
    };

    let mut is_repo_boundary = false;
    let mut subdirectories: Vec<String> = Vec::new();
    for entry in dirents {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            is_repo_boundary = true;
            continue;
        }
        let Ok(file_type) = entry
            .file_type()
            .or_else(|_| std::fs::symlink_metadata(entry.path()).map(|m| m.file_type()))
        else {
            continue;
        };
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        if GIT_DIRS_SKIP_LIST.contains(&name.as_str()) {
            continue;
        }
        if depth >= max_depth {
            continue;
        }
        subdirectories.push(name);
    }
    *visited += 1;

    if is_repo_boundary {
        if dir != root_path {
            results.push(dir.to_path_buf());
        }
        return Ok(());
    }

    subdirectories.sort();
    for name in subdirectories {
        if *visited >= max_dirs {
            break;
        }
        walk(
            root_path,
            &dir.join(name),
            depth + 1,
            max_depth,
            max_dirs,
            visited,
            results,
        )?;
    }
    Ok(())
}

/// git 目录发现的行为测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 创建带进程号隔离的临时目录，避免测试间相互污染。
    fn unique_temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fsport-gitdirs-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// 验证发现嵌套仓库、在仓库边界停止下钻（边界内的嵌套仓库不上报）。
    #[test]
    fn finds_nested_repos_and_stops_at_boundaries() {
        let root = unique_temp_dir("walk");
        std::fs::create_dir_all(root.join("outer/inner")).unwrap();
        std::fs::create_dir_all(root.join("real/.git")).unwrap();
        std::fs::create_dir(root.join("outer/.git")).unwrap();

        let repos = find_git_directories(&root).unwrap();
        let rendered: Vec<String> = repos
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(
            rendered.contains(&root.join("real").to_string_lossy().into_owned()),
            "{rendered:?}"
        );
        assert!(
            rendered.contains(&root.join("outer").to_string_lossy().into_owned()),
            "{rendered:?}"
        );
        // Nested repo inside a repo boundary is not reported.
        assert!(!rendered.contains(&root.join("outer/inner").to_string_lossy().into_owned()));
        std::fs::remove_dir_all(&root).ok();
    }

    /// 验证跳过名单与深度上限：node_modules 不下钻，深度 4 的仓库被截断。
    #[test]
    fn skip_list_and_depth_caps_are_enforced() {
        let root = unique_temp_dir("caps");
        // Junk directories are never descended into.
        std::fs::create_dir_all(root.join("node_modules/dep/.git")).unwrap();
        // a/b/c is depth 3 → still walked; a/b/c/d/.git is depth 4 → skipped.
        std::fs::create_dir_all(root.join("a/b/c/.git")).unwrap();
        std::fs::create_dir_all(root.join("a/b/c/d/.git")).unwrap();

        let repos = find_git_directories(&root).unwrap();
        let rendered: Vec<String> = repos
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            rendered,
            vec![
                root.join("a")
                    .join("b")
                    .join("c")
                    .to_string_lossy()
                    .into_owned()
            ]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 验证根自身是仓库时返回空列表并终止遍历（JS 只 readdir 一次）。
    #[test]
    fn root_repo_yields_no_results_and_stops_the_walk() {
        let root = unique_temp_dir("rootrepo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("proj-a/.git")).unwrap();
        // A repository boundary at the root stops descent entirely — nested
        // repos inside the root repo are not reported (JS: readdir once).
        assert_eq!(find_git_directories(&root).unwrap(), Vec::<PathBuf>::new());
        std::fs::remove_dir_all(&root).ok();
    }
}
