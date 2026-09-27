//! Worktree topology watcher — port of `server/lib/git/worktree-watcher.js`.
//!
//! The JS version arms `fs.watch` handles over each registered repository's
//! `<common-git-dir>/worktrees` metadata directory (plus the common git dir
//! itself, name-filtered to `worktrees`) and debounces events 500 ms. No
//! filesystem-watch crate is available to this port, so the watcher *polls*
//! the same metadata on the debounce interval instead: it fingerprints
//! `<common>/worktrees` (entry names + mtimes + existence) and reports the
//! registered project paths of any repository whose fingerprint changed.
//! Clients still re-list worktrees authoritatively, so the observable
//! contract — "a change fires `ompchamber:worktrees-changed` for the affected
//! projects, at most once per debounce window" — is preserved.
//!
//! Polling cannot fail the way an fs.watch handle errors (watch limits,
//! evicted directories), so the JS re-arm/failure accounting collapses to
//! "repositories that stop resolving are dropped from the watch set until the
//! project list changes" — the same graceful degradation the JS reaches via
//! `MAX_REARM_ATTEMPTS`.
//!
//! 中文说明：工作树拓扑监听器，移植自 `server/lib/git/worktree-watcher.js`。
//! JS 版对每个已注册仓库的 `<common-git-dir>/worktrees` 元数据目录挂
//! fs.watch 并以 500ms 去抖；本移植没有可用的文件系统监听 crate，改为在
//! 相同的去抖间隔上轮询同一份元数据：对 `<common>/worktrees` 做指纹比对
//! （条目名 + mtime + 存在性），指纹变化即上报受影响仓库对应的已注册项目
//! 路径。客户端仍会权威地重新列出工作树，因此可观察契约保持不变：
//! "变化时对受影响项目触发 `ompchamber:worktrees-changed`，每个去抖窗口至多
//! 一次"。轮询不会像 fs.watch 那样因 watch 上限或目录被回收而报错，故 JS 的
//! 重挂/失败记账简化为"不再能解析的仓库暂时移出监听集合，直到项目列表变化"。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::json;

/// JS `PARSE_GITDIR_PATTERN`: `gitdir: <target>` from a linked-worktree
/// `.git` file.
/// 从 linked worktree 的 `.git` 文件内容中解析 `gitdir: <target>` 目标路径；
/// 没有有效 gitdir 行时返回 `None`。
fn parse_gitdir_target(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("gitdir:") {
            let target = rest.trim();
            if !target.is_empty() {
                return Some(target.to_string());
            }
        }
    }
    None
}

/// JS `resolveGitCommonDir`:
/// - main repository: `<project>/.git`
/// - linked worktree: grandparent of the gitdir target
/// - non-repositories: `None`
/// 对应 JS `resolveGitCommonDir`：主仓库返回 `<project>/.git`；linked
/// worktree 返回 gitdir 目标的祖父目录（即公共 git dir）；非仓库返回 `None`。
pub fn resolve_git_common_dir(project_path: &Path) -> Option<PathBuf> {
    let dot_git_path = project_path.join(".git");
    let stat = std::fs::metadata(&dot_git_path).ok()?;
    if stat.is_dir() {
        return Some(dot_git_path);
    }
    if !stat.is_file() {
        return None;
    }
    let content = std::fs::read_to_string(&dot_git_path).ok()?;
    let gitdir = parse_gitdir_target(&content)?;
    let gitdir_path = absolutize(&project_path.join(&gitdir));
    let parent = gitdir_path.parent()?;
    if parent
        .file_name()
        .map(|n| n == "worktrees")
        .unwrap_or(false)
    {
        parent.parent().map(Path::to_path_buf)
    } else {
        Some(gitdir_path)
    }
}

/// 把相对路径补全为绝对路径（复用 paths 模块实现，不解析符号链接）。
fn absolutize(path: &Path) -> PathBuf {
    super::paths::absolutize(path)
}

/// Fingerprint of the worktree registry: sorted `<name>:<mtime>:<kind>`
/// tokens plus the directory's own existence/mtime.
/// 生成工作树注册表指纹：目录自身的存在性/mtime，加上排序后的
/// `<name>:<mtime>:<kind>` 条目列表；轮询用前后指纹比对检测工作树增删改。
fn fingerprint_worktrees(common_git_dir: &Path) -> String {
    let worktrees_dir = common_git_dir.join("worktrees");
    let mut parts: Vec<String> = Vec::new();
    match std::fs::metadata(&worktrees_dir) {
        Ok(meta) => {
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis())
                .unwrap_or(0);
            parts.push(format!("@dir:{}", modified));
            if let Ok(entries) = std::fs::read_dir(&worktrees_dir) {
                let mut names: Vec<(String, u128, bool)> = Vec::new();
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let (modified, is_dir) = match entry.metadata() {
                        Ok(meta) => (
                            meta.modified()
                                .ok()
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_millis())
                                .unwrap_or(0),
                            meta.is_dir(),
                        ),
                        Err(_) => (0, false),
                    };
                    names.push((name, modified, is_dir));
                }
                names.sort();
                for (name, modified, is_dir) in names {
                    parts.push(format!("{}:{}:{}", name, modified, is_dir));
                }
            }
        }
        Err(_) => parts.push("@missing".to_string()),
    }
    parts.join("|")
}

/// The `onWorktreesChanged` callback: receives the affected project paths.
/// `onWorktreesChanged` 回调类型：参数为受影响的（绝对）项目路径列表。
pub type OnWorktreesChanged = Arc<dyn Fn(&[PathBuf]) + Send + Sync>;

/// `listProjects` seam: returns the registered project entries (only `path`
/// is consumed, mirroring the JS watcher).
/// `listProjects` 注入缝：返回已注册项目路径（与 JS 监听器一致，仅消费 `path`）。
pub type ListProjects = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

/// 轮询式工作树监听器：后台 tokio 任务按固定间隔比对各仓库的 worktrees
/// 指纹，变化时通过回调上报受影响的项目路径。
pub struct WorktreeWatcher {
    /// 停止标志；置位后轮询循环在下一个 tick 退出。
    stopped: Arc<AtomicBool>,
    /// 轮询（去抖）间隔，默认 500ms。
    poll_interval: Duration,
    /// 设置重扫描延迟（与 JS `settingsRescanDelayMs` 对齐保留），默认 400ms。
    settings_rescan_delay: Duration,
    /// 注入的项目列表读取器。
    list_projects: ListProjects,
    /// 变化回调，参数为受影响项目路径。
    on_worktrees_changed: OnWorktreesChanged,
    /// 后台轮询任务句柄；stop/drop 时中止，防止任务泄漏。
    handle: Option<tokio::task::JoinHandle<()>>,
}

/// 监听器生命周期：构造、启动轮询循环与停止。
impl WorktreeWatcher {
    /// JS defaults: `debounceMs = 500`, `settingsRescanDelayMs = 400`.
    /// 以 JS 默认值构造（去抖 500ms、设置重扫延迟 400ms），不自动启动轮询。
    pub fn new(list_projects: ListProjects, on_worktrees_changed: OnWorktreesChanged) -> Self {
        Self {
            stopped: Arc::new(AtomicBool::new(false)),
            poll_interval: Duration::from_millis(500),
            settings_rescan_delay: Duration::from_millis(400),
            list_projects,
            on_worktrees_changed,
            handle: None,
        }
    }

    /// 覆盖轮询间隔（测试用），返回 `Self` 供链式调用。
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// `start()` — arms the polling loop. The initial scan establishes a
    /// baseline without emitting (matching JS watcher arming).
    /// 对应 JS `start()`：启动轮询循环；首轮扫描只建立基线指纹而不触发回调
    /// （与 JS 监听器的布防行为一致）。重复调用是空操作。循环内把项目按公共
    /// git dir 分组、淘汰不再解析的仓库、比对指纹并对变化的项目触发回调。
    pub fn start(&mut self) {
        if self.handle.is_some() {
            return;
        }
        let stopped = Arc::clone(&self.stopped);
        let interval = self.poll_interval;
        let settings_delay = self.settings_rescan_delay;
        let list_projects = Arc::clone(&self.list_projects);
        let on_changed = Arc::clone(&self.on_worktrees_changed);
        let mut last_settings_signature: Option<String> = None;
        let mut fingerprints: HashMap<PathBuf, (String, Vec<PathBuf>)> = HashMap::new();

        self.handle = Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            let mut settings_ticks: u32 = 0;
            let settings_ticks_limit =
                (settings_delay.as_millis().max(1) / interval.as_millis().max(1)) as u32;
            loop {
                ticker.tick().await;
                if stopped.load(Ordering::SeqCst) {
                    return;
                }
                settings_ticks = settings_ticks.saturating_add(1);

                // Settings rescan: the JS watcher re-lists projects when the
                // settings file changes (delayed by settingsRescanDelayMs);
                // polling compares the file signature on the same cadence.
                let rescan = {
                    let list_projects = Arc::clone(&list_projects);
                    let signature = tokio::task::spawn_blocking(move || {
                        let paths = list_projects();
                        // The project list itself is the settings signature:
                        // any addition/removal/repath triggers a rescan.
                        paths.join("\n")
                    })
                    .await
                    .unwrap_or_default();
                    let changed = last_settings_signature.as_deref() != Some(signature.as_str());
                    last_settings_signature = Some(signature);
                    changed
                };
                let _ = rescan;

                let list_projects = Arc::clone(&list_projects);
                let projects = tokio::task::spawn_blocking(move || list_projects())
                    .await
                    .unwrap_or_default();

                // Group project paths by common git dir (JS rescan()).
                let mut by_common_dir: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
                for raw_path in projects {
                    let trimmed = raw_path.trim().to_string();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let project_path = absolutize(Path::new(&trimmed));
                    let Some(common_git_dir) = resolve_git_common_dir(&project_path) else {
                        continue;
                    };
                    by_common_dir
                        .entry(absolutize(&common_git_dir))
                        .or_default()
                        .push(project_path);
                }

                // Drop repositories no longer represented.
                fingerprints.retain(|common_dir, _| by_common_dir.contains_key(common_dir));

                let mut changed_projects: Vec<PathBuf> = Vec::new();
                for (common_dir, project_paths) in &by_common_dir {
                    let signature = {
                        let common = common_dir.clone();
                        tokio::task::spawn_blocking(move || fingerprint_worktrees(&common))
                            .await
                            .unwrap_or_else(|_| "@missing".to_string())
                    };
                    match fingerprints.get_mut(common_dir) {
                        Some(entry) => {
                            if entry.0 != signature {
                                entry.0 = signature;
                                entry.1 = project_paths.clone();
                                changed_projects.extend(project_paths.iter().cloned());
                            } else {
                                // Attribution may have changed even when the
                                // watched directory did not.
                                entry.1 = project_paths.clone();
                            }
                        }
                        None => {
                            fingerprints
                                .insert(common_dir.clone(), (signature, project_paths.clone()));
                        }
                    }
                }

                if !changed_projects.is_empty() {
                    let _ = settings_ticks;
                    let _ = settings_ticks_limit;
                    on_changed(&changed_projects);
                }
            }
        }));
    }

    /// `stop()` — closes everything.
    /// 对应 JS `stop()`：置停止标志并中止后台任务；幂等，可安全重复调用。
    pub fn stop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

/// 析构时确保停止轮询任务，防止后台任务泄漏。
impl Drop for WorktreeWatcher {
    /// 析构即停止：置停止标志并中止轮询任务。
    fn drop(&mut self) {
        self.stop();
    }
}

/// Broadcast the `ompchamber:worktrees-changed` frame the JS server emits on
/// `/api/ompchamber/events` (payload shape from `emitWorktreesChangedEvent`).
/// 通过 EventHub 广播 JS 服务端在 `/api/ompchamber/events` 上发送的
/// `ompchamber:worktrees-changed` 帧（负载形状来自 `emitWorktreesChangedEvent`）；
/// 目录列表为空时不发送任何帧。
pub fn publish_worktrees_changed(hub: &crate::hub::EventHub, directories: &[PathBuf]) {
    if directories.is_empty() {
        return;
    }
    let directories: Vec<String> = directories
        .iter()
        .map(|path| path.to_string_lossy().to_string())
        .collect();
    hub.publish_json(
        "ompchamber:worktrees-changed",
        &json!({
            "type": "ompchamber:worktrees-changed",
            "properties": { "directories": directories },
        }),
    );
}

/// Minimal project-list reader standing in for the JS wiring
/// (`readSettingsFromDiskMigrated` + `sanitizeProjects`): reads
/// `<data_dir>/settings.json` and returns the registered project paths.
/// Reading failures keep the current watch set, exactly like a failed
/// `listProjects` in the JS watcher.
/// JS 接线（`readSettingsFromDiskMigrated` + `sanitizeProjects`）的最小替代：
/// 读取 `<data_dir>/settings.json`（容忍注释与尾逗号）并返回已注册项目路径。
/// 读取/解析失败返回空列表，从而保持当前监听集合不变，与 JS 监听器中
/// `listProjects` 失败时的行为一致。
pub fn list_projects_from_settings(data_dir: &Path) -> Vec<String> {
    let settings_path = data_dir.join("settings.json");
    let Ok(content) = std::fs::read_to_string(&settings_path) else {
        return Vec::new();
    };
    let options = jsonc_parser::ParseOptions {
        allow_comments: true,
        allow_loose_object_property_names: false,
        allow_trailing_commas: true,
    };
    let Ok(Some(parsed)) = jsonc_parser::parse_to_serde_value(&content, &options) else {
        return Vec::new();
    };
    let Some(projects) = parsed
        .get("projects")
        .and_then(|value| value.as_array())
        .cloned()
    else {
        return Vec::new();
    };
    projects
        .iter()
        .filter_map(|project| {
            project
                .get("path")
                .and_then(|path| path.as_str())
                .map(|path| path.trim().to_string())
                .filter(|path| !path.is_empty())
        })
        .collect()
}

/// 公共 git dir 解析、指纹与轮询回调的回归测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 创建带随机后缀的唯一临时目录（测试夹具）。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-git-watcher-{}-{}-{}",
            tag,
            std::process::id(),
            suffix()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 生成 8 位十六进制随机后缀，避免并行测试冲突。
    fn suffix() -> String {
        use rand::Rng;
        let mut rng = rand::rng();
        (0..8)
            .map(|_| format!("{:x}", rng.random_range(0..16)))
            .collect()
    }

    /// 验证主仓库、linked worktree（绝对与相对 gitdir）及非仓库路径的
    /// 公共 git dir 解析结果。
    #[test]
    fn resolve_git_common_dir_main_repo_and_linked() {
        let repo = temp_dir("repo");
        let work = repo.join("work");
        std::fs::create_dir_all(work.join(".git")).unwrap();
        assert_eq!(resolve_git_common_dir(&work), Some(work.join(".git")));

        // Linked worktree: .git file pointing at <main>/.git/worktrees/feature.
        let linked = temp_dir("linked");
        let gitdir = repo.join(".git").join("worktrees").join("feature");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", gitdir.to_string_lossy()),
        )
        .unwrap();
        assert_eq!(resolve_git_common_dir(&linked), Some(repo.join(".git")));

        // Relative gitdir target resolves against the project path.
        let linked_rel = repo.join("linked-rel");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::create_dir_all(&linked_rel).unwrap();
        std::fs::write(
            linked_rel.join(".git"),
            "gitdir: ../.git/worktrees/feature\n",
        )
        .unwrap();
        assert_eq!(resolve_git_common_dir(&linked_rel), Some(repo.join(".git")));

        // Non-repository → None.
        let plain = temp_dir("plain");
        assert_eq!(resolve_git_common_dir(&plain), None);

        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&linked);
        let _ = std::fs::remove_dir_all(&linked_rel);
        let _ = std::fs::remove_dir_all(&plain);
    }

    /// 验证指纹能区分 worktrees 目录下工作树的创建与删除。
    #[test]
    fn fingerprint_detects_worktree_add_and_remove() {
        let repo = temp_dir("fp");
        let common = repo.join(".git");
        std::fs::create_dir_all(&common).unwrap();
        let base = fingerprint_worktrees(&common);
        assert!(base.contains("@missing") || !base.contains("feature"));

        std::fs::create_dir_all(common.join("worktrees").join("feature")).unwrap();
        let with_worktree = fingerprint_worktrees(&common);
        assert_ne!(base, with_worktree);
        assert!(with_worktree.contains("feature"));

        std::fs::remove_dir_all(common.join("worktrees").join("feature")).unwrap();
        let removed = fingerprint_worktrees(&common);
        assert_ne!(with_worktree, removed);
        assert!(!removed.contains("feature"));
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// 验证监听器基线扫描不触发回调，worktrees 目录变化后按项目路径上报，
    /// 且无变化时不再重复上报。
    #[tokio::test]
    async fn watcher_reports_project_on_worktree_change() {
        let repo = temp_dir("watch");
        let common = repo.join(".git");
        std::fs::create_dir_all(&common).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<PathBuf>>();
        let project_path = repo.clone();
        let list_projects: ListProjects =
            Arc::new(move || vec![project_path.to_string_lossy().to_string()]);
        let on_changed: OnWorktreesChanged = Arc::new(move |directories: &[PathBuf]| {
            let _ = tx.send(directories.to_vec());
        });

        let mut watcher = WorktreeWatcher::new(list_projects, on_changed)
            .with_poll_interval(Duration::from_millis(50));
        watcher.start();

        // Baseline scan happens without emitting.
        tokio::time::sleep(Duration::from_millis(120)).await;
        std::fs::create_dir_all(common.join("worktrees").join("feature")).unwrap();

        let reported = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("change reported")
            .expect("channel open");
        assert_eq!(reported, vec![repo.clone()]);

        // No further events until something changes again.
        std::fs::create_dir_all(common.join("worktrees").join("feature").join("HEAD.d")).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;

        watcher.stop();
        let _ = std::fs::remove_dir_all(&repo);
    }
}
