//! Port of `server/lib/scheduled-tasks/service.js` — the route-facing task
//! service (CRUD, loop-file mutations, manual run, status).
//!
//! `OMPChamberControlError` maps to [`ServiceError`] (status + message, and
//! for run failures the offending task).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};

use crate::error::AppResult;
use crate::projects::ScheduledTask;

use super::loops::set_loop_file_enabled;
use super::runtime::{ProjectRef, ProjectsAccess, ScheduledTaskStore, SharedRuntime};

/// `OMPChamberControlError { statusCode, message, task? }`.
#[derive(Debug, Clone)]
pub struct ServiceError {
    pub status: u16,
    pub message: String,
    pub task: Option<Value>,
}

impl ServiceError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            task: None,
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, message)
    }
}

/// `listProjects` — settings.json `projects` entries, sanitized to the
/// id/path subset this module consumes (full-field sanitization belongs to
/// the settings module port).
pub struct SettingsProjects {
    settings_path: PathBuf,
}

impl SettingsProjects {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            settings_path: data_dir.join("settings.json"),
        }
    }

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

impl ProjectsAccess for SettingsProjects {
    fn list_projects(&self) -> super::dispatch::BoxFut<'static, AppResult<Vec<ProjectRef>>> {
        let projects = self.read();
        Box::pin(async move { Ok(projects) })
    }
}

pub struct ScheduledTaskService {
    projects: Arc<dyn ProjectsAccess>,
    store: Arc<dyn ScheduledTaskStore>,
    runtime: SharedRuntime,
}

#[derive(Debug)]
pub struct RunSuccess {
    pub task: Option<ScheduledTask>,
    pub session_id: Option<String>,
    pub persist_error: Option<String>,
}

impl ScheduledTaskService {
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

    async fn list_projects(&self) -> AppResult<Vec<ProjectRef>> {
        self.projects.list_projects().await
    }

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
    pub async fn list(&self, project_id: &str) -> Result<Vec<ScheduledTask>, ServiceError> {
        self.find_project_by_id(project_id).await?;
        self.runtime
            .sync_project(&non_empty(project_id).unwrap_or_default())
            .await
            .map_err(|error| ServiceError::new(500, error.to_string()))
    }

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
    pub fn status(&self) -> Value {
        self.runtime.get_status()
    }
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduled_tasks::runtime::{RuntimeDeps, ScheduledTasksRuntime};
    use crate::scheduled_tasks::testing::{
        MemoryProjects, MemoryStore, RecordingDispatch, daily_task, make_runtime,
    };
    use std::sync::atomic::Ordering;

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

    fn service() -> Arc<ScheduledTaskService> {
        service_with(
            Arc::new(MemoryStore::new(daily_task())),
            RecordingDispatch::new(),
        )
    }

    #[tokio::test]
    async fn list_reconciles_and_returns_tasks() {
        let service = service();
        let tasks = service.list("p1").await.expect("list ok");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "task-1");
    }

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

    #[tokio::test]
    async fn blank_project_id_is_400() {
        let service = service();
        let error = service.list("   ").await.expect_err("400");
        assert_eq!(error.status, 400);
        assert_eq!(error.message, "projectId is required");
    }

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

    #[tokio::test]
    async fn remove_reports_missing_task_as_404() {
        let service = service();
        let error = service.remove("p1", "ghost").await.expect_err("404");
        assert_eq!(error.status, 404);
        assert_eq!(error.message, "Task not found");
    }

    #[tokio::test]
    async fn remove_requires_task_id() {
        let service = service();
        let error = service.remove("p1", "  ").await.expect_err("400");
        assert_eq!(error.status, 400);
        assert_eq!(error.message, "taskId is required");
    }

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

    #[tokio::test]
    async fn run_maps_missing_task_to_404() {
        let service = service();
        let error = service.run("p1", "ghost").await.expect_err("404");
        assert_eq!(error.status, 404);
        assert_eq!(error.message, "Task not found or disabled");
    }

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
