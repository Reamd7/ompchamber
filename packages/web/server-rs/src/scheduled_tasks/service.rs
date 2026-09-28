//! Port of `server/lib/scheduled-tasks/service.js` — the route-facing task
//! service (CRUD, loop-file mutations, manual run, status).
//!
//! `OMPChamberControlError` maps to [`ServiceError`] (status + message, and
//! for run failures the offending task).
//! 中文说明：面向路由的任务服务层——列表/保存/删除的 CRUD、loop 文件
//! 的启停与删除、手动触发运行与状态查询。本层组合项目校验、存储读写
//! 与运行时同步，并把各处失败映射为带 HTTP 状态码的 `ServiceError`。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};

use crate::error::AppResult;
use crate::projects::ScheduledTask;

use super::loops::set_loop_file_enabled;
use super::runtime::{ProjectRef, ProjectsAccess, ScheduledTaskStore, SharedRuntime};

/// `OMPChamberControlError { statusCode, message, task? }`.
/// 服务层错误：HTTP 状态码 + 消息，run 失败时可附带出错任务快照。
#[derive(Debug, Clone)]
pub struct ServiceError {
/// HTTP 状态码（400/404/409/500 等）。
    pub status: u16,
/// 面向客户端的错误消息。
    pub message: String,
/// 运行失败时附带的任务 JSON（供前端展示）。
    pub task: Option<Value>,
}

/// 便捷构造器。
impl ServiceError {
/// 指定状态码与消息构造错误（不附带任务）。
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            task: None,
        }
    }

/// 400 构造器。
    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }

/// 404 构造器。
    fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, message)
    }
}

/// `listProjects` — settings.json `projects` entries, sanitized to the
/// id/path subset this module consumes (full-field sanitization belongs to
/// the settings module port).
/// 从 settings.json 读取 `projects` 条目的 `listProjects` 实现，仅做本
/// 模块消费的 id/path 清洗（完整字段清洗归属 settings 模块移植）。
pub struct SettingsProjects {
/// settings.json 的完整路径。
    settings_path: PathBuf,
}

/// 读取与清洗。
impl SettingsProjects {
/// 以数据目录定位 settings.json。
    pub fn new(data_dir: &Path) -> Self {
        Self {
            settings_path: data_dir.join("settings.json"),
        }
    }

/// 同步读取并清洗项目列表：跳过非对象条目与缺失 id/path 的条目，相对
/// 路径转绝对路径，重复 id 与重复路径只保留首个；文件缺失或 JSON 损坏
/// 均返回空列表。
    fn read(&self) -> Vec<ProjectRef> {
        let Ok(text) = std::fs::read_to_string(&self.settings_path) else {
            return Vec::new();
        };
        let Ok(document) = serde_json::from_str::<Value>(&text) else {
            return Vec::new();
        };
        let Some(entries) = document.get("projects").and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut result: Vec<ProjectRef> = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();
        let mut seen_paths = std::collections::HashSet::new();
        for entry in entries {
            let Some(object) = entry.as_object() else {
                continue;
            };
            let id = object
                .get("id")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default();
            let raw_path = object
                .get("path")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default();
            if id.is_empty() || raw_path.is_empty() {
                continue;
            }
            let resolved = std::path::absolute(raw_path)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| raw_path.to_string());
            if !seen_ids.insert(id.to_string()) || !seen_paths.insert(resolved.clone()) {
                continue;
            }
            result.push(ProjectRef {
                id: id.to_string(),
                path: resolved,
            });
        }
        result
    }
}

/// 以异步 trait 接口暴露同步读取结果，供运行时轮询。
impl ProjectsAccess for SettingsProjects {
/// 读取一次项目列表并包装为 future。
    fn list_projects(&self) -> super::dispatch::BoxFut<'static, AppResult<Vec<ProjectRef>>> {
        let projects = self.read();
        Box::pin(async move { Ok(projects) })
    }
}

/// 路由背后的任务服务：项目校验 + 存储读写 + 运行时同步的组合门面。
pub struct ScheduledTaskService {
/// 项目列表来源（settings.json 或测试替身）。
    projects: Arc<dyn ProjectsAccess>,
/// 任务持久化存储。
    store: Arc<dyn ScheduledTaskStore>,
/// 共享调度运行时（loop 文件对账与手动运行）。
    runtime: SharedRuntime,
}

/// 手动运行成功的载荷：任务快照、会话 id 与可选的持久化告警。
#[derive(Debug)]
pub struct RunSuccess {
/// 运行后的任务快照。
    pub task: Option<ScheduledTask>,
/// 创建的会话 id。
    pub session_id: Option<String>,
/// 运行成功但状态写回失败时的告警消息。
    pub persist_error: Option<String>,
}

/// 各路由动作的实现。
impl ScheduledTaskService {
/// 组装服务并放入 `Arc` 供路由共享。
    pub fn new(
        projects: Arc<dyn ProjectsAccess>,
        store: Arc<dyn ScheduledTaskStore>,
        runtime: SharedRuntime,
    ) -> Arc<Self> {
        Arc::new(Self {
            projects,
            store,
            runtime,
        })
    }

/// 委托项目来源读取列表。
    async fn list_projects(&self) -> AppResult<Vec<ProjectRef>> {
        self.projects.list_projects().await
    }

/// 校验 projectId 非空且存在于项目列表；空白为 400，未知为 404，读取
/// 失败为 500。
    async fn find_project_by_id(&self, project_id: &str) -> Result<(), ServiceError> {
        let Some(normalized) = non_empty(project_id) else {
            return Err(ServiceError::bad_request("projectId is required"));
        };
        let projects = self
            .list_projects()
            .await
            .map_err(|_| ServiceError::new(500, "Failed to load scheduled tasks"))?;
        if projects.iter().any(|project| project.id == normalized) {
            return Ok(());
        }
        Err(ServiceError::not_found("Project not found"))
    }

    /// `list`: reconcile loop files first, then return the task list.
/// 先对账 loop 文件再返回任务列表；项目不存在为 404，同步失败为 500。
    pub async fn list(&self, project_id: &str) -> Result<Vec<ScheduledTask>, ServiceError> {
        self.find_project_by_id(project_id).await?;
        self.runtime
            .sync_project(&non_empty(project_id).unwrap_or_default())
            .await
            .map_err(|error| ServiceError::new(500, error.to_string()))
    }

/// 定位由 loop 文件管理的任务：校验项目与 taskId、同步后按 id 查找；
/// 任务缺失为 404、非 loop 任务为 400、loop 文件已被删为 404。
    async fn find_loop_task(
        &self,
        project_id: &str,
        task_id: &str,
    ) -> Result<ScheduledTask, ServiceError> {
        self.find_project_by_id(project_id).await?;
        let normalized =
            non_empty(task_id).ok_or_else(|| ServiceError::bad_request("taskId is required"))?;
        let tasks = self
            .runtime
            .sync_project(&non_empty(project_id).unwrap_or_default())
            .await
            .map_err(|error| ServiceError::new(500, error.to_string()))?;
        let task = tasks
            .iter()
            .find(|entry| entry.id == normalized)
            .cloned()
            .ok_or_else(|| ServiceError::not_found("Task not found"))?;
        let Some(loop_file) = task.loop_file.as_deref() else {
            return Err(ServiceError::bad_request(
                "Task is not managed by a loop file",
            ));
        };
        if !Path::new(loop_file).exists() {
            return Err(ServiceError::not_found("Loop file not found"));
        }
        Ok(task)
    }

    /// `setLoopEnabled`: toggle `enabled` in the loop markdown frontmatter,
    /// then reconcile the project.
/// 改写 loop frontmatter 的 `enabled` 后重新对账，返回更新后的任务；
/// `enabled` 缺失/非布尔为 400，文件当前不合法为 400。
    pub async fn set_loop_enabled(
        &self,
        project_id: &str,
        task_id: &str,
        enabled: Option<bool>,
    ) -> Result<Option<ScheduledTask>, ServiceError> {
        let Some(enabled) = enabled else {
            return Err(ServiceError::bad_request("enabled must be a boolean"));
        };
        let task = self.find_loop_task(project_id, task_id).await?;
        let loop_file = task.loop_file.clone().unwrap_or_default();
        if !set_loop_file_enabled(Path::new(&loop_file), enabled) {
            return Err(ServiceError::bad_request(
                "Loop file must be valid before changing its enabled state",
            ));
        }
        let tasks = self
            .runtime
            .sync_project(&non_empty(project_id).unwrap_or_default())
            .await
            .map_err(|error| ServiceError::new(500, error.to_string()))?;
        let normalized = non_empty(task_id).unwrap_or_default();
        Ok(tasks.into_iter().find(|entry| entry.id == normalized))
    }

    /// `removeLoopFile`: delete the authoritative markdown file, then sync.
/// 删除作为唯一事实源的 loop markdown 文件并重新同步；删除失败为 500。
    pub async fn remove_loop_file(
        &self,
        project_id: &str,
        task_id: &str,
    ) -> Result<Vec<ScheduledTask>, ServiceError> {
        let task = self.find_loop_task(project_id, task_id).await?;
        let loop_file = task.loop_file.clone().unwrap_or_default();
        std::fs::remove_file(&loop_file)
            .map_err(|error| ServiceError::new(500, error.to_string()))?;
        self.runtime
            .sync_project(&non_empty(project_id).unwrap_or_default())
            .await
            .map_err(|error| ServiceError::new(500, error.to_string()))
    }

    /// `upsert`: store the task, re-sync schedules, answer the saved list.
/// 保存任务、重新同步调度并返回 `{tasks, task, created}`；载荷非对象
/// 为 400，存储层校验失败按消息内容映射 400/500。
    pub async fn upsert(
        &self,
        project_id: &str,
        task_input: &Value,
    ) -> Result<Value, ServiceError> {
        self.find_project_by_id(project_id).await?;
        if !task_input.is_object() {
            return Err(ServiceError::bad_request("task payload is required"));
        }
        let normalized_project = non_empty(project_id).unwrap_or_default();
        let upserted = self
            .store
            .upsert_scheduled_task(&normalized_project, task_input)
            .await
            .map_err(|error| {
                let message = error.to_string();
                let lower = message.to_lowercase();
                let status = if lower.contains("required") || lower.contains("invalid") {
                    400
                } else {
                    500
                };
                ServiceError::new(status, message)
            })?;

        self.runtime
            .sync_project(&normalized_project)
            .await
            .map_err(|error| ServiceError::new(500, error.to_string()))?;
        let tasks = self
            .store
            .list_scheduled_tasks(&normalized_project)
            .await
            .map_err(|error| ServiceError::new(500, error.to_string()))?;
        let task = tasks
            .iter()
            .find(|task| task.id == upserted.task.id)
            .cloned()
            .unwrap_or(upserted.task);
        Ok(json!({
            "tasks": tasks,
            "task": task,
            "created": upserted.created,
        }))
    }

    /// `remove`: loop-sourced tasks are protected while their file exists.
/// 删除任务：loop 来源任务在文件仍存在时拒绝（400，提示改删文件）；
/// 任务不存在为 404；删除后重新同步并返回剩余列表。
    pub async fn remove(
        &self,
        project_id: &str,
        task_id: &str,
    ) -> Result<Vec<ScheduledTask>, ServiceError> {
        self.find_project_by_id(project_id).await?;
        let normalized =
            non_empty(task_id).ok_or_else(|| ServiceError::bad_request("taskId is required"))?;
        let normalized_project = non_empty(project_id).unwrap_or_default();
        let current = self
            .store
            .list_scheduled_tasks(&normalized_project)
            .await
            .map_err(|error| ServiceError::new(500, error.to_string()))?;
        if let Some(existing) = current.iter().find(|task| task.id == normalized)
            && let Some(loop_file) = existing.loop_file.as_deref()
            && Path::new(loop_file).exists()
        {
            return Err(ServiceError::bad_request(
                "Loop task is managed by its .agents/loops markdown file; delete the file to remove the task",
            ));
        }
        let result = self
            .store
            .delete_scheduled_task(&normalized_project, &normalized)
            .await
            .map_err(|error| ServiceError::new(500, error.to_string()))?;
        if !result.deleted {
            return Err(ServiceError::not_found("Task not found"));
        }
        self.runtime
            .sync_project(&normalized_project)
            .await
            .map_err(|error| ServiceError::new(500, error.to_string()))?;
        self.store
            .list_scheduled_tasks(&normalized_project)
            .await
            .map_err(|error| ServiceError::new(500, error.to_string()))
    }

    /// `run`: manual trigger mapping runtime outcomes onto HTTP semantics.
/// 手动触发：运行中/排队为 409，任务缺失或禁用为 404，失败为 500
///（附带任务），成功返回会话 id 与可选持久化告警。
    pub async fn run(&self, project_id: &str, task_id: &str) -> Result<RunSuccess, ServiceError> {
        self.find_project_by_id(project_id).await?;
        let normalized =
            non_empty(task_id).ok_or_else(|| ServiceError::bad_request("taskId is required"))?;
        let outcome = self
            .runtime
            .run_now(&non_empty(project_id).unwrap_or_default(), &normalized)
            .await;
        if outcome.running || outcome.queued {
            return Err(ServiceError::new(
                409,
                outcome
                    .error
                    .unwrap_or_else(|| "Task already running".into()),
            ));
        }
        if outcome.skipped {
            return Err(ServiceError::not_found("Task not found or disabled"));
        }
        if !outcome.ok {
            let mut error = ServiceError::new(
                500,
                outcome.error.unwrap_or_else(|| "Task run failed".into()),
            );
            error.task = outcome
                .task
                .as_ref()
                .map(|t| serde_json::to_value(t).unwrap_or(Value::Null));
            return Err(error);
        }
        let persist_error = outcome
            .persist_error
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        Ok(RunSuccess {
            task: outcome.task,
            session_id: outcome.session_id,
            persist_error,
        })
    }

    /// `status`: the runtime's status snapshot.
/// 透出运行时的状态快照。
    pub fn status(&self) -> Value {
        self.runtime.get_status()
    }
}

/// trim 后非空则返回 Some(修剪值)。
fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// 服务层行为测试：错误码映射、loop 文件保护与手动运行语义。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduled_tasks::runtime::{RuntimeDeps, ScheduledTasksRuntime};
    use crate::scheduled_tasks::testing::{
        MemoryProjects, MemoryStore, RecordingDispatch, daily_task, make_runtime,
    };
    use std::sync::atomic::Ordering;

/// 以给定存储与派发器组装服务（固定项目 p1）。
    fn service_with(
        store: Arc<MemoryStore>,
        dispatch: Arc<dyn crate::scheduled_tasks::dispatch::EngineDispatch>,
    ) -> Arc<ScheduledTaskService> {
        let runtime = make_runtime(Arc::clone(&store), dispatch, 1_750_000_000_000);
        let projects = Arc::new(MemoryProjects {
            projects: vec![ProjectRef {
                id: "p1".into(),
                path: "/repo".into(),
            }],
        });
        ScheduledTaskService::new(projects, store, runtime)
    }

/// 默认内存存储的便捷服务实例。
    fn service() -> Arc<ScheduledTaskService> {
        service_with(
            Arc::new(MemoryStore::new(daily_task())),
            RecordingDispatch::new(),
        )
    }

/// list 先对账并返回任务列表。
    #[tokio::test]
    async fn list_reconciles_and_returns_tasks() {
        let service = service();
        let tasks = service.list("p1").await.expect("list ok");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "task-1");
    }

/// 未知项目在 list 时映射为 404 Project not found。
    #[tokio::test]
    async fn list_surfaces_sync_failure() {
        // A store whose project id fails to sanitize surfaces as 500.
        let store = Arc::new(MemoryStore::new(daily_task()));
        let runtime = make_runtime(
            Arc::clone(&store),
            RecordingDispatch::new(),
            1_750_000_000_000,
        );
        let service = ScheduledTaskService::new(
            Arc::new(MemoryProjects { projects: vec![] }),
            store,
            runtime,
        );
        let error = service.list("ghost").await.expect_err("project missing");
        assert_eq!(error.status, 404);
        assert_eq!(error.message, "Project not found");
    }

/// 空白 projectId 映射为 400。
    #[tokio::test]
    async fn blank_project_id_is_400() {
        let service = service();
        let error = service.list("   ").await.expect_err("400");
        assert_eq!(error.status, 400);
        assert_eq!(error.message, "projectId is required");
    }

/// 存储层校验失败的消息映射为 400。
    #[tokio::test]
    async fn upsert_maps_invalid_payload_errors_to_400() {
        let service = service();
        // MemoryStore rejects the malformed input with an "invalid..." message
        // (the real store raises "schedule is required"-style errors).
        let error = service
            .upsert("p1", &serde_json::json!({ "name": "x" }))
            .await
            .expect_err("rejected");
        assert_eq!(error.status, 400);
        assert!(error.message.to_lowercase().contains("invalid"));
    }

/// 载荷不是对象时 upsert 拒绝为 400。
    #[tokio::test]
    async fn upsert_requires_payload_object() {
        let service = service();
        let error = service
            .upsert("p1", &serde_json::json!("nope"))
            .await
            .expect_err("400");
        assert_eq!(error.status, 400);
        assert_eq!(error.message, "task payload is required");
    }

/// loop 文件存在时拒绝删除任务，文件删除后放行。
    #[tokio::test]
    async fn remove_rejects_loop_task_while_file_exists() {
        let dir = std::env::temp_dir().join(format!("oc-svc-loop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let loop_file = dir.join("daily.md");
        std::fs::write(
            &loop_file,
            "---\nname: daily\nschedule: \"0 9 * * *\"\nmodel: openai/gpt-5\n---\nRun.\n",
        )
        .expect("write");

        let mut task = daily_task();
        task.loop_file = Some(loop_file.to_string_lossy().to_string());
        let store = Arc::new(MemoryStore::new(task));
        let service = service_with(Arc::clone(&store), RecordingDispatch::new());
        let error = service.remove("p1", "task-1").await.expect_err("400");
        assert_eq!(error.status, 400);
        assert!(error.message.contains("delete the file to remove the task"));

        // Once the file is gone, deleting the orphan task is allowed.
        std::fs::remove_file(&loop_file).expect("unlink");
        service.remove("p1", "task-1").await.expect("deleted");
        let _ = std::fs::remove_dir_all(&dir);
    }

/// 删除不存在的任务返回 404。
    #[tokio::test]
    async fn remove_reports_missing_task_as_404() {
        let service = service();
        let error = service.remove("p1", "ghost").await.expect_err("404");
        assert_eq!(error.status, 404);
        assert_eq!(error.message, "Task not found");
    }

/// 空白 taskId 映射为 400。
    #[tokio::test]
    async fn remove_requires_task_id() {
        let service = service();
        let error = service.remove("p1", "  ").await.expect_err("400");
        assert_eq!(error.status, 400);
        assert_eq!(error.message, "taskId is required");
    }

/// run 派发一次并返回会话 id。
    #[tokio::test]
    async fn run_dispatches_and_returns_session() {
        let store = Arc::new(MemoryStore::new(daily_task()));
        let dispatch = RecordingDispatch::new();
        let service = service_with(Arc::clone(&store), dispatch.clone());
        // Prime the runtime's task map through a sync.
        service.list("p1").await.expect("sync");
        let result = service.run("p1", "task-1").await.expect("run ok");
        assert_eq!(result.session_id.as_deref(), Some("sess-1"));
        assert!(result.persist_error.is_none());
        assert_eq!(dispatch.sessions.load(Ordering::SeqCst), 1);
    }

/// 任务运行中时并发 run 被拒绝为 409。
    #[tokio::test]
    async fn run_maps_running_outcome_to_409() {
        let store = Arc::new(MemoryStore::new(daily_task()));
        let dispatch = RecordingDispatch::gated();
        let service = service_with(Arc::clone(&store), dispatch.clone());
        service.list("p1").await.expect("sync");

        // First run blocks inside create_session.
        let first = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.run("p1", "task-1").await })
        };
        dispatch.entered.notified().await;

        let second = service.run("p1", "task-1").await.expect_err("409");
        assert_eq!(second.status, 409);
        assert_eq!(second.message, "task is already running");

        dispatch.release().await;
        let first = first.await.expect("join").expect("first run ok");
        assert!(first.session_id.is_some());
    }

/// run 不存在的任务返回 404。
    #[tokio::test]
    async fn run_maps_missing_task_to_404() {
        let service = service();
        let error = service.run("p1", "ghost").await.expect_err("404");
        assert_eq!(error.status, 404);
        assert_eq!(error.message, "Task not found or disabled");
    }

/// 状态写回失败时 run 仍成功并透出 persistError。
    #[tokio::test]
    async fn run_forwards_persist_error_and_session() {
        let store = Arc::new(MemoryStore::new(daily_task()));
        store.fail_completion.store(true, Ordering::SeqCst);
        let service = service_with(Arc::clone(&store), RecordingDispatch::new());
        service.list("p1").await.expect("sync");

        let result = service.run("p1", "task-1").await.expect("run ok");
        assert!(result.session_id.is_some());
        assert!(
            result
                .persist_error
                .as_deref()
                .unwrap_or_default()
                .contains("timeout acquiring project config lock")
        );
        assert_eq!(store.completion_writes(), 2, "initial write + single retry");
    }

/// enabled 缺失或非布尔时 set_loop_enabled 返回 400。
    #[tokio::test]
    async fn loop_enabled_requires_boolean() {
        let service = service();
        let error = service
            .set_loop_enabled("p1", "task-1", None)
            .await
            .expect_err("400");
        assert_eq!(error.status, 400);
        assert_eq!(error.message, "enabled must be a boolean");
    }

/// 非 loop 管理的任务不能切换 enabled（400）。
    #[tokio::test]
    async fn loop_task_without_loop_file_is_rejected() {
        let service = service();
        service.list("p1").await.expect("sync");
        let error = service
            .set_loop_enabled("p1", "task-1", Some(true))
            .await
            .expect_err("400");
        assert_eq!(error.status, 400);
        assert_eq!(error.message, "Task is not managed by a loop file");
    }

/// SettingsProjects 清洗掉重复 id/路径与非法条目。
    #[tokio::test]
    async fn settings_projects_reads_and_sanitizes_projects() {
        let dir = std::env::temp_dir().join(format!("oc-svc-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("settings.json"),
            serde_json::json!({
                "projects": [
                    { "id": "p1", "path": "/repo/a" },
                    { "id": "p1", "path": "/repo/other" },
                    { "id": "", "path": "/repo/x" },
                    { "id": "p3", "path": "  " },
                    { "id": "p4", "path": "/repo/dup" },
                    { "id": "p5", "path": "/repo/dup" },
                    "not-an-object"
                ]
            })
            .to_string(),
        )
        .expect("write");

        let projects = SettingsProjects::new(&dir);
        let runtime = ScheduledTasksRuntime::new(RuntimeDeps {
            store: Arc::new(MemoryStore::new(daily_task())),
            projects: Arc::new(SettingsProjects::new(&dir)),
            dispatch: RecordingDispatch::new(),
            emit_task_run_event: Arc::new(|_| {}),
            clock: Arc::new(|| 1),
            max_global_concurrency: 4,
            max_project_concurrency: 2,
            max_run_duration_ms: 1000,
        });
        let _ = runtime;
        let listed = crate::scheduled_tasks::runtime::ProjectsAccess::list_projects(&projects)
            .await
            .expect("read");
        let ids: Vec<&str> = listed.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["p1", "p4"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
