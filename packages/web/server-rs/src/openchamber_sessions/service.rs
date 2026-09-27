//! Port of `createOMPChamberSessionService` from
//! `openchamber-sessions/routes.js`: the `create` flow (directory → optional
//! worktree + bootstrap wait → session create → prompt dispatch → event
//! emit) and the `runExisting` flow (`send`/`fork` with partial-failure
//! bookkeeping).
//!
//! 中文说明：本文件实现 JS 版 `createOMPChamberSessionService` 的两个主流程——
//! `create`（解析目录 → 可选创建 worktree 并等待 bootstrap 就绪 → 创建会话 →
//! 派发初始 prompt → 广播 session-created 事件）与 `runExisting`
//! （`send`/`fork` 的共享主体，含部分失败记账）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::scheduled_tasks::compute::{
    expand_command_goal_objective, parse_scheduled_command_prompt,
};
use crate::scheduled_tasks::snippets::expand_snippets;
use crate::session_goal::create::build_goal_intro_text;

use super::client::{BoxFut, EngineClient, EngineReply, SessionCommandParams};
use super::error::{PartialDetails, SvcError};
use super::payload::{
    GoalInput, ModelRef, as_non_empty_string, is_truthy, resolve_goal_input,
    resolve_requested_model, resolve_worktree_input,
};
use super::selection::{
    fetch_selection_inputs, resolve_default_selection, validate_requested_selection,
};

/// `wait_for_prompt_landed` 确认 prompt 落地的总超时（毫秒），对应 JS 的 5000。
const PROMPT_LANDED_TIMEOUT_MS: u64 = 5_000;
/// 两次轮询消息列表之间的间隔（毫秒）。
const PROMPT_LANDED_POLL_MS: u64 = 150;
/// 等待 worktree bootstrap 进入可用阶段的总超时（毫秒），对应 JS 的 60000。
const WORKTREE_BOOTSTRAP_TIMEOUT_MS: u64 = 60_000;
/// bootstrap 状态轮询间隔（毫秒）。
const WORKTREE_BOOTSTRAP_POLL_MS: u64 = 150;

/// `createWorktree` / `getWorktreeBootstrapStatus` from `git/index.js` — the
/// JS tests mock both, so the seam stays injectable.
///
/// 中文说明：git worktree 操作接缝——创建 worktree 与查询 bootstrap 状态；
/// JS 测试对两者都做了 mock，因此保持可注入。
pub trait WorktreeOps: Send + Sync {
    /// Returns the worktree record embedded verbatim in the route response.
    /// 返回的 worktree 记录会原样嵌入路由响应；`Err` 携带 git 服务的错误文本。
    fn create(&self, directory: &str, input: &Value) -> BoxFut<'_, Result<Value, String>>;
    /// 查询指定目录 worktree bootstrap 的当前状态/阶段。
    fn bootstrap_status(&self, directory: &str) -> BoxFut<'_, Result<Value, String>>;
}

/// One `createSessionGoal` invocation (JS passes the same fields plus
/// `baseUrl`/`authHeaders`, which the engine-backed creator resolves
/// itself).
///
/// 中文说明：一次 `createSessionGoal` 调用的参数集（JS 侧还传
/// `baseUrl`/`authHeaders`，由 engine 后端创建器自行解析）。
#[derive(Debug, Clone)]
pub struct GoalCall {
    /// 目标会话 ID。
    pub session_id: String,
    /// 会话所在目录（canonical 路径）。
    pub directory: String,
    /// 目标文本（命令模板展开后的目标，或展开后的 prompt）。
    pub objective: String,
    /// 可选的 token 预算。
    pub token_budget: Option<u64>,
    /// 模型 provider ID。
    pub provider_id: String,
    /// 模型 model ID。
    pub model_id: String,
}

/// 依赖注入闭包：读取共享设置存储的 raw Map（异步）。
pub type ReadSettings = Arc<dyn Fn() -> BoxFut<'static, Map<String, Value>> + Send + Sync>;
/// 依赖注入闭包：把 settings.projects 清洗为安全的项目列表。
pub type SanitizeProjects = Arc<dyn Fn(&Value) -> Vec<Value> + Send + Sync>;
/// 依赖注入闭包：校验并规范化目录路径，`Err` 为用户可读的错误消息。
pub type ValidateDirectory =
    Arc<dyn Fn(&str) -> BoxFut<'static, Result<String, String>> + Send + Sync>;
/// 依赖注入闭包：等待本地 engine 就绪（含超时）。
pub type WaitReady = Arc<dyn Fn() -> BoxFut<'static, Result<(), String>> + Send + Sync>;
/// 依赖注入闭包：为会话创建 goal 元数据，`Err` 为错误消息。
pub type GoalCreator = Arc<dyn Fn(GoalCall) -> BoxFut<'static, Result<(), String>> + Send + Sync>;
/// 依赖注入闭包：广播 session-created 事件（同步，永不使请求失败）。
pub type EmitSessionCreated = Arc<dyn Fn(&Value) + Send + Sync>;

/// The JS `dependencies` object of `createOMPChamberSessionService`.
///
/// 中文说明：对应 JS `createOMPChamberSessionService` 的 `dependencies`
/// 对象；测试与生产（mod.rs 组合根）各注入一份实现。
pub struct SessionDeps {
    /// 本地 engine 客户端（SDK 约定 + raw fetch 帮助函数）。
    pub client: Arc<dyn EngineClient>,
    /// 设置读取注入点。
    pub read_settings: ReadSettings,
    /// projects 清洗注入点。
    pub sanitize_projects: SanitizeProjects,
    /// 目录校验注入点。
    pub validate_directory: ValidateDirectory,
    /// engine 就绪等待注入点。
    pub wait_ready: WaitReady,
    /// worktree 操作注入点。
    pub worktrees: Arc<dyn WorktreeOps>,
    /// goal 创建注入点。
    pub create_goal: GoalCreator,
    /// session-created 事件广播注入点。
    pub emit_session_created: EmitSessionCreated,
}

/// The outcome of `dispatchPrompt`.
///
/// 中文说明：记录派发最终生效的 selection 与结果标志——model/agent/variant、
/// prompt 是否落地、是否以命令派发，以及未落地时的错误文本。
#[derive(Debug, Clone)]
struct DispatchOutcome {
    /// 实际生效的模型（经默认值回填）。
    model: Option<ModelRef>,
    /// 实际生效的 agent 名。
    agent: Option<String>,
    /// 实际生效的 variant。
    variant: Option<String>,
    /// prompt 是否已被 engine 接受并出现在会话中。
    prompt_dispatched: bool,
    /// 是否按 slash 命令（而非普通 prompt）派发。
    dispatched_as_command: bool,
    /// 未落地等非致命错误的描述文本。
    prompt_error: Option<String>,
}

/// `resolveRequestedDirectory` output.
///
/// 中文说明：目录解析结果——canonical 目录路径，以及按 projectId 请求时
/// 命中的项目 ID（按 directory 请求时为 `None`）。
struct ResolvedDirectory {
    /// 校验后的 canonical 目录路径。
    directory: String,
    /// 请求携带 projectId 时命中的项目 ID。
    project_id: Option<String>,
}

/// 会话编排服务：持有全部依赖闭包，实现 create / send / fork 流程。
pub struct SessionService {
    /// 注入的依赖集合。
    deps: SessionDeps,
}

/// `create` 与 `runExisting` 两个流程的实现主体。
impl SessionService {
    /// 以给定依赖构造服务。
    pub fn new(deps: SessionDeps) -> Self {
        Self { deps }
    }

    /// 读取当前设置并等待结果（各流程内联使用）。
    fn log_settings(&self) -> BoxFut<'static, Map<String, Value>> {
        (self.deps.read_settings)()
    }

    /// JS `resolveRequestedDirectory`: `projectId`/`projectID` lookup in the
    /// sanitized settings projects (404 when unknown), else the `directory`
    /// field through validation.
    ///
    /// 中文说明：优先按 `projectId`/`projectID` 在清洗后的 settings.projects
    /// 中查找（未命中 404，项目路径校验失败 400）；否则对 `directory` 字段
    /// 做目录校验（失败 400）。
    async fn resolve_requested_directory(
        &self,
        payload: &Value,
    ) -> Result<ResolvedDirectory, SvcError> {
        let project_id = as_non_empty_string(payload.get("projectId"))
            .or_else(|| as_non_empty_string(payload.get("projectID")));
        if let Some(project_id) = project_id {
            let settings = self.log_settings().await;
            let projects =
                (self.deps.sanitize_projects)(settings.get("projects").unwrap_or(&Value::Null));
            let project = projects
                .iter()
                .find(|entry| entry.get("id").and_then(Value::as_str) == Some(project_id.as_str()));
            let path = project
                .and_then(|entry| entry.get("path"))
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty());
            let Some(path) = path else {
                return Err(SvcError::control("Project not found", 404));
            };
            return match (self.deps.validate_directory)(path).await {
                Ok(directory) => Ok(ResolvedDirectory {
                    directory,
                    project_id: Some(project_id),
                }),
                Err(error) => Err(SvcError::control(
                    if error.is_empty() {
                        "Invalid project directory".to_string()
                    } else {
                        error
                    },
                    400,
                )),
            };
        }

        let directory = as_non_empty_string(payload.get("directory"));
        match (self.deps.validate_directory)(directory.as_deref().unwrap_or("")).await {
            Ok(directory) => Ok(ResolvedDirectory {
                directory,
                project_id: None,
            }),
            Err(error) => Err(SvcError::control(
                if error.is_empty() {
                    "Invalid directory".to_string()
                } else {
                    error
                },
                400,
            )),
        }
    }

    /// JS `waitForWorktreeBootstrapReady`: poll until the bootstrap reaches a
    /// git-ready phase, reporting the bootstrap failure or a 60s timeout as
    /// 500s.
    ///
    /// 中文说明：轮询 bootstrap 状态直到 ready 或进入 git-ready/setup-ready
    /// 阶段；bootstrap 标记失败或超过 60 秒未就绪均按 500 返回。
    async fn wait_for_worktree_bootstrap_ready(&self, directory: &str) -> Result<(), SvcError> {
        let deadline = Instant::now() + Duration::from_millis(WORKTREE_BOOTSTRAP_TIMEOUT_MS);
        loop {
            let status = self
                .deps
                .worktrees
                .bootstrap_status(directory)
                .await
                .map_err(SvcError::plain)?;
            if status.get("status").and_then(Value::as_str) == Some("failed") {
                let error = status
                    .get("error")
                    .and_then(Value::as_str)
                    .filter(|error| !error.is_empty())
                    .unwrap_or("unknown error");
                return Err(SvcError::control(
                    format!("Worktree bootstrap failed: {error}"),
                    500,
                ));
            }
            let phase = status.get("phase").and_then(Value::as_str);
            let ready = status.get("status").and_then(Value::as_str) == Some("ready")
                || matches!(phase, Some("git-ready") | Some("setup-ready"));
            if ready {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(SvcError::control(
                    "Timed out waiting for the worktree bootstrap",
                    500,
                ));
            }
            tokio::time::sleep(Duration::from_millis(WORKTREE_BOOTSTRAP_POLL_MS)).await;
        }
    }

    /// JS `latestUserMessageID` — `Err` marks a failed lookup (the JS
    /// returns `{ ok: false }`), `Ok(None)` a session without user messages.
    ///
    /// 中文说明：拉取最近 100 条消息取最新 user 消息 ID；`Err` 表示查询失败
    /// （对应 JS 的 `{ ok: false }`），`Ok(None)` 表示会话尚无 user 消息。
    async fn latest_user_message_id(
        &self,
        session_id: &str,
        directory: &str,
    ) -> Result<Option<String>, ()> {
        let reply = self
            .deps
            .client
            .session_messages(session_id, directory, 100)
            .await
            .map_err(|_| ())?;
        Ok(latest_message_id(&reply, "user"))
    }

    /// JS `latestCompletedAssistantMessageID`.
    ///
    /// 中文说明：取最新一条已完成（`time.completed` 为有限数）的 assistant
    /// 消息 ID，作为 send/fork 前的基线，供前端判断增量回复。
    async fn latest_completed_assistant_message_id(
        &self,
        session_id: &str,
        directory: &str,
    ) -> Option<String> {
        let reply = self
            .deps
            .client
            .session_messages(session_id, directory, 100)
            .await
            .ok()?;
        let messages = reply.data.as_ref().and_then(Value::as_array)?;
        let mut latest: Option<&Value> = None;
        for message in messages {
            let Some(info) = message.get("info") else {
                continue;
            };
            if info.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let completed = info
                .get("time")
                .and_then(|time| time.get("completed"))
                .and_then(Value::as_f64);
            if !completed.is_some_and(|completed| completed.is_finite()) {
                continue;
            }
            let created = info
                .get("time")
                .and_then(|time| time.get("created"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let latest_created = latest
                .and_then(|info| info.get("time"))
                .and_then(|time| time.get("created"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            if latest.is_none() || created >= latest_created {
                latest = Some(info);
            }
        }
        as_non_empty_string(latest.and_then(|info| info.get("id")))
    }

    /// JS `waitForPromptLanded`: confirm the accepted prompt was recorded; a
    /// failed lookup is not authoritative evidence of loss.
    ///
    /// 中文说明：轮询确认被接受的 prompt 已出现在会话中（最新 user 消息 ID
    /// 相对基线发生变化）；查询失败不构成丢失证据，直接视为已落地。
    async fn wait_for_prompt_landed(
        &self,
        session_id: &str,
        directory: &str,
        baseline_user_message_id: Option<&str>,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_millis(PROMPT_LANDED_TIMEOUT_MS);
        loop {
            let latest = self.latest_user_message_id(session_id, directory).await;
            if latest.is_err() {
                return true;
            }
            if let Ok(Some(message_id)) = &latest
                && Some(message_id.as_str()) != baseline_user_message_id
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(PROMPT_LANDED_POLL_MS)).await;
        }
    }

    /// JS `fetchLastUserSelection`: newest user message carrying a model.
    ///
    /// 中文说明：从最近 20 条消息里找最新一条携带 model 的 user 消息，返回
    /// (model, agent, variant) 三元组，用于复用既有会话的 selection。
    async fn fetch_last_user_selection(
        &self,
        session_id: &str,
        directory: &str,
    ) -> Option<(ModelRef, Option<String>, Option<String>)> {
        let reply = self
            .deps
            .client
            .session_messages(session_id, directory, 20)
            .await
            .ok()?;
        let records = reply.data.as_ref().and_then(Value::as_array)?;
        for record in records.iter().rev() {
            let Some(info) = record.get("info") else {
                continue;
            };
            if info.get("role").and_then(Value::as_str) != Some("user") {
                continue;
            }
            let model = info.get("model");
            let (Some(provider_id), Some(model_id)) = (
                as_non_empty_string(model.and_then(|m| m.get("providerID"))),
                as_non_empty_string(model.and_then(|m| m.get("modelID"))),
            ) else {
                continue;
            };
            let agent = as_non_empty_string(info.get("agent"));
            let variant = as_non_empty_string(model.and_then(|m| m.get("variant")));
            return Some((
                ModelRef {
                    provider_id,
                    model_id,
                },
                agent,
                variant,
            ));
        }
        None
    }

    /// JS `dispatchPrompt` — selection resolution, goal creation, command or
    /// prompt dispatch, and the landed confirmation.
    ///
    /// 中文说明：selection 解析（复用会话历史 → 设置默认 → 引擎配置默认）→
    /// snippet 展开 → slash 命令识别 →（可选）goal 创建 → 按 command 或
    /// prompt_async 派发 → 确认 prompt 落地。
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_prompt(
        &self,
        session_id: &str,
        directory: &str,
        prompt: &str,
        goal_input: GoalInput,
        requested_model: Option<ModelRef>,
        requested_agent: Option<String>,
        requested_variant: Option<String>,
        reuse_session_selection: bool,
    ) -> Result<DispatchOutcome, SvcError> {
        let mut model = requested_model;
        let mut agent = requested_agent;
        let mut variant = requested_variant;
        if reuse_session_selection
            && (model.is_none() || agent.is_none())
            && let Some((previous_model, previous_agent, previous_variant)) =
                self.fetch_last_user_selection(session_id, directory).await
        {
            if model.is_none() {
                model = Some(previous_model);
                if variant.is_none() {
                    variant = previous_variant;
                }
            }
            if agent.is_none() {
                agent = previous_agent;
            }
        }
        if model.is_none() || agent.is_none() {
            let settings = self.log_settings().await;
            let inputs =
                fetch_selection_inputs(self.deps.client.as_ref(), directory, settings).await;
            let defaults = resolve_default_selection(&inputs);
            if model.is_none() {
                model = defaults.model;
                if variant.is_none() {
                    variant = defaults.variant;
                }
            }
            if agent.is_none() {
                agent = defaults.agent;
            }
        }
        let Some(model) = model else {
            return Err(SvcError::with_status(
                "No model is configured or available for the requested directory",
                400,
            ));
        };

        let expanded_prompt = expand_snippets(prompt, Some(&PathBuf::from(directory)));
        let parsed_command = parse_scheduled_command_prompt(prompt);
        let mut resolved_command: Option<(String, String, Option<String>)> = None;
        if let Some((command, arguments)) = parsed_command {
            // `client.command.list` failures are swallowed (JS try/catch).
            if let Ok(reply) = self.deps.client.command_list(directory).await {
                let commands = reply.data.as_ref().and_then(Value::as_array);
                if let Some(found) = commands.and_then(|commands| {
                    commands.iter().find(|candidate| {
                        candidate.get("name").and_then(Value::as_str) == Some(command.as_str())
                    })
                }) {
                    let template = found
                        .get("template")
                        .and_then(Value::as_str)
                        .map(String::from);
                    resolved_command = Some((command, arguments, template));
                }
            }
        }
        if goal_input.enabled {
            let objective = resolved_command
                .as_ref()
                .and_then(|(_, arguments, template)| {
                    expand_command_goal_objective(template.as_deref(), arguments)
                })
                .unwrap_or_else(|| expanded_prompt.clone());
            let call = GoalCall {
                session_id: session_id.to_string(),
                directory: directory.to_string(),
                objective,
                token_budget: goal_input.token_budget,
                provider_id: model.provider_id.clone(),
                model_id: model.model_id.clone(),
            };
            (self.deps.create_goal)(call)
                .await
                .map_err(SvcError::plain)?;
        }

        let dispatched_as_command = resolved_command.is_some();
        if let Some((command, arguments, _)) = resolved_command {
            let params = SessionCommandParams {
                directory: directory.to_string(),
                command,
                arguments,
                agent: agent.clone(),
                model: model.to_slash_form(),
                variant: variant.clone(),
            };
            if let Err(error) = self.deps.client.session_command(session_id, &params).await {
                return Err(SvcError::plain(error).mark_goal_partial(goal_input.enabled));
            }
        } else {
            let baseline = self.latest_user_message_id(session_id, directory).await;
            // The session-knowledge runtime is not ported yet; the JS default
            // with a missing runtime is empty background context.
            let knowledge_text = "";
            let mut parts = Vec::new();
            if !knowledge_text.is_empty() {
                parts.push(json!({
                    "type": "text",
                    "text": knowledge_text,
                    "synthetic": true,
                }));
            }
            parts.push(json!({ "type": "text", "text": expanded_prompt }));
            if goal_input.enabled {
                parts.push(json!({
                    "type": "text",
                    "text": build_goal_intro_text(goal_input.token_budget),
                    "synthetic": true,
                }));
            }
            let mut payload = Map::new();
            payload.insert("model".to_string(), model.to_json());
            if let Some(agent) = &agent {
                payload.insert("agent".to_string(), Value::String(agent.clone()));
            }
            if let Some(variant) = &variant {
                payload.insert("variant".to_string(), Value::String(variant.clone()));
            }
            payload.insert("parts".to_string(), Value::Array(parts));
            if let Err(error) = self
                .deps
                .client
                .prompt_async(session_id, directory, &Value::Object(payload))
                .await
            {
                return Err(SvcError::plain(error).mark_goal_partial(goal_input.enabled));
            }
            let landed = self
                .wait_for_prompt_landed(session_id, directory, baseline.ok().flatten().as_deref())
                .await;
            if !landed {
                return Ok(DispatchOutcome {
                    model: Some(model),
                    agent,
                    variant,
                    prompt_dispatched: false,
                    dispatched_as_command: false,
                    prompt_error: Some(
                        "OpenCode accepted the prompt but it never appeared in the session"
                            .to_string(),
                    ),
                });
            }
        }

        Ok(DispatchOutcome {
            model: Some(model),
            agent,
            variant,
            prompt_dispatched: true,
            dispatched_as_command,
            prompt_error: None,
        })
    }

    /// JS `create`.
    ///
    /// 中文说明：完整建会话流程——校验 goal/model/worktree 输入、解析目录、
    /// 等待 engine 就绪、（有 prompt 时）校验 selection、（可选）创建
    /// worktree 并等待 bootstrap、创建会话、派发初始 prompt、组装响应并
    /// 广播 session-created 事件（事件发送永不使请求失败）。
    pub async fn create(&self, payload: &Value) -> Result<Value, SvcError> {
        let title = as_non_empty_string(payload.get("title"));
        let prompt = as_non_empty_string(payload.get("prompt"));
        let goal_input = resolve_goal_input(payload, prompt.as_deref())
            .map_err(|error| SvcError::control(error, 400))?;
        let model = resolve_requested_model(payload);
        let agent = as_non_empty_string(payload.get("agent"));
        let variant = as_non_empty_string(payload.get("variant"));

        let resolved = self.resolve_requested_directory(payload).await?;

        let worktree_input = resolve_worktree_input(payload);
        let mut session_directory = resolved.directory.clone();
        if is_truthy(payload.get("worktree")) && worktree_input.is_none() {
            return Err(SvcError::control(
                "worktree.name is required when worktree is provided",
                400,
            ));
        }

        (self.deps.wait_ready)().await.map_err(SvcError::plain)?;

        if prompt.is_some() {
            let settings = self.log_settings().await;
            validate_requested_selection(
                self.deps.client.as_ref(),
                settings,
                &resolved.directory,
                model.as_ref(),
                agent.as_deref(),
                variant.as_deref(),
            )
            .await?;
        }

        let mut worktree: Option<Value> = None;
        if let Some(input) = worktree_input {
            let created = self
                .deps
                .worktrees
                .create(&resolved.directory, &input)
                .await
                .map_err(SvcError::plain)?;
            session_directory = created
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            worktree = Some(created);
            self.wait_for_worktree_bootstrap_ready(&session_directory)
                .await?;
        }

        let session_id = self
            .deps
            .client
            .create_session(&session_directory, title.as_deref())
            .await
            .map_err(SvcError::plain)?;

        let mut dispatch = DispatchOutcome {
            model: model.clone(),
            agent: agent.clone(),
            variant: variant.clone(),
            prompt_dispatched: false,
            dispatched_as_command: false,
            prompt_error: None,
        };
        if let Some(prompt) = &prompt {
            dispatch = self
                .dispatch_prompt(
                    &session_id,
                    &session_directory,
                    prompt,
                    goal_input,
                    model.clone(),
                    agent.clone(),
                    variant.clone(),
                    false,
                )
                .await?;
        }

        let mut result = Map::new();
        result.insert("sessionId".to_string(), Value::String(session_id.clone()));
        result.insert(
            "directory".to_string(),
            Value::String(session_directory.clone()),
        );
        if let Some(project_id) = &resolved.project_id {
            result.insert("projectId".to_string(), Value::String(project_id.clone()));
        }
        if let Some(title) = &title {
            result.insert("title".to_string(), Value::String(title.clone()));
        }
        if let Some(worktree) = &worktree {
            result.insert("worktree".to_string(), worktree.clone());
        }
        if prompt.is_some() && dispatch.model.is_some() {
            result.insert(
                "model".to_string(),
                dispatch.model.clone().unwrap().to_json(),
            );
        }
        if prompt.is_some() && dispatch.agent.is_some() {
            result.insert(
                "agent".to_string(),
                Value::String(dispatch.agent.clone().unwrap()),
            );
        }
        if prompt.is_some() && dispatch.variant.is_some() {
            result.insert(
                "variant".to_string(),
                Value::String(dispatch.variant.clone().unwrap()),
            );
        }
        result.insert(
            "promptDispatched".to_string(),
            Value::Bool(dispatch.prompt_dispatched),
        );
        if let Some(prompt_error) = &dispatch.prompt_error {
            result.insert(
                "promptError".to_string(),
                Value::String(prompt_error.clone()),
            );
        }
        result.insert(
            "dispatchedAsCommand".to_string(),
            Value::Bool(dispatch.dispatched_as_command),
        );
        if goal_input.enabled {
            result.insert("goalEnabled".to_string(), Value::Bool(true));
        }
        if let Some(token_budget) = goal_input.token_budget {
            result.insert("goalTokenBudget".to_string(), json!(token_budget));
        }

        // `emitSessionCreatedEvent` — never fails the request (JS try/catch).
        let mut event = Map::new();
        event.insert("sessionID".to_string(), Value::String(session_id.clone()));
        event.insert(
            "directory".to_string(),
            Value::String(session_directory.clone()),
        );
        if let Some(project_id) = &resolved.project_id {
            event.insert("projectID".to_string(), Value::String(project_id.clone()));
        }
        if let Some(title) = &title {
            event.insert("title".to_string(), Value::String(title.clone()));
        }
        if let Some(worktree) = &worktree {
            event.insert("worktree".to_string(), worktree.clone());
        }
        if prompt.is_some() && dispatch.model.is_some() {
            event.insert(
                "model".to_string(),
                dispatch.model.clone().unwrap().to_json(),
            );
        }
        if prompt.is_some() && dispatch.agent.is_some() {
            event.insert(
                "agent".to_string(),
                Value::String(dispatch.agent.clone().unwrap()),
            );
        }
        if prompt.is_some() && dispatch.variant.is_some() {
            event.insert(
                "variant".to_string(),
                Value::String(dispatch.variant.clone().unwrap()),
            );
        }
        event.insert(
            "promptDispatched".to_string(),
            Value::Bool(dispatch.prompt_dispatched),
        );
        event.insert(
            "dispatchedAsCommand".to_string(),
            Value::Bool(dispatch.dispatched_as_command),
        );
        if goal_input.enabled {
            event.insert("goalEnabled".to_string(), Value::Bool(true));
        }
        if let Some(token_budget) = goal_input.token_budget {
            event.insert("goalTokenBudget".to_string(), json!(token_budget));
        }
        event.insert("createdAt".to_string(), json!(now_ms()));
        (self.deps.emit_session_created)(&Value::Object(event));

        Ok(Value::Object(result))
    }

    /// JS `runExisting` — the shared body of `send` and `fork`, including
    /// the partial-failure catch.
    ///
    /// 中文说明：`send`/`fork` 的共享主体——校验 sessionId/prompt/goal 输入、
    /// 解析目录、fork 时先复制会话、记录基线 assistant 消息、派发 prompt、
    /// 组装响应（fork 成功还广播事件）；失败路径保留内部状态码，并在 fork
    /// 已创建或 goal 已配置时附加 partial 细节。
    pub async fn run_existing(
        &self,
        action: &str,
        source_session_id: &str,
        payload: &Value,
    ) -> Result<Value, SvcError> {
        let source_session_id = source_session_id.trim();
        if source_session_id.is_empty() {
            return Err(SvcError::control("sessionId is required", 400));
        }
        let source_session_id = source_session_id.to_string();
        let prompt = match as_non_empty_string(payload.get("prompt")) {
            Some(prompt) => prompt,
            None => return Err(SvcError::control("prompt is required", 400)),
        };
        let goal_input = resolve_goal_input(payload, Some(&prompt))
            .map_err(|error| SvcError::control(error, 400))?;
        let requested_model = resolve_requested_model(payload);
        let requested_agent = as_non_empty_string(payload.get("agent"));
        let requested_variant = as_non_empty_string(payload.get("variant"));

        let mut target_session_id = source_session_id.clone();
        let mut directory: Option<String> = None;
        let mut fork_created = false;

        let outcome: Result<Value, SvcError> = async {
            let resolved = self.resolve_requested_directory(payload).await?;
            directory = Some(resolved.directory.clone());
            let directory = resolved.directory.clone();

            (self.deps.wait_ready)().await.map_err(SvcError::plain)?;

            let settings = self.log_settings().await;
            validate_requested_selection(
                self.deps.client.as_ref(),
                settings,
                &directory,
                requested_model.as_ref(),
                requested_agent.as_deref(),
                requested_variant.as_deref(),
            )
            .await?;

            let mut target_session: Option<Value> = None;
            if action == "fork" {
                let message_id = as_non_empty_string(payload.get("messageId"));
                let reply = self
                    .deps
                    .client
                    .session_fork(&source_session_id, &directory, message_id.as_deref())
                    .await
                    .map_err(SvcError::plain)?;
                let session = reply.data.clone().unwrap_or(Value::Null);
                let Some(id) = session
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                else {
                    return Err(SvcError::plain("failed to fork session"));
                };
                target_session_id = id.to_string();
                target_session = Some(session);
                fork_created = true;
            }

            let baseline_assistant_message_id = self
                .latest_completed_assistant_message_id(&target_session_id, &directory)
                .await;

            let dispatch = self
                .dispatch_prompt(
                    &target_session_id,
                    &directory,
                    &prompt,
                    goal_input,
                    requested_model.clone(),
                    requested_agent.clone(),
                    requested_variant.clone(),
                    true,
                )
                .await?;

            let mut result = Map::new();
            result.insert("action".to_string(), Value::String(action.to_string()));
            result.insert(
                "sessionId".to_string(),
                Value::String(target_session_id.clone()),
            );
            result.insert("directory".to_string(), Value::String(directory.clone()));
            if action == "fork" {
                result.insert(
                    "sourceSessionId".to_string(),
                    Value::String(source_session_id.clone()),
                );
            }
            let target_title = target_session
                .as_ref()
                .and_then(|session| session.get("title"))
                .and_then(Value::as_str)
                .map(String::from);
            if let Some(title) = &target_title {
                result.insert("title".to_string(), Value::String(title.clone()));
            }
            if let Some(baseline) = &baseline_assistant_message_id {
                result.insert(
                    "baselineAssistantMessageId".to_string(),
                    Value::String(baseline.clone()),
                );
            }
            result.insert(
                "model".to_string(),
                dispatch
                    .model
                    .clone()
                    .map(|m| m.to_json())
                    .unwrap_or(Value::Null),
            );
            if let Some(agent) = &dispatch.agent {
                result.insert("agent".to_string(), Value::String(agent.clone()));
            }
            if let Some(variant) = &dispatch.variant {
                result.insert("variant".to_string(), Value::String(variant.clone()));
            }
            result.insert(
                "promptDispatched".to_string(),
                Value::Bool(dispatch.prompt_dispatched),
            );
            if let Some(prompt_error) = &dispatch.prompt_error {
                result.insert(
                    "promptError".to_string(),
                    Value::String(prompt_error.clone()),
                );
            }
            result.insert(
                "dispatchedAsCommand".to_string(),
                Value::Bool(dispatch.dispatched_as_command),
            );
            if goal_input.enabled {
                result.insert("goalEnabled".to_string(), Value::Bool(true));
            }
            if let Some(token_budget) = goal_input.token_budget {
                result.insert("goalTokenBudget".to_string(), json!(token_budget));
            }

            if action == "fork" {
                let mut event = Map::new();
                event.insert(
                    "sessionID".to_string(),
                    Value::String(target_session_id.clone()),
                );
                event.insert("directory".to_string(), Value::String(directory.clone()));
                event.insert(
                    "sourceSessionID".to_string(),
                    Value::String(source_session_id.clone()),
                );
                if let Some(title) = &target_title {
                    event.insert("title".to_string(), Value::String(title.clone()));
                }
                event.insert(
                    "model".to_string(),
                    dispatch
                        .model
                        .clone()
                        .map(|m| m.to_json())
                        .unwrap_or(Value::Null),
                );
                if let Some(agent) = &dispatch.agent {
                    event.insert("agent".to_string(), Value::String(agent.clone()));
                }
                if let Some(variant) = &dispatch.variant {
                    event.insert("variant".to_string(), Value::String(variant.clone()));
                }
                event.insert(
                    "promptDispatched".to_string(),
                    Value::Bool(dispatch.prompt_dispatched),
                );
                event.insert(
                    "dispatchedAsCommand".to_string(),
                    Value::Bool(dispatch.dispatched_as_command),
                );
                if goal_input.enabled {
                    event.insert("goalEnabled".to_string(), Value::Bool(true));
                }
                if let Some(token_budget) = goal_input.token_budget {
                    event.insert("goalTokenBudget".to_string(), json!(token_budget));
                }
                event.insert("createdAt".to_string(), json!(now_ms()));
                (self.deps.emit_session_created)(&Value::Object(event));
            }

            Ok(Value::Object(result))
        }
        .await;

        match outcome {
            Ok(result) => Ok(result),
            Err(mut error) => {
                // JS: `Number(error?.statusCode) || 500` — the status of the
                // inner error survives; partial details attach when the fork
                // was created or goal metadata was configured.
                if fork_created || error.goal_configured {
                    error.partial = Some(PartialDetails {
                        action: if fork_created {
                            "fork-created".to_string()
                        } else {
                            "goal-configured".to_string()
                        },
                        session_id: target_session_id.clone(),
                        directory: directory.clone(),
                    });
                }
                Err(error)
            }
        }
    }

    /// 向既有会话派发 prompt（`action = "send"`）。
    pub async fn send(&self, session_id: &str, payload: &Value) -> Result<Value, SvcError> {
        self.run_existing("send", session_id, payload).await
    }

    /// fork 既有会话后向新会话派发 prompt（`action = "fork"`）。
    pub async fn fork(&self, session_id: &str, payload: &Value) -> Result<Value, SvcError> {
        self.run_existing("fork", session_id, payload).await
    }
}

/// 当前 Unix 时间戳（毫秒），用于事件负载的 `createdAt` 字段。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Shared tail of `latestUserMessageID` / `latestCompletedAssistantMessageID`:
/// newest message of `role` by `time.created` (ties keep the later entry,
/// matching the JS `>=` comparison).
///
/// 中文说明：`latestUserMessageID` / `latestCompletedAssistantMessageID`
/// 的共用尾部——按 `time.created` 取指定角色的最新消息 ID，平局保留靠后
/// 条目（对齐 JS 的 `>=` 比较）。
fn latest_message_id(reply: &EngineReply, role: &str) -> Option<String> {
    let messages = reply.data.as_ref().and_then(Value::as_array)?;
    let mut latest: Option<&Value> = None;
    for message in messages {
        let Some(info) = message.get("info") else {
            continue;
        };
        if info.get("role").and_then(Value::as_str) != Some(role) {
            continue;
        }
        let created = info
            .get("time")
            .and_then(|time| time.get("created"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let latest_created = latest
            .and_then(|info| info.get("time"))
            .and_then(|time| time.get("created"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        if latest.is_none() || created >= latest_created {
            latest = Some(info);
        }
    }
    as_non_empty_string(latest.and_then(|info| info.get("id")))
}
