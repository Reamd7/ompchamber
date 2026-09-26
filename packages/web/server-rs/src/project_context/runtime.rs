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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Map, Number, Value};
use tokio::sync::Mutex as AsyncMutex;

use crate::context::RouterContext;
use crate::error::{AppError, AppResult};

pub const PROJECT_CONTEXT_VERSION: u8 = 2;
const PROJECT_NOTE_BODY_MAX_LENGTH: usize = 3000;
const PROJECT_NOTE_MAX_ITEMS: usize = 200;
const PROJECT_TODO_TEXT_MAX_LENGTH: usize = 120;
const PROJECT_PLAN_TITLE_MAX_LENGTH: usize = 160;
const PROJECT_PLAN_BODY_MAX_LENGTH: usize = 200_000;
const PROJECT_TODO_MAX_ITEMS: usize = 500;
const PROJECT_PLAN_MAX_ITEMS: usize = 500;

// ---------------------------------------------------------------------------
// Wire model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NoteOrigin {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "messageId", skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Note {
    pub id: String,
    pub body: String,
    /// Milliseconds since epoch; stored as a raw JSON number so hand-edited
    /// fractional timestamps round-trip exactly like the JS does.
    #[serde(rename = "createdAt")]
    pub created_at: Number,
    #[serde(rename = "updatedAt")]
    pub updated_at: Number,
    pub source: String,
    pub pinned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<NoteOrigin>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Todo {
    pub id: String,
    pub text: String,
    pub completed: bool,
    #[serde(rename = "createdAt")]
    pub created_at: Number,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanLink {
    pub id: String,
    pub file: String,
    pub title: String,
    #[serde(rename = "createdAt")]
    pub created_at: Number,
    pub pinned: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProjectContext {
    pub version: u8,
    pub notes: Vec<Note>,
    pub todos: Vec<Todo>,
    pub plans: Vec<PlanLink>,
}

/// `readPlan` result — the parsed projection of one markdown file.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanRead {
    pub id: String,
    pub file: String,
    #[serde(rename = "createdAt")]
    pub created_at: Number,
    pub title: String,
    pub body: String,
    pub raw: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NoteMutation {
    pub note: Note,
    pub context: ProjectContext,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanCreateResult {
    pub plan: PlanLink,
    pub context: ProjectContext,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanSaveResult {
    pub plan: PlanLink,
    pub context: ProjectContext,
    pub title: String,
    pub body: String,
    pub raw: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeleteOutcome {
    pub deleted: bool,
    pub context: ProjectContext,
}

// ---------------------------------------------------------------------------
// JS-string semantics helpers
// ---------------------------------------------------------------------------

fn system_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// The exact `RegExp \s` class (no `\u{0085}`, includes `\u{feff}`).
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
fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

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

fn as_non_empty_string(value: &Value) -> Option<String> {
    let text = value.as_str()?;
    let trimmed = js_trim(text);
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn is_note_source(value: &str) -> bool {
    matches!(value, "manual" | "selection" | "agent")
}

fn sanitize_note_origin(value: &Value) -> Option<NoteOrigin> {
    let object = value.as_object()?;
    let session_id = as_non_empty_string(object.get("sessionId").unwrap_or(&Value::Null))?;
    let message_id = as_non_empty_string(object.get("messageId").unwrap_or(&Value::Null));
    Some(NoteOrigin {
        session_id,
        message_id,
    })
}

fn timestamp_or(value: Option<&Value>, fallback: Number) -> Number {
    match value.and_then(Value::as_number) {
        Some(number) if number.as_f64().is_some_and(|v| v >= 0.0) => number.clone(),
        _ => fallback,
    }
}

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\u{2028}' | '\u{2029}')
}

// ---------------------------------------------------------------------------
// Sanitizers (ports of sanitizeNotes / sanitizeTodos / sanitizePlanLinks)
// ---------------------------------------------------------------------------

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

fn sanitize_plan_title(value: &Value) -> String {
    clamp_length(
        &as_non_empty_string(value).unwrap_or_default(),
        PROJECT_PLAN_TITLE_MAX_LENGTH,
    )
}

fn is_plan_file_name(file: &str) -> bool {
    // /^[a-zA-Z0-9._-]+\.md$/
    if !file.ends_with(".md") || file.len() <= 3 {
        return false;
    }
    file[..file.len() - 3]
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

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

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedPlan {
    pub title: String,
    pub body: String,
}

fn normalize_newlines(raw: &str) -> String {
    if !raw.contains('\r') {
        return raw.to_string();
    }
    raw.replace("\r\n", "\n").replace('\r', "\n")
}

/// `\s*(?:\n+|$)` anchored at `at`, returning the match end.
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

fn strip_leading_heading_marks(line: &str) -> &str {
    let without_hashes = line.trim_start_matches('#');
    if without_hashes.len() == line.len() {
        return line;
    }
    &without_hashes[skip_js_whitespace(without_hashes, 0)..]
}

/// Port of the exported `parsePlanMarkdown`.
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

fn empty_context() -> ProjectContext {
    ProjectContext {
        version: PROJECT_CONTEXT_VERSION,
        notes: Vec::new(),
        todos: Vec::new(),
        plans: Vec::new(),
    }
}

fn path_basename(value: &str) -> String {
    let trimmed = value.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or_default().to_string()
}

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

struct ReadJson {
    missing: bool,
    /// `None` while present means malformed JSON or a non-object root.
    value: Option<Map<String, Value>>,
}

/// Node's `readFile(path, 'utf8')` replaces invalid UTF-8 with U+FFFD.
async fn read_text_lossy(path: &Path) -> std::io::Result<String> {
    let bytes = tokio::fs::read(path).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

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
#[derive(Clone)]
pub struct ProjectContextRuntime {
    inner: Arc<RuntimeInner>,
}

struct RuntimeInner {
    projects_dir: PathBuf,
    id_factory: Arc<dyn Fn() -> String + Send + Sync>,
    write_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

/// JS default id factory (`crypto.randomUUID`), built from `rand`.
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

impl ProjectContextRuntime {
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
    pub fn for_context(ctx: &RouterContext) -> Self {
        Self::new(ctx.config.data_dir.join("projects"))
    }

    /// Test/dependency seam for the JS `createId` injection.
    pub fn with_id_factory(mut self, id_factory: Arc<dyn Fn() -> String + Send + Sync>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("runtime not shared yet")
            .id_factory = id_factory;
        self
    }

    fn storage_dir_for(&self, project_id: &str) -> AppResult<PathBuf> {
        Ok(self
            .inner
            .projects_dir
            .join(sanitize_project_id(project_id)?))
    }

    pub fn context_path_for(&self, project_id: &str) -> AppResult<PathBuf> {
        Ok(self.storage_dir_for(project_id)?.join("context.json"))
    }

    pub fn plans_dir_for(&self, project_id: &str) -> AppResult<PathBuf> {
        Ok(self.storage_dir_for(project_id)?.join("plans"))
    }

    fn legacy_config_path_for(&self, project_id: &str) -> AppResult<PathBuf> {
        Ok(self
            .inner
            .projects_dir
            .join(format!("{}.json", sanitize_project_id(project_id)?)))
    }

    fn now_ms(&self) -> u64 {
        system_now_ms()
    }

    /// `withWriteLock` — in-process per-project chain.
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

    async fn write_context(&self, project_id: &str, context: &ProjectContext) -> AppResult<()> {
        write_json_atomic(&self.context_path_for(project_id)?, context).await
    }

    /// `saveTodos`.
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
    pub plan: PlanLink,
    pub context: ProjectContext,
}

/// Process-wide shared runtime per projects directory, mirroring the JS
/// module-level `projectContextRuntime` singleton (routes and the
/// session-knowledge port must share one write-lock map).
pub fn shared(projects_dir: &Path) -> Arc<ProjectContextRuntime> {
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
