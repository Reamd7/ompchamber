//! Nested git repository discovery — port of routes.js `findGitDirectories`
//! (depth- and visit-capped readdir walk; `.git` directory, file, or symlink
//! marks a repository boundary; junk directories and symlinks are never
//! descended into).

use std::path::{Path, PathBuf};

pub const GIT_DIRS_MAX_DEPTH: usize = 3;
pub const GIT_DIRS_MAX_DIRS: usize = 100;
const GIT_DIRS_SKIP_LIST: [&str; 6] = ["node_modules", "dist", "build", ".venv", "target", ".next"];

/// Walks `root_path` and returns every nested git repository path. The root
/// itself, when it is a repo, yields no results; unreadable subtrees are
/// silently skipped while an unreadable root still errors.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fsport-gitdirs-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

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
