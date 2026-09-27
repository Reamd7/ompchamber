//! Port of `server/lib/project-context/runtime.js`.
//!
//! Project context storage: notes, todos, and plan files under
//! `<projects-dir>/<projectId>/`. The server is the sole writer of
//! `<projectId>/context.json`; the sibling `<projectId>.json` stays
//! client-owned except for the legacy keys migrated out of it by
//! [`ProjectContextRuntime::read_context`]. Plan bodies live as markdown at
//! `<projectId>/plans/<file>.md` and are referenced by base name only.
//!
//! Behavioral notes carried over from the JS:
//! - reads are strict `JSON.parse` (unlike the projects module's JSONC
//!   tolerance) — a malformed `context.json` is an error, never empty data;
//! - writes are pretty-printed (2-space) temp-file + atomic rename;
//! - mutators serialize through an in-process per-project chain
//!   (`withWriteLock`); the legacy migration deliberately runs unlocked
//!   because its two writes are atomic renames of identical content.
//!
//! 中文说明：项目上下文（notes / todos / plan）存储运行时。清单持久化在
//! `<projects-dir>/<projectId>/context.json`，plan 正文以 markdown 存于
//! `plans/` 子目录；读取为严格 JSON 解析，写入走临时文件 + 原子 rename，
//! 变更操作经按 projectId 的进程内写锁链串行化（详见上方英文说明）。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Map, Number, Value};
use tokio::sync::Mutex as AsyncMutex;

use crate::context::RouterContext;
use crate::error::{AppError, AppResult};

/// 当前 context.json 的 schema 版本；读取输出始终按此版本重建。
pub const PROJECT_CONTEXT_VERSION: u8 = 2;
/// 单条 note 正文的长度上限（UTF-16 码元），清洗时截断。
const PROJECT_NOTE_BODY_MAX_LENGTH: usize = 3000;
/// 单个项目允许保留的 note 条数上限，创建超出即报错。
const PROJECT_NOTE_MAX_ITEMS: usize = 200;
/// 单条 todo 文本的长度上限（UTF-16 码元）。
const PROJECT_TODO_TEXT_MAX_LENGTH: usize = 120;
/// plan 标题的长度上限（UTF-16 码元）。
const PROJECT_PLAN_TITLE_MAX_LENGTH: usize = 160;
/// plan 正文（raw markdown）的长度上限（UTF-16 码元）。
const PROJECT_PLAN_BODY_MAX_LENGTH: usize = 200_000;
/// 清洗后保留的 todo 条数上限。
const PROJECT_TODO_MAX_ITEMS: usize = 500;
/// 清洗后保留的 plan 链接条数上限。
const PROJECT_PLAN_MAX_ITEMS: usize = 500;

// ---------------------------------------------------------------------------
// Wire model
// ---------------------------------------------------------------------------

/// note 的采集来源定位（selection/agent 来源时记录会话与消息）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NoteOrigin {
    /// 来源会话 id；缺失时整个 origin 被丢弃。
    #[serde(rename = "sessionId")]
    pub session_id: String,
    /// 可选的来源消息 id；为 None 时序列化省略该字段。
    #[serde(rename = "messageId", skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

/// 一条项目便签（note），存储于 context.json 的 notes 数组。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Note {
    /// 便签 id（创建时由 id 工厂生成）。
    pub id: String,
    /// 便签正文（trim 后非空，长度受上限约束）。
    pub body: String,
    /// Milliseconds since epoch; stored as a raw JSON number so hand-edited
    /// fractional timestamps round-trip exactly like the JS does.
    /// 创建时间（epoch 毫秒）。
    #[serde(rename = "createdAt")]
    pub created_at: Number,
    /// 最近更新时间（epoch 毫秒）；仅修改 body 时刷新。
    #[serde(rename = "updatedAt")]
    pub updated_at: Number,
    /// 来源标记：manual / selection / agent 之一，非法值清洗为 manual。
    pub source: String,
    /// 是否置顶。
    pub pinned: bool,
    /// 可选的采集来源；为 None 时序列化省略该字段。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<NoteOrigin>,
}

/// 一条待办事项（todo）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Todo {
    /// 待办 id。
    pub id: String,
    /// 待办文本（非空，长度受上限约束）。
    pub text: String,
    /// 是否已完成。
    pub completed: bool,
    /// 创建时间（epoch 毫秒）。
    #[serde(rename = "createdAt")]
    pub created_at: Number,
}

/// context.json 中指向 plan markdown 文件的链接（manifest 条目）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanLink {
    /// plan id。
    pub id: String,
    /// plans/ 目录下的 markdown 文件名（仅 base name，须匹配安全模式）。
    pub file: String,
    /// 标题：从 markdown 首个标题派生，并随内容更新重算。
    pub title: String,
    /// 创建时间（epoch 毫秒）。
    #[serde(rename = "createdAt")]
    pub created_at: Number,
    /// 是否置顶。
    pub pinned: bool,
}

/// 一个项目的完整上下文清单，即 context.json 的内存投影。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProjectContext {
    /// schema 版本，当前恒为 2。
    pub version: u8,
    /// 便签列表，最新的排在最前。
    pub notes: Vec<Note>,
    /// 待办列表，保持写入时的顺序。
    pub todos: Vec<Todo>,
    /// plan 链接列表，最新的排在最前。
    pub plans: Vec<PlanLink>,
}

/// `readPlan` result — the parsed projection of one markdown file.
/// readPlan 的结果：单个 markdown 文件解析出的投影。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanRead {
    /// plan id（来自清单链接）。
    pub id: String,
    /// markdown 文件名。
    pub file: String,
    /// 创建时间（epoch 毫秒）。
    #[serde(rename = "createdAt")]
    pub created_at: Number,
    /// 从 markdown 派生的标题。
    pub title: String,
    /// 标题之后的正文部分。
    pub body: String,
    /// 文件原始内容（未经解析改写）。
    pub raw: String,
}

/// createNote / updateNote 的返回：被写入的便签与整份最新上下文。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NoteMutation {
    /// 新建或更新后的便签。
    pub note: Note,
    /// 写入后的完整上下文。
    pub context: ProjectContext,
}

/// createPlan 的返回。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanCreateResult {
    /// 新建的 plan 链接。
    pub plan: PlanLink,
    /// 写入后的完整上下文。
    pub context: ProjectContext,
}

/// updatePlan 的返回：链接、上下文与重新解析出的内容。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanSaveResult {
    /// 更新后的 plan 链接（标题已重算）。
    pub plan: PlanLink,
    /// 写入后的完整上下文。
    pub context: ProjectContext,
    /// 从新 raw 重新派生的标题。
    pub title: String,
    /// 从新 raw 重新派生的正文。
    pub body: String,
    /// 已写入文件的（截断后）原始内容。
    pub raw: String,
}

/// deleteNote / deletePlan 的返回。
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteOutcome {
    /// 是否真的删除了目标（未知 id 时为 false）。
    pub deleted: bool,
    /// 操作后的完整上下文。
    pub context: ProjectContext,
}

// ---------------------------------------------------------------------------
// JS-string semantics helpers
// ---------------------------------------------------------------------------

/// 当前系统时间（epoch 毫秒）；时钟异常时回退为 0。
fn system_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// The exact `RegExp \s` class (no `\u{0085}`, includes `\u{feff}`).
/// 精确复刻 JS RegExp `\s` 字符类（含 BOM，不含 `\u{0085}`）。
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// JS `String.prototype.trim` (Unicode + BOM, per the `\s` class above).
/// 等价于 JS 的 String.prototype.trim：按上述 `\s` 字符类裁剪两端。
fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

/// 返回 `from` 起连续 JS 空白（`\s*`）之后的首个字节下标。
fn skip_js_whitespace(value: &str, from: usize) -> usize {
    let mut index = from;
    for c in value[from..].chars() {
        if is_js_whitespace(c) {
            index += c.len_utf8();
        } else {
            break;
        }
    }
    index
}

/// JS `value.slice(0, maxLength)` — counts UTF-16 code units, never splitting
/// a surrogate pair.
/// 按 UTF-16 码元计数截断，绝不切开代理对。
fn clamp_length(value: &str, max_length: usize) -> String {
    let mut units = 0usize;
    for (index, c) in value.char_indices() {
        let c_units = c.len_utf16();
        if units + c_units > max_length {
            return value[..index].to_string();
        }
        units += c_units;
    }
    value.to_string()
}

/// 取字符串值并按 JS trim 裁剪；非字符串或裁剪后为空则返回 None。
fn as_non_empty_string(value: &Value) -> Option<String> {
    let text = value.as_str()?;
    let trimmed = js_trim(text);
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 判断是否为合法的 note 来源枚举值。
fn is_note_source(value: &str) -> bool {
    matches!(value, "manual" | "selection" | "agent")
}

/// 清洗 origin：必须是对象且 sessionId 非空，否则整体丢弃（返回 None）。
fn sanitize_note_origin(value: &Value) -> Option<NoteOrigin> {
    let object = value.as_object()?;
    let session_id = as_non_empty_string(object.get("sessionId").unwrap_or(&Value::Null))?;
    let message_id = as_non_empty_string(object.get("messageId").unwrap_or(&Value::Null));
    Some(NoteOrigin {
        session_id,
        message_id,
    })
}

/// 取非负数字时间戳；值缺失、非数字或为负时回退到 fallback。
fn timestamp_or(value: Option<&Value>, fallback: Number) -> Number {
    match value.and_then(Value::as_number) {
        Some(number) if number.as_f64().is_some_and(|v| v >= 0.0) => number.clone(),
        _ => fallback,
    }
}

/// JS 意义上的行终止符（正则 `.` 不匹配它们）。
fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\u{2028}' | '\u{2029}')
}

// ---------------------------------------------------------------------------
// Sanitizers (ports of sanitizeNotes / sanitizeTodos / sanitizePlanLinks)
// ---------------------------------------------------------------------------

/// 移植 sanitizeNotes：兼容 v1 的单字符串便签格式；剔除无 id、空 body、
/// 重复 id 的条目，截断超长正文，按创建时间降序排列，并限制条数上限。
fn sanitize_notes(value: &Value, now: &Number) -> Vec<Note> {
    if let Some(text) = value.as_str() {
        // Version 1 stored notes as one string blob.
        let clamped = clamp_length(text, PROJECT_NOTE_BODY_MAX_LENGTH);
        let body = js_trim(&clamped).to_string();
        if body.is_empty() {
            return Vec::new();
        }
        return vec![Note {
            id: format!("note_legacy_{now}"),
            body,
            created_at: now.clone(),
            updated_at: now.clone(),
            source: "manual".to_string(),
            pinned: false,
            origin: None,
        }];
    }

    let Some(entries) = value.as_array() else {
        return Vec::new();
    };

    let mut result: Vec<Note> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for entry in entries {
        if result.len() >= PROJECT_NOTE_MAX_ITEMS {
            break;
        }
        let Some(object) = entry.as_object() else {
            continue;
        };
        let id = as_non_empty_string(object.get("id").unwrap_or(&Value::Null));
        let raw_body = object.get("body").and_then(Value::as_str).unwrap_or("");
        let body = js_trim(&clamp_length(raw_body, PROJECT_NOTE_BODY_MAX_LENGTH)).to_string();
        let Some(id) = id else { continue };
        if body.is_empty() || !seen.insert(id.clone()) {
            continue;
        }
        let created_at = timestamp_or(object.get("createdAt"), now.clone());
        let updated_at = timestamp_or(object.get("updatedAt"), created_at.clone());
        let source = object
            .get("source")
            .and_then(Value::as_str)
            .filter(|source| is_note_source(source))
            .unwrap_or("manual")
            .to_string();
        let pinned = object.get("pinned") == Some(&Value::Bool(true));
        let origin = sanitize_note_origin(object.get("origin").unwrap_or(&Value::Null));
        result.push(Note {
            id,
            body,
            created_at,
            updated_at,
            source,
            pinned,
            origin,
        });
    }

    result.sort_by(|a, b| {
        let av = a.created_at.as_f64().unwrap_or(0.0);
        let bv = b.created_at.as_f64().unwrap_or(0.0);
        bv.partial_cmp(&av).unwrap_or(std::cmp::Ordering::Equal)
    });
    result
}

/// 移植 sanitizeTodos：剔除无 id、空文本、重复 id 的条目并截断超长文本，
/// 保留传入顺序。
fn sanitize_todos(value: &Value, now: &Number) -> Vec<Todo> {
    let Some(entries) = value.as_array() else {
        return Vec::new();
    };
    let mut result: Vec<Todo> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for entry in entries {
        if result.len() >= PROJECT_TODO_MAX_ITEMS {
            break;
        }
        let Some(object) = entry.as_object() else {
            continue;
        };
        let id = as_non_empty_string(object.get("id").unwrap_or(&Value::Null));
        let text = clamp_length(
            &as_non_empty_string(object.get("text").unwrap_or(&Value::Null)).unwrap_or_default(),
            PROJECT_TODO_TEXT_MAX_LENGTH,
        );
        let Some(id) = id else { continue };
        if text.is_empty() || !seen.insert(id.clone()) {
            continue;
        }
        let created_at = timestamp_or(object.get("createdAt"), now.clone());
        let completed = object.get("completed") == Some(&Value::Bool(true));
        result.push(Todo {
            id,
            text,
            completed,
            created_at,
        });
    }
    result
}

/// 清洗 plan 标题：trim 后按上限截断；空值返回空串（回退文案由调用方决定）。
fn sanitize_plan_title(value: &Value) -> String {
    clamp_length(
        &as_non_empty_string(value).unwrap_or_default(),
        PROJECT_PLAN_TITLE_MAX_LENGTH,
    )
}

/// 校验 plan 文件名匹配 /^[a-zA-Z0-9._-]+\.md$/，防止路径逃逸。
fn is_plan_file_name(file: &str) -> bool {
    // /^[a-zA-Z0-9._-]+\.md$/
    if !file.ends_with(".md") || file.len() <= 3 {
        return false;
    }
    file[..file.len() - 3]
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// 移植 sanitizePlanLinks：剔除无 id、文件名非法、id 或文件重复的条目；
/// 空标题回退 "Plan"；按创建时间降序排列并限制条数。
fn sanitize_plan_links(value: &Value, now: &Number) -> Vec<PlanLink> {
    let Some(entries) = value.as_array() else {
        return Vec::new();
    };
    let mut result: Vec<PlanLink> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut seen_files: HashSet<String> = HashSet::new();
    for entry in entries {
        if result.len() >= PROJECT_PLAN_MAX_ITEMS {
            break;
        }
        let Some(object) = entry.as_object() else {
            continue;
        };
        let id = as_non_empty_string(object.get("id").unwrap_or(&Value::Null));
        let file = as_non_empty_string(object.get("file").unwrap_or(&Value::Null));
        let (Some(id), Some(file)) = (id, file) else {
            continue;
        };
        if !is_plan_file_name(&file) {
            continue;
        }
        if seen_ids.contains(&id) || seen_files.contains(&file) {
            continue;
        }
        seen_ids.insert(id.clone());
        seen_files.insert(file.clone());
        let title = {
            let sanitized = sanitize_plan_title(object.get("title").unwrap_or(&Value::Null));
            if sanitized.is_empty() {
                "Plan".to_string()
            } else {
                sanitized
            }
        };
        let created_at = timestamp_or(object.get("createdAt"), now.clone());
        let pinned = object.get("pinned") == Some(&Value::Bool(true));
        result.push(PlanLink {
            id,
            file,
            title,
            created_at,
            pinned,
        });
    }
    result.sort_by(|a, b| {
        let av = a.created_at.as_f64().unwrap_or(0.0);
        let bv = b.created_at.as_f64().unwrap_or(0.0);
        bv.partial_cmp(&av).unwrap_or(std::cmp::Ordering::Equal)
    });
    result
}

// ---------------------------------------------------------------------------
// Plan markdown (port of parsePlanMarkdown / formatPlanMarkdown / slugify)
// ---------------------------------------------------------------------------

/// parsePlanMarkdown 的输出：从 markdown 派生的标题与正文。
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedPlan {
    /// 标题：首个 `#` 标题或回退行；清洗后为空则回退 "Plan"。
    pub title: String,
    /// 正文：首个标题之后的内容；无标题时为 trim 后的全文。
    pub body: String,
}

/// 把 CRLF 与孤立 CR 统一为 LF；不含 `\r` 时原样返回以避免无谓拷贝。
fn normalize_newlines(raw: &str) -> String {
    if !raw.contains('\r') {
        return raw.to_string();
    }
    raw.replace("\r\n", "\n").replace('\r', "\n")
}

/// `\s*(?:\n+|$)` anchored at `at`, returning the match end.
/// 在 `at` 处匹配该模式并返回匹配结束位置；`\s*` 会回溯让位于 `\n+`。
fn match_ws_then_newlines_or_eos(value: &str, at: usize) -> Option<usize> {
    let mut end = at;
    let mut first_newline: Option<usize> = None;
    for c in value[at..].chars() {
        if !is_js_whitespace(c) {
            break;
        }
        end += c.len_utf8();
        if c == '\n' && first_newline.is_none() {
            first_newline = Some(end - 1);
        }
    }
    if end == value.len() {
        return Some(end);
    }
    // Backtrack `\s*` to just before the first '\n' so `\n+` can take over.
    let start = first_newline?;
    let mut newline_end = start;
    for c in value[start..].chars() {
        if c == '\n' {
            newline_end += 1;
        } else {
            break;
        }
    }
    Some(newline_end)
}

/// `(.+?)\s*(?:\n+|$)` with the capture starting at `start`.
/// 从 `start` 起做该捕获，返回 (整体匹配结束位置, 捕获文本)。
fn lazy_capture_from(value: &str, start: usize) -> Option<(usize, String)> {
    let mut end = start;
    for c in value[start..].chars() {
        if is_line_terminator(c) {
            break;
        }
        end += c.len_utf8();
        if let Some(match_end) = match_ws_then_newlines_or_eos(value, end) {
            return Some((match_end, value[start..end].to_string()));
        }
    }
    None
}

/// `/^\s*#\s+(.+?)\s*(?:\n+|$)/` — returns (whole-match end, title capture).
/// 匹配行首 plan 标题，返回 (整体匹配结束位置, 标题捕获)。
fn match_plan_heading(value: &str) -> Option<(usize, String)> {
    let after_leading_ws = skip_js_whitespace(value, 0);
    if !value[after_leading_ws..].starts_with('#') {
        return None;
    }
    let after_hash = after_leading_ws + 1;
    // `\s+` greedy first, backtracking one whitespace char at a time (the
    // capture's `.` also matches whitespace, so the backtrack is observable).
    let mut cut_positions: Vec<usize> = vec![after_hash];
    let mut index = after_hash;
    for c in value[after_hash..].chars() {
        if is_js_whitespace(c) {
            index += c.len_utf8();
            cut_positions.push(index);
        } else {
            break;
        }
    }
    for &capture_start in cut_positions[1..].iter().rev() {
        if let Some(hit) = lazy_capture_from(value, capture_start) {
            return Some(hit);
        }
    }
    None
}

/// 去掉行首的 `#` 前缀及随后的空白（回退路径提取标题用）。
fn strip_leading_heading_marks(line: &str) -> &str {
    let without_hashes = line.trim_start_matches('#');
    if without_hashes.len() == line.len() {
        return line;
    }
    &without_hashes[skip_js_whitespace(without_hashes, 0)..]
}

/// Port of the exported `parsePlanMarkdown`.
/// 解析 plan markdown：优先取首个 `#` 标题（标题后须有空白），
/// 否则回退到首个非空行（剥掉 `#` 前缀）；正文为标题之后的内容。
pub fn parse_plan_markdown(raw: &str) -> ParsedPlan {
    let normalized = normalize_newlines(raw);
    if let Some((match_end, capture)) = match_plan_heading(&normalized) {
        let title = {
            let sanitized = sanitize_plan_title(&Value::String(capture.clone()));
            if sanitized.is_empty() {
                "Plan".to_string()
            } else {
                sanitized
            }
        };
        let body = normalized[match_end..].trim_start_matches('\n').to_string();
        return ParsedPlan { title, body };
    }
    let first_line = normalized
        .split('\n')
        .map(js_trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Plan");
    let title = {
        let sanitized = sanitize_plan_title(&Value::String(
            strip_leading_heading_marks(first_line).to_string(),
        ));
        if sanitized.is_empty() {
            "Plan".to_string()
        } else {
            sanitized
        }
    };
    ParsedPlan {
        title,
        body: js_trim(&normalized).to_string(),
    }
}

/// 把标题与正文格式化为规范 markdown：`# 标题` + 空行 + 正文；
/// 正文为空时只写标题行。标题清洗后为空则回退 "Plan"。
fn format_plan_markdown(title: &str, body: &str) -> String {
    let normalized_title = {
        let sanitized = sanitize_plan_title(&Value::String(title.to_string()));
        if sanitized.is_empty() {
            "Plan".to_string()
        } else {
            sanitized
        }
    };
    let normalized_body = js_trim(body);
    if normalized_body.is_empty() {
        format!("# {normalized_title}\n")
    } else {
        format!("# {normalized_title}\n\n{normalized_body}")
    }
}

/// 移植 slugify：trim + 小写化，剔除 markdown 标点，空白段折叠为 `-`，
/// 其余非法字符替换为 `-`，最后折叠连续 `-` 并修剪首尾；空结果回退 "plan"。
fn slugify_plan_title(value: &str) -> String {
    let lowered = js_trim(value).to_lowercase();
    let mut stripped = String::with_capacity(lowered.len());
    for c in lowered.chars() {
        if matches!(
            c,
            '`' | '*'
                | '_'
                | '#'
                | '>'
                | '['
                | ']'
                | '('
                | ')'
                | '{'
                | '}'
                | '.'
                | '!'
                | '?'
                | ','
                | ':'
                | ';'
                | '"'
                | '\''
        ) {
            continue;
        }
        stripped.push(c);
    }
    let mut dashed = String::with_capacity(stripped.len());
    let mut in_whitespace_run = false;
    for c in stripped.chars() {
        if is_js_whitespace(c) {
            if !in_whitespace_run {
                dashed.push('-');
                in_whitespace_run = true;
            }
            continue;
        }
        in_whitespace_run = false;
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
            dashed.push(c);
        } else {
            dashed.push('-');
        }
    }
    // Collapse '-' runs, then trim leading/trailing '-'.
    let mut collapsed = String::with_capacity(dashed.len());
    let mut in_dash_run = false;
    for c in dashed.chars() {
        if c == '-' {
            if !in_dash_run {
                collapsed.push('-');
                in_dash_run = true;
            }
        } else {
            in_dash_run = false;
            collapsed.push(c);
        }
    }
    let trimmed = collapsed.trim_matches('-');
    if trimmed.is_empty() {
        "plan".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 构造 version 2 的空上下文。
fn empty_context() -> ProjectContext {
    ProjectContext {
        version: PROJECT_CONTEXT_VERSION,
        notes: Vec::new(),
        todos: Vec::new(),
        plans: Vec::new(),
    }
}

/// 取路径最后一段（忽略尾部 `/`），把记录的绝对路径还原为文件名。
fn path_basename(value: &str) -> String {
    let trimmed = value.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or_default().to_string()
}

/// 校验 projectId：trim 后必须非空，且仅允许 ASCII 字母数字与 `. _ : -`，
/// 否则返回错误（防止路径穿越等非法存储路径）。
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

// ---------------------------------------------------------------------------
// Storage primitives
// ---------------------------------------------------------------------------

/// read_json 的结果：区分「文件不存在」与「存在但损坏/非对象」两种情形。
struct ReadJson {
    /// 文件是否不存在（IO NotFound）。
    missing: bool,
    /// `None` while present means malformed JSON or a non-object root.
    /// 解析成功且根为对象时才有值。
    value: Option<Map<String, Value>>,
}

/// Node's `readFile(path, 'utf8')` replaces invalid UTF-8 with U+FFFD.
/// 行为等同 Node 的 readFile(path, 'utf8')：非法字节替换为 U+FFFD 而非报错。
async fn read_text_lossy(path: &Path) -> std::io::Result<String> {
    let bytes = tokio::fs::read(path).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// 读取 JSON 文件：NotFound 归一为 missing=true；文件存在但解析失败或
/// 根不是对象时 value=None（是否报错由调用方决定）；其他 IO 错误传播。
async fn read_json(path: &Path) -> AppResult<ReadJson> {
    let text = match read_text_lossy(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ReadJson {
                missing: true,
                value: None,
            });
        }
        Err(error) => return Err(AppError::Io(error)),
    };
    let value = match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Some(map),
        Ok(_) | Err(_) => None,
    };
    Ok(ReadJson {
        missing: false,
        value,
    })
}

/// 以 2 空格缩进的 pretty JSON 原子写入：先写同目录唯一临时文件再 rename，
/// 任一步失败时清理临时文件并返回错误。
async fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> AppResult<()> {
    let parent = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let temporary_path = PathBuf::from(format!(
        "{}.tmp-{}-{}-{:x}",
        path.display(),
        std::process::id(),
        system_now_ms(),
        rand::random::<u64>()
    ));
    let content = serde_json::to_string_pretty(value)
        .map_err(|error| AppError::internal(format!("failed to serialize JSON: {error}")))?;
    let write = async {
        tokio::fs::create_dir_all(&parent).await?;
        tokio::fs::write(&temporary_path, content.as_bytes()).await?;
        tokio::fs::rename(&temporary_path, path).await
    };
    if let Err(error) = write.await {
        let _ = tokio::fs::remove_file(&temporary_path).await;
        return Err(AppError::Io(error));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

/// Port of the object returned by `createProjectContextRuntime`.
/// 移植 createProjectContextRuntime 返回的对象：项目上下文存储运行时。
#[derive(Clone)]
pub struct ProjectContextRuntime {
    /// 共享内部状态：目录、id 工厂与写锁表。
    inner: Arc<RuntimeInner>,
}

/// 运行时的共享内部状态（经 Arc 被 clone 共享）。
struct RuntimeInner {
    /// projects 根目录，所有项目数据都在其下按 `<projectId>/` 分目录存放。
    projects_dir: PathBuf,
    /// 生成便签/plan id 的工厂闭包（默认 UUIDv4，可注入）。
    id_factory: Arc<dyn Fn() -> String + Send + Sync>,
    /// 按 projectId 的写锁链；无等待者时条目被回收。
    write_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

/// JS default id factory (`crypto.randomUUID`), built from `rand`.
/// 默认 id 工厂：基于 rand 生成带版本/变体位的 UUIDv4 字符串
/// （对应 JS 的 crypto.randomUUID）。
fn default_id_factory() -> Arc<dyn Fn() -> String + Send + Sync> {
    Arc::new(|| {
        use rand::RngCore;
        let mut bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut bytes);
        bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
        bytes[8] = (bytes[8] & 0x3f) | 0x80; // RFC 4122 variant
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        format!(
            "{}-{}-{}-{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        )
    })
}

/// 项目上下文运行时实现：围绕 context.json 清单与 plans/ 目录提供
/// 读取（含 legacy 迁移）与 notes/todos/plans 的增删改查。
impl ProjectContextRuntime {
    /// 以给定 projects 根目录创建运行时（默认 UUIDv4 id 工厂、空写锁表）。
    pub fn new(projects_dir: PathBuf) -> Self {
        Self {
            inner: Arc::new(RuntimeInner {
                projects_dir,
                id_factory: default_id_factory(),
                write_locks: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// `<data-dir>/projects` — the JS `OMPCHAMBER_PROJECTS_CONFIG_DIR`.
    /// 以 `<data-dir>/projects`（JS 的 OMPCHAMBER_PROJECTS_CONFIG_DIR）为根目录。
    pub fn for_context(ctx: &RouterContext) -> Self {
        Self::new(ctx.config.data_dir.join("projects"))
    }

    /// Test/dependency seam for the JS `createId` injection.
    /// 注入自定义 id 工厂（对应 JS 的 createId 缝，测试/依赖注入用）；
    /// 仅当运行时尚未被共享（Arc 可独占）时可用，否则 panic。
    pub fn with_id_factory(mut self, id_factory: Arc<dyn Fn() -> String + Send + Sync>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("runtime not shared yet")
            .id_factory = id_factory;
        self
    }

    /// 项目存储目录 `<projects-dir>/<projectId>`（projectId 先经校验）。
    fn storage_dir_for(&self, project_id: &str) -> AppResult<PathBuf> {
        Ok(self
            .inner
            .projects_dir
            .join(sanitize_project_id(project_id)?))
    }

    /// 项目 context.json 的路径。
    pub fn context_path_for(&self, project_id: &str) -> AppResult<PathBuf> {
        Ok(self.storage_dir_for(project_id)?.join("context.json"))
    }

    /// 项目 plans/ 目录的路径。
    pub fn plans_dir_for(&self, project_id: &str) -> AppResult<PathBuf> {
        Ok(self.storage_dir_for(project_id)?.join("plans"))
    }

    /// 旧版客户端配置 `<projectId>.json` 的路径（迁移数据源）。
    fn legacy_config_path_for(&self, project_id: &str) -> AppResult<PathBuf> {
        Ok(self
            .inner
            .projects_dir
            .join(format!("{}.json", sanitize_project_id(project_id)?)))
    }

    /// 当前时间（epoch 毫秒）。
    fn now_ms(&self) -> u64 {
        system_now_ms()
    }

    /// `withWriteLock` — in-process per-project chain.
    /// 移植 withWriteLock：按 projectId 串行执行异步变更；结束后在无人
    /// 等待时回收锁表条目，避免 map 无限增长。
    async fn with_write_lock<T, F>(&self, project_id: &str, mutate: F) -> AppResult<T>
    where
        F: Future<Output = AppResult<T>>,
    {
        let key = sanitize_project_id(project_id)?;
        let chain = {
            let mut locks = self
                .inner
                .write_locks
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            locks
                .entry(key.clone())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let _guard = chain.lock().await;
        let result = mutate.await;
        // Mirror the JS writeLocks entry cleanup: drop the chain entry when no
        // other waiter holds a clone (a fresh waiter re-creates it).
        let mut locks = self
            .inner
            .write_locks
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if Arc::strong_count(&chain) <= 2 {
            locks.remove(&key);
        }
        result
    }

    /// One-time migration of `projectNotes` / `projectTodos` /
    /// `projectPlanFiles` out of the client-owned `<projectId>.json`.
    /// Runs unlocked on purpose (see module docs).
    /// 一次性迁移：把 projectNotes/projectTodos/projectPlanFiles 三个
    /// legacy key 从客户端持有的 `<projectId>.json` 搬入 context.json。
    /// 记录在 plans/ 目录之外的 markdown 会从原路径抢救回来；markdown
    /// 已丢失的链接被剔除；旧配置中的其余字段原样保留。
    async fn migrate_from_legacy_config(
        &self,
        project_id: &str,
        now: &Number,
    ) -> AppResult<Option<ProjectContext>> {
        let legacy_path = self.legacy_config_path_for(project_id)?;
        let legacy = read_json(&legacy_path).await?;
        let Some(legacy_map) = legacy.value else {
            return Ok(None);
        };

        let has_legacy_keys = legacy_map.contains_key("projectNotes")
            || legacy_map.contains_key("projectTodos")
            || legacy_map.contains_key("projectPlanFiles");
        if !has_legacy_keys {
            return Ok(None);
        }

        let plans_dir = self.plans_dir_for(project_id)?;
        let mut links: Vec<Value> = Vec::new();
        let raw_links = legacy_map
            .get("projectPlanFiles")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for entry in &raw_links {
            let Some(object) = entry.as_object() else {
                continue;
            };
            let Some(id) = as_non_empty_string(object.get("id").unwrap_or(&Value::Null)) else {
                continue;
            };
            let Some(absolute_path) =
                as_non_empty_string(object.get("path").unwrap_or(&Value::Null))
            else {
                continue;
            };

            let file = path_basename(&absolute_path);
            if !is_plan_file_name(&file) {
                continue;
            }
            let target_path = plans_dir.join(&file);

            let raw = match read_text_lossy(&target_path).await {
                Ok(raw) => raw,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // Not in the plans directory yet — recover it from the
                    // recorded path.
                    let raw = match read_text_lossy(Path::new(&absolute_path)).await {
                        Ok(raw) => raw,
                        Err(recover) if recover.kind() == std::io::ErrorKind::NotFound => {
                            continue;
                        }
                        Err(recover) => return Err(AppError::Io(recover)),
                    };
                    tokio::fs::create_dir_all(&plans_dir).await?;
                    tokio::fs::write(&target_path, raw.as_bytes()).await?;
                    raw
                }
                Err(error) => return Err(AppError::Io(error)),
            };

            let created_at = timestamp_or(object.get("createdAt"), now.clone());
            links.push(serde_json::json!({
                "id": id,
                "file": file,
                "title": parse_plan_markdown(&raw).title,
                "createdAt": created_at,
            }));
        }

        let migrated = ProjectContext {
            version: PROJECT_CONTEXT_VERSION,
            notes: sanitize_notes(legacy_map.get("projectNotes").unwrap_or(&Value::Null), now),
            todos: sanitize_todos(legacy_map.get("projectTodos").unwrap_or(&Value::Null), now),
            plans: sanitize_plan_links(&Value::Array(links), now),
        };

        write_json_atomic(&self.context_path_for(project_id)?, &migrated).await?;

        let mut remaining = legacy_map.clone();
        remaining.remove("projectNotes");
        remaining.remove("projectTodos");
        remaining.remove("projectPlanFiles");
        write_json_atomic(&legacy_path, &Value::Object(remaining)).await?;

        Ok(Some(migrated))
    }

    /// `readContext` — pub for the session-knowledge port. A missing file is
    /// authoritative empty; malformed JSON is a failure; I/O errors propagate.
    /// 移植 readContext：读取项目上下文（缺失时按需触发 legacy 迁移）。
    /// 文件缺失视为权威空数据；JSON 损坏报错；所有条目经清洗后返回。
    pub async fn read_context(&self, project_id: &str) -> AppResult<ProjectContext> {
        let now = Number::from(self.now_ms());
        let stored = read_json(&self.context_path_for(project_id)?).await?;

        if !stored.missing && stored.value.is_none() {
            return Err(AppError::internal("Stored project context is malformed"));
        }

        if stored.missing {
            if let Some(migrated) = self.migrate_from_legacy_config(project_id, &now).await? {
                let value = serde_json::to_value(&migrated).map_err(|error| {
                    AppError::internal(format!("serialization failed: {error}"))
                })?;
                return Ok(ProjectContext {
                    version: PROJECT_CONTEXT_VERSION,
                    notes: sanitize_notes(value.get("notes").unwrap_or(&Value::Null), &now),
                    todos: sanitize_todos(value.get("todos").unwrap_or(&Value::Null), &now),
                    plans: sanitize_plan_links(value.get("plans").unwrap_or(&Value::Null), &now),
                });
            }
            return Ok(empty_context());
        }

        let map = stored.value.unwrap_or_default();
        Ok(ProjectContext {
            version: PROJECT_CONTEXT_VERSION,
            notes: sanitize_notes(map.get("notes").unwrap_or(&Value::Null), &now),
            todos: sanitize_todos(map.get("todos").unwrap_or(&Value::Null), &now),
            plans: sanitize_plan_links(map.get("plans").unwrap_or(&Value::Null), &now),
        })
    }

    /// 把整份上下文原子写入 context.json。
    async fn write_context(&self, project_id: &str, context: &ProjectContext) -> AppResult<()> {
        write_json_atomic(&self.context_path_for(project_id)?, context).await
    }

    /// `saveTodos`.
    /// 移植 saveTodos：整体替换 todos（经清洗），保留 notes 与 plans。
    pub async fn save_todos(&self, project_id: &str, todos: &Value) -> AppResult<ProjectContext> {
        self.with_write_lock(project_id, async {
            let now = Number::from(self.now_ms());
            let current = self.read_context(project_id).await?;
            let next = ProjectContext {
                version: PROJECT_CONTEXT_VERSION,
                notes: current.notes,
                todos: sanitize_todos(todos, &now),
                plans: current.plans,
            };
            self.write_context(project_id, &next).await?;
            Ok(next)
        })
        .await
    }

    /// `createNote`.
    /// 移植 createNote：新建便签并插入列表首位；body trim 后为空报错，
    /// 已达条数上限时拒绝扩容。
    pub async fn create_note(&self, project_id: &str, value: &Value) -> AppResult<NoteMutation> {
        let raw_body = value.get("body").and_then(Value::as_str).unwrap_or("");
        let body = js_trim(&clamp_length(raw_body, PROJECT_NOTE_BODY_MAX_LENGTH)).to_string();
        if body.is_empty() {
            return Err(AppError::internal("body is required"));
        }

        self.with_write_lock(project_id, async {
            let now = Number::from(self.now_ms());
            let current = self.read_context(project_id).await?;
            if current.notes.len() >= PROJECT_NOTE_MAX_ITEMS {
                return Err(AppError::internal(format!(
                    "A project can hold at most {PROJECT_NOTE_MAX_ITEMS} notes"
                )));
            }

            let note = Note {
                id: (self.inner.id_factory)(),
                body,
                created_at: now.clone(),
                updated_at: now,
                source: value
                    .get("source")
                    .and_then(Value::as_str)
                    .filter(|source| is_note_source(source))
                    .unwrap_or("manual")
                    .to_string(),
                pinned: false,
                origin: sanitize_note_origin(value.get("origin").unwrap_or(&Value::Null)),
            };

            let mut notes = Vec::with_capacity(current.notes.len() + 1);
            notes.push(note.clone());
            notes.extend(current.notes.iter().cloned());
            let next = ProjectContext {
                version: PROJECT_CONTEXT_VERSION,
                notes,
                todos: current.todos,
                plans: current.plans,
            };
            self.write_context(project_id, &next).await?;
            Ok(NoteMutation {
                note,
                context: next,
            })
        })
        .await
    }

    /// `updateNote` — omitted fields are left alone.
    /// 移植 updateNote：按 patch 更新 body 和/或 pinned；未提供的字段
    /// 保持原值；未知 id 返回 Ok(None)。
    pub async fn update_note(
        &self,
        project_id: &str,
        note_id: &str,
        patch: &Value,
    ) -> AppResult<Option<NoteMutation>> {
        let id = as_non_empty_string(&Value::String(note_id.to_string()))
            .ok_or_else(|| AppError::internal("noteId is required"))?;
        let has_body = patch.get("body").is_some_and(Value::is_string);
        let has_pinned = patch.get("pinned").is_some_and(Value::is_boolean);
        if !has_body && !has_pinned {
            return Err(AppError::internal("body or pinned is required"));
        }
        let body = if has_body {
            let raw = patch.get("body").and_then(Value::as_str).unwrap_or("");
            let clamped = js_trim(&clamp_length(raw, PROJECT_NOTE_BODY_MAX_LENGTH)).to_string();
            if clamped.is_empty() {
                return Err(AppError::internal("body is required"));
            }
            clamped
        } else {
            String::new()
        };

        self.with_write_lock(project_id, async {
            let now = Number::from(self.now_ms());
            let current = self.read_context(project_id).await?;
            let Some(index) = current.notes.iter().position(|note| note.id == id) else {
                return Ok(None);
            };

            let mut note = current.notes[index].clone();
            if has_body {
                note.body = body;
                note.updated_at = now;
            }
            if has_pinned {
                note.pinned = patch
                    .get("pinned")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            }
            let mut notes = current.notes.clone();
            notes[index] = note.clone();
            let next = ProjectContext {
                version: PROJECT_CONTEXT_VERSION,
                notes,
                todos: current.todos,
                plans: current.plans,
            };
            self.write_context(project_id, &next).await?;
            Ok(Some(NoteMutation {
                note,
                context: next,
            }))
        })
        .await
    }

    /// `deleteNote`.
    /// 移植 deleteNote：删除指定便签；未知 id 时 deleted=false 且不写盘。
    pub async fn delete_note(&self, project_id: &str, note_id: &str) -> AppResult<DeleteOutcome> {
        let id = as_non_empty_string(&Value::String(note_id.to_string()))
            .ok_or_else(|| AppError::internal("noteId is required"))?;

        self.with_write_lock(project_id, async {
            let current = self.read_context(project_id).await?;
            let Some(index) = current.notes.iter().position(|note| note.id == id) else {
                return Ok(DeleteOutcome {
                    deleted: false,
                    context: current,
                });
            };
            let notes: Vec<Note> = current
                .notes
                .iter()
                .enumerate()
                .filter(|(position, _)| *position != index)
                .map(|(_, note)| note.clone())
                .collect();
            let next = ProjectContext {
                version: PROJECT_CONTEXT_VERSION,
                notes,
                todos: current.todos,
                plans: current.plans,
            };
            self.write_context(project_id, &next).await?;
            Ok(DeleteOutcome {
                deleted: true,
                context: next,
            })
        })
        .await
    }

    /// `readPlan` — pub for the session-knowledge port. `Ok(None)` covers both
    /// a missing link and deleted markdown.
    /// 移植 readPlan：读取单个 plan 并解析其 markdown；链接不存在或
    /// markdown 已被删除都返回 Ok(None)，其余 IO 错误向上传播。
    pub async fn read_plan(&self, project_id: &str, plan_id: &str) -> AppResult<Option<PlanRead>> {
        let id = as_non_empty_string(&Value::String(plan_id.to_string()))
            .ok_or_else(|| AppError::internal("planId is required"))?;
        let context = self.read_context(project_id).await?;
        let Some(link) = context.plans.iter().find(|plan| plan.id == id) else {
            return Ok(None);
        };

        let path = self.plans_dir_for(project_id)?.join(&link.file);
        let raw = match read_text_lossy(&path).await {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(AppError::Io(error)),
        };

        let parsed = parse_plan_markdown(&raw);
        Ok(Some(PlanRead {
            id: link.id.clone(),
            file: link.file.clone(),
            created_at: link.created_at.clone(),
            title: parsed.title,
            body: parsed.body,
            raw,
        }))
    }

    /// `updatePlan` — overwrites the markdown verbatim (clamped) and
    /// re-derives the manifest title.
    /// 移植 updatePlan：用给定 raw 原样覆写 markdown（截断到上限），
    /// 并从新内容重算清单标题；未知 id 或文件已被删除均返回 None。
    pub async fn update_plan(
        &self,
        project_id: &str,
        plan_id: &str,
        value: &Value,
    ) -> AppResult<Option<PlanSaveResult>> {
        let id = as_non_empty_string(&Value::String(plan_id.to_string()))
            .ok_or_else(|| AppError::internal("planId is required"))?;
        let raw_input = value
            .get("raw")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::internal("raw is required"))?;
        let raw = clamp_length(raw_input, PROJECT_PLAN_BODY_MAX_LENGTH);

        self.with_write_lock(project_id, async {
            let current = self.read_context(project_id).await?;
            let Some(index) = current.plans.iter().position(|plan| plan.id == id) else {
                return Ok(None);
            };
            let link = current.plans[index].clone();

            let file_path = self.plans_dir_for(project_id)?.join(&link.file);
            // Refuse to recreate a file that was deleted underneath us.
            match tokio::fs::metadata(&file_path).await {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(AppError::Io(error)),
            }

            tokio::fs::write(&file_path, raw.as_bytes()).await?;

            let parsed = parse_plan_markdown(&raw);
            let next_link = PlanLink {
                title: parsed.title.clone(),
                ..link
            };
            let mut plans = current.plans.clone();
            plans[index] = next_link.clone();
            let next = ProjectContext {
                version: PROJECT_CONTEXT_VERSION,
                notes: current.notes,
                todos: current.todos,
                plans,
            };
            self.write_context(project_id, &next).await?;

            Ok(Some(PlanSaveResult {
                plan: next_link,
                context: next,
                title: parsed.title,
                body: parsed.body,
                raw,
            }))
        })
        .await
    }

    /// `createPlan` — markdown file first, then the manifest entry.
    /// 移植 createPlan：先写 markdown 文件（`<毫秒时间戳>-<slug>.md`，
    /// 同名时追加 `-1`、`-2` … 后缀），再在清单头部插入链接。
    pub async fn create_plan(
        &self,
        project_id: &str,
        value: &Value,
    ) -> AppResult<PlanCreateResult> {
        let title = {
            let sanitized = sanitize_plan_title(value.get("title").unwrap_or(&Value::Null));
            if sanitized.is_empty() {
                "Plan".to_string()
            } else {
                sanitized
            }
        };
        let body = clamp_length(
            value.get("body").and_then(Value::as_str).unwrap_or(""),
            PROJECT_PLAN_BODY_MAX_LENGTH,
        );

        self.with_write_lock(project_id, async {
            let current = self.read_context(project_id).await?;
            let created_at_ms = self.now_ms();
            let plans_dir = self.plans_dir_for(project_id)?;
            tokio::fs::create_dir_all(&plans_dir).await?;

            let base_name = format!("{created_at_ms}-{}", slugify_plan_title(&title));
            let mut file = format!("{base_name}.md");
            let mut attempt = 1;
            while current.plans.iter().any(|plan| plan.file == file) {
                file = format!("{base_name}-{attempt}.md");
                attempt += 1;
            }

            tokio::fs::write(
                plans_dir.join(&file),
                format_plan_markdown(&title, &body).as_bytes(),
            )
            .await?;

            let link = PlanLink {
                id: (self.inner.id_factory)(),
                file,
                title,
                created_at: Number::from(created_at_ms),
                pinned: false,
            };
            let mut plans = Vec::with_capacity(current.plans.len() + 1);
            plans.push(link.clone());
            plans.extend(current.plans.iter().cloned());
            let next = ProjectContext {
                version: PROJECT_CONTEXT_VERSION,
                notes: current.notes,
                todos: current.todos,
                plans,
            };
            self.write_context(project_id, &next).await?;
            Ok(PlanCreateResult {
                plan: link,
                context: next,
            })
        })
        .await
    }

    /// `setPlanPinned`.
    /// 移植 setPlanPinned：仅更新置顶位，标题与文件保持不变。
    pub async fn set_plan_pinned(
        &self,
        project_id: &str,
        plan_id: &str,
        pinned: bool,
    ) -> AppResult<Option<PlanPinnedResult>> {
        let id = as_non_empty_string(&Value::String(plan_id.to_string()))
            .ok_or_else(|| AppError::internal("planId is required"))?;

        self.with_write_lock(project_id, async {
            let current = self.read_context(project_id).await?;
            let Some(index) = current.plans.iter().position(|plan| plan.id == id) else {
                return Ok(None);
            };
            let mut plan = current.plans[index].clone();
            plan.pinned = pinned;
            let mut plans = current.plans.clone();
            plans[index] = plan.clone();
            let next = ProjectContext {
                version: PROJECT_CONTEXT_VERSION,
                notes: current.notes,
                todos: current.todos,
                plans,
            };
            self.write_context(project_id, &next).await?;
            Ok(Some(PlanPinnedResult {
                plan,
                context: next,
            }))
        })
        .await
    }

    /// `deletePlan` — manifest entry first, then the (harmless) unlink.
    /// 移植 deletePlan：先移除清单条目，再删除（可能已不存在的）markdown 文件；
    /// 未知 id 时 deleted=false 且不写盘。
    pub async fn delete_plan(&self, project_id: &str, plan_id: &str) -> AppResult<DeleteOutcome> {
        let id = as_non_empty_string(&Value::String(plan_id.to_string()))
            .ok_or_else(|| AppError::internal("planId is required"))?;

        self.with_write_lock(project_id, async {
            let current = self.read_context(project_id).await?;
            let Some(index) = current.plans.iter().position(|plan| plan.id == id) else {
                return Ok(DeleteOutcome {
                    deleted: false,
                    context: current,
                });
            };
            let file = current.plans[index].file.clone();
            let plans: Vec<PlanLink> = current
                .plans
                .iter()
                .enumerate()
                .filter(|(position, _)| *position != index)
                .map(|(_, plan)| plan.clone())
                .collect();
            let next = ProjectContext {
                version: PROJECT_CONTEXT_VERSION,
                notes: current.notes,
                todos: current.todos,
                plans,
            };
            self.write_context(project_id, &next).await?;
            // rm force:true — an unlink failure is ignored (the entry is
            // already gone; the leftover markdown is unreferenced).
            let _ = tokio::fs::remove_file(self.plans_dir_for(project_id)?.join(&file)).await;
            Ok(DeleteOutcome {
                deleted: true,
                context: next,
            })
        })
        .await
    }
}

/// `setPlanPinned` result: `{ plan, context }`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanPinnedResult {
    /// 更新置顶后的 plan 链接。
    pub plan: PlanLink,
    /// 写入后的完整上下文。
    pub context: ProjectContext,
}

/// Process-wide shared runtime per projects directory, mirroring the JS
/// module-level `projectContextRuntime` singleton (routes and the
/// session-knowledge port must share one write-lock map).
/// 进程级共享单例：按 projects 目录复用同一个运行时实例，保证路由与
/// session-knowledge 移植共享同一份写锁表；全部引用释放后允许重建。
pub fn shared(projects_dir: &Path) -> Arc<ProjectContextRuntime> {
    // 进程级注册表：projects 目录 -> 运行时的弱引用（Weak），无人持有时可被回收。
    static REGISTRY: LazyLock<Mutex<HashMap<PathBuf, Weak<ProjectContextRuntime>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let mut registry = REGISTRY.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(existing) = registry.get(projects_dir).and_then(Weak::upgrade) {
        return existing;
    }
    let runtime = Arc::new(ProjectContextRuntime::new(projects_dir.to_path_buf()));
    registry.insert(projects_dir.to_path_buf(), Arc::downgrade(&runtime));
    runtime
}
