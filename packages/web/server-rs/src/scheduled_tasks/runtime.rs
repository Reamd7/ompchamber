//! Port of `server/lib/scheduled-tasks/runtime.js` — the scheduler runtime.
//!
//! Responsibilities (see scheduled-tasks/DOCUMENTATION.md):
//! - per-project task maps armed with tokio timers (+ up to 2 s jitter),
//! - a dispatch queue with global (4) and per-project (2) concurrency caps,
//! - cross-instance occurrence claiming before scheduled runs (#2710):
//!   `lastScheduledFor` within `TASK_DUE_SLACK_MS` decides the winner,
//! - run lifecycle state writes with the JS error/retry semantics (claim
//!   failures release the running slot, completion-write failures keep the
//!   session and surface `persistError`),
//! - one-time task consumption and next-occurrence re-arming.
//!
//! `Date.now()` is the injectable [`Clock`]; engine calls sit behind
//! [`EngineDispatch`]; persistence behind [`ScheduledTaskStore`] (implemented
//! by the ported `projects` project-config runtime).
//!
//! 中文概述：定时任务调度运行时（JS runtime.js 的移植）。核心组成：
//! 每项目任务表 + tokio 定时器（最多 2 秒抖动）、带全局/单项目并发
//! 上限的派发队列、跨实例占位声明（#2710，防止多实例重复跑同一次
//! 计划）、完整运行生命周期的状态写回（含失败重试语义），以及
//! 一次性任务消费与下次占位重武装。时钟、引擎调用与持久化都是
//! 可注入 seam，测试用内存替身驱动全流程。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::error::AppResult;
use crate::projects::{
    DeleteResult, LoopEntry, ProjectConfigRuntime, Schedule, ScheduledTask, StateUpdateResult,
    UpsertResult,
};

use super::compute::{
    TASK_DUE_SLACK_MS, build_goal_intro_text, compute_next_run_at, expand_command_goal_objective,
    format_scheduled_session_title, parse_scheduled_command_prompt, safe_error_message,
};
use super::dispatch::{BoxFut, EngineDispatch, ScheduledCommand};
use super::loops::{DiscoveredLoop, discover_loops};
use super::snippets::expand_snippets;

/// 默认全局并发上限：整个进程同时最多 4 个定时任务在跑。
pub const DEFAULT_GLOBAL_CONCURRENCY: usize = 4;
/// 默认单项目并发上限：同一项目同时最多 2 个任务。
pub const DEFAULT_PROJECT_CONCURRENCY: usize = 2;
/// 单次运行的 watchdog 时长：30 分钟，超时按 error 收尾。
pub const DEFAULT_MAX_RUN_MS: u64 = 30 * 60 * 1000;
/// 定时器附加抖动上限（毫秒）：错开同时到期的任务，避免惊群。
pub const JITTER_MAX_MS: i64 = 2_000;

/// Wall clock in unix ms (JS `Date.now()`).
///
/// 中文补充：调度、watchdog 与全部状态时间戳都取自该时钟。
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// 生产时钟：SystemTime 换算的 unix 毫秒（早于 epoch 时取 0）。
pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    })
}

/// `emitTaskRunEvent` — publishes `ompchamber:scheduled-task-ran` frames.
///
/// 中文补充：running 与终态（success/error）各发一次，由上层接到
/// SSE 广播。
#[derive(Debug, Clone)]
pub struct TaskRunEvent {
    /// 项目 id。
    pub project_id: String,
    /// 任务 id。
    pub task_id: String,
    /// 事件产生时间（unix ms）。
    pub ran_at: i64,
    /// 事件状态：running / success / error。
    pub status: String,
    /// 关联的引擎会话 id（running 起始即携带）。
    pub session_id: Option<String>,
}

/// 任务运行事件的发射回调类型：由上层接线到 SSE 广播通道。
pub type EmitTaskRunEvent = Arc<dyn Fn(&TaskRunEvent) + Send + Sync>;

/// 触发本次运行的原因：定时（携带占位时间戳）或手动。
#[derive(Debug, Clone)]
pub enum RunReason {
    /// `scheduled_for` is the armed occurrence timestamp (ms). `None` mirrors
    /// the JS `missing-scheduled-for` path (a scheduled queue item without a
    /// finite occurrence).
    ///
    /// 中文补充：定时触发；占位时间戳同时用于跨实例去重。
    Scheduled {
        /// 触发该运行的计划占位时间戳；None 走 missing-scheduled-for
        /// 跳过路径（JS 同名语义）。
        scheduled_for: Option<i64>,
    },
    /// 手动触发（runNow）：不参与占位声明，也不消费一次性任务。
    Manual,
}

/// RunReason 的字符串表示实现。
impl RunReason {
    /// 返回 "scheduled" 或 "manual"，用于日志与事件。
    pub fn as_str(&self) -> &'static str {
        match self {
            RunReason::Scheduled { .. } => "scheduled",
            RunReason::Manual => "manual",
        }
    }
}

/// Result of one `runTask`/`runNow` invocation (JS return shapes unified).
///
/// 中文补充：reason 是机器可读的结果归类（occurrence-claimed、
/// claim-failed、completion-state-failed 等），配合各布尔位还原 JS
/// 的多种返回形状。
#[derive(Debug, Clone, Default)]
pub struct RunOutcome {
    /// 本次运行是否成功（完成状态写回失败不影响该位）。
    pub ok: bool,
    /// 因跳过（禁用/缺失/占位被抢等）未执行。
    pub skipped: bool,
    /// 去重命中：任务已在运行中。
    pub running: bool,
    /// 去重命中：任务已在队列中（仅 runNow 检查）。
    pub queued: bool,
    /// 终态："success" 或 "error"。
    pub status: Option<String>,
    /// 创建的引擎会话 id（派发启动后可用）。
    pub session_id: Option<String>,
    /// 运行结束后的最新任务快照（含状态回写结果）。
    pub task: Option<ScheduledTask>,
    /// 运行/启动失败的错误消息（已截断脱敏）。
    pub error: Option<String>,
    /// 完成状态写回失败的错误消息（会话本身可能已成功）。
    pub persist_error: Option<String>,
    /// 机器可读的结果原因码。
    pub reason: Option<String>,
}

/// Persistence seam over the ported project-config runtime (JS injects the
/// same object from server/index.js); tests may substitute a double.
///
/// 中文补充：方法集合与 JS 注入的 project-config runtime 对象一一
/// 对应；统一返回 BoxFut 让 trait 保持对象安全。
pub trait ScheduledTaskStore: Send + Sync {
    /// 列出某项目的全部定时任务。
    fn list_scheduled_tasks<'a>(
        &'a self,
        project_id: &'a str,
    ) -> BoxFut<'a, AppResult<Vec<ScheduledTask>>>;
    /// 新增或整体更新一个任务（一次性任务消费复用该入口）。
    fn upsert_scheduled_task<'a>(
        &'a self,
        project_id: &'a str,
        input: &'a Value,
    ) -> BoxFut<'a, AppResult<UpsertResult>>;
    /// 删除任务。
    fn delete_scheduled_task<'a>(
        &'a self,
        project_id: &'a str,
        task_id: &'a str,
    ) -> BoxFut<'a, AppResult<DeleteResult>>;
    /// 无条件更新任务运行状态（patch 合并写回）。
    fn update_scheduled_task_state<'a>(
        &'a self,
        project_id: &'a str,
        task_id: &'a str,
        patch: &'a Value,
    ) -> BoxFut<'a, AppResult<StateUpdateResult>>;
    /// 条件更新：predicate 通过才应用 patch（占位声明的 CAS 语义）。
    fn update_scheduled_task_state_if<'a>(
        &'a self,
        project_id: &'a str,
        task_id: &'a str,
        predicate: Box<dyn Fn(&ScheduledTask) -> bool + Send + Sync>,
        patch: &'a Value,
    ) -> BoxFut<'a, AppResult<StateUpdateResult>>;
    /// 把发现的 loop 文件与持久化任务对账（增删同步）。
    fn reconcile_loop_tasks<'a>(
        &'a self,
        project_id: &'a str,
        loops: &'a [LoopEntry],
    ) -> BoxFut<'a, AppResult<Vec<ScheduledTask>>>;
}

/// 用真实的项目配置 runtime 实现存储 seam：把固有异步方法装箱成
/// BoxFut 以满足对象安全，语义完全委托原实现。
impl ScheduledTaskStore for ProjectConfigRuntime {
    /// 委托 ProjectConfigRuntime::list_scheduled_tasks。
    fn list_scheduled_tasks<'a>(
        &'a self,
        project_id: &'a str,
    ) -> BoxFut<'a, AppResult<Vec<ScheduledTask>>> {
        Box::pin(ProjectConfigRuntime::list_scheduled_tasks(self, project_id))
    }
    /// 委托 ProjectConfigRuntime::upsert_scheduled_task。
    fn upsert_scheduled_task<'a>(
        &'a self,
        project_id: &'a str,
        input: &'a Value,
    ) -> BoxFut<'a, AppResult<UpsertResult>> {
        Box::pin(ProjectConfigRuntime::upsert_scheduled_task(
            self, project_id, input,
        ))
    }
    /// 委托 ProjectConfigRuntime::delete_scheduled_task。
    fn delete_scheduled_task<'a>(
        &'a self,
        project_id: &'a str,
        task_id: &'a str,
    ) -> BoxFut<'a, AppResult<DeleteResult>> {
        Box::pin(ProjectConfigRuntime::delete_scheduled_task(
            self, project_id, task_id,
        ))
    }
    /// 委托 ProjectConfigRuntime::update_scheduled_task_state。
    fn update_scheduled_task_state<'a>(
        &'a self,
        project_id: &'a str,
        task_id: &'a str,
        patch: &'a Value,
    ) -> BoxFut<'a, AppResult<StateUpdateResult>> {
        Box::pin(ProjectConfigRuntime::update_scheduled_task_state(
            self, project_id, task_id, patch,
        ))
    }
    /// 委托 update_scheduled_task_state_if；谓词闭包包一层以匹配签名。
    fn update_scheduled_task_state_if<'a>(
        &'a self,
        project_id: &'a str,
        task_id: &'a str,
        predicate: Box<dyn Fn(&ScheduledTask) -> bool + Send + Sync>,
        patch: &'a Value,
    ) -> BoxFut<'a, AppResult<StateUpdateResult>> {
        let wrapper = move |task: &ScheduledTask| predicate(task);
        Box::pin(async move {
            ProjectConfigRuntime::update_scheduled_task_state_if(
                self, project_id, task_id, &wrapper, patch,
            )
            .await
        })
    }
    /// 委托 ProjectConfigRuntime::reconcile_loop_tasks。
    fn reconcile_loop_tasks<'a>(
        &'a self,
        project_id: &'a str,
        loops: &'a [LoopEntry],
    ) -> BoxFut<'a, AppResult<Vec<ScheduledTask>>> {
        Box::pin(ProjectConfigRuntime::reconcile_loop_tasks(
            self, project_id, loops,
        ))
    }
}

/// A discovered project (`listProjects` → settings.json projects).
///
/// 中文补充：调度器只消费 id 与路径这两个字段。
#[derive(Debug, Clone)]
pub struct ProjectRef {
    /// 项目 id（settings.json projects 的键）。
    pub id: String,
    /// 项目根目录路径（loop 发现与引擎会话的 cwd）。
    pub path: String,
}

/// 项目列表访问 seam（生产来自 settings.json 的 projects 配置，
/// 测试可注入内存列表）。
pub trait ProjectsAccess: Send + Sync {
    /// 返回全部已发现项目；失败会让同步流程整体报错。
    fn list_projects(&self) -> BoxFut<'static, AppResult<Vec<ProjectRef>>>;
}

/// 运行时的全部依赖注入点：存储、项目列表、引擎派发、事件发射、
/// 时钟与三组并发/时长上限。
pub struct RuntimeDeps {
    /// 持久化 seam（见 ScheduledTaskStore）。
    pub store: Arc<dyn ScheduledTaskStore>,
    /// 项目列表 seam（见 ProjectsAccess）。
    pub projects: Arc<dyn ProjectsAccess>,
    /// 引擎派发 seam（会话创建、prompt、命令执行）。
    pub dispatch: Arc<dyn EngineDispatch>,
    /// 任务运行事件的发射回调。
    pub emit_task_run_event: EmitTaskRunEvent,
    /// 可注入时钟（unix ms）。
    pub clock: Clock,
    /// 全局并发上限。
    pub max_global_concurrency: usize,
    /// 单项目并发上限。
    pub max_project_concurrency: usize,
    /// 单次运行 watchdog 时长（ms）。
    pub max_run_duration_ms: u64,
}

/// 派发队列中的一项：哪个项目的哪个任务、按哪个占位时间触发。
#[derive(Debug, Clone)]
struct QueueItem {
    /// 项目 id。
    project_id: String,
    /// 任务 id。
    task_id: String,
    /// 触发本次排队的计划占位时间戳；入队路径总是 Some。
    scheduled_for: Option<i64>,
}

/// 单把 Mutex 保护的全部可变调度状态（任务表、定时器、队列与计数）。
/// 字段只在持锁期间读写；pub(crate) 仅为让测试注入。
#[derive(Default)]
pub(crate) struct RuntimeState {
    /// start() 已置位：定时器允许武装、队列允许泵送。
    pub(crate) started: bool,
    /// 每项目的任务内存快照（任务 id -> 任务）。
    pub(crate) tasks_by_project: HashMap<String, HashMap<String, ScheduledTask>>,
    /// 项目 id -> 根路径缓存。
    pub(crate) project_path_by_id: HashMap<String, String>,
    /// task_key -> 定时器句柄（重武装前先 abort 旧句柄）。
    timers_by_task_key: HashMap<String, tokio::task::JoinHandle<()>>,
    /// 已入队任务的 task_key 集合（防重复入队）。
    queued_task_keys: HashSet<String>,
    /// 正在运行任务的 task_key 集合。
    running_task_keys: HashSet<String>,
    /// 每项目正在运行的任务数。
    running_count_by_project: HashMap<String, usize>,
    /// 全局正在运行的任务数。
    running_global_count: usize,
    /// 待派发队列（FIFO）。
    queue: VecDeque<QueueItem>,
}

/// 内存状态的小型辅助操作。
impl RuntimeState {
    /// 用最新快照覆盖内存中的任务；项目不存在时静默丢弃（等下次 sync）。
    fn update_in_memory_task(&mut self, project_id: &str, task: ScheduledTask) {
        if let Some(map) = self.tasks_by_project.get_mut(project_id) {
            map.insert(task.id.clone(), task);
        }
    }
}

/// 调度器本体：不可变依赖 + 一把 Mutex 保护的状态。
pub struct ScheduledTasksRuntime {
    /// 注入的依赖集合。
    deps: RuntimeDeps,
    /// 全部可变状态（含定时器句柄与队列）。
    state: Mutex<RuntimeState>,
}

/// 运行时共享句柄：定时器任务与泵送级联都持有它。
pub type SharedRuntime = Arc<ScheduledTasksRuntime>;

/// 拼接 task_key（`{project_id}:{task_id}`），作为定时器/队列/运行的
/// 统一去重键。
fn build_task_key(project_id: &str, task_id: &str) -> String {
    format!("{project_id}:{task_id}")
}

/// 调度器的全部操作：定时器管理、项目同步、队列泵送与运行生命周期。
impl ScheduledTasksRuntime {
    /// 构造共享运行时（未启动、空任务表）。
    pub fn new(deps: RuntimeDeps) -> SharedRuntime {
        Arc::new(Self {
            deps,
            state: Mutex::new(RuntimeState::default()),
        })
    }

    /// 加锁；锁中毒（某持有者 panic）时恢复并继续，保证调度器不死锁。
    fn lock_state(&self) -> std::sync::MutexGuard<'_, RuntimeState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 测试专用：直接暴露状态守卫以注入任务表。
    #[cfg(test)]
    pub(crate) fn state_for_tests(&self) -> std::sync::MutexGuard<'_, RuntimeState> {
        self.lock_state()
    }

    /// 当前时钟读数（unix ms）。
    fn now_ms(&self) -> i64 {
        (self.deps.clock)()
    }

    // -- timers ------------------------------------------------------------

    /// 摘除并 abort 指定 task_key 的定时器（不存在则不动）。
    fn clear_timer_for_key(state: &mut RuntimeState, task_key: &str) {
        if let Some(handle) = state.timers_by_task_key.remove(task_key) {
            handle.abort();
        }
    }

    /// 清掉某项目全部任务的定时器与排队标记（整体替换任务表前调用）。
    fn clear_project_timers(state: &mut RuntimeState, project_id: &str) {
        let keys: Vec<String> = state
            .tasks_by_project
            .get(project_id)
            .map(|tasks| {
                tasks
                    .keys()
                    .map(|id| build_task_key(project_id, id))
                    .collect()
            })
            .unwrap_or_default();
        for key in keys {
            Self::clear_timer_for_key(state, &key);
            state.queued_task_keys.remove(&key);
        }
    }

    /// 整体替换某项目的内存任务表：先清旧定时器，再插入新 map。
    fn set_project_tasks(state: &mut RuntimeState, project_id: &str, tasks: Vec<ScheduledTask>) {
        Self::clear_project_timers(state, project_id);
        let mut map = HashMap::new();
        for task in tasks {
            map.insert(task.id.clone(), task);
        }
        state.tasks_by_project.insert(project_id.to_string(), map);
    }

    /// `scheduleTask`: arm a timer for `next_run_at` (ms) with jitter. Tokio
    /// sleeps take arbitrary durations, so the JS `MAX_TIMER_DELAY_MS`
    /// re-arm hop is unnecessary.
    ///
    /// 中文补充：先在锁内清旧定时器并算出「剩余延迟 + 随机抖动」，
    /// 锁外 spawn sleep 任务；到点后在锁内复查启用与启动状态再入队，
    /// 最后按最新状态登记句柄或丢弃，避免与 stop() 竞态留下孤儿定时器。
    pub(crate) fn schedule_task(
        self: &SharedRuntime,
        project_id: &str,
        task_id: &str,
        next_run_at: i64,
    ) {
        let task_key = build_task_key(project_id, task_id);
        let delay_ms = {
            let mut state = self.lock_state();
            Self::clear_timer_for_key(&mut state, &task_key);
            if !state.started || next_run_at <= 0 {
                return;
            }
            let delay_base = (next_run_at - self.now_ms()).max(0);
            let jitter = rand::random_range(0..=JITTER_MAX_MS);
            delay_base + jitter
        };

        let runtime = Arc::clone(self);
        let project_id = project_id.to_string();
        let task_id = task_id.to_string();
        let timer_task_key = task_key.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms.max(0) as u64)).await;
            let task_key = timer_task_key;
            let enqueue = {
                let mut state = runtime.lock_state();
                Self::clear_timer_for_key(&mut state, &task_key);
                let task_enabled = state
                    .tasks_by_project
                    .get(&project_id)
                    .and_then(|map| map.get(&task_id))
                    .map(|task| task.enabled)
                    .unwrap_or(false);
                if !task_enabled || !state.started {
                    false
                } else {
                    let queued = state.queued_task_keys.insert(task_key.clone());
                    if queued {
                        state.queue.push_back(QueueItem {
                            project_id: project_id.clone(),
                            task_id: task_id.clone(),
                            scheduled_for: Some(next_run_at),
                        });
                        true
                    } else {
                        false
                    }
                }
            };
            if enqueue {
                runtime.pump_queue().await;
            }
        });
        let mut state = self.lock_state();
        if state.started {
            state.timers_by_task_key.insert(task_key, handle);
        } else {
            handle.abort();
        }
    }

    /// `scheduleFutureRun`: arm only a strictly-future occurrence. Arming a
    /// past slot re-enters the claim path at delay 0 and can spin (notably
    /// for once tasks whose claim cannot advance nextRunAt).
    ///
    /// 中文补充：next 缺失或不严格晚于 from_ms 都不武装并返回
    /// false，由调用方决定是否重算。
    fn schedule_future_run(
        self: &SharedRuntime,
        project_id: &str,
        task_id: &str,
        next_run_at: Option<i64>,
        from_ms: i64,
    ) -> bool {
        let Some(next) = next_run_at else {
            return false;
        };
        if next <= from_ms {
            return false;
        }
        self.schedule_task(project_id, task_id, next);
        true
    }

    /// `rearmFromTaskOrCompute`: prefer a still-future persisted slot, else
    /// compute the next occurrence; never re-arm a past slot.
    ///
    /// 中文补充：优先用内存（或 fallback 任务）里持久化的
    /// nextRunAt，只有它不可用时才用 compute_next_run_at 重算；
    /// 两个路径都保证只武装未来的时刻。
    fn rearm_from_task_or_compute(
        self: &SharedRuntime,
        project_id: &str,
        task_id: &str,
        fallback_task: Option<ScheduledTask>,
        from_ms: i64,
    ) {
        let latest = {
            let state = self.lock_state();
            state
                .tasks_by_project
                .get(project_id)
                .and_then(|map| map.get(task_id))
                .cloned()
                .or(fallback_task)
        };
        let Some(task) = latest else { return };
        if !task.enabled {
            return;
        }
        if self.schedule_future_run(
            project_id,
            task_id,
            task.state.next_run_at.map(|v| v as i64),
            from_ms,
        ) {
            return;
        }
        let computed = compute_next_run_at(&task, from_ms);
        self.schedule_future_run(project_id, task_id, computed, from_ms);
    }

    // -- sync --------------------------------------------------------------

    /// 取项目根路径：命中缓存直接返回，否则查项目列表并回填缓存；
    /// 项目未知时返回 None。
    async fn ensure_project_path(&self, project_id: &str) -> Option<String> {
        if let Some(cached) = self.lock_state().project_path_by_id.get(project_id) {
            return Some(cached.clone());
        }
        let projects = self.deps.projects.list_projects().await.ok()?;
        let path = projects
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.path.clone())?;
        self.lock_state()
            .project_path_by_id
            .insert(project_id.to_string(), path.clone());
        Some(path)
    }

    /// 把某任务的 nextRunAt 补写进持久化状态，并按返回的最新任务
    /// 重武装定时器；写失败静默跳过（下次 sync 重试）。
    async fn sync_task_schedule(self: &SharedRuntime, project_id: &str, task: &ScheduledTask) {
        let next_run_at = compute_next_run_at(task, self.now_ms());
        let patch = json!({
            "nextRunAt": next_run_at,
            "updatedAt": self.now_ms(),
        });
        let Ok(result) = self
            .deps
            .store
            .update_scheduled_task_state(project_id, &task.id, &patch)
            .await
        else {
            return;
        };
        if let Some(task) = result.task {
            let task_id = task.id.clone();
            let enabled = task.enabled;
            let next = task.state.next_run_at.map(|v| v as i64);
            self.lock_state().update_in_memory_task(project_id, task);
            if enabled && let Some(next) = next {
                self.schedule_task(project_id, &task_id, next);
            }
        }
    }

    /// `syncProject`: reconcile loops for a known project path (or list the
    /// persisted tasks), then re-arm every task.
    ///
    /// 中文补充：能解析到项目路径才做 loop 对账（否则退化为纯列表，
    /// 不产生任何写）；随后整体替换内存任务表并逐个补写 nextRunAt。
    pub async fn sync_project(
        self: &SharedRuntime,
        project_id: &str,
    ) -> AppResult<Vec<ScheduledTask>> {
        let project_path = self.ensure_project_path(project_id).await;
        let tasks = if let Some(path) = project_path.as_deref() {
            let loops = discover_loops(Some(std::path::Path::new(path)));
            let entries: Vec<LoopEntry> = loops.iter().map(loop_entry_from).collect();
            self.deps
                .store
                .reconcile_loop_tasks(project_id, &entries)
                .await?
        } else {
            self.deps.store.list_scheduled_tasks(project_id).await?
        };

        {
            let mut state = self.lock_state();
            Self::set_project_tasks(&mut state, project_id, tasks.clone());
        }
        for task in &tasks {
            self.sync_task_schedule(project_id, task).await;
        }
        Ok(tasks)
    }

    /// `syncAllProjects`: drop vanished projects, sync active ones.
    ///
    /// 中文补充：先在锁内清空路径缓存并剔除已消失项目（连定时器
    /// 一起），再逐个同步存活项目；任一项目失败即整体返回错误。
    pub async fn sync_all_projects(self: &SharedRuntime) -> AppResult<()> {
        let projects = self.deps.projects.list_projects().await?;
        let active: HashSet<String> = projects.iter().map(|p| p.id.clone()).collect();
        {
            let mut state = self.lock_state();
            state.project_path_by_id.clear();
            let stale: Vec<String> = state
                .tasks_by_project
                .keys()
                .filter(|id| !active.contains(*id))
                .cloned()
                .collect();
            for id in stale {
                Self::clear_project_timers(&mut state, &id);
                state.tasks_by_project.remove(&id);
            }
            for project in &projects {
                state
                    .project_path_by_id
                    .insert(project.id.clone(), project.path.clone());
            }
        }
        for project_id in active {
            self.sync_project(&project_id).await?;
        }
        Ok(())
    }

    /// `start()`: begin scheduling and sync every project.
    ///
    /// 中文补充：幂等——已启动直接返回 Ok；置位后立即全量同步。
    pub async fn start(self: &SharedRuntime) -> AppResult<()> {
        if self.lock_state().started {
            return Ok(());
        }
        self.lock_state().started = true;
        self.sync_all_projects().await
    }

    /// `stop()`: clear timers and the queue.
    ///
    /// 中文补充：abort 全部定时器并清空排队状态；已在运行的任务
    /// 不受影响（由各自的完成路径收尾）。
    pub fn stop(&self) {
        let mut state = self.lock_state();
        if !state.started {
            return;
        }
        state.started = false;
        for (_, handle) in state.timers_by_task_key.drain() {
            handle.abort();
        }
        state.queued_task_keys.clear();
        state.queue.clear();
    }

    /// `getStatus`.
    ///
    /// 中文补充：仅统计内存态（启用数/运行数），无任何 I/O。
    pub fn get_status(&self) -> Value {
        let state = self.lock_state();
        let enabled_count = state
            .tasks_by_project
            .values()
            .flat_map(|map| map.values())
            .filter(|task| task.enabled)
            .count();
        let running_count = state.running_task_keys.len();
        json!({
            "hasEnabledScheduledTasks": enabled_count > 0,
            "hasRunningScheduledTasks": running_count > 0,
            "enabledScheduledTasksCount": enabled_count,
            "runningScheduledTasksCount": running_count,
        })
    }

    // -- queue -------------------------------------------------------------

    /// 并发闸门：全局运行数与该项目运行数都未达上限才允许启动。
    fn can_run_task(&self, state: &RuntimeState, project_id: &str) -> bool {
        if state.running_global_count >= self.deps.max_global_concurrency {
            return false;
        }
        let running = state
            .running_count_by_project
            .get(project_id)
            .copied()
            .unwrap_or(0);
        running < self.deps.max_project_concurrency
    }

    /// 预占运行槽位：登记 task_key 并累加全局/项目计数。必须在锁内
    /// 调用，保证上限不被穿透。
    fn reserve_slot(state: &mut RuntimeState, project_id: &str, task_key: &str) {
        state.running_task_keys.insert(task_key.to_string());
        state.running_global_count += 1;
        *state
            .running_count_by_project
            .entry(project_id.to_string())
            .or_insert(0) += 1;
    }

    /// 释放运行槽位：移除 task_key、递减计数；项目计数归零时移除
    /// 条目。每条运行结束路径都必须恰好调用一次。
    fn release_running_slot(&self, project_id: &str, task_key: &str) {
        let mut state = self.lock_state();
        state.running_task_keys.remove(task_key);
        state.running_global_count = state.running_global_count.saturating_sub(1);
        let next = state
            .running_count_by_project
            .get(project_id)
            .copied()
            .unwrap_or(1)
            .saturating_sub(1);
        if next == 0 {
            state.running_count_by_project.remove(project_id);
        } else {
            state
                .running_count_by_project
                .insert(project_id.to_string(), next);
        }
    }

    /// `pumpQueue`: start queued runs while concurrency caps allow. Slots are
    /// reserved synchronously under the lock so caps cannot be overshot.
    ///
    /// The completion pump re-enters `pumpQueue` from spawned tasks; the
    /// recursion goes through a boxed future so the spawned blocks can prove
    /// `Send` (a direct recursive await would be an unprovable cycle).
    ///
    /// 中文补充：对外入口只是装箱版的薄封装（见 pump_queue_boxed）。
    pub(crate) async fn pump_queue(self: &SharedRuntime) {
        self.pump_queue_boxed().await
    }

    /// 泵送实现：锁内批量挑出可启动项并同步预占槽位，锁外 spawn
    /// 各运行任务；每个任务结束后再次泵送形成级联。装箱以打破
    /// 异步递归的 Send 证明环。
    fn pump_queue_boxed<'a>(self: &'a SharedRuntime) -> BoxFut<'a, ()> {
        Box::pin(async move {
            let mut starts: Vec<(String, String, Option<i64>)> = Vec::new();
            {
                let mut state = self.lock_state();
                if !state.started {
                    return;
                }
                let mut index = 0;
                while index < state.queue.len() {
                    let item = state.queue[index].clone();
                    if !self.can_run_task(&state, &item.project_id) {
                        index += 1;
                        continue;
                    }
                    state.queue.remove(index);
                    let task_key = build_task_key(&item.project_id, &item.task_id);
                    state.queued_task_keys.remove(&task_key);
                    Self::reserve_slot(&mut state, &item.project_id, &task_key);
                    starts.push((item.project_id, item.task_id, item.scheduled_for));
                }
            }
            for (project_id, task_id, scheduled_for) in starts {
                let runtime = Arc::clone(self);
                let reason = RunReason::Scheduled { scheduled_for };
                tokio::spawn(async move {
                    let _ = runtime
                        .run_task_with_reserved_slot(&project_id, &task_id, reason)
                        .await;
                    runtime.pump_queue_boxed().await;
                });
            }
        })
    }

    // -- running ------------------------------------------------------------

    /// `runTask`: full lifecycle; every path releases the running slot.
    ///
    /// 中文补充：锁内完成「存在/启用/非运行中」三项检查并预占槽位，
    /// 失败路径各自返回 skipped/running；随后执行完整生命周期，
    /// 任何路径都会释放槽位。
    pub async fn run_task(
        self: &SharedRuntime,
        project_id: &str,
        task_id: &str,
        reason: RunReason,
    ) -> RunOutcome {
        let task_key = build_task_key(project_id, task_id);
        let task = {
            let mut state = self.lock_state();
            let Some(task) = state
                .tasks_by_project
                .get(project_id)
                .and_then(|map| map.get(task_id))
                .cloned()
            else {
                return RunOutcome {
                    ok: false,
                    skipped: true,
                    ..Default::default()
                };
            };
            if !task.enabled {
                return RunOutcome {
                    ok: false,
                    skipped: true,
                    ..Default::default()
                };
            }
            if state.running_task_keys.contains(&task_key) {
                return RunOutcome {
                    ok: false,
                    running: true,
                    ..Default::default()
                };
            }
            Self::reserve_slot(&mut state, project_id, &task_key);
            task
        };
        let outcome = self.run_task_body(project_id, task_id, reason, task).await;
        self.release_running_slot(project_id, &task_key);
        outcome
    }

    /// Pump entry point: the slot is already reserved by `pump_queue`.
    ///
    /// 中文补充：仅复查任务仍存在且启用；失败同样释放槽位并返回
    /// skipped。
    async fn run_task_with_reserved_slot(
        self: &SharedRuntime,
        project_id: &str,
        task_id: &str,
        reason: RunReason,
    ) -> RunOutcome {
        let task_key = build_task_key(project_id, task_id);
        let task = {
            let state = self.lock_state();
            state
                .tasks_by_project
                .get(project_id)
                .and_then(|map| map.get(task_id))
                .cloned()
        };
        let Some(task) = task.filter(|task| task.enabled) else {
            self.release_running_slot(project_id, &task_key);
            return RunOutcome {
                ok: false,
                skipped: true,
                ..Default::default()
            };
        };
        let outcome = self.run_task_body(project_id, task_id, reason, task).await;
        self.release_running_slot(project_id, &task_key);
        outcome
    }

    /// `runNow`: manual trigger; manual runs never claim a schedule slot.
    ///
    /// 中文补充：先做运行中/已排队去重（带各自的错误消息），再走
    /// run_task(Manual)；手动运行不做占位声明。
    pub async fn run_now(self: &SharedRuntime, project_id: &str, task_id: &str) -> RunOutcome {
        let task_key = build_task_key(project_id, task_id);
        {
            let state = self.lock_state();
            if state.running_task_keys.contains(&task_key) {
                return RunOutcome {
                    ok: false,
                    running: true,
                    error: Some("task is already running".into()),
                    ..Default::default()
                };
            }
            if state.queued_task_keys.contains(&task_key) {
                return RunOutcome {
                    ok: false,
                    queued: true,
                    error: Some("task is already queued".into()),
                    ..Default::default()
                };
            }
        }
        self.run_task(project_id, task_id, RunReason::Manual).await
    }

    /// 运行生命周期主体：占位声明/手动启动写状态 → watchdog 执行 →
    /// 一次性任务消费 → 完成状态写回与重武装。所有持久化失败路径
    /// 都保证内存态收敛到终态；槽位由两个入口（run_task /
    /// run_task_with_reserved_slot）负责释放。
    async fn run_task_body(
        self: &SharedRuntime,
        project_id: &str,
        task_id: &str,
        reason: RunReason,
        task: ScheduledTask,
    ) -> RunOutcome {
        let run_started_at = self.now_ms();

        match &reason {
            RunReason::Scheduled {
                scheduled_for: Some(scheduled_for),
            } => {
                let scheduled_for = *scheduled_for;
                if let Err(outcome) = self
                    .claim_occurrence(project_id, task_id, &task, run_started_at, scheduled_for)
                    .await
                {
                    return outcome;
                }
            }
            RunReason::Scheduled {
                scheduled_for: None,
            } => {
                return RunOutcome {
                    ok: false,
                    skipped: true,
                    reason: Some("missing-scheduled-for".into()),
                    ..Default::default()
                };
            }
            RunReason::Manual => {
                let patch = json!({
                    "lastRunAt": run_started_at,
                    "lastStatus": "running",
                    "lastError": null,
                });
                match self
                    .deps
                    .store
                    .update_scheduled_task_state(project_id, task_id, &patch)
                    .await
                {
                    Ok(result) => {
                        if let Some(task) = result.task {
                            self.lock_state().update_in_memory_task(project_id, task);
                        }
                    }
                    Err(error) => {
                        let message = safe_error_message(&error.to_string(), 2000);
                        tracing::warn!(
                            "[ScheduledTasks] manual start state write failed: {message}"
                        );
                        return RunOutcome {
                            ok: false,
                            error: Some(message),
                            reason: Some("start-state-failed".into()),
                            ..Default::default()
                        };
                    }
                }
            }
        }

        // Execute under the watchdog.
        let mut status = "success".to_string();
        let mut session_id: Option<String> = None;
        let mut duration_ms: i64 = 0;
        let mut error_message: Option<String> = None;
        let run = self.execute_run(project_id, &task);
        match tokio::time::timeout(Duration::from_millis(self.deps.max_run_duration_ms), run).await
        {
            Ok(Ok((sid, elapsed))) => {
                session_id = sid;
                duration_ms = elapsed;
            }
            Ok(Err(error)) => {
                status = "error".into();
                error_message = Some(safe_error_message(&error, 2000));
            }
            Err(_) => {
                status = "error".into();
                error_message = Some("scheduled task run timed out".into());
            }
        }
        if status == "error" {
            tracing::warn!(
                "[ScheduledTasks] run failed: project={project_id} task={task_id} reason={} error={}",
                reason.as_str(),
                error_message.as_deref().unwrap_or_default()
            );
        }

        let finished_at = self.now_ms();
        if duration_ms == 0 {
            duration_ms = (finished_at - run_started_at).max(0);
        }

        let mut latest_task = {
            let state = self.lock_state();
            state
                .tasks_by_project
                .get(project_id)
                .and_then(|map| map.get(task_id))
                .cloned()
                .unwrap_or_else(|| task.clone())
        };

        // One-time tasks are consumed after a scheduled run.
        let is_once_scheduled = matches!(latest_task.schedule, Schedule::Once { .. })
            && matches!(reason, RunReason::Scheduled { .. });
        if is_once_scheduled && latest_task.enabled {
            let mut input = serde_json::to_value(&latest_task).unwrap_or_else(|_| json!({}));
            if let Some(map) = input.as_object_mut() {
                map.insert("enabled".into(), Value::Bool(false));
            }
            match self
                .deps
                .store
                .upsert_scheduled_task(project_id, &input)
                .await
            {
                Ok(consumed) => {
                    latest_task = consumed.task;
                    let snapshot = latest_task.clone();
                    self.lock_state()
                        .update_in_memory_task(project_id, snapshot);
                }
                Err(error) => {
                    tracing::warn!(
                        "[ScheduledTasks] failed to consume one-time task: {}",
                        safe_error_message(&error.to_string(), 2000)
                    );
                }
            }
        }

        let next_run_at = compute_next_run_at(&latest_task, finished_at);
        let state_patch = json!({
            "lastStatus": status,
            "lastDurationMs": duration_ms,
            "lastError": if status == "error" { error_message.clone() } else { None },
            "lastSessionId": if status == "success" { session_id.clone() } else { None },
            "nextRunAt": next_run_at,
            "updatedAt": finished_at,
        });

        let state_result = self
            .deps
            .store
            .update_scheduled_task_state(project_id, task_id, &state_patch)
            .await;
        match state_result {
            Ok(result) => {
                let task = result.task;
                if let Some(t) = &task {
                    self.lock_state()
                        .update_in_memory_task(project_id, t.clone());
                    if t.enabled {
                        let next = t.state.next_run_at.map(|v| v as i64);
                        self.schedule_future_run(project_id, task_id, next, finished_at);
                    }
                }
                self.emit_event(TaskRunEvent {
                    project_id: project_id.into(),
                    task_id: task_id.into(),
                    ran_at: finished_at,
                    status: status.clone(),
                    session_id: session_id.clone(),
                });
                RunOutcome {
                    ok: status == "success",
                    status: Some(status),
                    session_id,
                    task,
                    error: error_message,
                    ..Default::default()
                }
            }
            Err(persist_error) => {
                let message = safe_error_message(&persist_error.to_string(), 2000);
                tracing::warn!("[ScheduledTasks] run completion state write failed: {message}");

                // Keep in-memory status terminal (not stuck 'running').
                let mut recovered = latest_task.clone();
                recovered.state.last_status = status.clone();
                recovered.state.last_duration_ms = Some(duration_ms.max(0) as u64);
                recovered.state.last_error = if status == "error" {
                    error_message.clone()
                } else {
                    None
                };
                recovered.state.last_session_id = if status == "success" {
                    session_id.clone()
                } else {
                    None
                };
                recovered.state.next_run_at = next_run_at.map(|v| v.max(0) as u64);
                recovered.state.updated_at = finished_at.max(0) as u64;
                {
                    let snapshot = recovered.clone();
                    self.lock_state()
                        .update_in_memory_task(project_id, snapshot);
                }

                // Best-effort single retry so persisted lastStatus recovers.
                let mut effective_task = recovered.clone();
                let mut retry_succeeded = false;
                match self
                    .deps
                    .store
                    .update_scheduled_task_state(project_id, task_id, &state_patch)
                    .await
                {
                    Ok(retry) => {
                        if let Some(task) = retry.task {
                            if task.enabled {
                                let next = task.state.next_run_at.map(|v| v as i64);
                                self.schedule_future_run(project_id, task_id, next, finished_at);
                            }
                            let snapshot = task.clone();
                            self.lock_state()
                                .update_in_memory_task(project_id, snapshot);
                            effective_task = task;
                            retry_succeeded = true;
                        }
                    }
                    Err(retry_error) => {
                        tracing::warn!(
                            "[ScheduledTasks] run completion state retry failed: {}",
                            safe_error_message(&retry_error.to_string(), 2000)
                        );
                    }
                }
                if !retry_succeeded {
                    let snapshot = recovered.clone();
                    self.rearm_from_task_or_compute(
                        project_id,
                        task_id,
                        Some(snapshot),
                        finished_at,
                    );
                }

                // The session already ran: surface the persist failure without
                // turning a successful dispatch into a hard run failure.
                RunOutcome {
                    ok: status == "success",
                    status: Some(status),
                    session_id: session_id.clone(),
                    task: Some(effective_task),
                    error: error_message,
                    persist_error: Some(message),
                    reason: Some("completion-state-failed".into()),
                    ..Default::default()
                }
            }
        }
    }

    /// Claim the occurrence in shared project config (#2710). `Err(outcome)`
    /// means the run must not proceed.
    ///
    /// 中文补充：以 lastScheduledFor 在 TASK_DUE_SLACK_MS 内的 CAS
    /// 谓词抢占该占位；赢者继续运行，输者跳过（occurrence-claimed），
    /// 存储错误同样跳过（claim-failed）并尽力补记错误状态。
    async fn claim_occurrence(
        self: &SharedRuntime,
        project_id: &str,
        task_id: &str,
        task: &ScheduledTask,
        run_started_at: i64,
        scheduled_for: i64,
    ) -> Result<(), RunOutcome> {
        let next_after_claim = compute_next_run_at(task, run_started_at.max(scheduled_for + 1));
        let claim_patch = json!({
            "lastScheduledFor": scheduled_for,
            "lastRunAt": run_started_at,
            "lastStatus": "running",
            "lastError": null,
            // Always set nextRunAt (null clears) so a consumed once slot is
            // cleared even when there is no following occurrence.
            "nextRunAt": next_after_claim,
        });

        // Duplicate protection is solely lastScheduledFor within slack of
        // this occurrence — never the advanced on-disk nextRunAt (a second
        // instance syncing inside the slack window must not suppress later
        // days). See DOCUMENTATION.md.
        let predicate = Box::new(move |candidate: &ScheduledTask| {
            if !candidate.enabled {
                return false;
            }
            if let Some(last) = candidate.state.last_scheduled_for
                && (last as i64 - scheduled_for).abs() <= TASK_DUE_SLACK_MS
            {
                return false;
            }
            true
        });

        let claim_result = self
            .deps
            .store
            .update_scheduled_task_state_if(project_id, task_id, predicate, &claim_patch)
            .await;

        match claim_result {
            Err(claim_error) => {
                let message = safe_error_message(&claim_error.to_string(), 2000);
                tracing::warn!("[ScheduledTasks] occurrence claim failed: {message}");
                self.rearm_from_task_or_compute(
                    project_id,
                    task_id,
                    Some(task.clone()),
                    run_started_at.max(scheduled_for + 1),
                );

                // Best-effort record so once tasks are not left enabled-but-
                // inert; never clobber a winner that claimed this occurrence.
                let failure_patch = json!({
                    "lastStatus": "error",
                    "lastError": format!("Scheduled claim failed: {message}"),
                });
                let guard_predicate = Box::new(move |candidate: &ScheduledTask| {
                    if let Some(last) = candidate.state.last_scheduled_for
                        && (last as i64 - scheduled_for).abs() <= TASK_DUE_SLACK_MS
                    {
                        return false;
                    }
                    true
                });
                match self
                    .deps
                    .store
                    .update_scheduled_task_state_if(
                        project_id,
                        task_id,
                        guard_predicate,
                        &failure_patch,
                    )
                    .await
                {
                    Ok(recorded) => {
                        if let Some(task) = recorded.task {
                            self.lock_state().update_in_memory_task(project_id, task);
                        }
                    }
                    Err(_) => {
                        let mut in_memory = task.clone();
                        in_memory.state.last_status = "error".into();
                        in_memory.state.last_error =
                            Some(format!("Scheduled claim failed: {message}"));
                        self.lock_state()
                            .update_in_memory_task(project_id, in_memory);
                    }
                }

                Err(RunOutcome {
                    ok: false,
                    skipped: true,
                    reason: Some("claim-failed".into()),
                    error: Some(message),
                    ..Default::default()
                })
            }
            Ok(result) => {
                if !result.updated {
                    if let Some(task) = &result.task {
                        let snapshot = task.clone();
                        self.lock_state()
                            .update_in_memory_task(project_id, snapshot);
                        // The loser must not re-arm a past slot (once-spin).
                        let base = self.now_ms().max(scheduled_for + 1);
                        self.rearm_from_task_or_compute(
                            project_id,
                            task_id,
                            Some(task.clone()),
                            base,
                        );
                    }
                    return Err(RunOutcome {
                        ok: false,
                        skipped: true,
                        reason: Some("occurrence-claimed".into()),
                        ..Default::default()
                    });
                }
                if let Some(task) = result.task {
                    self.lock_state().update_in_memory_task(project_id, task);
                }
                Ok(())
            }
        }
    }

    /// 转发事件到注入的发射回调（SSE 广播）。
    fn emit_event(&self, event: TaskRunEvent) {
        (self.deps.emit_task_run_event)(&event);
    }

    // -- engine execution ----------------------------------------------------

    /// `runTaskWithWatchdog` body: wait ready, create the session, emit the
    /// running event, then dispatch the prompt or command.
    ///
    /// 中文补充：任一步失败返回 Err(消息)，由外层 watchdog 记为
    /// error 终态；permission auto-accept 失败仅告警不中断运行。
    async fn execute_run(
        self: &SharedRuntime,
        project_id: &str,
        task: &ScheduledTask,
    ) -> Result<(Option<String>, i64), String> {
        let started_at = self.now_ms();
        let title = format_scheduled_session_title(task, started_at);
        let project_path = {
            let state = self.lock_state();
            state.project_path_by_id.get(project_id).cloned()
        };
        let Some(project_path) = project_path else {
            return Err("project path is unavailable".to_string());
        };

        self.deps.dispatch.wait_ready().await?;

        let session_id = self
            .deps
            .dispatch
            .create_session(&project_path, &title)
            .await
            .map_err(|_| "failed to create session".to_string())?;
        if session_id.is_empty() {
            return Err("failed to create session".to_string());
        }

        self.emit_event(TaskRunEvent {
            project_id: project_id.into(),
            task_id: task.id.clone(),
            ran_at: started_at,
            status: "running".into(),
            session_id: Some(session_id.clone()),
        });

        if task.execution.permission_auto_accept == Some(true) {
            // Enroll before the prompt goes out; failure must not kill the run.
            if let Err(error) = self
                .deps
                .dispatch
                .set_session_auto_accept(&session_id, true, &project_path)
                .await
            {
                tracing::warn!(
                    "[scheduled-tasks] failed to enable permission auto-accept for session {session_id}: {error}"
                );
            }
        }

        let scheduled_command = self.resolve_scheduled_command(&project_path, task).await;

        if task.execution.goal_enabled == Some(true) {
            let objective = match &scheduled_command {
                Some(command) => {
                    expand_command_goal_objective(command.template.as_deref(), &command.arguments)
                }
                None => None,
            }
            .unwrap_or_else(|| {
                expand_snippets(
                    &task.execution.prompt,
                    Some(std::path::Path::new(&project_path)),
                )
            });
            self.deps
                .dispatch
                .create_session_goal(
                    &session_id,
                    &project_path,
                    &objective,
                    task.execution.goal_token_budget,
                    task.execution.provider_id.as_deref(),
                    task.execution.model_id.as_deref(),
                )
                .await?;
        }

        match &scheduled_command {
            Some(command) => {
                // JS ignores the session.command result envelope.
                let _ = self
                    .deps
                    .dispatch
                    .run_session_command(&session_id, &project_path, command, &task.execution)
                    .await;
            }
            None => {
                let payload = build_prompt_async_payload(task, &project_path);
                self.deps
                    .dispatch
                    .prompt_async(&session_id, &project_path, &payload)
                    .await?;
            }
        }

        let finished_at = self.now_ms();
        Ok((Some(session_id), (finished_at - started_at).max(0)))
    }

    /// `resolveScheduledCommand`: a slash-command prompt whose command exists
    /// in the engine's command list.
    ///
    /// 中文补充：prompt 不是斜杠命令、或命令不在引擎命令列表中
    /// 时返回 None，退回普通 prompt 派发。
    async fn resolve_scheduled_command(
        &self,
        project_path: &str,
        task: &ScheduledTask,
    ) -> Option<ScheduledCommand> {
        let (command, arguments) = parse_scheduled_command_prompt(&task.execution.prompt)?;
        let commands = self.deps.dispatch.list_commands(project_path).await.ok()?;
        commands
            .into_iter()
            .find(|candidate| candidate.command == command)
            .map(|mut found| {
                found.arguments = arguments;
                found
            })
    }
}

/// 把 loops 模块发现的 loop 文件转换为存储层的对账条目（scope 字符串
/// 化、路径转 String、定义转任务输入 JSON）。
fn loop_entry_from(loop_file: &DiscoveredLoop) -> LoopEntry {
    LoopEntry {
        scope: loop_file.scope.as_str().to_string(),
        file_path: loop_file.file_path.to_string_lossy().to_string(),
        definition: loop_file.definition.as_ref().map(|def| def.to_task_input()),
    }
}

/// 构造 prompt_async 的请求体：provider+model 齐全才带 model 对象，
/// 可选 agent/variant；parts 为展开 snippet 后的 prompt 文本，goal
/// 启用时追加合成的 goal 提醒分片（synthetic=true）。
fn build_prompt_async_payload(task: &ScheduledTask, project_path: &str) -> Value {
    let mut payload = serde_json::Map::new();
    match (&task.execution.provider_id, &task.execution.model_id) {
        (Some(provider), Some(model)) => {
            payload.insert(
                "model".into(),
                json!({ "providerID": provider, "modelID": model }),
            );
        }
        // Role-follow tasks omit the model so the engine resolves its default.
        _ => {}
    }
    if let Some(agent) = &task.execution.agent {
        payload.insert("agent".into(), json!(agent));
    }
    if let Some(variant) = &task.execution.variant {
        payload.insert("variant".into(), json!(variant));
    }
    let mut parts = vec![json!({
        "type": "text",
        "text": expand_snippets(&task.execution.prompt, Some(std::path::Path::new(project_path))),
    })];
    if task.execution.goal_enabled == Some(true) {
        parts.push(json!({
            "type": "text",
            "text": build_goal_intro_text(task.execution.goal_token_budget),
            "synthetic": true,
        }));
    }
    payload.insert("parts".into(), Value::Array(parts));
    Value::Object(payload)
}

/// 调度运行时的行为测试：跨实例占位声明、各错误路径的槽位/状态收敛、
/// 一次性任务消费、状态计数与 loop 对账（对照 JS runtime 的测试用例）。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduled_tasks::testing::{
        MemoryProjects, MemoryStore, RecordingDispatch, daily_task, make_runtime, once_task, prime,
    };
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    /// 固定的「当前时刻」，让全部测试的调度与占位计算可复现。
    const NOW: i64 = 1_768_000_000_000; // arbitrary fixed instant

    /// 行为契约：两个运行时实例争抢同一占位时，只有赢家真正派发
    /// （恰好创建一个会话），输家以 occurrence-claimed 跳过。
    #[tokio::test]
    async fn two_instances_claiming_one_occurrence_create_one_session() {
        let store = Arc::new(MemoryStore::new(daily_task()));
        let dispatch = RecordingDispatch::new();
        let a = make_runtime(Arc::clone(&store), dispatch.clone(), NOW);
        let b = make_runtime(Arc::clone(&store), dispatch.clone(), NOW);

        prime(&a, daily_task());
        prime(&b, daily_task());

        let scheduled_for = NOW + 60_000;
        let out_a = a
            .run_task(
                "p1",
                "task-1",
                RunReason::Scheduled {
                    scheduled_for: Some(scheduled_for),
                },
            )
            .await;
        let out_b = b
            .run_task(
                "p1",
                "task-1",
                RunReason::Scheduled {
                    scheduled_for: Some(scheduled_for),
                },
            )
            .await;

        assert!(out_a.ok, "winner dispatches: {:?}", out_a.reason);
        assert!(!out_b.ok && out_b.skipped);
        assert_eq!(out_b.reason.as_deref(), Some("occurrence-claimed"));
        assert_eq!(dispatch.sessions.load(Ordering::SeqCst), 1);
        assert_eq!(store.claim_attempts.load(Ordering::SeqCst), 2);
    }

    /// 行为契约：同一占位的重复声明在 slack 窗口内被拒，但次日的
    /// 新占位照常触发第二次运行。
    #[tokio::test]
    async fn same_occurrence_again_is_rejected_but_next_day_fires() {
        let store = Arc::new(MemoryStore::new(daily_task()));
        let dispatch = RecordingDispatch::new();
        let runtime = make_runtime(Arc::clone(&store), dispatch.clone(), NOW);
        prime(&runtime, daily_task());

        let scheduled_for = NOW + 60_000;
        let first = runtime
            .run_task(
                "p1",
                "task-1",
                RunReason::Scheduled {
                    scheduled_for: Some(scheduled_for),
                },
            )
            .await;
        assert!(first.ok);

        // A duplicate claim for the same occurrence (within slack) loses.
        let duplicate = runtime
            .run_task(
                "p1",
                "task-1",
                RunReason::Scheduled {
                    scheduled_for: Some(scheduled_for),
                },
            )
            .await;
        assert_eq!(duplicate.reason.as_deref(), Some("occurrence-claimed"));
        assert_eq!(dispatch.sessions.load(Ordering::SeqCst), 1);

        // The next day's occurrence is a different slot and claims fine.
        let next_day = scheduled_for + 24 * 3_600_000;
        let second = runtime
            .run_task(
                "p1",
                "task-1",
                RunReason::Scheduled {
                    scheduled_for: Some(next_day),
                },
            )
            .await;
        assert!(second.ok, "{:?}", second.reason);
        assert_eq!(dispatch.sessions.load(Ordering::SeqCst), 2);
    }

    /// 行为契约：占位声明失败必须释放运行槽位（计数归零），且不阻碍
    /// 随后的手动 runNow。
    #[tokio::test]
    async fn claim_failure_releases_slot_and_manual_run_still_works() {
        let store = Arc::new(MemoryStore::new(daily_task()));
        store.fail_claim.store(true, Ordering::SeqCst);
        let dispatch = RecordingDispatch::new();
        let runtime = make_runtime(Arc::clone(&store), dispatch.clone(), NOW);
        prime(&runtime, daily_task());

        let out = runtime
            .run_task(
                "p1",
                "task-1",
                RunReason::Scheduled {
                    scheduled_for: Some(NOW + 60_000),
                },
            )
            .await;
        assert!(!out.ok);
        assert_eq!(out.reason.as_deref(), Some("claim-failed"));
        assert_eq!(dispatch.sessions.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.get_status()["runningScheduledTasksCount"], 0);
        assert_eq!(runtime.get_status()["hasRunningScheduledTasks"], false);

        // Manual runNow must not be stuck behind a permanently running claim.
        let manual = runtime.run_now("p1", "task-1").await;
        assert!(manual.ok, "{:?}", manual.reason);
        assert_eq!(dispatch.sessions.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.get_status()["runningScheduledTasksCount"], 0);
    }

    /// 行为契约：一次性任务声明失败时补记 error 状态（含错误消息）
    /// 并保持 enabled，等待下次触发。
    #[tokio::test]
    async fn once_claim_failure_records_error_state() {
        let store = Arc::new(MemoryStore::new(once_task()));
        store.fail_claim.store(true, Ordering::SeqCst);
        let dispatch = RecordingDispatch::new();
        let runtime = make_runtime(Arc::clone(&store), dispatch.clone(), NOW);
        prime(&runtime, once_task());

        let out = runtime
            .run_task(
                "p1",
                "task-1",
                RunReason::Scheduled {
                    scheduled_for: Some(NOW + 60_000),
                },
            )
            .await;
        assert_eq!(out.reason.as_deref(), Some("claim-failed"));

        let stored = store.current();
        assert_eq!(stored.state.last_status, "error");
        assert!(
            stored
                .state
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains("Scheduled claim failed")
        );
        assert!(stored.enabled);
    }

    /// 行为契约：手动运行的完成状态写失败仍返回会话 id 与
    /// persistError，内存态收敛为 success 且槽位归零。
    #[tokio::test]
    async fn manual_completion_write_failure_returns_session_and_persist_error() {
        let store = Arc::new(MemoryStore::new(daily_task()));
        store.fail_completion.store(true, Ordering::SeqCst);
        let dispatch = RecordingDispatch::new();
        let runtime = make_runtime(Arc::clone(&store), dispatch.clone(), NOW);
        prime(&runtime, daily_task());

        let manual = runtime.run_now("p1", "task-1").await;
        assert!(manual.ok);
        assert!(manual.session_id.is_some());
        assert_eq!(manual.reason.as_deref(), Some("completion-state-failed"));
        assert!(
            manual
                .persist_error
                .as_deref()
                .unwrap_or_default()
                .contains("timeout acquiring project config lock")
        );
        assert_eq!(
            manual.task.as_ref().map(|t| t.state.last_status.as_str()),
            Some("success")
        );
        assert_eq!(runtime.get_status()["runningScheduledTasksCount"], 0);
    }

    /// 行为契约：手动启动的状态写失败会让运行中止、不创建会话，
    /// 并释放运行槽位。
    #[tokio::test]
    async fn manual_start_state_write_failure_releases_slot() {
        let store = Arc::new(MemoryStore::new(daily_task()));
        store.fail_start.store(true, Ordering::SeqCst);
        let dispatch = RecordingDispatch::new();
        let runtime = make_runtime(Arc::clone(&store), dispatch.clone(), NOW);
        prime(&runtime, daily_task());

        let manual = runtime.run_now("p1", "task-1").await;
        assert!(!manual.ok);
        assert_eq!(manual.reason.as_deref(), Some("start-state-failed"));
        assert_eq!(dispatch.sessions.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.get_status()["runningScheduledTasksCount"], 0);
    }

    /// 行为契约：一次性任务在计划运行后被消费（enabled=false）。
    #[tokio::test]
    async fn once_task_is_consumed_after_scheduled_run() {
        let store = Arc::new(MemoryStore::new(once_task()));
        let dispatch = RecordingDispatch::new();
        let runtime = make_runtime(Arc::clone(&store), dispatch.clone(), NOW);
        prime(&runtime, once_task());

        let out = runtime
            .run_task(
                "p1",
                "task-1",
                RunReason::Scheduled {
                    scheduled_for: Some(NOW + 60_000),
                },
            )
            .await;
        assert!(out.ok);
        let stored = store.current();
        assert!(
            !stored.enabled,
            "one-time task consumed after its scheduled run"
        );
    }

    /// 行为契约：缺失占位时间戳的计划运行按 missing-scheduled-for
    /// 跳过，不创建任何会话。
    #[tokio::test]
    async fn scheduled_run_without_occurrence_is_skipped() {
        let store = Arc::new(MemoryStore::new(daily_task()));
        let dispatch = RecordingDispatch::new();
        let runtime = make_runtime(Arc::clone(&store), dispatch.clone(), NOW);
        prime(&runtime, daily_task());

        let out = runtime
            .run_task(
                "p1",
                "task-1",
                RunReason::Scheduled {
                    scheduled_for: None,
                },
            )
            .await;
        assert!(out.skipped);
        assert_eq!(out.reason.as_deref(), Some("missing-scheduled-for"));
        assert_eq!(dispatch.sessions.load(Ordering::SeqCst), 0);
    }

    /// 行为契约：任务缺失或已禁用时跳过，不创建会话。
    #[tokio::test]
    async fn disabled_or_missing_tasks_skip() {
        let store = Arc::new(MemoryStore::new(daily_task()));
        let dispatch = RecordingDispatch::new();
        let runtime = make_runtime(Arc::clone(&store), dispatch.clone(), NOW);
        prime(&runtime, daily_task());

        let missing = runtime.run_task("p1", "nope", RunReason::Manual).await;
        assert!(missing.skipped);

        let mut disabled = daily_task();
        disabled.enabled = false;
        {
            let mut state = runtime.lock_state();
            state
                .tasks_by_project
                .insert("p1".into(), HashMap::from([("task-1".into(), disabled)]));
        }
        let out = runtime.run_task("p1", "task-1", RunReason::Manual).await;
        assert!(out.skipped);
        assert_eq!(dispatch.sessions.load(Ordering::SeqCst), 0);
    }

    /// 行为契约：状态计数只统计启用与运行中的任务。
    #[tokio::test]
    async fn status_counts_enabled_and_running_tasks() {
        let store = Arc::new(MemoryStore::new(daily_task()));
        let dispatch = RecordingDispatch::new();
        let runtime = make_runtime(Arc::clone(&store), dispatch.clone(), NOW);
        prime(&runtime, daily_task());
        let status = runtime.get_status();
        assert_eq!(status["enabledScheduledTasksCount"], 1);
        assert_eq!(status["hasEnabledScheduledTasks"], true);
        assert_eq!(status["runningScheduledTasksCount"], 0);
    }

    /// 行为契约：prompt 载荷形状与 JS 一致——model 对象、goal 合成
    /// 分片、角色跟随任务省略 model。
    #[tokio::test]
    async fn prompt_payload_shape_matches_js() {
        let task = daily_task();
        let payload = build_prompt_async_payload(&task, "/repo");
        assert_eq!(
            payload["model"],
            json!({ "providerID": "openai", "modelID": "gpt-4o" })
        );
        assert_eq!(payload["parts"][0]["type"], "text");
        assert_eq!(payload["parts"][0]["text"], "Summarize open issues");
        assert!(payload.get("agent").is_none());
        assert_eq!(payload["parts"].as_array().map(|p| p.len()), Some(1));

        // Goal-enabled task appends the synthetic reminder.
        let mut goal_task = task.clone();
        goal_task.execution.goal_enabled = Some(true);
        let payload = build_prompt_async_payload(&goal_task, "/repo");
        assert_eq!(payload["parts"].as_array().map(|p| p.len()), Some(2));
        assert_eq!(payload["parts"][1]["synthetic"], true);

        // Role-follow tasks omit the model object entirely.
        let mut role_task = goal_task;
        role_task.execution.provider_id = None;
        role_task.execution.model_id = None;
        let payload = build_prompt_async_payload(&role_task, "/repo");
        assert!(payload.get("model").is_none());
    }

    /// JS `reconciles discovered loops when the project path is known`:
    /// the real on-disk project config round-trips loop tasks (persistence
    /// roundtrip through `ProjectConfigRuntime`).
    ///
    /// 中文补充：走真实 ProjectConfigRuntime 的端到端持久化往返。
    #[tokio::test]
    async fn sync_project_reconciles_loops_through_real_store() {
        let root = std::env::temp_dir().join(format!(
            "oc-rt-loop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let repo = root.join("repo");
        let loops_dir = repo.join(".agents").join("loops");
        std::fs::create_dir_all(&loops_dir).expect("mkdir loops");
        std::fs::write(
            loops_dir.join("daily.md"),
            "---\nname: daily\nschedule: \"0 9 * * *\"\nenabled: true\nmodel: openai/gpt-5\n---\nRun daily.\n",
        )
        .expect("write loop");

        let store = Arc::new(crate::projects::ProjectConfigRuntime::new(
            root.join("config"),
        ));
        let projects = Arc::new(MemoryProjects {
            projects: vec![ProjectRef {
                id: "proj".into(),
                path: repo.to_string_lossy().to_string(),
            }],
        });
        let deps = RuntimeDeps {
            store: Arc::clone(&store) as Arc<dyn ScheduledTaskStore>,
            projects: Arc::clone(&projects) as Arc<dyn ProjectsAccess>,
            dispatch: RecordingDispatch::new(),
            emit_task_run_event: Arc::new(|_| {}),
            clock: Arc::new(|| 1_750_000_000_000),
            max_global_concurrency: DEFAULT_GLOBAL_CONCURRENCY,
            max_project_concurrency: DEFAULT_PROJECT_CONCURRENCY,
            max_run_duration_ms: DEFAULT_MAX_RUN_MS,
        };
        let runtime = ScheduledTasksRuntime::new(deps);
        let tasks = runtime.sync_project("proj").await.expect("sync");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "loop:project:daily");
        assert_eq!(
            tasks[0].loop_file.as_deref(),
            Some(loops_dir.join("daily.md").to_string_lossy().as_ref())
        );
        // The returned list predates the schedule patch (JS returns the same
        // array); the persisted state is what carries nextRunAt.
        // Roundtrip: a fresh runtime over the same config dir sees the task.
        let reloaded = store.list_scheduled_tasks("proj").await.expect("list");
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].id, "loop:project:daily");
        assert!(reloaded[0].state.next_run_at.unwrap_or(0) > 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// JS `falls back to plain listing when the project path cannot be
    /// resolved`: no reconcile write happens for an unknown project.
    ///
    /// 中文补充：同时断言未知项目不产生任何 reconcile 写文件。
    #[tokio::test]
    async fn sync_project_falls_back_to_plain_listing() {
        let root = std::env::temp_dir().join(format!(
            "oc-rt-list-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let config_dir = root.join("config");
        std::fs::create_dir_all(&config_dir).expect("mkdir");

        let store = Arc::new(crate::projects::ProjectConfigRuntime::new(
            config_dir.clone(),
        ));
        let projects = Arc::new(MemoryProjects { projects: vec![] });
        let deps = RuntimeDeps {
            store: Arc::clone(&store) as Arc<dyn ScheduledTaskStore>,
            projects: Arc::clone(&projects) as Arc<dyn ProjectsAccess>,
            dispatch: RecordingDispatch::new(),
            emit_task_run_event: Arc::new(|_| {}),
            clock: Arc::new(|| 1_750_000_000_000),
            max_global_concurrency: DEFAULT_GLOBAL_CONCURRENCY,
            max_project_concurrency: DEFAULT_PROJECT_CONCURRENCY,
            max_run_duration_ms: DEFAULT_MAX_RUN_MS,
        };
        let runtime = ScheduledTasksRuntime::new(deps);
        let tasks = runtime.sync_project("proj").await.expect("sync");
        assert!(tasks.is_empty());
        // Plain listing never creates the project config file.
        let entries: Vec<_> = std::fs::read_dir(&config_dir)
            .expect("dir")
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            entries.is_empty(),
            "no reconcile write for unknown projects"
        );
        assert_eq!(
            store.list_scheduled_tasks("proj").await.expect("list"),
            Vec::new()
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 行为契约：loop 条目转换保留 scope、路径与定义 JSON。
    #[tokio::test]
    async fn loop_entry_conversion_keeps_scope_and_definition() {
        let def = super::super::loops::LoopDefinition {
            name: "daily".into(),
            enabled: true,
            cron: "0 9 * * *".into(),
            timezone: None,
            prompt: "Run daily.".into(),
            provider_id: "openai".into(),
            model_id: "gpt-5".into(),
            agent: None,
        };
        let discovered = DiscoveredLoop {
            scope: super::super::loops::LoopScope::Project,
            file_path: std::path::PathBuf::from("/repo/.agents/loops/daily.md"),
            definition: Some(def),
        };
        let entry = loop_entry_from(&discovered);
        assert_eq!(entry.scope, "project");
        assert_eq!(entry.file_path, "/repo/.agents/loops/daily.md");
        assert_eq!(entry.definition.as_ref().unwrap()["name"], "daily");
        assert_eq!(
            entry.definition.as_ref().unwrap()["schedule"]["cron"],
            "0 9 * * *"
        );
    }
}
