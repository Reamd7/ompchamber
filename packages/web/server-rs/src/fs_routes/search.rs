//! Fuzzy filesystem search — port of `server/lib/fs/search.js`
//! (`createFsSearchRuntime.searchFilesystemFiles`). Used by non-fs routes
//! (project icon discovery); exposed here as a library function with the JS
//! scoring/ordering semantics intact.
//!
//! 中文说明：模糊文件系统搜索运行时。BFS 按批次（并发 5）遍历目录，
//! 跳过隐藏目录与排除清单（不区分大小写），可选地用
//! `git check-ignore` 过滤被忽略的条目；候选按"子串优先、否则子序列"
//! 的模糊评分排序。语义（评分、排序、limit 行为）与 JS 版逐一对齐。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::workspace::git_binary;

/// 每批并发读取的目录数上限（JS `queue.splice(0, 5)` 的批大小）。
const FILE_SEARCH_MAX_CONCURRENCY: usize = 5;
/// 永不进入的目录名清单（匹配时不区分大小写）。
const FILE_SEARCH_EXCLUDED_DIRS: [&str; 10] = [
    "node_modules",
    ".git",
    "dist",
    "build",
    ".next",
    ".turbo",
    ".cache",
    "coverage",
    "tmp",
    "logs",
];

/// 搜索参数；对应 JS `searchFilesystemFiles` 的 options 对象。
#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    /// 返回结果的最大条数；0 表示直接返回空结果。
    pub limit: usize,
    /// 搜索词（内部会 trim 并转小写；空串表示罗列全部文件）。
    pub query: String,
    /// 是否包含点号开头的隐藏文件/目录。
    pub include_hidden: bool,
    /// Defaults to true in the JS (`respectGitignore !== false`).
    ///
    /// 中文说明：JS 中默认为 true（`respectGitignore !== false`），
    /// 置 false 可跳过 `git check-ignore` 子进程调用。
    pub respect_gitignore: bool,
}

/// 单条搜索命中，序列化形状与 JS 返回对象一致（camelCase，
/// extension 缺省时省略，score 不下发）。
#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    /// 文件名（含扩展名）。
    pub name: String,
    /// 绝对路径字符串。
    pub path: String,
    /// 相对搜索根的路径（以 `/` 分隔）。
    #[serde(rename = "relativePath")]
    pub relative_path: String,
    /// None when the file name has no extension (JS `extension: undefined`).
    ///
    /// 中文说明：文件名不含点号时无扩展名，序列化时整个字段省略
    /// （对齐 JS `extension: undefined`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extension: Option<String>,
    /// 模糊匹配得分，仅用于排序，不随 JSON 下发。
    #[serde(skip_serializing)]
    pub score: i64,
}

/// 目录项的轻量描述（名称 + 类型标志），供遍历与 gitignore 过滤复用。
struct WalkDirent {
    /// 目录项名称。
    name: String,
    /// 是否为目录。
    is_dir: bool,
    /// 是否为普通文件。
    is_file: bool,
    /// 是否为符号链接。
    is_symlink: bool,
}

/// search.js `normalizeRelativeSearchPath`.
///
/// 中文说明：计算 `target_path` 相对 `root_path` 的展示路径：先各自
/// 词法 resolve，再求相对路径；目标恰为根时取 basename；统一把平台
/// 分隔符替换为 `/`。
fn normalize_relative_search_path(root_path: &Path, target_path: &Path) -> String {
    let root_text = root_path.to_string_lossy();
    let target_text = target_path.to_string_lossy();
    let root = super::paths::resolve_path(&root_text);
    let target = super::paths::resolve_path(&target_text);
    let mut relative = super::paths::lexical_relative(&root, &target);
    if relative.is_empty() {
        relative = super::paths::basename(&target);
    }
    relative.replace(std::path::MAIN_SEPARATOR, "/")
}

/// search.js `shouldSkipSearchDirectory`.
///
/// 中文说明：目录是否应被跳过——空名不跳过；未开启隐藏包含时点号
/// 开头的目录跳过；命中排除清单（小写比较）恒跳过。
fn should_skip_search_directory(name: &str, include_hidden: bool) -> bool {
    if name.is_empty() {
        return false;
    }
    if !include_hidden && name.starts_with('.') {
        return true;
    }
    FILE_SEARCH_EXCLUDED_DIRS.contains(&name.to_lowercase().as_str())
}

/// search.js `listDirectoryEntries` — unreadable directories read as empty.
///
/// 中文说明：同步读取一层目录并降级为 `WalkDirent` 列表；目录不可读
/// 或单个条目获取类型失败时按不存在处理（条目被静默丢弃）。
fn list_directory_entries(dir_path: &Path) -> Vec<WalkDirent> {
    let Ok(entries) = std::fs::read_dir(dir_path) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let file_type = entry
                .file_type()
                .or_else(|_| std::fs::symlink_metadata(entry.path()).map(|m| m.file_type()))
                .ok()?;
            Some(WalkDirent {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir: file_type.is_dir(),
                is_file: file_type.is_file(),
                is_symlink: file_type.is_symlink(),
            })
        })
        .collect()
}

/// search.js `fuzzyMatchScoreNormalized` — substring fast path plus a
/// subsequence scorer; None means "no match".
///
/// 中文说明：模糊评分（大小写不敏感）。查询为空串时恒得 0 分；
/// 候选包含完整子串时走快路径（100 基分 + 开头/分隔符加成 − 位置
/// 与长度惩罚）；否则按子序列逐字符计分（连续、靠前、分隔符后
/// 均有加成），任一字符缺失即返回 None（不匹配）。
pub fn fuzzy_match_score_normalized(normalized_query: &str, candidate: &str) -> Option<i64> {
    if normalized_query.is_empty() {
        return Some(0);
    }
    let query: Vec<char> = normalized_query.chars().collect();
    let candidate_lower: Vec<char> = candidate.to_lowercase().chars().collect();

    let find_from = |start: usize, needle: &[char]| -> Option<usize> {
        if needle.is_empty() || start > candidate_lower.len() {
            return None;
        }
        candidate_lower[start..]
            .windows(needle.len().max(1))
            .position(|window| window == needle)
            .map(|offset| start + offset)
    };

    if let Some(index) = find_from(0, &query) {
        let bonus = if index == 0 {
            20
        } else {
            let previous = candidate_lower[index - 1];
            if matches!(previous, '/' | '_' | '-' | '.' | ' ') {
                15
            } else {
                0
            }
        };
        return Some(100 + bonus - index.min(20) as i64 - (candidate_lower.len() / 5) as i64);
    }

    let mut score: i64 = 0;
    let mut last_index: i64 = -1;
    let mut consecutive: i64 = 0;

    for &ch in &query {
        if ch == ' ' {
            continue;
        }

        let search_from = if last_index < 0 {
            0
        } else {
            (last_index + 1) as usize
        };
        let Some(index) = candidate_lower.get(search_from..).and_then(|tail| {
            tail.iter()
                .position(|&c| c == ch)
                .map(|offset| search_from + offset)
        }) else {
            return None;
        };

        let gap = (index as i64) - last_index - 1;
        consecutive = if gap == 0 { consecutive + 1 } else { 0 };

        score += 10;
        score += 0i64.max(18 - index as i64);
        score -= gap.min(10);

        if index == 0 {
            score += 12;
        } else {
            let previous = candidate_lower[index - 1];
            if matches!(previous, '/' | '_' | '-' | '.' | ' ') {
                score += 10;
            }
        }
        if consecutive > 0 {
            score += 12;
        }
        last_index = index as i64;
    }

    score += 0i64.max(24 - (candidate_lower.len() / 3) as i64);
    Some(score)
}

/// search.js `searchFilesystemFiles`. `limit: 0` yields an empty result (the
/// JS collect-limit comparison is false from the start).
///
/// 中文说明：BFS 搜索 `root_path` 下的文件。空查询罗列全部文件
/// （score 全 0）；非空查询收集上限为 limit×3（至少 200）以便排序后
/// 截断仍够数。目录访问不可读即跳过；`respect_gitignore` 开启时每批
/// 目录用一次 `git check-ignore` 过滤。最终按 score 降序、相对路径
/// 长度、字典序排序并截断到 limit。
pub async fn search_filesystem_files(root_path: &Path, options: &SearchOptions) -> Vec<SearchHit> {
    let include_hidden_entries = options.include_hidden;
    let normalized_query = options.query.trim().to_lowercase();
    let match_all = normalized_query.is_empty();
    let should_respect_gitignore = options.respect_gitignore;
    let collect_limit = if match_all {
        options.limit
    } else {
        (options.limit.saturating_mul(3)).max(200)
    };

    let root_path = super::paths::resolve_path(&root_path.to_string_lossy());
    let mut queue: Vec<PathBuf> = vec![root_path.clone()];
    let mut visited: HashSet<PathBuf> = HashSet::from([root_path.clone()]);
    let mut candidates: Vec<SearchHit> = Vec::new();

    while !queue.is_empty() && candidates.len() < collect_limit {
        // JS `queue.splice(0, FILE_SEARCH_MAX_CONCURRENCY)` — take the first
        // batch, keep the rest queued.
        let take = queue.len().min(FILE_SEARCH_MAX_CONCURRENCY);
        let batch: Vec<PathBuf> = queue.drain(..take).collect();

        let dir_results = futures::future::join_all(batch.into_iter().map(|dir| async move {
            let dirents = list_directory_entries(&dir);
            if !should_respect_gitignore {
                return (dir, dirents, HashSet::new());
            }
            let ignored = run_git_check_ignore(&dir, &dirents).await;
            (dir, dirents, ignored)
        }))
        .await;

        'outer: for (current_dir, dirents, ignored_paths) in dir_results {
            for dirent in dirents {
                let entry_name = dirent.name;
                if entry_name.is_empty() || (!include_hidden_entries && entry_name.starts_with('.'))
                {
                    continue;
                }
                if should_respect_gitignore && ignored_paths.contains(&entry_name) {
                    continue;
                }

                let entry_path = current_dir.join(&entry_name);

                if dirent.is_dir {
                    if should_skip_search_directory(&entry_name, include_hidden_entries) {
                        continue;
                    }
                    if visited.insert(entry_path.clone()) {
                        queue.push(entry_path);
                    }
                    continue;
                }
                if !dirent.is_file {
                    continue;
                }

                let relative_path = normalize_relative_search_path(&root_path, &entry_path);
                let extension = if entry_name.contains('.') {
                    entry_name.rsplit('.').next().map(|e| e.to_lowercase())
                } else {
                    None
                };

                if match_all {
                    candidates.push(SearchHit {
                        name: entry_name,
                        path: entry_path.to_string_lossy().into_owned(),
                        relative_path,
                        extension,
                        score: 0,
                    });
                } else if let Some(score) =
                    fuzzy_match_score_normalized(&normalized_query, &relative_path)
                {
                    candidates.push(SearchHit {
                        name: entry_name,
                        path: entry_path.to_string_lossy().into_owned(),
                        relative_path,
                        extension,
                        score,
                    });
                }

                if candidates.len() >= collect_limit {
                    queue.clear();
                    break 'outer;
                }
            }
        }
    }

    if !match_all {
        candidates.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.relative_path.len().cmp(&b.relative_path.len()))
                .then_with(|| a.relative_path.cmp(&b.relative_path))
        });
    }

    candidates.truncate(options.limit);
    candidates
}

/// `git check-ignore -- <names>` in `dir`; failures read as "nothing
/// ignored" (search.js resolves '' on spawn error or close).
///
/// 中文说明：在 `dir` 中运行 `git check-ignore -- <names>`，返回被
/// 忽略的条目名集合；spawn 失败或非零退出（含"无忽略项"的退出码 1）
/// 一律读作空集合，不阻塞搜索。
async fn run_git_check_ignore(dir: &Path, dirents: &[WalkDirent]) -> HashSet<String> {
    let names: Vec<&str> = dirents
        .iter()
        .map(|d| d.name.as_str())
        .filter(|n| !n.is_empty())
        .collect();
    if names.is_empty() {
        return HashSet::new();
    }
    let mut command = tokio::process::Command::new(git_binary());
    command
        .arg("check-ignore")
        .arg("--")
        .args(&names)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let Ok(output) = command.output().await else {
        return HashSet::new();
    };
    if !output.status.success() {
        // Exit code 1 = nothing ignored; anything else also reads as empty.
        return HashSet::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// 搜索运行时的评分、跳过规则与端到端行为测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 验证子串快路径的位置/分隔符加成与子序列回退的评分数值。
    #[test]
    fn substring_match_scores_with_positional_bonus() {
        // "readme.md" is 9 chars: floor(9/5) = 1.
        // Direct substring at the start: 100 + 20 - 0 - 1 = 119.
        assert_eq!(fuzzy_match_score_normalized("read", "readme.md"), Some(119));
        // Substring at index 4 behind 'd' (no separator bonus): 100 - 4 - 1 = 95.
        assert_eq!(fuzzy_match_score_normalized("me", "readme.md"), Some(95));
        // Substring right after '/' in a 12-char candidate: 100 + 15 - 4 - 2.
        assert_eq!(
            fuzzy_match_score_normalized("logo", "src/logo.png"),
            Some(109)
        );
        // No subsequence match at all.
        assert!(fuzzy_match_score_normalized("zzz", "readme.md").is_none());
        // Subsequence match without substring.
        assert!(fuzzy_match_score_normalized("rmd", "readme.md").is_some());
        // Empty query scores 0 for every candidate.
        assert_eq!(fuzzy_match_score_normalized("", "anything"), Some(0));
    }

    /// 验证目录跳过规则与 JS 一致：排除清单大小写不敏感、隐藏目录
    /// 仅在未开启包含时跳过。
    #[test]
    fn skip_directory_rules_match_js() {
        assert!(should_skip_search_directory("node_modules", true));
        assert!(should_skip_search_directory("Node_Modules", true));
        assert!(should_skip_search_directory(".git", false));
        assert!(
            should_skip_search_directory(".git", true),
            "excluded even when hidden is included"
        );
        assert!(!should_skip_search_directory("src", false));
        assert!(!should_skip_search_directory("", false));
    }

    /// 创建带进程号隔离的临时目录，避免测试间相互污染。
    fn unique_temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fsport-search-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// 写入文件（自动创建父目录）的测试辅助。
    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    /// 验证搜索命中匹配文件、跳过 node_modules 与隐藏文件，
    /// 且 extension/relative_path 字段形状正确。
    #[tokio::test]
    async fn search_finds_matches_and_misses_excluded_directories() {
        let root = unique_temp_dir("find");
        write(&root.join("src/app_controller.rs"), "// controller");
        write(&root.join("docs/appendix.md"), "appendix");
        write(&root.join("node_modules/app_controller.js"), "// hidden");
        write(&root.join("docs/.hidden_app.md"), "hidden");

        let hits = search_filesystem_files(
            &root,
            &SearchOptions {
                limit: 10,
                query: "appcontroller".to_string(),
                include_hidden: false,
                respect_gitignore: false,
            },
        )
        .await;

        let names: Vec<&str> = hits.iter().map(|h| h.name.as_str()).collect();
        assert!(names.contains(&"app_controller.rs"), "{names:?}");
        assert!(
            !names.contains(&"app_controller.js"),
            "node_modules must be skipped"
        );
        assert!(
            !names.contains(&".hidden_app.md"),
            "hidden entries excluded by default"
        );

        let controller = hits.iter().find(|h| h.name == "app_controller.rs").unwrap();
        assert_eq!(controller.extension.as_deref(), Some("rs"));
        assert_eq!(controller.relative_path, format!("src/app_controller.rs"));
        std::fs::remove_dir_all(&root).ok();
    }

    /// 验证排序契约：分隔符加成与更短路径让 `a/logo.png` 排在
    /// `zz_logo.png` 之前。
    #[tokio::test]
    async fn search_orders_by_score_then_path_length() {
        let root = unique_temp_dir("order");
        write(&root.join("zz_logo.png"), "b");
        write(&root.join("a/logo.png"), "b");

        let hits = search_filesystem_files(
            &root,
            &SearchOptions {
                limit: 2,
                query: "logo".to_string(),
                include_hidden: false,
                respect_gitignore: false,
            },
        )
        .await;
        // "a/logo.png" wins on the separator bonus and shorter path.
        assert_eq!(hits[0].relative_path, "a/logo.png");
        assert_eq!(hits.len(), 2);
        std::fs::remove_dir_all(&root).ok();
    }

    /// 验证空查询（trim 后为空）罗列全部文件直到 limit，且 score 全 0。
    #[tokio::test]
    async fn match_all_lists_every_file_up_to_limit() {
        let root = unique_temp_dir("all");
        write(&root.join("one.txt"), "1");
        write(&root.join("two.txt"), "2");
        write(&root.join("three.txt"), "3");

        let hits = search_filesystem_files(
            &root,
            &SearchOptions {
                limit: 2,
                query: "   ".to_string(), // trims to empty → match all
                include_hidden: false,
                respect_gitignore: false,
            },
        )
        .await;
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|hit| hit.score == 0));
        std::fs::remove_dir_all(&root).ok();
    }
}
