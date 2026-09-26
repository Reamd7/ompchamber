//! Typed task model + dynamic-JSON normalizers for the per-project config.
//!
//! Port of the helper layer of `server/lib/projects/project-config.js`
//! (everything above `createProjectConfigRuntime`). The JS normalizes loose
//! objects; here the same rules run on `serde_json::Value` inputs and produce
//! the typed [`ScheduledTask`], whose serialized shape matches the JS objects
//! field-for-field (camelCase names, optional fields omitted, `providerID` /
//! `modelID` spelling preserved).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROJECT_CONFIG_VERSION: u64 = 1;
pub const MAX_TASK_NAME_LENGTH: usize = 80;
pub const MAX_TASK_PROMPT_LENGTH: usize = 20_000;
pub const MAX_CRON_LENGTH: usize = 200;
pub const MAX_LAST_ERROR_LENGTH: usize = 2_000;

// ============== small JS-semantic helpers ==============

/// `asNonEmptyString`: strings trim to a non-empty value.
pub(crate) fn as_non_empty_string(value: &Value) -> Option<String> {
    value.as_str().and_then(|s| {
        let trimmed = s.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn non_empty_str(s: &str) -> Option<String> {
    let trimmed = s.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// `clampLength`: truncate by UTF-16 code units (JS `String.length`/`slice`).
pub(crate) fn clamp_length_utf16(value: &str, max_length: usize) -> String {
    if value.encode_utf16().count() <= max_length {
        return value.to_string();
    }
    let units: Vec<u16> = value.encode_utf16().take(max_length).collect();
    String::from_utf16_lossy(&units)
}

/// `Math.round`: half rounds toward +infinity.
fn js_round(n: f64) -> f64 {
    (n + 0.5).floor()
}

/// `Math.max(0, Math.round(n))` for finite values.
fn ms_clamped(n: f64) -> u64 {
    let rounded = js_round(n);
    if rounded < 0.0 { 0 } else { rounded as u64 }
}

fn normalize_status(value: Option<&str>) -> String {
    match value {
        Some("running" | "success" | "error" | "idle") => value.unwrap().to_string(),
        _ => "idle".to_string(),
    }
}

/// Milliseconds since the Unix epoch (JS `Date.now()`).
pub(crate) fn system_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            if leap { 29 } else { 28 }
        }
        _ => 0,
    }
}

// ============== schedule piece normalizers ==============

/// `^([01]\d|2[0-3]):([0-5]\d)$`
fn normalize_time_value(value: &Value) -> Option<String> {
    let time = as_non_empty_string(value)?;
    let bytes = time.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' {
        return None;
    }
    let h1 = bytes[0];
    let h2 = bytes[1];
    let m1 = bytes[3];
    let m2 = bytes[4];
    if !h1.is_ascii_digit() || !h2.is_ascii_digit() || !m1.is_ascii_digit() || !m2.is_ascii_digit()
    {
        return None;
    }
    let hours = (h2 - b'0') as u32 + (h1 - b'0') as u32 * 10;
    let minutes = (m2 - b'0') as u32 + (m1 - b'0') as u32 * 10;
    (hours <= 23 && minutes <= 59).then_some(time)
}

/// `^\d{4}-\d{2}-\d{2}$` plus a real UTC calendar date.
fn normalize_date_value(value: &Value) -> Option<String> {
    let date = as_non_empty_string(value)?;
    let bytes = date.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    if !bytes
        .iter()
        .enumerate()
        .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
    {
        return None;
    }
    let year = date[0..4].parse::<i64>().ok()?;
    let month = date[5..7].parse::<u32>().ok()?;
    let day = date[8..10].parse::<u32>().ok()?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    Some(date)
}

/// Integers 0..=6, deduplicated and sorted; an empty list is invalid.
fn normalize_weekdays(value: &Value) -> Option<Vec<u8>> {
    let entries = value.as_array()?;
    let mut unique = HashSet::new();
    for entry in entries {
        let weekday = entry.as_i64()?;
        if !(0..=6).contains(&weekday) {
            return None;
        }
        unique.insert(weekday as u8);
    }
    if unique.is_empty() {
        return None;
    }
    let mut days: Vec<u8> = unique.into_iter().collect();
    days.sort_unstable();
    Some(days)
}

fn existing_schedule_times(schedule: Option<&Schedule>) -> Option<&Vec<String>> {
    match schedule {
        Some(Schedule::Daily { times, .. }) | Some(Schedule::Weekly { times, .. }) => Some(times),
        _ => None,
    }
}

/// Collect `times[]` plus the legacy single `time`, falling back to the
/// existing schedule's times when the incoming value carries none.
fn resolve_schedule_times(
    value: &Value,
    existing_schedule: Option<&Schedule>,
) -> Result<Option<Vec<String>>, String> {
    let mut times: Vec<String> = Vec::new();

    if let Some(items) = value.get("times").and_then(Value::as_array) {
        for item in items {
            let normalized = normalize_time_value(item)
                .ok_or_else(|| "schedule.times must contain HH:mm values".to_string())?;
            times.push(normalized);
        }
    }

    if let Some(legacy) = normalize_time_value(value.get("time").unwrap_or(&Value::Null)) {
        times.push(legacy);
    }

    if times.is_empty()
        && let Some(existing) = existing_schedule_times(existing_schedule)
    {
        for item in existing {
            if let Some(normalized) = normalize_time_value(&Value::String(item.clone())) {
                times.push(normalized);
            }
        }
    }

    let mut unique = times;
    unique.sort();
    unique.dedup();
    if unique.is_empty() {
        return Ok(None);
    }
    Ok(Some(unique))
}

// ============== timezone + cron validation ==============

fn zoneinfo_roots() -> Vec<std::path::PathBuf> {
    let mut roots = Vec::new();
    if let Ok(tzdir) = std::env::var("TZDIR")
        && !tzdir.trim().is_empty()
    {
        roots.push(tzdir.into());
    }
    roots.push("/usr/share/zoneinfo".into());
    roots.push("/etc/zoneinfo".into());
    roots.push("/usr/share/lib/zoneinfo".into());
    roots.push("/var/db/timezone/zoneinfo".into());
    roots
}

/// `IANAZone.isValidZone`: accept real zone database entries. Where no
/// zoneinfo database is present (non-unix), syntactic validity is accepted.
pub(crate) fn is_valid_iana_zone(name: &str) -> bool {
    if name.is_empty() || name.len() > 100 {
        return false;
    }
    if name.starts_with('/') || name.ends_with('/') || name.contains("//") || name.contains("..") {
        return false;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '.' | '/'))
    {
        return false;
    }
    if name == "UTC" {
        return true;
    }
    let roots = zoneinfo_roots();
    if !roots.iter().any(|root| root.is_dir()) {
        return true;
    }
    roots.iter().any(|root| root.join(name).is_file())
}

/// `DateTime.local().zoneName` — local zone from TZ or /etc/localtime, else UTC.
pub(crate) fn resolve_default_timezone() -> String {
    if let Ok(tz) = std::env::var("TZ") {
        let tz = tz.trim();
        if !tz.is_empty() && is_valid_iana_zone(tz) {
            return tz.to_string();
        }
    }
    if let Ok(target) = std::fs::read_link("/etc/localtime") {
        let text = target.to_string_lossy();
        if let Some(index) = text.rfind("zoneinfo/") {
            let name = &text[index + "zoneinfo/".len()..];
            if is_valid_iana_zone(name) {
                return name.to_string();
            }
        }
    }
    "UTC".to_string()
}

/// `normalizeTimezone`: None means invalid (caller raises); empty falls back.
fn normalize_timezone(value: &Value, fallback: &str) -> Option<String> {
    match as_non_empty_string(value) {
        None => Some(fallback.to_string()),
        Some(zone) => is_valid_iana_zone(&zone).then_some(zone),
    }
}

/// `validateCronExpression`: delegated to the shared cron-parser port
/// (`scheduled_tasks::cron`, cron-parser 4.9 semantics), same
/// expression/timezone/currentDate inputs the JS passes to cron-parser.
pub(crate) fn validate_cron_expression(expression: &str, timezone: &str) -> bool {
    crate::scheduled_tasks::cron::validate(expression, timezone, system_now_ms() as i64)
}

// ============== schedule / execution / state normalizers ==============

/// `normalizeSchedule` — exact JS error strings.
pub(crate) fn normalize_schedule(
    value: &Value,
    existing: Option<&Schedule>,
) -> Result<Schedule, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "schedule is required".to_string())?;

    let kind = as_non_empty_string(obj.get("kind").unwrap_or(&Value::Null))
        .ok_or_else(|| "schedule.kind must be daily, weekly, once, or cron".to_string())?;
    if !matches!(kind.as_str(), "daily" | "weekly" | "once" | "cron") {
        return Err("schedule.kind must be daily, weekly, once, or cron".to_string());
    }

    let fallback_timezone = existing
        .map(|s| s.timezone().to_string())
        .unwrap_or_else(resolve_default_timezone);
    let timezone = normalize_timezone(
        obj.get("timezone").unwrap_or(&Value::Null),
        &fallback_timezone,
    )
    .ok_or_else(|| "schedule.timezone must be a valid IANA timezone".to_string())?;

    match kind.as_str() {
        "daily" => {
            let times = resolve_schedule_times(value, existing)?.ok_or_else(|| {
                "schedule.times must include at least one HH:mm value for daily schedule"
                    .to_string()
            })?;
            Ok(Schedule::Daily { times, timezone })
        }
        "weekly" => {
            let times = resolve_schedule_times(value, existing)?.ok_or_else(|| {
                "schedule.times must include at least one HH:mm value for weekly schedule"
                    .to_string()
            })?;
            let weekdays = normalize_weekdays(obj.get("weekdays").unwrap_or(&Value::Null))
                .ok_or_else(|| {
                    "schedule.weekdays must include values from 0 to 6 for weekly schedule"
                        .to_string()
                })?;
            Ok(Schedule::Weekly {
                times,
                weekdays,
                timezone,
            })
        }
        "once" => {
            let date = normalize_date_value(obj.get("date").unwrap_or(&Value::Null))
                .ok_or_else(|| "schedule.date must be YYYY-MM-DD for once schedule".to_string())?;
            let time = normalize_time_value(obj.get("time").unwrap_or(&Value::Null))
                .ok_or_else(|| "schedule.time must be HH:mm for once schedule".to_string())?;
            Ok(Schedule::Once {
                date,
                time,
                timezone,
            })
        }
        _ => {
            let cron_raw =
                as_non_empty_string(obj.get("cron").unwrap_or(&Value::Null)).unwrap_or_default();
            let cron = clamp_length_utf16(&cron_raw, MAX_CRON_LENGTH);
            if cron.is_empty() {
                return Err("schedule.cron is required for cron schedule".to_string());
            }
            if !validate_cron_expression(&cron, &timezone) {
                return Err("schedule.cron is invalid".to_string());
            }
            Ok(Schedule::Cron { cron, timezone })
        }
    }
}

/// `normalizeExecution` — a task pins provider+model or follows the engine's
/// default model role; UI-only toggles ride along when truthy.
pub(crate) fn normalize_execution(value: &Value) -> Result<Execution, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "execution is required".to_string())?;

    let prompt = clamp_length_utf16(
        &as_non_empty_string(obj.get("prompt").unwrap_or(&Value::Null)).unwrap_or_default(),
        MAX_TASK_PROMPT_LENGTH,
    );
    let provider_id = as_non_empty_string(obj.get("providerID").unwrap_or(&Value::Null));
    let model_id = as_non_empty_string(obj.get("modelID").unwrap_or(&Value::Null));
    let model_role = if obj.get("modelRole").and_then(Value::as_str) == Some("default") {
        Some("default".to_string())
    } else {
        None
    };
    let variant = as_non_empty_string(obj.get("variant").unwrap_or(&Value::Null));
    let agent = as_non_empty_string(obj.get("agent").unwrap_or(&Value::Null));
    let goal_enabled = obj.get("goalEnabled") == Some(&Value::Bool(true));
    let permission_auto_accept = obj.get("permissionAutoAccept") == Some(&Value::Bool(true));
    let goal_token_budget = obj
        .get("goalTokenBudget")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| js_round(n).max(0.0) as u64);

    if prompt.is_empty() {
        return Err("execution.prompt is required".to_string());
    }
    if provider_id.is_none() && model_role.is_none() {
        return Err("execution.providerID is required".to_string());
    }
    if model_id.is_none() && model_role.is_none() {
        return Err("execution.modelID is required".to_string());
    }

    Ok(Execution {
        prompt,
        provider_id,
        model_id,
        model_role,
        variant,
        agent,
        goal_enabled: goal_enabled.then_some(true),
        goal_token_budget: goal_token_budget
            .filter(|_| goal_enabled)
            .filter(|budget| *budget > 0),
        permission_auto_accept: permission_auto_accept.then_some(true),
    })
}

/// `normalizeState` — the source is the incoming value when it is an object,
/// otherwise the fallback state; defaults come from `default_now` (JS
/// `Date.now()`).
pub(crate) fn normalize_state(
    value: Option<&Value>,
    fallback: Option<&TaskState>,
    default_now: u64,
) -> TaskState {
    let fallback_value;
    let source: Option<serde_json::Map<String, Value>> =
        if let Some(map) = value.and_then(Value::as_object) {
            Some(map.clone())
        } else if let Some(fallback) = fallback {
            fallback_value = serialize_task_state(fallback);
            fallback_value.as_object().cloned()
        } else {
            None
        };
    let get = |key: &str| source.as_ref().and_then(|map| map.get(key));
    let number = |key: &str| {
        get(key)
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite())
            .map(ms_clamped)
    };
    TaskState {
        created_at: number("createdAt").unwrap_or(default_now),
        updated_at: number("updatedAt").unwrap_or(default_now),
        last_status: normalize_status(get("lastStatus").and_then(|v| v.as_str())),
        last_run_at: number("lastRunAt"),
        last_duration_ms: number("lastDurationMs"),
        next_run_at: number("nextRunAt"),
        last_scheduled_for: number("lastScheduledFor"),
        last_session_id: get("lastSessionId")
            .and_then(|v| v.as_str())
            .and_then(non_empty_str),
        last_error: get("lastError")
            .and_then(|v| v.as_str())
            .and_then(non_empty_str)
            .map(|s| clamp_length_utf16(&s, MAX_LAST_ERROR_LENGTH)),
    }
}

/// Options for [`normalize_task_for_storage`] (JS `normalizeTaskForStorage`).
pub struct NormalizeOptions<'a> {
    pub now: u64,
    /// Real-clock default used inside `normalizeState` (JS `Date.now()`).
    pub default_now: u64,
    pub create_id: &'a (dyn Fn() -> String + Send + Sync),
    pub existing_task: Option<&'a ScheduledTask>,
    pub allow_create: bool,
    pub refresh_updated_at: bool,
}

/// `normalizeTaskForStorage` — validates and canonicalizes one task object.
pub fn normalize_task_for_storage(
    value: &Value,
    options: &NormalizeOptions<'_>,
) -> Result<ScheduledTask, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "task is required".to_string())?;

    let incoming_id = as_non_empty_string(obj.get("id").unwrap_or(&Value::Null));
    let existing = options.existing_task;
    if let (Some(existing_task), Some(incoming)) = (existing, incoming_id.as_deref())
        && incoming != existing_task.id
    {
        return Err("task.id is immutable".to_string());
    }
    if existing.is_none() && incoming_id.is_some() && !options.allow_create {
        return Err("task.id does not exist".to_string());
    }
    let id = existing
        .map(|task| task.id.clone())
        .or(incoming_id)
        .unwrap_or_else(|| (options.create_id)());

    let name = clamp_length_utf16(
        &as_non_empty_string(obj.get("name").unwrap_or(&Value::Null)).unwrap_or_default(),
        MAX_TASK_NAME_LENGTH,
    );
    if name.is_empty() {
        return Err("task.name is required".to_string());
    }

    let enabled = obj
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| existing.map(|task| task.enabled).unwrap_or(true));

    let schedule = normalize_schedule(
        obj.get("schedule").unwrap_or(&Value::Null),
        existing.map(|task| &task.schedule),
    )?;
    let execution = normalize_execution(obj.get("execution").unwrap_or(&Value::Null))?;

    let loop_file = as_non_empty_string(obj.get("loopFile").unwrap_or(&Value::Null))
        .or_else(|| existing.and_then(|task| task.loop_file.clone()));

    let now_ms = options.now;
    let base_state = normalize_state(
        obj.get("state"),
        existing.map(|task| &task.state),
        options.default_now,
    );
    let state = TaskState {
        created_at: existing
            .map(|task| task.state.created_at)
            .unwrap_or(base_state.created_at),
        updated_at: if options.refresh_updated_at {
            now_ms
        } else {
            base_state.updated_at
        },
        ..base_state
    };

    Ok(ScheduledTask {
        id,
        name,
        enabled,
        schedule,
        execution,
        state,
        loop_file,
    })
}

// ============== typed shapes (JS camelCase wire format) ==============

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Schedule {
    Daily {
        times: Vec<String>,
        timezone: String,
    },
    Weekly {
        times: Vec<String>,
        weekdays: Vec<u8>,
        timezone: String,
    },
    Once {
        date: String,
        time: String,
        timezone: String,
    },
    Cron {
        cron: String,
        timezone: String,
    },
}

impl Schedule {
    pub fn timezone(&self) -> &str {
        match self {
            Schedule::Daily { timezone, .. }
            | Schedule::Weekly { timezone, .. }
            | Schedule::Once { timezone, .. }
            | Schedule::Cron { timezone, .. } => timezone,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Schedule::Daily { .. } => "daily",
            Schedule::Weekly { .. } => "weekly",
            Schedule::Once { .. } => "once",
            Schedule::Cron { .. } => "cron",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Execution {
    pub prompt: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "providerID"
    )]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "modelID")]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_token_budget: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_auto_accept: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskState {
    pub created_at: u64,
    pub updated_at: u64,
    pub last_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scheduled_for: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledTask {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub schedule: Schedule,
    pub execution: Execution,
    pub state: TaskState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_file: Option<String>,
}

/// One discovered `.agents/loops` entry handed to `reconcileLoopTasks`:
/// `definition` is `None` for a file that exists but fails to parse.
#[derive(Debug, Clone)]
pub struct LoopEntry {
    pub scope: String,
    pub file_path: String,
    pub definition: Option<Value>,
}

pub(crate) fn serialize_task_state(state: &TaskState) -> Value {
    serde_json::to_value(state).unwrap_or(Value::Null)
}

pub(crate) fn serialize_task(task: &ScheduledTask) -> Value {
    serde_json::to_value(task).unwrap_or(Value::Null)
}
