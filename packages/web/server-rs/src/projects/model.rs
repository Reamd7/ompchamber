//! Typed task model + dynamic-JSON normalizers for the per-project config.
//!
//! Port of the helper layer of `server/lib/projects/project-config.js`
//! (everything above `createProjectConfigRuntime`). The JS normalizes loose
//! objects; here the same rules run on `serde_json::Value` inputs and produce
//! the typed [`ScheduledTask`], whose serialized shape matches the JS objects
//! field-for-field (camelCase names, optional fields omitted, `providerID` /
//! `modelID` spelling preserved).
//!
//! 中文说明：本模块是 JS project-config.js 帮助层的移植，职责是把宽松的
//! JSON 任务对象规整成类型化的 ScheduledTask——校验 schedule / execution /
//! state 三块、复刻 JS 的 trim / clamp / 取整等语义，错误文案与 JS 逐字
//! 相同；序列化结果与 JS 对象逐字段一致（camelCase、可选字段省略输出、
//! providerID / modelID 拼写保持不变）。

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 项目配置文件当前的 schema 版本号，随配置一起落盘，读取时校验。
pub const PROJECT_CONFIG_VERSION: u64 = 1;
/// 任务名长度上限（UTF-16 码元，对齐 JS String.length 语义）。
pub const MAX_TASK_NAME_LENGTH: usize = 80;
/// 任务 prompt 长度上限（UTF-16 码元），超长截断而非报错。
pub const MAX_TASK_PROMPT_LENGTH: usize = 20_000;
/// cron 表达式长度上限（UTF-16 码元），截断后再做语法校验。
pub const MAX_CRON_LENGTH: usize = 200;
/// state.lastError 长度上限（UTF-16 码元），避免长错误信息撑爆配置文件。
pub const MAX_LAST_ERROR_LENGTH: usize = 2_000;

// ============== small JS-semantic helpers ==============

/// `asNonEmptyString`: strings trim to a non-empty value.
/// 中文：仅当值是 trim 后仍非空的字符串时返回 Some（返回修剪结果）；
/// 其余类型与空串一律 None，等价 JS 的 asNonEmptyString。
pub(crate) fn as_non_empty_string(value: &Value) -> Option<String> {
    value.as_str().and_then(|s| {
        let trimmed = s.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

/// 与 as_non_empty_string 同一套规则，但输入已是 &str：trim 后非空返回
/// Some，否则 None；供 lastSessionId / lastError 等字符串字段复用。
fn non_empty_str(s: &str) -> Option<String> {
    let trimmed = s.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// `clampLength`: truncate by UTF-16 code units (JS `String.length`/`slice`).
/// 中文：按 UTF-16 码元截断到 max_length（对齐 JS 的 slice 语义）；
/// 未超限时原样返回。截断可能落在代理对中间，回解码按损失容忍处理。
pub(crate) fn clamp_length_utf16(value: &str, max_length: usize) -> String {
    if value.encode_utf16().count() <= max_length {
        return value.to_string();
    }
    let units: Vec<u16> = value.encode_utf16().take(max_length).collect();
    String::from_utf16_lossy(&units)
}

/// `Math.round`: half rounds toward +infinity.
/// 复刻 JS Math.round：恰为 .5 时向 +∞ 取整（区别于 Rust round 的远离零）。
fn js_round(n: f64) -> f64 {
    (n + 0.5).floor()
}

/// `Math.max(0, Math.round(n))` for finite values.
/// 毫秒数值规整：先走 js_round 再夹到非负，作为各时间戳字段的兜底。
fn ms_clamped(n: f64) -> u64 {
    let rounded = js_round(n);
    if rounded < 0.0 { 0 } else { rounded as u64 }
}

/// 任务状态白名单：仅接受 running / success / error / idle；
/// 缺省或非法值一律回退为 "idle"，不产生错误。
fn normalize_status(value: Option<&str>) -> String {
    match value {
        Some("running" | "success" | "error" | "idle") => value.unwrap().to_string(),
        _ => "idle".to_string(),
    }
}

/// Milliseconds since the Unix epoch (JS `Date.now()`).
/// 当前系统时间（Unix epoch 毫秒），对应 JS Date.now()；时钟早于 epoch 时返回 0。
pub(crate) fn system_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// 指定年月的天数：大月 31、小月 30，二月按格里高利闰年规则取 28/29；
/// 非法月份返回 0，供日期校验拒绝「2 月 30 日」这类值。
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
/// 中文：HH:mm 校验——恰好 5 字节、第三位是冒号、其余为 ASCII 数字，
/// 且小时 ≤ 23、分钟 ≤ 59；通过则原样返回该字符串。
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
/// 中文：YYYY-MM-DD 校验——先查形状（4-2-2 位数字、连字符分隔），
/// 再用 days_in_month 确认是真实存在的日历日期。
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
/// 中文：星期列表规整——只接受 0..=6 的整数，去重后升序返回；
/// 空列表视为无效（每周任务至少要选一天）。
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

/// 取既有 schedule 携带的 times（仅 daily / weekly 有），作为入参未带
/// 时间时的回退来源；once / cron 形态返回 None。
fn existing_schedule_times(schedule: Option<&Schedule>) -> Option<&Vec<String>> {
    match schedule {
        Some(Schedule::Daily { times, .. }) | Some(Schedule::Weekly { times, .. }) => Some(times),
        _ => None,
    }
}

/// Collect `times[]` plus the legacy single `time`, falling back to the
/// existing schedule's times when the incoming value carries none.
/// 中文：合并 times[] 与旧版单个 time 字段；两者皆空时回退到既有
/// schedule 的时间。结果排序去重，空集返回 Ok(None) 由调用方报错。
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

/// zoneinfo 数据库的候选根目录：优先 TZDIR 环境变量，再叠加常见 Unix
/// 路径；任一目录存在即启用真实时区库校验。
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
/// 中文：存在 zoneinfo 数据库时要求名字对应库内真实文件（"UTC" 恒真），
/// 并先做长度 / 字符集 / 路径穿越检查；找不到任何库的环境退化为仅
/// 语法校验。
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
/// 中文：解析默认时区——依次尝试 TZ 环境变量、/etc/localtime 符号链接
/// 中的 zoneinfo/ 后缀，全部失败则兜底 "UTC"。
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
/// 中文：时区规整——空值或缺省回退到 fallback；非空值须通过 IANA 校验，
/// 非法返回 None 由调用方生成错误文案。
fn normalize_timezone(value: &Value, fallback: &str) -> Option<String> {
    match as_non_empty_string(value) {
        None => Some(fallback.to_string()),
        Some(zone) => is_valid_iana_zone(&zone).then_some(zone),
    }
}

/// `validateCronExpression`: delegated to the shared cron-parser port
/// (`scheduled_tasks::cron`, cron-parser 4.9 semantics), same
/// expression/timezone/currentDate inputs the JS passes to cron-parser.
/// 中文：cron 校验委托共享的 cron 移植（cron-parser 4.9 语义），
/// 以当前系统时间作为求值基准。
pub(crate) fn validate_cron_expression(expression: &str, timezone: &str) -> bool {
    crate::scheduled_tasks::cron::validate(expression, timezone, system_now_ms() as i64)
}

// ============== schedule / execution / state normalizers ==============

/// `normalizeSchedule` — exact JS error strings.
/// 中文：schedule 规整入口——按 kind 分派到 daily / weekly / once / cron
/// 四种形态，各字段复用上面的窄校验器；时区缺省回退到既有值或系统
/// 默认时区，错误字符串与 JS 版逐字一致。
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
/// 中文：execution 规整——prompt 必填、超长截断；provider+model 与
/// modelRole("default") 二选一，后者表示跟随引擎默认模型；布尔开关仅在
/// 为 true 时落盘，goalTokenBudget 还要求 goalEnabled 且为正的有限数。
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
/// 中文：state 规整——入参是对象则以其为源，否则回退到 fallback 状态；
/// 数值字段走毫秒夹取并要求有限值，时间戳缺省取 default_now，
/// lastError 截断到上限，lastStatus 走白名单。
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
/// 中文：normalize_task_for_storage 的选项集——控制时间基准、新 id 的
/// 生成器、是否允许凭空创建、是否强制刷新 updatedAt。
pub struct NormalizeOptions<'a> {
    /// 「当前时间」毫秒数，用于刷新 updatedAt。
    pub now: u64,
    /// Real-clock default used inside `normalizeState` (JS `Date.now()`).
    /// 真实时钟默认值，state 各时间戳缺省时使用（JS Date.now()）。
    pub default_now: u64,
    /// 新任务 id 的生成器闭包，仅在需要分配新 id 时调用。
    pub create_id: &'a (dyn Fn() -> String + Send + Sync),
    /// 同 id 的既有任务：提供各字段回退值并锁定不可变字段；None 表示新建。
    pub existing_task: Option<&'a ScheduledTask>,
    /// 为 false 时拒绝携带未知 id 的新建请求（报 "task.id does not exist"）。
    pub allow_create: bool,
    /// 为 true 时把 updatedAt 强制刷新为 options.now。
    pub refresh_updated_at: bool,
}

/// `normalizeTaskForStorage` — validates and canonicalizes one task object.
/// 中文：单个任务对象的校验与规范化入口——id 创建后不可变、未获允许时
/// 不能凭空指定、name 必填；schedule / execution / state 分别走对应规整
/// 器，createdAt 只认既有值，返回可直接落盘的 ScheduledTask。
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

/// 触发计划。serde 以 kind 字段做内部标签、变体名小写序列化，
/// 四种形态与 JS 版的 wire 格式一一对应。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Schedule {
    /// 每天在指定时刻触发。
    Daily {
        /// HH:mm 触发时刻列表（已排序去重）。
        times: Vec<String>,
        /// IANA 时区名。
        timezone: String,
    },
    /// 每周在指定星期与时刻触发。
    Weekly {
        /// HH:mm 触发时刻列表（已排序去重）。
        times: Vec<String>,
        /// 星期几（0=周日 .. 6=周六），已去重升序。
        weekdays: Vec<u8>,
        /// IANA 时区名。
        timezone: String,
    },
    /// 在指定日期的指定时刻触发一次。
    Once {
        /// 触发日期（YYYY-MM-DD）。
        date: String,
        /// 触发时刻（HH:mm）。
        time: String,
        /// IANA 时区名。
        timezone: String,
    },
    /// 按 cron 表达式触发。
    Cron {
        /// cron 表达式（cron-parser 4.9 语义）。
        cron: String,
        /// IANA 时区名。
        timezone: String,
    },
}

/// Schedule 的公共只读访问器。
impl Schedule {
    /// 返回该计划的 IANA 时区名（四种形态都携带）。
    pub fn timezone(&self) -> &str {
        match self {
            Schedule::Daily { timezone, .. }
            | Schedule::Weekly { timezone, .. }
            | Schedule::Once { timezone, .. }
            | Schedule::Cron { timezone, .. } => timezone,
        }
    }

    /// 返回 wire 格式的 kind 标签（daily / weekly / once / cron）。
    pub fn kind(&self) -> &'static str {
        match self {
            Schedule::Daily { .. } => "daily",
            Schedule::Weekly { .. } => "weekly",
            Schedule::Once { .. } => "once",
            Schedule::Cron { .. } => "cron",
        }
    }
}

/// 一次执行的参数（camelCase wire 格式；providerID / modelID 保留 JS 的
/// 大写 ID 拼写，可选字段为 None 时不参与序列化）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Execution {
    /// 发给引擎的 prompt，必填，超长已截断。
    pub prompt: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "providerID"
    )]
    /// 指定的 provider id；与 modelRole 二选一。
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "modelID")]
    /// 指定的 model id；与 modelRole 二选一。
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// 值恒为 "default"：跟随引擎默认模型角色，此时可省略 provider/model。
    pub model_role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// UI 选择的 variant（仅透传）。
    pub variant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// UI 选择的 agent（仅透传）。
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// 仅为 true 时序列化：goal 模式开关。
    pub goal_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// goal 模式的 token 预算，仅在 goalEnabled 且为正时保留。
    pub goal_token_budget: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// 仅为 true 时序列化：权限自动接受开关。
    pub permission_auto_accept: Option<bool>,
}

/// 任务的运行状态快照（camelCase wire 格式；可选字段为 None 时不序列化）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskState {
    /// 创建时间（epoch 毫秒）。
    pub created_at: u64,
    /// 最近一次写盘时间（epoch 毫秒）。
    pub updated_at: u64,
    /// 最近一次运行结果：running / success / error / idle。
    pub last_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// 最近一次运行的开始时间（epoch 毫秒）。
    pub last_run_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// 最近一次运行耗时（毫秒）。
    pub last_duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// 下次计划触发时间（epoch 毫秒）。
    pub next_run_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// 上次调度器锁定的触发点（epoch 毫秒），用于防重复触发。
    pub last_scheduled_for: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// 最近一次运行关联的引擎 session id。
    pub last_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// 最近一次失败的错误摘要（已按上限截断）。
    pub last_error: Option<String>,
}

/// 一条计划任务的完整存储形态（camelCase wire 格式），由
/// normalize_task_for_storage 产出后直接写入项目配置文件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledTask {
    /// 稳定 id：创建后不可变；loop 任务由作用域与名称派生。
    pub id: String,
    /// 展示名（必填，已按上限截断）。
    pub name: String,
    /// 是否参与调度；缺省 true。
    pub enabled: bool,
    /// 触发计划。
    pub schedule: Schedule,
    /// 执行参数。
    pub execution: Execution,
    /// 运行状态快照。
    pub state: TaskState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// 来源 .agents/loops 文件路径；手工创建的任务为 None。
    pub loop_file: Option<String>,
}

/// One discovered `.agents/loops` entry handed to `reconcileLoopTasks`:
/// `definition` is `None` for a file that exists but fails to parse.
/// 中文：loop 任务对账（reconcileLoopTasks）时的一条发现记录——
/// definition 为 None 表示文件存在但解析失败，调度器仍会保住对应任务
/// 直到文件被修复。
#[derive(Debug, Clone)]
pub struct LoopEntry {
    /// loop 的作用域字符串（"project" / "user"），参与派生 loop 任务 id
    /// （loop:{scope}:{name}）。
    pub scope: String,
    /// loop 定义文件的绝对路径。
    pub file_path: String,
    /// 解析后的定义 JSON；文件存在但解析失败时为 None。
    pub definition: Option<Value>,
}

/// 把状态对象序列化为 JSON 值；serde 失败时退化为 Null（字段均为基础
/// 类型，实际不会发生）。
pub(crate) fn serialize_task_state(state: &TaskState) -> Value {
    serde_json::to_value(state).unwrap_or(Value::Null)
}

/// 把任务对象序列化为 JSON 值；serde 失败时退化为 Null（字段均为基础
/// 类型，实际不会发生）。
pub(crate) fn serialize_task(task: &ScheduledTask) -> Value {
    serde_json::to_value(task).unwrap_or(Value::Null)
}
