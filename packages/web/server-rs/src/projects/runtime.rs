//! Port of `createProjectConfigRuntime` from
//! `server/lib/projects/project-config.js`.
//!
//! Per-project task persistence at `<projects-dir>/<projectID>.json`:
//! JSONC-tolerant reads, per-task isolation for broken entries, unknown-field
//! preservation (untouched tasks are re-persisted verbatim; only deliberately
//! replaced tasks are re-serialized), atomic temp+rename writes, and a
//! cross-process `<config>.lock` with stale recovery layered under an
//! in-process per-project write chain.
//!
//! （中文说明）`createProjectConfigRuntime`（`server/lib/projects/project-config.js`）
//! 的移植：把每个项目的定时任务持久化到 `<projects-dir>/<projectID>.json`。
//! 核心契约：JSONC 容错读取；损坏任务条目按单条隔离；未知字段保留（未触碰
//! 的任务按原始 JSON 原样回写，仅被显式替换的任务才重新序列化）；temp+rename
//! 原子写；跨进程 `<config>.lock` 文件锁（含失效恢复）叠加在进程内按项目
//! 串行化的写链之上。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Map, Value};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex as AsyncMutex;

use crate::context::RouterContext;
use crate::error::{AppError, AppResult};

use super::model::{
    NormalizeOptions, PROJECT_CONFIG_VERSION, as_non_empty_string, normalize_state,
    normalize_task_for_storage, serialize_task, serialize_task_state, system_now_ms,
};

// The typed model is re-exported for sibling modules (scheduled-tasks port).
pub use super::model::{Execution, LoopEntry, Schedule, ScheduledTask, TaskState};

/// 抢跨进程锁的最长等待（毫秒），对应 JS 硬编码的 10 秒；超时报 internal 错误。
const PROJECT_FILE_LOCK_WAIT_MS: u64 = 10_000;
/// 锁失效阈值（毫秒），对应 JS 硬编码的 60 秒：锁龄或 payload 时间戳超过它即可窃取。
const PROJECT_FILE_LOCK_STALE_MS: u64 = 60_000;
/// 抢锁失败后的重试间隔（毫秒），对应 JS 硬编码的 20 毫秒。
const PROJECT_FILE_LOCK_RETRY_MS: u64 = 20;
/// `upsertScheduledTask` 的返回：写入后的任务、项目全量任务列表，以及是否为新建。
#[derive(Debug, Clone, Serialize)]
pub struct UpsertResult {
    /// 写入后（已规范化）的任务。
    pub task: ScheduledTask,
    /// 写入后的项目全部任务。
    pub tasks: Vec<ScheduledTask>,
    /// 是否为新建；false 表示更新了已存在的任务。
    pub created: bool,
}
/// `deleteScheduledTask` 的返回：是否真的删除，以及删除后的任务列表。
#[derive(Debug, Clone, Serialize)]
pub struct DeleteResult {
    /// 是否删除了任务；目标不存在时为 false。
    pub deleted: bool,
    /// 删除后的项目全部任务。
    pub tasks: Vec<ScheduledTask>,
}

/// 状态更新（`updateScheduledTaskState` / `..._if`）的返回：涉及的任务、
/// 全量列表，以及是否真正发生了写入。
#[derive(Debug, Clone, Serialize)]
pub struct StateUpdateResult {
    /// 涉及的任务：目标不存在为 None；条件更新的谓词未通过时为更新前的任务。
    pub task: Option<ScheduledTask>,
    /// 项目全部任务（无论是否写入都返回当前列表）。
    pub tasks: Vec<ScheduledTask>,
    /// 是否真正发生了状态写入。
    pub updated: bool,
}

/// Raw on-disk record alongside the normalized view: writers persist
/// untouched tasks from `raw_tasks_by_id` verbatim so fields unknown to this
/// build survive writes (JS `readProjectConfigFromDisk`/`toStoredTasks`).
///
/// 磁盘原始记录与规范化视图的配对：回写时未触碰的任务直接取
/// `raw_tasks_by_id` 的原始 JSON 原样输出，使本构建不认识的字段在写入后
/// 幸存（对应 JS `readProjectConfigFromDisk`/`toStoredTasks`）。
struct RawProjectConfig {
    /// 规范化后的任务列表（读取视图）。
    scheduled_tasks: Vec<ScheduledTask>,
    /// 任务 id → 磁盘上的原始 JSON 记录（回写视图）。
    raw_tasks_by_id: HashMap<String, Value>,
}

/// Port of the object returned by `createProjectConfigRuntime`.
///
/// `createProjectConfigRuntime` 返回对象的移植：内部状态经 `Arc<RuntimeInner>`
/// 共享，Clone 即共享同一个 runtime 实例。
#[derive(Clone)]
pub struct ProjectConfigRuntime {
    /// 共享的内部状态：目录、可注入的 id 工厂与时钟、锁时序参数、写链表。
    inner: Arc<RuntimeInner>,
}

/// runtime 的共享状态：projects 目录、可注入的任务 id 工厂与毫秒时钟、
/// 文件锁时序参数，以及按项目键控的进程内写链（`write_chains`）。
struct RuntimeInner {
    /// `<data-dir>/projects` 目录。
    projects_dir: PathBuf,
    /// 任务 id 工厂（JS `createTaskID` 注入点；默认 `task_<ms>_<random>`）。
    task_id_factory: Arc<dyn Fn() -> String + Send + Sync>,
    /// 毫秒级 epoch 时钟（默认即 `Date.now()` 语义，测试可注入替换）。
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// 抢锁最长等待（毫秒）。
    lock_wait_ms: u64,
    /// 锁失效阈值（毫秒）。
    lock_stale_ms: u64,
    /// 抢锁重试间隔（毫秒）。
    lock_retry_ms: u64,
    /// 项目 id → 进程内写链：同项目的写操作先在此串行化，再去抢跨进程文件锁。
    write_chains: std::sync::Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

/// JS default `createTaskID` fallback (`task_<Date.now()>_<random>`).
///
/// JS 默认 `createTaskID` 兜底实现：`task_<毫秒时间戳>_<8 位随机小写字母数字>`。
fn default_task_id_factory() -> Arc<dyn Fn() -> String + Send + Sync> {
    Arc::new(|| {
        use rand::Rng;
        let suffix: String = rand::rng()
            .sample_iter(rand::distr::Alphanumeric)
            .map(|byte| (byte as char).to_ascii_lowercase())
            .take(8)
            .collect();
        format!("task_{}_{}", system_now_ms(), suffix)
    })
}

/// 校验并清洗项目 id：去空白后必须非空，且仅允许 ASCII 字母数字与
/// `.` `_` `:` `-`；不满足则返回 internal 错误（防止路径穿越到目录之外）。
fn sanitize_project_id(project_id: &str) -> AppResult<String> {
    let value = as_non_empty_string(&Value::String(project_id.to_string()))
        .ok_or_else(|| AppError::internal("projectId is required"))?;
    let supported = value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'));
    if !supported {
        return Err(AppError::internal(
            "projectId contains unsupported characters",
        ));
    }
    Ok(value)
}

/// Tolerant JSONC parse (comments + trailing commas) of one config file.
/// A broken file is an error — never silently treated as empty (a partial
/// parse must not be able to overwrite a full config with a stub).
///
/// 以 JSONC 容错选项（允许注释与尾逗号）解析单个配置文件；非 object 根
/// 降级为空 map（与 JS `parsed && typeof parsed === 'object'` 判定一致）；
/// 文件损坏一律返回 `Err`，绝不静默当作空配置（防止半截解析把完整配置
/// 覆盖成空桩）。
pub(crate) fn parse_project_config_text(text: &str) -> Result<Map<String, Value>, String> {
    let options = jsonc_parser::ParseOptions {
        allow_comments: true,
        allow_loose_object_property_names: false,
        allow_trailing_commas: true,
    };
    match jsonc_parser::parse_to_serde_value(text, &options) {
        // Non-object roots degrade to `{}` exactly like the JS
        // (`parsed && typeof parsed === 'object' && !Array.isArray(parsed)`).
        Ok(Some(Value::Object(map))) => Ok(map),
        Ok(_) => Ok(Map::new()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(unix)]
// Unix 下用于探测进程存活的原生 kill(2)。
unsafe extern "C" {
    /// 向进程发送信号；本模块只用信号 0 做存活探测，返回 0 表示成功。
    fn kill(pid: i32, sig: i32) -> i32;
}

/// `isProcessAlive` via `kill(pid, 0)`; EPERM (another user's live process)
/// counts as alive.
///
/// `isProcessAlive` 的移植：`kill(pid, 0)` 返回 0 视为存活；EPERM（进程
/// 属于其他用户但仍活着）同样视为存活；pid <= 0 视为不存在。
#[cfg(unix)]
fn is_process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // EPERM：进程存在但属于其他用户时 kill 的返回值，视为存活。
    const EPERM: i32 = 1;
    let result = unsafe { kill(pid, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(EPERM)
}

/// 非 Unix 平台无法探测进程存活：恒返回 true，锁失效只能依赖年龄阈值恢复。
#[cfg(not(unix))]
fn is_process_alive(_pid: i32) -> bool {
    // Liveness cannot be probed; rely on age-based staleness instead.
    true
}

/// Owned cross-process lock guard. Release unlinks the lock file only while
/// this pid still owns it, so a stale-recovery steal is never dropped.
///
/// 持有锁文件路径与属主 pid 的跨进程锁守卫；释放时只在锁文件仍属于本
/// pid 的情况下才删除它，避免误删被他人窃取后重建的锁。
pub(crate) struct FileLockGuard {
    /// `<config>.lock` 的路径。
    lock_path: PathBuf,
    /// 创建锁时写入 payload 的属主进程 id。
    pid: i32,
}

/// 锁守卫的释放逻辑。
impl FileLockGuard {
    /// 释放锁：重读锁文件并确认 payload 中的 pid 仍是本进程后才 unlink；
    /// 已被他人窃取（pid 不匹配）或解析失败时保留锁文件原样不动。
    pub(crate) async fn release(self) {
        if let Ok(raw) = tokio::fs::read_to_string(&self.lock_path).await
            && let Ok(parsed) = serde_json::from_str::<Value>(&raw)
        {
            let owner = parsed.get("pid").and_then(Value::as_f64);
            let still_ours = owner.is_some_and(|pid| pid == self.pid as f64);
            if !still_ours {
                return;
            }
            let _ = tokio::fs::remove_file(&self.lock_path).await;
        }
    }
}

/// `ProjectConfigRuntime` 的公开 API 与内部读取/写入/加锁实现。
impl ProjectConfigRuntime {
    /// 以默认 id 工厂、系统毫秒时钟和 JS 硬编码的锁时序（10s/60s/20ms）构建
    /// 指向 `projects_dir` 的 runtime。
    pub fn new(projects_dir: PathBuf) -> Self {
        Self {
            inner: Arc::new(RuntimeInner {
                projects_dir,
                task_id_factory: default_task_id_factory(),
                clock: Arc::new(system_now_ms),
                lock_wait_ms: PROJECT_FILE_LOCK_WAIT_MS,
                lock_stale_ms: PROJECT_FILE_LOCK_STALE_MS,
                lock_retry_ms: PROJECT_FILE_LOCK_RETRY_MS,
                write_chains: std::sync::Mutex::new(HashMap::new()),
            }),
        }
    }

    /// `<data-dir>/projects` — the JS `OMPCHAMBER_PROJECTS_CONFIG_DIR`.
    ///
    /// 便捷构造：使用 `<data-dir>/projects` 目录（对应 JS 的
    /// `OMPCHAMBER_PROJECTS_CONFIG_DIR`）。
    pub fn for_context(ctx: &RouterContext) -> Self {
        Self::new(ctx.config.data_dir.join("projects"))
    }

    /// Test/dependency seam for the JS `createTaskID` injection.
    ///
    /// 测试/依赖注入缝：替换任务 id 工厂（JS `createTaskID` 注入点）；仅在
    /// runtime 尚未被共享（Arc 引用计数为 1）时可调用，否则 panic。
    pub fn with_task_id_factory(mut self, factory: Arc<dyn Fn() -> String + Send + Sync>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("runtime not shared yet")
            .task_id_factory = factory;
        self
    }

    /// Test seam replacing `Date.now()` (JS has no injection point; the
    /// observable contract is a millisecond epoch clock).
    ///
    /// 测试缝：替换毫秒时钟（JS 无注入点；可观察契约是毫秒 epoch 时钟）。
    /// 同样要求 runtime 尚未被共享。
    pub fn with_clock(mut self, clock: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("runtime not shared yet")
            .clock = clock;
        self
    }

    /// Test seam for the lock timings (JS hardcodes 10s/60s/20ms).
    ///
    /// 测试缝：替换锁时序三元组（JS 硬编码 10s 等待 / 60s 失效 / 20ms 重试）。
    pub fn with_lock_timings(mut self, wait_ms: u64, stale_ms: u64, retry_ms: u64) -> Self {
        let inner = Arc::get_mut(&mut self.inner).expect("runtime not shared yet");
        inner.lock_wait_ms = wait_ms;
        inner.lock_stale_ms = stale_ms;
        inner.lock_retry_ms = retry_ms;
        self
    }

    /// `resolveProjectConfigPath`.
    /// `resolveProjectConfigPath`：清洗项目 id 后拼出 `<projects-dir>/<id>.json` 路径。
    pub fn resolve_project_config_path(&self, project_id: &str) -> AppResult<PathBuf> {
        let safe_project_id = sanitize_project_id(project_id)?;
        Ok(self
            .inner
            .projects_dir
            .join(format!("{}.json", safe_project_id)))
    }

    /// 读取注入的毫秒时钟（默认 `Date.now()` 语义）。
    fn now_ms(&self) -> u64 {
        (self.inner.clock)()
    }

    /// `readRawProjectConfigFromDisk`: ENOENT → empty object; a broken file
    /// is an error (isolated per project, never a silent empty overwrite).
    ///
    /// `readRawProjectConfigFromDisk`：文件不存在（ENOENT）返回空 map；文件
    /// 损坏返回错误（按项目隔离，绝不让一次静默的空读触发覆盖式写入）；其余
    /// IO 错误原样上抛。
    async fn read_raw_from_disk(&self, project_id: &str) -> AppResult<Map<String, Value>> {
        let file_path = self.resolve_project_config_path(project_id)?;
        match tokio::fs::read_to_string(&file_path).await {
            Ok(text) => parse_project_config_text(&text).map_err(|error| {
                AppError::internal(format!(
                    "failed to parse project config at {}: {}",
                    file_path.display(),
                    error
                ))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
            Err(error) => Err(AppError::Io(error)),
        }
    }

    /// `readProjectConfigFromDisk`: normalized tasks for reading plus each
    /// one's raw record for writing back. A task that fails normalization is
    /// skipped in isolation — it never blocks valid siblings.
    ///
    /// `readProjectConfigFromDisk`：返回规范化任务（读取视图）加上每条任务的
    /// 原始记录（回写视图）；单条任务规范化失败被隔离跳过，绝不阻塞合法的
    /// 兄弟任务。
    async fn read_config_from_disk(&self, project_id: &str) -> AppResult<RawProjectConfig> {
        let parsed = self.read_raw_from_disk(project_id).await?;
        let tasks_raw = parsed
            .get("scheduledTasks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let now = self.now_ms();
        let factory = self.inner.task_id_factory.clone();
        let mut scheduled_tasks = Vec::with_capacity(tasks_raw.len());
        let mut raw_tasks_by_id = HashMap::new();
        for task_value in tasks_raw {
            let options = NormalizeOptions {
                now,
                default_now: now,
                create_id: &*factory,
                existing_task: None,
                allow_create: true,
                refresh_updated_at: false,
            };
            match normalize_task_for_storage(&task_value, &options) {
                Ok(normalized) => {
                    raw_tasks_by_id.insert(normalized.id.clone(), task_value);
                    scheduled_tasks.push(normalized);
                }
                Err(_) => {
                    // Isolated per task (JS `catch {}`).
                }
            }
        }
        Ok(RawProjectConfig {
            scheduled_tasks,
            raw_tasks_by_id,
        })
    }

    /// `toStoredTasks`: replaced tasks go out normalized, everything else
    /// verbatim; a state-only update swaps just the `state` onto the stored
    /// record.
    ///
    /// `toStoredTasks`：被替换的任务（`replaced_ids`）输出规范化 JSON，其余
    /// 任务按磁盘原始记录原样输出；仅做状态更新的任务（`state_updated_id`）
    /// 只在原始记录上替换 `state` 字段，其余字段保持原始字节。
    fn to_stored_tasks(
        config: &RawProjectConfig,
        tasks: &[ScheduledTask],
        replaced_ids: &HashSet<String>,
        state_updated_id: Option<&str>,
    ) -> Vec<Value> {
        tasks
            .iter()
            .map(|task| {
                if replaced_ids.contains(&task.id) {
                    return serialize_task(task);
                }
                if let Some(stored) = config.raw_tasks_by_id.get(&task.id) {
                    if Some(task.id.as_str()) == state_updated_id {
                        let mut merged = stored.clone();
                        if let Some(object) = merged.as_object_mut() {
                            object.insert("state".to_string(), serialize_task_state(&task.state));
                        }
                        return merged;
                    }
                    return stored.clone();
                }
                serialize_task(task)
            })
            .collect()
    }

    /// `writeProjectConfigToDisk`: merge over the existing raw record
    /// (unknown top-level keys preserved), write temp, atomic rename.
    ///
    /// `writeProjectConfigToDisk`：在现有原始配置之上合并（保留未知顶层键），
    /// 强制写入 `version` 与 `scheduledTasks`，先写临时文件再原子 rename；
    /// 写失败时清理临时文件并上抛 IO 错误。
    async fn write_config_to_disk(
        &self,
        project_id: &str,
        stored_tasks: &[Value],
    ) -> AppResult<()> {
        let file_path = self.resolve_project_config_path(project_id)?;
        let parent_directory = file_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let temporary_path = PathBuf::from(format!(
            "{}.tmp-{}-{}-{:x}",
            file_path.to_string_lossy(),
            std::process::id(),
            self.now_ms(),
            rand::random::<u64>()
        ));

        let mut merged = self.read_raw_from_disk(project_id).await?;
        merged.insert("version".to_string(), Value::from(PROJECT_CONFIG_VERSION));
        merged.insert(
            "scheduledTasks".to_string(),
            Value::Array(stored_tasks.to_vec()),
        );

        tokio::fs::create_dir_all(&parent_directory).await?;
        let content = serde_json::to_string_pretty(&Value::Object(merged)).map_err(|error| {
            AppError::internal(format!("failed to serialize project config: {error}"))
        })?;
        let write = async {
            tokio::fs::write(&temporary_path, content.as_bytes()).await?;
            tokio::fs::rename(&temporary_path, &file_path).await
        };
        if let Err(error) = write.await {
            let _ = tokio::fs::remove_file(&temporary_path).await;
            return Err(AppError::Io(error));
        }
        Ok(())
    }

    /// `acquireProjectFileLock` — cross-process O_EXCL lockfile with
    /// dead-pid / age / unparseable-mtime stale recovery.
    ///
    /// `acquireProjectFileLock`：以 O_EXCL 创建锁文件实现跨进程互斥，带三类
    /// 失效恢复——属主 pid 已死、锁龄超阈值、payload 不可解析（退化为按
    /// mtime 判断）。等待超过 `lock_wait_ms` 后返回 internal 超时错误。
    pub(crate) async fn acquire_project_file_lock(
        &self,
        project_id: &str,
    ) -> AppResult<FileLockGuard> {
        let config_path = self.resolve_project_config_path(project_id)?;
        let lock_path = PathBuf::from(format!("{}.lock", config_path.to_string_lossy()));
        if let Some(parent) = lock_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let pid = std::process::id() as i32;
        let started_at = std::time::Instant::now();

        loop {
            match tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
                .await
            {
                Ok(file) => {
                    let payload =
                        serde_json::json!({ "pid": pid, "at": self.now_ms() }).to_string();
                    let mut file = file;
                    if let Err(error) = file.write_all(payload.as_bytes()).await {
                        let _ = tokio::fs::remove_file(&lock_path).await;
                        return Err(AppError::Io(error));
                    }
                    // tokio::fs::File is buffered: without an explicit flush
                    // the payload lands when the handle drops on a background
                    // thread — after another writer may have replaced the
                    // file. The lock payload must be on disk before the
                    // guard is handed out.
                    if let Err(error) = file.flush().await {
                        let _ = tokio::fs::remove_file(&lock_path).await;
                        return Err(AppError::Io(error));
                    }
                    return Ok(FileLockGuard { lock_path, pid });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(AppError::Io(error)),
            }

            let now = self.now_ms();
            let mut steal = false;
            let lock_payload = tokio::fs::read_to_string(&lock_path)
                .await
                .ok()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
            match lock_payload {
                Some(parsed) => {
                    let lock_pid = parsed.get("pid").and_then(Value::as_f64);
                    let lock_at = parsed.get("at").and_then(Value::as_f64);
                    let stale_by_pid = lock_pid
                        .filter(|pid| pid.fract() == 0.0 && *pid > 0.0 && *pid <= i32::MAX as f64)
                        .is_some_and(|pid| !is_process_alive(pid as i32));
                    let stale_by_age = lock_at.filter(|at| at.is_finite()).is_some_and(|at| {
                        now.saturating_sub(at as i64 as u64) > self.inner.lock_stale_ms
                    });
                    let pid_not_integer = lock_pid.is_none_or(|pid| pid.fract() != 0.0);
                    if stale_by_pid || stale_by_age || pid_not_integer {
                        steal = true;
                    }
                }
                None => {
                    // Crash between create and payload write (or partial
                    // payload): fall back to mtime age so recovery is wedged
                    // only until the stale window passes.
                    if let Ok(metadata) = tokio::fs::metadata(&lock_path).await
                        && let Ok(modified) = metadata.modified()
                    {
                        let age_ms = modified
                            .elapsed()
                            .map(|duration| duration.as_millis() as u64)
                            .unwrap_or(u64::MAX);
                        if age_ms > self.inner.lock_stale_ms {
                            steal = true;
                        }
                    }
                }
            }

            if steal {
                let _ = tokio::fs::remove_file(&lock_path).await;
                continue;
            }

            if started_at.elapsed().as_millis() as u64 >= self.inner.lock_wait_ms {
                return Err(AppError::internal(format!(
                    "timeout acquiring project config lock for {}",
                    project_id
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(self.inner.lock_retry_ms)).await;
        }
    }

    /// `withProjectWriteLock`: in-process per-project chain, then the
    /// cross-process file lock. A lock-acquire failure (timeout) still
    /// releases the chain so later writes for this project proceed.
    ///
    /// `withProjectWriteLock`：先取进程内按项目的写链，再取跨进程文件锁，然后
    /// 执行 `mutate`。抢锁失败（超时）也会释放写链，保证该项目后续写入不被
    /// 卡死；结束后在没有其他等待者时清理写链表项（镜像 JS 的 writeLocks 清理）。
    async fn with_project_write_lock<T, F>(&self, project_id: &str, mutate: F) -> AppResult<T>
    where
        F: Future<Output = AppResult<T>>,
    {
        let key = sanitize_project_id(project_id)?;
        let chain = {
            let mut chains = self
                .inner
                .write_chains
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            chains
                .entry(key.clone())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let _chain_guard = chain.lock().await;
        let file_lock = self.acquire_project_file_lock(project_id).await?;
        let result = mutate.await;
        file_lock.release().await;
        // Mirror the JS writeLocks entry cleanup: drop the chain entry when
        // no other waiter holds a clone (a fresh waiter re-creates it).
        let mut chains = self
            .inner
            .write_chains
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if Arc::strong_count(&chain) <= 2 {
            chains.remove(&key);
        }
        result
    }

    /// `listScheduledTasks`.
    /// `listScheduledTasks`：读取并返回项目的全部规范化任务。
    pub async fn list_scheduled_tasks(&self, project_id: &str) -> AppResult<Vec<ScheduledTask>> {
        Ok(self
            .read_config_from_disk(project_id)
            .await?
            .scheduled_tasks)
    }

    /// `upsertScheduledTask`.
    /// `upsertScheduledTask`：按输入 `id` 判断新建或替换；读-改-写整体在项目
    /// 写锁内完成，返回写入后的任务、全量列表与是否新建。
    pub async fn upsert_scheduled_task(
        &self,
        project_id: &str,
        task_input: &Value,
    ) -> AppResult<UpsertResult> {
        self.with_project_write_lock(project_id, async {
            let now = self.now_ms();
            let current = self.read_config_from_disk(project_id).await?;
            let incoming_id = as_non_empty_string(task_input.get("id").unwrap_or(&Value::Null));
            let existing_index = incoming_id.as_deref().and_then(|id| {
                current
                    .scheduled_tasks
                    .iter()
                    .position(|task| task.id == id)
            });
            let existing_task = existing_index.map(|index| &current.scheduled_tasks[index]);

            let factory = self.inner.task_id_factory.clone();
            let normalized_task = normalize_task_for_storage(
                task_input,
                &NormalizeOptions {
                    now,
                    default_now: now,
                    create_id: &*factory,
                    existing_task,
                    allow_create: true,
                    refresh_updated_at: true,
                },
            )
            .map_err(AppError::internal)?;

            let mut next_tasks = current.scheduled_tasks.clone();
            let created = existing_task.is_none();
            match existing_index {
                Some(index) => next_tasks[index] = normalized_task.clone(),
                None => next_tasks.push(normalized_task.clone()),
            }

            let replaced_ids = HashSet::from([normalized_task.id.clone()]);
            let stored = Self::to_stored_tasks(&current, &next_tasks, &replaced_ids, None);
            self.write_config_to_disk(project_id, &stored).await?;

            Ok(UpsertResult {
                task: normalized_task,
                tasks: next_tasks,
                created,
            })
        })
        .await
    }

    /// `deleteScheduledTask`.
    /// `deleteScheduledTask`：按 id 删除任务；未命中时不写盘并返回 `deleted=false`。
    pub async fn delete_scheduled_task(
        &self,
        project_id: &str,
        task_id: &str,
    ) -> AppResult<DeleteResult> {
        self.with_project_write_lock(project_id, async {
            let normalized_task_id = as_non_empty_string(&Value::String(task_id.to_string()))
                .ok_or_else(|| AppError::internal("taskId is required"))?;

            let current = self.read_config_from_disk(project_id).await?;
            let next_tasks: Vec<ScheduledTask> = current
                .scheduled_tasks
                .iter()
                .filter(|task| task.id != normalized_task_id)
                .cloned()
                .collect();
            let deleted = next_tasks.len() != current.scheduled_tasks.len();

            if deleted {
                let stored = Self::to_stored_tasks(&current, &next_tasks, &HashSet::new(), None);
                self.write_config_to_disk(project_id, &stored).await?;
            }

            Ok(DeleteResult {
                deleted,
                tasks: next_tasks,
            })
        })
        .await
    }

    /// `{...currentTask.state, ...patchObject, updatedAt: Date.now()}` fed
    /// through `normalizeState` (JS builds `nextTask` inline).
    ///
    /// 状态补丁合并：`{...currentTask.state, ...patch, updatedAt: now}` 之后再
    /// 过一遍 `normalizeState`（JS 在调用点内联构建 `nextTask`）。
    fn patch_task_state(
        current_task: &ScheduledTask,
        state_patch: &Value,
        now: u64,
    ) -> ScheduledTask {
        let mut merged = serialize_task_state(&current_task.state);
        if let Some(object) = merged.as_object_mut() {
            if let Some(patch) = state_patch.as_object() {
                for (key, value) in patch {
                    object.insert(key.clone(), value.clone());
                }
            }
            object.insert("updatedAt".to_string(), Value::from(now));
        }
        let state = normalize_state(Some(&merged), Some(&current_task.state), now);
        ScheduledTask {
            state,
            ..current_task.clone()
        }
    }

    /// `updateScheduledTaskState`.
    ///
    /// `updateScheduledTaskState`：合并状态补丁并落盘；任务不存在时返回
    /// `updated=false` 与当前列表，不产生任何写入。
    pub async fn update_scheduled_task_state(
        &self,
        project_id: &str,
        task_id: &str,
        state_patch: &Value,
    ) -> AppResult<StateUpdateResult> {
        self.with_project_write_lock(project_id, async {
            let normalized_task_id = as_non_empty_string(&Value::String(task_id.to_string()))
                .ok_or_else(|| AppError::internal("taskId is required"))?;

            let current = self.read_config_from_disk(project_id).await?;
            let Some(task_index) = current
                .scheduled_tasks
                .iter()
                .position(|task| task.id == normalized_task_id)
            else {
                return Ok(StateUpdateResult {
                    task: None,
                    tasks: current.scheduled_tasks,
                    updated: false,
                });
            };

            let current_task = &current.scheduled_tasks[task_index];
            let next_task = Self::patch_task_state(current_task, state_patch, self.now_ms());

            let mut next_tasks = current.scheduled_tasks.clone();
            next_tasks[task_index] = next_task.clone();

            let stored = Self::to_stored_tasks(
                &current,
                &next_tasks,
                &HashSet::new(),
                Some(next_task.id.as_str()),
            );
            self.write_config_to_disk(project_id, &stored).await?;

            Ok(StateUpdateResult {
                task: Some(next_task),
                tasks: next_tasks,
                updated: true,
            })
        })
        .await
    }

    /// `updateScheduledTaskStateIf` — occurrence claim across server
    /// instances: the predicate sees the latest on-disk task and a false
    /// return skips the write entirely.
    ///
    /// `updateScheduledTaskStateIf`：跨 server 实例的触发认领——谓词作用于
    /// 磁盘上最新的任务，返回 false 时完全跳过写盘（`updated=false`）。
    pub async fn update_scheduled_task_state_if(
        &self,
        project_id: &str,
        task_id: &str,
        predicate: &(dyn Fn(&ScheduledTask) -> bool + Send + Sync),
        state_patch: &Value,
    ) -> AppResult<StateUpdateResult> {
        self.with_project_write_lock(project_id, async {
            let normalized_task_id = as_non_empty_string(&Value::String(task_id.to_string()))
                .ok_or_else(|| AppError::internal("taskId is required"))?;

            let current = self.read_config_from_disk(project_id).await?;
            let Some(task_index) = current
                .scheduled_tasks
                .iter()
                .position(|task| task.id == normalized_task_id)
            else {
                return Ok(StateUpdateResult {
                    task: None,
                    tasks: current.scheduled_tasks,
                    updated: false,
                });
            };

            let current_task = &current.scheduled_tasks[task_index];
            if !predicate(current_task) {
                return Ok(StateUpdateResult {
                    task: Some(current_task.clone()),
                    tasks: current.scheduled_tasks,
                    updated: false,
                });
            }

            let next_task = Self::patch_task_state(current_task, state_patch, self.now_ms());
            let mut next_tasks = current.scheduled_tasks.clone();
            next_tasks[task_index] = next_task.clone();

            let stored = Self::to_stored_tasks(
                &current,
                &next_tasks,
                &HashSet::new(),
                Some(next_task.id.as_str()),
            );
            self.write_config_to_disk(project_id, &stored).await?;

            Ok(StateUpdateResult {
                task: Some(next_task),
                tasks: next_tasks,
                updated: true,
            })
        })
        .await
    }

    /// `reconcileLoopTasks` — markdown loop definitions win over JSON tasks
    /// by file-path identity (renames), JSON tasks adopt by name, removed
    /// files unschedule, transiently unparseable files keep the last good
    /// task. See the JS doc comment for the full rule set.
    ///
    /// `reconcileLoopTasks`：markdown loop 定义按文件路径身份（重命名可跟踪）
    /// 优先于 JSON 任务；同名 JSON 任务被收养；驱动文件被删除则取消调度；
    /// 暂时无法解析的文件保留最后一次有效任务。完整规则集见 JS 端文档注释。
    pub async fn reconcile_loop_tasks(
        &self,
        project_id: &str,
        loops: &[LoopEntry],
    ) -> AppResult<Vec<ScheduledTask>> {
        self.with_project_write_lock(project_id, async {
            let now = self.now_ms();
            let current = self.read_config_from_disk(project_id).await?;
            let factory = self.inner.task_id_factory.clone();

            let mut active_loop_file_paths: HashSet<String> = HashSet::new();
            // Insertion-ordered pending map (JS Map semantics).
            let mut pending_loops: HashMap<String, usize> = HashMap::new();
            let mut pending_order: Vec<String> = Vec::new();
            let mut loops_by_path: HashMap<String, usize> = HashMap::new();
            for (index, loop_entry) in loops.iter().enumerate() {
                if loop_entry.file_path.is_empty() {
                    continue;
                }
                active_loop_file_paths.insert(loop_entry.file_path.clone());
                if let Some(definition) = &loop_entry.definition {
                    if let Some(name) = definition.get("name").and_then(Value::as_str) {
                        let name = name.to_string();
                        if !pending_loops.contains_key(&name) {
                            pending_order.push(name.clone());
                        }
                        pending_loops.insert(name, index);
                    }
                    loops_by_path.insert(loop_entry.file_path.clone(), index);
                }
            }

            let mut consumed_loop_paths: HashSet<String> = HashSet::new();
            let mut next_tasks: Vec<ScheduledTask> = Vec::new();
            let mut replaced_ids: HashSet<String> = HashSet::new();
            for task in &current.scheduled_tasks {
                let task_loop_file = task.loop_file.clone();
                if let Some(loop_file) = &task_loop_file
                    && !active_loop_file_paths.contains(loop_file)
                {
                    // Driving loop file removed (or renamed) — unschedule.
                    continue;
                }

                let loop_index = if task_loop_file.is_some() {
                    task_loop_file
                        .as_deref()
                        .and_then(|file| loops_by_path.get(file).copied())
                } else {
                    pending_loops.get(&task.name).copied()
                };
                if let Some(loop_index) = loop_index {
                    let loop_entry = &loops[loop_index];
                    let definition = loop_entry.definition.clone().unwrap_or(Value::Null);
                    // { ...task, ...definition, execution: {...task.execution,
                    //   ...definition.execution}, loopFile }
                    let mut map = serialize_task(task)
                        .as_object()
                        .cloned()
                        .unwrap_or_default();
                    if let Some(definition_map) = definition.as_object() {
                        for (key, value) in definition_map {
                            map.insert(key.clone(), value.clone());
                        }
                    }
                    let mut execution =
                        serde_json::to_value(&task.execution).unwrap_or(Value::Null);
                    if let (Some(execution_map), Some(Value::Object(patch))) =
                        (execution.as_object_mut(), definition.get("execution"))
                    {
                        for (key, value) in patch {
                            execution_map.insert(key.clone(), value.clone());
                        }
                    }
                    map.insert("execution".to_string(), execution);
                    map.insert(
                        "loopFile".to_string(),
                        Value::String(loop_entry.file_path.clone()),
                    );

                    let options = NormalizeOptions {
                        now,
                        default_now: now,
                        create_id: &*factory,
                        existing_task: Some(task),
                        allow_create: false,
                        refresh_updated_at: false,
                    };
                    match normalize_task_for_storage(&Value::Object(map), &options) {
                        Ok(adopted) => {
                            if let Some(name) = loop_entry
                                .definition
                                .as_ref()
                                .and_then(|d| d.get("name"))
                                .and_then(Value::as_str)
                            {
                                pending_loops.remove(name);
                            }
                            if let Some(task_loop_file) = &task_loop_file {
                                consumed_loop_paths.insert(task_loop_file.clone());
                                loops_by_path.remove(task_loop_file);
                            }
                            replaced_ids.insert(adopted.id.clone());
                            next_tasks.push(adopted);
                        }
                        Err(error) => {
                            tracing::warn!(
                                "[scheduled-tasks] skipped loop {} for task \"{}\": {}",
                                loop_entry.file_path,
                                task.name,
                                error
                            );
                            next_tasks.push(task.clone());
                        }
                    }
                    continue;
                }

                if let Some(loop_file) = &task_loop_file
                    && consumed_loop_paths.contains(loop_file)
                {
                    // Orphan duplicate of an already-adopted loop file.
                    continue;
                }

                next_tasks.push(task.clone());
            }

            for name in &pending_order {
                let Some(&loop_index) = pending_loops.get(name) else {
                    continue;
                };
                let loop_entry = &loops[loop_index];
                let definition = loop_entry.definition.clone().unwrap_or(Value::Null);
                let mut map = Map::new();
                map.insert(
                    "id".to_string(),
                    Value::String(format!("loop:{}:{}", loop_entry.scope, name)),
                );
                if let Some(definition_map) = definition.as_object() {
                    for (key, value) in definition_map {
                        map.insert(key.clone(), value.clone());
                    }
                }
                map.insert(
                    "loopFile".to_string(),
                    Value::String(loop_entry.file_path.clone()),
                );

                let options = NormalizeOptions {
                    now,
                    default_now: now,
                    create_id: &*factory,
                    existing_task: None,
                    allow_create: true,
                    refresh_updated_at: false,
                };
                match normalize_task_for_storage(&Value::Object(map), &options) {
                    Ok(created) => {
                        replaced_ids.insert(created.id.clone());
                        next_tasks.push(created);
                    }
                    Err(error) => {
                        tracing::warn!(
                            "[scheduled-tasks] skipped loop {}: {}",
                            loop_entry.file_path,
                            error
                        );
                    }
                }
            }

            let stored = Self::to_stored_tasks(&current, &next_tasks, &replaced_ids, None);
            self.write_config_to_disk(project_id, &stored).await?;

            Ok(next_tasks)
        })
        .await
    }
}
