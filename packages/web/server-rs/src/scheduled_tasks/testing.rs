//! Shared test doubles for the scheduled-tasks module (runtime, service,
//! routes) — the Rust analog of the JS suites' SDK/store mocks.

#![cfg(test)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::error::{AppError, AppResult};
use crate::projects::{Execution, Schedule, ScheduledTask, TaskState};

use super::dispatch::{BoxFut, EngineDispatch, ScheduledCommand};
use super::runtime::{
    Clock, DEFAULT_GLOBAL_CONCURRENCY, DEFAULT_MAX_RUN_MS, DEFAULT_PROJECT_CONCURRENCY, ProjectRef,
    ProjectsAccess, RunOutcome, RuntimeDeps, ScheduledTaskStore, ScheduledTasksRuntime,
    SharedRuntime,
};

/// In-memory stand-in for the shared project config (the JS issue-2710
/// suite's `createSharedProjectConfigRuntime`): every runtime observing the
/// store sees the same task state, so occurrence claiming serializes.
pub struct MemoryStore {
    task: Mutex<Option<ScheduledTask>>,
    pub fail_claim: AtomicBool,
    pub fail_completion: AtomicBool,
    pub fail_start: AtomicBool,
    pub claim_attempts: AtomicUsize,
    completion_writes: AtomicUsize,
}

impl MemoryStore {
    pub fn new(task: ScheduledTask) -> Self {
        Self {
            task: Mutex::new(Some(task)),
            fail_claim: AtomicBool::new(false),
            fail_completion: AtomicBool::new(false),
            fail_start: AtomicBool::new(false),
            claim_attempts: AtomicUsize::new(0),
            completion_writes: AtomicUsize::new(0),
        }
    }

    /// The stored task, or a default (deleted) marker for assertions.
    pub fn current(&self) -> ScheduledTask {
        self.task
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(daily_task)
    }

    pub fn completion_writes(&self) -> usize {
        self.completion_writes.load(Ordering::SeqCst)
    }

    fn apply_patch(&self, patch: &Value) -> ScheduledTask {
        let mut guard = self.task.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut task) = guard.clone() else {
            return daily_task();
        };
        let state_value = serde_json::to_value(&task.state).unwrap_or_else(|_| json!({}));
        let mut merged = state_value.as_object().cloned().unwrap_or_default();
        if let Some(fields) = patch.as_object() {
            for (key, value) in fields {
                merged.insert(key.clone(), value.clone());
            }
        }
        merged.insert("updatedAt".into(), json!(1_750_000_000_000u64));
        if let Ok(next) = serde_json::from_value::<TaskState>(Value::Object(merged)) {
            task.state = next;
        }
        *guard = Some(task.clone());
        task
    }
}

impl ScheduledTaskStore for MemoryStore {
    fn list_scheduled_tasks<'a>(
        &'a self,
        _project_id: &'a str,
    ) -> BoxFut<'a, AppResult<Vec<ScheduledTask>>> {
        Box::pin(async {
            Ok(self
                .task
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .into_iter()
                .collect())
        })
    }

    fn upsert_scheduled_task<'a>(
        &'a self,
        _project_id: &'a str,
        input: &'a Value,
    ) -> BoxFut<'a, AppResult<crate::projects::UpsertResult>> {
        Box::pin(async {
            let parsed: ScheduledTask = serde_json::from_value(input.clone())
                .map_err(|e| AppError::internal(format!("invalid task: {e}")))?;
            *self.task.lock().unwrap_or_else(|e| e.into_inner()) = Some(parsed.clone());
            Ok(crate::projects::UpsertResult {
                task: parsed,
                tasks: Vec::new(),
                created: false,
            })
        })
    }

    fn delete_scheduled_task<'a>(
        &'a self,
        _project_id: &'a str,
        task_id: &'a str,
    ) -> BoxFut<'a, AppResult<crate::projects::DeleteResult>> {
        Box::pin(async move {
            let mut guard = self.task.lock().unwrap_or_else(|e| e.into_inner());
            let deleted = guard.as_ref().is_some_and(|task| task.id == task_id);
            if deleted {
                *guard = None;
            }
            Ok(crate::projects::DeleteResult {
                deleted,
                tasks: Vec::new(),
            })
        })
    }

    fn update_scheduled_task_state<'a>(
        &'a self,
        _project_id: &'a str,
        _task_id: &'a str,
        patch: &'a Value,
    ) -> BoxFut<'a, AppResult<crate::projects::StateUpdateResult>> {
        Box::pin(async {
            use crate::projects::StateUpdateResult;
            let last_status = patch.get("lastStatus").and_then(Value::as_str);
            if self.fail_start.load(Ordering::SeqCst) && last_status == Some("running") {
                return Err(AppError::internal(
                    "timeout acquiring project config lock for p1",
                ));
            }
            if matches!(last_status, Some("success") | Some("error")) {
                self.completion_writes.fetch_add(1, Ordering::SeqCst);
                if self.fail_completion.load(Ordering::SeqCst) {
                    return Err(AppError::internal(
                        "timeout acquiring project config lock for p1",
                    ));
                }
            }
            let task = self.apply_patch(patch);
            Ok(StateUpdateResult {
                task: Some(task),
                tasks: Vec::new(),
                updated: true,
            })
        })
    }

    fn update_scheduled_task_state_if<'a>(
        &'a self,
        _project_id: &'a str,
        _task_id: &'a str,
        predicate: Box<dyn Fn(&ScheduledTask) -> bool + Send + Sync>,
        patch: &'a Value,
    ) -> BoxFut<'a, AppResult<crate::projects::StateUpdateResult>> {
        Box::pin(async move {
            use crate::projects::StateUpdateResult;
            if patch.get("lastScheduledFor").is_some() {
                self.claim_attempts.fetch_add(1, Ordering::SeqCst);
                if self.fail_claim.load(Ordering::SeqCst) {
                    return Err(AppError::internal(
                        "timeout acquiring project config lock for p1",
                    ));
                }
            }
            let current = self.current();
            if !predicate(&current) {
                return Ok(StateUpdateResult {
                    task: Some(current),
                    tasks: Vec::new(),
                    updated: false,
                });
            }
            let task = self.apply_patch(patch);
            Ok(StateUpdateResult {
                task: Some(task),
                tasks: Vec::new(),
                updated: true,
            })
        })
    }

    fn reconcile_loop_tasks<'a>(
        &'a self,
        _project_id: &'a str,
        _loops: &'a [crate::projects::LoopEntry],
    ) -> BoxFut<'a, AppResult<Vec<ScheduledTask>>> {
        Box::pin(async { self.list_scheduled_tasks(_project_id).await })
    }
}

pub struct MemoryProjects {
    pub projects: Vec<ProjectRef>,
}

impl ProjectsAccess for MemoryProjects {
    fn list_projects(&self) -> BoxFut<'static, AppResult<Vec<ProjectRef>>> {
        let projects = self.projects.clone();
        Box::pin(async move { Ok(projects) })
    }
}

/// Recording dispatch double (JS SDK + fetch mocks). `gate` blocks
/// `create_session` until released, emulating a slow engine.
pub struct RecordingDispatch {
    pub sessions: AtomicUsize,
    pub entered: tokio::sync::Notify,
    gate: tokio::sync::Notify,
    gated: bool,
    pub created_titles: Mutex<Vec<String>>,
    pub prompts: Mutex<Vec<Value>>,
    pub command_runs: Mutex<Vec<String>>,
}

impl RecordingDispatch {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            sessions: AtomicUsize::new(0),
            entered: tokio::sync::Notify::new(),
            gate: tokio::sync::Notify::new(),
            gated: false,
            created_titles: Mutex::new(Vec::new()),
            prompts: Mutex::new(Vec::new()),
            command_runs: Mutex::new(Vec::new()),
        })
    }

    pub fn gated() -> Arc<Self> {
        Arc::new(Self {
            sessions: AtomicUsize::new(0),
            entered: tokio::sync::Notify::new(),
            gate: tokio::sync::Notify::new(),
            gated: true,
            created_titles: Mutex::new(Vec::new()),
            prompts: Mutex::new(Vec::new()),
            command_runs: Mutex::new(Vec::new()),
        })
    }

    pub async fn release(&self) {
        self.gate.notify_waiters();
    }
}

impl EngineDispatch for RecordingDispatch {
    fn wait_ready(&self) -> BoxFut<'_, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn create_session(&self, _directory: &str, title: &str) -> BoxFut<'_, Result<String, String>> {
        let n = self.sessions.fetch_add(1, Ordering::SeqCst) + 1;
        self.created_titles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(title.to_string());
        let gate = &self.gate;
        let gated = self.gated;
        let entered = &self.entered;
        Box::pin(async move {
            if gated {
                entered.notify_one();
                gate.notified().await;
            }
            Ok(format!("sess-{n}"))
        })
    }

    fn list_commands(&self, _directory: &str) -> BoxFut<'_, Result<Vec<ScheduledCommand>, String>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn run_session_command(
        &self,
        session_id: &str,
        _directory: &str,
        _command: &ScheduledCommand,
        _execution: &Execution,
    ) -> BoxFut<'_, Result<(), String>> {
        self.command_runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(session_id.to_string());
        Box::pin(async { Ok(()) })
    }

    fn prompt_async(
        &self,
        _session_id: &str,
        _directory: &str,
        payload: &Value,
    ) -> BoxFut<'_, Result<(), String>> {
        self.prompts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(payload.clone());
        Box::pin(async { Ok(()) })
    }

    fn create_session_goal(
        &self,
        _session_id: &str,
        _directory: &str,
        _objective: &str,
        _token_budget: Option<u64>,
        _provider_id: Option<&str>,
        _model_id: Option<&str>,
    ) -> BoxFut<'_, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn set_session_auto_accept(
        &self,
        _session_id: &str,
        _enabled: bool,
        _directory: &str,
    ) -> BoxFut<'_, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

pub fn daily_task() -> ScheduledTask {
    ScheduledTask {
        id: "task-1".into(),
        name: "Daily Sync".into(),
        enabled: true,
        schedule: Schedule::Daily {
            times: vec!["15:00".into()],
            timezone: "UTC".into(),
        },
        execution: Execution {
            prompt: "Summarize open issues".into(),
            provider_id: Some("openai".into()),
            model_id: Some("gpt-4o".into()),
            model_role: None,
            variant: None,
            agent: None,
            goal_enabled: None,
            goal_token_budget: None,
            permission_auto_accept: None,
        },
        state: TaskState::default(),
        loop_file: None,
    }
}

pub fn once_task() -> ScheduledTask {
    let mut task = daily_task();
    task.schedule = Schedule::Once {
        date: "2026-01-01".into(),
        time: "09:00".into(),
        timezone: "UTC".into(),
    };
    task
}

pub fn make_runtime(
    store: Arc<MemoryStore>,
    dispatch: Arc<dyn EngineDispatch>,
    now_ms: i64,
) -> SharedRuntime {
    let clock: Clock = Arc::new(move || now_ms);
    let deps = RuntimeDeps {
        store,
        projects: Arc::new(MemoryProjects {
            projects: vec![ProjectRef {
                id: "p1".into(),
                path: "/repo".into(),
            }],
        }),
        dispatch,
        emit_task_run_event: Arc::new(|_| {}),
        clock,
        max_global_concurrency: DEFAULT_GLOBAL_CONCURRENCY,
        max_project_concurrency: DEFAULT_PROJECT_CONCURRENCY,
        max_run_duration_ms: DEFAULT_MAX_RUN_MS,
    };
    ScheduledTasksRuntime::new(deps)
}

/// Load a task into the runtime's in-memory maps without touching the store
/// (the JS tests prime timers through `start()` with fakes instead).
pub fn prime(runtime: &SharedRuntime, task: ScheduledTask) {
    let mut state = runtime.state_for_tests();
    let mut map = HashMap::new();
    map.insert(task.id.clone(), task);
    state.tasks_by_project.insert("p1".into(), map);
    state.project_path_by_id.insert("p1".into(), "/repo".into());
}

/// A quiet run outcome builder for service-level fakes.
pub fn outcome_default() -> RunOutcome {
    RunOutcome::default()
}
