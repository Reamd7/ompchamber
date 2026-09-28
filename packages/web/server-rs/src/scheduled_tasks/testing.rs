//! Shared test doubles for the scheduled-tasks module (runtime, service,
//! routes) — the Rust analog of the JS suites' SDK/store mocks.
//!
//! scheduled_tasks（runtime/service/routes）共享的测试替身集合：
//! 内存版 store、projects 与 dispatch，等价于 JS 套件中的
//! createSharedProjectConfigRuntime 和 SDK/fetch mock。

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
/// 共享项目配置的内存替身：所有观察该 store 的 runtime 看到同一份
/// 任务状态，occurrence 认领因此得以串行化验证。
pub struct MemoryStore {
    /// 当前唯一持有的任务（None 表示已删除）；读写都经这把互斥锁。
    task: Mutex<Option<ScheduledTask>>,
    /// 测试开关：置 true 后带 lastScheduledFor 的条件更新报锁超时错误。
    pub fail_claim: AtomicBool,
    /// 测试开关：置 true 后完成态（success/error）写入报锁超时错误。
    pub fail_completion: AtomicBool,
    /// 测试开关：置 true 后置 running 的状态更新报锁超时错误。
    pub fail_start: AtomicBool,
    /// 认领尝试（lastScheduledFor 条件更新）的累计次数，供断言重试。
    pub claim_attempts: AtomicUsize,
    /// 完成态写入的累计次数（含失败那次）。
    completion_writes: AtomicUsize,
}

/// 构造、查询与补丁合并辅助。
impl MemoryStore {
    /// 以初始任务构造 store；失败开关全部关闭。
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
    /// 读当前任务；已删除时返回 daily_task() 作（已删除）占位便于断言。
    pub fn current(&self) -> ScheduledTask {
        self.task
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(daily_task)
    }

    /// 完成态写入次数（SeqCst 读取）。
    pub fn completion_writes(&self) -> usize {
        self.completion_writes.load(Ordering::SeqCst)
    }

    /// 把 JSON 补丁浅合并进任务 state（updatedAt 固定为常量），
    /// 仅当结果能反序列化回 TaskState 时才落盘。
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

/// ScheduledTaskStore 的内存实现：单任务语义，带可注入的失败开关。
impl ScheduledTaskStore for MemoryStore {
    /// 返回内存中的（至多一个）任务。
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

    /// 反序列化并整体替换任务；非法输入返回内部错误。
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

    /// id 匹配时删除任务，并报告是否真的删除。
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

    /// 无条件状态更新：按 last_status 触发 fail_start/fail_completion
    /// 开关，随后合并补丁。
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

    /// 条件状态更新（occurrence 认领）：lastScheduledFor 补丁触发认领
    /// 计数与 fail_claim；谓词不满足时不写入并返回 updated=false。
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

    /// 循环任务调和在此替身为空操作，仅回读任务列表。
    fn reconcile_loop_tasks<'a>(
        &'a self,
        _project_id: &'a str,
        _loops: &'a [crate::projects::LoopEntry],
    ) -> BoxFut<'a, AppResult<Vec<ScheduledTask>>> {
        Box::pin(async { self.list_scheduled_tasks(_project_id).await })
    }
}

/// projects 访问的内存替身：固定返回构造时给定的项目引用。
pub struct MemoryProjects {
    /// list_projects 直接克隆返回的固定列表。
    pub projects: Vec<ProjectRef>,
}

/// ProjectsAccess 的内存实现。
impl ProjectsAccess for MemoryProjects {
    /// 返回固定项目集合。
    fn list_projects(&self) -> BoxFut<'static, AppResult<Vec<ProjectRef>>> {
        let projects = self.projects.clone();
        Box::pin(async move { Ok(projects) })
    }
}

/// Recording dispatch double (JS SDK + fetch mocks). `gate` blocks
/// `create_session` until released, emulating a slow engine.
/// 记录型 dispatch 替身（对应 JS 的 SDK + fetch mock）：gated 模式下
/// create_session 阻塞直到 release，用于模拟慢引擎。
pub struct RecordingDispatch {
    /// 已创建会话的计数（兼作会话 id 序号）。
    pub sessions: AtomicUsize,
    /// gated create_session 已进入阻塞的通知点，测试据此同步。
    pub entered: tokio::sync::Notify,
    /// release() 用来唤醒被阻塞 create_session 的 Notify。
    gate: tokio::sync::Notify,
    /// 是否启用 create_session 门闩。
    gated: bool,
    /// 记录每次 create_session 的 title。
    pub created_titles: Mutex<Vec<String>>,
    /// 记录每次 prompt_async 的 payload。
    pub prompts: Mutex<Vec<Value>>,
    /// 记录每次 run_session_command 的会话 id。
    pub command_runs: Mutex<Vec<String>>,
}

/// 替身构造与门闩控制。
impl RecordingDispatch {
    /// 构造无门闩（create_session 立即返回）的替身。
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

    /// 构造带门闩的替身：create_session 需等待 release()。
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

    /// 放行所有被门闩阻塞的 create_session。
    pub async fn release(&self) {
        self.gate.notify_waiters();
    }
}

/// EngineDispatch 的记录实现：不触网，全部立即成功并留下调用痕迹；
/// goal 与自动放行为空操作。
impl EngineDispatch for RecordingDispatch {
    /// 立即就绪。
    fn wait_ready(&self) -> BoxFut<'_, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    /// 计数并记录 title；gated 时先通知 entered 再等门闩；返回 sess-N。
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

    /// 返回空命令表。
    fn list_commands(&self, _directory: &str) -> BoxFut<'_, Result<Vec<ScheduledCommand>, String>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    /// 记录会话 id 即成功。
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

    /// 记录 payload 即成功。
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

    /// 空操作成功。
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

    /// 空操作成功。
    fn set_session_auto_accept(
        &self,
        _session_id: &str,
        _enabled: bool,
        _directory: &str,
    ) -> BoxFut<'_, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

/// 造一个每天 15:00 UTC 触发的示例任务（openai/gpt-4o）。
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

/// 在 daily_task 基础上改成 2026-01-01 09:00 UTC 的一次性任务。
pub fn once_task() -> ScheduledTask {
    let mut task = daily_task();
    task.schedule = Schedule::Once {
        date: "2026-01-01".into(),
        time: "09:00".into(),
        timezone: "UTC".into(),
    };
    task
}

/// 用固定时钟（now_ms 恒定）加内存 projects/dispatch 组装 SharedRuntime，
/// 并发限额与运行时长取默认值。
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
/// 把任务直接塞进 runtime 的内存映射（绕过 store），对应 JS 测试用
/// fake start 预置定时器的做法。
pub fn prime(runtime: &SharedRuntime, task: ScheduledTask) {
    let mut state = runtime.state_for_tests();
    let mut map = HashMap::new();
    map.insert(task.id.clone(), task);
    state.tasks_by_project.insert("p1".into(), map);
    state.project_path_by_id.insert("p1".into(), "/repo".into());
}

/// A quiet run outcome builder for service-level fakes.
/// 构造安静的默认 RunOutcome，供 service 层替身使用。
pub fn outcome_default() -> RunOutcome {
    RunOutcome::default()
}
