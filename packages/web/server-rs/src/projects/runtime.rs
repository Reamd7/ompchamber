//! Port of `createProjectConfigRuntime` from
//! `server/lib/projects/project-config.js`.
//!
//! Per-project task persistence at `<projects-dir>/<projectID>.json`:
//! JSONC-tolerant reads, per-task isolation for broken entries, unknown-field
//! preservation (untouched tasks are re-persisted verbatim; only deliberately
//! replaced tasks are re-serialized), atomic temp+rename writes, and a
//! cross-process `<config>.lock` with stale recovery layered under an
//! in-process per-project write chain.

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

const PROJECT_FILE_LOCK_WAIT_MS: u64 = 10_000;
const PROJECT_FILE_LOCK_STALE_MS: u64 = 60_000;
const PROJECT_FILE_LOCK_RETRY_MS: u64 = 20;
#[derive(Debug, Clone, Serialize)]
pub struct UpsertResult {
    pub task: ScheduledTask,
    pub tasks: Vec<ScheduledTask>,
    pub created: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct DeleteResult {
    pub deleted: bool,
    pub tasks: Vec<ScheduledTask>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StateUpdateResult {
    pub task: Option<ScheduledTask>,
    pub tasks: Vec<ScheduledTask>,
    pub updated: bool,
}

/// Raw on-disk record alongside the normalized view: writers persist
/// untouched tasks from `raw_tasks_by_id` verbatim so fields unknown to this
/// build survive writes (JS `readProjectConfigFromDisk`/`toStoredTasks`).
struct RawProjectConfig {
    scheduled_tasks: Vec<ScheduledTask>,
    raw_tasks_by_id: HashMap<String, Value>,
}

/// Port of the object returned by `createProjectConfigRuntime`.
#[derive(Clone)]
pub struct ProjectConfigRuntime {
    inner: Arc<RuntimeInner>,
}

struct RuntimeInner {
    projects_dir: PathBuf,
    task_id_factory: Arc<dyn Fn() -> String + Send + Sync>,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    lock_wait_ms: u64,
    lock_stale_ms: u64,
    lock_retry_ms: u64,
    write_chains: std::sync::Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

/// JS default `createTaskID` fallback (`task_<Date.now()>_<random>`).
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
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// `isProcessAlive` via `kill(pid, 0)`; EPERM (another user's live process)
/// counts as alive.
#[cfg(unix)]
fn is_process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    const EPERM: i32 = 1;
    let result = unsafe { kill(pid, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(EPERM)
}

#[cfg(not(unix))]
fn is_process_alive(_pid: i32) -> bool {
    // Liveness cannot be probed; rely on age-based staleness instead.
    true
}

/// Owned cross-process lock guard. Release unlinks the lock file only while
/// this pid still owns it, so a stale-recovery steal is never dropped.
pub(crate) struct FileLockGuard {
    lock_path: PathBuf,
    pid: i32,
}

impl FileLockGuard {
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

impl ProjectConfigRuntime {
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
    pub fn for_context(ctx: &RouterContext) -> Self {
        Self::new(ctx.config.data_dir.join("projects"))
    }

    /// Test/dependency seam for the JS `createTaskID` injection.
    pub fn with_task_id_factory(mut self, factory: Arc<dyn Fn() -> String + Send + Sync>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("runtime not shared yet")
            .task_id_factory = factory;
        self
    }

    /// Test seam replacing `Date.now()` (JS has no injection point; the
    /// observable contract is a millisecond epoch clock).
    pub fn with_clock(mut self, clock: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("runtime not shared yet")
            .clock = clock;
        self
    }

    /// Test seam for the lock timings (JS hardcodes 10s/60s/20ms).
    pub fn with_lock_timings(mut self, wait_ms: u64, stale_ms: u64, retry_ms: u64) -> Self {
        let inner = Arc::get_mut(&mut self.inner).expect("runtime not shared yet");
        inner.lock_wait_ms = wait_ms;
        inner.lock_stale_ms = stale_ms;
        inner.lock_retry_ms = retry_ms;
        self
    }

    /// `resolveProjectConfigPath`.
    pub fn resolve_project_config_path(&self, project_id: &str) -> AppResult<PathBuf> {
        let safe_project_id = sanitize_project_id(project_id)?;
        Ok(self
            .inner
            .projects_dir
            .join(format!("{}.json", safe_project_id)))
    }

    fn now_ms(&self) -> u64 {
        (self.inner.clock)()
    }

    /// `readRawProjectConfigFromDisk`: ENOENT → empty object; a broken file
    /// is an error (isolated per project, never a silent empty overwrite).
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
    pub async fn list_scheduled_tasks(&self, project_id: &str) -> AppResult<Vec<ScheduledTask>> {
        Ok(self
            .read_config_from_disk(project_id)
            .await?
            .scheduled_tasks)
    }

    /// `upsertScheduledTask`.
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
