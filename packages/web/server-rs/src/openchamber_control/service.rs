//! Port of `server/lib/openchamber-control/service.js` — the typed control
//! contract shared by the OpenChamber CLI route and the managed
//! `openchamber` tool: project, model, session, scheduled-task, browser,
//! and memory orchestration over one fixed action allowlist.
//!
//! Invariants (DOCUMENTATION.md):
//! - session status/messages come from official directory-scoped engine
//!   APIs; message output includes only ordered `text` parts;
//! - wait never treats an initial idle response as completion after
//!   dispatch — observed activity or a newly completed assistant message
//!   is required; timeout is a failure, never an authoritative idle;
//! - usage errors name the missing or conflicting input;
//! - an explicitly requested model/agent/variant is validated by the
//!   session service before any side effect (its port owns that);
//! - `browser.capture` writes its image on the server, into
//!   `.ompchamber/screenshots/` under the scoped project directory, and
//!   returns the project-relative path rather than the image bytes.
//!
//! JS DI points map to injectable seams exactly: `sessionService`,
//! `scheduledTaskService`, `browserControl`, `agentMemoryActions`,
//! `createClient` (engine client), `sleep`, `now`. Until the
//! openchamber-sessions / browser-control / agent-memory ports land, the
//! session/browser/memory seams default to `None` and their actions
//! answer with the same "not available on this server" 503 shape the JS
//! service uses for a null `browserControl`/`agentMemoryActions`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;
use serde_json::{Map, Value, json};

use crate::engine::EngineState;
use crate::scheduled_tasks::runtime::{ProjectRef, ProjectsAccess};
use crate::scheduled_tasks::service::{ScheduledTaskService, SettingsProjects};
use crate::settings::normalization::{js_truthy, path_resolve, sanitize_projects};
use crate::settings::runtime::SettingsStore;

use super::actions::ALL_ACTIONS;
use super::engine_client::{BoxFut, ClientFactory, EngineClient, engine_client_factory};
use super::error::ControlError;
use super::screenshots::write_screenshot;

const DEFAULT_WAIT_TIMEOUT_SECONDS: u64 = 600;
const MAX_WAIT_TIMEOUT_SECONDS: u64 = 86_400;
const WAIT_POLL_INTERVAL_MS: i64 = 500;
/// Opening a page waits for the navigation to settle, so its budget has to
/// exceed the client's own wait; sharing one timeout with the quick
/// actions made a slow page indistinguishable from an unreachable browser.
const BROWSER_OPEN_TIMEOUT_MS: u64 = 45_000;
const BROWSER_ACTION_TIMEOUT_MS: u64 = 20_000;

const SCHEDULE_TASK_ID_ACTIONS: [&str; 3] = ["schedule.run", "schedule.delete", "schedule.toggle"];

// ---------------------------------------------------------------------------
// Seams (JS dependency-injection points)
// ---------------------------------------------------------------------------

/// `sessionService` from `openchamber-sessions/routes.js` — create/send/
/// fork with validation, worktrees, goals, and dispatch confirmation.
pub trait SessionService: Send + Sync {
    fn create<'a>(&'a self, payload: &'a Value) -> BoxFut<'a, Result<Value, ControlError>>;
    fn send<'a>(
        &'a self,
        session_id: &'a str,
        payload: &'a Value,
    ) -> BoxFut<'a, Result<Value, ControlError>>;
    fn fork<'a>(
        &'a self,
        session_id: &'a str,
        payload: &'a Value,
    ) -> BoxFut<'a, Result<Value, ControlError>>;
}

/// `browserControl` — the in-app browser panel broker (`request(action,
/// parameters, { signal, timeoutMs })`).
pub trait BrowserControl: Send + Sync {
    fn request<'a>(
        &'a self,
        action: &'a str,
        parameters: &'a Value,
        timeout_ms: u64,
    ) -> BoxFut<'a, Result<Value, ControlError>>;
}

/// `agentMemoryActions` — the managed memory tool adapter.
pub trait AgentMemoryActions: Send + Sync {
    fn execute<'a>(
        &'a self,
        action: &'a str,
        input: &'a Value,
        context_directory: Option<&'a str>,
    ) -> BoxFut<'a, Result<Value, ControlError>>;
}

/// `scheduledTaskService` from `scheduled-tasks/service.js` — the surface
/// the control service composes (including `resolveProjectID` and
/// `setEnabled`, which the ported scheduled-tasks module does not expose
/// and live in the adapter below).
pub trait ScheduleService: Send + Sync {
    fn status(&self) -> Value;
    fn resolve_project_id<'a>(
        &'a self,
        project_id: Option<&'a str>,
        directory: Option<&'a str>,
    ) -> BoxFut<'a, Result<String, ControlError>>;
    fn list<'a>(&'a self, project_id: &'a str) -> BoxFut<'a, Result<Vec<Value>, ControlError>>;
    fn upsert<'a>(
        &'a self,
        project_id: &'a str,
        task: &'a Value,
    ) -> BoxFut<'a, Result<Value, ControlError>>;
    fn run<'a>(
        &'a self,
        project_id: &'a str,
        task_id: &'a str,
    ) -> BoxFut<'a, Result<Value, ControlError>>;
    fn remove<'a>(
        &'a self,
        project_id: &'a str,
        task_id: &'a str,
    ) -> BoxFut<'a, Result<Vec<Value>, ControlError>>;
    fn set_enabled<'a>(
        &'a self,
        project_id: &'a str,
        task_id: &'a str,
        enabled: bool,
    ) -> BoxFut<'a, Result<Value, ControlError>>;
}

/// Adapter over the ported [`ScheduledTaskService`] plus the two pieces of
/// the JS service the ported module keeps private (`resolveProjectID`,
/// `setEnabled`, both straight ports of scheduled-tasks/service.js).
pub struct PortedScheduleService {
    service: Arc<ScheduledTaskService>,
    projects: Arc<dyn ProjectsAccess>,
}

impl PortedScheduleService {
    pub fn new(service: Arc<ScheduledTaskService>, projects: Arc<dyn ProjectsAccess>) -> Self {
        Self { service, projects }
    }

    async fn list_projects(&self) -> Result<Vec<ProjectRef>, ControlError> {
        self.projects
            .list_projects()
            .await
            .map_err(|error| ControlError::internal(error.to_string()))
    }
}

impl ScheduleService for PortedScheduleService {
    fn status(&self) -> Value {
        self.service.status()
    }

    fn resolve_project_id<'a>(
        &'a self,
        project_id: Option<&'a str>,
        directory: Option<&'a str>,
    ) -> BoxFut<'a, Result<String, ControlError>> {
        Box::pin(async move {
            let requested_id = non_empty(project_id);
            let requested_directory = non_empty(directory);
            if requested_id.is_some() && requested_directory.is_some() {
                return Err(ControlError::bad_request(
                    "Provide only one of projectId or directory",
                ));
            }
            if let Some(id) = requested_id {
                let projects = self.list_projects().await?;
                if projects.iter().any(|project| project.id == id) {
                    return Ok(id);
                }
                return Err(ControlError::not_found("Project not found"));
            }
            let Some(requested_directory) = requested_directory else {
                return Err(ControlError::bad_request(
                    "projectId or directory is required",
                ));
            };
            let resolved = path_resolve(&requested_directory)
                .to_string_lossy()
                .into_owned();
            let projects = self.list_projects().await?;
            let found = projects
                .iter()
                .find(|project| path_resolve(&project.path).to_string_lossy() == resolved);
            match found {
                Some(project) => Ok(project.id.clone()),
                None => Err(ControlError::not_found(format!(
                    "Project not found for directory: {resolved}"
                ))),
            }
        })
    }

    fn list<'a>(&'a self, project_id: &'a str) -> BoxFut<'a, Result<Vec<Value>, ControlError>> {
        Box::pin(async move {
            let tasks = self
                .service
                .list(project_id)
                .await
                .map_err(ControlError::from)?;
            tasks_value(tasks)
        })
    }

    fn upsert<'a>(
        &'a self,
        project_id: &'a str,
        task: &'a Value,
    ) -> BoxFut<'a, Result<Value, ControlError>> {
        Box::pin(async move {
            let result = self
                .service
                .upsert(project_id, task)
                .await
                .map_err(ControlError::from)?;
            Ok(json!({
                "task": result.get("task").cloned().unwrap_or(Value::Null),
                "created": result.get("created").cloned().unwrap_or(Value::Null),
            }))
        })
    }

    fn run<'a>(
        &'a self,
        project_id: &'a str,
        task_id: &'a str,
    ) -> BoxFut<'a, Result<Value, ControlError>> {
        Box::pin(async move {
            let success = self
                .service
                .run(project_id, task_id)
                .await
                .map_err(ControlError::from)?;
            let mut result = json!({
                "task": success
                    .task
                    .as_ref()
                    .map(|task| serde_json::to_value(task).unwrap_or(Value::Null))
                    .unwrap_or(Value::Null),
                "sessionId": success.session_id.clone().map_or(Value::Null, Value::String),
            });
            if let Some(persist_error) = &success.persist_error {
                result["persistError"] = json!(persist_error);
            }
            Ok(result)
        })
    }

    fn remove<'a>(
        &'a self,
        project_id: &'a str,
        task_id: &'a str,
    ) -> BoxFut<'a, Result<Vec<Value>, ControlError>> {
        Box::pin(async move {
            let tasks = self
                .service
                .remove(project_id, task_id)
                .await
                .map_err(ControlError::from)?;
            tasks_value(tasks)
        })
    }

    fn set_enabled<'a>(
        &'a self,
        project_id: &'a str,
        task_id: &'a str,
        enabled: bool,
    ) -> BoxFut<'a, Result<Value, ControlError>> {
        Box::pin(async move {
            let tasks = self.list(project_id).await?;
            let task = tasks
                .iter()
                .find(|entry| entry.get("id").and_then(Value::as_str) == Some(task_id))
                .cloned()
                .ok_or_else(|| ControlError::not_found("Task not found"))?;
            let mut updated = task;
            if !updated.is_object() {
                return Err(ControlError::internal("Stored task is not an object"));
            }
            updated
                .as_object_mut()
                .map(|object| object.insert("enabled".into(), json!(enabled)));
            let result = self
                .service
                .upsert(project_id, &updated)
                .await
                .map_err(ControlError::from)?;
            Ok(result.get("task").cloned().unwrap_or(Value::Null))
        })
    }
}

fn tasks_value(tasks: Vec<crate::projects::ScheduledTask>) -> Result<Vec<Value>, ControlError> {
    tasks
        .into_iter()
        .map(|task| {
            serde_json::to_value(&task).map_err(|error| ControlError::internal(error.to_string()))
        })
        .collect()
}

/// `sleep` seam (ms).
pub type SleepFn = Arc<dyn Fn(u64) -> BoxFut<'static, ()> + Send + Sync>;
/// `now` seam (unix ms).
pub type NowFn = Arc<dyn Fn() -> i64 + Send + Sync>;

pub fn default_sleep() -> SleepFn {
    Arc::new(|duration: u64| {
        Box::pin(tokio::time::sleep(Duration::from_millis(duration))) as BoxFut<'static, ()>
    })
}

pub fn default_now() -> NowFn {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0)
    })
}

/// The service's dependencies (JS `createOMPChamberControlService(deps)`).
pub struct ControlDeps {
    pub settings: Arc<SettingsStore>,
    pub projects: Arc<dyn ProjectsAccess>,
    pub scheduled: Arc<dyn ScheduleService>,
    pub session_service: Option<Arc<dyn SessionService>>,
    pub browser_control: Option<Arc<dyn BrowserControl>>,
    pub agent_memory: Option<Arc<dyn AgentMemoryActions>>,
    pub client_factory: ClientFactory,
    pub sleep: SleepFn,
    pub now: NowFn,
}

impl ControlDeps {
    /// Engine-backed defaults for the shared surfaces; the sibling-module
    /// seams start absent (their actions answer 503 until wired).
    pub fn for_engine(
        engine: Arc<EngineState>,
        settings: Arc<SettingsStore>,
        projects: Arc<dyn ProjectsAccess>,
        scheduled: Arc<dyn ScheduleService>,
    ) -> Self {
        Self {
            settings,
            projects,
            scheduled,
            session_service: None,
            browser_control: None,
            agent_memory: None,
            client_factory: engine_client_factory(engine),
            sleep: default_sleep(),
            now: default_now(),
        }
    }
}

pub struct ControlService {
    deps: ControlDeps,
}

// ---------------------------------------------------------------------------
// JS value helpers (service.js top-level coercions)
// ---------------------------------------------------------------------------

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|trimmed| !trimmed.is_empty())
        .map(String::from)
}

fn field_str<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

fn non_empty_field(input: &Value, key: &str) -> Option<String> {
    non_empty(field_str(input, key))
}

fn field_truthy(input: &Value, key: &str) -> bool {
    input.get(key).is_some_and(js_truthy)
}

fn is_true(input: &Value, key: &str) -> bool {
    matches!(input.get(key), Some(Value::Bool(true)))
}

/// JS `Number(value)` for the JSON values a caller can send (numbers,
/// numeric strings, booleans; null/objects/arrays are handled by the
/// caller or coerce to NaN).
fn js_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                Some(0.0)
            } else {
                trimmed.parse::<f64>().ok()
            }
        }
        Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        _ => None,
    }
}

fn is_safe_integer(value: f64) -> bool {
    value.is_finite() && value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_991.0
}

fn positive_integer(
    input: &Value,
    key: &str,
    fallback: u64,
    field: &str,
) -> Result<u64, ControlError> {
    let Some(value) = input.get(key) else {
        return Ok(fallback);
    };
    if value.is_null() {
        return Ok(fallback);
    }
    let number = js_number(value).unwrap_or(f64::NAN);
    if !is_safe_integer(number) || number < 1.0 {
        return Err(ControlError::bad_request(format!(
            "{field} must be a positive integer"
        )));
    }
    Ok(number as u64)
}

fn normalize_wait_timeout_ms(input: &Value) -> Result<u64, ControlError> {
    let seconds = match input.get("timeout") {
        None | Some(Value::Null) => DEFAULT_WAIT_TIMEOUT_SECONDS as f64,
        Some(value) => js_number(value).unwrap_or(f64::NAN),
    };
    if !is_safe_integer(seconds) || seconds < 1.0 || seconds > MAX_WAIT_TIMEOUT_SECONDS as f64 {
        return Err(ControlError::bad_request(format!(
            "timeout must be from 1 to {MAX_WAIT_TIMEOUT_SECONDS} seconds"
        )));
    }
    Ok((seconds as u64) * 1000)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageRole {
    All,
    User,
    Assistant,
}

impl MessageRole {
    fn as_str(self) -> &'static str {
        match self {
            MessageRole::All => "all",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
        }
    }

    fn from_field(value: &str) -> Option<Self> {
        match value {
            "all" => Some(MessageRole::All),
            "user" => Some(MessageRole::User),
            "assistant" => Some(MessageRole::Assistant),
            _ => None,
        }
    }
}

/// `extractTextMessages` — only ordered `text` parts, user/assistant roles,
/// sorted by creation time (stable, JS `sort` is stable too).
fn extract_text_messages(records: &[Value], role: MessageRole) -> Vec<Value> {
    let mut result: Vec<(f64, Value)> = Vec::new();
    for record in records {
        let Some(info) = record.get("info") else {
            continue;
        };
        let message_role = info.get("role").and_then(Value::as_str);
        let message_role = match message_role {
            Some("user") => MessageRole::User,
            Some("assistant") => MessageRole::Assistant,
            _ => continue,
        };
        if role != MessageRole::All && role != message_role {
            continue;
        }
        let text = record
            .get("parts")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter(|part| {
                        part.get("type").and_then(Value::as_str) == Some("text")
                            && part.get("text").is_some_and(Value::is_string)
                    })
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<String>()
            })
            .unwrap_or_default();
        let text = text.trim().to_string();
        if text.is_empty() {
            continue;
        }
        let provider = non_empty(info.get("providerID").and_then(Value::as_str));
        let model_id = non_empty(info.get("modelID").and_then(Value::as_str));
        let times = info.get("time");
        let finite_number = |value: Option<&Value>| -> Option<Value> {
            value
                .and_then(Value::as_number)
                .filter(|number| number.as_f64().is_some_and(f64::is_finite))
                .cloned()
                .map(Value::Number)
        };
        let created_at = finite_number(times.and_then(|time| time.get("created")));
        let completed_at = finite_number(times.and_then(|time| time.get("completed")));
        let message = json!({
            "id": non_empty(info.get("id").and_then(Value::as_str)).unwrap_or_default(),
            "role": message_role.as_str(),
            "createdAt": created_at.unwrap_or(Value::Null),
            "completedAt": completed_at.unwrap_or(Value::Null),
            "model": match (&provider, &model_id) {
                (Some(provider), Some(model)) => Some(format!("{provider}/{model}")),
                _ => None,
            },
            "text": text,
        });
        let order = message
            .get("createdAt")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        result.push((order, message));
    }
    result.sort_by(|left, right| {
        left.0
            .partial_cmp(&right.0)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    result.into_iter().map(|(_, message)| message).collect()
}

/// `parseModel` — `provider/model` with both halves present.
fn parse_model(value: Option<&str>) -> Result<(String, String), ControlError> {
    let Some(model) = non_empty(value) else {
        return Err(ControlError::bad_request("model is required"));
    };
    let Some(slash) = model.find('/') else {
        return Err(ControlError::bad_request(
            "model must be in provider/model format",
        ));
    };
    if slash == 0 || slash == model.len() - 1 {
        return Err(ControlError::bad_request(
            "model must be in provider/model format",
        ));
    }
    Ok((model[..slash].to_string(), model[slash + 1..].to_string()))
}

/// JS `parseInt(entry.trim(), 10)` — sign plus leading digits, NaN on
/// anything else.
fn js_parse_int(text: &str) -> Option<i64> {
    let trimmed = text.trim_start();
    let (negative, rest) = if let Some(rest) = trimmed.strip_prefix('-') {
        (true, rest)
    } else if let Some(rest) = trimmed.strip_prefix('+') {
        (false, rest)
    } else {
        (false, trimmed)
    };
    let digits: String = rest
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits
        .parse::<i64>()
        .ok()
        .map(|value| if negative { -value } else { value })
}

fn parse_weekdays(value: Option<&str>) -> Result<Vec<Value>, ControlError> {
    let Some(raw) = non_empty(value) else {
        return Err(ControlError::bad_request("weekly is required"));
    };
    let mut weekdays: Vec<i64> = raw
        .split(',')
        .map(js_parse_int)
        .collect::<Option<Vec<i64>>>()
        .ok_or_else(|| ControlError::bad_request("weekly must contain weekdays from 0 to 6"))?;
    if weekdays.iter().any(|entry| !(0..=6).contains(entry)) {
        return Err(ControlError::bad_request(
            "weekly must contain weekdays from 0 to 6",
        ));
    }
    weekdays.sort_unstable();
    weekdays.dedup();
    Ok(weekdays.into_iter().map(|entry| json!(entry)).collect())
}

/// `buildSchedule` — exactly one selector.
fn build_schedule(input: &Value) -> Result<Value, ControlError> {
    let daily = non_empty_field(input, "daily");
    let weekly = non_empty_field(input, "weekly");
    let once = non_empty_field(input, "once");
    let cron = non_empty_field(input, "cron");
    let selectors = [&daily, &weekly, &once, &cron]
        .iter()
        .filter(|entry| entry.is_some())
        .count();
    if selectors != 1 {
        return Err(ControlError::bad_request(
            "Provide exactly one of daily, weekly, once, or cron",
        ));
    }
    let timezone = non_empty_field(input, "timezone");
    let timezone = move |mut object: Map<String, Value>| {
        if let Some(zone) = timezone.clone() {
            object.insert("timezone".into(), json!(zone));
        }
        Value::Object(object)
    };
    if let Some(daily) = daily {
        let mut schedule = Map::new();
        schedule.insert("kind".into(), json!("daily"));
        schedule.insert("times".into(), json!([daily]));
        return Ok(timezone(schedule));
    }
    if let Some(weekly) = weekly {
        let Some(time) = non_empty_field(input, "time") else {
            return Err(ControlError::bad_request("time is required with weekly"));
        };
        let mut schedule = Map::new();
        schedule.insert("kind".into(), json!("weekly"));
        schedule.insert("weekdays".into(), json!(parse_weekdays(Some(&weekly))?));
        schedule.insert("times".into(), json!([time]));
        return Ok(timezone(schedule));
    }
    if let Some(once) = once {
        let Some(time) = non_empty_field(input, "time") else {
            return Err(ControlError::bad_request("time is required with once"));
        };
        let mut schedule = Map::new();
        schedule.insert("kind".into(), json!("once"));
        schedule.insert("date".into(), json!(once));
        schedule.insert("time".into(), json!(time));
        return Ok(timezone(schedule));
    }
    let Some(cron) = cron else {
        return Err(ControlError::bad_request(
            "Provide exactly one of daily, weekly, once, or cron",
        ));
    };
    let mut schedule = Map::new();
    schedule.insert("kind".into(), json!("cron"));
    schedule.insert("cron".into(), json!(cron));
    Ok(timezone(schedule))
}

/// `buildScheduledTask` — validation that protects side effects runs
/// before the task reaches the store.
fn build_scheduled_task(input: &Value) -> Result<Value, ControlError> {
    let Some(name) = non_empty_field(input, "name") else {
        return Err(ControlError::bad_request("name is required"));
    };
    let Some(prompt) = non_empty_field(input, "prompt") else {
        return Err(ControlError::bad_request("prompt is required"));
    };
    let (provider_id, model_id) = parse_model(field_str(input, "model"))?;
    let goal_token_budget = input.get("goalTokenBudget");
    if goal_token_budget.is_some() && !is_true(input, "goal") {
        return Err(ControlError::bad_request("goalTokenBudget requires goal"));
    }
    if let Some(budget) = goal_token_budget {
        // `Number.isSafeInteger` — no string/bool coercion here, unlike
        // the limit/timeout fields above.
        let valid = budget.as_f64().is_some_and(|value| {
            is_safe_integer(value) && (1000.0..=100_000_000.0).contains(&value)
        });
        if !valid {
            return Err(ControlError::bad_request(
                "goalTokenBudget must be from 1000 to 100000000",
            ));
        }
    }
    let mut execution = Map::new();
    execution.insert("prompt".into(), json!(prompt));
    execution.insert("providerID".into(), json!(provider_id));
    execution.insert("modelID".into(), json!(model_id));
    if let Some(agent) = non_empty_field(input, "agent") {
        execution.insert("agent".into(), json!(agent));
    }
    if let Some(variant) = non_empty_field(input, "variant") {
        execution.insert("variant".into(), json!(variant));
    }
    if is_true(input, "goal") {
        execution.insert("goalEnabled".into(), json!(true));
    }
    if let Some(budget) = goal_token_budget {
        execution.insert("goalTokenBudget".into(), budget.clone());
    }
    Ok(json!({
        "name": name,
        "enabled": !matches!(input.get("disabled"), Some(Value::Bool(true))),
        "schedule": build_schedule(input)?,
        "execution": Value::Object(execution),
    }))
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

impl ControlService {
    pub fn new(deps: ControlDeps) -> Self {
        Self { deps }
    }

    /// `execute` — one action from the fixed contract. Every failure is
    /// coerced to a [`ControlError`] (JS `asControlError` at the boundary).
    pub async fn execute(
        &self,
        action: &str,
        input: &Value,
        context_directory: Option<&str>,
    ) -> Result<Value, ControlError> {
        if !ALL_ACTIONS.contains(&action) {
            return Err(unsupported_action(action));
        }
        if action.starts_with("memory.") {
            let Some(memory) = &self.deps.agent_memory else {
                return Err(ControlError::new(
                    503,
                    "Agent memory is not available on this server",
                ));
            };
            return memory.execute(action, input, context_directory).await;
        }
        if action.starts_with("browser.") {
            let Some(browser) = &self.deps.browser_control else {
                return Err(ControlError::new(
                    503,
                    "The in-app browser is not available on this server",
                ));
            };
            return self
                .browser_action(browser, action, input, context_directory)
                .await;
        }
        if action == "projects.list" {
            return Ok(json!({ "projects": self.projects().await? }));
        }
        if action == "models.list" {
            return self.models().await;
        }
        if action == "schedule.status" {
            return Ok(self.deps.scheduled.status());
        }
        if action.starts_with("schedule.") {
            return self
                .execute_schedule_action(action, input, context_directory)
                .await;
        }
        if matches!(action, "session.create" | "session.send" | "session.fork") {
            return self
                .execute_session_action(action, input, context_directory)
                .await;
        }
        if action.starts_with("session.") {
            return self
                .execute_session_read(action, input, context_directory)
                .await;
        }
        Err(unsupported_action(action))
    }

    /// `projects()` — configured projects from settings, no HTTP or CLI
    /// round trip.
    async fn projects(&self) -> Result<Vec<Value>, ControlError> {
        let settings = self
            .deps
            .settings
            .read_migrated()
            .await
            .map_err(ControlError::from)?;
        let raw = settings.get("projects").filter(|value| !value.is_null());
        let sanitized = sanitize_projects(raw).unwrap_or_default();
        let mut result = Vec::with_capacity(sanitized.len());
        for project in sanitized {
            let id = project
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let path = path_resolve(
                project
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
            .to_string_lossy()
            .into_owned();
            let label = non_empty(project.get("label").and_then(Value::as_str))
                .or_else(|| {
                    std::path::Path::new(&path)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .map(String::from)
                        .filter(|name| !name.is_empty())
                })
                .unwrap_or_else(|| path.clone());
            result.push(json!({ "id": id, "path": path, "label": label }));
        }
        Ok(result)
    }

    /// `models()` — default, favorite, and recent model preferences.
    async fn models(&self) -> Result<Value, ControlError> {
        let settings = self
            .deps
            .settings
            .read_migrated()
            .await
            .map_err(ControlError::from)?;
        let preference = |key: &str| {
            non_empty(settings.get(key).and_then(Value::as_str)).map_or(Value::Null, Value::String)
        };
        let list = |key: &str| {
            settings
                .get(key)
                .filter(|value| value.is_array())
                .cloned()
                .unwrap_or_else(|| json!([]))
        };
        Ok(json!({
            "defaultModel": preference("defaultModel"),
            "defaultVariant": preference("defaultVariant"),
            "defaultAgent": preference("defaultAgent"),
            "favoriteModels": list("favoriteModels"),
            "recentModels": list("recentModels"),
        }))
    }

    async fn execute_schedule_action(
        &self,
        action: &str,
        input: &Value,
        context_directory: Option<&str>,
    ) -> Result<Value, ControlError> {
        let task_id = non_empty_field(input, "taskId");
        if SCHEDULE_TASK_ID_ACTIONS.contains(&action) && task_id.is_none() {
            return Err(ControlError::bad_request("taskId is required"));
        }
        let explicit_project_id = non_empty_field(input, "projectId");
        let explicit_directory = non_empty_field(input, "directory");
        // Explicit `projectId` or `directory` scope takes precedence over
        // the managed tool's current-session directory fallback; the
        // fallback never creates a conflicting second scope.
        let context_directory_fallback = if explicit_project_id.is_some() {
            None
        } else {
            non_empty(context_directory)
        };
        let project_id = self
            .deps
            .scheduled
            .resolve_project_id(
                explicit_project_id.as_deref(),
                explicit_directory
                    .as_deref()
                    .or(context_directory_fallback.as_deref()),
            )
            .await?;
        match action {
            "schedule.list" => Ok(json!({
                "scheduler": self.deps.scheduled.status(),
                "tasks": self.deps.scheduled.list(&project_id).await?,
            })),
            "schedule.create" => {
                let task = build_scheduled_task(input)?;
                self.deps.scheduled.upsert(&project_id, &task).await
            }
            "schedule.run" => {
                self.deps
                    .scheduled
                    .run(&project_id, task_id.as_deref().unwrap_or_default())
                    .await
            }
            "schedule.delete" => Ok(json!({
                "deleted": true,
                "tasks": self
                    .deps
                    .scheduled
                    .remove(&project_id, task_id.as_deref().unwrap_or_default())
                    .await?,
            })),
            "schedule.toggle" => {
                let Some(Value::Bool(disabled)) = input.get("disabled") else {
                    return Err(ControlError::bad_request(
                        "disabled is required for schedule.toggle",
                    ));
                };
                let enabled = !disabled;
                let task = self
                    .deps
                    .scheduled
                    .set_enabled(&project_id, task_id.as_deref().unwrap_or_default(), enabled)
                    .await?;
                Ok(json!({ "task": task, "enabled": enabled }))
            }
            _ => Err(unsupported_action(action)),
        }
    }

    /// `getClient` — engine client with the readiness preamble.
    async fn get_client(&self) -> Result<Arc<dyn EngineClient>, ControlError> {
        (self.deps.client_factory)().await
    }

    /// `sessionStatus` — directory-scoped official status lookup.
    async fn session_status(
        &self,
        client: &dyn EngineClient,
        session_id: &str,
        directory: Option<&str>,
    ) -> Result<Value, ControlError> {
        let response = client
            .session_status(directory)
            .await
            .map_err(ControlError::internal)?;
        let statuses = response.data;
        let Some(statuses) = statuses.filter(|value| value.is_object() && !value.is_array()) else {
            return Err(ControlError::internal("Invalid session status response"));
        };
        Ok(statuses
            .get(session_id)
            .filter(|value| js_truthy(value))
            .cloned()
            .unwrap_or_else(|| json!({ "type": "idle" })))
    }

    /// `sessionMessages` — ordered text projection, refetching the full
    /// history when the windowed fetch cannot fill the requested limit.
    async fn session_messages(
        &self,
        client: &dyn EngineClient,
        session_id: &str,
        directory: Option<&str>,
        role: MessageRole,
        limit: Option<u64>,
    ) -> Result<Vec<Value>, ControlError> {
        let fetch_limit = limit.map(|value| std::cmp::max(100, value.saturating_mul(4)));
        let mut response = client
            .session_messages(session_id, directory, fetch_limit)
            .await
            .map_err(ControlError::internal)?;
        let mut raw = response.data.as_ref().and_then(Value::as_array).cloned();
        let mut messages = extract_text_messages(raw.as_deref().unwrap_or_default(), role);
        if let Some(limit) = limit
            && let Some(fetch_limit) = fetch_limit
            && (messages.len() as u64) < limit
            && raw
                .as_ref()
                .is_some_and(|entries| entries.len() as u64 >= fetch_limit)
        {
            response = client
                .session_messages(session_id, directory, None)
                .await
                .map_err(ControlError::internal)?;
            raw = response.data.as_ref().and_then(Value::as_array).cloned();
            messages = extract_text_messages(raw.as_deref().unwrap_or_default(), role);
        }
        Ok(match limit {
            None => messages,
            Some(limit) => {
                let keep = (limit as usize).min(messages.len());
                messages.split_off(messages.len() - keep)
            }
        })
    }

    /// `waitForIdle` — never treats an initial idle response as completion
    /// after dispatch: observed activity or a newly completed assistant
    /// message is required, and a timeout is a failure.
    async fn wait_for_idle(
        &self,
        client: &dyn EngineClient,
        session_id: &str,
        directory: Option<&str>,
        timeout_ms: u64,
        require_activity: bool,
        baseline_message_id: Option<&str>,
        started_at: i64,
    ) -> Result<Value, ControlError> {
        let deadline = (self.deps.now)() + timeout_ms as i64;
        let mut observed_activity = false;
        loop {
            let status = self.session_status(client, session_id, directory).await?;
            let status_type = status.get("type").and_then(Value::as_str).unwrap_or("");
            if status_type == "busy" || status_type == "retry" {
                observed_activity = true;
            } else if !require_activity || observed_activity {
                return Ok(status);
            } else {
                let messages = self
                    .session_messages(
                        client,
                        session_id,
                        directory,
                        MessageRole::Assistant,
                        Some(1),
                    )
                    .await?;
                if let Some(message) = messages.first() {
                    let completed_at = message
                        .get("completedAt")
                        .and_then(Value::as_f64)
                        .filter(|value| *value != 0.0);
                    if let Some(completed_at) = completed_at {
                        let completed = match baseline_message_id {
                            Some(baseline) => {
                                message.get("id").and_then(Value::as_str) != Some(baseline)
                            }
                            None => completed_at >= started_at as f64,
                        };
                        if completed {
                            return Ok(status);
                        }
                    }
                }
            }
            let remaining = deadline - (self.deps.now)();
            if remaining <= 0 {
                return Err(ControlError::internal(format!(
                    "Session did not become idle within {} seconds",
                    (timeout_ms as f64 / 1000.0).ceil()
                )));
            }
            let pause = WAIT_POLL_INTERVAL_MS.min(remaining) as u64;
            (self.deps.sleep)(pause).await;
        }
    }

    /// `resolveSessionDirectory` — session.send/fork default the directory
    /// to the caller's context directory, which is wrong for sessions
    /// living in other worktrees: resolve the target session's directory
    /// from the global session list when the caller did not scope
    /// explicitly. A failed lookup never blocks the action.
    async fn resolve_session_directory(&self, session_id: &str) -> Option<String> {
        let client = self.get_client().await.ok()?;
        let response = client.experimental_session_list().await.ok()?;
        let sessions = response.data.as_ref().and_then(Value::as_array)?;
        let session = sessions
            .iter()
            .find(|entry| entry.get("id").and_then(Value::as_str) == Some(session_id))?;
        non_empty(session.get("directory").and_then(Value::as_str))
    }

    async fn execute_session_action(
        &self,
        action: &str,
        input: &Value,
        context_directory: Option<&str>,
    ) -> Result<Value, ControlError> {
        if input.get("timeout").is_some() && !is_true(input, "wait") {
            return Err(ControlError::bad_request("timeout requires wait"));
        }
        if is_true(input, "lastAssistant") && !is_true(input, "wait") {
            return Err(ControlError::bad_request("lastAssistant requires wait"));
        }
        let session_id = non_empty_field(input, "sessionId");
        let mut directory = non_empty_field(input, "directory").or_else(|| {
            if !field_truthy(input, "projectId") {
                non_empty(context_directory)
            } else {
                None
            }
        });
        if session_id.is_some()
            && action != "session.create"
            && non_empty_field(input, "directory").is_none()
            && !field_truthy(input, "projectId")
            && let Some(session_id) = session_id.as_deref()
            && let Some(resolved) = self.resolve_session_directory(session_id).await
        {
            directory = Some(resolved);
        }
        let mut payload = Map::new();
        if let Some(directory) = &directory {
            payload.insert("directory".into(), json!(directory));
        }
        if let Some(project_id) = non_empty_field(input, "projectId") {
            payload.insert("projectId".into(), json!(project_id));
        }
        if let Some(title) = non_empty_field(input, "title") {
            payload.insert("title".into(), json!(title));
        }
        if let Some(prompt) = non_empty_field(input, "prompt") {
            payload.insert("prompt".into(), json!(prompt));
        }
        if let Some(model) = non_empty_field(input, "model") {
            payload.insert("model".into(), json!(model));
        }
        if let Some(agent) = non_empty_field(input, "agent") {
            payload.insert("agent".into(), json!(agent));
        }
        if let Some(variant) = non_empty_field(input, "variant") {
            payload.insert("variant".into(), json!(variant));
        }
        if is_true(input, "goal") {
            payload.insert("goal".into(), json!(true));
        }
        if let Some(budget) = input.get("goalTokenBudget") {
            payload.insert("goalTokenBudget".into(), budget.clone());
        }
        if let Some(worktree) = non_empty_field(input, "worktree") {
            let mut worktree_payload = Map::new();
            worktree_payload.insert("name".into(), json!(worktree));
            if let Some(branch) = non_empty_field(input, "branch") {
                worktree_payload.insert("branchName".into(), json!(branch));
            }
            if let Some(start_ref) = non_empty_field(input, "startRef") {
                worktree_payload.insert("startRef".into(), json!(start_ref));
            }
            payload.insert("worktree".into(), Value::Object(worktree_payload));
        }
        if let Some(Value::Bool(set_upstream)) = input.get("setUpstream") {
            payload.insert("setUpstream".into(), json!(set_upstream));
        }
        if let Some(message_id) = non_empty_field(input, "messageId") {
            payload.insert("messageId".into(), json!(message_id));
        }
        let payload = Value::Object(payload);

        let started_at = (self.deps.now)();
        let result = if action == "session.create" {
            self.session_create(&payload).await?
        } else {
            let Some(session_id) = session_id.as_deref() else {
                return Err(ControlError::bad_request("sessionId is required"));
            };
            if action == "session.send" {
                self.session_send(session_id, &payload).await?
            } else {
                self.session_fork(session_id, &payload).await?
            }
        };

        if !is_true(input, "wait") {
            return Ok(strip_baseline(result));
        }
        let client = self.get_client().await?;
        let result_session_id =
            non_empty(result.get("sessionId").and_then(Value::as_str)).unwrap_or_default();
        let result_directory = result
            .get("directory")
            .and_then(Value::as_str)
            .map(String::from);
        let status = self
            .wait_for_idle(
                &*client,
                &result_session_id,
                result_directory.as_deref(),
                normalize_wait_timeout_ms(input)?,
                result.get("promptDispatched") == Some(&Value::Bool(true)),
                non_empty(
                    result
                        .get("baselineAssistantMessageId")
                        .and_then(Value::as_str),
                )
                .as_deref(),
                started_at,
            )
            .await?;
        let mut public_result = strip_baseline(result);
        if let Some(object) = public_result.as_object_mut() {
            object.insert("sessionStatus".into(), status);
        }
        if is_true(input, "lastAssistant") {
            let last = self
                .session_messages(
                    &*client,
                    &result_session_id,
                    result_directory.as_deref(),
                    MessageRole::Assistant,
                    Some(1),
                )
                .await?;
            let last_message = last.into_iter().next().unwrap_or(Value::Null);
            if let Some(object) = public_result.as_object_mut() {
                object.insert("lastAssistantMessage".into(), last_message);
            }
        }
        Ok(public_result)
    }

    async fn session_service(&self) -> Result<&Arc<dyn SessionService>, ControlError> {
        self.deps.session_service.as_ref().ok_or_else(|| {
            // JS always wires a session service; until the
            // openchamber-sessions port lands this is the honest answer in
            // the same shape the browser/memory seams use.
            ControlError::new(503, "Session actions are not available on this server")
        })
    }

    async fn session_create(&self, payload: &Value) -> Result<Value, ControlError> {
        self.session_service().await?.create(payload).await
    }

    async fn session_send(&self, session_id: &str, payload: &Value) -> Result<Value, ControlError> {
        self.session_service()
            .await?
            .send(session_id, payload)
            .await
    }

    async fn session_fork(&self, session_id: &str, payload: &Value) -> Result<Value, ControlError> {
        self.session_service()
            .await?
            .fork(session_id, payload)
            .await
    }

    /// Read-only session actions (`session.list`, `session.status`,
    /// `session.messages`).
    async fn execute_session_read(
        &self,
        action: &str,
        input: &Value,
        context_directory: Option<&str>,
    ) -> Result<Value, ControlError> {
        let directory =
            non_empty_field(input, "directory").or_else(|| non_empty(context_directory));
        let session_id = non_empty_field(input, "sessionId");
        let client = self.get_client().await?;
        if action == "session.list" {
            let limit = positive_integer(input, "limit", 10, "limit")?;
            let response = client
                .session_list(directory.as_deref())
                .await
                .map_err(ControlError::internal)?;
            let mut sessions: Vec<Value> = response
                .data
                .as_ref()
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if !is_true(input, "all") {
                sessions.retain(|session| {
                    !session
                        .get("time")
                        .and_then(|time| time.get("archived"))
                        .is_some_and(js_truthy)
                });
            }
            sessions.truncate(limit as usize);
            if is_true(input, "withStatus") {
                sessions = self.attach_session_statuses(&*client, sessions).await;
            }
            return Ok(json!({
                "sessions": sessions,
                "limit": limit,
                "directory": directory.clone().map_or(Value::Null, Value::String),
                "archived": if is_true(input, "all") { "included" } else { "excluded" },
            }));
        }
        let Some(session_id) = session_id else {
            return Err(ControlError::bad_request("sessionId is required"));
        };
        let Some(directory) = directory else {
            return Err(ControlError::bad_request("directory is required"));
        };
        if action == "session.status" {
            let status = self
                .session_status(&*client, &session_id, Some(&directory))
                .await?;
            return Ok(json!({
                "sessionId": session_id,
                "directory": directory,
                "sessionStatus": status,
            }));
        }
        if action == "session.messages" {
            if input.get("timeout").is_some() && !is_true(input, "wait") {
                return Err(ControlError::bad_request("timeout requires wait"));
            }
            let role = if is_true(input, "lastAssistant") {
                MessageRole::Assistant
            } else {
                match non_empty_field(input, "role").as_deref() {
                    None => MessageRole::All,
                    Some(requested) => MessageRole::from_field(requested).ok_or_else(|| {
                        ControlError::bad_request("role must be all, user, or assistant")
                    })?,
                }
            };
            let role_label = if is_true(input, "lastAssistant") {
                "assistant".to_string()
            } else {
                non_empty_field(input, "role").unwrap_or_else(|| "all".to_string())
            };
            let last = is_true(input, "last") || is_true(input, "lastAssistant");
            if is_true(input, "all") && (last || input.get("limit").is_some()) {
                return Err(ControlError::bad_request(
                    "all cannot be combined with last or limit",
                ));
            }
            if last && input.get("limit").is_some() {
                return Err(ControlError::bad_request(
                    "last cannot be combined with limit",
                ));
            }
            let current_status = if is_true(input, "wait") {
                self.wait_for_idle(
                    &*client,
                    &session_id,
                    Some(&directory),
                    normalize_wait_timeout_ms(input)?,
                    false,
                    None,
                    (self.deps.now)(),
                )
                .await?
            } else {
                self.session_status(&*client, &session_id, Some(&directory))
                    .await?
            };
            let limit = if is_true(input, "all") {
                None
            } else if last {
                Some(1)
            } else {
                Some(positive_integer(input, "limit", 10, "limit")?)
            };
            let messages = self
                .session_messages(&*client, &session_id, Some(&directory), role, limit)
                .await?;
            return Ok(json!({
                "sessionId": session_id,
                "directory": directory,
                "role": role_label,
                "sessionStatus": current_status,
                "messages": messages,
            }));
        }
        Err(unsupported_action(action))
    }

    /// One failed directory status lookup produces `unknown` for only that
    /// directory and does not erase other session results; a directory is
    /// looked up once however many sessions share it.
    async fn attach_session_statuses(
        &self,
        client: &dyn EngineClient,
        sessions: Vec<Value>,
    ) -> Vec<Value> {
        let mut directories: Vec<String> = Vec::new();
        for session in &sessions {
            if let Some(directory) = non_empty(session.get("directory").and_then(Value::as_str))
                && !directories.contains(&directory)
            {
                directories.push(directory);
            }
        }
        let lookups = join_all(directories.iter().map(|directory| async move {
            let response = client.session_status(Some(directory)).await;
            (directory.clone(), response)
        }))
        .await;
        let mut by_directory = std::collections::HashMap::new();
        for (directory, response) in lookups {
            by_directory.insert(directory, response);
        }
        sessions
            .into_iter()
            .map(|session| {
                let directory = non_empty(session.get("directory").and_then(Value::as_str));
                let status = match directory.as_deref().and_then(|key| by_directory.get(key)) {
                    // No directory, or the lookup was rejected: unknown for
                    // only this session (`catch(() => null)` in JS).
                    None | Some(Err(_)) => json!({ "type": "unknown" }),
                    Some(Ok(response)) => session
                        .get("id")
                        .and_then(Value::as_str)
                        .and_then(|id| response.data.as_ref().and_then(|data| data.get(id)))
                        .filter(|value| js_truthy(value))
                        .cloned()
                        .unwrap_or_else(|| json!({ "type": "idle" })),
                };
                let mut with_status = session;
                if let Some(object) = with_status.as_object_mut() {
                    object.insert("status".into(), status);
                }
                with_status
            })
            .collect()
    }

    /// `browserAction` — validates browser inputs here rather than in the
    /// renderer: an invalid call should come back as a usage error the
    /// agent can correct, without waking a client or waiting for a round
    /// trip.
    async fn browser_action(
        &self,
        browser: &Arc<dyn BrowserControl>,
        action: &str,
        input: &Value,
        context_directory: Option<&str>,
    ) -> Result<Value, ControlError> {
        let mut parameters = Map::new();
        let read_viewport =
            |required: bool, parameters: &mut Map<String, Value>| -> Result<(), ControlError> {
                let Some(viewport) = non_empty_field(input, "viewport") else {
                    if required {
                        return Err(ControlError::bad_request(
                            "viewport is required for browser.resize",
                        ));
                    }
                    return Ok(());
                };
                if !matches!(viewport.as_str(), "mobile" | "tablet" | "desktop" | "fill") {
                    return Err(ControlError::bad_request(
                        "viewport must be mobile, tablet, desktop, or fill",
                    ));
                }
                parameters.insert("viewport".into(), json!(viewport));
                Ok(())
            };

        if action == "browser.resize" {
            read_viewport(true, &mut parameters)?;
        }
        if action == "browser.capture"
            && let Some(label) = non_empty_field(input, "label")
        {
            parameters.insert("label".into(), json!(label));
        }
        if action == "browser.open" {
            read_viewport(false, &mut parameters)?;
            let Some(url) = non_empty_field(input, "url") else {
                return Err(ControlError::bad_request(
                    "url is required for browser.open",
                ));
            };
            let parsed = url::Url::parse(&url)
                .map_err(|_| ControlError::bad_request("url must be an absolute http(s) URL"))?;
            if parsed.scheme() != "http" && parsed.scheme() != "https" {
                return Err(ControlError::bad_request("url must use http or https"));
            }
            parameters.insert("url".into(), json!(parsed.to_string()));
        }
        if action == "browser.click" {
            let selector = non_empty_field(input, "selector");
            let text = non_empty_field(input, "text");
            if selector.is_none() && text.is_none() {
                return Err(ControlError::bad_request(
                    "browser.click requires selector or text",
                ));
            }
            if let Some(selector) = selector {
                parameters.insert("selector".into(), json!(selector));
            }
            if let Some(text) = text {
                parameters.insert("text".into(), json!(text));
            }
        }
        if action == "browser.snapshot"
            && let Some(selector) = non_empty_field(input, "selector")
        {
            parameters.insert("selector".into(), json!(selector));
        }
        if action == "browser.inspect" {
            let Some(selector) = non_empty_field(input, "selector") else {
                return Err(ControlError::bad_request(
                    "selector is required for browser.inspect",
                ));
            };
            parameters.insert("selector".into(), json!(selector));
        }
        if action == "browser.type" {
            let Some(selector) = non_empty_field(input, "selector") else {
                return Err(ControlError::bad_request(
                    "selector is required for browser.type",
                ));
            };
            let Some(value) = input.get("value").and_then(Value::as_str) else {
                return Err(ControlError::bad_request(
                    "value is required for browser.type",
                ));
            };
            parameters.insert("selector".into(), json!(selector));
            parameters.insert("value".into(), json!(value));
            parameters.insert("submit".into(), json!(is_true(input, "submit")));
        }
        if action == "browser.scroll" {
            let selector = non_empty_field(input, "selector");
            let direction = non_empty_field(input, "direction");
            if selector.is_none() && direction.is_none() {
                return Err(ControlError::bad_request(
                    "browser.scroll requires direction or selector",
                ));
            }
            if let Some(direction) = &direction
                && !matches!(direction.as_str(), "up" | "down" | "top" | "bottom")
            {
                return Err(ControlError::bad_request(
                    "direction must be up, down, top, or bottom",
                ));
            }
            if let Some(selector) = selector {
                parameters.insert("selector".into(), json!(selector));
            }
            if let Some(direction) = direction {
                parameters.insert("direction".into(), json!(direction));
            }
        }

        let parameters = Value::Object(parameters);
        let timeout_ms = if action == "browser.open" {
            BROWSER_OPEN_TIMEOUT_MS
        } else {
            BROWSER_ACTION_TIMEOUT_MS
        };
        let result = browser.request(action, &parameters, timeout_ms).await?;

        if action == "browser.capture" {
            let directory =
                non_empty_field(input, "directory").or_else(|| non_empty(context_directory));
            let Some(directory) = directory else {
                return Err(ControlError::bad_request(
                    "directory is required to save a screenshot",
                ));
            };
            let capture = result.as_object().cloned().unwrap_or_default();
            let saved = write_screenshot(
                &directory,
                capture.get("base64").and_then(Value::as_str).unwrap_or(""),
                capture.get("mime").and_then(Value::as_str),
                field_str(input, "label"),
                (self.deps.now)(),
            )
            .await
            .map_err(ControlError::internal)?;
            // The base64 never goes back to the caller: it is large, and
            // the path is what an answer, a commit, or a review can use.
            let field_or_null = |key: &str| capture.get(key).cloned().filter(|v| !v.is_null());
            return Ok(json!({
                "path": saved.path,
                // Saving the file is only half of showing it: chat renders
                // the image paths written in a finished answer below it.
                "hint": format!(
                    "Write ![]({}) in your reply to show this image to the user; it is rendered under your message.",
                    saved.path
                ),
                "url": field_or_null("url").unwrap_or(Value::Null),
                "title": field_or_null("title").unwrap_or(Value::Null),
                "viewport": field_or_null("viewport").unwrap_or(Value::Null),
                "width": field_or_null("width").unwrap_or(Value::Null),
                "height": field_or_null("height").unwrap_or(Value::Null),
            }));
        }

        Ok(result)
    }
}

/// `{ ...result }` without `baselineAssistantMessageId` — the baseline is
/// service bookkeeping, not part of the public result.
fn strip_baseline(mut result: Value) -> Value {
    if let Some(object) = result.as_object_mut() {
        object.remove("baselineAssistantMessageId");
    }
    result
}

fn unsupported_action(action: &str) -> ControlError {
    ControlError::bad_request(format!(
        "Unsupported OMPChamber action: {}",
        if action.is_empty() { "missing" } else { action }
    ))
}

/// Settings-backed [`ProjectsAccess`] (the scheduled-tasks module's
/// `SettingsProjects` over the shared data directory).
pub fn settings_projects(data_dir: &Path) -> Arc<SettingsProjects> {
    Arc::new(SettingsProjects::new(data_dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openchamber_control::engine_client::EngineResponse;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

    // ------------------------------------------------------------------
    // Fakes (the JS suite's dependency doubles)
    // ------------------------------------------------------------------

    enum Reply {
        Data(Value),
        Fail(String),
    }

    /// The injected `client` + `createClient` double.
    struct FakeEngineClient {
        list: Mutex<VecDeque<Reply>>,
        status: Mutex<VecDeque<Reply>>,
        messages: Mutex<VecDeque<Reply>>,
        experimental: Mutex<VecDeque<Reply>>,
        list_calls: AtomicUsize,
        status_calls: AtomicUsize,
        messages_calls: AtomicUsize,
    }

    impl FakeEngineClient {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                list: Mutex::new(VecDeque::new()),
                status: Mutex::new(VecDeque::new()),
                messages: Mutex::new(VecDeque::new()),
                experimental: Mutex::new(VecDeque::new()),
                list_calls: AtomicUsize::new(0),
                status_calls: AtomicUsize::new(0),
                messages_calls: AtomicUsize::new(0),
            })
        }

        fn queue_list_data(&self, data: Value) {
            self.list
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(Reply::Data(data));
        }

        fn queue_status_data(&self, data: Value) {
            self.status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(Reply::Data(data));
        }

        fn queue_status_failure(&self, message: &str) {
            self.status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(Reply::Fail(message.to_string()));
        }

        fn queue_messages_data(&self, data: Value) {
            self.messages
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(Reply::Data(data));
        }

        fn queue_experimental(&self, data: Value) {
            self.experimental
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(Reply::Data(data));
        }

        fn reply(queue: &Mutex<VecDeque<Reply>>, default: Value) -> Result<EngineResponse, String> {
            let next = queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
            Ok(match next {
                Some(Reply::Data(data)) => EngineResponse {
                    data: Some(data),
                    error: None,
                },
                // A rejected promise (the JS suite's `mockRejectedValueOnce`
                // doubles); the honest client never produces this.
                Some(Reply::Fail(message)) => return Err(message),
                None => EngineResponse {
                    data: Some(default),
                    error: None,
                },
            })
        }
    }

    impl EngineClient for FakeEngineClient {
        fn session_list<'a>(
            &'a self,
            _directory: Option<&'a str>,
        ) -> BoxFut<'a, Result<EngineResponse, String>> {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            let response = Self::reply(&self.list, json!([]));
            Box::pin(async move { response })
        }

        fn session_status<'a>(
            &'a self,
            _directory: Option<&'a str>,
        ) -> BoxFut<'a, Result<EngineResponse, String>> {
            self.status_calls.fetch_add(1, Ordering::SeqCst);
            let response = Self::reply(&self.status, json!({}));
            Box::pin(async move { response })
        }

        fn session_messages<'a>(
            &'a self,
            _session_id: &'a str,
            _directory: Option<&'a str>,
            _limit: Option<u64>,
        ) -> BoxFut<'a, Result<EngineResponse, String>> {
            self.messages_calls.fetch_add(1, Ordering::SeqCst);
            let response = Self::reply(&self.messages, json!([]));
            Box::pin(async move { response })
        }

        fn experimental_session_list<'a>(&'a self) -> BoxFut<'a, Result<EngineResponse, String>> {
            let response = Self::reply(&self.experimental, json!([]));
            Box::pin(async move { response })
        }
    }

    struct FakeSession {
        create_result: Mutex<Option<Result<Value, ControlError>>>,
        run_result: Option<Result<Value, ControlError>>,
        create_calls: Mutex<Vec<Value>>,
        run_calls: Mutex<Vec<(&'static str, String, Value)>>,
    }

    impl FakeSession {
        fn set_create_result(&self, result: Result<Value, ControlError>) {
            *self.create_result.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
        }

        fn new() -> Arc<Self> {
            Arc::new(Self {
                create_result: Mutex::new(None),
                run_result: None,
                create_calls: Mutex::new(Vec::new()),
                run_calls: Mutex::new(Vec::new()),
            })
        }

        fn create_calls(&self) -> Vec<Value> {
            self.create_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }

        fn run_calls(&self) -> Vec<(&'static str, String, Value)> {
            self.run_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }
    }

    impl SessionService for FakeSession {
        fn create<'a>(&'a self, payload: &'a Value) -> BoxFut<'a, Result<Value, ControlError>> {
            self.create_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(payload.clone());
            let result = self
                .create_result
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or(Ok(json!({
                    "sessionId": "ses_1",
                    "directory": "/repo",
                    "promptDispatched": false,
                })));
            Box::pin(async move { result })
        }

        fn send<'a>(
            &'a self,
            session_id: &'a str,
            payload: &'a Value,
        ) -> BoxFut<'a, Result<Value, ControlError>> {
            self.run_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(("send", session_id.to_string(), payload.clone()));
            let result = self.run_result.clone().unwrap_or(Ok(json!({
                "sessionId": "ses_1",
                "directory": "/repo",
            })));
            Box::pin(async move { result })
        }

        fn fork<'a>(
            &'a self,
            session_id: &'a str,
            payload: &'a Value,
        ) -> BoxFut<'a, Result<Value, ControlError>> {
            self.run_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(("fork", session_id.to_string(), payload.clone()));
            let result = self.run_result.clone().unwrap_or(Ok(json!({
                "sessionId": "ses_1",
                "directory": "/repo",
            })));
            Box::pin(async move { result })
        }
    }

    struct FakeSchedule {
        status_value: Value,
        resolved: Mutex<Option<Result<String, ControlError>>>,
        list_result: Mutex<Option<Result<Vec<Value>, ControlError>>>,
        upsert_result: Mutex<Option<Result<Value, ControlError>>>,
        run_result: Mutex<Option<Result<Value, ControlError>>>,
        remove_result: Mutex<Option<Result<Vec<Value>, ControlError>>>,
        set_enabled_result: Mutex<Option<Result<Value, ControlError>>>,
        resolve_calls: Mutex<Vec<(Option<String>, Option<String>)>>,
        upsert_calls: Mutex<Vec<(String, Value)>>,
        set_enabled_calls: Mutex<Vec<(String, String, bool)>>,
        run_calls: AtomicUsize,
    }

    impl FakeSchedule {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                status_value: json!({ "enabledScheduledTasksCount": 0 }),
                resolved: Mutex::new(None),
                list_result: Mutex::new(None),
                upsert_result: Mutex::new(None),
                run_result: Mutex::new(None),
                remove_result: Mutex::new(None),
                set_enabled_result: Mutex::new(None),
                resolve_calls: Mutex::new(Vec::new()),
                upsert_calls: Mutex::new(Vec::new()),
                set_enabled_calls: Mutex::new(Vec::new()),
                run_calls: AtomicUsize::new(0),
            })
        }

        fn set_upsert_result(&self, result: Result<Value, ControlError>) {
            *self.upsert_result.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
        }

        fn set_list_result(&self, result: Result<Vec<Value>, ControlError>) {
            *self.list_result.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
        }

        fn set_enabled_result(&self, result: Result<Value, ControlError>) {
            *self
                .set_enabled_result
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(result);
        }
    }

    impl ScheduleService for FakeSchedule {
        fn status(&self) -> Value {
            self.status_value.clone()
        }

        fn resolve_project_id<'a>(
            &'a self,
            project_id: Option<&'a str>,
            directory: Option<&'a str>,
        ) -> BoxFut<'a, Result<String, ControlError>> {
            self.resolve_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((project_id.map(String::from), directory.map(String::from)));
            let result = self
                .resolved
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or_else(|| Ok("project-1".to_string()));
            Box::pin(async move { result })
        }

        fn list<'a>(
            &'a self,
            _project_id: &'a str,
        ) -> BoxFut<'a, Result<Vec<Value>, ControlError>> {
            let result = self
                .list_result
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or_else(|| Ok(Vec::new()));
            Box::pin(async move { result })
        }

        fn upsert<'a>(
            &'a self,
            project_id: &'a str,
            task: &'a Value,
        ) -> BoxFut<'a, Result<Value, ControlError>> {
            self.upsert_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((project_id.to_string(), task.clone()));
            let result = self
                .upsert_result
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or_else(|| Ok(json!({ "task": Value::Null, "created": true })));
            Box::pin(async move { result })
        }

        fn run<'a>(
            &'a self,
            _project_id: &'a str,
            _task_id: &'a str,
        ) -> BoxFut<'a, Result<Value, ControlError>> {
            self.run_calls.fetch_add(1, Ordering::SeqCst);
            let result = self
                .run_result
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or_else(|| Ok(json!({ "task": Value::Null, "sessionId": Value::Null })));
            Box::pin(async move { result })
        }

        fn remove<'a>(
            &'a self,
            _project_id: &'a str,
            _task_id: &'a str,
        ) -> BoxFut<'a, Result<Vec<Value>, ControlError>> {
            let result = self
                .remove_result
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or_else(|| Ok(Vec::new()));
            Box::pin(async move { result })
        }

        fn set_enabled<'a>(
            &'a self,
            project_id: &'a str,
            task_id: &'a str,
            enabled: bool,
        ) -> BoxFut<'a, Result<Value, ControlError>> {
            self.set_enabled_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((project_id.to_string(), task_id.to_string(), enabled));
            let result = self
                .set_enabled_result
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or_else(|| Ok(json!({ "id": "task-1", "enabled": false })));
            Box::pin(async move { result })
        }
    }

    struct FakeBrowser {
        result: Value,
        calls: Mutex<Vec<(String, Value)>>,
    }

    impl FakeBrowser {
        fn new(result: Value) -> Arc<Self> {
            Arc::new(Self {
                result,
                calls: Mutex::new(Vec::new()),
            })
        }
    }

    impl BrowserControl for FakeBrowser {
        fn request<'a>(
            &'a self,
            action: &'a str,
            parameters: &'a Value,
            _timeout_ms: u64,
        ) -> BoxFut<'a, Result<Value, ControlError>> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((action.to_string(), parameters.clone()));
            let result = self.result.clone();
            Box::pin(async move { Ok(result) })
        }
    }

    // ------------------------------------------------------------------
    // Harness
    // ------------------------------------------------------------------

    fn temp_dir(tag: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "oc-control-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&directory).expect("temp dir");
        directory
    }

    async fn write_settings(directory: &Path, settings: Value) -> Arc<SettingsStore> {
        let path = directory.join("settings.json");
        let serialized = serde_json::to_string_pretty(&settings).expect("settings json");
        tokio::fs::write(&path, serialized)
            .await
            .expect("write settings");
        Arc::new(SettingsStore::new(path))
    }

    struct Harness {
        service: ControlService,
        client: Arc<FakeEngineClient>,
        session: Arc<FakeSession>,
        schedule: Arc<FakeSchedule>,
    }

    #[allow(clippy::too_many_arguments)]
    fn harness(
        tag: &str,
        settings: Value,
        session: Option<Arc<FakeSession>>,
        schedule: Option<Arc<FakeSchedule>>,
        browser: Option<Arc<FakeBrowser>>,
        memory: Option<Arc<dyn AgentMemoryActions>>,
        client: Option<Arc<FakeEngineClient>>,
        sleep_now: Option<(SleepFn, NowFn)>,
    ) -> Harness {
        let data_dir = temp_dir(tag);
        let settings_store = futures::executor::block_on(write_settings(&data_dir, settings));
        let client = client.unwrap_or_else(FakeEngineClient::new);
        let session = session.unwrap_or_else(FakeSession::new);
        let schedule = schedule.unwrap_or_else(FakeSchedule::new);
        let (sleep, now) = sleep_now.unwrap_or_else(|| (default_sleep(), default_now()));
        let client_for_factory = Arc::clone(&client);
        let deps = ControlDeps {
            settings: settings_store,
            projects: settings_projects(&data_dir),
            scheduled: Arc::clone(&schedule) as Arc<dyn ScheduleService>,
            session_service: Some(Arc::clone(&session) as Arc<dyn SessionService>),
            browser_control: browser.map(|browser| browser as Arc<dyn BrowserControl>),
            agent_memory: memory,
            client_factory: Arc::new(move || {
                let client = Arc::clone(&client_for_factory);
                Box::pin(async move { Ok(client as Arc<dyn EngineClient>) })
            }),
            sleep,
            now,
        };
        Harness {
            service: ControlService::new(deps),
            client,
            session,
            schedule,
        }
    }

    fn default_settings() -> Value {
        json!({
            "projects": [{ "id": "project-1", "path": "/repo", "label": "Repo" }],
            "defaultModel": "provider/model",
            "favoriteModels": [],
            "recentModels": [],
        })
    }

    // ------------------------------------------------------------------
    // service.test.js
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn serves_project_and_model_projections_without_a_round_trip() {
        // The settings file needs a real project directory for
        // sanitizeProjects to keep it, and the deterministic-id migration
        // rewrites `id` from the path on the migrated read, so store the
        // id it produces (the JS double skipped the migration entirely).
        let data_dir = temp_dir("projects");
        let repo = data_dir.join("repo");
        std::fs::create_dir_all(&repo).expect("repo dir");
        let canonical = crate::settings::normalization::strip_verbatim_prefix(
            std::fs::canonicalize(&repo).expect("canonicalize"),
        );
        let mut settings = default_settings();
        settings["projects"][0]["path"] = json!(repo.to_string_lossy());
        settings["projects"][0]["id"] =
            json!(crate::settings::normalization::create_project_id_from_path(
                &canonical.to_string_lossy(),
            ));
        let store = write_settings(&data_dir, settings).await;
        let engine = EngineState::external("http://127.0.0.1:4096".to_string(), None);
        let client = FakeEngineClient::new();
        let deps = ControlDeps::for_engine(
            engine,
            store,
            settings_projects(&data_dir),
            FakeSchedule::new(),
        );
        let service = ControlService::new(deps);

        let projects = service
            .execute("projects.list", &json!({}), None)
            .await
            .expect("projects.list");
        let expected_path = canonical.to_string_lossy().into_owned();
        let expected_id =
            crate::settings::normalization::create_project_id_from_path(&expected_path);
        assert_eq!(
            projects,
            json!({
                "projects": [{
                    "id": expected_id,
                    "path": expected_path,
                    "label": "Repo",
                }]
            })
        );

        let models = service
            .execute("models.list", &json!({}), None)
            .await
            .expect("models.list");
        assert_eq!(models["defaultModel"], json!("provider/model"));
        assert_eq!(models["favoriteModels"], json!([]));
        assert_eq!(models["recentModels"], json!([]));
        assert_eq!(models["defaultVariant"], Value::Null);
        // The engine client was never needed for these projections.
        assert_eq!(client.list_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn maps_schedule_creation_into_the_shared_scheduled_task_service() {
        let harness = harness(
            "schedule-create",
            default_settings(),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        harness.schedule.set_upsert_result(Ok(json!({
            "task": { "id": "task-1" },
            "created": true,
        })));
        let result = harness
            .service
            .execute(
                "schedule.create",
                &json!({
                    "directory": "/repo",
                    "name": "Daily",
                    "prompt": "Run checks",
                    "model": "provider/model",
                    "daily": " 09:00 ",
                    "goal": true,
                    "goalTokenBudget": 5000,
                }),
                None,
            )
            .await
            .expect("schedule.create");
        assert_eq!(
            result,
            json!({ "task": { "id": "task-1" }, "created": true })
        );

        let resolve_calls = harness
            .schedule
            .resolve_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(resolve_calls, vec![(None, Some("/repo".to_string()))]);
        let upsert_calls = harness
            .schedule
            .upsert_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(upsert_calls.len(), 1);
        assert_eq!(upsert_calls[0].0, "project-1");
        assert_eq!(upsert_calls[0].1["name"], json!("Daily"));
        assert_eq!(
            upsert_calls[0].1["schedule"],
            json!({ "kind": "daily", "times": ["09:00"] })
        );
        assert_eq!(
            upsert_calls[0].1["execution"]["prompt"],
            json!("Run checks")
        );
        assert_eq!(
            upsert_calls[0].1["execution"]["providerID"],
            json!("provider")
        );
        assert_eq!(upsert_calls[0].1["execution"]["modelID"], json!("model"));
        assert_eq!(upsert_calls[0].1["execution"]["goalEnabled"], json!(true));
        assert_eq!(
            upsert_calls[0].1["execution"]["goalTokenBudget"],
            json!(5000)
        );
        assert_eq!(upsert_calls[0].1["enabled"], json!(true));
    }

    #[tokio::test]
    async fn does_not_combine_an_explicit_schedule_project_with_the_tool_context_directory() {
        let harness = harness(
            "schedule-scope",
            default_settings(),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        harness
            .service
            .execute(
                "schedule.list",
                &json!({ "projectId": " project-1 " }),
                Some("/current-session"),
            )
            .await
            .expect("schedule.list");
        let resolve_calls = harness
            .schedule
            .resolve_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(resolve_calls, vec![(Some("project-1".to_string()), None)]);
    }

    #[tokio::test]
    async fn includes_scheduler_status_alongside_listed_tasks() {
        let harness = harness(
            "schedule-list",
            default_settings(),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        harness
            .schedule
            .set_list_result(Ok(vec![json!({ "id": "task-1" })]));
        let result = harness
            .service
            .execute("schedule.list", &json!({}), Some("/repo"))
            .await
            .expect("schedule.list");
        assert_eq!(
            result,
            json!({
                "scheduler": { "enabledScheduledTasksCount": 0 },
                "tasks": [{ "id": "task-1" }],
            })
        );
    }

    #[tokio::test]
    async fn toggles_a_scheduled_task_through_the_required_disabled_boolean() {
        let harness = harness(
            "schedule-toggle",
            default_settings(),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        harness
            .schedule
            .set_enabled_result(Ok(json!({ "id": "task-1", "enabled": false })));
        let missing = harness
            .service
            .execute(
                "schedule.toggle",
                &json!({ "taskId": "task-1" }),
                Some("/repo"),
            )
            .await
            .expect_err("missing disabled must fail");
        assert_eq!(missing.message, "disabled is required for schedule.toggle");
        assert_eq!(missing.status, 400);
        let result = harness
            .service
            .execute(
                "schedule.toggle",
                &json!({ "taskId": "task-1", "disabled": true }),
                Some("/repo"),
            )
            .await
            .expect("schedule.toggle");
        assert_eq!(
            result,
            json!({ "task": { "id": "task-1", "enabled": false }, "enabled": false })
        );
        let calls = harness
            .schedule
            .set_enabled_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(
            calls,
            vec![("project-1".to_string(), "task-1".to_string(), false)]
        );
    }

    #[tokio::test]
    async fn returns_an_actionable_taskid_error_before_resolving_schedule_scope() {
        let harness = harness(
            "schedule-taskid",
            default_settings(),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let error = harness
            .service
            .execute("schedule.run", &json!({}), Some("/repo"))
            .await
            .expect_err("missing taskId must fail");
        assert_eq!(error.message, "taskId is required");
        assert!(
            harness
                .schedule
                .resolve_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        );
        assert_eq!(harness.schedule.run_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn validates_wait_modifiers_before_creating_a_session() {
        let harness = harness(
            "session-validate",
            default_settings(),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let error = harness
            .service
            .execute(
                "session.create",
                &json!({ "directory": "/repo", "timeout": 30 }),
                None,
            )
            .await
            .expect_err("timeout without wait must fail");
        assert_eq!(error.message, "timeout requires wait");
        assert!(harness.session.create_calls().is_empty());
    }

    #[tokio::test]
    async fn uses_the_tool_context_directory_for_session_actions() {
        let harness = harness(
            "session-context",
            default_settings(),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        harness
            .service
            .execute(
                "session.create",
                &json!({ "title": "From tool" }),
                Some("/repo"),
            )
            .await
            .expect("session.create");
        let calls = harness.session.create_calls();
        assert_eq!(
            calls,
            vec![json!({ "directory": "/repo", "title": "From tool" })]
        );
    }

    #[tokio::test]
    async fn delegates_send_and_fork_directly_to_the_session_service() {
        for (action, method) in [("session.send", "send"), ("session.fork", "fork")] {
            let harness = harness(
                "session-delegate",
                default_settings(),
                None,
                None,
                None,
                None,
                None,
                None,
            );
            harness
                .service
                .execute(
                    action,
                    &json!({
                        "sessionId": "ses_1",
                        "directory": "/repo",
                        "prompt": "Continue",
                    }),
                    None,
                )
                .await
                .expect(action);
            let calls = harness.session.run_calls();
            assert_eq!(calls.len(), 1, "{action} must call once");
            assert_eq!(calls[0].0, method);
            assert_eq!(calls[0].1, "ses_1");
            assert_eq!(
                calls[0].2,
                json!({ "directory": "/repo", "prompt": "Continue" })
            );
        }
    }

    #[tokio::test]
    async fn resolves_the_target_directory_from_the_global_session_list() {
        let client = FakeEngineClient::new();
        client.queue_experimental(json!([
            { "id": "ses_other", "directory": "/repo/worktrees/other" },
            { "id": "ses_target", "directory": "/repo/worktrees/target" },
        ]));
        let session = FakeSession::new();
        let harness = harness(
            "session-resolve",
            default_settings(),
            Some(Arc::clone(&session)),
            None,
            None,
            None,
            Some(client),
            None,
        );
        let _ = harness
            .service
            .execute(
                "session.send",
                &json!({ "sessionId": "ses_target", "prompt": "Continue" }),
                Some("/repo"),
            )
            .await;
        let calls = session.run_calls();
        assert_eq!(
            calls[0].2,
            json!({ "directory": "/repo/worktrees/target", "prompt": "Continue" })
        );
    }

    #[tokio::test]
    async fn falls_back_to_the_context_directory_when_not_in_the_global_list() {
        let client = FakeEngineClient::new();
        client.queue_experimental(json!([]));
        let session = FakeSession::new();
        let harness = harness(
            "session-fallback",
            default_settings(),
            Some(Arc::clone(&session)),
            None,
            None,
            None,
            Some(client),
            None,
        );
        let _ = harness
            .service
            .execute(
                "session.send",
                &json!({ "sessionId": "ses_unknown", "prompt": "Continue" }),
                Some("/repo"),
            )
            .await;
        let calls = session.run_calls();
        assert_eq!(
            calls[0].2,
            json!({ "directory": "/repo", "prompt": "Continue" })
        );
    }

    #[tokio::test]
    async fn waits_past_initial_idle_until_a_completed_assistant_result_appears() {
        // Deterministic clock: sleep advances the timestamp instead of
        // waiting (the JS suite's `now`/`sleep` doubles).
        let timestamp = Arc::new(AtomicI64::new(1000));
        let now: NowFn = {
            let timestamp = Arc::clone(&timestamp);
            Arc::new(move || timestamp.load(Ordering::SeqCst))
        };
        let sleep: SleepFn = {
            let timestamp = Arc::clone(&timestamp);
            Arc::new(move |duration: u64| {
                let timestamp = Arc::clone(&timestamp);
                Box::pin(async move {
                    timestamp.fetch_add(duration as i64, Ordering::SeqCst);
                }) as BoxFut<'static, ()>
            })
        };

        let client = FakeEngineClient::new();
        client.queue_status_data(json!({ "ses_1": { "type": "idle" } }));
        client.queue_status_data(json!({ "ses_1": { "type": "idle" } }));
        client.queue_messages_data(json!([{
            "info": { "id": "msg_old", "role": "assistant", "time": { "completed": 900 } },
            "parts": [{ "type": "text", "text": "old" }],
        }]));
        client.queue_messages_data(json!([{
            "info": { "id": "msg_new", "role": "assistant", "time": { "completed": 1500 } },
            "parts": [{ "type": "text", "text": "done" }],
        }]));
        client.queue_messages_data(json!([{
            "info": { "id": "msg_new", "role": "assistant", "time": { "completed": 1500 } },
            "parts": [{ "type": "text", "text": "done" }],
        }]));

        let session = FakeSession::new();
        session.set_create_result(Ok(json!({
            "sessionId": "ses_1",
            "directory": "/repo",
            "promptDispatched": true,
            "baselineAssistantMessageId": "msg_old",
        })));
        let harness = harness(
            "session-wait",
            default_settings(),
            Some(Arc::clone(&session)),
            None,
            None,
            None,
            Some(client),
            Some((sleep, now)),
        );
        let result = harness
            .service
            .execute(
                "session.create",
                &json!({
                    "directory": "/repo",
                    "prompt": "work",
                    "wait": true,
                    "lastAssistant": true,
                    "timeout": 2,
                }),
                None,
            )
            .await
            .expect("session.create with wait");
        assert_eq!(result["sessionStatus"], json!({ "type": "idle" }));
        assert_eq!(result["lastAssistantMessage"]["id"], json!("msg_new"));
        assert_eq!(result["lastAssistantMessage"]["text"], json!("done"));
        // The baseline assistant message id never leaks into the result.
        assert!(result.get("baselineAssistantMessageId").is_none());
        assert_eq!(
            harness.client.status_calls.load(Ordering::SeqCst),
            2,
            "idle must be confirmed by a second poll"
        );
    }

    #[tokio::test]
    async fn session_wait_reports_a_timeout_as_a_failure() {
        let timestamp = Arc::new(AtomicI64::new(1000));
        let clock = Arc::clone(&timestamp);
        let now: NowFn = Arc::new(move || clock.load(Ordering::SeqCst));
        // Sleep jumps past the deadline: the next remaining check fails.
        let sleep: SleepFn = {
            let timestamp = Arc::clone(&timestamp);
            Arc::new(move |duration: u64| {
                let timestamp = Arc::clone(&timestamp);
                Box::pin(async move {
                    timestamp.fetch_add((duration as i64) * 100, Ordering::SeqCst);
                }) as BoxFut<'static, ()>
            })
        };
        let client = FakeEngineClient::new();
        client.queue_status_data(json!({ "ses_1": { "type": "busy" } }));
        client.queue_status_data(json!({ "ses_1": { "type": "busy" } }));
        let harness = harness(
            "session-timeout",
            default_settings(),
            None,
            None,
            None,
            None,
            Some(client),
            Some((sleep, now)),
        );
        let error = harness
            .service
            .execute(
                "session.messages",
                &json!({
                    "sessionId": "ses_1",
                    "directory": "/repo",
                    "wait": true,
                    "timeout": 2,
                }),
                None,
            )
            .await
            .expect_err("a busy session must time out");
        assert_eq!(error.status, 500);
        assert_eq!(
            error.message,
            "Session did not become idle within 2 seconds"
        );
    }

    #[tokio::test]
    async fn filters_archived_sessions_and_adds_directory_scoped_statuses() {
        let client = FakeEngineClient::new();
        client.queue_list_data(json!([
            { "id": "ses_active", "directory": "/repo", "time": {} },
            { "id": "ses_archived", "directory": "/repo", "time": { "archived": 100 } },
            { "id": "ses_other", "directory": "/other", "time": {} },
        ]));
        client.queue_status_data(json!({ "ses_active": { "type": "busy" } }));
        client.queue_status_failure("unavailable");
        let harness = harness(
            "session-list",
            default_settings(),
            None,
            None,
            None,
            None,
            Some(client),
            None,
        );
        let result = harness
            .service
            .execute(
                "session.list",
                &json!({ "limit": 10, "withStatus": true }),
                None,
            )
            .await
            .expect("session.list");
        assert_eq!(
            result,
            json!({
                "sessions": [
                    {
                        "id": "ses_active",
                        "directory": "/repo",
                        "time": {},
                        "status": { "type": "busy" },
                    },
                    {
                        "id": "ses_other",
                        "directory": "/other",
                        "time": {},
                        "status": { "type": "unknown" },
                    },
                ],
                "limit": 10,
                "directory": Value::Null,
                "archived": "excluded",
            })
        );
    }

    #[tokio::test]
    async fn names_limit_in_positive_integer_validation_errors() {
        let harness = harness(
            "session-limit",
            default_settings(),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let error = harness
            .service
            .execute("session.list", &json!({ "limit": 0 }), None)
            .await
            .expect_err("limit 0 must fail");
        assert_eq!(error.message, "limit must be a positive integer");
        assert_eq!(
            harness.client.list_calls.load(Ordering::SeqCst),
            0,
            "validation runs before the engine call"
        );
    }

    #[tokio::test]
    async fn projects_only_ordered_text_parts_from_session_messages() {
        let client = FakeEngineClient::new();
        client.queue_messages_data(json!([
            {
                "info": {
                    "id": "msg_assistant",
                    "role": "assistant",
                    "providerID": "openai",
                    "modelID": "gpt-5.4-mini",
                    "time": { "created": 20, "completed": 30 },
                },
                "parts": [
                    { "type": "reasoning", "text": "hidden" },
                    { "type": "text", "text": "First " },
                    { "type": "tool" },
                    { "type": "text", "text": "answer" },
                ],
            },
            {
                "info": { "id": "msg_user", "role": "user", "time": { "created": 10 } },
                "parts": [{ "type": "text", "text": "Question" }],
            },
            {
                "info": { "id": "msg_tool", "role": "assistant", "time": { "created": 15 } },
                "parts": [{ "type": "tool" }],
            },
        ]));
        let harness = harness(
            "session-messages",
            default_settings(),
            None,
            None,
            None,
            None,
            Some(client),
            None,
        );
        let result = harness
            .service
            .execute(
                "session.messages",
                &json!({
                    "sessionId": "ses_1",
                    "directory": "/repo",
                    "role": "all",
                    "all": true,
                }),
                None,
            )
            .await
            .expect("session.messages");
        assert_eq!(
            result,
            json!({
                "sessionId": "ses_1",
                "directory": "/repo",
                "role": "all",
                "sessionStatus": { "type": "idle" },
                "messages": [
                    {
                        "id": "msg_user",
                        "role": "user",
                        "createdAt": 10,
                        "completedAt": Value::Null,
                        "model": Value::Null,
                        "text": "Question",
                    },
                    {
                        "id": "msg_assistant",
                        "role": "assistant",
                        "createdAt": 20,
                        "completedAt": 30,
                        "model": "openai/gpt-5.4-mini",
                        "text": "First answer",
                    },
                ],
            })
        );
    }

    #[tokio::test]
    async fn rejects_actions_outside_the_fixed_contract() {
        let harness = harness(
            "unsupported",
            default_settings(),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let error = harness
            .service
            .execute("session.delete", &json!({}), None)
            .await
            .expect_err("session.delete is not in the contract");
        assert_eq!(error.status, 400);
        assert!(
            error.message.starts_with("Unsupported OMPChamber action"),
            "{}",
            error.message
        );
        let missing = harness
            .service
            .execute("", &json!({}), None)
            .await
            .expect_err("empty action");
        assert_eq!(missing.message, "Unsupported OMPChamber action: missing");
    }

    // ------------------------------------------------------------------
    // Browser capture (service.test.js "browser capture")
    // ------------------------------------------------------------------

    const PIXEL_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    #[tokio::test]
    async fn saves_the_image_beside_the_code_and_hands_back_a_usable_path() {
        let directory = temp_dir("capture");
        let browser = FakeBrowser::new(json!({
            "base64": PIXEL_BASE64,
            "mime": "image/png",
            "url": "http://localhost:3000/",
            "title": "App",
            "viewport": { "mode": "mobile", "width": 390, "height": 844 },
            "width": 390,
            "height": 844,
        }));
        let harness = harness(
            "browser-capture",
            default_settings(),
            None,
            None,
            Some(browser),
            None,
            None,
            None,
        );
        let result = harness
            .service
            .execute(
                "browser.capture",
                &json!({ "label": "After fix" }),
                directory.to_str(),
            )
            .await
            .expect("browser.capture");
        let path = result["path"].as_str().expect("path");
        assert!(
            path.starts_with(".ompchamber/screenshots/after-fix-"),
            "{path}"
        );
        assert!(path.ends_with(".png"), "{path}");
        assert_eq!(result["url"], json!("http://localhost:3000/"));
        assert_eq!(
            result["viewport"],
            json!({ "mode": "mobile", "width": 390, "height": 844 })
        );
        // The bytes stay on disk; a tool result is not a place to carry an
        // image.
        assert!(result.get("base64").is_none());
        let written = tokio::fs::read(directory.join(path))
            .await
            .expect("screenshot bytes");
        assert!(!written.is_empty());
    }

    #[tokio::test]
    async fn tells_the_agent_how_to_actually_show_the_image() {
        let directory = temp_dir("hint");
        let browser = FakeBrowser::new(json!({ "base64": PIXEL_BASE64, "mime": "image/png" }));
        let harness = harness(
            "browser-hint",
            default_settings(),
            None,
            None,
            Some(browser),
            None,
            None,
            None,
        );
        let result = harness
            .service
            .execute("browser.capture", &json!({}), directory.to_str())
            .await
            .expect("browser.capture");
        let path = result["path"].as_str().expect("path").to_string();
        let hint = result["hint"].as_str().expect("hint");
        assert!(hint.contains(&format!("![]({path})")), "hint: {hint}");
    }

    #[tokio::test]
    async fn refuses_to_capture_with_no_project_to_save_into() {
        let browser = FakeBrowser::new(json!({ "base64": PIXEL_BASE64, "mime": "image/png" }));
        let harness = harness(
            "browser-directory",
            default_settings(),
            None,
            None,
            Some(browser),
            None,
            None,
            None,
        );
        let error = harness
            .service
            .execute("browser.capture", &json!({}), None)
            .await
            .expect_err("no directory must fail");
        assert_eq!(error.status, 400);
        assert!(
            error.message.contains("directory is required"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn passes_a_label_through_and_validates_other_browser_actions() {
        let directory = temp_dir("label");
        let browser = FakeBrowser::new(json!({ "base64": PIXEL_BASE64, "mime": "image/png" }));
        let harness = harness(
            "browser-label",
            default_settings(),
            None,
            None,
            Some(Arc::clone(&browser)),
            None,
            None,
            None,
        );
        harness
            .service
            .execute(
                "browser.capture",
                &json!({ "label": "before" }),
                directory.to_str(),
            )
            .await
            .expect("browser.capture");
        let calls = browser
            .calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(
            calls,
            vec![("browser.capture".to_string(), json!({ "label": "before" }))]
        );

        // Input validation happens in the service, without waking a client.
        let click_error = harness
            .service
            .execute("browser.click", &json!({}), directory.to_str())
            .await
            .expect_err("click needs selector or text");
        assert_eq!(
            click_error.message,
            "browser.click requires selector or text"
        );
        let open_error = harness
            .service
            .execute(
                "browser.open",
                &json!({ "url": "ftp://example.com" }),
                directory.to_str(),
            )
            .await
            .expect_err("non-http url must fail");
        assert_eq!(open_error.message, "url must use http or https");
        let url_error = harness
            .service
            .execute(
                "browser.open",
                &json!({ "url": "not a url" }),
                directory.to_str(),
            )
            .await
            .expect_err("relative url must fail");
        assert_eq!(url_error.message, "url must be an absolute http(s) URL");
        let resize_error = harness
            .service
            .execute(
                "browser.resize",
                &json!({ "viewport": "huge" }),
                directory.to_str(),
            )
            .await
            .expect_err("bad viewport must fail");
        assert_eq!(
            resize_error.message,
            "viewport must be mobile, tablet, desktop, or fill"
        );
        let missing_viewport = harness
            .service
            .execute("browser.resize", &json!({}), directory.to_str())
            .await
            .expect_err("resize requires viewport");
        assert_eq!(
            missing_viewport.message,
            "viewport is required for browser.resize"
        );
    }

    // ------------------------------------------------------------------
    // Unavailable seams (service.js 503 branches; sessions pending port)
    // ------------------------------------------------------------------

    struct UnavailableMemory;

    impl AgentMemoryActions for UnavailableMemory {
        fn execute<'a>(
            &'a self,
            _action: &'a str,
            _input: &'a Value,
            _context_directory: Option<&'a str>,
        ) -> BoxFut<'a, Result<Value, ControlError>> {
            Box::pin(async { Ok(json!({ "remembered": true })) })
        }
    }

    #[tokio::test]
    async fn browser_and_memory_report_503_when_their_seams_are_absent() {
        let unavailable = harness(
            "unavailable",
            default_settings(),
            None,
            None,
            None, // no browserControl
            None, // no agentMemoryActions
            None,
            None,
        );
        let browser = unavailable
            .service
            .execute("browser.snapshot", &json!({}), None)
            .await
            .expect_err("no browser seam");
        assert_eq!(browser.status, 503);
        assert_eq!(
            browser.message,
            "The in-app browser is not available on this server"
        );
        let memory = unavailable
            .service
            .execute("memory.list", &json!({}), None)
            .await
            .expect_err("no memory seam");
        assert_eq!(memory.status, 503);
        assert_eq!(
            memory.message,
            "Agent memory is not available on this server"
        );

        // A wired memory seam is forwarded verbatim with its scope.
        let wired = harness(
            "memory-wired",
            default_settings(),
            None,
            None,
            None,
            Some(Arc::new(UnavailableMemory)),
            None,
            None,
        );
        let result = wired
            .service
            .execute("memory.save", &json!({ "title": "t" }), Some("/repo"))
            .await
            .expect("memory.save");
        assert_eq!(result, json!({ "remembered": true }));
    }

    #[tokio::test]
    async fn session_actions_report_503_until_the_sessions_port_lands() {
        let harness = harness_without_session();
        let error = harness
            .service
            .execute("session.send", &json!({ "sessionId": "ses_1" }), None)
            .await
            .expect_err("no session seam");
        assert_eq!(error.status, 503);
        assert_eq!(
            error.message,
            "Session actions are not available on this server"
        );
    }

    fn harness_without_session() -> Harness {
        let data_dir = temp_dir("no-session");
        let settings_store =
            futures::executor::block_on(write_settings(&data_dir, default_settings()));
        let _ = &data_dir;
        let engine = EngineState::external("http://127.0.0.1:4096".to_string(), None);
        let deps = ControlDeps::for_engine(
            engine,
            settings_store,
            settings_projects(&data_dir),
            FakeSchedule::new(),
        );
        Harness {
            service: ControlService::new(deps),
            client: FakeEngineClient::new(),
            session: FakeSession::new(),
            schedule: FakeSchedule::new(),
        }
    }
}
